//! The controller's view of the cluster: every object the translator reads.
//!
//! The watchers in [`crate::run`] keep reflector stores; each reconcile pass
//! builds a [`World`] from them (cheap `Arc` clones) and hands it to the pure
//! [`crate::translate::translate`]. Tests and `ferry controller translate`
//! build one from YAML manifests with [`World::from_manifests`].

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use k8s_openapi::api::core::v1::{Namespace, Secret, Service};
use k8s_openapi::api::discovery::v1::EndpointSlice;
use kube::{Resource, ResourceExt};
use serde::de::DeserializeOwned;

use crate::gateway_api::gatewayclasses::GatewayClass;
use crate::gateway_api::gateways::Gateway;
use crate::gateway_api::grpcroutes::GrpcRoute;
use crate::gateway_api::httproutes::HttpRoute;
use crate::gateway_api::referencegrants::ReferenceGrant;
use crate::policy::FerryPolicy;

/// The label EndpointSlices carry to name their Service.
pub const SERVICE_NAME_LABEL: &str = "kubernetes.io/service-name";

/// A namespaced object's identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjKey {
    /// Namespace.
    pub namespace: String,
    /// Name.
    pub name: String,
}

impl ObjKey {
    /// A key for `namespace/name`.
    #[must_use]
    pub fn new(namespace: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            name: name.into(),
        }
    }

    /// The key of a namespaced object (empty namespace if it has none).
    #[must_use]
    pub fn of<K: Resource>(obj: &K) -> Self {
        Self::new(obj.namespace().unwrap_or_default(), obj.name_any())
    }

    /// Parses `namespace/name`.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let (ns, name) = s.split_once('/')?;
        (!ns.is_empty() && !name.is_empty() && !name.contains('/')).then(|| Self::new(ns, name))
    }
}

impl fmt::Display for ObjKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.namespace, self.name)
    }
}

/// What the controller knows about the proxies it runs for one Gateway.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fleet {
    /// Proxy pods that are ready, which means they have applied a snapshot.
    pub ready_replicas: u32,
    /// Addresses of the proxy Service: `(type, value)` with type `IPAddress`
    /// or `Hostname`, as the Gateway API spells them.
    pub addresses: Vec<(String, String)>,
}

/// Every object the translator reads, keyed for deterministic iteration.
#[derive(Debug, Clone, Default)]
pub struct World {
    /// GatewayClasses by name.
    pub gateway_classes: BTreeMap<String, Arc<GatewayClass>>,
    /// Gateways.
    pub gateways: BTreeMap<ObjKey, Arc<Gateway>>,
    /// HTTPRoutes.
    pub http_routes: BTreeMap<ObjKey, Arc<HttpRoute>>,
    /// GRPCRoutes.
    pub grpc_routes: BTreeMap<ObjKey, Arc<GrpcRoute>>,
    /// ReferenceGrants.
    pub reference_grants: BTreeMap<ObjKey, Arc<ReferenceGrant>>,
    /// Services.
    pub services: BTreeMap<ObjKey, Arc<Service>>,
    /// EndpointSlices, grouped by the Service they belong to.
    pub endpoint_slices: BTreeMap<ObjKey, Vec<Arc<EndpointSlice>>>,
    /// TLS Secrets (the controller only watches `kubernetes.io/tls`).
    pub secrets: BTreeMap<ObjKey, Arc<Secret>>,
    /// Namespace labels, for `allowedRoutes` selectors.
    pub namespaces: BTreeMap<String, BTreeMap<String, String>>,
    /// FerryPolicies.
    pub policies: BTreeMap<ObjKey, Arc<FerryPolicy>>,
    /// Proxy fleet state by Gateway.
    pub fleets: BTreeMap<ObjKey, Fleet>,
}

/// A manifest that could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    /// The YAML did not parse.
    #[error("invalid YAML: {0}")]
    Yaml(String),
    /// A document did not match its kind's schema.
    #[error("{kind} {name}: {source}")]
    Object {
        /// Object kind.
        kind: String,
        /// Object name.
        name: String,
        /// Why it did not deserialize.
        #[source]
        source: serde_json::Error,
    },
}

impl World {
    /// Builds a world from multi-document YAML manifests (what `kubectl
    /// apply -f` would take). Objects without a namespace land in `default`.
    /// Unknown kinds are skipped.
    ///
    /// # Errors
    ///
    /// Fails on malformed YAML or an object that does not match its schema.
    pub fn from_manifests(yaml: &str) -> Result<Self, ManifestError> {
        let docs: Vec<serde_json::Value> =
            serde_saphyr::from_multiple(yaml).map_err(|e| ManifestError::Yaml(e.to_string()))?;
        let mut world = Self::default();
        for mut doc in docs {
            if doc.is_null() {
                continue;
            }
            let kind = doc
                .get("kind")
                .and_then(|k| k.as_str())
                .unwrap_or_default()
                .to_owned();
            if let Some(meta) = doc.get_mut("metadata").and_then(|m| m.as_object_mut()) {
                meta.entry("namespace")
                    .or_insert_with(|| serde_json::Value::String("default".into()));
            }
            world.insert_value(&kind, doc)?;
        }
        Ok(world)
    }

    fn insert_value(&mut self, kind: &str, doc: serde_json::Value) -> Result<(), ManifestError> {
        fn parse<K: DeserializeOwned>(
            kind: &str,
            doc: serde_json::Value,
        ) -> Result<K, ManifestError> {
            let name = doc
                .pointer("/metadata/name")
                .and_then(|n| n.as_str())
                .unwrap_or_default()
                .to_owned();
            serde_json::from_value(doc).map_err(|source| ManifestError::Object {
                kind: kind.to_owned(),
                name,
                source,
            })
        }
        match kind {
            "GatewayClass" => {
                let gc: GatewayClass = parse(kind, doc)?;
                self.gateway_classes.insert(gc.name_any(), Arc::new(gc));
            }
            "Gateway" => self.insert(parse::<Gateway>(kind, doc)?),
            "HTTPRoute" => self.insert(parse::<HttpRoute>(kind, doc)?),
            "GRPCRoute" => self.insert(parse::<GrpcRoute>(kind, doc)?),
            "ReferenceGrant" => self.insert(parse::<ReferenceGrant>(kind, doc)?),
            "Service" => self.insert(parse::<Service>(kind, doc)?),
            "EndpointSlice" => self.insert(parse::<EndpointSlice>(kind, doc)?),
            "Secret" => self.insert(parse::<Secret>(kind, doc)?),
            "FerryPolicy" => self.insert(parse::<FerryPolicy>(kind, doc)?),
            "Namespace" => {
                let ns: Namespace = parse(kind, doc)?;
                self.namespaces.insert(ns.name_any(), ns.labels().clone());
            }
            _ => {}
        }
        Ok(())
    }

    /// Adds one object of a watched kind.
    pub fn insert<K: Insert>(&mut self, obj: K) {
        obj.insert_into(self);
    }

    /// Labels of `namespace`; empty when the namespace is unknown.
    #[must_use]
    pub fn namespace_labels(&self, namespace: &str) -> Option<&BTreeMap<String, String>> {
        self.namespaces.get(namespace)
    }
}

/// Objects that can be added to a [`World`].
pub trait Insert {
    /// Adds `self` to `world`.
    fn insert_into(self, world: &mut World);
}

macro_rules! insert_keyed {
    ($ty:ty, $field:ident) => {
        impl Insert for $ty {
            fn insert_into(self, world: &mut World) {
                world.$field.insert(ObjKey::of(&self), Arc::new(self));
            }
        }
    };
}

insert_keyed!(Gateway, gateways);
insert_keyed!(HttpRoute, http_routes);
insert_keyed!(GrpcRoute, grpc_routes);
insert_keyed!(ReferenceGrant, reference_grants);
insert_keyed!(Service, services);
insert_keyed!(Secret, secrets);
insert_keyed!(FerryPolicy, policies);

impl Insert for GatewayClass {
    fn insert_into(self, world: &mut World) {
        world
            .gateway_classes
            .insert(self.name_any(), Arc::new(self));
    }
}

impl Insert for EndpointSlice {
    fn insert_into(self, world: &mut World) {
        let Some(service) = self.labels().get(SERVICE_NAME_LABEL).cloned() else {
            return;
        };
        let key = ObjKey::new(self.namespace().unwrap_or_default(), service);
        world
            .endpoint_slices
            .entry(key)
            .or_default()
            .push(Arc::new(self));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_parse_and_print() {
        let k = ObjKey::parse("infra/web").unwrap();
        assert_eq!(k.to_string(), "infra/web");
        assert!(ObjKey::parse("noslash").is_none());
        assert!(ObjKey::parse("/x").is_none());
        assert!(ObjKey::parse("a/b/c").is_none());
    }

    #[test]
    fn manifests_load_by_kind() {
        let world = World::from_manifests(
            r"
apiVersion: v1
kind: Namespace
metadata: { name: apps, labels: { team: a } }
---
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: { name: gw, namespace: apps }
spec:
  gatewayClassName: ferry
  listeners: [{ name: http, port: 80, protocol: HTTP }]
---
apiVersion: discovery.k8s.io/v1
kind: EndpointSlice
metadata: { name: web-abc, labels: { kubernetes.io/service-name: web } }
addressType: IPv4
endpoints: [{ addresses: [10.0.0.1] }]
---
kind: ConfigMap
metadata: { name: ignored }
",
        )
        .unwrap();
        assert_eq!(world.namespace_labels("apps").unwrap()["team"], "a");
        assert!(world.gateways.contains_key(&ObjKey::new("apps", "gw")));
        assert_eq!(
            world.endpoint_slices[&ObjKey::new("default", "web")].len(),
            1
        );
    }

    #[test]
    fn bad_object_names_its_kind() {
        let err =
            World::from_manifests("kind: Gateway\nmetadata: { name: g }\nspec: 3\n").unwrap_err();
        assert!(err.to_string().starts_with("Gateway g"), "{err}");
    }
}
