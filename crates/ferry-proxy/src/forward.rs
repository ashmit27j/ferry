//! Forwarding one request upstream and turning failures into responses.
//!
//! Per request: route → redirect or request filters → pick an endpoint →
//! check out a connection → send. A request that provably never reached an
//! upstream (connect failed, or hyper handed it back unsent) is retried on a
//! different endpoint, as is an idempotent bodiless request whose connection
//! broke before any response headers (ADR 0011). At most [`MAX_ATTEMPTS`]
//! endpoints are tried. Configurable retry policies and budgets arrive in M2.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use ferry_config::{Filter, PathMatch, RedirectTarget, UpstreamProtocol};
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::uri::{Authority, PathAndQuery, Scheme};
use http::{Method, Request, Response, StatusCode, Uri, Version};
use http_body_util::{BodyExt, Either, Empty, Full};
use hyper::body::{Body, Incoming};
use tracing::{debug, warn};

use crate::body::{ProxyBody, RequestBody, Tracked, UpstreamBody, is_body_too_large};
use crate::router::{RequestInfo, normalize_host};
use crate::state::{ClusterRuntime, Runtime, hash_key, set_cookie};

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
    /// Client address.
    pub(crate) peer: SocketAddr,
}

/// Handles one downstream request.
pub(crate) async fn handle(
    runtime: Arc<Runtime>,
    from: Downstream,
    req: Request<Incoming>,
) -> Result<Response<ProxyBody>, Infallible> {
    Ok(proxy(&runtime, from, req).await)
}

async fn proxy(runtime: &Runtime, from: Downstream, req: Request<Incoming>) -> Response<ProxyBody> {
    let Some(listener) = runtime
        .listeners
        .get(from.listener)
        .and_then(Option::as_ref)
    else {
        return local_reply(StatusCode::SERVICE_UNAVAILABLE, "listener removed");
    };
    let router = &listener.router;
    let body_limit = listener.config.limits().max_request_body_bytes();
    if let Some(limit) = body_limit
        && content_length(req.headers()).is_some_and(|len| len > limit)
    {
        return local_reply(StatusCode::PAYLOAD_TOO_LARGE, "request body too large");
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
        return local_reply(StatusCode::NOT_FOUND, "no route");
    };
    let rule = decision.rule;

    if let Some(redirect) = rule.redirect() {
        let host_only = normalize_host(host);
        let target = RedirectTarget {
            scheme: "http",
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
        return local_reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            "all backends have zero weight",
        );
    };
    let Some(cluster) = runtime.cluster(cluster_id) else {
        return local_reply(StatusCode::INTERNAL_SERVER_ERROR, "unknown cluster");
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
                        None => return local_reply(StatusCode::BAD_REQUEST, "bad rewritten path"),
                    }
                }
            }
            _ => {}
        }
    }

    let (hash, new_cookie) = hash_key(cluster.config.hash_policy(), &parts.headers, from.peer);
    let body = RequestBody::new(body, body_limit);
    let upstream = match UpstreamRequest::new(parts, path_and_query, cluster, body) {
        Ok(u) => u,
        Err(e) => {
            warn!(error = %e, "cannot build upstream request");
            return local_reply(StatusCode::BAD_REQUEST, "bad request");
        }
    };

    let mut resp = match send_with_retries(cluster, upstream, hash).await {
        Ok(r) => r,
        Err(failure) => {
            warn!(
                route = route_name,
                cluster = cluster.config.name(),
                error = %failure,
                "upstream request failed",
            );
            return failure.into_response();
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
    resp
}

/// A request ready for any endpoint of one cluster, plus what is needed to
/// rebuild it if it must be replayed after being sent.
struct UpstreamRequest {
    pending: Option<Request<UpstreamBody>>,
    replay: Option<(Method, Uri, Version, HeaderMap)>,
}

impl UpstreamRequest {
    fn new(
        mut parts: http::request::Parts,
        path_and_query: PathAndQuery,
        cluster: &ClusterRuntime,
        body: RequestBody,
    ) -> Result<Self, http::Error> {
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
                b.path_and_query(path_and_query).build()?
            }
            _ => {
                parts.version = Version::HTTP_11;
                Uri::from(path_and_query)
            }
        };
        let replay = (is_idempotent(&parts.method) && body.is_end_stream()).then(|| {
            (
                parts.method.clone(),
                parts.uri.clone(),
                parts.version,
                parts.headers.clone(),
            )
        });
        Ok(Self {
            pending: Some(Request::from_parts(parts, Either::Left(body))),
            replay,
        })
    }

    /// Restores a request that may have been sent, if that is safe.
    fn replay(&self) -> Option<Request<UpstreamBody>> {
        let (method, uri, version, headers) = self.replay.as_ref()?;
        let mut req = Request::new(Either::Right(Empty::new()));
        *req.method_mut() = method.clone();
        *req.uri_mut() = uri.clone();
        *req.version_mut() = *version;
        *req.headers_mut() = headers.clone();
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
}

impl Failure {
    fn into_response(self) -> Response<ProxyBody> {
        match self {
            Self::NoHealthyUpstream => {
                local_reply(StatusCode::SERVICE_UNAVAILABLE, "no healthy upstream")
            }
            Self::Send(e) if is_body_too_large(&e) => {
                local_reply(StatusCode::PAYLOAD_TOO_LARGE, "request body too large")
            }
            Self::Pool(_) | Self::Send(_) => {
                local_reply(StatusCode::BAD_GATEWAY, "upstream unavailable")
            }
        }
    }
}

async fn send_with_retries(
    cluster: &ClusterRuntime,
    mut req: UpstreamRequest,
    hash: Option<u64>,
) -> Result<Response<ProxyBody>, Failure> {
    let mut tried = [usize::MAX; MAX_ATTEMPTS];
    let mut last = Failure::NoHealthyUpstream;
    for attempt in 0..MAX_ATTEMPTS {
        let Some(index) = cluster.balancer.pick(hash, &tried[..attempt]) else {
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
        let guard = host.state.start_request();
        let conn = match pool.checkout(cluster.config.connect_timeout()).await {
            Ok(c) => c,
            Err(e) => {
                guard.observe_failure();
                debug!(endpoint = %host.state.address(), attempt, error = %e, "connect failed; trying another endpoint");
                last = Failure::Pool(e);
                continue;
            }
        };
        let Some(request) = req.pending.take().or_else(|| req.replay()) else {
            break;
        };
        match conn.send(request).await {
            Ok(resp) => {
                guard.observe_response();
                let (mut parts, body) = resp.into_parts();
                strip_hop_by_hop(&mut parts.headers);
                return Ok(Response::from_parts(
                    parts,
                    Tracked::new(body, guard).boxed(),
                ));
            }
            Err(mut e) => {
                guard.observe_failure();
                let unsent = e.take_message();
                let error = e.into_error();
                let retry = unsent.is_some() || req.replay.is_some();
                debug!(endpoint = %host.state.address(), attempt, error = %error, retry, "upstream send failed");
                if !retry {
                    return Err(Failure::Send(error));
                }
                req.pending = unsent;
                last = Failure::Send(error);
            }
        }
    }
    Err(last)
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
