//! Property tests for configuration validation and path matching.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use ferry_config::{
    BackendRefConfig, ClusterConfig, Config, EndpointConfig, LbPolicy, ListenerConfig, MatchConfig,
    PathMatch, PathMatchConfig, PathMatchKind, RouteConfig, RuleConfig, SnapshotVersion,
    from_yaml_str,
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
            .map(|c| ClusterConfig {
                name: format!("c{c}"),
                lb_policy: LbPolicy::RoundRobin,
                endpoints: (0..n_eps)
                    .map(|e| EndpointConfig {
                        address: format!("10.0.{c}.{}:8080", e + 1)
                            .parse()
                            .expect("static address"),
                    })
                    .collect(),
            })
            .collect::<Vec<_>>();
        prop::collection::vec(
            (path(), prop::bool::ANY, 0..n_clusters, 0u32..1000),
            n_routes,
        )
        .prop_map(move |rules| Config {
            listeners: vec![ListenerConfig {
                name: "l".into(),
                address: "127.0.0.1:0".parse().expect("static address"),
                routes: rules
                    .into_iter()
                    .enumerate()
                    .map(|(i, (p, exact, cluster, weight))| RouteConfig {
                        name: format!("r{i}"),
                        hostnames: vec![],
                        rules: vec![RuleConfig {
                            matches: vec![MatchConfig {
                                path: Some(PathMatchConfig {
                                    kind: if exact {
                                        PathMatchKind::Exact
                                    } else {
                                        PathMatchKind::PathPrefix
                                    },
                                    value: p,
                                }),
                            }],
                            backends: vec![BackendRefConfig {
                                cluster: format!("c{cluster}"),
                                weight,
                            }],
                        }],
                    })
                    .collect(),
            }],
            clusters: clusters.clone(),
        })
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
    fn arbitrary_text_never_panics(text in ".{0,512}") {
        if let Ok(config) = from_yaml_str(&text) {
            let _ = config.into_snapshot(SnapshotVersion(1));
        }
    }
}
