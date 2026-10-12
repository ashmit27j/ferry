//! Durations in the Gateway API format (GEP-2257).
//!
//! A duration is one to four `<number><unit>` components with units `h`, `m`,
//! `s` and `ms`, each number at most five digits: `1h`, `30s`, `1m30s`, `500ms`.
//! The format is the one `HTTPRoute.timeouts` uses, so the controller can copy
//! values through unchanged.

use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A non-negative duration written as GEP-2257 text, such as `1m30s`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Duration(std::time::Duration);

impl Duration {
    /// Wraps a standard duration. Sub-millisecond precision is dropped when the
    /// value is printed.
    #[must_use]
    pub const fn new(d: std::time::Duration) -> Self {
        Self(d)
    }

    /// The standard-library value.
    #[must_use]
    pub const fn get(self) -> std::time::Duration {
        self.0
    }
}

impl From<std::time::Duration> for Duration {
    fn from(d: std::time::Duration) -> Self {
        Self(d)
    }
}

impl From<Duration> for std::time::Duration {
    fn from(d: Duration) -> Self {
        d.0
    }
}

/// Why a duration string was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid duration {input:?}: {reason} (expected e.g. \"500ms\", \"5s\", \"1m30s\")")]
pub struct ParseDurationError {
    input: String,
    reason: &'static str,
}

impl FromStr for Duration {
    type Err = ParseDurationError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let err = |reason| ParseDurationError {
            input: input.to_owned(),
            reason,
        };
        if input.is_empty() {
            return Err(err("empty"));
        }
        let bytes = input.as_bytes();
        let mut total_ms: u64 = 0;
        let mut components = 0;
        let mut i = 0;
        while i < bytes.len() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let digits = &input[start..i];
            if digits.is_empty() {
                return Err(err("each component must start with a number"));
            }
            if digits.len() > 5 {
                return Err(err("numbers are limited to five digits"));
            }
            let n: u64 = digits.parse().map_err(|_| err("bad number"))?;
            let unit_start = i;
            while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                i += 1;
            }
            let ms_per_unit = match &input[unit_start..i] {
                "h" => 3_600_000,
                "m" => 60_000,
                "s" => 1_000,
                "ms" => 1,
                "" => return Err(err("missing unit")),
                _ => return Err(err("units are h, m, s and ms")),
            };
            components += 1;
            if components > 4 {
                return Err(err("at most four components"));
            }
            total_ms += n * ms_per_unit;
        }
        Ok(Self(std::time::Duration::from_millis(total_ms)))
    }
}

impl fmt::Display for Duration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut ms = u64::try_from(self.0.as_millis()).unwrap_or(u64::MAX);
        if ms == 0 {
            return f.write_str("0s");
        }
        for (unit, size) in [("h", 3_600_000), ("m", 60_000), ("s", 1_000), ("ms", 1)] {
            if ms >= size {
                write!(f, "{}{unit}", ms / size)?;
                ms %= size;
            }
        }
        Ok(())
    }
}

impl TryFrom<String> for Duration {
    type Error = ParseDurationError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<Duration> for String {
    fn from(d: Duration) -> Self {
        d.to_string()
    }
}

impl JsonSchema for Duration {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Duration".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^([0-9]{1,5}(h|m|s|ms)){1,4}$",
            "description": "GEP-2257 duration, for example 500ms, 5s or 1m30s",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::time::Duration as Std;

    #[test]
    fn parses_gateway_api_examples() {
        let cases = [
            ("0s", Std::ZERO),
            ("500ms", Std::from_millis(500)),
            ("5s", Std::from_secs(5)),
            ("1m30s", Std::from_secs(90)),
            ("1h", Std::from_secs(3600)),
            ("1h2m3s4ms", Std::from_millis(3_723_004)),
        ];
        for (text, want) in cases {
            assert_eq!(text.parse::<Duration>().unwrap().get(), want, "{text}");
        }
    }

    #[test]
    fn rejects_malformed() {
        for bad in [
            "",
            "5",
            "s",
            "5d",
            "1.5s",
            "-1s",
            "123456s",
            "1h1h1h1h1h",
            " 5s",
            "5 s",
        ] {
            assert!(bad.parse::<Duration>().is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn displays_canonically() {
        assert_eq!(Duration::new(Std::from_secs(90)).to_string(), "1m30s");
        assert_eq!(Duration::new(Std::ZERO).to_string(), "0s");
        assert_eq!(Duration::new(Std::from_millis(1500)).to_string(), "1s500ms");
    }

    proptest! {
        #[test]
        fn display_round_trips(ms in 0u64..(99_999 * 3_600_000)) {
            let d = Duration::new(Std::from_millis(ms));
            prop_assert_eq!(d.to_string().parse::<Duration>().unwrap(), d);
        }
    }
}
