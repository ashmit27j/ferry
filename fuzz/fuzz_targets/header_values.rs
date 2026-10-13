//! Fuzzes the small parsers that read request header values and config
//! strings: `grpc-timeout` (gRPC over HTTP/2) and GEP-2257 durations. Neither
//! may panic or overflow, and a parsed value must be consistent with its
//! input.

#![no_main]

use ferry_config::Duration;
use ferry_proxy::parse_grpc_timeout;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Some(d) = parse_grpc_timeout(text) {
        // At most 8 digits of hours.
        assert!(d <= std::time::Duration::from_secs(99_999_999 * 3600));
        assert!(text.len() <= 9);
    }
    if let Ok(d) = text.parse::<Duration>() {
        // Durations print back in a form that parses to the same value.
        let again: Duration = d.to_string().parse().expect("display round-trips");
        assert_eq!(again, d);
    }
});
