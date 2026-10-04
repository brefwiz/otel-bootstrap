// SPDX-License-Identifier: MIT
//! A span batch whose first connect is refused still reaches the collector.
//!
//! Nothing listens when the span is exported; a real OTLP trace collector
//! binds shortly after and the retry must deliver it.

#![cfg(feature = "grpc")]

mod boot_collector;

use std::time::Duration;

use boot_collector::{Collector, named};
use opentelemetry_proto::tonic::collector::trace::v1::trace_service_server::TraceServiceServer;
use otel_bootstrap::{ExportProtocol, Telemetry, TraceSampler};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_first_connect_is_retried_until_the_collector_answers() {
    let addr = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap()
    };
    // SAFETY: this test binary holds one test, run in its own process.
    unsafe {
        std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", format!("http://{addr}"));
        std::env::set_var("RUST_LOG", "info");
    }
    let handles = Telemetry::builder("span-retry-test")
        .with_protocol(ExportProtocol::Grpc)
        .with_sampler(TraceSampler::AlwaysOn)
        .with_metrics(false)
        .init()
        .expect("telemetry init");

    tracing::info_span!("span-before-the-collector-listened").in_scope(|| {});

    let collector = Collector::default();
    let served = collector.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let _ = tonic::transport::Server::builder()
            .add_service(TraceServiceServer::new(served))
            .serve_with_incoming(tonic::transport::server::TcpIncoming::from(listener))
            .await;
    });

    let tracer_provider = handles.tracer_provider.clone();
    tokio::task::spawn_blocking(move || {
        let _ = tracer_provider.force_flush();
    })
    .await
    .unwrap();

    let spans = collector.spans();
    assert_eq!(
        named(&spans, "span-before-the-collector-listened").len(),
        1,
        "the span was dropped by the refused first attempt: {spans:#?}"
    );
}
