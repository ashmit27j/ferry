//! Graceful drain and rolling restarts behind a layer-4 load balancer.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use ferry_integration_tests::{TcpBalancer, TestProxy, any_port, run_load, single_cluster_yaml};
use ferry_proxy::Drain;
use ferry_testkit::{Behavior, http as mock_http};

const DRAIN: Drain = Drain {
    delay: Duration::from_millis(400),
    timeout: Duration::from_secs(5),
};

/// The M2 exit criterion: replace every proxy replica, one at a time, under
/// steady load, and lose no request.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rolling_restart_under_load_drops_nothing() {
    let backend = mock_http::spawn(any_port(), Behavior::named("app"))
        .await
        .unwrap();
    let yaml = single_cluster_yaml(&[backend.addr()]);
    let mut replicas = vec![
        TestProxy::start_with(&yaml, DRAIN).await,
        TestProxy::start_with(&yaml, DRAIN).await,
    ];
    let lb = TcpBalancer::start(replicas.iter().map(|p| p.addr).collect()).await;

    let report = run_load(lb.addr, 16, async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        for i in 0..replicas.len() {
            // Kubernetes sends SIGTERM and removes the pod from the endpoints
            // at the same time; the NLB stops sending it new connections a
            // little later. The drain delay covers that gap.
            let mut old = replicas.remove(i);
            old.begin_shutdown();
            tokio::time::sleep(Duration::from_millis(100)).await;
            let mut others: Vec<_> = replicas.iter().map(|p| p.addr).collect();
            lb.set_targets(others.clone());
            old.shutdown().await;

            let fresh = TestProxy::start_with(&yaml, DRAIN).await;
            others.push(fresh.addr);
            lb.set_targets(others);
            replicas.insert(i, fresh);
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    })
    .await;

    assert!(
        report.failures.is_empty(),
        "{} failures, e.g. {:?}",
        report.failures.len(),
        &report.failures[..report.failures.len().min(3)]
    );
    assert!(report.ok > 200, "too little traffic: {}", report.ok);
}

#[tokio::test]
async fn draining_closes_http1_connections_after_each_response() {
    let backend = mock_http::spawn(any_port(), Behavior::named("app"))
        .await
        .unwrap();
    let mut proxy = TestProxy::start_with(
        &single_cluster_yaml(&[backend.addr()]),
        Drain {
            delay: Duration::from_secs(2),
            timeout: Duration::from_secs(5),
        },
    )
    .await;
    let before = ferry_integration_tests::get(proxy.addr, "/").await;
    assert!(before.headers.get("connection").is_none());

    proxy.begin_shutdown();
    let reload = proxy.reload.clone();
    ferry_integration_tests::eventually(Duration::from_secs(1), || reload.is_draining()).await;
    // Still accepting during the delay, but every response says goodbye.
    let during = ferry_integration_tests::get(proxy.addr, "/").await;
    assert_eq!(during.status, 200);
    assert_eq!(during.headers["connection"], "close");
}
