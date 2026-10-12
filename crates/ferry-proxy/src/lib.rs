//! Ferry's data plane.
//!
//! [`Proxy::bind`] binds every listener in a [`ferry_config::Snapshot`];
//! [`Proxy::run`] serves HTTP/1.1 and HTTP/2 (prior-knowledge h2c) until a
//! shutdown future resolves, then drains. [`ReloadHandle::swap`] publishes a new
//! snapshot without dropping requests.
//!
//! Request path in M0: accept → hyper auto (h1/h2c) → [`Router`] → weighted
//! backend choice → round-robin endpoint → pooled HTTP/1.1 upstream. TLS, the
//! tower filter chain, health checking and the remaining balancers land in M1
//! and M2.

mod error;
mod forward;
pub mod router;
mod server;
mod state;

pub use error::ProxyError;
pub use forward::ProxyBody;
pub use router::Router;
pub use server::{DEFAULT_DRAIN_TIMEOUT, Proxy, ReloadHandle};
