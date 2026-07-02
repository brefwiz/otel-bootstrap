#![cfg(feature = "profiling")]

use std::error::Error;
use std::sync::OnceLock;

#[cfg(feature = "profiling-bridge-pyroscope-rs")]
use opentelemetry::trace::TraceContextExt;

/// Validate that a pyroscope endpoint targets only loopback (per ADR platform/0203 AC1).
/// Allowed: 127.0.0.1, ::1, localhost, unix socket paths.
/// Rejects routable addresses to prevent unauthenticated plaintext profile data leaving the pod.
fn validate_pyroscope_endpoint(endpoint: &str) -> Result<(), Box<dyn Error>> {
    use url::Url;

    // Unix socket paths are allowed
    if endpoint.starts_with("unix://") {
        return Ok(());
    }

    // HTTP/HTTPS endpoints must target loopback
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        let url = Url::parse(endpoint)?;

        // Reject endpoints with userinfo (user:pass@host) to prevent redirect attacks
        if !url.username().is_empty() || url.password().is_some() {
            return Err(format!(
                "pyroscope endpoint must not contain userinfo; got: {endpoint} (ADR platform/0203 AC1)"
            ).into());
        }

        let host = url.host_str().unwrap_or("");

        match host {
            "127.0.0.1" | "::1" | "[::1]" | "localhost" => Ok(()),
            _ => Err(format!(
                "pyroscope endpoint must target loopback (127.0.0.1, ::1, localhost, or unix socket); \
                 got: {endpoint} (ADR platform/0203 AC1)"
            ).into()),
        }
    } else {
        Err(
            format!("pyroscope endpoint must be http://, https://, or unix://; got: {endpoint}")
                .into(),
        )
    }
}

/// Profiling bridge handle. Owns the active profiling agent and ensures
/// graceful shutdown on drop.
pub struct ProfilingHandle {
    #[cfg(feature = "profiling-bridge-pyroscope-rs")]
    agent: Option<pyroscope::PyroscopeAgent<pyroscope::pyroscope::PyroscopeAgentRunning>>,
}

#[cfg(feature = "profiling-bridge-pyroscope-rs")]
impl Drop for ProfilingHandle {
    fn drop(&mut self) {
        if let Some(agent) = self.agent.take() {
            let _ = agent.stop();
        }
    }
}

#[cfg(feature = "profiling-bridge-pyroscope-rs")]
type BoxedTagFn = Box<dyn Fn(String, String) -> pyroscope::Result<()> + Send + Sync>;

/// Module-level storage for profiling tag functions (add_tag, remove_tag)
/// obtained from the running pyroscope agent.
#[cfg(feature = "profiling-bridge-pyroscope-rs")]
static PROFILING_TAG_FNS: OnceLock<(BoxedTagFn, BoxedTagFn)> = OnceLock::new();

/// Guards against starting more than one profiling agent per process.
/// The `pprof` backend keeps a single process-wide profiler guard, so a
/// second concurrent agent would fail to start; subsequent calls are
/// treated as no-ops rather than errors.
#[cfg(feature = "profiling-bridge-pyroscope-rs")]
static PROFILING_STARTED: OnceLock<()> = OnceLock::new();

/// Start the pyroscope profiling bridge.
///
/// The bridge pushes profiles over plain HTTP/loopback to a local SPIFFE-terminating
/// sidecar (or an already-mTLS'd endpoint reachable without client-side TLS material).
/// pyroscope-rs hardcodes its own HTTP client internally with no hook
/// for custom TLS/identity, so in-process mTLS is not possible; the sidecar carries
/// the workload identity upstream.
///
/// **Temporary exception** (Tracks #40): This bridge is a sunset-bound interim implementation
/// pending a native Rust OTLP profiles exporter. See ADR platform/0202 and issue #40.
#[cfg(feature = "profiling-bridge-pyroscope-rs")]
pub(crate) fn start_pyroscope_bridge(
    service_name: &str,
    pyroscope_endpoint: &str,
) -> Result<Option<ProfilingHandle>, Box<dyn Error>> {
    use pyroscope_pprofrs::{PprofConfig, pprof_backend};

    // Validate endpoint targets loopback only (ADR platform/0203 AC1)
    validate_pyroscope_endpoint(pyroscope_endpoint)?;

    // Only one profiling agent may run per process (the `pprof` backend holds
    // a single process-wide profiler guard); ignore subsequent start attempts.
    if PROFILING_STARTED.set(()).is_err() {
        return Ok(None);
    }

    let agent = pyroscope::PyroscopeAgent::builder(pyroscope_endpoint, service_name)
        .backend(pprof_backend(PprofConfig::new().sample_rate(100)))
        .build()?
        .start()?;

    let (add_tag, remove_tag) = agent.tag_wrapper();
    PROFILING_TAG_FNS
        .set((Box::new(add_tag), Box::new(remove_tag)))
        .ok();

    Ok(Some(ProfilingHandle { agent: Some(agent) }))
}

/// No-op bridge for when profiling is enabled but the pyroscope feature is not.
#[cfg(all(feature = "profiling", not(feature = "profiling-bridge-pyroscope-rs")))]
pub(crate) fn start_pyroscope_bridge(
    _service_name: &str,
    _pyroscope_endpoint: &str,
) -> Result<Option<ProfilingHandle>, Box<dyn Error>> {
    Ok(None)
}

/// Tracing layer that tags active span enter/exit with trace_id and span_id
/// in the running pyroscope agent.
#[cfg(feature = "profiling-bridge-pyroscope-rs")]
pub struct ProfilingTagLayer;

#[cfg(feature = "profiling-bridge-pyroscope-rs")]
impl<S> tracing_subscriber::Layer<S> for ProfilingTagLayer
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_enter(&self, _id: &tracing::span::Id, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        if let Some((add_tag, _)) = PROFILING_TAG_FNS.get() {
            let cx = opentelemetry::Context::current();
            let span_ref = cx.span();
            let span_context = span_ref.span_context();
            if span_context.is_valid() {
                let trace_id = span_context.trace_id();
                let span_id = span_context.span_id();
                let _ = add_tag("trace_id".to_string(), format!("{trace_id:x}"));
                let _ = add_tag("span_id".to_string(), format!("{span_id:x}"));
            }
        }
    }

    fn on_exit(&self, _id: &tracing::span::Id, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        if let Some((_, remove_tag)) = PROFILING_TAG_FNS.get() {
            let cx = opentelemetry::Context::current();
            let span_ref = cx.span();
            let span_context = span_ref.span_context();
            if span_context.is_valid() {
                let trace_id = span_context.trace_id();
                let span_id = span_context.span_id();
                let _ = remove_tag("trace_id".to_string(), format!("{trace_id:x}"));
                let _ = remove_tag("span_id".to_string(), format!("{span_id:x}"));
            }
        }
    }
}

#[cfg(all(test, feature = "profiling-bridge-pyroscope-rs"))]
mod tests {
    use super::*;

    #[test]
    fn start_bridge_with_nonexistent_server() {
        let result = start_pyroscope_bridge("test-svc", "http://localhost:4040");
        assert!(
            result.is_ok(),
            "pyroscope agent start() is lazy and does not eagerly connect"
        );
        if let Ok(Some(_handle)) = result {
            // Bridge is active
        }
    }

    #[test]
    fn start_bridge_multiple_times_ignores_second() {
        let result1 = start_pyroscope_bridge("test-svc-1", "http://localhost:4040");
        assert!(result1.is_ok());
        let result2 = start_pyroscope_bridge("test-svc-2", "http://localhost:4041");
        assert!(result2.is_ok());
        // Second call is a no-op: the `pprof` backend only supports one
        // process-wide profiler guard, so the bridge returns `Ok(None)`.
        assert!(result2.unwrap().is_none());
    }

    #[test]
    fn validate_endpoint_accepts_loopback_ipv4() {
        assert!(validate_pyroscope_endpoint("http://127.0.0.1:4040").is_ok());
    }

    #[test]
    fn validate_endpoint_accepts_loopback_ipv6() {
        // IPv6 literals in a URL authority must be bracketed (RFC 3986 §3.2.2).
        assert!(validate_pyroscope_endpoint("http://[::1]:4040").is_ok());
    }

    #[test]
    fn validate_endpoint_accepts_localhost() {
        assert!(validate_pyroscope_endpoint("http://localhost:4040").is_ok());
    }

    #[test]
    fn validate_endpoint_accepts_https_loopback() {
        assert!(validate_pyroscope_endpoint("https://127.0.0.1:4040").is_ok());
    }

    #[test]
    fn validate_endpoint_rejects_routable_ipv4() {
        assert!(validate_pyroscope_endpoint("http://10.0.0.1:4040").is_err());
    }

    #[test]
    fn validate_endpoint_rejects_userinfo_bypass() {
        // Userinfo bypass: attacker tries to use loopback as userinfo but target evil.com
        assert!(validate_pyroscope_endpoint("http://127.0.0.1:4040@evil.com/").is_err());
    }

    #[test]
    fn validate_endpoint_rejects_userinfo_with_password() {
        assert!(validate_pyroscope_endpoint("http://user:pass@localhost:4040").is_err());
    }

    #[test]
    fn validate_endpoint_rejects_unix_socket_check() {
        assert!(validate_pyroscope_endpoint("unix:///var/run/profiling.sock").is_ok());
    }
}

#[cfg(all(
    test,
    feature = "profiling",
    not(feature = "profiling-bridge-pyroscope-rs")
))]
mod tests_no_bridge {
    use super::*;

    #[test]
    fn start_bridge_returns_none() {
        let result = start_pyroscope_bridge("test-svc", "http://localhost:4040");
        assert!(result.is_ok());
        if let Ok(handle) = result {
            assert!(handle.is_none());
        }
    }
}
