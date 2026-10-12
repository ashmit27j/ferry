//! Serialization helpers for `http` types, which do not implement `Serialize`.

use http::{HeaderName, HeaderValue, Method};
use serde::Serializer;
use serde::ser::SerializeSeq;

pub(crate) fn opt_method<S: Serializer>(m: &Option<Method>, s: S) -> Result<S::Ok, S::Error> {
    match m {
        Some(m) => s.serialize_some(m.as_str()),
        None => s.serialize_none(),
    }
}

pub(crate) fn header_name<S: Serializer>(n: &HeaderName, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(n.as_str())
}

pub(crate) fn header_names<S: Serializer>(names: &[HeaderName], s: S) -> Result<S::Ok, S::Error> {
    let mut seq = s.serialize_seq(Some(names.len()))?;
    for n in names {
        seq.serialize_element(n.as_str())?;
    }
    seq.end()
}

pub(crate) fn header_pairs<S: Serializer>(
    pairs: &[(HeaderName, HeaderValue)],
    s: S,
) -> Result<S::Ok, S::Error> {
    let mut seq = s.serialize_seq(Some(pairs.len()))?;
    for (n, v) in pairs {
        seq.serialize_element(&(n.as_str(), String::from_utf8_lossy(v.as_bytes())))?;
    }
    seq.end()
}

pub(crate) fn opt_header_value<S: Serializer>(
    v: &Option<HeaderValue>,
    s: S,
) -> Result<S::Ok, S::Error> {
    match v {
        Some(v) => s.serialize_some(&String::from_utf8_lossy(v.as_bytes())),
        None => s.serialize_none(),
    }
}
