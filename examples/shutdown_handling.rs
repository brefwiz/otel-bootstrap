//! Graceful shutdown example.
//!
//! Demonstrates how to hold [`TelemetryHandles`] for the lifetime of the
//! application and call [`TelemetryHandles::shutdown`] explicitly before exit
//! so all pending spans and metrics are flushed.
//!
//! Run with:
//! ```text
//! OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 cargo run --example shutdown_handling
//! ```

use std::error::Error;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let handles = otel_bootstrap::init_telemetry("shutdown-example")?;

    // Simulate application work.
    tracing::info!("application started");
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    {
        use tracing::info_span;
        let _span = info_span!("request.handle", http.method = "GET").entered();
        tracing::info!("handled request");
    }

    // Explicit shutdown ensures the batch exporter drains before the tokio
    // runtime shuts down. Without this, spans queued in the batch processor
    // may be dropped.
    tracing::info!("shutting down telemetry");
    handles.shutdown()?;

    Ok(())
}
