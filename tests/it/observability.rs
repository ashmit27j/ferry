//! Metrics, access logs and the admin API (M3).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use ferry_integration_tests::{TestProxy, any_port, eventually, get, metric, send, snapshot};
use ferry_proxy::Drain;
use ferry_telemetry::access_log::{self, AccessLog, Sink};
use ferry_telemetry::{Metrics, Telemetry};
use ferry_testkit::grpc::proto::EchoRequest;
use ferry_testkit::grpc::proto::echo_client::EchoClient;
use ferry_testkit::{Behavior, grpc as mock_grpc, http as mock_http};
use http_body_util::Empty;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

fn yaml(live: SocketAddr) -> String {
    format!(
        r#"
listeners:
  - name: web
    address: 127.0.0.1:0
    routes:
      - name: api
        rules:
          - matches: [{{ path: {{ type: PathPrefix, value: /api }} }}]
            backends: [{{ cluster: live }}]
      - name: limited
        rate_limit: {{ requests: 1, per: 1m }}
        rules:
          - matches: [{{ path: {{ type: PathPrefix, value: /limited }} }}]
            backends: [{{ cluster: live }}]
clusters:
  - name: live
    endpoints: [{{ address: "{live}" }}]
"#
    )
}

/// Telemetry with an access log written to a file in `dir`.
fn logged(dir: &tempfile::TempDir) -> (Arc<Telemetry>, std::path::PathBuf) {
    let path = dir.path().join("access.log");
    let metrics = Arc::new(Metrics::new());
    let log = AccessLog::spawn(
        &Sink::File(path.clone()),
        access_log::DEFAULT_CAPACITY,
        Arc::clone(&metrics.proxy().access_log_dropped),
    )
    .unwrap();
    let t = Telemetry {
        metrics,
        tracing: None,
        access_log: Some(log),
    };
    (Arc::new(t), path)
}

/// The access log's lines, once there are `n`. Lines are written when a
/// response body is dropped, which can be just after the client has it.
async fn log_lines(t: &Telemetry, path: &std::path::Path, n: usize) -> Vec<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        assert!(t.access_log.as_ref().unwrap().flush(Duration::from_secs(5)));
        let lines: Vec<serde_json::Value> = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        if lines.len() >= n || tokio::time::Instant::now() > deadline {
            return lines;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn red_metrics_and_access_logs_cover_every_outcome() {
    let b = mock_http::spawn(any_port(), Behavior::named("a"))
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (telemetry, log_path) = logged(&dir);
    let proxy = TestProxy::start_with_telemetry(&yaml(b.addr()), Arc::clone(&telemetry)).await;

    for _ in 0..3 {
        assert_eq!(get(proxy.addr, "/api/x").await.status, 200);
    }
    let failed = send(
        proxy.addr,
        "/api/x",
        &[(mock_http::STATUS_HEADER, "503")],
        false,
    )
    .await;
    assert_eq!(failed.status, 503);
    for _ in 0..2 {
        assert_eq!(get(proxy.addr, "/nothing").await.status, 404);
    }
    assert_eq!(get(proxy.addr, "/limited").await.status, 200);
    assert_eq!(get(proxy.addr, "/limited").await.status, 429);

    let lines = log_lines(&telemetry, &log_path, 8).await;
    let text = proxy.metrics_text();
    let m =
        |series: &str| metric(&text, series).unwrap_or_else(|| panic!("{series} missing:\n{text}"));
    assert_eq!(
        m(r#"ferry_requests_total{listener="web",route="api",code_class="2xx"}"#),
        3.0
    );
    assert_eq!(
        m(r#"ferry_requests_total{listener="web",route="api",code_class="5xx"}"#),
        1.0
    );
    assert_eq!(
        m(r#"ferry_requests_total{listener="web",route="-",code_class="4xx"}"#),
        2.0
    );
    assert_eq!(
        m(r#"ferry_requests_total{listener="web",route="limited",code_class="4xx"}"#),
        1.0
    );
    assert_eq!(
        m(r#"ferry_request_duration_seconds_count{listener="web",route="api"}"#),
        4.0
    );
    assert_eq!(
        m(r#"ferry_local_replies_total{listener="web",reason="no_route"}"#),
        2.0
    );
    assert_eq!(
        m(r#"ferry_local_replies_total{listener="web",reason="rate_limited"}"#),
        1.0
    );
    assert_eq!(
        m(r#"ferry_upstream_responses_total{cluster="live",code_class="2xx"}"#),
        4.0
    );
    assert_eq!(
        m(r#"ferry_upstream_responses_total{cluster="live",code_class="5xx"}"#),
        1.0
    );
    assert_eq!(
        m(r#"ferry_request_overhead_seconds_count{listener="web"}"#),
        8.0
    );
    assert_eq!(
        m(r#"ferry_downstream_connections_total{listener="web"}"#),
        8.0
    );
    let endpoint = b.addr().to_string();
    assert_eq!(
        m(&format!(
            r#"ferry_endpoint_healthy{{cluster="live",endpoint="{endpoint}"}}"#
        )),
        1.0
    );
    assert_eq!(m("ferry_config_version"), 1.0);

    assert_eq!(lines.len(), 8, "one line per request");
    let flags: Vec<&str> = lines
        .iter()
        .map(|l| l["response_flags"].as_str().unwrap())
        .collect();
    assert_eq!(flags, ["-", "-", "-", "-", "NR", "NR", "-", "RL"]);
    let first = &lines[0];
    assert_eq!(first["listener"], "web");
    assert_eq!(first["route"], "api");
    assert_eq!(first["upstream_cluster"], "live");
    assert_eq!(first["upstream_host"], endpoint);
    assert_eq!(first["attempts"], 1);
    assert_eq!(first["method"], "GET");
    assert_eq!(first["protocol"], "HTTP/1.1");
    assert!(first["bytes_sent"].as_u64().unwrap() > 0);
    assert_eq!(first["request_id"].as_str().unwrap().len(), 36);
    assert!(
        first["upstream_service_time_ms"].as_f64().unwrap()
            <= first["duration_ms"].as_f64().unwrap()
    );
    assert_eq!(lines[4]["route"], "-");
    assert!(lines[4].get("upstream_cluster").is_none());

    // A reload is counted and the version moves on; series survive it.
    proxy.reload.swap(snapshot(&yaml(b.addr()), 2)).unwrap();
    let text = proxy.metrics_text();
    assert_eq!(metric(&text, "ferry_config_version"), Some(2.0));
    assert_eq!(
        metric(&text, r#"ferry_config_reloads_total{result="success"}"#),
        Some(1.0)
    );
    assert_eq!(
        metric(
            &text,
            r#"ferry_requests_total{listener="web",route="api",code_class="2xx"}"#
        ),
        Some(3.0)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn removed_routes_and_clusters_stop_being_exported() {
    let b = mock_http::spawn(any_port(), Behavior::named("a"))
        .await
        .unwrap();
    let proxy = TestProxy::start(&yaml(b.addr())).await;
    assert_eq!(get(proxy.addr, "/api").await.status, 200);
    let renamed = yaml(b.addr())
        .replace("name: live", "name: fresh")
        .replace("cluster: live", "cluster: fresh");
    proxy.reload.swap(snapshot(&renamed, 2)).unwrap();
    let text = proxy.metrics_text();
    assert!(!text.contains(r#"cluster="live""#), "{text}");
    assert!(text.contains(r#"cluster="fresh""#), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn added_latency_excludes_time_spent_upstream() {
    let b = mock_http::spawn(
        any_port(),
        Behavior::named("slow").with_latency(Duration::from_millis(60)),
    )
    .await
    .unwrap();
    let telemetry = Arc::new(Telemetry::default());
    let proxy = TestProxy::start_with_telemetry(&yaml(b.addr()), Arc::clone(&telemetry)).await;
    for _ in 0..10 {
        assert_eq!(get(proxy.addr, "/api").await.status, 200);
    }
    let listener = telemetry.metrics.listener("web");
    let route = telemetry.metrics.route("web", "api");
    let overhead = listener.overhead.quantile_bound(0.99).unwrap();
    let total = route.duration.quantile_bound(0.5).unwrap();
    assert!(total >= 0.05, "requests took the backend's 60 ms: {total}");
    // Generous for debug builds on slow CI; the point is that the backend's
    // 60 ms is not counted.
    assert!(overhead <= 0.025, "added latency p99 bucket {overhead}");
}

#[tokio::test(flavor = "multi_thread")]
async fn abandoned_responses_are_flagged() {
    let b = mock_http::spawn(any_port(), Behavior::named("a"))
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (telemetry, log_path) = logged(&dir);
    let proxy = TestProxy::start_with_telemetry(&yaml(b.addr()), Arc::clone(&telemetry)).await;

    let client = Client::builder(TokioExecutor::new()).build_http::<Empty<Bytes>>();
    let req = http::Request::get(format!("http://{}/api", proxy.addr))
        .header(mock_http::BODY_DELAY_HEADER, "5000")
        .body(Empty::new())
        .unwrap();
    let resp = client.request(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    drop(resp); // hang up before the body arrives
    drop(client);

    let stats = telemetry.metrics.listener("web");
    eventually(Duration::from_secs(5), || stats.incomplete.get() == 1).await;
    let lines = log_lines(&telemetry, &log_path, 1).await;
    assert_eq!(lines[0]["response_flags"], "DC");
}

#[tokio::test(flavor = "multi_thread")]
async fn grpc_status_codes_are_counted() {
    let g = mock_grpc::spawn(any_port(), Behavior::named("g"))
        .await
        .unwrap();
    let yaml = format!(
        r#"
listeners:
  - name: grpc
    address: 127.0.0.1:0
    routes:
      - name: echo
        rules: [{{ backends: [{{ cluster: echo }}] }}]
clusters:
  - name: echo
    protocol: http2
    endpoints: [{{ address: "{}" }}]
"#,
        g.addr()
    );
    let proxy = TestProxy::start(&yaml).await;
    let mut client = EchoClient::connect(format!("http://{}", proxy.addr))
        .await
        .unwrap();
    for _ in 0..2 {
        client
            .unary(EchoRequest {
                message: "hi".into(),
                count: 0,
            })
            .await
            .unwrap();
    }
    let text = proxy.metrics_text();
    assert_eq!(
        metric(
            &text,
            r#"ferry_grpc_responses_total{listener="grpc",route="echo",grpc_code="OK"}"#
        ),
        Some(2.0),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn admin_api_reports_readiness_state_and_metrics() {
    let b = mock_http::spawn(any_port(), Behavior::named("a"))
        .await
        .unwrap();
    let mut proxy = TestProxy::start_full(
        &yaml(b.addr()),
        Drain {
            delay: Duration::from_secs(3),
            timeout: Duration::from_secs(5),
        },
        Arc::new(Telemetry::default()),
    )
    .await;
    let admin = proxy.serve_admin().await;

    let live = get(admin, "/live").await;
    assert_eq!(
        (live.status.as_u16(), &live.body[..]),
        (200, &b"live\n"[..])
    );
    assert_eq!(get(admin, "/ready").await.status, 200);

    let metrics = get(admin, "/metrics").await;
    assert_eq!(
        metrics.headers["content-type"],
        ferry_telemetry::metrics::CONTENT_TYPE
    );
    let text = String::from_utf8(metrics.body.to_vec()).unwrap();
    assert!(text.contains("# TYPE ferry_requests counter"), "{text}");
    assert!(text.ends_with("# EOF\n"));

    let config: serde_json::Value = get(admin, "/config").await.echo();
    assert_eq!(config["version"], 1);
    assert_eq!(config["clusters"][0]["name"], "live");

    let clusters: serde_json::Value = get(admin, "/clusters").await.echo();
    let c = &clusters["clusters"][0];
    assert_eq!(c["name"], "live");
    assert_eq!(c["eligible_endpoints"], 1);
    assert_eq!(c["endpoints"][0]["address"], b.addr().to_string());
    assert_eq!(c["endpoints"][0]["healthy"], true);
    assert_eq!(c["circuit_breakers"][0]["name"], "connections");

    assert_eq!(get(admin, "/nope").await.status, 404);

    // Draining: still serving traffic during the delay, but not ready.
    proxy.begin_shutdown();
    let mut ready = 200;
    for _ in 0..100 {
        ready = get(admin, "/ready").await.status.as_u16();
        if ready == 503 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(ready, 503);
    assert_eq!(get(proxy.addr, "/api").await.status, 200);
    let text = String::from_utf8(get(admin, "/metrics").await.body.to_vec()).unwrap();
    assert_eq!(metric(&text, "ferry_draining"), Some(1.0));
}

#[tokio::test(flavor = "multi_thread")]
async fn config_dump_redacts_private_keys() {
    let b = mock_http::spawn(any_port(), Behavior::named("a"))
        .await
        .unwrap();
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let key = ck.signing_key.serialize_pem();
    let tls = format!(
        "{{ certificates: [{{ cert_pem: {:?}, key_pem: {:?} }}] }}",
        ck.cert.pem(),
        key
    );
    let yaml = yaml(b.addr()).replace(
        "    address: 127.0.0.1:0
",
        &format!(
            "    address: 127.0.0.1:0
    tls: {tls}
"
        ),
    );
    let proxy = TestProxy::start(&yaml).await;
    let admin = proxy.serve_admin().await;
    let text = String::from_utf8(get(admin, "/config").await.body.to_vec()).unwrap();
    assert!(text.contains("<redacted>"), "{text}");
    assert!(!text.contains("PRIVATE KEY"), "{text}");
    assert!(
        text.contains("BEGIN CERTIFICATE"),
        "certificates are public"
    );
}
