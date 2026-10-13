//! Snapshots from the controller (ADR 0033).
//!
//! [`follow`] keeps one `ferry.v1.SnapshotService/StreamSnapshots` call open
//! to the controller. Each update is parsed and validated exactly like a
//! config file, swapped in, and acknowledged; a snapshot that fails is
//! rejected (NACK) and the proxy keeps serving the last good one. When the
//! stream breaks, the proxy keeps serving and reconnects with jittered
//! exponential backoff.

use std::time::Duration;

use ferry_api::v1::snapshot_service_client::SnapshotServiceClient;
use ferry_api::v1::{SnapshotAck, SnapshotUpdate};
use ferry_config::{Config, SnapshotVersion};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};
use tracing::{info, warn};

use crate::server::ReloadHandle;

/// Largest snapshot accepted, in bytes (certificates are inline).
pub const MAX_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;

const BACKOFF_MIN: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(10);

/// Where snapshots come from.
#[derive(Debug, Clone)]
pub struct RemoteSource {
    /// Controller URL, e.g. `https://ferry-controller.ferry-system:18000`.
    pub endpoint: String,
    /// The Gateway this proxy serves, as `namespace/name`.
    pub gateway: String,
    /// This proxy's identity (its pod name).
    pub proxy_id: String,
    /// Mutual TLS settings; `None` speaks plaintext (tests, local runs).
    pub tls: Option<RemoteTls>,
}

/// Client-side mutual TLS for the snapshot stream.
#[derive(Debug, Clone)]
pub struct RemoteTls {
    /// CA that signed the controller's certificate.
    pub ca_pem: String,
    /// This proxy's client certificate chain.
    pub cert_pem: String,
    /// Its key.
    pub key_pem: String,
    /// Name to verify in the controller's certificate.
    pub server_name: String,
}

/// Why a stream ended.
#[derive(Debug, thiserror::Error)]
enum StreamError {
    #[error("cannot connect: {0}")]
    Connect(#[from] tonic::transport::Error),
    #[error("stream failed: {0}")]
    Status(#[from] tonic::Status),
    #[error("controller closed the stream")]
    Closed,
}

/// Follows the controller until `stop` is cancelled.
pub async fn follow(source: RemoteSource, handle: ReloadHandle, stop: CancellationToken) {
    let mut backoff = BACKOFF_MIN;
    loop {
        let delivered = tokio::select! {
            () = stop.cancelled() => return,
            result = stream(&source, &handle) => match result {
                Ok(()) => return,
                Err((delivered, e)) => {
                    warn!(endpoint = %source.endpoint, error = %e,
                        "lost the controller; serving the last snapshot and reconnecting");
                    delivered
                }
            },
        };
        if delivered {
            backoff = BACKOFF_MIN;
        }
        let jittered = backoff.mul_f64(0.5 + fastrand::f64() / 2.0);
        tokio::select! {
            () = stop.cancelled() => return,
            () = tokio::time::sleep(jittered) => {}
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

async fn channel(source: &RemoteSource) -> Result<Channel, tonic::transport::Error> {
    let mut endpoint = Endpoint::from_shared(source.endpoint.clone())?
        .connect_timeout(Duration::from_secs(5))
        .http2_keep_alive_interval(Duration::from_secs(30))
        .keep_alive_timeout(Duration::from_secs(10))
        .keep_alive_while_idle(true);
    if let Some(tls) = &source.tls {
        endpoint = endpoint.tls_config(
            ClientTlsConfig::new()
                .ca_certificate(Certificate::from_pem(&tls.ca_pem))
                .identity(Identity::from_pem(&tls.cert_pem, &tls.key_pem))
                .domain_name(tls.server_name.clone()),
        )?;
    }
    endpoint.connect().await
}

/// One stream. On failure, says whether any update arrived first (to reset
/// the backoff).
async fn stream(source: &RemoteSource, handle: &ReloadHandle) -> Result<(), (bool, StreamError)> {
    let channel = channel(source).await.map_err(|e| (false, e.into()))?;
    let mut client =
        SnapshotServiceClient::new(channel).max_decoding_message_size(MAX_SNAPSHOT_BYTES);
    let (acks, rx) = mpsc::channel(4);
    let hello = SnapshotAck {
        proxy_id: source.proxy_id.clone(),
        version: handle.current_version().0,
        nonce: String::new(),
        error: String::new(),
        gateway: source.gateway.clone(),
    };
    // The receiver lives in the request stream, so this cannot fail yet.
    let _ = acks.send(hello).await;
    let mut updates = client
        .stream_snapshots(ReceiverStream::new(rx))
        .await
        .map_err(|e| (false, e.into()))?
        .into_inner();
    info!(endpoint = %source.endpoint, gateway = %source.gateway, "connected to the controller");
    let mut delivered = false;
    loop {
        let update = match updates.message().await {
            Ok(Some(u)) => u,
            Ok(None) => return Err((delivered, StreamError::Closed)),
            Err(e) => return Err((delivered, e.into())),
        };
        delivered = true;
        let ack = apply(handle, &update);
        if acks.send(ack).await.is_err() {
            return Err((delivered, StreamError::Closed));
        }
    }
}

/// Validates and swaps in one update; returns the ack or nack to send.
fn apply(handle: &ReloadHandle, update: &SnapshotUpdate) -> SnapshotAck {
    let result = serde_json::from_slice::<Config>(&update.snapshot_json)
        .map_err(|e| format!("snapshot does not parse: {e}"))
        .and_then(|c| {
            c.into_snapshot(SnapshotVersion(update.version))
                .map_err(|e| e.to_string())
        })
        .and_then(|s| handle.swap(s).map_err(|e| e.to_string()));
    let error = match result {
        Ok(()) => String::new(),
        Err(e) => {
            handle.rejected();
            warn!(version = update.version, error = %e, "rejected snapshot; keeping the current one");
            e
        }
    };
    SnapshotAck {
        proxy_id: String::new(),
        version: handle.current_version().0,
        nonce: update.nonce.clone(),
        error,
        gateway: String::new(),
    }
}
