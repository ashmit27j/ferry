//! Validated, immutable configuration snapshots.
//!
//! A [`Snapshot`] is the only configuration the data plane ever sees. It can only
//! be built by [`Config::into_snapshot`](crate::Config::into_snapshot), so holding
//! one proves that every cross-reference resolves and every value is in range.
//! Snapshots are never mutated: a reload builds a new one and swaps it in.
//!
//! Every type here has private fields and read-only accessors, and every enum is
//! `#[non_exhaustive]`, so the API can grow without breaking (ADR 0013).

mod cluster;
mod filter;
mod matching;
mod ser;

use std::fmt;
use std::net::SocketAddr;

use serde::Serialize;

pub use cluster::{Cluster, Endpoint, HashPolicy, HealthCheck, HealthCheckKind};
pub use filter::{Filter, HeaderModifier, PathRewrite, Redirect, RedirectTarget, UrlRewrite};
pub use matching::{
    HeaderMatch, MatchPrecedence, PathMatch, Pattern, QueryParamMatch, RequestMatch, StringMatch,
};

/// Monotonic configuration version, assigned by whoever builds the snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct SnapshotVersion(pub u64);

impl fmt::Display for SnapshotVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}", self.0)
    }
}

/// A complete, validated configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Snapshot {
    pub(crate) version: SnapshotVersion,
    pub(crate) listeners: Vec<Listener>,
    pub(crate) clusters: Vec<Cluster>,
}

impl Snapshot {
    /// The version this snapshot was built with.
    #[must_use]
    pub fn version(&self) -> SnapshotVersion {
        self.version
    }

    /// Listeners in file order.
    #[must_use]
    pub fn listeners(&self) -> &[Listener] {
        &self.listeners
    }

    /// Finds a listener by name.
    #[must_use]
    pub fn listener(&self, name: &str) -> Option<&Listener> {
        self.listeners.iter().find(|l| l.name == name)
    }

    /// Clusters in file order. [`ClusterId`] indexes into this slice.
    #[must_use]
    pub fn clusters(&self) -> &[Cluster] {
        &self.clusters
    }

    /// Looks up a cluster by id.
    #[must_use]
    pub fn cluster(&self, id: ClusterId) -> Option<&Cluster> {
        self.clusters.get(id.0)
    }

    /// Finds the id of the cluster called `name`.
    #[must_use]
    pub fn cluster_id(&self, name: &str) -> Option<ClusterId> {
        self.clusters
            .iter()
            .position(|c| c.name() == name)
            .map(ClusterId)
    }

    /// The same snapshot with a different version, for callers that number
    /// snapshots after validating them (such as the file watcher).
    #[must_use]
    pub fn with_version(mut self, version: SnapshotVersion) -> Self {
        self.version = version;
        self
    }
}

/// A listener and its route table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Listener {
    pub(crate) name: String,
    pub(crate) address: SocketAddr,
    pub(crate) routes: Vec<Route>,
}

impl Listener {
    /// Listener name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Bind address.
    #[must_use]
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Routes in file order.
    #[must_use]
    pub fn routes(&self) -> &[Route] {
        &self.routes
    }
}

/// A route: hostnames plus rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Route {
    pub(crate) name: String,
    pub(crate) hostnames: Vec<HostMatch>,
    pub(crate) rules: Vec<Rule>,
}

impl Route {
    /// Route name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Hostname matchers. Empty means any host.
    #[must_use]
    pub fn hostnames(&self) -> &[HostMatch] {
        &self.hostnames
    }

    /// Rules in file order.
    #[must_use]
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// How specifically this route matches `host`, or `None` if it does not.
    ///
    /// `host` must already be lowercased and stripped of any port.
    #[must_use]
    pub fn host_specificity(&self, host: &str) -> Option<HostSpecificity> {
        if self.hostnames.is_empty() {
            return Some(HostSpecificity::Any);
        }
        self.hostnames.iter().filter_map(|h| h.matches(host)).max()
    }
}

/// How specifically a route's hostname matched, ordered from least to most specific.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum HostSpecificity {
    /// The route lists no hostnames.
    Any,
    /// A wildcard matched; the value is the suffix length, so longer suffixes win.
    Wildcard(usize),
    /// The hostname matched exactly.
    Exact,
}

/// A hostname matcher.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[non_exhaustive]
pub enum HostMatch {
    /// Exact, lowercase hostname.
    Exact(String),
    /// `*.example.com`, stored as the suffix `.example.com`.
    Wildcard(String),
}

impl HostMatch {
    /// Returns how specifically `host` matches, or `None`.
    ///
    /// `host` must already be lowercased and stripped of any port.
    #[must_use]
    pub fn matches(&self, host: &str) -> Option<HostSpecificity> {
        match self {
            Self::Exact(name) => (name == host).then_some(HostSpecificity::Exact),
            Self::Wildcard(suffix) => (host.len() > suffix.len()
                && host.ends_with(suffix.as_str()))
            .then_some(HostSpecificity::Wildcard(suffix.len())),
        }
    }
}

/// A rule: OR of matches, then filters, then a weighted split across backends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Rule {
    pub(crate) matches: Vec<RequestMatch>,
    pub(crate) filters: Vec<Filter>,
    pub(crate) backends: Vec<WeightedBackend>,
}

impl Rule {
    /// Matches combined with OR. Never empty after validation.
    #[must_use]
    pub fn matches(&self) -> &[RequestMatch] {
        &self.matches
    }

    /// Filters in the order they apply.
    #[must_use]
    pub fn filters(&self) -> &[Filter] {
        &self.filters
    }

    /// The redirect filter, if the rule answers with a redirect.
    #[must_use]
    pub fn redirect(&self) -> Option<&Redirect> {
        self.filters.iter().find_map(|f| match f {
            Filter::RequestRedirect(r) => Some(r),
            _ => None,
        })
    }

    /// Backends. Empty only for redirect rules.
    #[must_use]
    pub fn backends(&self) -> &[WeightedBackend] {
        &self.backends
    }
}

/// Index of a cluster in [`Snapshot::clusters`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct ClusterId(pub(crate) usize);

impl ClusterId {
    /// The raw index, for building per-cluster runtime state in parallel arrays.
    #[must_use]
    pub fn index(self) -> usize {
        self.0
    }
}

/// A resolved backend reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WeightedBackend {
    pub(crate) cluster: ClusterId,
    pub(crate) weight: u32,
}

impl WeightedBackend {
    /// Target cluster.
    #[must_use]
    pub fn cluster(&self) -> ClusterId {
        self.cluster
    }

    /// Relative weight. May be zero for a drained backend.
    #[must_use]
    pub fn weight(&self) -> u32 {
        self.weight
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_requires_at_least_one_label() {
        let m = HostMatch::Wildcard(".example.com".into());
        assert!(m.matches("a.example.com").is_some());
        assert!(m.matches("a.b.example.com").is_some());
        assert!(m.matches("example.com").is_none());
        assert!(m.matches(".example.com").is_none());
    }

    #[test]
    fn exact_host_beats_wildcard_beats_any() {
        assert!(HostSpecificity::Exact > HostSpecificity::Wildcard(100));
        assert!(HostSpecificity::Wildcard(1) > HostSpecificity::Any);
        assert!(HostSpecificity::Wildcard(12) > HostSpecificity::Wildcard(4));
    }

    #[test]
    fn version_displays_with_prefix() {
        assert_eq!(SnapshotVersion(7).to_string(), "v7");
    }
}
