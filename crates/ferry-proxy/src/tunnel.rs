//! Byte relay for upgraded connections (WebSocket and other HTTP/1.1
//! upgrades) with an idle timeout (ADR 0037).
//!
//! Each direction copies until its reader reaches end of stream, then shuts
//! down the other side's writer, so half-closes pass through as with
//! `tokio::io::copy_bidirectional`. Both directions stamp the time of their
//! last read; one timer per tunnel checks the stamp and ends the tunnel when
//! neither side has sent anything for the idle period. Reads do not re-arm a
//! timer.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::Instant;

/// Per-direction copy buffer: small while a tunnel is quiet (most WebSockets
/// are), doubling up to [`MAX_BUFFER`] while reads keep filling it. The M5
/// soak held 10,000 tunnels; fixed 8 KiB buffers cost 16 KiB each of them.
const MIN_BUFFER: usize = 2 * 1024;
const MAX_BUFFER: usize = 16 * 1024;

/// Why a relay ended early.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RelayError {
    /// Neither side sent a byte for the idle period.
    #[error("idle for {0:?}")]
    Idle(Duration),
    /// Reading or writing failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Copies `a` to `b` and `b` to `a` until both directions finish, or until
/// both have been silent for `idle`. Returns the bytes sent each way
/// (`a` to `b`, `b` to `a`).
pub(crate) async fn relay<A, B>(a: A, b: B, idle: Duration) -> Result<(u64, u64), RelayError>
where
    A: AsyncRead + AsyncWrite,
    B: AsyncRead + AsyncWrite,
{
    let start = Instant::now();
    let last = AtomicU64::new(0);
    let (ar, aw) = tokio::io::split(a);
    let (br, bw) = tokio::io::split(b);
    let copy = async { tokio::try_join!(pump(ar, bw, &last, start), pump(br, aw, &last, start)) };
    let watchdog = async {
        loop {
            let quiet_since = start + Duration::from_millis(last.load(Ordering::Relaxed));
            let deadline = quiet_since + idle;
            if Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep_until(deadline).await;
        }
    };
    tokio::select! {
        result = copy => Ok(result?),
        () = watchdog => Err(RelayError::Idle(idle)),
    }
}

async fn pump<R, W>(mut r: R, mut w: W, last: &AtomicU64, start: Instant) -> std::io::Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; MIN_BUFFER];
    let mut total = 0u64;
    loop {
        let n = r.read(&mut buf).await?;
        if n == 0 {
            w.shutdown().await?;
            return Ok(total);
        }
        let now = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
        last.store(now, Ordering::Relaxed);
        w.write_all(buf.get(..n).unwrap_or_default()).await?;
        total += n as u64;
        if n == buf.len() && buf.len() < MAX_BUFFER {
            buf.resize(buf.len() * 2, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn relays_both_ways_and_passes_half_closes() {
        let (client, a) = tokio::io::duplex(64);
        let (b, backend) = tokio::io::duplex(64);
        let relay = tokio::spawn(relay(a, b, Duration::from_secs(60)));

        let (mut cr, mut cw) = tokio::io::split(client);
        let (mut br, mut bw) = tokio::io::split(backend);
        cw.write_all(b"ping").await.unwrap();
        let mut got = [0u8; 4];
        br.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"ping");
        // The client is done sending; the backend still answers.
        cw.shutdown().await.unwrap();
        assert_eq!(
            br.read(&mut got).await.unwrap(),
            0,
            "half-close reaches the backend"
        );
        bw.write_all(b"pong!").await.unwrap();
        bw.shutdown().await.unwrap();
        let mut answer = Vec::new();
        cr.read_to_end(&mut answer).await.unwrap();
        assert_eq!(answer, b"pong!");
        assert_eq!(relay.await.unwrap().unwrap(), (4, 5));
    }

    #[tokio::test]
    async fn bulk_transfers_arrive_intact() {
        let (client, a) = tokio::io::duplex(64 * 1024);
        let (b, backend) = tokio::io::duplex(64 * 1024);
        let relay = tokio::spawn(relay(a, b, Duration::from_secs(60)));
        let payload: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
        let (_cr, mut cw) = tokio::io::split(client);
        let (mut br, _bw) = tokio::io::split(backend);
        let sent = payload.clone();
        let writer = tokio::spawn(async move {
            cw.write_all(&sent).await.unwrap();
            cw.shutdown().await.unwrap();
        });
        let mut got = Vec::new();
        br.read_to_end(&mut got).await.unwrap();
        writer.await.unwrap();
        assert_eq!(got, payload);
        relay.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn silence_ends_the_tunnel_but_traffic_keeps_it_open() {
        let (client, a) = tokio::io::duplex(64);
        let (b, backend) = tokio::io::duplex(64);
        let idle = Duration::from_secs(10);
        let relay = tokio::spawn(relay(a, b, idle));
        let (_cr, mut cw) = tokio::io::split(client);
        let (mut br, _bw) = tokio::io::split(backend);

        // A byte every 6 s keeps it open well past 10 s.
        let mut buf = [0u8; 1];
        for _ in 0..4 {
            tokio::time::sleep(Duration::from_secs(6)).await;
            cw.write_all(b"x").await.unwrap();
            br.read_exact(&mut buf).await.unwrap();
        }
        assert!(!relay.is_finished());

        tokio::time::sleep(Duration::from_secs(11)).await;
        let err = relay.await.unwrap().unwrap_err();
        assert!(matches!(err, RelayError::Idle(d) if d == idle), "{err}");
    }
}
