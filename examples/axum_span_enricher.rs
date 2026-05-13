// SPDX-License-Identifier: MIT
//! Example axum server wiring the `SpanEnricherLayer`.
//!
//! Run with:
//! ```text
//! OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 \
//!   cargo run --example axum_span_enricher --features "grpc,axum"
//! ```

use axum::{Extension, Router, routing::get};
use otel_bootstrap::span_enrichment::EnrichSpan;
use std::error::Error;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

#[derive(Clone)]
struct RequestCtx {
    user_id: String,
    org_id: String,
}

impl EnrichSpan for RequestCtx {
    fn enrich_span(&self, span: &tracing::Span) {
        span.set_attribute("enduser.id", self.user_id.clone());
        span.set_attribute("enduser.org_id", self.org_id.clone());
    }
}

async fn hello() -> &'static str {
    tracing::info!("handling request");
    "hello"
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let handles = otel_bootstrap::init_telemetry("axum-span-enricher-example")?;

    let ctx = RequestCtx {
        user_id: "u_example".into(),
        org_id: "org_example".into(),
    };

    let _app: Router = Router::new()
        .route("/", get(hello))
        .layer(otel_bootstrap::span_enricher_layer::<RequestCtx>())
        .layer(Extension(ctx))
        .layer(otel_bootstrap::axum_layer());

    handles.shutdown()?;
    Ok(())
}
