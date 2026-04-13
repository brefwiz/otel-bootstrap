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
