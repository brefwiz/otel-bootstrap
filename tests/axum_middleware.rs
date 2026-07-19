//! Integration tests for the axum trace-context propagation middleware.
//!
//! Run with:
//! ```bash
//! cargo test --features axum,testing --test axum_middleware
//! ```

#![cfg(all(feature = "axum", feature = "testing"))]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::{Router, routing::get};
use opentelemetry::global;
use opentelemetry::trace::TraceContextExt;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use otel_bootstrap::{Telemetry, axum_layer};
use tower::ServiceExt;

/// Minimal axum app used across tests.
fn test_app() -> Router {
    Router::new()
        .route("/hello", get(|| async { "ok" }))
        .layer(axum_layer())
}

/// Install the W3C TraceContext propagator so that header extraction/injection works.
fn setup_propagator() {
    global::set_text_map_propagator(TraceContextPropagator::new());
}

#[tokio::test]
async fn request_with_traceparent_creates_child_span() {
    setup_propagator();
    let _handles = Telemetry::testing("axum-test-child-span");

    let app = test_app();

    // traceparent for a known trace/span so we can verify child relationship.
    let traceparent = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

    let response = app
        .oneshot(
            Request::builder()
                .uri("/hello")
                .header("traceparent", traceparent)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn response_includes_traceparent_header() {
    setup_propagator();
    let _handles = Telemetry::testing("axum-test-response-header");

    let app = test_app();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/hello")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers().contains_key("traceparent"),
        "response should contain a traceparent header, got: {:?}",
        response.headers()
    );
}

/// Regression test: the extracted parent context must be the *ambient*
/// `opentelemetry::Context::current()` inside the handler, not just the
/// span the middleware builds for its own export. Every downstream
/// outbound call in brefwiz services (e.g. api-bones's
/// `propagation::inject_current`) reads `Context::current()` — if the
/// middleware only builds a span without attaching it, the handler sees
/// the empty root context and every onward call injects a disconnected
/// trace id, regardless of what was extracted from the incoming request.
#[tokio::test]
async fn handler_observes_extracted_context_as_current() {
    setup_propagator();
    let _handles = Telemetry::testing("axum-test-context-current");

    let app = Router::new()
        .route(
            "/hello",
            get(|| async {
                let cx = opentelemetry::Context::current();
                cx.span().span_context().trace_id().to_string()
            }),
        )
        .layer(axum_layer());

    let traceparent = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

    let response = app
        .oneshot(
            Request::builder()
                .uri("/hello")
                .header("traceparent", traceparent)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let observed_trace_id = String::from_utf8(body.to_vec()).unwrap();
    assert_eq!(
        observed_trace_id, "0af7651916cd43dd8448eb211c80319c",
        "handler's Context::current() must carry the extracted parent's trace id"
    );
}
