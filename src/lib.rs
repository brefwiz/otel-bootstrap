//! One-call OpenTelemetry bootstrap — traces + metrics with OTLP gRPC export.
//!
//! Call [`init_telemetry`] at `main()` before starting the server. Keep the returned
//! [`TelemetryHandles`] alive for the duration of the process — dropping them flushes
//! and shuts down both providers.
//!
//! Configuration is via environment variables per the OpenTelemetry spec:
//! - `OTEL_EXPORTER_OTLP_ENDPOINT` (default: `http://localhost:4317`)
//! - `OTEL_SERVICE_NAME` (overridden by the `service_name` argument)

use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{Resource, metrics::SdkMeterProvider, trace::SdkTracerProvider};
use std::error::Error;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Handles returned by [`init_telemetry`].
///
/// Must be kept alive for the duration of the process.
/// On drop, providers are flushed and shut down.
pub struct TelemetryHandles {
    pub tracer_provider: SdkTracerProvider,
    pub meter_provider: SdkMeterProvider,
}

impl Drop for TelemetryHandles {
    fn drop(&mut self) {
        if let Err(e) = self.tracer_provider.shutdown() {
            eprintln!("tracer provider shutdown error: {e}");
        }
        if let Err(e) = self.meter_provider.shutdown() {
            eprintln!("meter provider shutdown error: {e}");
        }
    }
}

/// Initialise OpenTelemetry traces + metrics with OTLP gRPC export.
///
/// Sets up:
/// - `SdkTracerProvider` registered as the global tracer provider
/// - `SdkMeterProvider` (returned via [`TelemetryHandles`])
/// - `tracing_subscriber` with `OpenTelemetryLayer` + `EnvFilter`
///
/// # Example
/// ```no_run
/// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
/// let _tel = otel_bootstrap::init_telemetry("my-service")?;
/// // start axum server...
/// # Ok(())
/// # }
/// ```
pub fn init_telemetry(service_name: &str) -> Result<TelemetryHandles, Box<dyn Error>> {
    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .unwrap_or_else(|_| "http://localhost:4317".to_string());

    let resource = Resource::builder()
        .with_service_name(service_name.to_string())
        .build();

    // Tracer
    let trace_exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(&endpoint)
        .build()?;

    let tracer_provider = SdkTracerProvider::builder()
        .with_resource(resource.clone())
        .with_batch_exporter(trace_exporter)
        .build();

    // Register as global so tracing-opentelemetry's layer() picks it up
    opentelemetry::global::set_tracer_provider(tracer_provider.clone());

    // Meter
    let metric_exporter = opentelemetry_otlp::MetricExporter::builder()
        .with_tonic()
        .with_endpoint(&endpoint)
        .build()?;

    let meter_provider = SdkMeterProvider::builder()
        .with_resource(resource)
        .with_periodic_exporter(metric_exporter)
        .build();

    // Wire into tracing — layer() uses global tracer provider
    let otel_layer = tracing_opentelemetry::layer();

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer())
        .with(otel_layer)
        .try_init()
        .ok(); // Ignore if already initialised

    Ok(TelemetryHandles {
        tracer_provider,
        meter_provider,
    })
}
