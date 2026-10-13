//! A traced HTTP client: the first hop of an end-to-end trace.
//!
//! Each request gets a root client span, exported over OTLP, whose context
//! goes out in `traceparent`. With Ferry and a traced mock backend behind it,
//! one trace then spans client → Ferry → backend. Prints one line per request:
//! `<trace id> <status>`.
//!
//! ```text
//! trace-client --url http://127.0.0.1:8080/ --otlp-endpoint http://127.0.0.1:4317 --count 5
//! ```

use std::time::Duration;

use anyhow::Context;
use bytes::Bytes;
use clap::Parser;
use ferry_telemetry::Tracing;
use ferry_telemetry::trace::TraceConfig;
use http_body_util::{BodyExt, Empty};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

/// Sends traced GET requests.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Args {
    /// URL to request.
    #[arg(long)]
    url: http::Uri,
    /// OTLP/gRPC endpoint for the client's spans.
    #[arg(long, env = "OTEL_EXPORTER_OTLP_ENDPOINT")]
    otlp_endpoint: String,
    /// Requests to send.
    #[arg(long, default_value_t = 1)]
    count: u32,
    /// Milliseconds between requests.
    #[arg(long, default_value_t = 100)]
    interval_ms: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let tracing = Tracing::otlp(&TraceConfig {
        endpoint: args.otlp_endpoint.clone(),
        service_name: "trace-client".into(),
        sample_ratio: 1.0,
    })?;
    let client = Client::builder(TokioExecutor::new()).build_http::<Empty<Bytes>>();
    for _ in 0..args.count {
        let span = tracing.root_client_span("GET");
        let mut req = http::Request::get(args.url.clone()).body(Empty::new())?;
        tracing.inject(&span, req.headers_mut());
        let status = match client.request(req).await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let _ = resp.into_body().collect().await;
                span.end_status(status);
                status.to_string()
            }
            Err(e) => {
                span.end_error("connect");
                format!("error: {e}")
            }
        };
        #[allow(clippy::print_stdout)]
        {
            println!("{} {status}", span.trace_id());
        }
        tokio::time::sleep(Duration::from_millis(args.interval_ms)).await;
    }
    tokio::task::spawn_blocking(move || tracing.shutdown())
        .await
        .context("flushing spans")??;
    Ok(())
}
