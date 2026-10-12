//! Error types for loading and validating configuration.

use std::fmt;
use std::path::PathBuf;

/// One validation problem, located by a field path such as
/// `listeners[0].routes[2].rules[0].backends[1].cluster`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    /// Dotted path to the offending field.
    pub path: String,
    /// What is wrong with it.
    pub message: String,
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

/// Every validation problem found in one configuration.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct ValidationErrors(pub Vec<ValidationError>);

impl fmt::Display for ValidationErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} validation error(s)", self.0.len())?;
        for e in &self.0 {
            write!(f, "\n  - {e}")?;
        }
        Ok(())
    }
}

/// Anything that can go wrong turning a file into a snapshot.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read.
    #[error("cannot read {path}: {source}")]
    Io {
        /// File that failed.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The input exceeds [`MAX_CONFIG_BYTES`](crate::MAX_CONFIG_BYTES).
    #[error("config is {len} bytes; the limit is {limit} bytes")]
    TooLarge {
        /// Input size.
        len: u64,
        /// Configured limit.
        limit: usize,
    },
    /// The input is not valid YAML for the config schema.
    #[error("cannot parse config: {0}")]
    Parse(String),
    /// The input parsed but failed validation.
    #[error(transparent)]
    Invalid(#[from] ValidationErrors),
}
