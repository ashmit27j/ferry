//! The `ferry` binary: `proxy`, `controller` and `check-config`.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use ferry_config::{ConfigError, SnapshotVersion};
use ferry_telemetry::LogFormat;
use tracing::{error, info};

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// Ferry: a Kubernetes-native L7 load balancer and API gateway.
#[derive(Debug, Parser)]
#[command(version, about, propagate_version = true)]
struct Cli {
    /// Log format: "pretty" or "json". The filter comes from FERRY_LOG.
    #[arg(
        long,
        global = true,
        default_value = "pretty",
        env = "FERRY_LOG_FORMAT"
    )]
    log_format: LogFormat,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the data plane from a config file.
    Proxy {
        /// Path to the YAML config.
        #[arg(long, short)]
        config: PathBuf,
        /// Seconds to wait for in-flight requests on shutdown.
        #[arg(long, default_value_t = ferry_proxy::DEFAULT_DRAIN_TIMEOUT.as_secs())]
        drain_timeout_secs: u64,
    },
    /// Run the Kubernetes control plane (milestone M4).
    Controller,
    /// Validate a config file and exit non-zero if it is invalid.
    CheckConfig {
        /// Path to the YAML config. Omit with --schema.
        #[arg(required_unless_present = "schema")]
        config: Option<PathBuf>,
        /// Print the JSON Schema of the config format instead.
        #[arg(long)]
        schema: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Err(e) = ferry_telemetry::init_logging(cli.log_format) {
        report(&format!("cannot initialise logging: {e}"));
        return ExitCode::FAILURE;
    }
    match run(cli.command) {
        Ok(code) => code,
        Err(e) => {
            error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(command: Command) -> anyhow::Result<ExitCode> {
    match command {
        Command::CheckConfig { config, schema } => Ok(check_config(config, schema)),
        Command::Proxy {
            config,
            drain_timeout_secs,
        } => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("building tokio runtime")?;
            runtime.block_on(proxy(config, Duration::from_secs(drain_timeout_secs)))?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Controller => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("building tokio runtime")?;
            runtime.block_on(ferry_controller::run())?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

async fn proxy(config: PathBuf, drain_timeout: Duration) -> anyhow::Result<()> {
    let snapshot = ferry_config::load_snapshot(&config, SnapshotVersion(1))
        .with_context(|| format!("loading {}", config.display()))?;
    info!(config = %config.display(), version = %snapshot.version(), "config loaded");
    let proxy = ferry_proxy::Proxy::bind(snapshot).await?;
    proxy.run(shutdown_signal(), drain_timeout).await?;
    Ok(())
}

/// Resolves on Ctrl-C, or SIGTERM on Unix (what Kubernetes sends).
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            error!(error = %e, "cannot listen for ctrl-c");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    () = ctrl_c => {}
                    _ = term.recv() => {}
                }
            }
            Err(e) => {
                error!(error = %e, "cannot listen for SIGTERM");
                ctrl_c.await;
            }
        }
    }
    #[cfg(not(unix))]
    ctrl_c.await;
}

fn check_config(path: Option<PathBuf>, schema: bool) -> ExitCode {
    if schema {
        return match serde_json::to_string_pretty(&ferry_config::json_schema()) {
            Ok(s) => {
                out(&s);
                ExitCode::SUCCESS
            }
            Err(e) => {
                report(&format!("cannot render schema: {e}"));
                ExitCode::FAILURE
            }
        };
    }
    let Some(path) = path else {
        report("no config path given");
        return ExitCode::from(2);
    };
    match ferry_config::load_snapshot(&path, SnapshotVersion(0)) {
        Ok(snap) => {
            let routes: usize = snap.listeners().iter().map(|l| l.routes().len()).sum();
            out(&format!(
                "{}: OK ({} listener(s), {routes} route(s), {} cluster(s))",
                path.display(),
                snap.listeners().len(),
                snap.clusters().len(),
            ));
            ExitCode::SUCCESS
        }
        Err(ConfigError::Invalid(errors)) => {
            report(&format!("{}: invalid\n{errors}", path.display()));
            ExitCode::FAILURE
        }
        Err(e) => {
            report(&format!("{}: {e}", path.display()));
            ExitCode::FAILURE
        }
    }
}

/// Writes command output to stdout. `check-config` is a CLI, so printing is its job.
#[allow(clippy::print_stdout)]
fn out(message: &str) {
    println!("{message}");
}

/// Writes a user-facing error to stderr, bypassing log filters.
#[allow(clippy::print_stderr)]
fn report(message: &str) {
    eprintln!("{message}");
}
