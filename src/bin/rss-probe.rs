// SPDX-License-Identifier: LicenseRef-Brefwiz-Proprietary
//! Real-process probe for profiling-bridge memory stability.
//!
//! The bridge's memory behaviour is a property of the *process* — a live
//! pyroscope agent, a real `SIGPROF` sampler, a real subscriber stack, and
//! spans being entered and exited on worker threads. A `cargo test` binary
//! with the bridge merely linked exercises none of it, which is why the leak
//! that OOM-killed brefwiz-spiffe every ~5.5 hours was invisible to this
//! crate's entire test suite for two releases.
//!
//! What broke: `ProfilingTagLayer` called pyroscope's `add_tag`/`remove_tag`
//! on every span enter and exit, and each of those calls rebuilds and clears
//! the whole profile (pyroscope's own name for it is "workaround for pprof-rs
//! to interrupt the profiler"). Measured at ~87,000 profile rebuilds per
//! second against one 10-second upload. The symbolisation churn grew RSS
//! without bound.
//!
//! So this probe drives spans hard against the live bridge and watches RSS.
//! It deliberately does NOT special-case the bridge internals — it goes
//! through `Telemetry::builder().with_profiling()`, the same call a service
//! makes, so a regression anywhere in that path is caught.
//!
//! Driven by `tests/profiling_rss_stability.rs`, which reads the markers below.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use opentelemetry::Context as OtelContext;
use opentelemetry::trace::{TraceContextExt, Tracer, TracerProvider};

/// Seconds of steady-state measurement, after warm-up.
const MEASURE_SECS: u64 = 120;
/// Warm-up discarded before measuring.
///
/// Symbolisation faults the binary's symbol pages in on first use and the
/// allocator settles at a high-water mark; both are real, bounded, one-off
/// costs, and measuring through them reports convergence as a leak. Measured
/// on release static musl, the fixed bridge fits 5.17 MiB/h from t=120s and
/// ~1.3 MiB/h over an hour, so a window that opens much earlier than this is
/// reading the tail of the ramp rather than the steady state.
const WARMUP_SECS: u64 = 60;
/// Threads entering and exiting spans.
const SPAN_THREADS: usize = 2;
/// Multiply-add rounds per span — the only span-rate knob. See [`work`].
const WORK_ITERS: usize = 60_000;

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Resident set size in bytes, or `None` where it cannot be read.
///
/// `/proc/self/status` reports `VmRSS` in kB directly, which avoids having to
/// resolve the page size — this is a `[[bin]]`, so it sees only the library's
/// dependencies and `libc` is not among them.
fn rss_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
        let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kib * 1024)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let out = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .ok()?;
        let kib: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
        Some(kib * 1024)
    }
}

/// A sink that accepts and 200s every push, so the full encode + HTTP path
/// runs. Pointing at a dead port instead would leave the agent retrying and
/// never exercise the encode side.
///
/// It also weighs each upload. That size is the sharper of the two signals
/// this probe collects: per-span tagging cleared the collector far faster than
/// the sampler could fill it, so the broken build uploaded near-empty profiles
/// (`collector_entries=0` at every single upload, measured). Payload size
/// catches that deterministically, where the RSS slope needs a minute of
/// samples and a threshold.
fn spawn_sink(
    stop: Arc<AtomicBool>,
    pushes: Arc<AtomicU64>,
    push_bytes: Arc<AtomicU64>,
    tagged_pushes: Arc<AtomicU64>,
) -> std::io::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    listener.set_nonblocking(true)?;

    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut sock, _)) => {
                    sock.set_nonblocking(false).ok();
                    sock.set_read_timeout(Some(Duration::from_millis(250))).ok();
                    let mut buf = [0u8; 32 * 1024];
                    let mut total = 0u64;
                    let mut body: Vec<u8> = Vec::new();
                    while let Ok(n) = sock.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                        total += n as u64;
                        body.extend_from_slice(&buf[..n]);
                        if n < buf.len() {
                            break;
                        }
                    }
                    pushes.fetch_add(1, Ordering::Relaxed);
                    push_bytes.fetch_add(total, Ordering::Relaxed);
                    if body_has_per_span_labels(&body[..]) {
                        tagged_pushes.fetch_add(1, Ordering::Relaxed);
                    }
                    let _ = sock.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                }
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    });

    Ok(port)
}

/// Does this upload carry per-span tag labels?
///
/// This is the gate, and it is exact rather than statistical: the defect is
/// "the bridge tags profiles per span", and a tagged profile carries the label
/// keys verbatim in the pprof string table. Size and RSS both failed to
/// separate the two builds — measured 823 bytes fixed against 828 broken, and
/// an RSS slope that ran *higher* on the healthy build — so neither is usable
/// as a threshold. Label presence has no threshold to get wrong.
fn body_has_per_span_labels(body: &[u8]) -> bool {
    use std::io::Read as _;

    // Skip HTTP headers; the gzip member starts at the body.
    let Some(start) = body.windows(4).position(|w| w == b"\r\n\r\n") else {
        return false;
    };
    let mut out = Vec::new();
    if flate2::read::GzDecoder::new(&body[start + 4..])
        .read_to_end(&mut out)
        .is_err()
        && out.is_empty()
    {
        return false;
    }
    out.windows(7).any(|w| w == b"span_id") || out.windows(8).any(|w| w == b"trace_id")
}

/// On-CPU work per span.
///
/// Sized to hold the span rate in the low tens of thousands per second. Rate
/// matters in both directions: too low and a per-span regression hides under
/// the threshold, too high and the measurement stops being about the profiling
/// bridge at all — an early version of this probe drove 4.6M spans/second and
/// measured the OTLP span pipeline's own backpressure instead.
#[inline(never)]
fn work(n: u64) -> u64 {
    let mut acc = n;
    for _ in 0..WORK_ITERS {
        acc = std::hint::black_box(acc.wrapping_mul(6364136223846793005).wrapping_add(1));
    }
    acc
}

/// Least-squares slope of `(seconds, bytes)` in bytes per second.
fn slope(samples: &[(f64, f64)]) -> f64 {
    let n = samples.len() as f64;
    if n < 2.0 {
        return 0.0;
    }
    let mean_t = samples.iter().map(|s| s.0).sum::<f64>() / n;
    let mean_r = samples.iter().map(|s| s.1).sum::<f64>() / n;
    let num: f64 = samples
        .iter()
        .map(|(t, r)| (t - mean_t) * (r - mean_r))
        .sum();
    let den: f64 = samples.iter().map(|(t, _)| (t - mean_t).powi(2)).sum();
    if den > 0.0 { num / den } else { 0.0 }
}

fn main() {
    let measure_secs = env_u64("RSS_PROBE_MEASURE_SECS", MEASURE_SECS);
    let warmup_secs = env_u64("RSS_PROBE_WARMUP_SECS", WARMUP_SECS);

    if rss_bytes().is_none() {
        println!("RSS_PROBE_NO_RSS cannot read RSS on this platform");
        std::process::exit(3);
    }

    let stop = Arc::new(AtomicBool::new(false));
    let pushes = Arc::new(AtomicU64::new(0));
    let push_bytes = Arc::new(AtomicU64::new(0));
    let tagged_pushes = Arc::new(AtomicU64::new(0));
    let spans = Arc::new(AtomicU64::new(0));

    let port = match spawn_sink(
        stop.clone(),
        pushes.clone(),
        push_bytes.clone(),
        tagged_pushes.clone(),
    ) {
        Ok(p) => p,
        Err(e) => {
            println!("RSS_PROBE_SINK_FAILED {e}");
            std::process::exit(4);
        }
    };

    // `init()` installs OTLP exporters that need a runtime in context, and a
    // service bootstraps on a multi-thread one. Held for the whole run: the
    // exporters keep background tasks alive past init.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build runtime");
    let _rt_guard = rt.enter();

    // The same entry point a service uses. Traces have nowhere to go and that
    // is fine — init does not eagerly connect, and the profiling bridge is
    // what this probe is about.
    //
    // The control run omits `with_profiling` and nothing else. Span churn on
    // its own moves RSS — the tracing registry and the OTLP span pipeline both
    // allocate per span, and with no collector to drain to, the batch
    // processor spends the run filling and dropping. That cost is real but it
    // is not the bridge's, so the caller measures both and takes the
    // difference. Asserting an absolute slope here instead would be measuring
    // the span pipeline and calling it a profiling leak.
    let control = std::env::var("RSS_PROBE_NO_PROFILING").is_ok_and(|v| v == "1");
    let builder = otel_bootstrap::Telemetry::builder("rss-probe");
    let builder = if control {
        builder
    } else {
        builder.with_profiling(&format!("http://127.0.0.1:{port}"))
    };
    let handles = match builder.init() {
        Ok(h) => h,
        Err(e) => {
            println!("RSS_PROBE_INIT_FAILED {e}");
            std::process::exit(5);
        }
    };

    if !control && handles.profiling_handle.is_none() {
        // Nothing was measured, so a flat line would be meaningless.
        println!("RSS_PROBE_BRIDGE_INACTIVE profiling handle absent");
        std::process::exit(6);
    }

    // Span churn: this is the load that broke. Each enter/exit pair used to
    // cost four full profile rebuilds.
    for t in 0..SPAN_THREADS {
        let stop = stop.clone();
        let spans = spans.clone();
        std::thread::Builder::new()
            .name(format!("span-{t}"))
            .spawn(move || {
                // One tracer per thread; cloning the provider handle is cheap.
                let tracer = opentelemetry::global::tracer_provider().tracer("rss-probe");
                let mut n = t as u64;
                while !stop.load(Ordering::Relaxed) {
                    // A valid OpenTelemetry context is load-bearing, not
                    // decoration. `ProfilingTagLayer::on_enter` only tags when
                    // `Context::current()` carries a valid span context, so a
                    // bare `info_span!` leaves the whole tag path dormant and
                    // the probe measures nothing: an earlier version of this
                    // loop scored 14,518 spans/s broken against 14,499 fixed,
                    // identical, because the defect never fired. In a service
                    // the axum and tonic middleware attach the extracted
                    // context across the handler, which is what this mirrors.
                    let otel_span = tracer.start("rss_probe.unit");
                    let cx = OtelContext::current_with_span(otel_span);
                    let attached = cx.attach();

                    let span = tracing::info_span!("rss_probe.unit", thread = t);
                    let entered = span.enter();
                    n = work(n);
                    drop(entered);
                    drop(attached);
                    spans.fetch_add(1, Ordering::Relaxed);
                    std::hint::black_box(n);

                    // Deliberately no sleep. Span rate is paced by WORK_ITERS
                    // instead, because the sampler is ITIMER_PROF — CPU time,
                    // not wall clock. A probe that sleeps between spans is off
                    // CPU most of the run, collects almost no samples, and
                    // uploads near-empty profiles whether or not the bridge is
                    // healthy; an earlier sleep-paced version of this probe
                    // measured 925 bytes per profile fixed against 874 broken,
                    // which gates nothing. Saturating the CPU is what makes
                    // upload size mean something.
                }
            })
            .expect("spawn span thread");
    }

    let start = Instant::now();
    let mut samples: Vec<(f64, f64)> = Vec::new();
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let elapsed = start.elapsed().as_secs_f64();
        if elapsed >= (warmup_secs + measure_secs) as f64 {
            break;
        }
        let Some(rss) = rss_bytes() else { continue };
        if elapsed >= warmup_secs as f64 {
            samples.push((elapsed, rss as f64));
        }
        println!("RSS_PROBE_SAMPLE t={elapsed:.1} rss={rss}");
    }

    stop.store(true, Ordering::Relaxed);

    let bytes_per_sec = slope(&samples);
    let mib_per_hour = bytes_per_sec * 3600.0 / (1024.0 * 1024.0);
    let first = samples.first().map(|s| s.1 as u64).unwrap_or(0);
    let last = samples.last().map(|s| s.1 as u64).unwrap_or(0);
    let spans_done = spans.load(Ordering::Relaxed);

    // A run that never entered a span proves nothing; fail loudly rather than
    // report a flat line the load never challenged.
    if spans_done < 1000 {
        println!("RSS_PROBE_NO_LOAD spans={spans_done}");
        let _ = handles.shutdown();
        std::process::exit(7);
    }

    let pushes_done = pushes.load(Ordering::Relaxed);
    let bytes_done = push_bytes.load(Ordering::Relaxed);
    let bytes_per_push = bytes_done.checked_div(pushes_done).unwrap_or(0);
    let elapsed_secs = start.elapsed().as_secs_f64().max(1.0);

    println!(
        "RSS_PROBE_OK profiling={} mib_per_hour={mib_per_hour:.2} samples={} \
         spans={spans_done} spans_per_sec={:.0} pushes={pushes_done} \
         push_bytes={bytes_done} bytes_per_push={bytes_per_push} \
         tagged_pushes={} first_rss={first} last_rss={last}",
        u8::from(!control),
        samples.len(),
        spans_done as f64 / elapsed_secs,
        tagged_pushes.load(Ordering::Relaxed),
    );

    let _ = handles.shutdown();
}
