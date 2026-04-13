//! One-call OpenTelemetry bootstrap — traces + metrics with OTLP gRPC export.
//!
//! Call [`init_telemetry`] at `main()` before starting the server. Keep the returned
//! [`TelemetryHandles`] alive for the duration of the process — dropping them flushes
//! and shuts down both providers.
//!
//! Configuration is via environment variables per the OpenTelemetry spec:
//! - `OTEL_EXPORTER_OTLP_ENDPOINT` (default: `http://localhost:4317`)
//! - `OTEL_SERVICE_NAME` (overridden by the `service_name` argument)
//! - `OTEL_TRACES_SAMPLER` / `OTEL_TRACES_SAMPLER_ARG` (fallback when no explicit sampler is set)

use opentelemetry::KeyValue;
use opentelemetry::propagation::TextMapCompositePropagator;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{
    Resource,
    metrics::SdkMeterProvider,
    propagation::{BaggagePropagator, TraceContextPropagator},
    trace::{Sampler, SdkTracerProvider},
};
use opentelemetry_semantic_conventions::attribute::{
    DEPLOYMENT_ENVIRONMENT_NAME, HOST_NAME, PROCESS_PID, SERVICE_VERSION,
};
use std::error::Error;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Trace sampler configuration.
///
/// Controls how many traces are sampled. When no explicit sampler is passed to
/// [`init_telemetry_with_sampler`], the library falls back to the
/// `OTEL_TRACES_SAMPLER` / `OTEL_TRACES_SAMPLER_ARG` environment variables,
/// and finally to [`TraceSampler::AlwaysOn`] for backward compatibility.
#[derive(Debug, Clone)]
pub enum TraceSampler {
    /// Record every trace (the default).
    AlwaysOn,
    /// Never record any trace.
    AlwaysOff,
    /// Sample a fraction of traces. `ratio` must be between 0.0 and 1.0.
    TraceIdRatio(f64),
    /// Respect the parent span's sampling decision; use the given sampler for
    /// root spans (spans without a remote parent).
    ParentBased(Box<TraceSampler>),
}

impl TraceSampler {
    /// Convert to the SDK [`Sampler`].
    fn into_sdk_sampler(self) -> Sampler {
        match self {
            TraceSampler::AlwaysOn => Sampler::AlwaysOn,
            TraceSampler::AlwaysOff => Sampler::AlwaysOff,
            TraceSampler::TraceIdRatio(r) => Sampler::TraceIdRatioBased(r),
            TraceSampler::ParentBased(inner) => {
                Sampler::ParentBased(Box::new(inner.into_sdk_sampler()))
            }
        }
    }
}

/// Resolve the sampler from `OTEL_TRACES_SAMPLER` and `OTEL_TRACES_SAMPLER_ARG`
/// environment variables. Returns `None` when the variable is unset.
fn sampler_from_env() -> Option<TraceSampler> {
    let name = std::env::var("OTEL_TRACES_SAMPLER").ok()?;
    let arg = std::env::var("OTEL_TRACES_SAMPLER_ARG").ok();
    Some(match name.as_str() {
        "always_on" => TraceSampler::AlwaysOn,
        "always_off" => TraceSampler::AlwaysOff,
        "traceidratio" => {
            let ratio = arg.as_deref().unwrap_or("1.0").parse::<f64>().unwrap_or(1.0);
            TraceSampler::TraceIdRatio(ratio)
        }
        "parentbased_always_on" => TraceSampler::ParentBased(Box::new(TraceSampler::AlwaysOn)),
        "parentbased_always_off" => TraceSampler::ParentBased(Box::new(TraceSampler::AlwaysOff)),
        "parentbased_traceidratio" => {
            let ratio = arg.as_deref().unwrap_or("1.0").parse::<f64>().unwrap_or(1.0);
            TraceSampler::ParentBased(Box::new(TraceSampler::TraceIdRatio(ratio)))
        }
        _ => return None,
    })
}

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
/// Uses the default sampler (always-on), unless `OTEL_TRACES_SAMPLER` is set.
/// For explicit sampler control, use [`init_telemetry_with_sampler`].
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
    init_telemetry_with_sampler(service_name, None)
}

/// Initialise OpenTelemetry traces + metrics with OTLP gRPC export and an
/// explicit trace sampler.
///
/// When `sampler` is `None`, the function checks `OTEL_TRACES_SAMPLER` /
/// `OTEL_TRACES_SAMPLER_ARG` and falls back to always-on.
///
/// # Example
/// ```no_run
/// use otel_bootstrap::TraceSampler;
/// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
/// let sampler = TraceSampler::ParentBased(Box::new(TraceSampler::TraceIdRatio(0.1)));
/// let _tel = otel_bootstrap::init_telemetry_with_sampler("my-service", Some(sampler))?;
/// # Ok(())
/// # }
/// ```
pub fn init_telemetry_with_sampler(
    service_name: &str,
    sampler: Option<TraceSampler>,
) -> Result<TelemetryHandles, Box<dyn Error>> {
    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .unwrap_or_else(|_| "http://localhost:4317".to_string());

    let resource = build_resource(service_name, None, None);

    let sampler = sampler
        .or_else(sampler_from_env)
        .unwrap_or(TraceSampler::AlwaysOn);

    // Tracer
    let trace_exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(&endpoint)
        .build()?;

    let tracer_provider = SdkTracerProvider::builder()
        .with_resource(resource.clone())
        .with_sampler(sampler.into_sdk_sampler())
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

    #[test]
    fn trace_sampler_ratio_converts_to_sdk() {
        let sampler = TraceSampler::TraceIdRatio(0.5);
        let sdk = sampler.into_sdk_sampler();
        assert_eq!(format!("{sdk:?}"), "TraceIdRatioBased(0.5)");
    }

    #[test]
    fn trace_sampler_parent_based_converts_to_sdk() {
        let sampler = TraceSampler::ParentBased(Box::new(TraceSampler::TraceIdRatio(0.25)));
        let sdk = sampler.into_sdk_sampler();
        let debug = format!("{sdk:?}");
        assert!(debug.contains("ParentBased"));
        assert!(debug.contains("0.25"));
    }

    /// # Safety helper — env var manipulation is unsafe in Rust 2024 edition.
    unsafe fn set_env(key: &str, val: &str) {
        unsafe { std::env::set_var(key, val); }
    }

    unsafe fn remove_env(key: &str) {
        unsafe { std::env::remove_var(key); }
    }

    #[test]
    fn sampler_from_env_reads_traceidratio() {
        unsafe {
            set_env("OTEL_TRACES_SAMPLER", "traceidratio");
            set_env("OTEL_TRACES_SAMPLER_ARG", "0.42");
        }

        let sampler = sampler_from_env().expect("should parse env");
        assert!(matches!(sampler, TraceSampler::TraceIdRatio(r) if (r - 0.42).abs() < f64::EPSILON));

        unsafe {
            remove_env("OTEL_TRACES_SAMPLER");
            remove_env("OTEL_TRACES_SAMPLER_ARG");
        }
    }

    #[test]
    fn sampler_from_env_returns_none_when_unset() {
        unsafe { remove_env("OTEL_TRACES_SAMPLER"); }
        assert!(sampler_from_env().is_none());
    }

    #[test]
    fn sampler_from_env_reads_parentbased_traceidratio() {
        unsafe {
            set_env("OTEL_TRACES_SAMPLER", "parentbased_traceidratio");
            set_env("OTEL_TRACES_SAMPLER_ARG", "0.1");
        }

        let sampler = sampler_from_env().expect("should parse env");
        assert!(matches!(sampler, TraceSampler::ParentBased(inner) if matches!(*inner, TraceSampler::TraceIdRatio(r) if (r - 0.1).abs() < f64::EPSILON)));

        unsafe {
            remove_env("OTEL_TRACES_SAMPLER");
            remove_env("OTEL_TRACES_SAMPLER_ARG");
        }
    }

    #[test]
    fn sampler_from_env_always_on() {
        unsafe { set_env("OTEL_TRACES_SAMPLER", "always_on"); }
        let sampler = sampler_from_env().expect("should parse env");
        assert!(matches!(sampler, TraceSampler::AlwaysOn));
        unsafe { remove_env("OTEL_TRACES_SAMPLER"); }
    }

    #[test]
    fn sampler_from_env_always_off() {
        unsafe { set_env("OTEL_TRACES_SAMPLER", "always_off"); }
        let sampler = sampler_from_env().expect("should parse env");
        assert!(matches!(sampler, TraceSampler::AlwaysOff));
        unsafe { remove_env("OTEL_TRACES_SAMPLER"); }
    }
}
