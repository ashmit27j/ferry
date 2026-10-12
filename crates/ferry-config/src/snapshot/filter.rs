//! Rule filters: header modifiers, redirects and URL rewrites.

use http::{HeaderMap, HeaderName, HeaderValue};
use serde::Serialize;

use super::matching::PathMatch;
use super::ser;

/// A rule filter (Gateway API `HTTPRouteFilter`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub enum Filter {
    /// Edits request headers before forwarding.
    RequestHeaderModifier(HeaderModifier),
    /// Edits response headers before returning them downstream.
    ResponseHeaderModifier(HeaderModifier),
    /// Answers with a redirect instead of forwarding.
    RequestRedirect(Redirect),
    /// Rewrites host and path before forwarding.
    UrlRewrite(UrlRewrite),
}

/// Header edits, applied as set, then add, then remove.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct HeaderModifier {
    #[serde(serialize_with = "ser::header_pairs")]
    pub(crate) set: Vec<(HeaderName, HeaderValue)>,
    #[serde(serialize_with = "ser::header_pairs")]
    pub(crate) add: Vec<(HeaderName, HeaderValue)>,
    #[serde(serialize_with = "ser::header_names")]
    pub(crate) remove: Vec<HeaderName>,
}

impl HeaderModifier {
    /// Headers overwritten, replacing all existing values.
    #[must_use]
    pub fn set(&self) -> &[(HeaderName, HeaderValue)] {
        &self.set
    }

    /// Headers appended.
    #[must_use]
    pub fn add(&self) -> &[(HeaderName, HeaderValue)] {
        &self.add
    }

    /// Header names removed.
    #[must_use]
    pub fn remove(&self) -> &[HeaderName] {
        &self.remove
    }

    /// Applies the edits to `headers`.
    pub fn apply(&self, headers: &mut HeaderMap) {
        for (name, value) in &self.set {
            headers.insert(name.clone(), value.clone());
        }
        for (name, value) in &self.add {
            headers.append(name.clone(), value.clone());
        }
        for name in &self.remove {
            headers.remove(name);
        }
    }
}

/// How a redirect or rewrite changes the path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub enum PathRewrite {
    /// Replace the whole path.
    ReplaceFullPath(String),
    /// Replace the prefix the rule matched. Stored without a trailing slash,
    /// except for `/`.
    ReplacePrefixMatch(String),
}

impl PathRewrite {
    /// Returns the rewritten `path`. `matched` is the path condition that
    /// selected the rule; prefix replacement leaves the path unchanged unless
    /// it is a [`PathMatch::Prefix`].
    ///
    /// Prefix replacement follows the Gateway API: with prefix `/foo`,
    /// replacing with `/bar` maps `/foo/x` to `/bar/x`, and replacing with `/`
    /// maps `/foo/x` to `/x` and `/foo` to `/`.
    #[must_use]
    pub fn apply(&self, path: &str, matched: &PathMatch) -> String {
        match self {
            Self::ReplaceFullPath(full) => full.clone(),
            Self::ReplacePrefixMatch(replacement) => {
                let PathMatch::Prefix(prefix) = matched else {
                    return path.to_owned();
                };
                let rest = if prefix == "/" {
                    path
                } else {
                    path.get(prefix.len()..).unwrap_or("")
                };
                let rest = rest.strip_prefix('/').unwrap_or(rest);
                let base = replacement.trim_end_matches('/');
                match (base.is_empty(), rest.is_empty()) {
                    (true, true) => "/".to_owned(),
                    (true, false) => format!("/{rest}"),
                    (false, true) => base.to_owned(),
                    (false, false) => format!("{base}/{rest}"),
                }
            }
        }
    }
}

/// A redirect response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Redirect {
    pub(crate) scheme: Option<String>,
    pub(crate) hostname: Option<String>,
    pub(crate) path: Option<PathRewrite>,
    pub(crate) port: Option<u16>,
    pub(crate) status: u16,
}

/// The parts of a request a redirect needs.
#[derive(Debug, Clone, Copy)]
pub struct RedirectTarget<'a> {
    /// Scheme the request arrived on (`http` until TLS lands in M2).
    pub scheme: &'a str,
    /// Request host, without port.
    pub host: &'a str,
    /// Port of the listener that accepted the request.
    pub listener_port: u16,
    /// Request path.
    pub path: &'a str,
    /// Raw query string, without `?`.
    pub query: Option<&'a str>,
}

impl Redirect {
    /// Status code: 301, 302, 303, 307 or 308.
    #[must_use]
    pub fn status(&self) -> u16 {
        self.status
    }

    /// Scheme override.
    #[must_use]
    pub fn scheme(&self) -> Option<&str> {
        self.scheme.as_deref()
    }

    /// Hostname override.
    #[must_use]
    pub fn hostname(&self) -> Option<&str> {
        self.hostname.as_deref()
    }

    /// Path replacement.
    #[must_use]
    pub fn path(&self) -> Option<&PathRewrite> {
        self.path.as_ref()
    }

    /// Port override.
    #[must_use]
    pub fn port(&self) -> Option<u16> {
        self.port
    }

    /// Builds the `Location` value.
    ///
    /// Port rules follow the Gateway API: an explicit port wins; otherwise a
    /// changed scheme implies its well-known port; otherwise the listener port
    /// is kept. Ports 80 for `http` and 443 for `https` are omitted. The query
    /// string is preserved.
    #[must_use]
    pub fn location(&self, req: &RedirectTarget<'_>, matched: &PathMatch) -> String {
        let scheme = self.scheme.as_deref().unwrap_or(req.scheme);
        let host = self.hostname.as_deref().unwrap_or(req.host);
        let implied_port = match self.scheme.as_deref() {
            Some("https") => 443,
            Some(_) => 80,
            None => req.listener_port,
        };
        let port = self.port.unwrap_or(implied_port);
        let default_port = matches!((scheme, port), ("http", 80) | ("https", 443));
        let path = self
            .path
            .as_ref()
            .map_or_else(|| req.path.to_owned(), |p| p.apply(req.path, matched));
        let mut out = String::with_capacity(scheme.len() + host.len() + path.len() + 16);
        out.push_str(scheme);
        out.push_str("://");
        if host.contains(':') {
            out.push('[');
            out.push_str(host);
            out.push(']');
        } else {
            out.push_str(host);
        }
        if !default_port {
            out.push(':');
            out.push_str(&port.to_string());
        }
        out.push_str(&path);
        if let Some(q) = req.query {
            out.push('?');
            out.push_str(q);
        }
        out
    }
}

/// Host and path rewriting before forwarding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UrlRewrite {
    #[serde(serialize_with = "ser::opt_header_value")]
    pub(crate) hostname: Option<HeaderValue>,
    pub(crate) path: Option<PathRewrite>,
}

impl UrlRewrite {
    /// Replacement `Host`, if any.
    #[must_use]
    pub fn hostname(&self) -> Option<&HeaderValue> {
        self.hostname.as_ref()
    }

    /// Path replacement, if any.
    #[must_use]
    pub fn path(&self) -> Option<&PathRewrite> {
        self.path.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefix(p: &str) -> PathMatch {
        PathMatch::Prefix(p.into())
    }

    #[test]
    fn replace_prefix_follows_gateway_api_examples() {
        let cases = [
            ("/foo", "/bar", "/foo/x", "/bar/x"),
            ("/foo", "/bar", "/foo", "/bar"),
            ("/foo", "/bar/", "/foo/x", "/bar/x"),
            ("/foo", "/", "/foo/x", "/x"),
            ("/foo", "/", "/foo", "/"),
            ("/foo", "/", "/foo/", "/"),
            ("/prefix/one", "/one", "/prefix/one/two", "/one/two"),
            ("/", "/api", "/x/y", "/api/x/y"),
            ("/", "/api", "/", "/api"),
        ];
        for (pre, replacement, path, want) in cases {
            let got = PathRewrite::ReplacePrefixMatch(replacement.into()).apply(path, &prefix(pre));
            assert_eq!(got, want, "prefix {pre} -> {replacement} on {path}");
        }
    }

    #[test]
    fn replace_full_path_ignores_input() {
        let r = PathRewrite::ReplaceFullPath("/new".into());
        assert_eq!(r.apply("/anything", &prefix("/")), "/new");
    }

    fn target<'a>(path: &'a str, query: Option<&'a str>) -> RedirectTarget<'a> {
        RedirectTarget {
            scheme: "http",
            host: "example.com",
            listener_port: 8080,
            path,
            query,
        }
    }

    fn redirect() -> Redirect {
        Redirect {
            scheme: None,
            hostname: None,
            path: None,
            port: None,
            status: 302,
        }
    }

    #[test]
    fn redirect_keeps_listener_port_without_scheme_change() {
        let loc = redirect().location(&target("/a", Some("x=1")), &prefix("/"));
        assert_eq!(loc, "http://example.com:8080/a?x=1");
    }

    #[test]
    fn https_redirect_uses_well_known_port() {
        let r = Redirect {
            scheme: Some("https".into()),
            ..redirect()
        };
        assert_eq!(
            r.location(&target("/a", None), &prefix("/")),
            "https://example.com/a"
        );
    }

    #[test]
    fn explicit_port_and_host_win() {
        let r = Redirect {
            hostname: Some("other.test".into()),
            port: Some(9443),
            path: Some(PathRewrite::ReplacePrefixMatch("/v2".into())),
            ..redirect()
        };
        assert_eq!(
            r.location(&target("/v1/x", None), &prefix("/v1")),
            "http://other.test:9443/v2/x"
        );
        let r = Redirect {
            port: Some(80),
            ..redirect()
        };
        assert_eq!(
            r.location(&target("/", None), &prefix("/")),
            "http://example.com/"
        );
    }

    #[test]
    fn ipv6_host_is_bracketed() {
        let t = RedirectTarget {
            host: "::1",
            ..target("/", None)
        };
        assert_eq!(redirect().location(&t, &prefix("/")), "http://[::1]:8080/");
    }

    #[test]
    fn header_modifier_sets_adds_then_removes() {
        let m = HeaderModifier {
            set: vec![(
                HeaderName::from_static("x-a"),
                HeaderValue::from_static("set"),
            )],
            add: vec![(
                HeaderName::from_static("x-b"),
                HeaderValue::from_static("added"),
            )],
            remove: vec![HeaderName::from_static("x-c")],
        };
        let mut h = HeaderMap::new();
        h.append("x-a", HeaderValue::from_static("old1"));
        h.append("x-a", HeaderValue::from_static("old2"));
        h.append("x-b", HeaderValue::from_static("kept"));
        h.append("x-c", HeaderValue::from_static("gone"));
        m.apply(&mut h);
        assert_eq!(h.get_all("x-a").iter().collect::<Vec<_>>(), ["set"]);
        assert_eq!(
            h.get_all("x-b").iter().collect::<Vec<_>>(),
            ["kept", "added"]
        );
        assert!(!h.contains_key("x-c"));
    }
}
