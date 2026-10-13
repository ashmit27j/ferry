//! The `ferry` binary: `proxy`, `controller`, `certgen`, `crds` and `check-config`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use ferry_config::{ConfigError, SnapshotVersion};
use ferry_telemetry::access_log::{self, AccessLog, Sink};
use ferry_telemetry::trace::TraceConfig;
use ferry_telemetry::{LogFormat, Telemetry, Tracing};
use tracing::{error, info, warn};

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
    /// Run the data plane, from a config file or from the controller.
    Proxy {
        /// Path to the YAML config (standalone mode).
        #[arg(
            long,
            short,
            required_unless_present = "controller",
            conflicts_with = "controller"
        )]
        config: Option<PathBuf>,
        #[command(flatten)]
        remote: RemoteArgs,
        /// Seconds to wait for in-flight requests on shutdown.
        #[arg(long, default_value_t = ferry_proxy::DEFAULT_DRAIN_TIMEOUT.as_secs())]
        drain_timeout_secs: u64,
        /// Seconds to keep accepting after SIGTERM, closing client connections
        /// as their requests finish, before the listeners close. Set it to
        /// cover the load balancer's deregistration delay (the Helm chart does).
        #[arg(long, default_value_t = 0)]
        drain_delay_secs: u64,
        /// Milliseconds between checks of the config file for changes; 0
        /// disables hot reload.
        #[arg(long, default_value_t = 1000)]
        watch_interval_ms: u64,
        #[command(flatten)]
        observability: Observability,
    },
    /// Run the Kubernetes Gateway API controller.
    Controller(ControllerArgs),
    /// Create or renew the certificates of the controller-to-proxy stream
    /// (run by the Helm chart as a hook Job).
    Certgen {
        /// Namespace of the Secrets.
        #[arg(long, env = "POD_NAMESPACE", default_value = "ferry-system")]
        namespace: String,
        /// Secret for the controller's server certificate.
        #[arg(long, default_value = "ferry-controller-tls")]
        controller_secret: String,
        /// Secret for the proxies' client certificate.
        #[arg(long, default_value = "ferry-proxy-tls")]
        proxy_secret: String,
        /// DNS names of the controller Service (repeat the flag).
        #[arg(long = "dns-name", required = true)]
        dns_names: Vec<String>,
    },
    /// Print the FerryPolicy CustomResourceDefinition (JSON, which is valid YAML).
    Crds,
    /// Validate a config file and exit non-zero if it is invalid.
    CheckConfig {
        /// Path to the YAML config. Omit with --schema.
        #[arg(required_unless_present = "schema")]
        config: Option<PathBuf>,
        /// Print the JSON Schema of the config format instead.
        #[arg(long)]
        schema: bool,
        /// Output format: "text", or "json" for CI pipelines.
        #[arg(long, default_value = "text")]
        output: Output,
        /// Also print the validated config, normalised and with secrets
        /// redacted, as JSON (what the admin API's /config shows).
        #[arg(long, conflicts_with = "schema")]
        print: bool,
    },
}

/// `ferry proxy` in Kubernetes: follow the controller instead of a file.
#[derive(Debug, Clone, clap::Args)]
struct RemoteArgs {
    /// Controller snapshot endpoint, e.g. <https://ferry-controller.ferry-system.svc:18000>.
    #[arg(long, requires = "gateway")]
    controller: Option<String>,
    /// The Gateway this proxy serves, as namespace/name.
    #[arg(long)]
    gateway: Option<String>,
    /// This proxy's identity in acks (the pod name).
    #[arg(long, env = "POD_NAME", default_value = "ferry-proxy")]
    proxy_id: String,
    /// Directory with ca.crt, tls.crt and tls.key for mutual TLS to the
    /// controller. Without it the stream is plaintext (development only).
    #[arg(long)]
    controller_tls_dir: Option<PathBuf>,
    /// Name to verify in the controller's certificate.
    #[arg(long, default_value = "ferry-controller.ferry-system.svc")]
    controller_server_name: String,
}

/// `ferry controller`.
#[derive(Debug, clap::Args)]
struct ControllerArgs {
    /// The controller's namespace; proxy fleets and the leader Lease live here.
    #[arg(long, env = "POD_NAMESPACE", default_value = "ferry-system")]
    namespace: String,
    /// This replica's identity for leader election (the pod name).
    #[arg(long, env = "POD_NAME", default_value = "ferry-controller")]
    identity: String,
    /// GatewayClass spec.controllerName to own.
    #[arg(long, default_value = ferry_controller::CONTROLLER_NAME)]
    controller_name: String,
    /// Address of the snapshot gRPC server proxies connect to.
    #[arg(long, default_value = "0.0.0.0:18000")]
    snapshot_address: SocketAddr,
    /// Address of /healthz, /readyz and /debug/proxies.
    #[arg(long, default_value = "0.0.0.0:18001")]
    health_address: SocketAddr,
    /// Directory with ca.crt, tls.crt and tls.key for the snapshot server;
    /// proxies must present certificates signed by that CA.
    #[arg(long)]
    tls_dir: Option<PathBuf>,
    /// YAML file describing proxy Deployments and Services (the Helm chart
    /// writes it from its values).
    #[arg(long)]
    proxy_template: Option<PathBuf>,
    /// Lead without electing (a single replica).
    #[arg(long)]
    no_leader_election: bool,
}

/// Where `ferry proxy` gets its configuration.
#[derive(Debug)]
enum Source {
    File(PathBuf),
    Controller(RemoteArgs),
}

/// Admin API, metrics, tracing and access-log settings of `ferry proxy`.
#[derive(Debug, clap::Args)]
struct Observability {
    /// Address of the admin API (/ready, /live, /metrics, /config,
    /// /clusters), or "off". Bind the pod IP in Kubernetes so probes and
    /// Prometheus can reach it.
    #[arg(long, default_value = ferry_proxy::admin::DEFAULT_ADMIN_ADDRESS, env = "FERRY_ADMIN_ADDRESS")]
    admin_address: AdminAddress,
    /// Where JSON access logs go: "stdout", "stderr", a file path, or "off".
    #[arg(long, default_value = "off", env = "FERRY_ACCESS_LOG")]
    access_log: String,
    /// OTLP/gRPC endpoint for traces, e.g. http://otel-collector:4317.
    /// Tracing is off when unset.
    #[arg(long, env = "OTEL_EXPORTER_OTLP_ENDPOINT")]
    otlp_endpoint: Option<String>,
    /// service.name of Ferry's spans.
    #[arg(long, default_value = "ferry", env = "OTEL_SERVICE_NAME")]
    service_name: String,
    /// Fraction of new traces to sample (0-1). Requests with a traceparent
    /// follow the caller's decision.
    #[arg(long, default_value_t = 1.0, env = "FERRY_TRACE_SAMPLE_RATIO")]
    trace_sample_ratio: f64,
}

/// `--admin-address`: an address, or `off`.
#[derive(Debug, Clone, Copy)]
struct AdminAddress(Option<SocketAddr>);

impl std::str::FromStr for AdminAddress {
    type Err = std::net::AddrParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "off" {
            Ok(Self(None))
        } else {
            s.parse().map(|a| Self(Some(a)))
        }
    }
}

/// `check-config --output`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Output {
    Text,
    Json,
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
        Command::CheckConfig {
            config,
            schema,
            output,
            print,
        } => Ok(check_config(config, schema, output, print)),
        Command::Proxy {
            config,
            remote,
            drain_timeout_secs,
            drain_delay_secs,
            watch_interval_ms,
            observability,
        } => {
            let source = match (config, &remote.controller) {
                (Some(path), _) => Source::File(path),
                (None, Some(_)) => Source::Controller(remote),
                (None, None) => anyhow::bail!("give --config or --controller"),
            };
            tokio_runtime()?.block_on(proxy(
                source,
                ferry_proxy::Drain {
                    delay: Duration::from_secs(drain_delay_secs),
                    timeout: Duration::from_secs(drain_timeout_secs),
                },
                Duration::from_millis(watch_interval_ms),
                observability,
            ))?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Controller(args) => {
            tokio_runtime()?.block_on(controller(args))?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Certgen {
            namespace,
            controller_secret,
            proxy_secret,
            dns_names,
        } => {
            tokio_runtime()?.block_on(async {
                let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
                let client = kube::Client::try_default().await?;
                ferry_controller::certgen::ensure(
                    client,
                    &namespace,
                    &controller_secret,
                    &proxy_secret,
                    &dns_names,
                )
                .await?;
                Ok::<_, anyhow::Error>(())
            })?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Crds => {
            use kube::CustomResourceExt;
            out(&serde_json::to_string_pretty(
                &ferry_controller::policy::FerryPolicy::crd(),
            )?);
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn tokio_runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")
}

/// Reads `ca.crt`, `tls.crt` and `tls.key` from a mounted Secret.
fn tls_dir(dir: &std::path::Path) -> anyhow::Result<(String, String, String)> {
    let read = |f: &str| {
        let path = dir.join(f);
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
    };
    Ok((read("ca.crt")?, read("tls.crt")?, read("tls.key")?))
}

async fn controller(args: ControllerArgs) -> anyhow::Result<()> {
    let tls = match &args.tls_dir {
        Some(dir) => {
            let (ca_pem, cert_pem, key_pem) = tls_dir(dir)?;
            Some(ferry_controller::server::TlsFiles {
                ca_pem,
                cert_pem,
                key_pem,
            })
        }
        None => None,
    };
    let template = match &args.proxy_template {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            serde_saphyr::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
        }
        None => ferry_controller::provision::ProxyTemplate::default(),
    };
    let opts = ferry_controller::Options {
        namespace: args.namespace,
        identity: args.identity,
        controller_name: args.controller_name,
        snapshot_address: args.snapshot_address,
        admin_address: args.health_address,
        tls,
        template,
        leader_election: !args.no_leader_election,
    };
    info!(namespace = %opts.namespace, identity = %opts.identity, "starting the controller");
    ferry_controller::run(opts, shutdown_signal()).await?;
    Ok(())
}

/// Starts following the controller; the proxy has no config until the
/// first snapshot arrives.
fn follow_controller(
    remote: &RemoteArgs,
    proxy: &ferry_proxy::Proxy,
    stop: &tokio_util::sync::CancellationToken,
) -> anyhow::Result<()> {
    let tls = match &remote.controller_tls_dir {
        Some(dir) => {
            let (ca_pem, cert_pem, key_pem) = tls_dir(dir)?;
            Some(ferry_proxy::remote::RemoteTls {
                ca_pem,
                cert_pem,
                key_pem,
                server_name: remote.controller_server_name.clone(),
            })
        }
        None => {
            warn!("no --controller-tls-dir: the snapshot stream is not encrypted");
            None
        }
    };
    let source = ferry_proxy::remote::RemoteSource {
        endpoint: remote.controller.clone().unwrap_or_default(),
        gateway: remote.gateway.clone().unwrap_or_default(),
        proxy_id: remote.proxy_id.clone(),
        tls,
    };
    info!(controller = %source.endpoint, gateway = %source.gateway, "waiting for the first snapshot");
    tokio::spawn(ferry_proxy::remote::follow(
        source,
        proxy.reload_handle(),
        stop.clone(),
    ));
    Ok(())
}

/// Builds metrics, and tracing and access logs when enabled.
fn telemetry(o: &Observability) -> anyhow::Result<Telemetry> {
    let metrics = Arc::new(ferry_telemetry::Metrics::new());
    let access_log = match o.access_log.as_str() {
        "off" | "" => None,
        sink => Some(
            AccessLog::spawn(
                &sink.parse::<Sink>()?,
                access_log::DEFAULT_CAPACITY,
                Arc::clone(&metrics.proxy().access_log_dropped),
            )
            .with_context(|| format!("opening access log {sink}"))?,
        ),
    };
    let tracing = match &o.otlp_endpoint {
        Some(endpoint) if !endpoint.is_empty() => {
            let t = Tracing::otlp(&TraceConfig {
                endpoint: endpoint.clone(),
                service_name: o.service_name.clone(),
                sample_ratio: o.trace_sample_ratio,
            })?;
            info!(%endpoint, ratio = o.trace_sample_ratio, "exporting traces over OTLP");
            Some(t)
        }
        _ => None,
    };
    Ok(Telemetry {
        metrics,
        tracing,
        access_log,
    })
}

async fn proxy(
    source: Source,
    drain: ferry_proxy::Drain,
    watch: Duration,
    observability: Observability,
) -> anyhow::Result<()> {
    let telemetry = Arc::new(telemetry(&observability)?);
    let stop = tokio_util::sync::CancellationToken::new();
    let proxy = match &source {
        Source::File(config) => {
            let snapshot = ferry_config::load_snapshot(config, SnapshotVersion(1))
                .with_context(|| format!("loading {}", config.display()))?;
            info!(config = %config.display(), version = %snapshot.version(), "config loaded");
            ferry_proxy::Proxy::bind_with(snapshot, Arc::clone(&telemetry)).await?
        }
        Source::Controller(remote) => {
            let proxy = ferry_proxy::Proxy::bind_awaiting(Arc::clone(&telemetry)).await?;
            follow_controller(remote, &proxy, &stop)?;
            proxy
        }
    };
    if let Some(addr) = observability.admin_address.0 {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("binding the admin API on {addr}"))?;
        info!(address = %listener.local_addr()?, "admin API listening");
        let stop = stop.clone();
        let handle = proxy.reload_handle();
        tokio::spawn(async move {
            if let Err(e) =
                ferry_proxy::admin::serve(listener, handle, stop.cancelled_owned()).await
            {
                error!(error = %e, "admin API stopped");
            }
        });
    }
    if let Source::File(config) = &source
        && !watch.is_zero()
    {
        tokio::spawn(ferry_proxy::watch::watch_file(
            config.clone(),
            proxy.reload_handle(),
            watch,
            stop.clone(),
        ));
    }
    let result = proxy.run(shutdown_signal(), drain).await;
    stop.cancel();
    // Flushing blocks on exporter threads; keep it off the async workers.
    let flushed = tokio::task::spawn_blocking(move || {
        if let Some(log) = &telemetry.access_log {
            log.flush(Duration::from_secs(2));
        }
        if let Some(t) = &telemetry.tracing
            && let Err(e) = t.shutdown()
        {
            warn!(error = %e, "could not flush traces");
        }
    })
    .await;
    if let Err(e) = flushed {
        warn!(error = %e, "telemetry flush task failed");
    }
    result?;
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

fn check_config(path: Option<PathBuf>, schema: bool, output: Output, print: bool) -> ExitCode {
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
    let result = ferry_config::load_snapshot(&path, SnapshotVersion(0));
    if output == Output::Json {
        let value = match &result {
            Ok(snap) => serde_json::json!({
                "path": path.display().to_string(),
                "valid": true,
                "listeners": snap.listeners().len(),
                "routes": snap.listeners().iter().map(|l| l.routes().len()).sum::<usize>(),
                "clusters": snap.clusters().len(),
                "config": if print { serde_json::to_value(snap).ok() } else { None },
            }),
            Err(ConfigError::Invalid(errors)) => serde_json::json!({
                "path": path.display().to_string(),
                "valid": false,
                "errors": errors.0.iter().map(|e| serde_json::json!({
                    "path": e.path,
                    "message": e.message,
                })).collect::<Vec<_>>(),
            }),
            Err(e) => serde_json::json!({
                "path": path.display().to_string(),
                "valid": false,
                "errors": [{ "path": "", "message": e.to_string() }],
            }),
        };
        out(&serde_json::to_string_pretty(&value).unwrap_or_default());
        return if result.is_ok() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    match result {
        Ok(snap) => {
            let routes: usize = snap.listeners().iter().map(|l| l.routes().len()).sum();
            out(&format!(
                "{}: OK ({} listener(s), {routes} route(s), {} cluster(s))",
                path.display(),
                snap.listeners().len(),
                snap.clusters().len(),
            ));
            if print {
                match serde_json::to_string_pretty(&snap) {
                    Ok(s) => out(&s),
                    Err(e) => report(&format!("cannot render config: {e}")),
                }
            }
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
