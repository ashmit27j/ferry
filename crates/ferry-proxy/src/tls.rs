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
use ferry_config::{
    CaSource, CertificateSource, ListenerTls, TlsVersion, UpstreamProtocol,
    UpstreamTls as UpstreamTlsSettings,
};
use ferry_lb::pool::UpstreamTls;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use x509_parser::extensions::GeneralName;

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

/// When a certificate expires, and a name to show for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CertExpiry {
    /// The first DNS subject alternative name, else the subject common name,
    /// else empty.
    pub(crate) name: String,
    /// `notAfter`, in Unix seconds.
    pub(crate) not_after: i64,
}

/// Reads the expiry of a DER certificate; `None` if it does not parse.
pub(crate) fn cert_expiry(der: &[u8]) -> Option<CertExpiry> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).ok()?;
    let dns = cert
        .subject_alternative_name()
        .ok()
        .flatten()
        .and_then(|san| {
            san.value.general_names.iter().find_map(|n| match n {
                GeneralName::DNSName(d) => Some((*d).to_owned()),
                _ => None,
            })
        });
    let name = dns
        .or_else(|| {
            cert.subject()
                .iter_common_name()
                .next()
                .and_then(|cn| cn.as_str().ok())
                .map(str::to_owned)
        })
        .unwrap_or_default();
    Some(CertExpiry {
        name,
        not_after: cert.validity().not_after.timestamp(),
    })
}

/// The expiry of the first certificate in a PEM chain.
pub(crate) fn pem_expiry(pem: &[u8]) -> Option<CertExpiry> {
    let der = CertificateDer::pem_slice_iter(pem).next()?.ok()?;
    cert_expiry(&der)
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

/// Builds the TLS client side of a cluster: trusted roots (configured CAs or
/// the Mozilla store), an optional client certificate for mTLS, SNI, and ALPN
/// matching the cluster's protocol.
pub(crate) fn upstream_tls(
    cfg: &UpstreamTlsSettings,
    protocol: UpstreamProtocol,
) -> Result<UpstreamTls, TlsError> {
    let provider = provider();
    let mut hasher = DefaultHasher::new();
    cfg.sni().hash(&mut hasher);
    let mut roots = RootCertStore::empty();
    match cfg.ca() {
        None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
        Some(ca) => {
            let (pem, what) = match ca {
                CaSource::File(p) => (
                    std::fs::read(p).map_err(|source| TlsError::Read {
                        path: p.display().to_string(),
                        source,
                    })?,
                    p.display().to_string(),
                ),
                CaSource::Pem(s) => (s.as_bytes().to_vec(), "inline CA".to_owned()),
                _ => (Vec::new(), "CA".to_owned()),
            };
            pem.hash(&mut hasher);
            let mut added = 0;
            for der in CertificateDer::pem_slice_iter(&pem) {
                let der = der.map_err(|e| TlsError::Pem {
                    what: what.clone(),
                    message: format!("bad CA PEM: {e}"),
                })?;
                roots.add(der).map_err(|source| TlsError::Rustls {
                    what: what.clone(),
                    source,
                })?;
                added += 1;
            }
            if added == 0 {
                return Err(TlsError::Pem {
                    what,
                    message: "no CA certificate found".into(),
                });
            }
        }
    }
    let builder = ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(|source| TlsError::Rustls {
            what: "upstream TLS".into(),
            source,
        })?
        .with_root_certificates(roots);
    let mut client_certificate = None;
    let mut config = match cfg.client_certificate() {
        None => builder.with_no_client_auth(),
        Some(src) => {
            let (chain, key, what) = read_source(src)?;
            chain.hash(&mut hasher);
            key.hash(&mut hasher);
            // Validates the pair (including that the key matches) first.
            certified_key(&chain, &key, &what, &provider)?;
            let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&chain)
                .collect::<Result<_, _>>()
                .map_err(|e| TlsError::Pem {
                    what: what.clone(),
                    message: format!("bad certificate PEM: {e}"),
                })?;
            let key = PrivateKeyDer::from_pem_slice(&key).map_err(|e| TlsError::Pem {
                what: what.clone(),
                message: format!("bad private key PEM: {e}"),
            })?;
            client_certificate = certs.first().cloned();
            builder
                .with_client_auth_cert(certs, key)
                .map_err(|source| TlsError::Rustls { what, source })?
        }
    };
    config.alpn_protocols = match protocol {
        UpstreamProtocol::Http2 => vec![b"h2".to_vec()],
        _ => vec![b"http/1.1".to_vec()],
    };
    matches!(protocol, UpstreamProtocol::Http2).hash(&mut hasher);
    let server_name = ServerName::try_from(cfg.sni().to_owned()).map_err(|e| TlsError::Pem {
        what: "sni".into(),
        message: e.to_string(),
    })?;
    Ok(UpstreamTls {
        config: Arc::new(config),
        server_name,
        id: hasher.finish(),
        client_certificate,
    })
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

    /// The expiry of every certificate currently offered, in config order.
    pub(crate) fn expiries(&self) -> Vec<CertExpiry> {
        self.resolver
            .certs
            .load()
            .iter()
            .filter_map(|ck| ck.end_entity_cert().ok())
            .filter_map(|der| cert_expiry(der))
            .collect()
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
    fn expiry_comes_from_not_after_and_the_first_dns_name() {
        let mut params = rcgen::CertificateParams::new(vec!["a.example.com".into()]).unwrap();
        params.not_after = rcgen::date_time_ymd(2031, 1, 2);
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        let e = pem_expiry(cert.pem().as_bytes()).unwrap();
        assert_eq!(e.name, "a.example.com");
        // 2031-01-02T00:00:00Z
        assert_eq!(e.not_after, 1_925_078_400);

        let tls = ServerTls::new(&listener_tls(&[(cert.pem(), key.serialize_pem())])).unwrap();
        assert_eq!(tls.expiries(), vec![e]);
        assert_eq!(pem_expiry(b"not a certificate"), None);
        assert_eq!(cert_expiry(b"\x30\x03\x02\x01\x00"), None);
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
