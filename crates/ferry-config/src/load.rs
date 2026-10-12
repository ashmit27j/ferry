//! Reading configuration from YAML text or files.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::error::ConfigError;
use crate::model::Config;
use crate::snapshot::{Snapshot, SnapshotVersion};

/// Largest configuration file Ferry will read, in bytes.
pub const MAX_CONFIG_BYTES: usize = 4 * 1024 * 1024;

/// Parses YAML text into a [`Config`] without validating cross-references.
///
/// # Errors
///
/// Returns [`ConfigError::TooLarge`] for oversized input and
/// [`ConfigError::Parse`] for malformed YAML or unknown fields.
pub fn from_yaml_str(input: &str) -> Result<Config, ConfigError> {
    if input.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::TooLarge {
            len: input.len() as u64,
            limit: MAX_CONFIG_BYTES,
        });
    }
    serde_saphyr::from_str(input).map_err(|e| ConfigError::Parse(e.to_string()))
}

/// Reads and parses a YAML file into a [`Config`].
///
/// # Errors
///
/// Returns [`ConfigError::Io`] if the file cannot be read, plus anything
/// [`from_yaml_str`] returns.
pub fn load_file(path: &Path) -> Result<Config, ConfigError> {
    let io_err = |source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    };
    let file = File::open(path).map_err(io_err)?;
    let limit = MAX_CONFIG_BYTES as u64;
    let mut text = String::new();
    // Read one byte past the limit so an oversized file is detected without
    // trusting metadata, which can lie for pipes and procfs.
    file.take(limit + 1)
        .read_to_string(&mut text)
        .map_err(io_err)?;
    if text.len() as u64 > limit {
        return Err(ConfigError::TooLarge {
            len: text.len() as u64,
            limit: MAX_CONFIG_BYTES,
        });
    }
    from_yaml_str(&text)
}

/// Reads, parses and validates a file into a [`Snapshot`].
///
/// # Errors
///
/// Returns any error from [`load_file`] or [`ConfigError::Invalid`].
pub fn load_snapshot(path: &Path, version: SnapshotVersion) -> Result<Snapshot, ConfigError> {
    Ok(load_file(path)?.into_snapshot(version)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_fields() {
        let err = from_yaml_str("listeners: []\nclusters: []\nsurprise: 1\n").unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)), "{err}");
    }

    #[test]
    fn rejects_oversized_input() {
        let big = " ".repeat(MAX_CONFIG_BYTES + 1);
        assert!(matches!(
            from_yaml_str(&big),
            Err(ConfigError::TooLarge { .. })
        ));
    }

    #[test]
    fn rejects_bad_socket_address() {
        let yaml = "listeners: [{ name: a, address: 'not-an-addr' }]\nclusters: []\n";
        assert!(matches!(from_yaml_str(yaml), Err(ConfigError::Parse(_))));
    }

    #[test]
    fn missing_file_is_io_error() {
        let err = load_file(Path::new("definitely/not/here.yaml")).unwrap_err();
        assert!(matches!(err, ConfigError::Io { .. }));
    }
}
