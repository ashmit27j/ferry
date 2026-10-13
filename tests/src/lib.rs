//! Shared harness for Ferry's in-process integration tests.
//!
//! [`TestProxy::start`] binds a proxy on an ephemeral port from YAML and runs it
//! on the current tokio runtime; [`send`] issues requests over HTTP/1.1 or h2c.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::Bytes;
use ferry_config::{Snapshot, SnapshotVersion};
use ferry_proxy::{Drain, Proxy, ProxyError, ReloadHandle};
use ferry_telemetry::Telemetry;
use http::{HeaderMap, Request, StatusCode};
use http_body_util::{BodyExt, Empty, Full};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// Parses and validates YAML into a snapshot, panicking with the errors on failure.
#[must_use]
pub fn snapshot(yaml: &str, version: u64) -> Snapshot {
    ferry_config::from_yaml_str(yaml)
        .unwrap_or_else(|e| panic!("yaml does not parse: {e}\n{yaml}"))
        .into_snapshot(SnapshotVersion(version))
        .unwrap_or_else(|e| panic!("yaml does not validate: {e}\n{yaml}"))
}

/// YAML for one catch-all route over one round-robin cluster.
#[must_use]
pub fn single_cluster_yaml(endpoints: &[SocketAddr]) -> String {
    let eps: Vec<String> = endpoints
        .iter()
        .map(|a| format!("{{ address: \"{a}\" }}"))
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
    endpoints: [{}]
",
        eps.join(", ")
    )
}

/// A proxy running in the background of the test's runtime.
#[derive(Debug)]
pub struct TestProxy {
    /// Address of the first listener.
    pub addr: SocketAddr,
    /// Handle for hot-swapping snapshots.
    pub reload: ReloadHandle,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), ProxyError>>>,
}

impl TestProxy {
    /// Binds and starts a proxy for `yaml` (listeners should use port 0).
    pub async fn start(yaml: &str) -> Self {
        Self::start_with_drain(yaml, Duration::from_secs(5)).await
    }

    /// Like [`TestProxy::start`] with a custom drain timeout.
    pub async fn start_with_drain(yaml: &str, timeout: Duration) -> Self {
        Self::start_with(
            yaml,
            Drain {
                delay: Duration::ZERO,
                timeout,
            },
        )
        .await
    }

    /// Like [`TestProxy::start`] with full drain settings.
    pub async fn start_with(yaml: &str, drain: Drain) -> Self {
        Self::start_full(yaml, drain, Arc::new(Telemetry::default())).await
    }

    /// Like [`TestProxy::start`], reporting through `telemetry`.
    pub async fn start_with_telemetry(yaml: &str, telemetry: Arc<Telemetry>) -> Self {
        let drain = Drain {
            delay: Duration::ZERO,
            timeout: Duration::from_secs(5),
        };
        Self::start_full(yaml, drain, telemetry).await
    }

    /// Binds and starts a proxy with every setting explicit.
    pub async fn start_full(yaml: &str, drain: Drain, telemetry: Arc<Telemetry>) -> Self {
        let proxy = Proxy::bind_with(snapshot(yaml, 1), telemetry)
            .await
            .expect("proxy binds");
        let addr = proxy.local_addrs()[0];
        let reload = proxy.reload_handle();
        let (tx, rx) = oneshot::channel();
        let task = tokio::spawn(proxy.run(
            async {
                let _ = rx.await;
            },
            drain,
        ));
        Self {
            addr,
            reload,
            shutdown: Some(tx),
            task: Some(task),
        }
    }

    /// Serves the admin API on an ephemeral port until the test ends.
    pub async fn serve_admin(&self) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind(any_port()).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(ferry_proxy::admin::serve(
            listener,
            self.reload.clone(),
            std::future::pending(),
        ));
        addr
    }

    /// The proxy's metrics in the exposition format.
    #[must_use]
    pub fn metrics_text(&self) -> String {
        self.reload.telemetry().metrics.encode().unwrap()
    }

    /// Starts draining without waiting for it to finish.
    pub fn begin_shutdown(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }

    /// Drains and waits for the proxy to stop.
    pub async fn shutdown(mut self) {
        self.begin_shutdown();
        if let Some(task) = self.task.take() {
            task.await
                .expect("proxy task joins")
                .expect("proxy exits cleanly");
        }
    }
}

impl Drop for TestProxy {
    fn drop(&mut self) {
        self.begin_shutdown();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// A response with its body collected.
#[derive(Debug)]
pub struct Reply {
    /// Status code.
    pub status: StatusCode,
    /// Response headers.
    pub headers: HeaderMap,
    /// Response body.
    pub body: Bytes,
}

impl Reply {
    /// Parses the body as the mock backend's JSON echo.
    #[must_use]
    pub fn echo(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("not a JSON echo ({e}): {:?}", self.body))
    }

    /// Name of the mock backend that answered.
    #[must_use]
    pub fn backend(&self) -> String {
        self.echo()["backend"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }
}

/// Sends a GET through the proxy. `h2` selects prior-knowledge HTTP/2.
pub async fn send(addr: SocketAddr, path: &str, headers: &[(&str, &str)], h2: bool) -> Reply {
    let client = Client::builder(TokioExecutor::new())
        .http2_only(h2)
        .build_http::<Empty<Bytes>>();
    let mut req = Request::get(format!("http://{addr}{path}"));
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = client
        .request(req.body(Empty::new()).unwrap())
        .await
        .expect("request reaches proxy");
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        headers,
        body,
    }
}

/// HTTP/1.1 GET with no extra headers.
pub async fn get(addr: SocketAddr, path: &str) -> Reply {
    send(addr, path, &[], false).await
}

/// An address with nothing listening on it.
#[must_use]
pub fn dead_addr() -> SocketAddr {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap()
}

/// `127.0.0.1:0`.
#[must_use]
pub fn any_port() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

/// Sends a request with any method and body over HTTP/1.1.
pub async fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &'static str,
) -> Reply {
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let mut req = Request::builder()
        .method(method)
        .uri(format!("http://{addr}{path}"));
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = client
        .request(
            req.body(Full::new(Bytes::from_static(body.as_bytes())))
                .unwrap(),
        )
        .await
        .expect("request reaches proxy");
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        headers,
        body,
    }
}

/// What a load run saw.
#[derive(Debug, Default)]
pub struct LoadReport {
    /// Requests answered 200.
    pub ok: u64,
    /// Every other outcome: a status code or a client error.
    pub failures: Vec<String>,
    /// 200 responses per backend name (from the mock's response header).
    pub by_backend: HashMap<String, u64>,
}

/// Runs `concurrency` keep-alive clients sending `GET /` in a loop until
/// `stop` resolves, then reports what they saw.
pub async fn run_load(
    addr: SocketAddr,
    concurrency: usize,
    stop: impl std::future::Future<Output = ()>,
) -> LoadReport {
    let running = Arc::new(AtomicBool::new(true));
    let mut tasks = Vec::new();
    for _ in 0..concurrency {
        let running = Arc::clone(&running);
        tasks.push(tokio::spawn(async move {
            let client = Client::builder(TokioExecutor::new()).build_http::<Empty<Bytes>>();
            let mut report = LoadReport::default();
            while running.load(Ordering::Relaxed) {
                let req = Request::get(format!("http://{addr}/"))
                    .body(Empty::new())
                    .unwrap();
                match client.request(req).await {
                    Ok(resp) if resp.status() == StatusCode::OK => {
                        let backend = resp
                            .headers()
                            .get(ferry_testkit::http::BACKEND_HEADER)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("?")
                            .to_owned();
                        let _ = resp.into_body().collect().await;
                        report.ok += 1;
                        *report.by_backend.entry(backend).or_default() += 1;
                    }
                    Ok(resp) => {
                        let status = resp.status();
                        let body = resp.into_body().collect().await.map(|b| b.to_bytes());
                        report.failures.push(format!("{status}: {body:?}"));
                    }
                    Err(e) => report.failures.push(format!("client error: {e:?}")),
                }
            }
            report
        }));
    }
    stop.await;
    running.store(false, Ordering::Relaxed);
    let mut total = LoadReport::default();
    for t in tasks {
        let r = t.await.unwrap();
        total.ok += r.ok;
        total.failures.extend(r.failures);
        for (k, v) in r.by_backend {
            *total.by_backend.entry(k).or_default() += v;
        }
    }
    total
}

/// The value of one series (`name{labels}` exactly as exposed) in metrics
/// text, if present.
#[must_use]
pub fn metric(text: &str, series: &str) -> Option<f64> {
    text.lines()
        .find_map(|l| l.strip_prefix(series)?.strip_prefix(' ')?.parse().ok())
}

/// Polls `cond` every 10 ms until it holds, panicking after `timeout`.
pub async fn eventually(timeout: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + timeout;
    while !cond() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "condition not met within {timeout:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A layer-4 load balancer for tests, standing in for an NLB: each new TCP
/// connection goes to the next registered target, round robin. Deregistering
/// a target only affects new connections, as with a real NLB.
#[derive(Debug)]
pub struct TcpBalancer {
    /// Address clients connect to.
    pub addr: SocketAddr,
    targets: Arc<std::sync::Mutex<Vec<SocketAddr>>>,
    task: JoinHandle<()>,
}

impl TcpBalancer {
    /// Starts balancing across `targets`.
    pub async fn start(targets: Vec<SocketAddr>) -> Self {
        let listener = tokio::net::TcpListener::bind(any_port()).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let targets = Arc::new(std::sync::Mutex::new(targets));
        let shared = Arc::clone(&targets);
        let task = tokio::spawn(async move {
            let mut next = 0usize;
            loop {
                let Ok((mut client, _)) = listener.accept().await else {
                    continue;
                };
                let target = {
                    let t = shared.lock().unwrap();
                    if t.is_empty() {
                        continue;
                    }
                    next = next.wrapping_add(1);
                    t[next % t.len()]
                };
                tokio::spawn(async move {
                    let _ = client.set_nodelay(true);
                    if let Ok(mut upstream) = tokio::net::TcpStream::connect(target).await {
                        let _ = upstream.set_nodelay(true);
                        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                    }
                });
            }
        });
        Self {
            addr,
            targets,
            task,
        }
    }

    /// Replaces the targets for new connections.
    pub fn set_targets(&self, targets: Vec<SocketAddr>) {
        *self.targets.lock().unwrap() = targets;
    }
}

impl Drop for TcpBalancer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
