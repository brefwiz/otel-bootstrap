.PHONY: help setup check build fmt format fmt-check lint test \
        ci-format ci-lint ci-check ci-test ci-coverage ci-e2e ci-audit \
        install-nextest install-llvm-cov \
        e2e-up e2e-down e2e-logs e2e-run clean

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

fmt format: ## Format all code
	$(CARGO) fmt --all

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

ci-lint: ## CI: clippy strict
	$(CARGO) clippy --workspace -- -D warnings

ci-check: ci-format ci-lint ## CI: format + lint (stage 1)
	@echo "$(GREEN)✅ All code quality checks passed$(RESET)"

ci-test: ## CI: run unit tests with nextest
	RUSTFLAGS="-D warnings" $(CARGO) nextest run --workspace

ci-coverage: ## CI: coverage gate (≤1 uncovered line; see NOTE below)
	# NOTE: the `None => builder` arm in `init_telemetry_with_sampler` is
	# deliberately excluded.  Covering it would require a dedicated e2e test
	# that is functionally identical to the existing `init_telemetry` tests —
	# the `None` path is semantically a no-op (keeps the default builder).
	RUSTFLAGS="-D warnings" $(CARGO) llvm-cov nextest --workspace \
		--features integration-tests \
		--fail-uncovered-lines 1

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

pre-commit: fmt-check lint test ## Full local validation gate

clean: ## Remove build artifacts
	$(CARGO) clean
