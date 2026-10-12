//! Ferry's configuration model.
//!
//! Configuration flows one way: YAML text → [`Config`] (parsed, unvalidated) →
//! [`Snapshot`] (validated, versioned, immutable). The data plane only ever
//! holds `Snapshot`s, behind an `ArcSwap`, so a bad file can never reach the
//! request path.
//!
//! ```
//! use ferry_config::{from_yaml_str, SnapshotVersion};
//!
//! let yaml = r#"
//! listeners:
//!   - name: http
//!     address: 127.0.0.1:8080
//!     routes:
//!       - name: hello
//!         rules:
//!           - backends: [{ cluster: hello }]
//! clusters:
//!   - name: hello
//!     endpoints: [{ address: 127.0.0.1:9001 }]
//! "#;
//! let snapshot = from_yaml_str(yaml)?.into_snapshot(SnapshotVersion(1))?;
//! assert_eq!(snapshot.listeners()[0].name(), "http");
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! The public API of this crate is not yet frozen; it freezes at the end of M1.

mod error;
mod load;
mod model;
mod snapshot;
mod validate;

pub use error::{ConfigError, ValidationError, ValidationErrors};
pub use load::{MAX_CONFIG_BYTES, from_yaml_str, load_file, load_snapshot};
pub use model::{
    BackendRefConfig, ClusterConfig, Config, EndpointConfig, LbPolicy, ListenerConfig, MatchConfig,
    PathMatchConfig, PathMatchKind, RouteConfig, RuleConfig,
};
pub use snapshot::{
    Cluster, ClusterId, Endpoint, HostMatch, HostSpecificity, Listener, PathMatch, RequestMatch,
    Route, Rule, Snapshot, SnapshotVersion, WeightedBackend,
};
pub use validate::{MAX_BACKEND_WEIGHT, MAX_NAME_LEN, MAX_PATH_LEN};

/// JSON Schema for the configuration file format, for editors and `check-config --schema`.
#[must_use]
pub fn json_schema() -> serde_json::Value {
    schemars::schema_for!(Config).to_value()
}
