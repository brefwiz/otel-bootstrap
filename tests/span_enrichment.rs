// SPDX-License-Identifier: MIT
//! Integration tests for `SpanEnricherLayer<T>`.
//!
//! Run with:
//! ```bash
//! cargo test --features "axum,testing" --test span_enrichment
//! ```

#![cfg(all(feature = "axum", feature = "testing"))]

use axum::{
    Extension, Router,
    body::Body,
    http::{Request, StatusCode},
    routing::get,
};
use opentelemetry::{Key, Value, trace::TracerProvider as _};
use opentelemetry_sdk::trace::{
    InMemorySpanExporter, InMemorySpanExporterBuilder, SdkTracerProvider, SimpleSpanProcessor,
};
use otel_bootstrap::span_enrichment::{
    ENDUSER_ID, ENDUSER_ORG_ID, ENDUSER_ORG_PATH, ENDUSER_PRINCIPAL_KIND, EnrichSpan,
};
use tower::ServiceExt;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;
use tracing_subscriber::prelude::*;

/// Minimal context type used across all tests.
#[derive(Clone)]
struct TestCtx {
    user_id: String,
    org_id: String,
    org_path: Vec<String>,
    principal_kind: &'static str,
}

impl EnrichSpan for TestCtx {
    fn enrich_span(&self, span: &tracing::Span) {
        span.set_attribute(ENDUSER_ID, self.user_id.clone());
        span.set_attribute(ENDUSER_ORG_ID, self.org_id.clone());
        span.set_attribute(
            ENDUSER_ORG_PATH,
            opentelemetry::Value::Array(opentelemetry::Array::String(
                self.org_path
                    .iter()
                    .map(|s| opentelemetry::StringValue::from(s.clone()))
                    .collect(),
            )),
        );
        span.set_attribute(ENDUSER_PRINCIPAL_KIND, self.principal_kind);
    }
}

fn sample_ctx() -> TestCtx {
    TestCtx {
        user_id: "u_test".into(),
        org_id: "org_root".into(),
        org_path: vec!["org_root".into(), "org_leaf".into()],
        principal_kind: "user",
    }
}

fn isolated_tracer() -> (InMemorySpanExporter, SdkTracerProvider, tracing::Dispatch) {
    let exporter = InMemorySpanExporterBuilder::new().build();
    let provider = SdkTracerProvider::builder()
        .with_span_processor(SimpleSpanProcessor::new(exporter.clone()))
        .build();
    let tracer = provider.tracer("otel-bootstrap-test");
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
    let subscriber = tracing_subscriber::registry().with(otel_layer);
    let dispatch = tracing::Dispatch::new(subscriber);
    (exporter, provider, dispatch)
}

fn assert_enduser_attrs(attrs: &[opentelemetry::KeyValue], ctx: &TestCtx) {
    let by_key: std::collections::HashMap<_, _> = attrs
        .iter()
        .map(|kv| (kv.key.clone(), kv.value.clone()))
        .collect();

    assert_eq!(
        by_key
            .get(&Key::new(ENDUSER_ID))
            .map(|v| v.as_str().as_ref().to_owned()),
        Some(ctx.user_id.clone()),
        "missing or wrong {ENDUSER_ID}"
    );
    assert_eq!(
        by_key
            .get(&Key::new(ENDUSER_ORG_ID))
            .map(|v| v.as_str().as_ref().to_owned()),
        Some(ctx.org_id.clone()),
        "missing or wrong {ENDUSER_ORG_ID}"
    );
    assert_eq!(
        by_key
            .get(&Key::new(ENDUSER_PRINCIPAL_KIND))
            .map(|v| v.as_str().as_ref().to_owned()),
        Some(ctx.principal_kind.to_owned()),
        "missing or wrong {ENDUSER_PRINCIPAL_KIND}"
    );

    let path = by_key
        .get(&Key::new(ENDUSER_ORG_PATH))
        .unwrap_or_else(|| panic!("missing {ENDUSER_ORG_PATH}"));
    match path {
        Value::Array(opentelemetry::Array::String(segments)) => {
            let got: Vec<String> = segments.iter().map(|s| s.as_str().to_owned()).collect();
            assert_eq!(got, ctx.org_path, "org_path must be a typed string array");
        }
        other => panic!("enduser.org_path must be Value::Array(Array::String), got {other:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn layer_records_all_enduser_attributes_when_extension_present() {
    let (exporter, provider, dispatch) = isolated_tracer();
    let _guard = tracing::dispatcher::set_default(&dispatch);

    let ctx = sample_ctx();

    let app: Router = Router::new()
        .route(
            "/x",
            get(|| async {
                let _s = tracing::info_span!("handler").entered();
                "ok"
            }),
        )
        .layer(otel_bootstrap::span_enricher_layer::<TestCtx>())
        .layer(Extension(ctx.clone()));

    let root_span = tracing::info_span!("http.request");
    let _enter = root_span.enter();

    let response = app
        .oneshot(Request::builder().uri("/x").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    drop(_enter);
    drop(root_span);
    provider.force_flush().ok();

    let spans = exporter.get_finished_spans().expect("spans exportable");
    let request_span = spans
        .iter()
        .find(|s| s.name == "http.request")
        .expect("http.request span exported");

    assert_enduser_attrs(&request_span.attributes, &ctx);
}

#[tokio::test(flavor = "current_thread")]
async fn direct_enrich_span_call_records_attributes() {
    let (exporter, provider, dispatch) = isolated_tracer();
    let _guard = tracing::dispatcher::set_default(&dispatch);

    let ctx = sample_ctx();

    let worker_span = tracing::info_span!("job.process");
    {
        let _enter = worker_span.enter();
        ctx.enrich_span(&tracing::Span::current());
    }
    drop(worker_span);
    provider.force_flush().ok();

    let spans = exporter.get_finished_spans().expect("spans exportable");
    let job_span = spans
        .iter()
        .find(|s| s.name == "job.process")
        .expect("job.process span exported");

    assert_enduser_attrs(&job_span.attributes, &ctx);
}

#[tokio::test(flavor = "current_thread")]
async fn missing_extension_is_noop() {
    let (exporter, provider, dispatch) = isolated_tracer();
    let _guard = tracing::dispatcher::set_default(&dispatch);

    let app: Router = Router::new()
        .route("/health", get(|| async { "ok" }))
        .layer(otel_bootstrap::span_enricher_layer::<TestCtx>());

    let root_span = tracing::info_span!("platform.request");
    let _enter = root_span.enter();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    drop(_enter);
    drop(root_span);
    provider.force_flush().ok();

    let spans = exporter.get_finished_spans().expect("spans exportable");
    let request_span = spans
        .iter()
        .find(|s| s.name == "platform.request")
        .expect("platform.request span exported");

    for kv in &request_span.attributes {
        let key = kv.key.as_str();
        assert!(
            !key.starts_with("enduser."),
            "platform-scope request must not emit enduser.* attrs, found {key}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn child_span_shares_trace_id_with_enriched_parent() {
    let (exporter, provider, dispatch) = isolated_tracer();
    let _guard = tracing::dispatcher::set_default(&dispatch);

    let ctx = sample_ctx();

    let parent = tracing::info_span!("parent");
    {
        let _enter = parent.enter();
        ctx.enrich_span(&tracing::Span::current());
        let child = tracing::info_span!("child");
        let _c = child.enter();
        tracing::info!("work done");
    }
    drop(parent);
    provider.force_flush().ok();

    let spans = exporter.get_finished_spans().expect("spans exportable");
    let parent_span = spans
        .iter()
        .find(|s| s.name == "parent")
        .expect("parent span");
    let child_span = spans
        .iter()
        .find(|s| s.name == "child")
        .expect("child span");

    assert_eq!(
        parent_span.span_context.trace_id(),
        child_span.span_context.trace_id(),
        "child must share parent trace_id"
    );
}
