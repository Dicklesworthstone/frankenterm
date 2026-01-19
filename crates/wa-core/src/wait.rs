//! Shared wait-for utilities (no fixed sleeps).
//!
//! Provides retry-with-backoff helpers for tests and control loops.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use tokio::time::{Duration, Instant, sleep};

/// Backoff configuration for wait loops.
#[derive(Debug, Clone)]
pub struct Backoff {
    /// Initial delay before the second poll.
    pub initial: Duration,
    /// Maximum delay between polls.
    pub max: Duration,
    /// Multiplicative factor for backoff growth.
    pub factor: u32,
    /// Optional max retry count (inclusive of the first attempt).
    pub max_retries: Option<usize>,
}

impl Backoff {
    /// Compute the next delay given the current delay.
    #[must_use]
    pub fn next_delay(&self, current: Duration) -> Duration {
        let next = current.saturating_mul(self.factor);
        if next > self.max { self.max } else { next }
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(25),
            max: Duration::from_millis(1000),
            factor: 2,
            max_retries: None,
        }
    }
}

/// Result of a predicate check in a wait loop.
#[derive(Debug, Clone)]
pub enum WaitFor<T> {
    /// Predicate satisfied.
    Ready(T),
    /// Predicate not yet satisfied.
    NotReady { last_observed: Option<String> },
}

impl<T> WaitFor<T> {
    /// Convenience constructor for Ready.
    #[must_use]
    pub fn ready(value: T) -> Self {
        Self::Ready(value)
    }

    /// Convenience constructor for NotReady.
    #[must_use]
    pub fn not_ready(last_observed: impl Into<Option<String>>) -> Self {
        Self::NotReady {
            last_observed: last_observed.into(),
        }
    }
}

/// A predicate used by `wait_for`.
pub trait WaitPredicate {
    /// Output type when the predicate is satisfied.
    type Output: Send;

    /// Human-readable description for timeout errors.
    fn describe(&self) -> String;

    /// Execute a single poll.
    fn check(&mut self) -> Pin<Box<dyn Future<Output = WaitFor<Self::Output>> + Send + 'static>>;
}

/// Helper to build a `WaitPredicate` from a description and closure.
pub struct WaitCondition<F> {
    description: String,
    check: F,
}

impl<F> WaitCondition<F> {
    /// Create a new condition with description.
    #[must_use]
    pub fn new(description: impl Into<String>, check: F) -> Self {
        Self {
            description: description.into(),
            check,
        }
    }
}

impl<F, Fut, T> WaitPredicate for WaitCondition<F>
where
    F: FnMut() -> Fut + Send,
    Fut: Future<Output = WaitFor<T>> + Send + 'static,
    T: Send + 'static,
{
    type Output = T;

    fn describe(&self) -> String {
        self.description.clone()
    }

    fn check(&mut self) -> Pin<Box<dyn Future<Output = WaitFor<Self::Output>> + Send + 'static>> {
        Box::pin((self.check)())
    }
}

/// Timeout error returned by wait helpers.
#[derive(Debug, Clone)]
pub struct WaitError {
    /// Condition that was expected to become true.
    pub expected: String,
    /// Most recent observed state.
    pub last_observed: Option<String>,
    /// Number of retries attempted (including first poll).
    pub retries: usize,
    /// Elapsed time while waiting.
    pub elapsed: Duration,
}

impl fmt::Display for WaitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let last = self.last_observed.as_deref().unwrap_or("<none>");
        write!(
            f,
            "timeout waiting for {} after {}ms (retries={}, last_observed={})",
            self.expected,
            self.elapsed.as_millis(),
            self.retries,
            last
        )
    }
}

impl std::error::Error for WaitError {}

/// Wait for a predicate to become true within a timeout using backoff.
pub async fn wait_for<P>(
    mut predicate: P,
    timeout: Duration,
    backoff: Backoff,
) -> Result<P::Output, WaitError>
where
    P: WaitPredicate + Send,
{
    let expected = predicate.describe();
    let start = Instant::now();
    let deadline = start + timeout;
    let mut retries = 0usize;
    let mut delay = backoff.initial;
    let mut last_observed = None;

    loop {
        retries = retries.saturating_add(1);
        match predicate.check().await {
            WaitFor::Ready(value) => return Ok(value),
            WaitFor::NotReady { last_observed: obs } => {
                if obs.is_some() {
                    last_observed = obs;
                }
            }
        }

        let now = Instant::now();
        let timeout_reached = now >= deadline;
        let retries_exhausted = backoff.max_retries.is_some_and(|max| retries >= max);
        if timeout_reached || retries_exhausted {
            return Err(WaitError {
                expected,
                last_observed,
                retries,
                elapsed: now.saturating_duration_since(start),
            });
        }

        let remaining = deadline.saturating_duration_since(now);
        let sleep_for = if delay > remaining { remaining } else { delay };
        if !sleep_for.is_zero() {
            sleep(sleep_for).await;
        }
        delay = backoff.next_delay(delay);
    }
}

/// Wait for a query to return the expected value within a timeout.
pub async fn wait_for_value<F, Fut, T>(
    mut query: F,
    expected: T,
    timeout: Duration,
) -> Result<T, WaitError>
where
    F: FnMut() -> Fut + Send,
    Fut: Future<Output = T> + Send + 'static,
    T: PartialEq + fmt::Debug + Clone + Send + 'static,
{
    let expected_desc = format!("value == {expected:?}");
    let condition = WaitCondition::new(expected_desc, move || {
        let fut = query();
        let expected = expected.clone();
        async move {
            let observed = fut.await;
            if observed == expected {
                WaitFor::Ready(observed)
            } else {
                WaitFor::NotReady {
                    last_observed: Some(format!("{observed:?}")),
                }
            }
        }
    });
    wait_for(condition, timeout, Backoff::default()).await
}

/// Signals used to determine quiescence.
pub trait QuiescenceSignals {
    /// Returns true if the system is currently quiet.
    fn is_quiet(&self, now: Instant) -> bool;
    /// Human-readable description of the current state.
    fn describe(&self, now: Instant) -> String;
}

/// Snapshot of quiescence state for simple implementations.
#[derive(Debug, Clone)]
pub struct QuiescenceState {
    /// Pending work items.
    pub pending: usize,
    /// Last activity timestamp, if any.
    pub last_activity: Option<Instant>,
    /// Required quiet window duration.
    pub quiet_window: Duration,
}

impl QuiescenceState {
    #[must_use]
    fn is_quiet_at(&self, now: Instant) -> bool {
        if self.pending > 0 {
            return false;
        }
        self.last_activity
            .is_none_or(|last| now.saturating_duration_since(last) >= self.quiet_window)
    }

    #[must_use]
    fn describe_at(&self, now: Instant) -> String {
        let since_ms = self
            .last_activity
            .map_or(0, |last| now.saturating_duration_since(last).as_millis());
        format!(
            "pending={}, quiet_window_ms={}, since_last_ms={}",
            self.pending,
            self.quiet_window.as_millis(),
            since_ms
        )
    }
}

impl QuiescenceSignals for QuiescenceState {
    fn is_quiet(&self, now: Instant) -> bool {
        self.is_quiet_at(now)
    }

    fn describe(&self, now: Instant) -> String {
        self.describe_at(now)
    }
}

/// Wait for quiescence using default backoff.
pub async fn wait_for_quiescence<S>(signals: S, timeout: Duration) -> Result<(), WaitError>
where
    S: QuiescenceSignals + Clone + Send + 'static,
{
    wait_for_quiescence_with_backoff(signals, timeout, Backoff::default()).await
}

/// Wait for quiescence using a custom backoff.
pub async fn wait_for_quiescence_with_backoff<S>(
    signals: S,
    timeout: Duration,
    backoff: Backoff,
) -> Result<(), WaitError>
where
    S: QuiescenceSignals + Clone + Send + 'static,
{
    let condition = WaitCondition::new("quiescence", move || {
        let now = Instant::now();
        let signals = signals.clone();
        async move {
            if signals.is_quiet(now) {
                WaitFor::Ready(())
            } else {
                WaitFor::NotReady {
                    last_observed: Some(signals.describe(now)),
                }
            }
        }
    });
    wait_for(condition, timeout, backoff).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_schedule_increases_and_caps() {
        let backoff = Backoff {
            initial: Duration::from_millis(10),
            max: Duration::from_millis(70),
            factor: 2,
            max_retries: None,
        };

        let mut delay = backoff.initial;
        assert_eq!(delay, Duration::from_millis(10));
        delay = backoff.next_delay(delay);
        assert_eq!(delay, Duration::from_millis(20));
        delay = backoff.next_delay(delay);
        assert_eq!(delay, Duration::from_millis(40));
        delay = backoff.next_delay(delay);
        assert_eq!(delay, Duration::from_millis(70));
        delay = backoff.next_delay(delay);
        assert_eq!(delay, Duration::from_millis(70));
    }

    #[tokio::test]
    async fn wait_for_value_timeout_includes_debug_info() {
        let result = wait_for_value(|| async { 1u32 }, 2u32, Duration::from_millis(0)).await;
        let err = result.expect_err("should timeout");
        assert!(err.expected.contains("value == 2"));
        assert_eq!(err.last_observed.as_deref(), Some("1"));
        assert_eq!(err.retries, 1);
    }

    #[tokio::test]
    async fn wait_for_quiescence_succeeds_when_quiet() {
        let signals = QuiescenceState {
            pending: 0,
            last_activity: None,
            quiet_window: Duration::from_millis(0),
        };

        let result = wait_for_quiescence(signals, Duration::from_millis(0)).await;
        assert!(result.is_ok());
    }
}
