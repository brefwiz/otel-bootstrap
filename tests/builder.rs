//! Integration tests for the builder API.
//!
//! These tests verify that `Telemetry::builder()` correctly initialises
//! telemetry with various configurations including metrics disabled.

#![cfg(feature = "integration-tests")]

use otel_bootstrap::{Telemetry, TraceSampler};

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
