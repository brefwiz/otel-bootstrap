# Contributing

Thank you for your interest in contributing to otel-bootstrap!

## Before You Start

- Check [existing issues](https://github.com/brefwiz/otel-bootstrap/issues) to avoid duplicates.
- For significant changes, open an issue first to discuss the approach.

## Development Setup

```sh
git clone https://github.com/brefwiz/otel-bootstrap.git
cd otel-bootstrap
cargo build
cargo test --all-features
```

Requires Rust 1.85+. Install via [rustup](https://rustup.rs).

## Running Tests

```sh
# Unit tests
cargo test --all-features

# Integration tests (requires OTel Collector on :4317)
make e2e-up
cargo test --features integration-tests --test e2e
make e2e-down
```

## Code Style

```sh
cargo fmt --check
cargo clippy --all-features -- -D warnings
```

Both run in CI and must pass.

## Commit Messages

Follow [Conventional Commits](https://www.conventionalcommits.org/):
`feat:`, `fix:`, `chore:`, `docs:`, `refactor:`, `test:`

## Pull Requests

- One concern per PR.
- Include tests for new behaviour.
- Update `CHANGELOG.md` under `[Unreleased]`.

## License

By contributing, you agree your contributions are licensed under the [MIT License](LICENSE).
