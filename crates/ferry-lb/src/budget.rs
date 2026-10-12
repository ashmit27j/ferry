//! Retry budgets (Linkerd's model).
//!
//! Every original request deposits into the budget; every retry withdraws.
//! Retries are allowed while withdrawals in the last `ttl` stay below
//! `percent` of deposits plus `min_per_second × ttl`. The floor keeps retries
//! working on quiet clusters; the percentage stops a struggling backend from
//! being buried under a retry storm when traffic is heavy.
//!
//! The window is ten buckets of `ttl / 10`, each tagged with its epoch and
//! reset lazily by whoever touches it first in a new epoch. Everything is a
//! relaxed atomic: under contention the budget may overshoot by a few retries,
//! which is harmless.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const BUCKETS: usize = 10;

/// A sliding-window retry budget.
#[derive(Debug)]
pub struct RetryBudget {
    percent: u64,
    floor: u64,
    bucket_ns: u64,
    buckets: [Bucket; BUCKETS],
}

#[derive(Debug, Default)]
struct Bucket {
    epoch: AtomicU64,
    deposits: AtomicU64,
    withdrawals: AtomicU64,
}

impl RetryBudget {
    /// A budget allowing `percent` (0–100) of requests as retries, plus
    /// `min_per_second` retries per second, over a window of `ttl`.
    #[must_use]
    pub fn new(percent: u8, min_per_second: u32, ttl: Duration) -> Self {
        let ttl = ttl.max(Duration::from_millis(BUCKETS as u64));
        let bucket_ns = u64::try_from(ttl.as_nanos() / BUCKETS as u128)
            .unwrap_or(u64::MAX)
            .max(1);
        Self {
            percent: u64::from(percent.min(100)),
            floor: u64::from(min_per_second).saturating_mul(ttl.as_secs().max(1)),
            bucket_ns,
            buckets: Default::default(),
        }
    }

    /// Records an original request.
    pub fn deposit(&self) {
        self.deposit_at(now_ns());
    }

    /// Takes one retry from the budget; false if it is exhausted.
    pub fn try_withdraw(&self) -> bool {
        self.try_withdraw_at(now_ns())
    }

    fn epoch(&self, now: u64) -> u64 {
        // +1 so that epoch 0 never collides with a fresh bucket's default.
        now / self.bucket_ns + 1
    }

    fn bucket(&self, epoch: u64) -> &Bucket {
        let i = usize::try_from(epoch % BUCKETS as u64).unwrap_or(0);
        let b = &self.buckets[i];
        let seen = b.epoch.load(Ordering::Relaxed);
        if seen != epoch
            && b.epoch
                .compare_exchange(seen, epoch, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            b.deposits.store(0, Ordering::Relaxed);
            b.withdrawals.store(0, Ordering::Relaxed);
        }
        b
    }

    fn deposit_at(&self, now: u64) {
        self.bucket(self.epoch(now))
            .deposits
            .fetch_add(1, Ordering::Relaxed);
    }

    fn try_withdraw_at(&self, now: u64) -> bool {
        let current = self.epoch(now);
        let (mut deposits, mut withdrawals) = (0u64, 0u64);
        for b in &self.buckets {
            let e = b.epoch.load(Ordering::Relaxed);
            if e + (BUCKETS as u64) > current && e <= current {
                deposits += b.deposits.load(Ordering::Relaxed);
                withdrawals += b.withdrawals.load(Ordering::Relaxed);
            }
        }
        let allowed = self.floor + deposits * self.percent / 100;
        if withdrawals >= allowed {
            return false;
        }
        self.bucket(current)
            .withdrawals
            .fetch_add(1, Ordering::Relaxed);
        true
    }
}

fn now_ns() -> u64 {
    static BASE: OnceLock<Instant> = OnceLock::new();
    let base = *BASE.get_or_init(Instant::now);
    u64::try_from(base.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const SEC: u64 = 1_000_000_000;

    #[test]
    fn floor_allows_retries_without_traffic() {
        let b = RetryBudget::new(20, 2, Duration::from_secs(10));
        let granted = (0..100).filter(|_| b.try_withdraw_at(SEC)).count();
        assert_eq!(granted, 20, "2/s over a 10 s window");
    }

    #[test]
    fn percentage_scales_with_requests() {
        let b = RetryBudget::new(20, 0, Duration::from_secs(10));
        for _ in 0..1000 {
            b.deposit_at(SEC);
        }
        let granted = (0..1000).filter(|_| b.try_withdraw_at(SEC)).count();
        assert_eq!(granted, 200);
    }

    #[test]
    fn old_traffic_ages_out() {
        let b = RetryBudget::new(50, 0, Duration::from_secs(10));
        for _ in 0..100 {
            b.deposit_at(SEC);
        }
        assert!(b.try_withdraw_at(SEC));
        // Eleven seconds later every bucket of that traffic has expired.
        assert!(!b.try_withdraw_at(12 * SEC));
    }

    #[test]
    fn zero_budget_never_retries() {
        let b = RetryBudget::new(0, 0, Duration::from_secs(1));
        b.deposit_at(SEC);
        assert!(!b.try_withdraw_at(SEC));
    }

    proptest! {
        #[test]
        fn withdrawals_never_exceed_the_allowance(
            percent in 0u8..=100,
            floor in 0u32..5,
            deposits in 0u64..500,
            attempts in 0usize..500,
        ) {
            let b = RetryBudget::new(percent, floor, Duration::from_secs(10));
            for _ in 0..deposits {
                b.deposit_at(SEC);
            }
            let granted = (0..attempts).filter(|_| b.try_withdraw_at(SEC)).count() as u64;
            let allowed = u64::from(floor) * 10 + deposits * u64::from(percent) / 100;
            prop_assert_eq!(granted, allowed.min(attempts as u64));
        }
    }
}
