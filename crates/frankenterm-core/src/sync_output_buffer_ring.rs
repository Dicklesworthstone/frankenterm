//! Stateful BSU ring buffer using the orchestrator's policy
//! decisions (ft-1dq8h).
//!
//! The pure-policy substrate `sync_output_buffer_orchestrator`
//! ships the admission decision tree but doesn't carry any
//! bytes. This module ships the **stateful** ring buffer the
//! integration uses inside the per-pane BSU dispatch:
//!
//! - Holds bytes accumulated across the BSU window.
//! - Calls `evaluate_buffer_admission` from the orchestrator
//!   substrate on every PTY chunk.
//! - Implements the truncation behaviour the policy decides
//!   (drop oldest bytes, append new bytes).
//! - Drains atomically into a `Vec<u8>` at ESU.
//!
//! Cross-link: when ESU fires, integration's term-layer
//! consumes `drain_for_publish` and submits the drained bytes
//! to the triple-buffer's writer slot.
//!
//! ## What this module ships
//!
//! - [`BsuRingBuffer`] — `Vec<u8>`-backed buffer with the
//!   orchestrator's admission policy enforced on every push.
//! - [`PushOutcome`] mirrors `BufferAdmissionDecision` with
//!   the additional `bytes_appended` count for the integration's
//!   `mid_bsu_byte_count` telemetry counter.
//! - [`drain_for_publish`] — atomic-take of buffered bytes
//!   for ESU dispatch. Caller gets `Vec<u8>`; buffer empties.
//! - [`drained_bytes`] / [`buffered_bytes`] inspection
//!   getters for ft doctor.
//!
//! ## What is deferred to ft-1dq8h follow-up
//!
//! - asupersync timer wiring.
//! - PTY mode-query response emission.
//! - Per-pane override dispatcher (BEL / cursor / live-resize)
//!   — the existing orchestrator's `evaluate_override` covers
//!   the policy; integration calls into the right code paths.
//! - Triple-buffer publish at ESU drain.

#![allow(dead_code)]

use crate::sync_output_buffer_orchestrator::{
    evaluate_buffer_admission, BsuBufferConfig, BufferAdmissionDecision,
};

// ============================================================================
// Push outcome (extends BufferAdmissionDecision with bytes_appended)
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PushOutcome {
    /// All `bytes_appended` bytes appended to the buffer.
    Accepted { bytes_appended: u64 },
    /// `bytes_dropped` of the oldest buffered bytes dropped to
    /// make room; then `bytes_appended` bytes appended.
    Truncated {
        bytes_dropped: u64,
        bytes_appended: u64,
    },
    /// Substrate refused the push (incoming exceeds cap or
    /// truncate disabled). Buffer unchanged.
    Refused,
}

impl PushOutcome {
    /// Whether the push left bytes in the buffer.
    #[must_use]
    pub fn is_admitted(self) -> bool {
        matches!(self, Self::Accepted { .. } | Self::Truncated { .. })
    }

    /// Convert to the policy substrate's enum (without the
    /// per-step byte-count payload).
    #[must_use]
    pub fn as_admission_decision(self) -> BufferAdmissionDecision {
        match self {
            Self::Accepted { .. } => BufferAdmissionDecision::Accepted,
            Self::Truncated { bytes_dropped, .. } => BufferAdmissionDecision::Truncated {
                dropped_bytes: bytes_dropped,
            },
            Self::Refused => BufferAdmissionDecision::Refused,
        }
    }
}

// ============================================================================
// BsuRingBuffer
// ============================================================================

/// Stateful BSU buffer. Per-pane; one instance per active BSU
/// window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BsuRingBuffer {
    bytes: Vec<u8>,
    config: BsuBufferConfig,
}

impl BsuRingBuffer {
    /// Construct an empty buffer with the given config.
    #[must_use]
    pub fn new(config: BsuBufferConfig) -> Self {
        Self {
            bytes: Vec::new(),
            config,
        }
    }

    /// Construct with `capacity` pre-reserved bytes.
    #[must_use]
    pub fn with_initial_capacity(config: BsuBufferConfig, capacity: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(capacity),
            config,
        }
    }

    /// Currently buffered bytes (for inspection / ft doctor).
    #[must_use]
    pub fn buffered_bytes(&self) -> u64 {
        self.bytes.len() as u64
    }

    /// Buffer capacity per the substrate's effective cap.
    #[must_use]
    pub fn cap(&self) -> u64 {
        self.config.effective_max_bytes()
    }

    /// Whether the buffer holds any bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Operator-visible config.
    #[must_use]
    pub fn config(&self) -> BsuBufferConfig {
        self.config
    }

    /// Read-only view of the buffered bytes. The integration
    /// should only call this for `ft doctor` inspection, NOT
    /// to feed into a parser — use [`Self::drain_for_publish`]
    /// for that so the buffer empties atomically.
    #[must_use]
    pub fn peek_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Push a PTY chunk. Calls the orchestrator's policy
    /// substrate to classify, then mutates the buffer to match
    /// the decision.
    pub fn push(&mut self, chunk: &[u8]) -> PushOutcome {
        let incoming = chunk.len() as u64;
        let decision = evaluate_buffer_admission(self.buffered_bytes(), incoming, self.config);
        match decision {
            BufferAdmissionDecision::Accepted => {
                self.bytes.extend_from_slice(chunk);
                PushOutcome::Accepted { bytes_appended: incoming }
            }
            BufferAdmissionDecision::Truncated { dropped_bytes } => {
                // Drop `dropped_bytes` from the front of the
                // buffer (the oldest bytes), then append the
                // incoming chunk. Saturating cast for the
                // theoretically-impossible case where
                // dropped_bytes > usize::MAX.
                let drop_count = dropped_bytes.min(self.bytes.len() as u64) as usize;
                self.bytes.drain(..drop_count);
                self.bytes.extend_from_slice(chunk);
                PushOutcome::Truncated {
                    bytes_dropped: dropped_bytes,
                    bytes_appended: incoming,
                }
            }
            BufferAdmissionDecision::Refused => PushOutcome::Refused,
        }
    }

    /// Atomic drain at ESU. Caller takes the buffered bytes;
    /// buffer empties. Returns the drained bytes.
    pub fn drain_for_publish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.bytes)
    }

    /// Reconfigure the buffer in-place. The new config takes
    /// effect on subsequent pushes; existing buffered bytes
    /// stay (even if they would now exceed the new cap — the
    /// next push will trigger truncation).
    pub fn reconfigure(&mut self, config: BsuBufferConfig) {
        self.config = config;
    }

    /// Force-clear the buffer without publishing (e.g., on
    /// pane close or live-resize abort). Returns the count
    /// of bytes discarded.
    pub fn force_clear(&mut self) -> u64 {
        let count = self.bytes.len() as u64;
        self.bytes.clear();
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(cap: u64) -> BsuBufferConfig {
        BsuBufferConfig {
            per_pane_max_bytes: cap,
            min_bytes: 1,
            truncate_when_full: true,
        }
    }

    fn cfg_no_truncate(cap: u64) -> BsuBufferConfig {
        BsuBufferConfig {
            per_pane_max_bytes: cap,
            min_bytes: 1,
            truncate_when_full: false,
        }
    }

    // ----------------------------------------------------------------
    // Construction
    // ----------------------------------------------------------------

    #[test]
    fn buffer_starts_empty() {
        let b = BsuRingBuffer::new(cfg(1024));
        assert!(b.is_empty());
        assert_eq!(b.buffered_bytes(), 0);
        assert_eq!(b.cap(), 1024);
    }

    #[test]
    fn with_initial_capacity_starts_empty() {
        let b = BsuRingBuffer::with_initial_capacity(cfg(1024), 512);
        assert!(b.is_empty());
        // peek_bytes returns empty even if Vec has reserved
        // capacity.
        assert!(b.peek_bytes().is_empty());
    }

    // ----------------------------------------------------------------
    // push — acceptance path
    // ----------------------------------------------------------------

    #[test]
    fn push_accepted_under_cap() {
        let mut b = BsuRingBuffer::new(cfg(1000));
        let outcome = b.push(b"hello");
        assert_eq!(outcome, PushOutcome::Accepted { bytes_appended: 5 });
        assert_eq!(b.buffered_bytes(), 5);
        assert_eq!(b.peek_bytes(), b"hello");
    }

    #[test]
    fn push_accepted_at_exact_cap() {
        let mut b = BsuRingBuffer::new(cfg(5));
        let outcome = b.push(b"hello");
        assert_eq!(outcome, PushOutcome::Accepted { bytes_appended: 5 });
        assert_eq!(b.buffered_bytes(), 5);
    }

    #[test]
    fn push_multiple_chunks_accumulate() {
        let mut b = BsuRingBuffer::new(cfg(1000));
        b.push(b"hello ");
        b.push(b"world");
        assert_eq!(b.peek_bytes(), b"hello world");
        assert_eq!(b.buffered_bytes(), 11);
    }

    // ----------------------------------------------------------------
    // push — truncation path
    // ----------------------------------------------------------------

    #[test]
    fn push_truncates_oldest_when_over_cap() {
        let mut b = BsuRingBuffer::new(cfg(10));
        b.push(b"0123456789"); // fills to cap
        // Incoming 5 bytes; need to drop 5 from front.
        let outcome = b.push(b"ABCDE");
        assert_eq!(
            outcome,
            PushOutcome::Truncated {
                bytes_dropped: 5,
                bytes_appended: 5,
            },
        );
        // Front 5 dropped, last 5 of original + ABCDE.
        assert_eq!(b.peek_bytes(), b"56789ABCDE");
        assert_eq!(b.buffered_bytes(), 10);
    }

    #[test]
    fn push_partial_truncation_when_some_room_remains() {
        let mut b = BsuRingBuffer::new(cfg(10));
        b.push(b"01234"); // 5 bytes used
        // Push 8 more — total 13, overflow 3.
        let outcome = b.push(b"ABCDEFGH");
        assert_eq!(
            outcome,
            PushOutcome::Truncated {
                bytes_dropped: 3,
                bytes_appended: 8,
            },
        );
        // Front 3 ("012") dropped, "34" + "ABCDEFGH" = "34ABCDEFGH".
        assert_eq!(b.peek_bytes(), b"34ABCDEFGH");
    }

    #[test]
    fn push_truncated_buffer_byte_count_exactly_cap() {
        // After truncation the buffer should hold exactly `cap`
        // bytes (no overflow, no underflow).
        let mut b = BsuRingBuffer::new(cfg(10));
        b.push(b"0123456789");
        b.push(b"ABCDE");
        assert_eq!(b.buffered_bytes(), 10);
    }

    // ----------------------------------------------------------------
    // push — refused path
    // ----------------------------------------------------------------

    #[test]
    fn push_refused_when_incoming_exceeds_cap_with_truncate_on() {
        let mut b = BsuRingBuffer::new(cfg(10));
        b.push(b"hello"); // 5 bytes
        // Incoming 20 bytes > cap 10 — substrate refuses.
        let outcome = b.push(&[b'A'; 20]);
        assert_eq!(outcome, PushOutcome::Refused);
        // Buffer unchanged.
        assert_eq!(b.peek_bytes(), b"hello");
    }

    #[test]
    fn push_refused_when_truncate_disabled_and_full() {
        let mut b = BsuRingBuffer::new(cfg_no_truncate(10));
        b.push(b"0123456789"); // fills to cap
        // Truncate disabled — substrate refuses incoming.
        let outcome = b.push(b"ABC");
        assert_eq!(outcome, PushOutcome::Refused);
        assert_eq!(b.peek_bytes(), b"0123456789");
    }

    #[test]
    fn push_refused_outcome_predicate_returns_false_for_admitted() {
        assert!(!PushOutcome::Refused.is_admitted());
        assert!(PushOutcome::Accepted { bytes_appended: 10 }.is_admitted());
        assert!(PushOutcome::Truncated { bytes_dropped: 5, bytes_appended: 10 }.is_admitted());
    }

    #[test]
    fn push_outcome_converts_to_policy_decision() {
        let acc = PushOutcome::Accepted { bytes_appended: 100 };
        assert_eq!(acc.as_admission_decision(), BufferAdmissionDecision::Accepted);

        let trunc = PushOutcome::Truncated { bytes_dropped: 5, bytes_appended: 10 };
        assert_eq!(
            trunc.as_admission_decision(),
            BufferAdmissionDecision::Truncated { dropped_bytes: 5 },
        );

        let refused = PushOutcome::Refused;
        assert_eq!(refused.as_admission_decision(), BufferAdmissionDecision::Refused);
    }

    // ----------------------------------------------------------------
    // drain_for_publish
    // ----------------------------------------------------------------

    #[test]
    fn drain_for_publish_returns_buffered_bytes_and_empties() {
        let mut b = BsuRingBuffer::new(cfg(1000));
        b.push(b"hello world");
        let drained = b.drain_for_publish();
        assert_eq!(drained, b"hello world");
        assert!(b.is_empty());
        assert_eq!(b.buffered_bytes(), 0);
    }

    #[test]
    fn drain_for_publish_empty_returns_empty_vec() {
        let mut b = BsuRingBuffer::new(cfg(1000));
        let drained = b.drain_for_publish();
        assert!(drained.is_empty());
    }

    #[test]
    fn drain_then_push_works_normally() {
        let mut b = BsuRingBuffer::new(cfg(1000));
        b.push(b"first");
        let _ = b.drain_for_publish();
        b.push(b"second");
        assert_eq!(b.peek_bytes(), b"second");
    }

    // ----------------------------------------------------------------
    // reconfigure
    // ----------------------------------------------------------------

    #[test]
    fn reconfigure_changes_cap_for_subsequent_pushes() {
        let mut b = BsuRingBuffer::new(cfg(10));
        b.push(b"hello"); // 5 bytes
        // Tighten cap to 8.
        b.reconfigure(cfg(8));
        // Buffer still has 5 bytes; push 6 more — 11 > 8,
        // overflow 3.
        let outcome = b.push(b"ABCDEF");
        assert_eq!(
            outcome,
            PushOutcome::Truncated {
                bytes_dropped: 3,
                bytes_appended: 6,
            },
        );
        assert_eq!(b.buffered_bytes(), 8);
    }

    #[test]
    fn reconfigure_does_not_shrink_buffer_immediately() {
        // Reconfigure to a smaller cap doesn't auto-truncate.
        // Truncation only fires on next push.
        let mut b = BsuRingBuffer::new(cfg(100));
        b.push(b"0123456789012345678901234567890"); // 31 bytes
        b.reconfigure(cfg(10));
        // Buffer still has 31 bytes (over the new cap).
        assert_eq!(b.buffered_bytes(), 31);
        // Next push triggers truncation.
        let outcome = b.push(b"X");
        assert!(matches!(outcome, PushOutcome::Truncated { .. }));
    }

    // ----------------------------------------------------------------
    // force_clear
    // ----------------------------------------------------------------

    #[test]
    fn force_clear_returns_byte_count_and_empties() {
        let mut b = BsuRingBuffer::new(cfg(1000));
        b.push(b"hello world"); // 11 bytes
        let count = b.force_clear();
        assert_eq!(count, 11);
        assert!(b.is_empty());
    }

    #[test]
    fn force_clear_empty_returns_zero() {
        let mut b = BsuRingBuffer::new(cfg(1000));
        let count = b.force_clear();
        assert_eq!(count, 0);
    }

    // ----------------------------------------------------------------
    // Cross-cut scenarios
    // ----------------------------------------------------------------

    #[test]
    fn scenario_neovim_treesitter_bsu_window() {
        // Realistic Neovim treesitter redraw: 30 ms window
        // emits ~50 KiB across multiple PTY reads.
        let mut b = BsuRingBuffer::new(BsuBufferConfig::default());
        for chunk_size in [10_000, 20_000, 15_000, 5_000] {
            let chunk = vec![b'X'; chunk_size];
            let outcome = b.push(&chunk);
            assert_eq!(
                outcome,
                PushOutcome::Accepted {
                    bytes_appended: chunk_size as u64,
                },
            );
        }
        assert_eq!(b.buffered_bytes(), 50_000);
        let drained = b.drain_for_publish();
        assert_eq!(drained.len(), 50_000);
    }

    #[test]
    fn scenario_runaway_app_truncates_to_cap() {
        // App keeps writing past cap; substrate truncates,
        // never lets buffer exceed cap.
        let mut b = BsuRingBuffer::new(cfg(1024));
        for i in 0..10 {
            let chunk = vec![i as u8; 200]; // 200 bytes each
            b.push(&chunk);
        }
        // Buffer should be exactly cap.
        assert_eq!(b.buffered_bytes(), 1024);
    }

    #[test]
    fn scenario_pane_close_force_clear_drops_bytes() {
        // Pane closes mid-BSU; integration force-clears.
        let mut b = BsuRingBuffer::new(cfg(1000));
        b.push(b"in-flight bsu data");
        let dropped = b.force_clear();
        assert_eq!(dropped, 18);
        assert!(b.is_empty());
    }

    #[test]
    fn scenario_live_resize_drains_immediately() {
        // Live-resize forces an immediate drain mid-BSU.
        // Substrate's drain_for_publish atomically takes the
        // bytes; integration ships them to the triple-buffer.
        let mut b = BsuRingBuffer::new(cfg(1000));
        b.push(b"frame in progress");
        let frame = b.drain_for_publish();
        assert_eq!(frame, b"frame in progress");
        assert!(b.is_empty());
        // Subsequent push lands on a fresh buffer.
        b.push(b"resize complete");
        assert_eq!(b.peek_bytes(), b"resize complete");
    }
}
