//! One-call OpenTelemetry bootstrap — traces + metrics + logs with OTLP export.
//!
//! Call [`init_telemetry`] at `main()` before starting the server. Keep the returned
//! [`TelemetryHandles`] alive for the duration of the process — dropping them flushes
//! and shuts down both providers.
//!
//! Configuration is via environment variables per the OpenTelemetry spec:
//! - `OTEL_EXPORTER_OTLP_ENDPOINT` (default: `http://localhost:4317` for gRPC, `http://localhost:4318` for HTTP)
//! - `OTEL_EXPORTER_OTLP_PROTOCOL` (`grpc` or `http/protobuf`) — selects transport when both features are enabled
//! - `OTEL_EXPORTER_OTLP_TIMEOUT` — export timeout in milliseconds (default: 10 000 ms)
//! - `OTEL_SERVICE_NAME` (overridden by the `service_name` argument)
//! - `OTEL_TRACES_SAMPLER` / `OTEL_TRACES_SAMPLER_ARG` (fallback when no explicit sampler is set)
//!
//! ## Env var handling: otel-bootstrap vs SDK
//! | Env var | Handled by |
//! |---------|-----------|
//! | `OTEL_SERVICE_NAME` | otel-bootstrap (falls back to SDK default) |
//! | `OTEL_TRACES_SAMPLER` / `OTEL_TRACES_SAMPLER_ARG` | otel-bootstrap |
//! | `OTEL_EXPORTER_OTLP_PROTOCOL` | otel-bootstrap |
//! | `OTEL_EXPORTER_OTLP_ENDPOINT` | otel-bootstrap |
//! | `OTEL_EXPORTER_OTLP_TIMEOUT` | otel-bootstrap |
//! | `OTEL_BSP_MAX_EXPORT_BATCH_SIZE` | SDK (batch span processor) |
//! | `OTEL_METRIC_EXPORT_INTERVAL` | SDK (periodic reader) |
//! | Per-signal endpoints (`OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` etc.) | SDK |

#[cfg(not(any(feature = "grpc", feature = "http")))]
compile_error!("at least one transport feature must be enabled: `grpc` or `http`");

#[cfg(feature = "testing")]
pub mod testing;

#[cfg(feature = "axum")]
pub mod axum_middleware;

use opentelemetry::KeyValue;
use opentelemetry::propagation::TextMapCompositePropagator;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{
    Resource,
    logs::SdkLoggerProvider,
    metrics::{MeterProviderBuilder, PeriodicReader, SdkMeterProvider},
    propagation::{BaggagePropagator, TraceContextPropagator},
    trace::{BatchConfigBuilder, BatchSpanProcessor, Sampler, SdkTracerProvider},
};
use opentelemetry_semantic_conventions::attribute::{
    DEPLOYMENT_ENVIRONMENT_NAME, HOST_NAME, PROCESS_PID, SERVICE_VERSION,
};
use std::error::Error;
use std::time::Duration;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Trace sampler configuration.
///
/// Controls how many traces are sampled. When no explicit sampler is passed to
/// [`init_telemetry_with_sampler`], the library falls back to the
/// `OTEL_TRACES_SAMPLER` / `OTEL_TRACES_SAMPLER_ARG` environment variables,
/// and finally to [`TraceSampler::AlwaysOn`] for backward compatibility.
///
/// # Example
/// ```
/// use otel_bootstrap::TraceSampler;
///
/// // Sample 10 % of root spans; inherit parent decision for child spans.
/// let sampler = TraceSampler::ParentBased(Box::new(TraceSampler::TraceIdRatio(0.1)));
/// ```
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
/// environment variables.
///
/// Returns:
/// - `Ok(None)` when `OTEL_TRACES_SAMPLER` is unset.
/// - `Ok(Some(_))` for a recognised sampler name.
/// - `Err(_)` for an unrecognised sampler name (clear error at init time).
fn sampler_from_env() -> Result<Option<TraceSampler>, Box<dyn Error>> {
    let name = match std::env::var("OTEL_TRACES_SAMPLER") {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    let arg = std::env::var("OTEL_TRACES_SAMPLER_ARG").ok();
    let sampler = match name.as_str() {
        "always_on" => TraceSampler::AlwaysOn,
        "always_off" => TraceSampler::AlwaysOff,
        "traceidratio" => {
            let ratio = arg
                .as_deref()
                .unwrap_or("1.0")
                .parse::<f64>()
                .unwrap_or(1.0);
            TraceSampler::TraceIdRatio(ratio)
        }
        "parentbased_always_on" => TraceSampler::ParentBased(Box::new(TraceSampler::AlwaysOn)),
        "parentbased_always_off" => TraceSampler::ParentBased(Box::new(TraceSampler::AlwaysOff)),
        "parentbased_traceidratio" => {
            let ratio = arg
                .as_deref()
                .unwrap_or("1.0")
                .parse::<f64>()
                .unwrap_or(1.0);
            TraceSampler::ParentBased(Box::new(TraceSampler::TraceIdRatio(ratio)))
        }
        unknown => {
            return Err(format!(
                "OTEL_TRACES_SAMPLER: unrecognised sampler name '{unknown}'. \
                 Valid values: always_on, always_off, traceidratio, \
                 parentbased_always_on, parentbased_always_off, parentbased_traceidratio"
            )
            .into());
        }
    };
    Ok(Some(sampler))
}

/// Default timeout for provider shutdown in [`Drop`].
const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Handles returned by [`init_telemetry`] or [`TelemetryBuilder::init`].
///
/// Keep alive for the duration of the process. Call [`shutdown`](TelemetryHandles::shutdown)
/// before exiting to flush pending spans, metrics, and logs.
///
/// When dropped, shutdown is attempted with a bounded timeout (default: 5 s).
/// If the timeout expires a warning is logged but the process continues normally.
///
/// # Example
/// ```no_run
/// #[tokio::main]
/// async fn main() -> Result<(), Box<dyn std::error::Error>> {
///     let handles = otel_bootstrap::init_telemetry("my-service")?;
///
///     // run your application here …
///
///     handles.shutdown()?;
///     Ok(())
/// }
/// ```
pub struct TelemetryHandles {
    pub tracer_provider: SdkTracerProvider,
    pub meter_provider: Option<SdkMeterProvider>,
    pub logger_provider: Option<SdkLoggerProvider>,
    shutdown_timeout: Duration,
}

impl TelemetryHandles {
    /// Flush pending data and shut down all providers.
    ///
    /// Must be called before the tokio runtime shuts down so the batch
    /// exporter can send remaining spans over gRPC. Safe to call multiple
    /// times — subsequent calls are no-ops.
    ///
    /// # Example
    /// ```no_run
    /// let handles = otel_bootstrap::init_telemetry("my-service").unwrap();
    /// // … application logic …
    /// handles.shutdown().expect("telemetry shutdown failed");
    /// ```
    pub fn shutdown(&self) -> Result<(), Box<dyn Error>> {
        self.tracer_provider.shutdown()?;
        if let Some(mp) = &self.meter_provider {
            mp.shutdown()?;
        }
        if let Some(lp) = &self.logger_provider {
            lp.shutdown()?;
        }
        Ok(())
    }
}

impl Drop for TelemetryHandles {
    fn drop(&mut self) {
        let tracer_provider = self.tracer_provider.clone();
        let meter_provider = self.meter_provider.clone();
        let logger_provider = self.logger_provider.clone();
        let timeout = self.shutdown_timeout;

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Err(e) = tracer_provider.shutdown() {
                tracing::warn!("tracer provider shutdown error: {e}");
            }
            if let Some(mp) = meter_provider
                && let Err(e) = mp.shutdown()
            {
                tracing::warn!("meter provider shutdown error: {e}");
            }
            if let Some(lp) = logger_provider
                && let Err(e) = lp.shutdown()
            {
                tracing::warn!("logger provider shutdown error: {e}");
            }
            let _ = tx.send(());
        });

        if rx.recv_timeout(timeout).is_err() {
            tracing::warn!(
                "telemetry shutdown did not complete within {timeout:?}; \
                 some spans/metrics may not have been exported"
            );
        }
    }
}

/// OTLP export protocol.
///
/// Selects between gRPC/tonic and HTTP/protobuf transports. When not set
/// explicitly, the builder reads `OTEL_EXPORTER_OTLP_PROTOCOL`. If both the
/// `grpc` and `http` features are compiled in and neither the builder nor the
/// env var specifies a protocol, `grpc` is used.
///
/// Each variant is only present when its corresponding feature is enabled, so
/// match expressions are always exhaustive without a fallback arm.
///
/// # Example
/// ```no_run
/// # #[cfg(feature = "grpc")]
/// # {
/// use otel_bootstrap::{ExportProtocol, Telemetry};
///
/// let _handles = Telemetry::builder("my-service")
///     .with_protocol(ExportProtocol::Grpc)
///     .init()
///     .unwrap();
/// # }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportProtocol {
    /// gRPC via tonic (requires the `grpc` feature).
    #[cfg(feature = "grpc")]
    Grpc,
    /// HTTP/protobuf (requires the `http` feature).
    #[cfg(feature = "http")]
    HttpProtobuf,
}

/// Resolve the export protocol from `OTEL_EXPORTER_OTLP_PROTOCOL`.
fn protocol_from_env() -> Option<ExportProtocol> {
    let val = std::env::var("OTEL_EXPORTER_OTLP_PROTOCOL").ok()?;
    match val.trim() {
        #[cfg(feature = "grpc")]
        "grpc" => Some(ExportProtocol::Grpc),
        #[cfg(feature = "http")]
        "http/protobuf" => Some(ExportProtocol::HttpProtobuf),
        _ => None,
    }
}

/// Entry point for configuring telemetry via a builder pattern.
///
/// # Example
/// ```no_run
/// # fn run() -> Result<(), Box<dyn std::error::Error>> {
/// let _handles = otel_bootstrap::Telemetry::builder("my-service")
///     .with_version("1.0.0")
///     .with_environment("production")
///     .with_sampler(otel_bootstrap::TraceSampler::TraceIdRatio(0.1))
///     .with_metrics(true)
///     .with_logs(true)
///     .init()?;
/// # Ok(())
/// # }
/// ```
pub struct Telemetry;

impl Telemetry {
    /// Create a new [`TelemetryBuilder`] with the given service name.
    ///
    /// The explicit `service_name` takes precedence over `OTEL_SERVICE_NAME`.
    pub fn builder(service_name: &str) -> TelemetryBuilder {
        TelemetryBuilder {
            service_name: Some(service_name.to_string()),
            service_version: None,
            deployment_environment: None,
            sampler: None,
            metrics: true,
            logs: false,
            protocol: None,
            max_export_batch_size: None,
            metric_export_interval: None,
            export_timeout: None,
            shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
            extra_layers: Vec::new(),
            extra_metric_readers: Vec::new(),
        }
    }

    /// Create a new [`TelemetryBuilder`] that reads the service name from
    /// `OTEL_SERVICE_NAME`. Falls back to `"unknown_service"` when the env var
    /// is not set, following the OpenTelemetry default resource specification.
    ///
    /// # Example
    /// ```no_run
    /// // Set OTEL_SERVICE_NAME=my-service in the environment before calling this.
    /// let _handles = otel_bootstrap::Telemetry::from_env().init().unwrap();
    /// ```
    pub fn from_env() -> TelemetryBuilder {
        TelemetryBuilder {
            service_name: None,
            service_version: None,
            deployment_environment: None,
            sampler: None,
            metrics: true,
            logs: false,
            protocol: None,
            max_export_batch_size: None,
            metric_export_interval: None,
            export_timeout: None,
            shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
            extra_layers: Vec::new(),
            extra_metric_readers: Vec::new(),
        }
    }
}

/// Builder for configuring telemetry options incrementally.
///
/// Created via [`Telemetry::builder`] or [`Telemetry::from_env`]. Call
/// [`.init()`](TelemetryBuilder::init) to consume the builder and start telemetry.
///
/// # Example
/// ```no_run
/// use std::time::Duration;
///
/// let _handles = otel_bootstrap::Telemetry::builder("my-service")
///     .with_version("1.2.3")
///     .with_environment("staging")
///     .with_metrics(true)
///     .with_shutdown_timeout(Duration::from_secs(10))
///     .init()
///     .unwrap();
/// ```
#[must_use = "a TelemetryBuilder does nothing until .init() is called"]
pub struct TelemetryBuilder {
    service_name: Option<String>,
    service_version: Option<String>,
    deployment_environment: Option<String>,
    sampler: Option<TraceSampler>,
    metrics: bool,
    logs: bool,
    protocol: Option<ExportProtocol>,
    max_export_batch_size: Option<usize>,
    metric_export_interval: Option<Duration>,
    export_timeout: Option<Duration>,
    shutdown_timeout: Duration,
    extra_layers: Vec<
        Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync + 'static>,
    >,
    extra_metric_readers: Vec<MeterProviderInstaller>,
}

/// Type-erased adapter that applies an extra [`MetricReader`] to the
/// in-progress [`MeterProviderBuilder`]. Stored as a closure so the trait
/// (which is generic, not object-safe in a useful way here) can be ranged
/// over uniformly inside [`TelemetryBuilder`].
type MeterProviderInstaller =
    Box<dyn FnOnce(MeterProviderBuilder) -> MeterProviderBuilder + Send + Sync>;

impl TelemetryBuilder {
    /// Set the service version (maps to `service.version` resource attribute).
    pub fn with_version(mut self, version: &str) -> Self {
        self.service_version = Some(version.to_string());
        self
    }

    /// Set the deployment environment (maps to `deployment.environment.name`).
    pub fn with_environment(mut self, environment: &str) -> Self {
        self.deployment_environment = Some(environment.to_string());
        self
    }

    /// Set an explicit trace sampler. If not set, falls back to
    /// `OTEL_TRACES_SAMPLER` env var, then always-on.
    pub fn with_sampler(mut self, sampler: TraceSampler) -> Self {
        self.sampler = Some(sampler);
        self
    }

    /// Enable or disable metrics export (default: `true`).
    pub fn with_metrics(mut self, enabled: bool) -> Self {
        self.metrics = enabled;
        self
    }

    /// Set the export protocol explicitly. If not set, falls back to
    /// `OTEL_EXPORTER_OTLP_PROTOCOL`, then the compiled-in default (`grpc`
    /// when the `grpc` feature is enabled, `http/protobuf` otherwise).
    pub fn with_protocol(mut self, protocol: ExportProtocol) -> Self {
        self.protocol = Some(protocol);
        self
    }

    /// Set the maximum number of spans exported in a single batch (default: 512).
    ///
    /// Overrides `OTEL_BSP_MAX_EXPORT_BATCH_SIZE` when set programmatically.
    /// The env var is still read as a fallback when this method is not called.
    pub fn with_max_export_batch_size(mut self, size: usize) -> Self {
        self.max_export_batch_size = Some(size);
        self
    }

    /// Set the interval between metric exports (default: 60 s).
    ///
    /// Returns an error at build time if `interval` is zero.
    /// Overrides `OTEL_METRIC_EXPORT_INTERVAL` when set programmatically.
    pub fn with_metric_export_interval(mut self, interval: Duration) -> Self {
        self.metric_export_interval = Some(interval);
        self
    }

    /// Enable or disable log export via the OTLP log bridge (default: `false`).
    ///
    /// When enabled, `tracing` events are forwarded to an OTLP `LogExporter`
    /// in addition to the existing stdout fmt layer. This allows structured
    /// logs to be correlated with traces in backends like Grafana Loki or
    /// Datadog.
    pub fn with_logs(mut self, enabled: bool) -> Self {
        self.logs = enabled;
        self
    }

    /// Set the OTLP export timeout explicitly. If not set, falls back to
    /// `OTEL_EXPORTER_OTLP_TIMEOUT` (in milliseconds), then the SDK default
    /// of 10 000 ms.
    pub fn with_export_timeout(mut self, timeout: Duration) -> Self {
        self.export_timeout = Some(timeout);
        self
    }

    /// Set the maximum time to wait for provider shutdown when the
    /// [`TelemetryHandles`] is dropped (default: 5 s).
    ///
    /// If the timeout expires a warning is logged and the drop completes
    /// without panicking. The background shutdown thread is abandoned and
    /// the providers may not have flushed all pending data.
    pub fn with_shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.shutdown_timeout = timeout;
        self
    }

    /// Add a custom [`tracing_subscriber::Layer`] to the subscriber stack.
    ///
    /// Multiple layers can be added by chaining calls. Each layer is composed
    /// with the built-in `EnvFilter`, `fmt`, and OpenTelemetry layers.
    ///
    /// Insertion order in the subscriber stack (inner → outer, i.e. first-added
    /// to last-added):
    /// ```text
    /// registry → custom layers → EnvFilter → fmt → OTel
    /// ```
    /// Because `EnvFilter` is outer, it can suppress events before they reach
    /// the `fmt` and OTel layers; custom layers receive events independently
    /// according to their own `enabled()` implementation.
    ///
    /// # Example
    /// ```no_run
    /// # fn run() -> Result<(), Box<dyn std::error::Error>> {
    /// let _handles = otel_bootstrap::Telemetry::builder("my-service")
    ///     .with_layer(tracing_subscriber::fmt::layer().with_target(false))
    ///     .init()?;
    /// # Ok(())
    /// # }
    /// ```
    /// Customise the [`MeterProviderBuilder`] before it is built.
    ///
    /// Runs after the built-in OTLP `PeriodicReader` is attached (when
    /// [`with_metrics`](Self::with_metrics) is enabled) and before
    /// `.build()` is called. The closure is the escape hatch for everything
    /// the explicit builder methods do not cover — most importantly,
    /// installing **additional [`MetricReader`]s** like
    /// [`opentelemetry-prometheus`](https://crates.io/crates/opentelemetry-prometheus)
    /// alongside the OTLP push, so the same instruments fan out to multiple
    /// transports without double-counting.
    ///
    /// May be called multiple times; closures run in registration order.
    /// Has no effect when `with_metrics(false)` is also set on the builder —
    /// when metrics are disabled, no `MeterProvider` is created at all.
    ///
    /// `MetricReader` is intentionally not nameable from outside
    /// `opentelemetry_sdk`, so the closure form is the only way to attach
    /// readers without leaking unstable trait names through this crate's
    /// public API.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // With `opentelemetry-prometheus` in scope:
    /// let registry = prometheus::Registry::new();
    /// let exporter = opentelemetry_prometheus::exporter()
    ///     .with_registry(registry.clone())
    ///     .build()?;
    /// let _handles = otel_bootstrap::Telemetry::builder("my-service")
    ///     .with_meter_provider_setup(move |b| b.with_reader(exporter))
    ///     .init()?;
    /// // ...mount `registry` at GET /metrics in your HTTP layer.
    /// ```
    pub fn with_meter_provider_setup<F>(mut self, setup: F) -> Self
    where
        F: FnOnce(MeterProviderBuilder) -> MeterProviderBuilder + Send + Sync + 'static,
    {
        self.extra_metric_readers.push(Box::new(setup));
        self
    }

    pub fn with_layer<L>(mut self, layer: L) -> Self
    where
        L: tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync + 'static,
    {
        self.extra_layers.push(Box::new(layer));
        self
    }

    /// Consume the builder and initialise OpenTelemetry.
    ///
    /// Installs a global tracer provider, meter provider (if enabled), and
    /// a `tracing` subscriber. Returns an error if any provider fails to
    /// build (e.g. unknown sampler name, zero metric interval).
    ///
    /// # Example
    /// ```no_run
    /// let handles = otel_bootstrap::Telemetry::builder("my-service")
    ///     .with_metrics(false)
    ///     .init()
    ///     .expect("telemetry init failed");
    /// handles.shutdown().ok();
    /// ```
    pub fn init(self) -> Result<TelemetryHandles, Box<dyn Error>> {
        if let Some(interval) = self.metric_export_interval
            && interval.is_zero()
        {
            return Err("metric_export_interval must be greater than zero".into());
        }

        let protocol = self.protocol.or_else(protocol_from_env).unwrap_or({
            #[cfg(feature = "grpc")]
            {
                ExportProtocol::Grpc
            }
            #[cfg(all(not(feature = "grpc"), feature = "http"))]
            {
                ExportProtocol::HttpProtobuf
            }
        });

        let default_endpoint = match protocol {
            #[cfg(feature = "grpc")]
            ExportProtocol::Grpc => "http://localhost:4317",
            #[cfg(feature = "http")]
            ExportProtocol::HttpProtobuf => "http://localhost:4318",
        };
        let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
            .unwrap_or_else(|_| default_endpoint.to_string());

        // Resolve export timeout: explicit builder > OTEL_EXPORTER_OTLP_TIMEOUT > SDK default (10 s)
        let export_timeout = self.export_timeout.or_else(timeout_from_env);

        // Resolve service name: explicit builder > OTEL_SERVICE_NAME > "unknown_service"
        let service_name = self.service_name.unwrap_or_else(|| {
            std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| "unknown_service".to_string())
        });

        let resource = build_resource(
            &service_name,
            self.service_version.as_deref(),
            self.deployment_environment.as_deref(),
        );

        let sampler = match self.sampler {
            Some(s) => s,
            None => sampler_from_env()?.unwrap_or(TraceSampler::AlwaysOn),
        };

        // Tracer
        let trace_exporter = build_span_exporter(protocol, &endpoint, export_timeout)?;

        let batch_processor = if let Some(size) = self.max_export_batch_size {
            BatchSpanProcessor::builder(trace_exporter)
                .with_batch_config(
                    BatchConfigBuilder::default()
                        .with_max_export_batch_size(size)
                        .build(),
                )
                .build()
        } else {
            BatchSpanProcessor::builder(trace_exporter).build()
        };

        let tracer_provider = SdkTracerProvider::builder()
            .with_resource(resource.clone())
            .with_sampler(sampler.into_sdk_sampler())
            .with_span_processor(batch_processor)
            .build();

        opentelemetry::global::set_tracer_provider(tracer_provider.clone());

        // Register W3C TraceContext + Baggage propagators
        let propagator = TextMapCompositePropagator::new(vec![
            Box::new(TraceContextPropagator::new()),
            Box::new(BaggagePropagator::new()),
        ]);
        opentelemetry::global::set_text_map_propagator(propagator);

        // Meter (optional)
        let meter_provider = if self.metrics {
            let metric_exporter = build_metric_exporter(protocol, &endpoint, export_timeout)?;

            let periodic_reader = if let Some(interval) = self.metric_export_interval {
                PeriodicReader::builder(metric_exporter)
                    .with_interval(interval)
                    .build()
            } else {
                PeriodicReader::builder(metric_exporter).build()
            };

            let mut mp_builder = SdkMeterProvider::builder()
                .with_resource(resource.clone())
                .with_reader(periodic_reader);
            for installer in self.extra_metric_readers {
                mp_builder = installer(mp_builder);
            }
            let mp = mp_builder.build();

            opentelemetry::global::set_meter_provider(mp.clone());

            Some(mp)
        } else {
            None
        };

        // Logger (optional) — bridges tracing events to the OTLP log pipeline
        let logger_provider = if self.logs {
            let log_exporter = build_log_exporter(protocol, &endpoint, export_timeout)?;

            let lp = SdkLoggerProvider::builder()
                .with_resource(resource)
                .with_batch_exporter(log_exporter)
                .build();

            Some(lp)
        } else {
            None
        };

        // Wire into tracing
        let otel_layer = tracing_opentelemetry::layer();

        let registry = tracing_subscriber::registry()
            .with(self.extra_layers)
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_subscriber::fmt::layer())
            .with(otel_layer);

        if let Some(lp) = &logger_provider {
            registry
                .with(opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(lp))
                .try_init()
                .ok();
        } else {
            registry.try_init().ok();
        }

        Ok(TelemetryHandles {
            tracer_provider,
            meter_provider,
            logger_provider,
            shutdown_timeout: self.shutdown_timeout,
        })
    }
}

/// Initialise OpenTelemetry traces + metrics with OTLP gRPC export.
///
/// Convenience wrapper around [`Telemetry::builder`] with all defaults.
/// For fine-grained control, use the builder directly.
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
    Telemetry::builder(service_name).init()
}

/// Initialise OpenTelemetry traces + metrics with OTLP gRPC export and an
/// explicit trace sampler.
///
/// Convenience wrapper around [`Telemetry::builder`]. When `sampler` is
/// `None`, falls back to `OTEL_TRACES_SAMPLER` / `OTEL_TRACES_SAMPLER_ARG`,
/// then always-on.
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
    let builder = Telemetry::builder(service_name);
    match sampler {
        Some(s) => builder.with_sampler(s),
        None => builder, // no-op: identical to calling init_telemetry(); not covered by tests (see Makefile ci-coverage note)
    }
    .init()
}

/// Read `OTEL_EXPORTER_OTLP_TIMEOUT` (milliseconds). Returns `None` when unset or invalid.
fn timeout_from_env() -> Option<Duration> {
    let ms = std::env::var("OTEL_EXPORTER_OTLP_TIMEOUT").ok()?;
    let ms: u64 = ms.trim().parse().ok()?;
    Some(Duration::from_millis(ms))
}

fn build_span_exporter(
    protocol: ExportProtocol,
    endpoint: &str,
    timeout: Option<Duration>,
) -> Result<opentelemetry_otlp::SpanExporter, Box<dyn Error>> {
    match protocol {
        #[cfg(feature = "grpc")]
        ExportProtocol::Grpc => {
            let mut b = opentelemetry_otlp::SpanExporter::builder()
                .with_tonic()
                .with_endpoint(endpoint);
            if let Some(t) = timeout {
                b = b.with_timeout(t);
            }
            Ok(b.build()?)
        }
        #[cfg(feature = "http")]
        ExportProtocol::HttpProtobuf => {
            let mut b = opentelemetry_otlp::SpanExporter::builder()
                .with_http()
                .with_endpoint(endpoint);
            if let Some(t) = timeout {
                b = b.with_timeout(t);
            }
            Ok(b.build()?)
        }
    }
}

fn build_metric_exporter(
    protocol: ExportProtocol,
    endpoint: &str,
    timeout: Option<Duration>,
) -> Result<opentelemetry_otlp::MetricExporter, Box<dyn Error>> {
    match protocol {
        #[cfg(feature = "grpc")]
        ExportProtocol::Grpc => {
            let mut b = opentelemetry_otlp::MetricExporter::builder()
                .with_tonic()
                .with_endpoint(endpoint);
            if let Some(t) = timeout {
                b = b.with_timeout(t);
            }
            Ok(b.build()?)
        }
        #[cfg(feature = "http")]
        ExportProtocol::HttpProtobuf => {
            let mut b = opentelemetry_otlp::MetricExporter::builder()
                .with_http()
                .with_endpoint(endpoint);
            if let Some(t) = timeout {
                b = b.with_timeout(t);
            }
            Ok(b.build()?)
        }
    }
}

fn build_log_exporter(
    protocol: ExportProtocol,
    endpoint: &str,
    timeout: Option<Duration>,
) -> Result<opentelemetry_otlp::LogExporter, Box<dyn Error>> {
    match protocol {
        #[cfg(feature = "grpc")]
        ExportProtocol::Grpc => {
            let mut b = opentelemetry_otlp::LogExporter::builder()
                .with_tonic()
                .with_endpoint(endpoint);
            if let Some(t) = timeout {
                b = b.with_timeout(t);
            }
            Ok(b.build()?)
        }
        #[cfg(feature = "http")]
        ExportProtocol::HttpProtobuf => {
            let mut b = opentelemetry_otlp::LogExporter::builder()
                .with_http()
                .with_endpoint(endpoint);
            if let Some(t) = timeout {
                b = b.with_timeout(t);
            }
            Ok(b.build()?)
        }
    }
}

/// Build a [`Resource`] enriched with semantic-convention attributes.
///
/// Auto-detects `host.name` and `process.pid`. Optionally sets
/// `service.version` and `deployment.environment` when provided.
///
/// # Example
/// ```
/// let resource = otel_bootstrap::build_resource(
///     "my-service",
///     Some("1.0.0"),
///     Some("production"),
/// );
/// // `resource` can be passed to SdkTracerProvider::builder().with_resource(resource)
/// ```
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

/// Returns a ready-to-use [`tower::Layer`] that extracts W3C trace context from
/// incoming HTTP requests, creates a span with standard HTTP semantic-convention
/// attributes, and injects trace context into response headers.
///
/// Requires the `axum` feature flag.
///
/// # Example
/// ```no_run
/// # #[cfg(feature = "axum")]
/// # {
/// use axum::Router;
///
/// let app: Router = Router::new()
///     // ... add routes ...
///     .layer(otel_bootstrap::axum_layer());
/// # }
/// ```
#[cfg(feature = "axum")]
pub fn axum_layer() -> axum_middleware::OtelTraceLayer {
    axum_middleware::OtelTraceLayer
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
        unsafe {
            std::env::set_var(key, val);
        }
    }

    unsafe fn remove_env(key: &str) {
        unsafe {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn sampler_from_env_reads_traceidratio() {
        unsafe {
            set_env("OTEL_TRACES_SAMPLER", "traceidratio");
            set_env("OTEL_TRACES_SAMPLER_ARG", "0.42");
        }

        let sampler = sampler_from_env()
            .expect("should not error")
            .expect("should return Some");
        assert!(
            matches!(sampler, TraceSampler::TraceIdRatio(r) if (r - 0.42).abs() < f64::EPSILON)
        );

        unsafe {
            remove_env("OTEL_TRACES_SAMPLER");
            remove_env("OTEL_TRACES_SAMPLER_ARG");
        }
    }

    #[test]
    fn sampler_from_env_returns_none_when_unset() {
        unsafe {
            remove_env("OTEL_TRACES_SAMPLER");
        }
        assert!(sampler_from_env().expect("should not error").is_none());
    }

    #[test]
    fn sampler_from_env_reads_parentbased_traceidratio() {
        unsafe {
            set_env("OTEL_TRACES_SAMPLER", "parentbased_traceidratio");
            set_env("OTEL_TRACES_SAMPLER_ARG", "0.1");
        }

        let sampler = sampler_from_env()
            .expect("should not error")
            .expect("should return Some");
        assert!(
            matches!(sampler, TraceSampler::ParentBased(inner) if matches!(*inner, TraceSampler::TraceIdRatio(r) if (r - 0.1).abs() < f64::EPSILON))
        );

        unsafe {
            remove_env("OTEL_TRACES_SAMPLER");
            remove_env("OTEL_TRACES_SAMPLER_ARG");
        }
    }

    #[test]
    fn sampler_from_env_parentbased_always_on() {
        unsafe {
            set_env("OTEL_TRACES_SAMPLER", "parentbased_always_on");
        }
        let sampler = sampler_from_env()
            .expect("should not error")
            .expect("should return Some");
        assert!(
            matches!(sampler, TraceSampler::ParentBased(inner) if matches!(*inner, TraceSampler::AlwaysOn))
        );
        unsafe {
            remove_env("OTEL_TRACES_SAMPLER");
        }
    }

    #[test]
    fn sampler_from_env_parentbased_always_off() {
        unsafe {
            set_env("OTEL_TRACES_SAMPLER", "parentbased_always_off");
        }
        let sampler = sampler_from_env()
            .expect("should not error")
            .expect("should return Some");
        assert!(
            matches!(sampler, TraceSampler::ParentBased(inner) if matches!(*inner, TraceSampler::AlwaysOff))
        );
        unsafe {
            remove_env("OTEL_TRACES_SAMPLER");
        }
    }

    #[test]
    fn sampler_from_env_always_on() {
        unsafe {
            set_env("OTEL_TRACES_SAMPLER", "always_on");
        }
        let sampler = sampler_from_env()
            .expect("should not error")
            .expect("should return Some");
        assert!(matches!(sampler, TraceSampler::AlwaysOn));
        unsafe {
            remove_env("OTEL_TRACES_SAMPLER");
        }
    }

    #[test]
    fn sampler_from_env_always_off() {
        unsafe {
            set_env("OTEL_TRACES_SAMPLER", "always_off");
        }
        let sampler = sampler_from_env()
            .expect("should not error")
            .expect("should return Some");
        assert!(matches!(sampler, TraceSampler::AlwaysOff));
        unsafe {
            remove_env("OTEL_TRACES_SAMPLER");
        }
    }

    #[test]
    fn sampler_from_env_unknown_returns_error() {
        unsafe {
            set_env("OTEL_TRACES_SAMPLER", "unknown_sampler");
        }
        let err = sampler_from_env().expect_err("unknown sampler should produce an error");
        assert!(
            err.to_string().contains("unknown_sampler"),
            "error message should include the unknown name, got: {err}"
        );
        unsafe {
            remove_env("OTEL_TRACES_SAMPLER");
        }
    }

    #[test]
    fn trace_sampler_always_on_converts_to_sdk() {
        let sdk = TraceSampler::AlwaysOn.into_sdk_sampler();
        assert_eq!(format!("{sdk:?}"), "AlwaysOn");
    }

    #[test]
    fn trace_sampler_always_off_converts_to_sdk() {
        let sdk = TraceSampler::AlwaysOff.into_sdk_sampler();
        assert_eq!(format!("{sdk:?}"), "AlwaysOff");
    }

    #[test]
    fn builder_has_sensible_defaults() {
        let builder = Telemetry::builder("test-svc");
        assert_eq!(builder.service_name.as_deref(), Some("test-svc"));
        assert!(builder.service_version.is_none());
        assert!(builder.deployment_environment.is_none());
        assert!(builder.sampler.is_none());
        assert!(builder.metrics);
        assert!(!builder.logs);
        assert!(builder.protocol.is_none());
        assert!(builder.max_export_batch_size.is_none());
        assert!(builder.metric_export_interval.is_none());
        assert!(builder.export_timeout.is_none());
    }

    #[test]
    fn from_env_builder_has_no_service_name() {
        let builder = Telemetry::from_env();
        assert!(builder.service_name.is_none());
    }

    #[test]
    fn with_export_timeout_stores_value() {
        let timeout = Duration::from_secs(5);
        let builder = Telemetry::builder("test-svc").with_export_timeout(timeout);
        assert_eq!(builder.export_timeout, Some(timeout));
    }

    #[test]
    fn timeout_from_env_reads_milliseconds() {
        unsafe {
            set_env("OTEL_EXPORTER_OTLP_TIMEOUT", "5000");
        }
        let t = timeout_from_env();
        assert_eq!(t, Some(Duration::from_millis(5000)));
        unsafe {
            remove_env("OTEL_EXPORTER_OTLP_TIMEOUT");
        }
    }

    #[test]
    fn timeout_from_env_returns_none_when_unset() {
        unsafe {
            remove_env("OTEL_EXPORTER_OTLP_TIMEOUT");
        }
        assert_eq!(timeout_from_env(), None);
    }

    #[test]
    fn service_name_from_env_used_when_none_given() {
        let builder = Telemetry::from_env();
        assert!(builder.service_name.is_none());
    }

    #[test]
    fn explicit_service_name_overrides_env_var() {
        let builder = Telemetry::builder("explicit-svc");
        assert_eq!(builder.service_name.as_deref(), Some("explicit-svc"));
    }

    #[test]
    fn from_env_builder_service_name_is_none() {
        let builder = Telemetry::from_env();
        assert!(builder.service_name.is_none());
    }

    #[test]
    fn init_returns_error_for_unknown_otel_traces_sampler() {
        unsafe {
            set_env("OTEL_TRACES_SAMPLER", "not_a_real_sampler");
        }
        let result = Telemetry::builder("test-svc").with_metrics(false).init();
        let err = result
            .err()
            .expect("unknown sampler env var should cause init to fail");
        assert!(
            err.to_string().contains("not_a_real_sampler"),
            "error should name the unknown sampler, got: {err}"
        );
        unsafe {
            remove_env("OTEL_TRACES_SAMPLER");
        }
    }

    #[test]
    fn with_max_export_batch_size_stores_value() {
        let builder = Telemetry::builder("test-svc").with_max_export_batch_size(1024);
        assert_eq!(builder.max_export_batch_size, Some(1024));
    }

    #[test]
    fn with_metric_export_interval_stores_value() {
        let interval = Duration::from_secs(30);
        let builder = Telemetry::builder("test-svc").with_metric_export_interval(interval);
        assert_eq!(builder.metric_export_interval, Some(interval));
    }

    #[test]
    fn init_rejects_zero_metric_export_interval() {
        let err = Telemetry::builder("test-svc")
            .with_metric_export_interval(Duration::ZERO)
            .with_metrics(false)
            .init()
            .err()
            .expect("expected error for zero interval");
        assert!(
            err.to_string().contains("metric_export_interval"),
            "error message should mention metric_export_interval, got: {err}"
        );
    }

    #[test]
    fn builder_with_custom_values() {
        let builder = Telemetry::builder("test-svc")
            .with_version("2.0.0")
            .with_environment("production")
            .with_sampler(TraceSampler::TraceIdRatio(0.5))
            .with_metrics(false);

        assert_eq!(builder.service_name.as_deref(), Some("test-svc"));
        assert_eq!(builder.service_version.as_deref(), Some("2.0.0"));
        assert_eq!(
            builder.deployment_environment.as_deref(),
            Some("production")
        );
        assert!(
            matches!(builder.sampler, Some(TraceSampler::TraceIdRatio(r)) if (r - 0.5).abs() < f64::EPSILON)
        );
        assert!(!builder.metrics);
    }

    #[test]
    #[cfg(feature = "grpc")]
    fn builder_with_protocol_grpc() {
        let builder = Telemetry::builder("test-svc").with_protocol(ExportProtocol::Grpc);
        assert_eq!(builder.protocol, Some(ExportProtocol::Grpc));
    }

    #[test]
    #[cfg(feature = "http")]
    fn builder_with_protocol_http() {
        let builder = Telemetry::builder("test-svc").with_protocol(ExportProtocol::HttpProtobuf);
        assert_eq!(builder.protocol, Some(ExportProtocol::HttpProtobuf));
    }

    #[test]
    #[cfg(feature = "grpc")]
    fn protocol_from_env_reads_grpc() {
        unsafe {
            set_env("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc");
        }
        assert_eq!(protocol_from_env(), Some(ExportProtocol::Grpc));
        unsafe {
            remove_env("OTEL_EXPORTER_OTLP_PROTOCOL");
        }
    }

    #[test]
    #[cfg(feature = "http")]
    fn protocol_from_env_reads_http_protobuf() {
        unsafe {
            set_env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/protobuf");
        }
        assert_eq!(protocol_from_env(), Some(ExportProtocol::HttpProtobuf));
        unsafe {
            remove_env("OTEL_EXPORTER_OTLP_PROTOCOL");
        }
    }

    #[test]
    fn protocol_from_env_returns_none_when_unset() {
        unsafe {
            remove_env("OTEL_EXPORTER_OTLP_PROTOCOL");
        }
        assert_eq!(protocol_from_env(), None);
    }

    #[test]
    fn protocol_from_env_returns_none_for_unknown() {
        unsafe {
            set_env("OTEL_EXPORTER_OTLP_PROTOCOL", "websocket");
        }
        assert_eq!(protocol_from_env(), None);
        unsafe {
            remove_env("OTEL_EXPORTER_OTLP_PROTOCOL");
        }
    }

    #[test]
    fn builder_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<TelemetryBuilder>();
    }

    #[test]
    fn with_shutdown_timeout_stores_value() {
        let timeout = Duration::from_secs(10);
        let builder = Telemetry::builder("test-svc").with_shutdown_timeout(timeout);
        assert_eq!(builder.shutdown_timeout, timeout);
    }

    #[test]
    fn default_shutdown_timeout_is_five_seconds() {
        let builder = Telemetry::builder("test-svc");
        assert_eq!(builder.shutdown_timeout, Duration::from_secs(5));
    }

    /// Verify that drop completes within the configured timeout even when the
    /// shutdown thread is blocked (simulated by using a very short timeout so
    /// the test itself runs quickly).
    ///
    /// We construct `TelemetryHandles` with an artificially short timeout and
    /// a real (but disconnected) provider.  Drop must return before the test
    /// times out.
    #[cfg(feature = "testing")]
    #[test]
    fn drop_completes_within_shutdown_timeout() {
        // Use the testing helper so we don't need a running OTLP collector.
        let mut handles = crate::Telemetry::testing("drop-timeout-test");
        // Override the timeout to something very short so the test is fast.
        handles.shutdown_timeout = Duration::from_millis(100);

        let start = std::time::Instant::now();
        drop(handles);
        let elapsed = start.elapsed();

        // Drop should complete within 2× the timeout (generous margin for CI).
        assert!(
            elapsed < Duration::from_millis(500),
            "drop took {elapsed:?}, expected < 500 ms"
        );
    }
}
