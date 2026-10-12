//! Passive outlier ejection through the proxy.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use ferry_integration_tests::{TestProxy, any_port, get};
use ferry_testkit::{Behavior, http as mock_http};

#[tokio::test]
async fn failing_endpoint_is_ejected_then_returns() {
    let broken = mock_http::spawn(any_port(), Behavior::named("broken").with_errors(1.0, 503))
        .await
        .unwrap();
    let healthy = mock_http::spawn(any_port(), Behavior::named("healthy"))
        .await
        .unwrap();
    let yaml = format!(
        r#"
listeners:
  - name: http
    address: 127.0.0.1:0
    routes:
      - name: all
        rules: [{{ backends: [{{ cluster: c }}] }}]
clusters:
  - name: c
    outlier_detection: {{ consecutive_5xx: 3, base_ejection_time: 1s, max_ejection_percent: 50 }}
    endpoints: [{{ address: "{}" }}, {{ address: "{}" }}]
"#,
        broken.addr(),
        healthy.addr()
    );
    let proxy = TestProxy::start(&yaml).await;

    for _ in 0..20 {
        get(proxy.addr, "/").await;
    }
    assert_eq!(
        broken.stats().requests(),
        3,
        "ejected after its third consecutive 503"
    );
    for _ in 0..10 {
        assert_eq!(get(proxy.addr, "/").await.status, 200);
    }
    assert_eq!(broken.stats().requests(), 3, "still ejected");

    // After the ejection (1 s, swept every 500 ms) it gets traffic again.
    tokio::time::sleep(Duration::from_millis(1600)).await;
    let before = broken.stats().requests();
    for _ in 0..4 {
        get(proxy.addr, "/").await;
    }
    assert!(broken.stats().requests() > before, "back in rotation");
}

#[tokio::test]
async fn connect_failures_count_as_gateway_failures() {
    let healthy = mock_http::spawn(any_port(), Behavior::named("healthy"))
        .await
        .unwrap();
    let dead = ferry_integration_tests::dead_addr();
    let yaml = format!(
        r#"
listeners:
  - name: http
    address: 127.0.0.1:0
    routes:
      - name: all
        rules: [{{ backends: [{{ cluster: c }}] }}]
clusters:
  - name: c
    outlier_detection: {{ consecutive_gateway_failure: 1, base_ejection_time: 1m, max_ejection_percent: 50 }}
    endpoints: [{{ address: "{dead}" }}, {{ address: "{}" }}]
"#,
        healthy.addr()
    );
    let proxy = TestProxy::start(&yaml).await;
    // The first request to the dead endpoint fails over (safe retry) and
    // ejects it; afterwards it is never tried, so requests stay fast.
    for _ in 0..4 {
        assert_eq!(get(proxy.addr, "/").await.status, 200);
    }
    let started = std::time::Instant::now();
    for _ in 0..10 {
        assert_eq!(get(proxy.addr, "/").await.status, 200);
    }
    // On Windows a refused connect takes ~2 s, so any attempt on the dead
    // endpoint would show up here.
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
}
