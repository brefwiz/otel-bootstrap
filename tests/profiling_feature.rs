#[cfg(feature = "profiling-bridge-pyroscope-rs")]
#[tokio::test]
async fn profiling_bridge_enabled_when_sub_feature_active() {
    let result = otel_bootstrap::Telemetry::builder("test-service")
        .with_profiling("http://localhost:4040")
        .init();

    assert!(
        result.is_ok(),
        "init should succeed — pyroscope agent start() is lazy and does not eagerly connect"
    );

    if let Ok(handles) = result {
        // Test that spans can be created within the tracing context
        // and the profiling tag layer can exercise its on_enter/on_exit paths
        let span = tracing::info_span!("profiling_test");
        let _guard = span.enter();
        // If we reach here without panicking, ProfilingTagLayer::on_enter/on_exit
        // executed cleanly with the bridge active.

        // Shutdown gracefully
        let _ = handles.shutdown();
    }
}

#[cfg(all(feature = "profiling", not(feature = "profiling-bridge-pyroscope-rs")))]
#[tokio::test]
async fn profiling_enabled_but_bridge_not_active() {
    let result = otel_bootstrap::Telemetry::builder("test-service").init();

    assert!(result.is_ok(), "init should succeed when profiling enabled");

    if let Ok(handles) = result {
        // profiling_handle should be None when bridge is not active
        assert!(
            handles.profiling_handle.is_none(),
            "profiling_handle should be None when bridge feature not enabled"
        );
        let _ = handles.shutdown();
    }
}

#[cfg(feature = "profiling-bridge-pyroscope-rs")]
#[tokio::test]
async fn profiling_bridge_initializes_without_server() {
    // This verifies the lazy start() semantics: pyroscope agent is spawned
    // as a background thread, not eagerly connected
    let result = otel_bootstrap::Telemetry::builder("test-service")
        .with_profiling("http://localhost:19999")
        .init();

    assert!(
        result.is_ok(),
        "init should succeed even with unreachable endpoint"
    );

    if let Ok(handles) = result {
        assert!(
            handles.profiling_handle.is_some(),
            "profiling_handle should be Some when bridge is active"
        );
        let _ = handles.shutdown();
    }
}

#[cfg(feature = "profiling-bridge-pyroscope-rs")]
#[tokio::test]
async fn profiling_bridge_rejects_non_loopback_endpoint() {
    // ADR platform/0203 AC1: endpoint must target loopback only
    let result = otel_bootstrap::Telemetry::builder("test-service")
        .with_profiling("http://10.0.1.5:4040")
        .init();

    assert!(
        result.is_err(),
        "init should fail when endpoint targets non-loopback address"
    );

    if let Err(e) = result {
        let msg = e.to_string();
        assert!(
            msg.contains("loopback") || msg.contains("127.0.0.1"),
            "error should mention loopback requirement: {msg}"
        );
    }
}

#[cfg(feature = "profiling-bridge-pyroscope-rs")]
#[tokio::test]
async fn profiling_bridge_accepts_ipv6_loopback() {
    // IPv6 loopback ::1 is valid
    let result = otel_bootstrap::Telemetry::builder("test-service")
        .with_profiling("http://[::1]:4040")
        .init();

    assert!(
        result.is_ok(),
        "init should succeed with IPv6 loopback endpoint"
    );

    if let Ok(handles) = result {
        let _ = handles.shutdown();
    }
}

#[cfg(feature = "profiling-bridge-pyroscope-rs")]
#[tokio::test]
async fn profiling_bridge_accepts_unix_socket() {
    // Unix socket paths are valid
    let result = otel_bootstrap::Telemetry::builder("test-service")
        .with_profiling("unix:///tmp/pyroscope.sock")
        .init();

    assert!(
        result.is_ok(),
        "init should succeed with unix socket endpoint"
    );

    if let Ok(handles) = result {
        let _ = handles.shutdown();
    }
}

#[cfg(not(feature = "profiling"))]
#[tokio::test]
async fn profiling_feature_disabled() {
    // When profiling feature is not enabled, the builder and init should work
    // without profiling fields
    let result = otel_bootstrap::Telemetry::builder("test-service").init();

    assert!(
        result.is_ok(),
        "init should succeed when profiling disabled"
    );
}
