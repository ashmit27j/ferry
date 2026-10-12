//! Validation of TLS, limits, timeouts, retries, rate limits and upstream
//! resilience settings.

use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use super::{Validator, is_dns_subdomain};
use crate::duration::Duration as ConfigDuration;
use crate::policy::{
    CertificateConfig, CircuitBreakersConfig, ListenerLimitsConfig, ListenerTimeoutsConfig,
    ListenerTlsConfig, OutlierDetectionConfig, RateLimitConfig, RateLimitKeyConfig,
    RetryBudgetConfig, RetryConfig, RouteTimeoutsConfig, TlsVersion, UpstreamTlsConfig,
};
use crate::snapshot::{
    CaSource, CertificateSource, CircuitBreakers, Limits, ListenerTimeouts, ListenerTls,
    OutlierDetection, RateLimit, RateLimitKey, RetryBudget, RetryPolicy, RouteTimeouts, Secret,
    UpstreamTls,
};

/// Longest accepted duration for any timeout or period.
const MAX_DURATION: Duration = Duration::from_secs(24 * 3600);

impl Validator {
    fn duration(&mut self, at: String, d: Option<ConfigDuration>, default: Duration) -> Duration {
        let d = d.map_or(default, ConfigDuration::get);
        if d.is_zero() {
            self.err(at, "must be greater than 0");
        } else if d > MAX_DURATION {
            self.err(at, "must be at most 24h");
        }
        d
    }

    fn range(&mut self, at: String, v: Option<u32>, default: u32, min: u32, max: u32) -> u32 {
        let v = v.unwrap_or(default);
        if !(min..=max).contains(&v) {
            self.err(at, format!("must be between {min} and {max}"));
        }
        v
    }

    pub(super) fn certificate(
        &mut self,
        at: &str,
        c: &CertificateConfig,
    ) -> Option<CertificateSource> {
        let nonempty = |s: &Option<String>| s.as_deref().is_some_and(|v| !v.is_empty());
        let files = nonempty(&c.cert_file) || nonempty(&c.key_file);
        let pem = nonempty(&c.cert_pem) || nonempty(&c.key_pem);
        match (files, pem) {
            (true, false) => match (&c.cert_file, &c.key_file) {
                (Some(cert), Some(key)) if !cert.is_empty() && !key.is_empty() => {
                    Some(CertificateSource::Files {
                        cert: PathBuf::from(cert),
                        key: PathBuf::from(key),
                    })
                }
                _ => {
                    self.err(at, "cert_file and key_file must both be set");
                    None
                }
            },
            (false, true) => match (&c.cert_pem, &c.key_pem) {
                (Some(cert), Some(key)) => {
                    if !cert.contains("-----BEGIN") {
                        self.err(format!("{at}.cert_pem"), "is not PEM");
                    }
                    if !key.contains("-----BEGIN") {
                        self.err(format!("{at}.key_pem"), "is not PEM");
                    }
                    Some(CertificateSource::Pem {
                        cert: cert.clone(),
                        key: Secret::new(key.clone()),
                    })
                }
                _ => {
                    self.err(at, "cert_pem and key_pem must both be set");
                    None
                }
            },
            _ => {
                self.err(
                    at,
                    "set either cert_file and key_file, or cert_pem and key_pem",
                );
                None
            }
        }
    }

    pub(super) fn listener_tls(
        &mut self,
        at: &str,
        tls: &ListenerTlsConfig,
    ) -> Option<ListenerTls> {
        if tls.certificates.is_empty() {
            self.err(
                format!("{at}.certificates"),
                "must list at least one certificate",
            );
        }
        let certificates: Vec<CertificateSource> = tls
            .certificates
            .iter()
            .enumerate()
            .filter_map(|(i, c)| self.certificate(&format!("{at}.certificates[{i}]"), c))
            .collect();
        Some(ListenerTls {
            certificates,
            min_version: tls.min_version.unwrap_or(TlsVersion::Tls12),
        })
    }

    pub(super) fn limits(&mut self, at: &str, l: Option<&ListenerLimitsConfig>) -> Limits {
        let d = Limits::default();
        let Some(l) = l else { return d };
        let kb = self.range(
            format!("{at}.max_request_headers_kb"),
            l.max_request_headers_kb,
            60,
            8,
            1024,
        );
        let count = self.range(
            format!("{at}.max_request_headers"),
            l.max_request_headers,
            100,
            1,
            1024,
        );
        if l.max_request_body_bytes == Some(0) {
            self.err(
                format!("{at}.max_request_body_bytes"),
                "must be greater than 0",
            );
        }
        let streams = self.range(
            format!("{at}.h2_max_concurrent_streams"),
            l.h2_max_concurrent_streams,
            d.h2_max_concurrent_streams,
            1,
            10_000,
        );
        Limits {
            max_request_headers_bytes: kb as usize * 1024,
            max_request_headers: count as usize,
            max_request_body_bytes: l.max_request_body_bytes,
            h2_max_concurrent_streams: streams,
        }
    }

    pub(super) fn listener_timeouts(
        &mut self,
        at: &str,
        t: Option<&ListenerTimeoutsConfig>,
    ) -> ListenerTimeouts {
        let d = ListenerTimeouts::default();
        let Some(t) = t else { return d };
        ListenerTimeouts {
            header_read: self.duration(format!("{at}.header_read"), t.header_read, d.header_read),
            h2_keep_alive: self.duration(
                format!("{at}.h2_keep_alive"),
                t.h2_keep_alive,
                d.h2_keep_alive,
            ),
        }
    }

    pub(super) fn rate_limit(&mut self, at: &str, r: &RateLimitConfig) -> Option<RateLimit> {
        if r.requests == 0 {
            self.err(format!("{at}.requests"), "must be greater than 0");
        }
        let per = self.duration(format!("{at}.per"), Some(r.per), Duration::ZERO);
        let burst = r.burst.unwrap_or(r.requests);
        if burst == 0 {
            self.err(format!("{at}.burst"), "must be greater than 0");
        }
        let key = match &r.key {
            RateLimitKeyConfig::Header(name) => match http::HeaderName::from_bytes(name.as_bytes())
            {
                Ok(n) => RateLimitKey::Header(n),
                Err(_) => {
                    self.err(
                        format!("{at}.key.header"),
                        format!("{name:?} is not a valid header name"),
                    );
                    return None;
                }
            },
            _ => RateLimitKey::ClientIp,
        };
        Some(RateLimit {
            requests: r.requests,
            per,
            burst,
            key,
        })
    }

    pub(super) fn route_timeouts(
        &mut self,
        at: &str,
        t: Option<&RouteTimeoutsConfig>,
    ) -> RouteTimeouts {
        let d = RouteTimeouts::default();
        let Some(t) = t else { return d };
        let request = t
            .request
            .map(|v| self.duration(format!("{at}.request"), Some(v), Duration::ZERO));
        let backend_request = t
            .backend_request
            .map(|v| self.duration(format!("{at}.backend_request"), Some(v), Duration::ZERO));
        if let (Some(r), Some(b)) = (request, backend_request)
            && b > r
        {
            self.err(format!("{at}.backend_request"), "must not exceed request");
        }
        RouteTimeouts {
            request,
            backend_request,
            idle: self.duration(format!("{at}.idle"), t.idle, d.idle),
        }
    }

    pub(super) fn retry(&mut self, at: &str, r: &RetryConfig) -> RetryPolicy {
        let attempts = self.range(format!("{at}.attempts"), r.attempts, 2, 0, 10);
        let codes = if r.codes.is_empty() {
            vec![502, 503, 504]
        } else {
            r.codes.clone()
        };
        if codes.iter().any(|c| !(500..=599).contains(c)) {
            self.err(
                format!("{at}.codes"),
                "must be status codes between 500 and 599",
            );
        }
        let backoff = r
            .backoff
            .map_or(Duration::from_millis(25), ConfigDuration::get);
        if backoff > Duration::from_secs(10) {
            self.err(format!("{at}.backoff"), "must be at most 10s");
        }
        RetryPolicy {
            attempts,
            codes,
            backoff,
        }
    }

    pub(super) fn upstream_tls(
        &mut self,
        at: &str,
        t: &UpstreamTlsConfig,
        cluster_name: &str,
    ) -> Option<UpstreamTls> {
        let sni = t
            .sni
            .clone()
            .unwrap_or_else(|| cluster_name.to_ascii_lowercase());
        if sni.parse::<IpAddr>().is_err() && !is_dns_subdomain(&sni) {
            self.err(
                format!("{at}.sni"),
                format!("{sni:?} is not a DNS name or IP address (set sni explicitly)"),
            );
        }
        let ca = match (&t.ca_file, &t.ca_pem) {
            (Some(_), Some(_)) => {
                self.err(at, "set at most one of ca_file and ca_pem");
                None
            }
            (Some(f), None) => Some(CaSource::File(PathBuf::from(f))),
            (None, Some(p)) => {
                if !p.contains("-----BEGIN") {
                    self.err(format!("{at}.ca_pem"), "is not PEM");
                }
                Some(CaSource::Pem(p.clone()))
            }
            (None, None) => None,
        };
        let client_certificate = t
            .client_certificate
            .as_ref()
            .and_then(|c| self.certificate(&format!("{at}.client_certificate"), c));
        Some(UpstreamTls {
            sni,
            ca,
            client_certificate,
        })
    }

    pub(super) fn circuit_breakers(
        &mut self,
        at: &str,
        c: Option<&CircuitBreakersConfig>,
    ) -> CircuitBreakers {
        let d = CircuitBreakers::default();
        let Some(c) = c else { return d };
        let max = 1_000_000;
        CircuitBreakers {
            max_connections: self.range(
                format!("{at}.max_connections"),
                c.max_connections,
                d.max_connections,
                1,
                max,
            ),
            max_pending_requests: self.range(
                format!("{at}.max_pending_requests"),
                c.max_pending_requests,
                d.max_pending_requests,
                1,
                max,
            ),
            max_requests: self.range(
                format!("{at}.max_requests"),
                c.max_requests,
                d.max_requests,
                1,
                max,
            ),
            max_retries: self.range(
                format!("{at}.max_retries"),
                c.max_retries,
                d.max_retries,
                0,
                max,
            ),
        }
    }

    pub(super) fn outlier_detection(
        &mut self,
        at: &str,
        o: &OutlierDetectionConfig,
    ) -> OutlierDetection {
        let base = self.duration(
            format!("{at}.base_ejection_time"),
            o.base_ejection_time,
            Duration::from_secs(30),
        );
        let max = self.duration(
            format!("{at}.max_ejection_time"),
            o.max_ejection_time,
            Duration::from_secs(300),
        );
        if max < base {
            self.err(
                format!("{at}.max_ejection_time"),
                "must be at least base_ejection_time",
            );
        }
        let percent = o.max_ejection_percent.unwrap_or(10);
        if percent > 100 {
            self.err(format!("{at}.max_ejection_percent"), "must be at most 100");
        }
        OutlierDetection {
            consecutive_5xx: self.range(
                format!("{at}.consecutive_5xx"),
                o.consecutive_5xx,
                5,
                1,
                1000,
            ),
            consecutive_gateway_failure: self.range(
                format!("{at}.consecutive_gateway_failure"),
                o.consecutive_gateway_failure,
                5,
                1,
                1000,
            ),
            base_ejection_time: base,
            max_ejection_time: max,
            max_ejection_percent: percent,
        }
    }

    pub(super) fn retry_budget(&mut self, at: &str, b: Option<&RetryBudgetConfig>) -> RetryBudget {
        let d = RetryBudget::default();
        let Some(b) = b else { return d };
        let percent = b.budget_percent.unwrap_or(d.budget_percent);
        if percent > 100 {
            self.err(format!("{at}.budget_percent"), "must be at most 100");
        }
        let ttl = b.ttl.map_or(d.ttl, ConfigDuration::get);
        if !(Duration::from_secs(1)..=Duration::from_secs(60)).contains(&ttl) {
            self.err(format!("{at}.ttl"), "must be between 1s and 60s");
        }
        RetryBudget {
            budget_percent: percent,
            min_retries_per_second: self.range(
                format!("{at}.min_retries_per_second"),
                b.min_retries_per_second,
                d.min_retries_per_second,
                0,
                1_000_000,
            ),
            ttl,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::super::tests::{assert_errors_at, snapshot};
    use crate::snapshot::{CaSource, CertificateSource, RateLimitKey};

    const PEM_CERT: &str = "-----BEGIN CERTIFICATE-----\\nMIIB\\n-----END CERTIFICATE-----";
    const PEM_KEY: &str = "-----BEGIN PRIVATE KEY-----\\nMIIE\\n-----END PRIVATE KEY-----";

    fn yaml(
        listener_extra: &str,
        route_extra: &str,
        rule_extra: &str,
        cluster_extra: &str,
    ) -> String {
        format!(
            r"
listeners:
  - name: l
    address: 127.0.0.1:0
{listener_extra}
    routes:
      - name: r
{route_extra}
        rules:
          - backends: [{{ cluster: c }}]
{rule_extra}
clusters:
  - name: c
    endpoints: [{{ address: 10.0.0.1:80 }}]
{cluster_extra}
"
        )
    }

    #[test]
    fn defaults_are_filled_in() {
        let snap = snapshot(&yaml("", "", "", "")).unwrap();
        let l = &snap.listeners()[0];
        assert!(l.tls().is_none() && !l.proxy_protocol());
        assert_eq!(l.limits().max_request_headers(), 100);
        assert_eq!(l.timeouts().header_read(), Duration::from_secs(30));
        let route = &l.routes()[0];
        assert!(route.rate_limit().is_none());
        let rule = &route.rules()[0];
        assert_eq!(rule.timeouts().idle(), Duration::from_secs(300));
        assert!(rule.retry().is_none());
        let c = &snap.clusters()[0];
        assert_eq!(c.circuit_breakers().max_requests(), 1024);
        assert!(c.outlier_detection().is_none() && c.tls().is_none());
        assert_eq!(c.retry_budget().min_retries_per_second(), 10);
    }

    #[test]
    fn everything_set_is_accepted() {
        let snap = snapshot(&yaml(
            &format!(
                r#"    proxy_protocol: true
    tls:
      min_version: "1.3"
      certificates:
        - {{ cert_file: /etc/ferry/a.crt, key_file: /etc/ferry/a.key }}
        - {{ cert_pem: "{PEM_CERT}", key_pem: "{PEM_KEY}" }}
    limits: {{ max_request_headers_kb: 16, max_request_headers: 50, max_request_body_bytes: 1048576, h2_max_concurrent_streams: 50 }}
    timeouts: {{ header_read: 5s, h2_keep_alive: 10s }}"#
            ),
            "        rate_limit: { requests: 100, per: 1s, burst: 200, key: { header: x-api-key } }",
            "            timeouts: { request: 10s, backend_request: 2s, idle: 1m }\n            retry: { attempts: 3, codes: [500, 503], backoff: 50ms }",
            r"    tls: { sni: api.internal, ca_file: /etc/ca.pem, client_certificate: { cert_file: /c.crt, key_file: /c.key } }
    circuit_breakers: { max_connections: 10, max_pending_requests: 5, max_requests: 20, max_retries: 1 }
    outlier_detection: { consecutive_5xx: 3, base_ejection_time: 10s }
    retry_budget: { budget_percent: 10, min_retries_per_second: 1, ttl: 5s }",
        ))
        .unwrap();
        let l = &snap.listeners()[0];
        let tls = l.tls().unwrap();
        assert_eq!(tls.certificates().len(), 2);
        assert!(matches!(
            tls.certificates()[0],
            CertificateSource::Files { .. }
        ));
        assert_eq!(l.limits().max_request_headers_bytes(), 16 * 1024);
        let rl = l.routes()[0].rate_limit().unwrap();
        assert_eq!((rl.requests(), rl.burst()), (100, 200));
        assert!(matches!(rl.key(), RateLimitKey::Header(h) if h == "x-api-key"));
        let rule = &l.routes()[0].rules()[0];
        assert_eq!(
            rule.timeouts().backend_request(),
            Some(Duration::from_secs(2))
        );
        assert_eq!(rule.retry().unwrap().codes(), [500, 503]);
        let c = &snap.clusters()[0];
        let up = c.tls().unwrap();
        assert_eq!(up.sni(), "api.internal");
        assert!(matches!(up.ca(), Some(CaSource::File(_))));
        assert_eq!(c.circuit_breakers().max_retries(), 1);
        let od = c.outlier_detection().unwrap();
        assert_eq!(
            (od.consecutive_5xx(), od.consecutive_gateway_failure()),
            (3, 5)
        );
        assert_eq!(c.retry_budget().ttl(), Duration::from_secs(5));
        // Private keys never leak through Debug.
        assert!(!format!("{snap:?}").contains("PRIVATE KEY"));
    }

    #[test]
    fn bad_values_are_reported() {
        let y = yaml(
            r"    tls: { certificates: [{ cert_file: a.crt }, { cert_pem: nope, key_pem: nope }, {}] }
    limits: { max_request_headers_kb: 4, max_request_headers: 0, max_request_body_bytes: 0, h2_max_concurrent_streams: 0 }
    timeouts: { header_read: 0s }",
            "        rate_limit: { requests: 0, per: 0s, key: { header: 'bad name' } }",
            "            timeouts: { request: 1s, backend_request: 2s }\n            retry: { attempts: 11, codes: [404] }",
            r"    tls: { ca_file: a, ca_pem: b }
    circuit_breakers: { max_requests: 0 }
    outlier_detection: { consecutive_5xx: 0, base_ejection_time: 10m, max_ejection_time: 1m, max_ejection_percent: 101 }
    retry_budget: { budget_percent: 101, ttl: 2m }",
        );
        let l = "listeners[0]";
        let r = "listeners[0].routes[0]";
        let rule = "listeners[0].routes[0].rules[0]";
        assert_errors_at(
            &y,
            &[
                &format!("{l}.tls.certificates[0]"),
                &format!("{l}.tls.certificates[1].cert_pem"),
                &format!("{l}.tls.certificates[1].key_pem"),
                &format!("{l}.tls.certificates[2]"),
                &format!("{l}.limits.max_request_headers_kb"),
                &format!("{l}.limits.max_request_headers"),
                &format!("{l}.limits.max_request_body_bytes"),
                &format!("{l}.limits.h2_max_concurrent_streams"),
                &format!("{l}.timeouts.header_read"),
                &format!("{r}.rate_limit.requests"),
                &format!("{r}.rate_limit.per"),
                &format!("{r}.rate_limit.key.header"),
                &format!("{rule}.timeouts.backend_request"),
                &format!("{rule}.retry.attempts"),
                &format!("{rule}.retry.codes"),
                "clusters[0].tls",
                "clusters[0].circuit_breakers.max_requests",
                "clusters[0].outlier_detection.consecutive_5xx",
                "clusters[0].outlier_detection.max_ejection_time",
                "clusters[0].outlier_detection.max_ejection_percent",
                "clusters[0].retry_budget.budget_percent",
                "clusters[0].retry_budget.ttl",
            ],
        );
    }

    #[test]
    fn upstream_sni_defaults_to_cluster_name_and_must_be_a_name() {
        let ok = snapshot(&yaml("", "", "", "    tls: {}")).unwrap();
        assert_eq!(ok.clusters()[0].tls().unwrap().sni(), "c");
        let bad = yaml("", "", "", "    tls: {}")
            .replace("name: c\n", "name: my_svc\n")
            .replace("cluster: c", "cluster: my_svc");
        assert_errors_at(&bad, &["clusters[0].tls.sni"]);
    }
}
