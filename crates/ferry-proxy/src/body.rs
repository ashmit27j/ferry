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
pub(crate) type UpstreamBody = Either<Incoming, Empty<Bytes>>;

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
