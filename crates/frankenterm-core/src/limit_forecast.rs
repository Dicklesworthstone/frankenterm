//! Deterministic fleet-capacity forecasting from the durable `limit_windows`
//! ledger (W7.2, ft-7h5da.8.2).
//!
//! The watcher already records *when* a pane/account becomes usable again via
//! [`crate::storage::LimitWindowRecord`]. This module turns that ledger into the
//! operator's real capacity number: how many panes are usable right now, and a
//! forward-looking timeline of when limited panes recover. It is a pure,
//! side-effect-free transform so the output can be pinned with golden tests and
//! served verbatim by `ft robot limits`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::storage::LimitWindowRecord;

/// A currently-active limit window with its effective reset deadline.
///
/// A single pane can appear more than once when it is limited across multiple
/// services/accounts; each underlying window is reported individually here so
/// the detail is not lost in the per-pane rollup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveLimitWindow {
    /// Pane the limit was detected on.
    pub pane_id: u64,
    /// Service/provider namespace for the account key.
    pub service: String,
    /// Stable account key (`unknown` when the detector lacked account context).
    pub account_id: String,
    /// Effective reset deadline (epoch ms): parsed reset or conservative TTL.
    pub effective_reset_at_ms: i64,
    /// How the deadline was derived: `absolute`, `retry_after`, or `unknown_ttl`.
    pub reset_source: String,
    /// `false` when the deadline is a conservative fallback (`unknown_ttl`).
    pub reset_known: bool,
}

/// One step on the recovery timeline: at `at_ms`, `usable_panes` of
/// `total_panes` are expected to be free of any active limit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitForecastPoint {
    /// Epoch ms at which the panes in `recovering_panes` become usable.
    pub at_ms: i64,
    /// Cumulative usable-pane count once this point's windows have expired.
    pub usable_panes: u32,
    /// Fleet panes still limited immediately after this point.
    pub limited_panes: u32,
    /// Pane ids whose final active window expires at this point (ascending).
    pub recovering_panes: Vec<u64>,
}

/// Current limit status plus a deterministic forward capacity timeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitsForecast {
    /// Reference time the forecast was computed against (epoch ms).
    pub now_ms: i64,
    /// Size of the live fleet the forecast was scoped to.
    pub total_panes: u32,
    /// Panes usable right now (no active limit window).
    pub usable_now: u32,
    /// Panes limited right now (at least one active limit window).
    pub limited_now: u32,
    /// Distinct future recovery points, ascending by time.
    pub timeline: Vec<LimitForecastPoint>,
    /// Every currently-active window, sorted deterministically by
    /// (reset, pane, service, account).
    pub active: Vec<ActiveLimitWindow>,
}

/// Build a deterministic capacity forecast from the limit-window ledger.
///
/// `total_panes` is the size of the live fleet. `windows` is the set of durable
/// limit windows to consider (typically
/// [`crate::storage::StorageHandle::list_active_limit_windows`]); any window
/// already expired at `now_ms` is ignored. A pane is counted limited until
/// *all* of its active windows have expired, so its recovery time is the
/// maximum effective reset across its windows.
///
/// The result preserves the invariant `usable + limited == total_panes` at
/// every timeline point. If more distinct panes are limited than the reported
/// fleet size (stale windows for departed panes), the limited count is clamped
/// to `total_panes`.
#[must_use]
pub fn build_limits_forecast(
    now_ms: i64,
    total_panes: u32,
    windows: &[LimitWindowRecord],
) -> LimitsForecast {
    // Per-pane recovery time = max effective reset across that pane's active
    // windows (the pane is usable only once the last window clears).
    let mut pane_recovery: BTreeMap<u64, i64> = BTreeMap::new();
    let mut active: Vec<ActiveLimitWindow> = Vec::new();

    for window in windows {
        if !window.is_active(now_ms) {
            continue;
        }
        let reset = window.effective_reset_at_ms();
        let entry = pane_recovery.entry(window.pane_id).or_insert(reset);
        *entry = (*entry).max(reset);
        active.push(ActiveLimitWindow {
            pane_id: window.pane_id,
            service: window.service.clone(),
            account_id: window.account_id.clone(),
            effective_reset_at_ms: reset,
            reset_source: window.reset_source.clone(),
            reset_known: window.reset_known(),
        });
    }

    active.sort_by(|a, b| {
        a.effective_reset_at_ms
            .cmp(&b.effective_reset_at_ms)
            .then_with(|| a.pane_id.cmp(&b.pane_id))
            .then_with(|| a.service.cmp(&b.service))
            .then_with(|| a.account_id.cmp(&b.account_id))
    });

    let limited_distinct = u32::try_from(pane_recovery.len()).unwrap_or(u32::MAX);
    let limited_now = limited_distinct.min(total_panes);
    let usable_now = total_panes.saturating_sub(limited_now);

    // Group pane recoveries by deadline for the timeline.
    let mut by_time: BTreeMap<i64, Vec<u64>> = BTreeMap::new();
    for (pane_id, reset) in &pane_recovery {
        by_time.entry(*reset).or_default().push(*pane_id);
    }

    let mut timeline = Vec::with_capacity(by_time.len());
    let mut recovered: u32 = 0;
    for (at_ms, mut panes) in by_time {
        panes.sort_unstable();
        recovered = recovered.saturating_add(u32::try_from(panes.len()).unwrap_or(u32::MAX));
        // Recovered panes can never exceed those counted limited now.
        let recovered_clamped = recovered.min(limited_now);
        let usable_panes = usable_now.saturating_add(recovered_clamped).min(total_panes);
        let limited_panes = total_panes.saturating_sub(usable_panes);
        timeline.push(LimitForecastPoint {
            at_ms,
            usable_panes,
            limited_panes,
            recovering_panes: panes,
        });
    }

    LimitsForecast {
        now_ms,
        total_panes,
        usable_now,
        limited_now,
        timeline,
        active,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a limit-window record with the fields the forecast cares about;
    /// everything else gets benign defaults.
    fn window(
        pane_id: u64,
        service: &str,
        account_id: &str,
        reset_at: Option<i64>,
        reset_source: &str,
        last_seen_at: i64,
        conservative_ttl_ms: i64,
    ) -> LimitWindowRecord {
        LimitWindowRecord {
            id: 0,
            pane_id,
            service: service.to_string(),
            account_id: account_id.to_string(),
            account_db_id: None,
            account_known: false,
            agent_type: None,
            rule_id: "test.rule".to_string(),
            event_type: "usage.reached".to_string(),
            limited_at: last_seen_at,
            reset_at,
            reset_source: reset_source.to_string(),
            reset_text: None,
            conservative_ttl_ms,
            last_seen_at,
            seen_count: 1,
            metadata: None,
            created_at: last_seen_at,
            updated_at: last_seen_at,
        }
    }

    #[test]
    fn effective_reset_prefers_reset_at_else_conservative_ttl() {
        let absolute = window(1, "openai", "a", Some(5_000), "absolute", 1_000, 0);
        assert_eq!(absolute.effective_reset_at_ms(), 5_000);
        assert!(absolute.reset_known());

        let unknown = window(1, "openai", "a", None, "unknown_ttl", 1_000, 300_000);
        assert_eq!(unknown.effective_reset_at_ms(), 301_000);
        assert!(!unknown.reset_known());

        // Saturating: a corrupt TTL cannot overflow.
        let corrupt = window(1, "openai", "a", None, "unknown_ttl", i64::MAX, 1_000);
        assert_eq!(corrupt.effective_reset_at_ms(), i64::MAX);
    }

    #[test]
    fn is_active_is_exclusive_of_the_reset_instant() {
        let w = window(1, "openai", "a", Some(5_000), "absolute", 1_000, 0);
        assert!(w.is_active(4_999));
        assert!(!w.is_active(5_000), "pane is usable exactly at reset");
        assert!(!w.is_active(5_001));
    }

    #[test]
    fn empty_ledger_reports_full_capacity() {
        let forecast = build_limits_forecast(1_000, 10, &[]);
        assert_eq!(forecast.usable_now, 10);
        assert_eq!(forecast.limited_now, 0);
        assert!(forecast.timeline.is_empty());
        assert!(forecast.active.is_empty());
    }

    #[test]
    fn single_active_window_recovers_one_pane_at_reset() {
        let windows = [window(7, "openai", "a", Some(14_000), "absolute", 1_000, 0)];
        let forecast = build_limits_forecast(10_000, 31, &windows);
        assert_eq!(forecast.usable_now, 30);
        assert_eq!(forecast.limited_now, 1);
        assert_eq!(forecast.timeline.len(), 1);
        let point = &forecast.timeline[0];
        assert_eq!(point.at_ms, 14_000);
        assert_eq!(point.usable_panes, 31);
        assert_eq!(point.limited_panes, 0);
        assert_eq!(point.recovering_panes, vec![7]);
        assert_eq!(forecast.active.len(), 1);
        assert!(forecast.active[0].reset_known);
    }

    #[test]
    fn expired_windows_are_ignored() {
        let windows = [
            window(1, "openai", "a", Some(500), "absolute", 100, 0), // expired
            window(2, "openai", "a", Some(20_000), "absolute", 100, 0),
        ];
        let forecast = build_limits_forecast(10_000, 5, &windows);
        assert_eq!(forecast.limited_now, 1);
        assert_eq!(forecast.usable_now, 4);
        assert_eq!(forecast.active.len(), 1);
        assert_eq!(forecast.active[0].pane_id, 2);
    }

    #[test]
    fn pane_limited_on_two_accounts_recovers_at_latest_window() {
        let windows = [
            window(3, "openai", "a", Some(12_000), "absolute", 1_000, 0),
            window(3, "anthropic", "b", Some(20_000), "absolute", 1_000, 0),
        ];
        let forecast = build_limits_forecast(10_000, 4, &windows);
        // One distinct pane is limited despite two windows.
        assert_eq!(forecast.limited_now, 1);
        assert_eq!(forecast.usable_now, 3);
        assert_eq!(forecast.timeline.len(), 1);
        assert_eq!(forecast.timeline[0].at_ms, 20_000, "recovers at latest");
        assert_eq!(forecast.timeline[0].usable_panes, 4);
        // Both windows surface in the detail list.
        assert_eq!(forecast.active.len(), 2);
    }

    #[test]
    fn timeline_is_deterministic_and_cumulative() {
        let windows = [
            window(5, "openai", "a", Some(30_000), "absolute", 1_000, 0),
            window(2, "openai", "a", Some(20_000), "absolute", 1_000, 0),
            window(9, "openai", "a", Some(20_000), "absolute", 1_000, 0),
        ];
        let forecast = build_limits_forecast(10_000, 10, &windows);
        assert_eq!(forecast.usable_now, 7);
        assert_eq!(forecast.limited_now, 3);
        assert_eq!(forecast.timeline.len(), 2);
        // First point: panes 2 and 9 recover together (sorted ascending).
        assert_eq!(forecast.timeline[0].at_ms, 20_000);
        assert_eq!(forecast.timeline[0].recovering_panes, vec![2, 9]);
        assert_eq!(forecast.timeline[0].usable_panes, 9);
        assert_eq!(forecast.timeline[0].limited_panes, 1);
        // Second point: pane 5 recovers; fleet fully usable.
        assert_eq!(forecast.timeline[1].at_ms, 30_000);
        assert_eq!(forecast.timeline[1].recovering_panes, vec![5]);
        assert_eq!(forecast.timeline[1].usable_panes, 10);
        assert_eq!(forecast.timeline[1].limited_panes, 0);
    }

    #[test]
    fn unknown_ttl_window_is_marked_not_known() {
        let windows = [window(1, "google", "a", None, "unknown_ttl", 10_000, 300_000)];
        let forecast = build_limits_forecast(10_000, 2, &windows);
        assert_eq!(forecast.limited_now, 1);
        assert_eq!(forecast.active.len(), 1);
        assert!(!forecast.active[0].reset_known);
        assert_eq!(forecast.active[0].effective_reset_at_ms, 310_000);
        assert_eq!(forecast.timeline[0].at_ms, 310_000);
    }

    #[test]
    fn limited_count_clamps_to_fleet_size() {
        // More distinct limited panes than the reported fleet size (stale
        // windows for departed panes): limited clamps to total, usable to 0,
        // and the timeline drives usable back up to the fleet size.
        let windows = [
            window(1, "openai", "a", Some(20_000), "absolute", 1_000, 0),
            window(2, "openai", "a", Some(20_000), "absolute", 1_000, 0),
            window(3, "openai", "a", Some(30_000), "absolute", 1_000, 0),
        ];
        let forecast = build_limits_forecast(10_000, 2, &windows);
        assert_eq!(forecast.limited_now, 2);
        assert_eq!(forecast.usable_now, 0);
        // Invariant holds at every point.
        for point in &forecast.timeline {
            assert_eq!(point.usable_panes + point.limited_panes, 2);
        }
        assert_eq!(forecast.timeline.last().unwrap().usable_panes, 2);
    }
}
