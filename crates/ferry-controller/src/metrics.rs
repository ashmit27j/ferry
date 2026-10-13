//! The controller's Prometheus metrics, served on its admin port (18001).
//!
//! Like the proxy's (ADR 0027), state that already exists is read at scrape
//! time: readiness, leadership, and from the [`Hub`] the published snapshot
//! versions, connected proxies and rejections. Only reconcile passes and
//! Kubernetes write errors are counted as they happen, in
//! [`ControllerStats`].

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use ferry_telemetry::histogram::{Histogram, REQUEST_BUCKETS};
use ferry_telemetry::metrics::encode::Family;
use ferry_telemetry::process::ProcessCollector;
use prometheus_client::collector::Collector;
use prometheus_client::encoding::DescriptorEncoder;
use prometheus_client::metrics::MetricType;
use prometheus_client::registry::Registry;
use tokio::sync::watch;

use crate::server::Hub;

/// A Kubernetes write the controller makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOp {
    /// A status subresource patch.
    Status,
    /// A server-side apply of a fleet object.
    Apply,
    /// Deleting a stale fleet object.
    Delete,
}

impl WriteOp {
    const ALL: [Self; 3] = [Self::Status, Self::Apply, Self::Delete];

    fn name(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Apply => "apply",
            Self::Delete => "delete",
        }
    }
}

/// Counters the reconcile loop updates.
#[derive(Debug)]
pub struct ControllerStats {
    reconciles: AtomicU64,
    reconcile_duration: Histogram,
    invalid_gateways: AtomicU64,
    write_errors: [AtomicU64; 3],
}

impl Default for ControllerStats {
    fn default() -> Self {
        Self {
            reconciles: AtomicU64::new(0),
            reconcile_duration: Histogram::new(REQUEST_BUCKETS),
            invalid_gateways: AtomicU64::new(0),
            write_errors: Default::default(),
        }
    }
}

impl ControllerStats {
    /// Records one reconcile pass and how many Gateways it could not publish.
    pub fn reconciled(&self, took: Duration, invalid_gateways: usize) {
        self.reconciles.fetch_add(1, Ordering::Relaxed);
        self.reconcile_duration.observe(took);
        self.invalid_gateways
            .store(invalid_gateways as u64, Ordering::Relaxed);
    }

    /// Counts a failed Kubernetes write (conflicts that are retried are not
    /// failures).
    pub fn write_failed(&self, op: WriteOp) {
        if let Some(c) = self.write_errors.get(op as usize) {
            c.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Reconcile passes so far.
    #[must_use]
    pub fn reconciles(&self) -> u64 {
        self.reconciles.load(Ordering::Relaxed)
    }
}

/// The controller's registry.
#[derive(Debug)]
pub struct ControllerMetrics {
    registry: Mutex<Registry>,
}

impl ControllerMetrics {
    /// Metrics over the controller's shared state, plus `process_*`.
    #[must_use]
    pub fn new(
        hub: Arc<Hub>,
        stats: Arc<ControllerStats>,
        ready: Arc<AtomicBool>,
        leader: watch::Receiver<bool>,
    ) -> Self {
        let mut registry = Registry::default();
        registry.register_collector(Box::new(StateCollector {
            hub,
            stats,
            ready,
            leader,
        }));
        registry.register_collector(Box::new(ProcessCollector::new()));
        Self {
            registry: Mutex::new(registry),
        }
    }

    /// The OpenMetrics text exposition.
    ///
    /// # Errors
    ///
    /// Propagates formatting errors (none in practice).
    pub fn encode(&self) -> Result<String, std::fmt::Error> {
        let mut out = String::with_capacity(4096);
        let registry = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        prometheus_client::encoding::text::encode(&mut out, &registry)?;
        Ok(out)
    }
}

#[derive(Debug)]
struct StateCollector {
    hub: Arc<Hub>,
    stats: Arc<ControllerStats>,
    ready: Arc<AtomicBool>,
    leader: watch::Receiver<bool>,
}

fn flag(b: bool) -> f64 {
    if b { 1.0 } else { 0.0 }
}

#[allow(clippy::cast_precision_loss)]
fn float(v: u64) -> f64 {
    v as f64
}

impl Collector for StateCollector {
    fn encode(&self, mut e: DescriptorEncoder<'_>) -> std::fmt::Result {
        let mut f = Family::start(
            &mut e,
            "ferry_build_info",
            "Ferry version.",
            MetricType::Gauge,
        )?;
        f.gauge_i64(&[("version", env!("CARGO_PKG_VERSION"))], 1)?;
        Family::start(
            &mut e,
            "ferry_controller_ready",
            "1 once the controller's caches have synced and it reconciles.",
            MetricType::Gauge,
        )?
        .single_gauge(flag(self.ready.load(Ordering::Relaxed)))?;
        Family::start(
            &mut e,
            "ferry_controller_leader",
            "1 while this replica holds the leader Lease (writes status, provisions fleets).",
            MetricType::Gauge,
        )?
        .single_gauge(flag(*self.leader.borrow()))?;

        let s = &self.stats;
        Family::start(
            &mut e,
            "ferry_controller_reconciles",
            "Reconcile passes: translate the cluster, publish snapshots, and (leader) write status and fleets.",
            MetricType::Counter,
        )?
        .single_counter(s.reconciles())?;
        let mut f = Family::start(
            &mut e,
            "ferry_controller_reconcile_duration_seconds",
            "Time one reconcile pass took.",
            MetricType::Histogram,
        )?;
        let unlabelled: [(&str, &str); 0] = [];
        f.metric(&unlabelled, &s.reconcile_duration)?;
        Family::start(
            &mut e,
            "ferry_controller_invalid_gateways",
            "Gateways whose translated config does not validate, so their last snapshot stays in place.",
            MetricType::Gauge,
        )?
        .single_gauge(float(s.invalid_gateways.load(Ordering::Relaxed)))?;
        let mut f = Family::start(
            &mut e,
            "ferry_controller_kube_write_errors",
            "Kubernetes writes that failed, by operation (status, apply, delete).",
            MetricType::Counter,
        )?;
        for op in WriteOp::ALL {
            let n = s
                .write_errors
                .get(op as usize)
                .map_or(0, |c| c.load(Ordering::Relaxed));
            f.counter(&[("op", op.name())], n)?;
        }

        // Per Gateway, from the hub.
        let versions = self.hub.published_versions();
        let mut proxies: BTreeMap<String, u64> = versions.keys().map(|g| (g.clone(), 0)).collect();
        for view in self.hub.proxy_views().values() {
            *proxies.entry(view.gateway.clone()).or_default() += 1;
        }
        let mut f = Family::start(
            &mut e,
            "ferry_controller_snapshot_version",
            "Version of the snapshot published for each Gateway.",
            MetricType::Gauge,
        )?;
        for (g, v) in &versions {
            f.gauge(&[("gateway", g.as_str())], float(*v))?;
        }
        let mut f = Family::start(
            &mut e,
            "ferry_controller_connected_proxies",
            "Proxies streaming snapshots from this replica, by Gateway.",
            MetricType::Gauge,
        )?;
        for (g, n) in &proxies {
            f.gauge(&[("gateway", g.as_str())], float(*n))?;
        }
        let mut f = Family::start(
            &mut e,
            "ferry_controller_snapshot_nacks",
            "Snapshots proxies rejected (NACKed), by Gateway.",
            MetricType::Counter,
        )?;
        for (g, n) in &self.hub.nacks() {
            f.counter(&[("gateway", g.as_str())], *n)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::ObjKey;

    fn sample(text: &str, series: &str) -> Option<f64> {
        text.lines()
            .find_map(|l| l.strip_prefix(series)?.strip_prefix(' ')?.parse().ok())
    }

    #[test]
    fn exports_state_and_counters() {
        let hub = Hub::new();
        let gw = ObjKey::new("apps", "gw");
        let config = ferry_config::Config::new(Vec::new(), Vec::new());
        let version = hub.publish(&gw, &config).unwrap().unwrap();
        let stats = Arc::new(ControllerStats::default());
        let ready = Arc::new(AtomicBool::new(false));
        let (leader_tx, leader_rx) = watch::channel(false);
        let m = ControllerMetrics::new(
            Arc::clone(&hub),
            Arc::clone(&stats),
            Arc::clone(&ready),
            leader_rx,
        );

        let text = m.encode().unwrap();
        assert_eq!(sample(&text, "ferry_controller_ready"), Some(0.0));
        assert_eq!(sample(&text, "ferry_controller_leader"), Some(0.0));
        assert_eq!(
            sample(
                &text,
                r#"ferry_controller_connected_proxies{gateway="apps/gw"}"#
            ),
            Some(0.0)
        );
        assert_eq!(
            sample(
                &text,
                r#"ferry_controller_snapshot_version{gateway="apps/gw"}"#
            ),
            Some(float(version))
        );

        ready.store(true, Ordering::Relaxed);
        leader_tx.send_replace(true);
        stats.reconciled(Duration::from_millis(3), 1);
        stats.write_failed(WriteOp::Status);
        let text = m.encode().unwrap();
        assert_eq!(sample(&text, "ferry_controller_ready"), Some(1.0));
        assert_eq!(sample(&text, "ferry_controller_leader"), Some(1.0));
        assert_eq!(
            sample(&text, "ferry_controller_reconciles_total"),
            Some(1.0)
        );
        assert_eq!(
            sample(&text, "ferry_controller_reconcile_duration_seconds_count{}"),
            Some(1.0),
            "{text}"
        );
        assert_eq!(
            sample(&text, "ferry_controller_invalid_gateways"),
            Some(1.0)
        );
        assert_eq!(
            sample(
                &text,
                r#"ferry_controller_kube_write_errors_total{op="status"}"#
            ),
            Some(1.0)
        );
        assert_eq!(
            sample(
                &text,
                r#"ferry_controller_kube_write_errors_total{op="apply"}"#
            ),
            Some(0.0)
        );
        assert!(text.contains("process_start_time_seconds"));
        assert!(text.ends_with("# EOF\n"));
    }
}
