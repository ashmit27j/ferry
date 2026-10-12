//! Validated, immutable configuration snapshots.
//!
//! A [`Snapshot`] is the only configuration the data plane ever sees. It can only
//! be built by [`Config::into_snapshot`](crate::Config::into_snapshot), so holding
//! one proves that every cross-reference resolves and every value is in range.
//! Snapshots are never mutated: a reload builds a new one and swaps it in.

use std::fmt;
use std::net::SocketAddr;

use serde::Serialize;

pub use crate::model::LbPolicy;

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
            .position(|c| c.name == name)
            .map(ClusterId)
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HostSpecificity {
    /// The route lists no hostnames.
    Any,
    /// A wildcard matched; the value is the suffix length, so longer suffixes win.
    Wildcard(usize),
    /// The hostname matched exactly.
    Exact,
}

/// A hostname matcher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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

/// A rule: OR of matches, then a weighted split across backends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Rule {
    pub(crate) matches: Vec<RequestMatch>,
    pub(crate) backends: Vec<WeightedBackend>,
}

impl Rule {
    /// Matches combined with OR. Never empty after validation.
    #[must_use]
    pub fn matches(&self) -> &[RequestMatch] {
        &self.matches
    }

    /// Backends with non-zero total weight.
    #[must_use]
    pub fn backends(&self) -> &[WeightedBackend] {
        &self.backends
    }
}

/// Conditions that must all hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RequestMatch {
    pub(crate) path: PathMatch,
}

impl RequestMatch {
    /// The path condition.
    #[must_use]
    pub fn path(&self) -> &PathMatch {
        &self.path
    }
}

/// A path condition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum PathMatch {
    /// Path equals the value.
    Exact(String),
    /// Path starts with the value on a `/` boundary. Stored without a trailing
    /// slash, except for the root prefix `/`.
    Prefix(String),
}

impl PathMatch {
    /// Returns true if `path` (without query string) satisfies this condition.
    ///
    /// Prefix matching follows the Gateway API: `/foo` matches `/foo`, `/foo/`
    /// and `/foo/bar`, but not `/foobar`.
    #[must_use]
    pub fn matches(&self, path: &str) -> bool {
        match self {
            Self::Exact(value) => path == value,
            Self::Prefix(prefix) => {
                if prefix == "/" {
                    return path.starts_with('/');
                }
                match path.strip_prefix(prefix.as_str()) {
                    Some(rest) => rest.is_empty() || rest.starts_with('/'),
                    None => false,
                }
            }
        }
    }

    /// Precedence key: exact matches beat prefixes, longer prefixes beat shorter.
    #[must_use]
    pub fn precedence(&self) -> (u8, usize) {
        match self {
            Self::Exact(value) => (1, value.len()),
            Self::Prefix(prefix) => (0, prefix.len()),
        }
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

/// A group of upstream endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Cluster {
    pub(crate) name: String,
    pub(crate) lb_policy: LbPolicy,
    pub(crate) endpoints: Vec<Endpoint>,
}

impl Cluster {
    /// Cluster name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Load-balancing policy.
    #[must_use]
    pub fn lb_policy(&self) -> LbPolicy {
        self.lb_policy
    }

    /// Endpoints. Never empty after validation.
    #[must_use]
    pub fn endpoints(&self) -> &[Endpoint] {
        &self.endpoints
    }
}

/// One upstream address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Endpoint {
    pub(crate) address: SocketAddr,
}

impl Endpoint {
    /// Socket address.
    #[must_use]
    pub fn address(&self) -> SocketAddr {
        self.address
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_matches_on_element_boundary() {
        let m = PathMatch::Prefix("/foo".into());
        assert!(m.matches("/foo"));
        assert!(m.matches("/foo/"));
        assert!(m.matches("/foo/bar"));
        assert!(!m.matches("/foobar"));
        assert!(!m.matches("/fo"));
    }

    #[test]
    fn root_prefix_matches_everything() {
        let m = PathMatch::Prefix("/".into());
        assert!(m.matches("/"));
        assert!(m.matches("/anything/at/all"));
    }

    #[test]
    fn exact_match_is_exact() {
        let m = PathMatch::Exact("/healthz".into());
        assert!(m.matches("/healthz"));
        assert!(!m.matches("/healthz/"));
    }

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
}
