//! Tonic gRPC trace-context propagation, client and server side.
//!
//! Enabled by the `tonic-tracing` feature. Mirrors [`crate::axum_middleware`]
//! but for raw tonic `Channel`/`Server` usage (services that don't go through
//! an axum router — e.g. a hand-rolled tonic client/server pair).
//!
//! # Client side
//!
//! ```no_run
//! # #[cfg(feature = "tonic-tracing")]
//! # async fn example() -> Result<(), tonic::transport::Error> {
//! let channel = tonic::transport::Channel::from_static("http://localhost:50051")
//!     .connect()
//!     .await?;
//! let channel = tower::ServiceBuilder::new()
//!     .layer(otel_bootstrap::grpc_client_layer())
//!     .service(channel);
//! # Ok(())
//! # }
//! ```
//!
//! # Server side
//!
//! ```no_run
//! # #[cfg(feature = "tonic-tracing")]
//! # fn example<S>(svc: S) {
//! let _ = tonic::transport::Server::builder()
//!     .layer(otel_bootstrap::grpc_server_layer());
//! # }
//! ```

use opentelemetry::{
    global,
    propagation::{Extractor, Injector},
    trace::{SpanKind, Status, TraceContextExt, Tracer},
};
use std::{
    future::Future,
    pin::Pin,
    task::{self, Poll},
};
use tonic::body::Body;
use tower::{Layer, Service};

/// Tower [`Layer`] that injects the current trace context into outgoing gRPC
/// request metadata. Wrap a tonic [`tonic::transport::Channel`] with this
/// before constructing the generated client stub.
///
/// Construct via [`crate::grpc_client_layer`].
#[derive(Clone, Debug, Default)]
pub struct GrpcClientTraceLayer;

impl<S> Layer<S> for GrpcClientTraceLayer {
    type Service = GrpcClientTraceService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        GrpcClientTraceService { inner }
    }
}

/// Tower [`Service`] produced by [`GrpcClientTraceLayer`].
#[derive(Clone, Debug)]
pub struct GrpcClientTraceService<S> {
    inner: S,
}

impl<S> Service<http::Request<Body>> for GrpcClientTraceService<S>
where
    S: Service<http::Request<Body>> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<Body>) -> Self::Future {
        let path = req.uri().path().to_string();

        let tracer = global::tracer("otel-bootstrap");
        let span = tracer
            .span_builder(path)
            .with_kind(SpanKind::Client)
            .start(&tracer);
        let cx = opentelemetry::Context::current_with_span(span);

        global::get_text_map_propagator(|propagator| {
            propagator.inject_context(&cx, &mut MetadataInjector(req.headers_mut()));
        });

        let mut inner = self.inner.clone();
        Box::pin(async move { inner.call(req).await })
    }
}

/// Tower [`Layer`] that extracts trace context from incoming gRPC request
/// metadata and opens a child span. Attach to a tonic
/// [`tonic::transport::Server`] via `.layer(...)`.
///
/// Construct via [`crate::grpc_server_layer`].
#[derive(Clone, Debug, Default)]
pub struct GrpcServerTraceLayer;

impl<S> Layer<S> for GrpcServerTraceLayer {
    type Service = GrpcServerTraceService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        GrpcServerTraceService { inner }
    }
}

/// Tower [`Service`] produced by [`GrpcServerTraceLayer`].
#[derive(Clone, Debug)]
pub struct GrpcServerTraceService<S> {
    inner: S,
}

impl<S> Service<http::Request<Body>> for GrpcServerTraceService<S>
where
    S: Service<http::Request<Body>, Response = http::Response<Body>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    type Response = http::Response<Body>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<Body>) -> Self::Future {
        let path = req.uri().path().to_string();

        let parent_cx = global::get_text_map_propagator(|propagator| {
            propagator.extract(&MetadataExtractor(req.headers()))
        });

        let tracer = global::tracer("otel-bootstrap");
        let span = tracer
            .span_builder(path)
            .with_kind(SpanKind::Server)
            .start_with_context(&tracer, &parent_cx);
        let cx = parent_cx.with_span(span);

        let mut inner = self.inner.clone();
        Box::pin(async move {
            let result = inner.call(req).await;

            match &result {
                Ok(resp) => {
                    // gRPC status is carried in the `grpc-status` trailer, not the
                    // HTTP status — a non-OK RPC still returns HTTP 200. Tonic
                    // trailers aren't available at this layer (they're written
                    // after the body stream completes), so only genuine transport
                    // failures (HTTP-level errors) are recorded here.
                    if resp.status().is_server_error() {
                        cx.span().set_status(Status::Error {
                            description: resp.status().canonical_reason().unwrap_or("").into(),
                        });
                    }
                }
                Err(_) => {
                    cx.span().set_status(Status::Error {
                        description: "transport error".into(),
                    });
                }
            }

            result
        })
    }
}

/// [`Extractor`] that reads from tonic/http [`http::HeaderMap`].
struct MetadataExtractor<'a>(&'a http::HeaderMap);

impl Extractor for MetadataExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(http::HeaderName::as_str).collect()
    }
}

/// [`Injector`] that writes into a mutable [`http::HeaderMap`].
struct MetadataInjector<'a>(&'a mut http::HeaderMap);

impl Injector for MetadataInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        if let (Ok(name), Ok(val)) = (
            http::HeaderName::from_bytes(key.as_bytes()),
            http::HeaderValue::from_str(&value),
        ) {
            self.0.insert(name, val);
        }
    }
}
