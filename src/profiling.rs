#![cfg(feature = "profiling")]

use std::error::Error;
use std::sync::OnceLock;

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

/// Identity attached to every profile this process uploads.
///
/// Pyroscope stores a profile series per tag set. Without these, every replica
/// of a service collapses into one unlabelled series: you cannot tell two pods
/// apart, cannot follow one pod across a restart, and cannot line a profile up
/// against the logs and metrics for the same instance.
///
/// Field names deliberately match the resource attributes exported on logs and
/// traces (`host_name`, `deployment_environment`, `service_version`) so the
/// same value joins across all three signals without translation.
#[derive(Debug, Clone, Default)]
pub(crate) struct ProfilingIdentity {
    /// Host name — the pod name under Kubernetes.
    pub host_name: Option<String>,
    /// Deployment environment, e.g. `prod`.
    pub deployment_environment: Option<String>,
    /// Service version.
    pub service_version: Option<String>,
}

#[cfg(feature = "profiling-bridge-pyroscope-rs")]
impl ProfilingIdentity {
    /// Flatten to the `(key, value)` pairs the pyroscope builder takes.
    ///
    /// Absent fields are omitted rather than emitted empty: an empty tag value
    /// still forks the series, which is the precise problem this exists to
    /// avoid.
    fn tag_pairs(&self) -> Vec<(&'static str, &str)> {
        let mut pairs = Vec::new();
        if let Some(host) = self.host_name.as_deref().filter(|s| !s.is_empty()) {
            pairs.push(("host_name", host));
        }
        if let Some(env) = self
            .deployment_environment
            .as_deref()
            .filter(|s| !s.is_empty())
        {
            pairs.push(("deployment_environment", env));
        }
        if let Some(version) = self.service_version.as_deref().filter(|s| !s.is_empty()) {
            pairs.push(("service_version", version));
        }
        pairs
    }
}

/// Profiling bridge handle. Owns the active profiling agents and ensures
/// graceful shutdown on drop.
pub struct ProfilingHandle {
    /// CPU profiler (`pprof` backend).
    #[cfg(feature = "profiling-bridge-pyroscope-rs")]
    agent: Option<pyroscope::PyroscopeAgent<pyroscope::pyroscope::PyroscopeAgentRunning>>,
    /// Heap profiler (jemalloc backend).
    ///
    /// A separate agent because `PyroscopeAgentBuilder` takes exactly one
    /// backend, and the two sample different things: `pprof` samples on-CPU
    /// time, jemalloc samples allocations. A process stalled off-CPU produces
    /// an empty CPU profile while still allocating, so the heap agent is the
    /// one that has anything to say in that case.
    #[cfg(feature = "profiling-memory-jemalloc")]
    memory_agent: Option<pyroscope::PyroscopeAgent<pyroscope::pyroscope::PyroscopeAgentRunning>>,
}

#[cfg(feature = "profiling-bridge-pyroscope-rs")]
impl Drop for ProfilingHandle {
    fn drop(&mut self) {
        if let Some(agent) = self.agent.take() {
            let _ = agent.stop();
        }
        #[cfg(feature = "profiling-memory-jemalloc")]
        if let Some(agent) = self.memory_agent.take() {
            let _ = agent.stop();
        }
    }
}

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
    identity: &ProfilingIdentity,
) -> Result<Option<ProfilingHandle>, Box<dyn Error>> {
    use pyroscope::backend::{BackendConfig, PprofConfig, pprof_backend};

    // Validate endpoint targets loopback only (ADR platform/0203 AC1)
    validate_pyroscope_endpoint(pyroscope_endpoint)?;

    // The `pprof` backend holds a single process-wide profiler guard, so the
    // bridge starts at most once; ignore subsequent start attempts.
    if PROFILING_STARTED.set(()).is_err() {
        return Ok(None);
    }

    let tags = identity.tag_pairs();

    let agent = pyroscope::pyroscope::PyroscopeAgentBuilder::new(
        pyroscope_endpoint,
        service_name,
        100,
        "pyroscope-rs",
        env!("CARGO_PKG_VERSION"),
        pprof_backend(PprofConfig { sample_rate: 100 }, BackendConfig::default()),
    )
    .tags(tags.clone())
    .build()?
    .start()?;

    // Deliberately NOT calling `agent.tag_wrapper()`. See [`ProfilingTagLayer`]:
    // every tag call rebuilds and clears the whole profile, which both leaked
    // memory and emptied the profiles it was meant to enrich.

    Ok(Some(ProfilingHandle {
        agent: Some(agent),
        #[cfg(feature = "profiling-memory-jemalloc")]
        memory_agent: start_memory_agent(service_name, pyroscope_endpoint, &tags)?,
    }))
}

/// Start the jemalloc heap-profiling agent.
///
/// Returns `Ok(None)` — never an error — when heap profiling is unavailable.
/// The backend needs the process to use jemalloc as its global allocator and
/// to have been built with profiling support; neither is visible at compile
/// time, and a binary that merely links this feature must still boot normally
/// without it. Losing heap profiles is an observability regression, not a
/// reason to fail service startup.
///
/// ## Arm inactive, activate here
///
/// Consumers should set `_RJEM_MALLOC_CONF=prof:true,prof_active:false` and
/// let this function turn sampling on. **Do not set `prof_active:true`.**
///
/// On x86_64 static musl, arming profiling at process start segfaults before
/// `main` runs. Isolated on a real service image, same host, only the env var
/// differing:
///
/// ```text
/// prof:true,prof_active:true                  -> exit 139 (SIGSEGV)
/// prof:true,prof_active:true,lg_prof_sample:30 -> exit 139 (SIGSEGV)
/// prof:true,prof_active:false                 -> runs clean
/// ```
///
/// `lg_prof_sample:30` samples roughly once per gigabyte and the probe never
/// allocated near that, so the fault is in activation itself rather than in
/// walking a sampled allocation's backtrace. Activating from here instead runs
/// after the runtime is fully initialised.
///
/// Activation failure is non-fatal for the same reason as everything else in
/// this path: CPU profiling continues, and the service boots.
/// Turn jemalloc sampling on, if the consumer armed `prof` but left it inactive.
///
/// Split out of [`start_memory_agent`] so it can be exercised directly by the
/// `heap-probe` binary: this is the whole of what runs before any Pyroscope
/// endpoint is involved, and it is where both shipped profiling defects lived.
///
/// The outer `Result` is `Err` when the call panicked rather than failed —
/// reading the mallctl panics rather than erroring when jemalloc is not the
/// process allocator.
///
/// ## Why not `blocking_lock`
///
/// `PROF_CTL` is a `tokio::sync::Mutex`, and callers reach this from inside a
/// runtime — `with_profiling()` runs during service bootstrap. `blocking_lock`
/// panics with "Cannot block the current thread from within a runtime", which
/// 2.12.0 shipped: the panic was caught, heap profiling silently never armed,
/// and the service looked healthy. `try_lock` is correct rather than merely
/// panic-free, because activation happens once at startup with nothing else
/// holding the lock; there is no contention to wait out.
#[cfg(feature = "profiling-memory-jemalloc")]
#[doc(hidden)]
pub fn activate_jemalloc_sampling() -> SamplingActivation {
    let caught = std::panic::catch_unwind(|| match jemalloc_pprof::PROF_CTL.as_ref() {
        None => Err("jemalloc profiling not compiled into this binary".to_owned()),
        Some(ctl) => {
            let Ok(mut guard) = ctl.try_lock() else {
                return Err(
                    "jemalloc profiling control is held elsewhere; sampling not activated"
                        .to_owned(),
                );
            };
            if guard.activated() {
                // Already active — the consumer set prof_active:true. It works
                // on some targets, so this is not an error, but it is the
                // configuration that crashes on x86_64 musl, and a process
                // that reaches here has already survived it.
                return Ok(());
            }
            guard.activate().map_err(|e| e.to_string())
        }
    });
    match caught {
        Ok(Ok(())) => SamplingActivation::Activated,
        Ok(Err(e)) => SamplingActivation::Unavailable(e),
        Err(_) => SamplingActivation::Panicked,
    }
}

/// Outcome of [`activate_jemalloc_sampling`].
///
/// `Panicked` is a distinct variant rather than folded into `Unavailable`
/// because the two call for different responses: `Unavailable` is a
/// configuration the operator can correct, while `Panicked` means the process
/// is not the one this code assumes it is running in.
#[cfg(feature = "profiling-memory-jemalloc")]
#[doc(hidden)]
#[derive(Debug)]
pub enum SamplingActivation {
    /// Sampling is on.
    Activated,
    /// Sampling could not be turned on, with the reason.
    Unavailable(String),
    /// Reading the mallctl panicked — jemalloc is not this process's allocator.
    Panicked,
}

#[cfg(feature = "profiling-memory-jemalloc")]
fn start_memory_agent(
    service_name: &str,
    pyroscope_endpoint: &str,
    tags: &[(&'static str, &str)],
) -> Result<
    Option<pyroscope::PyroscopeAgent<pyroscope::pyroscope::PyroscopeAgentRunning>>,
    Box<dyn Error>,
> {
    use pyroscope::backend::jemalloc::jemalloc_backend;

    match activate_jemalloc_sampling() {
        SamplingActivation::Activated => {}
        SamplingActivation::Unavailable(e) => {
            tracing::warn!(
                error = %e,
                "jemalloc heap profiling unavailable — continuing without it; \
                 set _RJEM_MALLOC_CONF=prof:true,prof_active:false and use jemalloc \
                 as the global allocator"
            );
            return Ok(None);
        }
        SamplingActivation::Panicked => {
            tracing::warn!(
                "jemalloc heap profiling unavailable — this process is not using \
                 jemalloc as its global allocator; continuing without it"
            );
            return Ok(None);
        }
    }

    // `catch_unwind`, not just error handling, because the failure is a panic.
    // `jemalloc_pprof`'s `JemallocProfCtl::get` reads the `opt.prof` mallctl
    // and `unwrap()`s it; when the process is not actually using jemalloc that
    // read fails and the unwrap panics rather than returning an error we could
    // match on. A binary that merely compiles this feature — every test binary
    // in a consuming workspace, for one — links jemalloc_pprof without
    // installing the allocator, so this is the normal case, not an edge one.
    //
    // Nothing here is left half-initialised by the unwind: the closure owns the
    // backend and the partially-built agent, and both are dropped with it.
    let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pyroscope::pyroscope::PyroscopeAgentBuilder::new(
            pyroscope_endpoint,
            service_name,
            100,
            "pyroscope-rs",
            env!("CARGO_PKG_VERSION"),
            jemalloc_backend(),
        )
        .tags(tags.to_vec())
        .build()
    }));

    let agent = match built {
        Ok(Ok(agent)) => agent,
        Ok(Err(e)) => {
            tracing::warn!(
                error = %e,
                "jemalloc heap profiling unavailable — continuing without it; \
                 check the global allocator is jemalloc and prof:true,prof_active:true is set"
            );
            return Ok(None);
        }
        Err(_) => {
            tracing::warn!(
                "jemalloc heap profiling unavailable — this process is not using \
                 jemalloc as its global allocator; continuing without it"
            );
            return Ok(None);
        }
    };

    match agent.start() {
        Ok(running) => {
            tracing::info!("jemalloc heap profiling started");
            Ok(Some(running))
        }
        Err(e) => {
            tracing::warn!(error = %e, "jemalloc heap profiling failed to start — continuing without it");
            Ok(None)
        }
    }
}

/// No-op bridge for when profiling is enabled but the pyroscope feature is not.
#[cfg(all(feature = "profiling", not(feature = "profiling-bridge-pyroscope-rs")))]
pub(crate) fn start_pyroscope_bridge(
    _service_name: &str,
    _pyroscope_endpoint: &str,
    _identity: &ProfilingIdentity,
) -> Result<Option<ProfilingHandle>, Box<dyn Error>> {
    Ok(None)
}

/// Inert. Was: a tracing layer that tagged the running pyroscope agent with
/// `trace_id`/`span_id` on every span enter and exit.
///
/// # Why this does nothing
///
/// Per-span tagging is not implementable against the `pprof` backend, and
/// enabling it was strictly worse than having no correlation at all. In
/// pyroscope-rs the backend's own comment calls `dump_report` a *"workaround
/// for pprof-rs to interrupt the profiler"*, and both `Backend::add_tag` and
/// `Backend::remove_tag` call it unconditionally. One `dump_report` symbolises
/// every entry currently in the collector — `backtrace::resolve` per frame,
/// building a fresh `Vec<Vec<Symbol>>` with a `String` and a `PathBuf` per
/// symbol — and then clears the collector.
///
/// So each span enter cost two full profile rebuilds, and each exit two more.
/// Measured on a static-musl build at the shipped 100 Hz sample rate, driving
/// spans from two threads:
///
/// ```text
/// t=10s  dumps=868726  clears=868725  sessions=1  collector_entries=0
/// ```
///
/// ~87,000 profile rebuilds per second against one 10-second upload. Two
/// consequences, and the second is why this is not merely a tuning problem:
///
/// 1. The allocation churn grew RSS without bound. Over 90 minutes on static
///    musl the leak was 7.4 MiB/h with this layer and 1.0 MiB/h without, which
///    matches the 6.5 MiB/h observed on brefwiz-spiffe in production, where it
///    OOM-killed both replicas roughly every 5.5 hours against a 512Mi cgroup.
/// 2. `collector_entries=0` at *every* upload: the collector was cleared far
///    faster than the 100 Hz sampler could fill it, so the profiles this layer
///    existed to enrich were arriving essentially empty. The correlation
///    feature destroyed the very data it annotated.
///
/// Sampling itself is not implicated — with tagging off, 63,254 samples over
/// 420 seconds moved RSS by 0.03 MiB/h.
///
/// # What replaces it
///
/// Nothing, on this backend: there is no bounded form. The cost is per tag
/// call, so sampling spans or tagging only roots still buys full profile
/// rebuilds at a fraction of the span rate, and each rebuild still truncates
/// the sample window. Trace/profile correlation returns with the native OTLP
/// profiles exporter this bridge is already sunset-bound against.
///
/// The type is kept, registered, and doing nothing so the subscriber stack and
/// the public surface are unchanged; it is removed in the next major.
#[cfg(feature = "profiling-bridge-pyroscope-rs")]
#[deprecated(
    since = "2.15.0",
    note = "inert: per-span pyroscope tagging leaked memory and emptied profiles; \
            correlation returns with the OTLP profiles exporter"
)]
pub struct ProfilingTagLayer;

#[cfg(feature = "profiling-bridge-pyroscope-rs")]
#[allow(deprecated)]
impl<S> tracing_subscriber::Layer<S> for ProfilingTagLayer where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>
{
}

#[cfg(all(test, feature = "profiling-bridge-pyroscope-rs"))]
mod tests {
    use super::*;

    #[test]
    fn start_bridge_with_nonexistent_server() {
        let result = start_pyroscope_bridge(
            "test-svc",
            "http://localhost:4040",
            &ProfilingIdentity::default(),
        );
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
        let result1 = start_pyroscope_bridge(
            "test-svc-1",
            "http://localhost:4040",
            &ProfilingIdentity::default(),
        );
        assert!(result1.is_ok());
        let result2 = start_pyroscope_bridge(
            "test-svc-2",
            "http://localhost:4041",
            &ProfilingIdentity::default(),
        );
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
        let result = start_pyroscope_bridge(
            "test-svc",
            "http://localhost:4040",
            &ProfilingIdentity::default(),
        );
        assert!(result.is_ok());
        if let Ok(handle) = result {
            assert!(handle.is_none());
        }
    }
}
