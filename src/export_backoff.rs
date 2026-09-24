// SPDX-License-Identifier: MIT
//! An export failure is logged once, then again only as its backoff allows.
//!
//! The OpenTelemetry SDK logs every failed export batch at `ERROR`: with the
//! default five-second schedule and three signals, an unreachable collector
//! writes a line every couple of seconds, forever, and buries every real
//! error the service logs. [`ExportFailureBackoff`] lets the first failure of
//! each kind through, then the next one only after a minute, then two, four,
//! and so on up to an hour. Any other event, including the SDK's own events
//! that are not failures, passes untouched.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

const FIRST_BACKOFF: Duration = Duration::from_secs(60);
const MAX_BACKOFF: Duration = Duration::from_secs(60 * 60);

#[derive(Clone, Copy)]
struct Window {
    next_allowed: Instant,
    backoff: Duration,
}

/// Rate-limits the SDK's own export-failure events; see the module docs.
#[derive(Default)]
pub struct ExportFailureBackoff {
    windows: Mutex<HashMap<String, Window>>,
}

impl ExportFailureBackoff {
    /// Whether an event of `kind` at `now` is let through, and records it.
    fn admit(&self, kind: &str, now: Instant) -> bool {
        let mut windows = self
            .windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match windows.get_mut(kind) {
            None => {
                windows.insert(
                    kind.to_owned(),
                    Window {
                        next_allowed: now + FIRST_BACKOFF,
                        backoff: FIRST_BACKOFF,
                    },
                );
                true
            }
            Some(window) if now >= window.next_allowed => {
                window.backoff = (window.backoff * 2).min(MAX_BACKOFF);
                window.next_allowed = now + window.backoff;
                true
            }
            Some(_) => false,
        }
    }
}

/// The SDK names each internal event in a `name` field, e.g.
/// `BatchSpanProcessor.ExportError`.
#[derive(Default)]
struct EventName(Option<String>);

impl Visit for EventName {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "name" {
            self.0 = Some(value.to_owned());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "name" && self.0.is_none() {
            self.0 = Some(format!("{value:?}").trim_matches('"').to_owned());
        }
    }
}

/// The failure kind of an SDK export-failure event, or `None` for any other
/// event.
fn export_failure_kind(event: &Event<'_>) -> Option<String> {
    let metadata = event.metadata();
    if !metadata.target().starts_with("opentelemetry") || *metadata.level() > Level::WARN {
        return None;
    }
    let mut name = EventName::default();
    event.record(&mut name);
    let kind = name.0.unwrap_or_else(|| metadata.name().to_owned());
    kind.contains("Export").then_some(kind)
}

impl<S: Subscriber> Layer<S> for ExportFailureBackoff {
    fn event_enabled(&self, event: &Event<'_>, _ctx: Context<'_, S>) -> bool {
        match export_failure_kind(event) {
            Some(kind) => self.admit(&kind, Instant::now()),
            None => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_failure_is_logged_and_repeats_wait_for_the_backoff() {
        let backoff = ExportFailureBackoff::default();
        let start = Instant::now();
        assert!(backoff.admit("BatchSpanProcessor.ExportError", start));
        assert!(!backoff.admit(
            "BatchSpanProcessor.ExportError",
            start + Duration::from_secs(5)
        ));
        assert!(!backoff.admit(
            "BatchSpanProcessor.ExportError",
            start + Duration::from_secs(59)
        ));
        assert!(backoff.admit(
            "BatchSpanProcessor.ExportError",
            start + Duration::from_secs(60)
        ));
        // The window doubles: two minutes after the second line.
        assert!(!backoff.admit(
            "BatchSpanProcessor.ExportError",
            start + Duration::from_secs(170)
        ));
        assert!(backoff.admit(
            "BatchSpanProcessor.ExportError",
            start + Duration::from_secs(180)
        ));
    }

    #[test]
    fn each_kind_of_failure_has_its_own_window() {
        let backoff = ExportFailureBackoff::default();
        let start = Instant::now();
        assert!(backoff.admit("BatchSpanProcessor.ExportError", start));
        assert!(backoff.admit("BatchLogProcessor.ExportError", start));
    }

    #[test]
    fn the_backoff_never_exceeds_an_hour() {
        let backoff = ExportFailureBackoff::default();
        let mut now = Instant::now();
        assert!(backoff.admit("k", now));
        for _ in 0..20 {
            now += MAX_BACKOFF;
            assert!(backoff.admit("k", now));
        }
    }

    #[test]
    fn only_sdk_export_failures_are_rate_limited() {
        use tracing_subscriber::prelude::*;

        let seen = std::sync::Arc::new(Mutex::new(Vec::<String>::new()));
        struct Record(std::sync::Arc<Mutex<Vec<String>>>);
        impl<S: Subscriber> Layer<S> for Record {
            fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
                self.0
                    .lock()
                    .unwrap()
                    .push(event.metadata().target().to_owned());
            }
        }
        let subscriber = tracing_subscriber::registry()
            .with(ExportFailureBackoff::default())
            .with(Record(seen.clone()));
        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..3 {
                tracing::error!(target: "opentelemetry_sdk", name = "BatchSpanProcessor.ExportError", "export failed");
                tracing::error!(target: "my_service", "a real error");
            }
        });
        let seen = seen.lock().unwrap();
        assert_eq!(seen.iter().filter(|t| *t == "opentelemetry_sdk").count(), 1);
        assert_eq!(seen.iter().filter(|t| *t == "my_service").count(), 3);
    }
}
