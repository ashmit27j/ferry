//! OpenTelemetry tracing with W3C trace context (ADR 0028).
//!
//! Each proxied request gets a server span, a child of the client's
//! `traceparent` when it sent one, and each upstream attempt a client span
//! under it, whose context is injected into the upstream request. A backend
//! that continues the trace therefore appears below Ferry, and Ferry below
//! the client: client → Ferry (server span → client span) → backend.
//!
//! Spans are exported over OTLP/gRPC by the SDK's batch processor, which runs
//! on its own thread and drops spans rather than blocking when its queue is
//! full. Sampling is parent-based: Ferry follows the caller's decision and
//! samples new traces at a configurable ratio.
//!
//! When tracing is off, none of this runs and `traceparent` passes through
//! untouched like any other header.

use std::time::{Duration, SystemTime};

use http::HeaderMap;
use http::header::{HeaderName, HeaderValue};
use opentelemetry::propagation::{Extractor, Injector, TextMapPropagator};
use opentelemetry::trace::{SpanKind, Status, TraceContextExt, Tracer as _, TracerProvider as _};
use opentelemetry::{Context, KeyValue};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{Sampler, SdkTracer, SdkTracerProvider, SpanExporter};

use crate::TelemetryError;

/// Tracing settings.
#[derive(Debug, Clone, PartialEq)]
pub struct TraceConfig {
    /// OTLP/gRPC collector endpoint, e.g. `http://otel-collector:4317`.
    pub endpoint: String,
    /// `service.name` of Ferry's spans.
    pub service_name: String,
    /// Fraction (0–1) of new traces to sample. Requests that arrive with a
    /// `traceparent` follow its sampled flag instead.
    pub sample_ratio: f64,
}

/// A running tracer.
#[derive(Debug)]
pub struct Tracing {
    provider: SdkTracerProvider,
    tracer: SdkTracer,
    propagator: TraceContextPropagator,
}

impl Tracing {
    /// Exports to an OTLP/gRPC collector. Must be called inside a tokio
    /// runtime, which then carries the exporter's connection.
    ///
    /// # Errors
    ///
    /// Fails if the endpoint is not a valid URI.
    pub fn otlp(config: &TraceConfig) -> Result<Self, TelemetryError> {
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(config.endpoint.clone())
            .with_timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| TelemetryError::Tracing(e.to_string()))?;
        Ok(Self::with_exporter(
            exporter,
            &config.service_name,
            config.sample_ratio,
        ))
    }

    /// Exports to any span exporter, for tests and custom pipelines.
    #[must_use]
    pub fn with_exporter(
        exporter: impl SpanExporter + 'static,
        service_name: &str,
        sample_ratio: f64,
    ) -> Self {
        let provider = SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_sampler(Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
                sample_ratio.clamp(0.0, 1.0),
            ))))
            .with_resource(
                Resource::builder()
                    .with_service_name(service_name.to_owned())
                    .with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")))
                    .build(),
            )
            .build();
        let tracer = provider.tracer("ferry");
        Self {
            provider,
            tracer,
            propagator: TraceContextPropagator::new(),
        }
    }

    /// Starts the server span of a request, continuing the trace in its
    /// `traceparent` header if any.
    #[must_use]
    pub fn server_span(&self, headers: &HeaderMap, method: &str, start: SystemTime) -> ServerSpan {
        let parent = self.propagator.extract(&HeaderExtractor(headers));
        let span = self
            .tracer
            .span_builder(method.to_owned())
            .with_kind(SpanKind::Server)
            .with_start_time(start)
            .start_with_context(&self.tracer, &parent);
        ServerSpan {
            cx: parent.with_span(span),
        }
    }

    /// Starts a client span for one upstream attempt under `parent`.
    #[must_use]
    pub fn client_span(&self, parent: &ServerSpan, method: &str) -> ClientSpan {
        let span = self
            .tracer
            .span_builder(method.to_owned())
            .with_kind(SpanKind::Client)
            .start_with_context(&self.tracer, &parent.cx);
        ClientSpan {
            cx: parent.cx.with_span(span),
        }
    }

    /// Writes `traceparent` (and `tracestate`) for `span` into `headers`.
    pub fn inject(&self, span: &ClientSpan, headers: &mut HeaderMap) {
        self.propagator
            .inject_context(&span.cx, &mut HeaderInjector(headers));
    }

    /// Exports every finished span now. Blocks; call off the async workers.
    ///
    /// # Errors
    ///
    /// Fails if the exporter fails or times out.
    pub fn flush(&self) -> Result<(), TelemetryError> {
        self.provider
            .force_flush()
            .map_err(|e| TelemetryError::Tracing(e.to_string()))
    }

    /// Flushes and stops the exporter. Blocks; call off the async workers.
    ///
    /// # Errors
    ///
    /// Fails if the exporter fails or times out.
    pub fn shutdown(&self) -> Result<(), TelemetryError> {
        self.provider
            .shutdown_with_timeout(Duration::from_secs(5))
            .map_err(|e| TelemetryError::Tracing(e.to_string()))
    }
}

/// The span covering one downstream request.
#[derive(Debug)]
pub struct ServerSpan {
    cx: Context,
}

impl ServerSpan {
    /// Adds attributes.
    pub fn set_attributes(&self, attributes: impl IntoIterator<Item = KeyValue>) {
        self.cx.span().set_attributes(attributes);
    }

    /// Renames the span, e.g. to `GET <route>` once the route is known.
    pub fn rename(&self, name: String) {
        self.cx.span().update_name(name);
    }

    /// Hex trace ID, for access logs.
    #[must_use]
    pub fn trace_id(&self) -> String {
        self.cx.span().span_context().trace_id().to_string()
    }

    /// Whether this span is sampled (and so will be exported).
    #[must_use]
    pub fn is_sampled(&self) -> bool {
        self.cx.span().span_context().is_sampled()
    }

    /// Ends the span with the response status. Server spans are errors only
    /// for 5xx, per the HTTP semantic conventions.
    pub fn end(&self, status: u16, error: Option<&'static str>) {
        let span = self.cx.span();
        span.set_attribute(KeyValue::new(
            "http.response.status_code",
            i64::from(status),
        ));
        if let Some(e) = error {
            span.set_attribute(KeyValue::new("error.type", e));
        }
        if status >= 500 {
            span.set_status(Status::error(error.unwrap_or("server error")));
        }
        span.end();
    }
}

/// The span covering one upstream attempt.
#[derive(Debug)]
pub struct ClientSpan {
    cx: Context,
}

impl ClientSpan {
    /// Adds attributes.
    pub fn set_attributes(&self, attributes: impl IntoIterator<Item = KeyValue>) {
        self.cx.span().set_attributes(attributes);
    }

    /// Ends the span with the upstream's response status. Client spans are
    /// errors for 4xx and 5xx.
    pub fn end_status(&self, status: u16) {
        let span = self.cx.span();
        span.set_attribute(KeyValue::new(
            "http.response.status_code",
            i64::from(status),
        ));
        if status >= 400 {
            span.set_status(Status::error(""));
        }
        span.end();
    }

    /// Ends the span for an attempt that got no response.
    pub fn end_error(&self, error: &'static str) {
        let span = self.cx.span();
        span.set_attribute(KeyValue::new("error.type", error));
        span.set_status(Status::error(error));
        span.end();
    }
}

struct HeaderExtractor<'a>(&'a HeaderMap);

impl Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(HeaderName::as_str).collect()
    }
}

struct HeaderInjector<'a>(&'a mut HeaderMap);

impl Injector for HeaderInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(key.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            self.0.insert(name, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::{SpanId, TraceId};
    use opentelemetry_sdk::trace::InMemorySpanExporter;

    const PARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    fn tracing(ratio: f64) -> (Tracing, InMemorySpanExporter) {
        let exporter = InMemorySpanExporter::default();
        (
            Tracing::with_exporter(exporter.clone(), "ferry-test", ratio),
            exporter,
        )
    }

    #[test]
    fn continues_the_callers_trace_and_propagates_the_attempt() {
        let (t, exporter) = tracing(0.0);
        let mut incoming = HeaderMap::new();
        incoming.insert("traceparent", HeaderValue::from_static(PARENT));
        let server = t.server_span(&incoming, "GET", SystemTime::now());
        assert!(
            server.is_sampled(),
            "the caller's sampled flag wins over ratio 0"
        );
        assert_eq!(server.trace_id(), "4bf92f3577b34da6a3ce929d0e0e4736");
        let client = t.client_span(&server, "GET");
        let mut upstream = HeaderMap::new();
        t.inject(&client, &mut upstream);
        client.end_status(200);
        server.rename("GET api".into());
        server.end(200, None);
        t.flush().unwrap();

        let spans = exporter.get_finished_spans().unwrap();
        let s = spans
            .iter()
            .find(|s| s.span_kind == SpanKind::Server)
            .unwrap();
        let c = spans
            .iter()
            .find(|s| s.span_kind == SpanKind::Client)
            .unwrap();
        let trace = TraceId::from_hex("4bf92f3577b34da6a3ce929d0e0e4736").unwrap();
        assert_eq!(s.span_context.trace_id(), trace);
        assert_eq!(
            s.parent_span_id,
            SpanId::from_hex("00f067aa0ba902b7").unwrap()
        );
        assert_eq!(s.name, "GET api");
        assert_eq!(c.parent_span_id, s.span_context.span_id());
        let sent = upstream["traceparent"].to_str().unwrap();
        assert_eq!(
            sent,
            format!("00-{trace}-{}-01", c.span_context.span_id()),
            "the backend's parent is Ferry's client span"
        );
    }

    #[test]
    fn samples_new_traces_by_ratio() {
        let (t, exporter) = tracing(0.0);
        let server = t.server_span(&HeaderMap::new(), "GET", SystemTime::now());
        assert!(!server.is_sampled());
        let client = t.client_span(&server, "GET");
        let mut upstream = HeaderMap::new();
        t.inject(&client, &mut upstream);
        // Unsampled, but the context still propagates so backends agree.
        assert!(upstream["traceparent"].to_str().unwrap().ends_with("-00"));
        server.end(200, None);
        t.flush().unwrap();
        assert!(exporter.get_finished_spans().unwrap().is_empty());

        let (t, exporter) = tracing(1.0);
        t.server_span(&HeaderMap::new(), "GET", SystemTime::now())
            .end(503, Some("no_healthy_upstream"));
        t.flush().unwrap();
        let spans = exporter.get_finished_spans().unwrap();
        assert_eq!(spans.len(), 1);
        assert!(matches!(spans[0].status, Status::Error { .. }));
    }

    #[test]
    fn ignores_a_malformed_traceparent() {
        let (t, _) = tracing(1.0);
        let mut h = HeaderMap::new();
        h.insert("traceparent", HeaderValue::from_static("00-zz-yy-01"));
        let s = t.server_span(&h, "GET", SystemTime::now());
        assert!(s.is_sampled());
        assert_ne!(s.trace_id(), "00000000000000000000000000000000");
    }
}
