//! Upstream connection pools (ADR 0011).
//!
//! One [`Pool`] per `(endpoint address, protocol)`:
//!
//! - **HTTP/1.1:** idle connections wait in a lock-free queue. A checkout pops
//!   one that is still open and ready, or dials a new one. After a response's
//!   headers arrive, a small task waits for the connection to finish the
//!   response and returns it to the queue, as hyper's own pooled client does.
//! - **h2c:** one shared connection, multiplexed; a checkout clones its sender
//!   and dials a replacement only when it has closed.
//!
//! A checkout connects *before* the request is handed over, so a refused or
//! timed-out connect leaves the request untouched and the caller can try
//! another endpoint. Sending uses hyper's `try_send_request`, which also hands
//! the request back if the connection died before it was written.

use std::collections::{HashMap, HashSet};
use std::error::Error as StdError;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use arc_swap::ArcSwapOption;
use crossbeam_queue::SegQueue;
use http::{Request, Response};
use hyper::body::{Body, Incoming};
use hyper::client::conn::{TrySendError, http1, http2};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tracing::debug;

use crate::breaker::{Limit, Permit};

/// Wire protocol of a pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Protocol {
    /// HTTP/1.1 with keep-alive.
    Http1,
    /// HTTP/2 with prior knowledge over cleartext.
    Http2,
}

/// Pool tuning.
#[derive(Debug, Clone, Copy)]
pub struct PoolSettings {
    /// Idle HTTP/1.1 connections older than this are closed.
    pub idle_timeout: Duration,
    /// Most idle HTTP/1.1 connections kept per endpoint.
    pub max_idle: usize,
    /// Interval of HTTP/2 keep-alive pings, which detect dead connections.
    pub h2_keep_alive: Duration,
}

impl Default for PoolSettings {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(90),
            max_idle: 1024,
            h2_keep_alive: Duration::from_secs(20),
        }
    }
}

/// Why no connection could be obtained.
#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    /// TCP connect failed.
    #[error("connect to {addr} failed: {source}")]
    Connect {
        /// Endpoint.
        addr: SocketAddr,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// TCP connect did not finish in time.
    #[error("connect to {addr} timed out after {timeout:?}")]
    ConnectTimeout {
        /// Endpoint.
        addr: SocketAddr,
        /// Configured timeout.
        timeout: Duration,
    },
    /// The TLS handshake with the endpoint failed.
    #[error("TLS handshake with {addr} failed: {source}")]
    Tls {
        /// Endpoint.
        addr: SocketAddr,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The cluster's `max_connections` circuit breaker is open.
    #[error("connection limit reached for {addr}")]
    Overflow {
        /// Endpoint.
        addr: SocketAddr,
    },
    /// The HTTP handshake failed.
    #[error("handshake with {addr} failed: {source}")]
    Handshake {
        /// Endpoint.
        addr: SocketAddr,
        /// Underlying error.
        #[source]
        source: hyper::Error,
    },
}

type BoxError = Box<dyn StdError + Send + Sync>;

/// Connections to one endpoint.
#[derive(Debug)]
pub struct Pool<B> {
    addr: SocketAddr,
    protocol: Protocol,
    tls: Option<Arc<UpstreamTls>>,
    settings: PoolSettings,
    idle: SegQueue<Idle<B>>,
    idle_len: AtomicUsize,
    h2: ArcSwapOption<http2::SendRequest<B>>,
    dial_lock: tokio::sync::Mutex<()>,
}

#[derive(Debug)]
struct Idle<B> {
    sender: http1::SendRequest<B>,
    since: Instant,
}

/// A connection checked out of a pool, ready for one request.
#[derive(Debug)]
pub struct Connection<B> {
    inner: Inner<B>,
    pool: Arc<Pool<B>>,
    reused: bool,
}

#[derive(Debug)]
enum Inner<B> {
    H1(http1::SendRequest<B>),
    H2(http2::SendRequest<B>),
}

impl<B> Pool<B>
where
    B: Body + Send + Unpin + 'static,
    B::Data: Send,
    B::Error: Into<BoxError>,
{
    /// An empty pool.
    #[must_use]
    pub fn new(
        addr: SocketAddr,
        protocol: Protocol,
        tls: Option<Arc<UpstreamTls>>,
        settings: PoolSettings,
    ) -> Arc<Self> {
        Arc::new(Self {
            addr,
            protocol,
            tls,
            settings,
            idle: SegQueue::new(),
            idle_len: AtomicUsize::new(0),
            h2: ArcSwapOption::empty(),
            dial_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Endpoint address.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Wire protocol.
    #[must_use]
    pub fn protocol(&self) -> Protocol {
        self.protocol
    }

    /// Idle HTTP/1.1 connections currently pooled.
    #[must_use]
    pub fn idle_count(&self) -> usize {
        self.idle_len.load(Ordering::Relaxed)
    }

    /// Returns a usable connection, reusing one when possible. A new
    /// connection takes a slot from `limit` (the cluster's connection circuit
    /// breaker) for as long as it stays open.
    ///
    /// # Errors
    ///
    /// Fails if a new connection is needed and `limit` is full, or it cannot
    /// be established within `connect_timeout`. The caller's request is never
    /// involved.
    pub async fn checkout(
        self: &Arc<Self>,
        connect_timeout: Duration,
        limit: Option<&Arc<Limit>>,
    ) -> Result<Connection<B>, PoolError> {
        match self.protocol {
            Protocol::Http1 => {
                if let Some(sender) = self.pop_idle() {
                    return Ok(self.wrap(Inner::H1(sender), true));
                }
                let permit = self.permit(limit)?;
                let sender = self.dial_h1(connect_timeout, permit).await?;
                Ok(self.wrap(Inner::H1(sender), false))
            }
            Protocol::Http2 => {
                if let Some(sender) = self.live_h2() {
                    return Ok(self.wrap(Inner::H2(sender), true));
                }
                let _dialing = self.dial_lock.lock().await;
                // Another task may have dialed while we waited.
                if let Some(sender) = self.live_h2() {
                    return Ok(self.wrap(Inner::H2(sender), true));
                }
                let permit = self.permit(limit)?;
                let sender = self.dial_h2(connect_timeout, permit).await?;
                self.h2.store(Some(Arc::new(sender.clone())));
                Ok(self.wrap(Inner::H2(sender), false))
            }
        }
    }

    /// Closes idle HTTP/1.1 connections past the idle timeout, and forgets a
    /// closed HTTP/2 connection.
    pub fn evict_idle(&self) {
        let n = self.idle_len.load(Ordering::Relaxed);
        for _ in 0..n {
            let Some(idle) = self.idle.pop() else { break };
            self.idle_len.fetch_sub(1, Ordering::Relaxed);
            if self.is_fresh(&idle) {
                self.push_idle(idle.sender);
            }
        }
        if self.h2.load().as_ref().is_some_and(|s| s.is_closed()) {
            self.h2.store(None);
        }
    }

    fn permit(&self, limit: Option<&Arc<Limit>>) -> Result<Option<Permit>, PoolError> {
        match limit {
            None => Ok(None),
            Some(l) => l
                .try_acquire()
                .map(Some)
                .ok_or(PoolError::Overflow { addr: self.addr }),
        }
    }

    fn wrap(self: &Arc<Self>, inner: Inner<B>, reused: bool) -> Connection<B> {
        Connection {
            inner,
            pool: Arc::clone(self),
            reused,
        }
    }

    fn is_fresh(&self, idle: &Idle<B>) -> bool {
        idle.since.elapsed() < self.settings.idle_timeout
            && !idle.sender.is_closed()
            && idle.sender.is_ready()
    }

    fn pop_idle(&self) -> Option<http1::SendRequest<B>> {
        while let Some(idle) = self.idle.pop() {
            self.idle_len.fetch_sub(1, Ordering::Relaxed);
            if self.is_fresh(&idle) {
                return Some(idle.sender);
            }
        }
        None
    }

    fn push_idle(&self, sender: http1::SendRequest<B>) {
        if self.idle_len.fetch_add(1, Ordering::Relaxed) < self.settings.max_idle {
            self.idle.push(Idle {
                sender,
                since: Instant::now(),
            });
        } else {
            self.idle_len.fetch_sub(1, Ordering::Relaxed);
        }
    }

    fn live_h2(&self) -> Option<http2::SendRequest<B>> {
        let guard = self.h2.load();
        let sender = guard.as_ref()?;
        (!sender.is_closed()).then(|| http2::SendRequest::clone(sender))
    }

    /// TCP connect, then the TLS handshake when the pool uses TLS; both
    /// within `timeout`.
    async fn connect(&self, timeout: Duration) -> Result<TokioIo<Stream>, PoolError> {
        let addr = self.addr;
        tokio::time::timeout(timeout, dial(addr, self.tls.as_deref()))
            .await
            .map_err(|_| PoolError::ConnectTimeout { addr, timeout })?
            .map(TokioIo::new)
    }

    async fn dial_h1(
        &self,
        timeout: Duration,
        permit: Option<Permit>,
    ) -> Result<http1::SendRequest<B>, PoolError> {
        let io = self.connect(timeout).await?;
        let addr = self.addr;
        let (sender, conn) = http1::Builder::new()
            .handshake(io)
            .await
            .map_err(|source| PoolError::Handshake { addr, source })?;
        tokio::spawn(async move {
            // The connection slot is held until the connection ends.
            let _permit = permit;
            // with_upgrades lets a 101 response hand the socket to a tunnel.
            if let Err(e) = conn.with_upgrades().await {
                debug!(%addr, error = %e, "upstream http/1.1 connection closed with error");
            }
        });
        Ok(sender)
    }

    async fn dial_h2(
        &self,
        timeout: Duration,
        permit: Option<Permit>,
    ) -> Result<http2::SendRequest<B>, PoolError> {
        let io = self.connect(timeout).await?;
        let addr = self.addr;
        let (sender, conn) = http2::Builder::new(TokioExecutor::new())
            .timer(TokioTimer::new())
            .keep_alive_interval(self.settings.h2_keep_alive)
            .keep_alive_timeout(self.settings.h2_keep_alive / 2)
            .handshake(io)
            .await
            .map_err(|source| PoolError::Handshake { addr, source })?;
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(e) = conn.await {
                debug!(%addr, error = %e, "upstream h2 connection closed with error");
            }
        });
        Ok(sender)
    }
}

impl<B> Connection<B>
where
    B: Body + Send + Unpin + 'static,
    B::Data: Send,
    B::Error: Into<BoxError>,
{
    /// True if this connection carried an earlier request. A failure on a
    /// reused connection may just mean the upstream closed it while idle.
    #[must_use]
    pub fn is_reused(&self) -> bool {
        self.reused
    }

    /// Sends one request and waits for the response headers.
    ///
    /// # Errors
    ///
    /// On failure the error carries the request back if it was never written
    /// to the connection ([`TrySendError::take_message`]), so it can be retried
    /// safely regardless of method.
    // The error is large because it carries the request back; that is its purpose
    // (hyper's own `try_send_request` makes the same trade).
    #[allow(clippy::result_large_err)]
    pub async fn send(
        self,
        req: Request<B>,
    ) -> Result<Response<Incoming>, TrySendError<Request<B>>> {
        let Self { inner, pool, .. } = self;
        match inner {
            Inner::H1(mut sender) => {
                let resp = sender.try_send_request(req).await?;
                // The connection becomes ready again once this response's body
                // has been read; hand it back to the pool then.
                tokio::spawn(async move {
                    if sender.ready().await.is_ok() {
                        pool.push_idle(sender);
                    }
                });
                Ok(resp)
            }
            Inner::H2(mut sender) => sender.try_send_request(req).await,
        }
    }
}

/// How a pool speaks TLS to its endpoint.
#[derive(Debug)]
pub struct UpstreamTls {
    /// Client configuration (roots, client certificate, ALPN).
    pub config: Arc<rustls::ClientConfig>,
    /// Name sent in SNI and verified against the server certificate.
    pub server_name: ServerName<'static>,
    /// Fingerprint of the settings; pools with different TLS settings to the
    /// same endpoint are kept apart.
    pub id: u64,
    /// The end-entity client certificate presented for mutual TLS, kept for
    /// reporting its expiry.
    pub client_certificate: Option<CertificateDer<'static>>,
}

/// A plaintext or TLS upstream connection.
#[derive(Debug)]
pub(crate) enum Stream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl Stream {
    /// True when TLS negotiated `h2` through ALPN.
    pub(crate) fn negotiated_h2(&self) -> bool {
        matches!(self, Self::Tls(s) if s.get_ref().1.alpn_protocol() == Some(b"h2"))
    }
}

/// Connects to `addr` and, with `tls`, completes the TLS handshake. Shared by
/// pools and health checks; callers apply their own timeout.
pub(crate) async fn dial(addr: SocketAddr, tls: Option<&UpstreamTls>) -> Result<Stream, PoolError> {
    let stream = TcpStream::connect(addr)
        .await
        .map_err(|source| PoolError::Connect { addr, source })?;
    if let Err(e) = stream.set_nodelay(true) {
        debug!(%addr, error = %e, "set_nodelay failed");
    }
    match tls {
        None => Ok(Stream::Plain(stream)),
        Some(t) => tokio_rustls::TlsConnector::from(Arc::clone(&t.config))
            .connect(t.server_name.clone(), stream)
            .await
            .map(|s| Stream::Tls(Box::new(s)))
            .map_err(|source| PoolError::Tls { addr, source }),
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_write_vectored(cx, bufs),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_write_vectored(cx, bufs),
        }
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            Self::Plain(s) => s.is_write_vectored(),
            Self::Tls(s) => s.is_write_vectored(),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_flush(cx),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

/// Identifies a pool: endpoint address, wire protocol and TLS settings.
pub type PoolKey = (SocketAddr, Protocol, Option<u64>);

/// Every pool of a proxy, keyed by endpoint and protocol.
///
/// Only touched off the hot path: when a snapshot is installed (to resolve
/// each endpoint's pool) and by the periodic idle sweep.
#[derive(Debug)]
pub struct PoolSet<B> {
    settings: PoolSettings,
    pools: Mutex<HashMap<PoolKey, Arc<Pool<B>>>>,
}

impl<B> PoolSet<B>
where
    B: Body + Send + Unpin + 'static,
    B::Data: Send,
    B::Error: Into<BoxError>,
{
    /// An empty set.
    #[must_use]
    pub fn new(settings: PoolSettings) -> Self {
        Self {
            settings,
            pools: Mutex::new(HashMap::new()),
        }
    }

    /// The pool for `addr` and `protocol`, created on first use.
    pub fn get(
        &self,
        addr: SocketAddr,
        protocol: Protocol,
        tls: Option<&Arc<UpstreamTls>>,
    ) -> Arc<Pool<B>> {
        let mut pools = self.pools.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(
            pools
                .entry((addr, protocol, tls.map(|t| t.id)))
                .or_insert_with(|| Pool::new(addr, protocol, tls.cloned(), self.settings)),
        )
    }

    /// Forgets pools not in `keep`. Requests still holding a forgotten pool
    /// finish normally; its connections close when the last one is dropped.
    pub fn retain(&self, keep: &HashSet<PoolKey>) {
        self.pools
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|k, _| keep.contains(k));
    }

    /// Runs [`Pool::evict_idle`] on every pool.
    pub fn evict_idle(&self) {
        let pools: Vec<Arc<Pool<B>>> = self
            .pools
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        for p in pools {
            p.evict_idle();
        }
    }

    /// Number of pools.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pools
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// True when there are no pools.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use ferry_testkit::{Behavior, http as mock_http};
    use http_body_util::{BodyExt, Empty};

    type B = Empty<Bytes>;

    fn get(addr: SocketAddr, h2: bool) -> Request<B> {
        let uri = if h2 {
            format!("http://{addr}/")
        } else {
            "/".to_owned()
        };
        Request::get(uri)
            .header("host", addr.to_string())
            .body(Empty::new())
            .unwrap()
    }

    const T: Duration = Duration::from_secs(2);

    #[tokio::test]
    async fn http1_connections_are_reused() {
        let mock = mock_http::spawn("127.0.0.1:0".parse().unwrap(), Behavior::named("a"))
            .await
            .unwrap();
        let pool: Arc<Pool<B>> =
            Pool::new(mock.addr(), Protocol::Http1, None, PoolSettings::default());
        for i in 0..3 {
            let conn = pool.checkout(T, None).await.unwrap();
            assert_eq!(conn.is_reused(), i > 0, "request {i}");
            let resp = conn.send(get(mock.addr(), false)).await.unwrap();
            assert_eq!(resp.status(), 200);
            resp.into_body().collect().await.unwrap();
            // Let the return-to-pool task run.
            for _ in 0..50 {
                if pool.idle_count() == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert_eq!(pool.idle_count(), 1);
        }
    }

    #[tokio::test]
    async fn h2_connection_is_shared() {
        let mock = mock_http::spawn("127.0.0.1:0".parse().unwrap(), Behavior::named("a"))
            .await
            .unwrap();
        let pool: Arc<Pool<B>> =
            Pool::new(mock.addr(), Protocol::Http2, None, PoolSettings::default());
        let first = pool.checkout(T, None).await.unwrap();
        let second = pool.checkout(T, None).await.unwrap();
        assert!(!first.is_reused());
        assert!(second.is_reused());
        let (a, b) = tokio::join!(
            first.send(get(mock.addr(), true)),
            second.send(get(mock.addr(), true))
        );
        assert_eq!(a.unwrap().version(), http::Version::HTTP_2);
        assert_eq!(b.unwrap().status(), 200);
    }

    #[tokio::test]
    async fn refused_connect_is_a_pool_error() {
        let dead = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let pool: Arc<Pool<B>> = Pool::new(dead, Protocol::Http1, None, PoolSettings::default());
        // Linux refuses at once; Windows retries the SYN for about two seconds,
        // so a short timeout may fire first.
        assert!(matches!(
            pool.checkout(Duration::from_millis(500), None).await,
            Err(PoolError::Connect { .. } | PoolError::ConnectTimeout { .. })
        ));
    }

    #[tokio::test]
    async fn dead_idle_connections_are_skipped() {
        let mock = mock_http::spawn("127.0.0.1:0".parse().unwrap(), Behavior::named("a"))
            .await
            .unwrap();
        let addr = mock.addr();
        let pool: Arc<Pool<B>> = Pool::new(addr, Protocol::Http1, None, PoolSettings::default());
        let resp = pool
            .checkout(T, None)
            .await
            .unwrap()
            .send(get(addr, false))
            .await
            .unwrap();
        resp.into_body().collect().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(pool.idle_count(), 1);
        mock.kill().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        // The idle connection is closed, so checkout must dial (and fail).
        assert!(pool.checkout(T, None).await.is_err());
        assert_eq!(pool.idle_count(), 0);
    }

    #[tokio::test]
    async fn connection_limit_blocks_new_connections_only() {
        let mock = mock_http::spawn("127.0.0.1:0".parse().unwrap(), Behavior::named("a"))
            .await
            .unwrap();
        let pool: Arc<Pool<B>> =
            Pool::new(mock.addr(), Protocol::Http1, None, PoolSettings::default());
        let limit = Limit::new(1);
        let first = pool.checkout(T, Some(&limit)).await.unwrap();
        assert_eq!(limit.active(), 1);
        assert!(matches!(
            pool.checkout(T, Some(&limit)).await,
            Err(PoolError::Overflow { .. })
        ));
        // Once the first connection is back in the pool it can be reused
        // without a new slot.
        let resp = first.send(get(mock.addr(), false)).await.unwrap();
        resp.into_body().collect().await.unwrap();
        for _ in 0..50 {
            if pool.idle_count() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(pool.checkout(T, Some(&limit)).await.unwrap().is_reused());
        assert_eq!(limit.active(), 1);
    }

    #[test]
    fn pool_set_dedupes_and_retains() {
        let set: PoolSet<B> = PoolSet::new(PoolSettings::default());
        let a: SocketAddr = "10.0.0.1:80".parse().unwrap();
        let b: SocketAddr = "10.0.0.2:80".parse().unwrap();
        let p1 = set.get(a, Protocol::Http1, None);
        assert!(Arc::ptr_eq(&p1, &set.get(a, Protocol::Http1, None)));
        set.get(a, Protocol::Http2, None);
        set.get(b, Protocol::Http1, None);
        assert_eq!(set.len(), 3);
        set.retain(&HashSet::from([(a, Protocol::Http1, None)]));
        assert_eq!(set.len(), 1);
        assert!(Arc::ptr_eq(&p1, &set.get(a, Protocol::Http1, None)));
    }
}
