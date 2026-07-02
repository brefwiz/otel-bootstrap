# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [2.5.0] — 2026-07-02

### Added

- **`profiling` feature (off by default)** — continuous CPU/alloc profiling as a fourth OTLP-adjacent signal, via a `pyroscope-rs` direct-push bridge behind the distinct `profiling-bridge-pyroscope-rs` sub-feature (never co-equal with the eventual OTLP-profiles path). Every profile pushed via the bridge is tagged with the active span's `trace_id`/`span_id` for cross-linking in Grafana. `TelemetryBuilder::with_profiling(endpoint)` starts the bridge; disabled builds compile it out entirely (`cargo tree -e features` shows `pyroscope` only when the sub-feature is enabled).
- **SPIFFE-mTLS transport via loopback sidecar (ADR platform/0203, platform/0205)** — `pyroscope-rs` hardcodes its own HTTP client with no hook for custom TLS/client-cert injection, so the bridge pushes plaintext to a local sidecar (never a routable address) which holds the workload's SPIFFE SVID and forwards over mTLS to the real Pyroscope backend. See `examples/telemetry_profiling.rs`.
- Tracking issue `chore(profiling): remove pyroscope-rs bridge once Rust OTLP profiles exporter ships` (otel-bootstrap#40) stays open until `opentelemetry-rust` ships an OTLP profiles exporter — this bridge is a tracked, sunset-bound exception per platform/0202, not a permanent second export plane.

## [2.4.0] — 2026-07-01

### Fixed

- **`GrpcClientTraceService` / `GrpcServerTraceService` / `OtelTraceService`: panic on inner services that track readiness per-handle.** `call()` cloned `self.inner` and fired the request on the fresh clone instead of the handle `poll_ready` was called on, violating the tower `Service` contract. `tonic::transport::Channel` wraps a `tower::buffer::Buffer` internally, which enforces this per-handle — firing on an unpolled clone panicked with `"send_item called without first calling poll_reserve"`. Fixed with the standard `mem::replace` swap (clone for next time, fire on the already-ready handle).

## [2.3.0] — 2026-07-01

### Added

- **`tonic-tracing` feature** — `grpc_client_layer()` / `grpc_server_layer()`, tower `Layer`s that propagate W3C trace context (`traceparent`) over raw tonic gRPC clients/servers, mirroring the existing `axum_layer()` for HTTP. For services that hand-roll a tonic `Channel`/`Server` instead of going through an axum router.

## [2.2.0] — 2026-06-30

### Changed

- **Dependency bump** — coordinated opentelemetry ecosystem upgrade: `opentelemetry` + `opentelemetry_sdk` + `opentelemetry-otlp` + `opentelemetry-appender-tracing` + `opentelemetry-semantic-conventions` `0.31` → `0.32`; `tracing-opentelemetry` `0.32` → `0.33`.

### Added

- **`SpanAwareLogBridge`** — replaces `opentelemetry_appender_tracing::OpenTelemetryTracingBridge`. Propagates span-level fields and trace/span context into every OTLP log record. Two capture paths: (1) tracing-native fields declared at `info_span!` creation time (via `FieldCollector`); (2) fields written post-creation via `record_span_log_attr_on` (via `SpanLogAttrs` span extension).
- **`SpanLogAttrs` span extension** — stores key-value pairs attached to a span after creation. Populated via `record_span_log_attr_on`; replayed onto log records by `SpanAwareLogBridge`.
- **`record_span_log_attr(key, value)`** — write a log-propagation attribute on the current span from any non-Layer context (middleware, enrichers).
- **`record_span_log_attr_on(span, key, value)`** — same, targeting an explicit span.
- **`PROPAGATED_SPAN_FIELDS`** — default slice of field names captured at span creation and replayed into log records: `request.id`, `enduser.*`, and common `http.*` fields.
- **`TelemetryBuilder::with_propagated_span_fields`** — override the default field set per service.
- **`span_enrichment::emit_request_id(id)`** and `emit_request_id_on(span, id)` — dual-write `request.id` to the OTLP trace attribute and `SpanLogAttrs` so it surfaces in both Tempo and Loki.
- **`span_enrichment::REQUEST_ID`** — canonical `"request.id"` constant.

## [2.1.2] — 2026-05-21

### Fixed

- **Empty `extra_layers` silences all tracing** — `Vec<L>::register_callsite()` on an empty Vec returns `Interest::never()`, which `Layered::pick_interest()` propagates through the entire subscriber chain when `outer=Always, inner=Never, inner_has_layer_filter=false`. Globally disabled all tracing callsites, producing zero stdout/stderr/OTLP output. Fixed by wrapping `extra_layers` in `Option`: `None` returns `Interest::always()` and is a transparent no-op.

## [2.1.1] — 2026-05-20

### Fixed

- **Warn on subscriber clobber** — `registry.try_init().ok()` silently discarded errors when a global tracing subscriber was already installed. This masked the root cause of OTLP logs never reaching Loki in production. Both branches (with and without `logger_provider`) now `eprintln!` a clear message so the failure is visible in container logs.

## [2.1.0] — 2026-05-19

### Added

- **`grpc-mtls` feature flag** — opt-in mTLS for the gRPC OTLP exporter. Implies `grpc`; enables `opentelemetry-otlp/{tls,tls-roots}` and `tonic/tls-native-roots`.
- **`MtlsMaterial` struct** — carries PEM-encoded client cert chain, client key, and trust bundle. `Debug` impl redacts contents.
- **`TelemetryBuilder::with_mtls(MtlsMaterial)`** — installs the material on all three (span/metric/log) exporters via `opentelemetry-otlp::WithTonicConfig::with_tls_config`. Pins `ExportProtocol::Grpc` regardless of `OTEL_EXPORTER_OTLP_PROTOCOL` so misconfig can't silently downgrade to plaintext.

### Notes

- Caller-side helpers for converting a SPIFFE `SvidWatcher` SVID into `MtlsMaterial` live in service-kit (`spiffe-client` + `service-kit/spiffe_otlp`), keeping otel-bootstrap free of brefwiz-specific SDK dependencies.

### Known follow-ups (next minor)

- **In-process cert rotation.** Material is read once at `init()`; the tonic Channel is built once and reused for the lifetime of the process. Mitigation: issue long-lived (≥365 days) client certs so manual rotation is infrequent; natural pod restarts (deploys, reschedules) re-read the SVID at `init()`. Live rotation watcher is the next milestone — see open issue. The shape will be a `CertSource` trait with a `next_rotation()` async hook; otel-bootstrap will rebuild OTLP providers + swap globals on rotation. Design intentionally deferred so v1 ships behind a small, reviewable surface.

### Changed (build infra)

- `Makefile` `ci-lint` now uses `--all-features --all-targets` so feature-gated paths are clippy-checked.
- `Makefile` `ci-test` enumerates concrete features (`grpc,http,axum,testing,grpc-mtls`); the `integration-tests` feature (which needs a live :4317 collector) is opt-in.

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

[Unreleased]: https://github.com/brefwiz/otel-bootstrap/compare/v2.2.0...HEAD
[2.2.0]: https://github.com/brefwiz/otel-bootstrap/compare/v2.1.2...v2.2.0
[2.0.0]: https://github.com/brefwiz/otel-bootstrap/compare/v1.0.0...v2.0.0
[1.0.0]: https://github.com/brefwiz/otel-bootstrap/compare/v0.4.0...v1.0.0
[0.4.0]: https://github.com/brefwiz/otel-bootstrap/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/brefwiz/otel-bootstrap/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/brefwiz/otel-bootstrap/compare/v0.2.3...v0.3.0
[0.2.3]: https://github.com/brefwiz/otel-bootstrap/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/brefwiz/otel-bootstrap/compare/v0.2.0...v0.2.2
[0.2.0]: https://github.com/brefwiz/otel-bootstrap/releases/tag/v0.2.0
