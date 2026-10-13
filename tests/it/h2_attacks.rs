//! HTTP/2 attacks, sent as raw frames, must be rejected cleanly (M2 exit
//! criterion): the attacking connection is closed, little or nothing reaches
//! the backend, and the proxy keeps serving everyone else.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use ferry_integration_tests::{TestProxy, any_port, get, send, single_cluster_yaml};
use ferry_testkit::{Behavior, MockServer, http as mock_http};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
const HEADERS: u8 = 0x1;
const RST_STREAM: u8 = 0x3;
const SETTINGS: u8 = 0x4;
const GOAWAY: u8 = 0x7;
const CONTINUATION: u8 = 0x9;
const END_STREAM: u8 = 0x1;
const END_HEADERS: u8 = 0x4;
const ENHANCE_YOUR_CALM: u32 = 0xb;

fn frame(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let len = u32::try_from(payload.len()).unwrap();
    let mut f = len.to_be_bytes()[1..].to_vec();
    f.push(kind);
    f.push(flags);
    f.extend_from_slice(&stream.to_be_bytes());
    f.extend_from_slice(payload);
    f
}

/// HPACK for `GET / http` with `:authority: x`: three static-table entries
/// and one literal without indexing.
const GET_BLOCK: &[u8] = &[0x82, 0x86, 0x84, 0x01, 0x01, b'x'];

/// One literal header field (`x-flood: aaa…`) of about `n` bytes.
fn literal_field(n: usize) -> Vec<u8> {
    let name = b"x-flood";
    let mut f = vec![0x00, u8::try_from(name.len()).unwrap()];
    f.extend_from_slice(name);
    let value_len = n.min(126);
    f.push(u8::try_from(value_len).unwrap());
    f.extend(std::iter::repeat_n(b'a', value_len));
    f
}

/// What the server did with the connection.
#[derive(Debug)]
struct Outcome {
    goaway: Option<u32>,
    closed: bool,
}

/// Opens a connection, writes `attack` (ignoring write errors once the server
/// hangs up), and collects frames until the server closes or `wait` passes.
async fn attack(addr: SocketAddr, attack: Vec<u8>, wait: Duration) -> Outcome {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut bytes = PREFACE.to_vec();
    bytes.extend(frame(SETTINGS, 0, 0, &[]));
    bytes.extend(attack);
    let (mut r, mut w) = s.split();
    let writer = async {
        for chunk in bytes.chunks(16 * 1024) {
            if w.write_all(chunk).await.is_err() {
                break;
            }
        }
    };
    let mut buf = Vec::new();
    let reader = async {
        let deadline = Instant::now() + wait;
        let mut chunk = [0u8; 16 * 1024];
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, r.read(&mut chunk)).await {
                Ok(Ok(0)) | Ok(Err(_)) => return true,
                Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
                Err(_) => return false,
            }
        }
    };
    let ((), closed) = tokio::join!(writer, reader);
    Outcome {
        goaway: goaway_code(&buf),
        closed,
    }
}

fn goaway_code(mut buf: &[u8]) -> Option<u32> {
    let mut found = None;
    while buf.len() >= 9 {
        let len = usize::try_from(u32::from_be_bytes([0, buf[0], buf[1], buf[2]])).unwrap();
        if buf.len() < 9 + len {
            break;
        }
        if buf[3] == GOAWAY && len >= 8 {
            found = Some(u32::from_be_bytes([buf[13], buf[14], buf[15], buf[16]]));
        }
        buf = &buf[9 + len..];
    }
    found
}

async fn setup(limits: &str) -> (MockServer, TestProxy) {
    let backend = mock_http::spawn(any_port(), Behavior::named("a"))
        .await
        .unwrap();
    let yaml = single_cluster_yaml(&[backend.addr()]).replace(
        "    address: 127.0.0.1:0\n",
        &format!("    address: 127.0.0.1:0\n    limits: {limits}\n"),
    );
    (backend, TestProxy::start(&yaml).await)
}

async fn still_healthy(addr: SocketAddr) {
    assert_eq!(
        get(addr, "/").await.status,
        200,
        "HTTP/1.1 after the attack"
    );
    assert_eq!(
        send(addr, "/", &[], true).await.status,
        200,
        "h2c after the attack"
    );
}

/// CVE-2023-44487: open a stream and reset it at once, a thousand times.
#[tokio::test]
async fn rapid_reset_is_cut_off_with_enhance_your_calm() {
    let (backend, proxy) = setup("{}").await;
    let mut flood = Vec::new();
    for i in 0..1000u32 {
        let id = 2 * i + 1;
        flood.extend(frame(HEADERS, END_STREAM | END_HEADERS, id, GET_BLOCK));
        flood.extend(frame(RST_STREAM, 0, id, &8u32.to_be_bytes()));
    }
    let out = attack(proxy.addr, flood, Duration::from_secs(5)).await;
    assert_eq!(out.goaway, Some(ENHANCE_YOUR_CALM), "{out:?}");
    assert!(out.closed, "the attacker's connection must be closed");
    assert!(
        backend.stats().requests() < 100,
        "{} of 1000 reset streams reached the backend",
        backend.stats().requests()
    );
    still_healthy(proxy.addr).await;
}

/// CVE-2024-27316 family: a header block continued forever.
#[tokio::test]
async fn continuation_flood_is_bounded_by_the_header_limit() {
    let (backend, proxy) = setup("{ max_request_headers_kb: 16 }").await;
    let mut flood = frame(HEADERS, END_STREAM, 1, GET_BLOCK);
    let field = literal_field(120);
    // About 3 MiB of headers, never finished: 25,000 frames.
    for _ in 0..25_000 {
        flood.extend(frame(CONTINUATION, 0, 1, &field));
    }
    let started = Instant::now();
    let out = attack(proxy.addr, flood, Duration::from_secs(10)).await;
    assert!(
        out.closed,
        "the attacker's connection must be closed: {out:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(backend.stats().requests(), 0);
    still_healthy(proxy.addr).await;
}

/// More concurrent streams than SETTINGS_MAX_CONCURRENT_STREAMS allows.
#[tokio::test]
async fn excess_concurrent_streams_are_refused() {
    let backend = mock_http::spawn(
        any_port(),
        Behavior::named("slow").with_latency(Duration::from_millis(500)),
    )
    .await
    .unwrap();
    let yaml = single_cluster_yaml(&[backend.addr()]).replace(
        "    address: 127.0.0.1:0\n",
        "    address: 127.0.0.1:0\n    limits: { h2_max_concurrent_streams: 10 }\n",
    );
    let proxy = TestProxy::start(&yaml).await;
    let mut flood = Vec::new();
    for i in 0..200u32 {
        flood.extend(frame(
            HEADERS,
            END_STREAM | END_HEADERS,
            2 * i + 1,
            GET_BLOCK,
        ));
    }
    let _ = attack(proxy.addr, flood, Duration::from_secs(2)).await;
    assert!(
        backend.stats().requests() <= 10,
        "{} streams were served at once; the limit is 10",
        backend.stats().requests()
    );
    tokio::time::sleep(Duration::from_millis(600)).await;
    still_healthy(proxy.addr).await;
}

#[test]
fn frame_helpers_round_trip() {
    let f = frame(GOAWAY, 0, 0, &[0, 0, 0, 1, 0, 0, 0, 0xb]);
    assert_eq!(goaway_code(&f), Some(ENHANCE_YOUR_CALM));
    assert_eq!(literal_field(10).len(), 2 + 7 + 1 + 10);
}
