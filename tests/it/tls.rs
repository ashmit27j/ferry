//! TLS termination: ALPN, SNI certificate selection, hot rotation, errors.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use ferry_integration_tests::{TestProxy, any_port, snapshot};
use ferry_testkit::{Behavior, MockServer, http as mock_http};
use http_body_util::{BodyExt, Empty};
use hyper_util::rt::{TokioExecutor, TokioIo};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::net::TcpStream;

struct Cert {
    pem: String,
    key: String,
}

fn cert(names: &[&str]) -> Cert {
    let ck = rcgen::generate_simple_self_signed(
        names.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
    )
    .unwrap();
    Cert {
        pem: ck.cert.pem(),
        key: ck.signing_key.serialize_pem(),
    }
}

fn write(dir: &Path, name: &str, c: &Cert) -> (String, String) {
    let cert = dir.join(format!("{name}.crt"));
    let key = dir.join(format!("{name}.key"));
    std::fs::write(&cert, &c.pem).unwrap();
    std::fs::write(&key, &c.key).unwrap();
    (
        cert.display().to_string().replace('\\', "/"),
        key.display().to_string().replace('\\', "/"),
    )
}

fn yaml(backend: SocketAddr, tls: &str) -> String {
    format!(
        r#"
listeners:
  - name: https
    address: 127.0.0.1:0
    tls: {tls}
    routes:
      - name: all
        rules: [{{ backends: [{{ cluster: c }}] }}]
clusters:
  - {{ name: c, endpoints: [{{ address: "{backend}" }}] }}
"#
    )
}

struct Answer {
    version: http::Version,
    echo: serde_json::Value,
    peer_cert: Vec<u8>,
}

/// A TLS GET trusting `roots`, with the given SNI and ALPN offer.
async fn https_get(
    addr: SocketAddr,
    sni: &str,
    roots: &[&Cert],
    alpn: &[&[u8]],
    versions: &[&'static rustls::SupportedProtocolVersion],
) -> Result<Answer, String> {
    let mut store = rustls::RootCertStore::empty();
    for r in roots {
        store
            .add(CertificateDer::from_pem_slice(r.pem.as_bytes()).unwrap())
            .unwrap();
    }
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(versions)
        .unwrap()
        .with_root_certificates(store)
        .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    let tcp = TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
    let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from(sni.to_owned()).unwrap(), tcp)
        .await
        .map_err(|e| e.to_string())?;
    let (_, conn) = tls.get_ref();
    let peer_cert = conn.peer_certificates().unwrap()[0].as_ref().to_vec();
    let h2 = conn.alpn_protocol() == Some(b"h2");
    let req = http::Request::get(format!("https://{sni}/tls"))
        .header("host", sni)
        .body(Empty::<Bytes>::new())
        .unwrap();
    let io = TokioIo::new(tls);
    let resp = if h2 {
        let (mut send, conn) = hyper::client::conn::http2::handshake(TokioExecutor::new(), io)
            .await
            .unwrap();
        tokio::spawn(conn);
        send.send_request(req).await.unwrap()
    } else {
        let (mut send, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
        tokio::spawn(conn);
        send.send_request(req).await.unwrap()
    };
    let version = resp.version();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    Ok(Answer {
        version,
        echo: serde_json::from_slice(&body).unwrap(),
        peer_cert,
    })
}

const BOTH: &[&[u8]] = &[b"h2", b"http/1.1"];
const ALL_VERSIONS: &[&rustls::SupportedProtocolVersion] =
    &[&rustls::version::TLS13, &rustls::version::TLS12];

fn der(c: &Cert) -> Vec<u8> {
    CertificateDer::from_pem_slice(c.pem.as_bytes())
        .unwrap()
        .as_ref()
        .to_vec()
}

async fn backend() -> MockServer {
    mock_http::spawn(any_port(), Behavior::named("a"))
        .await
        .unwrap()
}

#[tokio::test]
async fn terminates_tls_with_alpn_h2_and_http1() {
    let b = backend().await;
    let dir = tempfile::tempdir().unwrap();
    let c = cert(&["app.test"]);
    let (crt, key) = write(dir.path(), "app", &c);
    let proxy = TestProxy::start(&yaml(
        b.addr(),
        &format!("{{ certificates: [{{ cert_file: '{crt}', key_file: '{key}' }}] }}"),
    ))
    .await;

    let h2 = https_get(proxy.addr, "app.test", &[&c], BOTH, ALL_VERSIONS)
        .await
        .unwrap();
    assert_eq!(h2.version, http::Version::HTTP_2);
    assert_eq!(h2.echo["headers"]["x-forwarded-proto"], "https");
    assert_eq!(h2.echo["path"], "/tls");

    let h1 = https_get(proxy.addr, "app.test", &[&c], &[b"http/1.1"], ALL_VERSIONS)
        .await
        .unwrap();
    assert_eq!(h1.version, http::Version::HTTP_11);
    assert_eq!(h1.echo["backend"], "a");
}

#[tokio::test]
async fn sni_selects_the_certificate() {
    let b = backend().await;
    let first = cert(&["one.test"]);
    let second = cert(&["*.two.test"]);
    let tls = format!(
        "{{ certificates: [{{ cert_pem: {:?}, key_pem: {:?} }}, {{ cert_pem: {:?}, key_pem: {:?} }}] }}",
        first.pem, first.key, second.pem, second.key
    );
    let proxy = TestProxy::start(&yaml(b.addr(), &tls)).await;
    let roots = [&first, &second];
    let a = https_get(proxy.addr, "one.test", &roots, BOTH, ALL_VERSIONS)
        .await
        .unwrap();
    assert_eq!(a.peer_cert, der(&first));
    let b2 = https_get(proxy.addr, "api.two.test", &roots, BOTH, ALL_VERSIONS)
        .await
        .unwrap();
    assert_eq!(b2.peer_cert, der(&second));
}

#[tokio::test]
async fn rotated_certificate_files_take_effect_without_restart() {
    let b = backend().await;
    let dir = tempfile::tempdir().unwrap();
    let old = cert(&["app.test"]);
    let (crt, key) = write(dir.path(), "app", &old);
    let proxy = TestProxy::start(&yaml(
        b.addr(),
        &format!("{{ certificates: [{{ cert_file: '{crt}', key_file: '{key}' }}] }}"),
    ))
    .await;
    let first = https_get(proxy.addr, "app.test", &[&old], BOTH, ALL_VERSIONS)
        .await
        .unwrap();
    assert_eq!(first.peer_cert, der(&old));

    // cert-manager style rotation: same names, new key pair.
    let new = cert(&["app.test"]);
    write(dir.path(), "app", &new);
    let mut rotated = false;
    for _ in 0..50 {
        if let Ok(a) = https_get(proxy.addr, "app.test", &[&old, &new], BOTH, ALL_VERSIONS).await
            && a.peer_cert == der(&new)
        {
            rotated = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(rotated, "the new certificate was never served");
}

#[tokio::test]
async fn reload_with_broken_certificate_is_rejected() {
    let b = backend().await;
    let good = cert(&["app.test"]);
    let other = cert(&["other.test"]);
    let tls = format!(
        "{{ certificates: [{{ cert_pem: {:?}, key_pem: {:?} }}] }}",
        good.pem, good.key
    );
    let proxy = TestProxy::start(&yaml(b.addr(), &tls)).await;
    // A key that belongs to a different certificate.
    let broken = format!(
        "{{ certificates: [{{ cert_pem: {:?}, key_pem: {:?} }}] }}",
        good.pem, other.key
    );
    let err = proxy
        .reload
        .swap(snapshot(&yaml(b.addr(), &broken), 2))
        .unwrap_err();
    assert!(err.to_string().contains("https"), "{err}");
    assert_eq!(
        proxy.reload.current_version().0,
        1,
        "old snapshot keeps serving"
    );
    assert!(
        https_get(proxy.addr, "app.test", &[&good], BOTH, ALL_VERSIONS)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn min_version_1_3_refuses_tls_1_2_clients() {
    let b = backend().await;
    let c = cert(&["app.test"]);
    let tls = format!(
        "{{ min_version: '1.3', certificates: [{{ cert_pem: {:?}, key_pem: {:?} }}] }}",
        c.pem, c.key
    );
    let proxy = TestProxy::start(&yaml(b.addr(), &tls)).await;
    let tls12_only: &[&rustls::SupportedProtocolVersion] = &[&rustls::version::TLS12];
    assert!(
        https_get(proxy.addr, "app.test", &[&c], BOTH, tls12_only)
            .await
            .is_err()
    );
    assert!(
        https_get(proxy.addr, "app.test", &[&c], BOTH, ALL_VERSIONS)
            .await
            .is_ok()
    );
}
