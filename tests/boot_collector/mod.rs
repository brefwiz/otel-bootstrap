// SPDX-License-Identifier: MIT
//! An in-process OTLP/gRPC trace collector for the boot timeline tests.

// Each test binary includes this module and uses a different subset of it.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use opentelemetry_proto::tonic::collector::trace::v1::trace_service_server::{
    TraceService, TraceServiceServer,
};
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use opentelemetry_proto::tonic::common::v1::any_value;
use opentelemetry_proto::tonic::trace::v1::Span;
use otel_bootstrap::{ExportProtocol, Telemetry, TelemetryHandles, TraceSampler};

#[derive(Clone, Default)]
pub struct Collector {
    spans: Arc<Mutex<Vec<Span>>>,
}

impl Collector {
    pub fn spans(&self) -> Vec<Span> {
        self.spans.lock().unwrap().clone()
    }
}

#[tonic::async_trait]
impl TraceService for Collector {
    async fn export(
        &self,
        request: tonic::Request<ExportTraceServiceRequest>,
    ) -> Result<tonic::Response<ExportTraceServiceResponse>, tonic::Status> {
        let spans = request
            .into_inner()
            .resource_spans
            .into_iter()
            .flat_map(|resource| resource.scope_spans)
            .flat_map(|scope| scope.spans);
        self.spans.lock().unwrap().extend(spans);
        Ok(tonic::Response::new(ExportTraceServiceResponse::default()))
    }
}

/// Start a collector and initialise telemetry exporting to it over gRPC.
pub async fn telemetry_exporting_to_collector(service: &str) -> (Collector, TelemetryHandles) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let collector = Collector::default();
    let server = tonic::transport::Server::builder()
        .add_service(TraceServiceServer::new(collector.clone()))
        .serve_with_incoming(tonic::transport::server::TcpIncoming::from(listener));
    tokio::spawn(server);

    // SAFETY: each test binary here holds one test, run in its own process.
    unsafe {
        std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", &endpoint);
    }
    let handles = Telemetry::builder(service)
        .with_protocol(ExportProtocol::Grpc)
        .with_sampler(TraceSampler::AlwaysOn)
        .with_metrics(false)
        .init()
        .expect("telemetry init");
    (collector, handles)
}

pub fn nanos(time: SystemTime) -> u64 {
    u64::try_from(time.duration_since(UNIX_EPOCH).unwrap().as_nanos()).unwrap()
}

pub fn string_attribute<'s>(span: &'s Span, key: &str) -> Option<&'s str> {
    span.attributes
        .iter()
        .find(|attribute| attribute.key == key)
        .and_then(|attribute| attribute.value.as_ref())
        .and_then(|value| match &value.value {
            Some(any_value::Value::StringValue(text)) => Some(text.as_str()),
            _ => None,
        })
}

pub fn named<'s>(spans: &'s [Span], name: &str) -> Vec<&'s Span> {
    spans.iter().filter(|span| span.name == name).collect()
}
