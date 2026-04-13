//! Basic telemetry bootstrap example.
//!
//! Shows the minimal setup to initialise OpenTelemetry traces, metrics and logs
//! with a single call, then emit a span and shut down cleanly.
//!
//! Run with:
//! ```text
//! OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 cargo run --example basic_setup
//! ```
//! Without a collector running the export will fail but the example itself will
//! still complete — exporter errors are non-fatal by design.

use std::error::Error;
use tracing::info_span;

fn main() -> Result<(), Box<dyn Error>> {
    // Initialise traces + metrics (logs are opt-in).
    let handles = otel_bootstrap::init_telemetry("basic-setup-example")?;

    // Emit a sample span so there is something to export.
    let _span = info_span!("example.work", step = 1).entered();
    tracing::info!("basic_setup example running");
    drop(_span);

    // Flush and shut down all providers before the process exits.
    handles.shutdown()?;
    Ok(())
}
