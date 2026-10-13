//! Per-request telemetry: what one exchange did, recorded when its response
//! body finishes (ADR 0027–0029).
//!
//! [`Exchange`] is created when a request arrives and filled in as it is
//! routed and forwarded. When the response headers are ready, Ferry's added
//! latency is observed; the exchange then rides inside the response body
//! ([`Observed`]) and, when the last byte has been sent (or the body is
//! dropped early), records the request in its route's RED metrics, ends the
//! server span and writes the access-log line.

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use ferry_config::ClusterId;
use ferry_telemetry::Telemetry;
use ferry_telemetry::access_log::{Entry, rfc3339_millis};
use ferry_telemetry::metrics::{ListenerStats, Reason, RouteStats};
use ferry_telemetry::trace::{Attribute, ServerSpan};
use http::header::{self, HeaderValue};
use http::{HeaderMap, Method, Response, Uri, Version};
use hyper::body::{Body, Frame, SizeHint};

use crate::body::{BoxError, ProxyBody};
use crate::state::Runtime;

static GRPC_STATUS: header::HeaderName = header::HeaderName::from_static("grpc-status");

/// Time spent on upstream attempts, which is not Ferry's added latency.
#[derive(Debug, Default)]
pub(crate) struct UpstreamLog {
    /// Waiting for response headers (all attempts) and retry backoff.
    pub(crate) wait: Duration,
    /// Start of the attempt in progress, if one is.
    pub(crate) in_flight: Option<Instant>,
    /// Attempts made.
    pub(crate) attempts: u32,
    /// Endpoint of the last attempt.
    pub(crate) endpoint: Option<SocketAddr>,
}

impl UpstreamLog {
    pub(crate) fn begin(&mut self) {
        self.in_flight = Some(Instant::now());
    }

    pub(crate) fn end(&mut self) -> Duration {
        let spent = self
            .in_flight
            .take()
            .map_or(Duration::ZERO, |t| t.elapsed());
        self.wait += spent;
        spent
    }

    /// Total upstream time, counting an attempt cut short by a deadline.
    fn total(&self) -> Duration {
        self.wait + self.in_flight.map_or(Duration::ZERO, |t| t.elapsed())
    }
}

/// The request fields [`Exchange::capture`] reads.
struct Parts<'a> {
    method: &'a Method,
    uri: &'a Uri,
    version: Version,
    headers: &'a HeaderMap,
}

/// Request fields copied for the access log, only when it is enabled.
#[derive(Debug)]
struct LogFields {
    start: SystemTime,
    method: Method,
    uri: Uri,
    version: Version,
    authority: Option<HeaderValue>,
    user_agent: Option<HeaderValue>,
    forwarded_for: Option<HeaderValue>,
    request_id: Option<HeaderValue>,
}

/// One request's telemetry.
#[derive(Debug)]
pub(crate) struct Exchange {
    started: Instant,
    /// Wall-clock arrival time, taken only when a span or log needs it.
    wall: Option<SystemTime>,
    runtime: Arc<Runtime>,
    telemetry: Arc<Telemetry>,
    listener_index: usize,
    peer: SocketAddr,
    pub(crate) listener: Option<Arc<ListenerStats>>,
    route: Option<Arc<RouteStats>>,
    route_index: Option<usize>,
    pub(crate) cluster: Option<ClusterId>,
    reason: Option<Reason>,
    pub(crate) upstream: UpstreamLog,
    pub(crate) span: Option<ServerSpan>,
    log: Option<Box<LogFields>>,
    grpc_status: Option<u8>,
}

impl Exchange {
    /// Starts an exchange for a request that just arrived; starts its server
    /// span when tracing is on.
    pub(crate) fn start(
        runtime: Arc<Runtime>,
        telemetry: Arc<Telemetry>,
        listener_index: usize,
        peer: SocketAddr,
        headers: &HeaderMap,
        method: &Method,
    ) -> Self {
        let started = Instant::now();
        let wall =
            (telemetry.tracing.is_some() || telemetry.access_log.is_some()).then(SystemTime::now);
        let span = telemetry
            .tracing
            .as_ref()
            .zip(wall)
            .map(|(t, at)| t.server_span(headers, method.as_str(), at));
        let listener = runtime
            .listeners
            .get(listener_index)
            .and_then(Option::as_ref)
            .map(|l| Arc::clone(&l.stats));
        Self {
            started,
            wall,
            runtime,
            telemetry,
            listener_index,
            peer,
            listener,
            route: None,
            route_index: None,
            cluster: None,
            reason: None,
            upstream: UpstreamLog::default(),
            span,
            log: None,
            grpc_status: None,
        }
    }

    /// Copies what the access log and span need from the request as it
    /// arrived, before anything can answer it.
    pub(crate) fn capture<B>(&mut self, req: &http::Request<B>, scheme: &'static str) {
        let parts = Parts {
            method: req.method(),
            uri: req.uri(),
            version: req.version(),
            headers: req.headers(),
        };
        let host = parts.headers.get(header::HOST).cloned().or_else(|| {
            parts
                .uri
                .authority()
                .and_then(|a| HeaderValue::from_str(a.as_str()).ok())
        });
        if let Some(span) = &self.span {
            let mut attrs = vec![
                Attribute::new("http.request.method", parts.method.to_string()),
                Attribute::new("url.path", parts.uri.path().to_owned()),
                Attribute::new("url.scheme", scheme),
                Attribute::new("client.address", self.peer.ip().to_string()),
                Attribute::new("network.protocol.version", protocol_version(parts.version)),
            ];
            if let Some(h) = host.as_ref().and_then(|h| h.to_str().ok()) {
                attrs.push(Attribute::new("server.address", h.to_owned()));
            }
            if let Some(ua) = parts
                .headers
                .get(header::USER_AGENT)
                .and_then(|v| v.to_str().ok())
            {
                attrs.push(Attribute::new("user_agent.original", ua.to_owned()));
            }
            if let Some(name) = self.listener_name() {
                attrs.push(Attribute::new("ferry.listener", name.to_owned()));
            }
            span.set_attributes(attrs);
        }
        if self.telemetry.access_log.is_some() {
            self.log = Some(Box::new(LogFields {
                start: self.wall.unwrap_or_else(SystemTime::now),
                method: parts.method.clone(),
                uri: parts.uri.clone(),
                version: parts.version,
                authority: host,
                user_agent: parts.headers.get(header::USER_AGENT).cloned(),
                forwarded_for: parts.headers.get("x-forwarded-for").cloned(),
                request_id: parts.headers.get("x-request-id").cloned(),
            }));
        }
    }

    /// Records the request ID and `X-Forwarded-For` as forwarded upstream.
    pub(crate) fn forwarded(&mut self, headers: &HeaderMap) {
        if let Some(log) = self.log.as_mut() {
            log.request_id = headers.get("x-request-id").cloned();
            log.forwarded_for = headers.get("x-forwarded-for").cloned();
        }
    }

    /// The request matched no route (or the listener is gone).
    pub(crate) fn unrouted(&mut self) {
        self.route = self
            .runtime
            .listeners
            .get(self.listener_index)
            .and_then(Option::as_ref)
            .map(|l| Arc::clone(&l.unmatched));
    }

    /// The request matched route `index` of its listener.
    pub(crate) fn routed(&mut self, index: usize, method: &Method) {
        let listener = self
            .runtime
            .listeners
            .get(self.listener_index)
            .and_then(Option::as_ref);
        self.route = listener.and_then(|l| l.route_stats.get(index).cloned());
        self.route_index = Some(index);
        if let (Some(span), Some(route)) = (
            &self.span,
            listener.and_then(|l| l.config.routes().get(index)),
        ) {
            span.rename(format!("{method} {}", route.name()));
            span.set_attributes([Attribute::new("http.route", route.name().to_owned())]);
        }
    }

    /// Ferry answers the request itself, for `reason`.
    pub(crate) fn local(&mut self, reason: Reason) {
        self.reason = Some(reason);
    }

    fn listener_name(&self) -> Option<&str> {
        self.runtime
            .listeners
            .get(self.listener_index)
            .and_then(Option::as_ref)
            .map(|l| l.config.name())
    }

    fn route_name(&self) -> &str {
        self.route_index
            .and_then(|i| {
                self.runtime
                    .listeners
                    .get(self.listener_index)
                    .and_then(Option::as_ref)?
                    .config
                    .routes()
                    .get(i)
            })
            .map_or(ferry_telemetry::metrics::NO_ROUTE, |r| r.name())
    }

    fn cluster_name(&self) -> Option<&str> {
        self.cluster
            .and_then(|id| self.runtime.cluster(id))
            .map(|c| c.config.name())
    }

    /// Response headers are ready: records Ferry's added latency and wraps
    /// the body so the rest is recorded when it finishes.
    pub(crate) fn respond(mut self, resp: Response<ProxyBody>) -> Response<Observed> {
        let to_headers = self.started.elapsed();
        let overhead = to_headers.saturating_sub(self.upstream.total());
        if let Some(l) = &self.listener {
            l.overhead.observe(overhead);
            if let Some(r) = self.reason {
                l.local_reply(r);
            }
        }
        self.grpc_status = grpc_status(resp.headers());
        if let (Some(span), Some(cluster)) = (&self.span, self.cluster_name()) {
            span.set_attributes([Attribute::new("ferry.cluster", cluster.to_owned())]);
        }
        let status = resp.status().as_u16();
        let (parts, body) = resp.into_parts();
        Response::from_parts(
            parts,
            Observed {
                inner: body,
                exchange: Some(self),
                status,
                bytes: 0,
            },
        )
    }

    /// The response is finished (or abandoned): record everything.
    fn finish(self, status: u16, bytes: u64, complete: bool) {
        let duration = self.started.elapsed();
        if let Some(r) = &self.route {
            r.record(status, duration);
            if let Some(code) = self.grpc_status {
                r.record_grpc(code);
            }
        }
        if !complete && let Some(l) = &self.listener {
            l.incomplete.inc();
        }
        let reason = self.reason.map(Reason::name);
        if let Some(span) = &self.span {
            if let Some(code) = self.grpc_status {
                span.set_attributes([Attribute::new("rpc.grpc.status_code", i64::from(code))]);
            }
            span.end(status, reason.or((!complete).then_some("incomplete")));
        }
        if let (Some(log), Some(fields)) = (&self.telemetry.access_log, &self.log) {
            let mut flags = String::new();
            if let Some(r) = self.reason {
                flags.push_str(r.flag());
            }
            if !complete {
                if !flags.is_empty() {
                    flags.push(',');
                }
                flags.push_str("DC");
            }
            if flags.is_empty() {
                flags.push('-');
            }
            fn text(v: &Option<HeaderValue>) -> Option<&str> {
                v.as_ref().and_then(|v| v.to_str().ok())
            }
            let path = fields
                .uri
                .path_and_query()
                .map_or_else(|| fields.uri.path(), |pq| pq.as_str());
            log.log(&Entry {
                start_time: rfc3339_millis(fields.start),
                duration_ms: millis(duration),
                upstream_service_time_ms: (self.upstream.attempts > 0)
                    .then(|| millis(self.upstream.total())),
                listener: self.listener_name().unwrap_or_default(),
                route: self.route_name(),
                upstream_cluster: self.cluster_name(),
                upstream_host: self.upstream.endpoint.map(|a| a.to_string()),
                attempts: self.upstream.attempts,
                method: fields.method.as_str(),
                authority: text(&fields.authority).unwrap_or_default(),
                path,
                protocol: protocol_name(fields.version),
                response_code: status,
                grpc_status: self.grpc_status,
                response_flags: flags,
                bytes_sent: bytes,
                downstream_remote_address: self.peer.to_string(),
                x_forwarded_for: text(&fields.forwarded_for),
                user_agent: text(&fields.user_agent),
                request_id: text(&fields.request_id).unwrap_or_default(),
                trace_id: self.span.as_ref().map(ServerSpan::trace_id),
            });
        }
    }
}

fn millis(d: Duration) -> f64 {
    (d.as_secs_f64() * 1e6).round() / 1e3
}

fn protocol_name(v: Version) -> &'static str {
    match v {
        Version::HTTP_09 => "HTTP/0.9",
        Version::HTTP_10 => "HTTP/1.0",
        Version::HTTP_2 => "HTTP/2",
        Version::HTTP_3 => "HTTP/3",
        _ => "HTTP/1.1",
    }
}

fn protocol_version(v: Version) -> &'static str {
    match v {
        Version::HTTP_10 => "1.0",
        Version::HTTP_2 => "2",
        _ => "1.1",
    }
}

fn grpc_status(headers: &HeaderMap) -> Option<u8> {
    headers
        .get(&GRPC_STATUS)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
}

/// A downstream response body that records its exchange when it finishes.
#[derive(Debug)]
pub(crate) struct Observed {
    inner: ProxyBody,
    exchange: Option<Exchange>,
    status: u16,
    bytes: u64,
}

impl Observed {
    fn finish(&mut self, complete: bool) {
        if let Some(ex) = self.exchange.take() {
            ex.finish(self.status, self.bytes, complete);
        }
    }
}

impl Body for Observed {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.inner).poll_frame(cx);
        match &polled {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.bytes += data.len() as u64;
                } else if let (Some(trailers), Some(ex)) =
                    (frame.trailers_ref(), this.exchange.as_mut())
                {
                    ex.grpc_status = grpc_status(trailers).or(ex.grpc_status);
                }
                if this.inner.is_end_stream() {
                    this.finish(true);
                }
            }
            Poll::Ready(None) => this.finish(true),
            Poll::Ready(Some(Err(_))) => this.finish(false),
            Poll::Pending => {}
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for Observed {
    fn drop(&mut self) {
        // hyper may never poll a body that is already at its end (HEAD,
        // empty replies); anything else dropped early did not complete.
        let complete = self.inner.is_end_stream();
        self.finish(complete);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_time_counts_the_attempt_in_flight() {
        let mut log = UpstreamLog::default();
        log.begin();
        std::thread::sleep(Duration::from_millis(5));
        assert!(log.total() >= Duration::from_millis(5));
        let spent = log.end();
        assert!(spent >= Duration::from_millis(5));
        assert_eq!(log.total(), log.wait);
        assert!(log.in_flight.is_none());
    }

    #[test]
    fn reads_grpc_status() {
        let mut h = HeaderMap::new();
        assert_eq!(grpc_status(&h), None);
        h.insert("grpc-status", HeaderValue::from_static("14"));
        assert_eq!(grpc_status(&h), Some(14));
    }

    #[test]
    fn millis_keep_microseconds() {
        assert!((millis(Duration::from_micros(1234)) - 1.234).abs() < 1e-9);
    }
}
