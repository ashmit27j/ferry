//! Hostname intersection between a listener and a route (Gateway API).
//!
//! A listener hostname and a route hostname intersect when one matches the
//! other; the result is the more specific of the two. A listener without a
//! hostname accepts every route hostname, and a route without hostnames
//! takes the listener's.

/// The hostnames a route serves through a listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Hosts {
    /// Every hostname.
    Any,
    /// These hostnames (exact or `*.` wildcards), deduplicated.
    Some(Vec<String>),
}

/// Intersects a listener hostname with a route's hostnames. `None` means
/// they share no hostname, so the route cannot attach to the listener.
pub(crate) fn intersect(listener: Option<&str>, route: &[String]) -> Option<Hosts> {
    let Some(listener) = listener else {
        return Some(if route.is_empty() {
            Hosts::Any
        } else {
            Hosts::Some(dedup(
                route.iter().map(|h| h.to_ascii_lowercase()).collect(),
            ))
        });
    };
    let listener = listener.to_ascii_lowercase();
    if route.is_empty() {
        return Some(Hosts::Some(vec![listener]));
    }
    let out: Vec<String> = route
        .iter()
        .filter_map(|h| narrower(&listener, &h.to_ascii_lowercase()))
        .collect();
    (!out.is_empty()).then(|| Hosts::Some(dedup(out)))
}

/// The more specific of two hostnames when one matches the other.
fn narrower(a: &str, b: &str) -> Option<String> {
    if a == b {
        return Some(a.to_owned());
    }
    if matches(a, b) {
        return Some(b.to_owned());
    }
    if matches(b, a) {
        return Some(a.to_owned());
    }
    None
}

/// Whether `pattern` (exact or `*.suffix`) covers `host` (exact or wildcard).
/// `*.example.com` covers `a.example.com`, `a.b.example.com` and
/// `*.b.example.com`, but not `example.com`.
fn matches(pattern: &str, host: &str) -> bool {
    match pattern.strip_prefix('*') {
        // `suffix` is ".example.com"; a wildcard host compares by its own
        // suffix, so `*.b.example.com` is covered but `*.example.com` is not
        // strictly narrower (equality is handled by the caller).
        Some(suffix) => {
            let bare = host.strip_prefix('*').unwrap_or(host);
            bare.len() > suffix.len() && bare.ends_with(suffix)
        }
        None => pattern == host,
    }
}

fn dedup(mut v: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    v.retain(|h| seen.insert(h.clone()));
    v
}

/// Unions the hostnames of several attachments of one route.
pub(crate) fn union(a: Hosts, b: Hosts) -> Hosts {
    match (a, b) {
        (Hosts::Any, _) | (_, Hosts::Any) => Hosts::Any,
        (Hosts::Some(mut x), Hosts::Some(y)) => {
            x.extend(y);
            Hosts::Some(dedup(x))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|h| (*h).to_owned()).collect()
    }

    fn some(v: &[&str]) -> Option<Hosts> {
        Some(Hosts::Some(s(v)))
    }

    #[test]
    fn listener_without_hostname_takes_the_route_hostnames() {
        assert_eq!(intersect(None, &[]), Some(Hosts::Any));
        assert_eq!(intersect(None, &s(&["A.example"])), some(&["a.example"]));
    }

    #[test]
    fn route_without_hostnames_takes_the_listener_hostname() {
        assert_eq!(intersect(Some("*.wild.org"), &[]), some(&["*.wild.org"]));
    }

    /// Listener hostname, route hostnames, expected intersection.
    type Case<'a> = (&'a str, &'a [&'a str], Option<&'a [&'a str]>);

    /// The table from the Gateway API's HTTPRoute hostname documentation.
    #[test]
    fn intersections_keep_the_more_specific_name() {
        let cases: &[Case<'_>] = &[
            (
                "foo.example.com",
                &["foo.example.com"],
                Some(&["foo.example.com"]),
            ),
            (
                "foo.example.com",
                &["*.example.com"],
                Some(&["foo.example.com"]),
            ),
            (
                "*.example.com",
                &["foo.example.com"],
                Some(&["foo.example.com"]),
            ),
            (
                "*.example.com",
                &["a.b.example.com"],
                Some(&["a.b.example.com"]),
            ),
            (
                "*.example.com",
                &["*.foo.example.com"],
                Some(&["*.foo.example.com"]),
            ),
            (
                "*.foo.example.com",
                &["*.example.com"],
                Some(&["*.foo.example.com"]),
            ),
            ("*.example.com", &["example.com"], None),
            ("foo.example.com", &["bar.example.com"], None),
            (
                "*.example.com",
                &["*.example.com", "x.other.org"],
                Some(&["*.example.com"]),
            ),
            (
                "very.specific.com",
                &["non.matching.com", "*.specific.com"],
                Some(&["very.specific.com"]),
            ),
        ];
        for (listener, route, want) in cases {
            let got = intersect(Some(listener), &s(route));
            let want = want.map(|w| Hosts::Some(s(w)));
            assert_eq!(got, want, "listener {listener} route {route:?}");
        }
    }

    #[test]
    fn union_with_any_is_any() {
        assert_eq!(union(Hosts::Any, Hosts::Some(s(&["a"]))), Hosts::Any);
        assert_eq!(
            union(Hosts::Some(s(&["a", "b"])), Hosts::Some(s(&["b", "c"]))),
            Hosts::Some(s(&["a", "b", "c"]))
        );
    }
}
