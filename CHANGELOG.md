# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [2.0.0] — 2026-05-13

### Changed

- **`SpanEnricherLayer<T>`** — replaces `OrgContextSpanEnricher`; accepts any `T: EnrichSpan + Clone + Send + Sync + 'static` instead of being hardwired to `quorum-identity`.
- **`span_enricher_layer::<T>()`** — replaces `org_context_span_enricher_layer()`; same axum tower layer, now generic.
- **`span_enrichment` module unconditional** — no longer gated on the `org-context` feature flag; the module ships in all builds.

### Removed

- **`org-context` feature flag** — dropped; `span_enrichment` is always available.
- **`quorum-identity` dependency** — removed entirely; callers implement `EnrichSpan` on their own context type.

### Added

- **`EnrichSpan` trait** — implement this on any type to drive `SpanEnricherLayer<T>` without coupling to brefwiz-internal crates.

## [1.0.0] — 2026-05-05

### Added

- **Quorum identity integration** — `span_enrichment` module supports enriching spans with `quorum_identity` context via new `OrganizationContext` type. Includes new axum middleware layer for automatic enduser span attribute population from request extensions. Fixes #57.

## [0.4.0] — 2026-04-25

### Changed

- **License** — `LicenseRef-Proprietary` → MIT.
- **Repository** — moved from `git.brefwiz.com` to `github.com/brefwiz/otel-bootstrap`.
- **`span_enrichment` doc comment** — generalized language; removed internal ADR references.

### Added

- `README.md`, `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `SECURITY.md`, `LICENSE` — standard open-source packaging.

## [0.3.1] — 2026-04-21

### Changed

- Bumped `api-bones` dependency `3.1` → `4.0.1` (now sourced from crates.io).

## [0.3.0] — 2026-04-21

### Added

- **`enduser.*` span enrichment** — new `span_enrichment` module behind the `org-context` feature flag emits the four canonical attributes mandated by ADR platform/0015 (amending platform/0010): `enduser.id`, `enduser.org_id`, `enduser.org_path` (typed array of UUID strings, root-first — not a joined string), and `enduser.principal_kind`. `emit_enduser_fields(&ctx)` is the single helper shared by HTTP, NATS, and job-worker entry points. A tower layer for axum (`otel_bootstrap::org_context_span_enricher_layer`, gated on `axum + org-context`) reads `OrganizationContext` from request extensions and records the attributes on the active tracing span; missing-context requests (platform-scope routes) are a no-op with a single `warn!` per process. Fixes #49.

### Changed

- **Dependencies** — `tokio` → 1.51.1, `axum` → 0.8.9 (batch Renovate update).

## [0.2.3] — 2026-04-07

### Changed

- **`tracing-opentelemetry`** bumped from `0.30` to `0.32`.

## [0.2.2] — 2026-04-07

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

[Unreleased]: https://github.com/brefwiz/otel-bootstrap/compare/v2.0.0...HEAD
[2.0.0]: https://github.com/brefwiz/otel-bootstrap/compare/v1.0.0...v2.0.0
[1.0.0]: https://github.com/brefwiz/otel-bootstrap/compare/v0.4.0...v1.0.0
[0.4.0]: https://github.com/brefwiz/otel-bootstrap/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/brefwiz/otel-bootstrap/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/brefwiz/otel-bootstrap/compare/v0.2.3...v0.3.0
[0.2.3]: https://github.com/brefwiz/otel-bootstrap/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/brefwiz/otel-bootstrap/compare/v0.2.0...v0.2.2
[0.2.0]: https://github.com/brefwiz/otel-bootstrap/releases/tag/v0.2.0
