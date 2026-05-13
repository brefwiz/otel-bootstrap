//! Axum tower middleware for W3C trace context propagation.
//!
//! Enabled by the `axum` feature flag. Add to any axum [`Router`] via
//! [`crate::axum_layer()`]:
//!
//! ```no_run
//! use axum::Router;
//!
//! let app: Router = Router::new().layer(otel_bootstrap::axum_layer());
//! ```

use axum::{
    body::Body,
    http::{HeaderMap, HeaderName, HeaderValue, Request, Response},
};
use opentelemetry::{
    global,
    propagation::{Extractor, Injector},
    trace::{SpanKind, Status, TraceContextExt, Tracer},
};
use opentelemetry_semantic_conventions::attribute::{
    HTTP_REQUEST_METHOD, HTTP_RESPONSE_STATUS_CODE,
};
use std::{
    future::Future,
    pin::Pin,
    task::{self, Poll},
};
use tower::{Layer, Service};

/// Tower [`Layer`] that instruments incoming HTTP requests with OpenTelemetry
/// trace context propagation.
///
/// Attach to an axum router with [`crate::axum_layer()`].
///
/// # Example
/// ```no_run
/// use axum::Router;
///
/// let app: Router = Router::new().layer(otel_bootstrap::axum_layer());
/// ```
#[derive(Clone, Debug)]
pub struct OtelTraceLayer;

impl<S> Layer<S> for OtelTraceLayer {
    type Service = OtelTraceService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        OtelTraceService { inner }
    }
}

/// Tower [`Service`] produced by [`OtelTraceLayer`].
///
/// This type is not constructed directly. It is returned by
/// [`OtelTraceLayer`] when wrapping an inner [`tower::Service`].
#[derive(Clone, Debug)]
pub struct OtelTraceService<S> {
    inner: S,
}

impl<S> Service<Request<Body>> for OtelTraceService<S>
where
    S: Service<Request<Body>, Response = Response<Body>> + Send + Clone + 'static,
    S::Future: Send + 'static,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    type Response = Response<Body>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let method = req.method().to_string();
        let route = req.uri().path().to_string();

        // Extract parent context from incoming headers.
        let parent_cx = global::get_text_map_propagator(|propagator| {
            propagator.extract(&HeaderExtractor(req.headers()))
        });

        // Create a child span inheriting the remote parent.
        let tracer = global::tracer("otel-bootstrap");
        let span = tracer
            .span_builder(format!("{method} {route}"))
            .with_kind(SpanKind::Server)
            .with_attributes([opentelemetry::KeyValue::new(HTTP_REQUEST_METHOD, method)])
            .start_with_context(&tracer, &parent_cx);

        let cx = parent_cx.with_span(span);
        let mut inner = self.inner.clone();

        Box::pin(async move {
            let mut response = inner.call(req).await?;

            // Record HTTP status on the span.
            let status_code = response.status().as_u16();
            cx.span().set_attribute(opentelemetry::KeyValue::new(
                HTTP_RESPONSE_STATUS_CODE,
                status_code as i64,
            ));
            if response.status().is_server_error() {
                cx.span().set_status(Status::Error {
                    description: response.status().canonical_reason().unwrap_or("").into(),
                });
            }

            // Inject outgoing trace context into response headers.
            let mut injector = HeaderInjector(response.headers_mut());
            global::get_text_map_propagator(|propagator| {
                propagator.inject_context(&cx, &mut injector);
            });

            Ok(response)
        })
    }
}

/// Tower [`Layer`] that records span attributes from a `T: EnrichSpan`
/// extension on each incoming request.
///
/// Construct via [`crate::span_enricher_layer`]. When no `T` extension is
/// found in the request (e.g. a platform-scope route), the service is a no-op.
///
/// # Example
/// ```no_run
/// use axum::{Router, Extension, routing::get};
/// use otel_bootstrap::span_enrichment::EnrichSpan;
/// use tracing_opentelemetry::OpenTelemetrySpanExt as _;
///
/// #[derive(Clone)]
/// struct MyCtx { user_id: String }
///
/// impl EnrichSpan for MyCtx {
///     fn enrich_span(&self, span: &tracing::Span) {
///         span.set_attribute("enduser.id", self.user_id.clone());
///     }
/// }
///
/// let app: Router = Router::new()
///     .route("/", get(|| async { "ok" }))
///     .layer(otel_bootstrap::span_enricher_layer::<MyCtx>())
///     .layer(Extension(MyCtx { user_id: "u1".into() }))
///     .layer(otel_bootstrap::axum_layer());
/// ```
#[derive(Debug)]
pub struct SpanEnricherLayer<T>(std::marker::PhantomData<T>);

impl<T> Default for SpanEnricherLayer<T> {
    fn default() -> Self {
        Self(std::marker::PhantomData)
    }
}

impl<T> Clone for SpanEnricherLayer<T> {
    fn clone(&self) -> Self {
        Self(std::marker::PhantomData)
    }
}

impl<T, S> Layer<S> for SpanEnricherLayer<T>
where
    T: crate::span_enrichment::EnrichSpan + Clone + Send + Sync + 'static,
{
    type Service = SpanEnricherService<T, S>;

    fn layer(&self, inner: S) -> Self::Service {
        SpanEnricherService {
            inner,
            _marker: std::marker::PhantomData,
        }
    }
}

/// Tower [`Service`] produced by [`SpanEnricherLayer`].
#[derive(Clone, Debug)]
pub struct SpanEnricherService<T, S> {
    inner: S,
    _marker: std::marker::PhantomData<T>,
}

impl<T, S> Service<Request<Body>> for SpanEnricherService<T, S>
where
    T: crate::span_enrichment::EnrichSpan + Clone + Send + Sync + 'static,
    S: Service<Request<Body>, Response = Response<Body>>,
{
    type Response = Response<Body>;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        if let Some(ctx) = req.extensions().get::<T>() {
            ctx.enrich_span(&tracing::Span::current());
        }
        self.inner.call(req)
    }
}

/// [`Extractor`] that reads from [`HeaderMap`].
struct HeaderExtractor<'a>(&'a HeaderMap);

impl Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(HeaderName::as_str).collect()
    }
}

/// [`Injector`] that writes into a mutable [`HeaderMap`].
struct HeaderInjector<'a>(&'a mut HeaderMap);

impl Injector for HeaderInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        if let (Ok(name), Ok(val)) = (
            HeaderName::from_bytes(key.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            self.0.insert(name, val);
        }
    }
}
