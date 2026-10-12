//! Turns a parsed [`Config`] into a validated [`Snapshot`].
//!
//! Validation collects every problem rather than stopping at the first, so one
//! `check-config` run reports everything wrong with a file.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};

use crate::error::{ValidationError, ValidationErrors};
use crate::model::{Config, PathMatchConfig, PathMatchKind};
use crate::snapshot::{
    Cluster, ClusterId, Endpoint, HostMatch, Listener, PathMatch, RequestMatch, Route, Rule,
    Snapshot, SnapshotVersion, WeightedBackend,
};

/// Largest accepted backend weight, matching the Gateway API limit.
pub const MAX_BACKEND_WEIGHT: u32 = 1_000_000;
/// Longest accepted path match value.
pub const MAX_PATH_LEN: usize = 1024;
/// Longest accepted name for listeners, routes and clusters.
pub const MAX_NAME_LEN: usize = 253;

impl Config {
    /// Validates this configuration and freezes it into a [`Snapshot`].
    ///
    /// # Errors
    ///
    /// Returns every validation problem found, each with the path of the
    /// offending field (for example `listeners[0].routes[1].rules[0]`).
    pub fn into_snapshot(self, version: SnapshotVersion) -> Result<Snapshot, ValidationErrors> {
        let mut v = Validator::default();
        let clusters = v.clusters(&self);
        let cluster_ids: HashMap<&str, ClusterId> = self
            .clusters
            .iter()
            .enumerate()
            .map(|(i, c)| (c.name.as_str(), ClusterId(i)))
            .collect();
        let listeners = v.listeners(&self, &cluster_ids);
        v.finish(Snapshot {
            version,
            listeners,
            clusters,
        })
    }
}

#[derive(Default)]
struct Validator {
    errors: Vec<ValidationError>,
}

impl Validator {
    fn err(&mut self, path: impl Into<String>, message: impl Into<String>) {
        self.errors.push(ValidationError {
            path: path.into(),
            message: message.into(),
        });
    }

    fn finish(self, snapshot: Snapshot) -> Result<Snapshot, ValidationErrors> {
        if self.errors.is_empty() {
            Ok(snapshot)
        } else {
            Err(ValidationErrors(self.errors))
        }
    }

    fn name(&mut self, path: &str, name: &str) {
        if name.is_empty() {
            self.err(path, "must not be empty");
        } else if name.len() > MAX_NAME_LEN {
            self.err(path, format!("must be at most {MAX_NAME_LEN} characters"));
        } else if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            self.err(
                path,
                "may only contain ASCII letters, digits, '-', '_' and '.'",
            );
        }
    }

    fn clusters(&mut self, config: &Config) -> Vec<Cluster> {
        let mut seen = HashSet::new();
        let mut out = Vec::with_capacity(config.clusters.len());
        for (ci, cluster) in config.clusters.iter().enumerate() {
            let at = format!("clusters[{ci}]");
            self.name(&format!("{at}.name"), &cluster.name);
            if !seen.insert(cluster.name.as_str()) {
                self.err(
                    format!("{at}.name"),
                    format!("duplicate cluster name {:?}", cluster.name),
                );
            }
            if cluster.endpoints.is_empty() {
                self.err(format!("{at}.endpoints"), "must list at least one endpoint");
            }
            let mut addrs = HashSet::new();
            let mut endpoints = Vec::with_capacity(cluster.endpoints.len());
            for (ei, ep) in cluster.endpoints.iter().enumerate() {
                let ep_at = format!("{at}.endpoints[{ei}].address");
                if ep.address.port() == 0 {
                    self.err(&ep_at, "port must not be 0");
                }
                if ep.address.ip().is_unspecified() {
                    self.err(&ep_at, "must not be an unspecified address");
                }
                if !addrs.insert(ep.address) {
                    self.err(&ep_at, format!("duplicate endpoint {}", ep.address));
                }
                endpoints.push(Endpoint {
                    address: ep.address,
                });
            }
            out.push(Cluster {
                name: cluster.name.clone(),
                lb_policy: cluster.lb_policy,
                endpoints,
            });
        }
        out
    }

    fn listeners(
        &mut self,
        config: &Config,
        cluster_ids: &HashMap<&str, ClusterId>,
    ) -> Vec<Listener> {
        if config.listeners.is_empty() {
            self.err("listeners", "must define at least one listener");
        }
        let mut names = HashSet::new();
        let mut addrs: HashSet<SocketAddr> = HashSet::new();
        let mut out = Vec::with_capacity(config.listeners.len());
        for (li, listener) in config.listeners.iter().enumerate() {
            let at = format!("listeners[{li}]");
            self.name(&format!("{at}.name"), &listener.name);
            if !names.insert(listener.name.as_str()) {
                self.err(
                    format!("{at}.name"),
                    format!("duplicate listener name {:?}", listener.name),
                );
            }
            if listener.address.port() != 0 && !addrs.insert(listener.address) {
                self.err(
                    format!("{at}.address"),
                    format!("duplicate listener address {}", listener.address),
                );
            }
            let mut route_names = HashSet::new();
            let mut routes = Vec::with_capacity(listener.routes.len());
            for (ri, route) in listener.routes.iter().enumerate() {
                let r_at = format!("{at}.routes[{ri}]");
                self.name(&format!("{r_at}.name"), &route.name);
                if !route_names.insert(route.name.as_str()) {
                    self.err(
                        format!("{r_at}.name"),
                        format!("duplicate route name {:?}", route.name),
                    );
                }
                let hostnames = route
                    .hostnames
                    .iter()
                    .enumerate()
                    .filter_map(|(hi, h)| self.hostname(&format!("{r_at}.hostnames[{hi}]"), h))
                    .collect();
                if route.rules.is_empty() {
                    self.err(format!("{r_at}.rules"), "must define at least one rule");
                }
                let rules = route
                    .rules
                    .iter()
                    .enumerate()
                    .map(|(ri2, rule)| {
                        let rule_at = format!("{r_at}.rules[{ri2}]");
                        let matches = if rule.matches.is_empty() {
                            vec![RequestMatch {
                                path: PathMatch::Prefix("/".into()),
                            }]
                        } else {
                            rule.matches
                                .iter()
                                .enumerate()
                                .map(|(mi, m)| RequestMatch {
                                    path: self.path_match(
                                        &format!("{rule_at}.matches[{mi}].path"),
                                        m.path.as_ref(),
                                    ),
                                })
                                .collect()
                        };
                        let backends = self.backends(&rule_at, &rule.backends, cluster_ids);
                        Rule { matches, backends }
                    })
                    .collect();
                routes.push(Route {
                    name: route.name.clone(),
                    hostnames,
                    rules,
                });
            }
            out.push(Listener {
                name: listener.name.clone(),
                address: listener.address,
                routes,
            });
        }
        out
    }

    fn backends(
        &mut self,
        rule_at: &str,
        refs: &[crate::model::BackendRefConfig],
        cluster_ids: &HashMap<&str, ClusterId>,
    ) -> Vec<WeightedBackend> {
        if refs.is_empty() {
            self.err(
                format!("{rule_at}.backends"),
                "must list at least one backend",
            );
        }
        let mut out = Vec::with_capacity(refs.len());
        for (bi, backend) in refs.iter().enumerate() {
            let b_at = format!("{rule_at}.backends[{bi}]");
            if backend.weight > MAX_BACKEND_WEIGHT {
                self.err(
                    format!("{b_at}.weight"),
                    format!("must be at most {MAX_BACKEND_WEIGHT}"),
                );
            }
            match cluster_ids.get(backend.cluster.as_str()) {
                Some(&cluster) => out.push(WeightedBackend {
                    cluster,
                    weight: backend.weight,
                }),
                None => self.err(
                    format!("{b_at}.cluster"),
                    format!("unknown cluster {:?}", backend.cluster),
                ),
            }
        }
        out
    }

    fn path_match(&mut self, at: &str, path: Option<&PathMatchConfig>) -> PathMatch {
        let Some(path) = path else {
            return PathMatch::Prefix("/".into());
        };
        let value = path.value.as_str();
        if !value.starts_with('/') {
            self.err(format!("{at}.value"), "must start with '/'");
        }
        if value.len() > MAX_PATH_LEN {
            self.err(
                format!("{at}.value"),
                format!("must be at most {MAX_PATH_LEN} bytes"),
            );
        }
        if value.contains(['?', '#']) || value.chars().any(char::is_whitespace) {
            self.err(
                format!("{at}.value"),
                "must not contain '?', '#' or whitespace",
            );
        }
        match path.kind {
            PathMatchKind::Exact => PathMatch::Exact(value.to_owned()),
            PathMatchKind::PathPrefix => {
                // The Gateway API ignores a trailing slash on prefixes.
                let trimmed = value.trim_end_matches('/');
                PathMatch::Prefix(if trimmed.is_empty() { "/" } else { trimmed }.to_owned())
            }
        }
    }

    fn hostname(&mut self, at: &str, raw: &str) -> Option<HostMatch> {
        let host = raw.to_ascii_lowercase();
        if host.parse::<IpAddr>().is_ok() {
            self.err(at, "must be a DNS name, not an IP address");
            return None;
        }
        let (wildcard, name) = match host.strip_prefix("*.") {
            Some(rest) => (true, rest),
            None => (false, host.as_str()),
        };
        if !is_dns_subdomain(name) {
            self.err(
                at,
                format!("{raw:?} is not a valid hostname (optionally prefixed with '*.')"),
            );
            return None;
        }
        Some(if wildcard {
            HostMatch::Wildcard(format!(".{name}"))
        } else {
            HostMatch::Exact(name.to_owned())
        })
    }
}

/// RFC 1123 subdomain check on an already-lowercased name.
fn is_dns_subdomain(name: &str) -> bool {
    if name.is_empty() || name.len() > 253 {
        return false;
    }
    name.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load::from_yaml_str;

    fn snapshot(yaml: &str) -> Result<Snapshot, ValidationErrors> {
        from_yaml_str(yaml)
            .expect("test yaml parses")
            .into_snapshot(SnapshotVersion(1))
    }

    const VALID: &str = r#"
listeners:
  - name: http
    address: 127.0.0.1:8080
    routes:
      - name: api
        hostnames: ["API.example.com", "*.example.com"]
        rules:
          - matches: [{ path: { type: PathPrefix, value: /api/ } }]
            backends: [{ cluster: api, weight: 90 }, { cluster: api-canary, weight: 10 }]
          - backends: [{ cluster: api }]
clusters:
  - name: api
    endpoints: [{ address: 10.0.0.1:80 }, { address: 10.0.0.2:80 }]
  - name: api-canary
    endpoints: [{ address: 10.0.0.3:80 }]
"#;

    #[test]
    fn accepts_valid_config_and_normalises() {
        let snap = snapshot(VALID).expect("valid");
        let route = &snap.listeners()[0].routes()[0];
        assert_eq!(
            route.hostnames(),
            &[
                HostMatch::Exact("api.example.com".into()),
                HostMatch::Wildcard(".example.com".into())
            ]
        );
        let rule0 = &route.rules()[0];
        assert_eq!(rule0.matches()[0].path(), &PathMatch::Prefix("/api".into()));
        assert_eq!(rule0.backends()[1].cluster(), ClusterId(1));
        let rule1 = &route.rules()[1];
        assert_eq!(rule1.matches()[0].path(), &PathMatch::Prefix("/".into()));
        assert_eq!(rule1.backends()[0].weight(), 1);
    }

    #[test]
    fn reports_all_errors_with_paths() {
        let yaml = r#"
listeners:
  - name: http
    address: 127.0.0.1:8080
    routes:
      - name: r
        hostnames: ["bad_host!", "10.0.0.1"]
        rules:
          - matches: [{ path: { value: "no-slash" } }]
            backends: [{ cluster: missing }]
  - name: http
    address: 127.0.0.1:8080
    routes: []
clusters:
  - name: c
    endpoints: []
"#;
        let errs = snapshot(yaml).expect_err("invalid").0;
        let paths: Vec<&str> = errs.iter().map(|e| e.path.as_str()).collect();
        for want in [
            "clusters[0].endpoints",
            "listeners[0].routes[0].hostnames[0]",
            "listeners[0].routes[0].hostnames[1]",
            "listeners[0].routes[0].rules[0].matches[0].path.value",
            "listeners[0].routes[0].rules[0].backends[0].cluster",
            "listeners[1].name",
            "listeners[1].address",
        ] {
            assert!(paths.contains(&want), "missing {want} in {paths:?}");
        }
    }

    #[test]
    fn rejects_empty_listeners_and_rules() {
        let errs = snapshot("listeners: []\nclusters: []\n")
            .expect_err("invalid")
            .0;
        assert_eq!(errs[0].path, "listeners");
    }

    #[test]
    fn rejects_overweight_backend() {
        let yaml = VALID.replace("weight: 90", "weight: 1000001");
        let errs = snapshot(&yaml).expect_err("invalid").0;
        assert!(errs.iter().any(|e| e.path.ends_with("backends[0].weight")));
    }

    #[test]
    fn dns_subdomain_rules() {
        assert!(is_dns_subdomain("a.b-c.example"));
        assert!(!is_dns_subdomain("-a.example"));
        assert!(!is_dns_subdomain("a..example"));
        assert!(!is_dns_subdomain(&"a".repeat(64)));
        assert!(!is_dns_subdomain("UPPER.example"));
    }

    #[test]
    fn port_zero_listener_is_allowed_for_ephemeral_binds() {
        let yaml = VALID.replace("127.0.0.1:8080", "127.0.0.1:0");
        assert!(snapshot(&yaml).is_ok());
    }
}
