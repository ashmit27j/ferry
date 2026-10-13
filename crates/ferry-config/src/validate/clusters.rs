//! Validation of clusters: endpoints, balancing, stickiness and health checks.

use std::collections::HashSet;
use std::time::Duration;

use http::HeaderValue;

use super::{MAX_ENDPOINT_WEIGHT, Validator};
use crate::model::{ClusterConfig, Config, HashPolicyConfig, HealthCheckConfig, LbPolicy};
use crate::snapshot::{Cluster, Endpoint, HashPolicy, HealthCheck, HealthCheckKind};

/// Default upstream connect timeout, as in Envoy.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Default panic threshold percentage, as in Envoy.
pub const DEFAULT_PANIC_THRESHOLD: u8 = 50;

const DEFAULT_HC_INTERVAL: Duration = Duration::from_secs(5);
const DEFAULT_HC_TIMEOUT: Duration = Duration::from_secs(1);
const DEFAULT_UNHEALTHY_THRESHOLD: u32 = 3;
const DEFAULT_HEALTHY_THRESHOLD: u32 = 2;
const MIN_HC_INTERVAL: Duration = Duration::from_millis(100);
const MAX_THRESHOLD: u32 = 100;

impl Validator {
    pub(super) fn clusters(&mut self, config: &Config) -> Vec<Cluster> {
        let mut seen = HashSet::new();
        config
            .clusters
            .iter()
            .enumerate()
            .map(|(ci, cluster)| {
                let at = format!("clusters[{ci}]");
                if !seen.insert(cluster.name.as_str()) {
                    self.err(
                        format!("{at}.name"),
                        format!("duplicate cluster name {:?}", cluster.name),
                    );
                }
                self.cluster(&at, cluster)
            })
            .collect()
    }

    fn cluster(&mut self, at: &str, cluster: &ClusterConfig) -> Cluster {
        self.name(&format!("{at}.name"), &cluster.name);
        let mut addrs = HashSet::new();
        let mut endpoints = Vec::with_capacity(cluster.endpoints.len());
        for (ei, ep) in cluster.endpoints.iter().enumerate() {
            let ep_at = format!("{at}.endpoints[{ei}]");
            if ep.address.port() == 0 {
                self.err(format!("{ep_at}.address"), "port must not be 0");
            }
            if ep.address.ip().is_unspecified() {
                self.err(
                    format!("{ep_at}.address"),
                    "must not be an unspecified address",
                );
            }
            if !addrs.insert(ep.address) {
                self.err(
                    format!("{ep_at}.address"),
                    format!("duplicate endpoint {}", ep.address),
                );
            }
            if !(1..=MAX_ENDPOINT_WEIGHT).contains(&ep.weight) {
                self.err(
                    format!("{ep_at}.weight"),
                    format!("must be between 1 and {MAX_ENDPOINT_WEIGHT}"),
                );
            }
            endpoints.push(Endpoint {
                address: ep.address,
                weight: ep.weight,
            });
        }

        let hash_policy = match (cluster.lb_policy, &cluster.hash_policy) {
            (LbPolicy::Maglev, None) => {
                self.err(
                    format!("{at}.hash_policy"),
                    "is required when lb_policy is maglev",
                );
                None
            }
            (LbPolicy::Maglev, Some(p)) => self.hash_policy(&format!("{at}.hash_policy"), p),
            (_, Some(_)) => {
                self.err(
                    format!("{at}.hash_policy"),
                    "is only used with lb_policy maglev",
                );
                None
            }
            (_, None) => None,
        };

        let health_check = cluster
            .health_check
            .as_ref()
            .and_then(|hc| self.health_check(&format!("{at}.health_check"), hc, &cluster.name));

        let panic_threshold = cluster.panic_threshold.unwrap_or(DEFAULT_PANIC_THRESHOLD);
        if panic_threshold > 100 {
            self.err(format!("{at}.panic_threshold"), "must be at most 100");
        }
        let connect_timeout = cluster
            .connect_timeout
            .map_or(DEFAULT_CONNECT_TIMEOUT, |d| d.get());
        if connect_timeout.is_zero() {
            self.err(format!("{at}.connect_timeout"), "must be greater than 0");
        }

        let tls = cluster
            .tls
            .as_ref()
            .and_then(|t| self.upstream_tls(&format!("{at}.tls"), t, &cluster.name));
        let circuit_breakers = self.circuit_breakers(
            &format!("{at}.circuit_breakers"),
            cluster.circuit_breakers.as_ref(),
        );
        let outlier_detection = cluster
            .outlier_detection
            .as_ref()
            .map(|o| self.outlier_detection(&format!("{at}.outlier_detection"), o));
        let retry_budget =
            self.retry_budget(&format!("{at}.retry_budget"), cluster.retry_budget.as_ref());

        Cluster {
            name: cluster.name.clone(),
            protocol: cluster.protocol,
            lb_policy: cluster.lb_policy,
            hash_policy,
            health_check,
            panic_threshold,
            connect_timeout,
            tls,
            circuit_breakers,
            outlier_detection,
            retry_budget,
            endpoints,
        }
    }

    fn hash_policy(&mut self, at: &str, p: &HashPolicyConfig) -> Option<HashPolicy> {
        match p {
            HashPolicyConfig::Header(name) => match http::HeaderName::from_bytes(name.as_bytes()) {
                Ok(n) => Some(HashPolicy::Header(n)),
                Err(_) => {
                    self.err(
                        format!("{at}.header"),
                        format!("{name:?} is not a valid header name"),
                    );
                    None
                }
            },
            HashPolicyConfig::Cookie(c) => {
                if c.name.is_empty() || !c.name.bytes().all(is_cookie_token_byte) {
                    self.err(
                        format!("{at}.cookie.name"),
                        "must be a non-empty RFC 6265 token",
                    );
                }
                let path = c.path.clone().unwrap_or_else(|| "/".to_owned());
                if !path.starts_with('/') || path.bytes().any(|b| b == b';' || b.is_ascii_control())
                {
                    self.err(
                        format!("{at}.cookie.path"),
                        "must start with '/' and contain no ';' or control characters",
                    );
                }
                Some(HashPolicy::Cookie {
                    name: c.name.clone(),
                    ttl: c.ttl.map(|d| d.get()),
                    path,
                })
            }
            HashPolicyConfig::SourceIp => Some(HashPolicy::SourceIp),
        }
    }

    fn health_check(
        &mut self,
        at: &str,
        hc: &HealthCheckConfig,
        cluster_name: &str,
    ) -> Option<HealthCheck> {
        let interval = hc.interval.map_or(DEFAULT_HC_INTERVAL, |d| d.get());
        if interval < MIN_HC_INTERVAL {
            self.err(format!("{at}.interval"), "must be at least 100ms");
        }
        let timeout = hc.timeout.map_or(DEFAULT_HC_TIMEOUT, |d| d.get());
        if timeout.is_zero() {
            self.err(format!("{at}.timeout"), "must be greater than 0");
        }
        let unhealthy_threshold = hc
            .unhealthy_threshold
            .unwrap_or(DEFAULT_UNHEALTHY_THRESHOLD);
        let healthy_threshold = hc.healthy_threshold.unwrap_or(DEFAULT_HEALTHY_THRESHOLD);
        for (field, value) in [
            ("unhealthy_threshold", unhealthy_threshold),
            ("healthy_threshold", healthy_threshold),
        ] {
            if !(1..=MAX_THRESHOLD).contains(&value) {
                self.err(
                    format!("{at}.{field}"),
                    format!("must be between 1 and {MAX_THRESHOLD}"),
                );
            }
        }
        let kind = match (&hc.http, &hc.grpc) {
            (Some(http), None) => {
                if !http.path.starts_with('/') || http.path.chars().any(char::is_whitespace) {
                    self.err(
                        format!("{at}.http.path"),
                        "must start with '/' and contain no whitespace",
                    );
                }
                let host = http.host.clone().unwrap_or_else(|| cluster_name.to_owned());
                if HeaderValue::from_str(&host).is_err() {
                    self.err(format!("{at}.http.host"), "is not a valid header value");
                }
                let expected_statuses = if http.expected_statuses.is_empty() {
                    vec![200]
                } else {
                    http.expected_statuses.clone()
                };
                if expected_statuses.iter().any(|s| !(100..=599).contains(s)) {
                    self.err(
                        format!("{at}.http.expected_statuses"),
                        "must be HTTP status codes between 100 and 599",
                    );
                }
                HealthCheckKind::Http {
                    path: http.path.clone(),
                    host,
                    expected_statuses,
                }
            }
            (None, Some(grpc)) => {
                let authority = grpc
                    .authority
                    .clone()
                    .unwrap_or_else(|| cluster_name.to_owned());
                if authority.parse::<http::uri::Authority>().is_err() {
                    self.err(format!("{at}.grpc.authority"), "is not a valid authority");
                }
                HealthCheckKind::Grpc {
                    service: grpc.service.clone(),
                    authority,
                }
            }
            _ => {
                self.err(at, "must set exactly one of http and grpc");
                return None;
            }
        };
        Some(HealthCheck {
            interval,
            timeout,
            unhealthy_threshold,
            healthy_threshold,
            kind,
        })
    }
}

/// RFC 6265 `token` characters (RFC 2616 token: no separators or controls).
fn is_cookie_token_byte(b: u8) -> bool {
    b.is_ascii_graphic()
        && !matches!(
            b,
            b'(' | b')'
                | b'<'
                | b'>'
                | b'@'
                | b','
                | b';'
                | b':'
                | b'\\'
                | b'"'
                | b'/'
                | b'['
                | b']'
                | b'?'
                | b'='
                | b'{'
                | b'}'
        )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::super::tests::{assert_errors_at, snapshot};
    use crate::model::{LbPolicy, UpstreamProtocol};
    use crate::snapshot::{HashPolicy, HealthCheckKind};

    fn cluster(body: &str) -> String {
        format!(
            r"
listeners:
  - name: l
    address: 127.0.0.1:0
    routes:
      - name: r
        rules: [{{ backends: [{{ cluster: c }}] }}]
clusters:
  - name: c
{body}
"
        )
    }

    #[test]
    fn defaults_are_filled_in() {
        let snap = snapshot(&cluster("    endpoints: [{ address: 10.0.0.1:80 }]")).unwrap();
        let c = &snap.clusters()[0];
        assert_eq!(c.protocol(), UpstreamProtocol::Http1);
        assert_eq!(c.lb_policy(), LbPolicy::RoundRobin);
        assert_eq!(c.panic_threshold(), 50);
        assert_eq!(c.connect_timeout(), Duration::from_secs(5));
        assert_eq!(c.endpoints()[0].weight(), 1);
        assert!(c.health_check().is_none() && c.hash_policy().is_none());
    }

    #[test]
    fn full_cluster_is_accepted() {
        let snap = snapshot(&cluster(
            r"    protocol: http2
    lb_policy: maglev
    hash_policy: { cookie: { name: sid, ttl: 1h } }
    panic_threshold: 0
    connect_timeout: 250ms
    health_check:
      interval: 2s
      timeout: 500ms
      unhealthy_threshold: 2
      healthy_threshold: 1
      grpc: { service: echo }
    endpoints: [{ address: 10.0.0.1:80, weight: 3 }, { address: 10.0.0.2:80 }]",
        ))
        .unwrap();
        let c = &snap.clusters()[0];
        assert_eq!(c.protocol(), UpstreamProtocol::Http2);
        assert_eq!(
            c.hash_policy(),
            Some(&HashPolicy::Cookie {
                name: "sid".into(),
                ttl: Some(Duration::from_secs(3600)),
                path: "/".into()
            })
        );
        let hc = c.health_check().unwrap();
        assert_eq!(hc.interval(), Duration::from_secs(2));
        assert_eq!(
            hc.kind(),
            &HealthCheckKind::Grpc {
                service: "echo".into(),
                authority: "c".into()
            }
        );
        assert_eq!(c.endpoints()[0].weight(), 3);
    }

    #[test]
    fn hash_policy_forms_parse() {
        for (yaml, want) in [
            (
                "{ header: X-User }",
                HashPolicy::Header(http::HeaderName::from_static("x-user")),
            ),
            ("source_ip", HashPolicy::SourceIp),
        ] {
            let snap = snapshot(&cluster(&format!(
                "    lb_policy: maglev\n    hash_policy: {yaml}\n    endpoints: [{{ address: 10.0.0.1:80 }}]"
            )))
            .unwrap();
            assert_eq!(snap.clusters()[0].hash_policy(), Some(&want));
        }
    }

    #[test]
    fn bad_clusters_are_reported() {
        let yaml = cluster(
            r"    lb_policy: maglev
    panic_threshold: 101
    connect_timeout: 0s
    health_check:
      interval: 10ms
      unhealthy_threshold: 0
      http: { path: healthz, expected_statuses: [99] }
      grpc: {}
    endpoints: [{ address: 10.0.0.1:80, weight: 0 }, { address: 10.0.0.1:80, weight: 129 }]",
        );
        assert_errors_at(
            &yaml,
            &[
                "clusters[0].hash_policy",
                "clusters[0].panic_threshold",
                "clusters[0].connect_timeout",
                "clusters[0].health_check",
                "clusters[0].health_check.interval",
                "clusters[0].health_check.unhealthy_threshold",
                "clusters[0].endpoints[0].weight",
                "clusters[0].endpoints[1].address",
                "clusters[0].endpoints[1].weight",
            ],
        );
    }

    #[test]
    fn hash_policy_without_maglev_and_bad_http_check() {
        let yaml = cluster(
            r"    hash_policy: { cookie: { name: 'bad name', path: nope } }
    health_check: { http: { path: healthz, expected_statuses: [600] } }
    endpoints: [{ address: 10.0.0.1:80 }]",
        );
        assert_errors_at(
            &yaml,
            &[
                "clusters[0].hash_policy",
                "clusters[0].health_check.http.path",
                "clusters[0].health_check.http.expected_statuses",
            ],
        );
    }

    #[test]
    fn bad_cookie_is_reported_under_maglev() {
        let yaml = cluster(
            r"    lb_policy: maglev
    hash_policy: { cookie: { name: 'bad name', path: nope } }
    endpoints: [{ address: 10.0.0.1:80 }]",
        );
        assert_errors_at(
            &yaml,
            &[
                "clusters[0].hash_policy.cookie.name",
                "clusters[0].hash_policy.cookie.path",
            ],
        );
    }
}
