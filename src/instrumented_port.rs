// SPDX-License-Identifier: MIT
//! Generic client-span wrapper for outbound hexagonal port calls.
//!
//! [`Instrumented<P>`] wraps any hexagonal outbound port (a KMS provider, a secret
//! store, any `#[async_trait]` port trait object) so every call through it emits an
//! `otel.kind = "client"` span **by construction**, not by call-site discipline.
//!
//! Adapter-specific attributes (KMS key ARN, GCP project, ...) are never hardcoded
//! here — the caller supplies a `provider_hint` string at construction time, and
//! this module records only that generic hint plus the operation name.
//!
//! This is plain delegation, not a proc-macro: implement the
//! wrapped port trait for `Instrumented<P>` by hand, and call
//! [`Instrumented::call`] inside each method to open the span around the delegated
//! call. See `examples/instrumented_port.rs` for a full worked example.
//!
//! # Example
//! ```
//! use otel_bootstrap::instrumented_port::Instrumented;
//!
//! # async fn example() {
//! struct Echo;
//! impl Echo {
//!     async fn ping(&self) -> Result<&'static str, std::convert::Infallible> {
//!         Ok("pong")
//!     }
//! }
//!
//! let wrapped = Instrumented::new(Echo, "echo-port", Some("in-memory"));
//! let reply = wrapped.call("ping", |inner| inner.ping()).await.unwrap();
//! assert_eq!(reply, "pong");
//! # }
//! ```

use opentelemetry::trace::{FutureExt, SpanKind, Status, TraceContextExt, Tracer};
use opentelemetry::{Context, KeyValue, global};
use std::future::Future;
use std::sync::Arc;

/// Attribute key for the operation name (e.g. `wrap`, `unwrap`, `get_secret`).
pub const PORT_OPERATION: &str = "port.operation";
/// Attribute key for the caller-supplied provider/adapter hint (e.g. `aws-kms`,
/// `vault-transit`). Never populated with adapter-specific secrets (ARNs, project
/// IDs) — those stay out of otel-bootstrap.
pub const PORT_PROVIDER_HINT: &str = "port.provider_hint";
/// Attribute key for the port name (the trait being wrapped, e.g. `KmsProvider`).
pub const PORT_NAME: &str = "port.name";

/// The tracer instrumentation scope name used for all client spans emitted by
/// [`Instrumented`].
const INSTRUMENTATION_SCOPE: &str = "otel-bootstrap/instrumented-port";

/// Wraps any hexagonal outbound port `P` so calls through it carry an
/// `otel.kind = "client"` span by construction.
///
/// `P` is typically an adapter value or a trait object handle (`Arc<dyn Port>`).
/// Construct via [`Instrumented::new`], then implement the wrapped port trait for
/// `Instrumented<P>` via plain delegation, calling [`Instrumented::call`] inside
/// each method.
#[derive(Debug, Clone)]
pub struct Instrumented<P> {
    inner: P,
    port_name: &'static str,
    provider_hint: Option<Arc<str>>,
}

impl<P> Instrumented<P> {
    /// Wrap `inner` for calls through port `port_name` (e.g. `"KmsProvider"`).
    ///
    /// `provider_hint` is an optional, caller-supplied label identifying the
    /// concrete adapter (e.g. `"aws-kms"`, `"vault-transit"`) — generic enough to
    /// be safe to export, never an adapter-specific secret.
    pub fn new(inner: P, port_name: &'static str, provider_hint: Option<&str>) -> Self {
        Self {
            inner,
            port_name,
            provider_hint: provider_hint.map(Arc::from),
        }
    }

    /// Borrow the wrapped value — use inside a delegating trait impl to reach the
    /// real adapter once the span is open, or when no span is needed for a given
    /// call.
    pub fn inner(&self) -> &P {
        &self.inner
    }

    /// Run `op` (a closure producing the delegated future) inside a client span
    /// named `"{port_name}.{operation}"`, tagged `otel.kind = client` plus
    /// [`PORT_NAME`], [`PORT_OPERATION`], and — when present — [`PORT_PROVIDER_HINT`].
    ///
    /// On `Err`, the span status is set to [`Status::error`] with a fixed,
    /// non-adapter-controlled description (the wrapped error's `Display` output is
    /// never forwarded into telemetry, since adapter error types may embed
    /// sensitive detail such as key identifiers or provider diagnostics); the
    /// error itself is still returned to the caller unchanged.
    pub async fn call<'a, F, Fut, T, E>(&'a self, operation: &str, op: F) -> Result<T, E>
    where
        F: FnOnce(&'a P) -> Fut,
        Fut: Future<Output = Result<T, E>>,
        E: std::fmt::Display,
    {
        let tracer = global::tracer(INSTRUMENTATION_SCOPE);
        let span_name = format!("{}.{}", self.port_name, operation);

        let mut attributes = vec![
            KeyValue::new(PORT_NAME, self.port_name),
            KeyValue::new(PORT_OPERATION, operation.to_owned()),
        ];
        if let Some(hint) = &self.provider_hint {
            attributes.push(KeyValue::new(PORT_PROVIDER_HINT, hint.to_string()));
        }

        // Parent on the current context only when it carries a *valid* span.
        // `start(&tracer)` / `start_with_context(&tracer, &Context::current())`
        // copies the parent's trace id verbatim — including an all-zero
        // (invalid) one. On background / boot paths (e.g. a KMS unwrap outside
        // any request trace) the current context can hold a span with an
        // invalid SpanContext, yielding a 0-bit trace id that the collector's
        // Tempo exporter rejects ("trace ids must be 128 bit, received 0 bits"),
        // dropping the whole batch. Detaching to a fresh context when there is
        // no valid parent makes the SDK mint a new root trace id instead.
        let parent_cx = Context::current();
        let span = if parent_cx.span().span_context().is_valid() {
            tracer
                .span_builder(span_name)
                .with_kind(SpanKind::Client)
                .with_attributes(attributes)
                .start_with_context(&tracer, &parent_cx)
        } else {
            tracer
                .span_builder(span_name)
                .with_kind(SpanKind::Client)
                .with_attributes(attributes)
                .start_with_context(&tracer, &Context::new())
        };
        let cx = Context::current_with_span(span);

        let fut = op(&self.inner);
        let result = fut.with_context(cx.clone()).await;

        if result.is_err() {
            cx.span().set_status(Status::error("port call failed"));
        }

        result
    }
}

/// Convenience alias for the common shape of a port registry entry: an
/// [`Instrumented`] wrapper around a shared, trait-object handle to a port `P`.
///
/// Port registries (`KmsRegistry` and equivalents) should
/// return `InstrumentedArc<dyn Port>` rather than a bare `Arc<dyn Port>`.
pub type InstrumentedArc<P> = Instrumented<Arc<P>>;

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_sdk::error::OTelSdkResult;
    use opentelemetry_sdk::trace::{SdkTracerProvider, SpanData, SpanExporter};
    use std::sync::{LazyLock, Mutex};

    #[derive(Debug, Clone, Default)]
    struct CapturingExporter {
        spans: Arc<Mutex<Vec<SpanData>>>,
    }

    impl SpanExporter for CapturingExporter {
        async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
            self.spans.lock().unwrap().extend(batch);
            Ok(())
        }
    }

    struct Adapter;
    impl Adapter {
        async fn wrap(&self) -> Result<&'static str, &'static str> {
            Ok("wrapped")
        }
        async fn unwrap(&self) -> Result<&'static str, &'static str> {
            Err("boom")
        }
    }

    fn install_capturing_tracer() -> (SdkTracerProvider, Arc<Mutex<Vec<SpanData>>>) {
        let exporter = CapturingExporter::default();
        let spans = exporter.spans.clone();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter)
            .build();
        (provider, spans)
    }

    // `opentelemetry::global::set_tracer_provider` mutates process-global state.
    // Serialize the tests that touch it so they don't race on each other's
    // provider/exporter under the default multi-threaded test runner.
    static GLOBAL_TRACER_LOCK: LazyLock<tokio::sync::Mutex<()>> =
        LazyLock::new(|| tokio::sync::Mutex::new(()));

    #[tokio::test]
    async fn call_emits_client_span_with_operation_and_provider_hint() {
        let _guard = GLOBAL_TRACER_LOCK.lock().await;
        let (provider, spans) = install_capturing_tracer();
        opentelemetry::global::set_tracer_provider(provider.clone());

        let wrapped = Instrumented::new(Adapter, "KmsProvider", Some("aws-kms"));
        let out = wrapped.call("wrap", |inner| inner.wrap()).await.unwrap();
        assert_eq!(out, "wrapped");

        provider.force_flush().unwrap();

        let captured = spans.lock().unwrap();
        assert_eq!(
            captured.len(),
            1,
            "exactly one span must be emitted per call"
        );
        let span = &captured[0];

        assert_eq!(span.span_kind, opentelemetry::trace::SpanKind::Client);
        assert_eq!(span.name, "KmsProvider.wrap");

        let has_attr = |key: &str, value: &str| {
            span.attributes
                .iter()
                .any(|kv| kv.key.as_str() == key && kv.value.as_str() == value)
        };
        assert!(has_attr(PORT_NAME, "KmsProvider"));
        assert!(has_attr(PORT_OPERATION, "wrap"));
        assert!(has_attr(PORT_PROVIDER_HINT, "aws-kms"));
    }

    #[tokio::test]
    async fn call_records_error_status_and_returns_error_unchanged() {
        let _guard = GLOBAL_TRACER_LOCK.lock().await;
        let (provider, spans) = install_capturing_tracer();
        opentelemetry::global::set_tracer_provider(provider.clone());

        let wrapped = Instrumented::new(Adapter, "KmsProvider", None);
        let out = wrapped.call("unwrap", |inner| inner.unwrap()).await;
        assert_eq!(out, Err("boom"));

        provider.force_flush().unwrap();

        let captured = spans.lock().unwrap();
        assert_eq!(captured.len(), 1);
        let span = &captured[0];
        assert_eq!(span.name, "KmsProvider.unwrap");
        assert!(matches!(
            &span.status,
            opentelemetry::trace::Status::Error { .. }
        ));
        let has_no_hint = !span
            .attributes
            .iter()
            .any(|kv| kv.key.as_str() == PORT_PROVIDER_HINT);
        assert!(has_no_hint, "no provider hint attribute when none supplied");
    }

    #[tokio::test]
    async fn call_mints_valid_trace_id_with_no_parent() {
        let _guard = GLOBAL_TRACER_LOCK.lock().await;
        let (provider, spans) = install_capturing_tracer();
        opentelemetry::global::set_tracer_provider(provider.clone());

        let wrapped = Instrumented::new(Adapter, "KmsProvider", None);
        wrapped.call("unwrap", |inner| inner.unwrap()).await.ok();
        provider.force_flush().unwrap();

        let captured = spans.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert!(
            captured[0].span_context.is_valid(),
            "root span must carry a valid (non-zero) trace id"
        );
    }

    // Regression: a KMS unwrap on a background / boot path runs with an invalid
    // span context as current (no real request trace). The span must still get a
    // fresh root trace id, not inherit the invalid parent's 0-bit id — a 0-bit
    // trace id makes the collector's Tempo exporter reject the whole batch.
    #[tokio::test]
    async fn call_mints_fresh_trace_id_when_current_parent_is_invalid() {
        use opentelemetry::trace::SpanContext;

        let _guard = GLOBAL_TRACER_LOCK.lock().await;
        let (provider, spans) = install_capturing_tracer();
        opentelemetry::global::set_tracer_provider(provider.clone());

        // Make an invalid span context current — reproduces the prod condition.
        let invalid_parent = Context::new().with_remote_span_context(SpanContext::empty_context());
        assert!(!invalid_parent.span().span_context().is_valid());
        let _attach = invalid_parent.attach();

        let wrapped = Instrumented::new(Adapter, "KmsProvider", None);
        wrapped.call("unwrap", |inner| inner.unwrap()).await.ok();
        provider.force_flush().unwrap();

        let captured = spans.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert!(
            captured[0].span_context.is_valid(),
            "span must mint a fresh root trace id instead of inheriting the invalid parent's 0-bit id"
        );
    }

    #[tokio::test]
    async fn instrumented_arc_alias_wraps_shared_port_handle() {
        let _guard = GLOBAL_TRACER_LOCK.lock().await;
        let shared: Arc<Adapter> = Arc::new(Adapter);
        let wrapped: InstrumentedArc<Adapter> = Instrumented::new(shared, "KmsProvider", None);
        let out = wrapped.call("wrap", |inner| inner.wrap()).await.unwrap();
        assert_eq!(out, "wrapped");
    }
}
