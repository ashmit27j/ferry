//! Standalone mock backend for manual testing and load tests.
//!
//! ```text
//! mock-backend --name a --http 127.0.0.1:9001 --grpc 127.0.0.1:9101 --ws 127.0.0.1:9201
//! ```

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use ferry_testkit::{Behavior, grpc, http, ws};

/// Mock HTTP/1.1 + h2c, gRPC and WebSocket backends with latency and error injection.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Args {
    /// Name reported in responses.
    #[arg(long, default_value = "mock")]
    name: String,
    /// Address for the HTTP/1.1 + h2c echo server.
    #[arg(long)]
    http: Option<SocketAddr>,
    /// Address for the gRPC echo server.
    #[arg(long)]
    grpc: Option<SocketAddr>,
    /// Address for the WebSocket echo server.
    #[arg(long)]
    ws: Option<SocketAddr>,
    /// Latency added to every response, in milliseconds.
    #[arg(long, default_value_t = 0)]
    latency_ms: u64,
    /// Fraction of requests that fail, from 0.0 to 1.0.
    #[arg(long, default_value_t = 0.0)]
    error_rate: f64,
    /// HTTP status returned for injected failures.
    #[arg(long, default_value_t = 503)]
    error_status: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let behavior = Behavior::named(&args.name)
        .with_latency(Duration::from_millis(args.latency_ms))
        .with_errors(args.error_rate, args.error_status);

    let mut servers = Vec::new();
    if let Some(addr) = args.http {
        servers.push((
            "http",
            http::spawn(addr, behavior.clone()).await.context("http")?,
        ));
    }
    if let Some(addr) = args.grpc {
        servers.push((
            "grpc",
            grpc::spawn(addr, behavior.clone()).await.context("grpc")?,
        ));
    }
    if let Some(addr) = args.ws {
        servers.push(("ws", ws::spawn(addr, behavior.clone()).await.context("ws")?));
    }
    anyhow::ensure!(
        !servers.is_empty(),
        "pass at least one of --http, --grpc, --ws"
    );
    for (kind, s) in &servers {
        #[allow(clippy::print_stderr)]
        {
            eprintln!(
                "mock-backend {}: {kind} listening on {}",
                args.name,
                s.addr()
            );
        }
    }
    tokio::signal::ctrl_c()
        .await
        .context("waiting for ctrl-c")?;
    Ok(())
}
