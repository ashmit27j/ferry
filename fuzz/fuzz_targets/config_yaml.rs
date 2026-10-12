//! Fuzzes the config parser and validator: arbitrary bytes must never panic,
//! hang or exhaust memory, whatever the YAML (anchors, aliases, deep nesting).

#![no_main]

use ferry_config::{SnapshotVersion, from_yaml_str};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(config) = from_yaml_str(text) {
        let _ = config.into_snapshot(SnapshotVersion(1));
    }
});
