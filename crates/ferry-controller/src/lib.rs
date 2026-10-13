//! Ferry's control plane.
//!
//! In M4 this crate watches Gateway API resources, `FerryPolicy`, Services,
//! EndpointSlices and Secrets with kube-rs, reconciles them into validated
//! snapshots, writes status conditions back, and streams snapshots to proxies
//! over the `ferry.v1.SnapshotService` gRPC API defined in `ferry-api`.
//!
//! Until then, [`run`] reports that the controller is not available so the
//! `ferry controller` subcommand fails cleanly instead of pretending to work.

pub mod certgen;
pub mod gateway_api;
pub mod policy;
pub mod server;
pub mod translate;
pub mod world;

/// Errors from the controller.
#[derive(Debug, thiserror::Error)]
pub enum ControllerError {
    /// The controller is scheduled for milestone M4 and does not exist yet.
    #[error("the Kubernetes controller is not implemented yet (milestone M4)")]
    NotImplemented,
}

/// Runs the controller until shutdown.
///
/// # Errors
///
/// Always returns [`ControllerError::NotImplemented`] until M4.
pub async fn run() -> Result<(), ControllerError> {
    Err(ControllerError::NotImplemented)
}
