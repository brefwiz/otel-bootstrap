// SPDX-License-Identifier: MIT
//! A boot that completes before telemetry comes up (an init deferred past
//! ready) still exports its phases and a root ending where boot ended.

#![cfg(feature = "grpc")]

mod boot_collector;

use std::time::Duration;

use boot_collector::{named, nanos, string_attribute, telemetry_exporting_to_collector};
use otel_bootstrap::boot::{self, Timeline};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_boot_ready_before_init_exports_its_root_at_ready() {
    let value: Result<u8, ()> = boot::try_time("config", async { Ok(7) }).await;
    assert_eq!(value, Ok(7));
    boot::ready();
    let ready_by = std::time::SystemTime::now();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (collector, handles) = telemetry_exporting_to_collector("boot-ready-first-test").await;
    handles.shutdown().unwrap();

    let spans = collector.spans();
    let roots = named(&spans, "boot");
    assert_eq!(roots.len(), 1, "{spans:#?}");
    let root = roots[0];
    assert_eq!(string_attribute(root, "boot.outcome"), Some("ready"));
    assert!(root.end_time_unix_nano <= nanos(ready_by));

    let phase = &Timeline::global().phases()[0];
    let config = named(&spans, "config");
    assert_eq!(config.len(), 1);
    assert_eq!(config[0].parent_span_id, root.span_id);
    assert_eq!(config[0].end_time_unix_nano, nanos(phase.end));
}
