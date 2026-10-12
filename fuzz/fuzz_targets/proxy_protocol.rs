//! Fuzzes the PROXY protocol parser: arbitrary bytes must never panic, and a
//! parsed header must never claim more bytes than it was given.

#![no_main]

use ferry_proxy::proxy_protocol::parse;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(h) = parse(data) {
        assert!(h.len <= data.len());
    }
});
