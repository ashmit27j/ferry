//! End-to-end tests of the real `ferry` binary.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use ferry_testkit::{Behavior, http as mock_http};
use http_body_util::{BodyExt, Empty};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

const FERRY: &str = env!("CARGO_BIN_EXE_ferry");

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn ferry(args: &[&str]) -> std::process::Output {
    Command::new(FERRY)
        .args(args)
        .env("FERRY_LOG", "warn")
        .output()
        .expect("ferry runs")
}

#[test]
fn check_config_accepts_hello_example() {
    let path = repo_root().join("examples/hello.yaml");
    let out = ferry(&["check-config", path.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("OK (1 listener(s), 1 route(s), 1 cluster(s))"),
        "{stdout}"
    );
}

#[test]
fn check_config_accepts_every_example() {
    for entry in std::fs::read_dir(repo_root().join("examples")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "yaml") {
            let out = ferry(&["check-config", path.to_str().unwrap()]);
            assert!(
                out.status.success(),
                "{}: {}",
                path.display(),
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}

#[test]
fn check_config_reports_every_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.yaml");
    std::fs::write(
        &path,
        "listeners:\n  - name: http\n    address: 127.0.0.1:8080\n    routes:\n      - name: r\n        rules:\n          - backends: [{ cluster: nope }]\nclusters: []\n",
    )
    .unwrap();
    let out = ferry(&["check-config", path.to_str().unwrap()]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(
            "listeners[0].routes[0].rules[0].backends[0].cluster: unknown cluster \"nope\""
        ),
        "{stderr}"
    );
}

#[test]
fn check_config_prints_schema() {
    let out = ferry(&["check-config", "--schema"]);
    assert!(out.status.success());
    let schema: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(schema["title"], "Config");
    assert!(schema["properties"]["listeners"].is_object());
}

#[test]
fn controller_fails_cleanly_until_m4() {
    let out = ferry(&["controller"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("milestone M4"));
}

fn free_port() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

/// Kills the child process even when the test fails.
struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn proxy_forwards_to_mock_backend() {
    let backend = mock_http::spawn("127.0.0.1:0".parse().unwrap(), Behavior::named("hello"))
        .await
        .unwrap();
    let listen = free_port();
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("hello.yaml");
    let hello = std::fs::read_to_string(repo_root().join("examples/hello.yaml"))
        .unwrap()
        .replace("127.0.0.1:8080", &listen.to_string())
        .replace("127.0.0.1:9001", &backend.addr().to_string());
    std::fs::write(&config, hello).unwrap();

    let _child = ChildGuard(
        Command::new(FERRY)
            .args(["proxy", "--config", config.to_str().unwrap()])
            .env("FERRY_LOG", "warn")
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );

    let client = Client::builder(TokioExecutor::new()).build_http::<Empty<bytes::Bytes>>();
    let uri: http::Uri = format!("http://{listen}/hello").parse().unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let resp = loop {
        match client.get(uri.clone()).await {
            Ok(resp) => break resp,
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => panic!("proxy never came up: {e}"),
        }
    };
    assert_eq!(resp.status(), 200);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let echo: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(echo["backend"], "hello");
    assert_eq!(echo["path"], "/hello");
    assert_eq!(backend.stats().requests(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn proxy_hot_reloads_when_the_config_file_changes() {
    let a = mock_http::spawn("127.0.0.1:0".parse().unwrap(), Behavior::named("a"))
        .await
        .unwrap();
    let b = mock_http::spawn("127.0.0.1:0".parse().unwrap(), Behavior::named("b"))
        .await
        .unwrap();
    let listen = free_port();
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("ferry.yaml");
    let template = std::fs::read_to_string(repo_root().join("examples/hello.yaml"))
        .unwrap()
        .replace("127.0.0.1:8080", &listen.to_string());
    std::fs::write(
        &config,
        template.replace("127.0.0.1:9001", &a.addr().to_string()),
    )
    .unwrap();

    let _child = ChildGuard(
        Command::new(FERRY)
            .args([
                "proxy",
                "--config",
                config.to_str().unwrap(),
                "--watch-interval-ms",
                "100",
            ])
            .env("FERRY_LOG", "error")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );

    let client = Client::builder(TokioExecutor::new()).build_http::<Empty<bytes::Bytes>>();
    let uri: http::Uri = format!("http://{listen}/").parse().unwrap();
    let backend = |client: Client<_, Empty<bytes::Bytes>>, uri: http::Uri| async move {
        let resp = client.get(uri).await.ok()?;
        resp.headers()
            .get(mock_http::BACKEND_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let wait_for = |want: &'static str| {
        let client = client.clone();
        let uri = uri.clone();
        async move {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                if backend(client.clone(), uri.clone()).await.as_deref() == Some(want) {
                    return;
                }
                assert!(Instant::now() < deadline, "never served by {want}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    };
    wait_for("a").await;

    std::fs::write(
        &config,
        template.replace("127.0.0.1:9001", &b.addr().to_string()),
    )
    .unwrap();
    wait_for("b").await;

    // A broken file is rejected and the last good config keeps serving.
    std::fs::write(&config, "listeners: [").unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        backend(client.clone(), uri.clone()).await.as_deref(),
        Some("b")
    );
}
