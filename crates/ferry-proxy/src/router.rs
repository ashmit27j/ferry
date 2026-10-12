//! Request routing: host and path to a weighted backend choice.
//!
//! Precedence follows the Gateway API: the most specific hostname match wins
//! (exact, then the longest wildcard suffix, then routes with no hostnames).
//! Among those, an exact path beats a prefix and a longer prefix beats a
//! shorter one. Remaining ties go to the route, then rule, listed first.

use std::borrow::Cow;
use std::cmp::Reverse;

use ferry_config::{ClusterId, HostSpecificity, Listener, Route};
use ferry_lb::{Balancer, WeightedSelector};

/// A compiled route table for one listener. Immutable after construction
/// except for the lock-free counters inside each rule's selector.
#[derive(Debug)]
pub struct Router {
    routes: Vec<CompiledRoute>,
}

#[derive(Debug)]
struct CompiledRoute {
    route: Route,
    /// One selector per rule, parallel to `route.rules()`.
    selectors: Vec<WeightedSelector>,
}

/// The outcome of routing a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision<'a> {
    /// Name of the matched route.
    pub route: &'a str,
    /// Index of the matched rule within the route.
    pub rule: usize,
    /// Chosen cluster, or `None` if every backend of the rule has weight zero
    /// (the Gateway API says such requests get a 500).
    pub cluster: Option<ClusterId>,
}

type Precedence = (HostSpecificity, (u8, usize), Reverse<usize>, Reverse<usize>);

impl Router {
    /// Compiles the routes of a listener.
    #[must_use]
    pub fn new(listener: &Listener) -> Self {
        let routes = listener
            .routes()
            .iter()
            .map(|route| CompiledRoute {
                selectors: route
                    .rules()
                    .iter()
                    .map(|rule| {
                        let weights: Vec<u32> =
                            rule.backends().iter().map(|b| b.weight()).collect();
                        WeightedSelector::new(&weights)
                    })
                    .collect(),
                route: route.clone(),
            })
            .collect();
        Self { routes }
    }

    /// Routes a request. `host` may carry a port and any letter case; `path`
    /// must not carry the query string.
    #[must_use]
    pub fn route(&self, host: &str, path: &str) -> Option<Decision<'_>> {
        let host = normalize_host(host);
        let mut best: Option<(Precedence, usize, usize)> = None;
        for (ri, compiled) in self.routes.iter().enumerate() {
            let Some(spec) = compiled.route.host_specificity(&host) else {
                continue;
            };
            for (rule_i, rule) in compiled.route.rules().iter().enumerate() {
                for m in rule.matches() {
                    if !m.path().matches(path) {
                        continue;
                    }
                    let key = (spec, m.path().precedence(), Reverse(ri), Reverse(rule_i));
                    if best.as_ref().is_none_or(|(b, _, _)| key > *b) {
                        best = Some((key, ri, rule_i));
                    }
                }
            }
        }
        let (_, ri, rule_i) = best?;
        let compiled = self.routes.get(ri)?;
        let rule = compiled.route.rules().get(rule_i)?;
        let cluster = compiled
            .selectors
            .get(rule_i)
            .and_then(Balancer::pick)
            .and_then(|i| rule.backends().get(i))
            .map(ferry_config::WeightedBackend::cluster);
        Some(Decision {
            route: compiled.route.name(),
            rule: rule_i,
            cluster,
        })
    }
}

/// Lowercases a host and strips any port, borrowing when nothing changes.
fn normalize_host(host: &str) -> Cow<'_, str> {
    let without_port = if let Some(rest) = host.strip_prefix('[') {
        // IPv6 literal: "[::1]:8080" -> "::1"
        rest.split_once(']').map_or(rest, |(ip, _)| ip)
    } else {
        host.rsplit_once(':')
            .filter(|(_, port)| port.bytes().all(|b| b.is_ascii_digit()))
            .map_or(host, |(name, _)| name)
    };
    let trimmed = without_port.strip_suffix('.').unwrap_or(without_port);
    if trimmed.bytes().any(|b| b.is_ascii_uppercase()) {
        Cow::Owned(trimmed.to_ascii_lowercase())
    } else {
        Cow::Borrowed(trimmed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferry_config::{SnapshotVersion, from_yaml_str};

    fn router(yaml: &str) -> Router {
        let snap = from_yaml_str(yaml)
            .unwrap()
            .into_snapshot(SnapshotVersion(1))
            .unwrap();
        Router::new(&snap.listeners()[0])
    }

    const TABLE: &str = r#"
listeners:
  - name: l
    address: 127.0.0.1:0
    routes:
      - name: catch-all
        rules:
          - backends: [{ cluster: c0 }]
      - name: wildcard
        hostnames: ["*.example.com"]
        rules:
          - backends: [{ cluster: c1 }]
      - name: exact-host
        hostnames: ["api.example.com"]
        rules:
          - matches: [{ path: { value: /v1 } }]
            backends: [{ cluster: c2 }]
          - matches: [{ path: { type: Exact, value: /v1/health } }]
            backends: [{ cluster: c3 }]
          - matches: [{ path: { value: /v1/users } }]
            backends: [{ cluster: c0 }]
      - name: drained
        hostnames: ["drained.example.com"]
        rules:
          - backends: [{ cluster: c0, weight: 0 }]
clusters:
  - { name: c0, endpoints: [{ address: 10.0.0.1:80 }] }
  - { name: c1, endpoints: [{ address: 10.0.0.2:80 }] }
  - { name: c2, endpoints: [{ address: 10.0.0.3:80 }] }
  - { name: c3, endpoints: [{ address: 10.0.0.4:80 }] }
"#;

    fn route_of(r: &Router, host: &str, path: &str) -> Option<(String, Option<usize>)> {
        r.route(host, path)
            .map(|d| (d.route.to_owned(), d.cluster.map(ClusterId::index)))
    }

    #[test]
    fn exact_host_beats_wildcard_beats_catch_all() {
        let r = router(TABLE);
        assert_eq!(
            route_of(&r, "api.example.com", "/v1/x").unwrap().0,
            "exact-host"
        );
        assert_eq!(route_of(&r, "www.example.com", "/").unwrap().0, "wildcard");
        assert_eq!(route_of(&r, "other.test", "/").unwrap().0, "catch-all");
    }

    #[test]
    fn exact_path_beats_prefix_and_longest_prefix_wins() {
        let r = router(TABLE);
        assert_eq!(
            route_of(&r, "api.example.com", "/v1/health").unwrap().1,
            Some(3)
        );
        assert_eq!(
            route_of(&r, "api.example.com", "/v1/users/7").unwrap().1,
            Some(0)
        );
        assert_eq!(route_of(&r, "api.example.com", "/v1").unwrap().1, Some(2));
    }

    #[test]
    fn host_match_ignores_port_case_and_trailing_dot() {
        let r = router(TABLE);
        assert_eq!(
            route_of(&r, "API.Example.COM:8443", "/v1").unwrap().0,
            "exact-host"
        );
        assert_eq!(
            route_of(&r, "api.example.com.", "/v1").unwrap().0,
            "exact-host"
        );
    }

    #[test]
    fn exact_host_with_unmatched_path_falls_back_to_less_specific_host() {
        // Gateway API: hostname precedence applies to matching rules only.
        let r = router(TABLE);
        assert_eq!(
            route_of(&r, "api.example.com", "/other").unwrap().0,
            "wildcard"
        );
    }

    #[test]
    fn zero_weight_rule_matches_without_cluster() {
        let r = router(TABLE);
        assert_eq!(
            route_of(&r, "drained.example.com", "/"),
            Some(("drained".into(), None))
        );
    }

    #[test]
    fn no_match_returns_none() {
        let yaml = TABLE.replace(
            "      - name: catch-all\n        rules:\n          - backends: [{ cluster: c0 }]\n",
            "",
        );
        let r = router(&yaml);
        assert_eq!(route_of(&r, "nothing.test", "/"), None);
    }

    #[test]
    fn normalize_host_cases() {
        assert_eq!(normalize_host("Example.com:80"), "example.com");
        assert_eq!(normalize_host("[::1]:8080"), "::1");
        assert_eq!(normalize_host("example.com"), "example.com");
        assert!(matches!(normalize_host("example.com"), Cow::Borrowed(_)));
    }
}
