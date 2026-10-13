//! The Grafana dashboard (M3 exit criterion: every SLO can be read from it).
//!
//! Checks the shipped JSON against a running proxy: every metric any panel,
//! annotation or variable queries must be one Ferry exports, and the SLO row
//! must have a panel for each SLO in the plan.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;

use ferry_integration_tests::{TestProxy, any_port, get, single_cluster_yaml};
use ferry_testkit::{Behavior, http as mock_http};

fn dashboard() -> serde_json::Value {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../deploy/grafana/ferry-dashboard.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Every PromQL string in the dashboard.
fn queries(d: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    for p in d["panels"].as_array().unwrap() {
        for t in p["targets"].as_array().into_iter().flatten() {
            out.push(t["expr"].as_str().unwrap().to_owned());
        }
    }
    for a in d["annotations"]["list"].as_array().unwrap() {
        if let Some(e) = a["expr"].as_str() {
            out.push(e.to_owned());
        }
    }
    for v in d["templating"]["list"].as_array().unwrap() {
        if let Some(q) = v["query"]["query"].as_str() {
            out.push(q.to_owned());
        }
    }
    out
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
async fn every_dashboard_metric_is_exported() {
    let b = mock_http::spawn(any_port(), Behavior::named("a"))
        .await
        .unwrap();
    let proxy = TestProxy::start(&single_cluster_yaml(&[b.addr()])).await;
    assert_eq!(get(proxy.addr, "/").await.status, 200);
    let names = exported(&proxy.metrics_text());

    let metric = regex::Regex::new(r"\b((?:ferry|process)_[a-z0-9_]+)\b").unwrap();
    let mut missing = BTreeSet::new();
    let d = dashboard();
    for q in queries(&d) {
        for m in metric.captures_iter(&q) {
            let name = &m[1];
            // procfs metrics exist only on Linux, the deployment target.
            let linux_only = name.starts_with("process_") && name != "process_start_time_seconds";
            if !names.contains(name) && !(linux_only && !cfg!(target_os = "linux")) {
                missing.insert(name.to_owned());
            }
        }
    }
    assert!(
        missing.is_empty(),
        "dashboard queries metrics Ferry does not export: {missing:?}"
    );
}

#[test]
fn slo_row_covers_every_slo() {
    let d = dashboard();
    let panels = d["panels"].as_array().unwrap();
    let slo_row = panels
        .iter()
        .position(|p| p["type"] == "row" && p["title"] == "Service level objectives")
        .expect("SLO row");
    let next_row = panels
        .iter()
        .skip(slo_row + 1)
        .position(|p| p["type"] == "row")
        .map_or(panels.len(), |i| i + slo_row + 1);
    let slo: Vec<&serde_json::Value> = panels[slo_row + 1..next_row].iter().collect();
    let has = |needle: &str| {
        slo.iter().any(|p| {
            p["title"].as_str().unwrap().contains(needle)
                && p["targets"].as_array().is_some_and(|t| !t.is_empty())
        })
    };
    // Section 5 of the plan.
    assert!(has("Added latency p50"), "p50 under 0.2 ms");
    assert!(has("Added latency p99"), "p99 under 1 ms");
    assert!(has("Requests per core-second"), "at 10k req/s per core");
    assert!(has("Success ratio"), "99.95% succeed");
    assert!(
        has("Failed or cut-off"),
        "zero drops during reloads and restarts"
    );
    assert!(has("Reloads, restarts"), "reload and restart timeline");
    assert!(has("Resident memory"), "flat memory");

    // Thresholds on the stat panels match the SLO values.
    let threshold = |title: &str| {
        let p = slo.iter().find(|p| p["title"] == title).unwrap();
        p["fieldConfig"]["defaults"]["thresholds"]["steps"][1]["value"]
            .as_f64()
            .unwrap()
    };
    assert!((threshold("Added latency p50") - 0.0002).abs() < 1e-12);
    assert!((threshold("Added latency p99") - 0.001).abs() < 1e-12);
    assert!((threshold("Success ratio") - 0.9995).abs() < 1e-12);
}

#[test]
fn panels_are_well_formed() {
    let d = dashboard();
    assert_eq!(d["uid"], "ferry-overview");
    let mut ids = HashSet::new();
    for p in d["panels"].as_array().unwrap() {
        assert!(ids.insert(p["id"].as_u64().unwrap()), "duplicate panel id");
        let g = &p["gridPos"];
        assert!(
            g["x"].as_u64().unwrap() + g["w"].as_u64().unwrap() <= 24,
            "{p}"
        );
        if p["type"] != "row" {
            assert_eq!(p["datasource"]["uid"], "${datasource}");
            assert!(!p["description"].as_str().unwrap().is_empty());
        }
    }
}
