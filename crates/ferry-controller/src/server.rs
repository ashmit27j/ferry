//! The snapshot gRPC server (`ferry.v1.SnapshotService`, ADR 0033).
//!
//! The reconcile loop [`Hub::publish`]es one config per Gateway; every proxy
//! stream subscribed to that Gateway receives it. Streams are served by every
//! controller replica, leader or not: translation is deterministic, so each
//! replica publishes the same config for the same cluster state.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ferry_api::v1::snapshot_service_server::{SnapshotService, SnapshotServiceServer};
use ferry_api::v1::{SnapshotAck, SnapshotUpdate};
use ferry_config::Config;
use futures_util::Stream;
use serde::Serialize;
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, info, warn};

use crate::world::ObjKey;

/// Largest snapshot accepted or sent, in bytes. Certificates are inline, so
/// a Gateway with many listeners can exceed gRPC's 4 MiB default.
pub const MAX_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;

/// Longest proxy id kept, to bound memory per stream.
const MAX_PROXY_ID: usize = 253;

/// One Gateway's snapshot as proxies receive it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    /// Snapshot version.
    pub version: u64,
    /// Nonce acks must echo.
    pub nonce: String,
    /// `Config` JSON.
    pub json: Arc<Vec<u8>>,
}

/// A connected proxy, for the controller's debug endpoint.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ProxyView {
    /// Gateway it serves.
    pub gateway: String,
    /// Last version it accepted.
    pub acked_version: u64,
    /// Its last rejection, if the latest update was rejected.
    pub last_error: Option<String>,
}

/// Snapshots by Gateway, and who is listening.
#[derive(Debug, Default)]
pub struct Hub {
    channels: Mutex<HashMap<ObjKey, watch::Sender<Option<Arc<Published>>>>>,
    last_version: Mutex<u64>,
    proxies: Mutex<BTreeMap<String, ProxyView>>,
}

impl Hub {
    /// A hub with nothing published.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn channels(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<ObjKey, watch::Sender<Option<Arc<Published>>>>> {
        self.channels.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn proxies(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, ProxyView>> {
        self.proxies.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The next version: wall-clock milliseconds, or one more than the last
    /// version if the clock has not moved on, so versions always increase
    /// and a new controller replica continues above an old one.
    fn next_version(&self) -> u64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        let mut last = self
            .last_version
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *last = now.max(last.saturating_add(1));
        *last
    }

    /// Publishes `config` for `gateway` unless it is unchanged. Returns the
    /// new version, or `None` if nothing changed.
    ///
    /// # Errors
    ///
    /// Fails if the config cannot be serialised (never for valid configs).
    pub fn publish(
        &self,
        gateway: &ObjKey,
        config: &Config,
    ) -> Result<Option<u64>, serde_json::Error> {
        let json = serde_json::to_vec(config)?;
        let mut channels = self.channels();
        let tx = channels
            .entry(gateway.clone())
            .or_insert_with(|| watch::channel(None).0);
        if tx.borrow().as_ref().is_some_and(|p| *p.json == json) {
            return Ok(None);
        }
        let version = self.next_version();
        tx.send_replace(Some(Arc::new(Published {
            version,
            nonce: version.to_string(),
            json: Arc::new(json),
        })));
        Ok(Some(version))
    }

    /// Forgets Gateways not in `keep`. Their proxies stay connected and keep
    /// their last snapshot until they are deleted.
    pub fn retain(&self, keep: &BTreeSet<ObjKey>) {
        self.channels()
            .retain(|k, tx| keep.contains(k) || tx.receiver_count() > 0);
    }

    /// The current snapshot of `gateway`.
    #[must_use]
    pub fn current(&self, gateway: &ObjKey) -> Option<Arc<Published>> {
        self.channels()
            .get(gateway)
            .and_then(|tx| tx.borrow().clone())
    }

    fn subscribe(&self, gateway: &ObjKey) -> watch::Receiver<Option<Arc<Published>>> {
        self.channels()
            .entry(gateway.clone())
            .or_insert_with(|| watch::channel(None).0)
            .subscribe()
    }

    /// Connected proxies by id.
    #[must_use]
    pub fn proxy_views(&self) -> BTreeMap<String, ProxyView> {
        self.proxies().clone()
    }

    fn ack(&self, proxy: &str, gateway: &ObjKey, ack: &SnapshotAck) {
        let mut proxies = self.proxies();
        let view = proxies
            .entry(proxy.to_owned())
            .or_insert_with(|| ProxyView {
                gateway: gateway.to_string(),
                acked_version: 0,
                last_error: None,
            });
        if ack.error.is_empty() {
            view.acked_version = ack.version;
            view.last_error = None;
            debug!(proxy, %gateway, version = ack.version, "snapshot acknowledged");
        } else {
            view.last_error = Some(ack.error.clone());
            warn!(proxy, %gateway, nonce = %ack.nonce, error = %ack.error,
                "proxy rejected a snapshot; it keeps serving version {}", ack.version);
        }
    }

    fn disconnect(&self, proxy: &str) {
        self.proxies().remove(proxy);
    }
}

/// The gRPC service.
#[derive(Debug, Clone)]
pub struct SnapshotServer {
    hub: Arc<Hub>,
}

impl SnapshotServer {
    /// A service backed by `hub`.
    #[must_use]
    pub fn new(hub: Arc<Hub>) -> Self {
        Self { hub }
    }

    /// The tonic service, with size limits set.
    #[must_use]
    pub fn into_service(self) -> SnapshotServiceServer<Self> {
        SnapshotServiceServer::new(self)
            .max_decoding_message_size(MAX_SNAPSHOT_BYTES)
            .max_encoding_message_size(MAX_SNAPSHOT_BYTES)
    }
}

type UpdateStream = Pin<Box<dyn Stream<Item = Result<SnapshotUpdate, Status>> + Send>>;

fn update(p: &Published) -> SnapshotUpdate {
    SnapshotUpdate {
        version: p.version,
        nonce: p.nonce.clone(),
        snapshot_json: p.json.to_vec(),
    }
}

#[tonic::async_trait]
impl SnapshotService for SnapshotServer {
    type StreamSnapshotsStream = UpdateStream;

    async fn stream_snapshots(
        &self,
        request: Request<Streaming<SnapshotAck>>,
    ) -> Result<Response<Self::StreamSnapshotsStream>, Status> {
        let peer = request.remote_addr();
        let mut inbound = request.into_inner();
        let hello = inbound
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("the stream must start with a hello"))?;
        let gateway = ObjKey::parse(&hello.gateway)
            .ok_or_else(|| Status::invalid_argument("hello.gateway must be namespace/name"))?;
        if hello.proxy_id.is_empty() || hello.proxy_id.len() > MAX_PROXY_ID {
            return Err(Status::invalid_argument(
                "hello.proxy_id must be 1-253 characters",
            ));
        }
        let proxy = hello.proxy_id;
        info!(%proxy, %gateway, ?peer, version = hello.version, "proxy connected");
        let mut snapshots = self.hub.subscribe(&gateway);
        let (tx, rx) = mpsc::channel(2);
        let hub = Arc::clone(&self.hub);
        tokio::spawn(async move {
            let first = snapshots.borrow_and_update().clone();
            if let Some(p) = first
                && tx.send(Ok(update(&p))).await.is_err()
            {
                return;
            }
            loop {
                tokio::select! {
                    changed = snapshots.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        let next = snapshots.borrow_and_update().clone();
                        if let Some(p) = next
                            && tx.send(Ok(update(&p))).await.is_err()
                        {
                            break;
                        }
                    }
                    ack = inbound.message() => match ack {
                        Ok(Some(ack)) => hub.ack(&proxy, &gateway, &ack),
                        Ok(None) => break,
                        Err(e) => {
                            debug!(%proxy, error = %e, "proxy stream failed");
                            break;
                        }
                    },
                }
            }
            hub.disconnect(&proxy);
            info!(%proxy, %gateway, "proxy disconnected");
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}

/// PEM material for mutual TLS on the snapshot stream (ADR 0033).
#[derive(Debug, Clone)]
pub struct TlsFiles {
    /// CA that signs proxy client certificates.
    pub ca_pem: String,
    /// The server certificate chain.
    pub cert_pem: String,
    /// The server key.
    pub key_pem: String,
}

/// Serves the snapshot API on `addr` until `stop` is cancelled. With `tls`,
/// every client must present a certificate signed by its CA.
///
/// # Errors
///
/// Fails if the address cannot be bound or the TLS material is invalid.
pub async fn serve(
    addr: SocketAddr,
    hub: Arc<Hub>,
    tls: Option<TlsFiles>,
    stop: CancellationToken,
) -> Result<(), tonic::transport::Error> {
    let mut builder = Server::builder()
        .http2_keepalive_interval(Some(Duration::from_secs(30)))
        .http2_keepalive_timeout(Some(Duration::from_secs(10)));
    if let Some(t) = tls {
        builder = builder.tls_config(
            ServerTlsConfig::new()
                .identity(Identity::from_pem(t.cert_pem, t.key_pem))
                .client_ca_root(Certificate::from_pem(t.ca_pem)),
        )?;
    } else {
        warn!("snapshot server runs without TLS; certificates cross the network in clear text");
    }
    info!(%addr, "snapshot server listening");
    builder
        .add_service(SnapshotServer::new(hub).into_service())
        .serve_with_shutdown(addr, stop.cancelled_owned())
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_skips_unchanged_configs_and_versions_increase() {
        let hub = Hub::new();
        let gw = ObjKey::new("ns", "gw");
        let empty = Config::new(Vec::new(), Vec::new());
        let v1 = hub.publish(&gw, &empty).unwrap().unwrap();
        assert!(
            hub.publish(&gw, &empty).unwrap().is_none(),
            "same bytes, no update"
        );
        let mut other = Config::new(Vec::new(), Vec::new());
        other
            .clusters
            .push(ferry_config::ClusterConfig::new("c", Vec::new()));
        let v2 = hub.publish(&gw, &other).unwrap().unwrap();
        assert!(v2 > v1);
        assert_eq!(hub.current(&gw).unwrap().version, v2);
        assert!(
            v1 > 1_700_000_000_000,
            "versions start from wall-clock milliseconds"
        );
    }

    #[test]
    fn retain_drops_unsubscribed_gateways() {
        let hub = Hub::new();
        let (a, b) = (ObjKey::new("ns", "a"), ObjKey::new("ns", "b"));
        let empty = Config::new(Vec::new(), Vec::new());
        hub.publish(&a, &empty).unwrap();
        hub.publish(&b, &empty).unwrap();
        let _watching = hub.subscribe(&b);
        hub.retain(&BTreeSet::new());
        assert!(hub.current(&a).is_none());
        assert!(
            hub.current(&b).is_some(),
            "a connected proxy keeps its snapshot"
        );
    }

    #[test]
    fn acks_and_nacks_are_tracked() {
        let hub = Hub::new();
        let gw = ObjKey::new("ns", "gw");
        let mut ack = SnapshotAck {
            proxy_id: "p".into(),
            version: 5,
            nonce: "5".into(),
            error: String::new(),
            gateway: String::new(),
        };
        hub.ack("p", &gw, &ack);
        assert_eq!(hub.proxy_views()["p"].acked_version, 5);
        ack.error = "bad".into();
        ack.nonce = "6".into();
        hub.ack("p", &gw, &ack);
        let view = &hub.proxy_views()["p"];
        assert_eq!(
            (view.acked_version, view.last_error.as_deref()),
            (5, Some("bad"))
        );
        hub.disconnect("p");
        assert!(hub.proxy_views().is_empty());
    }
}
