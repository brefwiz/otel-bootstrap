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
