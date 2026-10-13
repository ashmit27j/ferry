//! Opens many WebSockets through a proxy and holds them (M5: 10,000 idle
//! WebSockets).
//!
//! Each socket connects (at most `--concurrency` handshakes at a time),
//! then sends one small text message every `--interval` and checks the echo,
//! until `--hold` has passed. At the end it reports how many opened, how
//! many failed to open, and how many were still alive and echoing, as JSON
//! on stdout. It exits non-zero unless every socket opened and stayed alive.
//!
//! ```sh
//! ws-flood --url ws://127.0.0.1:8080/ws --count 10000 --hold 600s --interval 30s
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::Semaphore;
use tokio_tungstenite::tungstenite::Message;

#[derive(Parser, Debug)]
#[command(about = "Open and hold many WebSockets")]
struct Args {
    /// WebSocket URL, e.g. `ws://ferry:8080/ws`.
    #[arg(long)]
    url: String,
    /// Sockets to open.
    #[arg(long, default_value_t = 10_000)]
    count: u64,
    /// Handshakes in flight at once.
    #[arg(long, default_value_t = 200)]
    concurrency: usize,
    /// How long to hold every socket once all have been attempted, e.g. `600s`.
    #[arg(long, default_value = "600s", value_parser = humantime)]
    hold: Duration,
    /// Message interval per socket (a keep-alive, as applications send).
    #[arg(long, default_value = "30s", value_parser = humantime)]
    interval: Duration,
}

fn humantime(s: &str) -> Result<Duration, String> {
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = n.parse().map_err(|e| format!("{s}: {e}"))?;
    match unit {
        "ms" => Ok(Duration::from_millis(n)),
        "s" | "" => Ok(Duration::from_secs(n)),
        "m" => Ok(Duration::from_secs(n * 60)),
        "h" => Ok(Duration::from_secs(n * 3600)),
        _ => Err(format!("{s}: unit must be ms, s, m or h")),
    }
}

#[derive(Default)]
struct Counts {
    opened: AtomicU64,
    open_failed: AtomicU64,
    messages: AtomicU64,
    echo_failed: AtomicU64,
    alive_at_end: AtomicU64,
}

// Progress on stderr, the report on stdout: this is a CLI.
#[allow(clippy::print_stdout, clippy::print_stderr)]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let counts = Arc::new(Counts::default());
    let gate = Arc::new(Semaphore::new(args.concurrency));
    let started = Instant::now();
    // Everyone holds until the same moment: all attempted, plus `hold`.
    let (end_tx, end_rx) = tokio::sync::watch::channel(false);
    let mut tasks = Vec::new();
    for i in 0..args.count {
        let permit = Arc::clone(&gate)
            .acquire_owned()
            .await
            .context("semaphore closed")?;
        let (url, counts, mut end) = (args.url.clone(), Arc::clone(&counts), end_rx.clone());
        let interval = args.interval;
        tasks.push(tokio::spawn(async move {
            let socket = tokio::time::timeout(
                Duration::from_secs(30),
                tokio_tungstenite::connect_async(url.as_str()),
            )
            .await;
            drop(permit);
            let mut socket = match socket {
                Ok(Ok((s, _))) => {
                    counts.opened.fetch_add(1, Ordering::Relaxed);
                    s
                }
                _ => {
                    counts.open_failed.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            };
            // Spread keep-alives over the interval.
            let offset = interval.mul_f64((i % 1000) as f64 / 1000.0);
            let mut next = tokio::time::Instant::now() + offset;
            let mut seq = 0u64;
            loop {
                tokio::select! {
                    _ = end.changed() => break,
                    () = tokio::time::sleep_until(next) => {}
                }
                seq += 1;
                let msg = format!("{i}:{seq}");
                let echoed = async {
                    socket.send(Message::text(msg.clone())).await.ok()?;
                    loop {
                        match socket.next().await? {
                            Ok(Message::Text(t)) if t.as_str() == msg => return Some(()),
                            Ok(Message::Ping(_) | Message::Pong(_)) => {}
                            _ => return None,
                        }
                    }
                };
                match tokio::time::timeout(Duration::from_secs(10), echoed).await {
                    Ok(Some(())) => {
                        counts.messages.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {
                        counts.echo_failed.fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                }
                next += interval;
            }
            counts.alive_at_end.fetch_add(1, Ordering::Relaxed);
            let _ = socket.close(None).await;
        }));
    }
    let attempted_after = started.elapsed();
    eprintln!(
        "attempted {} sockets in {:.1}s; holding for {:?}",
        args.count,
        attempted_after.as_secs_f64(),
        args.hold
    );
    tokio::time::sleep(args.hold).await;
    let _ = end_tx.send(true);
    for t in tasks {
        let _ = t.await;
    }
    let c = &counts;
    let report = serde_json::json!({
        "count": args.count,
        "opened": c.opened.load(Ordering::Relaxed),
        "open_failed": c.open_failed.load(Ordering::Relaxed),
        "messages_echoed": c.messages.load(Ordering::Relaxed),
        "echo_failed": c.echo_failed.load(Ordering::Relaxed),
        "alive_at_end": c.alive_at_end.load(Ordering::Relaxed),
        "connect_phase_seconds": attempted_after.as_secs_f64(),
        "hold_seconds": args.hold.as_secs(),
    });
    println!("{report}");
    if c.alive_at_end.load(Ordering::Relaxed) != args.count {
        anyhow::bail!("not every socket stayed alive");
    }
    Ok(())
}
