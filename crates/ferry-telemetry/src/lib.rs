//! Telemetry for Ferry.
//!
//! M0 sets up structured logging. Prometheus metrics, OpenTelemetry tracing and
//! the access-log formatter arrive in M3.

use std::io::IsTerminal;
use std::str::FromStr;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::FmtSpan;

/// Environment variable read for the log filter, in `tracing` `EnvFilter` syntax.
pub const LOG_ENV: &str = "FERRY_LOG";

/// Default filter when [`LOG_ENV`] is unset.
pub const DEFAULT_FILTER: &str = "info";

/// Output format for logs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LogFormat {
    /// Human-readable, for terminals.
    #[default]
    Pretty,
    /// One JSON object per line, for log pipelines.
    Json,
}

impl FromStr for LogFormat {
    type Err = TelemetryError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pretty" => Ok(Self::Pretty),
            "json" => Ok(Self::Json),
            other => Err(TelemetryError::UnknownFormat(other.to_owned())),
        }
    }
}

/// Telemetry setup failures.
#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    /// The log format name is not recognised.
    #[error("unknown log format {0:?}; expected \"pretty\" or \"json\"")]
    UnknownFormat(String),
    /// The filter in [`LOG_ENV`] does not parse.
    #[error("invalid {LOG_ENV} filter: {0}")]
    BadFilter(String),
    /// A global subscriber was already installed.
    #[error("tracing subscriber already initialised: {0}")]
    AlreadyInitialised(String),
}

/// Installs the global tracing subscriber, writing to stderr.
///
/// # Errors
///
/// Fails if [`LOG_ENV`] holds an invalid filter or a subscriber is already set.
pub fn init_logging(format: LogFormat) -> Result<(), TelemetryError> {
    let filter = match std::env::var(LOG_ENV) {
        Ok(spec) => {
            EnvFilter::try_new(spec).map_err(|e| TelemetryError::BadFilter(e.to_string()))?
        }
        Err(_) => EnvFilter::new(DEFAULT_FILTER),
    };
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_span_events(FmtSpan::NONE);
    let result = match format {
        LogFormat::Pretty => builder.try_init(),
        LogFormat::Json => builder.json().flatten_event(true).try_init(),
    };
    result.map_err(|e| TelemetryError::AlreadyInitialised(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_formats() {
        assert_eq!("json".parse::<LogFormat>().unwrap(), LogFormat::Json);
        assert_eq!("pretty".parse::<LogFormat>().unwrap(), LogFormat::Pretty);
        assert!("xml".parse::<LogFormat>().is_err());
    }
}
