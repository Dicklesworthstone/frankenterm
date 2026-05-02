//! BSU override dispatcher (br-ft-a9eu1 step 5 substrate slice).
//!
//! The orchestrator substrate
//! ([`sync_output_buffer_orchestrator`]) ships
//! [`evaluate_override`] — a pure decision function that
//! maps `(OverrideTrigger, bsu_depth)` to an
//! [`OverrideAction`] (`PassThrough` / `Coalesce` /
//! `ForceFlushNow`). This module ships the **stateful
//! dispatcher** the integration drives: it holds the
//! coalesced-overrides queue (for `A11yQuery` events that
//! arrive mid-BSU and must be batched until ESU), routes
//! each incoming trigger through [`evaluate_override`], and
//! emits typed [`DispatchOutcome`] values the integration
//! acts on.
//!
//! [`sync_output_buffer_orchestrator`]: crate::sync_output_buffer_orchestrator
//! [`evaluate_override`]: crate::sync_output_buffer_orchestrator::evaluate_override
//! [`OverrideAction`]: crate::sync_output_buffer_orchestrator::OverrideAction
//!
//! ## Bead invariants this dispatcher enforces
//!
//! - **A11y batched at ESU boundary**: mid-BSU AT-SPI /
//!   NSAccessibility queries are queued via
//!   [`OverrideDispatcher::dispatch`] (returning
//!   `Coalesced`); the integration calls
//!   [`OverrideDispatcher::drain_coalesced_at_esu`] when ESU
//!   fires to flush them in arrival order.
//! - **Cursor blink + BEL pass through**: substrate routes
//!   these to `PassThrough`; the dispatcher returns
//!   `ImmediateDispatch`, telemetry records the override.
//! - **Live-resize forces immediate flush**: substrate
//!   routes to `ForceFlushNow`; the dispatcher returns
//!   `RequestForceFlush` so the integration drains the BSU
//!   buffer with `DrainCause::LiveResizeForce`.
//!
//! ## What this module *doesn't* do
//!
//! - It does NOT touch the per-pane byte buffer. The
//!   integration calls
//!   [`crate::sync_output_buffer_ring::BsuRingBuffer`] when
//!   the dispatcher returns `RequestForceFlush`.
//! - It does NOT speak BSU depth — the caller passes
//!   `bsu_depth` per dispatch (queried from
//!   [`crate::sync_output_watchdog::BsuDepthCounter`]).
//! - It does NOT serialize the AT payload. The dispatcher
//!   stores opaque [`OverridePayload`] handles; the
//!   integration's a11y bridge owns translation.

use std::collections::VecDeque;

use crate::sync_output_buffer_orchestrator::{
    OverrideAction, OverrideTrigger, SyncOutputOrchestratorTelemetry,
    evaluate_override,
};

// ============================================================================
// OverridePayload — opaque carrier for queued overrides
// ============================================================================

/// Opaque payload the integration's caller attaches to an
/// override. The dispatcher stores it verbatim and hands it
/// back at drain time.
///
/// Variants are non-exhaustive at the substrate level — the
/// integration's a11y bridge / bell driver / cursor-blink
/// timer each pick the variant they care about. Substrate
/// doesn't dispatch on the contents.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum OverridePayload {
    /// AT-SPI / NSAccessibility query handle. The
    /// integration's a11y bridge maps this to the platform
    /// callback that owes a response. Numeric handle is
    /// arbitrary — the integration's bridge owns the
    /// mapping.
    A11yQueryHandle(u64),
    /// Bell event — visual / audible flash dispatch.
    Bell,
    /// Cursor-blink redraw tick.
    CursorBlink,
    /// Live-resize state change. Substrate's
    /// `ForceFlushNow` dispatch consumes this; the
    /// integration uses it to attribute the resulting
    /// drain.
    LiveResizeStateChange,
    /// Operator-controlled / test-injection variant. The
    /// substrate doesn't emit this, but tests assert
    /// dispatch shape against it.
    Test(u64),
}

// ============================================================================
// Queue entry
// ============================================================================

/// One entry in the coalesced-overrides queue. Carries the
/// trigger, payload, and a monotonic sequence number so
/// drain order is deterministic regardless of the queue's
/// internal storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoalescedEntry {
    pub trigger: OverrideTrigger,
    pub payload: OverridePayload,
    /// Per-dispatcher monotonic sequence number assigned at
    /// queue-time. Useful when the integration logs
    /// dispatch events and needs to match them back to a
    /// drain.
    pub seq: u64,
}

// ============================================================================
// Dispatch outcome
// ============================================================================

/// What the integration must do for one override call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchOutcome {
    /// Substrate routed the trigger to `PassThrough`. The
    /// integration dispatches the override out-of-band
    /// immediately (e.g., flash the bell, redraw cursor,
    /// answer the a11y query).
    ImmediateDispatch {
        trigger: OverrideTrigger,
        payload: OverridePayload,
    },
    /// Substrate routed the trigger to `Coalesce`. The
    /// payload was queued; integration receives it later via
    /// [`OverrideDispatcher::drain_coalesced_at_esu`].
    Coalesced { seq: u64 },
    /// Substrate routed the trigger to `ForceFlushNow`. The
    /// integration must drain the BSU buffer with
    /// `DrainCause::LiveResizeForce` (or analogous) BEFORE
    /// dispatching the override.
    RequestForceFlush {
        trigger: OverrideTrigger,
        payload: OverridePayload,
    },
}

// ============================================================================
// Dispatcher
// ============================================================================

/// Stateful BSU override dispatcher. One per pane.
#[derive(Debug, Default)]
pub struct OverrideDispatcher {
    /// FIFO queue of coalesced overrides awaiting ESU drain.
    /// VecDeque so drain is O(n) front-to-back without
    /// reallocation.
    queue: VecDeque<CoalescedEntry>,
    /// Monotonic sequence counter — bumped per `dispatch`
    /// call regardless of outcome class so logs can
    /// correlate.
    next_seq: u64,
}

impl OverrideDispatcher {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            next_seq: 0,
        }
    }

    /// Number of currently queued (coalesced) overrides.
    /// Useful for `ft doctor` surfaces and tests.
    #[must_use]
    pub fn queue_len(&self) -> usize {
        self.queue.len()
    }

    /// Whether the coalesced queue is empty.
    #[must_use]
    pub fn queue_is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Read the next sequence number that would be assigned
    /// (without bumping). Tests use this to predict the seq
    /// each dispatch returns.
    #[must_use]
    pub const fn peek_next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Dispatch one override.
    ///
    /// Routes via [`evaluate_override`]; queues if
    /// `Coalesce`; returns the typed outcome the integration
    /// acts on. Telemetry counters are bumped via
    /// [`SyncOutputOrchestratorTelemetry::record_override`].
    pub fn dispatch(
        &mut self,
        trigger: OverrideTrigger,
        payload: OverridePayload,
        bsu_depth: u32,
        telemetry: &mut SyncOutputOrchestratorTelemetry,
    ) -> DispatchOutcome {
        let action = evaluate_override(trigger, bsu_depth);
        telemetry.record_override(trigger, action);
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        match action {
            OverrideAction::PassThrough => DispatchOutcome::ImmediateDispatch {
                trigger,
                payload,
            },
            OverrideAction::Coalesce => {
                self.queue.push_back(CoalescedEntry {
                    trigger,
                    payload,
                    seq,
                });
                DispatchOutcome::Coalesced { seq }
            }
            OverrideAction::ForceFlushNow => DispatchOutcome::RequestForceFlush {
                trigger,
                payload,
            },
        }
    }

    /// Drain all coalesced entries (in arrival order) and
    /// reset the queue. The integration calls this from the
    /// ESU drain path.
    #[must_use]
    pub fn drain_coalesced_at_esu(&mut self) -> Vec<CoalescedEntry> {
        self.queue.drain(..).collect()
    }

    /// Drop all coalesced entries without returning them.
    /// Used when the pane closes mid-BSU and the queued
    /// payloads are no longer addressable.
    pub fn clear(&mut self) {
        self.queue.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn telemetry() -> SyncOutputOrchestratorTelemetry {
        SyncOutputOrchestratorTelemetry::default()
    }

    // ----------------------------------------------------------------
    // PassThrough triggers (Bell, CursorBlink) — always immediate
    // ----------------------------------------------------------------

    #[test]
    fn bell_dispatches_immediately_at_any_depth() {
        let mut d = OverrideDispatcher::new();
        let mut t = telemetry();
        for depth in [0u32, 1, 5, 100] {
            let out =
                d.dispatch(OverrideTrigger::Bell, OverridePayload::Bell, depth, &mut t);
            assert!(matches!(
                out,
                DispatchOutcome::ImmediateDispatch {
                    trigger: OverrideTrigger::Bell,
                    payload: OverridePayload::Bell,
                }
            ));
        }
        assert_eq!(d.queue_len(), 0);
    }

    #[test]
    fn cursor_blink_dispatches_immediately_at_any_depth() {
        let mut d = OverrideDispatcher::new();
        let mut t = telemetry();
        for depth in [0u32, 1, 5, 100] {
            let out = d.dispatch(
                OverrideTrigger::CursorBlink,
                OverridePayload::CursorBlink,
                depth,
                &mut t,
            );
            assert!(matches!(
                out,
                DispatchOutcome::ImmediateDispatch {
                    trigger: OverrideTrigger::CursorBlink,
                    payload: OverridePayload::CursorBlink,
                }
            ));
        }
    }

    // ----------------------------------------------------------------
    // ForceFlushNow trigger (LiveResize)
    // ----------------------------------------------------------------

    #[test]
    fn live_resize_requests_force_flush_at_any_depth() {
        let mut d = OverrideDispatcher::new();
        let mut t = telemetry();
        for depth in [0u32, 1, 5, 100] {
            let out = d.dispatch(
                OverrideTrigger::LiveResize,
                OverridePayload::LiveResizeStateChange,
                depth,
                &mut t,
            );
            assert!(matches!(
                out,
                DispatchOutcome::RequestForceFlush {
                    trigger: OverrideTrigger::LiveResize,
                    ..
                }
            ));
        }
    }

    // ----------------------------------------------------------------
    // A11yQuery — Coalesce while BSU open, PassThrough when closed
    // ----------------------------------------------------------------

    #[test]
    fn a11y_query_passes_through_when_bsu_closed() {
        let mut d = OverrideDispatcher::new();
        let mut t = telemetry();
        let out = d.dispatch(
            OverrideTrigger::A11yQuery,
            OverridePayload::A11yQueryHandle(42),
            0,
            &mut t,
        );
        assert!(matches!(
            out,
            DispatchOutcome::ImmediateDispatch {
                trigger: OverrideTrigger::A11yQuery,
                ..
            }
        ));
        assert_eq!(d.queue_len(), 0);
    }

    #[test]
    fn a11y_query_coalesces_when_bsu_open() {
        let mut d = OverrideDispatcher::new();
        let mut t = telemetry();
        let out = d.dispatch(
            OverrideTrigger::A11yQuery,
            OverridePayload::A11yQueryHandle(7),
            1,
            &mut t,
        );
        match out {
            DispatchOutcome::Coalesced { seq } => assert_eq!(seq, 0),
            other => panic!("expected Coalesced, got {other:?}"),
        }
        assert_eq!(d.queue_len(), 1);
    }

    // ----------------------------------------------------------------
    // Coalesce queue — FIFO + drain semantics
    // ----------------------------------------------------------------

    #[test]
    fn coalesced_queue_drains_in_arrival_order() {
        let mut d = OverrideDispatcher::new();
        let mut t = telemetry();
        for handle in [10u64, 20, 30, 40] {
            let _ = d.dispatch(
                OverrideTrigger::A11yQuery,
                OverridePayload::A11yQueryHandle(handle),
                3,
                &mut t,
            );
        }
        assert_eq!(d.queue_len(), 4);

        let drained = d.drain_coalesced_at_esu();
        assert_eq!(drained.len(), 4);
        assert_eq!(
            drained
                .iter()
                .map(|e| match &e.payload {
                    OverridePayload::A11yQueryHandle(h) => *h,
                    _ => panic!("unexpected payload"),
                })
                .collect::<Vec<_>>(),
            vec![10, 20, 30, 40],
        );
        // Queue is empty after drain.
        assert_eq!(d.queue_len(), 0);
    }

    #[test]
    fn drain_empty_queue_returns_empty_vec() {
        let mut d = OverrideDispatcher::new();
        let drained = d.drain_coalesced_at_esu();
        assert!(drained.is_empty());
    }

    #[test]
    fn drained_entries_carry_seq_in_arrival_order() {
        let mut d = OverrideDispatcher::new();
        let mut t = telemetry();
        for h in 0u64..3 {
            let _ = d.dispatch(
                OverrideTrigger::A11yQuery,
                OverridePayload::A11yQueryHandle(h),
                1,
                &mut t,
            );
        }
        let drained = d.drain_coalesced_at_esu();
        assert_eq!(
            drained.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![0, 1, 2],
        );
    }

    // ----------------------------------------------------------------
    // Sequence numbering
    // ----------------------------------------------------------------

    #[test]
    fn seq_bumps_on_every_dispatch_regardless_of_outcome() {
        let mut d = OverrideDispatcher::new();
        let mut t = telemetry();
        // Bell (immediate) — bumps seq.
        let _ = d.dispatch(
            OverrideTrigger::Bell,
            OverridePayload::Bell,
            0,
            &mut t,
        );
        // LiveResize (force-flush) — bumps seq.
        let _ = d.dispatch(
            OverrideTrigger::LiveResize,
            OverridePayload::LiveResizeStateChange,
            5,
            &mut t,
        );
        // A11yQuery (coalesced) — bumps seq.
        let _ = d.dispatch(
            OverrideTrigger::A11yQuery,
            OverridePayload::A11yQueryHandle(0),
            5,
            &mut t,
        );
        assert_eq!(d.peek_next_seq(), 3);
    }

    #[test]
    fn coalesced_entries_preserve_seq_assigned_at_dispatch() {
        let mut d = OverrideDispatcher::new();
        let mut t = telemetry();
        // Dispatch a Bell first (seq=0) — pass-through, not
        // queued. Then two A11y queries (seq=1, seq=2) —
        // coalesced.
        let _ = d.dispatch(OverrideTrigger::Bell, OverridePayload::Bell, 1, &mut t);
        let _ = d.dispatch(
            OverrideTrigger::A11yQuery,
            OverridePayload::A11yQueryHandle(1),
            1,
            &mut t,
        );
        let _ = d.dispatch(
            OverrideTrigger::A11yQuery,
            OverridePayload::A11yQueryHandle(2),
            1,
            &mut t,
        );
        let drained = d.drain_coalesced_at_esu();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].seq, 1);
        assert_eq!(drained[1].seq, 2);
    }

    // ----------------------------------------------------------------
    // Telemetry plumb-through
    // ----------------------------------------------------------------

    #[test]
    fn telemetry_records_each_dispatch() {
        let mut d = OverrideDispatcher::new();
        let mut t = telemetry();
        // 3 bells, 2 cursor blinks, 1 live resize, 4 a11y queries
        // (3 mid-BSU coalesced + 1 BSU-closed pass-through).
        for _ in 0..3 {
            let _ = d.dispatch(
                OverrideTrigger::Bell,
                OverridePayload::Bell,
                0,
                &mut t,
            );
        }
        for _ in 0..2 {
            let _ = d.dispatch(
                OverrideTrigger::CursorBlink,
                OverridePayload::CursorBlink,
                0,
                &mut t,
            );
        }
        let _ = d.dispatch(
            OverrideTrigger::LiveResize,
            OverridePayload::LiveResizeStateChange,
            5,
            &mut t,
        );
        for h in 0..3 {
            let _ = d.dispatch(
                OverrideTrigger::A11yQuery,
                OverridePayload::A11yQueryHandle(h),
                3,
                &mut t,
            );
        }
        let _ = d.dispatch(
            OverrideTrigger::A11yQuery,
            OverridePayload::A11yQueryHandle(99),
            0, // closed → pass-through
            &mut t,
        );

        // Per-trigger counters.
        assert_eq!(t.overrides_by_trigger.bell, 3);
        assert_eq!(t.overrides_by_trigger.cursor_blink, 2);
        assert_eq!(t.overrides_by_trigger.live_resize, 1);
        assert_eq!(t.overrides_by_trigger.a11y_query, 4);

        // Per-action counters: 3 bell + 2 cursor + 1 a11y(closed)
        // = 6 PassThrough; 3 a11y(open) = Coalesce; 1 LiveResize =
        // ForceFlush.
        assert_eq!(t.overrides_pass_through, 6);
        assert_eq!(t.overrides_coalesced, 3);
        assert_eq!(t.overrides_force_flush, 1);
    }

    // ----------------------------------------------------------------
    // clear()
    // ----------------------------------------------------------------

    #[test]
    fn clear_drops_queued_without_telemetry_update() {
        let mut d = OverrideDispatcher::new();
        let mut t = telemetry();
        for h in 0..5u64 {
            let _ = d.dispatch(
                OverrideTrigger::A11yQuery,
                OverridePayload::A11yQueryHandle(h),
                1,
                &mut t,
            );
        }
        assert_eq!(d.queue_len(), 5);
        let snap_pass = t.overrides_pass_through;
        let snap_coalesce = t.overrides_coalesced;
        d.clear();
        assert_eq!(d.queue_len(), 0);
        // Telemetry untouched by clear (the 5 dispatches
        // already counted; clear doesn't re-route).
        assert_eq!(t.overrides_pass_through, snap_pass);
        assert_eq!(t.overrides_coalesced, snap_coalesce);
    }

    // ----------------------------------------------------------------
    // Bead's "DO NOT BREAK" invariants — end-to-end
    // ----------------------------------------------------------------

    #[test]
    fn invariant_a11y_batched_at_esu_boundary() {
        // Bead: AT updates batched at ESU boundary; no
        // mid-BSU announcements.
        let mut d = OverrideDispatcher::new();
        let mut t = telemetry();

        // BSU opens (depth=1). 3 a11y queries arrive.
        for h in 0u64..3 {
            let out = d.dispatch(
                OverrideTrigger::A11yQuery,
                OverridePayload::A11yQueryHandle(h),
                1,
                &mut t,
            );
            assert!(matches!(out, DispatchOutcome::Coalesced { .. }));
        }
        // Queue holds 3 — none have been dispatched.
        assert_eq!(d.queue_len(), 3);

        // ESU fires — drain.
        let drained = d.drain_coalesced_at_esu();
        assert_eq!(drained.len(), 3);
        // Queue cleared.
        assert!(d.queue_is_empty());
    }

    #[test]
    fn invariant_bell_pass_through_during_bsu() {
        // Bead: BEL within BSU still flashes immediately.
        let mut d = OverrideDispatcher::new();
        let mut t = telemetry();
        let out = d.dispatch(
            OverrideTrigger::Bell,
            OverridePayload::Bell,
            5, // BSU depth high
            &mut t,
        );
        assert!(matches!(out, DispatchOutcome::ImmediateDispatch { .. }));
    }

    #[test]
    fn invariant_cursor_blink_continues_during_bsu() {
        // Bead: cursor blink continues during BSU.
        let mut d = OverrideDispatcher::new();
        let mut t = telemetry();
        let out = d.dispatch(
            OverrideTrigger::CursorBlink,
            OverridePayload::CursorBlink,
            10, // BSU depth high
            &mut t,
        );
        assert!(matches!(out, DispatchOutcome::ImmediateDispatch { .. }));
    }

    #[test]
    fn invariant_live_resize_force_flush_during_bsu() {
        // Bead: live-resize Resizing forces immediate flush.
        let mut d = OverrideDispatcher::new();
        let mut t = telemetry();
        let out = d.dispatch(
            OverrideTrigger::LiveResize,
            OverridePayload::LiveResizeStateChange,
            7,
            &mut t,
        );
        assert!(matches!(out, DispatchOutcome::RequestForceFlush { .. }));
    }
}
