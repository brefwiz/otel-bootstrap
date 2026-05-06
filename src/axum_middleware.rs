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

/// Tower [`Layer`] that records `enduser.*` attributes on the active span from
/// a [`quorum_identity::OrganizationContext`] carried in the request extensions.
///
/// Intended to sit *inside* the axum [`axum::Extension`] layer that injects
/// `OrganizationContext`, so the context is present by the time a request
/// reaches this service.
///
/// When the extension is absent (e.g. a platform-scope route that is not
/// inside the authenticated tenant surface) the service is a no-op and emits
/// a single `tracing::warn!` the first time the condition is observed.
///
/// # Example
/// ```no_run
/// # #[cfg(all(feature = "axum", feature = "org-context"))] {
/// use axum::{Router, Extension, routing::get};
/// use quorum_identity::{OrganizationContext, OrgId, Principal, RequestId};
/// use uuid::Uuid;
///
/// let ctx = OrganizationContext::new(
///     OrgId::new(Uuid::new_v4().to_string()),
///     Principal::human(Uuid::new_v4()),
///     RequestId::new(),
/// );
///
/// let app: Router = Router::new()
///     .route("/", get(|| async { "ok" }))
///     .layer(otel_bootstrap::org_context_span_enricher_layer())
///     .layer(Extension(ctx))
///     .layer(otel_bootstrap::axum_layer());
/// # }
/// ```
#[cfg(feature = "org-context")]
#[derive(Clone, Debug)]
pub struct OrgContextSpanEnricher;

#[cfg(feature = "org-context")]
impl<S> Layer<S> for OrgContextSpanEnricher {
    type Service = OrgContextSpanEnricherService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        OrgContextSpanEnricherService { inner }
    }
}

/// Tower [`Service`] produced by [`OrgContextSpanEnricher`].
///
/// This type is not constructed directly. It is returned by
/// [`OrgContextSpanEnricher`] when wrapping an inner [`tower::Service`].
#[cfg(feature = "org-context")]
#[derive(Clone, Debug)]
pub struct OrgContextSpanEnricherService<S> {
    inner: S,
}

#[cfg(feature = "org-context")]
static MISSING_ORG_CONTEXT_WARNED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(feature = "org-context")]
impl<S> Service<Request<Body>> for OrgContextSpanEnricherService<S>
where
    S: Service<Request<Body>, Response = Response<Body>>,
{
    type Response = Response<Body>;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        match req
            .extensions()
            .get::<quorum_identity::OrganizationContext>()
        {
            Some(ctx) => {
                crate::span_enrichment::emit_enduser_fields(ctx);
            }
            None => {
                if !MISSING_ORG_CONTEXT_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    tracing::warn!(
                        target: "otel_bootstrap::org_context",
                        "OrganizationContext extension missing from request; \
                         enduser.* span attributes will not be emitted. This \
                         warning is logged once per process."
                    );
                }
            }
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
