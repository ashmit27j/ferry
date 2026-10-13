//! Backend references, ReferenceGrants, endpoints and TLS Secrets.

use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr};

use ferry_config::UpstreamProtocol;
use k8s_openapi::api::core::v1::{Secret, ServicePort};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use super::RouteKind;
use super::route::BackendRef;
use crate::gateway_api::GROUP;
use crate::world::{ObjKey, World};

/// Why a reference did not resolve: a Gateway API reason and a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Problem {
    pub(crate) reason: &'static str,
    pub(crate) message: String,
}

impl Problem {
    pub(crate) fn new(reason: &'static str, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
        }
    }
}

/// A Service port as a cluster: one Ferry cluster per Service, port and
/// upstream protocol.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ClusterSpec {
    pub(crate) service: ObjKey,
    pub(crate) port: i32,
    pub(crate) http2: bool,
}

impl ClusterSpec {
    /// The Ferry cluster name. Kubernetes names never contain `_`, so the
    /// parts cannot run together.
    pub(crate) fn name(&self) -> String {
        format!(
            "{}_{}_{}{}",
            self.service.namespace,
            self.service.name,
            self.port,
            if self.http2 { "_h2" } else { "" }
        )
    }

    pub(crate) fn protocol(&self) -> UpstreamProtocol {
        if self.http2 {
            UpstreamProtocol::Http2
        } else {
            UpstreamProtocol::Http1
        }
    }
}

/// Whether a ReferenceGrant in `to.namespace` lets an object of
/// `from_group/from_kind` in `from_ns` refer to `to`.
pub(crate) fn permitted(
    world: &World,
    from_kind: &str,
    from_ns: &str,
    to_group: &str,
    to_kind: &str,
    to: &ObjKey,
) -> bool {
    if from_ns == to.namespace {
        return true;
    }
    world
        .reference_grants
        .iter()
        .filter(|(k, _)| k.namespace == to.namespace)
        .any(|(_, g)| {
            g.spec
                .from
                .iter()
                .any(|f| f.group == GROUP && f.kind == from_kind && f.namespace == from_ns)
                && g.spec.to.iter().any(|t| {
                    t.group == to_group
                        && t.kind == to_kind
                        && t.name.as_deref().is_none_or(|n| n == to.name)
                })
        })
}

/// Resolves a backend reference to a cluster, or explains why it is invalid
/// (Gateway API route `ResolvedRefs` reasons).
pub(crate) fn resolve(
    world: &World,
    kind: RouteKind,
    route_ns: &str,
    b: &BackendRef,
) -> Result<ClusterSpec, Problem> {
    if !(b.group.is_empty() || b.group == "core") || b.kind != "Service" {
        return Err(Problem::new(
            "InvalidKind",
            format!(
                "backendRef {}/{} {} is not a Service",
                b.group, b.kind, b.name
            ),
        ));
    }
    let key = ObjKey::new(b.namespace.clone(), b.name.clone());
    if !permitted(world, kind.as_str(), route_ns, "", "Service", &key) {
        return Err(Problem::new(
            "RefNotPermitted",
            format!(
                "no ReferenceGrant in {} allows a reference to Service {key}",
                key.namespace
            ),
        ));
    }
    let Some(svc) = world.services.get(&key) else {
        return Err(Problem::new(
            "BackendNotFound",
            format!("Service {key} not found"),
        ));
    };
    let Some(port) = b.port else {
        return Err(Problem::new(
            "BackendNotFound",
            format!("backendRef to Service {key} has no port"),
        ));
    };
    let spec = svc.spec.as_ref();
    if spec.and_then(|s| s.type_.as_deref()) == Some("ExternalName") {
        return Err(Problem::new(
            "UnsupportedProtocol",
            format!("Service {key} is ExternalName, which Ferry does not support"),
        ));
    }
    let Some(sp) = service_port(world, &key, port) else {
        return Err(Problem::new(
            "BackendNotFound",
            format!("Service {key} has no port {port}"),
        ));
    };
    let http2 = match kind {
        RouteKind::Grpc => true,
        RouteKind::Http => sp.app_protocol.as_deref() == Some("kubernetes.io/h2c"),
    };
    Ok(ClusterSpec {
        service: key,
        port,
        http2,
    })
}

fn service_port<'a>(world: &'a World, key: &ObjKey, port: i32) -> Option<&'a ServicePort> {
    world
        .services
        .get(key)?
        .spec
        .as_ref()?
        .ports
        .iter()
        .flatten()
        .find(|p| p.port == port && p.protocol.as_deref().is_none_or(|p| p == "TCP"))
}

/// Ready endpoints of a Service port, from its EndpointSlices: the slice
/// port with the Service port's name, at every address whose `ready`
/// condition is not false. Sorted and deduplicated.
pub(crate) fn endpoints(world: &World, spec: &ClusterSpec) -> Vec<SocketAddr> {
    let Some(sp) = service_port(world, &spec.service, spec.port) else {
        return Vec::new();
    };
    let port_name = sp.name.as_deref().unwrap_or_default();
    let mut out = BTreeSet::new();
    for slice in world
        .endpoint_slices
        .get(&spec.service)
        .into_iter()
        .flatten()
    {
        if !matches!(slice.address_type.as_str(), "IPv4" | "IPv6") {
            continue;
        }
        let Some(target) = slice
            .ports
            .iter()
            .flatten()
            .find(|p| p.name.as_deref().unwrap_or_default() == port_name)
            .and_then(|p| p.port)
            .and_then(|p| u16::try_from(p).ok())
        else {
            continue;
        };
        for ep in &slice.endpoints {
            if ep.conditions.as_ref().and_then(|c| c.ready) == Some(false) {
                continue;
            }
            for a in &ep.addresses {
                if let Ok(ip) = a.parse::<IpAddr>() {
                    out.insert(SocketAddr::new(ip, target));
                }
            }
        }
    }
    out.into_iter().collect()
}

fn secret_text<'a>(secret: &'a Secret, key: &str) -> Result<&'a str, String> {
    let bytes = secret
        .data
        .as_ref()
        .and_then(|d| d.get(key))
        .ok_or_else(|| format!("has no {key}"))?;
    std::str::from_utf8(&bytes.0).map_err(|_| format!("{key} is not text"))
}

/// The certificate chain and key of a `kubernetes.io/tls` Secret, as PEM,
/// after checking they parse and belong together.
pub(crate) fn tls_pair(secret: &Secret) -> Result<(String, String), String> {
    let cert = secret_text(secret, "tls.crt")?;
    let key = secret_text(secret, "tls.key")?;
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert.as_bytes())
        .collect::<Result<_, _>>()
        .map_err(|e| format!("tls.crt does not parse: {e}"))?;
    if chain.is_empty() {
        return Err("tls.crt holds no certificate".into());
    }
    let der = PrivateKeyDer::from_pem_slice(key.as_bytes())
        .map_err(|e| format!("tls.key does not parse: {e}"))?;
    rustls::sign::CertifiedKey::from_der(
        chain,
        der,
        &rustls::crypto::aws_lc_rs::default_provider(),
    )
    .map_err(|e| format!("tls.key does not match tls.crt: {e}"))?;
    Ok((cert.to_owned(), key.to_owned()))
}

/// The CA bundle (`ca.crt`) of a Secret, after checking it parses.
pub(crate) fn ca_bundle(secret: &Secret) -> Result<String, String> {
    let ca = secret_text(secret, "ca.crt")?;
    let n = CertificateDer::pem_slice_iter(ca.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("ca.crt does not parse: {e}"))?
        .len();
    if n == 0 {
        return Err("ca.crt holds no certificate".into());
    }
    Ok(ca.to_owned())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use k8s_openapi::ByteString;
    use std::collections::BTreeMap;

    /// A self-signed certificate and key for `names`.
    pub(crate) fn self_signed(names: &[&str]) -> (String, String) {
        let names: Vec<String> = names.iter().map(|n| (*n).to_owned()).collect();
        let c = rcgen::generate_simple_self_signed(names).unwrap();
        (c.cert.pem(), c.signing_key.serialize_pem())
    }

    pub(crate) fn tls_secret(cert: &str, key: &str) -> Secret {
        let mut data = BTreeMap::new();
        data.insert("tls.crt".to_owned(), ByteString(cert.as_bytes().to_vec()));
        data.insert("tls.key".to_owned(), ByteString(key.as_bytes().to_vec()));
        Secret {
            data: Some(data),
            type_: Some("kubernetes.io/tls".into()),
            ..Secret::default()
        }
    }

    #[test]
    fn tls_pairs_are_checked() {
        let (cert, key) = self_signed(&["a.example"]);
        assert!(tls_pair(&tls_secret(&cert, &key)).is_ok());
        let (_, other_key) = self_signed(&["b.example"]);
        assert!(
            tls_pair(&tls_secret(&cert, &other_key))
                .unwrap_err()
                .contains("match")
        );
        assert!(tls_pair(&tls_secret("Hello world\n", "Hello world\n")).is_err());
        assert!(
            tls_pair(&Secret::default())
                .unwrap_err()
                .contains("tls.crt")
        );
    }

    #[test]
    fn cluster_names_are_unambiguous() {
        let spec = ClusterSpec {
            service: ObjKey::new("ns", "svc"),
            port: 8080,
            http2: true,
        };
        assert_eq!(spec.name(), "ns_svc_8080_h2");
    }
}
