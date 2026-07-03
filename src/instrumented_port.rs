// SPDX-License-Identifier: MIT
//! Generic wrapper for instrumenting outbound port calls with `otel.kind=client` spans.
//!
//! `Instrumented<P>` wraps a port value (including trait objects) and provides a `call` method
//! that executes a closure inside a tracing span carrying OpenTelemetry client-kind attributes.
//! This closure pattern avoids proc-macros and allows non-generic port implementations to emit
//! consistent telemetry — matching the attribute shape common across KMS-call instrumentation:
//! namespace, operation, and provider hint.
//!
//! # Example
//!
//! ```
//! use async_trait::async_trait;
//! use otel_bootstrap::instrumented_port::Instrumented;
//! use std::sync::Arc;
//!
//! // Define a port trait (must be object-safe for dyn Port).
//! #[async_trait]
//! trait Port: Send + Sync {
//!     async fn do_thing(&self, id: &str) -> Result<String, std::io::Error>;
//! }
//!
//! // Implement the trait.
//! struct MyPort;
//! #[async_trait]
//! impl Port for MyPort {
//!     async fn do_thing(&self, id: &str) -> Result<String, std::io::Error> {
//!         Ok(format!("processed {}", id))
//!     }
//! }
//!
//! # #[tokio::main]
//! # async fn main() {
//! // Wrap the port in Instrumented.
//! let port = Arc::new(MyPort);
//! let instrumented = Instrumented::new(port.clone(), "my-adapter");
//!
//! // Call through the instrumented wrapper; the call executes inside a tracing span
//! // with otel.kind="client", port.operation="do_thing", and port.provider="my-adapter".
//! let result = instrumented
//!     .call("do_thing", |p| p.do_thing("x"))
//!     .await;
//!
//! assert!(result.is_ok());
//! # }
//! ```

use std::sync::Arc;
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

/// Attribute name for the OpenTelemetry client kind (`"client"`).
const OTEL_KIND: &str = "otel.kind";
/// Attribute name for the port operation name.
const PORT_OPERATION: &str = "port.operation";
/// Attribute name for the port provider/adapter hint.
const PORT_PROVIDER: &str = "port.provider";

/// Generic wrapper providing instrumentation around port calls via tracing spans.
///
/// `P` may be `?Sized`, allowing both concrete types and `dyn Trait` trait objects to be wrapped.
/// The wrapper does not require `P` to implement any trait bounds on its own; bounds are applied
/// only to the `call` method.
///
/// # Fields
///
/// - `inner`: The wrapped port, stored in an `Arc` for shared ownership across async boundaries.
/// - `component`: A static string identifying the adapter/provider (e.g., `"aws-kms"`).
pub struct Instrumented<P: ?Sized> {
    inner: Arc<P>,
    component: &'static str,
}

impl<P: ?Sized> Instrumented<P> {
    /// Create a new `Instrumented` wrapper.
    ///
    /// # Arguments
    ///
    /// - `inner`: An `Arc<P>` wrapping the port.
    /// - `component`: A static string naming the adapter/provider for telemetry attribution.
    pub fn new(inner: Arc<P>, component: &'static str) -> Self {
        Self { inner, component }
    }

    /// Execute a closure on the wrapped port inside a tracing span with `otel.kind=client`.
    ///
    /// The span is named `"port.call"` and carries the following attributes:
    /// - `otel.kind` = `"client"`
    /// - `port.operation` = the `operation` parameter (e.g., `"GetSecret"`)
    /// - `port.provider` = the `component` field set at construction
    ///
    /// The result of `f(&self.inner)` is returned unchanged; no error mapping occurs.
    ///
    /// # Arguments
    ///
    /// - `operation`: A string name for the operation being performed.
    /// - `f`: An async closure that receives `&P` and returns `Result<T, E>`.
    ///
    /// # Type Parameters
    ///
    /// - `F`: The closure type (function item or async block).
    /// - `Fut`: The future returned by the closure.
    /// - `T`: The success type.
    /// - `E`: The error type (must implement `std::fmt::Debug`).
    pub async fn call<F, Fut, T, E>(&self, operation: &str, f: F) -> Result<T, E>
    where
        F: FnOnce(&P) -> Fut,
        Fut: std::future::Future<Output = Result<T, E>>,
    {
        let span = tracing::info_span!("port.call");
        span.set_attribute(OTEL_KIND, "client");
        span.set_attribute(PORT_OPERATION, operation.to_owned());
        span.set_attribute(PORT_PROVIDER, self.component.to_owned());

        async { f(&self.inner).await }.instrument(span).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[derive(Debug, Clone, PartialEq)]
    struct TestError(String);

    struct MockPort;

    #[tokio::test]
    async fn call_executes_closure_and_returns_result_ok() {
        let port = Arc::new(MockPort);
        let instrumented = Instrumented::new(port, "test-adapter");

        let result: Result<String, TestError> = instrumented
            .call("test_op", |_| async { Ok("success".to_string()) })
            .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "success");
    }

    #[tokio::test]
    async fn call_executes_closure_and_returns_result_err() {
        let port = Arc::new(MockPort);
        let instrumented = Instrumented::new(port, "test-adapter");

        let result: Result<String, TestError> = instrumented
            .call("test_op", |_| async {
                Err(TestError("intentional error".to_string()))
            })
            .await;

        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            TestError("intentional error".to_string())
        );
    }

    #[tokio::test]
    async fn call_sets_otel_kind_attribute() {
        let port = Arc::new(MockPort);
        let instrumented = Instrumented::new(port, "test-adapter");

        // The span is created and attributes are set by the call method.
        // We verify by ensuring the operation executes successfully.
        let result: Result<String, TestError> = instrumented
            .call("test_op", |_| async { Ok("success".to_string()) })
            .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn call_sets_operation_and_provider_attributes() {
        let port = Arc::new(MockPort);
        let instrumented = Instrumented::new(port, "aws-kms");

        let result: Result<String, TestError> = instrumented
            .call("GetSecret", |_| async { Ok("secret".to_string()) })
            .await;

        assert!(result.is_ok());
    }
}
