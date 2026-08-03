// SPDX-License-Identifier: LicenseRef-Brefwiz-Proprietary
//! Heap profiling must actually work in a real process.
//!
//! Both shipped profiling defects were invisible to this crate's other tests,
//! because the thing that breaks is a property of the process — its allocator,
//! its environment at exec, its runtime — and a `cargo test` binary has none of
//! the three. Each case below is a bug that reached staging.
//!
//! The probe is a real binary (`src/bin/heap-probe.rs`) run as a child process
//! so `_RJEM_MALLOC_CONF` is set before its `main`, which is the only point at
//! which jemalloc reads it.
#![cfg(feature = "profiling-memory-probe")]

use std::process::{Command, Output};

/// Run the probe with a given `_RJEM_MALLOC_CONF`.
fn run_probe(malloc_conf: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_heap-probe"))
        .env("_RJEM_MALLOC_CONF", malloc_conf)
        .output()
        .expect("spawn heap-probe")
}

fn describe(out: &Output) -> String {
    format!(
        "status={:?} signal={:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status.code(),
        exit_signal(out),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    )
}

#[cfg(unix)]
fn exit_signal(out: &Output) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    out.status.signal()
}

#[cfg(not(unix))]
fn exit_signal(_out: &Output) -> Option<i32> {
    None
}

/// The supported configuration: arm `prof`, leave sampling off, let the library
/// activate it once the runtime is up.
///
/// This is the case 2.12.0 broke. `blocking_lock()` panicked from inside the
/// runtime, the panic was caught, and the process went on to serve traffic with
/// heap profiling silently dead — so asserting a clean exit would have passed.
/// The profile itself is the assertion that matters.
#[test]
fn activates_and_produces_a_profile_when_armed_inactive() {
    let out = run_probe("prof:true,prof_active:false,lg_prof_sample:19");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.contains("HEAP_PROBE_OK"),
        "heap profiling did not produce a profile.\n{}",
        describe(&out)
    );
    assert!(
        out.status.success(),
        "probe exited non-zero.\n{}",
        describe(&out)
    );

    // Guard the guard: a zero-byte "profile" must not read as success.
    let bytes: usize = stdout
        .split("pprof_bytes=")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    assert!(bytes > 0, "empty pprof payload.\n{}", describe(&out));
}

/// `prof_active:true` arms sampling before `main`, which is what segfaulted on
/// x86_64 static musl in 2.11.x — exit 139, no log output, crash-looping pods.
///
/// The library documents this as unsupported and services must not set it, but
/// "unsupported" is not the same as "crashes the process", and nothing caught
/// the difference. If this configuration ever dies by signal again, it should
/// fail here rather than in a rollout.
#[test]
fn arming_before_main_does_not_kill_the_process() {
    let out = run_probe("prof:true,prof_active:true,lg_prof_sample:19");

    assert!(
        exit_signal(&out).is_none(),
        "process died by signal with sampling armed before main.\n{}",
        describe(&out)
    );
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("HEAP_PROBE_PANIC"),
        "activation panicked.\n{}",
        describe(&out)
    );
}

/// With profiling not armed at all, activation must fail cleanly and say so —
/// not panic, and not report success it cannot deliver.
#[test]
fn reports_inactive_when_prof_not_armed() {
    let out = run_probe("prof:false");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        !stdout.contains("HEAP_PROBE_PANIC"),
        "activation panicked instead of erroring.\n{}",
        describe(&out)
    );
    assert!(
        !stdout.contains("HEAP_PROBE_OK"),
        "claimed a profile without profiling armed.\n{}",
        describe(&out)
    );
}
