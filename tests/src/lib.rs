//! Shared harness for Ferry's in-process integration tests.
//!
//! [`TestProxy::start`] binds a proxy on an ephemeral port from YAML and runs it
//! on the current tokio runtime; [`send`] issues requests over HTTP/1.1 or h2c.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use ferry_config::{Snapshot, SnapshotVersion};
use ferry_proxy::{Proxy, ProxyError, ReloadHandle};
use http::{HeaderMap, Request, StatusCode};
use http_body_util::{BodyExt, Empty};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// Parses and validates YAML into a snapshot, panicking with the errors on failure.
#[must_use]
pub fn snapshot(yaml: &str, version: u64) -> Snapshot {
    ferry_config::from_yaml_str(yaml)
        .unwrap_or_else(|e| panic!("yaml does not parse: {e}\n{yaml}"))
        .into_snapshot(SnapshotVersion(version))
        .unwrap_or_else(|e| panic!("yaml does not validate: {e}\n{yaml}"))
}

/// YAML for one catch-all route over one round-robin cluster.
#[must_use]
pub fn single_cluster_yaml(endpoints: &[SocketAddr]) -> String {
    let eps: Vec<String> = endpoints
        .iter()
        .map(|a| format!("{{ address: \"{a}\" }}"))
        .collect();
    format!(
        r"
listeners:
  - name: http
    address: 127.0.0.1:0
    routes:
      - name: all
        rules:
          - backends: [{{ cluster: c }}]
clusters:
  - name: c
    endpoints: [{}]
",
        eps.join(", ")
    )
}

/// A proxy running in the background of the test's runtime.
#[derive(Debug)]
pub struct TestProxy {
    /// Address of the first listener.
    pub addr: SocketAddr,
    /// Handle for hot-swapping snapshots.
    pub reload: ReloadHandle,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), ProxyError>>>,
}

impl TestProxy {
    /// Binds and starts a proxy for `yaml` (listeners should use port 0).
    pub async fn start(yaml: &str) -> Self {
        Self::start_with_drain(yaml, Duration::from_secs(5)).await
    }

    /// Like [`TestProxy::start`] with a custom drain timeout.
    pub async fn start_with_drain(yaml: &str, drain: Duration) -> Self {
        let proxy = Proxy::bind(snapshot(yaml, 1)).await.expect("proxy binds");
        let addr = proxy.local_addrs()[0];
        let reload = proxy.reload_handle();
        let (tx, rx) = oneshot::channel();
        let task = tokio::spawn(proxy.run(
            async {
                let _ = rx.await;
            },
            drain,
        ));
        Self {
            addr,
            reload,
            shutdown: Some(tx),
            task: Some(task),
        }
    }

    /// Starts draining without waiting for it to finish.
    pub fn begin_shutdown(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }

    /// Drains and waits for the proxy to stop.
    pub async fn shutdown(mut self) {
        self.begin_shutdown();
        if let Some(task) = self.task.take() {
            task.await
                .expect("proxy task joins")
                .expect("proxy exits cleanly");
        }
    }
}

impl Drop for TestProxy {
    fn drop(&mut self) {
        self.begin_shutdown();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// A response with its body collected.
#[derive(Debug)]
pub struct Reply {
    /// Status code.
    pub status: StatusCode,
    /// Response headers.
    pub headers: HeaderMap,
    /// Response body.
    pub body: Bytes,
}

impl Reply {
    /// Parses the body as the mock backend's JSON echo.
    #[must_use]
    pub fn echo(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("not a JSON echo ({e}): {:?}", self.body))
    }

    /// Name of the mock backend that answered.
    #[must_use]
    pub fn backend(&self) -> String {
        self.echo()["backend"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }
}

/// Sends a GET through the proxy. `h2` selects prior-knowledge HTTP/2.
pub async fn send(addr: SocketAddr, path: &str, headers: &[(&str, &str)], h2: bool) -> Reply {
    let client = Client::builder(TokioExecutor::new())
        .http2_only(h2)
        .build_http::<Empty<Bytes>>();
    let mut req = Request::get(format!("http://{addr}{path}"));
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = client
        .request(req.body(Empty::new()).unwrap())
        .await
        .expect("request reaches proxy");
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        headers,
        body,
    }
}

/// HTTP/1.1 GET with no extra headers.
pub async fn get(addr: SocketAddr, path: &str) -> Reply {
    send(addr, path, &[], false).await
}

/// An address with nothing listening on it.
#[must_use]
pub fn dead_addr() -> SocketAddr {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap()
}

/// `127.0.0.1:0`.
#[must_use]
pub fn any_port() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}
