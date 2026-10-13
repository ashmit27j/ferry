//! JSON access logs (ADR 0029).
//!
//! One JSON object per line, written when a response finishes. Requests never
//! wait for the log: each line is handed to a dedicated writer thread through
//! a bounded channel, and dropped (and counted in `ferry_access_log_dropped`)
//! if the writer has fallen behind, as Envoy's async file sink does. Logs go to
//! stdout by default, the Kubernetes convention, separate from Ferry's own
//! diagnostics on stderr.

use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Lines buffered between requests and the writer before lines are dropped.
pub const DEFAULT_CAPACITY: usize = 16 * 1024;

/// Where access logs go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sink {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
    /// Appended to a file.
    File(PathBuf),
}

impl std::str::FromStr for Sink {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "stdout" | "-" => Self::Stdout,
            "stderr" => Self::Stderr,
            path => Self::File(PathBuf::from(path)),
        })
    }
}

enum Msg {
    Line(String),
    Flush(SyncSender<()>),
}

/// A non-blocking access-log writer.
pub struct AccessLog {
    tx: SyncSender<Msg>,
    dropped: Arc<AtomicU64>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl std::fmt::Debug for AccessLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessLog")
            .field("dropped", &self.dropped.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl AccessLog {
    /// Starts the writer thread. Dropped lines are counted in `dropped`.
    ///
    /// # Errors
    ///
    /// Fails if a log file cannot be opened or the thread cannot start.
    pub fn spawn(sink: &Sink, capacity: usize, dropped: Arc<AtomicU64>) -> std::io::Result<Self> {
        let out: Box<dyn Write + Send> = match sink {
            Sink::Stdout => Box::new(std::io::stdout()),
            Sink::Stderr => Box::new(std::io::stderr()),
            Sink::File(path) => Box::new(OpenOptions::new().create(true).append(true).open(path)?),
        };
        let (tx, rx) = sync_channel(capacity.max(1));
        let worker = std::thread::Builder::new()
            .name("ferry-access-log".into())
            .spawn(move || write_loop(&rx, BufWriter::with_capacity(64 * 1024, out)))?;
        Ok(Self {
            tx,
            dropped,
            worker: Mutex::new(Some(worker)),
        })
    }

    /// Queues one entry. Never blocks.
    pub fn log(&self, entry: &Entry<'_>) {
        match serde_json::to_string(entry) {
            Ok(line) => self.write_line(line),
            Err(_) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Queues one preformatted line. Never blocks.
    pub fn write_line(&self, line: String) {
        match self.tx.try_send(Msg::Line(line)) {
            Ok(()) => {}
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Waits up to `timeout` for every queued line to be written. Blocks the
    /// calling thread; call it at shutdown, not on the request path.
    pub fn flush(&self, timeout: Duration) -> bool {
        let (ack, done) = sync_channel(1);
        if self.tx.send(Msg::Flush(ack)).is_err() {
            return false;
        }
        done.recv_timeout(timeout).is_ok()
    }

    /// Lines dropped so far.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl Drop for AccessLog {
    fn drop(&mut self) {
        self.flush(Duration::from_secs(2));
        // The channel closes when `tx` drops after this; the worker then exits.
        let worker = self
            .worker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        drop(worker);
    }
}

fn write_loop(rx: &Receiver<Msg>, mut out: BufWriter<Box<dyn Write + Send>>) {
    loop {
        // Write as lines arrive; flush whenever the queue runs dry, so a quiet
        // proxy's last lines are not stuck in the buffer.
        let msg = match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(m) => m,
            Err(RecvTimeoutError::Timeout) => {
                let _ = out.flush();
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => {
                let _ = out.flush();
                return;
            }
        };
        match msg {
            Msg::Line(line) => {
                let _ = out.write_all(line.as_bytes());
                let _ = out.write_all(b"\n");
            }
            Msg::Flush(ack) => {
                let _ = out.flush();
                let _ = ack.try_send(());
            }
        }
    }
}

/// One access-log line. Field names follow Envoy's default JSON format where
/// one exists.
#[derive(Debug, Default, Serialize)]
pub struct Entry<'a> {
    /// When the request arrived, RFC 3339 UTC with milliseconds.
    pub start_time: String,
    /// Arrival to last response byte, in milliseconds.
    pub duration_ms: f64,
    /// Time spent waiting on upstreams for response headers, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_service_time_ms: Option<f64>,
    /// Listener name.
    pub listener: &'a str,
    /// Route name, or `-` when none matched.
    pub route: &'a str,
    /// Cluster the request was sent to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_cluster: Option<&'a str>,
    /// Endpoint of the last attempt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_host: Option<String>,
    /// Upstream attempts made.
    pub attempts: u32,
    /// Request method.
    pub method: &'a str,
    /// Host or `:authority`.
    pub authority: &'a str,
    /// Path and query as received.
    pub path: &'a str,
    /// `HTTP/1.1` or `HTTP/2`.
    pub protocol: &'a str,
    /// Response status sent downstream.
    pub response_code: u16,
    /// `grpc-status` of a gRPC response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grpc_status: Option<u8>,
    /// Comma-separated flags (`NR`, `UF`, `UT`, `DC`, ...), or `-`.
    pub response_flags: String,
    /// Response body bytes sent downstream.
    pub bytes_sent: u64,
    /// Client address (from the PROXY header when enabled).
    pub downstream_remote_address: String,
    /// `X-Forwarded-For` as forwarded upstream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x_forwarded_for: Option<&'a str>,
    /// `User-Agent`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<&'a str>,
    /// `X-Request-Id`.
    pub request_id: &'a str,
    /// W3C trace ID, when tracing is on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

/// Formats `t` as RFC 3339 UTC with millisecond precision.
#[must_use]
pub fn rfc3339_millis(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    let (y, m, day) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
        d.subsec_millis()
    )
}

/// Days since 1970-01-01 to (year, month, day), from Howard Hinnant's
/// `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (
        y,
        u32::try_from(m).unwrap_or(1),
        u32::try_from(d).unwrap_or(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_rfc3339() {
        assert_eq!(rfc3339_millis(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        let t = UNIX_EPOCH + Duration::from_millis(1_791_853_506_789);
        assert_eq!(rfc3339_millis(t), "2026-10-13T01:05:06.789Z");
        // Leap day.
        let t = UNIX_EPOCH + Duration::from_secs(951_782_400);
        assert_eq!(rfc3339_millis(t), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn parses_sinks() {
        assert_eq!("stdout".parse::<Sink>().unwrap(), Sink::Stdout);
        assert_eq!("-".parse::<Sink>().unwrap(), Sink::Stdout);
        assert_eq!("stderr".parse::<Sink>().unwrap(), Sink::Stderr);
        assert_eq!(
            "/var/log/a.log".parse::<Sink>().unwrap(),
            Sink::File("/var/log/a.log".into())
        );
    }

    #[test]
    fn writes_json_lines_to_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("access.log");
        let dropped = Arc::new(AtomicU64::new(0));
        let log = AccessLog::spawn(&Sink::File(path.clone()), 16, Arc::clone(&dropped)).unwrap();
        for i in 0..3u16 {
            log.log(&Entry {
                method: "GET",
                path: "/a\"b",
                response_code: 200 + i,
                response_flags: "-".into(),
                request_id: "id",
                ..Entry::default()
            });
        }
        assert!(log.flush(Duration::from_secs(5)));
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[2]["response_code"], 202);
        assert_eq!(lines[0]["path"], "/a\"b");
        assert!(
            lines[0].get("trace_id").is_none(),
            "absent fields are omitted"
        );
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn drops_instead_of_blocking_when_full() {
        let dropped = Arc::new(AtomicU64::new(0));
        let (tx, _rx) = sync_channel(1);
        // A log whose writer never drains: the second line must be dropped.
        let log = AccessLog {
            tx,
            dropped: Arc::clone(&dropped),
            worker: Mutex::new(None),
        };
        log.write_line("a".into());
        log.write_line("b".into());
        assert_eq!(log.dropped(), 1);
        std::mem::forget(log); // its Drop would wait for a flush nobody answers
    }
}
