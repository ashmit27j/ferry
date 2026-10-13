//! Listeners, connection serving, hot swap and graceful drain.

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use arc_swap::ArcSwap;
use ferry_config::{Config, Listener, Snapshot, SnapshotVersion};
use ferry_lb::pool::{PoolSet, PoolSettings};
use http::header::{self, HeaderValue};
use http::{Request, StatusCode, Version};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use hyper_util::server::graceful::GracefulShutdown;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::body::UpstreamBody;
use crate::collector::StateCollector;
use crate::error::ProxyError;
use crate::forward::{Downstream, handle};
use crate::proxy_protocol;
use crate::state::{BoundListener, Control, HealthSink, ListenerRuntime, Runtime, prepare};
use crate::tls::HANDSHAKE_TIMEOUT;
use ferry_telemetry::Telemetry;
use ferry_telemetry::metrics::ListenerStats;

/// How long a draining proxy waits for in-flight requests before closing connections.
pub const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Shutdown timing; see [`Proxy::run`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Drain {
    /// How long to keep accepting, with `Connection: close` on every HTTP/1.1
    /// response, before closing the listeners. Should cover the load
    /// balancer's deregistration delay.
    pub delay: Duration,
    /// How long to wait for in-flight requests once the listeners close.
    pub timeout: Duration,
}

impl Default for Drain {
    fn default() -> Self {
        Self {
            delay: Duration::ZERO,
            timeout: DEFAULT_DRAIN_TIMEOUT,
        }
    }
}

/// How often certificate files of TLS listeners are re-read.
const CERT_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// How often expired outlier ejections are ended.
const EJECTION_SWEEP_INTERVAL: Duration = Duration::from_millis(500);

/// How often idle upstream connections are swept.
const POOL_SWEEP_INTERVAL: Duration = Duration::from_secs(15);

/// Pending-connection queue of each listening socket (tokio's default).
const LISTEN_BACKLOG: i32 = 1024;

/// Listener slots, parallel to [`Runtime::listeners`]. Index `i` of `bound`
/// and of `stops` describe the same socket.
#[derive(Debug, Default)]
struct Slots {
    bound: Vec<BoundListener>,
    /// Cancels the accept loop of each slot.
    stops: Vec<CancellationToken>,
}

/// A socket bound by a reload, waiting for [`Proxy::run`] to accept on it.
#[derive(Debug)]
struct NewListener {
    index: usize,
    addr: SocketAddr,
    socket: std::net::TcpListener,
    stop: CancellationToken,
}

/// How a snapshot's listeners map onto slots: what to keep, bind and retire.
#[derive(Debug)]
struct ListenerPlan {
    /// Every slot after the change, retired ones included.
    bound: Vec<BoundListener>,
    /// Slots whose listener left the config.
    retired: Vec<usize>,
    /// Newly bound sockets, by slot index, in increasing order.
    sockets: Vec<(usize, std::net::TcpListener)>,
}

/// Matches `snapshot`'s listeners against the current slots and binds a
/// socket for each new one. A listener keeps its slot when its name and
/// address are unchanged; a renamed listener on the same address takes over
/// the old socket. Nothing is published: on error the new sockets are
/// dropped and the running proxy is untouched.
fn plan_listeners(
    current: &[BoundListener],
    snapshot: &Snapshot,
) -> Result<ListenerPlan, ProxyError> {
    let mut bound = current.to_vec();
    let mut claimed = vec![false; bound.len()];
    let mut unplaced = Vec::new();
    for l in snapshot.listeners() {
        let same_name = bound.iter().enumerate().position(|(i, b)| {
            !b.retired && !claimed[i] && b.name == l.name() && b.configured == l.address()
        });
        if let Some(i) = same_name {
            claimed[i] = true;
        } else {
            unplaced.push(l);
        }
    }
    let mut fresh = Vec::new();
    for l in unplaced {
        // A renamed listener keeps the socket bound to its address, which a
        // second bind could not take while the old one is open.
        let same_address = bound.iter().enumerate().position(|(i, b)| {
            !b.retired && !claimed[i] && b.configured == l.address() && l.address().port() != 0
        });
        match same_address {
            Some(i) => {
                claimed[i] = true;
                if let Some(b) = bound.get_mut(i) {
                    info!(from = %b.name, to = l.name(), address = %b.addr, "listener renamed");
                    l.name().clone_into(&mut b.name);
                }
            }
            None => fresh.push(l),
        }
    }
    let mut retired = Vec::new();
    for (i, b) in bound.iter_mut().enumerate() {
        if !b.retired && !claimed[i] {
            b.retired = true;
            retired.push(i);
        }
    }
    let mut sockets = Vec::with_capacity(fresh.len());
    for l in fresh {
        let bind_err = |source| ProxyError::Bind {
            address: l.address(),
            source,
        };
        let socket = bind_socket(l.address()).map_err(bind_err)?;
        let addr = socket.local_addr().map_err(bind_err)?;
        bound.push(BoundListener::new(l.name(), addr, l.address()));
        sockets.push((bound.len() - 1, socket));
    }
    Ok(ListenerPlan {
        bound,
        retired,
        sockets,
    })
}

/// Binds a non-blocking listening socket. Synchronous, so a reload can fail
/// before publishing anything; `SO_REUSEADDR` (on Unix) lets a removed and
/// re-added listener rebind while old connections linger in `TIME_WAIT`.
fn bind_socket(addr: SocketAddr) -> std::io::Result<std::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    // On Windows SO_REUSEADDR lets another socket steal a bound port; skip it.
    #[cfg(unix)]
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(LISTEN_BACKLOG)?;
    Ok(socket.into())
}

/// Shared between listeners, the reload handle and health checkers.
#[derive(Debug)]
pub(crate) struct Shared {
    runtime: ArcSwap<Runtime>,
    /// Set when shutdown starts: HTTP/1.1 responses then carry `Connection: close`.
    draining: AtomicBool,
    /// Set until the first snapshot arrives for a proxy started with
    /// [`Proxy::bind_awaiting`]; readiness fails meanwhile.
    awaiting: AtomicBool,
    /// Listener slots. Lock order: `slots`, then `control`.
    slots: Mutex<Slots>,
    /// Serialises reloads with health-driven host-set rebuilds.
    control: Mutex<Control>,
    /// Hands newly bound sockets to [`Proxy::run`].
    new_listeners: mpsc::UnboundedSender<NewListener>,
    /// Parent of every accept loop's stop token; cancelled at shutdown.
    stop: CancellationToken,
    pools: PoolSet<UpstreamBody>,
    telemetry: Arc<Telemetry>,
}

impl Shared {
    /// The runtime serving new requests.
    pub(crate) fn runtime(&self) -> Arc<Runtime> {
        self.runtime.load_full()
    }

    /// Whether shutdown has started.
    pub(crate) fn telemetry(&self) -> &Arc<Telemetry> {
        &self.telemetry
    }

    pub(crate) fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Relaxed)
    }

    fn control(&self) -> std::sync::MutexGuard<'_, Control> {
        self.control.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn slots(&self) -> std::sync::MutexGuard<'_, Slots> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Builds and publishes a runtime for `snapshot`: binds new listeners,
    /// loads certificates, then swaps. Any failure before the swap leaves
    /// the running proxy untouched.
    fn install(self: &Arc<Self>, snapshot: Snapshot) -> Result<(), ProxyError> {
        let weak = Arc::downgrade(self);
        let mut slots = self.slots();
        let plan = plan_listeners(&slots.bound, &snapshot)?;
        let mut control = self.control();
        let prev = self.runtime.load_full();
        let prepared = prepare(&snapshot, Some(&prev), &plan.bound)?;
        let next = control.build(
            snapshot,
            Some(&prev),
            &plan.bound,
            &self.pools,
            &weak,
            &prepared,
            &self.telemetry.metrics,
        );
        self.runtime.store(Arc::new(next));
        drop(control);
        self.commit(&mut slots, plan);
        Ok(())
    }

    /// Applies a published plan: closes retired listeners and starts new ones.
    fn commit(&self, slots: &mut Slots, plan: ListenerPlan) {
        for i in plan.retired {
            if let (Some(stop), Some(b)) = (slots.stops.get(i), plan.bound.get(i)) {
                info!(listener = %b.name, address = %b.addr, "listener removed");
                stop.cancel();
            }
        }
        for (index, socket) in plan.sockets {
            let Some(b) = plan.bound.get(index) else {
                continue;
            };
            info!(listener = %b.name, address = %b.addr, "listening");
            let stop = self.stop.child_token();
            slots.stops.push(stop.clone());
            let new = NewListener {
                index,
                addr: b.addr,
                socket,
                stop,
            };
            if self.new_listeners.send(new).is_err() {
                warn!(listener = %b.name, "proxy stopped; not accepting on the new listener");
            }
        }
        slots.bound = plan.bound;
    }

    /// Re-reads certificate files of TLS listeners and swaps in any that
    /// changed. A failed read keeps the old certificates.
    fn reload_certificates(&self) {
        for l in self.runtime.load().listeners.iter().flatten() {
            if let Some(tls) = &l.tls {
                match tls.reload_if_changed() {
                    Ok(true) => info!(listener = l.config.name(), "certificates reloaded"),
                    Ok(false) => {}
                    Err(e) => warn!(listener = l.config.name(), error = %e,
                        "cannot reload certificates; keeping the current ones"),
                }
            }
        }
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
    /// Addresses bound at startup, in listener order.
    initial: Vec<SocketAddr>,
    new_listeners: mpsc::UnboundedReceiver<NewListener>,
}

/// A cheap, cloneable handle for swapping in new snapshots.
#[derive(Debug, Clone)]
pub struct ReloadHandle {
    shared: Arc<Shared>,
}

impl Proxy {
    /// Binds every listener in `snapshot` and starts its health checkers,
    /// with metrics only (no tracing or access log).
    ///
    /// # Errors
    ///
    /// Returns [`ProxyError::Bind`] if any address cannot be bound.
    pub async fn bind(snapshot: Snapshot) -> Result<Self, ProxyError> {
        Self::bind_with(snapshot, Arc::new(Telemetry::default())).await
    }

    /// Like [`Proxy::bind`], reporting through `telemetry`. Registers a
    /// collector for live state (endpoint health, pools, breakers, config
    /// version) with its metrics.
    ///
    /// # Errors
    ///
    /// Returns [`ProxyError::Bind`] if any address cannot be bound, or a TLS
    /// error if a listener's certificates cannot be loaded.
    pub async fn bind_with(
        snapshot: Snapshot,
        telemetry: Arc<Telemetry>,
    ) -> Result<Self, ProxyError> {
        Self::start(snapshot, telemetry, false)
    }

    /// A proxy with no configuration yet, for the Kubernetes mode: it binds
    /// nothing and reports not ready on the admin API until the first
    /// [`ReloadHandle::swap`] succeeds.
    ///
    /// # Errors
    ///
    /// Currently infallible; the `Result` matches [`Proxy::bind_with`].
    pub async fn bind_awaiting(telemetry: Arc<Telemetry>) -> Result<Self, ProxyError> {
        let empty = Config::new(Vec::new(), Vec::new())
            .into_snapshot(SnapshotVersion(0))
            .map_err(|e| ProxyError::Snapshot(e.to_string()))?;
        Self::start(empty, telemetry, true)
    }

    fn start(
        snapshot: Snapshot,
        telemetry: Arc<Telemetry>,
        awaiting: bool,
    ) -> Result<Self, ProxyError> {
        let plan = plan_listeners(&[], &snapshot)?;
        let pools = PoolSet::new(PoolSettings::default());
        let prepared = prepare(&snapshot, None, &plan.bound)?;
        let (tx, rx) = mpsc::unbounded_channel();
        let shared = Arc::new_cyclic(|weak: &Weak<Shared>| {
            let mut control = Control::default();
            let runtime = control.build(
                snapshot,
                None,
                &plan.bound,
                &pools,
                weak,
                &prepared,
                &telemetry.metrics,
            );
            Shared {
                runtime: ArcSwap::from_pointee(runtime),
                draining: AtomicBool::new(false),
                awaiting: AtomicBool::new(awaiting),
                slots: Mutex::new(Slots::default()),
                control: Mutex::new(control),
                new_listeners: tx,
                stop: CancellationToken::new(),
                pools,
                telemetry: Arc::clone(&telemetry),
            }
        });
        let initial = plan.bound.iter().map(|b| b.addr).collect();
        shared.commit(&mut shared.slots(), plan);
        telemetry
            .metrics
            .register_collector(Box::new(StateCollector::new(Arc::downgrade(&shared))));
        if !awaiting {
            telemetry.metrics.proxy().mark_loaded();
        }
        // A checker could have flipped an endpoint before `shared` existed.
        shared.refresh_all();
        Ok(Self {
            shared,
            initial,
            new_listeners: rx,
        })
    }

    /// Addresses bound at startup, in listener order. Useful when a config
    /// asks for port 0. Listeners added later are found with
    /// [`ReloadHandle::listener_addr`].
    #[must_use]
    pub fn local_addrs(&self) -> Vec<SocketAddr> {
        self.initial.clone()
    }

    /// A handle that swaps the active snapshot while the proxy runs.
    #[must_use]
    pub fn reload_handle(&self) -> ReloadHandle {
        ReloadHandle {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Serves until `shutdown` resolves, then drains in two stages (ADR 0026).
    ///
    /// Listeners added by later reloads start accepting as they are bound,
    /// and removed ones stop, draining their connections like a shutdown.
    ///
    /// 1. For `drain.delay`, keep accepting while load balancers deregister
    ///    this instance, but end every HTTP/1.1 response with
    ///    `Connection: close` so clients move elsewhere one request at a time.
    /// 2. Stop accepting, send HTTP/2 GOAWAY and close idle HTTP/1.1
    ///    connections, and wait up to `drain.timeout` for in-flight requests
    ///    before dropping what remains.
    ///
    /// Health checkers stop once draining finishes.
    ///
    /// # Errors
    ///
    /// Currently infallible after binding; the `Result` reserves room for
    /// listener failures that should stop the process.
    pub async fn run(
        mut self,
        shutdown: impl Future<Output = ()> + Send,
        drain: Drain,
    ) -> Result<(), ProxyError> {
        let drain_timeout = drain.timeout;
        let token = self.shared.stop.clone();
        let mut tasks = JoinSet::new();
        let sweeper = tokio::spawn(maintenance(Arc::downgrade(&self.shared), token.clone()));
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                () = &mut shutdown => break,
                Some(new) = self.new_listeners.recv() => {
                    let index = new.index;
                    let addr = new.addr;
                    match TcpListener::from_std(new.socket) {
                        Ok(tcp) => {
                            tasks.spawn(accept_loop(
                                Arc::clone(&self.shared),
                                index,
                                addr,
                                tcp,
                                new.stop,
                                drain_timeout,
                            ));
                        }
                        Err(e) => warn!(listener = %addr, error = %e, "cannot accept on listener"),
                    }
                }
                Some(res) = tasks.join_next(), if !tasks.is_empty() => {
                    if let Err(e) = res {
                        warn!(error = %e, "listener task ended abnormally");
                    }
                }
            }
        }
        self.shared.draining.store(true, Ordering::Relaxed);
        if !drain.delay.is_zero() {
            info!(delay = ?drain.delay, "shutdown requested; closing client connections as they finish");
            tokio::time::sleep(drain.delay).await;
        }
        info!(
            ?drain_timeout,
            "no longer accepting; draining in-flight requests"
        );
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
    /// health and connection pools carry over. New listeners are bound and
    /// start accepting; removed ones stop accepting and drain.
    ///
    /// # Errors
    ///
    /// Fails, leaving the current snapshot serving, if a new listener cannot
    /// be bound or a TLS listener's certificates cannot be loaded.
    pub fn swap(&self, snapshot: Snapshot) -> Result<(), ProxyError> {
        let version = snapshot.version();
        let result = self.shared.install(snapshot);
        self.shared.telemetry.metrics.proxy().reload(result.is_ok());
        result?;
        if self.shared.awaiting.swap(false, Ordering::Relaxed) {
            self.shared.telemetry.metrics.proxy().mark_loaded();
        }
        info!(%version, "snapshot swapped");
        Ok(())
    }

    /// Counts a config change rejected before it reached [`ReloadHandle::swap`]
    /// (it did not parse or validate).
    pub fn rejected(&self) {
        self.shared.telemetry.metrics.proxy().reload(false);
    }

    /// The telemetry this proxy reports through.
    #[must_use]
    pub fn telemetry(&self) -> &Arc<Telemetry> {
        &self.shared.telemetry
    }

    /// The snapshot currently serving new requests.
    #[must_use]
    pub fn snapshot(&self) -> Arc<Snapshot> {
        Arc::clone(&self.shared.runtime.load().snapshot)
    }

    pub(crate) fn runtime(&self) -> Arc<Runtime> {
        self.shared.runtime.load_full()
    }

    /// Whether shutdown has started.
    #[must_use]
    pub fn is_draining(&self) -> bool {
        self.shared.draining.load(Ordering::Relaxed)
    }

    /// Whether the proxy still waits for its first snapshot (see
    /// [`Proxy::bind_awaiting`]).
    #[must_use]
    pub fn is_awaiting_config(&self) -> bool {
        self.shared.awaiting.load(Ordering::Relaxed)
    }

    /// The address listener `name` is bound to, if it is live.
    #[must_use]
    pub fn listener_addr(&self, name: &str) -> Option<SocketAddr> {
        self.shared
            .slots()
            .bound
            .iter()
            .find(|b| !b.retired && b.name == name)
            .map(|b| b.addr)
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

/// Background upkeep: ends expired outlier ejections every
/// [`EJECTION_SWEEP_INTERVAL`] and closes idle upstream connections every
/// [`POOL_SWEEP_INTERVAL`].
async fn maintenance(shared: Weak<Shared>, token: CancellationToken) {
    let mut last_pool_sweep = tokio::time::Instant::now();
    let mut last_cert_check = tokio::time::Instant::now();
    loop {
        tokio::select! {
            () = token.cancelled() => return,
            () = tokio::time::sleep(EJECTION_SWEEP_INTERVAL) => {}
        }
        let Some(shared) = shared.upgrade() else {
            return;
        };
        for c in &shared.runtime.load().clusters {
            c.balancer.sweep_ejections();
        }
        if last_cert_check.elapsed() >= CERT_CHECK_INTERVAL {
            shared.reload_certificates();
            last_cert_check = tokio::time::Instant::now();
        }
        if last_pool_sweep.elapsed() >= POOL_SWEEP_INTERVAL {
            shared.pools.evict_idle();
            last_pool_sweep = tokio::time::Instant::now();
        }
    }
}

/// HTTP/2 flow-control windows recommended by Envoy for edge proxies.
const H2_STREAM_WINDOW: u32 = 64 * 1024;
const H2_CONNECTION_WINDOW: u32 = 1024 * 1024;
/// Streams a client may open and reset before we see them, after which the
/// connection is closed with ENHANCE_YOUR_CALM (HTTP/2 Rapid Reset, CVE-2023-44487).
const H2_MAX_PENDING_RESETS: usize = 20;
/// Streams we may reset because of client protocol errors before closing the
/// connection ("MadeYouReset", CVE-2025-8671).
const H2_MAX_LOCAL_ERROR_RESETS: usize = 1024;

/// A connection builder applying a listener's limits and timeouts (ADR 0016).
fn connection_builder(listener: &Listener) -> auto::Builder<TokioExecutor> {
    let limits = listener.limits();
    let timeouts = listener.timeouts();
    let header_bytes = limits.max_request_headers_bytes();
    let mut b = auto::Builder::new(TokioExecutor::new());
    b.http1()
        .timer(TokioTimer::new())
        .header_read_timeout(timeouts.header_read())
        .keep_alive(true)
        // hyper refuses buffers under 8 KiB; validation enforces the floor too.
        .max_buf_size(header_bytes.max(8 * 1024))
        .max_headers(limits.max_request_headers());
    b.http2()
        .timer(TokioTimer::new())
        .max_concurrent_streams(limits.h2_max_concurrent_streams())
        .max_header_list_size(u32::try_from(header_bytes).unwrap_or(u32::MAX))
        .max_pending_accept_reset_streams(H2_MAX_PENDING_RESETS)
        .max_local_error_reset_streams(H2_MAX_LOCAL_ERROR_RESETS)
        .initial_stream_window_size(H2_STREAM_WINDOW)
        .initial_connection_window_size(H2_CONNECTION_WINDOW)
        .keep_alive_interval(timeouts.h2_keep_alive())
        .keep_alive_timeout(timeouts.h2_keep_alive());
    b
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
    // Rebuilt only when a reload changes this listener's settings.
    let mut cached: Option<(Arc<ListenerRuntime>, auto::Builder<TokioExecutor>)> = None;

    loop {
        tokio::select! {
            () = token.cancelled() => break,
            accepted = tcp.accept() => match accepted {
                Ok((stream, peer)) => {
                    let runtime = shared.runtime.load();
                    let Some(listener) = runtime.listeners.get(index).cloned().flatten() else {
                        // Removed from the config: refuse until restart.
                        drop(stream);
                        continue;
                    };
                    let scheme = if listener.config.tls().is_some() { "https" } else { "http" };
                    let stats = Arc::clone(&listener.stats);
                    let proxy_protocol = listener.config.proxy_protocol();
                    let tls = listener.tls.as_ref().map(|t| Arc::clone(&t.config));
                    // Redirects keep the port clients used, not the bound one.
                    let port = listener.config.advertised_port().unwrap_or(addr.port());
                    let builder = match &cached {
                        Some((l, b)) if Arc::ptr_eq(l, &listener) => b,
                        _ => {
                            let b = connection_builder(&listener.config);
                            &cached.insert((listener, b)).1
                        }
                    };
                    let from = Downstream {
                        listener: index,
                        port,
                        peer,
                        scheme,
                    };
                    let accepted = Accepted { from, proxy_protocol, tls, stats };
                    serve(&shared, builder, &graceful, accepted, stream);
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

/// Per-connection settings fixed at accept time.
struct Accepted {
    from: Downstream,
    proxy_protocol: bool,
    tls: Option<Arc<rustls::ServerConfig>>,
    stats: Arc<ListenerStats>,
}

fn serve(
    shared: &Arc<Shared>,
    builder: &auto::Builder<TokioExecutor>,
    graceful: &GracefulShutdown,
    accepted: Accepted,
    mut stream: TcpStream,
) {
    let Accepted {
        mut from,
        proxy_protocol,
        tls,
        stats,
    } = accepted;
    if let Err(e) = stream.set_nodelay(true) {
        debug!(peer = %from.peer, error = %e, "set_nodelay failed");
    }
    let shared = Arc::clone(shared);
    let builder = builder.clone();
    let watcher = graceful.watcher();
    tokio::spawn(async move {
        let _open = stats.open_connection();
        // PROXY header first (the NLB sends it before anything else), then TLS.
        if proxy_protocol {
            match proxy_protocol::read_header(&mut stream, from.peer).await {
                Ok(client) => from.peer = client,
                Err(e) => {
                    stats.proxy_protocol_errors.inc();
                    debug!(peer = %from.peer, error = %e, "rejected connection without a valid PROXY header");
                    return;
                }
            }
        }
        let tls_errors = Arc::clone(&stats);
        let service = service_fn(move |req: Request<Incoming>| {
            // One load per request: the request keeps this snapshot until it completes.
            let runtime = shared.runtime.load_full();
            let telemetry = Arc::clone(&shared.telemetry);
            let shared = Arc::clone(&shared);
            let http1 = req.version() < Version::HTTP_2;
            async move {
                let mut resp = handle(runtime, telemetry, from, req).await?;
                if http1
                    && shared.draining.load(Ordering::Relaxed)
                    && resp.status() != StatusCode::SWITCHING_PROTOCOLS
                {
                    resp.headers_mut()
                        .insert(header::CONNECTION, HeaderValue::from_static("close"));
                }
                Ok::<_, Infallible>(resp)
            }
        });
        let result = match tls {
            None => {
                let conn = builder.serve_connection_with_upgrades(TokioIo::new(stream), service);
                watcher.watch(conn.into_owned()).await
            }
            Some(config) => {
                let acceptor = tokio_rustls::TlsAcceptor::from(config);
                let tls_stream =
                    match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
                        Ok(Ok(s)) => s,
                        Ok(Err(e)) => {
                            tls_errors.tls_errors.inc();
                            debug!(peer = %from.peer, error = %e, "TLS handshake failed");
                            return;
                        }
                        Err(_) => {
                            tls_errors.tls_errors.inc();
                            debug!(peer = %from.peer, "TLS handshake timed out");
                            return;
                        }
                    };
                let conn =
                    builder.serve_connection_with_upgrades(TokioIo::new(tls_stream), service);
                watcher.watch(conn.into_owned()).await
            }
        };
        if let Err(e) = result {
            debug!(peer = %from.peer, error = %e, "connection closed with error");
        }
    });
}
