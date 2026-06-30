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

/// `request.id` — UUIDv7 assigned per inbound request.
pub const REQUEST_ID: &str = "request.id";

/// Emit `request.id` on the current span.
///
/// Dual-writes to the OTLP trace attribute and to [`crate::SpanLogAttrs`] so
/// the value propagates into every OTLP log record emitted within the span.
pub fn emit_request_id(request_id: &str) {
    emit_request_id_on(&tracing::Span::current(), request_id);
}

/// Emit `request.id` on an explicit span.
pub fn emit_request_id_on(span: &tracing::Span, request_id: &str) {
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;
    span.set_attribute(REQUEST_ID, request_id.to_owned());
    crate::log_bridge::record_span_log_attr_on(
        span,
        opentelemetry::Key::new(REQUEST_ID),
        opentelemetry::logs::AnyValue::String(request_id.to_owned().into()),
    );
}

/// Implement on any request-context type to drive [`crate::span_enricher_layer`].
///
/// The method receives the **active** tracing span. Use
/// [`tracing_opentelemetry::OpenTelemetrySpanExt::set_attribute`] to record
/// attributes that flow through to the OTLP backend.
pub trait EnrichSpan {
    fn enrich_span(&self, span: &tracing::Span);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log_bridge::{SpanAwareLogBridge, SpanLogAttrs};
    use opentelemetry::{
        InstrumentationScope, Key,
        logs::{AnyValue, LogRecord, Logger, LoggerProvider},
    };
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::registry::LookupSpan;

    #[derive(Default, Clone)]
    struct Rec {
        attributes: Vec<(Key, AnyValue)>,
    }
    #[derive(Default, Clone)]
    struct Cap(Arc<Mutex<Rec>>);
    impl LogRecord for Cap {
        fn set_event_name(&mut self, _: &'static str) {}
        fn set_target<T: Into<std::borrow::Cow<'static, str>>>(&mut self, _: T) {}
        fn set_timestamp(&mut self, _: std::time::SystemTime) {}
        fn set_observed_timestamp(&mut self, _: std::time::SystemTime) {}
        fn set_severity_text(&mut self, _: &'static str) {}
        fn set_severity_number(&mut self, _: opentelemetry::logs::Severity) {}
        fn set_body(&mut self, _: AnyValue) {}
        fn add_attributes<I, K, V>(&mut self, attrs: I)
        where
            I: IntoIterator<Item = (K, V)>,
            K: Into<Key>,
            V: Into<AnyValue>,
        {
            let mut g = self.0.lock().unwrap();
            for (k, v) in attrs {
                g.attributes.push((k.into(), v.into()));
            }
        }
        fn add_attribute<K, V>(&mut self, k: K, v: V)
        where
            K: Into<Key>,
            V: Into<AnyValue>,
        {
            self.0.lock().unwrap().attributes.push((k.into(), v.into()));
        }
    }
    #[derive(Clone, Default)]
    struct CapLogger {
        records: Arc<Mutex<Vec<Rec>>>,
    }
    impl Logger for CapLogger {
        type LogRecord = Cap;
        fn create_log_record(&self) -> Cap {
            Cap(Arc::new(Mutex::new(Rec::default())))
        }
        fn emit(&self, r: Cap) {
            self.records
                .lock()
                .unwrap()
                .push(r.0.lock().unwrap().clone());
        }
        fn event_enabled(
            &self,
            _level: opentelemetry::logs::Severity,
            _target: &str,
            _name: Option<&str>,
        ) -> bool {
            true
        }
    }
    #[derive(Clone, Default)]
    struct CapProvider {
        logger: CapLogger,
    }
    impl LoggerProvider for CapProvider {
        type Logger = CapLogger;
        fn logger_with_scope(&self, _: InstrumentationScope) -> CapLogger {
            self.logger.clone()
        }
    }

    #[test]
    fn emit_request_id_on_populates_span_log_attrs() {
        let provider = CapProvider::default();
        let records = provider.logger.records.clone();
        let bridge = SpanAwareLogBridge::new(&provider, &[]);
        let sub = tracing_subscriber::registry().with(bridge);
        let _guard = tracing::subscriber::set_default(sub);

        let span = tracing::info_span!("req");
        let _enter = span.enter();
        emit_request_id_on(&span, "req-id-abc");
        tracing::info!("after emit");

        let recs = records.lock().unwrap();
        assert!(!recs.is_empty());
        let has_req_id = recs[0].attributes.iter().any(|(k, v)| {
            k.as_str() == REQUEST_ID
                && matches!(v, AnyValue::String(s) if s.as_str() == "req-id-abc")
        });
        assert!(has_req_id, "request.id must propagate via SpanLogAttrs");
    }

    #[test]
    fn emit_request_id_current_span() {
        let provider = CapProvider::default();
        let records = provider.logger.records.clone();
        let bridge = SpanAwareLogBridge::new(&provider, &[]);
        let sub = tracing_subscriber::registry().with(bridge);
        let _guard = tracing::subscriber::set_default(sub);

        let span = tracing::info_span!("req");
        let _enter = span.enter();
        emit_request_id("current-id-xyz");
        tracing::info!("after emit");

        let recs = records.lock().unwrap();
        assert!(!recs.is_empty());
        let has_req_id = recs[0].attributes.iter().any(|(k, v)| {
            k.as_str() == REQUEST_ID
                && matches!(v, AnyValue::String(s) if s.as_str() == "current-id-xyz")
        });
        assert!(has_req_id);
    }

    #[test]
    fn span_log_attrs_accessible_from_extensions() {
        let sub = tracing_subscriber::registry();
        let _guard = tracing::subscriber::set_default(sub);

        let span = tracing::info_span!("check");
        let _enter = span.enter();
        crate::log_bridge::record_span_log_attr_on(
            &span,
            Key::new("probe"),
            AnyValue::String("ok".into()),
        );
        // verify SpanLogAttrs is populated by inspecting it via with_subscriber
        span.with_subscriber(|(id, dispatch)| {
            if let Some(reg) = dispatch.downcast_ref::<tracing_subscriber::Registry>() {
                if let Some(span_ref) = reg.span(id) {
                    let ext = span_ref.extensions();
                    let attrs = ext.get::<SpanLogAttrs>();
                    assert!(attrs.is_some(), "SpanLogAttrs must be present");
                    assert_eq!(attrs.unwrap().0.len(), 1);
                }
            }
        });
    }
}
