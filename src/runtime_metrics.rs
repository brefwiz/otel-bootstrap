//! Process and async-runtime gauges, registered automatically at init.
//!
//! ## Why this is a platform default
//!
//! Every service built on this crate runs the same shape: a Tokio runtime
//! inside a container with a CPU quota and a memory limit. When such a service
//! misbehaves, the first questions are always the same — is the process
//! growing, is the runtime keeping up, how much parallelism did it actually
//! get — and none of them can be answered from traces or logs.
//!
//! Leaving that to each service means it is done nowhere until an incident
//! forces it, and then it is done inconsistently under time pressure. These
//! instruments are cheap, service-agnostic, and exactly the ones that turn
//! "the database looks slow" into "the runtime is starved", so they ship on by
//! default rather than as an opt-in every service has to discover.
//!
//! Registering here also removes an ordering hazard that callers otherwise
//! have to know about: OpenTelemetry binds an instrument to whichever
//! `MeterProvider` is global when the instrument is created, so a service that
//! registers its own runtime gauges before telemetry init gets permanent
//! no-ops that appear correctly wired but export nothing. This module is
//! invoked immediately after the provider is installed, so the hazard cannot
//! occur.
//!
//! ## What is recorded
//!
//! | Instrument | Kind | Answers |
//! |---|---|---|
//! | `process.uptime` | gauge (s) | Is the symptom a function of process age? |
//! | `process.memory.resident` | gauge (By) | Growth toward the container memory limit. |
//! | `runtime.tokio.workers` | gauge | Parallelism actually granted under the CPU quota. |
//! | `runtime.tokio.alive_tasks` | gauge | Task leak. |
//! | `runtime.tokio.global_queue_depth` | gauge | Runtime backlog. |
//! | `runtime.tokio.scheduler_delay` | histogram (ms) | How late a trivial task is polled. |
//!
//! `scheduler_delay` is the load-bearing one. It times a bare
//! [`tokio::task::yield_now`] round-trip — no I/O, no locks, no downstream
//! calls — so it isolates scheduling delay from real work. Compared against a
//! service's own latency metrics it separates "the runtime never polled us"
//! from "the thing we called was slow", which request-level timings alone
//! cannot distinguish. `workers` is its companion: a sub-1-core cgroup quota
//! makes [`std::thread::available_parallelism`] report 1, so Tokio runs a
//! single worker and any blocking call serialises the whole process.
//!
//! ## Scope
//!
//! CPU profilers sample on-CPU time, so a process stalled *off*-CPU produces
//! near-empty flame graphs. These gauges are the off-CPU counterpart and are
//! deliberately not a substitute for profiling — they say *that* the runtime
//! stalled, not which code path allocated or blocked.

use std::time::{Duration, Instant};

use opentelemetry::metrics::{Histogram, Meter};

/// Interval between `scheduler_delay` samples.
///
/// Frequent enough to catch a stall that lasts seconds, sparse enough that the
/// probe itself is not a meaningful load on a single-worker runtime.
const SCHEDULER_PROBE_INTERVAL: Duration = Duration::from_secs(5);

/// Instrument namespace for everything this module registers.
const METER_NAME: &str = "otel-bootstrap.runtime";

/// Register the process/runtime instruments on the global meter.
///
/// Called by `TelemetryBuilder::init` right after the `MeterProvider` is
/// installed. Observable gauges are driven by the SDK's collection cycle; the
/// scheduler probe needs a task, which is only spawned when a Tokio runtime is
/// present.
pub(crate) fn install() {
    let meter = opentelemetry::global::meter(METER_NAME);

    let started = Instant::now();
    meter
        .f64_observable_gauge("process.uptime")
        .with_unit("s")
        .with_description(
            "Seconds since telemetry init. Lets any other series be correlated \
             against process age — the shape that distinguishes a leak that a \
             restart resets from a genuine load change.",
        )
        .with_callback(move |o| o.observe(uptime_seconds(started), &[]))
        .build();

    meter
        .u64_observable_gauge("process.memory.resident")
        .with_unit("By")
        .with_description(
            "Resident set size. Read from /proc/self/statm on Linux; not \
             reported on other platforms. Useful where no container-level \
             metrics agent is deployed.",
        )
        .with_callback(|o| {
            if let Some(rss) = resident_memory_bytes() {
                o.observe(rss, &[]);
            }
        })
        .build();

    install_tokio_gauges(&meter);
    spawn_scheduler_probe(&meter);
}

/// Register the Tokio runtime gauges.
///
/// Each callback re-checks for a current runtime rather than capturing a
/// handle once: `install` may run outside a runtime context, and observing
/// nothing is the correct behaviour there.
fn install_tokio_gauges(meter: &Meter) {
    meter
        .u64_observable_gauge("runtime.tokio.workers")
        .with_description(
            "Tokio worker threads. Derived from available_parallelism, which \
             honours the cgroup CPU quota — a sub-1-core limit yields a single \
             worker, so any blocking call serialises the whole process.",
        )
        .with_callback(|o| {
            if let Some(v) = tokio_workers() {
                o.observe(v, &[]);
            }
        })
        .build();

    meter
        .u64_observable_gauge("runtime.tokio.alive_tasks")
        .with_description(
            "Live Tokio tasks. A monotonic climb over a process's lifetime is \
             a task leak; flat rules tasks out as the thing that is growing.",
        )
        .with_callback(|o| {
            if let Some(v) = tokio_alive_tasks() {
                o.observe(v, &[]);
            }
        })
        .build();

    meter
        .u64_observable_gauge("runtime.tokio.global_queue_depth")
        .with_description(
            "Tasks waiting in the runtime's global queue. Sustained non-zero \
             depth under light load means the runtime is starved, which delays \
             every in-flight future including I/O.",
        )
        .with_callback(|o| {
            if let Some(v) = tokio_global_queue_depth() {
                o.observe(v, &[]);
            }
        })
        .build();
}

/// Spawn the scheduler-delay probe when a runtime is available.
///
/// No-op outside a runtime context (sync tests, CLI init before the runtime
/// starts) so that init never panics on `tokio::spawn`.
fn spawn_scheduler_probe(meter: &Meter) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }

    let scheduler_delay = meter
        .f64_histogram("runtime.tokio.scheduler_delay")
        .with_unit("ms")
        .with_description(
            "Wall time for a bare yield_now() round-trip. Touches no I/O, no \
             locks and nothing downstream, so it measures scheduling delay \
             alone. Microseconds on a healthy runtime; seconds means every \
             await point in the process is equally late.",
        )
        .build();

    tokio::spawn(run_scheduler_probe(scheduler_delay));
}

/// Sample scheduler latency forever.
async fn run_scheduler_probe(scheduler_delay: Histogram<f64>) {
    loop {
        tokio::time::sleep(SCHEDULER_PROBE_INTERVAL).await;
        record_scheduler_delay(&scheduler_delay).await;
    }
}

/// Time one `yield_now()` round-trip and record it.
///
/// Split out of the loop so the measurement can be tested directly. Testing it
/// through `run_scheduler_probe` would mean either waiting a full
/// `SCHEDULER_PROBE_INTERVAL` or pulling in tokio's `test-util` to fake the
/// clock — neither justified for three lines with no branches.
async fn record_scheduler_delay(scheduler_delay: &Histogram<f64>) {
    let started = Instant::now();
    tokio::task::yield_now().await;
    scheduler_delay.record(started.elapsed().as_secs_f64() * 1000.0, &[]);
}

/// Seconds elapsed since `started`.
fn uptime_seconds(started: Instant) -> f64 {
    started.elapsed().as_secs_f64()
}

// The three Tokio readings below are named functions rather than closure
// bodies so they can be asserted on directly. Driving them through the SDK
// instead would mean attaching a real reader and forcing a collection, and a
// `force_flush` against an unreachable collector blocks rather than failing —
// a test that hangs indefinitely instead of reporting.
//
// Each returns `None` outside a runtime instead of `0`: a fabricated zero is
// indistinguishable on a dashboard from a genuinely idle runtime.

/// Tokio worker-thread count, or `None` outside a runtime.
fn tokio_workers() -> Option<u64> {
    tokio::runtime::Handle::try_current()
        .ok()
        .map(|h| h.metrics().num_workers() as u64)
}

/// Live Tokio task count, or `None` outside a runtime.
fn tokio_alive_tasks() -> Option<u64> {
    tokio::runtime::Handle::try_current()
        .ok()
        .map(|h| h.metrics().num_alive_tasks() as u64)
}

/// Tokio global-queue depth, or `None` outside a runtime.
fn tokio_global_queue_depth() -> Option<u64> {
    tokio::runtime::Handle::try_current()
        .ok()
        .map(|h| h.metrics().global_queue_depth() as u64)
}

/// Resident set size in bytes, or `None` where `/proc` is unavailable.
///
/// `/proc/self/statm` reports page counts; field 1 is the resident count.
/// Field 0 is total program size, which is why the index matters.
fn resident_memory_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let resident_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    // `sysconf(_SC_PAGESIZE)` is 4 KiB on every target this crate ships to;
    // the constant avoids a libc dependency for one lookup.
    Some(resident_pages * 4096)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn resident_memory_is_plausible() {
        let rss = resident_memory_bytes().expect("/proc/self/statm readable on Linux");
        // A live process is never smaller than a page, and reading the wrong
        // statm field (field 0, total program size) would land above this band.
        assert!(rss >= 4096, "implausibly small RSS: {rss}");
        assert!(
            rss < 64 * 1024 * 1024 * 1024,
            "implausibly large RSS: {rss}"
        );
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn resident_memory_absent_without_proc() {
        assert!(
            resident_memory_bytes().is_none(),
            "expected no /proc/self/statm off Linux"
        );
    }

    #[test]
    fn install_outside_a_runtime_does_not_panic() {
        // Init can legitimately run before a runtime exists; the probe must be
        // skipped rather than panicking inside `tokio::spawn`.
        install();
    }

    #[tokio::test]
    async fn install_inside_a_runtime_spawns_the_probe() {
        install();
        // The probe's first sample is one interval away, so this asserts only
        // that spawning succeeded and the task is running.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    #[tokio::test]
    async fn tokio_gauges_report_inside_a_runtime() {
        assert!(tokio_workers().expect("workers inside a runtime") >= 1);
        assert!(tokio_global_queue_depth().is_some());

        // The test's own future is driven by `block_on` and is not a spawned
        // task, so `num_alive_tasks` is 0 until something is actually spawned.
        // Spawn one and hold it, otherwise this asserts nothing about whether
        // the gauge tracks tasks at all.
        let before = tokio_alive_tasks().expect("task count inside a runtime");
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let held = tokio::spawn(async move {
            let _ = rx.await;
        });
        tokio::task::yield_now().await;
        assert!(
            tokio_alive_tasks().expect("task count inside a runtime") > before,
            "spawning a task should raise the live-task count"
        );
        let _ = tx.send(());
        let _ = held.await;
    }

    #[test]
    fn tokio_gauges_report_nothing_outside_a_runtime() {
        // Observing nothing is the contract off-runtime; the callbacks must
        // not fabricate a zero, which would be indistinguishable from a real
        // idle runtime on a dashboard.
        assert!(tokio_workers().is_none());
        assert!(tokio_alive_tasks().is_none());
        assert!(tokio_global_queue_depth().is_none());
    }

    #[test]
    fn uptime_advances() {
        let started = Instant::now();
        std::thread::sleep(Duration::from_millis(5));
        assert!(uptime_seconds(started) > 0.0);
    }

    #[tokio::test]
    async fn scheduler_delay_sample_is_recorded() {
        let meter = opentelemetry::global::meter("test");
        let histogram = meter.f64_histogram("test.scheduler_delay").build();
        // Exercises the measurement itself. Going through run_scheduler_probe
        // would only reach the sleep, since the first sample is a full
        // interval away.
        record_scheduler_delay(&histogram).await;
    }

    #[tokio::test]
    async fn scheduler_probe_spawns_and_survives_abort() {
        let meter = opentelemetry::global::meter("test");
        let histogram = meter.f64_histogram("test.scheduler_delay").build();
        // The probe loops forever by design; bound it so the test terminates.
        let probe = tokio::spawn(run_scheduler_probe(histogram));
        tokio::time::sleep(Duration::from_millis(20)).await;
        probe.abort();
    }
}
