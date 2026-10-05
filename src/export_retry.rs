// SPDX-License-Identifier: MIT
//! A log or span export refused as unavailable is attempted again before it is lost.
//!
//! The first log and span batches of a process leave about a second after boot,
//! and carry the service's boot-time telemetry. A pod can reach that moment before
//! the route to the collector's Service is programmed, so the connect is
//! refused. Transport retries are disabled so this wrapper owns the retry budget.
//! It retries on the runtime that was current at construction, so the delay
//! runs wherever the processor thread is.

use std::time::Duration;

use opentelemetry::logs::Severity;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::logs::{LogBatch, LogExporter};
use opentelemetry_sdk::trace::{SpanData, SpanExporter};

/// Delays before the second, third and fourth attempt.
const BACKOFF: [Duration; 3] = [
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
];

/// Wraps a [`LogExporter`] or [`SpanExporter`], retrying an export that failed as `Unavailable`.
#[derive(Debug)]
pub(crate) struct RetryUnavailable<E> {
    inner: E,
    runtime: Option<tokio::runtime::Handle>,
    backoff: &'static [Duration],
}

impl<E> RetryUnavailable<E> {
    /// Must be called inside a Tokio runtime to retry at all; outside one the
    /// wrapper makes the single attempt the inner exporter would.
    pub(crate) fn new(inner: E) -> Self {
        Self::with_backoff(inner, &BACKOFF)
    }

    fn with_backoff(inner: E, backoff: &'static [Duration]) -> Self {
        Self {
            inner,
            runtime: tokio::runtime::Handle::try_current().ok(),
            backoff,
        }
    }

    async fn pause(&self, delay: Duration) {
        if let Some(runtime) = &self.runtime {
            // The timer is created inside the task: building it here, on a
            // thread with no runtime entered, panics.
            let _ = runtime
                .spawn(async move { tokio::time::sleep(delay).await })
                .await;
        }
    }
}

fn is_unavailable(result: &OTelSdkResult) -> bool {
    matches!(result, Err(error) if error.to_string().contains("Unavailable"))
}

impl<E: LogExporter> LogExporter for RetryUnavailable<E> {
    async fn export(&self, batch: LogBatch<'_>) -> OTelSdkResult {
        let records: Vec<_> = batch.iter().collect();
        let mut result = self.inner.export(LogBatch::new(&records)).await;
        if self.runtime.is_none() {
            return result;
        }
        for delay in self.backoff {
            if !is_unavailable(&result) {
                break;
            }
            self.pause(*delay).await;
            result = self.inner.export(LogBatch::new(&records)).await;
        }
        result
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn event_enabled(&self, level: Severity, target: &str, name: Option<&str>) -> bool {
        self.inner.event_enabled(level, target, name)
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

impl<E: SpanExporter> SpanExporter for RetryUnavailable<E> {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        if self.runtime.is_none() {
            return self.inner.export(batch).await;
        }
        let mut result = self.inner.export(batch.clone()).await;
        for delay in self.backoff {
            if !is_unavailable(&result) {
                break;
            }
            self.pause(*delay).await;
            result = self.inner.export(batch.clone()).await;
        }
        result
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use opentelemetry_sdk::error::OTelSdkError;

    const FAST: [Duration; 3] = [Duration::from_millis(1); 3];

    #[derive(Debug)]
    struct Flaky {
        failures: usize,
        error: &'static str,
        calls: AtomicUsize,
    }

    impl LogExporter for Flaky {
        async fn export(&self, batch: LogBatch<'_>) -> OTelSdkResult {
            assert_eq!(batch.iter().count(), 0);
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call < self.failures {
                Err(OTelSdkError::InternalFailure(self.error.into()))
            } else {
                Ok(())
            }
        }
    }

    impl SpanExporter for Flaky {
        async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
            assert!(batch.is_empty());
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call < self.failures {
                Err(OTelSdkError::InternalFailure(self.error.into()))
            } else {
                Ok(())
            }
        }
    }

    fn flaky(failures: usize, error: &'static str) -> RetryUnavailable<Flaky> {
        RetryUnavailable::with_backoff(
            Flaky {
                failures,
                error,
                calls: AtomicUsize::new(0),
            },
            &FAST,
        )
    }

    #[tokio::test]
    async fn an_unavailable_export_is_retried_until_it_succeeds() {
        let exporter = flaky(2, "gRPC code: Unavailable: Connection refused");
        assert!(
            LogExporter::export(&exporter, LogBatch::new(&[]))
                .await
                .is_ok()
        );
        assert_eq!(exporter.inner.calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn retries_are_bounded_and_the_last_failure_is_returned() {
        let exporter = flaky(usize::MAX, "gRPC code: Unavailable");
        assert!(
            LogExporter::export(&exporter, LogBatch::new(&[]))
                .await
                .is_err()
        );
        assert_eq!(exporter.inner.calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn any_other_failure_is_not_retried() {
        let exporter = flaky(usize::MAX, "gRPC code: InvalidArgument");
        assert!(
            LogExporter::export(&exporter, LogBatch::new(&[]))
                .await
                .is_err()
        );
        assert_eq!(exporter.inner.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_unavailable_span_export_is_retried_until_it_succeeds() {
        let exporter = flaky(2, "gRPC code: Unavailable");
        assert!(SpanExporter::export(&exporter, Vec::new()).await.is_ok());
        assert_eq!(exporter.inner.calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn outside_a_runtime_a_single_attempt_is_made() {
        let exporter = flaky(usize::MAX, "gRPC code: Unavailable");
        let result = futures_executor_block(LogExporter::export(&exporter, LogBatch::new(&[])));
        assert!(result.is_err());
        assert_eq!(exporter.inner.calls.load(Ordering::SeqCst), 1);
    }

    fn futures_executor_block<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(future)
    }
}
