//! Custom configuration example.
//!
//! Shows how to use [`Telemetry::builder`] to configure the service name,
//! version, deployment environment, trace sampler, metrics, logs, and the
//! shutdown timeout.
//!
//! Run with:
//! ```text
//! OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 cargo run --example custom_config
//! ```

use std::error::Error;
use std::time::Duration;

fn main() -> Result<(), Box<dyn Error>> {
    let handles = otel_bootstrap::Telemetry::builder("custom-config-example")
        // Embed the release version in every span/metric.
        .with_version(env!("CARGO_PKG_VERSION"))
        // Tag resources with the deployment environment.
        .with_environment("development")
        // Sample every trace (the default; shown here for documentation).
        .with_sampler(otel_bootstrap::TraceSampler::AlwaysOn)
        // Enable the metrics pipeline (on by default, shown explicitly).
        .with_metrics(true)
        // Enable the log-bridge pipeline.
        .with_logs(true)
        // Allow up to 10 s for the exporters to drain on shutdown.
        .with_shutdown_timeout(Duration::from_secs(10))
        .init()?;

    tracing::info!(target: "example", "custom_config example running");

    {
        use tracing::info_span;
        let _span = info_span!("example.custom", config = "full").entered();
        tracing::info!("inside custom-configured span");
    }

    handles.shutdown()?;
    Ok(())
}
