// SPDX-License-Identifier: MIT
//! Span enrichment via the [`EnrichSpan`] trait.
//!
//! Implement [`EnrichSpan`] on any request-context type, then wire it into
//! [`crate::span_enricher_layer`] to record attributes on the active span for
//! every incoming request.
//!
//! # Example
//! ```no_run
//! use otel_bootstrap::span_enrichment::EnrichSpan;
//! use tracing_opentelemetry::OpenTelemetrySpanExt as _;
//!
//! #[derive(Clone)]
//! struct MyCtx { user_id: String }
//!
//! impl EnrichSpan for MyCtx {
//!     fn enrich_span(&self, span: &tracing::Span) {
//!         span.set_attribute("enduser.id", self.user_id.clone());
//!     }
//! }
//! ```

/// `enduser.id` — the principal's opaque identifier.
pub const ENDUSER_ID: &str = "enduser.id";
/// `enduser.org_id` — the tenant UUID as a string.
pub const ENDUSER_ORG_ID: &str = "enduser.org_id";
/// `enduser.org_path` — typed array of tenant-path UUID strings, root-first.
pub const ENDUSER_ORG_PATH: &str = "enduser.org_path";
/// `enduser.principal_kind` — `"user"`, `"service"`, or `"system"`.
pub const ENDUSER_PRINCIPAL_KIND: &str = "enduser.principal_kind";

/// Implement on any request-context type to drive [`crate::span_enricher_layer`].
///
/// The method receives the **active** tracing span. Use
/// [`tracing_opentelemetry::OpenTelemetrySpanExt::set_attribute`] to record
/// attributes that flow through to the OTLP backend.
pub trait EnrichSpan {
    fn enrich_span(&self, span: &tracing::Span);
}
