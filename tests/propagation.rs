//! Integration tests for W3C TraceContext propagation.
//!
//! These tests verify that after `init_telemetry`, trace context is correctly
//! injected into and extracted from HTTP-style header maps.

#![cfg(feature = "integration-tests")]

use opentelemetry::{
    Context, global,
    propagation::Injector,
    trace::{SpanKind, TraceContextExt, Tracer},
};
use std::collections::HashMap;

struct HeaderMap(HashMap<String, String>);

impl Injector for HeaderMap {
    fn set(&mut self, key: &str, value: String) {
        self.0.insert(key.to_lowercase(), value);
    }
}

impl opentelemetry::propagation::Extractor for HeaderMap {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(&key.to_lowercase()).map(|v| v.as_str())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|k| k.as_str()).collect()
    }
}

#[tokio::test]
async fn inject_traceparent_header() {
    let _handles = otel_bootstrap::init_telemetry("test-inject").unwrap();

    let tracer = global::tracer("test");
    let span = tracer
        .span_builder("test-span")
        .with_kind(SpanKind::Client)
        .start(&tracer);
    let cx = Context::current_with_span(span);

    let mut headers = HeaderMap(HashMap::new());
    global::get_text_map_propagator(|propagator| {
        propagator.inject_context(&cx, &mut headers);
    });

    assert!(
        headers.0.contains_key("traceparent"),
        "traceparent header must be injected; got: {:?}",
        headers.0
    );

    let traceparent = &headers.0["traceparent"];
    assert!(
        traceparent.starts_with("00-"),
        "traceparent must start with version 00: {traceparent}"
    );
}

#[tokio::test]
async fn extract_traceparent_header() {
    let _handles = otel_bootstrap::init_telemetry("test-extract").unwrap();

    let mut headers = HeaderMap(HashMap::new());
    headers.0.insert(
        "traceparent".to_string(),
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".to_string(),
    );

    let cx = global::get_text_map_propagator(|propagator| propagator.extract(&headers));

    let span_context = cx.span().span_context().clone();
    assert!(
        span_context.is_valid(),
        "extracted span context must be valid"
    );
    assert_eq!(
        span_context.trace_id().to_string(),
        "4bf92f3577b34da6a3ce929d0e0e4736"
    );
    assert_eq!(span_context.span_id().to_string(), "00f067aa0ba902b7");
}
