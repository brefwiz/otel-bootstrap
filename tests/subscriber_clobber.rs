//! Exercises the subscriber-already-installed warning paths in `Telemetry::init`.
//!
//! Each test runs in its own process (nextest), so global tracing state is clean.
//! We deliberately pre-install a subscriber before calling `init()` to trigger
//! the `try_init()` error branches that emit the diagnostic warning.

#![cfg(feature = "integration-tests")]

use otel_bootstrap::Telemetry;

/// Branch: no logger_provider, subscriber already installed.
/// `registry.try_init()` returns `Err` → warning printed, `init` still succeeds.
#[tokio::test]
async fn warns_when_no_logs_and_subscriber_already_installed() {
    // Endpoint points at a dead port — exporter is lazy, builder succeeds.
    unsafe {
        std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:1");
    }
    // Pre-occupy the global subscriber slot.
    let _ = tracing_subscriber::fmt().try_init();

    let handles = Telemetry::builder("clobber-test-no-logs").init();
    assert!(
        handles.is_ok(),
        "init must not fail on clobber: {:?}",
        handles.err()
    );
}

/// Branch: logger_provider present, subscriber already installed.
/// `registry.with(OtelBridge).try_init()` returns `Err` → warning printed, `init` still succeeds.
#[tokio::test]
async fn warns_when_logs_enabled_and_subscriber_already_installed() {
    unsafe {
        std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:1");
    }
    // Pre-occupy the global subscriber slot.
    let _ = tracing_subscriber::fmt().try_init();

    let handles = Telemetry::builder("clobber-test-with-logs")
        .with_logs(true)
        .init();
    assert!(
        handles.is_ok(),
        "init must not fail on clobber: {:?}",
        handles.err()
    );
}
