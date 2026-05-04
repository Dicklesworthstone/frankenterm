//! Shared epoch-clock-anomaly observability surface (br-ft-0n4nx).
//!
//! Multiple call sites compute "current ms since UNIX_EPOCH" via
//! `SystemTime::now().duration_since(UNIX_EPOCH)`. The naive
//! `unwrap_or_default()` / `.ok().unwrap_or(0)` pattern silently
//! produces a zero timestamp when the system clock is BEFORE
//! 1970-01-01 (NTP fail / container restart with un-synced clock /
//! hostile CI fixture). Across forensic, persistence, and identity
//! generation paths this is catastrophic — operators see a flat
//! line at epoch 0 with no diagnostic, persisted checkpoints
//! collide on `checkpoint_at = 0`, and watcher-client identifiers
//! repeat as `cl-0-0001` / `cl-0-0002`.
//!
//! ft-bn6qi shipped the per-module pattern for `web/sse.rs`;
//! ft-crpvd extended it to `recording.rs` + `wire_protocol.rs`.
//! This module is the cross-module successor: a single shared
//! counter so a forensic dashboard can scrape one number to detect
//! anomalies anywhere in the codebase, plus a typed helper that
//! every call site can route through.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Cumulative count of `SystemTime::now() < UNIX_EPOCH` events
/// across every call site that routes through [`epoch_ms_u64`] or
/// [`epoch_ms_i64`]. A single observability scrape detects clock
/// anomalies anywhere in the recorder / persistence / identity
/// surfaces.
///
/// Note: per-module counters in `web/sse.rs`, `recording.rs`, and
/// `wire_protocol.rs` (shipped under ft-bn6qi / ft-crpvd) remain
/// in place for site-specific dashboards. This counter is the
/// fleet-wide aggregator.
static CLOCK_ANOMALY_COUNT: AtomicU64 = AtomicU64::new(0);

/// Cumulative count of clock-before-1970 anomalies observed across
/// every call site routing through [`epoch_ms_u64`] / [`epoch_ms_i64`].
#[must_use]
pub fn clock_anomaly_count() -> u64 {
    CLOCK_ANOMALY_COUNT.load(Ordering::Relaxed)
}

/// Test helper: reset the shared counter so regression tests can
/// assert post-bump values without state leakage between tests.
#[cfg(test)]
pub fn reset_clock_anomaly_count_for_test() {
    CLOCK_ANOMALY_COUNT.store(0, Ordering::Relaxed);
}

/// Returns current epoch ms as `u64`. On clock-before-epoch error,
/// bumps [`CLOCK_ANOMALY_COUNT`], emits a structured `tracing::warn`
/// with `target = source` (caller-supplied for site-level
/// attribution) and the `br-ft-0n4nx` breadcrumb, and returns 0 so
/// the scalar contract stays the same.
#[must_use]
pub fn epoch_ms_u64(source: &'static str) -> u64 {
    epoch_ms_u64_from(SystemTime::now(), source)
}

/// Testable inner helper: takes a `SystemTime` so a regression
/// test can inject `UNIX_EPOCH - 100s` (a valid pre-epoch
/// SystemTime) without depending on the host clock.
pub(crate) fn epoch_ms_u64_from(now: SystemTime, source: &'static str) -> u64 {
    match now.duration_since(UNIX_EPOCH) {
        Ok(ts) => ts.as_millis() as u64,
        Err(err) => {
            CLOCK_ANOMALY_COUNT.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                target: "ft.clock_anomaly",
                event = "clock_anomaly",
                source = source,
                pre_epoch_secs = err.duration().as_secs(),
                "system clock is before UNIX_EPOCH; falling back to 0 (br-ft-0n4nx)"
            );
            0
        }
    }
}

/// Returns current epoch ms as `i64`. Saturates to `i64::MAX` if
/// the millisecond count overflows `i64`. On clock-before-epoch
/// error, bumps the shared counter, emits `tracing::warn`, and
/// returns 0.
#[must_use]
pub fn epoch_ms_i64(source: &'static str) -> i64 {
    epoch_ms_i64_from(SystemTime::now(), source)
}

/// Testable inner helper for the i64 variant.
pub(crate) fn epoch_ms_i64_from(now: SystemTime, source: &'static str) -> i64 {
    match now.duration_since(UNIX_EPOCH) {
        Ok(ts) => i64::try_from(ts.as_millis()).unwrap_or(i64::MAX),
        Err(err) => {
            CLOCK_ANOMALY_COUNT.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                target: "ft.clock_anomaly",
                event = "clock_anomaly",
                source = source,
                pre_epoch_secs = err.duration().as_secs(),
                "system clock is before UNIX_EPOCH; falling back to 0 (br-ft-0n4nx)"
            );
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn epoch_ms_u64_post_epoch_no_anomaly_ft_0n4nx() {
        reset_clock_anomaly_count_for_test();
        let post = UNIX_EPOCH + Duration::from_secs(1_704_067_200);
        let ms = epoch_ms_u64_from(post, "ft.test.clock");
        assert!(ms > 0);
        assert_eq!(clock_anomaly_count(), 0);
    }

    #[test]
    fn epoch_ms_u64_pre_epoch_bumps_counter_ft_0n4nx() {
        reset_clock_anomaly_count_for_test();
        let pre = UNIX_EPOCH - Duration::from_secs(100);
        let ms = epoch_ms_u64_from(pre, "ft.test.clock");
        assert_eq!(ms, 0);
        assert_eq!(clock_anomaly_count(), 1);
    }

    #[test]
    fn epoch_ms_i64_pre_epoch_returns_zero_ft_0n4nx() {
        reset_clock_anomaly_count_for_test();
        let pre = UNIX_EPOCH - Duration::from_secs(50);
        let ms = epoch_ms_i64_from(pre, "ft.test.clock");
        assert_eq!(ms, 0);
        assert_eq!(clock_anomaly_count(), 1);
    }

    #[test]
    fn epoch_ms_i64_post_epoch_returns_positive_ft_0n4nx() {
        reset_clock_anomaly_count_for_test();
        let post = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let ms = epoch_ms_i64_from(post, "ft.test.clock");
        assert!(ms > 0);
        assert_eq!(clock_anomaly_count(), 0);
    }

    #[test]
    fn shared_counter_aggregates_across_callers_ft_0n4nx() {
        // Multiple distinct sources bump the same shared counter
        // so a single observability scrape covers all sites.
        reset_clock_anomaly_count_for_test();
        let pre = UNIX_EPOCH - Duration::from_secs(10);
        let _ = epoch_ms_u64_from(pre, "ft.recording.clock");
        let _ = epoch_ms_u64_from(pre, "ft.web.clock");
        let _ = epoch_ms_i64_from(pre, "ft.runtime.clock");
        assert_eq!(clock_anomaly_count(), 3);
    }
}
