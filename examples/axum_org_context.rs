// SPDX-License-Identifier: MIT
//! Example axum server wiring the `OrgContextSpanEnricher` layer.
//!
//! Demonstrates the tower layer ordering required by ADR platform/0015:
//! the OrgContextSpanEnricher sits *inside* the [`axum::Extension`] layer that
//! injects `OrganizationContext` and *outside* the handler. The outer
//! [`otel_bootstrap::axum_layer`] opens an OTel server span; the enricher
//! records the four `enduser.*` attributes on the active tracing span for the
//! current request.
//!
//! Run with:
//! ```text
//! OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 \
//!   cargo run --example axum_org_context --features "grpc,axum,org-context"
//! ```

use axum::{Extension, Router, routing::get};
use quorum_identity::{OrgId, OrganizationContext, Principal, RequestId};
use std::error::Error;
use uuid::Uuid;

async fn hello() -> &'static str {
    tracing::info!("handling request");
    "hello"
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let handles = otel_bootstrap::init_telemetry("axum-org-context-example")?;

    let ctx = OrganizationContext::new(
        OrgId::new(Uuid::new_v4().to_string()),
        Principal::human(Uuid::new_v4()),
        RequestId::new(),
    );

    let _app: Router = Router::new()
        .route("/", get(hello))
        .layer(otel_bootstrap::org_context_span_enricher_layer())
        .layer(Extension(ctx))
        .layer(otel_bootstrap::axum_layer());

    handles.shutdown()?;
    Ok(())
}
