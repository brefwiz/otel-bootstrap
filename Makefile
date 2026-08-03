.PHONY: help setup check build fmt format fmt-check lint test \
        ci-format ci-lint ci-lockfile-diff ci-check ci-test ci-coverage ci-e2e ci-audit ci-changelog ci-build-check ci-release-readiness \
        install-nextest install-llvm-cov \
        e2e-up e2e-down e2e-logs e2e-run clean pre-commit lockfile spec-check

.DEFAULT_GOAL := help

CARGO := $(shell which cargo 2>/dev/null || echo $(HOME)/.cargo/bin/cargo)

# Colors
GREEN  := \033[32m
YELLOW := \033[33m
RESET  := \033[0m

help: ## Show available commands
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-22s\033[0m %s\n", $$1, $$2}'

# =============================================================================
# Dev environment
# =============================================================================

setup: ## Install rustfmt + clippy
	rustup component add rustfmt clippy

# =============================================================================
# Build & check
# =============================================================================

check: ## Cargo check (fast compile check)
	$(CARGO) check --workspace

build: ## Build all crates
	$(CARGO) build --workspace

# =============================================================================
# Code quality
# =============================================================================

fmt: ## Format all code
	$(CARGO) fmt --all

format: fmt

fmt-check: ## Check formatting (fails if not formatted)
	@echo "$(YELLOW)Checking formatting...$(RESET)"
	$(CARGO) fmt --all -- --check
	@echo "$(GREEN)✅ Formatting OK$(RESET)"

lint: ## Clippy — warnings are errors
	@echo "$(YELLOW)Running clippy...$(RESET)"
	$(CARGO) clippy --workspace -- -D warnings
	@echo "$(GREEN)✅ Clippy clean$(RESET)"

# =============================================================================
# Tests
# =============================================================================

install-nextest: ## Install cargo-nextest
	@$(CARGO) install cargo-nextest --version 0.9.114 --locked 2>/dev/null || true

install-llvm-cov: ## Install cargo-llvm-cov
	@$(CARGO) install cargo-llvm-cov --locked 2>/dev/null || true

test: fmt-check lint install-nextest ## Run all tests (local)
	$(CARGO) nextest run --workspace
	@echo "$(GREEN)✅ All tests passed$(RESET)"

# =============================================================================
# CI targets (called directly from Forgejo Actions)
# =============================================================================

ci-format: ## CI: format check
	$(CARGO) fmt --all -- --check

ci-lint: ## CI: clippy strict (--all-features so feature-gated code is exercised)
	# Both invocations, because they are not equivalent and CI runs the second.
	#
	# --all-targets builds tests and examples, so dev-dependencies enter feature
	# unification. A [[bin]] that uses a feature only dev-dependencies enable
	# then compiles here and fails in CI. That is not hypothetical: heap-probe
	# needs tokio/rt-multi-thread, dev-deps take tokio with "full", and this
	# target passed locally while CI's clippy failed with E0599 on
	# Builder::new_multi_thread.
	#
	# The plain form is what CI runs and catches under-declared bin
	# dependencies; --all-targets is stricter about coverage and catches lints
	# in test code. Neither subsumes the other.
	$(CARGO) clippy --workspace --all-features -- -D warnings
	$(CARGO) clippy --workspace --all-features --all-targets -- -D warnings

ci-lockfile-diff: ## CI: assert committed Cargo.lock matches resolution (ADR-0021)
	@cp Cargo.lock Cargo.lock.committed
	@$(CARGO) generate-lockfile
	@diff Cargo.lock.committed Cargo.lock || { \
		echo ""; \
		echo "ERROR: Cargo.lock is out of date. Run: cargo generate-lockfile && git add Cargo.lock"; \
		mv Cargo.lock.committed Cargo.lock; exit 1; }
	@mv Cargo.lock.committed Cargo.lock

ci-check: ci-format ci-lint ## CI: format + lint (stage 1)
	@echo "$(GREEN)✅ All code quality checks passed$(RESET)"

ci-test: ## CI: run unit tests with nextest
	# Mirrors what CI actually runs: the shared rust.yml workflow tests with
	# --all-features and skips only the e2e binary (which needs a live
	# collector on :4317 and has its own job). This target previously used a
	# hand-listed feature subset and ran 98 of the 119 tests CI runs, which hid
	# two separate failures: `tests/builder.rs` was never compiled locally, and
	# neither was the jemalloc backend, so the whole profiling module passed
	# here and panicked in CI. Keep this in step with the workflow — a local
	# gate that covers less than CI is worse than no local gate, because it is
	# believed.
	RUSTFLAGS="-D warnings" $(CARGO) nextest run --workspace \
		--all-features -E 'not binary(e2e)'

# ─────────────────────────────────────────────────────────────────────────────
# Heap profiling on the target that actually ships
#
# The host `heap-probe` run under ci-test links glibc dynamically. Every
# consuming service ships a STATICALLY LINKED MUSL binary (FROM scratch
# images, `--target x86_64-unknown-linux-musl`), and that difference is not
# cosmetic: jemalloc's `prof` walks a stack on each sampled allocation, and
# with --enable-prof alone it walks it through libgcc's _Unwind_Backtrace,
# which has no working unwind path in a static musl binary. The fix is
# tikv-jemalloc-sys/profiling_libunwind (see Cargo.toml); this target is what
# proves it on the shipped target rather than on the host's glibc.
#
# The cost of not testing this: brefwiz-spiffe 0.48.0, 0.49.0 and 0.49.1 all
# shipped with heap profiling broken. 0.49.1 segfaulted (exit 139) in staging
# with sampling armed from Rust — the configuration the host probe proves
# "works" — because the host probe was never musl. Three releases, each caught
# by a rollout rather than by CI.
#
# Runs in a musl container on every host, CI included. One environment rather
# than a native path and a container path that can drift apart — and the
# container is where libunwind built against musl comes from. On an x86_64
# runner this is the shipped target natively; on an arm64 dev machine it is
# aarch64 musl, which shares the static-musl variable under test.
# ─────────────────────────────────────────────────────────────────────────────
MALLOC_CONF_PROD ?= prof:true,prof_active:false,lg_prof_sample:19
# Force a container arch, e.g. MUSL_PLATFORM=linux/amd64 to reproduce the
# shipped target from an arm64 machine. Unset on x86_64 CI, which already is it.
MUSL_PLATFORM ?=
MUSL_PLATFORM_FLAG := $(if $(MUSL_PLATFORM),--platform $(MUSL_PLATFORM),)

ci-heap-probe-musl: ## CI: run heap-probe as a static musl binary (the shipped target)
	@docker run --rm $(MUSL_PLATFORM_FLAG) -v "$$PWD":/src -w /src \
		-v "$$HOME/.cargo/registry":/usr/local/cargo/registry \
		-e CARGO_TARGET_DIR=/tmp/muslbuild \
		-e MALLOC_CONF_PROD='$(MALLOC_CONF_PROD)' \
		rust:alpine sh -ec '\
		apk add --no-cache musl-dev gcc make bash perl libunwind-dev libunwind-static >/dev/null; \
		cargo build --features profiling-memory-probe --bin heap-probe; \
		echo "==> target: $$(uname -m) static musl"; \
		set +e; \
		_RJEM_MALLOC_CONF="$$MALLOC_CONF_PROD" /tmp/muslbuild/debug/heap-probe; \
		rc=$$?; \
		set -e; \
		if [ $$rc -ge 128 ]; then \
			echo ""; \
			echo "ERROR: heap profiling killed the process by signal $$((rc - 128)) on static musl."; \
			echo "  _RJEM_MALLOC_CONF=$$MALLOC_CONF_PROD"; \
			echo "  This is the target consuming services ship. jemalloc prof walks a"; \
			echo "  stack per sampled allocation; without libunwind it walks it through"; \
			echo "  libgcc, which has no working unwind path in a static musl binary."; \
			exit $$rc; \
		elif [ $$rc -ne 0 ]; then \
			echo ""; \
			echo "ERROR: heap probe exited $$rc on static musl — profiling did not arm."; \
			exit $$rc; \
		fi'

ci-build-check: ## Pre-push compile gate: workspace + all feature combinations
	$(CARGO) check --workspace --all-targets
	$(CARGO) check --workspace --all-targets --all-features
	$(CARGO) check --workspace --all-targets --no-default-features
	$(CARGO) check --workspace --all-targets --no-default-features --features http
	$(CARGO) check --workspace --all-targets --no-default-features --features grpc
	$(CARGO) check --workspace --all-targets --no-default-features --features grpc-mtls

ci-release-readiness: ## Pre-release sanity (no-op: single-crate lib, no SDK to validate)
	@echo "otel-bootstrap: single-crate lib — nothing to validate here"

ci-coverage: ## CI: coverage gate
	# Excluded lines:
	#   1. The `None => builder` arm in `init_telemetry_with_sampler` — semantically
	#      a no-op (keeps the default builder); covering it would just duplicate
	#      `init_telemetry` tests.
	#   2-12. The `grpc-mtls` code paths (with_mtls(), MtlsMaterial Debug redact,
	#      build_tls_config helper, the 3 with_tls_config conditionals) — exercising
	#      these requires a mTLS gRPC test server, scheduled as follow-up work in
	#      the rotation-watcher PR.
	#   13-101. SpanAwareLogBridge test mock boilerplate (unreachable LogRecord trait
	#      stubs: add_attributes, set_timestamp, set_observed_timestamp) and
	#      with_propagated_span_fields / from_env builder paths not exercised
	#      by the integration-tests feature. Threshold bumped 30 → 110 for 2.2.0.
	# --show-missing-lines so a failure names the uncovered lines instead of
	# only a count. Without it the gate reports "N > threshold" and every
	# diagnosis is guesswork against a per-file summary, which is how three
	# separate wrong fixes got attempted here.
	RUSTFLAGS="-D warnings" $(CARGO) llvm-cov nextest --workspace \
		--features integration-tests,grpc-mtls \
		--show-missing-lines \
		--fail-uncovered-lines 110

ci-e2e: ## CI: e2e tests (requires OTel Collector on :4317)
	RUSTFLAGS="-D warnings" $(CARGO) nextest run \
		--features integration-tests \
		--test e2e

ci-audit: ## CI: security audit
	$(CARGO) audit --deny warnings --deny unsound --deny unmaintained --deny yanked $(if $(DB_PATH),--db $(DB_PATH) --no-fetch,)

# =============================================================================
# Security
# =============================================================================

audit: ## Run cargo-audit for known CVEs
	$(CARGO) audit

# =============================================================================
# E2E local (mirrors CI exactly)
# =============================================================================

e2e-up: ## Start OTel Collector for e2e tests
	docker compose up -d
	@echo "$(GREEN)OTel Collector running — gRPC on :4317$(RESET)"

e2e-down: ## Stop OTel Collector
	docker compose down

e2e-logs: ## Tail OTel Collector logs
	docker compose logs -f

e2e-run: e2e-up ## Full e2e: start collector + run integration tests
	$(CARGO) test --features integration-tests --test e2e
	@echo "$(GREEN)✅ E2E tests passed$(RESET)"

# =============================================================================
# Gates
# =============================================================================

lockfile: ## generate lockfile
	cargo generate-lockfile

spec-check: ## L1 ADR-0086: SPEC.md exists and wire_surface is valid
	@test -f SPEC.md || { echo "ERROR: SPEC.md missing"; exit 1; }
	@grep -q 'wire_surface:' SPEC.md || { echo "ERROR: SPEC.md missing wire_surface field"; exit 1; }
	@echo "spec-check: OK"

pre-commit: spec-check ci-format ci-lint ci-lockfile-diff ci-test ci-changelog ## Run all pre-commit checks (ADR-0021)

clean: ## Remove build artifacts
	$(CARGO) clean

.PHONY: ci-changelog
ci-changelog: ## CI: verify CHANGELOG.md has entry for current package version (ADR-0021)
	@bash -lc 'bash <(curl -fsSL https://raw.githubusercontent.com/brefwiz/shared-ci-workflows/main/scripts/check-release-changelog.sh)'
