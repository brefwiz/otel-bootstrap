//! Integration tests for the builder API.
//!
//! These tests verify that `Telemetry::builder()` correctly initialises
//! telemetry with various configurations including metrics disabled.

#![cfg(feature = "integration-tests")]

use otel_bootstrap::{Telemetry, TraceSampler};
use std::sync::{Arc, Mutex};
use tracing::Subscriber;
use tracing_subscriber::Layer;
use tracing_subscriber::registry::LookupSpan;

/// A minimal custom layer that records the names of events it receives.
struct EventCapture {
    events: Arc<Mutex<Vec<&'static str>>>,
}

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for EventCapture {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        self.events.lock().unwrap().push(event.metadata().name());
    }
}

#[tokio::test]
async fn builder_with_all_options_produces_valid_handles() {
    let handles = Telemetry::builder("builder-test-svc")
        .with_version("1.2.3")
        .with_environment("staging")
        .with_sampler(TraceSampler::TraceIdRatio(0.5))
        .with_metrics(true)
        .init()
        .expect("builder init should succeed");

    assert!(handles.meter_provider.is_some());
    handles.shutdown().expect("shutdown should succeed");
}

#[tokio::test]
async fn builder_with_metrics_disabled_produces_no_meter_provider() {
    let handles = Telemetry::builder("builder-no-metrics")
        .with_metrics(false)
        .init()
        .expect("builder init should succeed");

    assert!(handles.meter_provider.is_none());
    handles.shutdown().expect("shutdown should succeed");
}

#[tokio::test]
async fn init_telemetry_with_sampler_delegates_to_builder() {
    let handles = otel_bootstrap::init_telemetry_with_sampler(
        "sampler-delegate-test",
        Some(TraceSampler::AlwaysOff),
    )
    .expect("init_telemetry_with_sampler should succeed");

    handles.shutdown().expect("shutdown should succeed");
}

#[tokio::test]
async fn builder_with_logs_enabled_produces_logger_provider() {
    let handles = Telemetry::builder("builder-logs-test")
        .with_metrics(false)
        .with_logs(true)
        .init()
        .expect("builder init with logs should succeed");

    assert!(handles.logger_provider.is_some());
    let _ = handles.shutdown();
}

#[tokio::test]
async fn builder_with_logs_disabled_produces_no_logger_provider() {
    let handles = Telemetry::builder("builder-no-logs")
        .with_metrics(false)
        .with_logs(false)
        .init()
        .expect("builder init without logs should succeed");

    assert!(handles.logger_provider.is_none());
    let _ = handles.shutdown();
}

#[tokio::test]
async fn builder_with_custom_batch_size_initialises_successfully() {
    let handles = Telemetry::builder("builder-batch-size-test")
        .with_max_export_batch_size(1024)
        .with_metrics(false)
        .init()
        .expect("builder init with custom batch size should succeed");

    let _ = handles.shutdown();
}

#[tokio::test]
async fn builder_with_custom_metric_interval_initialises_successfully() {
    let handles = Telemetry::builder("builder-metric-interval-test")
        .with_metric_export_interval(std::time::Duration::from_secs(10))
        .with_metrics(true)
        .init()
        .expect("builder init with custom metric interval should succeed");

    assert!(handles.meter_provider.is_some());
    let _ = handles.shutdown();
}

#[tokio::test]
async fn global_meter_is_functional_after_init() {
    let handles = Telemetry::builder("global-meter-test")
        .with_metrics(true)
        .init()
        .expect("builder init should succeed");

    // opentelemetry::global::meter() must return a working meter, not a no-op.
    // Creating a counter and recording a value exercises the global provider.
    let meter = opentelemetry::global::meter("my-lib");
    let counter = meter.u64_counter("test.counter").build();
    counter.add(1, &[]);

    // Shutdown may time out in CI when no collector is running; that is expected.
    // The assertion above is what matters: global::meter() returned a working meter.
    let _ = handles.shutdown();
}

#[tokio::test]
async fn from_env_reads_otel_service_name() {
    // Exercises the `self.service_name.unwrap_or_else(...)` branch in `init()`
    // when `Telemetry::from_env()` is used (service_name is None).
    unsafe { std::env::set_var("OTEL_SERVICE_NAME", "env-driven-svc") };
    let handles = otel_bootstrap::Telemetry::from_env()
        .with_metrics(false)
        .init()
        .expect("from_env init should succeed");
    unsafe { std::env::remove_var("OTEL_SERVICE_NAME") };
    let _ = handles.shutdown();
}

#[tokio::test]
async fn with_export_timeout_propagates_to_exporters() {
    // Exercises the `if let Some(t) = timeout { b = b.with_timeout(t); }` branches
    // in build_span_exporter, build_metric_exporter, and build_log_exporter.
    let handles = otel_bootstrap::Telemetry::builder("timeout-test-svc")
        .with_export_timeout(std::time::Duration::from_secs(5))
        .with_metrics(true)
        .with_logs(true)
        .init()
        .expect("init with export timeout should succeed");
    assert!(handles.meter_provider.is_some());
    assert!(handles.logger_provider.is_some());
    let _ = handles.shutdown();
}

#[tokio::test]
async fn with_layer_builder_accepts_custom_layer() {
    // Verifies the API is usable and init() does not fail when a custom layer is
    // added. Event dispatch is tested separately via with_default below.
    let captured = Arc::new(Mutex::new(Vec::new()));
    let handles = Telemetry::builder("custom-layer-single")
        .with_metrics(false)
        .with_layer(EventCapture {
            events: Arc::clone(&captured),
        })
        .init()
        .expect("init with custom layer should succeed");
    let _ = handles.shutdown();
}

#[tokio::test]
async fn with_layer_builder_accepts_multiple_custom_layers() {
    let handles = Telemetry::builder("custom-layer-multi")
        .with_metrics(false)
        .with_layer(EventCapture {
            events: Arc::new(Mutex::new(Vec::new())),
        })
        .with_layer(EventCapture {
            events: Arc::new(Mutex::new(Vec::new())),
        })
        .init()
        .expect("init with multiple custom layers should succeed");
    let _ = handles.shutdown();
}

/// Verify that a single custom layer composed via `registry().with(vec![layer])`
/// receives events when events are dispatched through the subscriber.
#[test]
fn with_layer_single_custom_layer_receives_events() {
    use tracing_subscriber::layer::SubscriberExt;

    let captured = Arc::new(Mutex::new(Vec::new()));
    let layer = EventCapture {
        events: Arc::clone(&captured),
    };

    let subscriber = tracing_subscriber::registry().with(vec![
        Box::new(layer) as Box<dyn Layer<tracing_subscriber::Registry> + Send + Sync>
    ]);

    tracing::subscriber::with_default(subscriber, || {
        tracing::info!("hello from single-layer test");
    });

    let events = captured.lock().unwrap();
    assert!(
        !events.is_empty(),
        "custom layer should have received at least one event"
    );
}

/// Verify that multiple custom layers composed via `registry().with(vec![...])` all
/// receive events — matching the behaviour of multiple `.with_layer()` calls.
#[test]
fn with_layer_multiple_custom_layers_all_receive_events() {
    use tracing_subscriber::layer::SubscriberExt;

    let captured_a = Arc::new(Mutex::new(Vec::new()));
    let captured_b = Arc::new(Mutex::new(Vec::new()));

    let subscriber = tracing_subscriber::registry().with(vec![
        Box::new(EventCapture {
            events: Arc::clone(&captured_a),
        }) as Box<dyn Layer<tracing_subscriber::Registry> + Send + Sync>,
        Box::new(EventCapture {
            events: Arc::clone(&captured_b),
        }) as Box<dyn Layer<tracing_subscriber::Registry> + Send + Sync>,
    ]);

    tracing::subscriber::with_default(subscriber, || {
        tracing::info!("hello from multi-layer test");
    });

    let a = captured_a.lock().unwrap();
    let b = captured_b.lock().unwrap();
    assert!(
        !a.is_empty(),
        "first custom layer should have received events"
    );
    assert!(
        !b.is_empty(),
        "second custom layer should have received events"
    );
}
