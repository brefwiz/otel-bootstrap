//! Tests for the no-op / testing mode.

#![cfg(feature = "testing")]

use otel_bootstrap::Telemetry;

/// Testing mode must initialise without error and shut down cleanly.
#[test]
fn testing_mode_initialises_without_error() {
    let handles = Telemetry::testing("test-noop-svc");
    handles.shutdown().expect("shutdown should succeed");
}

/// Testing mode must provide a meter provider (in-memory, no network).
#[test]
fn testing_mode_provides_meter_provider() {
    let handles = Telemetry::testing("test-noop-metrics");
    assert!(handles.meter_provider.is_some());
    handles.shutdown().expect("shutdown should succeed");
}

/// Testing mode must not wire up a logger provider.
#[test]
fn testing_mode_has_no_logger_provider() {
    let handles = Telemetry::testing("test-noop-logs");
    assert!(handles.logger_provider.is_none());
    handles.shutdown().expect("shutdown should succeed");
}

/// Spans emitted via `tracing` must not panic even when called multiple times
/// (e.g. when test harness runs tests in the same process with a shared global subscriber).
#[test]
fn testing_mode_tracing_macros_do_not_panic() {
    let handles = Telemetry::testing("test-noop-tracing");
    tracing::info!("hello from test");
    let _span = tracing::info_span!("test-span").entered();
    tracing::debug!("inside span");
    drop(_span);
    handles.shutdown().expect("shutdown should succeed");
}

/// The global meter returned after testing init must be functional (records without panic).
#[test]
fn testing_mode_global_meter_is_functional() {
    let handles = Telemetry::testing("test-global-meter");

    let meter = opentelemetry::global::meter("test-lib");
    let counter = meter.u64_counter("test.counter").build();
    counter.add(1, &[]);

    handles.shutdown().expect("shutdown should succeed");
}
