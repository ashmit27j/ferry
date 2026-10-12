//! Request, per-attempt and idle timeouts, and gRPC deadlines.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use ferry_integration_tests::{TestProxy, any_port, send};
use ferry_testkit::grpc::proto::EchoRequest;
use ferry_testkit::grpc::proto::echo_client::EchoClient;
use ferry_testkit::{Behavior, grpc as mock_grpc, http as mock_http};
use http_body_util::BodyExt;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn yaml(backend: SocketAddr, timeouts: &str, protocol: &str) -> String {
    format!(
        r#"
listeners:
  - name: http
    address: 127.0.0.1:0
    routes:
      - name: all
        rules:
          - backends: [{{ cluster: c }}]
            timeouts: {timeouts}
clusters:
  - {{ name: c, protocol: {protocol}, endpoints: [{{ address: "{backend}" }}] }}
"#
    )
}

#[tokio::test]
async fn request_timeout_answers_504() {
    let b = mock_http::spawn(
        any_port(),
        Behavior::named("slow").with_latency(Duration::from_millis(800)),
    )
    .await
    .unwrap();
    let proxy = TestProxy::start(&yaml(b.addr(), "{ request: 100ms }", "http1")).await;
    let started = Instant::now();
    let r = send(proxy.addr, "/", &[], false).await;
    assert_eq!(r.status, 504);
    assert_eq!(&r.body[..], b"upstream request timeout");
    assert!(
        started.elapsed() < Duration::from_millis(600),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn backend_request_timeout_answers_504() {
    let b = mock_http::spawn(any_port(), Behavior::named("a"))
        .await
        .unwrap();
    let proxy = TestProxy::start(&yaml(
        b.addr(),
        "{ request: 5s, backend_request: 100ms }",
        "http1",
    ))
    .await;
    let r = send(
        proxy.addr,
        "/",
        &[(mock_http::LATENCY_HEADER, "800")],
        false,
    )
    .await;
    assert_eq!(r.status, 504);
    let fast = send(proxy.addr, "/", &[], false).await;
    assert_eq!(fast.status, 200);
}

#[tokio::test]
async fn stalled_response_body_is_cut_off() {
    let b = mock_http::spawn(any_port(), Behavior::named("a"))
        .await
        .unwrap();
    let proxy = TestProxy::start(&yaml(b.addr(), "{ idle: 150ms }", "http1")).await;
    let client =
        Client::builder(TokioExecutor::new()).build_http::<http_body_util::Empty<bytes::Bytes>>();
    let req = http::Request::get(format!("http://{}/", proxy.addr))
        .header(mock_http::BODY_DELAY_HEADER, "2000")
        .body(http_body_util::Empty::new())
        .unwrap();
    let resp = client.request(req).await.unwrap();
    assert_eq!(resp.status(), 200, "headers arrive before the stall");
    let started = Instant::now();
    let body = resp.into_body().collect().await;
    assert!(body.is_err(), "the stalled body must be reset");
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "{:?}",
        started.elapsed()
    );

    // A body that arrives within the idle timeout is fine.
    let ok = send(
        proxy.addr,
        "/",
        &[(mock_http::BODY_DELAY_HEADER, "50")],
        false,
    )
    .await;
    assert_eq!(ok.status, 200);
    assert_eq!(ok.echo()["backend"], "a");
}

#[tokio::test]
async fn stalled_request_body_answers_408() {
    let b = mock_http::spawn(any_port(), Behavior::named("a"))
        .await
        .unwrap();
    let proxy = TestProxy::start(&yaml(b.addr(), "{ idle: 150ms }", "http1")).await;
    let mut s = TcpStream::connect(proxy.addr).await.unwrap();
    s.write_all(b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 100\r\n\r\npartial")
        .await
        .unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), s.read_to_end(&mut out)).await;
    let resp = String::from_utf8_lossy(&out);
    assert!(resp.starts_with("HTTP/1.1 408"), "{resp}");
}

#[tokio::test]
async fn grpc_deadline_caps_the_request_and_answers_deadline_exceeded() {
    let g = mock_grpc::spawn(
        any_port(),
        Behavior::named("g").with_latency(Duration::from_millis(800)),
    )
    .await
    .unwrap();
    // No route timeout: the client's grpc-timeout alone must bound the wait.
    let proxy = TestProxy::start(&yaml(g.addr(), "{}", "http2")).await;
    // A plain HTTP/2 client: tonic would enforce grpc-timeout itself and
    // give up first, hiding whether Ferry honours it.
    let client = Client::builder(TokioExecutor::new())
        .http2_only(true)
        .build_http::<http_body_util::Full<bytes::Bytes>>();
    let msg = prost::Message::encode_to_vec(&EchoRequest {
        message: "x".into(),
        count: 0,
    });
    let mut framed = vec![0u8];
    framed.extend_from_slice(&u32::try_from(msg.len()).unwrap().to_be_bytes());
    framed.extend_from_slice(&msg);
    let req = http::Request::post(format!(
        "http://{}/ferry.testkit.echo.v1.Echo/Unary",
        proxy.addr
    ))
    .header("content-type", "application/grpc")
    .header("te", "trailers")
    .header("grpc-timeout", "100m")
    .body(http_body_util::Full::new(bytes::Bytes::from(framed)))
    .unwrap();
    let started = Instant::now();
    let resp = client.request(req).await.unwrap();
    assert_eq!(resp.status(), 200, "gRPC errors travel as HTTP 200");
    assert_eq!(resp.headers()["grpc-status"], "4", "DEADLINE_EXCEEDED");
    assert_eq!(resp.headers()["grpc-message"], "upstream request timeout");
    assert!(started.elapsed() < Duration::from_millis(600));
}

#[tokio::test]
async fn grpc_route_timeout_answers_deadline_exceeded() {
    let g = mock_grpc::spawn(
        any_port(),
        Behavior::named("g").with_latency(Duration::from_millis(800)),
    )
    .await
    .unwrap();
    let proxy = TestProxy::start(&yaml(g.addr(), "{ request: 100ms }", "http2")).await;
    let mut client = EchoClient::connect(format!("http://{}", proxy.addr))
        .await
        .unwrap();
    let err = client
        .unary(EchoRequest {
            message: "x".into(),
            count: 0,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::DeadlineExceeded, "{err:?}");
    assert_eq!(err.message(), "upstream request timeout");
}

#[tokio::test]
async fn grpc_local_replies_carry_grpc_status() {
    let g = mock_grpc::spawn(any_port(), Behavior::named("g"))
        .await
        .unwrap();
    let yaml = format!(
        r#"
listeners:
  - name: http
    address: 127.0.0.1:0
    routes:
      - name: only-other-host
        hostnames: [other.test]
        rules: [{{ backends: [{{ cluster: c }}] }}]
clusters:
  - {{ name: c, protocol: http2, endpoints: [{{ address: "{}" }}] }}
"#,
        g.addr()
    );
    let proxy = TestProxy::start(&yaml).await;
    let mut client = EchoClient::connect(format!("http://{}", proxy.addr))
        .await
        .unwrap();
    let err = client
        .unary(EchoRequest {
            message: "x".into(),
            count: 0,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unimplemented, "{err:?}");
    assert_eq!(err.message(), "no route");
}
