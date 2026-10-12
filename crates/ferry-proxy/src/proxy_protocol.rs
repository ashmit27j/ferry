//! PROXY protocol v1 and v2 (HAProxy's spec), as sent by AWS NLBs.
//!
//! [`parse`] is a pure function over the first bytes of a connection, so it is
//! easy to test and fuzz. [`read_header`] peeks at the socket until a complete
//! header is available, consumes exactly the header, and leaves the HTTP bytes
//! behind for hyper.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

/// The v2 signature.
const V2_SIGNATURE: [u8; 12] = *b"\r\n\r\n\0\r\nQUIT\n";
/// Longest v1 header, including CRLF.
const V1_MAX: usize = 107;
/// Longest v2 header we accept (16-byte prefix plus addresses and TLVs).
pub const V2_MAX: usize = 16 + 512;
/// Time allowed for the whole header to arrive.
pub const HEADER_TIMEOUT: Duration = Duration::from_secs(5);

/// A parsed header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Bytes the header occupies at the start of the stream.
    pub len: usize,
    /// The original client, or `None` for LOCAL connections (the load
    /// balancer's own health checks) and unknown families.
    pub source: Option<SocketAddr>,
}

/// Why a header could not be parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// More bytes are needed.
    #[error("incomplete PROXY protocol header")]
    Incomplete,
    /// The bytes are not a valid PROXY protocol header.
    #[error("invalid PROXY protocol header: {0}")]
    Invalid(&'static str),
}

/// Parses a PROXY protocol header at the start of `buf`.
///
/// # Errors
///
/// [`Error::Incomplete`] if `buf` is a valid prefix of a header;
/// [`Error::Invalid`] otherwise.
pub fn parse(buf: &[u8]) -> Result<Header, Error> {
    if buf.starts_with(&V2_SIGNATURE[..buf.len().min(12)]) && !buf.starts_with(b"PROXY") {
        return parse_v2(buf);
    }
    if b"PROXY ".starts_with(&buf[..buf.len().min(6)]) {
        return parse_v1(buf);
    }
    Err(Error::Invalid("missing signature"))
}

fn parse_v1(buf: &[u8]) -> Result<Header, Error> {
    let Some(end) = buf.windows(2).position(|w| w == b"\r\n") else {
        return if buf.len() >= V1_MAX {
            Err(Error::Invalid("v1 header too long"))
        } else {
            Err(Error::Incomplete)
        };
    };
    let len = end + 2;
    if len > V1_MAX {
        return Err(Error::Invalid("v1 header too long"));
    }
    let line = std::str::from_utf8(&buf[..end]).map_err(|_| Error::Invalid("v1 not ASCII"))?;
    let mut parts = line.split(' ');
    if parts.next() != Some("PROXY") {
        return Err(Error::Invalid("v1 missing PROXY"));
    }
    match parts.next() {
        Some("UNKNOWN") => Ok(Header { len, source: None }),
        Some(proto @ ("TCP4" | "TCP6")) => {
            let src: IpAddr = parts
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or(Error::Invalid("v1 bad source address"))?;
            let dst: IpAddr = parts
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or(Error::Invalid("v1 bad destination address"))?;
            let sport: u16 = parts
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or(Error::Invalid("v1 bad source port"))?;
            let _dport: u16 = parts
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or(Error::Invalid("v1 bad destination port"))?;
            if parts.next().is_some() {
                return Err(Error::Invalid("v1 trailing fields"));
            }
            let v4 = proto == "TCP4";
            if src.is_ipv4() != v4 || dst.is_ipv4() != v4 {
                return Err(Error::Invalid("v1 address family mismatch"));
            }
            Ok(Header {
                len,
                source: Some(SocketAddr::new(src, sport)),
            })
        }
        _ => Err(Error::Invalid("v1 unknown protocol")),
    }
}

fn parse_v2(buf: &[u8]) -> Result<Header, Error> {
    if buf.len() < 16 {
        return Err(Error::Incomplete);
    }
    let ver_cmd = buf[12];
    if ver_cmd >> 4 != 2 {
        return Err(Error::Invalid("v2 bad version"));
    }
    let addr_len = usize::from(u16::from_be_bytes([buf[14], buf[15]]));
    let len = 16 + addr_len;
    if len > V2_MAX {
        return Err(Error::Invalid("v2 header too long"));
    }
    if buf.len() < len {
        return Err(Error::Incomplete);
    }
    let body = &buf[16..len];
    match ver_cmd & 0x0f {
        0 => return Ok(Header { len, source: None }), // LOCAL
        1 => {}                                       // PROXY
        _ => return Err(Error::Invalid("v2 bad command")),
    }
    let source = match buf[13] {
        // TCP over IPv4: src(4) dst(4) sport(2) dport(2)
        0x11 => {
            let b = body
                .get(..12)
                .ok_or(Error::Invalid("v2 short IPv4 block"))?;
            let ip = Ipv4Addr::new(b[0], b[1], b[2], b[3]);
            Some(SocketAddr::new(ip.into(), u16::from_be_bytes([b[8], b[9]])))
        }
        // TCP over IPv6: src(16) dst(16) sport(2) dport(2)
        0x21 => {
            let b = body
                .get(..36)
                .ok_or(Error::Invalid("v2 short IPv6 block"))?;
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&b[..16]);
            let ip = Ipv6Addr::from(octets);
            Some(SocketAddr::new(
                ip.into(),
                u16::from_be_bytes([b[32], b[33]]),
            ))
        }
        // UNSPEC, UDP or UNIX: accept, but there is no TCP client address.
        _ => None,
    };
    Ok(Header { len, source })
}

/// Reads and consumes the PROXY protocol header from `stream`, returning the
/// client address it names (or `fallback` when it names none).
///
/// # Errors
///
/// Fails if the header is missing, invalid, or does not arrive within
/// [`HEADER_TIMEOUT`].
pub async fn read_header(
    stream: &mut TcpStream,
    fallback: SocketAddr,
) -> Result<SocketAddr, Error> {
    let fut = async {
        let mut buf = [0u8; V2_MAX];
        loop {
            let n = stream
                .peek(&mut buf)
                .await
                .map_err(|_| Error::Invalid("read failed"))?;
            if n == 0 {
                return Err(Error::Invalid("connection closed"));
            }
            match parse(&buf[..n]) {
                Ok(h) => {
                    let mut header = vec![0u8; h.len];
                    stream
                        .read_exact(&mut header)
                        .await
                        .map_err(|_| Error::Invalid("read failed"))?;
                    return Ok(h.source.unwrap_or(fallback));
                }
                Err(Error::Incomplete) => {
                    // Wait for more bytes rather than spinning on the same peek.
                    stream
                        .readable()
                        .await
                        .map_err(|_| Error::Invalid("read failed"))?;
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                Err(e) => return Err(e),
            }
        }
    };
    tokio::time::timeout(HEADER_TIMEOUT, fut)
        .await
        .map_err(|_| Error::Invalid("timed out"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn v2(cmd: u8, fam: u8, addrs: &[u8]) -> Vec<u8> {
        let mut b = V2_SIGNATURE.to_vec();
        b.push(0x20 | cmd);
        b.push(fam);
        b.extend_from_slice(&u16::try_from(addrs.len()).unwrap().to_be_bytes());
        b.extend_from_slice(addrs);
        b
    }

    #[test]
    fn v1_tcp4_and_tcp6() {
        let line = b"PROXY TCP4 192.0.2.1 198.51.100.2 51000 443\r\n";
        let mut with_request = line.to_vec();
        with_request.extend_from_slice(b"GET /");
        let h = parse(&with_request).unwrap();
        assert_eq!(h.source, Some("192.0.2.1:51000".parse().unwrap()));
        assert_eq!(h.len, line.len(), "the request bytes are not consumed");
        let h = parse(b"PROXY TCP6 2001:db8::1 2001:db8::2 1 2\r\n").unwrap();
        assert_eq!(h.source, Some("[2001:db8::1]:1".parse().unwrap()));
        assert_eq!(parse(b"PROXY UNKNOWN\r\n").unwrap().source, None);
    }

    #[test]
    fn v1_rejects_garbage() {
        assert!(matches!(
            parse(b"PROXY TCP4 1.2.3.4 5.6.7.8 x 1\r\n"),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            parse(b"PROXY TCP4 ::1 ::2 1 2\r\n"),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            parse(b"GET / HTTP/1.1\r\n"),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(parse(&[b'P'; 200]), Err(Error::Invalid(_))));
        assert_eq!(parse(b"PROXY TCP4 1.2"), Err(Error::Incomplete));
        assert_eq!(parse(b"PRO"), Err(Error::Incomplete));
    }

    #[test]
    fn v2_ipv4_ipv6_and_local() {
        let mut a4 = vec![192, 0, 2, 1, 10, 0, 0, 1];
        a4.extend_from_slice(&51000u16.to_be_bytes());
        a4.extend_from_slice(&443u16.to_be_bytes());
        let h = parse(&v2(1, 0x11, &a4)).unwrap();
        assert_eq!(h.source, Some("192.0.2.1:51000".parse().unwrap()));
        assert_eq!(h.len, 28);

        let mut a6 = vec![0u8; 36];
        a6[0] = 0x20;
        a6[1] = 0x01;
        a6[15] = 1;
        a6[32..34].copy_from_slice(&7u16.to_be_bytes());
        let h = parse(&v2(1, 0x21, &a6)).unwrap();
        assert_eq!(h.source, Some("[2001::1]:7".parse().unwrap()));

        let local = parse(&v2(0, 0x00, &[])).unwrap();
        assert_eq!((local.len, local.source), (16, None));
    }

    #[test]
    fn v2_tlvs_are_skipped_and_truncation_is_incomplete() {
        let mut a4 = vec![1, 2, 3, 4, 5, 6, 7, 8, 0, 80, 0, 81];
        a4.extend_from_slice(&[0x04, 0x00, 0x02, 0xab, 0xcd]); // a TLV
        let full = v2(1, 0x11, &a4);
        assert_eq!(parse(&full).unwrap().len, full.len());
        for cut in 1..full.len() {
            assert_eq!(parse(&full[..cut]), Err(Error::Incomplete), "cut at {cut}");
        }
    }

    #[test]
    fn v2_rejects_bad_version_and_command() {
        let mut b = v2(1, 0x11, &[0; 12]);
        b[12] = 0x11;
        assert!(matches!(parse(&b), Err(Error::Invalid(_))));
        let b = v2(5, 0x11, &[0; 12]);
        assert!(matches!(parse(&b), Err(Error::Invalid(_))));
        let mut b = v2(1, 0x11, &[0; 12]);
        b[14..16].copy_from_slice(&2000u16.to_be_bytes());
        assert!(matches!(parse(&b), Err(Error::Invalid(_))));
    }

    proptest! {
        #[test]
        fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..600)) {
            let _ = parse(&bytes);
        }

        #[test]
        fn v1_round_trips(a in any::<[u8; 4]>(), b in any::<[u8; 4]>(), sp in any::<u16>(), dp in any::<u16>()) {
            let (a, b) = (Ipv4Addr::from(a), Ipv4Addr::from(b));
            let line = format!("PROXY TCP4 {a} {b} {sp} {dp}\r\n");
            let h = parse(line.as_bytes()).unwrap();
            prop_assert_eq!(h.len, line.len());
            prop_assert_eq!(h.source, Some(SocketAddr::new(a.into(), sp)));
        }
    }
}
