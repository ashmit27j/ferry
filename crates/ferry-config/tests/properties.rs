//! Property tests for configuration validation and path matching.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use ferry_config::{
    BackendRefConfig, ClusterConfig, Config, Duration, EndpointConfig, LbPolicy, ListenerConfig,
    MatchConfig, PathMatch, PathMatchConfig, PathMatchKind, RouteConfig, RuleConfig,
    SnapshotVersion, from_yaml_str,
};
use proptest::prelude::*;

fn segment() -> impl Strategy<Value = String> {
    "[a-z0-9_-]{1,8}"
}

fn path() -> impl Strategy<Value = String> {
    prop::collection::vec(segment(), 0..4).prop_map(|s| format!("/{}", s.join("/")))
}

/// Generates configs that are valid by construction.
fn valid_config() -> impl Strategy<Value = Config> {
    (1usize..4, 1usize..4, 1usize..6).prop_flat_map(|(n_clusters, n_routes, n_eps)| {
        let clusters = (0..n_clusters)
            .map(|c| {
                let endpoints = (0..n_eps)
                    .map(|e| {
                        let mut ep = EndpointConfig::new(
                            format!("10.0.{c}.{}:8080", e + 1)
                                .parse()
                                .expect("static address"),
                        );
                        ep.weight = u32::try_from(e % 4 + 1).expect("small");
                        ep
                    })
                    .collect();
                let mut cluster = ClusterConfig::new(format!("c{c}"), endpoints);
                cluster.lb_policy = [
                    LbPolicy::RoundRobin,
                    LbPolicy::WeightedRoundRobin,
                    LbPolicy::LeastRequest,
                    LbPolicy::P2cEwma,
                ][c % 4];
                cluster
            })
            .collect::<Vec<_>>();
        prop::collection::vec((path(), 0u8..3, 0..n_clusters, 0u32..1000), n_routes).prop_map(
            move |rules| {
                let mut listener =
                    ListenerConfig::new("l", "127.0.0.1:0".parse().expect("static address"));
                listener.routes = rules
                    .into_iter()
                    .enumerate()
                    .map(|(i, (p, kind, cluster, weight))| {
                        let kind = match kind {
                            0 => PathMatchKind::Exact,
                            1 => PathMatchKind::PathPrefix,
                            _ => PathMatchKind::RegularExpression,
                        };
                        let mut rule = RuleConfig::forward(vec![BackendRefConfig::new(
                            format!("c{cluster}"),
                            weight,
                        )]);
                        rule.matches = vec![MatchConfig::path(PathMatchConfig::new(kind, p))];
                        RouteConfig::new(format!("r{i}"), vec![rule])
                    })
                    .collect();
                Config::new(vec![listener], clusters.clone())
            },
        )
    })
}

proptest! {
    #[test]
    fn valid_configs_always_validate(config in valid_config()) {
        let routes = config.listeners[0].routes.len();
        let clusters = config.clusters.len();
        let snap = config.into_snapshot(SnapshotVersion(7)).expect("valid by construction");
        prop_assert_eq!(snap.version(), SnapshotVersion(7));
        prop_assert_eq!(snap.listeners()[0].routes().len(), routes);
        prop_assert_eq!(snap.clusters().len(), clusters);
    }

    #[test]
    fn yaml_round_trip_preserves_config(config in valid_config()) {
        let yaml = serde_saphyr::to_string(&config).expect("serialises");
        let back = from_yaml_str(&yaml).expect("parses");
        prop_assert_eq!(back, config);
    }

    #[test]
    fn prefix_matches_itself_and_children(prefix in path(), child in path()) {
        let m = PathMatch::Prefix(prefix.clone());
        prop_assert!(m.matches(&prefix));
        let joined = if prefix == "/" { child.clone() } else { format!("{prefix}{child}") };
        prop_assert!(m.matches(&joined));
    }

    #[test]
    fn prefix_never_matches_sibling_with_extra_chars(prefix in path(), extra in "[a-z0-9]{1,4}") {
        prop_assume!(prefix != "/");
        let m = PathMatch::Prefix(prefix.clone());
        let sibling = format!("{prefix}{extra}");
        prop_assert!(!m.matches(&sibling));
    }

    #[test]
    fn regex_of_a_literal_path_matches_only_that_path(p in path(), other in path()) {
        // Paths from the generator contain no regex metacharacters except '-'
        // outside classes, which is literal there.
        let yaml = format!(
            "listeners: [{{ name: l, address: 127.0.0.1:0, routes: [{{ name: r, rules: [{{ matches: [{{ path: {{ type: RegularExpression, value: '{p}' }} }}], backends: [{{ cluster: c }}] }}] }}] }}]
clusters: [{{ name: c, endpoints: [{{ address: 10.0.0.1:80 }}] }}]
"
        );
        let snap = from_yaml_str(&yaml).unwrap().into_snapshot(SnapshotVersion(1)).unwrap();
        let m = snap.listeners()[0].routes()[0].rules()[0].matches()[0].path();
        prop_assert!(m.matches(&p));
        prop_assert_eq!(m.matches(&other), other == p);
    }

    #[test]
    fn durations_round_trip_through_text(ms in 0u64..10_000_000) {
        let d = Duration::new(std::time::Duration::from_millis(ms));
        let back: Duration = d.to_string().parse().unwrap();
        prop_assert_eq!(back, d);
    }

    #[test]
    fn arbitrary_text_never_panics(text in ".{0,512}") {
        if let Ok(config) = from_yaml_str(&text) {
            let _ = config.into_snapshot(SnapshotVersion(1));
        }
    }
}
