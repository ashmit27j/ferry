//! Runtime state derived from a snapshot.
//!
//! A [`Runtime`] pairs an immutable [`Snapshot`] with the mutable-by-atomics
//! state the hot path needs (route tables, balancer counters, pre-parsed
//! upstream authorities). It is built off the hot path and published with one
//! `ArcSwap::store`; each request loads it once and keeps that `Arc` until it
//! completes, so in-flight requests finish on the snapshot they started with.

use std::sync::Arc;

use ferry_config::{Cluster, ClusterId, Snapshot};
use ferry_lb::{Balancer, RoundRobin};
use http::uri::Authority;

use crate::router::Router;

/// Everything a request needs, frozen at one snapshot version.
#[derive(Debug)]
pub(crate) struct Runtime {
    pub(crate) snapshot: Arc<Snapshot>,
    /// One router per listener, parallel to `snapshot.listeners()`.
    pub(crate) routers: Vec<Router>,
    /// One entry per cluster, parallel to `snapshot.clusters()`.
    pub(crate) clusters: Vec<ClusterRuntime>,
}

#[derive(Debug)]
pub(crate) struct ClusterRuntime {
    balancer: RoundRobin,
    authorities: Vec<Authority>,
}

impl Runtime {
    pub(crate) fn new(snapshot: Snapshot) -> Self {
        let routers = snapshot.listeners().iter().map(Router::new).collect();
        let clusters = snapshot
            .clusters()
            .iter()
            .enumerate()
            .map(|(i, c)| ClusterRuntime::new(c, i))
            .collect();
        Self {
            snapshot: Arc::new(snapshot),
            routers,
            clusters,
        }
    }

    /// Picks an upstream authority in `cluster`.
    pub(crate) fn pick_endpoint(&self, cluster: ClusterId) -> Option<&Authority> {
        let c = self.clusters.get(cluster.index())?;
        c.authorities.get(c.balancer.pick()?)
    }

    pub(crate) fn cluster_name(&self, cluster: ClusterId) -> &str {
        self.snapshot.cluster(cluster).map_or("?", Cluster::name)
    }
}

impl ClusterRuntime {
    fn new(cluster: &Cluster, seed: usize) -> Self {
        // SocketAddr's Display is always a valid authority ("1.2.3.4:80", "[::1]:80"),
        // but parse defensively rather than trust that.
        let authorities: Vec<Authority> = cluster
            .endpoints()
            .iter()
            .filter_map(|e| e.address().to_string().parse().ok())
            .collect();
        Self {
            balancer: RoundRobin::with_start(authorities.len(), seed),
            authorities,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferry_config::{SnapshotVersion, from_yaml_str};

    #[test]
    fn picks_ipv4_and_ipv6_endpoints() {
        let yaml = r#"
listeners: [{ name: l, address: 127.0.0.1:0, routes: [] }]
clusters:
  - name: c
    endpoints: [{ address: 10.0.0.1:80 }, { address: "[fd00::1]:8080" }]
"#;
        let snap = from_yaml_str(yaml)
            .unwrap()
            .into_snapshot(SnapshotVersion(1))
            .unwrap();
        let rt = Runtime::new(snap);
        let id = rt.snapshot.cluster_id("c").unwrap();
        let mut seen: Vec<String> = (0..2)
            .filter_map(|_| rt.pick_endpoint(id))
            .map(ToString::to_string)
            .collect();
        seen.sort();
        assert_eq!(seen, ["10.0.0.1:80", "[fd00::1]:8080"]);
        assert_eq!(rt.cluster_name(id), "c");
    }
}
