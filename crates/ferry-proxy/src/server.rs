//! Listeners, connection serving, hot swap and graceful drain.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use arc_swap::ArcSwap;
use ferry_config::{Snapshot, SnapshotVersion};
use ferry_lb::pool::{PoolSet, PoolSettings};
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use hyper_util::server::graceful::GracefulShutdown;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::body::UpstreamBody;
use crate::error::ProxyError;
use crate::forward::{Downstream, handle};
use crate::state::{BoundListener, Control, HealthSink, Runtime};

/// How long a draining proxy waits for in-flight requests before closing connections.
pub const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Time allowed for a client to send complete request headers.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// How often idle upstream connections are swept.
const POOL_SWEEP_INTERVAL: Duration = Duration::from_secs(15);

/// Shared between listeners, the reload handle and health checkers.
#[derive(Debug)]
struct Shared {
    runtime: ArcSwap<Runtime>,
    /// Serialises reloads with health-driven host-set rebuilds.
    control: Mutex<Control>,
    pools: PoolSet<UpstreamBody>,
    bound: Vec<BoundListener>,
}

impl Shared {
    fn control(&self) -> std::sync::MutexGuard<'_, Control> {
        self.control.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Builds and publishes a runtime for `snapshot` under the control lock.
    fn install(self: &Arc<Self>, snapshot: Snapshot) {
        let weak = Arc::downgrade(self);
        let mut control = self.control();
        let prev = self.runtime.load_full();
        let next = control.build(snapshot, Some(&prev), &self.bound, &self.pools, &weak);
        self.runtime.store(Arc::new(next));
    }

    /// Rebuilds every cluster's host set from current health.
    fn refresh_all(&self) {
        let _control = self.control();
        for c in &self.runtime.load().clusters {
            c.balancer.refresh();
        }
    }
}

impl HealthSink for Shared {
    fn health_changed(&self, cluster: &str) {
        let _control = self.control();
        let runtime = self.runtime.load();
        if let Some(c) = runtime.clusters.iter().find(|c| c.config.name() == cluster) {
            c.balancer.refresh();
            let set = c.balancer.host_set();
            info!(
                cluster,
                eligible = set.len(),
                panic = set.is_panic(),
                "host set rebuilt after health change"
            );
        }
    }
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
    /// Binds every listener in `snapshot` and starts its health checkers.
    ///
    /// # Errors
    ///
    /// Returns [`ProxyError::Bind`] if any address cannot be bound.
    pub async fn bind(snapshot: Snapshot) -> Result<Self, ProxyError> {
        let mut listeners = Vec::with_capacity(snapshot.listeners().len());
        let mut bound = Vec::with_capacity(snapshot.listeners().len());
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
            bound.push(BoundListener {
                name: l.name().to_owned(),
                addr: local,
            });
        }
        let pools = PoolSet::new(PoolSettings::default());
        let shared = Arc::new_cyclic(|weak: &Weak<Shared>| {
            let mut control = Control::default();
            let runtime = control.build(snapshot, None, &bound, &pools, weak);
            Shared {
                runtime: ArcSwap::from_pointee(runtime),
                control: Mutex::new(control),
                pools,
                bound,
            }
        });
        // A checker could have flipped an endpoint before `shared` existed.
        shared.refresh_all();
        Ok(Self { shared, listeners })
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
    /// Health checkers stop once draining finishes.
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
        let sweeper = tokio::spawn(sweep_pools(Arc::downgrade(&self.shared), token.clone()));
        shutdown.await;
        info!(?drain_timeout, "shutdown requested; draining");
        token.cancel();
        while let Some(res) = tasks.join_next().await {
            if let Err(e) = res {
                warn!(error = %e, "listener task ended abnormally");
            }
        }
        let _ = sweeper.await;
        self.shared.control().shutdown();
        info!("drained");
        Ok(())
    }
}

impl ReloadHandle {
    /// Atomically replaces the active snapshot. Requests already in flight keep
    /// the snapshot they started with. Unchanged clusters and routers, endpoint
    /// health and connection pools carry over. Listener addresses are not
    /// rebound; a changed listener set takes effect on restart.
    pub fn swap(&self, snapshot: Snapshot) {
        let version = snapshot.version();
        self.shared.install(snapshot);
        info!(%version, "snapshot swapped");
    }

    /// Version of the snapshot currently serving new requests.
    #[must_use]
    pub fn current_version(&self) -> SnapshotVersion {
        self.shared.runtime.load().snapshot.version()
    }

    /// Health of every endpoint of `cluster` in the current snapshot, in
    /// config order, or `None` if there is no such cluster.
    #[must_use]
    pub fn endpoint_health(&self, cluster: &str) -> Option<Vec<(SocketAddr, bool)>> {
        let runtime = self.shared.runtime.load();
        let c = runtime
            .clusters
            .iter()
            .find(|c| c.config.name() == cluster)?;
        Some(
            c.balancer
                .hosts()
                .iter()
                .map(|h| (h.state.address(), h.state.is_healthy()))
                .collect(),
        )
    }
}

async fn sweep_pools(shared: Weak<Shared>, token: CancellationToken) {
    loop {
        tokio::select! {
            () = token.cancelled() => return,
            () = tokio::time::sleep(POOL_SWEEP_INTERVAL) => {}
        }
        let Some(shared) = shared.upgrade() else {
            return;
        };
        shared.pools.evict_idle();
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
                    let from = Downstream { listener: index, port: addr.port(), peer };
                    serve(&shared, &builder, &graceful, from, stream);
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
    from: Downstream,
    stream: TcpStream,
) {
    if let Err(e) = stream.set_nodelay(true) {
        debug!(peer = %from.peer, error = %e, "set_nodelay failed");
    }
    let svc_shared = Arc::clone(shared);
    let service = service_fn(move |req| {
        // One load per request: the request keeps this snapshot until it completes.
        let runtime = svc_shared.runtime.load_full();
        handle(runtime, from, req)
    });
    let conn = builder
        .serve_connection(TokioIo::new(stream), service)
        .into_owned();
    let conn = graceful.watch(conn);
    let peer = from.peer;
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            debug!(%peer, error = %e, "connection closed with error");
        }
    });
}
