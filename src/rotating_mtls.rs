//! Live-sourced mTLS for the gRPC OTLP exporters.
//!
//! [`MtlsMaterial`](crate::MtlsMaterial) is a snapshot: the tonic channel built
//! from it presents the same client certificate and trusts the same bundle for
//! the life of the process. A workload identity that rotates every hour, or a
//! process that starts before its identity provider is reachable, cannot live
//! with that. A [`CertSource`] is asked for the current material **each time a
//! new connection is made**, so a reconnect after rotation presents the rotated
//! certificate and trusts the rotated bundle, and a source that is not ready
//! yet only delays the first connection instead of silently downgrading it.

use std::error::Error;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use hyper_util::rt::TokioIo;
use rustls::RootCertStore;
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio::net::TcpStream;
use tonic::codegen::http::Uri;
use tonic::transport::{Channel, Endpoint};

use crate::MtlsMaterial;

type BoxError = Box<dyn Error + Send + Sync>;

/// A source of the mTLS material the OTLP exporters present and trust.
///
/// Implementations return whatever is current *now*. The exporters call
/// [`current`](CertSource::current) on every new connection, never once at
/// start-up, so an implementation backed by a rotating identity needs no
/// restart to be picked up.
pub trait CertSource: Send + Sync + 'static {
    /// The material to use for a connection made now, or `None` when the
    /// source has none yet (for example an identity agent that has not
    /// answered). A `None` fails that connection attempt; the channel retries
    /// with backoff, so telemetry starts flowing as soon as the source is ready.
    fn current(&self) -> Option<MtlsMaterial>;
}

/// A [`CertSource`] that always returns the same material. Equivalent to
/// [`TelemetryBuilder::with_mtls`](crate::TelemetryBuilder::with_mtls) for
/// callers that hold a `CertSource` generically.
#[derive(Clone)]
pub struct StaticCertSource(MtlsMaterial);

impl StaticCertSource {
    /// Wrap fixed material.
    #[must_use]
    pub fn new(material: MtlsMaterial) -> Self {
        Self(material)
    }
}

impl CertSource for StaticCertSource {
    fn current(&self) -> Option<MtlsMaterial> {
        Some(self.0.clone())
    }
}

/// Build the rustls client configuration for one connection from `material`.
///
/// The trust bundle may hold several certificates; every one becomes an anchor.
fn client_config(material: &MtlsMaterial) -> Result<rustls::ClientConfig, BoxError> {
    let mut roots = RootCertStore::empty();
    for ca in CertificateDer::pem_slice_iter(&material.trust_bundle_pem) {
        roots.add(ca?)?;
    }
    if roots.is_empty() {
        return Err("mTLS trust bundle holds no certificates".into());
    }
    let chain = CertificateDer::pem_slice_iter(&material.client_cert_chain_pem)
        .collect::<Result<Vec<_>, _>>()?;
    let key = PrivateKeyDer::from_pem_slice(&material.client_key_pem)?;

    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_client_auth_cert(chain, key)?;
    // gRPC requires HTTP/2; a server that does not agree it is refused by the
    // transport, so say so up front.
    config.alpn_protocols = vec![b"h2".to_vec()];
    Ok(config)
}

/// Dials the endpoint's host and runs mutual TLS with whatever the
/// [`CertSource`] holds at that moment.
#[derive(Clone)]
pub(crate) struct RotatingConnector {
    source: Arc<dyn CertSource>,
}

type TlsIo = TokioIo<tokio_rustls::client::TlsStream<TcpStream>>;

impl RotatingConnector {
    async fn connect(source: Arc<dyn CertSource>, uri: Uri) -> Result<TlsIo, BoxError> {
        let host = uri.host().ok_or("OTLP endpoint has no host")?.to_owned();
        let port = uri.port_u16().unwrap_or(443);
        let material = source
            .current()
            .ok_or("mTLS material is not available yet")?;
        let config = client_config(&material)?;
        let tcp = TcpStream::connect((host.as_str(), port)).await?;
        tcp.set_nodelay(true)?;
        let name = ServerName::try_from(host)?;
        let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(name, tcp)
            .await?;
        Ok(TokioIo::new(tls))
    }
}

impl tower::Service<Uri> for RotatingConnector {
    type Response = TlsIo;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<TlsIo, BoxError>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        Box::pin(Self::connect(Arc::clone(&self.source), uri))
    }
}

/// A lazily connecting channel to `endpoint` whose every connection takes its
/// material from `source`. Must be called inside a Tokio runtime.
///
/// The endpoint's scheme is ignored: the connector always speaks TLS, and
/// tonic itself is handed an `http` URI so it does not try to layer its own.
pub(crate) fn channel(
    endpoint: &str,
    source: Arc<dyn CertSource>,
    timeout: Option<Duration>,
) -> Result<Channel, BoxError> {
    let parsed: Uri = endpoint.parse()?;
    let authority = parsed
        .authority()
        .ok_or("OTLP endpoint has no host")?
        .as_str();
    let mut ep = Endpoint::from_shared(format!("http://{authority}"))?
        .connect_timeout(Duration::from_secs(10));
    if let Some(t) = timeout {
        ep = ep.timeout(t);
    }
    Ok(ep.connect_with_connector_lazy(RotatingConnector { source }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;
    use tower::Service as _;

    // Fixed test PKI under tests/fixtures/mtls (EC P-256, 100-year validity,
    // throwaway keys that protect nothing): a CA, a server leaf for
    // `localhost`, two client leaves, and a CA that signed none of them.
    const CA: &str = include_str!("../tests/fixtures/mtls/ca.pem");
    const STRANGER_CA: &str = include_str!("../tests/fixtures/mtls/stranger-ca.pem");
    const SERVER_CERT: &str = include_str!("../tests/fixtures/mtls/server.pem");
    const SERVER_KEY: &str = include_str!("../tests/fixtures/mtls/server.key");
    const CLIENT_ONE_CERT: &str = include_str!("../tests/fixtures/mtls/client-one.pem");
    const CLIENT_ONE_KEY: &str = include_str!("../tests/fixtures/mtls/client-one.key");
    const CLIENT_TWO_CERT: &str = include_str!("../tests/fixtures/mtls/client-two.pem");
    const CLIENT_TWO_KEY: &str = include_str!("../tests/fixtures/mtls/client-two.key");

    fn material(cert: &str, key: &str, bundle: &str) -> MtlsMaterial {
        MtlsMaterial {
            client_cert_chain_pem: cert.as_bytes().to_vec(),
            client_key_pem: key.as_bytes().to_vec(),
            trust_bundle_pem: bundle.as_bytes().to_vec(),
        }
    }

    fn client_one() -> MtlsMaterial {
        material(CLIENT_ONE_CERT, CLIENT_ONE_KEY, CA)
    }

    fn client_two() -> MtlsMaterial {
        material(CLIENT_TWO_CERT, CLIENT_TWO_KEY, CA)
    }

    /// A TLS server that requires a client certificate signed by the CA and
    /// records the leaf it was shown on every connection.
    async fn server() -> (u16, Arc<Mutex<Vec<Vec<u8>>>>) {
        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from_pem_slice(CA.as_bytes()).unwrap())
            .unwrap();
        let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
            Arc::new(roots),
            Arc::new(rustls::crypto::aws_lc_rs::default_provider()),
        )
        .build()
        .unwrap();
        let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            CertificateDer::pem_slice_iter(SERVER_CERT.as_bytes())
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            PrivateKeyDer::from_pem_slice(SERVER_KEY.as_bytes()).unwrap(),
        )
        .unwrap();
        config.alpn_protocols = vec![b"h2".to_vec()];
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_task = Arc::clone(&seen);
        tokio::spawn(async move {
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                let acceptor = acceptor.clone();
                let seen = Arc::clone(&seen_task);
                tokio::spawn(async move {
                    if let Ok(mut tls) = acceptor.accept(tcp).await {
                        let leaf = tls
                            .get_ref()
                            .1
                            .peer_certificates()
                            .and_then(|c| c.first())
                            .map(|c| c.as_ref().to_vec());
                        if let Some(leaf) = leaf {
                            seen.lock().unwrap().push(leaf);
                        }
                        let mut b = [0_u8; 1];
                        let _ = tls.read(&mut b).await;
                        let _ = tls.shutdown().await;
                    }
                });
            }
        });
        (port, seen)
    }

    struct Swappable(Mutex<Option<MtlsMaterial>>);
    impl CertSource for Swappable {
        fn current(&self) -> Option<MtlsMaterial> {
            self.0.lock().unwrap().clone()
        }
    }

    fn uri(port: u16) -> Uri {
        format!("http://localhost:{port}").parse().unwrap()
    }

    fn pem_to_der(pem: &[u8]) -> Vec<u8> {
        CertificateDer::from_pem_slice(pem).unwrap().to_vec()
    }

    #[tokio::test]
    async fn each_new_connection_presents_the_material_current_at_that_moment() {
        let (port, seen) = server().await;
        let source = Arc::new(Swappable(Mutex::new(Some(client_one()))));
        let mut connector = RotatingConnector {
            source: Arc::clone(&source) as Arc<dyn CertSource>,
        };

        std::future::poll_fn(|cx| connector.poll_ready(cx))
            .await
            .expect("connector is always ready");
        drop(connector.call(uri(port)).await.expect("first connection"));

        *source.0.lock().unwrap() = Some(client_two());
        drop(connector.call(uri(port)).await.expect("rotated connection"));

        tokio::time::timeout(Duration::from_secs(5), async {
            while seen.lock().unwrap().len() < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("server saw both clients");
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0], pem_to_der(CLIENT_ONE_CERT.as_bytes()));
        assert_eq!(seen[1], pem_to_der(CLIENT_TWO_CERT.as_bytes()));
    }

    #[tokio::test]
    async fn a_source_not_ready_at_first_connect_fails_that_attempt_then_connects_once_ready() {
        let (port, seen) = server().await;
        let source = Arc::new(Swappable(Mutex::new(None)));
        let mut connector = RotatingConnector {
            source: Arc::clone(&source) as Arc<dyn CertSource>,
        };

        let err = connector
            .call(uri(port))
            .await
            .err()
            .expect("an unready source must fail the attempt");
        assert!(err.to_string().contains("not available yet"), "{err}");
        assert!(seen.lock().unwrap().is_empty());

        *source.0.lock().unwrap() = Some(client_one());
        drop(
            connector
                .call(uri(port))
                .await
                .expect("connects once ready"),
        );
    }

    #[tokio::test]
    async fn a_bundle_of_several_certificates_trusts_each_of_them() {
        let (port, _seen) = server().await;
        // The issuing root is the second certificate of the bundle, as it is
        // when a trust domain holds a root alongside a successor.
        let bundle = format!("{STRANGER_CA}{CA}");
        let mut connector = RotatingConnector {
            source: Arc::new(StaticCertSource::new(material(
                CLIENT_ONE_CERT,
                CLIENT_ONE_KEY,
                &bundle,
            ))),
        };
        drop(
            connector
                .call(uri(port))
                .await
                .expect("second anchor trusted"),
        );
    }

    #[tokio::test]
    async fn a_server_the_bundle_does_not_vouch_for_is_refused() {
        let (port, _seen) = server().await;
        let mut connector = RotatingConnector {
            source: Arc::new(StaticCertSource::new(material(
                CLIENT_ONE_CERT,
                CLIENT_ONE_KEY,
                STRANGER_CA,
            ))),
        };
        assert!(connector.call(uri(port)).await.is_err());
    }

    #[test]
    fn malformed_material_is_an_error_not_a_panic() {
        let bad = MtlsMaterial {
            client_cert_chain_pem: b"x".to_vec(),
            client_key_pem: b"x".to_vec(),
            trust_bundle_pem: Vec::new(),
        };
        assert!(
            client_config(&bad)
                .unwrap_err()
                .to_string()
                .contains("no certificates")
        );
    }

    #[test]
    fn static_source_returns_its_material() {
        let m = client_one();
        let got = StaticCertSource::new(m.clone()).current().unwrap();
        assert_eq!(got.client_cert_chain_pem, m.client_cert_chain_pem);
    }

    #[tokio::test]
    async fn channel_rejects_an_endpoint_without_a_host() {
        let source: Arc<dyn CertSource> = Arc::new(Swappable(Mutex::new(None)));
        assert!(channel("not a uri", Arc::clone(&source), None).is_err());
        assert!(
            channel(
                "https://collector.example:4320",
                source,
                Some(Duration::from_secs(3))
            )
            .is_ok()
        );
    }
}
