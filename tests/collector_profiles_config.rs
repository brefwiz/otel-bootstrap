use serde_yaml::Value;

#[test]
fn test_profiles_pipeline_in_collector_config() {
    // Read and parse the collector config
    let config_yaml = include_str!("../collector-config.yaml");
    let config: Value =
        serde_yaml::from_str(config_yaml).expect("collector-config.yaml must be valid YAML");

    // Assert profiles pipeline exists
    let service = config["service"]
        .as_mapping()
        .expect("service section must exist");
    let pipelines = service["pipelines"]
        .as_mapping()
        .expect("service.pipelines must exist");

    let profiles_pipeline = pipelines
        .get("profiles")
        .expect("service.pipelines.profiles must exist");

    // Assert profiles pipeline receivers contain otlp
    let receivers = profiles_pipeline["receivers"]
        .as_sequence()
        .expect("profiles pipeline must have receivers");
    assert!(
        receivers.iter().any(|r| r.as_str() == Some("otlp")),
        "profiles pipeline receivers must contain 'otlp'"
    );

    // Assert profiles pipeline exporters contain otlp/profiles
    let exporters = profiles_pipeline["exporters"]
        .as_sequence()
        .expect("profiles pipeline must have exporters");
    let has_otlp_profiles = exporters
        .iter()
        .any(|e| e.as_str() == Some("otlp/profiles"));
    assert!(
        has_otlp_profiles,
        "profiles pipeline exporters must contain 'otlp/profiles'"
    );

    // Assert otlp/profiles exporter exists at top level
    let exporters_section = config["exporters"]
        .as_mapping()
        .expect("exporters section must exist");
    let profiles_exporter = exporters_section
        .get("otlp/profiles")
        .expect("exporters.otlp/profiles must exist");

    // Assert the exporter targets the pyroscope hostname
    let endpoint = profiles_exporter["endpoint"]
        .as_str()
        .expect("otlp/profiles exporter must have endpoint");
    assert!(
        endpoint.contains("pyroscope"),
        "otlp/profiles exporter endpoint must target the compose-internal 'pyroscope' hostname, got: {}",
        endpoint
    );

    // Regression: ensure traces pipeline still exists and uses file/traces exporter
    let traces_pipeline = pipelines
        .get("traces")
        .expect("traces pipeline must still exist");
    let traces_exporters = traces_pipeline["exporters"]
        .as_sequence()
        .expect("traces pipeline must have exporters");
    assert!(
        traces_exporters
            .iter()
            .any(|e| e.as_str() == Some("file/traces")),
        "traces pipeline exporters must still contain 'file/traces'"
    );

    // Regression: ensure metrics pipeline still exists and uses file/metrics exporter
    let metrics_pipeline = pipelines
        .get("metrics")
        .expect("metrics pipeline must still exist");
    let metrics_exporters = metrics_pipeline["exporters"]
        .as_sequence()
        .expect("metrics pipeline must have exporters");
    assert!(
        metrics_exporters
            .iter()
            .any(|e| e.as_str() == Some("file/metrics")),
        "metrics pipeline exporters must still contain 'file/metrics'"
    );
}
