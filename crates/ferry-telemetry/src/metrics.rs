//! Prometheus metrics (ADR 0027).
//!
//! The request path never looks a metric up. When a snapshot is installed,
//! the proxy asks [`Metrics`] for the stats of each listener, route and
//! cluster ([`Metrics::listener`], [`Metrics::route`], [`Metrics::cluster`])
//! and keeps the returned `Arc`s in its runtime. Requests then only bump
//! relaxed atomics. The same name always returns the same `Arc`, so counters
//! keep counting across reloads; [`Metrics::retain`] forgets names a snapshot
//! no longer has.
//!
//! Gauges that describe live state (endpoint health, pool sizes, breaker
//! usage) are not stored here: the proxy registers a [`Collector`] that reads
//! them from its runtime at scrape time, so they can never go stale.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use prometheus_client::collector::Collector;
use prometheus_client::encoding::{DescriptorEncoder, EncodeMetric, MetricEncoder, NoLabelSet};
use prometheus_client::metrics::MetricType;
use prometheus_client::registry::Registry;

use crate::histogram::{Histogram, OVERHEAD_BUCKETS, REQUEST_BUCKETS};
use crate::process::ProcessCollector;

/// Label value used for requests that matched no route.
pub const NO_ROUTE: &str = "-";

/// Content type of [`Metrics::encode`] output.
pub const CONTENT_TYPE: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";

/// Status-class label values, indexed by [`class_index`].
pub const CLASSES: [&str; 5] = ["1xx", "2xx", "3xx", "4xx", "5xx"];

/// Index into [`CLASSES`] for an HTTP status code.
#[must_use]
pub fn class_index(status: u16) -> usize {
    usize::from(status / 100).clamp(1, 5) - 1
}

/// gRPC status codes 0–16, by number.
pub const GRPC_CODES: [&str; 17] = [
    "OK",
    "CANCELLED",
    "UNKNOWN",
    "INVALID_ARGUMENT",
    "DEADLINE_EXCEEDED",
    "NOT_FOUND",
    "ALREADY_EXISTS",
    "PERMISSION_DENIED",
    "RESOURCE_EXHAUSTED",
    "FAILED_PRECONDITION",
    "ABORTED",
    "OUT_OF_RANGE",
    "UNIMPLEMENTED",
    "INTERNAL",
    "UNAVAILABLE",
    "DATA_LOSS",
    "UNAUTHENTICATED",
];

/// Why Ferry answered a request itself instead of with an upstream response.
///
/// The flags follow Envoy's access-log response flags where one exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Reason {
    /// No route matched (404).
    NoRoute,
    /// The route's rate limit refused it (429).
    RateLimited,
    /// No endpoint of the cluster was eligible (503).
    NoHealthyUpstream,
    /// A circuit breaker was full (503).
    Overflow,
    /// Connecting to or exchanging with the upstream failed (502).
    UpstreamFailure,
    /// The route or gRPC deadline expired before a response (504).
    Timeout,
    /// The request body exceeded the listener limit (413).
    BodyTooLarge,
    /// The client stalled while sending its body (408).
    BodyTimeout,
    /// The request could not be forwarded as sent (400).
    BadRequest,
    /// The configuration cannot serve it: zero weights, a removed listener (500/503).
    Misconfigured,
}

impl Reason {
    /// Every reason, in a fixed order.
    pub const ALL: [Self; 10] = [
        Self::NoRoute,
        Self::RateLimited,
        Self::NoHealthyUpstream,
        Self::Overflow,
        Self::UpstreamFailure,
        Self::Timeout,
        Self::BodyTooLarge,
        Self::BodyTimeout,
        Self::BadRequest,
        Self::Misconfigured,
    ];

    /// Label value, e.g. `no_route`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::NoRoute => "no_route",
            Self::RateLimited => "rate_limited",
            Self::NoHealthyUpstream => "no_healthy_upstream",
            Self::Overflow => "overflow",
            Self::UpstreamFailure => "upstream_failure",
            Self::Timeout => "timeout",
            Self::BodyTooLarge => "body_too_large",
            Self::BodyTimeout => "body_timeout",
            Self::BadRequest => "bad_request",
            Self::Misconfigured => "misconfigured",
        }
    }

    /// Short access-log flag, e.g. `NR`.
    #[must_use]
    pub fn flag(self) -> &'static str {
        match self {
            Self::NoRoute => "NR",
            Self::RateLimited => "RL",
            Self::NoHealthyUpstream => "UH",
            Self::Overflow => "UO",
            Self::UpstreamFailure => "UF",
            Self::Timeout => "UT",
            Self::BodyTooLarge => "BTL",
            Self::BodyTimeout => "BT",
            Self::BadRequest => "BR",
            Self::Misconfigured => "MC",
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|r| *r == self).unwrap_or(0)
    }
}

/// A monotonically increasing counter.
#[derive(Debug, Default)]
pub struct Counter(AtomicU64);

impl Counter {
    /// Adds one.
    pub fn inc(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    /// Adds `n`.
    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }

    /// Current value.
    #[must_use]
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Per-listener stats.
#[derive(Debug)]
pub struct ListenerStats {
    /// Connections accepted.
    pub connections: Counter,
    /// Connections open now.
    pub active_connections: AtomicI64,
    /// TLS handshakes that failed or timed out.
    pub tls_errors: Counter,
    /// Connections dropped for a missing or malformed PROXY header.
    pub proxy_protocol_errors: Counter,
    /// Connections closed on accept because the listener was full.
    pub overflows: Counter,
    /// Responses whose body was not delivered in full (client went away or
    /// the stream broke).
    pub incomplete: Counter,
    /// Time Ferry itself added to each request, up to response headers.
    pub overhead: Histogram,
    local_replies: [Counter; Reason::ALL.len()],
}

impl Default for ListenerStats {
    fn default() -> Self {
        Self {
            connections: Counter::default(),
            active_connections: AtomicI64::new(0),
            tls_errors: Counter::default(),
            proxy_protocol_errors: Counter::default(),
            overflows: Counter::default(),
            incomplete: Counter::default(),
            overhead: Histogram::new(OVERHEAD_BUCKETS),
            local_replies: Default::default(),
        }
    }
}

impl ListenerStats {
    /// Counts a local reply.
    pub fn local_reply(&self, reason: Reason) {
        if let Some(c) = self.local_replies.get(reason.index()) {
            c.inc();
        }
    }

    /// Local replies so far for `reason`.
    #[must_use]
    pub fn local_replies(&self, reason: Reason) -> u64 {
        self.local_replies
            .get(reason.index())
            .map_or(0, Counter::get)
    }

    /// Marks a connection open; the guard marks it closed when dropped.
    #[must_use]
    pub fn open_connection(self: &Arc<Self>) -> ConnectionGuard {
        self.connections.inc();
        self.active_connections.fetch_add(1, Ordering::Relaxed);
        ConnectionGuard(Arc::clone(self))
    }

    /// Keeps an upgraded connection (a WebSocket tunnel) counted as open
    /// after the HTTP server hands it over, without counting a new
    /// connection. The guard marks it closed when dropped.
    #[must_use]
    pub fn hold_upgraded(self: &Arc<Self>) -> ConnectionGuard {
        self.active_connections.fetch_add(1, Ordering::Relaxed);
        ConnectionGuard(Arc::clone(self))
    }
}

/// An open downstream connection; see [`ListenerStats::open_connection`].
#[derive(Debug)]
pub struct ConnectionGuard(Arc<ListenerStats>);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.active_connections.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Per-route stats (RED: rate and errors by status class, duration).
#[derive(Debug)]
pub struct RouteStats {
    requests: [Counter; 5],
    grpc: [Counter; 17],
    /// Whole-request duration: arrival to the last response byte.
    pub duration: Histogram,
}

impl Default for RouteStats {
    fn default() -> Self {
        Self {
            requests: Default::default(),
            grpc: Default::default(),
            duration: Histogram::new(REQUEST_BUCKETS),
        }
    }
}

impl RouteStats {
    /// Counts a finished request and its duration.
    pub fn record(&self, status: u16, duration: Duration) {
        if let Some(c) = self.requests.get(class_index(status)) {
            c.inc();
        }
        self.duration.observe(duration);
    }

    /// Counts a gRPC status code (unknown codes count as `UNKNOWN`).
    pub fn record_grpc(&self, code: u8) {
        let i = if usize::from(code) < GRPC_CODES.len() {
            usize::from(code)
        } else {
            2
        };
        if let Some(c) = self.grpc.get(i) {
            c.inc();
        }
    }

    /// Requests so far with a status in `class` (0 = 1xx … 4 = 5xx).
    #[must_use]
    pub fn requests(&self, class: usize) -> u64 {
        self.requests.get(class).map_or(0, Counter::get)
    }
}

/// Which circuit breaker refused a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Breaker {
    /// `max_connections`.
    Connections,
    /// `max_pending_requests`.
    Pending,
    /// `max_requests`.
    Requests,
    /// `max_retries`.
    Retries,
}

const BREAKERS: [&str; 4] = ["connections", "pending", "requests", "retries"];

/// Per-cluster upstream stats.
#[derive(Debug)]
pub struct ClusterStats {
    responses: [Counter; 5],
    /// Time from sending each attempt to its response headers.
    pub upstream_latency: Histogram,
    /// Attempts that could not get a connection.
    pub connect_failures: Counter,
    /// Attempts whose connection broke before response headers.
    pub resets: Counter,
    /// Attempts that hit the per-try timeout.
    pub attempt_timeouts: Counter,
    /// Attempts after the first, for any reason.
    pub retries: Counter,
    /// Policy retries refused because the retry budget was spent.
    pub budget_exhausted: Counter,
    overflow: [Counter; 4],
}

impl Default for ClusterStats {
    fn default() -> Self {
        Self {
            responses: Default::default(),
            upstream_latency: Histogram::new(REQUEST_BUCKETS),
            connect_failures: Counter::default(),
            resets: Counter::default(),
            attempt_timeouts: Counter::default(),
            retries: Counter::default(),
            budget_exhausted: Counter::default(),
            overflow: Default::default(),
        }
    }
}

impl ClusterStats {
    /// Counts an upstream response to one attempt.
    pub fn response(&self, status: u16, latency: Duration) {
        if let Some(c) = self.responses.get(class_index(status)) {
            c.inc();
        }
        self.upstream_latency.observe(latency);
    }

    /// Counts a request refused by a circuit breaker.
    pub fn overflow(&self, breaker: Breaker) {
        if let Some(c) = self.overflow.get(breaker as usize) {
            c.inc();
        }
    }

    /// Upstream responses so far in `class` (0 = 1xx … 4 = 5xx).
    #[must_use]
    pub fn responses(&self, class: usize) -> u64 {
        self.responses.get(class).map_or(0, Counter::get)
    }
}

/// Proxy-wide stats.
#[derive(Debug, Default)]
pub struct ProxyStats {
    /// Snapshots installed after startup.
    pub reloads_ok: Counter,
    /// Config changes rejected (unparseable, invalid, or unloadable certificates).
    pub reloads_failed: Counter,
    /// Unix time of the last successful load, in seconds.
    pub last_reload: AtomicU64,
    /// Access-log lines dropped because the writer fell behind.
    pub access_log_dropped: Arc<AtomicU64>,
    /// The snapshot stream to the controller; see [`ControllerLink`].
    pub controller: ControllerLink,
}

/// A proxy's link to the controller (Kubernetes mode only).
#[derive(Debug, Default)]
pub struct ControllerLink {
    /// 0 = no controller (file config), 1 = disconnected, 2 = connected.
    state: AtomicU8,
    /// Name and `notAfter` (Unix seconds) of the client certificate the
    /// proxy presents to the controller.
    pub certificate: OnceLock<(String, i64)>,
}

impl ControllerLink {
    /// Records whether the stream is up. The first call marks the proxy as
    /// following a controller, which makes `ferry_controller_connected`
    /// appear.
    pub fn set_connected(&self, up: bool) {
        self.state.store(if up { 2 } else { 1 }, Ordering::Relaxed);
    }

    /// `None` without a controller, else whether the stream is up.
    #[must_use]
    pub fn connected(&self) -> Option<bool> {
        match self.state.load(Ordering::Relaxed) {
            0 => None,
            s => Some(s == 2),
        }
    }
}

impl ProxyStats {
    /// Records the outcome of a reload.
    pub fn reload(&self, ok: bool) {
        if ok {
            self.reloads_ok.inc();
            self.mark_loaded();
        } else {
            self.reloads_failed.inc();
        }
    }
}

impl ProxyStats {
    /// Records that a config was loaded now (startup or reload).
    pub fn mark_loaded(&self) {
        self.last_reload.store(unix_now(), Ordering::Relaxed);
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

type RouteKey = (String, String);

#[derive(Debug, Default)]
struct Families {
    listeners: Mutex<HashMap<String, Arc<ListenerStats>>>,
    routes: Mutex<HashMap<RouteKey, Arc<RouteStats>>>,
    clusters: Mutex<HashMap<String, Arc<ClusterStats>>>,
    proxy: ProxyStats,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Sorted copy of a map's entries, so scrapes are stable and the lock is
/// held only for the copy.
fn entries<K: Clone + Ord, V>(m: &Mutex<HashMap<K, Arc<V>>>) -> Vec<(K, Arc<V>)> {
    let mut v: Vec<_> = lock(m)
        .iter()
        .map(|(k, s)| (k.clone(), Arc::clone(s)))
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

/// The metrics registry of one process.
#[derive(Debug)]
pub struct Metrics {
    families: Arc<Families>,
    registry: Mutex<Registry>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    /// A registry with Ferry's request metrics and process metrics.
    #[must_use]
    pub fn new() -> Self {
        let families = Arc::new(Families::default());
        let mut registry = Registry::default();
        registry.register_collector(Box::new(Arc::clone(&families)));
        registry.register_collector(Box::new(ProcessCollector::new()));
        Self {
            families,
            registry: Mutex::new(registry),
        }
    }

    /// Adds a collector that is asked for metrics on every scrape.
    pub fn register_collector(&self, collector: Box<dyn Collector>) {
        lock(&self.registry).register_collector(collector);
    }

    /// Stats for listener `name`, created on first use.
    #[must_use]
    pub fn listener(&self, name: &str) -> Arc<ListenerStats> {
        Arc::clone(
            lock(&self.families.listeners)
                .entry(name.to_owned())
                .or_default(),
        )
    }

    /// Stats for route `route` of `listener` ([`NO_ROUTE`] for unmatched requests).
    #[must_use]
    pub fn route(&self, listener: &str, route: &str) -> Arc<RouteStats> {
        Arc::clone(
            lock(&self.families.routes)
                .entry((listener.to_owned(), route.to_owned()))
                .or_default(),
        )
    }

    /// Stats for cluster `name`, created on first use.
    #[must_use]
    pub fn cluster(&self, name: &str) -> Arc<ClusterStats> {
        Arc::clone(
            lock(&self.families.clusters)
                .entry(name.to_owned())
                .or_default(),
        )
    }

    /// Proxy-wide stats.
    #[must_use]
    pub fn proxy(&self) -> &ProxyStats {
        &self.families.proxy
    }

    /// Forgets stats for names not listed, so removed routes and clusters stop
    /// being exported. Requests still holding them finish harmlessly.
    pub fn retain(
        &self,
        listeners: &HashSet<String>,
        routes: &HashSet<(String, String)>,
        clusters: &HashSet<String>,
    ) {
        lock(&self.families.listeners).retain(|k, _| listeners.contains(k));
        lock(&self.families.routes).retain(|k, _| k.1 == NO_ROUTE || routes.contains(k));
        lock(&self.families.clusters).retain(|k, _| clusters.contains(k));
    }

    /// Renders every metric in the OpenMetrics text format ([`CONTENT_TYPE`]).
    ///
    /// # Errors
    ///
    /// Only if a collector fails to write, which none of Ferry's do.
    pub fn encode(&self) -> Result<String, std::fmt::Error> {
        let mut out = String::with_capacity(16 * 1024);
        prometheus_client::encoding::text::encode(&mut out, &lock(&self.registry))?;
        Ok(out)
    }
}

/// Writes one metric family: a descriptor, then one sample per label set.
/// Helpers for collectors, including the proxy's.
pub mod encode {
    use super::{DescriptorEncoder, EncodeMetric, MetricEncoder, MetricType, NoLabelSet};
    use prometheus_client::encoding::EncodeLabelSet;
    use prometheus_client::metrics::gauge::ConstGauge;

    /// A family being written.
    #[derive(Debug)]
    pub struct Family<'a>(MetricEncoder<'a>);

    impl<'a> Family<'a> {
        /// Starts a family. Counter names omit `_total`; it is appended.
        ///
        /// # Errors
        ///
        /// Propagates writer errors.
        pub fn start(
            encoder: &'a mut DescriptorEncoder<'_>,
            name: &'a str,
            help: &str,
            kind: MetricType,
        ) -> Result<Self, std::fmt::Error> {
            Ok(Self(encoder.encode_descriptor(name, help, None, kind)?))
        }

        /// Writes a counter sample.
        ///
        /// # Errors
        ///
        /// Propagates writer errors.
        pub fn counter(&mut self, labels: &impl EncodeLabelSet, v: u64) -> std::fmt::Result {
            self.0
                .encode_family(labels)?
                .encode_counter::<NoLabelSet, _, u64>(&v, None)
        }

        /// Writes a counter sample with a fractional value (CPU seconds, say).
        ///
        /// # Errors
        ///
        /// Propagates writer errors.
        pub fn counter_f64(&mut self, labels: &impl EncodeLabelSet, v: f64) -> std::fmt::Result {
            self.0
                .encode_family(labels)?
                .encode_counter::<NoLabelSet, _, f64>(&v, None)
        }

        /// Writes a gauge sample.
        ///
        /// # Errors
        ///
        /// Propagates writer errors.
        pub fn gauge(&mut self, labels: &impl EncodeLabelSet, v: f64) -> std::fmt::Result {
            ConstGauge::new(v).encode(self.0.encode_family(labels)?)
        }

        /// Writes an integer gauge sample.
        ///
        /// # Errors
        ///
        /// Propagates writer errors.
        pub fn gauge_i64(&mut self, labels: &impl EncodeLabelSet, v: i64) -> std::fmt::Result {
            ConstGauge::new(v).encode(self.0.encode_family(labels)?)
        }

        /// Writes the only sample of an unlabelled gauge.
        ///
        /// # Errors
        ///
        /// Propagates writer errors.
        pub fn single_gauge(mut self, v: f64) -> std::fmt::Result {
            self.0.encode_gauge(&v)
        }

        /// Writes the only sample of an unlabelled counter.
        ///
        /// # Errors
        ///
        /// Propagates writer errors.
        pub fn single_counter(mut self, v: u64) -> std::fmt::Result {
            self.0.encode_counter::<NoLabelSet, _, u64>(&v, None)
        }

        /// Writes the only sample of an unlabelled fractional counter.
        ///
        /// # Errors
        ///
        /// Propagates writer errors.
        pub fn single_counter_f64(mut self, v: f64) -> std::fmt::Result {
            self.0.encode_counter::<NoLabelSet, _, f64>(&v, None)
        }

        /// Writes a sample of any metric (a histogram, say).
        ///
        /// # Errors
        ///
        /// Propagates writer errors.
        pub fn metric(
            &mut self,
            labels: &impl EncodeLabelSet,
            m: &impl EncodeMetric,
        ) -> std::fmt::Result {
            m.encode(self.0.encode_family(labels)?)
        }
    }
}

use encode::Family;

/// Name, help and getter of a per-object counter family.
type CounterSpec<T> = (&'static str, &'static str, fn(&T) -> u64);

impl Collector for Families {
    fn encode(&self, mut e: DescriptorEncoder<'_>) -> std::fmt::Result {
        let listeners = entries(&self.listeners);
        let routes = entries(&self.routes);
        let clusters = entries(&self.clusters);

        let mut f = Family::start(
            &mut e,
            "ferry_build_info",
            "Ferry version.",
            MetricType::Gauge,
        )?;
        f.gauge_i64(&[("version", env!("CARGO_PKG_VERSION"))], 1)?;

        // Downstream, per route.
        let mut f = Family::start(
            &mut e,
            "ferry_requests",
            "Downstream requests finished, by route and status class.",
            MetricType::Counter,
        )?;
        for ((l, r), s) in &routes {
            for (i, class) in CLASSES.iter().enumerate() {
                f.counter(
                    &[
                        ("listener", l.as_str()),
                        ("route", r),
                        ("code_class", class),
                    ],
                    s.requests(i),
                )?;
            }
        }
        let mut f = Family::start(
            &mut e,
            "ferry_request_duration_seconds",
            "Downstream request duration, from arrival to the last response byte.",
            MetricType::Histogram,
        )?;
        for ((l, r), s) in &routes {
            f.metric(&[("listener", l.as_str()), ("route", r)], &s.duration)?;
        }
        let mut f = Family::start(
            &mut e,
            "ferry_grpc_responses",
            "gRPC responses by route and grpc-status.",
            MetricType::Counter,
        )?;
        for ((l, r), s) in &routes {
            for (i, code) in GRPC_CODES.iter().enumerate() {
                let n = s.grpc.get(i).map_or(0, Counter::get);
                if n > 0 {
                    f.counter(
                        &[("listener", l.as_str()), ("route", r), ("grpc_code", code)],
                        n,
                    )?;
                }
            }
        }

        // Downstream, per listener.
        let mut f = Family::start(
            &mut e,
            "ferry_request_overhead_seconds",
            "Latency Ferry added to each request: time to response headers minus time spent waiting on upstreams.",
            MetricType::Histogram,
        )?;
        for (l, s) in &listeners {
            f.metric(&[("listener", l.as_str())], &s.overhead)?;
        }
        let mut f = Family::start(
            &mut e,
            "ferry_local_replies",
            "Requests Ferry answered itself, by reason.",
            MetricType::Counter,
        )?;
        for (l, s) in &listeners {
            for r in Reason::ALL {
                f.counter(
                    &[("listener", l.as_str()), ("reason", r.name())],
                    s.local_replies(r),
                )?;
            }
        }
        let listener_counters: [CounterSpec<ListenerStats>; 5] = [
            (
                "ferry_downstream_connections",
                "Downstream connections accepted.",
                |s| s.connections.get(),
            ),
            (
                "ferry_downstream_tls_errors",
                "Downstream TLS handshakes that failed or timed out.",
                |s| s.tls_errors.get(),
            ),
            (
                "ferry_downstream_proxy_protocol_errors",
                "Downstream connections dropped for a bad PROXY header.",
                |s| s.proxy_protocol_errors.get(),
            ),
            (
                "ferry_downstream_connection_overflows",
                "Downstream connections closed on accept because the listener held max_connections.",
                |s| s.overflows.get(),
            ),
            (
                "ferry_downstream_incomplete_responses",
                "Responses not delivered in full (client disconnect or stream error).",
                |s| s.incomplete.get(),
            ),
        ];
        for (name, help, get) in listener_counters {
            let mut f = Family::start(&mut e, name, help, MetricType::Counter)?;
            for (l, s) in &listeners {
                f.counter(&[("listener", l.as_str())], get(s))?;
            }
        }
        let mut f = Family::start(
            &mut e,
            "ferry_downstream_active_connections",
            "Downstream connections open now.",
            MetricType::Gauge,
        )?;
        for (l, s) in &listeners {
            f.gauge_i64(
                &[("listener", l.as_str())],
                s.active_connections.load(Ordering::Relaxed),
            )?;
        }

        // Upstream, per cluster.
        let mut f = Family::start(
            &mut e,
            "ferry_upstream_responses",
            "Upstream responses to each attempt, by cluster and status class.",
            MetricType::Counter,
        )?;
        for (c, s) in &clusters {
            for (i, class) in CLASSES.iter().enumerate() {
                f.counter(
                    &[("cluster", c.as_str()), ("code_class", class)],
                    s.responses(i),
                )?;
            }
        }
        let mut f = Family::start(
            &mut e,
            "ferry_upstream_latency_seconds",
            "Time from sending an upstream attempt to its response headers.",
            MetricType::Histogram,
        )?;
        for (c, s) in &clusters {
            f.metric(&[("cluster", c.as_str())], &s.upstream_latency)?;
        }
        let cluster_counters: [CounterSpec<ClusterStats>; 5] = [
            (
                "ferry_upstream_connect_failures",
                "Upstream attempts that could not get a connection.",
                |s| s.connect_failures.get(),
            ),
            (
                "ferry_upstream_resets",
                "Upstream attempts whose connection broke before response headers.",
                |s| s.resets.get(),
            ),
            (
                "ferry_upstream_attempt_timeouts",
                "Upstream attempts that hit the per-try timeout.",
                |s| s.attempt_timeouts.get(),
            ),
            (
                "ferry_upstream_retries",
                "Upstream attempts after the first.",
                |s| s.retries.get(),
            ),
            (
                "ferry_retry_budget_exhausted",
                "Policy retries refused because the retry budget was spent.",
                |s| s.budget_exhausted.get(),
            ),
        ];
        for (name, help, get) in cluster_counters {
            let mut f = Family::start(&mut e, name, help, MetricType::Counter)?;
            for (c, s) in &clusters {
                f.counter(&[("cluster", c.as_str())], get(s))?;
            }
        }
        let mut f = Family::start(
            &mut e,
            "ferry_circuit_breaker_overflows",
            "Requests refused by a full circuit breaker.",
            MetricType::Counter,
        )?;
        for (c, s) in &clusters {
            for (i, b) in BREAKERS.iter().enumerate() {
                let n = s.overflow.get(i).map_or(0, Counter::get);
                f.counter(&[("cluster", c.as_str()), ("breaker", b)], n)?;
            }
        }

        // Proxy-wide.
        let p = &self.proxy;
        let mut f = Family::start(
            &mut e,
            "ferry_config_reloads",
            "Config reloads after startup, by result.",
            MetricType::Counter,
        )?;
        f.counter(&[("result", "success")], p.reloads_ok.get())?;
        f.counter(&[("result", "failure")], p.reloads_failed.get())?;
        let f = Family::start(
            &mut e,
            "ferry_config_last_reload_success_timestamp_seconds",
            "Unix time of the last successful config load.",
            MetricType::Gauge,
        )?;
        #[allow(clippy::cast_precision_loss)]
        f.single_gauge(p.last_reload.load(Ordering::Relaxed) as f64)?;
        let f = Family::start(
            &mut e,
            "ferry_access_log_dropped",
            "Access-log lines dropped because the writer fell behind.",
            MetricType::Counter,
        )?;
        f.single_counter(p.access_log_dropped.load(Ordering::Relaxed))?;
        if let Some(up) = p.controller.connected() {
            Family::start(
                &mut e,
                "ferry_controller_connected",
                "1 while the snapshot stream to the controller is up.",
                MetricType::Gauge,
            )?
            .single_gauge(if up { 1.0 } else { 0.0 })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(text: &str, series: &str) -> Option<f64> {
        text.lines()
            .find_map(|l| l.strip_prefix(series)?.strip_prefix(' ')?.parse().ok())
    }

    #[test]
    fn controller_link_appears_once_a_controller_is_followed() {
        let m = Metrics::new();
        let link = &m.proxy().controller;
        assert_eq!(link.connected(), None);
        link.set_connected(false);
        let text = m.encode().unwrap();
        assert_eq!(sample(&text, "ferry_controller_connected"), Some(0.0));
        link.set_connected(true);
        let text = m.encode().unwrap();
        assert_eq!(sample(&text, "ferry_controller_connected"), Some(1.0));
        assert_eq!(link.connected(), Some(true));
    }

    #[test]
    fn classes() {
        assert_eq!(CLASSES[class_index(101)], "1xx");
        assert_eq!(CLASSES[class_index(200)], "2xx");
        assert_eq!(CLASSES[class_index(404)], "4xx");
        assert_eq!(CLASSES[class_index(599)], "5xx");
        assert_eq!(CLASSES[class_index(0)], "1xx");
        assert_eq!(CLASSES[class_index(999)], "5xx");
    }

    #[test]
    fn same_name_same_stats_until_retained_away() {
        let m = Metrics::new();
        let a = m.route("l", "r");
        a.record(200, Duration::from_millis(1));
        assert!(Arc::ptr_eq(&a, &m.route("l", "r")));
        let keep_routes = HashSet::from([("l".to_owned(), "other".to_owned())]);
        m.retain(&HashSet::new(), &keep_routes, &HashSet::new());
        let b = m.route("l", "r");
        assert!(!Arc::ptr_eq(&a, &b));
        assert_eq!(b.requests(1), 0);
    }

    #[test]
    fn unmatched_route_stats_survive_retain() {
        let m = Metrics::new();
        let a = m.route("l", NO_ROUTE);
        m.retain(&HashSet::new(), &HashSet::new(), &HashSet::new());
        assert!(Arc::ptr_eq(&a, &m.route("l", NO_ROUTE)));
    }

    #[test]
    fn encodes_request_metrics() {
        let m = Metrics::new();
        let r = m.route("web", "api");
        r.record(200, Duration::from_millis(3));
        r.record(503, Duration::from_millis(1));
        r.record_grpc(14);
        let l = m.listener("web");
        l.local_reply(Reason::NoHealthyUpstream);
        l.overhead.observe(Duration::from_micros(80));
        let _conn = l.open_connection();
        let c = m.cluster("backend");
        c.response(200, Duration::from_millis(2));
        c.overflow(Breaker::Pending);
        m.proxy().reload(true);
        m.proxy().reload(false);

        let text = m.encode().unwrap();
        let s = |series: &str| {
            sample(&text, series).unwrap_or_else(|| panic!("{series} missing in\n{text}"))
        };
        assert_eq!(
            s(r#"ferry_requests_total{listener="web",route="api",code_class="2xx"}"#),
            1.0
        );
        assert_eq!(
            s(r#"ferry_requests_total{listener="web",route="api",code_class="5xx"}"#),
            1.0
        );
        assert_eq!(
            s(r#"ferry_request_duration_seconds_count{listener="web",route="api"}"#),
            2.0
        );
        assert_eq!(
            s(r#"ferry_request_duration_seconds_bucket{le="0.0025",listener="web",route="api"}"#),
            1.0
        );
        assert_eq!(
            s(r#"ferry_grpc_responses_total{listener="web",route="api",grpc_code="UNAVAILABLE"}"#),
            1.0
        );
        assert_eq!(
            s(r#"ferry_local_replies_total{listener="web",reason="no_healthy_upstream"}"#),
            1.0
        );
        assert_eq!(
            s(r#"ferry_request_overhead_seconds_bucket{le="0.0001",listener="web"}"#),
            1.0
        );
        assert_eq!(
            s(r#"ferry_downstream_active_connections{listener="web"}"#),
            1.0
        );
        assert_eq!(
            s(r#"ferry_upstream_responses_total{cluster="backend",code_class="2xx"}"#),
            1.0
        );
        assert_eq!(
            s(r#"ferry_circuit_breaker_overflows_total{cluster="backend",breaker="pending"}"#),
            1.0
        );
        assert_eq!(s(r#"ferry_config_reloads_total{result="success"}"#), 1.0);
        assert_eq!(s(r#"ferry_config_reloads_total{result="failure"}"#), 1.0);
        assert!(s("ferry_config_last_reload_success_timestamp_seconds") > 1.0e9);
        assert!(
            sample(&text, "ferry_controller_connected").is_none(),
            "absent in file mode"
        );
        assert!(text.contains("process_start_time_seconds"));
        assert!(text.ends_with("# EOF\n"));
    }

    #[test]
    fn connection_guard_decrements() {
        let m = Metrics::new();
        let l = m.listener("x");
        let g = l.open_connection();
        assert_eq!(l.active_connections.load(Ordering::Relaxed), 1);
        drop(g);
        assert_eq!(l.active_connections.load(Ordering::Relaxed), 0);
        assert_eq!(l.connections.get(), 1);
    }

    #[test]
    fn reasons_are_distinct() {
        let names: HashSet<_> = Reason::ALL.iter().map(|r| r.name()).collect();
        let flags: HashSet<_> = Reason::ALL.iter().map(|r| r.flag()).collect();
        assert_eq!(names.len(), Reason::ALL.len());
        assert_eq!(flags.len(), Reason::ALL.len());
        for (i, r) in Reason::ALL.iter().enumerate() {
            assert_eq!(r.index(), i);
        }
    }
}
