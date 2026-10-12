//! Deterministic weighted selection across a rule's backends.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::Balancer;

/// Splits picks across candidates in exact proportion to their weights.
///
/// Every window of `total_weight` consecutive picks contains exactly
/// `weight[i]` picks of candidate `i`. Within a window, picks are interleaved by
/// stepping through the weight space with a stride coprime to the total, so a
/// 90/10 canary split sends one request in ten to the canary rather than ten in
/// a row.
#[derive(Debug)]
pub struct WeightedSelector {
    /// Running sums of weights; candidate `i` owns `[cumulative[i-1], cumulative[i])`.
    cumulative: Vec<u64>,
    total: u64,
    stride: u64,
    next: AtomicU64,
}

impl WeightedSelector {
    /// Builds a selector. Candidates with weight zero are never picked.
    #[must_use]
    pub fn new(weights: &[u32]) -> Self {
        let mut total = 0u64;
        let cumulative = weights
            .iter()
            .map(|&w| {
                total += u64::from(w);
                total
            })
            .collect();
        let stride = golden_stride(total);
        Self {
            cumulative,
            total,
            stride,
            next: AtomicU64::new(0),
        }
    }

    /// Sum of all weights.
    #[must_use]
    pub fn total_weight(&self) -> u64 {
        self.total
    }
}

impl Balancer for WeightedSelector {
    fn pick(&self) -> Option<usize> {
        if self.total == 0 {
            return None;
        }
        let ticket = self.next.fetch_add(1, Ordering::Relaxed) % self.total;
        let point = (u128::from(ticket) * u128::from(self.stride) % u128::from(self.total)) as u64;
        // First candidate whose cumulative weight exceeds the point.
        Some(self.cumulative.partition_point(|&c| c <= point))
    }

    fn len(&self) -> usize {
        self.cumulative.len()
    }
}

/// Smallest stride at or above `0.618 * total` that is coprime with `total`.
///
/// Coprimality makes `ticket * stride mod total` a permutation of the weight
/// space; the golden-ratio start spreads consecutive tickets far apart.
fn golden_stride(total: u64) -> u64 {
    if total <= 2 {
        return 1;
    }
    let start = (total.saturating_mul(618) / 1000).max(1);
    (start..total).find(|&s| gcd(s, total) == 1).unwrap_or(1)
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn all_zero_weights_pick_none() {
        assert_eq!(WeightedSelector::new(&[0, 0]).pick(), None);
        assert_eq!(WeightedSelector::new(&[]).pick(), None);
    }

    #[test]
    fn canary_split_is_interleaved() {
        let sel = WeightedSelector::new(&[9, 1]);
        let picks: Vec<_> = (0..10).filter_map(|_| sel.pick()).collect();
        assert_eq!(picks.iter().filter(|&&i| i == 1).count(), 1);
        // Naive cumulative selection sends nine primaries back to back, then the canary.
        assert_ne!(picks, [0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
    }

    #[test]
    fn even_split_alternates() {
        let sel = WeightedSelector::new(&[5, 5]);
        let picks: Vec<_> = (0..4).filter_map(|_| sel.pick()).collect();
        assert_ne!(picks[0], picks[1]);
    }

    #[test]
    fn stride_is_coprime() {
        for total in 1..500 {
            assert_eq!(gcd(golden_stride(total), total), 1, "total {total}");
        }
    }

    proptest! {
        #[test]
        fn each_window_matches_weights_exactly(weights in prop::collection::vec(0u32..50, 1..8)) {
            let sel = WeightedSelector::new(&weights);
            let total = sel.total_weight();
            prop_assume!(total > 0);
            for _window in 0..2 {
                let mut counts = vec![0u64; weights.len()];
                for _ in 0..total {
                    let i = sel.pick().unwrap();
                    prop_assert!(i < weights.len());
                    counts[i] += 1;
                }
                let expected: Vec<u64> = weights.iter().map(|&w| u64::from(w)).collect();
                prop_assert_eq!(counts, expected);
            }
        }

        #[test]
        fn never_picks_zero_weight(weights in prop::collection::vec(0u32..5, 1..8), n in 1usize..200) {
            let sel = WeightedSelector::new(&weights);
            for _ in 0..n {
                if let Some(i) = sel.pick() {
                    prop_assert!(weights[i] > 0);
                }
            }
        }
    }
}
