//! Runtime state derived from a snapshot, and how it survives reloads.
//!
//! A [`Runtime`] pairs an immutable [`Snapshot`] with everything the hot path
//! needs: one router per bound listener and one [`ClusterRuntime`] per cluster
//! (balancer, connection pools, stickiness settings). It is built off the hot
//! path and published with one `ArcSwap::store`; each request loads it once and
//! keeps that `Arc` until it completes, so in-flight requests finish on the
//! snapshot they started with.
//!
//! [`Control`] carries state across reloads (ADR 0014):
//!
//! - a cluster whose config is unchanged keeps its whole `ClusterRuntime`
//!   (balancer cursors, Maglev table, host set); a listener whose routes are
//!   unchanged keeps its router (and so its weighted-split counters);
//! - endpoint state (health, in-flight count, latency EWMA) is keyed by
//!   `(cluster, address)` and reused by any cluster that still lists it;
//! - connection pools are keyed by `(address, protocol)` and reused;
//! - health checkers keep running when their endpoint and settings are
//!   unchanged, and are started or stopped to match the new snapshot.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Weak};

use ferry_config::{
    Cluster, ClusterId, HashPolicy, HealthCheck, HealthCheckKind, LbPolicy, Listener, Snapshot,
    UpstreamProtocol,
};
use ferry_lb::health::{self, HealthSpec, Probe};
use ferry_lb::pool::{Pool, PoolKey, PoolSet, Protocol};
use ferry_lb::{ClusterBalancer, EndpointState, Host, Policy};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::body::UpstreamBody;
use crate::router::Router;

/// Everything a request needs, frozen at one snapshot version.
#[derive(Debug)]
pub(crate) struct Runtime {
    pub(crate) snapshot: Arc<Snapshot>,
    /// One entry per bound listener, in bind order. `None` when the listener
    /// is no longer in the snapshot.
    pub(crate) listeners: Vec<Option<Arc<ListenerRuntime>>>,
    /// One entry per cluster, parallel to `snapshot.clusters()`.
    pub(crate) clusters: Vec<Arc<ClusterRuntime>>,
}

/// One bound listener's settings and route table.
#[derive(Debug)]
pub(crate) struct ListenerRuntime {
    pub(crate) config: Listener,
    pub(crate) router: Arc<Router>,
}

/// One cluster's balancer and pools.
#[derive(Debug)]
pub(crate) struct ClusterRuntime {
    pub(crate) config: Cluster,
    pub(crate) balancer: ClusterBalancer,
    /// Parallel to `balancer.hosts()`.
    pub(crate) pools: Vec<Arc<Pool<UpstreamBody>>>,
}

impl Runtime {
    pub(crate) fn cluster(&self, id: ClusterId) -> Option<&Arc<ClusterRuntime>> {
        self.clusters.get(id.index())
    }
}

/// A listener as bound at startup. Addresses are not rebound on reload.
#[derive(Debug, Clone)]
pub(crate) struct BoundListener {
    pub(crate) name: String,
    pub(crate) addr: SocketAddr,
}

/// Cross-reload state. Only touched on the control path, under a mutex.
#[derive(Debug, Default)]
pub(crate) struct Control {
    endpoints: HashMap<(String, SocketAddr), Arc<EndpointState>>,
    checkers: HashMap<(String, SocketAddr), (HealthSpec, CancellationToken)>,
}

/// Something that can rebuild a cluster's host set by name after a health
/// change; implemented by the proxy's shared state.
pub(crate) trait HealthSink: Send + Sync + 'static {
    fn health_changed(&self, cluster: &str);
}

impl Control {
    /// Builds the runtime for `snapshot`, reusing what `prev` and the registry
    /// already hold, and reconciles health checkers.
    pub(crate) fn build<S: HealthSink>(
        &mut self,
        snapshot: Snapshot,
        prev: Option<&Runtime>,
        bound: &[BoundListener],
        pools: &PoolSet<UpstreamBody>,
        sink: &Weak<S>,
    ) -> Runtime {
        let mut used_endpoints = HashSet::new();
        let mut used_pools: HashSet<PoolKey> = HashSet::new();
        let mut desired_checks: HashMap<(String, SocketAddr), HealthSpec> = HashMap::new();

        let clusters: Vec<Arc<ClusterRuntime>> = snapshot
            .clusters()
            .iter()
            .map(|c| {
                for e in c.endpoints() {
                    used_endpoints.insert((c.name().to_owned(), e.address()));
                    used_pools.insert((e.address(), protocol(c.protocol())));
                    if let Some(hc) = c.health_check() {
                        desired_checks.insert((c.name().to_owned(), e.address()), health_spec(hc));
                    }
                }
                let reused = prev.and_then(|p| {
                    p.clusters
                        .iter()
                        .find(|old| old.config.name() == c.name() && old.config == *c)
                });
                match reused {
                    Some(old) => Arc::clone(old),
                    None => Arc::new(self.cluster_runtime(c, pools)),
                }
            })
            .collect();

        let listeners = bound
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let Some(listener) = snapshot.listener(&b.name) else {
                    warn!(listener = %b.name, "listener removed from config; it now answers 503 until restart");
                    return None;
                };
                if listener.address() != b.addr && listener.address().port() != 0 {
                    warn!(listener = %b.name, bound = %b.addr, configured = %listener.address(),
                        "listener address changed; the new address takes effect on restart");
                }
                let old = prev.and_then(|p| p.listeners.get(i).cloned().flatten());
                if let Some(old) = old.as_ref().filter(|o| o.config == *listener) {
                    return Some(Arc::clone(old));
                }
                // Settings changed; keep the router (and its counters) if the
                // routes did not.
                let router = old
                    .filter(|o| o.config.routes() == listener.routes())
                    .map_or_else(|| Arc::new(Router::new(listener)), |o| Arc::clone(&o.router));
                Some(Arc::new(ListenerRuntime {
                    config: listener.clone(),
                    router,
                }))
            })
            .collect();

        for name in snapshot.listeners().iter().map(|l| l.name()) {
            if !bound.iter().any(|b| b.name == name) {
                warn!(
                    listener = name,
                    "new listener in config; it is bound on restart"
                );
            }
        }

        self.endpoints.retain(|k, _| used_endpoints.contains(k));
        pools.retain(&used_pools);
        self.reconcile_checks(desired_checks, &clusters, sink);

        Runtime {
            snapshot: Arc::new(snapshot),
            listeners,
            clusters,
        }
    }

    fn endpoint(&mut self, cluster: &str, addr: SocketAddr) -> Arc<EndpointState> {
        Arc::clone(
            self.endpoints
                .entry((cluster.to_owned(), addr))
                .or_insert_with(|| Arc::new(EndpointState::new(addr))),
        )
    }

    fn cluster_runtime(&mut self, c: &Cluster, pools: &PoolSet<UpstreamBody>) -> ClusterRuntime {
        let hosts: Vec<Host> = c
            .endpoints()
            .iter()
            .map(|e| {
                let state = self.endpoint(c.name(), e.address());
                if c.health_check().is_none() {
                    // Health checking was removed: nothing would ever mark
                    // these endpoints healthy again.
                    state.set_healthy(true);
                }
                Host {
                    state,
                    weight: e.weight(),
                }
            })
            .collect();
        let pools = c
            .endpoints()
            .iter()
            .map(|e| pools.get(e.address(), protocol(c.protocol())))
            .collect();
        ClusterRuntime {
            config: c.clone(),
            balancer: ClusterBalancer::new(
                hosts,
                policy(c.lb_policy()),
                c.panic_threshold(),
                fastrand::usize(..),
            ),
            pools,
        }
    }

    fn reconcile_checks<S: HealthSink>(
        &mut self,
        desired: HashMap<(String, SocketAddr), HealthSpec>,
        clusters: &[Arc<ClusterRuntime>],
        sink: &Weak<S>,
    ) {
        self.checkers.retain(|key, (spec, token)| {
            let keep = desired.get(key) == Some(spec);
            if !keep {
                debug!(cluster = %key.0, endpoint = %key.1, "stopping health checker");
                token.cancel();
            }
            keep
        });
        for (key, spec) in desired {
            if self.checkers.contains_key(&key) {
                continue;
            }
            let Some(state) = clusters
                .iter()
                .find(|c| c.config.name() == key.0)
                .and_then(|c| {
                    c.balancer
                        .hosts()
                        .iter()
                        .find(|h| h.state.address() == key.1)
                        .map(|h| Arc::clone(&h.state))
                })
            else {
                continue;
            };
            let token = CancellationToken::new();
            let cluster = key.0.clone();
            let sink = Weak::clone(sink);
            health::spawn(
                spec.clone(),
                state,
                Arc::new(move || {
                    if let Some(s) = sink.upgrade() {
                        s.health_changed(&cluster);
                    }
                }),
                token.clone(),
            );
            debug!(cluster = %key.0, endpoint = %key.1, "started health checker");
            self.checkers.insert(key, (spec, token));
        }
    }

    /// Stops every health checker.
    pub(crate) fn shutdown(&mut self) {
        for (_, (_, token)) in self.checkers.drain() {
            token.cancel();
        }
        info!("health checkers stopped");
    }

    #[cfg(test)]
    pub(crate) fn checker_count(&self) -> usize {
        self.checkers.len()
    }
}

fn protocol(p: UpstreamProtocol) -> Protocol {
    match p {
        UpstreamProtocol::Http2 => Protocol::Http2,
        _ => Protocol::Http1,
    }
}

fn policy(p: LbPolicy) -> Policy {
    match p {
        LbPolicy::WeightedRoundRobin => Policy::WeightedRoundRobin,
        LbPolicy::LeastRequest => Policy::LeastRequest,
        LbPolicy::P2cEwma => Policy::P2cEwma,
        LbPolicy::Maglev => Policy::Maglev,
        _ => Policy::RoundRobin,
    }
}

fn health_spec(hc: &HealthCheck) -> HealthSpec {
    let probe = match hc.kind() {
        HealthCheckKind::Grpc { service, authority } => Probe::Grpc {
            service: service.clone(),
            authority: authority.clone(),
        },
        HealthCheckKind::Http {
            path,
            host,
            expected_statuses,
        } => Probe::Http {
            path: path.clone(),
            host: host.clone(),
            expected_statuses: expected_statuses.clone(),
        },
        // Validation only produces the kinds above; treat anything newer as
        // an HTTP check of "/" rather than failing a reload.
        _ => Probe::Http {
            path: "/".into(),
            host: String::new(),
            expected_statuses: vec![200],
        },
    };
    HealthSpec {
        interval: hc.interval(),
        timeout: hc.timeout(),
        unhealthy_threshold: hc.unhealthy_threshold(),
        healthy_threshold: hc.healthy_threshold(),
        probe,
    }
}

/// The hash key for a Maglev cluster. Returns the key hash and, for cookie
/// stickiness without a cookie, the cookie value to set on the response.
pub(crate) fn hash_key(
    policy: Option<&HashPolicy>,
    headers: &http::HeaderMap,
    peer: SocketAddr,
) -> (Option<u64>, Option<String>) {
    use ferry_lb::maglev::hash_key as h;
    match policy {
        Some(HashPolicy::Header(name)) => (headers.get(name).map(|v| h(v.as_bytes())), None),
        Some(HashPolicy::SourceIp) => {
            let hash = match peer.ip() {
                std::net::IpAddr::V4(ip) => h(&ip.octets()),
                std::net::IpAddr::V6(ip) => h(&ip.octets()),
            };
            (Some(hash), None)
        }
        Some(HashPolicy::Cookie { name, .. }) => match find_cookie(headers, name) {
            Some(value) => (Some(h(value.as_bytes())), None),
            None => {
                let value = format!("{:016x}", fastrand::u64(..));
                (Some(h(value.as_bytes())), Some(value))
            }
        },
        _ => (None, None),
    }
}

/// The value of cookie `name` across all `Cookie` headers.
fn find_cookie<'a>(headers: &'a http::HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(http::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|pair| {
            let (k, v) = pair.trim().split_once('=')?;
            (k == name).then_some(v)
        })
}

/// The `Set-Cookie` value for a generated stickiness cookie.
pub(crate) fn set_cookie(policy: Option<&HashPolicy>, value: &str) -> Option<String> {
    let Some(HashPolicy::Cookie { name, ttl, path }) = policy else {
        return None;
    };
    let mut cookie = format!("{name}={value}; Path={path}; HttpOnly");
    if let Some(ttl) = ttl {
        cookie.push_str(&format!("; Max-Age={}", ttl.as_secs()));
    }
    Some(cookie)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferry_config::{SnapshotVersion, from_yaml_str};
    use ferry_lb::pool::PoolSettings;
    use http::{HeaderMap, HeaderValue};

    struct NoSink;
    impl HealthSink for NoSink {
        fn health_changed(&self, _: &str) {}
    }

    fn snap(yaml: &str) -> Snapshot {
        from_yaml_str(yaml)
            .unwrap()
            .into_snapshot(SnapshotVersion(1))
            .unwrap()
    }

    const BASE: &str = r"
listeners: [{ name: l, address: 127.0.0.1:0, routes: [{ name: r, rules: [{ backends: [{ cluster: a }] }] }] }]
clusters:
  - { name: a, endpoints: [{ address: 10.0.0.1:80 }, { address: 10.0.0.2:80 }] }
  - { name: b, lb_policy: least_request, endpoints: [{ address: 10.0.0.3:80 }] }
";

    fn bound() -> Vec<BoundListener> {
        vec![BoundListener {
            name: "l".into(),
            addr: "127.0.0.1:1".parse().unwrap(),
        }]
    }

    #[tokio::test]
    async fn unchanged_clusters_and_routers_are_reused() {
        let pools = PoolSet::new(PoolSettings::default());
        let sink: Weak<NoSink> = Weak::new();
        let mut control = Control::default();
        let first = control.build(snap(BASE), None, &bound(), &pools, &sink);
        let changed = BASE.replace("10.0.0.3:80", "10.0.0.4:80");
        let second = control.build(snap(&changed), Some(&first), &bound(), &pools, &sink);
        assert!(Arc::ptr_eq(&first.clusters[0], &second.clusters[0]));
        assert!(!Arc::ptr_eq(&first.clusters[1], &second.clusters[1]));
        assert!(Arc::ptr_eq(
            first.listeners[0].as_ref().unwrap(),
            second.listeners[0].as_ref().unwrap()
        ));
        // Pools for the dropped endpoint are forgotten.
        assert_eq!(pools.len(), 3);
    }

    #[tokio::test]
    async fn listener_setting_change_keeps_the_router() {
        let pools = PoolSet::new(PoolSettings::default());
        let sink: Weak<NoSink> = Weak::new();
        let mut control = Control::default();
        let first = control.build(snap(BASE), None, &bound(), &pools, &sink);
        let limited = BASE.replace(
            "address: 127.0.0.1:0,",
            "address: 127.0.0.1:0, limits: { max_request_headers: 10 },",
        );
        let second = control.build(snap(&limited), Some(&first), &bound(), &pools, &sink);
        let (a, b) = (
            first.listeners[0].as_ref().unwrap(),
            second.listeners[0].as_ref().unwrap(),
        );
        assert!(!Arc::ptr_eq(a, b));
        assert!(Arc::ptr_eq(&a.router, &b.router));
        assert_eq!(b.config.limits().max_request_headers(), 10);
    }

    #[tokio::test]
    async fn endpoint_state_survives_cluster_changes() {
        let pools = PoolSet::new(PoolSettings::default());
        let sink: Weak<NoSink> = Weak::new();
        let mut control = Control::default();
        let first = control.build(snap(BASE), None, &bound(), &pools, &sink);
        let guard = first.clusters[0].balancer.hosts()[0].state.start_request();
        let changed = BASE.replace(
            "{ address: 10.0.0.2:80 }",
            "{ address: 10.0.0.2:80, weight: 2 }",
        );
        let second = control.build(snap(&changed), Some(&first), &bound(), &pools, &sink);
        assert!(!Arc::ptr_eq(&first.clusters[0], &second.clusters[0]));
        assert_eq!(
            second.clusters[0].balancer.hosts()[0]
                .state
                .active_requests(),
            1
        );
        drop(guard);
        assert_eq!(
            second.clusters[0].balancer.hosts()[0]
                .state
                .active_requests(),
            0
        );
    }

    #[tokio::test]
    async fn health_checkers_follow_config() {
        let pools = PoolSet::new(PoolSettings::default());
        let sink: Weak<NoSink> = Weak::new();
        let mut control = Control::default();
        let with_hc = BASE.replace(
            "{ name: a, endpoints",
            "{ name: a, health_check: { interval: 1h, http: { path: /healthz } }, endpoints",
        );
        let first = control.build(snap(&with_hc), None, &bound(), &pools, &sink);
        assert_eq!(control.checker_count(), 2);
        let second = control.build(snap(BASE), Some(&first), &bound(), &pools, &sink);
        assert_eq!(control.checker_count(), 0);
        drop(second);
        control.shutdown();
    }

    #[test]
    fn removed_listener_has_no_router() {
        let pools = PoolSet::new(PoolSettings::default());
        let sink: Weak<NoSink> = Weak::new();
        let mut control = Control::default();
        let mut b = bound();
        b.push(BoundListener {
            name: "gone".into(),
            addr: "127.0.0.1:2".parse().unwrap(),
        });
        let rt = control.build(snap(BASE), None, &b, &pools, &sink);
        assert!(rt.listeners[0].is_some());
        assert!(rt.listeners[1].is_none());
    }

    #[test]
    fn cookie_hashing_generates_and_reads() {
        let policy = HashPolicy::Cookie {
            name: "sid".into(),
            ttl: Some(std::time::Duration::from_secs(60)),
            path: "/".into(),
        };
        let peer = "10.1.1.1:5000".parse().unwrap();
        let (hash, generated) = hash_key(Some(&policy), &HeaderMap::new(), peer);
        let value = generated.unwrap();
        assert_eq!(
            set_cookie(Some(&policy), &value).unwrap(),
            format!("sid={value}; Path=/; HttpOnly; Max-Age=60")
        );
        let mut h = HeaderMap::new();
        h.insert(
            http::header::COOKIE,
            HeaderValue::from_str(&format!("a=1; sid={value}")).unwrap(),
        );
        let (again, none) = hash_key(Some(&policy), &h, peer);
        assert_eq!(again, hash);
        assert!(none.is_none());
    }

    #[test]
    fn source_ip_and_header_hashing() {
        let peer_a = "10.1.1.1:5000".parse().unwrap();
        let peer_b = "10.1.1.1:6000".parse().unwrap();
        let (a, _) = hash_key(Some(&HashPolicy::SourceIp), &HeaderMap::new(), peer_a);
        let (b, _) = hash_key(Some(&HashPolicy::SourceIp), &HeaderMap::new(), peer_b);
        assert_eq!(a, b, "the port must not matter");
        let name = http::HeaderName::from_static("x-user");
        let policy = HashPolicy::Header(name);
        assert_eq!(hash_key(Some(&policy), &HeaderMap::new(), peer_a).0, None);
    }
}
