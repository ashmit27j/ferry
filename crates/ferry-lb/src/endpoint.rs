//! Per-endpoint state shared by balancers, health checks and in-flight requests.
//!
//! An [`EndpointState`] outlives config reloads: the proxy keeps one per
//! `(cluster, address)` and hands the same `Arc` to every snapshot that still
//! lists the endpoint. That carries health, in-flight counts and latency
//! estimates across reloads, so least-request and P2C-EWMA keep their memory.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::outlier::OutlierState;

/// Latency assumed for an endpoint before its first response.
pub const DEFAULT_RTT: Duration = Duration::from_millis(1);
/// Time constant of the latency EWMA, as in Linkerd.
pub const EWMA_DECAY: Duration = Duration::from_secs(10);
/// Latency recorded for a connection-level failure, so P2C-EWMA steers away
/// from endpoints that fail fast (a refused connection is very "quick").
pub const FAILURE_PENALTY: Duration = Duration::from_millis(500);

/// Live state of one upstream endpoint.
#[derive(Debug)]
pub struct EndpointState {
    address: SocketAddr,
    healthy: AtomicBool,
    active: AtomicU64,
    ewma: PeakEwma,
    pub(crate) outlier: OutlierState,
}

impl EndpointState {
    /// A healthy endpoint with no history.
    #[must_use]
    pub fn new(address: SocketAddr) -> Self {
        Self {
            address,
            healthy: AtomicBool::new(true),
            active: AtomicU64::new(0),
            ewma: PeakEwma::new(DEFAULT_RTT),
            outlier: OutlierState::default(),
        }
    }

    /// Endpoint address.
    #[must_use]
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Whether the last health verdict was healthy. Endpoints without active
    /// health checking are always healthy.
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    /// Whether passive outlier detection has ejected this endpoint.
    #[must_use]
    pub fn is_ejected(&self) -> bool {
        self.outlier.is_ejected()
    }

    /// Records a health verdict; returns true if it changed.
    pub fn set_healthy(&self, healthy: bool) -> bool {
        self.healthy.swap(healthy, Ordering::Relaxed) != healthy
    }

    /// Requests currently in flight to this endpoint.
    #[must_use]
    pub fn active_requests(&self) -> u64 {
        self.active.load(Ordering::Relaxed)
    }

    /// Current P2C-EWMA cost: decayed peak latency (ns) times `active + 1`.
    #[must_use]
    pub fn cost(&self, now: Instant) -> f64 {
        // Precision loss converting large counts to f64 is irrelevant for a cost.
        #[allow(clippy::cast_precision_loss)]
        let load = (self.active_requests() + 1) as f64;
        self.ewma.estimate(now) * load
    }

    /// Latency estimate in nanoseconds, decayed to `now`.
    #[must_use]
    pub fn latency_estimate(&self, now: Instant) -> f64 {
        self.ewma.estimate(now)
    }

    /// Marks a request as started. The returned guard decrements the
    /// in-flight count when dropped, wherever the response body ends up.
    #[must_use]
    pub fn start_request(self: &Arc<Self>) -> RequestGuard {
        self.active.fetch_add(1, Ordering::Relaxed);
        RequestGuard {
            state: Arc::clone(self),
            started: Instant::now(),
        }
    }
}

/// An in-flight request to one endpoint.
#[derive(Debug)]
pub struct RequestGuard {
    state: Arc<EndpointState>,
    started: Instant,
}

impl RequestGuard {
    /// Records the time to response headers as a latency sample.
    pub fn observe_response(&self) {
        let now = Instant::now();
        self.state
            .ewma
            .observe(now.saturating_duration_since(self.started), now);
    }

    /// Records a connection-level failure as a [`FAILURE_PENALTY`] sample.
    pub fn observe_failure(&self) {
        self.state.ewma.observe(FAILURE_PENALTY, Instant::now());
    }

    /// The endpoint this request went to.
    #[must_use]
    pub fn endpoint(&self) -> &Arc<EndpointState> {
        &self.state
    }
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.state.active.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Peak-sensitive exponentially weighted moving average of latency
/// (Finagle's and Linkerd's "peak EWMA").
///
/// A sample above the estimate replaces it at once, so a slowdown is seen
/// immediately; lower samples are blended in with weight `1 - e^(-Δt/τ)`.
/// Reads decay the estimate towards zero by the time since the last sample,
/// so an endpoint that stops being picked becomes attractive again.
///
/// Two atomics hold the estimate (as `f64` bits) and its timestamp. Concurrent
/// updates may lose a sample, which only adds noise to an estimate.
#[derive(Debug)]
struct PeakEwma {
    rtt_ns: AtomicU64,
    stamp_ns: AtomicU64,
}

impl PeakEwma {
    fn new(initial: Duration) -> Self {
        Self {
            rtt_ns: AtomicU64::new(duration_ns(initial).to_bits()),
            stamp_ns: AtomicU64::new(nanos_since_base(Instant::now())),
        }
    }

    fn decay_factor(&self, now_ns: u64) -> f64 {
        let elapsed = now_ns.saturating_sub(self.stamp_ns.load(Ordering::Relaxed));
        // u64 nanoseconds to f64 loses precision only beyond 2^53 ns (104 days).
        #[allow(clippy::cast_precision_loss)]
        let elapsed = elapsed as f64;
        (-elapsed / duration_ns(EWMA_DECAY)).exp()
    }

    fn estimate(&self, now: Instant) -> f64 {
        let rtt = f64::from_bits(self.rtt_ns.load(Ordering::Relaxed));
        rtt * self.decay_factor(nanos_since_base(now))
    }

    fn observe(&self, sample: Duration, now: Instant) {
        let now_ns = nanos_since_base(now);
        let sample = duration_ns(sample);
        let prev = f64::from_bits(self.rtt_ns.load(Ordering::Relaxed));
        let next = if sample > prev {
            sample
        } else {
            let w = self.decay_factor(now_ns);
            prev * w + sample * (1.0 - w)
        };
        self.rtt_ns.store(next.to_bits(), Ordering::Relaxed);
        self.stamp_ns.store(now_ns, Ordering::Relaxed);
    }
}

fn duration_ns(d: Duration) -> f64 {
    d.as_secs_f64() * 1e9
}

/// Nanoseconds since a process-wide base instant, for storing in atomics.
pub(crate) fn nanos_since_base(now: Instant) -> u64 {
    static BASE: OnceLock<Instant> = OnceLock::new();
    let base = *BASE.get_or_init(Instant::now);
    u64::try_from(now.saturating_duration_since(base).as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> Arc<EndpointState> {
        Arc::new(EndpointState::new("127.0.0.1:1".parse().unwrap()))
    }

    #[test]
    fn guard_tracks_in_flight_requests() {
        let s = state();
        let a = s.start_request();
        let b = s.start_request();
        assert_eq!(s.active_requests(), 2);
        drop(a);
        assert_eq!(s.active_requests(), 1);
        drop(b);
        assert_eq!(s.active_requests(), 0);
    }

    #[test]
    fn health_flag_reports_changes() {
        let s = state();
        assert!(s.is_healthy());
        assert!(!s.set_healthy(true));
        assert!(s.set_healthy(false));
        assert!(!s.is_healthy());
    }

    #[test]
    fn peak_ewma_jumps_up_and_decays_down() {
        let e = PeakEwma::new(Duration::from_millis(1));
        let t0 = Instant::now();
        e.observe(Duration::from_millis(100), t0);
        let at_peak = e.estimate(t0);
        assert!((at_peak - 100e6).abs() < 1e3, "{at_peak}");
        // One time constant later the estimate has decayed to about 1/e.
        let later = e.estimate(t0 + EWMA_DECAY);
        assert!((later / at_peak - (-1f64).exp()).abs() < 0.01, "{later}");
        // A low sample right after the peak barely moves it.
        e.observe(Duration::from_millis(1), t0 + Duration::from_millis(1));
        assert!(e.estimate(t0 + Duration::from_millis(1)) > 99e6);
    }

    #[test]
    fn cost_scales_with_load() {
        let s = state();
        let now = Instant::now();
        let idle = s.cost(now);
        let _g = s.start_request();
        let busy = s.cost(now);
        assert!((busy / idle - 2.0).abs() < 0.01, "{idle} {busy}");
    }

    #[test]
    fn failure_penalises_latency() {
        let s = state();
        s.start_request().observe_failure();
        assert!(s.latency_estimate(Instant::now()) >= duration_ns(FAILURE_PENALTY) * 0.99);
    }
}
