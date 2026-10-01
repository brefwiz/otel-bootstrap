//! The gRPC exporters accept both mTLS shapes: a fixed snapshot and a live
//! [`CertSource`]. Each runs in its own process (nextest), so each may install
//! the global providers.

#![cfg(feature = "grpc-mtls")]

use std::sync::Arc;

use otel_bootstrap::{CertSource, MtlsMaterial, StaticCertSource, Telemetry};

const CA: &str = include_str!("fixtures/mtls/ca.pem");
const CLIENT_CERT: &str = include_str!("fixtures/mtls/client-one.pem");
const CLIENT_KEY: &str = include_str!("fixtures/mtls/client-one.key");

/// A closed port: these tests build exporters, they never need a collector, and
/// must not export into the one the e2e tests read back from.
const NOWHERE: &str = "https://127.0.0.1:1";

fn material() -> MtlsMaterial {
    MtlsMaterial {
        client_cert_chain_pem: CLIENT_CERT.as_bytes().to_vec(),
        client_key_pem: CLIENT_KEY.as_bytes().to_vec(),
        trust_bundle_pem: CA.as_bytes().to_vec(),
    }
}

#[test]
fn material_debug_never_prints_key_bytes() {
    let rendered = format!("{:?}", material());
    assert!(!rendered.contains("BEGIN"));
    assert!(rendered.contains("redacted"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_snapshot_builds_all_three_exporters() {
    let handles = Telemetry::builder("mtls-snapshot")
        .with_logs(true)
        .with_default_endpoint(NOWHERE)
        .with_mtls(material())
        .init()
        .expect("snapshot mTLS init");
    assert!(handles.meter_provider.is_some());
    assert!(handles.logger_provider.is_some());
}

/// A source with nothing to offer yet must not fail `init`: the channel connects
/// lazily and retries, so telemetry starts when the source is ready.
#[tokio::test(flavor = "multi_thread")]
async fn a_live_source_that_is_not_ready_does_not_fail_init() {
    struct NotReady;
    impl CertSource for NotReady {
        fn current(&self) -> Option<MtlsMaterial> {
            None
        }
    }
    let handles = Telemetry::builder("mtls-live-not-ready")
        .with_logs(true)
        .with_default_endpoint(NOWHERE)
        .with_mtls_source(Arc::new(NotReady))
        .init()
        .expect("live mTLS init with an unready source");
    assert!(handles.meter_provider.is_some());
    assert!(handles.logger_provider.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_live_source_replaces_an_earlier_snapshot() {
    Telemetry::builder("mtls-live-replaces")
        .with_default_endpoint(NOWHERE)
        .with_mtls(material())
        .with_mtls_source(Arc::new(StaticCertSource::new(material())))
        .init()
        .expect("live mTLS init");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_snapshot_replaces_an_earlier_live_source() {
    Telemetry::builder("mtls-snapshot-replaces")
        .with_default_endpoint(NOWHERE)
        .with_mtls_source(Arc::new(StaticCertSource::new(material())))
        .with_default_endpoint(NOWHERE)
        .with_mtls(material())
        .init()
        .expect("snapshot mTLS init");
}
