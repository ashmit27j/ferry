//! Validation of route rules: matches, filters and backend references.

use std::collections::{HashMap, HashSet};

use http::{HeaderName, HeaderValue, Method};

use super::{MAX_BACKEND_WEIGHT, MAX_CONDITIONS, MAX_PATH_LEN, Validator};
use crate::model::{
    BackendRefConfig, FilterConfig, HeaderModifierConfig, MatchConfig, PathMatchConfig,
    PathMatchKind, PathRewriteConfig, RedirectConfig, RuleConfig, StringMatchKind,
    UrlRewriteConfig,
};
use crate::snapshot::{
    ClusterId, Filter, HeaderMatch, HeaderModifier, PathMatch, PathRewrite, Pattern,
    QueryParamMatch, Redirect, RequestMatch, Rule, StringMatch, UrlRewrite, WeightedBackend,
};

/// Methods a match may name: the Gateway API's `HTTPMethod` enum.
const METHODS: [&str; 9] = [
    "GET", "HEAD", "POST", "PUT", "DELETE", "CONNECT", "OPTIONS", "TRACE", "PATCH",
];

/// Request headers a header modifier may not touch, because changing them
/// breaks framing or enables request smuggling. `Host` is changed with
/// `URLRewrite.hostname` instead.
const PROTECTED_HEADERS: [&str; 9] = [
    "host",
    "connection",
    "content-length",
    "transfer-encoding",
    "keep-alive",
    "proxy-connection",
    "te",
    "trailer",
    "upgrade",
];

/// Longest accepted header or query value in a match or modifier.
const MAX_VALUE_LEN: usize = 4096;

impl Validator {
    pub(super) fn rule(
        &mut self,
        at: &str,
        rule: &RuleConfig,
        cluster_ids: &HashMap<&str, ClusterId>,
    ) -> Rule {
        let matches = if rule.matches.is_empty() {
            vec![RequestMatch::path_only(PathMatch::Prefix("/".into()))]
        } else {
            rule.matches
                .iter()
                .enumerate()
                .map(|(i, m)| self.request_match(&format!("{at}.matches[{i}]"), m))
                .collect()
        };
        let all_prefix = matches
            .iter()
            .all(|m| matches!(m.path(), PathMatch::Prefix(_)));
        let filters = self.filters(at, &rule.filters, all_prefix);
        let redirect = filters
            .iter()
            .any(|f| matches!(f, Filter::RequestRedirect(_)));
        if redirect && !rule.backends.is_empty() {
            self.err(
                format!("{at}.backends"),
                "must be empty when the rule has a RequestRedirect filter",
            );
        }
        if redirect && rule.invalid_backend_weight > 0 {
            self.err(
                format!("{at}.invalid_backend_weight"),
                "must be 0 when the rule has a RequestRedirect filter",
            );
        }
        if !redirect && rule.backends.is_empty() && rule.invalid_backend_weight == 0 {
            self.err(format!("{at}.backends"), "must list at least one backend");
        }
        if rule.invalid_backend_weight > MAX_BACKEND_WEIGHT {
            self.err(
                format!("{at}.invalid_backend_weight"),
                format!("must be at most {MAX_BACKEND_WEIGHT}"),
            );
        }
        let backends = self.backends(at, &rule.backends, cluster_ids);
        let timeouts = self.route_timeouts(&format!("{at}.timeouts"), rule.timeouts.as_ref());
        let retry = rule
            .retry
            .as_ref()
            .map(|r| self.retry(&format!("{at}.retry"), r));
        Rule {
            matches,
            filters,
            backends,
            invalid_backend_weight: rule.invalid_backend_weight,
            timeouts,
            retry,
        }
    }

    fn request_match(&mut self, at: &str, m: &MatchConfig) -> RequestMatch {
        let path = self.path_match(&format!("{at}.path"), m.path.as_ref());
        let method = m.method.as_deref().and_then(|raw| {
            if METHODS.contains(&raw) {
                Method::from_bytes(raw.as_bytes()).ok()
            } else {
                self.err(
                    format!("{at}.method"),
                    format!("{raw:?} is not one of {}", METHODS.join(", ")),
                );
                None
            }
        });
        if m.headers.len() > MAX_CONDITIONS {
            self.err(
                format!("{at}.headers"),
                format!("at most {MAX_CONDITIONS} conditions"),
            );
        }
        let mut seen = HashSet::new();
        let headers = m
            .headers
            .iter()
            .enumerate()
            .filter_map(|(i, h)| {
                let h_at = format!("{at}.headers[{i}]");
                let name = self.header_name(&format!("{h_at}.name"), &h.name)?;
                if !seen.insert(name.clone()) {
                    self.err(format!("{h_at}.name"), format!("duplicate header {name}"));
                }
                let value = self.string_match(&format!("{h_at}.value"), h.kind, &h.value)?;
                Some(HeaderMatch { name, value })
            })
            .collect();
        if m.query_params.len() > MAX_CONDITIONS {
            self.err(
                format!("{at}.query_params"),
                format!("at most {MAX_CONDITIONS} conditions"),
            );
        }
        let mut seen = HashSet::new();
        let query_params = m
            .query_params
            .iter()
            .enumerate()
            .filter_map(|(i, q)| {
                let q_at = format!("{at}.query_params[{i}]");
                if q.name.is_empty() || q.name.len() > 256 || q.name.contains(['&', '=', '#']) {
                    self.err(
                        format!("{q_at}.name"),
                        "must be 1 to 256 bytes without '&', '=' or '#'",
                    );
                }
                if !seen.insert(q.name.as_str()) {
                    self.err(
                        format!("{q_at}.name"),
                        format!("duplicate parameter {:?}", q.name),
                    );
                }
                let value = self.string_match(&format!("{q_at}.value"), q.kind, &q.value)?;
                Some(QueryParamMatch {
                    name: q.name.clone(),
                    value,
                })
            })
            .collect();
        RequestMatch {
            path,
            method,
            headers,
            query_params,
        }
    }

    fn path_match(&mut self, at: &str, path: Option<&PathMatchConfig>) -> PathMatch {
        let Some(path) = path else {
            return PathMatch::Prefix("/".into());
        };
        let value = path.value.as_str();
        if value.len() > MAX_PATH_LEN {
            self.err(
                format!("{at}.value"),
                format!("must be at most {MAX_PATH_LEN} bytes"),
            );
        }
        match path.kind {
            PathMatchKind::Exact => {
                self.literal_path(&format!("{at}.value"), value);
                PathMatch::Exact(value.to_owned())
            }
            PathMatchKind::PathPrefix => {
                self.literal_path(&format!("{at}.value"), value);
                // The Gateway API ignores a trailing slash on prefixes.
                let trimmed = value.trim_end_matches('/');
                PathMatch::Prefix(if trimmed.is_empty() { "/" } else { trimmed }.to_owned())
            }
            PathMatchKind::RegularExpression => match self.pattern(&format!("{at}.value"), value) {
                Some(p) => PathMatch::Regex(p),
                None => PathMatch::Exact(value.to_owned()),
            },
        }
    }

    fn literal_path(&mut self, at: &str, value: &str) {
        if !value.starts_with('/') {
            self.err(at, "must start with '/'");
        }
        if value.contains(['?', '#']) || value.chars().any(char::is_whitespace) {
            self.err(at, "must not contain '?', '#' or whitespace");
        }
    }

    fn pattern(&mut self, at: &str, source: &str) -> Option<Pattern> {
        if source.len() > MAX_PATH_LEN {
            self.err(at, format!("must be at most {MAX_PATH_LEN} bytes"));
            return None;
        }
        match Pattern::new(source) {
            Ok(p) => Some(p),
            Err(e) => {
                self.err(at, format!("invalid regular expression: {e}"));
                None
            }
        }
    }

    fn string_match(
        &mut self,
        at: &str,
        kind: StringMatchKind,
        value: &str,
    ) -> Option<StringMatch> {
        if value.len() > MAX_VALUE_LEN {
            self.err(at, format!("must be at most {MAX_VALUE_LEN} bytes"));
            return None;
        }
        match kind {
            StringMatchKind::Exact => Some(StringMatch::Exact(value.to_owned())),
            StringMatchKind::RegularExpression => self.pattern(at, value).map(StringMatch::Regex),
        }
    }

    fn header_name(&mut self, at: &str, raw: &str) -> Option<HeaderName> {
        match HeaderName::from_bytes(raw.as_bytes()) {
            Ok(n) => Some(n),
            Err(_) => {
                self.err(at, format!("{raw:?} is not a valid header name"));
                None
            }
        }
    }

    fn header_value(&mut self, at: &str, raw: &str) -> Option<HeaderValue> {
        if raw.len() > MAX_VALUE_LEN {
            self.err(at, format!("must be at most {MAX_VALUE_LEN} bytes"));
            return None;
        }
        match HeaderValue::from_str(raw) {
            Ok(v) => Some(v),
            Err(_) => {
                self.err(at, "is not a valid header value");
                None
            }
        }
    }

    fn filters(
        &mut self,
        rule_at: &str,
        filters: &[FilterConfig],
        all_prefix: bool,
    ) -> Vec<Filter> {
        if filters.len() > MAX_CONDITIONS {
            self.err(
                format!("{rule_at}.filters"),
                format!("at most {MAX_CONDITIONS} filters"),
            );
        }
        let mut kinds = HashSet::new();
        let mut out = Vec::with_capacity(filters.len());
        for (i, f) in filters.iter().enumerate() {
            let at = format!("{rule_at}.filters[{i}]");
            let kind = match f {
                FilterConfig::RequestHeaderModifier(_) => "RequestHeaderModifier",
                FilterConfig::ResponseHeaderModifier(_) => "ResponseHeaderModifier",
                FilterConfig::RequestRedirect(_) => "RequestRedirect",
                FilterConfig::UrlRewrite(_) => "URLRewrite",
            };
            if !kinds.insert(kind) {
                self.err(&at, format!("{kind} may appear at most once per rule"));
            }
            let filter = match f {
                FilterConfig::RequestHeaderModifier(m) => {
                    Filter::RequestHeaderModifier(self.header_modifier(&at, m, true))
                }
                FilterConfig::ResponseHeaderModifier(m) => {
                    Filter::ResponseHeaderModifier(self.header_modifier(&at, m, false))
                }
                FilterConfig::RequestRedirect(r) => {
                    Filter::RequestRedirect(self.redirect(&at, r, all_prefix))
                }
                FilterConfig::UrlRewrite(r) => {
                    Filter::UrlRewrite(self.url_rewrite(&at, r, all_prefix))
                }
            };
            out.push(filter);
        }
        if kinds.contains("RequestRedirect") && kinds.contains("URLRewrite") {
            self.err(
                format!("{rule_at}.filters"),
                "RequestRedirect and URLRewrite cannot be combined",
            );
        }
        out
    }

    fn header_modifier(
        &mut self,
        at: &str,
        m: &HeaderModifierConfig,
        request: bool,
    ) -> HeaderModifier {
        let mut seen = HashSet::new();
        let mut check = |v: &mut Self, field_at: String, name: &HeaderName| {
            if !seen.insert(name.clone()) {
                v.err(
                    &field_at,
                    format!("header {name} is modified more than once"),
                );
            }
            if request && PROTECTED_HEADERS.contains(&name.as_str()) {
                v.err(&field_at, format!("header {name} cannot be modified"));
            }
        };
        let mut pairs = |v: &mut Self, field: &str, list: &[crate::model::HeaderValueConfig]| {
            list.iter()
                .enumerate()
                .filter_map(|(i, h)| {
                    let h_at = format!("{at}.{field}[{i}]");
                    let name = v.header_name(&format!("{h_at}.name"), &h.name)?;
                    check(v, format!("{h_at}.name"), &name);
                    let value = v.header_value(&format!("{h_at}.value"), &h.value)?;
                    Some((name, value))
                })
                .collect::<Vec<_>>()
        };
        let set = pairs(self, "set", &m.set);
        let add = pairs(self, "add", &m.add);
        let remove = m
            .remove
            .iter()
            .enumerate()
            .filter_map(|(i, raw)| {
                let r_at = format!("{at}.remove[{i}]");
                let name = self.header_name(&r_at, raw)?;
                check(self, r_at, &name);
                Some(name)
            })
            .collect();
        HeaderModifier { set, add, remove }
    }

    fn redirect(&mut self, at: &str, r: &RedirectConfig, all_prefix: bool) -> Redirect {
        if let Some(scheme) = &r.scheme
            && scheme != "http"
            && scheme != "https"
        {
            self.err(format!("{at}.scheme"), "must be \"http\" or \"https\"");
        }
        let hostname = r
            .hostname
            .as_deref()
            .and_then(|h| self.plain_hostname(&format!("{at}.hostname"), h));
        let path = r
            .path
            .as_ref()
            .and_then(|p| self.path_rewrite(&format!("{at}.path"), p, all_prefix));
        if r.port == Some(0) {
            self.err(format!("{at}.port"), "must be between 1 and 65535");
        }
        let status = r.status_code.unwrap_or(302);
        if ![301, 302, 303, 307, 308].contains(&status) {
            self.err(
                format!("{at}.status_code"),
                "must be one of 301, 302, 303, 307 or 308",
            );
        }
        Redirect {
            scheme: r.scheme.clone(),
            hostname,
            path,
            port: r.port,
            status,
        }
    }

    fn url_rewrite(&mut self, at: &str, r: &UrlRewriteConfig, all_prefix: bool) -> UrlRewrite {
        let hostname = r.hostname.as_deref().and_then(|h| {
            let host = self.plain_hostname(&format!("{at}.hostname"), h)?;
            self.header_value(&format!("{at}.hostname"), &host)
        });
        let path = r
            .path
            .as_ref()
            .and_then(|p| self.path_rewrite(&format!("{at}.path"), p, all_prefix));
        UrlRewrite { hostname, path }
    }

    fn path_rewrite(
        &mut self,
        at: &str,
        p: &PathRewriteConfig,
        all_prefix: bool,
    ) -> Option<PathRewrite> {
        let (field, value) = match p {
            PathRewriteConfig::ReplaceFullPath { replace_full_path } => {
                ("replace_full_path", replace_full_path)
            }
            PathRewriteConfig::ReplacePrefixMatch {
                replace_prefix_match,
            } => ("replace_prefix_match", replace_prefix_match),
        };
        let v_at = format!("{at}.{field}");
        if value.len() > MAX_PATH_LEN {
            self.err(&v_at, format!("must be at most {MAX_PATH_LEN} bytes"));
        }
        self.literal_path(&v_at, value);
        Some(match p {
            PathRewriteConfig::ReplaceFullPath { .. } => {
                PathRewrite::ReplaceFullPath(value.clone())
            }
            PathRewriteConfig::ReplacePrefixMatch { .. } => {
                if !all_prefix {
                    self.err(
                        &v_at,
                        "requires every match of the rule to use a PathPrefix path",
                    );
                }
                let trimmed = value.trim_end_matches('/');
                PathRewrite::ReplacePrefixMatch(
                    if trimmed.is_empty() { "/" } else { trimmed }.to_owned(),
                )
            }
        })
    }

    fn backends(
        &mut self,
        rule_at: &str,
        refs: &[BackendRefConfig],
        cluster_ids: &HashMap<&str, ClusterId>,
    ) -> Vec<WeightedBackend> {
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
}

#[cfg(test)]
mod tests {
    use super::super::tests::{assert_errors_at, snapshot};
    use crate::snapshot::{Filter, PathMatch, PathRewrite, StringMatch};

    fn one_rule(rule: &str) -> String {
        format!(
            r"
listeners:
  - name: l
    address: 127.0.0.1:0
    routes:
      - name: r
        rules:
          - {rule}
clusters:
  - {{ name: c, endpoints: [{{ address: 10.0.0.1:80 }}] }}
"
        )
    }

    #[test]
    fn full_match_vocabulary_is_accepted() {
        let yaml = one_rule(
            r#"matches:
              - path: { type: RegularExpression, value: '/users/\d+' }
                method: POST
                headers: [{ name: X-Env, value: canary }, { type: RegularExpression, name: x-ver, value: 'v[12]' }]
                query_params: [{ name: debug, value: "1" }]
            backends: [{ cluster: c }]"#,
        );
        let snap = snapshot(&yaml).expect("valid");
        let m = &snap.listeners()[0].routes()[0].rules()[0].matches()[0];
        assert!(matches!(m.path(), PathMatch::Regex(p) if p.as_str() == r"/users/\d+"));
        assert_eq!(m.method().map(http::Method::as_str), Some("POST"));
        assert_eq!(m.headers()[0].name().as_str(), "x-env");
        assert!(matches!(m.headers()[1].value(), StringMatch::Regex(_)));
        assert_eq!(m.query_params()[0].name(), "debug");
    }

    #[test]
    fn bad_matches_are_reported() {
        let yaml = one_rule(
            r#"matches:
              - path: { type: RegularExpression, value: '(' }
                method: get
                headers: [{ name: "bad header", value: x }, { name: a, value: "1" }, { name: A, value: "2" }]
                query_params: [{ name: "", value: x }]
            backends: [{ cluster: c }]"#,
        );
        let at = "listeners[0].routes[0].rules[0].matches[0]";
        assert_errors_at(
            &yaml,
            &[
                &format!("{at}.path.value"),
                &format!("{at}.method"),
                &format!("{at}.headers[0].name"),
                &format!("{at}.headers[2].name"),
                &format!("{at}.query_params[0].name"),
            ],
        );
    }

    #[test]
    fn filters_parse_and_normalise() {
        let yaml = one_rule(
            r"matches: [{ path: { value: /v1/ } }]
            filters:
              - type: RequestHeaderModifier
                set: [{ name: X-A, value: '1' }]
                add: [{ name: x-b, value: '2' }]
                remove: [x-c]
              - type: ResponseHeaderModifier
                remove: [server]
              - type: URLRewrite
                hostname: Internal.Example
                path: { type: ReplacePrefixMatch, replace_prefix_match: /v2/ }
            backends: [{ cluster: c }]",
        );
        let snap = snapshot(&yaml).expect("valid");
        let filters = snap.listeners()[0].routes()[0].rules()[0].filters();
        assert_eq!(filters.len(), 3);
        let Filter::UrlRewrite(rw) = &filters[2] else {
            panic!("expected rewrite, got {:?}", filters[2]);
        };
        assert_eq!(rw.hostname().unwrap(), "internal.example");
        assert_eq!(
            rw.path(),
            Some(&PathRewrite::ReplacePrefixMatch("/v2".into()))
        );
    }

    #[test]
    fn redirect_rules_have_no_backends() {
        let ok = one_rule("filters: [{ type: RequestRedirect, scheme: https, status_code: 301 }]");
        let snap = snapshot(&ok).expect("valid");
        let rule = &snap.listeners()[0].routes()[0].rules()[0];
        assert_eq!(rule.redirect().unwrap().status(), 301);
        assert!(rule.backends().is_empty());

        let both = one_rule(
            "filters: [{ type: RequestRedirect }]\n            backends: [{ cluster: c }]",
        );
        assert_errors_at(&both, &["listeners[0].routes[0].rules[0].backends"]);
        let neither = one_rule("filters: []");
        assert_errors_at(&neither, &["listeners[0].routes[0].rules[0].backends"]);
        let redirect_and_invalid =
            one_rule("filters: [{ type: RequestRedirect }]\n            invalid_backend_weight: 1");
        assert_errors_at(
            &redirect_and_invalid,
            &["listeners[0].routes[0].rules[0].invalid_backend_weight"],
        );
    }

    /// Rules whose backend references were all invalid answer 500; the
    /// controller states that with `invalid_backend_weight` (Gateway API).
    #[test]
    fn invalid_backend_weight_stands_in_for_backends() {
        let only_invalid = one_rule("invalid_backend_weight: 1");
        let snap = snapshot(&only_invalid).expect("valid");
        let rule = &snap.listeners()[0].routes()[0].rules()[0];
        assert!(rule.backends().is_empty());
        assert_eq!(rule.invalid_backend_weight(), 1);

        let mixed = one_rule(
            "backends: [{ cluster: c, weight: 3 }]\n            invalid_backend_weight: 1",
        );
        let snap = snapshot(&mixed).expect("valid");
        assert_eq!(
            snap.listeners()[0].routes()[0].rules()[0].invalid_backend_weight(),
            1
        );

        let heavy = one_rule("invalid_backend_weight: 1000001");
        assert_errors_at(
            &heavy,
            &["listeners[0].routes[0].rules[0].invalid_backend_weight"],
        );
    }

    #[test]
    fn bad_filters_are_reported() {
        let yaml = one_rule(
            r"matches: [{ path: { type: Exact, value: /x } }]
            filters:
              - type: RequestHeaderModifier
                set: [{ name: host, value: evil }, { name: x-a, value: '1' }]
                remove: [x-a]
              - type: RequestHeaderModifier
              - type: RequestRedirect
                scheme: ftp
                port: 0
                status_code: 200
                path: { type: ReplacePrefixMatch, replace_prefix_match: /y }
              - type: URLRewrite
                hostname: '*.wild'",
        );
        let at = "listeners[0].routes[0].rules[0].filters";
        assert_errors_at(
            &yaml,
            &[
                &format!("{at}[0].set[0].name"),
                &format!("{at}[0].remove[0]"),
                &format!("{at}[1]"),
                &format!("{at}[2].scheme"),
                &format!("{at}[2].port"),
                &format!("{at}[2].status_code"),
                &format!("{at}[2].path.replace_prefix_match"),
                &format!("{at}[3].hostname"),
                at,
            ],
        );
    }

    #[test]
    fn unknown_filter_type_is_a_parse_error() {
        let yaml = one_rule("filters: [{ type: Mirror }]\n            backends: [{ cluster: c }]");
        assert!(crate::from_yaml_str(&yaml).is_err());
    }

    #[test]
    fn unknown_filter_field_is_a_parse_error() {
        let yaml = one_rule(
            "filters: [{ type: RequestHeaderModifier, sett: [] }]\n            backends: [{ cluster: c }]",
        );
        assert!(crate::from_yaml_str(&yaml).is_err());
    }
}
