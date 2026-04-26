# otel-bootstrap

[![CI](https://github.com/brefwiz/otel-bootstrap/actions/workflows/ci.yml/badge.svg)](https://github.com/brefwiz/otel-bootstrap/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/otel-bootstrap.svg)](https://crates.io/crates/otel-bootstrap)
[![docs.rs](https://docs.rs/otel-bootstrap/badge.svg)](https://docs.rs/otel-bootstrap)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust 1.85+](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org)

One-call OpenTelemetry bootstrap for Rust services — traces, metrics, and logs over OTLP with sensible defaults.

The standard `opentelemetry` + `opentelemetry-otlp` + `tracing-subscriber` wiring is the same in every service. This crate does it once: call `init_telemetry("my-service")`, keep the handle alive, and drop it to flush.

## Features

- **One call to wire it all up** — traces, metrics, and logs over OTLP, `tracing-subscriber` configured, W3C TraceContext + Baggage propagators registered.
- **gRPC or HTTP/protobuf** — select transport via cargo feature.
- **Env-var configuration** — follows the OpenTelemetry spec (`OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_SERVICE_NAME`, `OTEL_TRACES_SAMPLER`, …).
- **Builder API** — version, environment, sampler, custom meter setup, extra subscriber layers.
- **Optional axum middleware** — `OtelTraceLayer` for inbound HTTP trace propagation.
- **Optional `enduser.*` span enrichment** — emit the four canonical `enduser.*` attributes from an `api-bones::OrganizationContext` on every span.
- **Graceful shutdown** — drop `TelemetryHandles` to flush and shut down both providers.

## Quick start

```toml
[dependencies]
otel-bootstrap = "0.4"
tokio = { version = "1", features = ["full"] }
```

```rust
use otel_bootstrap::init_telemetry;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _telemetry = init_telemetry("my-service")?;

    tracing::info!("service started");
    // ... run your server ...
    Ok(())
    // _telemetry dropped here → SDK flushes and shuts down
}
```

For more control, use the builder:

```rust
use otel_bootstrap::Telemetry;

let _telemetry = Telemetry::builder("my-service")
    .version(env!("CARGO_PKG_VERSION"))
    .environment("production")
    .init()?;
```

## Cargo features

| Feature       | Default | Description |
|---------------|---------|-------------|
| `grpc`        | ✅      | OTLP/gRPC transport via `tonic` |
| `http`        | ❌      | OTLP/HTTP-protobuf transport via `reqwest` |
| `axum`        | ❌      | `OtelTraceLayer` for axum servers |
| `org-context` | ❌      | `enduser.*` span enrichment from `api-bones::OrganizationContext` |
| `testing`     | ❌      | In-memory exporters for unit tests |

At least one of `grpc` or `http` must be enabled (enforced at compile time).

## Configuration

| Variable | Default |
|---|---|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | `http://localhost:4317` (gRPC) / `http://localhost:4318` (HTTP) |
| `OTEL_EXPORTER_OTLP_PROTOCOL` | `grpc` |
| `OTEL_EXPORTER_OTLP_TIMEOUT` | `10000` ms |
| `OTEL_SERVICE_NAME` | overridden by the `service_name` argument |
| `OTEL_TRACES_SAMPLER` | `parentbased_always_on` |
| `OTEL_TRACES_SAMPLER_ARG` | sampler-specific (e.g. ratio for `traceidratio`) |

Full reference: [`OTEL_TRACES_SAMPLER` values](https://opentelemetry.io/docs/concepts/sdk-configuration/general-sdk-configuration/#otel_traces_sampler).

## Axum middleware

```toml
otel-bootstrap = { version = "0.4", features = ["axum"] }
```

```rust
use otel_bootstrap::axum_layer;

let app = Router::new()
    .route("/", get(handler))
    .layer(axum_layer());
```

This layer reads the `traceparent` / `tracestate` headers, starts a server span, and propagates context to all child spans.

## `enduser.*` span enrichment

```toml
otel-bootstrap = { version = "0.4", features = ["axum", "org-context"] }
```

```rust
use otel_bootstrap::org_context_span_enricher_layer;

let app = Router::new()
    .route("/", get(handler))
    .layer(org_context_span_enricher_layer());
```

Reads `OrganizationContext` from axum request extensions and records `enduser.id`, `enduser.org_id`, `enduser.org_path`, and `enduser.principal_kind` on the active span. Routes without a context are silently skipped.

## Examples

- [`basic_setup`](examples/basic_setup.rs) — minimal init
- [`shutdown_handling`](examples/shutdown_handling.rs) — explicit graceful flush
- [`custom_config`](examples/custom_config.rs) — builder API with version, environment, sampler
- [`axum_org_context`](examples/axum_org_context.rs) — axum + `enduser.*` enrichment

## License

MIT — see [LICENSE](LICENSE).
