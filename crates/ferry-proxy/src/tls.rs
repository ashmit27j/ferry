//! TLS termination (ADR 0023).
//!
//! A TLS listener offers every configured certificate. During the handshake
//! the [`SniResolver`] picks the first certificate whose DNS names cover the
//! client's SNI, falling back to the first certificate. Certificates are
//! loaded off the hot path; [`ServerTls::reload_if_changed`] re-reads file
//! sources and swaps new certificates in atomically, so rotation (for example
//! by cert-manager) takes effect on the next handshake with no restart.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;
use ferry_config::{CertificateSource, ListenerTls, TlsVersion};
use rustls::ServerConfig;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;

/// Time allowed for a client to complete the TLS handshake.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Why certificates could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    /// A PEM file could not be read.
    #[error("cannot read {path}: {source}")]
    Read {
        /// File.
        path: String,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The PEM data holds no usable certificate or key.
    #[error("{what}: {message}")]
    Pem {
        /// Which item failed.
        what: String,
        /// What is wrong.
        message: String,
    },
    /// rustls rejected the configuration (for example a key that does not
    /// match its certificate).
    #[error("{what}: {source}")]
    Rustls {
        /// Which item failed.
        what: String,
        /// Underlying error.
        #[source]
        source: rustls::Error,
    },
}

/// The crypto provider: aws-lc-rs, as the plan requires.
pub(crate) fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

/// Reads the PEM bytes of a certificate source: (chain, key, label).
fn read_source(src: &CertificateSource) -> Result<(Vec<u8>, Vec<u8>, String), TlsError> {
    let read = |p: &std::path::Path| {
        std::fs::read(p).map_err(|source| TlsError::Read {
            path: p.display().to_string(),
            source,
        })
    };
    match src {
        CertificateSource::Files { cert, key } => {
            Ok((read(cert)?, read(key)?, cert.display().to_string()))
        }
        CertificateSource::Pem { cert, key } => Ok((
            cert.as_bytes().to_vec(),
            key.expose().as_bytes().to_vec(),
            "inline certificate".to_owned(),
        )),
        _ => Err(TlsError::Pem {
            what: "certificate".into(),
            message: "unsupported source".into(),
        }),
    }
}

/// Parses a PEM chain and key into a rustls certified key.
pub(crate) fn certified_key(
    chain: &[u8],
    key: &[u8],
    what: &str,
    provider: &CryptoProvider,
) -> Result<CertifiedKey, TlsError> {
    let pem_err = |message: String| TlsError::Pem {
        what: what.to_owned(),
        message,
    };
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(chain)
        .collect::<Result<_, _>>()
        .map_err(|e| pem_err(format!("bad certificate PEM: {e}")))?;
    if certs.is_empty() {
        return Err(pem_err("no certificate found".into()));
    }
    let key = PrivateKeyDer::from_pem_slice(key)
        .map_err(|e| pem_err(format!("bad private key PEM: {e}")))?;
    CertifiedKey::from_der(certs, key, provider).map_err(|source| TlsError::Rustls {
        what: what.to_owned(),
        source,
    })
}

/// Picks a certificate by SNI.
#[derive(Debug)]
pub(crate) struct SniResolver {
    certs: ArcSwap<Vec<Arc<CertifiedKey>>>,
}

impl SniResolver {
    fn pick(&self, sni: Option<&str>) -> Option<Arc<CertifiedKey>> {
        let certs = self.certs.load();
        if let Some(name) = sni.and_then(|s| ServerName::try_from(s).ok()) {
            for ck in certs.iter() {
                let covers = ck.end_entity_cert().ok().is_some_and(|der| {
                    webpki::EndEntityCert::try_from(der)
                        .is_ok_and(|ee| ee.verify_is_valid_for_subject_name(&name).is_ok())
                });
                if covers {
                    return Some(Arc::clone(ck));
                }
            }
        }
        certs.first().cloned()
    }
}

impl ResolvesServerCert for SniResolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.pick(hello.server_name())
    }
}

/// One TLS listener's acceptor configuration and certificate state.
#[derive(Debug)]
pub(crate) struct ServerTls {
    pub(crate) config: Arc<ServerConfig>,
    resolver: Arc<SniResolver>,
    sources: ListenerTls,
    fingerprint: AtomicU64,
}

impl ServerTls {
    /// Loads every certificate and builds the acceptor configuration.
    pub(crate) fn new(tls: &ListenerTls) -> Result<Self, TlsError> {
        let provider = provider();
        let (certs, fingerprint) = load_all(tls, &provider)?;
        let resolver = Arc::new(SniResolver {
            certs: ArcSwap::from_pointee(certs),
        });
        let versions: &[&rustls::SupportedProtocolVersion] = match tls.min_version() {
            TlsVersion::Tls13 => &[&rustls::version::TLS13],
            _ => &[&rustls::version::TLS13, &rustls::version::TLS12],
        };
        let mut config = ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(versions)
            .map_err(|source| TlsError::Rustls {
                what: "listener TLS".into(),
                source,
            })?
            .with_no_client_auth()
            .with_cert_resolver(Arc::clone(&resolver) as Arc<dyn ResolvesServerCert>);
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Ok(Self {
            config: Arc::new(config),
            resolver,
            sources: tls.clone(),
            fingerprint: AtomicU64::new(fingerprint),
        })
    }

    /// The settings these certificates came from.
    pub(crate) fn sources(&self) -> &ListenerTls {
        &self.sources
    }

    /// Re-reads the certificate sources and swaps in new certificates if any
    /// changed. Returns `Ok(true)` after a swap. On error the old
    /// certificates stay in place.
    pub(crate) fn reload_if_changed(&self) -> Result<bool, TlsError> {
        let (certs, fingerprint) = load_all(&self.sources, &provider())?;
        if self.fingerprint.swap(fingerprint, Ordering::Relaxed) == fingerprint {
            return Ok(false);
        }
        self.resolver.certs.store(Arc::new(certs));
        Ok(true)
    }
}

fn load_all(
    tls: &ListenerTls,
    provider: &CryptoProvider,
) -> Result<(Vec<Arc<CertifiedKey>>, u64), TlsError> {
    let mut hasher = DefaultHasher::new();
    let mut certs = Vec::with_capacity(tls.certificates().len());
    for src in tls.certificates() {
        let (chain, key, what) = read_source(src)?;
        chain.hash(&mut hasher);
        key.hash(&mut hasher);
        certs.push(Arc::new(certified_key(&chain, &key, &what, provider)?));
    }
    Ok((certs, hasher.finish()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ferry_config::{SnapshotVersion, from_yaml_str};

    /// A self-signed certificate and key in PEM for `names`.
    pub(crate) fn self_signed(names: &[&str]) -> (String, String) {
        let ck = rcgen::generate_simple_self_signed(
            names.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
        )
        .unwrap();
        (ck.cert.pem(), ck.signing_key.serialize_pem())
    }

    fn listener_tls(certs: &[(String, String)]) -> ListenerTls {
        let list: Vec<String> = certs
            .iter()
            .map(|(c, k)| format!("{{ cert_pem: {c:?}, key_pem: {k:?} }}"))
            .collect();
        let yaml = format!(
            "listeners: [{{ name: l, address: 127.0.0.1:0, tls: {{ certificates: [{}] }} }}]\nclusters: []\n",
            list.join(", ")
        );
        from_yaml_str(&yaml)
            .unwrap()
            .into_snapshot(SnapshotVersion(1))
            .unwrap()
            .listeners()[0]
            .tls()
            .unwrap()
            .clone()
    }

    #[test]
    fn sni_picks_the_matching_certificate_and_defaults_to_the_first() {
        let a = self_signed(&["a.example.com"]);
        let b = self_signed(&["*.b.example.com"]);
        let tls = ServerTls::new(&listener_tls(&[a.clone(), b.clone()])).unwrap();
        let pick = |sni| {
            let ck = tls.resolver.pick(sni).unwrap();
            ck.end_entity_cert().unwrap().as_ref().to_vec()
        };
        let der = |pem: &str| {
            CertificateDer::from_pem_slice(pem.as_bytes())
                .unwrap()
                .as_ref()
                .to_vec()
        };
        assert_eq!(pick(Some("a.example.com")), der(&a.0));
        assert_eq!(pick(Some("x.b.example.com")), der(&b.0));
        assert_eq!(pick(Some("unknown.test")), der(&a.0));
        assert_eq!(pick(None), der(&a.0));
    }

    #[test]
    fn mismatched_key_is_rejected() {
        let a = self_signed(&["a.test"]);
        let b = self_signed(&["b.test"]);
        let err = ServerTls::new(&listener_tls(&[(a.0, b.1)])).unwrap_err();
        assert!(matches!(err, TlsError::Rustls { .. }), "{err}");
    }

    #[test]
    fn garbage_pem_is_rejected() {
        let tls = listener_tls(&[(
            "-----BEGIN CERTIFICATE-----\nnope\n-----END CERTIFICATE-----".into(),
            "-----BEGIN PRIVATE KEY-----\nnope\n-----END PRIVATE KEY-----".into(),
        )]);
        assert!(ServerTls::new(&tls).is_err());
    }
}
