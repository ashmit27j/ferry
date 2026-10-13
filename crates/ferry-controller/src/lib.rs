//! Ferry's control plane: the Kubernetes Gateway API controller.
//!
//! `ferry controller` watches GatewayClasses, Gateways, HTTPRoutes,
//! GRPCRoutes, ReferenceGrants, Services, EndpointSlices, TLS Secrets,
//! Namespaces and [`FerryPolicies`](policy::FerryPolicy), and on every change:
//!
//! 1. [`translate`](translate::translate)s them, as a pure function, into
//!    one validated ferry-config per Gateway plus every status condition;
//! 2. publishes each config to that Gateway's proxies over the
//!    `ferry.v1.SnapshotService` gRPC stream ([`server`]), which proxies
//!    acknowledge or reject;
//! 3. as the elected [`leader`], writes status back and provisions each
//!    Gateway's proxy Deployment, Service, PodDisruptionBudget and HPA
//!    ([`provision`]).
//!
//! See ADRs 0032-0036.

pub mod certgen;
pub mod gateway_api;
pub mod leader;
pub mod metrics;
pub mod policy;
pub mod provision;
pub mod run;
pub mod server;
pub mod status;
pub mod translate;
pub mod world;

pub use run::{Options, run};

/// The default GatewayClass `controllerName`.
pub const CONTROLLER_NAME: &str = "gateway.ferry.dev/controller";

/// Errors that stop the controller.
#[derive(Debug, thiserror::Error)]
pub enum ControllerError {
    /// The Kubernetes API failed.
    #[error("kubernetes: {0}")]
    Kube(#[from] kube::Error),
    /// The snapshot server failed.
    #[error("snapshot server: {0}")]
    Serve(#[from] tonic::transport::Error),
    /// An address could not be bound or a file read.
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// Certificates could not be generated.
    #[error("certificates: {0}")]
    Certificates(#[from] rcgen::Error),
}
