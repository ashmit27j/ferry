//! Distributed tracing (M3 exit criterion): one trace spans
//! client → Ferry → backend, exported over real OTLP/gRPC.
//!
//! The test runs an OTLP trace collector in-process. A traced client sends a
//! request with `traceparent`; Ferry and a traced mock backend export their
//! spans to the collector, which must then hold one trace whose spans chain
//! client span → Ferry server span → Ferry client span → backend server span.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferry_integration_tests::{TestProxy, any_port, single_cluster_yaml};
use ferry_telemetry::trace::TraceConfig;
use ferry_telemetry::{Metrics, Telemetry, Tracing};
use ferry_testkit::{Behavior, http as mock_http};
use opentelemetry_proto::tonic::collector::trace::v1::trace_service_server::{
    TraceService, TraceServiceServer,
};
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::trace::v1::span::SpanKind;

/// One exported span, flattened.
#[derive(Debug, Clone)]
struct Got {
    service: String,
    name: String,
    kind: i32,
    trace: String,
    id: String,
    parent: String,
    attributes: HashMap<String, String>,
}

#[derive(Debug, Default, Clone)]
struct Collector(Arc<Mutex<Vec<Got>>>);

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn text(v: Option<&opentelemetry_proto::tonic::common::v1::AnyValue>) -> String {
    match v.and_then(|v| v.value.as_ref()) {
        Some(Value::StringValue(s)) => s.clone(),
        Some(Value::IntValue(i)) => i.to_string(),
        Some(Value::BoolValue(b)) => b.to_string(),
        Some(Value::DoubleValue(d)) => d.to_string(),
        _ => String::new(),
    }
}

#[tonic::async_trait]
impl TraceService for Collector {
    async fn export(
        &self,
        request: tonic::Request<ExportTraceServiceRequest>,
    ) -> Result<tonic::Response<ExportTraceServiceResponse>, tonic::Status> {
        let mut out = self.0.lock().unwrap();
        for rs in request.into_inner().resource_spans {
            let service = rs
                .resource
                .iter()
                .flat_map(|r| &r.attributes)
                .find(|kv| kv.key == "service.name")
                .map(|kv| text(kv.value.as_ref()))
                .unwrap_or_default();
            for ss in rs.scope_spans {
                for s in ss.spans {
                    out.push(Got {
                        service: service.clone(),
                        name: s.name,
                        kind: s.kind,
                        trace: hex(&s.trace_id),
                        id: hex(&s.span_id),
                        parent: hex(&s.parent_span_id),
                        attributes: s
                            .attributes
                            .iter()
                            .map(|kv| (kv.key.clone(), text(kv.value.as_ref())))
                            .collect(),
                    });
                }
            }
        }
        Ok(tonic::Response::new(ExportTraceServiceResponse {
            partial_success: None,
        }))
    }
}

async fn start_collector() -> (SocketAddr, Collector) {
    let listener = tokio::net::TcpListener::bind(any_port()).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let collector = Collector::default();
    let svc = TraceServiceServer::new(collector.clone());
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(svc)
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
    );
    (addr, collector)
}

fn tracing(collector: SocketAddr, service: &str, ratio: f64) -> Arc<Tracing> {
    Arc::new(
        Tracing::otlp(&TraceConfig {
            endpoint: format!("http://{collector}"),
            service_name: service.into(),
            sample_ratio: ratio,
        })
        .unwrap(),
    )
}

async fn flush(t: &Arc<Tracing>) {
    let t = Arc::clone(t);
    tokio::task::spawn_blocking(move || t.flush())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn one_trace_spans_client_ferry_and_backend() {
    let (collector_addr, collector) = start_collector().await;
    let backend_tracing = tracing(collector_addr, "mock-a", 1.0);
    let backend = mock_http::spawn(
        any_port(),
        Behavior::named("a").with_tracing(Arc::clone(&backend_tracing)),
    )
    .await
    .unwrap();
    // Ferry samples no new traces of its own: it must follow the client's
    // sampled flag (parent-based sampling).
    let ferry_tracing = Tracing::otlp(&TraceConfig {
        endpoint: format!("http://{collector_addr}"),
        service_name: "ferry".into(),
        sample_ratio: 0.0,
    })
    .unwrap();
    let telemetry = Arc::new(Telemetry {
        metrics: Arc::new(Metrics::new()),
        tracing: Some(ferry_tracing),
        access_log: None,
    });
    let proxy = TestProxy::start_with_telemetry(
        &single_cluster_yaml(&[backend.addr()]),
        Arc::clone(&telemetry),
    )
    .await;

    // The client: a root span whose context goes out in traceparent.
    let client_tracing = tracing(collector_addr, "test-client", 1.0);
    let span = client_tracing.root_client_span("GET");
    let mut headers = http::HeaderMap::new();
    client_tracing.inject(&span, &mut headers);
    let traceparent = headers["traceparent"].to_str().unwrap().to_owned();
    let reply = ferry_integration_tests::send(
        proxy.addr,
        "/orders/7",
        &[("traceparent", traceparent.as_str())],
        false,
    )
    .await;
    assert_eq!(reply.status, 200);
    span.end_status(200);
    let trace_id = span.trace_id();
    let client_span_id = span.span_id();

    // What the backend received: Ferry's attempt span as parent, not the client's.
    let received = reply.echo()["headers"]["traceparent"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        received.starts_with(&format!("00-{trace_id}-")),
        "{received}"
    );
    assert!(
        !received.contains(&client_span_id),
        "Ferry inserts its own span"
    );

    flush(&client_tracing).await;
    flush(&backend_tracing).await;
    let ft = Arc::clone(&telemetry);
    tokio::task::spawn_blocking(move || ft.tracing.as_ref().unwrap().flush())
        .await
        .unwrap()
        .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let spans = loop {
        let spans: Vec<Got> = collector
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.trace == trace_id)
            .cloned()
            .collect();
        if spans.len() >= 4 {
            break spans;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "collector has {spans:#?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(spans.len(), 4, "{spans:#?}");
    let find = |service: &str, kind: SpanKind| {
        spans
            .iter()
            .find(|s| s.service == service && s.kind == kind as i32)
            .unwrap_or_else(|| panic!("no {kind:?} span from {service}: {spans:#?}"))
    };
    let client = find("test-client", SpanKind::Client);
    let ingress = find("ferry", SpanKind::Server);
    let egress = find("ferry", SpanKind::Client);
    let server = find("mock-a", SpanKind::Server);

    assert_eq!(client.id, client_span_id);
    assert!(client.parent.is_empty(), "the client span is the root");
    assert_eq!(
        ingress.parent, client.id,
        "Ferry continues the client's trace"
    );
    assert_eq!(
        egress.parent, ingress.id,
        "each attempt is a child of the request"
    );
    assert_eq!(
        server.parent, egress.id,
        "the backend continues Ferry's attempt span"
    );
    assert_eq!(received, format!("00-{trace_id}-{}-01", egress.id));

    assert_eq!(ingress.name, "GET all");
    assert_eq!(ingress.attributes["http.route"], "all");
    assert_eq!(ingress.attributes["http.response.status_code"], "200");
    assert_eq!(ingress.attributes["url.path"], "/orders/7");
    assert_eq!(ingress.attributes["ferry.cluster"], "c");
    assert_eq!(egress.attributes["server.address"], "127.0.0.1");
    assert_eq!(
        egress.attributes["server.port"],
        backend.addr().port().to_string()
    );
    assert_eq!(server.name, "GET /orders/7");
}

#[tokio::test(flavor = "multi_thread")]
async fn tracing_off_passes_traceparent_through() {
    let backend = mock_http::spawn(any_port(), Behavior::named("a"))
        .await
        .unwrap();
    let proxy = TestProxy::start(&single_cluster_yaml(&[backend.addr()])).await;
    let tp = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    let reply = ferry_integration_tests::send(proxy.addr, "/", &[("traceparent", tp)], false).await;
    assert_eq!(reply.echo()["headers"]["traceparent"], tp);
}
