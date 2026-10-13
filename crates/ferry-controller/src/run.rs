//! The controller process: watches, reconciles, serves (ADR 0035).
//!
//! - Reflectors keep a cache of every watched kind; any change wakes the
//!   reconcile loop, which waits [`DEBOUNCE`] for the burst to settle.
//! - Each pass builds a [`World`] from the caches and [`translate`]s it.
//!   Every replica publishes the resulting snapshots to its proxies.
//! - The leader also writes status and provisions proxy fleets, skipping
//!   anything already up to date, so a steady cluster sees no writes.
//! - A pass also runs every [`RESYNC`], to repair drift.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Debug;
use std::future::Future;
use std::hash::Hash;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::autoscaling::v2::HorizontalPodAutoscaler;
use k8s_openapi::api::coordination::v1::Lease;
use k8s_openapi::api::core::v1::{Namespace, Secret, Service};
use k8s_openapi::api::discovery::v1::EndpointSlice;
use k8s_openapi::api::policy::v1::PodDisruptionBudget;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use k8s_openapi::jiff::Timestamp;
use kube::api::{DeleteParams, Patch, PatchParams};
use kube::runtime::reflector::{self, Store};
use kube::runtime::{WatchStreamExt, watcher};
use kube::{Api, Client, Resource, ResourceExt};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::{Notify, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::gateway_api::gatewayclasses::GatewayClass;
use crate::gateway_api::gateways::Gateway;
use crate::gateway_api::grpcroutes::GrpcRoute;
use crate::gateway_api::httproutes::HttpRoute;
use crate::gateway_api::referencegrants::ReferenceGrant;
use crate::policy::FerryPolicy;
use crate::provision::{self, GATEWAY_ANNOTATION, MANAGED_BY, MANAGED_BY_LABEL, ProxyTemplate};
use crate::server::{self, Hub, TlsFiles};
use crate::translate::{RouteKind, Settings, Translation, translate};
use crate::world::{ObjKey, SERVICE_NAME_LABEL, World};
use crate::{ControllerError, status};

/// Quiet time after a change before reconciling.
pub const DEBOUNCE: Duration = Duration::from_millis(200);
/// Reconcile at least this often.
pub const RESYNC: Duration = Duration::from_secs(30);
/// Reapply unchanged proxy objects this often, to repair edits.
const REAPPLY: Duration = Duration::from_secs(600);
/// How long to wait for the first full list of every kind.
const SYNC_TIMEOUT: Duration = Duration::from_secs(60);
/// Field manager for server-side apply.
const MANAGER: &str = "ferry-controller";
/// Lease name.
const LEASE: &str = "ferry-controller";

/// How the controller runs.
#[derive(Debug, Clone)]
pub struct Options {
    /// The controller's namespace; proxy fleets and the Lease live here.
    pub namespace: String,
    /// This replica's identity (pod name).
    pub identity: String,
    /// GatewayClass `controllerName` to own.
    pub controller_name: String,
    /// Snapshot gRPC address.
    pub snapshot_address: SocketAddr,
    /// Health and debug address.
    pub admin_address: SocketAddr,
    /// mTLS for the snapshot server.
    pub tls: Option<TlsFiles>,
    /// How to deploy proxies.
    pub template: ProxyTemplate,
    /// Elect a leader; without it this replica always leads (tests, one replica).
    pub leader_election: bool,
}

/// Caches of every watched kind.
#[derive(Clone)]
struct Stores {
    classes: Store<GatewayClass>,
    gateways: Store<Gateway>,
    http_routes: Store<HttpRoute>,
    grpc_routes: Store<GrpcRoute>,
    grants: Store<ReferenceGrant>,
    services: Store<Service>,
    slices: Store<EndpointSlice>,
    secrets: Store<Secret>,
    namespaces: Store<Namespace>,
    policies: Store<FerryPolicy>,
    deployments: Store<Deployment>,
}

/// Starts a reflector for `api`, waking `notify` on every change.
fn reflect<K>(
    api: Api<K>,
    config: watcher::Config,
    notify: &Arc<Notify>,
    stop: &CancellationToken,
) -> Store<K>
where
    K: Resource + Clone + DeserializeOwned + Debug + Send + Sync + 'static,
    K::DynamicType: Default + Eq + Hash + Clone,
{
    let (reader, writer) = reflector::store();
    let stream = reflector::reflector(writer, watcher(api, config).default_backoff());
    let notify = Arc::clone(notify);
    let stop = stop.clone();
    tokio::spawn(async move {
        let mut stream = std::pin::pin!(stream);
        loop {
            tokio::select! {
                () = stop.cancelled() => break,
                event = stream.next() => match event {
                    Some(Ok(_)) => notify.notify_one(),
                    Some(Err(e)) => warn!(kind = %K::kind(&K::DynamicType::default()), error = %e, "watch failed; retrying"),
                    None => break,
                },
            }
        }
    });
    reader
}

impl Stores {
    fn start(
        client: &Client,
        namespace: &str,
        notify: &Arc<Notify>,
        stop: &CancellationToken,
    ) -> Self {
        let all = watcher::Config::default;
        let managed =
            || watcher::Config::default().labels(&format!("{MANAGED_BY_LABEL}={MANAGED_BY}"));
        Self {
            classes: reflect(Api::all(client.clone()), all(), notify, stop),
            gateways: reflect(Api::all(client.clone()), all(), notify, stop),
            http_routes: reflect(Api::all(client.clone()), all(), notify, stop),
            grpc_routes: reflect(Api::all(client.clone()), all(), notify, stop),
            grants: reflect(Api::all(client.clone()), all(), notify, stop),
            services: reflect(Api::all(client.clone()), all(), notify, stop),
            slices: reflect(Api::all(client.clone()), all(), notify, stop),
            // Only TLS Secrets: listener certificates and backend TLS.
            secrets: reflect(
                Api::all(client.clone()),
                all().fields("type=kubernetes.io/tls"),
                notify,
                stop,
            ),
            namespaces: reflect(Api::all(client.clone()), all(), notify, stop),
            policies: reflect(Api::all(client.clone()), all(), notify, stop),
            deployments: reflect(
                Api::namespaced(client.clone(), namespace),
                managed(),
                notify,
                stop,
            ),
        }
    }

    async fn wait_until_ready(&self) {
        let all = async {
            let _ = self.classes.wait_until_ready().await;
            let _ = self.gateways.wait_until_ready().await;
            let _ = self.http_routes.wait_until_ready().await;
            let _ = self.grpc_routes.wait_until_ready().await;
            let _ = self.grants.wait_until_ready().await;
            let _ = self.services.wait_until_ready().await;
            let _ = self.slices.wait_until_ready().await;
            let _ = self.secrets.wait_until_ready().await;
            let _ = self.namespaces.wait_until_ready().await;
            let _ = self.policies.wait_until_ready().await;
            let _ = self.deployments.wait_until_ready().await;
        };
        if tokio::time::timeout(SYNC_TIMEOUT, all).await.is_err() {
            warn!(
                "some kinds did not list within {SYNC_TIMEOUT:?} (are the Gateway API CRDs installed?); continuing"
            );
        }
    }

    /// A world from the caches. Proxy fleets come from our Deployments and
    /// Services in `namespace`.
    fn world(&self, namespace: &str) -> World {
        let mut w = World {
            gateway_classes: self
                .classes
                .state()
                .into_iter()
                .map(|c| (c.name_any(), c))
                .collect(),
            gateways: keyed(self.gateways.state()),
            http_routes: keyed(self.http_routes.state()),
            grpc_routes: keyed(self.grpc_routes.state()),
            reference_grants: keyed(self.grants.state()),
            services: keyed(self.services.state()),
            secrets: keyed(self.secrets.state()),
            policies: keyed(self.policies.state()),
            namespaces: self
                .namespaces
                .state()
                .into_iter()
                .map(|n| (n.name_any(), n.labels().clone()))
                .collect(),
            ..World::default()
        };
        for s in self.slices.state() {
            if let Some(svc) = s.labels().get(SERVICE_NAME_LABEL) {
                let key = ObjKey::new(s.namespace().unwrap_or_default(), svc.clone());
                w.endpoint_slices.entry(key).or_default().push(s);
            }
        }
        for d in self.deployments.state() {
            if let Some(gw) = d
                .annotations()
                .get(GATEWAY_ANNOTATION)
                .and_then(|g| ObjKey::parse(g))
            {
                let ready = d
                    .status
                    .as_ref()
                    .and_then(|s| s.ready_replicas)
                    .unwrap_or(0);
                w.fleets.entry(gw).or_default().ready_replicas = u32::try_from(ready).unwrap_or(0);
            }
        }
        for svc in self.services.state() {
            if svc.namespace().as_deref() != Some(namespace)
                || svc.labels().get(MANAGED_BY_LABEL).map(String::as_str) != Some(MANAGED_BY)
            {
                continue;
            }
            let Some(gw) = svc
                .annotations()
                .get(GATEWAY_ANNOTATION)
                .and_then(|g| ObjKey::parse(g))
            else {
                continue;
            };
            w.fleets.entry(gw).or_default().addresses = service_addresses(&svc);
        }
        w
    }
}

fn keyed<K: Resource>(objs: Vec<Arc<K>>) -> BTreeMap<ObjKey, Arc<K>> {
    objs.into_iter().map(|o| (ObjKey::of(&*o), o)).collect()
}

/// A proxy Service's addresses: its load balancer's, or its cluster IP for
/// `ClusterIP` Services.
fn service_addresses(svc: &Service) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for ing in svc
        .status
        .as_ref()
        .and_then(|s| s.load_balancer.as_ref())
        .and_then(|lb| lb.ingress.as_ref())
        .into_iter()
        .flatten()
    {
        if let Some(ip) = &ing.ip {
            out.push(("IPAddress".to_owned(), ip.clone()));
        } else if let Some(host) = &ing.hostname {
            out.push(("Hostname".to_owned(), host.clone()));
        }
    }
    let spec = svc.spec.as_ref();
    if out.is_empty()
        && spec.and_then(|s| s.type_.as_deref()) == Some("ClusterIP")
        && let Some(ip) = spec
            .and_then(|s| s.cluster_ip.as_ref())
            .filter(|ip| *ip != "None")
    {
        out.push(("IPAddress".to_owned(), ip.clone()));
    }
    out
}

/// What the leader last applied, to skip unchanged objects.
#[derive(Debug, Default)]
struct Applied {
    docs: HashMap<(String, String), (Value, tokio::time::Instant)>,
}

/// Runs the controller until `shutdown` resolves.
///
/// # Errors
///
/// Fails if the Kubernetes client cannot be built or a server cannot bind.
pub async fn run(
    opts: Options,
    shutdown: impl Future<Output = ()> + Send,
) -> Result<(), ControllerError> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let client = Client::try_default().await.map_err(ControllerError::Kube)?;
    let stop = CancellationToken::new();
    let notify = Arc::new(Notify::new());
    let hub = Hub::new();
    let synced = Arc::new(AtomicBool::new(false));
    let (leader_tx, leader_rx) = watch::channel(!opts.leader_election);

    let snapshot_listener = tokio::net::TcpListener::bind(opts.snapshot_address)
        .await
        .map_err(ControllerError::Io)?;
    let snapshots = tokio::spawn(server::serve(
        snapshot_listener,
        Arc::clone(&hub),
        opts.tls.clone(),
        stop.clone(),
    ));
    let admin_listener = tokio::net::TcpListener::bind(opts.admin_address)
        .await
        .map_err(ControllerError::Io)?;
    tokio::spawn(admin(
        admin_listener,
        Arc::clone(&hub),
        Arc::clone(&synced),
        leader_rx.clone(),
        stop.clone(),
    ));
    let election = opts.leader_election.then(|| {
        tokio::spawn(crate::leader::run(
            Api::<Lease>::namespaced(client.clone(), &opts.namespace),
            LEASE.to_owned(),
            opts.identity.clone(),
            leader_tx,
            stop.clone(),
        ))
    });

    let stores = Stores::start(&client, &opts.namespace, &notify, &stop);
    let reconciler = {
        let (client, opts, hub, synced, stop) = (
            client.clone(),
            opts.clone(),
            Arc::clone(&hub),
            Arc::clone(&synced),
            stop.clone(),
        );
        tokio::spawn(async move {
            stores.wait_until_ready().await;
            synced.store(true, Ordering::Relaxed);
            info!("caches synced; reconciling");
            reconcile_loop(&client, &opts, &stores, &hub, &notify, leader_rx, &stop).await;
        })
    };

    shutdown.await;
    info!("shutting down");
    stop.cancel();
    let _ = reconciler.await;
    if let Some(e) = election {
        let _ = e.await;
    }
    match snapshots.await {
        Ok(Ok(())) | Err(_) => Ok(()),
        Ok(Err(e)) => Err(ControllerError::Serve(e)),
    }
}

async fn reconcile_loop(
    client: &Client,
    opts: &Options,
    stores: &Stores,
    hub: &Hub,
    notify: &Notify,
    mut leader: watch::Receiver<bool>,
    stop: &CancellationToken,
) {
    let settings = Settings {
        controller_name: opts.controller_name.clone(),
        proxy_protocol: opts.template.proxy_protocol,
    };
    let mut applied = Applied::default();
    loop {
        let world = stores.world(&opts.namespace);
        let t = translate(&world, &settings);
        publish(hub, &t);
        if *leader.borrow_and_update() {
            write_statuses(client, &world, &t, &opts.controller_name).await;
            provision(client, opts, &world, &t, &mut applied).await;
        }
        tokio::select! {
            () = stop.cancelled() => return,
            () = notify.notified() => {}
            _ = leader.changed() => {}
            () = tokio::time::sleep(RESYNC) => {}
        }
        // Let a burst of events settle into one pass.
        tokio::select! {
            () = stop.cancelled() => return,
            () = tokio::time::sleep(DEBOUNCE) => {}
        }
    }
}

fn publish(hub: &Hub, t: &Translation) {
    for (key, g) in &t.gateways {
        if !g.valid {
            error!(gateway = %key, "not publishing a config that does not validate");
            continue;
        }
        match hub.publish(key, &g.config) {
            Ok(Some(version)) => info!(gateway = %key, version, "published snapshot"),
            Ok(None) => {}
            Err(e) => error!(gateway = %key, error = %e, "cannot serialize snapshot"),
        }
    }
    hub.retain(&t.gateways.keys().cloned().collect());
}

fn now() -> Time {
    // Kubernetes stores seconds; matching that avoids a rewrite next pass.
    Time(Timestamp::from_second(Timestamp::now().as_second()).unwrap_or_else(|_| Timestamp::now()))
}

async fn patch_status<K>(api: Api<K>, name: &str, status: Value, resource_version: Option<String>)
where
    K: Resource + Clone + DeserializeOwned + Debug,
{
    let mut body = json!({ "status": status });
    if let Some(rv) = resource_version {
        // Optimistic concurrency: another controller may write the same
        // object; a conflict is retried on the next pass.
        body["metadata"] = json!({ "resourceVersion": rv });
    }
    match api
        .patch_status(name, &PatchParams::default(), &Patch::Merge(body))
        .await
    {
        Ok(_) => debug!(kind = std::any::type_name::<K>(), name, "status written"),
        Err(kube::Error::Api(e)) if e.code == 409 => {
            debug!(name, "status conflict; retrying next pass")
        }
        Err(e) => warn!(kind = std::any::type_name::<K>(), name, error = %e, "cannot write status"),
    }
}

fn status_of<T: serde::Serialize>(s: Option<&T>) -> Value {
    s.and_then(|s| serde_json::to_value(s).ok())
        .unwrap_or(Value::Null)
}

async fn write_statuses(client: &Client, world: &World, t: &Translation, controller: &str) {
    let now = now();
    for (name, desired) in &t.classes {
        let Some(gc) = world.gateway_classes.get(name) else {
            continue;
        };
        if let Some(s) = status::class(&status_of(gc.status.as_ref()), desired, &now) {
            patch_status(Api::<GatewayClass>::all(client.clone()), name, s, None).await;
        }
    }
    for (key, desired) in &t.gateways {
        let Some(gw) = world.gateways.get(key) else {
            continue;
        };
        if let Some(s) = status::gateway(&status_of(gw.status.as_ref()), desired, &now) {
            patch_status(
                Api::<Gateway>::namespaced(client.clone(), &key.namespace),
                &key.name,
                s,
                None,
            )
            .await;
        }
    }
    for ((kind, key), desired) in &t.routes {
        match kind {
            RouteKind::Http => {
                let Some(r) = world.http_routes.get(key) else {
                    continue;
                };
                if let Some(s) =
                    status::route(&status_of(r.status.as_ref()), desired, controller, &now)
                {
                    let api = Api::<HttpRoute>::namespaced(client.clone(), &key.namespace);
                    patch_status(api, &key.name, s, r.resource_version()).await;
                }
            }
            RouteKind::Grpc => {
                let Some(r) = world.grpc_routes.get(key) else {
                    continue;
                };
                if let Some(s) =
                    status::route(&status_of(r.status.as_ref()), desired, controller, &now)
                {
                    let api = Api::<GrpcRoute>::namespaced(client.clone(), &key.namespace);
                    patch_status(api, &key.name, s, r.resource_version()).await;
                }
            }
        }
    }
    for (key, desired) in &t.policies {
        let Some(p) = world.policies.get(key) else {
            continue;
        };
        if let Some(s) = status::policy(&status_of(p.status.as_ref()), desired, &now) {
            patch_status(
                Api::<FerryPolicy>::namespaced(client.clone(), &key.namespace),
                &key.name,
                s,
                None,
            )
            .await;
        }
    }
}

async fn apply_doc(client: &Client, ns: &str, doc: &Value) -> Result<(), kube::Error> {
    let name = doc["metadata"]["name"].as_str().unwrap_or_default();
    let params = PatchParams::apply(MANAGER).force();
    let patch = Patch::Apply(doc);
    match doc["kind"].as_str().unwrap_or_default() {
        "Deployment" => Api::<Deployment>::namespaced(client.clone(), ns)
            .patch(name, &params, &patch)
            .await
            .map(drop),
        "Service" => Api::<Service>::namespaced(client.clone(), ns)
            .patch(name, &params, &patch)
            .await
            .map(drop),
        "PodDisruptionBudget" => Api::<PodDisruptionBudget>::namespaced(client.clone(), ns)
            .patch(name, &params, &patch)
            .await
            .map(drop),
        "HorizontalPodAutoscaler" => Api::<HorizontalPodAutoscaler>::namespaced(client.clone(), ns)
            .patch(name, &params, &patch)
            .await
            .map(drop),
        _ => Ok(()),
    }
}

async fn delete(client: &Client, ns: &str, kind: &str, name: &str) {
    let dp = DeleteParams::default();
    let result = match kind {
        "Deployment" => Api::<Deployment>::namespaced(client.clone(), ns)
            .delete(name, &dp)
            .await
            .map(drop),
        "Service" => Api::<Service>::namespaced(client.clone(), ns)
            .delete(name, &dp)
            .await
            .map(drop),
        "PodDisruptionBudget" => Api::<PodDisruptionBudget>::namespaced(client.clone(), ns)
            .delete(name, &dp)
            .await
            .map(drop),
        "HorizontalPodAutoscaler" => Api::<HorizontalPodAutoscaler>::namespaced(client.clone(), ns)
            .delete(name, &dp)
            .await
            .map(drop),
        _ => Ok(()),
    };
    match result {
        Ok(()) => info!(kind, name, "deleted"),
        Err(kube::Error::Api(e)) if e.code == 404 => {}
        Err(e) => warn!(kind, name, error = %e, "cannot delete"),
    }
}

const FLEET_KINDS: [&str; 4] = [
    "Deployment",
    "Service",
    "PodDisruptionBudget",
    "HorizontalPodAutoscaler",
];

/// Applies every accepted Gateway's fleet and removes fleets (and parts of
/// fleets) that are no longer wanted.
async fn provision(
    client: &Client,
    opts: &Options,
    world: &World,
    t: &Translation,
    applied: &mut Applied,
) {
    let ns = &opts.namespace;
    let mut wanted: BTreeSet<(String, String)> = BTreeSet::new();
    let mut fleets: BTreeMap<String, ObjKey> = BTreeMap::new();
    for (key, g) in &t.gateways {
        let accepted = g
            .conditions
            .iter()
            .any(|c| c.type_ == "Accepted" && c.status);
        if !accepted {
            continue;
        }
        fleets.insert(provision::resource_name(key), key.clone());
        for doc in provision::render(key, &g.ports, &opts.template, ns) {
            let id = (
                doc["kind"].as_str().unwrap_or_default().to_owned(),
                doc["metadata"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            );
            wanted.insert(id.clone());
            let fresh = applied
                .docs
                .get(&id)
                .is_some_and(|(d, at)| *d == doc && at.elapsed() < REAPPLY);
            if fresh {
                continue;
            }
            match apply_doc(client, ns, &doc).await {
                Ok(()) => {
                    debug!(kind = %id.0, name = %id.1, "applied");
                    applied.docs.insert(id, (doc, tokio::time::Instant::now()));
                }
                Err(e) => warn!(kind = %id.0, name = %id.1, error = %e, "cannot apply"),
            }
        }
    }
    // Fleets of Gateways that are gone or no longer accepted.
    let mut stale: BTreeSet<(String, String)> = BTreeSet::new();
    for d in world.fleets.keys() {
        let name = provision::resource_name(d);
        if !fleets.contains_key(&name) {
            for kind in FLEET_KINDS {
                stale.insert((kind.to_owned(), name.clone()));
            }
        }
    }
    // Parts of live fleets that the template or ports no longer call for.
    for name in fleets.keys() {
        for kind in FLEET_KINDS {
            let id = (kind.to_owned(), name.clone());
            if !wanted.contains(&id) && applied.docs.contains_key(&id) {
                stale.insert(id);
            }
        }
    }
    for (kind, name) in stale {
        delete(client, ns, &kind, &name).await;
        applied.docs.remove(&(kind, name));
    }
}

/// `/healthz`, `/readyz` (caches synced) and `/debug/proxies`.
async fn admin(
    listener: tokio::net::TcpListener,
    hub: Arc<Hub>,
    synced: Arc<AtomicBool>,
    leader: watch::Receiver<bool>,
    stop: CancellationToken,
) {
    use axum::Router;
    use axum::http::StatusCode;
    use axum::routing::get;
    let app = Router::new()
        .route("/healthz", get(|| async { "ok\n" }))
        .route(
            "/readyz",
            get(move || {
                let synced = Arc::clone(&synced);
                async move {
                    if synced.load(Ordering::Relaxed) {
                        (StatusCode::OK, "ready\n")
                    } else {
                        (StatusCode::SERVICE_UNAVAILABLE, "syncing caches\n")
                    }
                }
            }),
        )
        .route(
            "/debug/proxies",
            get(move || {
                let hub = Arc::clone(&hub);
                let leading = *leader.borrow();
                async move { axum::Json(json!({ "leader": leading, "proxies": hub.proxy_views() })) }
            }),
        );
    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(stop.cancelled_owned())
        .await
    {
        error!(error = %e, "admin server failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cluster_ip_services_report_their_ip() {
        let svc: Service = serde_json::from_value(json!({
            "spec": { "type": "ClusterIP", "clusterIP": "10.96.0.10" },
        }))
        .unwrap();
        assert_eq!(
            service_addresses(&svc),
            [("IPAddress".to_owned(), "10.96.0.10".to_owned())]
        );
        let lb: Service = serde_json::from_value(json!({
            "spec": { "type": "LoadBalancer", "clusterIP": "10.96.0.11" },
            "status": { "loadBalancer": { "ingress": [{ "hostname": "abc.elb.amazonaws.com" }, { "ip": "172.18.0.50" }] } },
        }))
        .unwrap();
        assert_eq!(
            service_addresses(&lb),
            [
                ("Hostname".to_owned(), "abc.elb.amazonaws.com".to_owned()),
                ("IPAddress".to_owned(), "172.18.0.50".to_owned())
            ]
        );
        let pending: Service =
            serde_json::from_value(json!({ "spec": { "type": "LoadBalancer" } })).unwrap();
        assert!(service_addresses(&pending).is_empty());
    }
}
