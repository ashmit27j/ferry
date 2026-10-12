//! M1 behaviour through an in-process Ferry: balancing policies, stickiness,
//! health checking, safe retries, HTTP/2 and gRPC upstreams, filters and
//! reloads under load.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashSet;
use std::net::SocketAddr;
use std::time::Duration;

use ferry_integration_tests::{
    TestProxy, any_port, dead_addr, eventually, get, request, run_load, send, snapshot,
};
use ferry_testkit::grpc::proto::EchoRequest;
use ferry_testkit::grpc::proto::echo_client::EchoClient;
use ferry_testkit::{Behavior, MockServer, grpc as mock_grpc, http as mock_http};
use tokio_stream::StreamExt;

async fn backends(names: &[&str]) -> Vec<MockServer> {
    let mut out = Vec::new();
    for n in names {
        out.push(
            mock_http::spawn(any_port(), Behavior::named(*n))
                .await
                .unwrap(),
        );
    }
    out
}

/// One catch-all route to cluster `c`, with extra cluster settings.
fn cluster_yaml(extra: &str, endpoints: &[(SocketAddr, u32)]) -> String {
    let eps: Vec<String> = endpoints
        .iter()
        .map(|(a, w)| format!("{{ address: \"{a}\", weight: {w} }}"))
        .collect();
    format!(
        r"
listeners:
  - name: http
    address: 127.0.0.1:0
    routes:
      - name: all
        rules:
          - backends: [{{ cluster: c }}]
clusters:
  - name: c
{extra}
    endpoints: [{}]
",
        eps.join(", ")
    )
}

fn weighted(mocks: &[MockServer]) -> Vec<(SocketAddr, u32)> {
    mocks.iter().map(|m| (m.addr(), 1)).collect()
}

async fn backend_of(addr: SocketAddr, headers: &[(&str, &str)]) -> String {
    let r = send(addr, "/", headers, false).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    r.backend()
}

#[tokio::test]
async fn weighted_round_robin_follows_endpoint_weights() {
    let m = backends(&["a", "b", "c"]).await;
    let eps = [(m[0].addr(), 1), (m[1].addr(), 2), (m[2].addr(), 3)];
    let proxy = TestProxy::start(&cluster_yaml("    lb_policy: weighted_round_robin", &eps)).await;
    for _ in 0..60 {
        get(proxy.addr, "/").await;
    }
    let counts: Vec<u64> = m.iter().map(|b| b.stats().requests()).collect();
    assert_eq!(counts, [10, 20, 30]);
}

#[tokio::test]
async fn least_request_and_p2c_ewma_use_every_backend() {
    for policy in ["least_request", "p2c_ewma"] {
        let m = backends(&["a", "b", "c"]).await;
        let proxy = TestProxy::start(&cluster_yaml(
            &format!("    lb_policy: {policy}"),
            &weighted(&m),
        ))
        .await;
        let report = run_load(
            proxy.addr,
            8,
            tokio::time::sleep(Duration::from_millis(500)),
        )
        .await;
        assert!(
            report.failures.is_empty(),
            "{policy}: {:?}",
            report.failures
        );
        for b in &m {
            assert!(
                b.stats().requests() > 0,
                "{policy}: a backend got nothing: {:?}",
                report.by_backend
            );
        }
    }
}

#[tokio::test]
async fn least_request_avoids_a_slow_backend() {
    let fast = mock_http::spawn(any_port(), Behavior::named("fast"))
        .await
        .unwrap();
    let slow = mock_http::spawn(
        any_port(),
        Behavior::named("slow").with_latency(Duration::from_millis(50)),
    )
    .await
    .unwrap();
    for policy in ["least_request", "p2c_ewma"] {
        let proxy = TestProxy::start(&cluster_yaml(
            &format!("    lb_policy: {policy}"),
            &[(fast.addr(), 1), (slow.addr(), 1)],
        ))
        .await;
        let report = run_load(
            proxy.addr,
            8,
            tokio::time::sleep(Duration::from_millis(600)),
        )
        .await;
        let fast_n = report.by_backend.get("fast").copied().unwrap_or(0);
        let slow_n = report.by_backend.get("slow").copied().unwrap_or(0);
        assert!(
            fast_n > 5 * slow_n,
            "{policy}: fast {fast_n} vs slow {slow_n}"
        );
    }
}

#[tokio::test]
async fn maglev_header_stickiness() {
    let m = backends(&["a", "b", "c", "d"]).await;
    let proxy = TestProxy::start(&cluster_yaml(
        "    lb_policy: maglev\n    hash_policy: { header: x-user }",
        &weighted(&m),
    ))
    .await;
    let mut seen = HashSet::new();
    for user in 0..40 {
        let user = user.to_string();
        let first = backend_of(proxy.addr, &[("x-user", &user)]).await;
        for _ in 0..3 {
            assert_eq!(backend_of(proxy.addr, &[("x-user", &user)]).await, first);
        }
        seen.insert(first);
    }
    assert!(seen.len() >= 3, "users should spread: {seen:?}");
}

#[tokio::test]
async fn maglev_cookie_is_generated_then_sticks() {
    let m = backends(&["a", "b", "c"]).await;
    let proxy = TestProxy::start(&cluster_yaml(
        "    lb_policy: maglev\n    hash_policy: { cookie: { name: ferry-sid, ttl: 1h } }",
        &weighted(&m),
    ))
    .await;
    let first = get(proxy.addr, "/").await;
    let set_cookie = first.headers["set-cookie"].to_str().unwrap().to_owned();
    assert!(set_cookie.contains("Max-Age=3600"), "{set_cookie}");
    let pair = set_cookie.split(';').next().unwrap().to_owned();
    let backend = first.backend();
    for _ in 0..10 {
        let r = send(proxy.addr, "/", &[("cookie", &pair)], false).await;
        assert_eq!(r.backend(), backend);
        assert!(r.headers.get("set-cookie").is_none());
    }
}

#[tokio::test]
async fn maglev_source_ip_stickiness() {
    let m = backends(&["a", "b", "c"]).await;
    let proxy = TestProxy::start(&cluster_yaml(
        "    lb_policy: maglev\n    hash_policy: source_ip",
        &weighted(&m),
    ))
    .await;
    let first = backend_of(proxy.addr, &[]).await;
    for _ in 0..10 {
        assert_eq!(backend_of(proxy.addr, &[]).await, first);
    }
}

const FAST_HC: &str = "    health_check: { interval: 100ms, timeout: 200ms, unhealthy_threshold: 1, healthy_threshold: 1, http: { path: /healthz } }";

#[tokio::test]
async fn unhealthy_backend_is_skipped_until_it_recovers() {
    let m = backends(&["a", "b"]).await;
    let proxy = TestProxy::start(&cluster_yaml(FAST_HC, &weighted(&m))).await;
    m[1].set_healthy(false);
    let reload = proxy.reload.clone();
    eventually(Duration::from_secs(3), || {
        reload.endpoint_health("c").unwrap()[1] == (m[1].addr(), false)
    })
    .await;
    let before = m[1].stats().requests();
    for _ in 0..10 {
        assert_eq!(backend_of(proxy.addr, &[]).await, "a");
    }
    assert_eq!(m[1].stats().requests(), before);

    m[1].set_healthy(true);
    eventually(Duration::from_secs(3), || {
        reload.endpoint_health("c").unwrap()[1].1
    })
    .await;
    let mut seen = HashSet::new();
    for _ in 0..4 {
        seen.insert(backend_of(proxy.addr, &[]).await);
    }
    assert_eq!(seen.len(), 2);
}

#[tokio::test]
async fn grpc_health_check_marks_endpoint() {
    let g = mock_grpc::spawn(any_port(), Behavior::named("g"))
        .await
        .unwrap();
    let other = mock_http::spawn(any_port(), Behavior::named("h"))
        .await
        .unwrap();
    let yaml = cluster_yaml(
        "    health_check: { interval: 100ms, unhealthy_threshold: 1, healthy_threshold: 1, grpc: {} }",
        &[(g.addr(), 1), (other.addr(), 1)],
    );
    let proxy = TestProxy::start(&yaml).await;
    let reload = proxy.reload.clone();
    // The HTTP mock does not speak gRPC health, so it is marked unhealthy...
    eventually(Duration::from_secs(3), || {
        !reload.endpoint_health("c").unwrap()[1].1
    })
    .await;
    assert!(reload.endpoint_health("c").unwrap()[0].1);
    // ...and the gRPC mock follows its own health flag.
    g.set_healthy(false);
    eventually(Duration::from_secs(3), || {
        !reload.endpoint_health("c").unwrap()[0].1
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn killing_a_backend_under_load_causes_no_errors() {
    let m = backends(&["a", "b", "c"]).await;
    let addrs = weighted(&m);
    let proxy = TestProxy::start(&cluster_yaml(FAST_HC, &addrs)).await;
    let mut m = m.into_iter();
    let (a, b, victim) = (m.next().unwrap(), m.next().unwrap(), m.next().unwrap());
    let report = run_load(proxy.addr, 16, async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        victim.kill().await;
        tokio::time::sleep(Duration::from_millis(700)).await;
    })
    .await;
    assert!(
        report.failures.is_empty(),
        "{} failures: {:?}",
        report.failures.len(),
        &report.failures[..report.failures.len().min(5)]
    );
    assert!(report.ok > 200, "too little traffic: {}", report.ok);
    assert!(
        report.by_backend.get("c").copied().unwrap_or(0) > 0,
        "victim served before dying"
    );
    assert!(a.stats().requests() > 0 && b.stats().requests() > 0);
    assert!(
        !proxy.reload.endpoint_health("c").unwrap()[2].1,
        "victim marked unhealthy"
    );
}

#[tokio::test]
async fn connect_failure_is_retried_on_another_endpoint() {
    let live = mock_http::spawn(any_port(), Behavior::named("live"))
        .await
        .unwrap();
    // No health checks: only the retry keeps these requests from failing.
    let yaml = cluster_yaml("", &[(dead_addr(), 1), (live.addr(), 1)]);
    let proxy = TestProxy::start(&yaml).await;
    for _ in 0..4 {
        assert_eq!(backend_of(proxy.addr, &[]).await, "live");
    }
    // A POST with a body is retried too: the connect failed, so nothing was sent.
    let r = request(proxy.addr, "POST", "/submit", &[], "payload").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.echo()["body_bytes"], 7);
}

#[tokio::test]
async fn no_healthy_endpoint_with_zero_panic_threshold_is_503() {
    let m = backends(&["a"]).await;
    let yaml = cluster_yaml(&format!("    panic_threshold: 0\n{FAST_HC}"), &weighted(&m));
    let proxy = TestProxy::start(&yaml).await;
    m[0].set_healthy(false);
    let reload = proxy.reload.clone();
    eventually(Duration::from_secs(3), || {
        !reload.endpoint_health("c").unwrap()[0].1
    })
    .await;
    let r = get(proxy.addr, "/").await;
    assert_eq!(r.status, 503);
    assert_eq!(&r.body[..], b"no healthy upstream");
}

#[tokio::test]
async fn http2_upstream_carries_authority() {
    let m = backends(&["h2"]).await;
    let proxy = TestProxy::start(&cluster_yaml("    protocol: http2", &weighted(&m))).await;
    let r = send(proxy.addr, "/x?y=1", &[("host", "app.test")], false).await;
    assert_eq!(r.status, 200);
    let echo = r.echo();
    assert_eq!(echo["version"], "HTTP/2.0");
    assert_eq!(echo["path"], "/x");
    assert_eq!(echo["query"], "y=1");
    // hyper's h2 server exposes :authority as the URI authority, not a header;
    // the mock reports headers only, so Host must not leak as a header.
    assert!(echo["headers"].get("host").is_none(), "{echo}");
}

#[tokio::test]
async fn grpc_unary_and_streaming_through_ferry() {
    let g = mock_grpc::spawn(any_port(), Behavior::named("g"))
        .await
        .unwrap();
    let proxy = TestProxy::start(&cluster_yaml("    protocol: http2", &[(g.addr(), 1)])).await;
    let mut client = EchoClient::connect(format!("http://{}", proxy.addr))
        .await
        .unwrap();
    let unary = client
        .unary(EchoRequest {
            message: "hi".into(),
            count: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (unary.message.as_str(), unary.backend.as_str()),
        ("hi", "g")
    );
    let mut stream = client
        .server_stream(EchoRequest {
            message: "s".into(),
            count: 5,
        })
        .await
        .unwrap()
        .into_inner();
    let mut n = 0;
    while let Some(msg) = stream.next().await {
        assert_eq!(msg.unwrap().sequence, n);
        n += 1;
    }
    assert_eq!(n, 5);
}

#[tokio::test]
async fn grpc_error_status_survives_the_proxy() {
    let g = mock_grpc::spawn(any_port(), Behavior::named("g").with_errors(1.0, 503))
        .await
        .unwrap();
    let proxy = TestProxy::start(&cluster_yaml("    protocol: http2", &[(g.addr(), 1)])).await;
    let mut client = EchoClient::connect(format!("http://{}", proxy.addr))
        .await
        .unwrap();
    let err = client
        .unary(EchoRequest {
            message: "x".into(),
            count: 0,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unavailable);
}

fn filters_yaml(backend: SocketAddr) -> String {
    format!(
        r#"
listeners:
  - name: http
    address: 127.0.0.1:0
    routes:
      - name: legacy
        hostnames: [old.test]
        rules:
          - filters: [{{ type: RequestRedirect, scheme: https, hostname: new.test, status_code: 301 }}]
      - name: app
        rules:
          - matches: [{{ path: {{ value: /v1 }} }}]
            filters:
              - type: RequestHeaderModifier
                set: [{{ name: x-env, value: prod }}]
                add: [{{ name: x-tag, value: ferry }}]
                remove: [x-secret]
              - type: ResponseHeaderModifier
                set: [{{ name: x-served-by, value: ferry }}]
              - type: URLRewrite
                hostname: internal.test
                path: {{ type: ReplacePrefixMatch, replace_prefix_match: /v2 }}
            backends: [{{ cluster: c }}]
          - matches: [{{ headers: [{{ name: x-canary, value: "1" }}], method: GET }}]
            backends: [{{ cluster: c }}]
            filters: [{{ type: RequestHeaderModifier, set: [{{ name: x-route, value: canary }}] }}]
clusters:
  - {{ name: c, endpoints: [{{ address: "{backend}" }}] }}
"#
    )
}

#[tokio::test]
async fn redirect_filter_answers_without_a_backend() {
    let m = backends(&["a"]).await;
    let proxy = TestProxy::start(&filters_yaml(m[0].addr())).await;
    let r = send(proxy.addr, "/path?q=1", &[("host", "old.test:8080")], false).await;
    assert_eq!(r.status, 301);
    assert_eq!(r.headers["location"], "https://new.test/path?q=1");
    assert_eq!(m[0].stats().requests(), 0);
}

#[tokio::test]
async fn header_and_rewrite_filters_apply() {
    let m = backends(&["a"]).await;
    let proxy = TestProxy::start(&filters_yaml(m[0].addr())).await;
    let r = send(
        proxy.addr,
        "/v1/users?id=7",
        &[("x-env", "dev"), ("x-tag", "client"), ("x-secret", "s")],
        false,
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(r.headers["x-served-by"], "ferry");
    let echo = r.echo();
    assert_eq!(echo["path"], "/v2/users");
    assert_eq!(echo["query"], "id=7");
    let h = &echo["headers"];
    assert_eq!(h["host"], "internal.test");
    assert_eq!(h["x-env"], "prod");
    assert!(h.get("x-secret").is_none());
    // The mock joins repeated headers in arrival order.
    assert!(h["x-tag"].as_str().unwrap().contains("ferry"), "{h}");
}

#[tokio::test]
async fn header_and_method_matches_select_rules() {
    let m = backends(&["a"]).await;
    let proxy = TestProxy::start(&filters_yaml(m[0].addr())).await;
    let canary = send(proxy.addr, "/", &[("x-canary", "1")], false).await;
    assert_eq!(canary.echo()["headers"]["x-route"], "canary");
    let post = request(proxy.addr, "POST", "/", &[("x-canary", "1")], "").await;
    assert_eq!(post.status, 404, "method condition excludes POST");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reloading_under_load_drops_nothing() {
    let m = backends(&["a", "b", "c"]).await;
    let proxy = TestProxy::start(&cluster_yaml("", &weighted(&m))).await;
    let reload = proxy.reload.clone();
    let policies = [
        "round_robin",
        "least_request",
        "p2c_ewma",
        "weighted_round_robin",
    ];
    let addrs: Vec<SocketAddr> = m.iter().map(MockServer::addr).collect();
    let report = run_load(proxy.addr, 16, async {
        for i in 0..20u64 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            // Alternate endpoint sets and policies on every reload.
            let eps: Vec<(SocketAddr, u32)> = addrs
                .iter()
                .enumerate()
                .filter(|(j, _)| i % 3 != *j as u64)
                .map(|(_, a)| (*a, 1))
                .collect();
            let policy = policies[(i % 4) as usize];
            reload.swap(snapshot(
                &cluster_yaml(&format!("    lb_policy: {policy}"), &eps),
                i + 2,
            ));
        }
    })
    .await;
    assert!(
        report.failures.is_empty(),
        "{:?}",
        &report.failures[..report.failures.len().min(5)]
    );
    assert!(report.ok > 100);
    assert_eq!(proxy.reload.current_version().0, 21);
}

#[tokio::test]
async fn file_watcher_reloads_and_rejects() {
    let a = backends(&["a"]).await;
    let b = backends(&["b"]).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ferry.yaml");
    let yaml_a = cluster_yaml("", &weighted(&a));
    std::fs::write(&path, &yaml_a).unwrap();
    let proxy = TestProxy::start(&yaml_a).await;
    let mut watcher = ferry_proxy::watch::FileWatcher::new(path.clone(), proxy.reload.clone());
    assert_eq!(watcher.poll().await, ferry_proxy::watch::Poll::Unchanged);

    std::fs::write(&path, cluster_yaml("", &weighted(&b))).unwrap();
    assert!(matches!(
        watcher.poll().await,
        ferry_proxy::watch::Poll::Reloaded(_)
    ));
    assert_eq!(backend_of(proxy.addr, &[]).await, "b");

    std::fs::write(&path, "listeners: [").unwrap();
    assert!(matches!(
        watcher.poll().await,
        ferry_proxy::watch::Poll::Rejected(_)
    ));
    assert_eq!(watcher.poll().await, ferry_proxy::watch::Poll::Unchanged);
    assert_eq!(
        backend_of(proxy.addr, &[]).await,
        "b",
        "last good config keeps serving"
    );
}
