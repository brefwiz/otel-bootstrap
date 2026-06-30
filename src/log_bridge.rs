/// Span-aware OTLP log bridge.
///
/// Replaces `opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge` with a layer
/// that also propagates selected span fields (e.g. `"request.id"`) into every emitted log
/// record. Trace/span context is attached automatically by the SDK logger via
/// `opentelemetry::Context::current()` at emit time.
///
/// Background: the upstream bridge only visits log-event fields. Span fields set via
/// `info_span!("request", "request.id" = ...)` are invisible to it. This bridge captures
/// those fields in `on_new_span` and replays them onto each log record that fires within
/// the span's scope.
use opentelemetry::{
    Key,
    logs::{AnyValue, LogRecord, Logger, LoggerProvider, Severity},
};
use std::collections::HashMap;
use tracing::Subscriber;
use tracing_subscriber::{
    Layer, Registry,
    layer::Context,
    registry::{LookupSpan, SpanRef},
};

/// Span fields captured at span-creation time and stored in span extensions.
#[derive(Default)]
struct TrackedSpanFields(HashMap<&'static str, String>);

/// Tracing field visitor that captures a fixed set of field names.
struct FieldCollector<'a> {
    fields: &'a mut HashMap<&'static str, String>,
    keys: &'static [&'static str],
}

impl tracing::field::Visit for FieldCollector<'_> {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if self.keys.contains(&field.name()) {
            self.fields.insert(field.name(), value.to_owned());
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if self.keys.contains(&field.name()) {
            self.fields.insert(
                field.name(),
                format!("{value:?}").trim_matches('"').to_owned(),
            );
        }
    }
}

/// Tracing field visitor that sets event fields on an OTLP log record.
struct LogRecordVisitor<'a, LR: LogRecord>(&'a mut LR);

impl<LR: LogRecord> tracing::field::Visit for LogRecordVisitor<'_, LR> {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.0.set_body(AnyValue::String(value.to_owned().into()));
        } else {
            self.0.add_attribute(
                Key::new(field.name()),
                AnyValue::String(value.to_owned().into()),
            );
        }
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.0
            .add_attribute(Key::new(field.name()), AnyValue::Boolean(value));
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.0
            .add_attribute(Key::new(field.name()), AnyValue::Int(value));
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.0
            .add_attribute(Key::new(field.name()), AnyValue::Double(value));
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0
                .set_body(AnyValue::String(format!("{value:?}").into()));
        } else {
            self.0.add_attribute(
                Key::new(field.name()),
                AnyValue::String(format!("{value:?}").into()),
            );
        }
    }
}

fn severity_of_level(level: &tracing::Level) -> Severity {
    match *level {
        tracing::Level::TRACE => Severity::Trace,
        tracing::Level::DEBUG => Severity::Debug,
        tracing::Level::INFO => Severity::Info,
        tracing::Level::WARN => Severity::Warn,
        tracing::Level::ERROR => Severity::Error,
    }
}

/// Span-level log attributes written via [`record_span_log_attr_on`].
///
/// Stored in span extensions alongside `TrackedSpanFields`. Populated by
/// `span_enrichment::emit_enduser_fields` and `emit_request_id` — fields that
/// arrive via `OpenTelemetrySpanExt::set_attribute` (post-creation, not tracing
/// fields) and therefore cannot be captured by `FieldCollector` in `on_new_span`.
///
/// Uses the same `with_subscriber` + `Registry` downcast pattern as
/// `tracing_opentelemetry::OtelData` — safe to call from any non-Layer context.
#[derive(Default)]
pub struct SpanLogAttrs(pub(crate) Vec<(Key, AnyValue)>);

/// Write a key-value pair into the current span's [`SpanLogAttrs`] extension.
///
/// Callable from anywhere (middleware, enrichers) — not limited to Layer
/// `on_*` hooks. No-ops when no span is active or the subscriber is not
/// `tracing_subscriber::Registry`-backed.
pub fn record_span_log_attr(key: Key, value: AnyValue) {
    record_span_log_attr_on(&tracing::Span::current(), key, value);
}

/// Write a key-value pair into a specific span's [`SpanLogAttrs`] extension.
///
/// Prefer [`record_span_log_attr`] for the current span; use this variant
/// in `make_span_with` closures that thread an explicit span reference.
pub fn record_span_log_attr_on(span: &tracing::Span, key: Key, value: AnyValue) {
    span.with_subscriber(|(id, dispatch)| {
        if let Some(registry) = dispatch.downcast_ref::<Registry>() {
            if let Some(span_ref) = registry.span(id) {
                let mut ext = span_ref.extensions_mut();
                if ext.get_mut::<SpanLogAttrs>().is_none() {
                    ext.insert(SpanLogAttrs::default());
                }
                let attrs = ext.get_mut::<SpanLogAttrs>().unwrap();
                if let Some(existing) = attrs.0.iter_mut().find(|(k, _)| k == &key) {
                    existing.1 = value;
                } else {
                    attrs.0.push((key, value));
                }
            }
        }
    });
}

/// Span fields propagated automatically from ancestor spans into every log record.
///
/// These must be tracing span fields (set at `info_span!("name", key = val)` creation
/// time) — not attributes added later via `OpenTelemetrySpanExt::set_attribute`.
/// The bridge captures them in `on_new_span` via `FieldCollector`.
pub const PROPAGATED_SPAN_FIELDS: &[&str] = &[
    "request.id",
    "enduser.id",
    "enduser.org_id",
    "enduser.org_path",
    "enduser.principal_kind",
    "http.request.method",
    "http.response.status_code",
    "http.route",
];

/// OTLP log bridge that propagates span-level fields and trace context into log records.
pub struct SpanAwareLogBridge<P: LoggerProvider> {
    logger: P::Logger,
    span_fields: &'static [&'static str],
}

impl<P: LoggerProvider + Send + Sync> SpanAwareLogBridge<P> {
    /// Construct the bridge.
    ///
    /// `span_fields` is the set of tracing span field names whose values are
    /// captured in `on_new_span` and replayed onto every log record emitted
    /// within the span. Pass [`PROPAGATED_SPAN_FIELDS`] for the platform
    /// default set; callers may extend it via
    /// [`TelemetryBuilder::with_propagated_span_fields`].
    pub fn new(provider: &P, span_fields: &'static [&'static str]) -> Self {
        Self {
            logger: provider.logger("otel-bootstrap"),
            span_fields,
        }
    }
}

impl<S, P> Layer<S> for SpanAwareLogBridge<P>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    P: LoggerProvider + Send + Sync + 'static,
    P::Logger: Logger + Send + Sync,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        let mut tracked = TrackedSpanFields::default();
        attrs.record(&mut FieldCollector {
            fields: &mut tracked.0,
            keys: self.span_fields,
        });
        if !tracked.0.is_empty() {
            if let Some(span) = ctx.span(id) {
                span.extensions_mut().insert(tracked);
            }
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        let meta = event.metadata();
        let mut log_record = self.logger.create_log_record();

        log_record.set_severity_number(severity_of_level(meta.level()));
        log_record.set_severity_text(meta.level().as_str());
        log_record.set_target(meta.target());
        log_record.set_event_name(meta.name());

        event.record(&mut LogRecordVisitor(&mut log_record));

        if let Some(span) = ctx.event_span(event) {
            inject_span_context(&span, &mut log_record);
        }

        self.logger.emit(log_record);
    }
}

fn inject_span_context<S, LR>(span: &SpanRef<'_, S>, log_record: &mut LR)
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    LR: LogRecord,
{
    for ancestor in span.scope() {
        // Path 1: tracing-native fields captured at span creation (on_new_span).
        if let Some(tracked) = ancestor.extensions().get::<TrackedSpanFields>() {
            for (k, v) in &tracked.0 {
                log_record.add_attribute(Key::new(*k), AnyValue::String(v.clone().into()));
            }
        }

        // Path 2: dynamic attributes written via record_span_log_attr_on
        // (e.g. request.id / enduser.* set after span creation).
        if let Some(log_attrs) = ancestor.extensions().get::<SpanLogAttrs>() {
            for (k, v) in &log_attrs.0 {
                log_record.add_attribute(k.clone(), v.clone());
            }
        }
    }
    // Trace context (trace_id / span_id) is attached automatically by the SDK
    // logger via opentelemetry::Context::current() at emit time.
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::{InstrumentationScope, logs::Logger, logs::LoggerProvider};
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::layer::SubscriberExt;

    #[derive(Default, Clone)]
    struct CapturedRecord {
        body: Option<AnyValue>,
        attributes: Vec<(Key, AnyValue)>,
        severity: Option<Severity>,
    }

    #[derive(Default, Clone)]
    struct CapturingLogRecord(Arc<Mutex<CapturedRecord>>);

    impl opentelemetry::logs::LogRecord for CapturingLogRecord {
        fn set_event_name(&mut self, _name: &'static str) {}
        fn set_target<T: Into<std::borrow::Cow<'static, str>>>(&mut self, _target: T) {}
        fn set_timestamp(&mut self, _ts: std::time::SystemTime) {}
        fn set_observed_timestamp(&mut self, _ts: std::time::SystemTime) {}
        fn set_severity_text(&mut self, _text: &'static str) {}
        fn set_severity_number(&mut self, sev: opentelemetry::logs::Severity) {
            self.0.lock().unwrap().severity = Some(sev);
        }
        fn set_body(&mut self, body: AnyValue) {
            self.0.lock().unwrap().body = Some(body);
        }
        fn add_attributes<I, K, V>(&mut self, attributes: I)
        where
            I: IntoIterator<Item = (K, V)>,
            K: Into<Key>,
            V: Into<AnyValue>,
        {
            let mut guard = self.0.lock().unwrap();
            for (k, v) in attributes {
                guard.attributes.push((k.into(), v.into()));
            }
        }
        fn add_attribute<K, V>(&mut self, key: K, value: V)
        where
            K: Into<Key>,
            V: Into<AnyValue>,
        {
            self.0
                .lock()
                .unwrap()
                .attributes
                .push((key.into(), value.into()));
        }
    }

    #[derive(Clone, Default)]
    struct CapturingLogger {
        records: Arc<Mutex<Vec<CapturedRecord>>>,
    }

    impl Logger for CapturingLogger {
        type LogRecord = CapturingLogRecord;

        fn create_log_record(&self) -> Self::LogRecord {
            CapturingLogRecord(Arc::new(Mutex::new(CapturedRecord::default())))
        }

        fn emit(&self, record: Self::LogRecord) {
            let captured = record.0.lock().unwrap().clone();
            self.records.lock().unwrap().push(captured);
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
    struct CapturingLoggerProvider {
        logger: CapturingLogger,
    }

    impl LoggerProvider for CapturingLoggerProvider {
        type Logger = CapturingLogger;

        fn logger_with_scope(&self, _scope: InstrumentationScope) -> Self::Logger {
            self.logger.clone()
        }
    }

    fn make_subscriber(
        fields: &'static [&'static str],
    ) -> (impl tracing::Subscriber, Arc<Mutex<Vec<CapturedRecord>>>) {
        let provider = CapturingLoggerProvider::default();
        let records = provider.logger.records.clone();
        let bridge = SpanAwareLogBridge::new(&provider, fields);
        (tracing_subscriber::registry().with(bridge), records)
    }

    fn attr_str<'a>(record: &'a CapturedRecord, key: &str) -> Option<&'a str> {
        record.attributes.iter().find_map(|(k, v)| {
            if k.as_str() == key {
                if let AnyValue::String(s) = v {
                    Some(s.as_str())
                } else {
                    None
                }
            } else {
                None
            }
        })
    }

    #[test]
    fn request_id_tracing_field_propagates_to_log_record() {
        let (sub, records) = make_subscriber(&["request.id"]);
        let _guard = tracing::subscriber::set_default(sub);

        let span = tracing::info_span!("request", "request.id" = "test-uuid-1234");
        let _enter = span.enter();
        tracing::info!("hello from inside the span");

        let recs = records.lock().unwrap();
        assert!(!recs.is_empty());
        assert_eq!(attr_str(&recs[0], "request.id"), Some("test-uuid-1234"));
    }

    #[test]
    fn span_log_attrs_propagate_to_log_record() {
        let (sub, records) = make_subscriber(&[]);
        let _guard = tracing::subscriber::set_default(sub);

        let span = tracing::info_span!("req");
        let _enter = span.enter();
        record_span_log_attr_on(&span, Key::new("x-custom"), AnyValue::String("val".into()));
        tracing::info!("inside");

        let recs = records.lock().unwrap();
        assert!(!recs.is_empty());
        assert_eq!(attr_str(&recs[0], "x-custom"), Some("val"));
    }

    #[test]
    fn span_log_attrs_update_existing_key() {
        let (sub, records) = make_subscriber(&[]);
        let _guard = tracing::subscriber::set_default(sub);

        let span = tracing::info_span!("req");
        let _enter = span.enter();
        record_span_log_attr_on(&span, Key::new("k"), AnyValue::String("first".into()));
        record_span_log_attr_on(&span, Key::new("k"), AnyValue::String("second".into()));
        tracing::info!("inside");

        let recs = records.lock().unwrap();
        assert!(!recs.is_empty());
        let vals: Vec<_> = recs[0]
            .attributes
            .iter()
            .filter(|(k, _)| k.as_str() == "k")
            .collect();
        assert_eq!(vals.len(), 1, "duplicate key must be deduplicated");
        assert!(matches!(&vals[0].1, AnyValue::String(s) if s.as_str() == "second"));
    }

    #[test]
    fn record_span_log_attr_current_span_no_op_outside_span() {
        // Must not panic when called without an active span.
        record_span_log_attr(Key::new("k"), AnyValue::String("v".into()));
    }

    #[test]
    fn on_new_span_non_matching_fields_no_extension_inserted() {
        let (sub, records) = make_subscriber(&["request.id"]);
        let _guard = tracing::subscriber::set_default(sub);

        // Span has no fields that match our keys list.
        let span = tracing::info_span!("plain");
        let _enter = span.enter();
        tracing::info!("msg");

        let recs = records.lock().unwrap();
        assert!(!recs.is_empty());
        assert!(attr_str(&recs[0], "request.id").is_none());
    }

    #[test]
    fn log_outside_span_no_panic() {
        let (sub, records) = make_subscriber(&["request.id"]);
        let _guard = tracing::subscriber::set_default(sub);
        tracing::info!("no span active");
        assert!(!records.lock().unwrap().is_empty());
    }

    #[test]
    fn nested_span_outer_field_appears_in_inner_log() {
        let (sub, records) = make_subscriber(&["request.id"]);
        let _guard = tracing::subscriber::set_default(sub);

        let outer = tracing::info_span!("outer", "request.id" = "outer-id");
        let _e1 = outer.enter();
        let inner = tracing::info_span!("inner");
        let _e2 = inner.enter();
        tracing::info!("deep log");

        let recs = records.lock().unwrap();
        assert!(!recs.is_empty());
        assert_eq!(attr_str(&recs[0], "request.id"), Some("outer-id"));
    }

    #[test]
    fn log_record_visitor_bool_field() {
        let (sub, records) = make_subscriber(&[]);
        let _guard = tracing::subscriber::set_default(sub);
        tracing::info!(flag = true, "msg");
        let recs = records.lock().unwrap();
        assert!(!recs.is_empty());
        let has_flag = recs[0]
            .attributes
            .iter()
            .any(|(k, v)| k.as_str() == "flag" && matches!(v, AnyValue::Boolean(true)));
        assert!(has_flag);
    }

    #[test]
    fn log_record_visitor_i64_field() {
        let (sub, records) = make_subscriber(&[]);
        let _guard = tracing::subscriber::set_default(sub);
        tracing::info!(count = 42i64, "msg");
        let recs = records.lock().unwrap();
        assert!(!recs.is_empty());
        let has_count = recs[0]
            .attributes
            .iter()
            .any(|(k, v)| k.as_str() == "count" && matches!(v, AnyValue::Int(42)));
        assert!(has_count);
    }

    #[test]
    fn log_record_visitor_f64_field() {
        let (sub, records) = make_subscriber(&[]);
        let _guard = tracing::subscriber::set_default(sub);
        tracing::info!(ratio = 0.5f64, "msg");
        let recs = records.lock().unwrap();
        assert!(!recs.is_empty());
        let has_ratio = recs[0].attributes.iter().any(|(k, v)| {
            k.as_str() == "ratio" && matches!(v, AnyValue::Double(f) if (f - 0.5).abs() < 1e-9)
        });
        assert!(has_ratio);
    }

    #[test]
    fn log_record_visitor_message_becomes_body() {
        let (sub, records) = make_subscriber(&[]);
        let _guard = tracing::subscriber::set_default(sub);
        tracing::info!("the body text");
        let recs = records.lock().unwrap();
        assert!(!recs.is_empty());
        assert!(
            matches!(&recs[0].body, Some(AnyValue::String(s)) if s.as_str() == "the body text")
        );
    }

    #[test]
    fn severity_warn() {
        let (sub, records) = make_subscriber(&[]);
        let _guard = tracing::subscriber::set_default(sub);
        tracing::warn!("warn msg");
        let recs = records.lock().unwrap();
        assert!(!recs.is_empty());
        assert_eq!(recs[0].severity, Some(Severity::Warn));
    }

    #[test]
    fn severity_error() {
        let (sub, records) = make_subscriber(&[]);
        let _guard = tracing::subscriber::set_default(sub);
        tracing::error!("err msg");
        let recs = records.lock().unwrap();
        assert!(!recs.is_empty());
        assert_eq!(recs[0].severity, Some(Severity::Error));
    }

    #[test]
    fn severity_debug() {
        let (sub, records) = make_subscriber(&[]);
        let _guard = tracing::subscriber::set_default(sub);
        tracing::debug!("dbg msg");
        let recs = records.lock().unwrap();
        assert!(!recs.is_empty());
        assert_eq!(recs[0].severity, Some(Severity::Debug));
    }

    #[test]
    fn severity_trace() {
        let (sub, records) = make_subscriber(&[]);
        let _guard = tracing::subscriber::set_default(sub);
        tracing::trace!("trc msg");
        let recs = records.lock().unwrap();
        assert!(!recs.is_empty());
        assert_eq!(recs[0].severity, Some(Severity::Trace));
    }
}
