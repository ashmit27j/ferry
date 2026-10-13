//! Standard `process_*` metrics, read from `/proc/self` at scrape time.
//!
//! Resident memory is what the flat-memory SLO watches. On systems without
//! procfs (macOS, Windows dev boxes) only the start time is exported.

use std::time::{SystemTime, UNIX_EPOCH};

use prometheus_client::collector::Collector;
use prometheus_client::encoding::DescriptorEncoder;
use prometheus_client::metrics::MetricType;

use crate::metrics::encode::Family;

/// Clock ticks per second of `/proc/self/stat` CPU times. 100 on every
/// Linux platform Ferry targets (x86-64, arm64).
const CLOCK_TICKS: f64 = 100.0;

/// Exports `process_*` metrics.
#[derive(Debug)]
pub struct ProcessCollector {
    start: f64,
}

impl ProcessCollector {
    /// A collector that reports the current time as the process start.
    /// Ferry creates it at startup.
    #[must_use]
    pub fn new() -> Self {
        let start = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64());
        Self { start }
    }
}

impl Default for ProcessCollector {
    fn default() -> Self {
        Self::new()
    }
}

/// What procfs reports about this process.
#[derive(Debug, Default, PartialEq)]
struct ProcStats {
    resident_bytes: Option<f64>,
    virtual_bytes: Option<f64>,
    cpu_seconds: Option<f64>,
    threads: Option<f64>,
    open_fds: Option<f64>,
    max_fds: Option<f64>,
}

impl ProcStats {
    fn read() -> Self {
        let mut s = std::fs::read_to_string("/proc/self/status")
            .map(|t| parse_status(&t))
            .unwrap_or_default();
        s.cpu_seconds = std::fs::read_to_string("/proc/self/stat")
            .ok()
            .and_then(|t| parse_cpu(&t));
        s.open_fds = std::fs::read_dir("/proc/self/fd")
            .ok()
            .map(|d| count_f64(d.count()));
        s.max_fds = std::fs::read_to_string("/proc/self/limits")
            .ok()
            .and_then(|t| parse_max_fds(&t));
        s
    }
}

#[allow(clippy::cast_precision_loss)]
fn count_f64(n: usize) -> f64 {
    n as f64
}

/// `VmRSS`, `VmSize` and `Threads` from `/proc/self/status`.
fn parse_status(text: &str) -> ProcStats {
    let kb = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|rest| {
                rest.trim()
                    .trim_end_matches("kB")
                    .trim()
                    .parse::<f64>()
                    .ok()
            })
    };
    ProcStats {
        resident_bytes: kb("VmRSS:").map(|k| k * 1024.0),
        virtual_bytes: kb("VmSize:").map(|k| k * 1024.0),
        threads: kb("Threads:"),
        ..ProcStats::default()
    }
}

/// User plus system CPU seconds from `/proc/self/stat` (fields 14 and 15,
/// counted after the parenthesised command name, which may contain spaces).
fn parse_cpu(text: &str) -> Option<f64> {
    let after = &text[text.rfind(')')? + 1..];
    let fields: Vec<&str> = after.split_whitespace().collect();
    // `after` starts at field 3 (state), so utime (14) is index 11.
    let utime: f64 = fields.get(11)?.parse().ok()?;
    let stime: f64 = fields.get(12)?.parse().ok()?;
    Some((utime + stime) / CLOCK_TICKS)
}

/// The soft limit on open files from `/proc/self/limits`.
fn parse_max_fds(text: &str) -> Option<f64> {
    let line = text.lines().find(|l| l.starts_with("Max open files"))?;
    line.trim_start_matches("Max open files")
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

impl Collector for ProcessCollector {
    fn encode(&self, mut e: DescriptorEncoder<'_>) -> std::fmt::Result {
        let f = Family::start(
            &mut e,
            "process_start_time_seconds",
            "Start time of the process since the Unix epoch, in seconds.",
            MetricType::Gauge,
        )?;
        f.single_gauge(self.start)?;
        let s = ProcStats::read();
        let gauges = [
            (
                "process_resident_memory_bytes",
                "Resident memory size in bytes.",
                s.resident_bytes,
            ),
            (
                "process_virtual_memory_bytes",
                "Virtual memory size in bytes.",
                s.virtual_bytes,
            ),
            ("process_threads", "OS threads in the process.", s.threads),
            ("process_open_fds", "Open file descriptors.", s.open_fds),
            (
                "process_max_fds",
                "Limit on open file descriptors.",
                s.max_fds,
            ),
        ];
        for (name, help, value) in gauges {
            if let Some(v) = value {
                Family::start(&mut e, name, help, MetricType::Gauge)?.single_gauge(v)?;
            }
        }
        if let Some(cpu) = s.cpu_seconds {
            let f = Family::start(
                &mut e,
                "process_cpu_seconds",
                "Total user and system CPU time, in seconds.",
                MetricType::Counter,
            )?;
            f.single_counter_f64(cpu)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status() {
        let s =
            parse_status("Name:\tferry\nVmSize:\t  204800 kB\nVmRSS:\t   10240 kB\nThreads:\t9\n");
        assert_eq!(s.resident_bytes, Some(10_485_760.0));
        assert_eq!(s.virtual_bytes, Some(209_715_200.0));
        assert_eq!(s.threads, Some(9.0));
    }

    #[test]
    fn parses_cpu_with_spaces_in_the_name() {
        let stat = "42 (ferry proxy) S 1 42 42 0 -1 4194560 100 0 0 0 250 50 0 0 20 0 9 0 1234 0 0";
        assert_eq!(parse_cpu(stat), Some(3.0));
        assert_eq!(parse_cpu("garbage"), None);
    }

    #[test]
    fn parses_fd_limit() {
        let limits = "Limit                     Soft Limit           Hard Limit           Units\nMax open files            1048576              1048576              files\n";
        assert_eq!(parse_max_fds(limits), Some(1_048_576.0));
    }
}
