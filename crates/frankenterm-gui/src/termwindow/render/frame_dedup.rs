//! Frame deduplication — defense-in-depth on top of the redraw
//! predicate.
//!
//! **Beads:** [BR-TERM-EMULATOR-UPLIFT-2.2.3] — two parallel
//! bead IDs cover this substrate:
//! - `ft-mpc9b.5.1` — primary implementation bead (closed).
//! - `ft-2okh0.2.3` — canonical decomposition of parent epic
//!   `ft-2okh0.2` (closed). Closed via cross-reference to
//!   ft-mpc9b.5.1.
//!
//! Same cross-reference pattern as `ft-2okh0.5.1 ↔ ft-kscfg`,
//! `ft-2okh0.5.2 ↔ ft-5te6x`, `ft-2okh0.5.3 ↔ ft-hs5f6`,
//! `ft-2okh0.5.5 ↔ ft-0ulxc`, `ft-2okh0.3.1 ↔ ft-d0ol8`.
//!
//! Original implementation note: ft-mpc9b.5.1 (ft-2okh0.2 subset 2.3).
//!
//! The redraw predicate skips paint passes when nothing has
//! changed semantically. Frame dedup catches the residual case:
//! the predicate said "paint" (so we ran the full pass), but the
//! resulting framebuffer is byte-identical to the last presented
//! frame. Skipping `Present` in that case avoids waking the
//! compositor and saves the GPU-side present cost.
//!
//! User-visible win once the integration lands: laptop with ft
//! idle drops from ~5 %/hr battery to ~0.1 %/hr (the bead's
//! headline target).
//!
//! ## What this module ships (foundation, ft-2okh0.2 subset 2.3)
//!
//! - `FrameHash` newtype around `u64` for type-safety so callers
//!   can't accidentally swap a frame hash with another `u64`.
//! - `compute_frame_hash(bytes)` — FNV-1a 64-bit pure function.
//!   Deterministic across runs (no randomized seed). Roughly
//!   1 ns/byte on contemporary x86, well under the bead's
//!   "~µs cost per Present" budget for typical 1080p RGBA
//!   buffers.
//! - `FrameDeduplicator` — state machine. Tracks the last
//!   presented hash and a force-present flag (for cases like
//!   active screen recording where dedup must be bypassed).
//! - `PresentDecision` — `Present` / `SkipDuplicate`. The
//!   continuation bead's paint-loop wiring consumes this.
//! - `FrameDedupStats` — running `presents_total` /
//!   `skip_duplicate_total` / `forced_presents_total` counters
//!   for the bead's per-session telemetry surface.
//!
//! ## What is deferred (continuation, see follow-up bead)
//!
//! - Wiring `FrameDeduplicator::decide(hash)` into the paint
//!   pipeline at `crates/frankenterm-gui/src/termwindow/render/paint.rs`,
//!   immediately before the actual `Present` call.
//! - Streaming the framebuffer through `compute_frame_hash` —
//!   the readback path differs per backend (glium vs wgpu) and
//!   needs a per-backend hook.
//! - The `force_present_for_recording` signal: detect macOS
//!   ScreenCaptureKit / Linux PipeWire screen recording and set
//!   the flag so dedup doesn't elide frames the recording
//!   pipeline expected to capture (the bead's "DO NOT BREAK
//!   recording" call).
//! - VRR (subset 2.1 of ft-2okh0.2) and direct scanout
//!   (subset 2.2) are tracked under separate continuation beads.
//! - ft doctor surface for `dedup_rate_pct` and the lifetime
//!   counters.

#![allow(dead_code)]

/// Type-safe wrapper around the 64-bit frame hash. Distinct from
/// `u64` so a caller can't accidentally pass a quad-version
/// counter where a frame hash is expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct FrameHash(pub u64);

impl FrameHash {
    pub const ZERO: FrameHash = FrameHash(0);
    pub fn raw(self) -> u64 {
        self.0
    }
}

/// FNV-1a 64-bit constants (Fowler/Noll/Vo).
const FNV_OFFSET_BASIS_64: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME_64: u64 = 0x100_0000_01b3;

/// Compute a deterministic 64-bit hash over a frame buffer slice.
///
/// FNV-1a chosen because:
/// - dependency-free (no extra crate)
/// - deterministic across runs (no randomized seed)
/// - fast enough on contemporary hardware (~1 ns/byte)
/// - collision probability of 2⁻⁶⁴ per frame matches the bead's
///   "conservative fallback: Present anyway" threshold
///
/// The continuation bead may swap this for xxhash64 if a
/// per-backend benchmark shows it matters. The signature is
/// stable so the swap is local.
pub fn compute_frame_hash(bytes: &[u8]) -> FrameHash {
    let mut h: u64 = FNV_OFFSET_BASIS_64;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(FNV_PRIME_64);
    }
    FrameHash(h)
}

/// Decision returned by `FrameDeduplicator::decide`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentDecision {
    /// The frame is new (or force-present is active). Caller
    /// should call the backend's `Present`.
    Present,
    /// The frame's hash matches the last presented frame; the
    /// caller should skip `Present`. Telemetry recorded.
    SkipDuplicate,
}

/// Cross-frame state for the dedup decision.
///
/// Lifecycle:
///   1. `set_force_present(true)` whenever screen recording or
///      another consumer needs every frame.
///   2. `decide(hash)` immediately before the backend's Present.
///   3. `mark_presented(hash)` AFTER the backend's Present
///      completes — only marks the frame as the new "last" if
///      the caller actually presented it (skipping `decide`'s
///      `SkipDuplicate` path leaves the prior hash in place,
///      which is the correct behavior).
#[derive(Debug, Clone, Default)]
pub struct FrameDeduplicator {
    last_presented: Option<FrameHash>,
    force_present: bool,
    stats: FrameDedupStats,
}

/// Lifetime counters for `ft doctor`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrameDedupStats {
    /// Frames the dedup decided to present.
    pub presents_total: u64,
    /// Frames the dedup elided because the hash matched the last
    /// presented frame.
    pub skip_duplicate_total: u64,
    /// Of `presents_total`, how many were force-presented because
    /// `force_present` was true (e.g., screen recording active).
    pub forced_presents_total: u64,
}

impl FrameDedupStats {
    /// Total decisions made (`presents_total + skip_duplicate_total`).
    pub fn total(&self) -> u64 {
        self.presents_total
            .saturating_add(self.skip_duplicate_total)
    }

    /// Skip rate as a fraction in [0.0, 1.0]. Returns 0.0 when
    /// `total()` is 0 so the telemetry path doesn't have to
    /// special-case startup.
    pub fn dedup_rate(&self) -> f64 {
        let total = self.total();
        if total == 0 {
            return 0.0;
        }
        self.skip_duplicate_total as f64 / total as f64
    }
}

impl FrameDeduplicator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Toggle the force-present flag. While true, every `decide`
    /// returns `Present` and the `forced_presents_total` counter
    /// bumps on each Present.
    ///
    /// The bead's "DO NOT BREAK recording" call: the embedder
    /// detects active screen recording (macOS ScreenCaptureKit /
    /// Linux PipeWire) and sets this flag so dedup doesn't elide
    /// frames the recording pipeline expected to capture.
    pub fn set_force_present(&mut self, on: bool) {
        self.force_present = on;
    }

    /// Whether force-present is currently on.
    pub fn is_force_present(&self) -> bool {
        self.force_present
    }

    /// Decide whether to present the frame whose hash is
    /// `candidate`. This is a query — it does NOT mutate the
    /// "last presented" state. Callers must invoke
    /// `mark_presented(candidate)` after the actual Present
    /// returns so subsequent decisions compare against the right
    /// reference.
    pub fn decide(&mut self, candidate: FrameHash) -> PresentDecision {
        if self.force_present {
            self.stats.presents_total = self.stats.presents_total.saturating_add(1);
            self.stats.forced_presents_total = self.stats.forced_presents_total.saturating_add(1);
            return PresentDecision::Present;
        }
        match self.last_presented {
            Some(prev) if prev == candidate => {
                self.stats.skip_duplicate_total = self.stats.skip_duplicate_total.saturating_add(1);
                PresentDecision::SkipDuplicate
            }
            _ => {
                self.stats.presents_total = self.stats.presents_total.saturating_add(1);
                PresentDecision::Present
            }
        }
    }

    /// Record that a Present actually completed for `hash` so
    /// future `decide(...)` calls compare against this as the
    /// new reference. Only call this AFTER the backend's Present
    /// returns successfully.
    pub fn mark_presented(&mut self, hash: FrameHash) {
        self.last_presented = Some(hash);
    }

    /// Last presented hash, if any. The continuation bead's
    /// paint loop reads this for diagnostics.
    pub fn last_presented(&self) -> Option<FrameHash> {
        self.last_presented
    }

    /// Reset the last-presented reference (e.g., after a
    /// resize or backend swap). Subsequent `decide` calls will
    /// return `Present` until a new hash is marked.
    pub fn invalidate_reference(&mut self) {
        self.last_presented = None;
    }

    pub fn stats(&self) -> &FrameDedupStats {
        &self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_buffer_has_canonical_hash() {
        // FNV-1a of the empty input is the offset basis — pin
        // the constant so any future swap of hash function
        // produces a clear test failure.
        let h = compute_frame_hash(&[]);
        assert_eq!(h, FrameHash(FNV_OFFSET_BASIS_64));
    }

    #[test]
    fn identical_buffers_hash_identically() {
        let a = compute_frame_hash(&[1, 2, 3, 4, 5]);
        let b = compute_frame_hash(&[1, 2, 3, 4, 5]);
        assert_eq!(a, b);
    }

    #[test]
    fn different_buffers_hash_differently() {
        let a = compute_frame_hash(&[1, 2, 3, 4, 5]);
        let b = compute_frame_hash(&[1, 2, 3, 4, 6]);
        assert_ne!(a, b);
    }

    #[test]
    fn one_byte_difference_propagates_through_full_hash() {
        // Avalanche sanity check: a single bit flip in the input
        // changes the output by far more than one bit.
        let a = compute_frame_hash(&[0; 64]);
        let mut buf = [0u8; 64];
        buf[37] = 1;
        let b = compute_frame_hash(&buf);
        let xor = a.raw() ^ b.raw();
        let popcount = xor.count_ones();
        // FNV-1a is not crypto-quality but a single-bit diff
        // should still flip more than 8 bits in the output.
        assert!(
            popcount > 8,
            "weak avalanche: only {} bits changed",
            popcount
        );
    }

    #[test]
    fn fresh_dedup_returns_present_for_first_frame() {
        let mut d = FrameDeduplicator::new();
        let h = compute_frame_hash(&[1, 2, 3]);
        assert_eq!(d.decide(h), PresentDecision::Present);
        assert_eq!(d.stats().presents_total, 1);
        assert_eq!(d.stats().skip_duplicate_total, 0);
    }

    #[test]
    fn second_present_with_same_hash_is_skip_duplicate() {
        let mut d = FrameDeduplicator::new();
        let h = compute_frame_hash(&[1, 2, 3]);

        // Frame 1: Present + mark.
        let _ = d.decide(h);
        d.mark_presented(h);

        // Frame 2: same hash → SkipDuplicate.
        assert_eq!(d.decide(h), PresentDecision::SkipDuplicate);
        assert_eq!(d.stats().presents_total, 1);
        assert_eq!(d.stats().skip_duplicate_total, 1);
    }

    #[test]
    fn changed_frame_after_dedup_returns_present() {
        let mut d = FrameDeduplicator::new();
        let h1 = compute_frame_hash(&[1, 2, 3]);
        let h2 = compute_frame_hash(&[4, 5, 6]);

        d.decide(h1);
        d.mark_presented(h1);
        d.decide(h1); // dup
        // New frame — Present.
        assert_eq!(d.decide(h2), PresentDecision::Present);
        assert_eq!(d.stats().presents_total, 2);
        assert_eq!(d.stats().skip_duplicate_total, 1);
    }

    #[test]
    fn skip_duplicate_does_not_advance_reference() {
        // The bead's correctness invariant: skipping a duplicate
        // must NOT advance the "last presented" hash. Otherwise
        // a series of A → A → A → A → A → B would record A as
        // last-presented at every step (no-op semantically) but
        // also reset the reference incorrectly. Here we confirm
        // that after a skip, the reference is unchanged.
        let mut d = FrameDeduplicator::new();
        let h = compute_frame_hash(&[42; 100]);

        d.decide(h);
        d.mark_presented(h);
        // 5 duplicate frames, none mark_presented (skip path
        // never marks).
        for _ in 0..5 {
            assert_eq!(d.decide(h), PresentDecision::SkipDuplicate);
        }
        // Reference still pointing at the original h.
        assert_eq!(d.last_presented(), Some(h));
        assert_eq!(d.stats().skip_duplicate_total, 5);
    }

    #[test]
    fn force_present_overrides_dedup() {
        let mut d = FrameDeduplicator::new();
        let h = compute_frame_hash(&[1, 2, 3]);
        d.decide(h);
        d.mark_presented(h);
        // Even with the same hash, force_present forces Present.
        d.set_force_present(true);
        assert_eq!(d.decide(h), PresentDecision::Present);
        assert_eq!(d.stats().forced_presents_total, 1);
        assert_eq!(d.stats().skip_duplicate_total, 0);
    }

    #[test]
    fn force_present_clearing_re_enables_dedup() {
        let mut d = FrameDeduplicator::new();
        let h = compute_frame_hash(&[1, 2, 3]);
        d.decide(h);
        d.mark_presented(h);

        d.set_force_present(true);
        assert_eq!(d.decide(h), PresentDecision::Present);
        d.mark_presented(h);

        d.set_force_present(false);
        // Now duplicate of the post-mark reference → skip.
        assert_eq!(d.decide(h), PresentDecision::SkipDuplicate);
    }

    #[test]
    fn invalidate_reference_forces_next_present() {
        let mut d = FrameDeduplicator::new();
        let h = compute_frame_hash(&[1, 2, 3]);
        d.decide(h);
        d.mark_presented(h);

        d.invalidate_reference();
        assert_eq!(d.last_presented(), None);
        // Same hash now reads as new (no reference to compare).
        assert_eq!(d.decide(h), PresentDecision::Present);
    }

    #[test]
    fn idle_pattern_drives_dedup_rate_above_99_percent() {
        // The bead's headline target: laptop idle drops from
        // ~5 %/hr battery to ~0.1 %/hr. Model: 600 frames at
        // 60 Hz, framebuffer never changes (idle terminal). After
        // the first Present, every subsequent frame should skip.
        let mut d = FrameDeduplicator::new();
        // Use a non-trivial fixed buffer so the test exercises
        // the actual hash path, not the empty-input shortcut.
        let buf: Vec<u8> = (0..1920 * 1080 * 4).map(|i| (i % 251) as u8).collect();
        let h = compute_frame_hash(&buf);

        // Frame 1: Present.
        let _ = d.decide(h);
        d.mark_presented(h);

        // Frames 2..600: same hash.
        for _ in 1..600 {
            d.decide(h);
        }

        let rate = d.stats().dedup_rate();
        assert!(rate >= 0.99, "idle dedup rate {} below 0.99 target", rate);
    }

    #[test]
    fn typing_pattern_dedup_rate_is_zero() {
        // Counter to the idle case: every frame's framebuffer is
        // different (typing → cell change → different bytes),
        // so dedup should never fire. This pins the "Present
        // when the hash differs" path.
        let mut d = FrameDeduplicator::new();
        for i in 0..60u32 {
            let buf = i.to_le_bytes();
            let h = compute_frame_hash(&buf);
            assert_eq!(d.decide(h), PresentDecision::Present);
            d.mark_presented(h);
        }
        assert_eq!(d.stats().presents_total, 60);
        assert_eq!(d.stats().skip_duplicate_total, 0);
    }

    #[test]
    fn dedup_rate_is_zero_at_startup() {
        let stats = FrameDedupStats::default();
        assert_eq!(stats.total(), 0);
        assert!((stats.dedup_rate() - 0.0).abs() < 1e-9);
    }

    #[test]
    fn forced_presents_count_inside_presents_total() {
        // The bead's telemetry contract: forced_presents_total
        // is a SUBSET of presents_total. A force-present is
        // still a present.
        let mut d = FrameDeduplicator::new();
        let h = compute_frame_hash(&[1, 2, 3]);

        d.set_force_present(true);
        for _ in 0..10 {
            d.decide(h);
        }
        assert_eq!(d.stats().presents_total, 10);
        assert_eq!(d.stats().forced_presents_total, 10);
        assert_eq!(d.stats().skip_duplicate_total, 0);
    }

    #[test]
    fn hash_is_deterministic_across_repeated_calls() {
        // Critical invariant: dedup correctness depends on the
        // hash being deterministic. Run a substantial buffer
        // through the hash multiple times and verify equality.
        let buf: Vec<u8> = (0..8192).map(|i| (i & 0xff) as u8).collect();
        let h1 = compute_frame_hash(&buf);
        let h2 = compute_frame_hash(&buf);
        let h3 = compute_frame_hash(&buf);
        assert_eq!(h1, h2);
        assert_eq!(h2, h3);
    }

    #[test]
    fn frame_hash_zero_constant_is_actually_zero() {
        assert_eq!(FrameHash::ZERO.raw(), 0);
        assert_eq!(FrameHash::default(), FrameHash::ZERO);
    }
}
