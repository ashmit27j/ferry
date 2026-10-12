//! HTTP/1.1 and h2c echo backend.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderValue, Request, Response, StatusCode};
use http_body_util::BodyExt;
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use tokio::net::TcpListener;
use tokio::time::Sleep;
use tokio_util::sync::CancellationToken;

use crate::{Behavior, MockServer, Stats};

/// Request header that overrides the injected latency for one request.
pub const LATENCY_HEADER: &str = "x-mock-latency-ms";
/// Request header that forces the response status for one request.
pub const STATUS_HEADER: &str = "x-mock-status";
/// Request header that delays the response body (not the headers) by this
/// many milliseconds, to simulate a stalled stream.
pub const BODY_DELAY_HEADER: &str = "x-mock-body-delay-ms";
/// Response header naming the backend that answered.
pub const BACKEND_HEADER: &str = "x-mock-backend";
/// Health endpoint: 200 when healthy, 503 otherwise. Not counted in stats.
pub const HEALTH_PATH: &str = "/healthz";

/// Starts an HTTP/1.1 + h2c echo server on `addr`.
///
/// # Errors
///
/// Fails if `addr` cannot be bound.
pub async fn spawn(addr: SocketAddr, behavior: Behavior) -> std::io::Result<MockServer> {
    spawn_inner(addr, behavior, None).await
}

/// Starts the same echo server behind TLS. The echo gains a `tls` object
/// with the negotiated ALPN protocol and whether the client sent a
/// certificate, so tests can check mutual TLS.
///
/// # Errors
///
/// Fails if `addr` cannot be bound.
pub async fn spawn_tls(
    addr: SocketAddr,
    behavior: Behavior,
    config: Arc<rustls::ServerConfig>,
) -> std::io::Result<MockServer> {
    spawn_inner(addr, behavior, Some(config)).await
}

async fn spawn_inner(
    addr: SocketAddr,
    behavior: Behavior,
    tls: Option<Arc<rustls::ServerConfig>>,
) -> std::io::Result<MockServer> {
    let listener = TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    let stats = Arc::new(Stats::default());
    let token = CancellationToken::new();
    let task = tokio::spawn(accept_loop(
        listener,
        Arc::new(behavior),
        Arc::clone(&stats),
        token.clone(),
        tls,
    ));
    Ok(MockServer::new(local, stats, token, task))
}

/// What the TLS layer saw, reported in the echo.
#[derive(Debug, Clone)]
struct TlsInfo {
    alpn: Option<String>,
    client_cert: bool,
}

async fn accept_loop(
    listener: TcpListener,
    behavior: Arc<Behavior>,
    stats: Arc<Stats>,
    token: CancellationToken,
    tls: Option<Arc<rustls::ServerConfig>>,
) {
    let builder = auto::Builder::new(TokioExecutor::new());
    loop {
        let stream = tokio::select! {
            () = token.cancelled() => return,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(_) => continue,
            },
        };
        let _ = stream.set_nodelay(true);
        let behavior = Arc::clone(&behavior);
        let stats = Arc::clone(&stats);
        let builder = builder.clone();
        let token = token.clone();
        let tls = tls.clone();
        tokio::spawn(async move {
            let serve = async {
                match tls {
                    None => {
                        let service = service_fn(move |req| {
                            respond(req, Arc::clone(&behavior), Arc::clone(&stats), None)
                        });
                        let _ = builder
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    }
                    Some(config) => {
                        let Ok(stream) =
                            tokio_rustls::TlsAcceptor::from(config).accept(stream).await
                        else {
                            return;
                        };
                        let (_, conn) = stream.get_ref();
                        let info = TlsInfo {
                            alpn: conn
                                .alpn_protocol()
                                .map(|p| String::from_utf8_lossy(p).into_owned()),
                            client_cert: conn.peer_certificates().is_some_and(|c| !c.is_empty()),
                        };
                        let service = service_fn(move |req| {
                            respond(
                                req,
                                Arc::clone(&behavior),
                                Arc::clone(&stats),
                                Some(info.clone()),
                            )
                        });
                        let _ = builder
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    }
                }
            };
            // Cancelling drops the connection, closing the socket mid-flight.
            tokio::select! {
                () = serve => {}
                () = token.cancelled() => {}
            }
        });
    }
}

async fn respond(
    req: Request<Incoming>,
    behavior: Arc<Behavior>,
    stats: Arc<Stats>,
    tls: Option<TlsInfo>,
) -> Result<Response<MockBody>, Infallible> {
    if req.uri().path() == HEALTH_PATH {
        let mut resp = Response::new(MockBody::now(Bytes::from_static(b"ok")));
        if !stats.healthy() {
            *resp.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
        }
        return Ok(resp);
    }
    let latency = header_u64(&req, LATENCY_HEADER).map_or(behavior.latency, Duration::from_millis);
    let body_delay = header_u64(&req, BODY_DELAY_HEADER).map(Duration::from_millis);
    let forced = header_u64(&req, STATUS_HEADER)
        .and_then(|s| u16::try_from(s).ok())
        .and_then(|s| StatusCode::from_u16(s).ok());

    let (parts, body) = req.into_parts();
    let body_bytes = body
        .collect()
        .await
        .map(|b| b.to_bytes().len())
        .unwrap_or_default();
    if !latency.is_zero() {
        tokio::time::sleep(latency).await;
    }

    let status = forced.unwrap_or_else(|| {
        if behavior.should_fail() {
            StatusCode::from_u16(behavior.error_status).unwrap_or(StatusCode::SERVICE_UNAVAILABLE)
        } else {
            StatusCode::OK
        }
    });
    stats.record(status.is_server_error());

    let headers: BTreeMap<String, String> = parts
        .headers
        .iter()
        .map(|(k, v)| {
            (
                k.to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();
    let echo = serde_json::json!({
        "backend": behavior.name,
        "method": parts.method.as_str(),
        "path": parts.uri.path(),
        "query": parts.uri.query(),
        "version": format!("{:?}", parts.version),
        "headers": headers,
        "body_bytes": body_bytes,
        "tls": tls.map(|t| serde_json::json!({ "alpn": t.alpn, "client_cert": t.client_cert })),
    });
    let data = Bytes::from(echo.to_string());
    let mut resp = Response::new(match body_delay {
        Some(d) => MockBody::delayed(data, d),
        None => MockBody::now(data),
    });
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if let Ok(name) = HeaderValue::from_str(&behavior.name) {
        resp.headers_mut().insert(BACKEND_HEADER, name);
    }
    Ok(resp)
}

/// Response body of the HTTP mock: ready at once, or after a delay that
/// starts once the headers are sent.
#[derive(Debug)]
pub struct MockBody {
    delay: Option<Pin<Box<Sleep>>>,
    data: Option<Bytes>,
}

impl MockBody {
    fn now(data: Bytes) -> Self {
        Self {
            delay: None,
            data: Some(data),
        }
    }

    fn delayed(data: Bytes, delay: Duration) -> Self {
        Self {
            delay: Some(Box::pin(tokio::time::sleep(delay))),
            data: Some(data),
        }
    }
}

impl Body for MockBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        let this = self.get_mut();
        if let Some(delay) = this.delay.as_mut() {
            if delay.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
            this.delay = None;
        }
        Poll::Ready(this.data.take().map(|d| Ok(Frame::data(d))))
    }

    fn is_end_stream(&self) -> bool {
        self.data.is_none()
    }

    fn size_hint(&self) -> SizeHint {
        match (&self.delay, &self.data) {
            // Unknown length forces chunked encoding, so headers go out first.
            (Some(_), _) => SizeHint::default(),
            (None, Some(d)) => SizeHint::with_exact(d.len() as u64),
            (None, None) => SizeHint::with_exact(0),
        }
    }
}

fn header_u64<B>(req: &Request<B>, name: &str) -> Option<u64> {
    req.headers().get(name)?.to_str().ok()?.parse().ok()
}
