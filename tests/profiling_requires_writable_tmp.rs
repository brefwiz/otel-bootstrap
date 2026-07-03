//! Regression test for the brefwiz-spiffe 0.38.0->0.39.0 production incident:
//! the pprof backend behind `with_profiling()` needs a writable temp
//! directory (`NamedTempFile::new()` under `std::env::temp_dir()`) to start
//! at all. A `FROM scratch` container image has no `/tmp` — and every prior
//! version of the sidecar (0.32.0 through 0.38.0) crashed the main container
//! before boot ever reached this code path, so the missing mount went
//! undetected until the sidecar's own startup race was finally fixed.
//!
//! This lives in its own file (a separate test binary/process) rather than
//! alongside `tests/profiling_feature.rs` — `start_pyroscope_bridge`'s
//! `PROFILING_STARTED` guard is a process-wide `OnceLock` allowing only one
//! real profiler per process, and setting `TMPDIR` here must not leak into
//! (or race with) that file's own profiler-success assertions.

#![cfg(feature = "profiling-bridge-pyroscope-rs")]

#[tokio::test]
async fn profiler_creation_fails_without_a_writable_temp_dir() {
    // SAFETY: this is the only test in this process, so no other thread
    // reads TMPDIR concurrently.
    unsafe {
        std::env::set_var("TMPDIR", "/nonexistent-directory-simulating-scratch-image");
    }

    let result = otel_bootstrap::Telemetry::builder("test-svc-no-tmp")
        .with_profiling("http://127.0.0.1:4040")
        .init();

    let Err(err) = result else {
        panic!(
            "expected profiler creation to fail without a writable temp dir \
             (this is the exact 'create profiler error' that crash-looped \
             brefwiz-spiffe-server's main container in production), got Ok"
        );
    };
    let msg = err.to_string();
    assert!(
        msg.contains("profiler"),
        "expected the profiler-creation error to surface, got: {msg}"
    );
}
