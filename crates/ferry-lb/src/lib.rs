//! Load balancing for Ferry.
//!
//! Balancers pick an index into a slice the caller owns (endpoints of a
//! cluster, or backends of a rule). They hold only atomics, so a pick never
//! takes a lock and never allocates.
//!
//! M0 ships [`RoundRobin`] for endpoints and [`WeightedSelector`] for splitting
//! traffic across a rule's backends. M1 adds weighted round robin,
//! least-request, P2C-EWMA and Maglev behind the same [`Balancer`] trait.

mod round_robin;
mod weighted;

pub use round_robin::RoundRobin;
pub use weighted::WeightedSelector;

/// Picks one of `len` candidates.
pub trait Balancer: Send + Sync + std::fmt::Debug {
    /// Returns an index in `0..len()`, or `None` if there is nothing to pick.
    fn pick(&self) -> Option<usize>;

    /// Number of candidates this balancer was built for.
    fn len(&self) -> usize;

    /// True when there are no candidates.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
