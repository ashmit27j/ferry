//! Ferry's data plane.
//!
//! [`Proxy::bind`] binds every listener in a [`ferry_config::Snapshot`] and
//! starts its health checkers; [`Proxy::run`] serves HTTP/1.1 and HTTP/2
//! (prior-knowledge h2c) until a shutdown future resolves, then drains.
//! [`ReloadHandle::swap`] publishes a new snapshot without dropping requests,
//! and [`watch::watch_file`] does so whenever a config file changes.
//!
//! Request path: accept → hyper auto (h1/h2c) → [`Router`] (host index, then
//! path, method, header and query precedence) → redirect or request filters →
//! weighted backend choice → health-aware endpoint balancer → pooled HTTP/1.1
//! or h2c upstream, retrying on another endpoint when the request provably
//! never arrived. TLS, timeouts and configurable retries land in M2.

mod body;
mod error;
mod forward;
pub mod router;
mod server;
mod state;
pub mod watch;

pub use body::ProxyBody;
pub use error::ProxyError;
pub use forward::MAX_ATTEMPTS;
pub use router::Router;
pub use server::{DEFAULT_DRAIN_TIMEOUT, Proxy, ReloadHandle};
