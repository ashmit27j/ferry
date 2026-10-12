//! Request match predicates: path, method, headers and query parameters.

use std::fmt;

use http::{HeaderMap, HeaderName, Method};
use regex::Regex;
use serde::Serialize;

use super::ser;

/// Conditions that must all hold for a request to match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RequestMatch {
    pub(crate) path: PathMatch,
    #[serde(serialize_with = "ser::opt_method")]
    pub(crate) method: Option<Method>,
    pub(crate) headers: Vec<HeaderMatch>,
    pub(crate) query_params: Vec<QueryParamMatch>,
}

impl RequestMatch {
    /// A match on the path alone.
    pub(crate) fn path_only(path: PathMatch) -> Self {
        Self {
            path,
            method: None,
            headers: Vec::new(),
            query_params: Vec::new(),
        }
    }

    /// The path condition.
    #[must_use]
    pub fn path(&self) -> &PathMatch {
        &self.path
    }

    /// The method condition, if any.
    #[must_use]
    pub fn method(&self) -> Option<&Method> {
        self.method.as_ref()
    }

    /// Header conditions.
    #[must_use]
    pub fn headers(&self) -> &[HeaderMatch] {
        &self.headers
    }

    /// Query parameter conditions.
    #[must_use]
    pub fn query_params(&self) -> &[QueryParamMatch] {
        &self.query_params
    }

    /// True when every condition holds. `path` excludes the query string;
    /// `query` is the raw query string without the leading `?`.
    #[must_use]
    pub fn matches(
        &self,
        method: &Method,
        path: &str,
        query: Option<&str>,
        headers: &HeaderMap,
    ) -> bool {
        self.path.matches(path)
            && self.method.as_ref().is_none_or(|m| m == method)
            && self.headers.iter().all(|h| h.matches(headers))
            && self.query_params.iter().all(|q| q.matches(query))
    }

    /// Gateway API precedence of this match; higher wins.
    #[must_use]
    pub fn precedence(&self) -> MatchPrecedence {
        let (path_kind, path_len) = self.path.precedence();
        MatchPrecedence {
            path_kind,
            path_len,
            method: self.method.is_some(),
            headers: self.headers.len(),
            query_params: self.query_params.len(),
        }
    }
}

/// Ordering key between matches on the same hostname: exact path, then
/// regular expression, then the longest prefix; then a method condition; then
/// the most header conditions; then the most query conditions (ADR 0009).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MatchPrecedence {
    path_kind: u8,
    path_len: usize,
    method: bool,
    headers: usize,
    query_params: usize,
}

/// A path condition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub enum PathMatch {
    /// Path equals the value.
    Exact(String),
    /// Path starts with the value on a `/` boundary. Stored without a trailing
    /// slash, except for the root prefix `/`.
    Prefix(String),
    /// The whole path matches the pattern.
    Regex(Pattern),
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
            Self::Regex(p) => p.is_match(path),
        }
    }

    /// Precedence key: exact beats regex beats prefix; longer values win ties.
    #[must_use]
    pub fn precedence(&self) -> (u8, usize) {
        match self {
            Self::Exact(value) => (2, value.len()),
            Self::Regex(p) => (1, p.as_str().len()),
            Self::Prefix(prefix) => (0, prefix.len()),
        }
    }
}

/// A header condition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HeaderMatch {
    #[serde(serialize_with = "ser::header_name")]
    pub(crate) name: HeaderName,
    pub(crate) value: StringMatch,
}

impl HeaderMatch {
    /// Header name.
    #[must_use]
    pub fn name(&self) -> &HeaderName {
        &self.name
    }

    /// Value condition.
    #[must_use]
    pub fn value(&self) -> &StringMatch {
        &self.value
    }

    /// True when the header is present and its value matches. Repeated
    /// headers are joined with `,` first, as Envoy does.
    #[must_use]
    pub fn matches(&self, headers: &HeaderMap) -> bool {
        let mut values = headers.get_all(&self.name).iter();
        let Some(first) = values.next() else {
            return false;
        };
        let Some(rest) = values.next() else {
            return first.to_str().is_ok_and(|v| self.value.matches(v));
        };
        let mut joined = String::new();
        for v in [first, rest].into_iter().chain(values) {
            let Ok(v) = v.to_str() else {
                return false;
            };
            if !joined.is_empty() {
                joined.push(',');
            }
            joined.push_str(v);
        }
        self.value.matches(&joined)
    }
}

/// A query parameter condition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QueryParamMatch {
    pub(crate) name: String,
    pub(crate) value: StringMatch,
}

impl QueryParamMatch {
    /// Parameter name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Value condition.
    #[must_use]
    pub fn value(&self) -> &StringMatch {
        &self.value
    }

    /// True when the first occurrence of the parameter matches. Values are
    /// compared raw, without percent-decoding; `?flag` has the value `""`.
    #[must_use]
    pub fn matches(&self, query: Option<&str>) -> bool {
        let Some(query) = query else {
            return false;
        };
        query
            .split('&')
            .find_map(|pair| {
                let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                (k == self.name).then_some(v)
            })
            .is_some_and(|v| self.value.matches(v))
    }
}

/// A string comparison.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub enum StringMatch {
    /// Byte-for-byte equality.
    Exact(String),
    /// Whole-string regular expression.
    Regex(Pattern),
}

impl StringMatch {
    /// True when `value` satisfies the comparison.
    #[must_use]
    pub fn matches(&self, value: &str) -> bool {
        match self {
            Self::Exact(want) => value == want,
            Self::Regex(p) => p.is_match(value),
        }
    }
}

/// A compiled regular expression that must match the whole input.
///
/// Equality and serialization use the source text.
#[derive(Clone)]
pub struct Pattern {
    source: String,
    regex: Regex,
}

/// Largest compiled program a pattern may produce, in bytes.
pub(crate) const MAX_REGEX_SIZE: usize = 1 << 20;

impl Pattern {
    /// Compiles `source`, anchored at both ends.
    pub(crate) fn new(source: &str) -> Result<Self, regex::Error> {
        let regex = regex::RegexBuilder::new(&format!("^(?:{source})$"))
            .size_limit(MAX_REGEX_SIZE)
            .build()?;
        Ok(Self {
            source: source.to_owned(),
            regex,
        })
    }

    /// The pattern as written in the config.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.source
    }

    /// True when the whole of `input` matches.
    #[must_use]
    pub fn is_match(&self, input: &str) -> bool {
        self.regex.is_match(input)
    }
}

impl PartialEq for Pattern {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source
    }
}

impl Eq for Pattern {}

impl fmt::Debug for Pattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Pattern").field(&self.source).finish()
    }
}

impl Serialize for Pattern {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    fn pattern(s: &str) -> Pattern {
        Pattern::new(s).unwrap()
    }

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
    fn regex_is_anchored() {
        let m = PathMatch::Regex(pattern(r"/users/\d+"));
        assert!(m.matches("/users/42"));
        assert!(!m.matches("/users/42/edit"));
        assert!(!m.matches("/v1/users/42"));
        // Alternation must not escape the anchors.
        let alt = PathMatch::Regex(pattern("/a|/b"));
        assert!(!alt.matches("/a/x"));
        assert!(alt.matches("/b"));
    }

    #[test]
    fn path_precedence_orders_exact_regex_prefix() {
        let exact = PathMatch::Exact("/a".into()).precedence();
        let regex = PathMatch::Regex(pattern("/a.*")).precedence();
        let long_prefix = PathMatch::Prefix("/aaaaaaaa".into()).precedence();
        assert!(exact > regex && regex > long_prefix);
    }

    #[test]
    fn header_match_joins_repeated_values() {
        let mut h = HeaderMap::new();
        let m = HeaderMatch {
            name: HeaderName::from_static("x-env"),
            value: StringMatch::Exact("a,b".into()),
        };
        assert!(!m.matches(&h));
        h.append("x-env", HeaderValue::from_static("a"));
        assert!(!m.matches(&h));
        h.append("x-env", HeaderValue::from_static("b"));
        assert!(m.matches(&h));
    }

    #[test]
    fn query_match_uses_first_occurrence_and_raw_value() {
        let m = QueryParamMatch {
            name: "v".into(),
            value: StringMatch::Exact("2".into()),
        };
        assert!(m.matches(Some("a=1&v=2&v=3")));
        assert!(!m.matches(Some("v=3&v=2")));
        assert!(!m.matches(None));
        let flag = QueryParamMatch {
            name: "debug".into(),
            value: StringMatch::Exact(String::new()),
        };
        assert!(flag.matches(Some("debug")));
        let encoded = QueryParamMatch {
            name: "q".into(),
            value: StringMatch::Exact("a%20b".into()),
        };
        assert!(encoded.matches(Some("q=a%20b")));
    }

    #[test]
    fn request_match_requires_all_conditions() {
        let m = RequestMatch {
            path: PathMatch::Prefix("/api".into()),
            method: Some(Method::POST),
            headers: vec![HeaderMatch {
                name: HeaderName::from_static("x-env"),
                value: StringMatch::Regex(pattern("canary|beta")),
            }],
            query_params: vec![],
        };
        let mut h = HeaderMap::new();
        h.insert("x-env", HeaderValue::from_static("beta"));
        assert!(m.matches(&Method::POST, "/api/x", None, &h));
        assert!(!m.matches(&Method::GET, "/api/x", None, &h));
        assert!(!m.matches(&Method::POST, "/other", None, &h));
        assert!(!m.matches(&Method::POST, "/api", None, &HeaderMap::new()));
    }

    #[test]
    fn precedence_counts_conditions() {
        let base = RequestMatch::path_only(PathMatch::Prefix("/".into()));
        let mut with_method = base.clone();
        with_method.method = Some(Method::GET);
        let mut with_header = base.clone();
        with_header.headers.push(HeaderMatch {
            name: HeaderName::from_static("a"),
            value: StringMatch::Exact("1".into()),
        });
        assert!(with_method.precedence() > with_header.precedence());
        assert!(with_header.precedence() > base.precedence());
    }

    #[test]
    fn pattern_equality_is_by_source() {
        assert_eq!(pattern("a+"), pattern("a+"));
        assert_ne!(pattern("a+"), pattern("a*"));
        assert_eq!(format!("{:?}", pattern("x")), "Pattern(\"x\")");
    }
}
