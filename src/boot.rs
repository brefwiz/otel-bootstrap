// SPDX-License-Identifier: MIT
//! Boot timeline: how long each startup phase took, visible while the process
//! is still booting and exported as backdated spans once telemetry is live.
//!
//! A service spends its first seconds before any exporter exists: resolving
//! its identity, reaching the providers it depends on, fetching configuration.
//! A `tracing` event emitted there reaches no subscriber, and a span opened
//! there cannot be exported with its real start time later, because `tracing`
//! stamps spans when they are recorded, not when the work happened.
//!
//! The [`Timeline`] records each phase with its real start and end (monotonic
//! for durations, wall clock for export) and:
//!
//! - writes one `logfmt` line to stderr the moment a phase completes, so a
//!   process stuck in boot shows in its log where it got to:
//!
//!   ```text
//!   boot phase=config-fetch outcome=ok took_ms=812 at_ms=2345 service=orders
//!   boot phase=ready outcome=ok took_ms=3120 at_ms=3120 service=orders
//!   ```
//!
//!   `at_ms` is measured from process start, `took_ms` is the phase's own
//!   duration, and the reserved `phase=ready` line carries the whole boot;
//!
//! - once [`Timeline::flush`] connects it to a tracer provider (which
//!   [`crate::TelemetryBuilder::init`] does), exports every buffered phase as a
//!   span carrying its original start and end, parented under one `boot` root
//!   span that starts at process start and ends at [`Timeline::ready`], plus a
//!   structured log event per phase. Phases completing after that are exported
//!   as they complete.
//!
//! The timeline is process-global ([`Timeline::global`]): a boot crosses
//! several owners (a runtime, a framework, the application) and several init
//! paths, and none of them should have to thread a handle through the others.
//! Flushing is idempotent: the first provider wins and later calls export
//! nothing twice. With telemetry disabled, nothing is ever flushed; phases are
//! still echoed to stderr and nothing else is paid.
//!
//! # Example
//! ```no_run
//! # async fn connect() {}
//! # async fn run() {
//! use otel_bootstrap::boot;
//!
//! boot::time("broker-connect", connect()).await;
//!
//! let phase = boot::phase("load-cache");
//! // … synchronous work; dropping `phase` without `finish` records a failure …
//! phase.finish();
//!
//! boot::ready();
//! # }
//! ```

use std::borrow::Cow;
use std::fmt::Write as _;
use std::future::IntoFuture;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use opentelemetry::trace::{
    Span as _, SpanBuilder, SpanKind, Status, TraceContextExt as _, Tracer as _,
    TracerProvider as _,
};
use opentelemetry::{Context, KeyValue};
use opentelemetry_sdk::trace::{SdkTracer, SdkTracerProvider};

/// Most phases one timeline keeps. A phase recorded past this is still echoed
/// to stderr (and exported, if telemetry is already live) but not buffered;
/// the root span reports how many were dropped.
pub const MAX_PHASES: usize = 256;

/// Name of the milestone line [`Timeline::ready`] writes. Reserved.
pub const READY: &str = "ready";

const ROOT_SPAN: &str = "boot";
const LOG_TARGET: &str = "otel_bootstrap::boot";

/// How a phase ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The phase ran to completion.
    Ok,
    /// The phase was abandoned: an error, a panic, or a cancelled future.
    Failed,
}

impl Outcome {
    /// The value written as `outcome=` and exported as `boot.outcome`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Failed => "failed",
        }
    }
}

/// One completed boot phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phase {
    pub name: Cow<'static, str>,
    pub outcome: Outcome,
    /// Wall-clock start, as exported on the span.
    pub start: SystemTime,
    /// Wall-clock end, as exported on the span.
    pub end: SystemTime,
    /// Time spent in this phase.
    pub took: Duration,
    /// Time from the timeline's origin (process start, for the global one) to
    /// the end of this phase.
    pub at: Duration,
}

struct Recorded {
    phase: Phase,
    exported: bool,
}

#[derive(Clone)]
struct Exporter {
    tracer: SdkTracer,
    root: Context,
}

#[derive(Default)]
struct State {
    service: Option<String>,
    phases: Vec<Recorded>,
    dropped: u64,
    ready: Option<(SystemTime, Duration)>,
    exporter: Option<Exporter>,
    root_ended: bool,
}

/// A record of boot phases. See the [module documentation](self).
pub struct Timeline {
    origin: Instant,
    origin_wall: SystemTime,
    state: Mutex<State>,
    echo: Echo,
}

enum Echo {
    Stderr,
    #[cfg(test)]
    Capture(Mutex<Vec<String>>),
}

impl Default for Timeline {
    fn default() -> Self {
        Self::new()
    }
}

impl Timeline {
    /// A timeline whose origin is now. Services use [`Timeline::global`];
    /// this exists for tests and for embedding a second, independent boot.
    #[must_use]
    pub fn new() -> Self {
        Self::starting_at(Instant::now())
    }

    fn starting_at(origin: Instant) -> Self {
        let now = Instant::now();
        let origin_wall = SystemTime::now()
            .checked_sub(now.saturating_duration_since(origin))
            .unwrap_or_else(SystemTime::now);
        Self {
            origin,
            origin_wall,
            state: Mutex::new(State::default()),
            echo: Echo::Stderr,
        }
    }

    /// The process-wide timeline, whose origin is the process's start time
    /// where the platform reports it (Linux) and its first use otherwise.
    pub fn global() -> &'static Self {
        static GLOBAL: OnceLock<Timeline> = OnceLock::new();
        GLOBAL.get_or_init(|| {
            let now = Instant::now();
            let origin = process_age()
                .and_then(|age| now.checked_sub(age))
                .unwrap_or(now);
            Self::starting_at(origin)
        })
    }

    /// Name the service in the stderr lines. Without it the lines use
    /// `OTEL_SERVICE_NAME`, then the executable's name, until
    /// [`Timeline::flush`] supplies the telemetry service name.
    pub fn set_service_name(&self, name: &str) {
        self.lock().service = Some(name.to_owned());
    }

    /// Start timing a phase. The phase is recorded when the guard is
    /// [finished](PhaseGuard::finish) or, as [`Outcome::Failed`], when it is
    /// dropped unfinished.
    pub fn phase(&self, name: impl Into<Cow<'static, str>>) -> PhaseGuard<'_> {
        PhaseGuard {
            timeline: self,
            name: Some(name.into()),
            start: Instant::now(),
        }
    }

    /// Time `work` as one phase. If the future is dropped before completing,
    /// the phase is recorded as [`Outcome::Failed`].
    pub async fn time<F: IntoFuture>(
        &self,
        name: impl Into<Cow<'static, str>>,
        work: F,
    ) -> F::Output {
        let phase = self.phase(name);
        let output = work.into_future().await;
        phase.finish();
        output
    }

    /// Like [`Timeline::time`], recording [`Outcome::Failed`] when `work`
    /// returns `Err`.
    pub async fn try_time<T, E, F>(
        &self,
        name: impl Into<Cow<'static, str>>,
        work: F,
    ) -> Result<T, E>
    where
        F: IntoFuture<Output = Result<T, E>>,
    {
        let phase = self.phase(name);
        let output = work.into_future().await;
        match output {
            Ok(_) => phase.finish(),
            Err(_) => phase.fail(),
        }
        output
    }

    /// Mark boot complete: writes the `phase=ready` line and ends the `boot`
    /// root span. Only the first call counts.
    pub fn ready(&self) {
        let end = Instant::now();
        let at = end.saturating_duration_since(self.origin);
        let wall = self.wall(end);
        let mut state = self.lock();
        if state.ready.is_some() {
            return;
        }
        state.ready = Some((wall, at));
        let end_root = state.take_root_to_end();
        let service = state.service_label();
        drop(state);

        self.echo(&line(READY, Outcome::Ok, at, at, &service));
        if let Some((exporter, count, dropped)) = end_root {
            end_root_span(&exporter, wall, "ready", count, dropped);
            tracing::info!(
                target: LOG_TARGET,
                took_ms = millis(at),
                "boot ready"
            );
        }
    }

    /// Connect the timeline to `provider`: export every buffered phase as a
    /// span with its original timestamps under the `boot` root span, emit a
    /// log event for each, and export later phases as they complete.
    ///
    /// Idempotent: the first provider wins, and a later call exports nothing
    /// and returns `0`. Returns the number of phases exported by this call.
    pub fn flush(&self, provider: &SdkTracerProvider, service_name: &str) -> usize {
        self.attach(provider, service_name).unwrap_or(0)
    }

    /// `Some(exported)` when this call attached the provider, `None` when a
    /// provider was already attached.
    pub(crate) fn attach(&self, provider: &SdkTracerProvider, service_name: &str) -> Option<usize> {
        let mut state = self.lock();
        if state.exporter.is_some() {
            return None;
        }
        if state.service.is_none() {
            state.service = Some(service_name.to_owned());
        }
        let tracer = provider.tracer(concat!(env!("CARGO_PKG_NAME"), ".boot"));
        let root_span = tracer.build_with_context(
            SpanBuilder::from_name(ROOT_SPAN)
                .with_kind(SpanKind::Internal)
                .with_start_time(self.origin_wall),
            &Context::new(),
        );
        let exporter = Exporter {
            tracer,
            root: Context::new().with_span(root_span),
        };
        state.exporter = Some(exporter.clone());
        let pending: Vec<Phase> = state
            .phases
            .iter_mut()
            .filter(|recorded| !recorded.exported)
            .map(|recorded| {
                recorded.exported = true;
                recorded.phase.clone()
            })
            .collect();
        let ready = state.ready;
        let end_root = if ready.is_some() {
            state.take_root_to_end()
        } else {
            None
        };
        drop(state);

        for phase in &pending {
            export(&exporter, phase);
        }
        if let (Some((wall, at)), Some((exporter, count, dropped))) = (ready, end_root) {
            end_root_span(&exporter, wall, "ready", count, dropped);
            tracing::info!(target: LOG_TARGET, took_ms = millis(at), "boot ready");
        }
        Some(pending.len())
    }

    /// End the root span of a boot that never reached [`Timeline::ready`], so
    /// it is exported before its provider shuts down. No-op otherwise.
    pub(crate) fn close(&self) {
        let now = self.wall(Instant::now());
        let end_root = self.lock().take_root_to_end();
        if let Some((exporter, count, dropped)) = end_root {
            end_root_span(&exporter, now, "incomplete", count, dropped);
        }
    }

    /// Every phase buffered so far, in completion order.
    #[must_use]
    pub fn phases(&self) -> Vec<Phase> {
        self.lock()
            .phases
            .iter()
            .map(|recorded| recorded.phase.clone())
            .collect()
    }

    fn record(&self, name: Cow<'static, str>, start: Instant, end: Instant, outcome: Outcome) {
        let phase = Phase {
            name,
            outcome,
            start: self.wall(start),
            end: self.wall(end),
            took: end.saturating_duration_since(start),
            at: end.saturating_duration_since(self.origin),
        };
        let mut state = self.lock();
        let exporter = state.exporter.clone();
        if state.phases.len() < MAX_PHASES {
            state.phases.push(Recorded {
                phase: phase.clone(),
                exported: exporter.is_some(),
            });
        } else {
            state.dropped += 1;
        }
        let service = state.service_label();
        drop(state);

        self.echo(&line(&phase.name, outcome, phase.took, phase.at, &service));
        if let Some(exporter) = exporter {
            export(&exporter, &phase);
        }
    }

    fn wall(&self, at: Instant) -> SystemTime {
        self.origin_wall + at.saturating_duration_since(self.origin)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn echo(&self, line: &str) {
        match &self.echo {
            Echo::Stderr => eprintln!("{line}"),
            #[cfg(test)]
            Echo::Capture(lines) => lines
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(line.to_owned()),
        }
    }
}

impl State {
    fn service_label(&self) -> String {
        self.service
            .clone()
            .unwrap_or_else(|| default_service_name().to_owned())
    }

    /// The exporter and root-span attributes, exactly once, when there is a
    /// root span left to end.
    fn take_root_to_end(&mut self) -> Option<(Exporter, usize, u64)> {
        if self.root_ended {
            return None;
        }
        let exporter = self.exporter.clone()?;
        self.root_ended = true;
        Some((exporter, self.phases.len(), self.dropped))
    }
}

/// Times one phase until [finished](PhaseGuard::finish); dropping it
/// unfinished records [`Outcome::Failed`].
#[must_use = "a phase is recorded when its guard is finished or dropped"]
pub struct PhaseGuard<'t> {
    timeline: &'t Timeline,
    name: Option<Cow<'static, str>>,
    start: Instant,
}

impl PhaseGuard<'_> {
    /// Record the phase as completed.
    pub fn finish(mut self) {
        self.end(Outcome::Ok);
    }

    /// Record the phase as failed.
    pub fn fail(mut self) {
        self.end(Outcome::Failed);
    }

    fn end(&mut self, outcome: Outcome) {
        if let Some(name) = self.name.take() {
            self.timeline
                .record(name, self.start, Instant::now(), outcome);
        }
    }
}

impl Drop for PhaseGuard<'_> {
    fn drop(&mut self) {
        self.end(Outcome::Failed);
    }
}

/// Start timing a phase on the [global](Timeline::global) timeline.
pub fn phase(name: impl Into<Cow<'static, str>>) -> PhaseGuard<'static> {
    Timeline::global().phase(name)
}

/// Time `work` as one phase on the [global](Timeline::global) timeline.
pub async fn time<F: IntoFuture>(name: impl Into<Cow<'static, str>>, work: F) -> F::Output {
    Timeline::global().time(name, work).await
}

/// [`Timeline::try_time`] on the [global](Timeline::global) timeline.
pub async fn try_time<T, E, F>(name: impl Into<Cow<'static, str>>, work: F) -> Result<T, E>
where
    F: IntoFuture<Output = Result<T, E>>,
{
    Timeline::global().try_time(name, work).await
}

/// Mark boot complete on the [global](Timeline::global) timeline.
pub fn ready() {
    Timeline::global().ready();
}

fn export(exporter: &Exporter, phase: &Phase) {
    let mut span = exporter.tracer.build_with_context(
        SpanBuilder::from_name(phase.name.clone())
            .with_kind(SpanKind::Internal)
            .with_start_time(phase.start)
            .with_attributes([
                KeyValue::new("boot.phase", phase.name.clone()),
                KeyValue::new("boot.outcome", phase.outcome.as_str()),
                KeyValue::new("boot.at_ms", millis_i64(phase.at)),
            ]),
        &exporter.root,
    );
    if phase.outcome == Outcome::Failed {
        span.set_status(Status::error("boot phase did not complete"));
    }
    span.end_with_timestamp(phase.end);

    let (took_ms, at_ms) = (millis(phase.took), millis(phase.at));
    match phase.outcome {
        Outcome::Ok => tracing::info!(
            target: LOG_TARGET,
            phase = %phase.name,
            outcome = phase.outcome.as_str(),
            took_ms,
            at_ms,
            "boot phase complete"
        ),
        Outcome::Failed => tracing::warn!(
            target: LOG_TARGET,
            phase = %phase.name,
            outcome = phase.outcome.as_str(),
            took_ms,
            at_ms,
            "boot phase failed"
        ),
    }
}

fn end_root_span(
    exporter: &Exporter,
    end: SystemTime,
    outcome: &'static str,
    count: usize,
    dropped: u64,
) {
    let root = exporter.root.span();
    root.set_attribute(KeyValue::new("boot.outcome", outcome));
    root.set_attribute(KeyValue::new(
        "boot.phase_count",
        i64::try_from(count).unwrap_or(i64::MAX),
    ));
    root.set_attribute(KeyValue::new(
        "boot.phases_dropped",
        i64::try_from(dropped).unwrap_or(i64::MAX),
    ));
    root.end_with_timestamp(end);
}

/// One `logfmt` line: `boot phase=… outcome=… took_ms=… at_ms=… service=…`.
fn line(name: &str, outcome: Outcome, took: Duration, at: Duration, service: &str) -> String {
    let mut out = String::from("boot phase=");
    push_value(&mut out, name);
    let _ = write!(
        out,
        " outcome={} took_ms={} at_ms={} service=",
        outcome.as_str(),
        millis(took),
        millis(at)
    );
    push_value(&mut out, service);
    out
}

/// A `logfmt` value, quoted when it would otherwise not parse back.
fn push_value(out: &mut String, value: &str) {
    let bare = !value.is_empty()
        && value
            .chars()
            .all(|c| !c.is_whitespace() && !c.is_control() && c != '"' && c != '=' && c != '\\');
    if bare {
        out.push_str(value);
        return;
    }
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{{{:x}}}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn millis_i64(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

fn default_service_name() -> &'static str {
    static NAME: OnceLock<String> = OnceLock::new();
    NAME.get_or_init(|| {
        std::env::var("OTEL_SERVICE_NAME")
            .ok()
            .filter(|name| !name.is_empty())
            .or_else(|| {
                std::env::current_exe().ok().and_then(|exe| {
                    exe.file_stem()
                        .and_then(|stem| stem.to_str())
                        .map(str::to_owned)
                })
            })
            .unwrap_or_else(|| "unknown_service".to_owned())
    })
}

/// How long ago this process started, from `/proc/self/stat` (start time in
/// clock ticks since system boot) and `/proc/uptime`. The kernel's
/// user-visible tick rate (`USER_HZ`) is fixed at 100 on every architecture
/// Linux exposes it on, so it needs no `sysconf` call.
#[cfg(target_os = "linux")]
fn process_age() -> Option<Duration> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let uptime = std::fs::read_to_string("/proc/uptime").ok()?;
    process_age_from(&stat, &uptime)
}

#[cfg(not(target_os = "linux"))]
fn process_age() -> Option<Duration> {
    None
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn process_age_from(stat: &str, uptime: &str) -> Option<Duration> {
    const MILLIS_PER_TICK: u64 = 1000 / 100;
    // The command name is parenthesised and may itself contain spaces or
    // parentheses; every field after the last `)` is space-separated.
    // `starttime` is field 22 overall, the 20th after the name.
    let fields = stat.rsplit_once(')')?.1;
    let start_ticks: u64 = fields.split_whitespace().nth(19)?.parse().ok()?;
    let uptime_secs: f64 = uptime.split_whitespace().next()?.parse().ok()?;
    let uptime = Duration::try_from_secs_f64(uptime_secs).ok()?;
    uptime.checked_sub(Duration::from_millis(
        start_ticks.checked_mul(MILLIS_PER_TICK)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capturing() -> Timeline {
        Timeline {
            echo: Echo::Capture(Mutex::new(Vec::new())),
            ..Timeline::new()
        }
    }

    fn captured(timeline: &Timeline) -> Vec<String> {
        match &timeline.echo {
            Echo::Capture(lines) => lines.lock().unwrap().clone(),
            Echo::Stderr => unreachable!("capturing timeline"),
        }
    }

    #[test]
    fn each_completed_phase_is_one_logfmt_line_on_stderr() {
        let timeline = capturing();
        timeline.set_service_name("orders");
        let start = timeline.origin;
        timeline.record(
            "config-fetch".into(),
            start + Duration::from_millis(200),
            start + Duration::from_millis(1012),
            Outcome::Ok,
        );

        assert_eq!(
            captured(&timeline),
            ["boot phase=config-fetch outcome=ok took_ms=812 at_ms=1012 service=orders"]
        );
    }

    #[test]
    fn ready_writes_the_whole_boot_once() {
        let timeline = capturing();
        timeline.set_service_name("orders");
        timeline.ready();
        timeline.ready();

        let lines = captured(&timeline);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].starts_with("boot phase=ready outcome=ok took_ms="),
            "{}",
            lines[0]
        );
        assert!(lines[0].ends_with(" service=orders"), "{}", lines[0]);
    }

    #[test]
    fn values_that_would_not_parse_back_are_quoted() {
        let line = line(
            "load \"cache\"",
            Outcome::Failed,
            Duration::from_millis(5),
            Duration::from_millis(9),
            "a=b\\c\nd\u{1}",
        );
        assert_eq!(
            line,
            r#"boot phase="load \"cache\"" outcome=failed took_ms=5 at_ms=9 service="a=b\\c\nd\u{1}""#
        );
        assert_eq!(
            super::line("", Outcome::Ok, Duration::ZERO, Duration::ZERO, "svc"),
            r#"boot phase="" outcome=ok took_ms=0 at_ms=0 service=svc"#
        );
    }

    #[test]
    fn an_unfinished_guard_records_a_failure() {
        let timeline = capturing();
        drop(timeline.phase("dropped"));
        timeline.phase("failed").fail();
        timeline.phase("finished").finish();

        let outcomes: Vec<_> = timeline
            .phases()
            .into_iter()
            .map(|phase| (phase.name.into_owned(), phase.outcome))
            .collect();
        assert_eq!(
            outcomes,
            [
                ("dropped".to_owned(), Outcome::Failed),
                ("failed".to_owned(), Outcome::Failed),
                ("finished".to_owned(), Outcome::Ok),
            ]
        );
    }

    #[tokio::test]
    async fn futures_are_timed_and_errors_and_cancellation_fail_the_phase() {
        let timeline = capturing();
        timeline
            .time("sleep", tokio::time::sleep(Duration::from_millis(20)))
            .await;
        let ok: Result<u8, &str> = timeline.try_time("ok", async { Ok(1) }).await;
        let err: Result<u8, &str> = timeline.try_time("err", async { Err("no") }).await;
        assert_eq!((ok, err), (Ok(1), Err("no")));
        let cancelled = timeline.time("cancelled", std::future::pending::<()>());
        let _ = tokio::time::timeout(Duration::from_millis(1), cancelled).await;

        let phases = timeline.phases();
        let outcomes: Vec<_> = phases.iter().map(|p| (&*p.name, p.outcome)).collect();
        assert_eq!(
            outcomes,
            [
                ("sleep", Outcome::Ok),
                ("ok", Outcome::Ok),
                ("err", Outcome::Failed),
                ("cancelled", Outcome::Failed),
            ]
        );
        assert!(
            phases[0].took >= Duration::from_millis(20),
            "{:?}",
            phases[0]
        );
        assert_eq!(
            phases[0].end.duration_since(phases[0].start).unwrap(),
            phases[0].took
        );
        assert!(phases[1].at >= phases[0].at);
    }

    #[test]
    fn without_telemetry_phases_are_echoed_and_buffered_up_to_the_bound() {
        let timeline = capturing();
        for _ in 0..MAX_PHASES + 3 {
            timeline.phase("step").finish();
        }
        assert_eq!(timeline.phases().len(), MAX_PHASES);
        assert_eq!(captured(&timeline).len(), MAX_PHASES + 3);
        let state = timeline.lock();
        assert_eq!(state.dropped, 3);
        assert!(state.exporter.is_none());
        assert!(state.phases.iter().all(|recorded| !recorded.exported));
    }

    #[test]
    fn close_without_a_provider_does_nothing() {
        let timeline = capturing();
        timeline.close();
        assert!(!timeline.lock().root_ended);
    }

    #[test]
    fn the_service_falls_back_to_a_derived_name() {
        let timeline = capturing();
        timeline.phase("step").finish();
        let lines = captured(&timeline);
        assert!(
            lines[0].ends_with(&format!(" service={}", default_service_name())),
            "{}",
            lines[0]
        );
        assert!(!default_service_name().is_empty());
    }

    #[test]
    fn the_global_timeline_starts_no_later_than_its_first_use() {
        let before = Instant::now();
        let global = Timeline::global();
        assert!(global.origin <= before);
        assert!(std::ptr::eq(global, Timeline::global()));
    }

    #[test]
    fn process_age_reads_start_ticks_after_the_command_name() {
        let stat =
            "42 (my (odd) cmd) S 1 42 42 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 1000 1000 10 0";
        assert_eq!(
            process_age_from(stat, "15.50 30.00\n"),
            Some(Duration::from_millis(5500))
        );
        assert_eq!(process_age_from(stat, "5.00 1.00\n"), None, "negative age");
        assert_eq!(process_age_from("garbage", "1.0 1.0"), None);
        assert_eq!(process_age_from(stat, "-1.0 1.0"), None);
        assert_eq!(process_age_from(stat, ""), None);
    }
}
