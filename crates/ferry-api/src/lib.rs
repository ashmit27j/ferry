//! Protobuf types and gRPC stubs for Ferry's control-plane API.
//!
//! The service is defined in `proto/ferry/v1/snapshot.proto`. The controller
//! implements [`v1::snapshot_service_server::SnapshotService`]; proxies use
//! [`v1::snapshot_service_client::SnapshotServiceClient`].

/// `ferry.v1` package.
#[allow(
    missing_docs,
    missing_debug_implementations,
    clippy::all,
    clippy::pedantic
)]
pub mod v1 {
    tonic::include_proto!("ferry.v1");
}

#[cfg(test)]
mod tests {
    use super::v1::{SnapshotAck, SnapshotUpdate};
    use prost::Message;

    #[test]
    fn update_round_trips() {
        let update = SnapshotUpdate {
            version: 3,
            nonce: "n-3".into(),
            snapshot_json: br#"{"listeners":[]}"#.to_vec(),
        };
        let bytes = update.encode_to_vec();
        assert_eq!(SnapshotUpdate::decode(bytes.as_slice()).unwrap(), update);
    }

    #[test]
    fn empty_error_means_ack() {
        let ack = SnapshotAck {
            proxy_id: "ferry-proxy-0".into(),
            version: 3,
            nonce: "n-3".into(),
            error: String::new(),
        };
        assert!(ack.error.is_empty());
    }
}
