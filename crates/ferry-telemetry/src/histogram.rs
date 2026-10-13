//! A lock-free, fixed-bucket latency histogram.
//!
//! `prometheus-client`'s histogram takes a write lock on every observation.
//! Ferry observes several durations per request on every core, so this one is
//! a row of relaxed atomics: one counter per bucket and an integer sum in
//! nanoseconds. A scrape may see a bucket and the sum from slightly different
//! moments, which Prometheus tolerates; the count is derived from the buckets,
//! so `_count` always equals the `+Inf` bucket.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use prometheus_client::encoding::{EncodeMetric, MetricEncoder, NoLabelSet};
use prometheus_client::metrics::MetricType;

/// Upper bounds, in seconds, for whole-request durations: 0.5 ms to 60 s.
pub const REQUEST_BUCKETS: &[f64] = &[
    0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
    60.0,
];

/// Upper bounds, in seconds, for Ferry's own added latency: 25 µs to 100 ms,
/// dense around the 0.2 ms (p50) and 1 ms (p99) objectives.
pub const OVERHEAD_BUCKETS: &[f64] = &[
    0.000_025, 0.000_05, 0.000_1, 0.000_15, 0.000_2, 0.000_3, 0.000_5, 0.000_75, 0.001, 0.0015,
    0.0025, 0.005, 0.01, 0.025, 0.1,
];

/// A histogram of durations with fixed bucket bounds.
#[derive(Debug)]
pub struct Histogram {
    /// Bucket upper bounds in seconds, for encoding.
    bounds: &'static [f64],
    /// The same bounds in nanoseconds, for observing.
    bounds_ns: Box<[u64]>,
    /// Non-cumulative counts; the last slot is `+Inf`.
    counts: Box<[AtomicU64]>,
    sum_ns: AtomicU64,
}

impl Histogram {
    /// An empty histogram. `bounds` must be ascending.
    #[must_use]
    pub fn new(bounds: &'static [f64]) -> Self {
        // Bounds are small positive constants; the casts are exact enough.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let bounds_ns = bounds.iter().map(|b| (b * 1e9).round() as u64).collect();
        Self {
            bounds,
            bounds_ns,
            counts: (0..=bounds.len()).map(|_| AtomicU64::new(0)).collect(),
            sum_ns: AtomicU64::new(0),
        }
    }

    /// Records one duration.
    pub fn observe(&self, d: Duration) {
        let ns = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        let i = self.bounds_ns.partition_point(|&b| b < ns);
        if let Some(c) = self.counts.get(i) {
            c.fetch_add(1, Ordering::Relaxed);
        }
        self.sum_ns.fetch_add(ns, Ordering::Relaxed);
    }

    /// Observations so far.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.counts.iter().map(|c| c.load(Ordering::Relaxed)).sum()
    }

    /// Sum of observations.
    #[must_use]
    pub fn sum(&self) -> Duration {
        Duration::from_nanos(self.sum_ns.load(Ordering::Relaxed))
    }

    /// Cumulative `(upper bound in seconds, count)` pairs, ending with `+Inf`.
    #[must_use]
    pub fn buckets(&self) -> Vec<(f64, u64)> {
        let mut acc = 0;
        self.bounds
            .iter()
            .copied()
            .chain(std::iter::once(f64::INFINITY))
            .zip(self.counts.iter())
            .map(|(bound, c)| {
                acc += c.load(Ordering::Relaxed);
                (bound, acc)
            })
            .collect()
    }

    /// The upper bound of the bucket holding quantile `q` (0–1), or `None`
    /// when empty. Coarse, but enough for tests and the admin API.
    #[must_use]
    pub fn quantile_bound(&self, q: f64) -> Option<f64> {
        let buckets = self.buckets();
        let total = buckets.last().map_or(0, |b| b.1);
        if total == 0 {
            return None;
        }
        // Counts are far below 2^52, so the conversion is exact.
        #[allow(clippy::cast_precision_loss)]
        let rank = (q.clamp(0.0, 1.0) * total as f64).ceil().max(1.0);
        #[allow(clippy::cast_precision_loss)]
        buckets
            .iter()
            .find(|(_, c)| *c as f64 >= rank)
            .map(|(b, _)| *b)
    }
}

impl EncodeMetric for Histogram {
    fn encode(&self, mut encoder: MetricEncoder<'_>) -> Result<(), std::fmt::Error> {
        // The encoder accumulates, so it takes per-bucket counts.
        let per_bucket: Vec<(f64, u64)> = self
            .bounds
            .iter()
            .copied()
            .chain(std::iter::once(f64::INFINITY))
            .zip(self.counts.iter().map(|c| c.load(Ordering::Relaxed)))
            .collect();
        let count = per_bucket.iter().map(|b| b.1).sum();
        encoder.encode_histogram::<NoLabelSet>(self.sum().as_secs_f64(), count, &per_bucket, None)
    }

    fn metric_type(&self) -> MetricType {
        MetricType::Histogram
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn observations_land_in_the_right_bucket() {
        let h = Histogram::new(&[0.001, 0.01]);
        h.observe(Duration::from_micros(500));
        h.observe(Duration::from_millis(1)); // on the bound: le="0.001"
        h.observe(Duration::from_millis(5));
        h.observe(Duration::from_secs(1));
        assert_eq!(h.buckets(), vec![(0.001, 2), (0.01, 3), (f64::INFINITY, 4)]);
        assert_eq!(h.count(), 4);
        assert_eq!(h.sum(), Duration::from_micros(1_006_500));
    }

    #[test]
    fn quantile_bounds() {
        let h = Histogram::new(OVERHEAD_BUCKETS);
        assert_eq!(h.quantile_bound(0.5), None);
        for _ in 0..99 {
            h.observe(Duration::from_micros(80));
        }
        h.observe(Duration::from_millis(3));
        assert_eq!(h.quantile_bound(0.5), Some(0.000_1));
        assert_eq!(h.quantile_bound(0.99), Some(0.000_1));
        assert_eq!(h.quantile_bound(1.0), Some(0.005));
    }

    #[test]
    fn concurrent_observations_are_all_counted() {
        let h = Histogram::new(REQUEST_BUCKETS);
        std::thread::scope(|s| {
            for t in 0..8u64 {
                let h = &h;
                s.spawn(move || {
                    for i in 0..1000 {
                        h.observe(Duration::from_micros(t * 1000 + i));
                    }
                });
            }
        });
        assert_eq!(h.count(), 8000);
    }

    proptest! {
        #[test]
        fn buckets_are_cumulative_and_count_matches(ns in proptest::collection::vec(0u64..100_000_000_000, 0..200)) {
            let h = Histogram::new(REQUEST_BUCKETS);
            for n in &ns {
                h.observe(Duration::from_nanos(*n));
            }
            let b = h.buckets();
            prop_assert_eq!(b.len(), REQUEST_BUCKETS.len() + 1);
            prop_assert!(b.windows(2).all(|w| w[0].1 <= w[1].1));
            prop_assert_eq!(b.last().map(|x| x.1), Some(ns.len() as u64));
            prop_assert_eq!(h.sum(), Duration::from_nanos(ns.iter().sum()));
            // Each bucket counts exactly the observations at or below its bound.
            for (bound, count) in &b[..b.len() - 1] {
                let expected = ns.iter().filter(|&&n| (n as f64) <= bound * 1e9 + 0.5).count() as u64;
                prop_assert_eq!(*count, expected);
            }
        }
    }
}
