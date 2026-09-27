//! Cancellation, as a question rather than a token.
//!
//! Acquisition is bounded work that a user can interrupt, and the same
//! question is asked in three places: the scheduler between items, a source
//! between chunks, and a retry loop between attempts. A trait keeps the
//! dependency direction right — the engine owns the answer, the callers ask —
//! and matches the existing `zup-transaction` and `zup-plan` seams rather than
//! introducing a second cancellation model.

/// Whether the caller has asked for the work to stop.
pub trait Cancellation: Send + Sync {
    /// Whether cancellation has been requested.
    fn is_cancelled(&self) -> bool;
}

/// A caller that never cancels.
#[derive(Debug, Default, Clone, Copy)]
pub struct NeverCancelled;

impl Cancellation for NeverCancelled {
    fn is_cancelled(&self) -> bool {
        false
    }
}

/// A cancellation flag any thread or task can raise.
#[derive(Debug, Default)]
pub struct CancelFlag(std::sync::atomic::AtomicBool);

impl CancelFlag {
    /// A flag that has not been raised.
    pub fn new() -> Self {
        Self::default()
    }

    /// Raise the flag.
    pub fn cancel(&self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Cancellation for CancelFlag {
    fn is_cancelled(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
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
    if cancellation.is_cancelled() {
        return false;
    }
    let step = std::time::Duration::from_millis(50);
    let mut remaining = duration;
    while remaining > step {
        tokio::time::sleep(step).await;
        if cancellation.is_cancelled() {
            return false;
        }
        remaining -= step;
    }
    if remaining > std::time::Duration::ZERO {
        tokio::time::sleep(remaining).await;
    }
    !cancellation.is_cancelled()
}
