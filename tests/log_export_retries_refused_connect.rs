// SPDX-License-Identifier: MIT
//! A log batch whose first connect is refused still reaches the collector.
//!
//! A pod's first export can race the network programming the route to the
//! collector's Service. The attempt is refused, and the batch it carried is
//! the service's boot-time logs: with one attempt per batch they are gone for
//! good. Here nothing listens when the log is exported and a collector binds
//! shortly after; the retry must deliver it.

#![cfg(feature = "grpc")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use opentelemetry_proto::tonic::collector::logs::v1::logs_service_server::{
    LogsService, LogsServiceServer,
};
use opentelemetry_proto::tonic::collector::logs::v1::{
    ExportLogsServiceRequest, ExportLogsServiceResponse,
};
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use otel_bootstrap::{ExportProtocol, Telemetry};

#[derive(Clone, Default)]
struct Collector(Arc<Mutex<Vec<String>>>);

#[tonic::async_trait]
impl LogsService for Collector {
    async fn export(
        &self,
        request: tonic::Request<ExportLogsServiceRequest>,
    ) -> Result<tonic::Response<ExportLogsServiceResponse>, tonic::Status> {
        let bodies = request
            .into_inner()
            .resource_logs
            .into_iter()
            .flat_map(|r| r.scope_logs)
            .flat_map(|s| s.log_records)
            .filter_map(|record| match record.body?.value? {
                Value::StringValue(text) => Some(text),
                _ => None,
            });
        self.0.lock().unwrap().extend(bodies);
        Ok(tonic::Response::new(ExportLogsServiceResponse::default()))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_first_connect_is_retried_until_the_collector_answers() {
    // Reserve a port, then free it: nothing listens until the collector binds.
    let addr = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap()
    };
    // SAFETY: this test binary holds one test, run in its own process.
    unsafe {
        std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", format!("http://{addr}"));
        std::env::set_var("RUST_LOG", "info");
    }
    let handles = Telemetry::builder("log-retry-test")
        .with_protocol(ExportProtocol::Grpc)
        .with_metrics(false)
        .with_logs(true)
        .init()
        .expect("telemetry init");

    tracing::info!("logged-before-the-collector-listened");

    let collector = Collector::default();
    let served = collector.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let _ = tonic::transport::Server::builder()
            .add_service(LogsServiceServer::new(served))
            .serve_with_incoming(tonic::transport::server::TcpIncoming::from(listener))
            .await;
    });

    let logger_provider = handles.logger_provider.clone().expect("logs enabled");
    tokio::task::spawn_blocking(move || {
        let _ = logger_provider.force_flush();
    })
    .await
    .unwrap();

    let received = collector.0.lock().unwrap().clone();
    assert!(
        received
            .iter()
            .any(|body| body == "logged-before-the-collector-listened"),
        "the log was dropped by the refused first attempt: {received:?}"
    );
}
