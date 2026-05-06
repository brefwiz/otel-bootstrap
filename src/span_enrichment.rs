// SPDX-License-Identifier: MIT
//! Canonical `enduser.*` span-attribute emission from [`OrganizationContext`].
//!
//! Every span at or below the request boundary carries
//!
//! - `enduser.id = ctx.principal.id`
//! - `enduser.org_id = ctx.org_id` (UUID string)
//! - `enduser.org_path = ctx.org_path` (typed array of UUID strings, root-first)
//! - `enduser.principal_kind = ctx.principal.kind`
//!
//! Attribute keys follow OpenTelemetry semantic conventions for `enduser.*`;
//! avoid introducing vendor-prefixed variants.
//!
//! The attribute emitter is a single helper — [`emit_enduser_fields`] — that
//! the HTTP middleware, NATS consumers, and job workers all call. The
//! resulting attributes live on the currently active `tracing::Span`, so
//! structured logs emitted inside that span inherit them via span context
//! with no handler-side change.
//!
//! # Example
//! ```no_run
//! # #[cfg(feature = "org-context")] {
//! use quorum_identity::{OrganizationContext, OrgId, Principal, RequestId};
//! use uuid::Uuid;
//!
//! let ctx = OrganizationContext::new(
//!     OrgId::new(Uuid::new_v4().to_string()),
//!     Principal::human(Uuid::new_v4()),
//!     RequestId::new(),
//! );
//!
//! // Inside a tracing span:
//! let _enter = tracing::info_span!("handle_request").entered();
//! otel_bootstrap::span_enrichment::emit_enduser_fields(&ctx);
//! # }
//! ```

use opentelemetry::{Array, KeyValue, StringValue, Value};
use quorum_identity::{OrganizationContext, PrincipalKind};
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// `enduser.id` — the principal's opaque identifier.
pub const ENDUSER_ID: &str = "enduser.id";
/// `enduser.org_id` — the tenant UUID as a string.
pub const ENDUSER_ORG_ID: &str = "enduser.org_id";
/// `enduser.org_path` — typed array of tenant-path UUID strings, root-first.
pub const ENDUSER_ORG_PATH: &str = "enduser.org_path";
/// `enduser.principal_kind` — `"user"`, `"service"`, or `"system"`.
pub const ENDUSER_PRINCIPAL_KIND: &str = "enduser.principal_kind";

/// Record the four canonical `enduser.*` attributes on the currently active
/// `tracing::Span`.
///
/// The bridge to OpenTelemetry goes through
/// [`tracing_opentelemetry::OpenTelemetrySpanExt::set_attribute`], so the
/// attributes are visible to any backend fed by the tracing-opentelemetry
/// layer.
///
/// When called outside an active tracing span (e.g. a platform-scope code
/// path) this is a no-op.
pub fn emit_enduser_fields(ctx: &OrganizationContext) {
    emit_enduser_fields_on(&tracing::Span::current(), ctx);
}

/// Record the four canonical `enduser.*` attributes on a specific
/// `tracing::Span`.
///
/// Prefer [`emit_enduser_fields`] in request-boundary code; this variant
/// exists for callers that thread an explicit span (non-HTTP entry points
/// that construct the span themselves, tests that assert on a captured span).
pub fn emit_enduser_fields_on(span: &tracing::Span, ctx: &OrganizationContext) {
    span.set_attribute(ENDUSER_ID, ctx.principal.id.as_str().to_owned());
    span.set_attribute(ENDUSER_ORG_ID, ctx.org_id.as_str().to_owned());
    span.set_attribute(
        ENDUSER_ORG_PATH,
        Value::Array(Array::String(
            ctx.org_path
                .iter()
                .map(|id| StringValue::from(id.as_str().to_owned()))
                .collect(),
        )),
    );
    span.set_attribute(
        ENDUSER_PRINCIPAL_KIND,
        principal_kind_str(ctx.principal.kind),
    );
}

/// Render a [`PrincipalKind`] as the lowercase string used for the
/// `enduser.principal_kind` attribute value.
pub(crate) fn principal_kind_str(kind: PrincipalKind) -> &'static str {
    match kind {
        PrincipalKind::User => "user",
        PrincipalKind::Service => "service",
        PrincipalKind::System => "system",
        // `PrincipalKind` is `#[non_exhaustive]`. Any variant added upstream
        // before otel-bootstrap updates this match lands here.
        _ => "unknown",
    }
}

/// Build the four `enduser.*` [`KeyValue`]s without touching any span.
///
/// Exposed for callers that cannot use the tracing bridge (for example,
/// direct OpenTelemetry callers that record attributes against a span
/// reference they already hold) and to keep the HTTP, NATS, and job paths
/// producing identical attribute sets from one source.
#[must_use]
pub fn enduser_key_values(ctx: &OrganizationContext) -> [KeyValue; 4] {
    [
        KeyValue::new(ENDUSER_ID, ctx.principal.id.as_str().to_owned()),
        KeyValue::new(ENDUSER_ORG_ID, ctx.org_id.as_str().to_owned()),
        KeyValue::new(
            ENDUSER_ORG_PATH,
            Value::Array(Array::String(
                ctx.org_path
                    .iter()
                    .map(|id| StringValue::from(id.as_str().to_owned()))
                    .collect(),
            )),
        ),
        KeyValue::new(
            ENDUSER_PRINCIPAL_KIND,
            principal_kind_str(ctx.principal.kind),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::Key;
    use quorum_identity::{OrgId, Principal, RequestId};
    use uuid::Uuid;

    fn new_org_id() -> OrgId {
        OrgId::new(Uuid::new_v4().to_string())
    }

    fn ctx_with_path(org_id: OrgId, parents: &[OrgId]) -> OrganizationContext {
        let mut path = parents.to_vec();
        path.push(org_id.clone());
        OrganizationContext::new(org_id, Principal::human(Uuid::new_v4()), RequestId::new())
            .with_org_path(path)
    }

    #[test]
    fn principal_kind_str_maps_each_variant() {
        assert_eq!(principal_kind_str(PrincipalKind::User), "user");
        assert_eq!(principal_kind_str(PrincipalKind::Service), "service");
        assert_eq!(principal_kind_str(PrincipalKind::System), "system");
    }

    #[test]
    fn enduser_key_values_has_all_four_attributes() {
        let org_id = new_org_id();
        let ctx = ctx_with_path(org_id, &[]);
        let kvs = enduser_key_values(&ctx);

        let keys: Vec<&str> = kvs.iter().map(|kv| kv.key.as_str()).collect();
        assert!(keys.contains(&ENDUSER_ID));
        assert!(keys.contains(&ENDUSER_ORG_ID));
        assert!(keys.contains(&ENDUSER_ORG_PATH));
        assert!(keys.contains(&ENDUSER_PRINCIPAL_KIND));
    }

    #[test]
    fn enduser_org_path_is_a_string_array_not_a_joined_string() {
        let root = new_org_id();
        let leaf = new_org_id();
        let ctx = ctx_with_path(leaf.clone(), &[root.clone()]);
        let kvs = enduser_key_values(&ctx);

        let org_path = kvs
            .iter()
            .find(|kv| kv.key == Key::new(ENDUSER_ORG_PATH))
            .expect("org_path KeyValue present");

        match &org_path.value {
            Value::Array(Array::String(segments)) => {
                assert_eq!(segments.len(), 2, "root + leaf = 2 segments");
                assert_eq!(segments[0].as_str(), root.as_str());
                assert_eq!(segments[1].as_str(), leaf.as_str());
            }
            other => panic!("expected Value::Array(Array::String), got {other:?}"),
        }
    }

    #[test]
    fn enduser_org_path_empty_when_path_is_empty() {
        let org_id = new_org_id();
        let ctx =
            OrganizationContext::new(org_id, Principal::human(Uuid::new_v4()), RequestId::new());
        let kvs = enduser_key_values(&ctx);

        let org_path = kvs
            .iter()
            .find(|kv| kv.key == Key::new(ENDUSER_ORG_PATH))
            .expect("org_path KeyValue present");

        match &org_path.value {
            Value::Array(Array::String(segments)) => assert!(segments.is_empty()),
            other => panic!("expected empty Value::Array(Array::String), got {other:?}"),
        }
    }

    #[test]
    fn enduser_principal_kind_for_human_is_user() {
        let org_id = new_org_id();
        let ctx = ctx_with_path(org_id, &[]);
        let kvs = enduser_key_values(&ctx);

        let kind = kvs
            .iter()
            .find(|kv| kv.key == Key::new(ENDUSER_PRINCIPAL_KIND))
            .expect("principal_kind KeyValue present");

        assert_eq!(kind.value.as_str().as_ref(), "user");
    }

    #[test]
    fn enduser_principal_kind_for_system_is_system() {
        let org_id = new_org_id();
        let ctx = OrganizationContext::new(
            org_id,
            Principal::system("otel-bootstrap.test"),
            RequestId::new(),
        );
        let kvs = enduser_key_values(&ctx);

        let kind = kvs
            .iter()
            .find(|kv| kv.key == Key::new(ENDUSER_PRINCIPAL_KIND))
            .expect("principal_kind KeyValue present");

        assert_eq!(kind.value.as_str().as_ref(), "system");
    }

    #[test]
    fn enduser_org_id_is_the_string_of_ctx_org_id() {
        let org_id = new_org_id();
        let ctx = ctx_with_path(org_id.clone(), &[]);
        let kvs = enduser_key_values(&ctx);

        let kv_org_id = kvs
            .iter()
            .find(|kv| kv.key == Key::new(ENDUSER_ORG_ID))
            .expect("org_id KeyValue present");

        assert_eq!(kv_org_id.value.as_str().as_ref(), org_id.as_str());
    }

    #[test]
    fn enduser_id_is_the_principal_id_string() {
        let org_id = new_org_id();
        let id = Uuid::new_v4();
        let ctx = OrganizationContext::new(org_id, Principal::human(id), RequestId::new());
        let kvs = enduser_key_values(&ctx);

        let kv_id = kvs
            .iter()
            .find(|kv| kv.key == Key::new(ENDUSER_ID))
            .expect("id KeyValue present");

        assert_eq!(kv_id.value.as_str().as_ref(), id.to_string());
    }

    #[test]
    fn emit_enduser_fields_is_noop_without_active_span() {
        let org_id = new_org_id();
        let ctx = ctx_with_path(org_id, &[]);
        emit_enduser_fields(&ctx);
    }

    #[test]
    fn emit_enduser_fields_on_disabled_span_is_infallible() {
        let org_id = new_org_id();
        let ctx = ctx_with_path(org_id, &[]);
        emit_enduser_fields_on(&tracing::Span::none(), &ctx);
    }
}
