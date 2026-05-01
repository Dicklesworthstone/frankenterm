//! DEC 2026 BSU buffer-drain orchestrator + override
//! coordinator substrate (ft-1dq8h).
//!
//! Pure-logic substrate covering the substrate-shaped pieces
//! of ft-1dq8h's BSU buffer integration. Sibling modules
//! already shipped:
//!
//! - `sync_output_watchdog.rs` (commit e978c68ad) —
//!   BsuDepthCounter + WatchdogState + ModeQueryState.
//! - `dec_2026_presentation_hold.rs` (commit 12b684db6) —
//!   renderer presentation-hold state machine.
//! - `frankenterm/term/src/terminalstate/mod.rs` —
//!   `synchronized_output: bool` flag.
//! - `triple_buffer.rs` — atomic snapshot swap.
//!
//! This module adds the orchestrator that the integration's
//! BSU/ESU dispatch routes through:
//!
//! - `BsuBufferConfig` — per-pane buffer cap (1 MiB default,
//!   bead implies sized to typical Neovim treesitter
//!   redraw).
//! - `BufferAdmissionDecision` — `Accepted / Truncated /
//!   Refused` — what happens when mid-BSU output exceeds
//!   the per-pane cap.
//! - `evaluate_buffer_admission` pure decision over
//!   `(current_used_bytes, incoming_bytes, config)`.
//! - `OverrideTrigger` 4-variant covering the bead's "DO
//!   NOT BREAK" overrides (`Bell / CursorBlink / LiveResize
//!   / A11yQuery`).
//! - `OverrideAction` 3-variant `(PassThrough / Coalesce /
//!   ForceFlushNow)` — the BEL and live-resize cases force
//!   a flush even mid-BSU; cursor blink passes through; AT
//!   queries coalesce until ESU.
//! - `evaluate_override` pure decision over
//!   `(OverrideTrigger, BsuDepthCounter, BsuBufferConfig)`.
//! - `BufferDrainOutcome` — `Drained{bytes,frames} /
//!   ForceFlushed{bytes} / NoOp`. The integration drains the
//!   buffer at ESU (depth=0) into the triple-buffer's writer
//!   slot; substrate just records what happened.
//! - `SyncOutputOrchestratorTelemetry` — per-session
//!   counters specific to the orchestrator (admissions,
//!   overrides by trigger, mid-BSU bytes, drain outcomes).
//!
//! ## What is deferred to ft-1dq8h follow-up
//!
//! - asupersync timer wiring: `runtime_async::sleep_with_cx`
//!   on `WatchdogState::Pending` deadline.
//! - The actual per-pane ring buffer (Vec<u8> or VecDeque,
//!   integration's choice).
//! - Triple-buffer publish on ESU drain.
//! - PTY response channel emission for the
//!   `CSI ? 2026 $ p` mode-query reply (substrate's
//!   `format_mode_query_response` already returns the
//!   bytes).
//! - macOS/Linux/Windows reduce-motion probe wiring for
//!   live-resize coordination.

#![allow(dead_code)]

// ============================================================================
// Buffer config
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BsuBufferConfig {
    /// Per-pane buffer cap in bytes. Default 1 MiB.
    pub per_pane_max_bytes: u64,
    /// Minimum sane cap — substrate floors operator
    /// misconfiguration at 4 KiB so the buffer at least holds
    /// one PTY chunk.
    pub min_bytes: u64,
    /// Whether to truncate (keep the latest bytes, drop the
    /// oldest) or refuse outright when full. Truncation
    /// matches the bead's "preserve atomic frame at ESU"
    /// intent — better to render a slightly-trimmed frame
    /// than to drop it.
    pub truncate_when_full: bool,
}

pub const DEFAULT_BSU_BUFFER_BYTES: u64 = 1024 * 1024;
pub const MIN_BSU_BUFFER_BYTES: u64 = 4 * 1024;

impl Default for BsuBufferConfig {
    fn default() -> Self {
        Self {
            per_pane_max_bytes: DEFAULT_BSU_BUFFER_BYTES,
            min_bytes: MIN_BSU_BUFFER_BYTES,
            truncate_when_full: true,
        }
    }
}

impl BsuBufferConfig {
    /// Effective cap after the floor is applied.
    #[must_use]
    pub const fn effective_max_bytes(&self) -> u64 {
        if self.per_pane_max_bytes < self.min_bytes {
            self.min_bytes
        } else {
            self.per_pane_max_bytes
        }
    }
}

// ============================================================================
// Buffer admission decision
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BufferAdmissionDecision {
    /// Bytes fit; integration appends.
    Accepted,
    /// Buffer was full; integration drops `dropped_bytes` of
    /// the oldest data and appends. Only fires when
    /// `truncate_when_full`.
    Truncated { dropped_bytes: u64 },
    /// Buffer full and truncation disabled; integration
    /// drops the incoming chunk.
    Refused,
}

impl BufferAdmissionDecision {
    #[must_use]
    pub const fn is_admitted(self) -> bool {
        matches!(self, Self::Accepted | Self::Truncated { .. })
    }
}

/// Pure decision: should the integration admit this PTY
/// chunk into the BSU buffer?
#[must_use]
pub fn evaluate_buffer_admission(
    current_used_bytes: u64,
    incoming_bytes: u64,
    config: BsuBufferConfig,
) -> BufferAdmissionDecision {
    let cap = config.effective_max_bytes();
    let after = current_used_bytes.saturating_add(incoming_bytes);
    if after <= cap {
        return BufferAdmissionDecision::Accepted;
    }
    if !config.truncate_when_full {
        return BufferAdmissionDecision::Refused;
    }
    let overflow = after - cap;
    BufferAdmissionDecision::Truncated {
        dropped_bytes: overflow.min(current_used_bytes),
    }
}

// ============================================================================
// Override coordination — BEL / cursor / live-resize / A11y
// ============================================================================

/// The bead's "DO NOT BREAK" overrides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OverrideTrigger {
    /// `BEL` byte received during BSU. Bead: "BEL within BSU
    /// still flashes immediately (urgent signal)." Substrate
    /// dispatches the visual/audible bell out-of-band.
    Bell,
    /// Cursor-blink timer fired. Bead: "Cursor blink:
    /// continues during BSU." Doesn't touch the buffer; just
    /// repaints the cursor layer.
    CursorBlink,
    /// Live-resize entered. Bead: "Live-resize:
    /// LiveResizeState=Resizing forces immediate flush +
    /// Draft mode." Substrate forces a flush mid-BSU.
    LiveResize,
    /// AT-SPI / NSAccessibility query mid-BSU. Bead's a11y
    /// rule: AT updates batched at ESU boundary; substrate
    /// coalesces.
    A11yQuery,
}

impl OverrideTrigger {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Bell => "bell",
            Self::CursorBlink => "cursor_blink",
            Self::LiveResize => "live_resize",
            Self::A11yQuery => "a11y_query",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OverrideAction {
    /// Trigger fires its own work out-of-band; the BSU
    /// buffer continues unchanged.
    PassThrough,
    /// Trigger queues for delivery at ESU drain — coalesce
    /// to avoid a partial-frame announcement.
    Coalesce,
    /// Force a flush immediately, even mid-BSU. The
    /// integration must drain the buffer into the triple
    /// buffer NOW. Used for live-resize per the bead.
    ForceFlushNow,
}

/// Pure decision combining the trigger with current BSU
/// depth + config. Rules:
///
/// - `Bell` is always `PassThrough`: the bell rings
///   immediately, the buffer keeps accumulating.
/// - `CursorBlink` is always `PassThrough`: cursor renders
///   from the visible-state, not the buffered frame.
/// - `LiveResize` is `ForceFlushNow` regardless of depth —
///   bead's hard rule.
/// - `A11yQuery` is `Coalesce` while in BSU; `PassThrough`
///   when depth=0 (the integration can answer immediately).
#[must_use]
pub fn evaluate_override(
    trigger: OverrideTrigger,
    bsu_depth: u32,
) -> OverrideAction {
    match trigger {
        OverrideTrigger::Bell | OverrideTrigger::CursorBlink => {
            OverrideAction::PassThrough
        }
        OverrideTrigger::LiveResize => OverrideAction::ForceFlushNow,
        OverrideTrigger::A11yQuery => {
            if bsu_depth > 0 {
                OverrideAction::Coalesce
            } else {
                OverrideAction::PassThrough
            }
        }
    }
}

// ============================================================================
// Drain outcome
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DrainCause {
    /// ESU brought depth to zero — natural drain.
    Esu,
    /// Watchdog fired — force-drain after 150 ms timeout.
    Watchdog,
    /// Live-resize forced an immediate flush.
    LiveResizeForce,
    /// Operator-initiated (e.g., `ft flush`).
    Operator,
}

impl DrainCause {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Esu => "esu",
            Self::Watchdog => "watchdog",
            Self::LiveResizeForce => "live_resize_force",
            Self::Operator => "operator",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BufferDrainOutcome {
    /// Buffer drained `bytes` into the triple-buffer writer.
    /// `cause` distinguishes natural ESU vs. watchdog vs.
    /// live-resize.
    Drained { bytes: u64, cause: DrainCause },
    /// Caller asked to drain but buffer was empty.
    NoOp,
}

// ============================================================================
// Telemetry
// ============================================================================

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncOutputOrchestratorTelemetry {
    pub admissions_accepted: u64,
    pub admissions_truncated: u64,
    pub admissions_refused: u64,
    pub bytes_accepted: u64,
    pub bytes_truncated: u64,
    pub overrides_pass_through: u64,
    pub overrides_coalesced: u64,
    pub overrides_force_flush: u64,
    pub overrides_by_trigger: OverridesByTrigger,
    pub drains_esu: u64,
    pub drains_watchdog: u64,
    pub drains_live_resize: u64,
    pub drains_operator: u64,
    pub drains_no_op: u64,
    pub bytes_drained_total: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OverridesByTrigger {
    pub bell: u64,
    pub cursor_blink: u64,
    pub live_resize: u64,
    pub a11y_query: u64,
}

impl SyncOutputOrchestratorTelemetry {
    pub fn record_admission(
        &mut self,
        decision: BufferAdmissionDecision,
        incoming_bytes: u64,
    ) {
        match decision {
            BufferAdmissionDecision::Accepted => {
                self.admissions_accepted = self.admissions_accepted.saturating_add(1);
                self.bytes_accepted = self.bytes_accepted.saturating_add(incoming_bytes);
            }
            BufferAdmissionDecision::Truncated { dropped_bytes } => {
                self.admissions_truncated =
                    self.admissions_truncated.saturating_add(1);
                self.bytes_accepted = self.bytes_accepted.saturating_add(incoming_bytes);
                self.bytes_truncated = self.bytes_truncated.saturating_add(dropped_bytes);
            }
            BufferAdmissionDecision::Refused => {
                self.admissions_refused = self.admissions_refused.saturating_add(1);
            }
        }
    }

    pub fn record_override(&mut self, trigger: OverrideTrigger, action: OverrideAction) {
        let trigger_slot = match trigger {
            OverrideTrigger::Bell => &mut self.overrides_by_trigger.bell,
            OverrideTrigger::CursorBlink => &mut self.overrides_by_trigger.cursor_blink,
            OverrideTrigger::LiveResize => &mut self.overrides_by_trigger.live_resize,
            OverrideTrigger::A11yQuery => &mut self.overrides_by_trigger.a11y_query,
        };
        *trigger_slot = trigger_slot.saturating_add(1);
        match action {
            OverrideAction::PassThrough => {
                self.overrides_pass_through = self.overrides_pass_through.saturating_add(1);
            }
            OverrideAction::Coalesce => {
                self.overrides_coalesced = self.overrides_coalesced.saturating_add(1);
            }
            OverrideAction::ForceFlushNow => {
                self.overrides_force_flush = self.overrides_force_flush.saturating_add(1);
            }
        }
    }

    pub fn record_drain(&mut self, outcome: BufferDrainOutcome) {
        match outcome {
            BufferDrainOutcome::Drained { bytes, cause } => {
                self.bytes_drained_total = self.bytes_drained_total.saturating_add(bytes);
                let slot = match cause {
                    DrainCause::Esu => &mut self.drains_esu,
                    DrainCause::Watchdog => &mut self.drains_watchdog,
                    DrainCause::LiveResizeForce => &mut self.drains_live_resize,
                    DrainCause::Operator => &mut self.drains_operator,
                };
                *slot = slot.saturating_add(1);
            }
            BufferDrainOutcome::NoOp => {
                self.drains_no_op = self.drains_no_op.saturating_add(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----------------------------------------------------------------
    // BsuBufferConfig
    // ----------------------------------------------------------------

    #[test]
    fn config_defaults_match_bead() {
        let c = BsuBufferConfig::default();
        assert_eq!(c.per_pane_max_bytes, 1024 * 1024);
        assert_eq!(c.min_bytes, 4 * 1024);
        assert!(c.truncate_when_full);
    }

    #[test]
    fn config_below_floor_raised() {
        let c = BsuBufferConfig {
            per_pane_max_bytes: 100,
            min_bytes: 4 * 1024,
            truncate_when_full: true,
        };
        assert_eq!(c.effective_max_bytes(), 4 * 1024);
    }

    // ----------------------------------------------------------------
    // evaluate_buffer_admission
    // ----------------------------------------------------------------

    #[test]
    fn admit_accepted_when_under_cap() {
        let config = BsuBufferConfig::default();
        let d = evaluate_buffer_admission(100, 200, config);
        assert_eq!(d, BufferAdmissionDecision::Accepted);
    }

    #[test]
    fn admit_accepted_at_exact_cap() {
        let config = BsuBufferConfig::default();
        let cap = config.effective_max_bytes();
        let d = evaluate_buffer_admission(cap - 100, 100, config);
        assert_eq!(d, BufferAdmissionDecision::Accepted);
    }

    #[test]
    fn admit_truncated_when_over_cap() {
        let config = BsuBufferConfig {
            per_pane_max_bytes: 1_000,
            min_bytes: 100,
            truncate_when_full: true,
        };
        // Used 800; incoming 300 → over by 100.
        let d = evaluate_buffer_admission(800, 300, config);
        assert_eq!(d, BufferAdmissionDecision::Truncated { dropped_bytes: 100 });
    }

    #[test]
    fn admit_refused_when_over_cap_and_no_truncate() {
        let config = BsuBufferConfig {
            per_pane_max_bytes: 1_000,
            min_bytes: 100,
            truncate_when_full: false,
        };
        let d = evaluate_buffer_admission(800, 300, config);
        assert_eq!(d, BufferAdmissionDecision::Refused);
    }

    #[test]
    fn admit_truncated_dropped_bytes_capped_at_used() {
        // Incoming bytes way bigger than used; dropped_bytes
        // can't exceed current used.
        let config = BsuBufferConfig {
            per_pane_max_bytes: 1_000,
            min_bytes: 100,
            truncate_when_full: true,
        };
        let d = evaluate_buffer_admission(100, 5_000, config);
        match d {
            BufferAdmissionDecision::Truncated { dropped_bytes } => {
                assert!(dropped_bytes <= 100);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn admit_is_admitted_predicate() {
        assert!(BufferAdmissionDecision::Accepted.is_admitted());
        assert!(BufferAdmissionDecision::Truncated { dropped_bytes: 10 }.is_admitted());
        assert!(!BufferAdmissionDecision::Refused.is_admitted());
    }

    // ----------------------------------------------------------------
    // OverrideTrigger
    // ----------------------------------------------------------------

    #[test]
    fn override_label_stable() {
        assert_eq!(OverrideTrigger::Bell.label(), "bell");
        assert_eq!(OverrideTrigger::CursorBlink.label(), "cursor_blink");
        assert_eq!(OverrideTrigger::LiveResize.label(), "live_resize");
        assert_eq!(OverrideTrigger::A11yQuery.label(), "a11y_query");
    }

    // ----------------------------------------------------------------
    // evaluate_override
    // ----------------------------------------------------------------

    #[test]
    fn override_bell_always_pass_through() {
        // Bell rings immediately even in deep nesting.
        for depth in [0u32, 1, 5, 100] {
            assert_eq!(
                evaluate_override(OverrideTrigger::Bell, depth),
                OverrideAction::PassThrough,
            );
        }
    }

    #[test]
    fn override_cursor_blink_always_pass_through() {
        for depth in [0u32, 1, 5, 100] {
            assert_eq!(
                evaluate_override(OverrideTrigger::CursorBlink, depth),
                OverrideAction::PassThrough,
            );
        }
    }

    #[test]
    fn override_live_resize_always_force_flush() {
        // Bead's hard rule: Resizing state forces immediate
        // flush regardless of BSU depth.
        for depth in [0u32, 1, 5, 100] {
            assert_eq!(
                evaluate_override(OverrideTrigger::LiveResize, depth),
                OverrideAction::ForceFlushNow,
            );
        }
    }

    #[test]
    fn override_a11y_query_coalesces_in_bsu() {
        let in_bsu = evaluate_override(OverrideTrigger::A11yQuery, 1);
        assert_eq!(in_bsu, OverrideAction::Coalesce);
    }

    #[test]
    fn override_a11y_query_passes_through_when_idle() {
        let idle = evaluate_override(OverrideTrigger::A11yQuery, 0);
        assert_eq!(idle, OverrideAction::PassThrough);
    }

    // ----------------------------------------------------------------
    // Telemetry
    // ----------------------------------------------------------------

    #[test]
    fn telemetry_default_zero() {
        let t = SyncOutputOrchestratorTelemetry::default();
        assert_eq!(t.admissions_accepted, 0);
        assert_eq!(t.bytes_drained_total, 0);
    }

    #[test]
    fn telemetry_record_admission_routes() {
        let mut t = SyncOutputOrchestratorTelemetry::default();
        t.record_admission(BufferAdmissionDecision::Accepted, 100);
        t.record_admission(
            BufferAdmissionDecision::Truncated { dropped_bytes: 50 },
            200,
        );
        t.record_admission(BufferAdmissionDecision::Refused, 0);
        assert_eq!(t.admissions_accepted, 1);
        assert_eq!(t.admissions_truncated, 1);
        assert_eq!(t.admissions_refused, 1);
        assert_eq!(t.bytes_accepted, 300);
        assert_eq!(t.bytes_truncated, 50);
    }

    #[test]
    fn telemetry_record_override_routes() {
        let mut t = SyncOutputOrchestratorTelemetry::default();
        t.record_override(OverrideTrigger::Bell, OverrideAction::PassThrough);
        t.record_override(OverrideTrigger::LiveResize, OverrideAction::ForceFlushNow);
        t.record_override(OverrideTrigger::A11yQuery, OverrideAction::Coalesce);
        assert_eq!(t.overrides_by_trigger.bell, 1);
        assert_eq!(t.overrides_by_trigger.live_resize, 1);
        assert_eq!(t.overrides_by_trigger.a11y_query, 1);
        assert_eq!(t.overrides_pass_through, 1);
        assert_eq!(t.overrides_force_flush, 1);
        assert_eq!(t.overrides_coalesced, 1);
    }

    #[test]
    fn telemetry_record_drain_routes() {
        let mut t = SyncOutputOrchestratorTelemetry::default();
        t.record_drain(BufferDrainOutcome::Drained { bytes: 1024, cause: DrainCause::Esu });
        t.record_drain(BufferDrainOutcome::Drained { bytes: 512, cause: DrainCause::Watchdog });
        t.record_drain(BufferDrainOutcome::Drained { bytes: 256, cause: DrainCause::LiveResizeForce });
        t.record_drain(BufferDrainOutcome::Drained { bytes: 128, cause: DrainCause::Operator });
        t.record_drain(BufferDrainOutcome::NoOp);
        assert_eq!(t.drains_esu, 1);
        assert_eq!(t.drains_watchdog, 1);
        assert_eq!(t.drains_live_resize, 1);
        assert_eq!(t.drains_operator, 1);
        assert_eq!(t.drains_no_op, 1);
        assert_eq!(t.bytes_drained_total, 1024 + 512 + 256 + 128);
    }

    // ----------------------------------------------------------------
    // Cross-cut scenarios
    // ----------------------------------------------------------------

    #[test]
    fn scenario_neovim_treesitter_admitted_drained_at_esu() {
        // Neovim's typical 50 KiB BSU window fits the
        // 1 MiB cap.
        let config = BsuBufferConfig::default();
        let mut t = SyncOutputOrchestratorTelemetry::default();
        let mut used = 0u64;
        for chunk in [10_000u64, 20_000, 15_000, 5_000] {
            let d = evaluate_buffer_admission(used, chunk, config);
            t.record_admission(d, chunk);
            assert_eq!(d, BufferAdmissionDecision::Accepted);
            used = used.saturating_add(chunk);
        }
        assert_eq!(used, 50_000);
        // ESU drains the buffer.
        t.record_drain(BufferDrainOutcome::Drained { bytes: used, cause: DrainCause::Esu });
        assert_eq!(t.drains_esu, 1);
        assert_eq!(t.bytes_drained_total, 50_000);
    }

    #[test]
    fn scenario_runaway_bsu_truncated_at_cap() {
        // A buggy app keeps writing inside BSU; substrate
        // truncates rather than refuses (bead intent: keep
        // the latest atomic frame).
        let config = BsuBufferConfig::default();
        let cap = config.effective_max_bytes();
        let d = evaluate_buffer_admission(cap - 100, 1_000, config);
        match d {
            BufferAdmissionDecision::Truncated { dropped_bytes } => {
                assert!(dropped_bytes > 0);
            }
            other => panic!("expected truncation; got {other:?}"),
        }
    }

    #[test]
    fn scenario_bell_during_bsu_pass_through() {
        // BEL within BSU rings immediately even in deep
        // nesting; buffer continues.
        assert_eq!(
            evaluate_override(OverrideTrigger::Bell, 5),
            OverrideAction::PassThrough,
        );
    }

    #[test]
    fn scenario_live_resize_forces_flush_mid_bsu() {
        // User starts dragging the window mid-Neovim-redraw.
        // Substrate forces a flush so the resize is responsive.
        assert_eq!(
            evaluate_override(OverrideTrigger::LiveResize, 3),
            OverrideAction::ForceFlushNow,
        );
    }

    #[test]
    fn scenario_screen_reader_query_coalesces() {
        // VoiceOver/Orca queries the cell content mid-BSU.
        // Substrate coalesces — the AT gets the post-ESU
        // state, not a mid-frame snapshot.
        let in_bsu = evaluate_override(OverrideTrigger::A11yQuery, 2);
        assert_eq!(in_bsu, OverrideAction::Coalesce);
    }

    #[test]
    fn scenario_screen_reader_query_idle_passes_through() {
        // Same query when no BSU active: substrate answers
        // immediately.
        let idle = evaluate_override(OverrideTrigger::A11yQuery, 0);
        assert_eq!(idle, OverrideAction::PassThrough);
    }

    #[test]
    fn scenario_watchdog_force_drain_records_correctly() {
        // App stuck in BSU; watchdog fires at 150 ms.
        // Integration calls record_drain(Watchdog).
        let mut t = SyncOutputOrchestratorTelemetry::default();
        t.record_drain(BufferDrainOutcome::Drained {
            bytes: 2_000,
            cause: DrainCause::Watchdog,
        });
        assert_eq!(t.drains_watchdog, 1);
        assert_eq!(t.bytes_drained_total, 2_000);
    }
}
