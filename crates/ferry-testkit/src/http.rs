//! HTTP/1.1 and h2c echo backend.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderValue, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::{Behavior, MockServer, Stats};

/// Request header that overrides the injected latency for one request.
pub const LATENCY_HEADER: &str = "x-mock-latency-ms";
/// Request header that forces the response status for one request.
pub const STATUS_HEADER: &str = "x-mock-status";
/// Response header naming the backend that answered.
pub const BACKEND_HEADER: &str = "x-mock-backend";

/// Starts an HTTP/1.1 + h2c echo server on `addr`.
///
/// # Errors
///
/// Fails if `addr` cannot be bound.
pub async fn spawn(addr: SocketAddr, behavior: Behavior) -> std::io::Result<MockServer> {
    let listener = TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    let stats = Arc::new(Stats::default());
    let token = CancellationToken::new();
    let task = tokio::spawn(accept_loop(
        listener,
        Arc::new(behavior),
        Arc::clone(&stats),
        token.clone(),
    ));
    Ok(MockServer::new(local, stats, token, task))
}

async fn accept_loop(
    listener: TcpListener,
    behavior: Arc<Behavior>,
    stats: Arc<Stats>,
    token: CancellationToken,
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
        let service =
            service_fn(move |req| respond(req, Arc::clone(&behavior), Arc::clone(&stats)));
        let conn = builder
            .serve_connection(TokioIo::new(stream), service)
            .into_owned();
        let token = token.clone();
        tokio::spawn(async move {
            // Cancelling drops the connection, closing the socket mid-flight.
            tokio::select! {
                _ = conn => {}
                () = token.cancelled() => {}
            }
        });
    }
}

async fn respond(
    req: Request<Incoming>,
    behavior: Arc<Behavior>,
    stats: Arc<Stats>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let latency = header_u64(&req, LATENCY_HEADER).map_or(behavior.latency, Duration::from_millis);
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
    });
    let mut resp = Response::new(Full::new(Bytes::from(echo.to_string())));
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

fn header_u64<B>(req: &Request<B>, name: &str) -> Option<u64> {
    req.headers().get(name)?.to_str().ok()?.parse().ok()
}
