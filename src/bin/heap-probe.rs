// SPDX-License-Identifier: LicenseRef-Brefwiz-Proprietary
//! Real-process probe for jemalloc heap profiling.
//!
//! Heap profiling cannot be exercised from a normal `cargo test` binary. It
//! needs three things that are properties of the *process*, not of a test:
//!
//! 1. jemalloc installed as the global allocator,
//! 2. `_RJEM_MALLOC_CONF` present in the environment before `main` runs,
//! 3. activation attempted from inside a Tokio runtime, which is where service
//!    bootstrap actually calls it.
//!
//! A library test binary has none of these, which is why two consecutive
//! releases shipped broken heap profiling and both were caught in staging:
//!
//! - 2.11.x: `prof_active:true` segfaulted before `main` on x86_64 static musl.
//! - 2.12.0: `blocking_lock()` inside the runtime panicked; the panic was
//!   caught, so the process stayed healthy and profiling silently never armed.
//!
//! The second is why this probe asserts a *non-empty profile* rather than a
//! clean exit. A clean exit is exactly what the broken build produced.
//!
//! Driven by `tests/heap_profiling_probe.rs`, which runs it under each
//! `_RJEM_MALLOC_CONF` shape and reads the markers below.

#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// Allocated before dumping, and large enough to be sampled at the
/// `lg_prof_sample` rates services actually use.
const ALLOC_BYTES: usize = 96 * 1024 * 1024;

fn main() {
    // Multi-thread on purpose: `blocking_lock()` panics from within a runtime
    // worker, and a current-thread runtime is not a faithful stand-in for how
    // services bootstrap.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build runtime");

    rt.block_on(async {
        // The exact call that panicked in 2.12.0.
        use otel_bootstrap::profiling::SamplingActivation;
        match otel_bootstrap::profiling::activate_jemalloc_sampling() {
            SamplingActivation::Panicked => {
                println!("HEAP_PROBE_PANIC activation panicked");
                std::process::exit(2);
            }
            SamplingActivation::Unavailable(e) => {
                println!("HEAP_PROBE_INACTIVE {e}");
                std::process::exit(3);
            }
            SamplingActivation::Activated => {}
        }

        // Allocate something the sampler must see, and keep it live across the
        // dump so it cannot be freed out from under the profile.
        let mut ballast: Vec<Vec<u8>> = Vec::new();
        for _ in 0..96 {
            ballast.push(vec![7u8; ALLOC_BYTES / 96]);
        }
        std::hint::black_box(&ballast);

        let pprof = match dump_pprof() {
            Ok(bytes) => bytes,
            Err(e) => {
                println!("HEAP_PROBE_DUMP_FAILED {e}");
                std::process::exit(4);
            }
        };

        // A dump that produced no bytes means sampling was armed but never
        // recorded anything — indistinguishable from success if we only
        // checked the exit code.
        if pprof.is_empty() {
            println!("HEAP_PROBE_EMPTY_PROFILE");
            std::process::exit(5);
        }

        std::hint::black_box(&ballast);
        println!("HEAP_PROBE_OK pprof_bytes={}", pprof.len());
    });
}

/// Dump a pprof heap profile through the same control handle the agent uses.
fn dump_pprof() -> Result<Vec<u8>, String> {
    let ctl = jemalloc_pprof::PROF_CTL
        .as_ref()
        .ok_or_else(|| "jemalloc profiling not compiled into this binary".to_owned())?;
    let mut guard = ctl
        .try_lock()
        .map_err(|_| "prof ctl held elsewhere".to_owned())?;
    guard.dump_pprof().map_err(|e| e.to_string())
}
