//! The controller-to-proxy snapshot stream end to end, in process: mutual
//! TLS, ACKs, NACKs that keep the last good snapshot, and a controller
//! restart the proxy rides out.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use ferry_controller::certgen;
use ferry_controller::server::{Hub, TlsFiles, serve};
use ferry_controller::world::ObjKey;
use ferry_integration_tests::{any_port, eventually, get, metric};
use ferry_proxy::remote::{RemoteSource, RemoteTls, follow};
use ferry_proxy::{Drain, Proxy, ReloadHandle};
use ferry_telemetry::Telemetry;
use ferry_testkit::{Behavior, MockServer, http as mock_http};
use tokio_util::sync::CancellationToken;

const SERVER_NAME: &str = "ferry-controller.ferry-system.svc";

fn config(backend: SocketAddr, extra_listener: Option<SocketAddr>) -> ferry_config::Config {
    let extra = extra_listener
        .map(|a| {
            format!(
                "  - name: extra\n    address: \"{a}\"\n    routes: [{{ name: r, rules: [{{ backends: [{{ cluster: c }}] }}] }}]\n"
            )
        })
        .unwrap_or_default();
    ferry_config::from_yaml_str(&format!(
        r#"
listeners:
  - name: http
    address: 127.0.0.1:0
    routes: [{{ name: r, rules: [{{ backends: [{{ cluster: c }}] }}] }}]
{extra}
clusters:
  - {{ name: c, endpoints: [{{ address: "{backend}" }}] }}
"#
    ))
    .unwrap()
}

struct Controller {
    hub: Arc<Hub>,
    stop: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

async fn controller(addr: SocketAddr, tls: &certgen::Bundle) -> Controller {
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    let hub = Hub::new();
    let stop = CancellationToken::new();
    let files = TlsFiles {
        ca_pem: tls.ca_cert.clone(),
        cert_pem: tls.server_cert.clone(),
        key_pem: tls.server_key.clone(),
    };
    let task = tokio::spawn(serve(listener, Arc::clone(&hub), Some(files), stop.clone()).map_ok());
    Controller { hub, stop, task }
}

trait MapOk {
    fn map_ok(self) -> impl std::future::Future<Output = ()> + Send;
}

impl<F: std::future::Future<Output = Result<(), tonic::transport::Error>> + Send> MapOk for F {
    async fn map_ok(self) {
        self.await.unwrap();
    }
}

async fn proxy(
    port: u16,
    tls: &certgen::Bundle,
    client: &certgen::Bundle,
    id: &str,
) -> (ReloadHandle, CancellationToken) {
    let proxy = Proxy::bind_awaiting(Arc::new(Telemetry::default()))
        .await
        .unwrap();
    let handle = proxy.reload_handle();
    let stop = CancellationToken::new();
    let stopped = stop.clone();
    tokio::spawn(proxy.run(async move { stopped.cancelled().await }, Drain::default()));
    let source = RemoteSource {
        endpoint: format!("https://127.0.0.1:{port}"),
        gateway: "infra/gw".into(),
        proxy_id: id.into(),
        tls: Some(RemoteTls {
            ca_pem: tls.ca_cert.clone(),
            cert_pem: client.client_cert.clone(),
            key_pem: client.client_key.clone(),
            server_name: SERVER_NAME.into(),
        }),
    };
    tokio::spawn(follow(source, handle.clone(), stop.clone()));
    (handle, stop)
}

async fn backend(name: &str) -> MockServer {
    mock_http::spawn(any_port(), Behavior::named(name))
        .await
        .unwrap()
}

#[tokio::test]
async fn proxy_follows_the_controller_and_survives_its_restart() {
    let pki = certgen::generate(&[SERVER_NAME.into()]).unwrap();
    let addr = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let gw = ObjKey::new("infra", "gw");
    let (a, b) = (backend("a").await, backend("b").await);

    let ctl = controller(addr, &pki).await;
    let v1 = ctl
        .hub
        .publish(&gw, &config(a.addr(), None))
        .unwrap()
        .unwrap();
    let (handle, stop) = proxy(addr.port(), &pki, &pki, "proxy-0").await;
    eventually(Duration::from_secs(10), || {
        handle.listener_addr("http").is_some()
    })
    .await;
    assert!(!handle.is_awaiting_config());
    let listener = handle.listener_addr("http").unwrap();
    assert_eq!(get(listener, "/").await.backend(), "a");
    eventually(Duration::from_secs(5), || {
        ctl.hub
            .proxy_views()
            .get("proxy-0")
            .map(|v| v.acked_version)
            == Some(v1)
    })
    .await;

    // A snapshot the proxy cannot apply (its extra listener's port is taken)
    // is rejected, and the proxy keeps serving the previous one.
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    ctl.hub
        .publish(&gw, &config(b.addr(), Some(taken.local_addr().unwrap())))
        .unwrap();
    eventually(Duration::from_secs(5), || {
        ctl.hub
            .proxy_views()
            .get("proxy-0")
            .and_then(|v| v.last_error.clone())
            .is_some_and(|e| e.contains("cannot bind"))
    })
    .await;
    assert_eq!(get(listener, "/").await.backend(), "a");
    assert_eq!(handle.current_version().0, v1);
    // Counted once, although both the swap and the stream saw it fail.
    let text = handle.telemetry().metrics.encode().unwrap();
    assert_eq!(
        metric(&text, r#"ferry_config_reloads_total{result="failure"}"#),
        Some(1.0),
        "{text}"
    );
    assert_eq!(metric(&text, "ferry_controller_connected"), Some(1.0));
    assert!(
        text.contains(r#"ferry_tls_certificate_expiry_timestamp_seconds{kind="controller",owner="snapshot-stream""#),
        "{text}"
    );

    // The controller goes away: the proxy keeps serving.
    ctl.stop.cancel();
    ctl.task.await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(get(listener, "/").await.backend(), "a");
    let connected = || {
        let text = handle.telemetry().metrics.encode().unwrap();
        metric(&text, "ferry_controller_connected")
    };
    eventually(Duration::from_secs(5), || connected() == Some(0.0)).await;

    // A new controller (another replica, or the same after a restart)
    // publishes a new config; the proxy reconnects and applies it.
    let ctl = controller(addr, &pki).await;
    ctl.hub.publish(&gw, &config(b.addr(), None)).unwrap();
    eventually(Duration::from_secs(20), || {
        ctl.hub
            .proxy_views()
            .get("proxy-0")
            .is_some_and(|v| v.last_error.is_none() && v.acked_version > v1)
    })
    .await;
    assert_eq!(
        get(listener, "/").await.backend(),
        "b",
        "same socket, new routes"
    );
    assert_eq!(connected(), Some(1.0));
    stop.cancel();
    ctl.stop.cancel();
}

#[tokio::test]
async fn proxies_without_a_trusted_client_certificate_are_refused() {
    let pki = certgen::generate(&[SERVER_NAME.into()]).unwrap();
    let stranger = certgen::generate(&[SERVER_NAME.into()]).unwrap();
    let addr = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let a = backend("a").await;
    let ctl = controller(addr, &pki).await;
    ctl.hub
        .publish(&ObjKey::new("infra", "gw"), &config(a.addr(), None))
        .unwrap();
    // Trusts the right CA, but its client certificate comes from another one.
    let (handle, stop) = proxy(addr.port(), &pki, &stranger, "intruder").await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        handle.is_awaiting_config(),
        "no snapshot without a trusted client certificate"
    );
    assert!(ctl.hub.proxy_views().is_empty());
    stop.cancel();
    ctl.stop.cancel();
}
