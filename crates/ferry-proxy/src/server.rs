//! Listeners, connection serving, hot swap and graceful drain.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use ferry_config::{Snapshot, SnapshotVersion};
use hyper::service::service_fn;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use hyper_util::server::graceful::GracefulShutdown;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::error::ProxyError;
use crate::forward::{UpstreamClient, handle};
use crate::state::Runtime;

/// How long a draining proxy waits for in-flight requests before closing connections.
pub const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Time allowed for a client to send complete request headers.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Idle upstream connections are closed after this long.
const UPSTREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// Shared between listeners and the reload handle.
#[derive(Debug)]
struct Shared {
    runtime: ArcSwap<Runtime>,
    client: UpstreamClient,
}

/// A proxy whose listeners are bound but not yet serving.
#[derive(Debug)]
pub struct Proxy {
    shared: Arc<Shared>,
    listeners: Vec<(usize, SocketAddr, TcpListener)>,
}

/// A cheap, cloneable handle for swapping in new snapshots.
#[derive(Debug, Clone)]
pub struct ReloadHandle {
    shared: Arc<Shared>,
}

impl Proxy {
    /// Binds every listener in `snapshot`.
    ///
    /// # Errors
    ///
    /// Returns [`ProxyError::Bind`] if any address cannot be bound.
    pub async fn bind(snapshot: Snapshot) -> Result<Self, ProxyError> {
        let mut listeners = Vec::with_capacity(snapshot.listeners().len());
        for (i, l) in snapshot.listeners().iter().enumerate() {
            let tcp = TcpListener::bind(l.address())
                .await
                .map_err(|source| ProxyError::Bind {
                    address: l.address(),
                    source,
                })?;
            let local = tcp.local_addr().map_err(|source| ProxyError::Bind {
                address: l.address(),
                source,
            })?;
            info!(listener = l.name(), address = %local, "listening");
            listeners.push((i, local, tcp));
        }
        let mut connector = HttpConnector::new();
        connector.set_nodelay(true);
        let client = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(UPSTREAM_IDLE_TIMEOUT)
            .pool_timer(TokioTimer::new())
            .timer(TokioTimer::new())
            .build(connector);
        Ok(Self {
            shared: Arc::new(Shared {
                runtime: ArcSwap::from_pointee(Runtime::new(snapshot)),
                client,
            }),
            listeners,
        })
    }

    /// Addresses actually bound, in listener order. Useful when a config asks for port 0.
    #[must_use]
    pub fn local_addrs(&self) -> Vec<SocketAddr> {
        self.listeners.iter().map(|(_, addr, _)| *addr).collect()
    }

    /// A handle that swaps the active snapshot while the proxy runs.
    #[must_use]
    pub fn reload_handle(&self) -> ReloadHandle {
        ReloadHandle {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Serves until `shutdown` resolves, then drains.
    ///
    /// Draining stops accepting, tells HTTP/1.1 clients to close after their
    /// current response and sends HTTP/2 GOAWAY, then waits up to
    /// `drain_timeout` for in-flight requests before dropping what remains.
    ///
    /// # Errors
    ///
    /// Currently infallible after binding; the `Result` reserves room for
    /// listener failures that should stop the process.
    pub async fn run(
        self,
        shutdown: impl Future<Output = ()> + Send,
        drain_timeout: Duration,
    ) -> Result<(), ProxyError> {
        let token = CancellationToken::new();
        let mut tasks = JoinSet::new();
        for (index, addr, tcp) in self.listeners {
            tasks.spawn(accept_loop(
                Arc::clone(&self.shared),
                index,
                addr,
                tcp,
                token.clone(),
                drain_timeout,
            ));
        }
        shutdown.await;
        info!(?drain_timeout, "shutdown requested; draining");
        token.cancel();
        while let Some(res) = tasks.join_next().await {
            if let Err(e) = res {
                warn!(error = %e, "listener task ended abnormally");
            }
        }
        info!("drained");
        Ok(())
    }
}

impl ReloadHandle {
    /// Atomically replaces the active snapshot. Requests already in flight keep
    /// the snapshot they started with. Listener addresses are not rebound; a
    /// changed listener set takes effect on restart.
    pub fn swap(&self, snapshot: Snapshot) {
        let version = snapshot.version();
        self.shared.runtime.store(Arc::new(Runtime::new(snapshot)));
        info!(%version, "snapshot swapped");
    }

    /// Version of the snapshot currently serving new requests.
    #[must_use]
    pub fn current_version(&self) -> SnapshotVersion {
        self.shared.runtime.load().snapshot.version()
    }
}

async fn accept_loop(
    shared: Arc<Shared>,
    index: usize,
    addr: SocketAddr,
    tcp: TcpListener,
    token: CancellationToken,
    drain_timeout: Duration,
) {
    let graceful = GracefulShutdown::new();
    let mut builder = auto::Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT)
        .keep_alive(true);
    builder.http2().timer(TokioTimer::new());

    loop {
        tokio::select! {
            () = token.cancelled() => break,
            accepted = tcp.accept() => match accepted {
                Ok((stream, peer)) => {
                    serve(&shared, &builder, &graceful, index, stream, peer);
                }
                Err(e) => {
                    // Usually fd exhaustion; back off briefly instead of spinning.
                    warn!(listener = %addr, error = %e, "accept failed");
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            },
        }
    }
    drop(tcp);
    if tokio::time::timeout(drain_timeout, graceful.shutdown())
        .await
        .is_err()
    {
        warn!(listener = %addr, "drain timeout elapsed; closing remaining connections");
    }
}

fn serve(
    shared: &Arc<Shared>,
    builder: &auto::Builder<TokioExecutor>,
    graceful: &GracefulShutdown,
    index: usize,
    stream: TcpStream,
    peer: SocketAddr,
) {
    if let Err(e) = stream.set_nodelay(true) {
        debug!(%peer, error = %e, "set_nodelay failed");
    }
    let svc_shared = Arc::clone(shared);
    let service = service_fn(move |req| {
        // One load per request: the request keeps this snapshot until it completes.
        let runtime = svc_shared.runtime.load_full();
        handle(runtime, svc_shared.client.clone(), index, req)
    });
    let conn = builder
        .serve_connection(TokioIo::new(stream), service)
        .into_owned();
    let conn = graceful.watch(conn);
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            debug!(%peer, error = %e, "connection closed with error");
        }
    });
}
