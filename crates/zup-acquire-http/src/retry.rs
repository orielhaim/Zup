//! Retry classification and bounded exponential backoff supplied by `backon`.

use std::time::Duration;

use backon::BackoffBuilder;

use crate::error::{HttpError, RetryDecision, is_retryable_status, retry_after};

/// The shape of the wait between attempts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackoffPolicy {
    /// The first wait.
    pub initial: Duration,
    /// The ceiling on any single wait.
    pub maximum: Duration,
    /// The multiplier between attempts.
    pub factor: u32,
    /// How many attempts one origin gets before the scheduler moves on.
    pub max_attempts: u32,
    /// The ceiling on the total time one blob may take, across origins.
    pub budget: Duration,
}

impl Default for BackoffPolicy {
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(250),
            maximum: Duration::from_secs(10),
            factor: 2,
            max_attempts: 4,
            budget: Duration::from_secs(10 * 60),
        }
    }
}

impl BackoffPolicy {
    /// A policy that never retries, for a caller that wants a fast refusal.
    pub const fn never() -> Self {
        Self {
            initial: Duration::ZERO,
            maximum: Duration::ZERO,
            factor: 2,
            max_attempts: 1,
            budget: Duration::ZERO,
        }
    }

    /// Whether this policy ever sends a request twice.
    pub const fn retries(&self) -> bool {
        self.max_attempts > 1
    }

    /// Refuse a policy that could not make progress.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.max_attempts == 0 {
            return Err("a retry policy needs at least one attempt");
        }
        if !self.retries() {
            // A deliberate no-retry policy has no curve to speak of.
            return Ok(());
        }
        if self.factor < 2 {
            return Err("a retry policy needs a factor of at least 2");
        }
        if self.maximum < self.initial {
            return Err("a retry policy's ceiling is below its first wait");
        }
        Ok(())
    }

    /// The wait before attempt `attempt`, counting from one.
    ///
    /// `backon` supplies the exponential curve and the jitter. The jitter is
    /// the point: N clients that back off by exactly the same amount retry in
    /// lockstep, which is how one origin's bad minute becomes every client's.
    /// The ceiling is applied to the curve, so a jittered wait never exceeds it.
    ///
    /// A `Retry-After` from the server outranks this entirely, and is applied by
    /// the caller rather than here.
    pub fn delay(&self, attempt: u32) -> Duration {
        if !self.retries() {
            return Duration::ZERO;
        }
        let mut curve = backon::ExponentialBuilder::default()
            .with_min_delay(self.initial)
            .with_max_delay(self.maximum)
            .with_factor(self.factor as f32)
            .with_jitter()
            .build();
        curve
            .nth(attempt.saturating_sub(1) as usize)
            .unwrap_or(self.maximum)
            .min(self.maximum)
    }

    /// Decide what to do about a failure.
    pub fn decide(
        &self,
        error: &HttpError,
        attempt: u32,
        elapsed: Duration,
        retry_after_header: Option<&str>,
    ) -> RetryDecision {
        if matches!(error, HttpError::Cancelled) {
            return RetryDecision::Stop;
        }
        if elapsed >= self.budget {
            return RetryDecision::Elsewhere;
        }
        if attempt >= self.max_attempts {
            return if error.is_tryable_elsewhere() {
                RetryDecision::Elsewhere
            } else {
                RetryDecision::Stop
            };
        }
        if let Some(status) = status_of(error)
            && !is_retryable_status(status)
        {
            return RetryDecision::Stop;
        }
        if !error.is_tryable_elsewhere() {
            return RetryDecision::Stop;
        }
        // A server that told us when to come back outranks our own curve.
        let wait = retry_after(retry_after_header)
            .unwrap_or_else(|| self.delay(attempt))
            .min(self.maximum.max(Duration::from_secs(1)));
        RetryDecision::Again(wait)
    }
}

fn status_of(error: &HttpError) -> Option<u16> {
    match error {
        HttpError::Status { status, .. } => Some(*status),
        _ => None,
    }
}

#[derive(Debug)]
pub(crate) struct RetryState {
    policy: BackoffPolicy,
    attempt: u32,
    started: std::time::Instant,
}

impl RetryState {
    pub(crate) fn new(policy: BackoffPolicy) -> Self {
        Self {
            policy,
            attempt: 0,
            started: std::time::Instant::now(),
        }
    }

    pub(crate) fn record(
        &mut self,
        error: &HttpError,
        retry_after_header: Option<&str>,
    ) -> RetryDecision {
        self.attempt += 1;
        self.policy.decide(
            error,
            self.attempt,
            self.started.elapsed(),
            retry_after_header,
        )
    }
}
