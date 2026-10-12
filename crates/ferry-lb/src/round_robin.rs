//! Lock-free round robin.

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::Balancer;

/// Cycles through `0..len` in order, shared across threads.
#[derive(Debug)]
pub struct RoundRobin {
    next: AtomicUsize,
    len: usize,
}

impl RoundRobin {
    /// Creates a balancer over `len` candidates.
    #[must_use]
    pub fn new(len: usize) -> Self {
        Self {
            next: AtomicUsize::new(0),
            len,
        }
    }

    /// Creates a balancer whose first pick is `start % len`, so replicas built
    /// at the same moment do not all hit endpoint 0 first.
    #[must_use]
    pub fn with_start(len: usize, start: usize) -> Self {
        Self {
            next: AtomicUsize::new(start),
            len,
        }
    }
}

impl Balancer for RoundRobin {
    fn pick(&self) -> Option<usize> {
        if self.len == 0 {
            return None;
        }
        // Relaxed is enough: we need a unique ticket, not ordering with other memory.
        let ticket = self.next.fetch_add(1, Ordering::Relaxed);
        Some(ticket % self.len)
    }

    fn len(&self) -> usize {
        self.len
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn empty_picks_none() {
        assert_eq!(RoundRobin::new(0).pick(), None);
    }

    #[test]
    fn cycles_in_order() {
        let rr = RoundRobin::new(3);
        let picks: Vec<_> = (0..7).filter_map(|_| rr.pick()).collect();
        assert_eq!(picks, [0, 1, 2, 0, 1, 2, 0]);
    }

    #[test]
    fn survives_counter_wraparound() {
        let rr = RoundRobin::with_start(3, usize::MAX);
        assert!(rr.pick().is_some_and(|i| i < 3));
        assert!(rr.pick().is_some_and(|i| i < 3));
    }

    #[test]
    fn concurrent_picks_are_balanced() {
        let rr = std::sync::Arc::new(RoundRobin::new(4));
        let counts: Vec<[usize; 4]> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    let rr = rr.clone();
                    s.spawn(move || {
                        let mut c = [0usize; 4];
                        for _ in 0..1000 {
                            if let Some(i) = rr.pick() {
                                c[i] += 1;
                            }
                        }
                        c
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let mut total = [0usize; 4];
        for c in counts {
            for (t, n) in total.iter_mut().zip(c) {
                *t += n;
            }
        }
        assert_eq!(total, [1000; 4]);
    }

    proptest! {
        #[test]
        fn every_window_of_len_picks_each_once(len in 1usize..64, start in any::<usize>()) {
            let rr = RoundRobin::with_start(len, start);
            // Skip past any wraparound discontinuity, then check a full cycle.
            let mut seen = vec![0u32; len];
            let offset = usize::MAX - start;
            if offset < 2 * len {
                for _ in 0..=offset { rr.pick(); }
            }
            for _ in 0..len {
                let i = rr.pick().unwrap();
                seen[i] += 1;
            }
            prop_assert!(seen.iter().all(|&n| n == 1));
        }
    }
}
