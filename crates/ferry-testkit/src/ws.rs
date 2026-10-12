//! WebSocket echo backend.

use std::net::SocketAddr;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use crate::{Behavior, MockServer, Stats};

/// Starts a WebSocket echo server on `addr`. Any request path is accepted.
///
/// Text and binary frames are echoed back unchanged. With error injection, a
/// failing message closes the connection with code 1011 (internal error).
///
/// # Errors
///
/// Fails if `addr` cannot be bound.
pub async fn spawn(addr: SocketAddr, behavior: Behavior) -> std::io::Result<MockServer> {
    let listener = TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    let stats = Arc::new(Stats::default());
    let token = CancellationToken::new();
    let task = tokio::spawn(accept_loop(
        listener,
        Arc::new(behavior),
        Arc::clone(&stats),
        token.clone(),
    ));
    Ok(MockServer::new(local, stats, token, task))
}

async fn accept_loop(
    listener: TcpListener,
    behavior: Arc<Behavior>,
    stats: Arc<Stats>,
    token: CancellationToken,
) {
    loop {
        let stream = tokio::select! {
            () = token.cancelled() => return,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(_) => continue,
            },
        };
        let behavior = Arc::clone(&behavior);
        let stats = Arc::clone(&stats);
        let token = token.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = echo(stream, behavior, stats) => {}
                () = token.cancelled() => {}
            }
        });
    }
}

async fn echo(stream: TcpStream, behavior: Arc<Behavior>, stats: Arc<Stats>) {
    let _ = stream.set_nodelay(true);
    let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };
    while let Some(Ok(msg)) = ws.next().await {
        match msg {
            Message::Text(_) | Message::Binary(_) => {
                if !behavior.latency.is_zero() {
                    tokio::time::sleep(behavior.latency).await;
                }
                let fail = behavior.should_fail();
                stats.record(fail);
                if fail {
                    let frame = tokio_tungstenite::tungstenite::protocol::CloseFrame {
                        code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Error,
                        reason: "injected failure".into(),
                    };
                    let _ = ws.close(Some(frame)).await;
                    return;
                }
                if ws.send(msg).await.is_err() {
                    return;
                }
            }
            Message::Close(_) => return,
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
        }
    }
}
