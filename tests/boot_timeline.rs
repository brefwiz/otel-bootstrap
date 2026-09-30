// SPDX-License-Identifier: MIT
//! Boot phases recorded before telemetry exists reach a real OTLP collector as
//! spans carrying the times the phases actually ran, under one `boot` root.
//!
//! One test per file: the timeline is process-global, and nextest runs each
//! test binary's tests in their own process.

#![cfg(feature = "grpc")]

mod boot_collector;

use std::time::Duration;

use boot_collector::{named, nanos, string_attribute, telemetry_exporting_to_collector};
use opentelemetry_proto::tonic::trace::v1::status::StatusCode;
use otel_bootstrap::boot::{self, Timeline};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn phases_are_exported_once_with_their_original_timestamps() {
    boot::time("identity", tokio::time::sleep(Duration::from_millis(30))).await;
    boot::phase("config").fail();
    let before_init = Timeline::global().phases();
    // Telemetry comes up well after the phases ended, so a span stamped at
    // export time could not match.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (collector, handles) = telemetry_exporting_to_collector("boot-timeline-test").await;
    boot::phase("broker-connect").finish();
    boot::ready();

    assert_eq!(
        Timeline::global().flush(&handles.tracer_provider, "someone-else"),
        0,
        "a second flush exports nothing"
    );
    let (_, second) = telemetry_exporting_to_collector("second-init").await;
    drop(second);
    handles.shutdown().unwrap();

    let spans = collector.spans();
    let roots = named(&spans, "boot");
    assert_eq!(roots.len(), 1, "one root span: {spans:#?}");
    let root = roots[0];
    assert_eq!(string_attribute(root, "boot.outcome"), Some("ready"));

    let all = Timeline::global().phases();
    assert_eq!(all.len(), 3, "{all:?}");
    for phase in &all {
        let exported = named(&spans, &phase.name);
        assert_eq!(exported.len(), 1, "{} exported exactly once", phase.name);
        let span = exported[0];
        assert_eq!(span.trace_id, root.trace_id);
        assert_eq!(span.parent_span_id, root.span_id);
        assert_eq!(
            span.start_time_unix_nano,
            nanos(phase.start),
            "{}",
            phase.name
        );
        assert_eq!(span.end_time_unix_nano, nanos(phase.end), "{}", phase.name);
        assert_eq!(
            string_attribute(span, "boot.outcome"),
            Some(phase.outcome.as_str())
        );
    }
    assert_eq!(&all[..2], &before_init[..]);

    let failed = named(&spans, "config")[0];
    assert_eq!(
        failed.status.as_ref().map(|status| status.code),
        Some(StatusCode::Error as i32)
    );
    assert!(root.start_time_unix_nano <= nanos(all[0].start));
    assert!(root.end_time_unix_nano >= nanos(all[2].end));
}
