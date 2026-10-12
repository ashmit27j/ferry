//! Request limits and request-smuggling defences, exercised with raw bytes so
//! no client library can normalise the attack away.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::time::Duration;

use ferry_integration_tests::{TestProxy, any_port, request};
use ferry_testkit::{Behavior, MockServer, http as mock_http};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

async fn setup(limits: &str) -> (MockServer, TestProxy) {
    let backend = mock_http::spawn(any_port(), Behavior::named("a"))
        .await
        .unwrap();
    let yaml = format!(
        r#"
listeners:
  - name: http
    address: 127.0.0.1:0
    limits: {limits}
    routes:
      - name: all
        rules:
          - backends: [{{ cluster: c }}]
clusters:
  - {{ name: c, endpoints: [{{ address: "{}" }}] }}
"#,
        backend.addr()
    );
    (backend, TestProxy::start(&yaml).await)
}

/// Writes `raw` and reads until the proxy closes the connection.
async fn exchange(addr: SocketAddr, raw: &[u8]) -> String {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(raw).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out)).await;
    String::from_utf8_lossy(&out).into_owned()
}

fn status_lines(response: &str) -> Vec<&str> {
    response
        .lines()
        .filter(|l| l.starts_with("HTTP/1.1 "))
        .collect()
}

#[tokio::test]
async fn too_many_headers_get_431() {
    let (backend, proxy) = setup("{ max_request_headers: 10 }").await;
    let mut raw = String::from("GET / HTTP/1.1\r\nHost: x\r\n");
    for i in 0..20 {
        raw.push_str(&format!("x-h{i}: v\r\n"));
    }
    raw.push_str("Connection: close\r\n\r\n");
    let resp = exchange(proxy.addr, raw.as_bytes()).await;
    assert!(resp.starts_with("HTTP/1.1 431"), "{resp}");
    assert_eq!(backend.stats().requests(), 0);
}

#[tokio::test]
async fn oversized_header_block_gets_431() {
    let (backend, proxy) = setup("{ max_request_headers_kb: 8 }").await;
    let big = "a".repeat(10 * 1024);
    let raw = format!("GET / HTTP/1.1\r\nHost: x\r\nx-big: {big}\r\nConnection: close\r\n\r\n");
    let resp = exchange(proxy.addr, raw.as_bytes()).await;
    assert!(resp.starts_with("HTTP/1.1 431"), "{resp}");
    assert_eq!(backend.stats().requests(), 0);
}

#[tokio::test]
async fn declared_oversized_body_gets_413_without_reaching_backend() {
    let (backend, proxy) = setup("{ max_request_body_bytes: 16 }").await;
    let r = request(
        proxy.addr,
        "POST",
        "/",
        &[],
        "this body is longer than sixteen bytes",
    )
    .await;
    assert_eq!(r.status, 413);
    assert_eq!(backend.stats().requests(), 0);
    let ok = request(proxy.addr, "POST", "/", &[], "short").await;
    assert_eq!(ok.status, 200);
}

#[tokio::test]
async fn streamed_oversized_body_gets_413() {
    let (_backend, proxy) = setup("{ max_request_body_bytes: 16 }").await;
    // Chunked, so the size is only known while streaming.
    let raw = "POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n\
               10\r\n0123456789abcdef\r\n10\r\n0123456789abcdef\r\n0\r\n\r\n";
    let resp = exchange(proxy.addr, raw.as_bytes()).await;
    assert!(resp.starts_with("HTTP/1.1 413"), "{resp}");
}

/// CL.TE smuggling: a front end that trusted Content-Length would forward the
/// trailing `GET /smuggled` as part of the body; one that trusted
/// Transfer-Encoding but kept the connection open would parse it as a second
/// request. Ferry decodes the chunked body (empty), closes the connection,
/// and re-frames the request for the backend, so `/smuggled` never exists.
#[tokio::test]
async fn cl_te_smuggling_never_reaches_the_backend() {
    let (backend, proxy) = setup("{}").await;
    for raw in [
        "POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 30\r\nTransfer-Encoding: chunked\r\n\r\n\
         0\r\n\r\nGET /smuggled HTTP/1.1\r\nHost: x\r\n\r\n",
        "POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nContent-Length: 30\r\n\r\n\
         0\r\n\r\nGET /smuggled HTTP/1.1\r\nHost: x\r\n\r\n",
    ] {
        let resp = exchange(proxy.addr, raw.as_bytes()).await;
        assert_eq!(status_lines(&resp), ["HTTP/1.1 200 OK"], "{resp}");
        assert!(resp.contains("\"path\":\"/\""), "{resp}");
        assert!(!resp.contains("smuggled"), "{resp}");
    }
    assert_eq!(backend.stats().requests(), 2, "one request per connection");
}

#[tokio::test]
async fn conflicting_content_lengths_get_400() {
    let (backend, proxy) = setup("{}").await;
    let raw = "POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 3\r\nContent-Length: 5\r\n\r\nabcde";
    let resp = exchange(proxy.addr, raw.as_bytes()).await;
    assert!(resp.starts_with("HTTP/1.1 400"), "{resp}");
    assert_eq!(backend.stats().requests(), 0);
}

#[tokio::test]
async fn non_chunked_transfer_encoding_gets_400() {
    let (backend, proxy) = setup("{}").await;
    let raw = "POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: gzip\r\n\r\nabc";
    let resp = exchange(proxy.addr, raw.as_bytes()).await;
    assert!(resp.starts_with("HTTP/1.1 400"), "{resp}");
    assert_eq!(backend.stats().requests(), 0);
}
