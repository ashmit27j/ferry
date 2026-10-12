//! Tests for the mock backends themselves.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bytes::Bytes;
use ferry_testkit::grpc::proto::EchoRequest;
use ferry_testkit::grpc::proto::echo_client::EchoClient;
use ferry_testkit::{Behavior, grpc, http as mock_http, ws};
use futures_util::{SinkExt, StreamExt, stream};
use http_body_util::{BodyExt, Empty};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use tokio_tungstenite::tungstenite::Message;

fn any_port() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

async fn get(addr: SocketAddr, h2: bool, headers: &[(&str, &str)]) -> (u16, serde_json::Value) {
    let client = Client::builder(TokioExecutor::new())
        .http2_only(h2)
        .build_http::<Empty<Bytes>>();
    let mut req = http::Request::get(format!("http://{addr}/echo/path?q=1"));
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = client
        .request(req.body(Empty::new()).unwrap())
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn http1_echoes_request() {
    let mock = mock_http::spawn(any_port(), Behavior::named("a"))
        .await
        .unwrap();
    let (status, body) = get(mock.addr(), false, &[("x-test", "yes")]).await;
    assert_eq!(status, 200);
    assert_eq!(body["backend"], "a");
    assert_eq!(body["path"], "/echo/path");
    assert_eq!(body["query"], "q=1");
    assert_eq!(body["version"], "HTTP/1.1");
    assert_eq!(body["headers"]["x-test"], "yes");
    assert_eq!(mock.stats().requests(), 1);
}

#[tokio::test]
async fn h2c_prior_knowledge_is_served() {
    let mock = mock_http::spawn(any_port(), Behavior::named("b"))
        .await
        .unwrap();
    let (status, body) = get(mock.addr(), true, &[]).await;
    assert_eq!(status, 200);
    assert_eq!(body["version"], "HTTP/2.0");
}

#[tokio::test]
async fn latency_header_delays_response() {
    let mock = mock_http::spawn(any_port(), Behavior::named("slow"))
        .await
        .unwrap();
    let start = Instant::now();
    get(mock.addr(), false, &[(mock_http::LATENCY_HEADER, "150")]).await;
    assert!(start.elapsed() >= Duration::from_millis(150));
}

#[tokio::test]
async fn configured_latency_applies() {
    let behavior = Behavior::named("slow").with_latency(Duration::from_millis(100));
    let mock = mock_http::spawn(any_port(), behavior).await.unwrap();
    let start = Instant::now();
    get(mock.addr(), false, &[]).await;
    assert!(start.elapsed() >= Duration::from_millis(100));
}

#[tokio::test]
async fn status_header_forces_status() {
    let mock = mock_http::spawn(any_port(), Behavior::named("s"))
        .await
        .unwrap();
    let (status, _) = get(mock.addr(), false, &[(mock_http::STATUS_HEADER, "418")]).await;
    assert_eq!(status, 418);
}

#[tokio::test]
async fn error_rate_one_always_fails() {
    let mock = mock_http::spawn(any_port(), Behavior::named("bad").with_errors(1.0, 502))
        .await
        .unwrap();
    for _ in 0..5 {
        assert_eq!(get(mock.addr(), false, &[]).await.0, 502);
    }
    assert_eq!(mock.stats().errors(), 5);
}

#[tokio::test]
async fn killed_mock_refuses_connections() {
    let mock = mock_http::spawn(any_port(), Behavior::named("k"))
        .await
        .unwrap();
    let addr = mock.addr();
    get(addr, false, &[]).await;
    mock.kill().await;
    assert!(tokio::net::TcpStream::connect(addr).await.is_err());
}

async fn grpc_client(addr: SocketAddr) -> EchoClient<tonic::transport::Channel> {
    EchoClient::connect(format!("http://{addr}")).await.unwrap()
}

fn req(message: &str, count: u32) -> EchoRequest {
    EchoRequest {
        message: message.into(),
        count,
    }
}

#[tokio::test]
async fn grpc_unary() {
    let mock = grpc::spawn(any_port(), Behavior::named("g")).await.unwrap();
    let resp = grpc_client(mock.addr())
        .await
        .unary(req("hi", 0))
        .await
        .unwrap();
    assert_eq!(resp.get_ref().message, "hi");
    assert_eq!(resp.get_ref().backend, "g");
}

#[tokio::test]
async fn grpc_server_streaming() {
    let mock = grpc::spawn(any_port(), Behavior::named("g")).await.unwrap();
    let mut client = grpc_client(mock.addr()).await;
    let stream = client
        .server_stream(req("x", 5))
        .await
        .unwrap()
        .into_inner();
    let seqs: Vec<u32> = stream.map(|r| r.unwrap().sequence).collect().await;
    assert_eq!(seqs, [0, 1, 2, 3, 4]);
}

#[tokio::test]
async fn grpc_client_streaming() {
    let mock = grpc::spawn(any_port(), Behavior::named("g")).await.unwrap();
    let mut client = grpc_client(mock.addr()).await;
    let outbound = stream::iter(["a", "b", "c"].map(|m| req(m, 0)));
    let resp = client.client_stream(outbound).await.unwrap().into_inner();
    assert_eq!(resp.message, "a,b,c");
    assert_eq!(resp.sequence, 3);
}

#[tokio::test]
async fn grpc_bidi_streaming() {
    let mock = grpc::spawn(any_port(), Behavior::named("g")).await.unwrap();
    let mut client = grpc_client(mock.addr()).await;
    let outbound = stream::iter(["one", "two"].map(|m| req(m, 0)));
    let inbound = client.bidi(outbound).await.unwrap().into_inner();
    let got: Vec<(String, u32)> = inbound
        .map(|r| {
            let r = r.unwrap();
            (r.message, r.sequence)
        })
        .collect()
        .await;
    assert_eq!(got, [("one".into(), 0), ("two".into(), 1)]);
}

#[tokio::test]
async fn grpc_error_injection_returns_unavailable() {
    let mock = grpc::spawn(any_port(), Behavior::named("g").with_errors(1.0, 503))
        .await
        .unwrap();
    let err = grpc_client(mock.addr())
        .await
        .unary(req("x", 0))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unavailable);
}

#[tokio::test]
async fn websocket_echoes_text_and_binary() {
    let mock = ws::spawn(any_port(), Behavior::named("w")).await.unwrap();
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{}/chat", mock.addr()))
        .await
        .unwrap();
    socket.send(Message::text("hello")).await.unwrap();
    assert_eq!(
        socket.next().await.unwrap().unwrap(),
        Message::text("hello")
    );
    socket.send(Message::binary(vec![1u8, 2, 3])).await.unwrap();
    assert_eq!(
        socket.next().await.unwrap().unwrap(),
        Message::binary(vec![1u8, 2, 3])
    );
    socket.close(None).await.unwrap();
    assert_eq!(mock.stats().requests(), 2);
}

#[tokio::test]
async fn websocket_error_injection_closes_with_1011() {
    let mock = ws::spawn(any_port(), Behavior::named("w").with_errors(1.0, 503))
        .await
        .unwrap();
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{}/", mock.addr()))
        .await
        .unwrap();
    socket.send(Message::text("boom")).await.unwrap();
    match socket.next().await.unwrap().unwrap() {
        Message::Close(Some(frame)) => assert_eq!(u16::from(frame.code), 1011),
        other => panic!("expected close frame, got {other:?}"),
    }
}
