//! Gateway API types, generated from the official v1.6.3 standard-channel
//! CRDs by `gen/gateway_api.py` (kopium). Do not edit the submodules by hand.
//!
//! Only the resources Ferry watches are generated. Every enum carries an
//! `Unknown` variant, so a value introduced by a newer Gateway API release
//! makes one field unrecognised instead of failing the whole watch.

#![allow(
    missing_docs,
    clippy::all,
    clippy::pedantic,
    clippy::doc_markdown,
    rustdoc::bare_urls
)]

mod prelude {
    pub use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
    pub use kube::CustomResource;
    pub use serde::{Deserialize, Serialize};
    pub use std::collections::BTreeMap;
}

pub mod gatewayclasses;
pub mod gateways;
pub mod grpcroutes;
pub mod httproutes;
pub mod referencegrants;

/// The Gateway API group.
pub const GROUP: &str = "gateway.networking.k8s.io";

/// The Gateway API release these types were generated from, which is also
/// the newest release Ferry has been tested against.
pub const BUNDLE_VERSION: &str = "v1.6.3";
