//! Fixtures shared by Ferry's criterion benchmarks.

/// YAML for one listener with `n` routes, each on its own exact hostname with
/// three path rules, all pointing at one cluster. Approximates a mid-sized
/// multi-tenant gateway.
#[must_use]
pub fn route_table_yaml(n: usize) -> String {
    let mut yaml =
        String::from("listeners:\n  - name: http\n    address: 127.0.0.1:0\n    routes:\n");
    for i in 0..n {
        yaml.push_str(&format!(
            "      - name: r{i}\n        hostnames: [svc{i}.example.com]\n        rules:\n          \
             - matches: [{{ path: {{ type: Exact, value: /healthz }} }}]\n            backends: [{{ cluster: c }}]\n          \
             - matches: [{{ path: {{ value: /api/v1 }} }}]\n            backends: [{{ cluster: c, weight: 9 }}, {{ cluster: c, weight: 1 }}]\n          \
             - backends: [{{ cluster: c }}]\n"
        ));
    }
    yaml.push_str("clusters:\n  - name: c\n    endpoints: [{ address: 10.0.0.1:80 }, { address: 10.0.0.2:80 }]\n");
    yaml
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_is_valid() {
        let snap = ferry_config::from_yaml_str(&route_table_yaml(5))
            .and_then(|c| Ok(c.into_snapshot(ferry_config::SnapshotVersion(1))?));
        assert!(snap.is_ok(), "{snap:?}");
    }
}
