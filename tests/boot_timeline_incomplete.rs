// SPDX-License-Identifier: MIT
//! A boot that never reaches ready still exports its root span when telemetry
//! shuts down, marked incomplete, so a crash during boot is visible as one.

#![cfg(feature = "grpc")]

mod boot_collector;

use boot_collector::{named, string_attribute, telemetry_exporting_to_collector};
use otel_bootstrap::boot;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_before_ready_exports_an_incomplete_root() {
    let (collector, handles) = telemetry_exporting_to_collector("boot-incomplete-test").await;
    boot::phase("config").finish();
    handles.shutdown().unwrap();

    let spans = collector.spans();
    let roots = named(&spans, "boot");
    assert_eq!(roots.len(), 1, "{spans:#?}");
    assert_eq!(
        string_attribute(roots[0], "boot.outcome"),
        Some("incomplete")
    );
    assert_eq!(named(&spans, "config")[0].parent_span_id, roots[0].span_id);
}
