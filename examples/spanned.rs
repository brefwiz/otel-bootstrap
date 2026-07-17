// SPDX-License-Identifier: MIT
//! Await-safe alternative to `let _guard = span.enter();` using
//! [`otel_bootstrap::Spanned`] / [`otel_bootstrap::in_span`].
//!
//! Shows the pattern a `*_span(...)`-style attribute-recording helper (like
//! sealwiz-core's `telemetry::wrap_span`) should hand its caller: build the
//! span with attributes up front, then run the async work under it via
//! `Instrument` instead of returning a bare `Span` for `.enter()`.
//!
//! Run with:
//! ```text
//! cargo run --example spanned --features testing
//! ```

use otel_bootstrap::{Spanned, in_span};

async fn load_master() -> Result<&'static str, &'static str> {
    Ok("master-key")
}

#[tokio::main]
async fn main() {
    let handles = otel_bootstrap::Telemetry::testing("spanned-example");

    // Free-function form.
    let span = tracing::info_span!("sealwiz.wrap", sealwiz.namespace = "ns-1");
    let master = in_span(span, load_master()).await.expect("load succeeds");
    println!("{master}");

    // Builder form — reads better at multi-line call sites.
    let span = tracing::info_span!("sealwiz.unwrap", sealwiz.namespace = "ns-1");
    let master = Spanned::new(span)
        .run(load_master())
        .await
        .expect("load succeeds");
    println!("{master}");

    handles.shutdown().unwrap();
}
