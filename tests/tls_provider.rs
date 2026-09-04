//! The gRPC mTLS feature must name the rustls provider it wants.
//!
//! `opentelemetry-otlp` defines `tls = ["tls-ring"]`. The name reads as
//! provider-neutral and is not: enabling it selects ring. A consumer that has
//! standardised on aws-lc-rs then links both providers, or links the one it
//! rejected — and nothing in a build says so, because every feature name
//! involved is spelled the way you would spell the neutral one.
//!
//! Asserted on the manifest rather than at runtime. Which provider `rustls`
//! ends up with is decided at link time by Cargo feature unification across
//! the whole graph, so a runtime probe here would report this crate's own test
//! profile and say nothing about what a consumer resolves.

/// `grpc-mtls` names a provider explicitly, and it is not the ring alias.
#[test]
fn grpc_mtls_names_its_rustls_provider() {
    let manifest = include_str!("../Cargo.toml");
    let feature = manifest
        .lines()
        .find(|l| l.starts_with("grpc-mtls = ["))
        .expect("grpc-mtls feature is gone from Cargo.toml");

    assert!(
        feature.contains("opentelemetry-otlp/tls-aws-lc"),
        "grpc-mtls must select a provider by name; got: {feature}"
    );
    // The bare alias, exactly — `tls-aws-lc` and `tls-roots` both contain
    // `opentelemetry-otlp/tls` as a substring, so a looser check here would
    // fail on the correct manifest.
    assert!(
        !feature.contains("\"opentelemetry-otlp/tls\""),
        "`opentelemetry-otlp/tls` is an alias for tls-ring, not a neutral \
         choice — name the provider: {feature}"
    );
    assert!(
        !feature.contains("tls-ring"),
        "ring is not the provider this crate selects: {feature}"
    );
}
