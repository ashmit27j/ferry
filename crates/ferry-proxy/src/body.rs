//! Body types on both sides of the proxy.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use ferry_lb::RequestGuard;
use http_body_util::combinators::BoxBody;
use http_body_util::{Either, Empty};
use hyper::body::{Body, Frame, Incoming, SizeHint};

/// Body of every response Ferry produces.
pub type ProxyBody = BoxBody<Bytes, hyper::Error>;

/// Body of requests sent upstream: the downstream body, or nothing when a
/// bodiless request is replayed on another endpoint.
pub(crate) type UpstreamBody = Either<RequestBody, Empty<Bytes>>;

/// Boxed error for bodies Ferry wraps.
pub(crate) type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The downstream request body was larger than the listener allows.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("request body exceeds {limit} bytes")]
pub(crate) struct BodyTooLarge {
    pub(crate) limit: u64,
}

/// A downstream request body on its way upstream, cut off with
/// [`BodyTooLarge`] once it passes the listener's size limit.
#[derive(Debug)]
pub(crate) struct RequestBody {
    inner: Incoming,
    limit: Option<u64>,
    seen: u64,
}

impl RequestBody {
    pub(crate) fn new(inner: Incoming, limit: Option<u64>) -> Self {
        Self {
            inner,
            limit,
            seen: 0,
        }
    }
}

impl Body for RequestBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let (Some(limit), Some(data)) = (this.limit, frame.data_ref()) {
                    this.seen = this.seen.saturating_add(data.len() as u64);
                    if this.seen > limit {
                        return Poll::Ready(Some(Err(Box::new(BodyTooLarge { limit }))));
                    }
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(Box::new(e)))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// True if `err` or any error it wraps is a [`BodyTooLarge`].
pub(crate) fn is_body_too_large(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut cur = Some(err);
    while let Some(e) = cur {
        if e.is::<BodyTooLarge>() {
            return true;
        }
        cur = e.source();
    }
    false
}

/// An upstream response body that holds the endpoint's in-flight guard until
/// the body is finished or dropped, so least-request sees the whole exchange.
pub(crate) struct Tracked {
    inner: Incoming,
    _guard: RequestGuard,
}

impl Tracked {
    pub(crate) fn new(inner: Incoming, guard: RequestGuard) -> Self {
        Self {
            inner,
            _guard: guard,
        }
    }
}

impl Body for Tracked {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, hyper::Error>>> {
        Pin::new(&mut self.get_mut().inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}
