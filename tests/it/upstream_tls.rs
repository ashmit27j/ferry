//! TLS and mutual TLS from Ferry to backends.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use ferry_integration_tests::{TestProxy, any_port, get, metric, snapshot};
use ferry_testkit::{Behavior, MockServer, http as mock_http};
use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

/// A CA and leaf certificates signed by it.
struct Pki {
    ca: CertifiedIssuer<'static, KeyPair>,
}

struct Leaf {
    cert: String,
    key: String,
}

impl Pki {
    fn new() -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        Self {
            ca: CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap(),
        }
    }

    fn ca_pem(&self) -> String {
        self.ca.pem()
    }

    fn leaf(&self, names: &[&str]) -> Leaf {
        let key = KeyPair::generate().unwrap();
        let params =
            CertificateParams::new(names.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
                .unwrap();
        let cert = params.signed_by(&key, &self.ca).unwrap();
        Leaf {
            cert: cert.pem(),
            key: key.serialize_pem(),
        }
    }
}

fn server_config(
    leaf: &Leaf,
    client_ca: Option<&Pki>,
    alpn: &[&[u8]],
) -> Arc<rustls::ServerConfig> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let builder = rustls::ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .unwrap();
    let builder = match client_ca {
        None => builder.with_no_client_auth(),
        Some(pki) => {
            let mut roots = rustls::RootCertStore::empty();
            roots
                .add(CertificateDer::from_pem_slice(pki.ca_pem().as_bytes()).unwrap())
                .unwrap();
            let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
                Arc::new(roots),
                provider,
            )
            .build()
            .unwrap();
            builder.with_client_cert_verifier(verifier)
        }
    };
    let mut config = builder
        .with_single_cert(
            vec![CertificateDer::from_pem_slice(leaf.cert.as_bytes()).unwrap()],
            PrivateKeyDer::from_pem_slice(leaf.key.as_bytes()).unwrap(),
        )
        .unwrap();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    Arc::new(config)
}

async fn tls_backend(config: Arc<rustls::ServerConfig>) -> MockServer {
    mock_http::spawn_tls(any_port(), Behavior::named("secure"), config)
        .await
        .unwrap()
}

fn yaml(backend: std::net::SocketAddr, protocol: &str, tls: &str) -> String {
    format!(
        r#"
listeners:
  - name: http
    address: 127.0.0.1:0
    routes:
      - name: all
        rules: [{{ backends: [{{ cluster: c }}] }}]
clusters:
  - name: c
    protocol: {protocol}
    tls: {tls}
    endpoints: [{{ address: "{backend}" }}]
"#
    )
}

#[tokio::test]
async fn http1_and_h2_over_tls_with_a_private_ca() {
    let pki = Pki::new();
    let leaf = pki.leaf(&["backend.test"]);
    let backend = tls_backend(server_config(&leaf, None, &[b"h2", b"http/1.1"])).await;
    let tls = format!("{{ sni: backend.test, ca_pem: {:?} }}", pki.ca_pem());

    let h1 = TestProxy::start(&yaml(backend.addr(), "http1", &tls)).await;
    let r = get(h1.addr, "/").await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.echo()["tls"]["alpn"], "http/1.1");
    assert_eq!(r.echo()["tls"]["client_cert"], false);

    let h2 = TestProxy::start(&yaml(backend.addr(), "http2", &tls)).await;
    let r = get(h2.addr, "/").await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.echo()["version"], "HTTP/2.0");
    assert_eq!(r.echo()["tls"]["alpn"], "h2");
}

#[tokio::test]
async fn untrusted_backend_certificate_fails_with_502() {
    let pki = Pki::new();
    let other = Pki::new();
    let backend = tls_backend(server_config(
        &pki.leaf(&["backend.test"]),
        None,
        &[b"http/1.1"],
    ))
    .await;
    let tls = format!("{{ sni: backend.test, ca_pem: {:?} }}", other.ca_pem());
    let proxy = TestProxy::start(&yaml(backend.addr(), "http1", &tls)).await;
    assert_eq!(get(proxy.addr, "/").await.status, 502);
    assert_eq!(backend.stats().requests(), 0);

    // A name the certificate does not cover is refused too.
    let wrong_name = format!("{{ sni: other.test, ca_pem: {:?} }}", pki.ca_pem());
    let proxy = TestProxy::start(&yaml(backend.addr(), "http1", &wrong_name)).await;
    assert_eq!(get(proxy.addr, "/").await.status, 502);
}

#[tokio::test]
async fn mutual_tls_presents_the_client_certificate() {
    let server_pki = Pki::new();
    let client_pki = Pki::new();
    let backend = tls_backend(server_config(
        &server_pki.leaf(&["backend.test"]),
        Some(&client_pki),
        &[b"http/1.1"],
    ))
    .await;
    let client = client_pki.leaf(&["ferry.test"]);

    // Without a client certificate the backend refuses the handshake.
    let anonymous = format!("{{ sni: backend.test, ca_pem: {:?} }}", server_pki.ca_pem());
    let proxy = TestProxy::start(&yaml(backend.addr(), "http1", &anonymous)).await;
    assert_eq!(get(proxy.addr, "/").await.status, 502);

    let mtls = format!(
        "{{ sni: backend.test, ca_pem: {:?}, client_certificate: {{ cert_pem: {:?}, key_pem: {:?} }} }}",
        server_pki.ca_pem(),
        client.cert,
        client.key
    );
    let proxy = TestProxy::start(&yaml(backend.addr(), "http1", &mtls)).await;
    let r = get(proxy.addr, "/").await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.echo()["tls"]["client_cert"], true);
    let text = proxy.metrics_text();
    let series = r#"ferry_tls_certificate_expiry_timestamp_seconds{kind="upstream",owner="c",name="ferry.test"}"#;
    assert!(
        metric(&text, series).is_some(),
        "{series} missing in
{text}"
    );
}

#[tokio::test]
async fn broken_ca_rejects_the_config() {
    let backend = mock_http::spawn(any_port(), Behavior::named("a"))
        .await
        .unwrap();
    let proxy = TestProxy::start(&yaml(backend.addr(), "http1", "{ sni: backend.test }")).await;
    let bad = yaml(
        backend.addr(),
        "http1",
        "{ sni: backend.test, ca_pem: '-----BEGIN CERTIFICATE-----\nnope\n-----END CERTIFICATE-----' }",
    );
    let err = proxy.reload.swap(snapshot(&bad, 2)).unwrap_err();
    assert!(err.to_string().contains("cluster c"), "{err}");
    assert_eq!(proxy.reload.current_version().0, 1);
}
