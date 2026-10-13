//! Passive outlier detection (Envoy's model).
//!
//! Every upstream result is classified into an [`Outcome`]. Consecutive 5xx
//! responses, or consecutive gateway failures (502, 503, 504, and failures to
//! connect or get a response at all), eject an endpoint from its cluster's host
//! set for `base_ejection_time × n`, where n counts its ejections, capped at
//! `max_ejection_time`. At most `max_ejection_percent` of a cluster is ejected
//! at once, but one endpoint always may be. The multiplier decays by one for
//! every `base_ejection_time` an endpoint spends back in rotation.
//!
//! State lives in the endpoint's atomics, so it survives config reloads with
//! the rest of [`EndpointState`](crate::EndpointState).

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

/// Outlier detection settings for one cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutlierConfig {
    /// Consecutive 5xx results that eject.
    pub consecutive_5xx: u32,
    /// Consecutive gateway failures that eject.
    pub consecutive_gateway_failure: u32,
    /// Length of the first ejection.
    pub base_ejection_time: Duration,
    /// Longest ejection.
    pub max_ejection_time: Duration,
    /// Most of the cluster ejected at once, in percent.
    pub max_ejection_percent: u8,
}

/// How one upstream attempt ended, for outlier detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A response below 500.
    Success,
    /// A 5xx response other than 502, 503 and 504.
    ServerError,
    /// 502, 503 or 504, or no response at all (connect failure, reset, timeout).
    GatewayFailure,
}

impl Outcome {
    /// Classifies an HTTP status.
    #[must_use]
    pub fn from_status(status: u16) -> Self {
        match status {
            502..=504 => Self::GatewayFailure,
            500..=599 => Self::ServerError,
            _ => Self::Success,
        }
    }
}

/// Per-endpoint outlier state.
#[derive(Debug, Default)]
pub(crate) struct OutlierState {
    consecutive_5xx: AtomicU32,
    consecutive_gateway: AtomicU32,
    ejected: AtomicBool,
    /// While ejected: when the ejection ends. Afterwards: when the multiplier
    /// last decayed (or the ejection ended).
    until_ns: AtomicU64,
    ejections: AtomicU32,
    /// Ejections ever, for metrics.
    total: AtomicU64,
}

impl OutlierState {
    pub(crate) fn is_ejected(&self) -> bool {
        self.ejected.load(Ordering::Relaxed)
    }

    /// Updates the counters; returns true if the endpoint has crossed an
    /// ejection threshold.
    pub(crate) fn record(&self, outcome: Outcome, cfg: &OutlierConfig) -> bool {
        match outcome {
            Outcome::Success => {
                self.consecutive_5xx.store(0, Ordering::Relaxed);
                self.consecutive_gateway.store(0, Ordering::Relaxed);
                false
            }
            Outcome::ServerError => {
                self.consecutive_gateway.store(0, Ordering::Relaxed);
                let n = self.consecutive_5xx.fetch_add(1, Ordering::Relaxed) + 1;
                n >= cfg.consecutive_5xx
            }
            Outcome::GatewayFailure => {
                let n5 = self.consecutive_5xx.fetch_add(1, Ordering::Relaxed) + 1;
                let ng = self.consecutive_gateway.fetch_add(1, Ordering::Relaxed) + 1;
                n5 >= cfg.consecutive_5xx || ng >= cfg.consecutive_gateway_failure
            }
        }
    }

    /// Ejects the endpoint until `now + base × n` (capped); returns false if
    /// it was already ejected.
    pub(crate) fn eject(&self, now_ns: u64, cfg: &OutlierConfig) -> bool {
        if self.ejected.swap(true, Ordering::Relaxed) {
            return false;
        }
        self.total.fetch_add(1, Ordering::Relaxed);
        let n = self.ejections.fetch_add(1, Ordering::Relaxed) + 1;
        let length = cfg
            .base_ejection_time
            .saturating_mul(n)
            .min(cfg.max_ejection_time);
        let length_ns = u64::try_from(length.as_nanos()).unwrap_or(u64::MAX);
        self.until_ns
            .store(now_ns.saturating_add(length_ns), Ordering::Relaxed);
        self.consecutive_5xx.store(0, Ordering::Relaxed);
        self.consecutive_gateway.store(0, Ordering::Relaxed);
        true
    }

    /// Ends an expired ejection and decays the multiplier; returns true if the
    /// endpoint came back into rotation.
    pub(crate) fn sweep(&self, now_ns: u64, cfg: &OutlierConfig) -> bool {
        let until = self.until_ns.load(Ordering::Relaxed);
        if now_ns < until {
            return false;
        }
        if self.ejected.swap(false, Ordering::Relaxed) {
            self.until_ns.store(now_ns, Ordering::Relaxed);
            return true;
        }
        let base_ns = u64::try_from(cfg.base_ejection_time.as_nanos()).unwrap_or(u64::MAX);
        if self.ejections.load(Ordering::Relaxed) > 0 && now_ns.saturating_sub(until) >= base_ns {
            self.ejections.fetch_sub(1, Ordering::Relaxed);
            self.until_ns.store(now_ns, Ordering::Relaxed);
        }
        false
    }

    pub(crate) fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn ejections(&self) -> u32 {
        self.ejections.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000_000;

    fn cfg() -> OutlierConfig {
        OutlierConfig {
            consecutive_5xx: 3,
            consecutive_gateway_failure: 2,
            base_ejection_time: Duration::from_secs(10),
            max_ejection_time: Duration::from_secs(25),
            max_ejection_percent: 100,
        }
    }

    #[test]
    fn classifies_statuses() {
        assert_eq!(Outcome::from_status(200), Outcome::Success);
        assert_eq!(Outcome::from_status(404), Outcome::Success);
        assert_eq!(Outcome::from_status(500), Outcome::ServerError);
        assert_eq!(Outcome::from_status(503), Outcome::GatewayFailure);
    }

    #[test]
    fn thresholds_count_consecutive_results_only() {
        let s = OutlierState::default();
        let c = cfg();
        assert!(!s.record(Outcome::ServerError, &c));
        assert!(!s.record(Outcome::ServerError, &c));
        assert!(!s.record(Outcome::Success, &c), "success resets");
        assert!(!s.record(Outcome::ServerError, &c));
        assert!(!s.record(Outcome::ServerError, &c));
        assert!(s.record(Outcome::ServerError, &c));
        let g = OutlierState::default();
        assert!(!g.record(Outcome::GatewayFailure, &c));
        assert!(g.record(Outcome::GatewayFailure, &c));
        // A plain 500 breaks a gateway-failure streak (gateway failures also
        // count as 5xx, so raise that threshold to observe the gateway one).
        let gw_only = OutlierConfig {
            consecutive_5xx: 100,
            ..c
        };
        let h = OutlierState::default();
        h.record(Outcome::GatewayFailure, &gw_only);
        h.record(Outcome::ServerError, &gw_only);
        assert!(!h.record(Outcome::GatewayFailure, &gw_only));
        assert!(h.record(Outcome::GatewayFailure, &gw_only));
    }

    #[test]
    fn ejections_grow_cap_and_decay() {
        let s = OutlierState::default();
        let c = cfg();
        assert!(s.eject(0, &c));
        assert!(!s.eject(0, &c), "already ejected");
        assert!(!s.sweep(9 * S, &c));
        assert!(s.sweep(10 * S, &c), "first ejection lasts base");
        assert!(s.eject(11 * S, &c));
        assert!(!s.sweep(30 * S, &c));
        assert!(s.sweep(31 * S, &c), "second lasts 2 x base");
        assert!(s.eject(31 * S, &c));
        assert!(s.sweep(56 * S, &c), "third is capped at max (25 s)");
        assert_eq!(s.ejections(), 3);
        // Back in rotation: one decay per base period.
        s.sweep(66 * S, &c);
        assert_eq!(s.ejections(), 2);
        s.sweep(70 * S, &c);
        assert_eq!(s.ejections(), 2);
        s.sweep(76 * S, &c);
        assert_eq!(s.ejections(), 1);
        assert_eq!(s.total(), 3, "the lifetime total never decays");
    }
}
