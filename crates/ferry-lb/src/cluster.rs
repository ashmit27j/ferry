//! Health-aware endpoint selection for one cluster.
//!
//! A [`ClusterBalancer`] owns the cluster's endpoints and a precomputed
//! [`HostSet`]: the endpoints eligible for traffic, plus whatever the policy
//! needs (a weighted schedule or a Maglev table). The host set is rebuilt off
//! the hot path, by [`ClusterBalancer::refresh`], whenever health changes, and
//! published with one `ArcSwap::store`. A pick is then one atomic load plus the
//! policy's own work: no locks, no allocation.
//!
//! Panic mode follows Envoy: when fewer than `panic_threshold` percent of
//! endpoints are healthy, health is ignored and every endpoint is eligible,
//! on the theory that overloading a few survivors is worse than sending some
//! requests to endpoints that may be dead.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use arc_swap::ArcSwap;

use crate::Balancer;
use crate::endpoint::EndpointState;
use crate::maglev::Maglev;
use crate::weighted::WeightedSelector;

/// Endpoint selection policies (ADR 0010).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Policy {
    /// Each eligible endpoint in turn, ignoring weights.
    RoundRobin,
    /// Each eligible endpoint in proportion to its weight, interleaved.
    WeightedRoundRobin,
    /// Power of two choices on in-flight requests.
    LeastRequest,
    /// Power of two choices on peak latency EWMA times in-flight requests.
    P2cEwma,
    /// Maglev consistent hashing of a request key.
    Maglev,
}

/// One endpoint of a cluster as the balancer sees it.
#[derive(Debug, Clone)]
pub struct Host {
    /// Shared live state.
    pub state: Arc<EndpointState>,
    /// Relative weight, at least 1.
    pub weight: u32,
}

/// Selects endpoints for one cluster.
#[derive(Debug)]
pub struct ClusterBalancer {
    hosts: Vec<Host>,
    policy: Policy,
    panic_threshold: u8,
    set: ArcSwap<HostSet>,
    /// Round-robin cursor, kept across host-set rebuilds.
    cursor: AtomicUsize,
}

/// The endpoints eligible for traffic at one moment, with policy state.
#[derive(Debug)]
pub struct HostSet {
    /// Indices into the cluster's hosts.
    eligible: Vec<usize>,
    /// True when health is being ignored because too few hosts are healthy.
    panic: bool,
    schedule: Schedule,
}

#[derive(Debug)]
enum Schedule {
    None,
    Weighted(WeightedSelector),
    Maglev(Maglev),
}

impl HostSet {
    /// Number of eligible hosts.
    #[must_use]
    pub fn len(&self) -> usize {
        self.eligible.len()
    }

    /// True when no host is eligible.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.eligible.is_empty()
    }

    /// Whether the set is in panic mode.
    #[must_use]
    pub fn is_panic(&self) -> bool {
        self.panic
    }
}

impl ClusterBalancer {
    /// Builds a balancer and its first host set. `start` offsets the
    /// round-robin cursor so replicas do not all begin with the same host.
    #[must_use]
    pub fn new(hosts: Vec<Host>, policy: Policy, panic_threshold: u8, start: usize) -> Self {
        let set = build_set(&hosts, policy, panic_threshold);
        Self {
            hosts,
            policy,
            panic_threshold,
            set: ArcSwap::from_pointee(set),
            cursor: AtomicUsize::new(start),
        }
    }

    /// The cluster's hosts, in config order. Picks index into this slice.
    #[must_use]
    pub fn hosts(&self) -> &[Host] {
        &self.hosts
    }

    /// The selection policy.
    #[must_use]
    pub fn policy(&self) -> Policy {
        self.policy
    }

    /// The current host set.
    #[must_use]
    pub fn host_set(&self) -> Arc<HostSet> {
        self.set.load_full()
    }

    /// Recomputes the host set from current health. Call after any health
    /// change; it allocates and, for Maglev, rebuilds the table.
    pub fn refresh(&self) {
        self.set.store(Arc::new(build_set(
            &self.hosts,
            self.policy,
            self.panic_threshold,
        )));
    }

    /// Picks a host index. `hash` is the request key hash for Maglev (absent
    /// means random). `exclude` lists hosts already tried by this request; they
    /// are avoided when any other host is eligible.
    #[must_use]
    pub fn pick(&self, hash: Option<u64>, exclude: &[usize]) -> Option<usize> {
        let set = self.set.load();
        let eligible = &set.eligible;
        let n = eligible.len();
        if n == 0 {
            return None;
        }
        let ok = |i: usize| !exclude.contains(&i);
        if n == 1 {
            return eligible.first().copied();
        }
        let first = match (&set.schedule, self.policy) {
            (Schedule::Maglev(table), _) => match hash {
                Some(h) => table.lookup(h),
                None => self.random_two(eligible, &ok, false),
            },
            (Schedule::Weighted(sel), _) => sel.pick().and_then(|i| eligible.get(i).copied()),
            (_, Policy::LeastRequest) => self.random_two(eligible, &ok, false),
            (_, Policy::P2cEwma) => self.random_two(eligible, &ok, true),
            _ => {
                let t = self.cursor.fetch_add(1, Ordering::Relaxed);
                eligible.get(t % n).copied()
            }
        };
        match first {
            Some(i) if ok(i) => Some(i),
            _ => self.fallback(eligible, &ok).or(first),
        }
    }

    /// Power of two choices: two distinct random eligible hosts, keeping the one
    /// with fewer in-flight requests (or lower EWMA cost).
    fn random_two(
        &self,
        eligible: &[usize],
        ok: &impl Fn(usize) -> bool,
        ewma: bool,
    ) -> Option<usize> {
        let n = eligible.len();
        let a = fastrand::usize(..n);
        let mut b = fastrand::usize(..n - 1);
        if b >= a {
            b += 1;
        }
        let (a, b) = (*eligible.get(a)?, *eligible.get(b)?);
        match (ok(a), ok(b)) {
            (true, false) => return Some(a),
            (false, true) => return Some(b),
            _ => {}
        }
        let (ha, hb) = (self.hosts.get(a)?, self.hosts.get(b)?);
        let a_wins = if ewma {
            let now = Instant::now();
            ha.state.cost(now) <= hb.state.cost(now)
        } else {
            ha.state.active_requests() <= hb.state.active_requests()
        };
        Some(if a_wins { a } else { b })
    }

    /// Round robin over eligible hosts that are not excluded.
    fn fallback(&self, eligible: &[usize], ok: &impl Fn(usize) -> bool) -> Option<usize> {
        let n = eligible.len();
        let start = self.cursor.fetch_add(1, Ordering::Relaxed);
        (0..n)
            .filter_map(|k| eligible.get((start + k) % n).copied())
            .find(|&i| ok(i))
    }
}

fn build_set(hosts: &[Host], policy: Policy, panic_threshold: u8) -> HostSet {
    let healthy: Vec<usize> = hosts
        .iter()
        .enumerate()
        .filter(|(_, h)| h.state.is_healthy())
        .map(|(i, _)| i)
        .collect();
    let panic = healthy.len() * 100 < usize::from(panic_threshold) * hosts.len();
    let eligible = if panic {
        (0..hosts.len()).collect()
    } else {
        healthy
    };
    let weights: Vec<u32> = eligible
        .iter()
        .filter_map(|&i| hosts.get(i).map(|h| h.weight))
        .collect();
    let schedule = match policy {
        Policy::WeightedRoundRobin => Schedule::Weighted(WeightedSelector::new(&weights)),
        Policy::Maglev => {
            let keys: Vec<String> = eligible
                .iter()
                .filter_map(|&i| hosts.get(i).map(|h| h.state.address().to_string()))
                .collect();
            let key_refs: Vec<&[u8]> = keys.iter().map(String::as_bytes).collect();
            // Table slots hold positions in `eligible`; map them to host indices.
            match Maglev::build(&key_refs, &weights) {
                Some(t) => Schedule::Maglev(remap(t, &eligible)),
                None => Schedule::None,
            }
        }
        _ => Schedule::None,
    };
    HostSet {
        eligible,
        panic,
        schedule,
    }
}

/// Maglev stores positions in the eligible list; resolve them once at build
/// time so lookups return host indices directly.
fn remap(table: Maglev, eligible: &[usize]) -> Maglev {
    table.map_hosts(|pos| eligible.get(pos).copied())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn hosts(n: usize) -> Vec<Host> {
        (0..n)
            .map(|i| Host {
                state: Arc::new(EndpointState::new(
                    format!("10.0.0.{}:80", i + 1).parse().unwrap(),
                )),
                weight: 1,
            })
            .collect()
    }

    fn counts(b: &ClusterBalancer, picks: usize) -> Vec<usize> {
        let mut c = vec![0; b.hosts().len()];
        for _ in 0..picks {
            c[b.pick(None, &[]).unwrap()] += 1;
        }
        c
    }

    #[test]
    fn round_robin_cycles_healthy_hosts() {
        let b = ClusterBalancer::new(hosts(3), Policy::RoundRobin, 50, 0);
        assert_eq!(counts(&b, 9), [3, 3, 3]);
        b.hosts()[1].state.set_healthy(false);
        b.refresh();
        assert_eq!(counts(&b, 8), [4, 0, 4]);
    }

    #[test]
    fn weighted_round_robin_follows_endpoint_weights() {
        let mut h = hosts(3);
        h[0].weight = 1;
        h[1].weight = 2;
        h[2].weight = 5;
        let b = ClusterBalancer::new(h, Policy::WeightedRoundRobin, 50, 0);
        assert_eq!(counts(&b, 80), [10, 20, 50]);
    }

    #[test]
    fn panic_mode_uses_all_hosts() {
        let b = ClusterBalancer::new(hosts(4), Policy::RoundRobin, 50, 0);
        for h in &b.hosts()[..3] {
            h.state.set_healthy(false);
        }
        b.refresh();
        assert!(b.host_set().is_panic());
        assert_eq!(counts(&b, 8), [2, 2, 2, 2]);
    }

    #[test]
    fn zero_panic_threshold_sends_nothing_when_all_unhealthy() {
        let b = ClusterBalancer::new(hosts(2), Policy::RoundRobin, 0, 0);
        for h in b.hosts() {
            h.state.set_healthy(false);
        }
        b.refresh();
        assert_eq!(b.pick(None, &[]), None);
    }

    #[test]
    fn least_request_prefers_idle_host() {
        let b = ClusterBalancer::new(hosts(2), Policy::LeastRequest, 50, 0);
        let _busy: Vec<_> = (0..5).map(|_| b.hosts()[0].state.start_request()).collect();
        // With two hosts P2C always compares both, so the idle one always wins.
        assert_eq!(counts(&b, 20), [0, 20]);
    }

    #[test]
    fn p2c_ewma_prefers_fast_host() {
        let b = ClusterBalancer::new(hosts(2), Policy::P2cEwma, 50, 0);
        b.hosts()[0].state.start_request().observe_failure();
        assert_eq!(counts(&b, 20), [0, 20]);
    }

    #[test]
    fn maglev_is_sticky_and_moves_only_lost_keys() {
        let b = ClusterBalancer::new(hosts(5), Policy::Maglev, 0, 0);
        let keys: Vec<u64> = (0..1000u32)
            .map(|k| crate::maglev::hash_key(&k.to_le_bytes()))
            .collect();
        let before: Vec<usize> = keys
            .iter()
            .map(|&h| b.pick(Some(h), &[]).unwrap())
            .collect();
        // Same key, same host.
        for (&h, &want) in keys.iter().zip(&before) {
            assert_eq!(b.pick(Some(h), &[]), Some(want));
        }
        b.hosts()[2].state.set_healthy(false);
        b.refresh();
        let mut moved_needlessly = 0;
        for (&h, &was) in keys.iter().zip(&before) {
            let now = b.pick(Some(h), &[]).unwrap();
            assert_ne!(now, 2);
            if was != 2 && now != was {
                moved_needlessly += 1;
            }
        }
        assert!(moved_needlessly < 100, "{moved_needlessly}");
    }

    #[test]
    fn excluded_hosts_are_avoided_when_possible() {
        for policy in [
            Policy::RoundRobin,
            Policy::WeightedRoundRobin,
            Policy::LeastRequest,
            Policy::P2cEwma,
            Policy::Maglev,
        ] {
            let b = ClusterBalancer::new(hosts(3), policy, 50, 0);
            for _ in 0..30 {
                let i = b.pick(Some(42), &[0, 1]).unwrap();
                assert_eq!(i, 2, "{policy:?}");
            }
            // Everything excluded: still returns a host rather than nothing.
            assert!(b.pick(Some(42), &[0, 1, 2]).is_some(), "{policy:?}");
        }
    }

    proptest! {
        #[test]
        fn picks_are_always_eligible(
            health in prop::collection::vec(any::<bool>(), 1..8),
            policy in 0usize..5,
            threshold in 0u8..=100,
            hash in any::<u64>(),
        ) {
            let policy = [
                Policy::RoundRobin,
                Policy::WeightedRoundRobin,
                Policy::LeastRequest,
                Policy::P2cEwma,
                Policy::Maglev,
            ][policy];
            let b = ClusterBalancer::new(hosts(health.len()), policy, threshold, 0);
            for (h, &ok) in b.hosts().iter().zip(&health) {
                h.state.set_healthy(ok);
            }
            b.refresh();
            let set = b.host_set();
            for _ in 0..20 {
                match b.pick(Some(hash), &[]) {
                    Some(i) => {
                        prop_assert!(i < health.len());
                        prop_assert!(set.is_panic() || health[i]);
                    }
                    None => prop_assert!(set.is_empty()),
                }
            }
        }
    }
}
