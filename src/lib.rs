//! One-call OpenTelemetry bootstrap — traces + metrics with OTLP gRPC export.
//!
//! Call [`init_telemetry`] at `main()` before starting the server. Keep the returned
//! [`TelemetryHandles`] alive for the duration of the process — dropping them flushes
//! and shuts down both providers.
//!
//! Configuration is via environment variables per the OpenTelemetry spec:
//! - `OTEL_EXPORTER_OTLP_ENDPOINT` (default: `http://localhost:4317`)
//! - `OTEL_SERVICE_NAME` (overridden by the `service_name` argument)

use opentelemetry::KeyValue;
use opentelemetry::propagation::TextMapCompositePropagator;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{
    Resource,
    metrics::SdkMeterProvider,
    propagation::{BaggagePropagator, TraceContextPropagator},
    trace::SdkTracerProvider,
};
use opentelemetry_semantic_conventions::attribute::{
    DEPLOYMENT_ENVIRONMENT_NAME, HOST_NAME, PROCESS_PID, SERVICE_VERSION,
};
use std::error::Error;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Handles returned by [`init_telemetry`].
///
/// Keep alive for the duration of the process. Call [`shutdown`](TelemetryHandles::shutdown)
/// before exiting to flush pending spans and metrics.
pub struct TelemetryHandles {
    pub tracer_provider: SdkTracerProvider,
    pub meter_provider: SdkMeterProvider,
}

impl TelemetryHandles {
    /// Flush pending data and shut down both providers.
    ///
    /// Must be called before the tokio runtime shuts down so the batch
    /// exporter can send remaining spans over gRPC. Safe to call multiple
    /// times — subsequent calls are no-ops.
    pub fn shutdown(&self) -> Result<(), Box<dyn Error>> {
        self.tracer_provider.shutdown()?;
        self.meter_provider.shutdown()?;
        Ok(())
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

    let resource = build_resource(service_name, None, None);

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

    // Register W3C TraceContext + Baggage propagators for distributed tracing
    let propagator = TextMapCompositePropagator::new(vec![
        Box::new(TraceContextPropagator::new()),
        Box::new(BaggagePropagator::new()),
    ]);
    opentelemetry::global::set_text_map_propagator(propagator);

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

/// Build a [`Resource`] enriched with semantic-convention attributes.
///
/// Auto-detects `host.name` and `process.pid`. Optionally sets
/// `service.version` and `deployment.environment` when provided.
pub fn build_resource(
    service_name: &str,
    service_version: Option<&str>,
    deployment_environment: Option<&str>,
) -> Resource {
    let hostname = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_default();

    let mut builder = Resource::builder()
        .with_service_name(service_name.to_string())
        .with_attributes([
            KeyValue::new(HOST_NAME, hostname),
            KeyValue::new(PROCESS_PID, std::process::id() as i64),
        ]);

    if let Some(version) = service_version {
        builder = builder.with_attribute(KeyValue::new(SERVICE_VERSION, version.to_string()));
    }

    if let Some(env) = deployment_environment {
        builder =
            builder.with_attribute(KeyValue::new(DEPLOYMENT_ENVIRONMENT_NAME, env.to_string()));
    }

    builder.build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_contains_all_attributes_when_provided() {
        let resource = build_resource("test-svc", Some("1.2.3"), Some("staging"));

        assert_eq!(
            resource.get(&opentelemetry::Key::new("service.name")),
            Some(opentelemetry::Value::from("test-svc")),
        );
        assert_eq!(
            resource.get(&opentelemetry::Key::new(SERVICE_VERSION)),
            Some(opentelemetry::Value::from("1.2.3")),
        );
        assert_eq!(
            resource.get(&opentelemetry::Key::new(DEPLOYMENT_ENVIRONMENT_NAME)),
            Some(opentelemetry::Value::from("staging")),
        );
        assert!(resource.get(&opentelemetry::Key::new(HOST_NAME)).is_some());
        assert!(
            resource
                .get(&opentelemetry::Key::new(PROCESS_PID))
                .is_some()
        );
    }

    #[test]
    fn resource_graceful_when_optional_values_omitted() {
        let resource = build_resource("test-svc", None, None);

        assert_eq!(
            resource.get(&opentelemetry::Key::new("service.name")),
            Some(opentelemetry::Value::from("test-svc")),
        );
        assert!(
            resource
                .get(&opentelemetry::Key::new(SERVICE_VERSION))
                .is_none()
        );
        assert!(
            resource
                .get(&opentelemetry::Key::new(DEPLOYMENT_ENVIRONMENT_NAME))
                .is_none()
        );
        // Auto-detected attributes still present
        assert!(resource.get(&opentelemetry::Key::new(HOST_NAME)).is_some());
        assert!(
            resource
                .get(&opentelemetry::Key::new(PROCESS_PID))
                .is_some()
        );
    }
}
