// SPDX-License-Identifier: LicenseRef-Brefwiz-Proprietary
//! The profiling bridge must not grow memory under span load.
//!
//! This is the gate for the defect that OOM-killed brefwiz-spiffe every ~5.5
//! hours on both replicas in prod and staging: `ProfilingTagLayer` tagged the
//! pyroscope agent on every span enter and exit, and each tag call rebuilt and
//! cleared the entire profile. RSS grew ~6.5 MiB/h in production against a
//! 512Mi cgroup.
//!
//! Nothing in this crate's test suite could see it. The bridge was linked and
//! initialised in tests, but no test entered spans against a live agent for
//! long enough for a per-span cost to show as memory. So the assertion here is
//! on a real child process under real span load, not on the shape of the code.
//!
//! ## Which assertion gates the defect, and why not the obvious ones
//!
//! Neither memory nor upload size separates the two builds on the shipped
//! target, which is worth recording because both look like the natural choice:
//!
//! - Upload size: 823 bytes per profile fixed against 828 broken.
//! - RSS: the broken build measured *lower* — it cleared the collector so
//!   aggressively that uploads were near-empty, so it had less to symbolise
//!   and allocated less per cycle than a healthy bridge.
//!
//! So the gate asserts the thing the defect actually is: uploads carrying
//! per-span `trace_id`/`span_id` labels. Exact, no threshold to calibrate.
//! The RSS check is kept only as a coarse backstop for gross regressions.
//!
//! One trap, since it cost a full round of measurement: the tag path only
//! fires when `opentelemetry::Context::current()` carries a *valid* span
//! context. A bare `info_span!` does not attach one, so an earlier version of
//! this probe left the defect dormant and scored both builds identically.
//! The probe attaches a real OTel context, as the axum and tonic middleware do
//! in a service.
#![cfg(feature = "profiling-rss-probe")]

use std::process::{Command, Output};

/// Ceiling on the bridge's attributable growth.
///
/// This is a backstop against gross regressions, NOT the gate for the per-span
/// tagging defect — measure before assuming otherwise. On release static musl
/// at this probe's span rate the broken build measured 1.49 MiB/h attributable
/// and the fixed build 5.09, i.e. *inverted*: per-span tagging cleared the
/// collector so aggressively that uploads were near-empty and there was almost
/// nothing to symbolise, so the broken build did less work per cycle, not
/// more. No RSS threshold separates those two.
///
/// `profiling_bridge_uploads_non_empty_profiles_under_span_load` is what
/// catches that defect, and it catches it decisively. This bound exists to
/// notice a bridge that starts consuming memory at a rate no amount of
/// symbolisation explains.
const MAX_MIB_PER_HOUR: f64 = 25.0;

fn describe(out: &Output) -> String {
    format!(
        "status={:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    )
}

/// Parse `key=value` out of the probe's summary line.
fn field(stdout: &str, key: &str) -> Option<f64> {
    stdout
        .lines()
        .find(|l| l.starts_with("RSS_PROBE_OK"))?
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix(key)?.parse().ok())
}

/// Run the probe, with the profiling bridge on or off, and return its output.
fn run_probe(profiling: bool) -> (Output, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rss-probe"));
    if !profiling {
        cmd.env("RSS_PROBE_NO_PROFILING", "1");
    }
    let out = cmd.output().expect("spawn rss-probe");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    (out, stdout)
}

/// Checks common to both arms: the run completed, and it actually applied load.
fn assert_measured(out: &Output, stdout: &str) {
    assert!(
        stdout.contains("RSS_PROBE_OK"),
        "probe did not complete a measurement.\n{}",
        describe(out)
    );
    assert!(
        out.status.success(),
        "probe exited non-zero.\n{}",
        describe(out)
    );
    // Guard the guard: a probe that never entered spans, or never sampled,
    // would report a flat line without having tested anything.
    let spans = field(stdout, "spans=").unwrap_or(0.0);
    assert!(
        spans >= 1000.0,
        "probe reported implausibly little span load ({spans}).\n{}",
        describe(out)
    );
    let samples = field(stdout, "samples=").unwrap_or(0.0);
    assert!(
        samples >= 20.0,
        "probe collected too few RSS samples ({samples}) to fit a slope.\n{}",
        describe(out)
    );
}

#[test]
#[ignore = "takes ~130s (two probe runs); run via `make ci-rss-probe`"]
fn profiling_bridge_adds_no_rss_growth_under_span_load() {
    let (with_out, with_stdout) = run_probe(true);
    assert_measured(&with_out, &with_stdout);
    let (without_out, without_stdout) = run_probe(false);
    assert_measured(&without_out, &without_stdout);

    let with_slope = field(&with_stdout, "mib_per_hour=")
        .unwrap_or_else(|| panic!("no mib_per_hour.\n{}", describe(&with_out)));
    let without_slope = field(&without_stdout, "mib_per_hour=")
        .unwrap_or_else(|| panic!("no mib_per_hour.\n{}", describe(&without_out)));

    // The bridge's own contribution. Span churn alone moves RSS — the tracing
    // registry and the OTLP span pipeline allocate per span, and the batch
    // processor spends the run filling and dropping with no collector to drain
    // to — so the control arm carries that cost and the difference is what the
    // bridge added.
    let attributable = with_slope - without_slope;

    // Print on the happy path too. A gate that measures something should say
    // what it measured, so a slow drift toward the ceiling is visible in CI
    // logs before it trips rather than the day it does.
    println!(
        "profiling bridge RSS: with={with_slope:.2} MiB/h without={without_slope:.2} MiB/h \
         attributable={attributable:.2} MiB/h (ceiling {MAX_MIB_PER_HOUR:.1})"
    );

    assert!(
        attributable < MAX_MIB_PER_HOUR,
        "profiling bridge added {attributable:.2} MiB/h over the no-profiling \
         control (ceiling {MAX_MIB_PER_HOUR:.1}); with={with_slope:.2} \
         without={without_slope:.2}. This is the brefwiz-spiffe OOM regression \
         — check whether anything reintroduced per-span pyroscope tagging.\n\
         --- with profiling ---\n{}\n--- control ---\n{}",
        describe(&with_out),
        describe(&without_out),
    );
}

/// Reintroducing per-span tagging must not be possible unnoticed.
///
/// This is a source guard, not a behavioural one, and that is a deliberate
/// retreat from four failed attempts at the latter. Through the real
/// `Telemetry` path the defect is close to externally unobservable on the
/// shipped target: upload size 823 bytes fixed against 828 broken, RSS *higher*
/// on the healthy build, span throughput 14,499 against 14,518, and the
/// `trace_id`/`span_id` labels never reach the wire at all — `add_tag` pays for
/// a full `dump_report` and then the tag does not survive into the encoded
/// profile. Every one of those was measured, and every one of them passed on
/// the broken build.
///
/// What is unambiguous is the call that starts it. `tag_wrapper()` hands out
/// the `add_tag`/`remove_tag` closures, and nothing else in this crate has any
/// reason to want them, so its absence from the bridge is the invariant worth
/// pinning. The repo already gates this class of thing textually — see
/// ci-workflows' sidecar `/tmp` mount check, which is explicitly a heuristic
/// scan and has caught a production crash-loop.
///
/// The probe alongside this test is a memory smoke test, not this gate.
#[test]
fn profiling_bridge_does_not_wire_per_span_tagging() {
    let src = include_str!("../src/profiling.rs");

    let calls: Vec<&str> = src
        .lines()
        .map(str::trim)
        .filter(|l| l.contains("tag_wrapper()") && !l.starts_with("//") && !l.starts_with("///"))
        .collect();

    assert!(
        calls.is_empty(),
        "src/profiling.rs calls tag_wrapper(), which arms per-span pyroscope \
         tagging: every tag call rebuilds and clears the whole profile \
         (~87,000/s measured), which grew RSS 7.4 MiB/h against 1.0 without and \
         OOM-killed brefwiz-spiffe every ~5.5h against a 512Mi cgroup. It also \
         emptied the profiles it was meant to enrich. Offending lines: {calls:?}"
    );

    // Guard the guard: if the layer ever regains an on_enter/on_exit body, the
    // check above is looking at the wrong thing.
    let layer_impl = src
        .split("impl<S> tracing_subscriber::Layer<S> for ProfilingTagLayer")
        .nth(1)
        .expect("ProfilingTagLayer Layer impl not found — has it been renamed?");
    let body = &layer_impl[..layer_impl.find("\n}").unwrap_or(layer_impl.len())];
    assert!(
        !body.contains("fn on_enter") && !body.contains("fn on_exit"),
        "ProfilingTagLayer has regained span callbacks; it is supposed to be \
         inert. Body:\n{body}"
    );
}
