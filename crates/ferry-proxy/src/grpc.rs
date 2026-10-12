//! gRPC awareness: deadlines and status-coded local replies.
//!
//! Ferry proxies gRPC as ordinary HTTP/2, but two things need gRPC semantics:
//! the client's `grpc-timeout` caps how long Ferry waits, and replies Ferry
//! generates itself (no route, no healthy upstream, timeout) must be valid gRPC
//! responses ("trailers-only": HTTP 200 with `grpc-status`), or clients report a
//! protocol error instead of the real cause.

use std::time::Duration;

use bytes::Bytes;
use http::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use http::{Response, StatusCode};
use http_body_util::{BodyExt, Empty};

use crate::body::ProxyBody;

/// gRPC status codes Ferry generates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Code {
    Unknown = 2,
    DeadlineExceeded = 4,
    PermissionDenied = 7,
    ResourceExhausted = 8,
    Unimplemented = 12,
    Internal = 13,
    Unavailable = 14,
    Unauthenticated = 16,
}

/// True for gRPC requests (`content-type: application/grpc[+proto|...]`).
pub(crate) fn is_grpc(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| {
            ct.strip_prefix("application/grpc").is_some_and(|rest| {
                rest.is_empty() || rest.starts_with('+') || rest.starts_with(';')
            })
        })
}

/// Parses a `grpc-timeout` value: up to 8 digits and a unit (`H`, `M`, `S`,
/// `m`, `u`, `n`), as in the gRPC HTTP/2 spec.
pub(crate) fn parse_timeout(value: &str) -> Option<Duration> {
    let (digits, unit) = value.split_at(value.len().checked_sub(1)?);
    if digits.is_empty() || digits.len() > 8 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u64 = digits.parse().ok()?;
    Some(match unit {
        "H" => Duration::from_secs(n.checked_mul(3600)?),
        "M" => Duration::from_secs(n.checked_mul(60)?),
        "S" => Duration::from_secs(n),
        "m" => Duration::from_millis(n),
        "u" => Duration::from_micros(n),
        "n" => Duration::from_nanos(n),
        _ => return None,
    })
}

/// The client's deadline from `grpc-timeout`, if any.
pub(crate) fn deadline(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get("grpc-timeout")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_timeout)
}

/// The gRPC code for an HTTP status Ferry would otherwise send, following the
/// gRPC spec's HTTP-to-gRPC mapping.
pub(crate) fn code_for(status: StatusCode) -> Code {
    match status.as_u16() {
        400 => Code::Internal,
        401 => Code::Unauthenticated,
        403 => Code::PermissionDenied,
        404 => Code::Unimplemented,
        413 | 431 => Code::ResourceExhausted,
        429 | 502 | 503 => Code::Unavailable,
        504 => Code::DeadlineExceeded,
        500 => Code::Internal,
        _ => Code::Unknown,
    }
}

/// A trailers-only gRPC response carrying `code` and `message`.
pub(crate) fn reply(code: Code, message: &'static str) -> Response<ProxyBody> {
    let mut resp = Response::new(
        Empty::<Bytes>::new()
            .map_err(|never| match never {})
            .boxed(),
    );
    let h = resp.headers_mut();
    h.insert(CONTENT_TYPE, HeaderValue::from_static("application/grpc"));
    h.insert("grpc-status", HeaderValue::from(code as u16));
    h.insert("grpc-message", HeaderValue::from_static(message));
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_grpc_content_types() {
        let mut h = HeaderMap::new();
        for (ct, want) in [
            ("application/grpc", true),
            ("application/grpc+proto", true),
            ("application/grpc;charset=utf-8", true),
            ("application/grpc-web", false),
            ("application/json", false),
        ] {
            h.insert(CONTENT_TYPE, HeaderValue::from_static(ct));
            assert_eq!(is_grpc(&h), want, "{ct}");
        }
        assert!(!is_grpc(&HeaderMap::new()));
    }

    #[test]
    fn parses_timeout_units() {
        assert_eq!(parse_timeout("1H"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_timeout("2M"), Some(Duration::from_secs(120)));
        assert_eq!(parse_timeout("3S"), Some(Duration::from_secs(3)));
        assert_eq!(parse_timeout("250m"), Some(Duration::from_millis(250)));
        assert_eq!(parse_timeout("7u"), Some(Duration::from_micros(7)));
        assert_eq!(
            parse_timeout("99999999n"),
            Some(Duration::from_nanos(99_999_999))
        );
        for bad in ["", "m", "123456789S", "1x", "-1S", "1.5S", "S1"] {
            assert_eq!(parse_timeout(bad), None, "{bad}");
        }
    }

    #[test]
    fn maps_http_statuses_to_grpc_codes() {
        assert_eq!(code_for(StatusCode::NOT_FOUND), Code::Unimplemented);
        assert_eq!(code_for(StatusCode::SERVICE_UNAVAILABLE), Code::Unavailable);
        assert_eq!(
            code_for(StatusCode::GATEWAY_TIMEOUT),
            Code::DeadlineExceeded
        );
        assert_eq!(code_for(StatusCode::TOO_MANY_REQUESTS), Code::Unavailable);
        assert_eq!(code_for(StatusCode::IM_A_TEAPOT), Code::Unknown);
    }

    #[test]
    fn reply_is_trailers_only() {
        let r = reply(Code::DeadlineExceeded, "upstream request timeout");
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers()["grpc-status"], "4");
        assert_eq!(r.headers()["content-type"], "application/grpc");
    }
}
