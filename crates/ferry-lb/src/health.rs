//! Active health checking (ADR 0012).
//!
//! One task per checked endpoint probes it every `interval` (with ±10% jitter so
//! checks of many endpoints do not synchronise), each probe on a fresh
//! connection bounded by `timeout`. `unhealthy_threshold` consecutive failures
//! mark the endpoint unhealthy; `healthy_threshold` consecutive successes mark
//! it healthy again. Endpoints start healthy and are probed immediately, so a
//! dead endpoint is noticed within one round of probes plus the threshold.
//!
//! - **HTTP:** `GET path` over HTTP/1.1 with the configured `Host`; healthy if
//!   the status is in the expected set.
//! - **gRPC:** `grpc.health.v1.Health/Check` over h2c; healthy only if the call
//!   succeeds with `grpc-status: 0` and the response says `SERVING`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::{BufMut, Bytes, BytesMut};
use http::header::{CONTENT_TYPE, HOST, TE, USER_AGENT};
use http::{HeaderValue, Method, Request};
use http_body_util::{BodyExt, Empty, Full};
use hyper::client::conn::{http1, http2};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::endpoint::EndpointState;

/// `User-Agent` sent with HTTP checks.
pub const USER_AGENT_VALUE: &str = "ferry-health-check";

/// What to send and how to judge the answer.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Probe {
    /// HTTP/1.1 GET.
    Http {
        /// Request path.
        path: String,
        /// `Host` header.
        host: String,
        /// Statuses that count as healthy.
        expected_statuses: Vec<u16>,
    },
    /// gRPC health protocol over h2c.
    Grpc {
        /// Service name asked about.
        service: String,
        /// `:authority` sent.
        authority: String,
    },
}

/// Timing and thresholds for one checker.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HealthSpec {
    /// Time between probes.
    pub interval: Duration,
    /// Time allowed for one probe, connect included.
    pub timeout: Duration,
    /// Consecutive failures before marking unhealthy.
    pub unhealthy_threshold: u32,
    /// Consecutive successes before marking healthy.
    pub healthy_threshold: u32,
    /// The probe.
    pub probe: Probe,
}

/// Called after an endpoint's health changes, to rebuild host sets.
pub type OnChange = Arc<dyn Fn() + Send + Sync>;

/// Starts checking `state`'s endpoint until `token` is cancelled.
pub fn spawn(
    spec: HealthSpec,
    state: Arc<EndpointState>,
    on_change: OnChange,
    token: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run(spec, state, on_change, token))
}

async fn run(
    spec: HealthSpec,
    state: Arc<EndpointState>,
    on_change: OnChange,
    token: CancellationToken,
) {
    let addr = state.address();
    let mut successes = 0u32;
    let mut failures = 0u32;
    // A small initial jitter spreads the first round without delaying it much.
    let mut delay = jittered(spec.interval / 10, 1.0);
    loop {
        tokio::select! {
            () = token.cancelled() => return,
            () = tokio::time::sleep(delay) => {}
        }
        let verdict = tokio::select! {
            () = token.cancelled() => return,
            v = tokio::time::timeout(spec.timeout, probe(addr, &spec.probe)) => {
                v.unwrap_or_else(|_| Err("timed out".to_owned()))
            }
        };
        match verdict {
            Ok(()) => {
                successes = successes.saturating_add(1);
                failures = 0;
                if successes >= spec.healthy_threshold && !state.is_healthy() {
                    state.set_healthy(true);
                    info!(endpoint = %addr, "endpoint healthy");
                    on_change();
                }
            }
            Err(reason) => {
                failures = failures.saturating_add(1);
                successes = 0;
                debug!(endpoint = %addr, %reason, failures, "health check failed");
                if failures >= spec.unhealthy_threshold && state.is_healthy() {
                    state.set_healthy(false);
                    warn!(endpoint = %addr, %reason, "endpoint unhealthy");
                    on_change();
                }
            }
        }
        delay = jittered(spec.interval, 0.1);
    }
}

/// `base` scaled by a random factor in `[1 - spread, 1 + spread]`
/// (for `spread = 1.0`, a random fraction of `base`).
fn jittered(base: Duration, spread: f64) -> Duration {
    let factor = if spread >= 1.0 {
        fastrand::f64()
    } else {
        1.0 - spread + 2.0 * spread * fastrand::f64()
    };
    base.mul_f64(factor)
}

/// Runs one probe; `Err` carries a short reason for logs.
pub async fn probe(addr: SocketAddr, probe: &Probe) -> Result<(), String> {
    let stream = TcpStream::connect(addr)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    let _ = stream.set_nodelay(true);
    let io = TokioIo::new(stream);
    match probe {
        Probe::Http {
            path,
            host,
            expected_statuses,
        } => {
            let (mut sender, conn) = http1::handshake::<_, Empty<Bytes>>(io)
                .await
                .map_err(|e| format!("handshake: {e}"))?;
            tokio::spawn(conn);
            let req = Request::get(path.as_str())
                .header(HOST, host.as_str())
                .header(USER_AGENT, USER_AGENT_VALUE)
                .header(http::header::CONNECTION, "close")
                .body(Empty::new())
                .map_err(|e| format!("request: {e}"))?;
            let resp = sender
                .send_request(req)
                .await
                .map_err(|e| format!("request: {e}"))?;
            let status = resp.status().as_u16();
            if expected_statuses.contains(&status) {
                Ok(())
            } else {
                Err(format!("status {status}"))
            }
        }
        Probe::Grpc { service, authority } => {
            let (mut sender, conn) = http2::Builder::new(TokioExecutor::new())
                .timer(TokioTimer::new())
                .handshake::<_, Full<Bytes>>(io)
                .await
                .map_err(|e| format!("handshake: {e}"))?;
            tokio::spawn(conn);
            let req = Request::builder()
                .method(Method::POST)
                .uri(format!("http://{authority}/grpc.health.v1.Health/Check"))
                .header(CONTENT_TYPE, "application/grpc")
                .header(TE, "trailers")
                .header(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE))
                .body(Full::new(encode_check_request(service)))
                .map_err(|e| format!("request: {e}"))?;
            let resp = sender
                .send_request(req)
                .await
                .map_err(|e| format!("request: {e}"))?;
            let headers = resp.headers().clone();
            let collected = resp
                .into_body()
                .collect()
                .await
                .map_err(|e| format!("body: {e}"))?;
            let grpc_status = collected
                .trailers()
                .and_then(|t| t.get("grpc-status"))
                .or_else(|| headers.get("grpc-status"))
                .and_then(|v| v.to_str().ok())
                .unwrap_or("missing")
                .to_owned();
            if grpc_status != "0" {
                return Err(format!("grpc-status {grpc_status}"));
            }
            match decode_check_response(&collected.to_bytes()) {
                Some(SERVING) => Ok(()),
                Some(other) => Err(format!("serving status {other}")),
                None => Err("malformed response".to_owned()),
            }
        }
    }
}

/// `ServingStatus.SERVING` in `grpc.health.v1`.
const SERVING: u64 = 1;

/// A gRPC-framed `HealthCheckRequest { service = 1 }`.
fn encode_check_request(service: &str) -> Bytes {
    let mut msg = BytesMut::new();
    if !service.is_empty() {
        msg.put_u8(0x0a); // field 1, length-delimited
        put_varint(&mut msg, service.len() as u64);
        msg.put_slice(service.as_bytes());
    }
    let mut framed = BytesMut::with_capacity(5 + msg.len());
    framed.put_u8(0); // not compressed
    framed.put_u32(u32::try_from(msg.len()).unwrap_or(u32::MAX));
    framed.put_slice(&msg);
    framed.freeze()
}

fn put_varint(buf: &mut BytesMut, mut v: u64) {
    while v >= 0x80 {
        buf.put_u8((v as u8) | 0x80);
        v >>= 7;
    }
    buf.put_u8(v as u8);
}

/// Reads `status` (field 1, varint) from a gRPC-framed `HealthCheckResponse`.
/// A message without the field means `UNKNOWN` (0).
fn decode_check_response(framed: &[u8]) -> Option<u64> {
    let (&flag, rest) = framed.split_first()?;
    if flag != 0 {
        return None; // compressed; we never ask for compression
    }
    let len = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as usize;
    let mut msg = rest.get(4..4 + len)?;
    let mut status = 0;
    while let Some((&tag, after)) = msg.split_first() {
        msg = after;
        let field = tag >> 3;
        match tag & 0x7 {
            0 => {
                let (v, after) = read_varint(msg)?;
                msg = after;
                if field == 1 {
                    status = v;
                }
            }
            2 => {
                let (n, after) = read_varint(msg)?;
                msg = after.get(usize::try_from(n).ok()?..)?;
            }
            _ => return None,
        }
    }
    Some(status)
}

fn read_varint(mut buf: &[u8]) -> Option<(u64, &[u8])> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let (&b, rest) = buf.split_first()?;
        buf = rest;
        v |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some((v, buf));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferry_testkit::{Behavior, grpc as mock_grpc, http as mock_http};

    #[test]
    fn check_request_encoding() {
        assert_eq!(&encode_check_request("")[..], &[0, 0, 0, 0, 0]);
        assert_eq!(
            &encode_check_request("ab")[..],
            &[0, 0, 0, 0, 4, 0x0a, 2, b'a', b'b']
        );
    }

    #[test]
    fn check_response_decoding() {
        assert_eq!(decode_check_response(&[0, 0, 0, 0, 2, 0x08, 1]), Some(1));
        assert_eq!(decode_check_response(&[0, 0, 0, 0, 0]), Some(0));
        assert_eq!(decode_check_response(&[0, 0, 0, 0, 2, 0x08, 2]), Some(2));
        assert_eq!(decode_check_response(&[1, 0, 0, 0, 0]), None);
        assert_eq!(decode_check_response(&[0, 0, 0, 0, 5, 0x08]), None);
    }

    #[test]
    fn jitter_stays_in_range() {
        let base = Duration::from_secs(10);
        for _ in 0..100 {
            let d = jittered(base, 0.1);
            assert!(d >= Duration::from_secs(9) && d <= Duration::from_secs(11));
        }
    }

    fn http_probe(path: &str) -> Probe {
        Probe::Http {
            path: path.into(),
            host: "svc".into(),
            expected_statuses: vec![200],
        }
    }

    #[tokio::test]
    async fn http_probe_follows_mock_health() {
        let mock = mock_http::spawn("127.0.0.1:0".parse().unwrap(), Behavior::named("a"))
            .await
            .unwrap();
        assert_eq!(
            probe(mock.addr(), &http_probe(mock_http::HEALTH_PATH)).await,
            Ok(())
        );
        mock.set_healthy(false);
        assert!(
            probe(mock.addr(), &http_probe(mock_http::HEALTH_PATH))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn grpc_probe_follows_mock_health() {
        let mock = mock_grpc::spawn("127.0.0.1:0".parse().unwrap(), Behavior::named("g"))
            .await
            .unwrap();
        let p = Probe::Grpc {
            service: String::new(),
            authority: "svc".into(),
        };
        assert_eq!(probe(mock.addr(), &p).await, Ok(()));
        mock.set_healthy(false);
        assert!(probe(mock.addr(), &p).await.is_err());
    }

    #[tokio::test]
    async fn checker_flips_state_after_thresholds() {
        let mock = mock_http::spawn("127.0.0.1:0".parse().unwrap(), Behavior::named("a"))
            .await
            .unwrap();
        let state = Arc::new(EndpointState::new(mock.addr()));
        let changes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&changes);
        let token = CancellationToken::new();
        let spec = HealthSpec {
            interval: Duration::from_millis(20),
            timeout: Duration::from_millis(500),
            unhealthy_threshold: 2,
            healthy_threshold: 2,
            probe: http_probe(mock_http::HEALTH_PATH),
        };
        let task = spawn(
            spec,
            Arc::clone(&state),
            Arc::new(move || {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }),
            token.clone(),
        );
        mock.set_healthy(false);
        wait_for(|| !state.is_healthy()).await;
        mock.set_healthy(true);
        wait_for(|| state.is_healthy()).await;
        assert_eq!(changes.load(std::sync::atomic::Ordering::SeqCst), 2);
        token.cancel();
        task.await.unwrap();
    }

    async fn wait_for(cond: impl Fn() -> bool) {
        for _ in 0..200 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("condition not reached in 2s");
    }
}
