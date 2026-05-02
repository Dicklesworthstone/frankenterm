//! BSU → triple-buffer publish router (br-ft-2m6qe
//! sub-task 3 substrate slice).
//!
//! The bead's sub-task 3:
//!
//! > "Triple-buffer publish on ESU drain: at
//! > BsuDepthOutcome::Flushed, call buffer-drain with
//! > DrainCause::Esu, take the bytes, run the term-layer
//! > state apply, swap into triple_buffer writer slot."
//!
//! Pure-decision substrate that routes a BSU lifecycle
//! outcome (natural ESU, watchdog timer fire, live-resize
//! override, operator flush, adversarial underflow) into a
//! typed [`EsuPublishDecision`] the integration's
//! buffer-drain + term-layer-apply + triple_buffer.publish
//! pipeline acts on.
//!
//! Splitting the routing decision from the actual publish
//! call means the integration's hot path is one match
//! expression and the routing doctrine is unit-tested
//! against every (trigger, drain-outcome) combination.
//!
//! ## Decision matrix
//!
//! | Trigger                        | Drain bytes | Decision                               |
//! |--------------------------------|-------------|----------------------------------------|
//! | BsuDepthOutcome::Flushed       | > 0         | PublishWithCause(Esu)                  |
//! | BsuDepthOutcome::Flushed       | 0           | SkipPublishEmpty                       |
//! | BsuDepthOutcome::Closed        | any         | NoPublishStillInBsu                    |
//! | BsuDepthOutcome::Underflow     | any         | AdversarialUnderflow                   |
//! | BsuDepthOutcome::Opened        | any         | NoPublishJustOpened                    |
//! | Watchdog FireForceFlush        | > 0         | PublishWithCause(Watchdog)             |
//! | Watchdog FireForceFlush        | 0           | SkipPublishEmpty                       |
//! | Override ForceFlushNow         | > 0         | PublishWithCause(LiveResizeForce)      |
//! | Override ForceFlushNow         | 0           | SkipPublishEmpty                       |
//! | Operator `ft flush`            | > 0         | PublishWithCause(Operator)             |
//! | Operator `ft flush`            | 0           | SkipPublishEmpty                       |

use serde::{Deserialize, Serialize};

use crate::sync_output_buffer_orchestrator::{BufferDrainOutcome, DrainCause};
use crate::sync_output_watchdog::BsuDepthOutcome;

// ============================================================================
// Decision
// ============================================================================

/// Typed routing decision the integration acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EsuPublishDecision {
    /// Drain produced bytes; integration should run the
    /// term-layer state-apply pass on `drained_bytes` then
    /// call `triple_buffer.publish(new_state)`.
    PublishWithCause(DrainCause),
    /// Drain returned zero bytes (NoOp). Integration logs
    /// the no-op via telemetry but does NOT publish a
    /// fresh frame. The triple-buffer's reader continues
    /// to see the prior state.
    SkipPublishEmpty,
    /// `BsuDepthOutcome::Closed { new_depth: 1+ }` —
    /// a nested ESU closed but the BSU is still open.
    /// Integration must NOT publish; the BSU window
    /// continues accumulating.
    NoPublishStillInBsu,
    /// `BsuDepthOutcome::Opened` — the integration just
    /// opened a BSU window. There's nothing to publish at
    /// open-time; the integration arms the watchdog and
    /// keeps accumulating PTY chunks.
    NoPublishJustOpened,
    /// `BsuDepthOutcome::Underflow` — adversarial: the
    /// terminal received an ESU without a matching BSU.
    /// Integration logs to the audit channel and does NOT
    /// publish (no frame to publish; preserve the prior
    /// state).
    AdversarialUnderflow,
}

impl EsuPublishDecision {
    /// True iff the integration should run the term-layer
    /// + publish pipeline.
    #[must_use]
    pub const fn should_publish(self) -> bool {
        matches!(self, Self::PublishWithCause(_))
    }

    /// Drain cause to attribute on the publish, if any.
    #[must_use]
    pub const fn drain_cause(self) -> Option<DrainCause> {
        match self {
            Self::PublishWithCause(cause) => Some(cause),
            Self::SkipPublishEmpty
            | Self::NoPublishStillInBsu
            | Self::NoPublishJustOpened
            | Self::AdversarialUnderflow => None,
        }
    }

    /// Stable label for structured logging.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::PublishWithCause(_) => "publish_with_cause",
            Self::SkipPublishEmpty => "skip_publish_empty",
            Self::NoPublishStillInBsu => "no_publish_still_in_bsu",
            Self::NoPublishJustOpened => "no_publish_just_opened",
            Self::AdversarialUnderflow => "adversarial_underflow",
        }
    }
}

// ============================================================================
// Routers
// ============================================================================

/// Route a depth-outcome + drain-outcome pair to the typed
/// publish decision. This is the natural-ESU path: the
/// integration calls
/// [`BsuDepthCounter::close_esu`](crate::sync_output_watchdog::BsuDepthCounter::close_esu)
/// after parsing the ESU sequence, then drains the buffer,
/// then calls this router with both outcomes.
#[must_use]
pub fn route_esu_publish(
    depth_outcome: BsuDepthOutcome,
    drain_outcome: BufferDrainOutcome,
) -> EsuPublishDecision {
    match depth_outcome {
        BsuDepthOutcome::Opened { .. } => EsuPublishDecision::NoPublishJustOpened,
        BsuDepthOutcome::Closed { .. } => EsuPublishDecision::NoPublishStillInBsu,
        BsuDepthOutcome::Underflow => EsuPublishDecision::AdversarialUnderflow,
        BsuDepthOutcome::Flushed => match drain_outcome {
            BufferDrainOutcome::Drained { cause, .. } => {
                EsuPublishDecision::PublishWithCause(cause)
            }
            BufferDrainOutcome::NoOp => EsuPublishDecision::SkipPublishEmpty,
        },
    }
}

/// Route a force-drain (watchdog timer / live-resize override
/// / operator command) to the typed publish decision. The
/// integration calls this when it just performed a drain
/// driven by the watchdog tick or the override dispatcher,
/// independent of any ESU sequence on the wire.
#[must_use]
pub fn route_force_drain(drain_outcome: BufferDrainOutcome) -> EsuPublishDecision {
    match drain_outcome {
        BufferDrainOutcome::Drained { cause, .. } => {
            EsuPublishDecision::PublishWithCause(cause)
        }
        BufferDrainOutcome::NoOp => EsuPublishDecision::SkipPublishEmpty,
    }
}

// ============================================================================
// Telemetry
// ============================================================================

/// Per-pane publish-routing counters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BsuPublishTelemetry {
    pub publishes_esu: u64,
    pub publishes_watchdog: u64,
    pub publishes_live_resize_force: u64,
    pub publishes_operator: u64,
    pub skip_publish_empty: u64,
    pub no_publish_still_in_bsu: u64,
    pub no_publish_just_opened: u64,
    pub adversarial_underflow: u64,
    /// Total bytes published across all causes — useful
    /// for `ft doctor` size diagnostics.
    pub bytes_published_total: u64,
}

impl BsuPublishTelemetry {
    /// Record one routing decision. `published_bytes` is the
    /// drain's byte count from
    /// `BufferDrainOutcome::Drained { bytes, .. }`; pass 0
    /// for the non-publish variants.
    pub fn record_decision(
        &mut self,
        decision: EsuPublishDecision,
        published_bytes: u64,
    ) {
        match decision {
            EsuPublishDecision::PublishWithCause(cause) => {
                self.bytes_published_total =
                    self.bytes_published_total.saturating_add(published_bytes);
                let slot = match cause {
                    DrainCause::Esu => &mut self.publishes_esu,
                    DrainCause::Watchdog => &mut self.publishes_watchdog,
                    DrainCause::LiveResizeForce => {
                        &mut self.publishes_live_resize_force
                    }
                    DrainCause::Operator => &mut self.publishes_operator,
                };
                *slot = slot.saturating_add(1);
            }
            EsuPublishDecision::SkipPublishEmpty => {
                self.skip_publish_empty = self.skip_publish_empty.saturating_add(1);
            }
            EsuPublishDecision::NoPublishStillInBsu => {
                self.no_publish_still_in_bsu =
                    self.no_publish_still_in_bsu.saturating_add(1);
            }
            EsuPublishDecision::NoPublishJustOpened => {
                self.no_publish_just_opened =
                    self.no_publish_just_opened.saturating_add(1);
            }
            EsuPublishDecision::AdversarialUnderflow => {
                self.adversarial_underflow =
                    self.adversarial_underflow.saturating_add(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drained(bytes: u64, cause: DrainCause) -> BufferDrainOutcome {
        BufferDrainOutcome::Drained { bytes, cause }
    }

    // ----------------------------------------------------------------
    // EsuPublishDecision predicates
    // ----------------------------------------------------------------

    #[test]
    fn should_publish_only_for_publish_with_cause() {
        assert!(EsuPublishDecision::PublishWithCause(DrainCause::Esu).should_publish());
        assert!(!EsuPublishDecision::SkipPublishEmpty.should_publish());
        assert!(!EsuPublishDecision::NoPublishStillInBsu.should_publish());
        assert!(!EsuPublishDecision::NoPublishJustOpened.should_publish());
        assert!(!EsuPublishDecision::AdversarialUnderflow.should_publish());
    }

    #[test]
    fn drain_cause_some_only_for_publish_with_cause() {
        assert_eq!(
            EsuPublishDecision::PublishWithCause(DrainCause::Watchdog).drain_cause(),
            Some(DrainCause::Watchdog)
        );
        assert_eq!(EsuPublishDecision::SkipPublishEmpty.drain_cause(), None);
        assert_eq!(EsuPublishDecision::NoPublishStillInBsu.drain_cause(), None);
        assert_eq!(EsuPublishDecision::NoPublishJustOpened.drain_cause(), None);
        assert_eq!(EsuPublishDecision::AdversarialUnderflow.drain_cause(), None);
    }

    #[test]
    fn label_is_stable_and_unique() {
        let labels = [
            EsuPublishDecision::PublishWithCause(DrainCause::Esu).label(),
            EsuPublishDecision::SkipPublishEmpty.label(),
            EsuPublishDecision::NoPublishStillInBsu.label(),
            EsuPublishDecision::NoPublishJustOpened.label(),
            EsuPublishDecision::AdversarialUnderflow.label(),
        ];
        let unique: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len());
    }

    // ----------------------------------------------------------------
    // route_esu_publish
    // ----------------------------------------------------------------

    #[test]
    fn route_flushed_with_drained_bytes_publishes_esu() {
        let dec = route_esu_publish(
            BsuDepthOutcome::Flushed,
            drained(1024, DrainCause::Esu),
        );
        assert_eq!(dec, EsuPublishDecision::PublishWithCause(DrainCause::Esu));
    }

    #[test]
    fn route_flushed_with_noop_drain_skips_publish() {
        let dec = route_esu_publish(BsuDepthOutcome::Flushed, BufferDrainOutcome::NoOp);
        assert_eq!(dec, EsuPublishDecision::SkipPublishEmpty);
    }

    #[test]
    fn route_closed_inner_esu_does_not_publish() {
        let dec = route_esu_publish(
            BsuDepthOutcome::Closed { new_depth: 1 },
            drained(512, DrainCause::Esu),
        );
        assert_eq!(dec, EsuPublishDecision::NoPublishStillInBsu);
    }

    #[test]
    fn route_opened_does_not_publish() {
        let dec = route_esu_publish(
            BsuDepthOutcome::Opened { new_depth: 1 },
            BufferDrainOutcome::NoOp,
        );
        assert_eq!(dec, EsuPublishDecision::NoPublishJustOpened);
    }

    #[test]
    fn route_underflow_marks_adversarial() {
        let dec = route_esu_publish(
            BsuDepthOutcome::Underflow,
            BufferDrainOutcome::NoOp,
        );
        assert_eq!(dec, EsuPublishDecision::AdversarialUnderflow);
    }

    #[test]
    fn route_underflow_marks_adversarial_even_if_drain_returned_bytes() {
        // Defensive: if for some reason drain returned bytes
        // alongside an underflow signal, the substrate should
        // STILL route to AdversarialUnderflow — the depth
        // outcome takes priority because it indicates the
        // protocol-level violation.
        let dec = route_esu_publish(
            BsuDepthOutcome::Underflow,
            drained(100, DrainCause::Esu),
        );
        assert_eq!(dec, EsuPublishDecision::AdversarialUnderflow);
    }

    // ----------------------------------------------------------------
    // route_force_drain
    // ----------------------------------------------------------------

    #[test]
    fn route_force_drain_watchdog_publishes_with_watchdog_cause() {
        let dec = route_force_drain(drained(2048, DrainCause::Watchdog));
        assert_eq!(
            dec,
            EsuPublishDecision::PublishWithCause(DrainCause::Watchdog)
        );
    }

    #[test]
    fn route_force_drain_live_resize_publishes_with_live_resize_cause() {
        let dec = route_force_drain(drained(4096, DrainCause::LiveResizeForce));
        assert_eq!(
            dec,
            EsuPublishDecision::PublishWithCause(DrainCause::LiveResizeForce)
        );
    }

    #[test]
    fn route_force_drain_operator_publishes_with_operator_cause() {
        let dec = route_force_drain(drained(128, DrainCause::Operator));
        assert_eq!(
            dec,
            EsuPublishDecision::PublishWithCause(DrainCause::Operator)
        );
    }

    #[test]
    fn route_force_drain_noop_skips_publish() {
        let dec = route_force_drain(BufferDrainOutcome::NoOp);
        assert_eq!(dec, EsuPublishDecision::SkipPublishEmpty);
    }

    // ----------------------------------------------------------------
    // BsuPublishTelemetry
    // ----------------------------------------------------------------

    #[test]
    fn telemetry_records_publish_with_cause_in_correct_slot() {
        let mut t = BsuPublishTelemetry::default();
        t.record_decision(
            EsuPublishDecision::PublishWithCause(DrainCause::Esu),
            1024,
        );
        t.record_decision(
            EsuPublishDecision::PublishWithCause(DrainCause::Watchdog),
            512,
        );
        t.record_decision(
            EsuPublishDecision::PublishWithCause(DrainCause::LiveResizeForce),
            256,
        );
        t.record_decision(
            EsuPublishDecision::PublishWithCause(DrainCause::Operator),
            128,
        );
        assert_eq!(t.publishes_esu, 1);
        assert_eq!(t.publishes_watchdog, 1);
        assert_eq!(t.publishes_live_resize_force, 1);
        assert_eq!(t.publishes_operator, 1);
        assert_eq!(t.bytes_published_total, 1024 + 512 + 256 + 128);
    }

    #[test]
    fn telemetry_records_each_non_publish_variant() {
        let mut t = BsuPublishTelemetry::default();
        t.record_decision(EsuPublishDecision::SkipPublishEmpty, 0);
        t.record_decision(EsuPublishDecision::NoPublishStillInBsu, 0);
        t.record_decision(EsuPublishDecision::NoPublishJustOpened, 0);
        t.record_decision(EsuPublishDecision::AdversarialUnderflow, 0);
        assert_eq!(t.skip_publish_empty, 1);
        assert_eq!(t.no_publish_still_in_bsu, 1);
        assert_eq!(t.no_publish_just_opened, 1);
        assert_eq!(t.adversarial_underflow, 1);
        assert_eq!(t.bytes_published_total, 0);
    }

    #[test]
    fn telemetry_serde_roundtrip() {
        let mut t = BsuPublishTelemetry::default();
        t.record_decision(
            EsuPublishDecision::PublishWithCause(DrainCause::Esu),
            500,
        );
        t.record_decision(EsuPublishDecision::AdversarialUnderflow, 0);
        let json = serde_json::to_string(&t).unwrap();
        let back: BsuPublishTelemetry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, t);
    }

    // ----------------------------------------------------------------
    // End-to-end: clean BSU lifecycle
    // ----------------------------------------------------------------

    #[test]
    fn scenario_clean_bsu_open_then_natural_esu_publishes_once() {
        let mut t = BsuPublishTelemetry::default();

        // Open BSU.
        let dec_open = route_esu_publish(
            BsuDepthOutcome::Opened { new_depth: 1 },
            BufferDrainOutcome::NoOp,
        );
        t.record_decision(dec_open, 0);
        assert_eq!(dec_open, EsuPublishDecision::NoPublishJustOpened);

        // Natural ESU drain at depth=1 → 0 → Flushed with bytes.
        let dec_esu = route_esu_publish(
            BsuDepthOutcome::Flushed,
            drained(2048, DrainCause::Esu),
        );
        t.record_decision(dec_esu, 2048);
        assert_eq!(dec_esu, EsuPublishDecision::PublishWithCause(DrainCause::Esu));

        assert_eq!(t.publishes_esu, 1);
        assert_eq!(t.no_publish_just_opened, 1);
        assert_eq!(t.bytes_published_total, 2048);
    }

    #[test]
    fn scenario_watchdog_force_flush_then_natural_esu_after_reset() {
        // Watchdog fires, force-drains the buffer, then the
        // app eventually closes its ESU. After the watchdog
        // force-flush + force_reset, the depth counter is at
        // 0 → next close_esu yields Underflow (adversarial:
        // the app sent ESU bytes for a BSU that was already
        // recycled).
        let mut t = BsuPublishTelemetry::default();

        let dec_watchdog = route_force_drain(drained(4096, DrainCause::Watchdog));
        t.record_decision(dec_watchdog, 4096);
        assert_eq!(
            dec_watchdog,
            EsuPublishDecision::PublishWithCause(DrainCause::Watchdog)
        );

        let dec_underflow = route_esu_publish(
            BsuDepthOutcome::Underflow,
            BufferDrainOutcome::NoOp,
        );
        t.record_decision(dec_underflow, 0);
        assert_eq!(dec_underflow, EsuPublishDecision::AdversarialUnderflow);

        assert_eq!(t.publishes_watchdog, 1);
        assert_eq!(t.adversarial_underflow, 1);
        assert_eq!(t.bytes_published_total, 4096);
    }

    #[test]
    fn scenario_nested_bsu_inner_esu_does_not_publish() {
        // Outer BSU at depth=0→1, inner BSU at depth=1→2.
        // Inner ESU at depth=2→1: Closed { new_depth: 1 } —
        // do NOT publish. Outer ESU at depth=1→0: Flushed —
        // publish.
        let mut t = BsuPublishTelemetry::default();

        let outer_open = route_esu_publish(
            BsuDepthOutcome::Opened { new_depth: 1 },
            BufferDrainOutcome::NoOp,
        );
        t.record_decision(outer_open, 0);

        let inner_open = route_esu_publish(
            BsuDepthOutcome::Opened { new_depth: 2 },
            BufferDrainOutcome::NoOp,
        );
        t.record_decision(inner_open, 0);

        // Inner ESU.
        let inner_close = route_esu_publish(
            BsuDepthOutcome::Closed { new_depth: 1 },
            BufferDrainOutcome::NoOp,
        );
        t.record_decision(inner_close, 0);
        assert_eq!(inner_close, EsuPublishDecision::NoPublishStillInBsu);

        // Outer ESU drains the accumulated bytes.
        let outer_close = route_esu_publish(
            BsuDepthOutcome::Flushed,
            drained(8192, DrainCause::Esu),
        );
        t.record_decision(outer_close, 8192);
        assert_eq!(
            outer_close,
            EsuPublishDecision::PublishWithCause(DrainCause::Esu)
        );

        assert_eq!(t.publishes_esu, 1);
        assert_eq!(t.no_publish_still_in_bsu, 1);
        assert_eq!(t.no_publish_just_opened, 2);
    }
}
