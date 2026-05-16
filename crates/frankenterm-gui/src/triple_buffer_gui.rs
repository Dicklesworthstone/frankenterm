use frankenterm_core::session_pane_state::TerminalState;
use frankenterm_core::triple_buffer::PublishOutcome;
use frankenterm_core::triple_buffer_fleet_health::{
    ConsecutiveRecyclePolicy, PaneHealthSnapshot, PaneId, RecycleAlertDecision,
    evaluate_recycle_alert,
};
use frankenterm_core::watchdoged_triple_buffer::{
    PolledDecision, WatchdogedAcquireGuard, WatchdogedHealth, WatchdogedTripleBuffer,
};
use std::collections::{HashMap, hash_map::Entry};
use std::time::Instant;

pub type TerminalStateTripleBufferPane = GuiTripleBufferPane<TerminalState>;

#[derive(Default)]
pub struct TerminalStateTripleBufferRegistry {
    panes: HashMap<u64, TerminalStateTripleBufferPane>,
}

impl TerminalStateTripleBufferRegistry {
    pub fn publish(&mut self, pane_id: u64, state: TerminalState) -> PublishOutcome {
        match self.panes.entry(pane_id) {
            Entry::Occupied(entry) => entry.get().publish(state),
            Entry::Vacant(entry) => entry
                .insert(TerminalStateTripleBufferPane::new(pane_id, state.clone()))
                .publish(state),
        }
    }

    #[must_use]
    pub fn pane(&self, pane_id: u64) -> Option<&TerminalStateTripleBufferPane> {
        self.panes.get(&pane_id)
    }

    pub fn panes_mut(&mut self) -> impl Iterator<Item = &mut TerminalStateTripleBufferPane> {
        self.panes.values_mut()
    }

    pub fn remove(&mut self, pane_id: u64) -> Option<TerminalStateTripleBufferPane> {
        self.panes.remove(&pane_id)
    }

    #[must_use]
    pub fn contains(&self, pane_id: u64) -> bool {
        self.panes.contains_key(&pane_id)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.panes.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.panes.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuiTripleBufferPollReport {
    pub pane_id: u64,
    pub decision: PolledDecision,
    pub snapshot: PaneHealthSnapshot,
    pub alert: RecycleAlertDecision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuiTripleBufferFlushReport {
    pub pane_id: u64,
    pub snapshot: PaneHealthSnapshot,
}

pub struct GuiTripleBufferPane<T: Send + Sync + 'static> {
    pane_id: u64,
    buffer: WatchdogedTripleBuffer<T>,
    last_watchdog_force_recycle_ts_ms: u64,
    watchdog_force_recycle_history_ms: Vec<u64>,
}

impl<T: Send + Sync + 'static> GuiTripleBufferPane<T> {
    #[must_use]
    pub fn new(pane_id: u64, initial: T) -> Self {
        Self::from_buffer(pane_id, WatchdogedTripleBuffer::new(initial))
    }

    #[must_use]
    pub fn from_buffer(pane_id: u64, buffer: WatchdogedTripleBuffer<T>) -> Self {
        Self {
            pane_id,
            buffer,
            last_watchdog_force_recycle_ts_ms: 0,
            watchdog_force_recycle_history_ms: Vec::new(),
        }
    }

    #[must_use]
    pub const fn pane_id(&self) -> u64 {
        self.pane_id
    }

    #[must_use]
    pub fn buffer(&self) -> &WatchdogedTripleBuffer<T> {
        &self.buffer
    }

    pub fn acquire(&self, now: Instant) -> WatchdogedAcquireGuard<T> {
        self.buffer.acquire(now)
    }

    pub fn publish(&self, value: T) -> PublishOutcome {
        self.buffer.publish(value)
    }

    pub fn poll(
        &mut self,
        now: Instant,
        now_ms: u64,
        alert_policy: ConsecutiveRecyclePolicy,
    ) -> GuiTripleBufferPollReport {
        let decision = self.buffer.poll(now);
        if decision.force_recycled() {
            self.last_watchdog_force_recycle_ts_ms = now_ms;
            self.watchdog_force_recycle_history_ms.push(now_ms);
        }
        self.prune_recycle_history(now_ms, alert_policy);

        GuiTripleBufferPollReport {
            pane_id: self.pane_id,
            decision,
            snapshot: self.health_snapshot(),
            alert: evaluate_recycle_alert(
                &self.watchdog_force_recycle_history_ms,
                alert_policy,
                now_ms,
            ),
        }
    }

    pub fn force_flush(&self) -> GuiTripleBufferFlushReport {
        self.buffer.force_recycle();
        GuiTripleBufferFlushReport {
            pane_id: self.pane_id,
            snapshot: self.health_snapshot(),
        }
    }

    #[must_use]
    pub fn health_snapshot(&self) -> PaneHealthSnapshot {
        pane_health_snapshot_from_watchdoged_health(
            self.pane_id,
            &self.buffer.health(),
            self.last_watchdog_force_recycle_ts_ms,
        )
    }

    #[must_use]
    pub fn watchdog_force_recycle_history_ms(&self) -> &[u64] {
        &self.watchdog_force_recycle_history_ms
    }

    fn prune_recycle_history(&mut self, now_ms: u64, policy: ConsecutiveRecyclePolicy) {
        let window_start = now_ms.saturating_sub(policy.window_ms);
        self.watchdog_force_recycle_history_ms
            .retain(|ts| *ts >= window_start && *ts <= now_ms);
    }
}

#[must_use]
pub fn pane_health_snapshot_from_watchdoged_health(
    pane_id: u64,
    health: &WatchdogedHealth,
    last_force_recycle_ts_ms: u64,
) -> PaneHealthSnapshot {
    let stats = health.watchdog();
    PaneHealthSnapshot {
        pane_id: PaneId(pane_id),
        acquires: stats.acquires_total(),
        releases: stats.releases_total(),
        warnings: stats.warnings_emitted(),
        force_recycles: stats.force_recycle_invocations(),
        last_force_recycle_ts_ms,
        watchdog_active: health.watchdog_active(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn alert_on_first_recycle() -> ConsecutiveRecyclePolicy {
        ConsecutiveRecyclePolicy {
            alert_after_consecutive: 1,
            window_ms: 60_000,
        }
    }

    fn terminal_state(seq: u16) -> TerminalState {
        TerminalState {
            rows: 24,
            cols: 80,
            cursor_row: seq,
            cursor_col: seq.saturating_mul(2),
            is_alt_screen: seq % 2 == 1,
            title: format!("pane-{seq}"),
        }
    }

    #[test]
    fn terminal_state_registry_creates_and_updates_live_pane_buffer() {
        let origin = Instant::now();
        let mut registry = TerminalStateTripleBufferRegistry::default();

        let first = registry.publish(42, terminal_state(1));
        assert!(!first.overrun);
        assert_eq!(registry.len(), 1);
        assert!(registry.contains(42));

        let pane = registry.pane(42).expect("registry should own pane 42");
        let first_guard = pane.acquire(origin);
        assert_eq!(first_guard.cursor_row, 1);
        assert!(first_guard.is_alt_screen);
        drop(first_guard);

        let second = registry.publish(42, terminal_state(2));
        assert!(!second.overrun);
        assert_eq!(registry.len(), 1);

        let pane = registry.pane(42).expect("registry should retain pane 42");
        let second_guard = pane.acquire(origin + Duration::from_millis(1));
        assert_eq!(second_guard.cursor_row, 2);
        assert_eq!(second_guard.cursor_col, 4);
        assert!(!second_guard.is_alt_screen);
    }

    #[test]
    fn terminal_state_registry_removes_closed_pane_owner() {
        let mut registry = TerminalStateTripleBufferRegistry::default();
        registry.publish(7, terminal_state(1));
        registry.publish(8, terminal_state(2));

        let removed = registry.remove(7);
        assert!(removed.is_some());
        assert!(!registry.contains(7));
        assert!(registry.contains(8));
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn gui_triple_buffer_pane_publishes_and_acquires_snapshots() {
        let origin = Instant::now();
        let pane = GuiTripleBufferPane::new(7, 1_u32);

        let publish = pane.publish(2);
        assert!(!publish.overrun);
        let guard = pane.acquire(origin);

        assert_eq!(*guard, 2);
        drop(guard);

        let snapshot = pane.health_snapshot();
        assert_eq!(snapshot.pane_id, PaneId(7));
        assert_eq!(snapshot.acquires, 1);
        assert_eq!(snapshot.releases, 1);
        assert_eq!(snapshot.force_recycles, 0);
        assert!(!snapshot.watchdog_active);
    }

    #[test]
    fn gui_poll_records_watchdog_force_recycle_and_alert() {
        let origin = Instant::now();
        let mut pane = GuiTripleBufferPane::new(9, 1_u32);
        let _guard = pane.acquire(origin);

        let report = pane.poll(
            origin + Duration::from_secs(6),
            10_000,
            alert_on_first_recycle(),
        );

        assert!(report.decision.force_recycled());
        assert_eq!(report.pane_id, 9);
        assert_eq!(report.snapshot.force_recycles, 1);
        assert_eq!(report.snapshot.last_force_recycle_ts_ms, 10_000);
        assert_eq!(report.alert, RecycleAlertDecision::FireAlert);
        assert_eq!(pane.watchdog_force_recycle_history_ms(), &[10_000]);
    }

    #[test]
    fn operator_force_flush_does_not_increment_watchdog_recycle_stats() {
        let pane = GuiTripleBufferPane::new(11, 1_u32);

        let report = pane.force_flush();

        assert_eq!(report.pane_id, 11);
        assert_eq!(report.snapshot.pane_id, PaneId(11));
        assert_eq!(report.snapshot.force_recycles, 0);
        assert_eq!(report.snapshot.last_force_recycle_ts_ms, 0);
        assert!(pane.watchdog_force_recycle_history_ms().is_empty());
    }

    #[test]
    fn recycle_history_is_pruned_to_alert_window() {
        let origin = Instant::now();
        let mut pane = GuiTripleBufferPane::new(13, 1_u32);
        let policy = ConsecutiveRecyclePolicy {
            alert_after_consecutive: 2,
            window_ms: 100,
        };

        {
            let _guard = pane.acquire(origin);
            let first = pane.poll(origin + Duration::from_secs(6), 1_000, policy);
            assert!(first.decision.force_recycled());
        }
        {
            let _guard = pane.acquire(origin + Duration::from_secs(7));
            let second = pane.poll(origin + Duration::from_secs(13), 1_200, policy);
            assert!(second.decision.force_recycled());
            assert_eq!(second.alert, RecycleAlertDecision::Quiet);
        }

        assert_eq!(pane.watchdog_force_recycle_history_ms(), &[1_200]);
    }
}
