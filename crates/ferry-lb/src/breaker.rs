//! Circuit breakers: per-cluster concurrency limits (Envoy's model).
//!
//! Each limit is an atomic counter. Acquiring returns a [`Permit`] that gives
//! the slot back when dropped, wherever it ends up (a response body, a
//! connection task). Nothing waits: a request over a limit fails fast, which
//! is the point; queueing would only move the overload somewhere less visible.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

/// One concurrency limit.
#[derive(Debug)]
pub struct Limit {
    max: u32,
    active: AtomicU32,
}

impl Limit {
    /// A limit of `max` concurrent holders.
    #[must_use]
    pub fn new(max: u32) -> Arc<Self> {
        Arc::new(Self {
            max,
            active: AtomicU32::new(0),
        })
    }

    /// Takes a slot, or `None` if all `max` are in use.
    #[must_use]
    pub fn try_acquire(self: &Arc<Self>) -> Option<Permit> {
        self.active
            .try_update(Ordering::AcqRel, Ordering::Relaxed, |n| {
                (n < self.max).then_some(n + 1)
            })
            .ok()
            .map(|_| Permit {
                limit: Arc::clone(self),
            })
    }

    /// Slots in use.
    #[must_use]
    pub fn active(&self) -> u32 {
        self.active.load(Ordering::Relaxed)
    }

    /// The configured maximum.
    #[must_use]
    pub fn max(&self) -> u32 {
        self.max
    }
}

/// A held slot; released on drop.
#[derive(Debug)]
pub struct Permit {
    limit: Arc<Limit>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.limit.active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The four limits of one cluster.
#[derive(Debug)]
pub struct CircuitBreakers {
    /// Upstream connections opened for this cluster.
    pub connections: Arc<Limit>,
    /// Requests waiting for a connection.
    pub pending: Arc<Limit>,
    /// Requests in flight.
    pub requests: Arc<Limit>,
    /// Policy retries in flight.
    pub retries: Arc<Limit>,
}

impl CircuitBreakers {
    /// Breakers with the given maxima.
    #[must_use]
    pub fn new(connections: u32, pending: u32, requests: u32, retries: u32) -> Self {
        Self {
            connections: Limit::new(connections),
            pending: Limit::new(pending),
            requests: Limit::new(requests),
            retries: Limit::new(retries),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_is_enforced_and_released() {
        let l = Limit::new(2);
        let a = l.try_acquire().unwrap();
        let _b = l.try_acquire().unwrap();
        assert!(l.try_acquire().is_none());
        assert_eq!(l.active(), 2);
        drop(a);
        assert!(l.try_acquire().is_some());
    }

    #[test]
    fn zero_limit_never_grants() {
        assert!(Limit::new(0).try_acquire().is_none());
    }

    #[test]
    fn concurrent_acquires_never_exceed_max() {
        let l = Limit::new(10);
        let held: Vec<Vec<Permit>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let l = Arc::clone(&l);
                    s.spawn(move || (0..10).filter_map(|_| l.try_acquire()).collect())
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(held.iter().map(Vec::len).sum::<usize>(), 10);
        assert_eq!(l.active(), 10);
        drop(held);
        assert_eq!(l.active(), 0);
    }
}
