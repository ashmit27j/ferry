//! Forwarding one request upstream and turning failures into responses.
//!
//! Per request: route → redirect or request filters → pick an endpoint →
//! check out a connection → send. A request that provably never reached an
//! upstream (connect failed, or hyper handed it back unsent) is retried on a
//! different endpoint, as is an idempotent bodiless request whose connection
//! broke before any response headers (ADR 0011). At most [`MAX_ATTEMPTS`]
//! endpoints are tried. Rules with a retry policy also retry configured
//! statuses, resets and attempt timeouts, within the cluster's retry budget
//! (ADR 0019).

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use ferry_config::{Filter, PathMatch, RedirectTarget, RetryPolicy, UpstreamProtocol};
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::uri::{Authority, PathAndQuery, Scheme};
use http::{Method, Request, Response, StatusCode, Uri, Version};
use http_body_util::{BodyExt, Either, Full};
use hyper::body::{Body, Incoming};
use tracing::{debug, warn};

use crate::body::{
    BodyTooLarge, BoxError, IdleTimeout, ProxyBody, RequestBody, Tracked, UpstreamBody, caused_by,
};
use crate::grpc;
use crate::router::{RequestInfo, normalize_host};
use crate::state::{ClusterRuntime, Runtime, hash_key, set_cookie};
use ferry_lb::pool::PoolError;
use ferry_lb::{Outcome, Permit};

/// Most endpoints one request is sent to.
pub const MAX_ATTEMPTS: usize = 3;

/// Hop-by-hop headers (RFC 9110 §7.6.1) that must not be forwarded.
static HOP_BY_HOP: [HeaderName; 7] = [
    header::CONNECTION,
    HeaderName::from_static("proxy-connection"),
    HeaderName::from_static("keep-alive"),
    header::TE,
    header::TRAILER,
    header::TRANSFER_ENCODING,
    header::UPGRADE,
];

/// Where a request arrived.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Downstream {
    /// Index of the bound listener.
    pub(crate) listener: usize,
    /// Port of that listener, for redirects.
    pub(crate) port: u16,
    /// Client address: from the PROXY header when the listener expects one.
    pub(crate) peer: SocketAddr,
    /// `http` or `https`, for `X-Forwarded-Proto` and redirects.
    pub(crate) scheme: &'static str,
}

/// Request ID header: kept when the client sends a usable one, otherwise
/// generated; echoed on the response.
pub const REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");
static X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");
static X_FORWARDED_PROTO: HeaderName = HeaderName::from_static("x-forwarded-proto");
/// Set on replies refused by a route's rate limit, like Envoy's `x-envoy-ratelimited`.
pub const RATE_LIMITED: HeaderName = HeaderName::from_static("x-ferry-ratelimited");
/// Set on replies refused by a circuit breaker, like Envoy's `x-envoy-overloaded`.
pub const OVERLOADED: HeaderName = HeaderName::from_static("x-ferry-overloaded");
/// Longest client-supplied request ID kept.
const MAX_REQUEST_ID_LEN: usize = 128;

/// Handles one downstream request.
pub(crate) async fn handle(
    runtime: Arc<Runtime>,
    from: Downstream,
    req: Request<Incoming>,
) -> Result<Response<ProxyBody>, Infallible> {
    Ok(proxy(&runtime, from, req).await)
}

async fn proxy(runtime: &Runtime, from: Downstream, req: Request<Incoming>) -> Response<ProxyBody> {
    let grpc = grpc::is_grpc(req.headers());
    let fail = |status, message| error_reply(grpc, status, message);
    let Some(listener) = runtime
        .listeners
        .get(from.listener)
        .and_then(Option::as_ref)
    else {
        return fail(StatusCode::SERVICE_UNAVAILABLE, "listener removed");
    };
    let router = &listener.router;
    let body_limit = listener.config.limits().max_request_body_bytes();
    if let Some(limit) = body_limit
        && content_length(req.headers()).is_some_and(|len| len > limit)
    {
        return fail(StatusCode::PAYLOAD_TOO_LARGE, "request body too large");
    }
    let host = request_host(&req).unwrap_or_default();
    let info = RequestInfo {
        host,
        method: req.method(),
        path: req.uri().path(),
        query: req.uri().query(),
        headers: req.headers(),
    };
    let Some(decision) = router.route(&info) else {
        debug!(host, path = req.uri().path(), "no route");
        return fail(StatusCode::NOT_FOUND, "no route");
    };
    let rule = decision.rule;

    if let Some(limiter) = listener
        .rate_limiters
        .get(decision.route_index)
        .and_then(Option::as_ref)
        && let Err(limited) = limiter.check(from.peer.ip(), req.headers())
    {
        let mut resp = fail(StatusCode::TOO_MANY_REQUESTS, "rate limited");
        let secs =
            limited.retry_after.as_secs() + u64::from(limited.retry_after.subsec_nanos() > 0);
        resp.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from(secs.max(1)));
        resp.headers_mut()
            .insert(RATE_LIMITED, HeaderValue::from_static("true"));
        return resp;
    }

    if let Some(redirect) = rule.redirect() {
        let host_only = normalize_host(host);
        let target = RedirectTarget {
            scheme: from.scheme,
            host: &host_only,
            listener_port: from.port,
            path: req.uri().path(),
            query: req.uri().query(),
        };
        let location = redirect.location(&target, decision.matched.path());
        let status = StatusCode::from_u16(redirect.status()).unwrap_or(StatusCode::FOUND);
        let mut resp = local_reply(status, "");
        match HeaderValue::from_str(&location) {
            Ok(v) => {
                resp.headers_mut().insert(header::LOCATION, v);
            }
            Err(_) => return local_reply(StatusCode::BAD_REQUEST, "bad redirect target"),
        }
        return resp;
    }

    let Some(cluster_id) = decision.cluster else {
        return fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            "all backends have zero weight",
        );
    };
    let Some(cluster) = runtime.cluster(cluster_id) else {
        return fail(StatusCode::INTERNAL_SERVER_ERROR, "unknown cluster");
    };
    let route_name = decision.route;
    let matched_path = decision.matched.path();
    let filters = rule.filters();

    let (mut parts, body) = req.into_parts();
    let wants_trailers = parts.headers.get_all(header::TE).iter().any(|v| {
        v.to_str()
            .is_ok_and(|s| s.split(',').any(|t| t.trim() == "trailers"))
    });
    strip_hop_by_hop(&mut parts.headers);
    let request_id = set_edge_headers(&mut parts.headers, from);
    if wants_trailers {
        // The one TE value HTTP/2 allows, and the one gRPC requires.
        parts
            .headers
            .insert(header::TE, HeaderValue::from_static("trailers"));
    }
    // HTTP/2 carries the host in :authority; make it a Host header so filters
    // and HTTP/1.1 upstreams see it.
    if !parts.headers.contains_key(header::HOST)
        && let Some(value) = parts
            .uri
            .authority()
            .and_then(|a| HeaderValue::from_str(a.as_str()).ok())
    {
        parts.headers.insert(header::HOST, value);
    }
    let mut path_and_query = parts
        .uri
        .path_and_query()
        .cloned()
        .unwrap_or_else(|| PathAndQuery::from_static("/"));
    for filter in filters {
        match filter {
            Filter::RequestHeaderModifier(m) => m.apply(&mut parts.headers),
            Filter::UrlRewrite(rw) => {
                if let Some(h) = rw.hostname() {
                    parts.headers.insert(header::HOST, h.clone());
                }
                if let Some(p) = rw.path() {
                    match rewrite_path(&path_and_query, p, matched_path) {
                        Some(pq) => path_and_query = pq,
                        None => return fail(StatusCode::BAD_REQUEST, "bad rewritten path"),
                    }
                }
            }
            _ => {}
        }
    }

    let timeouts = rule.timeouts();
    // The client's gRPC deadline caps the route's request timeout.
    let deadline = match (
        timeouts.request(),
        grpc.then(|| grpc::deadline(&parts.headers)).flatten(),
    ) {
        (Some(route), Some(client)) => Some(route.min(client)),
        (route, client) => route.or(client),
    };
    let (hash, new_cookie) = hash_key(cluster.config.hash_policy(), &parts.headers, from.peer);
    let body = RequestBody::new(body, body_limit, timeouts.idle());
    let policy = rule.retry();
    let attempts = Attempts {
        cluster,
        hash,
        per_try: timeouts.backend_request(),
        idle: timeouts.idle(),
        policy,
    };
    let run = async {
        let upstream = UpstreamRequest::new(parts, path_and_query, cluster, body, policy).await?;
        attempts.run(upstream).await
    };
    let result = match deadline {
        Some(d) => tokio::time::timeout(d, run)
            .await
            .unwrap_or(Err(Failure::Timeout)),
        None => run.await,
    };
    let mut resp = match result {
        Ok(r) => r,
        Err(failure) => {
            warn!(
                route = route_name,
                cluster = cluster.config.name(),
                error = %failure,
                "upstream request failed",
            );
            return failure.into_response(grpc);
        }
    };
    for filter in filters {
        if let Filter::ResponseHeaderModifier(m) = filter {
            m.apply(resp.headers_mut());
        }
    }
    if let Some(cookie) = new_cookie
        .as_deref()
        .and_then(|v| set_cookie(cluster.config.hash_policy(), v))
        .and_then(|c| HeaderValue::from_str(&c).ok())
    {
        resp.headers_mut().append(header::SET_COOKIE, cookie);
    }
    resp.headers_mut().insert(REQUEST_ID, request_id);
    resp
}

/// Sets `X-Forwarded-For` (client IP appended, as Envoy does at the edge),
/// `X-Forwarded-Proto` (overwritten) and `X-Request-Id` (kept or generated).
/// Returns the request ID.
fn set_edge_headers(headers: &mut HeaderMap, from: Downstream) -> HeaderValue {
    let ip = from.peer.ip().to_string();
    let xff = match headers
        .get_all(&X_FORWARDED_FOR)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .fold(String::new(), |mut acc, v| {
            if !acc.is_empty() {
                acc.push_str(", ");
            }
            acc.push_str(v);
            acc
        }) {
        prior if prior.is_empty() => ip,
        prior => format!("{prior}, {ip}"),
    };
    if let Ok(v) = HeaderValue::from_str(&xff) {
        headers.insert(X_FORWARDED_FOR.clone(), v);
    }
    headers.insert(
        X_FORWARDED_PROTO.clone(),
        HeaderValue::from_static(from.scheme),
    );
    let id = headers
        .get(&REQUEST_ID)
        .filter(|v| {
            !v.is_empty()
                && v.len() <= MAX_REQUEST_ID_LEN
                && v.as_bytes().iter().all(|b| b.is_ascii_graphic())
        })
        .cloned()
        .unwrap_or_else(new_request_id);
    headers.insert(REQUEST_ID, id.clone());
    id
}

/// A random UUID (version 4).
fn new_request_id() -> HeaderValue {
    let n = fastrand::u128(..);
    let n = (n & !(0xf << 76)) | (0x4 << 76); // version 4
    let n = (n & !(0x3 << 62)) | (0x2 << 62); // RFC 4122 variant
    let s = format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        n >> 96,
        (n >> 80) & 0xffff,
        (n >> 64) & 0xffff,
        (n >> 48) & 0xffff,
        n & 0xffff_ffff_ffff
    );
    HeaderValue::from_str(&s).unwrap_or(HeaderValue::from_static("invalid"))
}

/// Request bodies up to this size are buffered when a retry policy applies,
/// so a policy retry can resend them. Larger or unsized bodies are streamed
/// and only get the always-safe retries.
pub const MAX_REPLAY_BODY: u64 = 64 * 1024;

/// Most retries a policy may ask for, plus the first attempt.
const MAX_TOTAL_ATTEMPTS: usize = 11;

/// A request ready for any endpoint of one cluster, plus what is needed to
/// rebuild it if it must be sent again after being sent once.
struct UpstreamRequest {
    pending: Option<Request<UpstreamBody>>,
    replay: Option<Replay>,
}

struct Replay {
    method: Method,
    uri: Uri,
    version: Version,
    headers: HeaderMap,
    body: Bytes,
    /// Idempotent and bodiless: safe to resend after a connection reset even
    /// without a retry policy (ADR 0011).
    always_safe: bool,
}

impl UpstreamRequest {
    async fn new(
        mut parts: http::request::Parts,
        path_and_query: PathAndQuery,
        cluster: &ClusterRuntime,
        body: RequestBody,
        policy: Option<&RetryPolicy>,
    ) -> Result<Self, Failure> {
        parts.uri = match cluster.config.protocol() {
            UpstreamProtocol::Http2 => {
                // :authority is the client-facing host, as Envoy sends it.
                let authority =
                    parts
                        .headers
                        .remove(header::HOST)
                        .and_then(|h| Authority::try_from(h.as_bytes()).ok())
                        .or_else(|| {
                            cluster.balancer.hosts().first().and_then(|h| {
                                Authority::try_from(h.state.address().to_string()).ok()
                            })
                        });
                let mut b = Uri::builder().scheme(Scheme::HTTP);
                if let Some(a) = authority {
                    b = b.authority(a);
                }
                parts.version = Version::HTTP_2;
                b.path_and_query(path_and_query)
                    .build()
                    .map_err(|_| Failure::BadRequest)?
            }
            _ => {
                parts.version = Version::HTTP_11;
                Uri::from(path_and_query)
            }
        };
        let bodiless = body.is_end_stream();
        let small = content_length(&parts.headers).is_some_and(|n| n <= MAX_REPLAY_BODY);
        let (body, buffered) = if !bodiless && policy.is_some() && small {
            let bytes = body
                .collect()
                .await
                .map_err(Failure::RequestBody)?
                .to_bytes();
            (Either::Right(Full::new(bytes.clone())), Some(bytes))
        } else {
            (Either::Left(body), bodiless.then(Bytes::new))
        };
        let always_safe = bodiless && is_idempotent(&parts.method);
        let replay = buffered
            .filter(|_| always_safe || policy.is_some())
            .map(|body| Replay {
                method: parts.method.clone(),
                uri: parts.uri.clone(),
                version: parts.version,
                headers: parts.headers.clone(),
                body,
                always_safe,
            });
        Ok(Self {
            pending: Some(Request::from_parts(parts, body)),
            replay,
        })
    }

    /// Rebuilds the request for another attempt, if it can be resent.
    fn replay(&self) -> Option<Request<UpstreamBody>> {
        let r = self.replay.as_ref()?;
        let mut req = Request::new(Either::Right(Full::new(r.body.clone())));
        *req.method_mut() = r.method.clone();
        *req.uri_mut() = r.uri.clone();
        *req.version_mut() = r.version;
        *req.headers_mut() = r.headers.clone();
        Some(req)
    }
}

/// Why a request got no upstream response.
#[derive(Debug, thiserror::Error)]
enum Failure {
    #[error("no healthy endpoint")]
    NoHealthyUpstream,
    #[error("{0}")]
    Pool(#[from] ferry_lb::pool::PoolError),
    #[error("{0}")]
    Send(hyper::Error),
    #[error("upstream request timeout")]
    Timeout,
    #[error("cannot build upstream request")]
    BadRequest,
    #[error("request body: {0}")]
    RequestBody(BoxError),
    #[error("circuit breaker open")]
    Overflow,
}

impl Failure {
    fn into_response(self, grpc: bool) -> Response<ProxyBody> {
        let (status, message) = match &self {
            Self::NoHealthyUpstream => (StatusCode::SERVICE_UNAVAILABLE, "no healthy upstream"),
            Self::Send(e) if caused_by::<BodyTooLarge>(e) => {
                (StatusCode::PAYLOAD_TOO_LARGE, "request body too large")
            }
            Self::RequestBody(e) if caused_by::<BodyTooLarge>(e.as_ref()) => {
                (StatusCode::PAYLOAD_TOO_LARGE, "request body too large")
            }
            // The client stopped sending its body (Envoy also answers 408).
            Self::Send(e) if caused_by::<IdleTimeout>(e) => {
                (StatusCode::REQUEST_TIMEOUT, "request body timeout")
            }
            Self::RequestBody(e) if caused_by::<IdleTimeout>(e.as_ref()) => {
                (StatusCode::REQUEST_TIMEOUT, "request body timeout")
            }
            Self::RequestBody(_) | Self::BadRequest => (StatusCode::BAD_REQUEST, "bad request"),
            Self::Pool(_) | Self::Send(_) => (StatusCode::BAD_GATEWAY, "upstream unavailable"),
            Self::Timeout => (StatusCode::GATEWAY_TIMEOUT, "upstream request timeout"),
            Self::Overflow => (StatusCode::SERVICE_UNAVAILABLE, "upstream overloaded"),
        };
        let mut resp = error_reply(grpc, status, message);
        if matches!(self, Self::Overflow) {
            resp.headers_mut()
                .insert(OVERLOADED, HeaderValue::from_static("true"));
        }
        resp
    }
}

/// A local error reply: plain text, or a trailers-only gRPC response for gRPC
/// requests so clients see a proper status.
fn error_reply(grpc: bool, status: StatusCode, message: &'static str) -> Response<ProxyBody> {
    if grpc {
        grpc::reply(grpc::code_for(status), message)
    } else {
        local_reply(status, message)
    }
}

/// What the retry loop needs to know about one request.
struct Attempts<'a> {
    cluster: &'a ClusterRuntime,
    hash: Option<u64>,
    per_try: Option<Duration>,
    idle: Duration,
    policy: Option<&'a RetryPolicy>,
}

impl Attempts<'_> {
    /// Whether a policy retry may follow a result with this status. Takes a
    /// retry slot from the circuit breaker (once per request) and one retry
    /// from the budget if so.
    fn policy_allows(
        &self,
        status: u16,
        policy_retries: u32,
        req: &UpstreamRequest,
        retry_slot: &mut Option<Permit>,
    ) -> bool {
        let Some(p) = self.policy else { return false };
        if !(p.codes().contains(&status) && policy_retries < p.attempts() && req.replay.is_some()) {
            return false;
        }
        if retry_slot.is_none() {
            *retry_slot = self.cluster.breakers.retries.try_acquire();
            if retry_slot.is_none() {
                debug!(
                    cluster = self.cluster.config.name(),
                    "retry circuit breaker open"
                );
                return false;
            }
        }
        self.cluster.budget.try_withdraw()
    }

    async fn backoff(&self, retry: u32) {
        let Some(base) = self
            .policy
            .map(RetryPolicy::backoff)
            .filter(|b| !b.is_zero())
        else {
            return;
        };
        // Exponential with full jitter, capped at ten times the base.
        let cap = base
            .saturating_mul(1 << retry.saturating_sub(1).min(10))
            .min(base * 10);
        tokio::time::sleep(cap.mul_f64(fastrand::f64())).await;
    }

    async fn run(&self, mut req: UpstreamRequest) -> Result<Response<ProxyBody>, Failure> {
        let cluster = self.cluster;
        cluster.budget.deposit();
        let policy_max = self.policy.map_or(0, RetryPolicy::attempts);
        let max = (1 + policy_max as usize).clamp(MAX_ATTEMPTS, MAX_TOTAL_ATTEMPTS);
        let mut tried = [usize::MAX; MAX_TOTAL_ATTEMPTS];
        let mut policy_retries = 0u32;
        let mut retry_slot: Option<Permit> = None;
        let mut last = Failure::NoHealthyUpstream;
        for attempt in 0..max {
            let Some(index) = cluster.balancer.pick(self.hash, &tried[..attempt]) else {
                break;
            };
            if let Some(slot) = tried.get_mut(attempt) {
                *slot = index;
            }
            let (Some(host), Some(pool)) = (
                cluster.balancer.hosts().get(index),
                cluster.pools.get(index),
            ) else {
                break;
            };
            let Some(request_slot) = cluster.breakers.requests.try_acquire() else {
                return Err(Failure::Overflow);
            };
            let Some(pending_slot) = cluster.breakers.pending.try_acquire() else {
                return Err(Failure::Overflow);
            };
            let guard = host.state.start_request();
            let checkout = pool
                .checkout(
                    cluster.config.connect_timeout(),
                    Some(&cluster.breakers.connections),
                )
                .await;
            drop(pending_slot);
            let conn = match checkout {
                Ok(c) => c,
                Err(PoolError::Overflow { .. }) => return Err(Failure::Overflow),
                Err(e) => {
                    // Nothing was sent: always safe to try elsewhere.
                    guard.observe_failure();
                    cluster.balancer.record(index, Outcome::GatewayFailure);
                    debug!(endpoint = %host.state.address(), attempt, error = %e, "connect failed; trying another endpoint");
                    last = Failure::Pool(e);
                    continue;
                }
            };
            let Some(request) = req.pending.take().or_else(|| req.replay()) else {
                break;
            };
            let sent = match self.per_try {
                Some(t) => tokio::time::timeout(t, conn.send(request)).await.ok(),
                None => Some(conn.send(request).await),
            };
            match sent {
                Some(Ok(resp)) => {
                    guard.observe_response();
                    cluster
                        .balancer
                        .record(index, Outcome::from_status(resp.status().as_u16()));
                    if attempt + 1 < max
                        && self.policy_allows(
                            resp.status().as_u16(),
                            policy_retries,
                            &req,
                            &mut retry_slot,
                        )
                    {
                        policy_retries += 1;
                        debug!(endpoint = %host.state.address(), status = %resp.status(), attempt, "retrying on status");
                        drop(resp);
                        self.backoff(policy_retries).await;
                        continue;
                    }
                    let (mut parts, body) = resp.into_parts();
                    strip_hop_by_hop(&mut parts.headers);
                    return Ok(Response::from_parts(
                        parts,
                        Tracked::new(body, guard, request_slot, self.idle).boxed(),
                    ));
                }
                Some(Err(mut e)) => {
                    guard.observe_failure();
                    cluster.balancer.record(index, Outcome::GatewayFailure);
                    let unsent = e.take_message();
                    let error = e.into_error();
                    debug!(endpoint = %host.state.address(), attempt, error = %error, unsent = unsent.is_some(), "upstream send failed");
                    if unsent.is_some() {
                        // hyper handed the request back unwritten.
                        req.pending = unsent;
                    } else if req.replay.as_ref().is_some_and(|r| r.always_safe) {
                        // Reset before response headers on an idempotent,
                        // bodiless request: resend without spending budget.
                    } else if attempt + 1 < max
                        && self.policy_allows(502, policy_retries, &req, &mut retry_slot)
                    {
                        policy_retries += 1;
                        self.backoff(policy_retries).await;
                    } else {
                        return Err(Failure::Send(error));
                    }
                    last = Failure::Send(error);
                }
                None => {
                    guard.observe_failure();
                    cluster.balancer.record(index, Outcome::GatewayFailure);
                    debug!(endpoint = %host.state.address(), attempt, "upstream attempt timed out");
                    if attempt + 1 < max
                        && self.policy_allows(504, policy_retries, &req, &mut retry_slot)
                    {
                        policy_retries += 1;
                        self.backoff(policy_retries).await;
                        last = Failure::Timeout;
                        continue;
                    }
                    return Err(Failure::Timeout);
                }
            }
        }
        Err(last)
    }
}

fn is_idempotent(m: &Method) -> bool {
    matches!(
        *m,
        Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE | Method::PUT | Method::DELETE
    )
}

fn rewrite_path(
    pq: &PathAndQuery,
    rewrite: &ferry_config::PathRewrite,
    matched: &PathMatch,
) -> Option<PathAndQuery> {
    let path = rewrite.apply(pq.path(), matched);
    let full = match pq.query() {
        Some(q) => format!("{path}?{q}"),
        None => path,
    };
    PathAndQuery::try_from(full).ok()
}

/// The declared `Content-Length`, if present and valid.
fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .parse()
        .ok()
}

/// The request's host: the `Host` header, or the URI authority for HTTP/2.
fn request_host<B>(req: &Request<B>) -> Option<&str> {
    req.headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .or_else(|| req.uri().authority().map(Authority::as_str))
}

/// Removes hop-by-hop headers, including any named in `Connection`.
pub(crate) fn strip_hop_by_hop(headers: &mut HeaderMap) {
    if headers.contains_key(header::CONNECTION) {
        let named: Vec<HeaderName> = headers
            .get_all(header::CONNECTION)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
            .collect();
        for name in named {
            headers.remove(name);
        }
    }
    for name in &HOP_BY_HOP {
        headers.remove(name);
    }
}

/// A response generated by Ferry itself.
pub(crate) fn local_reply(status: StatusCode, message: &'static str) -> Response<ProxyBody> {
    let mut resp = Response::new(
        Full::new(Bytes::from_static(message.as_bytes()))
            .map_err(|never| match never {})
            .boxed(),
    );
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_static_and_connection_named_headers() {
        let mut h = HeaderMap::new();
        h.insert(
            header::CONNECTION,
            HeaderValue::from_static("keep-alive, X-Secret"),
        );
        h.insert("x-secret", HeaderValue::from_static("1"));
        h.insert("keep-alive", HeaderValue::from_static("timeout=5"));
        h.insert(
            header::TRANSFER_ENCODING,
            HeaderValue::from_static("chunked"),
        );
        h.insert(header::UPGRADE, HeaderValue::from_static("h2c"));
        h.insert("x-keep", HeaderValue::from_static("yes"));
        strip_hop_by_hop(&mut h);
        assert_eq!(h.len(), 1);
        assert!(h.contains_key("x-keep"));
    }

    #[test]
    fn local_reply_sets_status_and_type() {
        let r = local_reply(StatusCode::NOT_FOUND, "no route");
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            r.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/plain; charset=utf-8"
        );
    }

    #[test]
    fn host_falls_back_to_authority() {
        let req = Request::builder()
            .uri("http://h2.example.com/x")
            .body(())
            .unwrap();
        assert_eq!(request_host(&req), Some("h2.example.com"));
        let req = Request::builder()
            .uri("/x")
            .header(header::HOST, "h1.example.com")
            .body(())
            .unwrap();
        assert_eq!(request_host(&req), Some("h1.example.com"));
    }

    fn downstream(ip: &str) -> Downstream {
        Downstream {
            listener: 0,
            port: 80,
            peer: SocketAddr::new(ip.parse().unwrap(), 5000),
            scheme: "https",
        }
    }

    #[test]
    fn edge_headers_append_xff_and_set_proto() {
        let mut h = HeaderMap::new();
        h.append("x-forwarded-for", HeaderValue::from_static("1.1.1.1"));
        h.append("x-forwarded-for", HeaderValue::from_static("2.2.2.2"));
        h.insert("x-forwarded-proto", HeaderValue::from_static("http"));
        set_edge_headers(&mut h, downstream("10.0.0.9"));
        assert_eq!(h["x-forwarded-for"], "1.1.1.1, 2.2.2.2, 10.0.0.9");
        assert_eq!(h["x-forwarded-proto"], "https");
        let mut fresh = HeaderMap::new();
        set_edge_headers(&mut fresh, downstream("::1"));
        assert_eq!(fresh["x-forwarded-for"], "::1");
    }

    #[test]
    fn request_id_is_kept_or_generated() {
        let mut h = HeaderMap::new();
        h.insert("x-request-id", HeaderValue::from_static("abc-123"));
        assert_eq!(set_edge_headers(&mut h, downstream("1.2.3.4")), "abc-123");
        let mut bad = HeaderMap::new();
        bad.insert("x-request-id", HeaderValue::from_static("has space"));
        let id = set_edge_headers(&mut bad, downstream("1.2.3.4"));
        assert_ne!(id, "has space");
        assert_eq!(bad["x-request-id"], id);
    }

    #[test]
    fn generated_ids_are_uuid_v4() {
        for _ in 0..100 {
            let id = new_request_id();
            let s = id.to_str().unwrap();
            assert_eq!(s.len(), 36);
            let parts: Vec<&str> = s.split('-').collect();
            assert_eq!(
                parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
                [8, 4, 4, 4, 12]
            );
            assert!(parts[2].starts_with('4'), "{s}");
            assert!(matches!(&parts[3][..1], "8" | "9" | "a" | "b"), "{s}");
        }
        assert_ne!(new_request_id(), new_request_id());
    }

    #[test]
    fn idempotent_methods() {
        assert!(is_idempotent(&Method::GET));
        assert!(is_idempotent(&Method::DELETE));
        assert!(!is_idempotent(&Method::POST));
        assert!(!is_idempotent(&Method::PATCH));
    }

    #[test]
    fn rewrite_keeps_query() {
        let pq = PathAndQuery::from_static("/old/x?a=1");
        let got = rewrite_path(
            &pq,
            &ferry_config::PathRewrite::ReplacePrefixMatch("/new".into()),
            &PathMatch::Prefix("/old".into()),
        )
        .unwrap();
        assert_eq!(got.as_str(), "/new/x?a=1");
    }
}
