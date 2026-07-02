//! Continuous profiling example.
//!
//! Demonstrates enabling the `profiling` feature via the builder. Profiles
//! are pushed to a local SPIFFE-terminating sidecar (never directly to a
//! remote Pyroscope backend) — see ADR platform/0202 and platform/0203.
//!
//! Run against the shipped compose stack with:
//! ```text
//! docker compose up -d
//! cargo run --example telemetry_profiling --features profiling-bridge-pyroscope-rs
//! ```
//! The endpoint must be loopback-only (enforced at init time).

#[cfg(feature = "profiling")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use tracing::info_span;

    // Traces + metrics + continuous profiling, all over the single OTLP
    // export plane. The profiling bridge pushes to a localhost
    // SPIFFE-terminating sidecar, never straight to a remote backend.
    let handles = otel_bootstrap::Telemetry::builder("telemetry-profiling-example")
        .with_profiling("http://localhost:4040")
        .init()?;

    let _span = info_span!("example.work", step = 1).entered();
    tracing::info!("telemetry_profiling example running");
    drop(_span);

    handles.shutdown()?;
    Ok(())
}

#[cfg(not(feature = "profiling"))]
fn main() {
    eprintln!(
        "telemetry_profiling example requires the `profiling` feature: \
         cargo run --example telemetry_profiling --features profiling-bridge-pyroscope-rs"
    );
}
