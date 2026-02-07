//! Shared wait-for utilities (no fixed sleeps).
//!
//! Provides retry-with-backoff helpers for tests and control loops.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

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
            max: Duration::from_secs(1),
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

// ---------------------------------------------------------------------------
// Quiescence detector: composable, live-signal integration
// ---------------------------------------------------------------------------

/// Atomic gauge that tracks pending work items (queue depth).
///
/// Increment on enqueue, decrement on dequeue. The detector reads the gauge
/// to determine whether queues have drained.
#[derive(Debug)]
pub struct QueueDepthGauge {
    name: String,
    count: AtomicUsize,
}

impl QueueDepthGauge {
    /// Create a new gauge with the given name and initial depth of zero.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            count: AtomicUsize::new(0),
        }
    }

    /// Increment pending count (call on enqueue).
    pub fn increment(&self) {
        self.count.fetch_add(1, Ordering::SeqCst);
    }

    /// Decrement pending count (call on dequeue). Saturates at zero.
    pub fn decrement(&self) {
        // fetch_sub with SeqCst; saturate to prevent underflow
        let prev = self.count.fetch_sub(1, Ordering::SeqCst);
        if prev == 0 {
            // Underflow occurred; restore to 0
            self.count.store(0, Ordering::SeqCst);
        }
    }

    /// Current depth.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }

    /// Gauge name (for diagnostics).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Tracks the most recent activity using a monotonic clock.
///
/// Records are lock-free via `AtomicU64` storing nanosecond offsets from an
/// epoch [`Instant`] captured at construction time.
#[derive(Debug)]
pub struct ActivityTracker {
    /// Monotonic reference point.
    epoch: Instant,
    /// Nanosecond offset from `epoch` of the last recorded activity.
    /// Zero means no activity has been recorded.
    last_offset_nanos: AtomicU64,
}

impl ActivityTracker {
    /// Create a new tracker with the current instant as epoch.
    #[must_use]
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
            last_offset_nanos: AtomicU64::new(0),
        }
    }

    /// Record an activity at the current instant.
    pub fn record(&self) {
        let offset = Instant::now()
            .saturating_duration_since(self.epoch)
            .as_nanos() as u64;
        let encoded = offset.max(1);
        // Monotonic: never go backwards.
        self.last_offset_nanos.fetch_max(encoded, Ordering::SeqCst);
    }

    /// Returns the instant of the last recorded activity, or `None` if
    /// no activity has been recorded.
    #[must_use]
    pub fn last_activity(&self) -> Option<Instant> {
        let offset = self.last_offset_nanos.load(Ordering::SeqCst);
        if offset == 0 {
            None
        } else {
            Some(self.epoch + Duration::from_nanos(offset - 1))
        }
    }

    /// Returns true if no activity has ever been recorded.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.last_offset_nanos.load(Ordering::SeqCst) == 0
    }
}

impl Default for ActivityTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Composable quiescence detector that aggregates live signal sources.
///
/// A system is quiescent when:
/// 1. All queue depth gauges read zero (queues drained).
/// 2. No activity has occurred within the configured quiet window.
///
/// The detector is cheaply cloneable (all fields are `Arc`).
#[derive(Clone)]
pub struct QuiescenceDetector {
    gauges: Vec<Arc<QueueDepthGauge>>,
    activity: Arc<ActivityTracker>,
    quiet_window: Duration,
}

impl QuiescenceDetector {
    /// Create a detector with the given quiet window and a default activity tracker.
    #[must_use]
    pub fn new(quiet_window: Duration) -> Self {
        Self {
            gauges: Vec::new(),
            activity: Arc::new(ActivityTracker::new()),
            quiet_window,
        }
    }

    /// Create a detector sharing an existing activity tracker.
    #[must_use]
    pub fn with_activity(mut self, activity: Arc<ActivityTracker>) -> Self {
        self.activity = activity;
        self
    }

    /// Add a queue depth gauge to monitor.
    pub fn add_gauge(&mut self, gauge: Arc<QueueDepthGauge>) {
        self.gauges.push(gauge);
    }

    /// Builder-style: add a gauge and return self.
    #[must_use]
    pub fn with_gauge(mut self, gauge: Arc<QueueDepthGauge>) -> Self {
        self.gauges.push(gauge);
        self
    }

    /// Total pending work across all gauges.
    #[must_use]
    pub fn total_pending(&self) -> usize {
        self.gauges.iter().map(|g| g.depth()).sum()
    }

    /// Take a diagnostic snapshot of the current state.
    #[must_use]
    pub fn snapshot(&self) -> QuiescenceSnapshot {
        let gauge_values: Vec<(String, usize)> = self
            .gauges
            .iter()
            .map(|g| (g.name().to_owned(), g.depth()))
            .collect();
        let total_pending: usize = gauge_values.iter().map(|(_, d)| d).sum();
        QuiescenceSnapshot {
            total_pending,
            gauges: gauge_values,
            last_activity: self.activity.last_activity(),
            quiet_window: self.quiet_window,
        }
    }
}

impl QuiescenceSignals for QuiescenceDetector {
    fn is_quiet(&self, now: Instant) -> bool {
        if self.total_pending() > 0 {
            return false;
        }
        match self.activity.last_activity() {
            None => true,
            Some(last) => now.saturating_duration_since(last) >= self.quiet_window,
        }
    }

    fn describe(&self, now: Instant) -> String {
        let snap = self.snapshot();
        snap.describe(now)
    }
}

impl fmt::Debug for QuiescenceDetector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QuiescenceDetector")
            .field("gauges", &self.gauges.len())
            .field("quiet_window", &self.quiet_window)
            .finish()
    }
}

/// Diagnostic snapshot of quiescence state.
///
/// Captures per-gauge depths, total pending, last activity, and the quiet
/// window at a single point in time. Useful for structured logging and
/// timeout error messages.
#[derive(Debug, Clone)]
pub struct QuiescenceSnapshot {
    /// Total pending across all gauges.
    pub total_pending: usize,
    /// Per-gauge (name, depth) pairs.
    pub gauges: Vec<(String, usize)>,
    /// Last activity instant (monotonic).
    pub last_activity: Option<Instant>,
    /// Required quiet window.
    pub quiet_window: Duration,
}

impl QuiescenceSnapshot {
    /// Human-readable description of this snapshot relative to `now`.
    #[must_use]
    pub fn describe(&self, now: Instant) -> String {
        let since_ms = self
            .last_activity
            .map_or(0, |last| now.saturating_duration_since(last).as_millis());
        let gauge_summary: String = if self.gauges.is_empty() {
            "no gauges".to_owned()
        } else {
            self.gauges
                .iter()
                .map(|(name, depth)| format!("{name}={depth}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        format!(
            "total_pending={}, gauges=[{}], quiet_window_ms={}, since_last_ms={}",
            self.total_pending,
            gauge_summary,
            self.quiet_window.as_millis(),
            since_ms
        )
    }
}

impl QuiescenceSignals for QuiescenceSnapshot {
    fn is_quiet(&self, now: Instant) -> bool {
        if self.total_pending > 0 {
            return false;
        }
        match self.last_activity {
            None => true,
            Some(last) => now.saturating_duration_since(last) >= self.quiet_window,
        }
    }

    fn describe(&self, now: Instant) -> String {
        self.describe(now)
    }
}

/// Wait for a boolean condition to become true.
///
/// Simpler alternative to [`wait_for`] for cases where you just need
/// a bool predicate without structured `WaitFor` returns.
pub async fn wait_for_condition<F, Fut>(
    description: impl Into<String>,
    mut check: F,
    timeout: Duration,
) -> Result<(), WaitError>
where
    F: FnMut() -> Fut + Send,
    Fut: Future<Output = bool> + Send + 'static,
{
    let desc = description.into();
    let condition = WaitCondition::new(desc, move || {
        let fut = check();
        async move {
            if fut.await {
                WaitFor::Ready(())
            } else {
                WaitFor::not_ready(None::<String>)
            }
        }
    });
    wait_for(condition, timeout, Backoff::default()).await
}

/// Wait for a boolean condition with custom backoff.
pub async fn wait_for_condition_with_backoff<F, Fut>(
    description: impl Into<String>,
    mut check: F,
    timeout: Duration,
    backoff: Backoff,
) -> Result<(), WaitError>
where
    F: FnMut() -> Fut + Send,
    Fut: Future<Output = bool> + Send + 'static,
{
    let desc = description.into();
    let condition = WaitCondition::new(desc, move || {
        let fut = check();
        async move {
            if fut.await {
                WaitFor::Ready(())
            } else {
                WaitFor::not_ready(None::<String>)
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

    #[test]
    fn backoff_schedule_factor_three() {
        let backoff = Backoff {
            initial: Duration::from_millis(5),
            max: Duration::from_millis(100),
            factor: 3,
            max_retries: None,
        };

        let mut delay = backoff.initial;
        assert_eq!(delay, Duration::from_millis(5));
        delay = backoff.next_delay(delay);
        assert_eq!(delay, Duration::from_millis(15));
        delay = backoff.next_delay(delay);
        assert_eq!(delay, Duration::from_millis(45));
        delay = backoff.next_delay(delay);
        // 45 * 3 = 135, capped at 100
        assert_eq!(delay, Duration::from_millis(100));
    }

    #[test]
    fn backoff_default_is_sane() {
        let b = Backoff::default();
        assert_eq!(b.initial, Duration::from_millis(25));
        assert_eq!(b.max, Duration::from_secs(1));
        assert_eq!(b.factor, 2);
        assert!(b.max_retries.is_none());
    }

    #[test]
    fn wait_error_display_includes_all_fields() {
        let err = WaitError {
            expected: "database ready".to_string(),
            last_observed: Some("connecting".to_string()),
            retries: 5,
            elapsed: Duration::from_millis(3200),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("database ready"),
            "should mention expected condition"
        );
        assert!(msg.contains("3200"), "should mention elapsed ms");
        assert!(msg.contains("retries=5"), "should mention retry count");
        assert!(
            msg.contains("connecting"),
            "should mention last observed state"
        );
    }

    #[test]
    fn wait_error_display_handles_none_observed() {
        let err = WaitError {
            expected: "ready".to_string(),
            last_observed: None,
            retries: 1,
            elapsed: Duration::from_millis(100),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("<none>"),
            "should show <none> for missing observation"
        );
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
    async fn wait_for_value_succeeds_immediately() {
        let result = wait_for_value(|| async { 42u32 }, 42u32, Duration::from_secs(1)).await;
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn wait_for_predicate_succeeds_after_n_attempts() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter2 = counter.clone();

        let condition = WaitCondition::new("counter reaches 3", move || {
            let n = counter2.fetch_add(1, Ordering::SeqCst);
            async move {
                if n >= 2 {
                    WaitFor::Ready(n)
                } else {
                    WaitFor::not_ready(Some(format!("count={n}")))
                }
            }
        });

        let backoff = Backoff {
            initial: Duration::from_millis(1),
            max: Duration::from_millis(5),
            factor: 2,
            max_retries: None,
        };

        let result = wait_for(condition, Duration::from_secs(1), backoff).await;
        let value = result.unwrap();
        assert!(value >= 2, "should have succeeded after attempts");
    }

    #[tokio::test]
    async fn wait_for_max_retries_exhausted() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter2 = counter.clone();

        let condition = WaitCondition::new("never ready", move || {
            let n = counter2.fetch_add(1, Ordering::SeqCst);
            async move { WaitFor::<()>::not_ready(Some(format!("attempt={n}"))) }
        });

        let backoff = Backoff {
            initial: Duration::from_millis(1),
            max: Duration::from_millis(5),
            factor: 2,
            max_retries: Some(3),
        };

        let result = wait_for(condition, Duration::from_secs(10), backoff).await;
        let err = result.expect_err("should exhaust retries");
        assert!(err.retries <= 3, "retries={} should be <= 3", err.retries);
        assert!(err.expected.contains("never ready"));
        assert!(err.last_observed.is_some());
    }

    #[tokio::test]
    async fn wait_for_condition_succeeds() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter2 = counter.clone();

        let result = wait_for_condition(
            "counter reaches 2",
            move || {
                let n = counter2.fetch_add(1, Ordering::SeqCst);
                async move { n >= 1 }
            },
            Duration::from_secs(1),
        )
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn wait_for_condition_times_out() {
        let result =
            wait_for_condition("impossible", || async { false }, Duration::from_millis(10)).await;
        let err = result.expect_err("should timeout");
        assert!(err.expected.contains("impossible"));
        assert!(err.retries >= 1);
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

    #[tokio::test]
    async fn wait_for_quiescence_timeout_with_pending_work() {
        let signals = QuiescenceState {
            pending: 5,
            last_activity: Some(Instant::now()),
            quiet_window: Duration::from_millis(100),
        };

        let result = wait_for_quiescence(signals, Duration::from_millis(10)).await;
        let err = result.expect_err("should timeout with pending work");
        assert!(err.expected.contains("quiescence"));
        assert!(err.last_observed.is_some());
        let obs = err.last_observed.unwrap();
        assert!(
            obs.contains("pending=5"),
            "should report pending count: {obs}"
        );
    }

    #[test]
    fn quiescence_state_quiet_with_no_activity() {
        let state = QuiescenceState {
            pending: 0,
            last_activity: None,
            quiet_window: Duration::from_secs(10),
        };
        assert!(state.is_quiet_at(Instant::now()));
    }

    #[test]
    fn quiescence_state_not_quiet_with_pending() {
        let state = QuiescenceState {
            pending: 1,
            last_activity: None,
            quiet_window: Duration::from_millis(0),
        };
        assert!(!state.is_quiet_at(Instant::now()));
    }

    #[test]
    fn quiescence_state_not_quiet_within_window() {
        let state = QuiescenceState {
            pending: 0,
            last_activity: Some(Instant::now()),
            quiet_window: Duration::from_secs(60),
        };
        // Recent activity + 60s window = not quiet yet
        assert!(!state.is_quiet_at(Instant::now()));
    }

    #[test]
    fn quiescence_describe_includes_fields() {
        let state = QuiescenceState {
            pending: 3,
            last_activity: Some(Instant::now()),
            quiet_window: Duration::from_millis(500),
        };
        let desc = state.describe_at(Instant::now());
        assert!(desc.contains("pending=3"));
        assert!(desc.contains("quiet_window_ms=500"));
    }

    #[test]
    fn wait_for_enum_constructors() {
        let ready: WaitFor<i32> = WaitFor::ready(42);
        assert!(matches!(ready, WaitFor::Ready(42)));

        let not_ready: WaitFor<i32> = WaitFor::not_ready(Some("waiting".to_string()));
        assert!(matches!(
            not_ready,
            WaitFor::NotReady {
                last_observed: Some(_)
            }
        ));

        let not_ready_none: WaitFor<i32> = WaitFor::not_ready(None::<String>);
        assert!(matches!(
            not_ready_none,
            WaitFor::NotReady {
                last_observed: None
            }
        ));
    }

    // =========================================================================
    // Integration tests: synthetic producer/consumer quiescence
    // =========================================================================

    #[tokio::test]
    async fn quiescence_producer_consumer_eventually_quiet() {
        // Simulate a producer/consumer where the consumer drains work
        // and quiescence is achieved once the queue is empty and quiet window elapses.
        let pending = Arc::new(AtomicUsize::new(5));
        let pending2 = pending.clone();

        // Spawn a "consumer" task that drains one item every 5ms
        let consumer = tokio::spawn(async move {
            loop {
                let current = pending2.load(Ordering::SeqCst);
                if current == 0 {
                    break;
                }
                pending2.fetch_sub(1, Ordering::SeqCst);
                sleep(Duration::from_millis(5)).await;
            }
        });

        // Wait for quiescence: pending must reach 0
        let pending_check = pending.clone();
        let condition = WaitCondition::new("queue drained", move || {
            let count = pending_check.load(Ordering::SeqCst);
            async move {
                if count == 0 {
                    WaitFor::Ready(())
                } else {
                    WaitFor::not_ready(Some(format!("pending={count}")))
                }
            }
        });

        let backoff = Backoff {
            initial: Duration::from_millis(2),
            max: Duration::from_millis(20),
            factor: 2,
            max_retries: None,
        };

        let result = wait_for(condition, Duration::from_secs(2), backoff).await;
        assert!(
            result.is_ok(),
            "should achieve quiescence after consumer drains"
        );

        consumer.await.unwrap();
        assert_eq!(pending.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn quiescence_with_shared_state_and_quiet_window() {
        // Test QuiescenceState with a shared atomic that simulates
        // work draining and then a quiet window.
        use std::sync::Mutex;

        let shared = Arc::new(Mutex::new(QuiescenceState {
            pending: 3,
            last_activity: Some(Instant::now()),
            quiet_window: Duration::from_millis(10),
        }));
        let shared2 = shared.clone();

        // Spawn a task that reduces pending and then goes quiet
        let worker = tokio::spawn(async move {
            for _ in 0..3 {
                sleep(Duration::from_millis(3)).await;
                let mut s = shared2.lock().unwrap();
                s.pending = s.pending.saturating_sub(1);
                s.last_activity = Some(Instant::now());
            }
        });

        // Poll the shared state for quiescence
        let shared_check = shared.clone();
        let condition = WaitCondition::new("shared state quiescent", move || {
            let state = shared_check.lock().unwrap().clone();
            let now = Instant::now();
            async move {
                if state.is_quiet_at(now) {
                    WaitFor::Ready(())
                } else {
                    WaitFor::not_ready(Some(state.describe_at(now)))
                }
            }
        });

        let backoff = Backoff {
            initial: Duration::from_millis(2),
            max: Duration::from_millis(20),
            factor: 2,
            max_retries: None,
        };

        let result = wait_for(condition, Duration::from_secs(2), backoff).await;
        assert!(result.is_ok(), "should detect quiescence after work drains");

        worker.await.unwrap();
    }

    #[tokio::test]
    async fn wait_for_condition_on_changing_source() {
        // Simulate a monotonically increasing counter and wait for it to
        // reach a threshold. Uses wait_for_condition (>= check) rather than
        // wait_for_value (exact equality) because the counter may overshoot
        // between polls.
        let counter = Arc::new(AtomicUsize::new(0));
        let counter2 = counter.clone();

        // Spawn an incrementer that ticks every 3ms up to 10
        let incrementer = tokio::spawn(async move {
            for _ in 0..10 {
                sleep(Duration::from_millis(3)).await;
                counter2.fetch_add(1, Ordering::SeqCst);
            }
        });

        let counter_read = counter.clone();
        let result = wait_for_condition(
            "counter >= 7",
            move || {
                let val = counter_read.load(Ordering::SeqCst);
                async move { val >= 7 }
            },
            Duration::from_secs(2),
        )
        .await;

        assert!(result.is_ok(), "counter should reach >= 7 within timeout");
        let final_val = counter.load(Ordering::SeqCst);
        assert!(
            final_val >= 7,
            "final counter value {final_val} should be >= 7"
        );
        incrementer.await.unwrap();
    }

    // =========================================================================
    // QueueDepthGauge tests
    // =========================================================================

    #[test]
    fn gauge_starts_at_zero() {
        let g = QueueDepthGauge::new("test");
        assert_eq!(g.depth(), 0);
        assert_eq!(g.name(), "test");
    }

    #[test]
    fn gauge_increment_decrement() {
        let g = QueueDepthGauge::new("q");
        g.increment();
        g.increment();
        g.increment();
        assert_eq!(g.depth(), 3);
        g.decrement();
        assert_eq!(g.depth(), 2);
        g.decrement();
        g.decrement();
        assert_eq!(g.depth(), 0);
    }

    #[test]
    fn gauge_decrement_saturates_at_zero() {
        let g = QueueDepthGauge::new("q");
        g.decrement(); // should not underflow
        assert_eq!(g.depth(), 0);
        g.decrement();
        assert_eq!(g.depth(), 0);
    }

    // =========================================================================
    // ActivityTracker tests
    // =========================================================================

    #[test]
    fn tracker_idle_initially() {
        let t = ActivityTracker::new();
        assert!(t.is_idle());
        assert!(t.last_activity().is_none());
    }

    #[test]
    fn tracker_last_activity_decodes_encoded_offset() {
        let t = ActivityTracker::new();
        t.last_offset_nanos.store(1, Ordering::SeqCst); // encoded zero offset
        assert_eq!(t.last_activity(), Some(t.epoch));
        assert!(!t.is_idle());
    }

    #[test]
    fn tracker_records_activity() {
        let t = ActivityTracker::new();
        t.record();
        assert!(!t.is_idle());
        assert!(t.last_activity().is_some());
    }

    #[tokio::test]
    async fn tracker_activity_monotonic() {
        let t = ActivityTracker::new();
        t.record();
        let first = t.last_activity().unwrap();
        sleep(Duration::from_millis(5)).await;
        t.record();
        let second = t.last_activity().unwrap();
        assert!(second >= first, "activity timestamps should be monotonic");
    }

    // =========================================================================
    // QuiescenceDetector tests
    // =========================================================================

    #[test]
    fn detector_quiet_with_no_gauges_no_activity() {
        let d = QuiescenceDetector::new(Duration::from_millis(10));
        assert!(d.is_quiet(Instant::now()));
        assert_eq!(d.total_pending(), 0);
    }

    #[test]
    fn detector_not_quiet_with_pending_work() {
        let g = Arc::new(QueueDepthGauge::new("ingest"));
        g.increment();
        let d = QuiescenceDetector::new(Duration::from_millis(0)).with_gauge(g);
        assert!(!d.is_quiet(Instant::now()));
        assert_eq!(d.total_pending(), 1);
    }

    #[test]
    fn detector_not_quiet_within_window() {
        let activity = Arc::new(ActivityTracker::new());
        activity.record();
        let d = QuiescenceDetector::new(Duration::from_secs(60)).with_activity(activity);
        // Recent activity + 60s window → not quiet
        assert!(!d.is_quiet(Instant::now()));
    }

    #[test]
    fn detector_quiet_after_window_elapses() {
        let activity = Arc::new(ActivityTracker::new());
        // Set activity in the past by not recording anything — idle = quiet
        let d = QuiescenceDetector::new(Duration::from_millis(0)).with_activity(activity);
        assert!(d.is_quiet(Instant::now()));
    }

    #[test]
    fn detector_multiple_gauges_all_must_drain() {
        let g1 = Arc::new(QueueDepthGauge::new("capture"));
        let g2 = Arc::new(QueueDepthGauge::new("writer"));
        g1.increment();
        let d = QuiescenceDetector::new(Duration::from_millis(0))
            .with_gauge(g1.clone())
            .with_gauge(g2);
        assert!(!d.is_quiet(Instant::now()), "g1 still pending");
        assert_eq!(d.total_pending(), 1);
        g1.decrement();
        assert!(d.is_quiet(Instant::now()), "all gauges drained");
    }

    #[test]
    fn snapshot_describe_includes_gauge_names() {
        let g = Arc::new(QueueDepthGauge::new("ingest_q"));
        g.increment();
        g.increment();
        let d = QuiescenceDetector::new(Duration::from_millis(100)).with_gauge(g);
        let snap = d.snapshot();
        assert_eq!(snap.total_pending, 2);
        assert_eq!(snap.gauges.len(), 1);
        assert_eq!(snap.gauges[0], ("ingest_q".to_owned(), 2));
        let desc = snap.describe(Instant::now());
        assert!(desc.contains("ingest_q=2"), "desc: {desc}");
        assert!(desc.contains("total_pending=2"), "desc: {desc}");
    }

    #[test]
    fn snapshot_implements_quiescence_signals() {
        let snap = QuiescenceSnapshot {
            total_pending: 0,
            gauges: vec![],
            last_activity: None,
            quiet_window: Duration::from_millis(0),
        };
        assert!(snap.is_quiet(Instant::now()));
    }

    // =========================================================================
    // Integration: multi-queue quiescence detection via detector
    // =========================================================================

    #[tokio::test]
    async fn detector_integration_multi_queue_drain() {
        // Simulate two queues being drained by independent consumers.
        // The detector should report quiescence only after both drain.
        let capture_gauge = Arc::new(QueueDepthGauge::new("capture"));
        let writer_gauge = Arc::new(QueueDepthGauge::new("writer"));
        let activity = Arc::new(ActivityTracker::new());

        // Enqueue work
        for _ in 0..5 {
            capture_gauge.increment();
        }
        for _ in 0..3 {
            writer_gauge.increment();
        }
        activity.record();

        let detector = QuiescenceDetector::new(Duration::from_millis(10))
            .with_gauge(capture_gauge.clone())
            .with_gauge(writer_gauge.clone())
            .with_activity(activity.clone());

        assert!(!detector.is_quiet(Instant::now()));

        // Spawn consumers
        let cg = capture_gauge.clone();
        let act1 = activity.clone();
        let consumer1 = tokio::spawn(async move {
            for _ in 0..5 {
                sleep(Duration::from_millis(2)).await;
                cg.decrement();
                act1.record();
            }
        });

        let wg = writer_gauge.clone();
        let act2 = activity.clone();
        let consumer2 = tokio::spawn(async move {
            for _ in 0..3 {
                sleep(Duration::from_millis(3)).await;
                wg.decrement();
                act2.record();
            }
        });

        // Wait for quiescence using the detector with wait_for_quiescence
        let backoff = Backoff {
            initial: Duration::from_millis(2),
            max: Duration::from_millis(20),
            factor: 2,
            max_retries: None,
        };

        let result =
            wait_for_quiescence_with_backoff(detector.clone(), Duration::from_secs(2), backoff)
                .await;

        assert!(
            result.is_ok(),
            "detector should report quiescence after both queues drain"
        );
        assert_eq!(detector.total_pending(), 0);

        consumer1.await.unwrap();
        consumer2.await.unwrap();
    }

    #[tokio::test]
    async fn detector_not_quiet_while_one_queue_still_draining() {
        let fast_gauge = Arc::new(QueueDepthGauge::new("fast"));
        let slow_gauge = Arc::new(QueueDepthGauge::new("slow"));

        fast_gauge.increment();
        slow_gauge.increment();
        slow_gauge.increment();
        slow_gauge.increment();

        let detector = QuiescenceDetector::new(Duration::from_millis(0))
            .with_gauge(fast_gauge.clone())
            .with_gauge(slow_gauge.clone());

        // Drain the fast queue immediately
        fast_gauge.decrement();

        // Still not quiet because slow_gauge has 3 pending
        assert!(!detector.is_quiet(Instant::now()));
        assert_eq!(detector.total_pending(), 3);

        // Drain slow queue
        slow_gauge.decrement();
        slow_gauge.decrement();
        slow_gauge.decrement();

        assert!(detector.is_quiet(Instant::now()));
    }
}
