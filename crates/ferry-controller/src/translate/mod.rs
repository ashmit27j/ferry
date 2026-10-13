//! Gateway API objects to per-Gateway Ferry configs and statuses (ADR 0032).
//!
//! [`translate`] is a pure function of a [`World`]: no I/O, no clock, no
//! randomness. The same world always yields the same [`Translation`], which
//! makes it the unit under test for Gateway API semantics and lets every
//! controller replica compute identical snapshots.
//!
//! For each Gateway of an accepted GatewayClass it:
//!
//! 1. validates listeners (protocol, port, certificates, route kinds,
//!    conflicts) and maps each Gateway port to a container port (ports
//!    below 1024 move up by 10000, so proxies run without privileges);
//! 2. attaches routes to listeners (`sectionName`, `port`, `allowedRoutes`,
//!    hostname intersection);
//! 3. resolves backend references to Service ports and their ready
//!    endpoints, honouring ReferenceGrants;
//! 4. applies FerryPolicies and builds one ferry-config [`Config`], with one
//!    Ferry listener per Gateway port.

mod backend;
mod hostname;
mod route;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;

use ferry_config::{
    BackendRefConfig, CertificateConfig, ClusterConfig, Config, EndpointConfig, ListenerConfig,
    ListenerTlsConfig, MAX_BACKEND_WEIGHT, RouteConfig, RuleConfig, UpstreamTlsConfig,
};
use kube::ResourceExt;

use crate::gateway_api::GROUP;
use crate::gateway_api::gateways::{
    Gateway, GatewayListeners, GatewayListenersAllowedRoutesNamespacesFrom,
    GatewayListenersAllowedRoutesNamespacesSelector, GatewayListenersTlsMode,
};
use crate::policy::{FerryPolicy, FerryPolicySpec, TargetKind};
use crate::world::{ObjKey, World};
use backend::{ClusterSpec, Problem};
use hostname::Hosts;
use route::{ParentRef, RouteIr};

/// Gateway API features Ferry implements, reported on the GatewayClass and
/// passed to the conformance suite.
pub const SUPPORTED_FEATURES: &[&str] = &[
    "GRPCRoute",
    "Gateway",
    "GatewayPort8080",
    "HTTPRoute",
    "HTTPRouteBackendProtocolH2C",
    "HTTPRouteBackendProtocolWebSocket",
    "HTTPRouteBackendTimeout",
    "HTTPRouteHostRewrite",
    "HTTPRouteMethodMatching",
    "HTTPRouteParentRefPort",
    "HTTPRoutePathRedirect",
    "HTTPRoutePathRewrite",
    "HTTPRoutePortRedirect",
    "HTTPRouteQueryParamMatching",
    "HTTPRouteRequestTimeout",
    "HTTPRouteResponseHeaderModification",
    "HTTPRouteSchemeRedirect",
    "ReferenceGrant",
];

/// Container ports proxies keep for themselves (the admin API).
pub const ADMIN_PORT: u16 = 19001;

/// The two route kinds Ferry serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RouteKind {
    /// `HTTPRoute`.
    Http,
    /// `GRPCRoute`.
    Grpc,
}

impl RouteKind {
    /// The Kubernetes kind.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Http => "HTTPRoute",
            Self::Grpc => "GRPCRoute",
        }
    }

    fn parse(group: Option<&str>, kind: &str) -> Option<Self> {
        if group.is_some_and(|g| g != GROUP) {
            return None;
        }
        match kind {
            "HTTPRoute" => Some(Self::Http),
            "GRPCRoute" => Some(Self::Grpc),
            _ => None,
        }
    }
}

/// Inputs that are not Kubernetes objects.
#[derive(Debug, Clone)]
pub struct Settings {
    /// `spec.controllerName` of the GatewayClasses this controller owns.
    pub controller_name: String,
    /// Proxies expect a PROXY protocol header (an NLB in front).
    pub proxy_protocol: bool,
}

/// One status condition, without timestamps (the status writer adds them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cond {
    /// Condition type, e.g. `Accepted`.
    pub type_: &'static str,
    /// True or False.
    pub status: bool,
    /// CamelCase reason.
    pub reason: String,
    /// Human-readable detail.
    pub message: String,
}

impl Cond {
    fn new(type_: &'static str, status: bool, reason: &str, message: impl Into<String>) -> Self {
        Self {
            type_,
            status,
            reason: reason.to_owned(),
            message: message.into(),
        }
    }

    fn ok(type_: &'static str, reason: &str) -> Self {
        Self::new(type_, true, reason, "")
    }

    fn from_problem(type_: &'static str, p: &Problem) -> Self {
        Self::new(type_, false, p.reason, p.message.clone())
    }
}

/// Everything one pass decides.
#[derive(Debug, Clone, Default)]
pub struct Translation {
    /// Status of every GatewayClass this controller owns.
    pub classes: BTreeMap<String, ClassStatus>,
    /// Configs and statuses of Gateways of accepted classes.
    pub gateways: BTreeMap<ObjKey, GatewayOutput>,
    /// For every route, its parent statuses for this controller (possibly
    /// none, so stale entries can be removed).
    pub routes: BTreeMap<(RouteKind, ObjKey), RouteStatus>,
    /// Status of every FerryPolicy.
    pub policies: BTreeMap<ObjKey, PolicyStatus>,
}

/// GatewayClass status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassStatus {
    /// `metadata.generation` the status describes.
    pub generation: i64,
    /// `Accepted`.
    pub conditions: Vec<Cond>,
}

/// One Gateway's result.
#[derive(Debug, Clone, PartialEq)]
pub struct GatewayOutput {
    /// `metadata.generation` the status describes.
    pub generation: i64,
    /// GatewayClass name.
    pub class: String,
    /// `Accepted` and `Programmed`.
    pub conditions: Vec<Cond>,
    /// Per listener, in spec order.
    pub listeners: Vec<ListenerStatus>,
    /// `status.addresses`: `(type, value)`.
    pub addresses: Vec<(String, String)>,
    /// The proxies' configuration, rate limits already split per replica.
    pub config: Config,
    /// Service ports the proxy Service exposes.
    pub ports: Vec<ProxyPort>,
}

/// One listener's status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenerStatus {
    /// Listener name.
    pub name: String,
    /// Route kinds the listener accepts.
    pub supported_kinds: Vec<RouteKind>,
    /// Routes attached to it.
    pub attached_routes: i32,
    /// `Accepted`, `ResolvedRefs`, `Conflicted`, `Programmed`.
    pub conditions: Vec<Cond>,
}

/// A port of the proxy Service.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProxyPort {
    /// Port name (`http-80`).
    pub name: String,
    /// The Gateway (Service) port.
    pub port: u16,
    /// The port proxies bind.
    pub container_port: u16,
}

/// A route's status for this controller.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouteStatus {
    /// `metadata.generation` the status describes.
    pub generation: i64,
    /// One entry per parentRef that names a Gateway of this controller.
    pub parents: Vec<ParentStatus>,
}

/// Status for one parentRef.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParentStatus {
    /// The parentRef, with group, kind and namespace filled in.
    pub group: String,
    /// Parent kind.
    pub kind: String,
    /// Parent namespace.
    pub namespace: String,
    /// Parent name.
    pub name: String,
    /// `sectionName`, as written.
    pub section_name: Option<String>,
    /// `port`, as written.
    pub port: Option<i32>,
    /// `Accepted` and `ResolvedRefs`.
    pub conditions: Vec<Cond>,
}

/// A FerryPolicy's status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyStatus {
    /// `metadata.generation` the status describes.
    pub generation: i64,
    /// `Accepted`.
    pub conditions: Vec<Cond>,
}

/// The container port proxies bind for a Gateway port: ports below 1024 move
/// up by 10000, so proxies need no `NET_BIND_SERVICE`.
#[must_use]
pub fn container_port(port: u16) -> u16 {
    if port < 1024 { port + 10_000 } else { port }
}

/// Translates a world (see the module docs).
#[must_use]
pub fn translate(world: &World, settings: &Settings) -> Translation {
    let mut out = Translation::default();
    let classes = classes(world, settings, &mut out);
    let policies = Policies::resolve(world, &mut out.policies);

    let mut routes: Vec<RouteIr> = world
        .http_routes
        .values()
        .map(|r| RouteIr::from_http(r))
        .chain(world.grpc_routes.values().map(|r| RouteIr::from_grpc(r)))
        .collect();
    // Gateway API precedence ties go to the oldest route, then by name.
    routes.sort_by(|a, b| {
        let (ta, ka) = a.order_key();
        let (tb, kb) = b.order_key();
        ta.map(|t| t.0)
            .cmp(&tb.map(|t| t.0))
            .then_with(|| ka.cmp(kb))
            .then_with(|| a.kind.cmp(&b.kind))
    });
    let resolved: Vec<Vec<Vec<Result<ClusterSpec, Problem>>>> = routes
        .iter()
        .map(|r| {
            r.rules
                .iter()
                .map(|rule| {
                    rule.backends
                        .iter()
                        .map(|b| backend::resolve(world, r.kind, &r.key.namespace, b))
                        .collect()
                })
                .collect()
        })
        .collect();

    let mut gateways: BTreeMap<ObjKey, GatewayState<'_>> = world
        .gateways
        .iter()
        .filter(|(_, g)| classes.contains(g.spec.gateway_class_name.as_str()))
        .map(|(k, g)| (k.clone(), GatewayState::new(world, k, g, &policies)))
        .collect();

    for (ri, r) in routes.iter().enumerate() {
        let mut status = RouteStatus {
            generation: r.generation,
            parents: Vec::new(),
        };
        let refs_problem = resolved[ri].iter().flatten().find_map(|b| b.as_ref().err());
        for p in &r.parents {
            let Some(state) = p.gateway().and_then(|k| gateways.get_mut(&k)) else {
                continue;
            };
            let accepted = state.attach(world, ri, r, p);
            let refs = refs_problem.map_or_else(
                || Cond::ok("ResolvedRefs", "ResolvedRefs"),
                |p| Cond::from_problem("ResolvedRefs", p),
            );
            status.parents.push(ParentStatus {
                group: p.group.clone(),
                kind: p.kind.clone(),
                namespace: p.namespace.clone(),
                name: p.name.clone(),
                section_name: p.section_name.clone(),
                port: p.port,
                conditions: vec![accepted, refs],
            });
        }
        out.routes.insert((r.kind, r.key.clone()), status);
    }

    for (key, state) in gateways {
        let output = state.finish(world, settings, &routes, &resolved, &policies);
        out.gateways.insert(key, output);
    }
    out
}

/// Records GatewayClass statuses; returns the names of accepted classes.
fn classes<'w>(world: &'w World, settings: &Settings, out: &mut Translation) -> BTreeSet<&'w str> {
    let mut accepted = BTreeSet::new();
    for (name, gc) in &world.gateway_classes {
        if gc.spec.controller_name != settings.controller_name {
            continue;
        }
        let cond = if gc.spec.parameters_ref.is_some() {
            Cond::new(
                "Accepted",
                false,
                "InvalidParameters",
                "Ferry's GatewayClass takes no parametersRef; configure proxies with Helm values",
            )
        } else {
            accepted.insert(name.as_str());
            Cond::ok("Accepted", "Accepted")
        };
        out.classes.insert(
            name.clone(),
            ClassStatus {
                generation: gc.metadata.generation.unwrap_or_default(),
                conditions: vec![cond],
            },
        );
    }
    accepted
}

/// The FerryPolicy in effect for each target.
#[derive(Debug, Default)]
struct Policies<'w> {
    by_target: BTreeMap<(TargetKind, ObjKey), &'w FerryPolicy>,
}

impl<'w> Policies<'w> {
    fn resolve(world: &'w World, out: &mut BTreeMap<ObjKey, PolicyStatus>) -> Self {
        let mut ordered: Vec<(&ObjKey, &'w FerryPolicy)> = world
            .policies
            .iter()
            .map(|(k, p)| (k, p.as_ref()))
            .collect();
        ordered.sort_by(|(ka, a), (kb, b)| {
            let ta = a.metadata.creation_timestamp.as_ref().map(|t| t.0);
            let tb = b.metadata.creation_timestamp.as_ref().map(|t| t.0);
            ta.cmp(&tb).then_with(|| ka.cmp(kb))
        });
        let mut me = Self::default();
        for (key, policy) in ordered {
            let mut problem: Option<(&str, String)> = None;
            for t in &policy.spec.target_refs {
                let Some(kind) = t.target_kind() else {
                    problem.get_or_insert((
                        "Invalid",
                        format!("unsupported target {}/{}", t.group, t.kind),
                    ));
                    continue;
                };
                let target = ObjKey::new(key.namespace.clone(), t.name.clone());
                let exists = match kind {
                    TargetKind::Gateway => world.gateways.contains_key(&target),
                    TargetKind::HttpRoute => world.http_routes.contains_key(&target),
                    TargetKind::GrpcRoute => world.grpc_routes.contains_key(&target),
                    TargetKind::Service => world.services.contains_key(&target),
                };
                if !exists {
                    problem.get_or_insert((
                        "TargetNotFound",
                        format!("{} {target} not found", t.kind),
                    ));
                    continue;
                }
                if let Err(why) = policy.spec.check(kind) {
                    problem.get_or_insert(("Invalid", why));
                    continue;
                }
                if let Some(winner) = me.by_target.get(&(kind, target.clone())) {
                    problem.get_or_insert((
                        "Conflicted",
                        format!(
                            "{} {target} already has FerryPolicy {}",
                            t.kind,
                            winner.name_any()
                        ),
                    ));
                    continue;
                }
                me.by_target.insert((kind, target), policy);
            }
            let cond = match problem {
                None => Cond::ok("Accepted", "Accepted"),
                Some((reason, message)) => Cond::new("Accepted", false, reason, message),
            };
            out.insert(
                key.clone(),
                PolicyStatus {
                    generation: policy.metadata.generation.unwrap_or_default(),
                    conditions: vec![cond],
                },
            );
        }
        me
    }

    fn get(&self, kind: TargetKind, key: &ObjKey) -> Option<&'w FerryPolicySpec> {
        self.by_target.get(&(kind, key.clone())).map(|p| &p.spec)
    }
}

/// Which namespaces a listener accepts routes from.
#[derive(Debug, Clone)]
enum Allowed<'w> {
    Same,
    All,
    Selector(&'w GatewayListenersAllowedRoutesNamespacesSelector),
}

/// A listener being validated.
#[derive(Debug)]
struct ListenerState<'w> {
    spec: &'w GatewayListeners,
    port: u16,
    container_port: u16,
    https: bool,
    hostname: Option<String>,
    allowed: Allowed<'w>,
    kinds: Vec<RouteKind>,
    /// `Accepted=False` reason, if not accepted.
    not_accepted: Option<Problem>,
    /// `ResolvedRefs=False` reason, if refs failed.
    unresolved: Option<Problem>,
    /// `Conflicted=True` reason, if in conflict.
    conflict: Option<Problem>,
    /// Valid certificate chains and keys (PEM), for HTTPS.
    certs: Vec<(String, String)>,
    attached: i32,
}

impl ListenerState<'_> {
    /// Whether the proxies serve this listener.
    fn served(&self) -> bool {
        self.not_accepted.is_none()
            && self.conflict.is_none()
            && (!self.https || !self.certs.is_empty())
    }
}

/// A route attached to a listener.
#[derive(Debug)]
struct Attachment {
    route: usize,
    listener: usize,
    hosts: Hosts,
}

/// A Gateway being translated.
#[derive(Debug)]
struct GatewayState<'w> {
    key: ObjKey,
    gw: &'w Gateway,
    listeners: Vec<ListenerState<'w>>,
    attachments: Vec<Attachment>,
    policy: Option<&'w FerryPolicySpec>,
}

impl<'w> GatewayState<'w> {
    fn new(world: &'w World, key: &ObjKey, gw: &'w Gateway, policies: &Policies<'w>) -> Self {
        let mut listeners: Vec<ListenerState<'w>> = gw
            .spec
            .listeners
            .iter()
            .map(|l| listener(world, key, l))
            .collect();
        detect_conflicts(&mut listeners);
        Self {
            key: key.clone(),
            gw,
            listeners,
            attachments: Vec::new(),
            policy: policies.get(TargetKind::Gateway, key),
        }
    }

    /// Attaches route `ri` through parentRef `p`; returns its `Accepted`.
    fn attach(&mut self, world: &World, ri: usize, r: &RouteIr, p: &ParentRef) -> Cond {
        if let Some(why) = &r.unsupported {
            return Cond::new("Accepted", false, "UnsupportedValue", why.clone());
        }
        let candidates: Vec<usize> = self
            .listeners
            .iter()
            .enumerate()
            .filter(|(_, l)| p.section_name.as_ref().is_none_or(|s| *s == l.spec.name))
            .filter(|(_, l)| p.port.is_none_or(|port| port == i32::from(l.port)))
            .map(|(i, _)| i)
            .collect();
        if candidates.is_empty() {
            return Cond::new(
                "Accepted",
                false,
                "NoMatchingParent",
                format!(
                    "Gateway {} has no listener matching the parentRef",
                    self.key
                ),
            );
        }
        let allowed: Vec<usize> = candidates
            .into_iter()
            .filter(|&i| {
                let l = &self.listeners[i];
                l.not_accepted.is_none()
                    && l.kinds.contains(&r.kind)
                    && namespace_allowed(world, &l.allowed, &self.key.namespace, &r.key.namespace)
            })
            .collect();
        if allowed.is_empty() {
            return Cond::new(
                "Accepted",
                false,
                "NotAllowedByListeners",
                format!("no listener of Gateway {} allows this route", self.key),
            );
        }
        let mut any = false;
        for i in allowed {
            let l = &mut self.listeners[i];
            if let Some(hosts) = hostname::intersect(l.hostname.as_deref(), &r.hostnames) {
                l.attached += 1;
                self.attachments.push(Attachment {
                    route: ri,
                    listener: i,
                    hosts,
                });
                any = true;
            }
        }
        if any {
            Cond::ok("Accepted", "Accepted")
        } else {
            Cond::new(
                "Accepted",
                false,
                "NoMatchingListenerHostname",
                "no listener hostname intersects the route's hostnames",
            )
        }
    }

    fn finish(
        self,
        world: &World,
        settings: &Settings,
        routes: &[RouteIr],
        resolved: &[Vec<Vec<Result<ClusterSpec, Problem>>>],
        policies: &Policies<'_>,
    ) -> GatewayOutput {
        let fleet = world.fleets.get(&self.key).cloned().unwrap_or_default();
        let replicas = fleet.ready_replicas.max(1);
        let (config, ports, invalid) =
            self.build(world, settings, routes, resolved, policies, replicas);

        let listener_statuses: Vec<ListenerStatus> = self
            .listeners
            .iter()
            .map(|l| {
                let accepted = l.not_accepted.as_ref().map_or_else(
                    || Cond::ok("Accepted", "Accepted"),
                    |p| Cond::from_problem("Accepted", p),
                );
                let resolved = l.unresolved.as_ref().map_or_else(
                    || Cond::ok("ResolvedRefs", "ResolvedRefs"),
                    |p| Cond::from_problem("ResolvedRefs", p),
                );
                let conflicted = l.conflict.as_ref().map_or_else(
                    || Cond::new("Conflicted", false, "NoConflicts", ""),
                    |p| Cond::new("Conflicted", true, p.reason, p.message.clone()),
                );
                let programmed = if l.served() && l.unresolved.is_none() {
                    Cond::ok("Programmed", "Programmed")
                } else {
                    Cond::new("Programmed", false, "Invalid", "the listener is not valid")
                };
                ListenerStatus {
                    name: l.spec.name.clone(),
                    supported_kinds: l.kinds.clone(),
                    attached_routes: l.attached,
                    conditions: vec![accepted, resolved, conflicted, programmed],
                }
            })
            .collect();

        let invalid_listeners = self
            .listeners
            .iter()
            .filter(|l| l.not_accepted.is_some() || l.conflict.is_some())
            .count();
        let any_accepted = self.listeners.iter().any(|l| l.not_accepted.is_none());
        let accepted = if self
            .gw
            .spec
            .addresses
            .as_ref()
            .is_some_and(|a| !a.is_empty())
        {
            Cond::new(
                "Accepted",
                false,
                "UnsupportedAddress",
                "Ferry assigns addresses from its LoadBalancer Service; spec.addresses is not supported",
            )
        } else if !any_accepted {
            Cond::new(
                "Accepted",
                false,
                "ListenersNotValid",
                "no listener is valid",
            )
        } else if invalid_listeners > 0 {
            Cond::new(
                "Accepted",
                true,
                "ListenersNotValid",
                format!("{invalid_listeners} listener(s) are not valid"),
            )
        } else {
            Cond::ok("Accepted", "Accepted")
        };
        let programmed = if !accepted.status {
            Cond::new(
                "Programmed",
                false,
                "Invalid",
                "the Gateway is not accepted",
            )
        } else if let Some(why) = invalid {
            Cond::new("Programmed", false, "Invalid", why)
        } else if fleet.ready_replicas == 0 {
            Cond::new(
                "Programmed",
                false,
                "Pending",
                "waiting for proxy pods to become ready",
            )
        } else if fleet.addresses.is_empty() {
            Cond::new(
                "Programmed",
                false,
                "AddressNotAssigned",
                "waiting for the proxy Service to get an address",
            )
        } else {
            Cond::ok("Programmed", "Programmed")
        };
        GatewayOutput {
            generation: self.gw.metadata.generation.unwrap_or_default(),
            class: self.gw.spec.gateway_class_name.clone(),
            conditions: vec![accepted, programmed],
            listeners: listener_statuses,
            addresses: fleet.addresses,
            config,
            ports,
        }
    }

    /// Builds the proxies' config. The third value explains why the config
    /// does not validate, which would be a translator bug.
    fn build(
        &self,
        world: &World,
        settings: &Settings,
        routes: &[RouteIr],
        resolved: &[Vec<Vec<Result<ClusterSpec, Problem>>>],
        policies: &Policies<'_>,
        replicas: u32,
    ) -> (Config, Vec<ProxyPort>, Option<String>) {
        let mut by_port: BTreeMap<u16, Vec<usize>> = BTreeMap::new();
        for (i, l) in self.listeners.iter().enumerate() {
            if l.served() {
                by_port.entry(l.port).or_default().push(i);
            }
        }
        let mut clusters: BTreeSet<ClusterSpec> = BTreeSet::new();
        let mut listeners = Vec::new();
        let mut ports = Vec::new();
        for (port, members) in &by_port {
            let first = &self.listeners[members[0]];
            let scheme = if first.https { "https" } else { "http" };
            let name = format!("{scheme}-{port}");
            let addr = SocketAddr::from(([0, 0, 0, 0], first.container_port));
            let mut fl = ListenerConfig::new(name.clone(), addr);
            fl.advertised_port = Some(*port);
            fl.proxy_protocol = settings.proxy_protocol;
            if first.https {
                let mut seen = BTreeSet::new();
                let certs = members
                    .iter()
                    .flat_map(|&i| self.listeners[i].certs.iter())
                    .filter(|(c, _)| seen.insert(c.clone()))
                    .map(|(c, k)| CertificateConfig::pem(c.clone(), k.clone()))
                    .collect();
                fl.tls = Some(ListenerTlsConfig::new(certs));
            }
            if let Some(p) = self.policy
                && let Err(e) = p.apply_to_listener(&mut fl)
            {
                tracing::warn!(gateway = %self.key, error = %e, "ignoring listener policy");
            }
            // Attachments on this port, merged per route in precedence order.
            let mut per_route: BTreeMap<usize, Hosts> = BTreeMap::new();
            for a in self
                .attachments
                .iter()
                .filter(|a| members.contains(&a.listener))
            {
                let merged = match per_route.remove(&a.route) {
                    Some(h) => hostname::union(h, a.hosts.clone()),
                    None => a.hosts.clone(),
                };
                per_route.insert(a.route, merged);
            }
            for (ri, hosts) in per_route {
                let r = &routes[ri];
                let kind = match r.kind {
                    RouteKind::Http => TargetKind::HttpRoute,
                    RouteKind::Grpc => TargetKind::GrpcRoute,
                };
                let policy = policies.get(kind, &r.key);
                fl.routes.push(self.route(
                    r,
                    &resolved[ri],
                    hosts,
                    policy,
                    replicas,
                    &mut clusters,
                ));
            }
            ports.push(ProxyPort {
                name,
                port: *port,
                container_port: first.container_port,
            });
            listeners.push(fl);
        }
        let clusters = clusters
            .into_iter()
            .map(|spec| cluster(world, &spec, policies))
            .collect();
        let config = Config::new(listeners, clusters);
        let invalid = config
            .clone()
            .into_snapshot(ferry_config::SnapshotVersion(0))
            .err()
            .map(|e| e.to_string());
        if let Some(e) = &invalid {
            tracing::error!(gateway = %self.key, error = %e, "translated config does not validate");
        }
        (config, ports, invalid)
    }

    fn route(
        &self,
        r: &RouteIr,
        resolved: &[Vec<Result<ClusterSpec, Problem>>],
        hosts: Hosts,
        policy: Option<&FerryPolicySpec>,
        replicas: u32,
        clusters: &mut BTreeSet<ClusterSpec>,
    ) -> RouteConfig {
        let rules = r
            .rules
            .iter()
            .zip(resolved)
            .map(|(rule, refs)| {
                let mut out = RuleConfig::default();
                out.matches.clone_from(&rule.matches);
                out.filters.clone_from(&rule.filters);
                if !rule.redirect {
                    let mut invalid: u64 = 0;
                    for (b, res) in rule.backends.iter().zip(refs) {
                        match res {
                            Ok(spec) => {
                                out.backends
                                    .push(BackendRefConfig::new(spec.name(), b.weight));
                                clusters.insert(spec.clone());
                            }
                            Err(_) => invalid += u64::from(b.weight),
                        }
                    }
                    if out.backends.is_empty() {
                        // Gateway API: a rule with no valid backendRefs (none
                        // at all, or only invalid ones of weight 0) answers 500.
                        invalid = invalid.max(1);
                    }
                    out.invalid_backend_weight =
                        u32::try_from(invalid.min(u64::from(MAX_BACKEND_WEIGHT)))
                            .unwrap_or(MAX_BACKEND_WEIGHT);
                }
                out.timeouts.clone_from(&rule.timeouts);
                if let Some(p) = policy {
                    if let Ok(Some(idle)) = p.idle_timeout() {
                        out.timeouts.get_or_insert_with(Default::default).idle = Some(idle);
                    }
                    if let Ok(retry) = p.retry_config() {
                        out.retry = retry;
                    }
                }
                out
            })
            .collect();
        let mut fr = RouteConfig::new(route::route_name(r.kind, &r.key), rules);
        if let Hosts::Some(h) = hosts {
            fr.hostnames = h;
        }
        if let Some(p) = policy {
            fr.rate_limit = p.rate_limit(replicas).ok().flatten();
        }
        fr
    }
}

/// Builds the cluster for a Service port: its ready endpoints and any
/// FerryPolicy targeting the Service.
fn cluster(world: &World, spec: &ClusterSpec, policies: &Policies<'_>) -> ClusterConfig {
    let endpoints = backend::endpoints(world, spec)
        .into_iter()
        .map(EndpointConfig::new)
        .collect();
    let mut c = ClusterConfig::new(spec.name(), endpoints);
    c.protocol = spec.protocol();
    let Some(policy) = policies.get(TargetKind::Service, &spec.service) else {
        return c;
    };
    if let Err(e) = policy.apply_to_cluster(&mut c) {
        tracing::warn!(cluster = %spec.name(), error = %e, "ignoring backend policy");
    }
    if let Some(tls) = &policy.backend_tls {
        match upstream_tls(world, &spec.service.namespace, tls) {
            Ok(t) => c.tls = Some(t),
            Err(e) => {
                // Fail closed: never fall back to plaintext.
                tracing::warn!(cluster = %spec.name(), error = %e, "backend TLS unusable; cluster has no endpoints");
                c.endpoints.clear();
            }
        }
    }
    c
}

fn upstream_tls(
    world: &World,
    namespace: &str,
    p: &crate::policy::BackendTlsPolicy,
) -> Result<UpstreamTlsConfig, String> {
    let mut t = UpstreamTlsConfig::default();
    t.sni.clone_from(&p.sni);
    let secret = |name: &String| {
        world
            .secrets
            .get(&ObjKey::new(namespace, name.clone()))
            .ok_or_else(|| format!("Secret {namespace}/{name} not found"))
    };
    if let Some(name) = &p.ca_secret {
        t.ca_pem = Some(backend::ca_bundle(secret(name)?).map_err(|e| format!("{name}: {e}"))?);
    }
    if let Some(name) = &p.client_certificate_secret {
        let (cert, key) = backend::tls_pair(secret(name)?).map_err(|e| format!("{name}: {e}"))?;
        t.client_certificate = Some(CertificateConfig::pem(cert, key));
    }
    Ok(t)
}

fn namespace_allowed(world: &World, allowed: &Allowed<'_>, gw_ns: &str, route_ns: &str) -> bool {
    match allowed {
        Allowed::Same => gw_ns == route_ns,
        Allowed::All => true,
        Allowed::Selector(sel) => {
            let empty = BTreeMap::new();
            let labels = world.namespace_labels(route_ns).unwrap_or(&empty);
            selector_matches(sel, labels)
        }
    }
}

fn selector_matches(
    sel: &GatewayListenersAllowedRoutesNamespacesSelector,
    labels: &BTreeMap<String, String>,
) -> bool {
    let labels_ok = sel
        .match_labels
        .iter()
        .flatten()
        .all(|(k, v)| labels.get(k) == Some(v));
    let exprs_ok = sel.match_expressions.iter().flatten().all(|e| {
        let values = e.values.as_deref().unwrap_or_default();
        let have = labels.get(&e.key);
        match e.operator.as_str() {
            "In" => have.is_some_and(|v| values.contains(v)),
            "NotIn" => have.is_none_or(|v| !values.contains(v)),
            "Exists" => have.is_some(),
            "DoesNotExist" => have.is_none(),
            _ => false,
        }
    });
    labels_ok && exprs_ok
}

/// Validates one listener (everything except conflicts and attachments).
fn listener<'w>(world: &'w World, gw: &ObjKey, l: &'w GatewayListeners) -> ListenerState<'w> {
    let port = u16::try_from(l.port).unwrap_or(0);
    let mut s = ListenerState {
        spec: l,
        port,
        container_port: container_port(port),
        https: false,
        hostname: l.hostname.clone(),
        allowed: Allowed::Same,
        kinds: Vec::new(),
        not_accepted: None,
        unresolved: None,
        conflict: None,
        certs: Vec::new(),
        attached: 0,
    };
    match l.protocol.as_str() {
        "HTTP" => {}
        "HTTPS" => s.https = true,
        other => {
            s.not_accepted = Some(Problem::new(
                "UnsupportedProtocol",
                format!("protocol {other} is not supported; Ferry serves HTTP and HTTPS"),
            ));
            return s;
        }
    }
    if port == 0 {
        s.not_accepted = Some(Problem::new(
            "PortUnavailable",
            format!("port {} is out of range", l.port),
        ));
        return s;
    }
    if s.container_port == ADMIN_PORT {
        s.not_accepted = Some(Problem::new(
            "PortUnavailable",
            format!(
                "port {port} maps to container port {ADMIN_PORT}, which the proxy's admin API uses"
            ),
        ));
        return s;
    }
    let routes = l.allowed_routes.as_ref();
    s.allowed = match routes.and_then(|r| r.namespaces.as_ref()) {
        Some(ns) => match (&ns.from, &ns.selector) {
            (Some(GatewayListenersAllowedRoutesNamespacesFrom::All), _) => Allowed::All,
            (Some(GatewayListenersAllowedRoutesNamespacesFrom::Selector), Some(sel)) => {
                Allowed::Selector(sel)
            }
            _ => Allowed::Same,
        },
        None => Allowed::Same,
    };
    match routes.and_then(|r| r.kinds.as_ref()) {
        None => s.kinds = vec![RouteKind::Http, RouteKind::Grpc],
        Some(kinds) => {
            for k in kinds {
                match RouteKind::parse(k.group.as_deref(), &k.kind) {
                    Some(kind) if !s.kinds.contains(&kind) => s.kinds.push(kind),
                    Some(_) => {}
                    None => {
                        s.unresolved.get_or_insert(Problem::new(
                            "InvalidRouteKinds",
                            format!("route kind {} is not supported", k.kind),
                        ));
                    }
                }
            }
        }
    }
    if s.https {
        certificates(world, gw, l, &mut s);
    }
    s
}

fn certificates(world: &World, gw: &ObjKey, l: &GatewayListeners, s: &mut ListenerState<'_>) {
    let Some(tls) = &l.tls else {
        s.unresolved = Some(Problem::new(
            "InvalidCertificateRef",
            "HTTPS listener without tls",
        ));
        return;
    };
    if matches!(
        tls.mode,
        Some(GatewayListenersTlsMode::Passthrough | GatewayListenersTlsMode::Unknown)
    ) {
        s.not_accepted = Some(Problem::new(
            "UnsupportedProtocol",
            "only TLS mode Terminate is supported on HTTPS listeners",
        ));
        return;
    }
    for r in tls.certificate_refs.iter().flatten() {
        let group = r.group.as_deref().unwrap_or_default();
        let kind = r.kind.as_deref().unwrap_or("Secret");
        if !(group.is_empty() || group == "core") || kind != "Secret" {
            s.unresolved.get_or_insert(Problem::new(
                "InvalidCertificateRef",
                format!("certificateRef {group}/{kind} {} is not a Secret", r.name),
            ));
            continue;
        }
        let key = ObjKey::new(
            r.namespace.clone().unwrap_or_else(|| gw.namespace.clone()),
            r.name.clone(),
        );
        if !backend::permitted(world, "Gateway", &gw.namespace, "", "Secret", &key) {
            s.unresolved.get_or_insert(Problem::new(
                "RefNotPermitted",
                format!(
                    "no ReferenceGrant in {} allows a reference to Secret {key}",
                    key.namespace
                ),
            ));
            continue;
        }
        let Some(secret) = world.secrets.get(&key) else {
            s.unresolved.get_or_insert(Problem::new(
                "InvalidCertificateRef",
                format!("TLS Secret {key} not found"),
            ));
            continue;
        };
        match backend::tls_pair(secret) {
            Ok(pair) => s.certs.push(pair),
            Err(e) => {
                s.unresolved.get_or_insert(Problem::new(
                    "InvalidCertificateRef",
                    format!("Secret {key} {e}"),
                ));
            }
        }
    }
    if s.certs.is_empty() && s.unresolved.is_none() {
        s.unresolved = Some(Problem::new("InvalidCertificateRef", "no certificateRefs"));
    }
}

/// Marks listeners that cannot share a port: a different protocol than the
/// first listener on the port, the same hostname twice, or two Gateway
/// ports that map to one container port. The first listener wins.
fn detect_conflicts(listeners: &mut [ListenerState<'_>]) {
    let mut first_on_port: BTreeMap<u16, (bool, BTreeSet<Option<String>>)> = BTreeMap::new();
    let mut container: BTreeMap<u16, u16> = BTreeMap::new();
    for l in listeners.iter_mut().filter(|l| l.not_accepted.is_none()) {
        if let Some(&other) = container.get(&l.container_port)
            && other != l.port
        {
            l.not_accepted = Some(Problem::new(
                "PortUnavailable",
                format!(
                    "ports {other} and {} map to the same container port {}",
                    l.port, l.container_port
                ),
            ));
            continue;
        }
        container.insert(l.container_port, l.port);
        let hostname = l.hostname.clone().map(|h| h.to_ascii_lowercase());
        match first_on_port.get_mut(&l.port) {
            None => {
                first_on_port.insert(l.port, (l.https, BTreeSet::from([hostname])));
            }
            Some((https, _)) if *https != l.https => {
                l.conflict = Some(Problem::new(
                    "ProtocolConflict",
                    format!("port {} is already used with another protocol", l.port),
                ));
            }
            Some((_, hosts)) => {
                if !hosts.insert(hostname) {
                    l.conflict = Some(Problem::new(
                        "HostnameConflict",
                        format!("another listener on port {} has the same hostname", l.port),
                    ));
                }
            }
        }
    }
}
