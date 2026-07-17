// SPDX-License-Identifier: MIT
//! Await-safe replacement for the `let _guard = span.enter();` pattern.
//!
//! [`Spanned::run`] and the free function [`in_span`] run an async block
//! under a `tracing::Span` via [`tracing::Instrument`], which attaches the
//! span to the future's poll rather than to a thread-local guard.
//!
//! ## The footgun this replaces
//!
//! `tracing::Span::enter()` returns a guard tied to a thread-local
//! "current span" stack. Holding that guard across an `.await` is unsound in
//! async code: the executor can suspend the task mid-span and resume it on a
//! different thread, or interleave another task on the same thread while the
//! guard is still "entered" — either way the thread-local stack gets
//! corrupted, and spans opened during or after the corrupted window can end
//! up detached or exported with a zero/garbled trace id (rejected by
//! Tempo's OTLP ingest, silently dropping the whole batch).
//!
//! ```compile_fail
//! # async fn load_master() -> Result<(), ()> { Ok(()) }
//! # async fn bad() -> Result<(), ()> {
//! let span = tracing::info_span!("sealwiz.wrap");
//! let _guard = span.enter(); // BAD: guard held across the await below
//! load_master().await?;
//! # Ok(())
//! # }
//! ```
//!
//! ## The fix
//!
//! ```
//! use otel_bootstrap::spanned::in_span;
//!
//! # async fn load_master() -> Result<&'static str, ()> { Ok("master") }
//! # async fn good() -> Result<&'static str, ()> {
//! let span = tracing::info_span!("sealwiz.wrap");
//! in_span(span, load_master()).await
//! # }
//! ```
//!
//! Prefer `#[tracing::instrument]` on the `async fn` itself when the span's
//! attributes are all available from the function's own parameters — it
//! expands to the same `Instrument::instrument` call and needs no helper.
//! Reach for [`in_span`] / [`Spanned`] when the span must be built ahead of
//! time with attributes computed by the caller (the shape sealwiz-core's
//! `telemetry::wrap_span`-style helpers use), where returning a bare `Span`
//! for the caller to `.enter()` is the footgun.

use std::future::Future;
use tracing::Instrument;

/// Run `fut` under `span` for its entire lifetime, including every await
/// point. Equivalent to `fut.instrument(span).await`, spelled out as a
/// standalone function so call sites read as "run this under that span"
/// instead of reaching for `Span::enter()`.
pub async fn in_span<F: Future>(span: tracing::Span, fut: F) -> F::Output {
    fut.instrument(span).await
}

/// Binds a pre-built `tracing::Span` to async work supplied later, so the
/// span can be constructed with attributes up front (mirroring
/// `telemetry::wrap_span`-style helpers) without ever exposing a bare
/// `Span` for the caller to `.enter()` manually.
///
/// # Example
/// ```
/// use otel_bootstrap::spanned::Spanned;
///
/// # async fn load_master() -> Result<&'static str, ()> { Ok("master") }
/// # async fn example() -> Result<&'static str, ()> {
/// let span = tracing::info_span!("sealwiz.wrap", sealwiz.namespace = "ns-1");
/// Spanned::new(span).run(load_master()).await
/// # }
/// ```
pub struct Spanned {
    span: tracing::Span,
}

impl Spanned {
    /// Wrap a pre-built span, e.g. one returned by a `*_span(...)`-style
    /// attribute-recording helper.
    pub fn new(span: tracing::Span) -> Self {
        Self { span }
    }

    /// Run `fut` under the wrapped span for its entire lifetime.
    pub async fn run<F: Future>(self, fut: F) -> F::Output {
        fut.instrument(self.span).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::registry;

    #[derive(Clone, Default)]
    struct RecordingLayer {
        entered: Arc<Mutex<Vec<String>>>,
    }

    impl<S> tracing_subscriber::Layer<S> for RecordingLayer
    where
        S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        fn on_enter(&self, id: &tracing::span::Id, ctx: tracing_subscriber::layer::Context<'_, S>) {
            if let Some(span) = ctx.span(id) {
                self.entered.lock().unwrap().push(span.name().to_string());
            }
        }
    }

    #[tokio::test]
    async fn in_span_enters_span_around_await_points() {
        let layer = RecordingLayer::default();
        let entered = layer.entered.clone();
        let subscriber = registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        let span = tracing::info_span!("sealwiz.wrap");
        let out = in_span(span, async {
            tokio::task::yield_now().await;
            "wrapped"
        })
        .await;

        assert_eq!(out, "wrapped");
        assert!(entered.lock().unwrap().iter().any(|n| n == "sealwiz.wrap"));
    }

    #[tokio::test]
    async fn spanned_run_survives_yield_across_await() {
        let layer = RecordingLayer::default();
        let entered = layer.entered.clone();
        let subscriber = registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        let span = tracing::info_span!("sealwiz.unwrap");
        let out = Spanned::new(span)
            .run(async {
                tokio::task::yield_now().await;
                tokio::task::yield_now().await;
                42
            })
            .await;

        assert_eq!(out, 42);
        assert!(
            entered
                .lock()
                .unwrap()
                .iter()
                .any(|n| n == "sealwiz.unwrap")
        );
    }
}
