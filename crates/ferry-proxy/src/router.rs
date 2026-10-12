//! Request routing: host, path, method, headers and query to a rule and cluster.
//!
//! Precedence follows the Gateway API (ADR 0009). Hostnames come first: an
//! exact hostname beats the longest matching wildcard, which beats routes with
//! no hostnames. Within one hostname group the first matching candidate in this
//! order wins: exact path, then regular expression, then the longest prefix;
//! then a method condition; then the most header conditions; then the most
//! query conditions; then the route listed first; then the rule listed first.
//! If nothing in a group matches, the next less specific group is tried.
//!
//! [`Router::new`] precomputes one candidate list per hostname group, already
//! sorted, and indexes groups by exact hostname and by wildcard suffix. A
//! lookup is therefore one hash lookup per DNS label of the request host plus a
//! scan that stops at the first match, instead of a scan of every route.

use std::borrow::Cow;
use std::collections::HashMap;

use ferry_config::{ClusterId, HostMatch, Listener, RequestMatch, Route, Rule};
use ferry_lb::{Balancer, WeightedSelector};
use http::{HeaderMap, Method};

/// A compiled route table for one listener. Immutable after construction
/// except for the lock-free counters inside each rule's selector.
#[derive(Debug)]
pub struct Router {
    routes: Vec<CompiledRoute>,
    exact: HashMap<String, Vec<Candidate>>,
    /// Keyed by suffix including the leading dot, such as `.example.com`.
    wildcard: HashMap<String, Vec<Candidate>>,
    any: Vec<Candidate>,
}

#[derive(Debug)]
struct CompiledRoute {
    route: Route,
    /// One selector per rule, parallel to `route.rules()`.
    selectors: Vec<WeightedSelector>,
}

/// One (route, rule, match) triple, stored in precedence order.
#[derive(Debug, Clone, Copy)]
struct Candidate {
    route: usize,
    rule: usize,
    matcher: usize,
}

/// The parts of a request the router looks at.
#[derive(Debug, Clone, Copy)]
pub struct RequestInfo<'a> {
    /// `Host` header or `:authority`; may carry a port and any letter case.
    pub host: &'a str,
    /// Request method.
    pub method: &'a Method,
    /// Path without the query string.
    pub path: &'a str,
    /// Raw query string without `?`.
    pub query: Option<&'a str>,
    /// Request headers.
    pub headers: &'a HeaderMap,
}

/// The outcome of routing a request.
#[derive(Debug, Clone, Copy)]
pub struct Decision<'a> {
    /// Name of the matched route.
    pub route: &'a str,
    /// Index of the matched route in the listener's routes.
    pub route_index: usize,
    /// Index of the matched rule within the route.
    pub rule_index: usize,
    /// The matched rule.
    pub rule: &'a Rule,
    /// The match that selected the rule (needed for prefix rewrites).
    pub matched: &'a RequestMatch,
    /// Chosen cluster, or `None` if the rule redirects or every backend has
    /// weight zero (the Gateway API says the latter gets a 500).
    pub cluster: Option<ClusterId>,
}

impl Router {
    /// Compiles the routes of a listener.
    #[must_use]
    pub fn new(listener: &Listener) -> Self {
        let routes: Vec<CompiledRoute> = listener
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

        let mut exact: HashMap<String, Vec<Candidate>> = HashMap::new();
        let mut wildcard: HashMap<String, Vec<Candidate>> = HashMap::new();
        let mut any = Vec::new();
        for (ri, compiled) in routes.iter().enumerate() {
            let candidates = candidates_of(ri, &compiled.route);
            if compiled.route.hostnames().is_empty() {
                any.extend_from_slice(&candidates);
            }
            for host in compiled.route.hostnames() {
                let bucket = match host {
                    HostMatch::Exact(name) => exact.entry(name.clone()).or_default(),
                    HostMatch::Wildcard(suffix) => wildcard.entry(suffix.clone()).or_default(),
                    _ => continue,
                };
                bucket.extend_from_slice(&candidates);
            }
        }
        let mut router = Self {
            routes,
            exact,
            wildcard,
            any,
        };
        let sort = |list: &mut Vec<Candidate>, routes: &[CompiledRoute]| {
            list.sort_by(|a, b| {
                let pa = precedence(routes, a);
                let pb = precedence(routes, b);
                pb.cmp(&pa)
                    .then(a.route.cmp(&b.route))
                    .then(a.rule.cmp(&b.rule))
                    .then(a.matcher.cmp(&b.matcher))
            });
            // A route listing two hostnames that land in one bucket (only
            // possible for duplicates) would add its candidates twice.
            list.dedup_by(|a, b| a.route == b.route && a.rule == b.rule && a.matcher == b.matcher);
        };
        let routes = &router.routes;
        for list in router.exact.values_mut() {
            sort(list, routes);
        }
        for list in router.wildcard.values_mut() {
            sort(list, routes);
        }
        sort(&mut router.any, routes);
        router
    }

    /// Routes a request, or returns `None` if no rule matches.
    #[must_use]
    pub fn route(&self, req: &RequestInfo<'_>) -> Option<Decision<'_>> {
        let host = normalize_host(req.host);
        if let Some(list) = self.exact.get(host.as_ref())
            && let Some(d) = self.first_match(list, req)
        {
            return Some(d);
        }
        if !self.wildcard.is_empty() {
            // Suffixes from longest to shortest: for a.b.example.com try
            // .b.example.com, then .example.com, then .com.
            for (i, _) in host.match_indices('.').filter(|(i, _)| *i > 0) {
                if let Some(list) = host.get(i..).and_then(|s| self.wildcard.get(s))
                    && let Some(d) = self.first_match(list, req)
                {
                    return Some(d);
                }
            }
        }
        self.first_match(&self.any, req)
    }

    fn first_match(&self, list: &[Candidate], req: &RequestInfo<'_>) -> Option<Decision<'_>> {
        list.iter().find_map(|c| {
            let compiled = self.routes.get(c.route)?;
            let rule = compiled.route.rules().get(c.rule)?;
            let matched = rule.matches().get(c.matcher)?;
            if !matched.matches(req.method, req.path, req.query, req.headers) {
                return None;
            }
            let cluster = compiled
                .selectors
                .get(c.rule)
                .and_then(Balancer::pick)
                .and_then(|i| rule.backends().get(i))
                .map(ferry_config::WeightedBackend::cluster);
            Some(Decision {
                route: compiled.route.name(),
                route_index: c.route,
                rule_index: c.rule,
                rule,
                matched,
                cluster,
            })
        })
    }
}

fn candidates_of(route_index: usize, route: &Route) -> Vec<Candidate> {
    route
        .rules()
        .iter()
        .enumerate()
        .flat_map(|(rule, r)| {
            (0..r.matches().len()).map(move |matcher| Candidate {
                route: route_index,
                rule,
                matcher,
            })
        })
        .collect()
}

fn precedence(routes: &[CompiledRoute], c: &Candidate) -> Option<ferry_config::MatchPrecedence> {
    routes
        .get(c.route)?
        .route
        .rules()
        .get(c.rule)?
        .matches()
        .get(c.matcher)
        .map(RequestMatch::precedence)
}

/// Lowercases a host and strips any port and trailing dot, borrowing when
/// nothing changes.
pub(crate) fn normalize_host(host: &str) -> Cow<'_, str> {
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
    use http::HeaderValue;

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
      - name: deep-wildcard
        hostnames: ["*.eu.example.com"]
        rules:
          - backends: [{ cluster: c2 }]
      - name: exact-host
        hostnames: ["api.example.com"]
        rules:
          - matches: [{ path: { value: /v1 } }]
            backends: [{ cluster: c2 }]
          - matches: [{ path: { type: Exact, value: /v1/health } }]
            backends: [{ cluster: c3 }]
          - matches: [{ path: { value: /v1/users } }]
            backends: [{ cluster: c0 }]
          - matches: [{ path: { type: RegularExpression, value: '/v1/users/\d+' } }]
            backends: [{ cluster: c1 }]
      - name: drained
        hostnames: ["drained.example.com"]
        rules:
          - backends: [{ cluster: c0, weight: 0 }]
      - name: conditions
        hostnames: ["cond.test"]
        rules:
          - matches: [{ path: { value: / } }]
            backends: [{ cluster: c0 }]
          - matches: [{ query_params: [{ name: v, value: "2" }] }]
            backends: [{ cluster: c1 }]
          - matches: [{ headers: [{ name: x-canary, value: "1" }] }]
            backends: [{ cluster: c2 }]
          - matches: [{ method: POST }]
            backends: [{ cluster: c3 }]
clusters:
  - { name: c0, endpoints: [{ address: 10.0.0.1:80 }] }
  - { name: c1, endpoints: [{ address: 10.0.0.2:80 }] }
  - { name: c2, endpoints: [{ address: 10.0.0.3:80 }] }
  - { name: c3, endpoints: [{ address: 10.0.0.4:80 }] }
"#;

    struct Req {
        host: &'static str,
        method: Method,
        path: &'static str,
        query: Option<&'static str>,
        headers: HeaderMap,
    }

    fn req(host: &'static str, path: &'static str) -> Req {
        Req {
            host,
            method: Method::GET,
            path,
            query: None,
            headers: HeaderMap::new(),
        }
    }

    fn route_of(r: &Router, q: &Req) -> Option<(String, Option<usize>)> {
        r.route(&RequestInfo {
            host: q.host,
            method: &q.method,
            path: q.path,
            query: q.query,
            headers: &q.headers,
        })
        .map(|d| (d.route.to_owned(), d.cluster.map(ClusterId::index)))
    }

    #[test]
    fn exact_host_beats_wildcard_beats_catch_all() {
        let r = router(TABLE);
        assert_eq!(
            route_of(&r, &req("api.example.com", "/v1/x")).unwrap().0,
            "exact-host"
        );
        assert_eq!(
            route_of(&r, &req("www.example.com", "/")).unwrap().0,
            "wildcard"
        );
        assert_eq!(
            route_of(&r, &req("other.test", "/")).unwrap().0,
            "catch-all"
        );
    }

    #[test]
    fn longest_wildcard_suffix_wins() {
        let r = router(TABLE);
        assert_eq!(
            route_of(&r, &req("x.eu.example.com", "/")).unwrap().0,
            "deep-wildcard"
        );
        assert_eq!(
            route_of(&r, &req("eu.example.com", "/")).unwrap().0,
            "wildcard"
        );
    }

    #[test]
    fn exact_regex_prefix_precedence() {
        let r = router(TABLE);
        let at = |p| route_of(&r, &req("api.example.com", p)).unwrap().1;
        assert_eq!(at("/v1/health"), Some(3), "exact");
        assert_eq!(at("/v1/users/7"), Some(1), "regex beats the longer prefix");
        assert_eq!(at("/v1/users/me"), Some(0), "longest prefix");
        assert_eq!(at("/v1"), Some(2));
    }

    #[test]
    fn method_beats_headers_beats_query() {
        let r = router(TABLE);
        let mut q = req("cond.test", "/");
        assert_eq!(route_of(&r, &q).unwrap().1, Some(0));
        q.query = Some("v=2");
        assert_eq!(route_of(&r, &q).unwrap().1, Some(1));
        q.headers.insert("x-canary", HeaderValue::from_static("1"));
        assert_eq!(route_of(&r, &q).unwrap().1, Some(2));
        q.method = Method::POST;
        assert_eq!(route_of(&r, &q).unwrap().1, Some(3));
    }

    #[test]
    fn host_match_ignores_port_case_and_trailing_dot() {
        let r = router(TABLE);
        assert_eq!(
            route_of(&r, &req("API.Example.COM:8443", "/v1")).unwrap().0,
            "exact-host"
        );
        assert_eq!(
            route_of(&r, &req("api.example.com.", "/v1")).unwrap().0,
            "exact-host"
        );
    }

    #[test]
    fn exact_host_with_unmatched_path_falls_back_to_less_specific_host() {
        // Gateway API: hostname precedence applies to matching rules only.
        let r = router(TABLE);
        assert_eq!(
            route_of(&r, &req("api.example.com", "/other")).unwrap().0,
            "wildcard"
        );
    }

    #[test]
    fn zero_weight_rule_matches_without_cluster() {
        let r = router(TABLE);
        assert_eq!(
            route_of(&r, &req("drained.example.com", "/")),
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
        assert_eq!(route_of(&r, &req("nothing.test", "/")), None);
    }

    #[test]
    fn normalize_host_cases() {
        assert_eq!(normalize_host("Example.com:80"), "example.com");
        assert_eq!(normalize_host("[::1]:8080"), "::1");
        assert_eq!(normalize_host("example.com"), "example.com");
        assert!(matches!(normalize_host("example.com"), Cow::Borrowed(_)));
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        type Key = (
            ferry_config::HostSpecificity,
            ferry_config::MatchPrecedence,
            std::cmp::Reverse<(usize, usize)>,
        );

        /// Brute force: the most specific host group with any matching rule,
        /// then the highest-precedence match within it.
        fn reference(listener: &Listener, host: &str, path: &str) -> Option<(usize, usize)> {
            let host = normalize_host(host);
            let empty = HeaderMap::new();
            let mut best: Option<Key> = None;
            for (ri, route) in listener.routes().iter().enumerate() {
                let Some(spec) = route.host_specificity(&host) else {
                    continue;
                };
                for (rule_i, rule) in route.rules().iter().enumerate() {
                    for m in rule.matches() {
                        if !m.matches(&Method::GET, path, None, &empty) {
                            continue;
                        }
                        let key = (spec, m.precedence(), std::cmp::Reverse((ri, rule_i)));
                        if best.as_ref().is_none_or(|b| key > *b) {
                            best = Some(key);
                        }
                    }
                }
            }
            best.map(|(_, _, std::cmp::Reverse(k))| k)
        }

        fn table() -> impl Strategy<Value = String> {
            let host = prop::sample::select(vec!["", "a.test", "*.test", "b.a.test", "*.a.test"]);
            let path = prop::sample::select(vec!["/", "/x", "/x/y", "/y"]);
            let kind = prop::sample::select(vec!["Exact", "PathPrefix"]);
            prop::collection::vec((host, prop::collection::vec((kind, path), 1..3)), 1..6).prop_map(
                |routes| {
                    let mut y = String::from("listeners:\n  - name: l\n    address: 127.0.0.1:0\n    routes:\n");
                    for (i, (host, rules)) in routes.iter().enumerate() {
                        y.push_str(&format!("      - name: r{i}\n"));
                        if !host.is_empty() {
                            y.push_str(&format!("        hostnames: ['{host}']\n"));
                        }
                        y.push_str("        rules:\n");
                        for (kind, path) in rules {
                            y.push_str(&format!(
                                "          - matches: [{{ path: {{ type: {kind}, value: '{path}' }} }}]\n            backends: [{{ cluster: c }}]\n"
                            ));
                        }
                    }
                    y.push_str("clusters: [{ name: c, endpoints: [{ address: 10.0.0.1:80 }] }]\n");
                    y
                },
            )
        }

        proptest! {
            #[test]
            fn index_agrees_with_brute_force(
                yaml in table(),
                host in prop::sample::select(vec!["a.test", "b.a.test", "c.b.a.test", "other", "A.TEST:80"]),
                path in prop::sample::select(vec!["/", "/x", "/x/y", "/x/y/z", "/y", "/z"]),
            ) {
                let snap = from_yaml_str(&yaml).unwrap().into_snapshot(SnapshotVersion(1)).unwrap();
                let listener = &snap.listeners()[0];
                let r = Router::new(listener);
                let empty = HeaderMap::new();
                let got = r.route(&RequestInfo { host, method: &Method::GET, path, query: None, headers: &empty })
                    .map(|d| {
                        let ri = listener.routes().iter().position(|x| x.name() == d.route).unwrap();
                        (ri, d.rule_index)
                    });
                prop_assert_eq!(got, reference(listener, host, path));
            }
        }
    }
}
