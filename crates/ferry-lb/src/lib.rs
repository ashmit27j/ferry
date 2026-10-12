//! Load balancing, health checking and upstream connection pools for Ferry.
//!
//! - [`ClusterBalancer`] picks an endpoint of a cluster with one of five
//!   policies (round robin, weighted round robin, least request, P2C-EWMA and
//!   Maglev), skipping unhealthy endpoints unless the cluster is in panic mode.
//! - [`EndpointState`] is the live, reload-surviving state of one endpoint:
//!   health, in-flight requests and a peak latency EWMA.
//! - [`health`] runs active HTTP and gRPC health checks.
//! - [`pool`] keeps upstream HTTP/1.1 and h2c connections.
//! - [`WeightedSelector`] and [`RoundRobin`] implement the small [`Balancer`]
//!   trait for splitting a rule's traffic across clusters.
//!
//! Every pick is lock-free and allocation-free; anything that allocates
//! (rebuilding a host set or a Maglev table) runs off the hot path.

mod budget;
mod cluster;
mod endpoint;
pub mod health;
pub mod maglev;
pub mod pool;
mod round_robin;
mod weighted;

pub use budget::RetryBudget;
pub use cluster::{ClusterBalancer, Host, HostSet, Policy};
pub use endpoint::{DEFAULT_RTT, EWMA_DECAY, EndpointState, FAILURE_PENALTY, RequestGuard};
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
