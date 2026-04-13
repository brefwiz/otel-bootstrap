//! End-to-end test: verifies that traces exported via OTLP gRPC reach the
//! OpenTelemetry Collector and contain the expected resource attributes.
//!
//! # Prerequisites
//!
//! ```bash
//! docker compose up -d   # start fresh collector
//! ```
//!
//! # Running
//!
//! ```bash
//! cargo test --features integration-tests --test e2e
//! ```

#![cfg(feature = "integration-tests")]

use serde_json::Value;
use std::time::Duration;

const TRACES_FILE: &str = "collector-output/traces.jsonl";

/// Wait for the collector to accept gRPC connections on :4317.
fn wait_for_collector() {
    use std::net::TcpStream;
    for _ in 0..60 {
        if TcpStream::connect("127.0.0.1:4317").is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    panic!("collector not accepting connections on :4317 within 30 seconds");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn traces_contain_enriched_resource_attributes() {
    wait_for_collector();

    // Bootstrap telemetry — exports to localhost:4317 (the collector)
    let handles =
        otel_bootstrap::init_telemetry("e2e-test-service").expect("init_telemetry failed");

    // Emit a span via the OpenTelemetry API directly
    use opentelemetry::trace::{Tracer, TracerProvider};
    let tracer = handles.tracer_provider.tracer("e2e-test");
    tracer.in_span("e2e_test_operation", |_cx| {});

    // Flush the batch exporter by shutting down both providers
    handles.shutdown().expect("shutdown failed");

    // Give the collector time to flush to disk (flush_interval: 1s in config)
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Read and parse the exported traces
    let raw = std::fs::read_to_string(TRACES_FILE)
        .expect("failed to read traces file — is the collector running?");
    assert!(
        !raw.trim().is_empty(),
        "collector wrote no trace data — is it running?"
    );

    // The file exporter writes one JSON object per line (JSONL)
    let mut found_service = false;
    let mut found_host = false;
    let mut found_pid = false;

    for line in raw.lines() {
        let Ok(doc) = serde_json::from_str::<Value>(line) else {
            continue; // skip partial or empty lines
        };

        // Navigate: resourceSpans[].resource.attributes[]
        let resource_spans = doc
            .pointer("/resourceSpans")
            .or_else(|| doc.pointer("/resource_spans"))
            .and_then(|v| v.as_array());

        let Some(spans) = resource_spans else {
            continue;
        };

        for rs in spans {
            let attrs = rs
                .pointer("/resource/attributes")
                .and_then(|v| v.as_array());

            let Some(attrs) = attrs else { continue };

            for attr in attrs {
                let key = attr.get("key").and_then(|k| k.as_str()).unwrap_or("");
                match key {
                    "service.name" => {
                        let val = attr
                            .pointer("/value/stringValue")
                            .or_else(|| attr.pointer("/value/string_value"))
                            .and_then(|v| v.as_str());
                        assert_eq!(val, Some("e2e-test-service"), "wrong service.name");
                        found_service = true;
                    }
                    "host.name" => {
                        let val = attr
                            .pointer("/value/stringValue")
                            .or_else(|| attr.pointer("/value/string_value"))
                            .and_then(|v| v.as_str());
                        assert!(
                            val.is_some_and(|v| !v.is_empty()),
                            "host.name should be non-empty"
                        );
                        found_host = true;
                    }
                    "process.pid" => {
                        let val = attr
                            .pointer("/value/intValue")
                            .or_else(|| attr.pointer("/value/int_value"));
                        assert!(val.is_some(), "process.pid should have an int value");
                        found_pid = true;
                    }
                    _ => {}
                }
            }
        }
    }

    assert!(found_service, "service.name not found in exported traces");
    assert!(found_host, "host.name not found in exported traces");
    assert!(found_pid, "process.pid not found in exported traces");
}
