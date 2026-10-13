//! Gauges read from the live runtime at scrape time (ADR 0027).
//!
//! Health, ejections, pool sizes, breaker usage and the retry budget already
//! live in atomics owned by the runtime. Reading them when Prometheus scrapes
//! means no copy can go stale and endpoints that leave the config simply stop
//! being exported.

use std::sync::Weak;
use std::time::Instant;

use ferry_telemetry::metrics::encode::Family;
use prometheus_client::collector::Collector;
use prometheus_client::encoding::DescriptorEncoder;
use prometheus_client::metrics::MetricType;

use crate::server::Shared;
use crate::state::ClusterRuntime;

/// Exports the proxy's live state.
#[derive(Debug)]
pub(crate) struct StateCollector {
    shared: Weak<Shared>,
}

impl StateCollector {
    pub(crate) fn new(shared: Weak<Shared>) -> Self {
        Self { shared }
    }
}

fn gauge_u64(v: u64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let f = v as f64;
    f
}

type ClusterGauge = (&'static str, &'static str, fn(&ClusterRuntime) -> f64);

impl Collector for StateCollector {
    fn encode(&self, mut e: DescriptorEncoder<'_>) -> std::fmt::Result {
        let Some(shared) = self.shared.upgrade() else {
            return Ok(());
        };
        let runtime = shared.runtime();
        let now = Instant::now();

        Family::start(
            &mut e,
            "ferry_config_version",
            "Version of the snapshot serving new requests.",
            MetricType::Gauge,
        )?
        .single_gauge(gauge_u64(runtime.snapshot.version().0))?;
        Family::start(
            &mut e,
            "ferry_draining",
            "1 while the proxy is shutting down.",
            MetricType::Gauge,
        )?
        .single_gauge(if shared.is_draining() { 1.0 } else { 0.0 })?;

        let clusters = &runtime.clusters;
        let per_cluster: [ClusterGauge; 6] = [
            ("ferry_cluster_endpoints", "Endpoints configured.", |c| {
                gauge_u64(c.balancer.hosts().len() as u64)
            }),
            (
                "ferry_cluster_healthy_endpoints",
                "Endpoints passing active health checks (all, without checks).",
                |c| {
                    gauge_u64(
                        c.balancer
                            .hosts()
                            .iter()
                            .filter(|h| h.state.is_healthy())
                            .count() as u64,
                    )
                },
            ),
            (
                "ferry_cluster_ejected_endpoints",
                "Endpoints ejected by outlier detection.",
                |c| {
                    gauge_u64(
                        c.balancer
                            .hosts()
                            .iter()
                            .filter(|h| h.state.is_ejected())
                            .count() as u64,
                    )
                },
            ),
            (
                "ferry_cluster_eligible_endpoints",
                "Endpoints receiving traffic now (healthy and not ejected, or all in panic mode).",
                |c| gauge_u64(c.balancer.host_set().len() as u64),
            ),
            (
                "ferry_cluster_panic",
                "1 while too few endpoints are healthy and health is ignored.",
                |c| {
                    if c.balancer.host_set().is_panic() {
                        1.0
                    } else {
                        0.0
                    }
                },
            ),
            (
                "ferry_retry_budget_remaining",
                "Policy retries the cluster's retry budget would allow now.",
                |c| gauge_u64(c.budget.remaining()),
            ),
        ];
        for (name, help, get) in per_cluster {
            let mut f = Family::start(&mut e, name, help, MetricType::Gauge)?;
            for c in clusters {
                f.gauge(&[("cluster", c.config.name())], get(c))?;
            }
        }

        let mut active = Family::start(
            &mut e,
            "ferry_circuit_breaker_active",
            "Slots in use per circuit breaker (connections: open upstream connections).",
            MetricType::Gauge,
        )?;
        for c in clusters {
            let b = &c.breakers;
            for (kind, limit) in [
                ("connections", &b.connections),
                ("pending", &b.pending),
                ("requests", &b.requests),
                ("retries", &b.retries),
            ] {
                active.gauge(
                    &[("cluster", c.config.name()), ("breaker", kind)],
                    f64::from(limit.active()),
                )?;
            }
        }
        let mut max = Family::start(
            &mut e,
            "ferry_circuit_breaker_max",
            "Configured maximum per circuit breaker.",
            MetricType::Gauge,
        )?;
        for c in clusters {
            let b = &c.breakers;
            for (kind, limit) in [
                ("connections", &b.connections),
                ("pending", &b.pending),
                ("requests", &b.requests),
                ("retries", &b.retries),
            ] {
                max.gauge(
                    &[("cluster", c.config.name()), ("breaker", kind)],
                    f64::from(limit.max()),
                )?;
            }
        }

        // Per endpoint.
        let endpoints: Vec<(&str, String, &ferry_lb::Host, usize)> = clusters
            .iter()
            .flat_map(|c| {
                c.balancer
                    .hosts()
                    .iter()
                    .map(move |h| (c.config.name(), h.state.address().to_string(), h))
                    .zip(std::iter::repeat(c))
                    .enumerate()
                    .map(|(i, ((name, addr, h), c))| {
                        (name, addr, h, c.pools.get(i).map_or(0, |p| p.idle_count()))
                    })
            })
            .collect();
        let mut f = Family::start(
            &mut e,
            "ferry_endpoint_healthy",
            "1 if the endpoint passes active health checks.",
            MetricType::Gauge,
        )?;
        for (c, a, h, _) in &endpoints {
            f.gauge(
                &[("cluster", *c), ("endpoint", a.as_str())],
                if h.state.is_healthy() { 1.0 } else { 0.0 },
            )?;
        }
        let mut f = Family::start(
            &mut e,
            "ferry_endpoint_ejected",
            "1 while outlier detection has ejected the endpoint.",
            MetricType::Gauge,
        )?;
        for (c, a, h, _) in &endpoints {
            f.gauge(
                &[("cluster", *c), ("endpoint", a.as_str())],
                if h.state.is_ejected() { 1.0 } else { 0.0 },
            )?;
        }
        let mut f = Family::start(
            &mut e,
            "ferry_endpoint_ejections",
            "Times outlier detection has ejected the endpoint.",
            MetricType::Counter,
        )?;
        for (c, a, h, _) in &endpoints {
            f.counter(
                &[("cluster", *c), ("endpoint", a.as_str())],
                h.state.ejections_total(),
            )?;
        }
        let mut f = Family::start(
            &mut e,
            "ferry_endpoint_active_requests",
            "Requests in flight to the endpoint.",
            MetricType::Gauge,
        )?;
        for (c, a, h, _) in &endpoints {
            f.gauge(
                &[("cluster", *c), ("endpoint", a.as_str())],
                gauge_u64(h.state.active_requests()),
            )?;
        }
        let mut f = Family::start(
            &mut e,
            "ferry_endpoint_latency_ewma_seconds",
            "Peak-EWMA latency estimate used by P2C-EWMA.",
            MetricType::Gauge,
        )?;
        for (c, a, h, _) in &endpoints {
            f.gauge(
                &[("cluster", *c), ("endpoint", a.as_str())],
                h.state.latency_estimate(now) / 1e9,
            )?;
        }
        let mut f = Family::start(
            &mut e,
            "ferry_pool_idle_connections",
            "Idle pooled HTTP/1.1 connections to the endpoint.",
            MetricType::Gauge,
        )?;
        for (c, a, _, idle) in &endpoints {
            f.gauge(
                &[("cluster", *c), ("endpoint", a.as_str())],
                gauge_u64(*idle as u64),
            )?;
        }
        Ok(())
    }
}
