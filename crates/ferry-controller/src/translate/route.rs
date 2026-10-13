//! HTTPRoute and GRPCRoute, normalised into one route shape.
//!
//! Matches and filters are converted to ferry-config types here, so the
//! attachment and backend logic is shared. A GRPCRoute method match becomes
//! a path match on `/service/method`, which is how gRPC frames calls.

use ferry_config::{
    FilterConfig, HeaderMatchConfig, HeaderModifierConfig, HeaderValueConfig, MatchConfig,
    PathMatchConfig, PathMatchKind, PathRewriteConfig, QueryParamMatchConfig, RedirectConfig,
    RouteTimeoutsConfig, StringMatchKind, UrlRewriteConfig,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;

use super::RouteKind;
use crate::gateway_api::GROUP;
use crate::gateway_api::grpcroutes as grpc;
use crate::gateway_api::httproutes as http;
use crate::world::ObjKey;

/// A route of either kind.
#[derive(Debug, Clone)]
pub(crate) struct RouteIr {
    pub(crate) kind: RouteKind,
    pub(crate) key: ObjKey,
    pub(crate) generation: i64,
    pub(crate) created: Option<Time>,
    pub(crate) hostnames: Vec<String>,
    pub(crate) parents: Vec<ParentRef>,
    pub(crate) rules: Vec<RuleIr>,
    /// Why the route cannot be accepted (an unsupported filter), if so.
    pub(crate) unsupported: Option<String>,
}

/// A parent reference with defaults filled in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParentRef {
    pub(crate) group: String,
    pub(crate) kind: String,
    pub(crate) namespace: String,
    pub(crate) name: String,
    pub(crate) section_name: Option<String>,
    pub(crate) port: Option<i32>,
}

impl ParentRef {
    pub(crate) fn gateway(&self) -> Option<ObjKey> {
        (self.group == GROUP && self.kind == "Gateway")
            .then(|| ObjKey::new(self.namespace.clone(), self.name.clone()))
    }
}

/// One rule, with matches and filters already in ferry-config form.
#[derive(Debug, Clone, Default)]
pub(crate) struct RuleIr {
    pub(crate) matches: Vec<MatchConfig>,
    pub(crate) filters: Vec<FilterConfig>,
    pub(crate) redirect: bool,
    pub(crate) backends: Vec<BackendRef>,
    pub(crate) timeouts: Option<RouteTimeoutsConfig>,
}

/// A backend reference with defaults filled in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BackendRef {
    pub(crate) group: String,
    pub(crate) kind: String,
    pub(crate) namespace: String,
    pub(crate) name: String,
    pub(crate) port: Option<i32>,
    pub(crate) weight: u32,
}

fn parent(
    route_ns: &str,
    group: Option<&String>,
    kind: Option<&String>,
    ns: Option<&String>,
    name: &str,
    section: Option<&String>,
    port: Option<i32>,
) -> ParentRef {
    ParentRef {
        group: group.cloned().unwrap_or_else(|| GROUP.to_owned()),
        kind: kind.cloned().unwrap_or_else(|| "Gateway".to_owned()),
        namespace: ns.cloned().unwrap_or_else(|| route_ns.to_owned()),
        name: name.to_owned(),
        section_name: section.cloned(),
        port,
    }
}

fn backend(
    route_ns: &str,
    group: Option<&String>,
    kind: Option<&String>,
    ns: Option<&String>,
    name: &str,
    port: Option<i32>,
    weight: Option<i32>,
) -> BackendRef {
    BackendRef {
        group: group.cloned().unwrap_or_default(),
        kind: kind.cloned().unwrap_or_else(|| "Service".to_owned()),
        namespace: ns.cloned().unwrap_or_else(|| route_ns.to_owned()),
        name: name.to_owned(),
        port,
        weight: weight.and_then(|w| u32::try_from(w).ok()).unwrap_or(1),
    }
}

fn modifier<'a>(
    set: impl IntoIterator<Item = (&'a str, &'a str)>,
    add: impl IntoIterator<Item = (&'a str, &'a str)>,
    remove: Option<&Vec<String>>,
) -> HeaderModifierConfig {
    let mut m = HeaderModifierConfig::default();
    m.set = set
        .into_iter()
        .map(|(n, v)| HeaderValueConfig::new(n, v))
        .collect();
    m.add = add
        .into_iter()
        .map(|(n, v)| HeaderValueConfig::new(n, v))
        .collect();
    m.remove = remove.cloned().unwrap_or_default();
    m
}

fn string_kind(regex: bool) -> StringMatchKind {
    if regex {
        StringMatchKind::RegularExpression
    } else {
        StringMatchKind::Exact
    }
}

fn duration(value: Option<&String>) -> Result<Option<ferry_config::Duration>, String> {
    let Some(v) = value else { return Ok(None) };
    let d: ferry_config::Duration = v.parse().map_err(|e| format!("timeout {v:?}: {e}"))?;
    // Gateway API: zero disables the timeout.
    Ok((!std::time::Duration::from(d).is_zero()).then_some(d))
}

impl RouteIr {
    /// Converts an HTTPRoute.
    pub(crate) fn from_http(route: &http::HttpRoute) -> Self {
        let key = ObjKey::of(route);
        let ns = key.namespace.clone();
        let mut unsupported = None;
        let parents = route
            .spec
            .parent_refs
            .iter()
            .flatten()
            .map(|p| {
                parent(
                    &ns,
                    p.group.as_ref(),
                    p.kind.as_ref(),
                    p.namespace.as_ref(),
                    &p.name,
                    p.section_name.as_ref(),
                    p.port,
                )
            })
            .collect();
        let rules = match &route.spec.rules {
            Some(rules) => rules
                .iter()
                .map(|r| http_rule(&ns, r, &mut unsupported))
                .collect(),
            // The CRD defaults rules to one catch-all rule without backends.
            None => vec![RuleIr::default()],
        };
        Self {
            kind: RouteKind::Http,
            generation: route.metadata.generation.unwrap_or_default(),
            created: route.metadata.creation_timestamp.clone(),
            hostnames: route.spec.hostnames.clone().unwrap_or_default(),
            parents,
            rules,
            unsupported,
            key,
        }
    }

    /// Converts a GRPCRoute.
    pub(crate) fn from_grpc(route: &grpc::GrpcRoute) -> Self {
        let key = ObjKey::of(route);
        let ns = key.namespace.clone();
        let mut unsupported = None;
        let parents = route
            .spec
            .parent_refs
            .iter()
            .flatten()
            .map(|p| {
                parent(
                    &ns,
                    p.group.as_ref(),
                    p.kind.as_ref(),
                    p.namespace.as_ref(),
                    &p.name,
                    p.section_name.as_ref(),
                    p.port,
                )
            })
            .collect();
        let rules = match &route.spec.rules {
            Some(rules) => rules
                .iter()
                .map(|r| grpc_rule(&ns, r, &mut unsupported))
                .collect(),
            None => vec![RuleIr::default()],
        };
        Self {
            kind: RouteKind::Grpc,
            generation: route.metadata.generation.unwrap_or_default(),
            created: route.metadata.creation_timestamp.clone(),
            hostnames: route.spec.hostnames.clone().unwrap_or_default(),
            parents,
            rules,
            unsupported,
            key,
        }
    }

    /// The creation time, for precedence ties (older first).
    pub(crate) fn order_key(&self) -> (Option<&Time>, &ObjKey) {
        (self.created.as_ref(), &self.key)
    }
}

fn note(unsupported: &mut Option<String>, what: String) {
    if unsupported.is_none() {
        *unsupported = Some(what);
    }
}

fn http_rule(ns: &str, rule: &http::HttpRouteRules, unsupported: &mut Option<String>) -> RuleIr {
    let matches = rule
        .matches
        .iter()
        .flatten()
        .map(|m| http_match(m, unsupported))
        .collect();
    let mut filters = Vec::new();
    let mut redirect = false;
    for f in rule.filters.iter().flatten() {
        match http_filter(f) {
            Ok(Some(fc)) => {
                redirect |= matches!(fc, FilterConfig::RequestRedirect(_));
                filters.push(fc);
            }
            Ok(None) => {}
            Err(why) => note(unsupported, why),
        }
    }
    let backends = rule
        .backend_refs
        .iter()
        .flatten()
        .map(|b| {
            if b.filters.as_ref().is_some_and(|f| !f.is_empty()) {
                note(
                    unsupported,
                    "backendRefs[].filters are not supported".into(),
                );
            }
            backend(
                ns,
                b.group.as_ref(),
                b.kind.as_ref(),
                b.namespace.as_ref(),
                &b.name,
                b.port,
                b.weight,
            )
        })
        .collect();
    let timeouts = rule.timeouts.as_ref().and_then(|t| {
        let parsed = (|| -> Result<RouteTimeoutsConfig, String> {
            let mut out = RouteTimeoutsConfig::default();
            out.request = duration(t.request.as_ref())?;
            out.backend_request = duration(t.backend_request.as_ref())?;
            Ok(out)
        })();
        match parsed {
            Ok(t) => Some(t),
            Err(why) => {
                note(unsupported, why);
                None
            }
        }
    });
    RuleIr {
        matches,
        filters,
        redirect,
        backends,
        timeouts,
    }
}

fn http_match(m: &http::HttpRouteRulesMatches, unsupported: &mut Option<String>) -> MatchConfig {
    let mut out = MatchConfig::default();
    if let Some(p) = &m.path {
        let kind = match p.r#type {
            Some(http::HttpRouteRulesMatchesPathType::Exact) => PathMatchKind::Exact,
            Some(http::HttpRouteRulesMatchesPathType::RegularExpression) => {
                PathMatchKind::RegularExpression
            }
            Some(http::HttpRouteRulesMatchesPathType::Unknown) => {
                note(unsupported, "unknown path match type".into());
                PathMatchKind::PathPrefix
            }
            _ => PathMatchKind::PathPrefix,
        };
        out.path = Some(PathMatchConfig::new(
            kind,
            p.value.clone().unwrap_or_else(|| "/".into()),
        ));
    }
    out.method = m.method.as_ref().and_then(|method| {
        let name = serde_json::to_value(method).ok()?.as_str()?.to_owned();
        if name == "Unknown" {
            note(unsupported, "unknown method".into());
            return None;
        }
        Some(name)
    });
    for h in m.headers.iter().flatten() {
        let regex = matches!(
            h.r#type,
            Some(http::HttpRouteRulesMatchesHeadersType::RegularExpression)
        );
        out.headers.push(HeaderMatchConfig::new(
            string_kind(regex),
            h.name.clone(),
            h.value.clone(),
        ));
    }
    for q in m.query_params.iter().flatten() {
        let regex = matches!(
            q.r#type,
            Some(http::HttpRouteRulesMatchesQueryParamsType::RegularExpression)
        );
        out.query_params.push(QueryParamMatchConfig::new(
            string_kind(regex),
            q.name.clone(),
            q.value.clone(),
        ));
    }
    out
}

fn pairs<'a, T>(
    v: Option<&'a Vec<T>>,
    f: impl Fn(&'a T) -> (&'a str, &'a str),
) -> Vec<(&'a str, &'a str)> {
    v.into_iter().flatten().map(f).collect()
}

fn path_rewrite(
    full: Option<&String>,
    prefix: Option<&String>,
    replace_full: bool,
) -> Result<PathRewriteConfig, String> {
    match (replace_full, full, prefix) {
        (true, Some(p), _) => Ok(PathRewriteConfig::ReplaceFullPath {
            replace_full_path: p.clone(),
        }),
        (false, _, Some(p)) => Ok(PathRewriteConfig::ReplacePrefixMatch {
            replace_prefix_match: p.clone(),
        }),
        _ => Err("path modifier without a value".into()),
    }
}

fn http_filter(f: &http::HttpRouteRulesFilters) -> Result<Option<FilterConfig>, String> {
    use http::HttpRouteRulesFiltersType as T;
    Ok(Some(match f.r#type {
        T::RequestHeaderModifier => {
            let Some(m) = &f.request_header_modifier else {
                return Ok(None);
            };
            FilterConfig::RequestHeaderModifier(modifier(
                pairs(m.set.as_ref(), |h| (h.name.as_str(), h.value.as_str())),
                pairs(m.add.as_ref(), |h| (h.name.as_str(), h.value.as_str())),
                m.remove.as_ref(),
            ))
        }
        T::ResponseHeaderModifier => {
            let Some(m) = &f.response_header_modifier else {
                return Ok(None);
            };
            FilterConfig::ResponseHeaderModifier(modifier(
                pairs(m.set.as_ref(), |h| (h.name.as_str(), h.value.as_str())),
                pairs(m.add.as_ref(), |h| (h.name.as_str(), h.value.as_str())),
                m.remove.as_ref(),
            ))
        }
        T::RequestRedirect => {
            let Some(r) = &f.request_redirect else {
                return Ok(None);
            };
            let mut out = RedirectConfig::default();
            out.scheme = r.scheme.as_ref().map(|s| match s {
                http::HttpRouteRulesFiltersRequestRedirectScheme::Https => "https".to_owned(),
                _ => "http".to_owned(),
            });
            out.hostname.clone_from(&r.hostname);
            out.port = r.port.and_then(|p| u16::try_from(p).ok());
            out.status_code = r.status_code.and_then(|c| u16::try_from(c).ok());
            out.path = r
                .path
                .as_ref()
                .map(|p| {
                    path_rewrite(
                        p.replace_full_path.as_ref(),
                        p.replace_prefix_match.as_ref(),
                        matches!(
                            p.r#type,
                            http::HttpRouteRulesFiltersRequestRedirectPathType::ReplaceFullPath
                        ),
                    )
                })
                .transpose()?;
            FilterConfig::RequestRedirect(out)
        }
        T::UrlRewrite => {
            let Some(r) = &f.url_rewrite else {
                return Ok(None);
            };
            let mut out = UrlRewriteConfig::default();
            out.hostname.clone_from(&r.hostname);
            out.path = r
                .path
                .as_ref()
                .map(|p| {
                    path_rewrite(
                        p.replace_full_path.as_ref(),
                        p.replace_prefix_match.as_ref(),
                        matches!(
                            p.r#type,
                            http::HttpRouteRulesFiltersUrlRewritePathType::ReplaceFullPath
                        ),
                    )
                })
                .transpose()?;
            FilterConfig::UrlRewrite(out)
        }
        T::RequestMirror => return Err("RequestMirror filters are not supported".into()),
        T::ExtensionRef => return Err("ExtensionRef filters are not supported".into()),
        T::Cors => return Err("CORS filters are not supported".into()),
        T::Unknown => return Err("unknown filter type".into()),
    }))
}

/// `[^/]+`: one path segment, for a gRPC service or method left unset.
const ANY_SEGMENT: &str = "[^/]+";

fn grpc_rule(ns: &str, rule: &grpc::GrpcRouteRules, unsupported: &mut Option<String>) -> RuleIr {
    let matches = rule
        .matches
        .iter()
        .flatten()
        .map(|m| grpc_match(m, unsupported))
        .collect();
    let mut filters = Vec::new();
    for f in rule.filters.iter().flatten() {
        use grpc::GrpcRouteRulesFiltersType as T;
        match f.r#type {
            T::RequestHeaderModifier => {
                if let Some(m) = &f.request_header_modifier {
                    filters.push(FilterConfig::RequestHeaderModifier(modifier(
                        pairs(m.set.as_ref(), |h| (h.name.as_str(), h.value.as_str())),
                        pairs(m.add.as_ref(), |h| (h.name.as_str(), h.value.as_str())),
                        m.remove.as_ref(),
                    )));
                }
            }
            T::ResponseHeaderModifier => {
                if let Some(m) = &f.response_header_modifier {
                    filters.push(FilterConfig::ResponseHeaderModifier(modifier(
                        pairs(m.set.as_ref(), |h| (h.name.as_str(), h.value.as_str())),
                        pairs(m.add.as_ref(), |h| (h.name.as_str(), h.value.as_str())),
                        m.remove.as_ref(),
                    )));
                }
            }
            T::RequestMirror => note(
                unsupported,
                "RequestMirror filters are not supported".into(),
            ),
            T::ExtensionRef => note(unsupported, "ExtensionRef filters are not supported".into()),
            T::Unknown => note(unsupported, "unknown filter type".into()),
        }
    }
    let backends = rule
        .backend_refs
        .iter()
        .flatten()
        .map(|b| {
            if b.filters.as_ref().is_some_and(|f| !f.is_empty()) {
                note(
                    unsupported,
                    "backendRefs[].filters are not supported".into(),
                );
            }
            backend(
                ns,
                b.group.as_ref(),
                b.kind.as_ref(),
                b.namespace.as_ref(),
                &b.name,
                b.port,
                b.weight,
            )
        })
        .collect();
    RuleIr {
        matches,
        filters,
        redirect: false,
        backends,
        timeouts: None,
    }
}

fn grpc_match(m: &grpc::GrpcRouteRulesMatches, unsupported: &mut Option<String>) -> MatchConfig {
    let mut out = MatchConfig::default();
    if let Some(method) = &m.method {
        let regex = match method.r#type {
            Some(grpc::GrpcRouteRulesMatchesMethodType::RegularExpression) => true,
            Some(grpc::GrpcRouteRulesMatchesMethodType::Unknown) => {
                note(unsupported, "unknown method match type".into());
                false
            }
            _ => false,
        };
        let (service, name) = (method.service.as_deref(), method.method.as_deref());
        out.path = match (regex, service, name) {
            (_, None, None) => None,
            (false, Some(s), Some(n)) => Some(PathMatchConfig::new(
                PathMatchKind::Exact,
                format!("/{s}/{n}"),
            )),
            (false, Some(s), None) => Some(PathMatchConfig::new(
                PathMatchKind::PathPrefix,
                format!("/{s}"),
            )),
            (false, None, Some(n)) => Some(PathMatchConfig::new(
                PathMatchKind::RegularExpression,
                format!("/{ANY_SEGMENT}/{}", regex_escape(n)),
            )),
            (true, s, n) => Some(PathMatchConfig::new(
                PathMatchKind::RegularExpression,
                format!("/{}/{}", s.unwrap_or(ANY_SEGMENT), n.unwrap_or(ANY_SEGMENT)),
            )),
        };
    }
    for h in m.headers.iter().flatten() {
        let regex = matches!(
            h.r#type,
            Some(grpc::GrpcRouteRulesMatchesHeadersType::RegularExpression)
        );
        out.headers.push(HeaderMatchConfig::new(
            string_kind(regex),
            h.name.clone(),
            h.value.clone(),
        ));
    }
    out
}

/// Escapes the regex metacharacters a gRPC method name could contain.
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.+*?()|[]{}^$#&-~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Names a route uses in metrics and in its Ferry route name.
pub(crate) fn route_name(kind: RouteKind, key: &ObjKey) -> String {
    let prefix = match kind {
        RouteKind::Http => "httproute",
        RouteKind::Grpc => "grpcroute",
    };
    format!("{prefix}_{}_{}", key.namespace, key.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grpc_route(yaml: &str) -> RouteIr {
        let r: grpc::GrpcRoute = serde_saphyr::from_str(yaml).unwrap();
        RouteIr::from_grpc(&r)
    }

    #[test]
    fn grpc_methods_become_paths() {
        let r = grpc_route(
            r"
metadata: { name: g, namespace: ns }
spec:
  rules:
    - matches:
        - method: { service: pkg.Echo, method: Ping }
        - method: { service: pkg.Echo }
        - method: { method: Ping.v2 }
        - method: { type: RegularExpression, service: 'pkg\..*' }
        - headers: [{ name: version, value: one }]
",
        );
        let paths: Vec<_> = r.rules[0]
            .matches
            .iter()
            .map(|m| m.path.as_ref().map(|p| (p.kind, p.value.clone())))
            .collect();
        assert_eq!(
            paths,
            vec![
                Some((PathMatchKind::Exact, "/pkg.Echo/Ping".into())),
                Some((PathMatchKind::PathPrefix, "/pkg.Echo".into())),
                Some((PathMatchKind::RegularExpression, "/[^/]+/Ping\\.v2".into())),
                Some((PathMatchKind::RegularExpression, "/pkg\\..*/[^/]+".into())),
                None,
            ]
        );
        assert!(r.unsupported.is_none());
        assert_eq!(r.parents.len(), 0);
    }

    #[test]
    fn http_defaults_are_filled_in() {
        let r: http::HttpRoute = serde_saphyr::from_str(
            r"
metadata: { name: h, namespace: ns, generation: 4 }
spec:
  parentRefs: [{ name: gw }]
  rules:
    - backendRefs: [{ name: svc, port: 80 }]
      timeouts: { request: 10s, backendRequest: 0s }
",
        )
        .unwrap();
        let r = RouteIr::from_http(&r);
        assert_eq!(r.generation, 4);
        assert_eq!(r.parents[0].gateway(), Some(ObjKey::new("ns", "gw")));
        let b = &r.rules[0].backends[0];
        assert_eq!(
            (b.kind.as_str(), b.namespace.as_str(), b.weight),
            ("Service", "ns", 1)
        );
        let t = r.rules[0].timeouts.as_ref().unwrap();
        assert_eq!(t.request.unwrap().to_string(), "10s");
        assert!(t.backend_request.is_none(), "0s disables the timeout");
    }

    #[test]
    fn unsupported_filters_are_reported() {
        let r: http::HttpRoute = serde_saphyr::from_str(
            r"
metadata: { name: h, namespace: ns }
spec:
  rules:
    - filters:
        - type: RequestMirror
          requestMirror: { backendRef: { name: m, port: 80 } }
",
        )
        .unwrap();
        let r = RouteIr::from_http(&r);
        assert!(r.unsupported.unwrap().contains("RequestMirror"));
    }
}
