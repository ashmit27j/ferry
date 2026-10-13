//! gRPC interop through Ferry (M2 exit criterion): unary, server streaming,
//! client streaming and bidirectional streaming, over h2c and over TLS.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::time::Duration;

use ferry_integration_tests::{TestProxy, any_port};
use ferry_testkit::grpc::proto::EchoRequest;
use ferry_testkit::grpc::proto::echo_client::EchoClient;
use ferry_testkit::{Behavior, MockServer, grpc as mock_grpc};
use tokio_stream::StreamExt;
use tonic::transport::Channel;

async fn backends(n: usize) -> Vec<MockServer> {
    let mut out = Vec::new();
    for i in 0..n {
        out.push(
            mock_grpc::spawn(any_port(), Behavior::named(format!("g{i}")))
                .await
                .unwrap(),
        );
    }
    out
}

fn yaml(endpoints: &[SocketAddr], listener_extra: &str) -> String {
    let eps: Vec<String> = endpoints
        .iter()
        .map(|a| format!("{{ address: \"{a}\" }}"))
        .collect();
    format!(
        r"
listeners:
  - name: grpc
    address: 127.0.0.1:0
{listener_extra}
    routes:
      - name: echo
        rules:
          - matches: [{{ path: {{ value: /ferry.testkit.echo.v1.Echo }} }}]
            backends: [{{ cluster: echo }}]
clusters:
  - name: echo
    protocol: http2
    lb_policy: least_request
    endpoints: [{}]
",
        eps.join(", ")
    )
}

fn req(message: &str, count: u32) -> EchoRequest {
    EchoRequest {
        message: message.into(),
        count,
    }
}

async fn all_four(mut client: EchoClient<Channel>) {
    // Unary.
    let r = client.unary(req("one", 0)).await.unwrap().into_inner();
    assert_eq!(r.message, "one");

    // Server streaming: 50 messages in order.
    let mut stream = client
        .server_stream(req("s", 50))
        .await
        .unwrap()
        .into_inner();
    let mut seen = 0;
    while let Some(m) = stream.next().await {
        assert_eq!(m.unwrap().sequence, seen);
        seen += 1;
    }
    assert_eq!(seen, 50);

    // Client streaming: the server joins what it received.
    let outbound = tokio_stream::iter((0..20).map(|i| req(&i.to_string(), 0)));
    let r = client.client_stream(outbound).await.unwrap().into_inner();
    let expected: Vec<String> = (0..20).map(|i| i.to_string()).collect();
    assert_eq!(r.message, expected.join(","));
    assert_eq!(r.sequence, 20);

    // Bidirectional: interleaved request and response messages.
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    let mut replies = client
        .bidi(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();
    for i in 0..30u32 {
        tx.send(req(&format!("b{i}"), 0)).await.unwrap();
        let m = replies.next().await.unwrap().unwrap();
        assert_eq!((m.message, m.sequence), (format!("b{i}"), i));
    }
    drop(tx);
    assert!(
        replies.next().await.is_none(),
        "stream ends after the client half-closes"
    );
}

#[tokio::test]
async fn all_four_call_types_over_h2c() {
    let g = backends(2).await;
    let proxy = TestProxy::start(&yaml(&[g[0].addr(), g[1].addr()], "")).await;
    let client = EchoClient::connect(format!("http://{}", proxy.addr))
        .await
        .unwrap();
    all_four(client).await;
}

#[tokio::test]
async fn grpc_errors_and_trailers_pass_through() {
    let failing = mock_grpc::spawn(any_port(), Behavior::named("bad").with_errors(1.0, 503))
        .await
        .unwrap();
    let proxy = TestProxy::start(&yaml(&[failing.addr()], "")).await;
    let mut client = EchoClient::connect(format!("http://{}", proxy.addr))
        .await
        .unwrap();
    for call in 0..3 {
        let status = match call {
            0 => client.unary(req("x", 0)).await.unwrap_err(),
            1 => client.server_stream(req("x", 3)).await.unwrap_err(),
            _ => client
                .client_stream(tokio_stream::iter(vec![req("x", 0)]))
                .await
                .unwrap_err(),
        };
        assert_eq!(status.code(), tonic::Code::Unavailable, "call {call}");
        assert!(status.message().contains("injected failure"), "{status:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_concurrent_streams_share_connections() {
    let g = backends(3).await;
    let addrs: Vec<_> = g.iter().map(MockServer::addr).collect();
    let proxy = TestProxy::start(&yaml(&addrs, "")).await;
    let channel = Channel::from_shared(format!("http://{}", proxy.addr))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let tasks: Vec<_> = (0..64)
        .map(|i| {
            let mut client = EchoClient::new(channel.clone());
            tokio::spawn(async move {
                let mut s = client
                    .server_stream(req(&format!("c{i}"), 25))
                    .await
                    .unwrap()
                    .into_inner();
                let mut n = 0;
                while let Some(m) = s.next().await {
                    assert_eq!(m.unwrap().message, format!("c{i}"));
                    n += 1;
                }
                n
            })
        })
        .collect();
    for t in tasks {
        assert_eq!(t.await.unwrap(), 25);
    }
    let total: u64 = g.iter().map(|b| b.stats().requests()).sum();
    assert_eq!(total, 64);
}

#[tokio::test]
async fn all_four_call_types_over_tls() {
    let g = backends(1).await;
    let ck = rcgen::generate_simple_self_signed(vec!["grpc.test".to_owned()]).unwrap();
    let (cert, key) = (ck.cert.pem(), ck.signing_key.serialize_pem());
    let tls = format!("    tls: {{ certificates: [{{ cert_pem: {cert:?}, key_pem: {key:?} }}] }}");
    let proxy = TestProxy::start(&yaml(&[g[0].addr()], &tls)).await;
    let tls_config = tonic::transport::ClientTlsConfig::new()
        .ca_certificate(tonic::transport::Certificate::from_pem(cert))
        .domain_name("grpc.test");
    let channel = Channel::from_shared(format!("https://{}", proxy.addr))
        .unwrap()
        .tls_config(tls_config)
        .unwrap()
        .timeout(Duration::from_secs(10))
        .connect()
        .await
        .unwrap();
    all_four(EchoClient::new(channel)).await;
}
