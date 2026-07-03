// SPDX-License-Identifier: MIT
//! Wrapping a hexagonal outbound port with [`otel_bootstrap::Instrumented`].
//!
//! Shows the pattern a service like sealwiz-core follows for its `KmsProvider`
//! port: define the port trait as usual, then implement it for
//! `Instrumented<Arc<dyn Port>>` via plain delegation through
//! [`Instrumented::call`]. Every call through the wrapper carries an
//! `otel.kind = "client"` span by construction — no call-site discipline required.
//!
//! Run with:
//! ```text
//! cargo run --example instrumented_port --features testing
//! ```

use async_trait::async_trait;
use otel_bootstrap::{Instrumented, InstrumentedArc};
use std::sync::Arc;

/// A toy outbound hexagonal port, standing in for something like sealwiz-core's
/// `KmsProvider` — an `#[async_trait]` trait implemented by adapter crates.
#[async_trait]
trait GreetingPort: Send + Sync {
    async fn greet(&self, name: &str) -> Result<String, String>;
}

/// A stand-in adapter (would be `sealwiz-aws-kms`, `sealwiz-vault`, etc.).
struct StaticGreeter;

#[async_trait]
impl GreetingPort for StaticGreeter {
    async fn greet(&self, name: &str) -> Result<String, String> {
        Ok(format!("hello, {name}"))
    }
}

/// Delegating impl: `Instrumented<Arc<dyn GreetingPort>>` implements
/// `GreetingPort` itself, so callers depend on the port trait exactly as before —
/// the instrumentation is invisible at the call site, present at construction.
#[async_trait]
impl GreetingPort for Instrumented<Arc<dyn GreetingPort>> {
    async fn greet(&self, name: &str) -> Result<String, String> {
        self.call("greet", |inner| inner.greet(name)).await
    }
}

#[tokio::main]
async fn main() {
    let handles = otel_bootstrap::Telemetry::testing("instrumented-port-example");

    let adapter: Arc<dyn GreetingPort> = Arc::new(StaticGreeter);
    // Registries return `InstrumentedArc<dyn Port>`, never a bare `Arc<dyn Port>` —
    // the provider hint is supplied by the caller, never hardcoded into
    // otel-bootstrap.
    let port: InstrumentedArc<dyn GreetingPort> =
        Instrumented::new(adapter, "GreetingPort", Some("static-greeter"));

    let reply = port.greet("world").await.expect("greet succeeds");
    println!("{reply}");

    handles.shutdown().unwrap();
}
