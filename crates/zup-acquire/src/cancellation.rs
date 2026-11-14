//! Cancellation queries and wakeable cancellation waits.
//!
//! `CancelFlag` wakes waiters directly. Query-only implementations remain valid
//! and use the default timer-based adapter; override `Cancellation::cancelled`
//! when the caller already owns a notification mechanism.

use std::future::Future;
use std::pin::Pin;

/// Whether the caller has asked for the work to stop.
pub trait Cancellation: Send + Sync {
    /// Whether cancellation has been requested.
    fn is_cancelled(&self) -> bool;

    /// Wait for cancellation. Override for notification-based cancellation.
    /// Query-only adapters are sampled every 50ms so existing implementers work.
    fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            while !self.is_cancelled() {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
    }
}

impl<F: Fn() -> bool + Send + Sync> Cancellation for F {
    fn is_cancelled(&self) -> bool {
        self()
    }
}

/// A caller that never cancels.
#[derive(Debug, Default, Clone, Copy)]
pub struct NeverCancelled;

impl Cancellation for NeverCancelled {
    fn is_cancelled(&self) -> bool {
        false
    }

    fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(std::future::pending())
    }
}

/// A cancellation flag any thread or task can raise.
#[derive(Debug, Default)]
pub struct CancelFlag(tokio_util::sync::CancellationToken);

impl CancelFlag {
    /// A flag that has not been raised.
    pub fn new() -> Self {
        Self::default()
    }

    /// Raise the flag and wake its waiters.
    pub fn cancel(&self) {
        self.0.cancel();
    }
}

impl Cancellation for CancelFlag {
    fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }

    fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(self.0.cancelled())
    }
}

/// Sleep for `duration`, or return early the moment the caller cancels.
///
/// A retry that ignored this would keep a cancelled installer waiting out its
/// backoff, which is the difference between a prompt cancel and one that appears
/// to hang.
pub async fn cancellable_sleep(
    duration: std::time::Duration,
    cancellation: &dyn Cancellation,
) -> bool {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => false,
        _ = tokio::time::sleep(duration) => !cancellation.is_cancelled(),
    }
}
