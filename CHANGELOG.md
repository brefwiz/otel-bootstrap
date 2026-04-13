# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Meter provider escape hatch** — `TelemetryBuilder::with_meter_provider_setup` lets callers customise the in-progress `MeterProviderBuilder` (e.g. attach an `opentelemetry-prometheus` reader alongside the built-in OTLP `PeriodicReader`) without forking the metrics wiring. Enables `/metrics` scrape endpoints to coexist with OTLP push.

## [0.2.0] — 2026-04-07

### Added

- **Axum middleware** — tower middleware for automatic W3C TraceContext propagation on incoming HTTP requests (`feat(axum): add tower middleware for W3C trace context propagation`, `50b5efa`)
- **Custom layer injection** — allow callers to inject additional `tracing-subscriber` layers via `TelemetryBuilder::with_layer` (`feat(otel-bootstrap): allow custom tracing-subscriber layer injection`, `5d8d597`)
- **Configurable shutdown timeout** — `TelemetryHandles::shutdown` now accepts a `Duration` to bound graceful flush (`feat(otel-bootstrap): add configurable shutdown timeout to TelemetryHandles`, `4c655a2`)
- **Standard OTEL env vars** — respect `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_SERVICE_NAME`, and friends at runtime (`feat(otel-bootstrap): respect standard OTEL environment variables`, `c182cdb`)
- **No-op testing mode** — `Telemetry::testing()` returns a no-op handle suitable for unit tests without a live collector (`feat(testing): add no-op testing mode via Telemetry::testing()`, `ffdd6e0`)
- **Batch size & metric interval tuning** — expose `TelemetryBuilder::with_batch_size` and `with_metric_interval` (`feat(otel-bootstrap): expose batch size and metric interval tuning`, `59d7c49`)
- **HTTP/protobuf export protocol** — optional `http-proto` feature flag to export via HTTP/protobuf instead of gRPC (`feat(otel-bootstrap): support HTTP/protobuf export protocol via feature flags`, `d5aa862`)
- **Logs pillar** — wire up the OpenTelemetry logs pillar via OTLP log bridge (`feat(otel-bootstrap): wire up logs pillar via OTLP log bridge`, `46292d2`)
- **Global meter provider registration** — `MeterProvider` is now registered as the global meter provider (`feat(otel-bootstrap): register meter provider globally`, `c545d7d`)
- **Builder pattern** — replace the `init` function with a fluent `TelemetryBuilder` (`feat(otel-bootstrap): replace init function with builder pattern`, `ca532f4`)
- **Trace sampler configuration** — configure the trace sampler (always-on, always-off, ratio-based) via builder (`feat(otel-bootstrap): add trace sampler configuration`, `65393b9`)
- **W3C TraceContext propagator** — register the W3C TraceContext propagator globally (`feat(otel-bootstrap): register W3C TraceContext propagator`, `d811163`)
- **Resource enrichment** — enrich the OTLP resource with semantic conventions (service name, version, host, etc.) (`feat(otel-bootstrap): add resource enrichment with semantic conventions`, `2da8080`)
- **CI pipeline & coverage gate** — Makefile, Gitea CI pipeline, and 100 % line-coverage gate (`ci(otel-bootstrap): add Makefile, CI pipeline, and 100% coverage gate`, `ee83852`)

### Fixed

- Merged `Timeout` and `Disconnected` shutdown arms to close a coverage gap (`fix(otel-bootstrap): merge shutdown Timeout/Disconnected arms to close coverage gap`, `d5044f5`)
- Formatting and 100 % coverage for trace sampler (`fix(otel-bootstrap): fix formatting and add 100% coverage for sampler`, `c33f92b`)
- Removed Docker dependency from E2E test binary (`fix(e2e): remove docker dependency from test binary`, `5690d86`)
- Switched coverage gate to `--fail-uncovered-lines 1` (`fix(ci): switch coverage gate to --fail-uncovered-lines 1`, `1c1267f`)

[Unreleased]: https://git.brefwiz.com/brefwiz/otel-bootstrap/compare/v0.2.0...HEAD
[0.2.0]: https://git.brefwiz.com/brefwiz/otel-bootstrap/releases/tag/v0.2.0
