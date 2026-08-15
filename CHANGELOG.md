# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [2.15.0] — 2026-08-14

### Fixed

- The profiling bridge no longer leaks memory, and no longer throws away the
  profiles it collects. `ProfilingTagLayer` tagged the pyroscope agent with
  `trace_id`/`span_id` on every span enter and exit, and in pyroscope's `pprof`
  backend both `add_tag` and `remove_tag` call `dump_report` — the backend's
  own comment calls it a "workaround for pprof-rs to interrupt the profiler".
  Each call symbolises every entry in the sample collector and then clears it.

  So each span enter cost two full profile rebuilds and each exit two more.
  Measured on static musl at the shipped 100 Hz sample rate: **868,726 rebuilds
  in the first 10 seconds**, 25.8 million over 300 seconds, against 30 uploads.

  Two consequences, and the second is why this was not a tuning problem:

  1. The symbolisation churn grew RSS without bound — 7.4 MiB/h with tagging
     against 1.0 MiB/h without, over 90-minute static-musl runs. brefwiz-spiffe
     saw 6.5 MiB/h in prod and 5.3 MiB/h in staging, and was OOM-killed against
     its 512Mi cgroup roughly every 5.5 hours, on both replicas, in both
     environments, for twelve days (~64 restarts in prod, ~56 in staging). The
     rate tracked span activity rather than request volume, which is why two
     environments at very different traffic levels leaked at nearly the same
     rate.
  2. The collector was cleared far faster than the sampler could fill it, so
     every upload carried an essentially empty profile — `collector_entries=0`
     at every single upload under load. The correlation feature destroyed the
     data it annotated.

  Sampling itself was never implicated: with tagging off, 63,254 samples over
  420 seconds moved RSS by 0.03 MiB/h.

  There is no bounded form of this on the `pprof` backend. The cost is per tag
  call, so sampling spans or tagging only roots still buys full profile
  rebuilds at a fraction of the span rate, and each rebuild still truncates the
  sample window. Trace/profile correlation returns with the native OTLP
  profiles exporter this bridge is sunset-bound against (ADR platform/0202).

### Added

- A guard against reintroducing per-span tagging, plus `rss-probe` and
  `make ci-rss-probe-musl` as a memory smoke test on the shipped target.

  The guard is a source check — `src/profiling.rs` must not call
  `tag_wrapper()`, and `ProfilingTagLayer` must not regain span callbacks — and
  that is a deliberate retreat from trying to assert it behaviourally. Through
  the real `Telemetry` path the defect is close to externally unobservable on
  release static musl. All four of these were measured, and all four passed on
  the broken build:

  | signal | fixed | broken |
  |---|---|---|
  | upload size | 823 B/profile | 828 B/profile |
  | attributable RSS | 5.09 MiB/h | 1.49 MiB/h (lower!) |
  | span throughput | 14,499/s | 14,518/s |
  | per-span labels on the wire | 0 | 0 |

  RSS runs *higher* on the healthy build because the broken one cleared the
  collector faster than the sampler could fill it, so it had less to symbolise.
  And the `trace_id`/`span_id` labels never reach the wire at all: `add_tag`
  pays for a full `dump_report` and the tag then fails to survive into the
  encoded profile — all cost, no signal. What is unambiguous is the call that
  arms it, so that is what is pinned. It fails on the reinstated defect and
  passes on the fix, in milliseconds.

  `rss-probe` remains as a memory smoke test: it drives spans against a live
  agent on static musl and bounds RSS growth against a no-profiling control. It
  is what established that the fixed bridge holds ~33 MB flat (0.39 MiB/h over
  55 minutes), but it is not the tagging gate and its RSS bound is deliberately
  coarse.

  Both run in a musl container for the same reason `ci-heap-probe-musl` does:
  every number here is dominated by symbolisation, and the same fixed code
  measures ~800 MiB/h on a macOS host, ~110 on a dev-profile musl build, and
  single digits on release static musl. Only the last is about production.

  One trap worth recording, since it cost a full round of measurement: the tag
  path only fires when `opentelemetry::Context::current()` carries a valid span
  context. A bare `info_span!` does not attach one, so a probe built that way
  leaves the defect dormant and scores both builds identically.

### Deprecated

- `profiling::ProfilingTagLayer` is inert and deprecated. It is still exported
  and still registered in the subscriber stack, so the public surface and the
  subscriber type are unchanged; it does nothing. Removed in the next major.

## [2.14.1] — 2026-08-03

### Fixed

- Missing musl libunwind is now a build failure instead of a silent one. The
  build script emitted `cargo:warning` and carried on, but cargo SUPPRESSES
  warnings from dependency build scripts — so the one message explaining the
  problem was invisible to every consumer, and the build produced a binary that
  segfaulted on its first sampled allocation.

  That is not hypothetical. brefwiz-spiffe hit it a fourth time after the fix
  had shipped: its CI image predated the libunwind rollout, this branch was
  taken silently, and the gate caught a SIGSEGV that took an image-provenance
  investigation to explain. The warning would have said so immediately if
  anyone could have seen it.

  Enabling heap profiling for musl without a usable libunwind is a broken
  binary, not a degraded mode, so the build now refuses — naming the target,
  every directory searched, and the three ways to resolve it.

## [2.14.0] — 2026-08-03

### Fixed

- Heap profiling now works on statically linked musl, the target every
  consuming service ships. 2.13.0 fixed the activation panic but the
  underlying link was still broken: jemalloc built with
  `--enable-prof-libunwind` calls `unw_backtrace`, and on static musl that
  needs more than `-lunwind`.

  Three things, each found by a gate rather than reasoning. rustc bundles its
  own LLVM libunwind for musl and it defines the same `_Unwind_*` symbols as
  the system nongnu libunwind, so linking the system archive wholesale
  collides on every one. `unw_backtrace` lives in the generic `libunwind.a`,
  not the arch-specific one, and is demand-loaded — by the time the linker
  reaches a `-l` appended at the end of the line, jemalloc's reference from an
  earlier rlib has already gone unsatisfied. And the system libunwind is not
  self-contained: built with minidebuginfo it needs liblzma and libz at static
  link time, which a dynamic link hides behind DT_NEEDED.

  A new build script derives all of it. It extracts the single archive member
  defining `unw_backtrace`, republishes it as its own static library, and
  emits `rustc-link-lib`/`rustc-link-search` — which propagate to the final
  binary link, unlike raw link args. Consumers enabling
  `profiling-memory-jemalloc` get a working link with no build changes, no
  flags, and no knowledge of any of the above. Non-musl targets are untouched.

### Added

- `heap-probe` binary and `profiling-memory-probe` feature: a real-process
  gate on the shipped target. brefwiz-spiffe shipped heap profiling broken in
  0.48.0, 0.49.0 and 0.49.1 with green CI every time, because the failure is a
  property of the process — jemalloc as global allocator, `_RJEM_MALLOC_CONF`
  before `main`, activation inside a runtime, static musl — and no library
  test binary has any of those.

  It asserts a non-empty profile rather than a clean exit: 0.49.1 exited
  cleanly and produced nothing at all. The same probe passes on glibc and on
  aarch64 musl, so only x86_64 musl reproduces the crash — which is exactly
  the combination nothing tested.

## [2.13.0] — 2026-08-03

### Fixed

- Heap profiling never armed. 2.12.0 activated jemalloc sampling with
  `blocking_lock()` on a `tokio::sync::Mutex`, and every caller reaches that
  path from inside a runtime — `with_profiling()` runs during service
  bootstrap. It panicked with "Cannot block the current thread from within a
  runtime". The panic was caught, so the service booted, served traffic and
  looked healthy while producing no heap profiles at all. Now `try_lock`,
  which is correct rather than merely panic-free: activation happens once at
  startup with nothing else holding the lock.

### Added

- `heap-probe` binary and `profiling-memory-probe` feature: a real-process
  gate for heap profiling, run by `tests/heap_profiling_probe.rs`.

  Two consecutive releases shipped broken heap profiling and both were caught
  in staging, because what breaks is a property of the *process* — jemalloc
  installed as the global allocator, `_RJEM_MALLOC_CONF` present before
  `main`, activation attempted from inside a runtime — and a `cargo test`
  binary has none of the three. The probe is a real binary run as a child
  process under each `_RJEM_MALLOC_CONF` shape.

  It asserts a **non-empty profile**, not a clean exit. A clean exit is
  exactly what the 2.12.0 build produced. Verified by reinstating
  `blocking_lock()`: the gate fails with `HEAP_PROBE_PANIC` and "heap
  profiling did not produce a profile".

  Also covers the 2.11.x defect — `prof_active:true` arming before `main`,
  which segfaulted on x86_64 static musl — by failing if the process dies by
  signal.

## [2.12.0] — 2026-08-03

### Fixed

- Heap profiling now arms sampling from Rust after startup instead of requiring
  `prof_active:true` in `_RJEM_MALLOC_CONF`. On x86_64 static musl, arming
  profiling at process start segfaults before `main`: isolated on a real
  service image with only the env var differing, `prof_active:true` exits 139
  while `prof_active:false` runs clean, and it still crashes at
  `lg_prof_sample:30` — which never samples in a short run — so the fault is
  activation itself rather than walking a sampled backtrace. Consumers should
  now set `prof:true,prof_active:false`; activation failure stays non-fatal and
  leaves CPU profiling running.

## [2.11.0] — 2026-08-03

### Added

- Process and Tokio runtime gauges, registered on the `MeterProvider` at init and on by default: `process.uptime`, `process.memory.resident`, `runtime.tokio.workers`, `runtime.tokio.alive_tasks`, `runtime.tokio.global_queue_depth`, `runtime.tokio.scheduler_delay`. `scheduler_delay` times a bare `yield_now()` round-trip, so comparing it against a service's own latency metrics separates a starved runtime from genuine downstream latency. Opt out with `TelemetryBuilder::with_runtime_metrics(false)`.
- Static identity on profiles: `host_name`, `deployment_environment` and `service_version` are attached to every profile upload, matching the resource attributes already on logs and traces. Previously all replicas of a service collapsed into one unlabelled series.
- `profiling-memory-jemalloc` feature adds a jemalloc heap-profiling agent alongside the CPU one, emitting `profile_type: "memory"`. Requires the consuming binary to install jemalloc as its global allocator, keep its symbol table, and run with `prof:true`; an unmet requirement warns and continues rather than failing startup.

### Fixed

- `TelemetryHandles::shutdown` no longer propagates provider flush failures. With no instruments registered there was nothing to export and it succeeded vacuously; with real instruments an unreachable collector turned every shutdown into a timeout and an error. Failing to deliver telemetry is not a failure of the program that produced it. Matches the existing `Drop` behaviour.

## [2.10.0] — 2026-07-27

### Added

- `TelemetryBuilder::with_log_filter` and `with_log_format` configure tracing directly without mutating process-global environment, enabling typed remote configuration sources to own logging safely.

## [2.9.1] — 2026-07-22

### Fixed

- Bind tracing-opentelemetry's subscriber layer to the configured SDK tracer provider. Tracing-native spans now receive valid trace and span IDs and reach the OTLP exporter instead of being handled by the layer's default no-op tracer.

## [2.9.0] — 2026-07-19

### Fixed

- **`axum_layer()`/`grpc_server_layer()` now attach the extracted parent context across the inner call, not just around it.** Both middlewares correctly extracted the incoming `traceparent` and built a properly-parented `SERVER` span for their own export — but never made that context ambient for the handler. `opentelemetry::Context::current()` inside a handler (and anything it calls — e.g. `api_bones::propagation::inject_current`, used by every brefwiz outbound client) saw the empty root context regardless of what was extracted, so any onward call the handler made injected a disconnected trace id. Confirmed as the root cause of the `quorumauth-origin`/`sealwiz-service -> brefwiz-spiffe` and downstream Tempo edges never linking past the first hop despite correct extraction/injection on both sides individually. Fixed via `opentelemetry::Context::FutureExt::with_context` around `inner.call(req)` — a bare `cx.attach()` guard would not survive the inner future resuming on a different tokio worker thread between polls. Regression test (`handler_observes_extracted_context_as_current`) asserts a handler's `Context::current()` carries the extracted parent's trace id.

## [2.8.0] — 2026-07-17

### Added

- **`spanned` module: `Spanned`/`in_span` helper to replace `Span::enter()` across an `.await`.** `tracing::Span::enter()` returns a guard tied to a thread-local "current span" stack; holding it across a suspend point is unsound in async code — the executor can resume the task on another thread or interleave another task while the guard is "entered," corrupting the stack and producing detached/zero-trace-id spans downstream (rejected wholesale by Tempo's OTLP ingest, dropping unrelated co-batched spans along with the corrupted one). `otel_bootstrap::spanned::in_span(span, fut)` and `Spanned::new(span).run(fut)` run a future under a pre-built span via `tracing::Instrument`, giving callers that build attributed spans ahead of time (the `*_span(...)`-style helper shape) an await-safe alternative to manual `.enter()`.

## [2.7.1] — 2026-07-15

### Fixed

- **`Instrumented::call` no longer emits spans with a 0-bit (invalid) trace id.** When a port call ran with an invalid span context as the current context (e.g. a KMS unwrap on a background / boot path outside any request trace), `start(&tracer)` inherited the invalid parent's all-zero trace id verbatim. Downstream, an OTLP collector's Tempo exporter rejects such spans (`trace ids must be 128 bit, received 0 bits`) and drops the **entire** batch — so one such producer can black out unrelated services' traces. `call` now parents on the current context only when it carries a valid span, and otherwise detaches to a fresh context so the SDK mints a new root trace id. Regression test covers the invalid-parent case.

### Changed

- **`pyroscope` bumped 0.5.8 → 2.1.0** (`profiling-bridge-pyroscope-rs` feature) — 2.1.0 vendors `pprof-rs` directly, so the separate `pyroscope_pprofrs` dependency is dropped; `start_pyroscope_bridge` moves to the new `PyroscopeAgentBuilder::new` constructor and `pyroscope::backend::pprof_backend`. Also adds `otel.scope.name`/`otel.scope.version`/`process.runtime.*` labels to pushed profiles for richer correlation in Grafana Pyroscope. Requires enabling the `backend-pprof-rs` feature on the `pyroscope` dependency.
- `uuid` bumped 1.23.4 → 1.23.5 (patch, hex parsing/formatting perf improvement).

## [2.7.0] — 2026-07-03

### Added

- **`Instrumented<P>` client-span wrapper for outbound hexagonal port calls** — `otel_bootstrap::Instrumented` wraps any outbound port trait object (a KMS provider, a secret store, any `#[async_trait]` port) so `Instrumented::call` opens an `otel.kind = "client"` span per call, tagged with `port.name`, `port.operation`, and an optional caller-supplied `port.provider_hint` — never an adapter-specific attribute hardcoded into otel-bootstrap. Plain delegation, not a proc-macro, so it works through `#[async_trait]` desugaring. Port registries (`KmsRegistry` and equivalents) should return `InstrumentedArc<dyn Port>` instead of a bare `Arc<dyn Port>`. See `examples/instrumented_port.rs`.

## [2.6.0] — 2026-07-03

### Added

- Regression test (`tests/profiling_requires_writable_tmp.rs`) proving `with_profiling()`'s `pprof` backend needs a writable temp directory to start at all — the root cause of a brefwiz-spiffe production incident where a `FROM scratch` container image with no `/tmp` crash-looped on `create profiler error` the first time its boot sequence ever reached profiler startup. `ci-test` now also runs the `profiling-bridge-pyroscope-rs` feature's existing test suite (previously compiled by `ci-lint --all-features` but never actually executed locally).

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
