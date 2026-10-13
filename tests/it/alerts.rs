//! The alerting rules (M5): every metric a rule queries must be one Ferry
//! exports (proxy or controller), and every alert needs a severity, a
//! summary, a description and a runbook page that exists.
//!
//! promtool unit-tests the rules' behaviour (`bench/exit/m5/alerts-test.sh`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use ferry_config::Config;
use ferry_controller::metrics::{ControllerMetrics, ControllerStats};
use ferry_controller::server::Hub;
use ferry_controller::world::ObjKey;
use ferry_integration_tests::{TestProxy, any_port, get, single_cluster_yaml};
use ferry_testkit::{Behavior, http as mock_http};
use serde_json::Value;

fn repo(path: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(path)
}

/// Every alerting rule in the chart's rule file.
fn alerts() -> Vec<Value> {
    let text = std::fs::read_to_string(repo("deploy/helm/ferry/files/alerts.yaml")).unwrap();
    let doc: Value = serde_saphyr::from_str(&text).unwrap();
    let rules: Vec<Value> = doc["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|g| g["rules"].as_array().unwrap().clone())
        .collect();
    assert!(
        rules.len() >= 9,
        "expected the M5 alerts, got {}",
        rules.len()
    );
    rules
}

/// Sample names a scrape can contain, from the `# TYPE` lines.
fn exported(text: &str) -> HashSet<String> {
    let mut names = HashSet::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("# TYPE ") else {
            continue;
        };
        let mut parts = rest.split(' ');
        let (name, kind) = (parts.next().unwrap(), parts.next().unwrap());
        match kind {
            "counter" => {
                names.insert(format!("{name}_total"));
            }
            "histogram" => {
                for suffix in ["_bucket", "_sum", "_count"] {
                    names.insert(format!("{name}{suffix}"));
                }
            }
            _ => {
                names.insert(name.to_owned());
            }
        }
    }
    names
}

#[tokio::test(flavor = "multi_thread")]
async fn every_alert_metric_is_exported() {
    let b = mock_http::spawn(any_port(), Behavior::named("a"))
        .await
        .unwrap();
    let proxy = TestProxy::start(&single_cluster_yaml(&[b.addr()])).await;
    assert_eq!(get(proxy.addr, "/").await.status, 200);
    // As a proxy following a controller reports it.
    proxy
        .reload
        .telemetry()
        .metrics
        .proxy()
        .controller
        .set_connected(true);
    let mut names = exported(&proxy.metrics_text());

    let hub = Hub::new();
    hub.publish(
        &ObjKey::new("apps", "gw"),
        &Config::new(Vec::new(), Vec::new()),
    )
    .unwrap();
    let (_leader_tx, leader_rx) = tokio::sync::watch::channel(true);
    let controller = ControllerMetrics::new(
        hub,
        Arc::new(ControllerStats::default()),
        Arc::new(AtomicBool::new(true)),
        leader_rx,
    );
    names.extend(exported(&controller.encode().unwrap()));

    let metric = regex::Regex::new(r"\b((?:ferry|process)_[a-z0-9_]+)\b").unwrap();
    let mut missing = BTreeSet::new();
    for rule in alerts() {
        let expr = rule["expr"].as_str().unwrap();
        for m in metric.captures_iter(expr) {
            if !names.contains(&m[1]) {
                missing.insert(m[1].to_owned());
            }
        }
    }
    assert!(
        missing.is_empty(),
        "alerts query metrics Ferry does not export: {missing:?}"
    );
}

#[test]
fn every_alert_has_a_runbook_and_annotations() {
    let index = std::fs::read_to_string(repo("docs/runbooks/README.md")).unwrap();
    for rule in alerts() {
        let name = rule["alert"].as_str().expect("only alerting rules");
        let severity = rule["labels"]["severity"].as_str().unwrap_or_default();
        assert!(
            ["critical", "warning"].contains(&severity),
            "{name}: severity {severity:?}"
        );
        for key in ["summary", "description"] {
            assert!(
                rule["annotations"][key]
                    .as_str()
                    .is_some_and(|s| !s.is_empty()),
                "{name}: no {key}"
            );
        }
        let url = rule["annotations"]["runbook_url"].as_str().unwrap();
        let page = url
            .strip_prefix("RUNBOOK_BASE/")
            .unwrap_or_else(|| panic!("{name}: runbook_url {url} must start with RUNBOOK_BASE/"));
        let path = repo(&format!("docs/runbooks/{page}"));
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{name}: runbook {} missing: {e}", path.display()));
        assert!(text.contains(name), "{name}: not mentioned in {page}");
        assert!(
            index.contains(&format!("`{name}`")),
            "{name}: not listed in docs/runbooks/README.md"
        );
    }
}
