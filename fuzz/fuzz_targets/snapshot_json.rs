//! Fuzzes the controller-to-proxy payload: a `SnapshotUpdate` carries the
//! config as JSON, which the proxy parses and validates before swapping it
//! in (ADR 0033). Arbitrary bytes must never panic, hang or exhaust memory.

#![no_main]

use ferry_config::{Config, SnapshotVersion};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(config) = serde_json::from_slice::<Config>(data) {
        let _ = config.into_snapshot(SnapshotVersion(1));
    }
});
