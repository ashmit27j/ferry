//! gRPC echo backend covering unary, server-, client- and bidi-streaming calls.

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

use futures_util::{Stream, StreamExt, stream};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status, Streaming};

use crate::{Behavior, MockServer, Stats};

/// Generated types for `ferry.testkit.echo.v1`.
#[allow(
    missing_docs,
    missing_debug_implementations,
    clippy::all,
    clippy::pedantic
)]
pub mod proto {
    tonic::include_proto!("ferry.testkit.echo.v1");
}

use proto::echo_server::{Echo, EchoServer};
use proto::{EchoRequest, EchoResponse};

/// Upper bound on `count` for server streaming, to keep tests bounded.
pub const MAX_STREAM_COUNT: u32 = 10_000;

/// Starts a gRPC echo server (h2c) on `addr`.
///
/// # Errors
///
/// Fails if `addr` cannot be bound.
pub async fn spawn(addr: SocketAddr, behavior: Behavior) -> std::io::Result<MockServer> {
    let listener = TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    let stats = Arc::new(Stats::default());
    let token = CancellationToken::new();
    let svc = EchoService {
        behavior: Arc::new(behavior),
        stats: Arc::clone(&stats),
    };
    let task = tokio::spawn(accept_loop(listener, svc, token.clone()));
    Ok(MockServer::new(local, stats, token, task))
}

async fn accept_loop(listener: TcpListener, svc: EchoService, token: CancellationToken) {
    let routes = tonic::service::Routes::new(EchoServer::new(svc)).prepare();
    let mut builder = auto::Builder::new(TokioExecutor::new());
    builder = builder.http2_only();
    loop {
        let stream = tokio::select! {
            () = token.cancelled() => return,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(_) => continue,
            },
        };
        let _ = stream.set_nodelay(true);
        let service = TowerToHyperService::new(routes.clone());
        let conn = builder
            .serve_connection(TokioIo::new(stream), service)
            .into_owned();
        let token = token.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = conn => {}
                () = token.cancelled() => {}
            }
        });
    }
}

#[derive(Debug, Clone)]
struct EchoService {
    behavior: Arc<Behavior>,
    stats: Arc<Stats>,
}

type ResponseStream = Pin<Box<dyn Stream<Item = Result<EchoResponse, Status>> + Send>>;

impl EchoService {
    /// Applies latency and error injection to one call.
    async fn gate(&self) -> Result<(), Status> {
        if !self.behavior.latency.is_zero() {
            tokio::time::sleep(self.behavior.latency).await;
        }
        let fail = self.behavior.should_fail();
        self.stats.record(fail);
        if fail {
            Err(Status::unavailable(format!(
                "injected failure from {}",
                self.behavior.name
            )))
        } else {
            Ok(())
        }
    }

    fn reply(&self, message: String, sequence: u32) -> EchoResponse {
        EchoResponse {
            message,
            backend: self.behavior.name.clone(),
            sequence,
        }
    }
}

#[tonic::async_trait]
impl Echo for EchoService {
    async fn unary(&self, req: Request<EchoRequest>) -> Result<Response<EchoResponse>, Status> {
        self.gate().await?;
        Ok(Response::new(self.reply(req.into_inner().message, 0)))
    }

    type ServerStreamStream = ResponseStream;

    async fn server_stream(
        &self,
        req: Request<EchoRequest>,
    ) -> Result<Response<Self::ServerStreamStream>, Status> {
        self.gate().await?;
        let req = req.into_inner();
        let count = req.count.min(MAX_STREAM_COUNT);
        let this = self.clone();
        let out = stream::iter(0..count).map(move |i| Ok(this.reply(req.message.clone(), i)));
        Ok(Response::new(Box::pin(out)))
    }

    async fn client_stream(
        &self,
        req: Request<Streaming<EchoRequest>>,
    ) -> Result<Response<EchoResponse>, Status> {
        self.gate().await?;
        let mut inbound = req.into_inner();
        let mut parts = Vec::new();
        while let Some(msg) = inbound.next().await {
            parts.push(msg?.message);
        }
        let count = u32::try_from(parts.len()).unwrap_or(u32::MAX);
        Ok(Response::new(self.reply(parts.join(","), count)))
    }

    type BidiStream = ResponseStream;

    async fn bidi(
        &self,
        req: Request<Streaming<EchoRequest>>,
    ) -> Result<Response<Self::BidiStream>, Status> {
        self.gate().await?;
        let this = self.clone();
        let out = req.into_inner().enumerate().map(move |(i, msg)| {
            let seq = u32::try_from(i).unwrap_or(u32::MAX);
            msg.map(|m| this.reply(m.message, seq))
        });
        Ok(Response::new(Box::pin(out)))
    }
}
