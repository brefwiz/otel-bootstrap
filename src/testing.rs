//! No-op / testing mode for `otel-bootstrap`.
//!
//! Enabled by the `testing` feature flag. Provides [`Telemetry::testing`], which
//! initialises traces and metrics with in-memory, zero-network exporters so callers
//! can init telemetry in unit and integration tests without running an OTLP collector.
//!
//! # Example
//! ```rust
//! # #[cfg(feature = "testing")]
//! # {
//! let handles = otel_bootstrap::Telemetry::testing("my-service");
//! // use tracing macros freely — no collector required
//! tracing::info!("hello from test");
//! handles.shutdown().unwrap();
//! # }
//! ```

use opentelemetry_sdk::{
    metrics::SdkMeterProvider,
    testing::{metrics::TestMetricReader, trace::NoopSpanExporter},
    trace::SdkTracerProvider,
};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::{TelemetryHandles, build_resource};

impl crate::Telemetry {
    /// Initialise telemetry with in-memory, no-network exporters for use in tests.
    ///
    /// - Traces are routed to [`NoopSpanExporter`] via a synchronous processor — no
    ///   batching, no I/O.
    /// - Metrics are collected by [`TestMetricReader`] — queryable in-process, no I/O.
    /// - No log exporter is wired up; `tracing` events still reach the `fmt` layer on
    ///   stdout.
    /// - The tracing subscriber is installed with [`try_init`](tracing_subscriber::util::SubscriberInitExt::try_init)
    ///   so parallel tests that each call this function do not panic.
    pub fn testing(service_name: &str) -> TelemetryHandles {
        let resource = build_resource(service_name, None, None);

        let tracer_provider = SdkTracerProvider::builder()
            .with_resource(resource.clone())
            .with_simple_exporter(NoopSpanExporter::new())
            .build();

        opentelemetry::global::set_tracer_provider(tracer_provider.clone());

        let metric_reader = TestMetricReader::new();
        let meter_provider = SdkMeterProvider::builder()
            .with_resource(resource)
            .with_reader(metric_reader)
            .build();

        opentelemetry::global::set_meter_provider(meter_provider.clone());

        let otel_layer = tracing_opentelemetry::layer();
        tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_subscriber::fmt::layer())
            .with(otel_layer)
            .try_init()
            .ok();

        TelemetryHandles {
            tracer_provider,
            meter_provider: Some(meter_provider),
            logger_provider: None,
            shutdown_timeout: crate::DEFAULT_SHUTDOWN_TIMEOUT,
        }
    }
}
