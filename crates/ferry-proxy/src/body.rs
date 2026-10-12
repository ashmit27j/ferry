//! Body types on both sides of the proxy.
//!
//! Both directions carry an idle timer (the route's `timeouts.idle`): if a body
//! produces nothing for that long, it fails, which resets the stream. The timer
//! is only armed when a poll actually has to wait, so bodies that are ready
//! immediately never touch the timer wheel.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use ferry_lb::RequestGuard;
use http_body_util::combinators::BoxBody;
use http_body_util::{Either, Empty};
use hyper::body::{Body, Frame, Incoming, SizeHint};
use tokio::time::Sleep;

/// Boxed error for bodies Ferry wraps.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Body of every response Ferry produces.
pub type ProxyBody = BoxBody<Bytes, BoxError>;

/// Body of requests sent upstream: the downstream body, or nothing when a
/// bodiless request is replayed on another endpoint.
pub(crate) type UpstreamBody = Either<RequestBody, Empty<Bytes>>;

/// The downstream request body was larger than the listener allows.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("request body exceeds {limit} bytes")]
pub(crate) struct BodyTooLarge {
    pub(crate) limit: u64,
}

/// A body produced nothing for longer than the route's idle timeout.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("body idle for more than {0:?}")]
pub(crate) struct IdleTimeout(pub(crate) Duration);

/// An idle timer armed lazily on the first `Pending` poll.
#[derive(Debug)]
struct IdleTimer {
    after: Duration,
    sleep: Option<Pin<Box<Sleep>>>,
}

impl IdleTimer {
    fn new(after: Duration) -> Self {
        Self { after, sleep: None }
    }

    /// Call when the inner body returned `Pending`. Returns true if the timer
    /// has expired.
    fn expired(&mut self, cx: &mut Context<'_>) -> bool {
        let after = self.after;
        let sleep = self
            .sleep
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(after)));
        sleep.as_mut().poll(cx).is_ready()
    }

    /// Call when the inner body made progress.
    fn reset(&mut self) {
        if let Some(sleep) = self.sleep.as_mut() {
            sleep
                .as_mut()
                .reset(tokio::time::Instant::now() + self.after);
        }
    }
}

/// A downstream request body on its way upstream. Fails with
/// [`BodyTooLarge`] past the listener's size limit and with [`IdleTimeout`]
/// when the client stalls.
#[derive(Debug)]
pub(crate) struct RequestBody {
    inner: Incoming,
    limit: Option<u64>,
    seen: u64,
    idle: IdleTimer,
}

impl RequestBody {
    pub(crate) fn new(inner: Incoming, limit: Option<u64>, idle: Duration) -> Self {
        Self {
            inner,
            limit,
            seen: 0,
            idle: IdleTimer::new(idle),
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
                this.idle.reset();
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
            Poll::Pending => {
                if this.idle.expired(cx) {
                    Poll::Ready(Some(Err(Box::new(IdleTimeout(this.idle.after)))))
                } else {
                    Poll::Pending
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// True if `err` or any error it wraps is a `T`.
pub(crate) fn caused_by<T: std::error::Error + 'static>(
    err: &(dyn std::error::Error + 'static),
) -> bool {
    let mut cur = Some(err);
    while let Some(e) = cur {
        if e.is::<T>() {
            return true;
        }
        cur = e.source();
    }
    false
}

/// An upstream response body that holds the endpoint's in-flight guard until
/// the body is finished or dropped, so least-request sees the whole exchange,
/// and fails with [`IdleTimeout`] when the upstream stalls.
pub(crate) struct Tracked {
    inner: Incoming,
    idle: IdleTimer,
    _guard: RequestGuard,
}

impl Tracked {
    pub(crate) fn new(inner: Incoming, guard: RequestGuard, idle: Duration) -> Self {
        Self {
            inner,
            idle: IdleTimer::new(idle),
            _guard: guard,
        }
    }
}

impl Body for Tracked {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                this.idle.reset();
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(Box::new(e)))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => {
                if this.idle.expired(cx) {
                    Poll::Ready(Some(Err(Box::new(IdleTimeout(this.idle.after)))))
                } else {
                    Poll::Pending
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}
