//! Proxy errors.

use std::net::SocketAddr;

/// Failures that stop the proxy from starting or running.
#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    /// A listener address could not be bound.
    #[error("cannot bind listener on {address}: {source}")]
    Bind {
        /// Requested address.
        address: SocketAddr,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
}
