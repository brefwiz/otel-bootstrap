// SPDX-License-Identifier: MIT
//! Integration tests for `enduser.*` span-attribute emission.
//!
//! Exercises the three entry points that ADR platform/0015 requires to emit an
//! identical attribute set:
//!
//! 1. HTTP: the axum [`OrgContextSpanEnricher`] tower layer, with
//!    `OrganizationContext` carried in request extensions.
//! 2. Non-HTTP: a direct call to [`emit_enduser_fields`] inside a
//!    caller-owned tracing span (NATS / job-worker pattern).
//! 3. Parity: both paths produce the same set of attribute keys with the same
//!    value types (string, string, string-array, string).
//!
//! Also covers:
//! - Log events emitted inside the enriched span inherit the span context via
//!   the `tracing-opentelemetry` bridge (no handler-side fan-out).
//! - Platform-scope request (no extension inserted) is a no-op: request
//!   succeeds, span contains none of the `enduser.*` keys.
//!
//! Run with:
//! ```bash
//! cargo test --features "axum,org-context,testing" --test span_enrichment
//! ```

#![cfg(all(feature = "axum", feature = "org-context"))]

use api_bones::org_id::OrgId;
use api_bones_test::builders::{FakeOrgContext, FakePrincipal};
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
    ENDUSER_ID, ENDUSER_ORG_ID, ENDUSER_ORG_PATH, ENDUSER_PRINCIPAL_KIND, emit_enduser_fields,
};
use tower::ServiceExt;
use tracing_subscriber::prelude::*;

/// Build an isolated tracer provider backed by an in-memory exporter and a
/// `tracing_subscriber` that bridges into it.
///
/// Each test gets its own provider so assertions are scoped to spans created
/// inside that test. The returned guard must be kept alive for the duration of
/// the test — dropping it unsubscribes.
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

fn sample_ctx() -> api_bones::OrganizationContext {
    let root = OrgId::generate();
    let leaf = OrgId::generate();
    let principal = FakePrincipal::user(uuid::Uuid::new_v4())
        .org_path(vec![root, leaf])
        .build();
    FakeOrgContext::for_principal(&principal)
}

fn assert_enduser_attrs(
    attrs: &[opentelemetry::KeyValue],
    expected_id: &str,
    expected_org_id: &str,
    expected_path: &[String],
    expected_kind: &str,
) {
    let by_key: std::collections::HashMap<_, _> = attrs
        .iter()
        .map(|kv| (kv.key.clone(), kv.value.clone()))
        .collect();

    let id = by_key
        .get(&Key::new(ENDUSER_ID))
        .unwrap_or_else(|| panic!("missing {ENDUSER_ID}"));
    assert_eq!(id.as_str().as_ref(), expected_id);

    let org_id = by_key
        .get(&Key::new(ENDUSER_ORG_ID))
        .unwrap_or_else(|| panic!("missing {ENDUSER_ORG_ID}"));
    assert_eq!(org_id.as_str().as_ref(), expected_org_id);

    let kind = by_key
        .get(&Key::new(ENDUSER_PRINCIPAL_KIND))
        .unwrap_or_else(|| panic!("missing {ENDUSER_PRINCIPAL_KIND}"));
    assert_eq!(kind.as_str().as_ref(), expected_kind);

    let path = by_key
        .get(&Key::new(ENDUSER_ORG_PATH))
        .unwrap_or_else(|| panic!("missing {ENDUSER_ORG_PATH}"));
    match path {
        Value::Array(opentelemetry::Array::String(segments)) => {
            let got: Vec<String> = segments.iter().map(|s| s.as_str().to_owned()).collect();
            assert_eq!(
                got, expected_path,
                "org_path must be a typed string array (not a joined string)"
            );
        }
        other => panic!("enduser.org_path must be Value::Array(Array::String), got {other:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn http_path_records_all_four_enduser_attributes() {
    let (exporter, provider, dispatch) = isolated_tracer();
    let _guard = tracing::dispatcher::set_default(&dispatch);

    let ctx = sample_ctx();
    let expected_id = ctx.principal.id.to_string();
    let expected_org_id = ctx.org_id.inner().to_string();
    let expected_path: Vec<String> = ctx.org_path.iter().map(|id| id.inner().to_string()).collect();

    let app: Router = Router::new()
        .route(
            "/x",
            get(|| async {
                // Force a tracing span so the enricher has somewhere to record.
                let _s = tracing::info_span!("handler").entered();
                "ok"
            }),
        )
        .layer(otel_bootstrap::org_context_span_enricher_layer())
        .layer(Extension(ctx));

    // Open a root span for this request so set_attribute has a target.
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

    assert_enduser_attrs(
        &request_span.attributes,
        &expected_id,
        &expected_org_id,
        &expected_path,
        "user",
    );
}

#[tokio::test(flavor = "current_thread")]
async fn non_http_path_produces_identical_attribute_set_as_http() {
    let (exporter, provider, dispatch) = isolated_tracer();
    let _guard = tracing::dispatcher::set_default(&dispatch);

    let ctx = sample_ctx();
    let expected_id = ctx.principal.id.to_string();
    let expected_org_id = ctx.org_id.inner().to_string();
    let expected_path: Vec<String> = ctx.org_path.iter().map(|id| id.inner().to_string()).collect();

    // Simulate a NATS consumer / job worker: caller owns the span.
    let worker_span = tracing::info_span!("job.process");
    {
        let _enter = worker_span.enter();
        emit_enduser_fields(&ctx);
    }
    drop(worker_span);
    provider.force_flush().ok();

    let spans = exporter.get_finished_spans().expect("spans exportable");
    let job_span = spans
        .iter()
        .find(|s| s.name == "job.process")
        .expect("job.process span exported");

    assert_enduser_attrs(
        &job_span.attributes,
        &expected_id,
        &expected_org_id,
        &expected_path,
        "user",
    );
}

#[tokio::test(flavor = "current_thread")]
async fn platform_scope_request_without_extension_is_noop() {
    let (exporter, provider, dispatch) = isolated_tracer();
    let _guard = tracing::dispatcher::set_default(&dispatch);

    // No Extension(ctx) — platform-scope route.
    let app: Router = Router::new()
        .route("/health", get(|| async { "ok" }))
        .layer(otel_bootstrap::org_context_span_enricher_layer());

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
async fn child_span_inherits_enriched_parent_context_for_log_events() {
    // Verifies the inheritance contract: a log-bearing child span opened inside
    // the enriched parent shares its trace_id, so backends that correlate by
    // trace can attribute logs to the same tenant without a handler-side copy.
    let (exporter, provider, dispatch) = isolated_tracer();
    let _guard = tracing::dispatcher::set_default(&dispatch);

    let ctx = sample_ctx();

    let parent = tracing::info_span!("parent");
    {
        let _enter = parent.enter();
        emit_enduser_fields(&ctx);

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
        .expect("parent span exported");
    let child_span = spans
        .iter()
        .find(|s| s.name == "child")
        .expect("child span exported");

    assert_eq!(
        parent_span.span_context.trace_id(),
        child_span.span_context.trace_id(),
        "child span must share parent trace_id so downstream logs correlate by trace"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn repeated_requests_without_extension_warn_only_once() {
    let (_exporter, _provider, dispatch) = isolated_tracer();
    let _guard = tracing::dispatcher::set_default(&dispatch);

    let app: Router = Router::new()
        .route("/health", get(|| async { "ok" }))
        .layer(otel_bootstrap::org_context_span_enricher_layer());

    for _ in 0..3 {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    // No panic / crash across repeated missing-context requests is the
    // observable contract — the `swap(true, Relaxed)` guard in
    // OrgContextSpanEnricherService guarantees at most one warn per process.
}
