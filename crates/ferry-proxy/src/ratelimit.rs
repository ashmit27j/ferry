//! Local rate limiting: a GCRA token bucket per key, in a fixed table of
//! atomic slots (ADR 0022).
//!
//! GCRA (the generic cell rate algorithm) keeps one number per bucket, the
//! *theoretical arrival time* (TAT) of the next request. A request at time t is
//! allowed if `max(TAT, t) - t <= tolerance`, and then advances TAT by the
//! emission interval `per / requests`. `tolerance = (burst - 1) × interval`
//! lets `burst` requests through at once. One `u64` per bucket, updated with a
//! compare-and-swap: no locks, no allocation.
//!
//! Keys (client IPs, header values) are hashed with a per-process random seed
//! into [`SLOTS`] buckets. The table never grows, so spraying addresses cannot
//! exhaust memory, and the seed stops a client from aiming its key at someone
//! else's bucket. Two keys sharing a slot share a limit, which only ever makes
//! the limit stricter.

use std::hash::{BuildHasher, Hash, Hasher, RandomState};
use std::net::IpAddr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ferry_config::{RateLimit, RateLimitKey};
use http::HeaderMap;

/// Buckets per rate-limited route.
pub const SLOTS: usize = 4096;

/// One route's limiter.
#[derive(Debug)]
pub(crate) struct RateLimiter {
    config: RateLimit,
    interval_ns: u64,
    tolerance_ns: u64,
    hasher: RandomState,
    tat: Box<[AtomicU64]>,
}

/// A refused request: how long until it would be allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limited {
    pub(crate) retry_after: Duration,
}

impl RateLimiter {
    pub(crate) fn new(config: &RateLimit) -> Self {
        let per_ns = u64::try_from(config.per().as_nanos()).unwrap_or(u64::MAX);
        let interval_ns = (per_ns / u64::from(config.requests().max(1))).max(1);
        let tolerance_ns = interval_ns.saturating_mul(u64::from(config.burst().max(1) - 1));
        Self {
            config: config.clone(),
            interval_ns,
            tolerance_ns,
            hasher: RandomState::new(),
            tat: (0..SLOTS).map(|_| AtomicU64::new(0)).collect(),
        }
    }

    /// The settings this limiter enforces.
    pub(crate) fn config(&self) -> &RateLimit {
        &self.config
    }

    /// Checks one request.
    pub(crate) fn check(&self, client: IpAddr, headers: &HeaderMap) -> Result<(), Limited> {
        self.check_slot(self.slot(client, headers), now_ns())
    }

    /// The slot a request's key hashes to.
    fn slot(&self, client: IpAddr, headers: &HeaderMap) -> usize {
        let mut h = self.hasher.build_hasher();
        match self.config.key() {
            RateLimitKey::Header(name) => {
                headers.get(name).map(|v| v.as_bytes()).hash(&mut h);
            }
            _ => client.hash(&mut h),
        }
        usize::try_from(h.finish() % SLOTS as u64).unwrap_or(0)
    }

    fn check_slot(&self, slot: usize, now: u64) -> Result<(), Limited> {
        let Some(cell) = self.tat.get(slot) else {
            return Ok(());
        };
        let mut current = cell.load(Ordering::Relaxed);
        loop {
            let tat = current.max(now);
            let ahead = tat - now;
            if ahead > self.tolerance_ns {
                return Err(Limited {
                    retry_after: Duration::from_nanos(ahead - self.tolerance_ns),
                });
            }
            match cell.compare_exchange_weak(
                current,
                tat + self.interval_ns,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) => current = actual,
            }
        }
    }
}

fn now_ns() -> u64 {
    static BASE: OnceLock<Instant> = OnceLock::new();
    let base = *BASE.get_or_init(Instant::now);
    // Start at one second so that an empty slot (0) is always in the past.
    u64::try_from(base.elapsed().as_nanos())
        .unwrap_or(u64::MAX)
        .saturating_add(1_000_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferry_config::{SnapshotVersion, from_yaml_str};
    use proptest::prelude::*;

    const S: u64 = 1_000_000_000;

    fn limiter(spec: &str) -> RateLimiter {
        let yaml = format!(
            "listeners: [{{ name: l, address: 127.0.0.1:0, routes: [{{ name: r, rate_limit: {spec}, rules: [{{ backends: [{{ cluster: c }}] }}] }}] }}]\nclusters: [{{ name: c, endpoints: [{{ address: 10.0.0.1:80 }}] }}]\n"
        );
        let snap = from_yaml_str(&yaml)
            .unwrap()
            .into_snapshot(SnapshotVersion(1))
            .unwrap();
        RateLimiter::new(snap.listeners()[0].routes()[0].rate_limit().unwrap())
    }

    #[test]
    fn burst_then_steady_rate() {
        let l = limiter("{ requests: 10, per: 1s, burst: 5 }");
        let t = 10 * S;
        assert_eq!((0..10).filter(|_| l.check_slot(0, t).is_ok()).count(), 5);
        // One request refills every 100 ms.
        assert!(l.check_slot(0, t + S / 10 - 1).is_err());
        assert!(l.check_slot(0, t + S / 10).is_ok());
        let err = l.check_slot(0, t + S / 10).unwrap_err();
        assert_eq!(err.retry_after, Duration::from_millis(100));
    }

    #[test]
    fn slots_are_independent() {
        let l = limiter("{ requests: 1, per: 1m }");
        assert!(l.check_slot(1, S).is_ok());
        assert!(l.check_slot(1, S).is_err());
        assert!(l.check_slot(2, S).is_ok());
    }

    #[test]
    fn keys_by_ip_or_header() {
        // The seed is random, so two fixed keys share a slot one run in
        // 4,096: pick ones that do not.
        let ip = limiter("{ requests: 1, per: 1m }");
        let empty = HeaderMap::new();
        let a: IpAddr = "10.0.0.1".parse().unwrap();
        let b = (2..=9)
            .map(|i| IpAddr::from([10, 0, 0, i]))
            .find(|&b| ip.slot(b, &empty) != ip.slot(a, &empty))
            .unwrap();
        assert!(ip.check(a, &empty).is_ok());
        assert!(ip.check(a, &empty).is_err());
        assert!(ip.check(b, &empty).is_ok());

        let hdr = limiter("{ requests: 1, per: 1m, key: { header: x-api-key } }");
        let key = |v: &str| {
            let mut m = HeaderMap::new();
            m.insert("x-api-key", v.parse().unwrap());
            m
        };
        let k1 = key("one");
        let k2 = ["two", "three", "four", "five"]
            .map(key)
            .into_iter()
            .find(|k2| hdr.slot(a, k2) != hdr.slot(a, &k1))
            .unwrap();
        assert!(hdr.check(a, &k1).is_ok());
        assert!(hdr.check(b, &k1).is_err(), "same key, different IP");
        assert!(hdr.check(a, &k2).is_ok());
    }

    proptest! {
        #[test]
        fn never_admits_more_than_burst_plus_rate(
            requests in 1u32..50,
            burst in 1u32..50,
            arrivals in prop::collection::vec(0u64..2_000_000_000, 1..400),
        ) {
            let l = limiter(&format!("{{ requests: {requests}, per: 1s, burst: {burst} }}"));
            let mut times = arrivals;
            times.sort_unstable();
            let admitted = times.iter().filter(|&&t| l.check_slot(0, S + t).is_ok()).count() as u64;
            // Over a 2 s window: the burst plus two seconds of refill (+1 for edges).
            prop_assert!(admitted <= u64::from(burst) + 2 * u64::from(requests) + 1);
        }
    }
}
