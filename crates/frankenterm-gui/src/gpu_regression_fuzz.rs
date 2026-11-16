//! Deterministic seed-based input stream generator for the 24h
//! adversarial fuzz lane (ft-mpc9b.1.6).
//!
//! The fuzz lane catches visual regressions by replaying a long
//! deterministic sequence of resize/scroll/content events against
//! the headless renderer and comparing the resulting frames to a
//! reference. Because the stream is fully determined by a `u64`
//! seed, any failure is reproducible — the failure-artifact emitter
//! records the seed and the event index, and rerunning with that
//! seed lands on the exact same offending frame.
//!
//! ## What this module ships (foundation, ft-mpc9b.1.6)
//!
//! - `FuzzSeed`: a `u64`-seeded xorshift64* PRNG. Same seed →
//!   identical event stream forever. No external crate dependency.
//! - `FuzzInputEvent`: tagged enum of resize / write / scroll /
//!   selection / focus / clear events.
//! - `FuzzStream`: bounded iterator over events generated from a
//!   seed; the integration bead points the harness binary at this
//!   to drive a 24h fuzz run.
//!
//! ## What remains deferred (continuation, see follow-up bead)
//!
//! - a richer analytic reference renderer/oracle beyond the harness's
//!   deterministic duplicate-render comparator
//! - cross-link RQ-S4 (24h fuzz, 0 critical artifacts) in the SLO
//!   catalog at docs/perf/resize-quality-slo.md

/// Tagged event types the fuzz lane drives into the headless
/// renderer. Variants intentionally cover the dirty-event sources
/// enumerated in ft-mpc9b.1.2: PTY content writes, cursor moves,
/// selection changes, resize, and focus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FuzzInputEvent {
    /// Resize the viewport. `cols` and `rows` are in cells.
    Resize { cols: u16, rows: u16 },
    /// Write an ASCII content burst to the active pane. The bytes
    /// are constrained to the printable range to keep the input
    /// deterministic and avoid escape-sequence parser excursions
    /// during the foundation slice. The escape-sequence variant
    /// (CSI/OSC/DCS injection) lives in `EscapeBurst` and is
    /// generated less often.
    Write { bytes: Vec<u8> },
    /// Inject a small ANSI escape sequence into the stream. Kept
    /// separate from `Write` so the integration bead can ramp the
    /// frequency independently while the parser stabilises.
    EscapeBurst { bytes: Vec<u8> },
    /// Scroll by a signed line delta. Negative scrolls back, positive
    /// scrolls forward.
    Scroll { lines: i32 },
    /// Begin a selection at the given cell coordinate.
    SelectStart { col: u16, row: u16 },
    /// Extend the active selection to the given cell coordinate.
    SelectExtend { col: u16, row: u16 },
    /// Drop the active selection (mouse-up).
    SelectEnd,
    /// Toggle pane focus on/off (mark border-row dirty).
    FocusToggle,
    /// Clear the active pane (resets dirty bitmap, golden frame
    /// returns to a known baseline).
    Clear,
}

/// Knobs for the event generator. The defaults are calibrated for a
/// 24h fuzz run on a 200-pane fleet (RQ-S4 in the SLO catalog) but
/// callers may override them per-run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuzzConfig {
    /// Maximum cells in the resize axis.
    pub max_cols: u16,
    /// Maximum cells in the resize axis.
    pub max_rows: u16,
    /// Minimum cells in either axis (avoids 0×N degenerate cases).
    pub min_axis: u16,
    /// Maximum bytes per `Write` burst.
    pub max_write_bytes: u16,
    /// Maximum line delta per `Scroll` event.
    pub max_scroll_lines: u16,
    /// Total events to generate. The 24h lane uses ~20M; tests use
    /// small numbers.
    pub event_budget: u64,
}

impl Default for FuzzConfig {
    fn default() -> Self {
        Self {
            max_cols: 256,
            max_rows: 96,
            min_axis: 8,
            max_write_bytes: 64,
            max_scroll_lines: 200,
            event_budget: 20_000_000,
        }
    }
}

/// xorshift64* PRNG: small, fast, deterministic, non-cryptographic.
/// We do not need cryptographic strength here — only repeatability.
#[derive(Debug, Clone, Copy)]
pub struct FuzzSeed {
    state: u64,
}

impl FuzzSeed {
    /// Construct a generator from a 64-bit seed. Seed `0` is mapped
    /// to a non-zero constant because the all-zero state is a fixed
    /// point of xorshift.
    pub fn new(seed: u64) -> Self {
        let state = if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        };
        Self { state }
    }

    /// Next pseudo-random `u64`.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Next `u64` in `[0, bound)`. Uses Lemire's
    /// nearly-divisionless method via 128-bit multiply, which has a
    /// negligible bias for our purposes.
    pub fn next_bounded_u64(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            return 0;
        }
        let r = self.next_u64();
        ((r as u128 * bound as u128) >> 64) as u64
    }

    /// Next `u32` in `[0, bound)`. Convenience wrapper.
    pub fn next_bounded_u32(&mut self, bound: u32) -> u32 {
        self.next_bounded_u64(u64::from(bound)) as u32
    }

    /// Snapshot the current internal state. The integration bead
    /// records this in the failure artifact so a fuzz violation is
    /// reproducible from the *exact* event index, not just from the
    /// original seed.
    pub fn state(&self) -> u64 {
        self.state
    }
}

/// Bounded iterator that turns a `FuzzSeed` into a stream of
/// `FuzzInputEvent` values per a `FuzzConfig`.
///
/// The iterator yields `event_budget` events then returns `None`.
/// The same `(seed, config)` pair always produces the same sequence.
#[derive(Debug, Clone)]
pub struct FuzzStream {
    rng: FuzzSeed,
    config: FuzzConfig,
    emitted: u64,
}

impl FuzzStream {
    /// Construct a stream from a seed and config.
    pub fn new(seed: u64, config: FuzzConfig) -> Self {
        Self {
            rng: FuzzSeed::new(seed),
            config,
            emitted: 0,
        }
    }

    /// How many events have been emitted so far.
    pub fn emitted(&self) -> u64 {
        self.emitted
    }

    /// Current internal PRNG state. Useful for failure artifacts.
    pub fn rng_state(&self) -> u64 {
        self.rng.state()
    }

    fn pick_axis(&mut self) -> u16 {
        let span = self.config.max_cols.max(self.config.max_rows);
        let min = self.config.min_axis.min(span);
        let extra = self.rng.next_bounded_u32(u32::from(span - min) + 1) as u16;
        min + extra
    }

    fn pick_write_burst(&mut self) -> Vec<u8> {
        // Keep writes in printable ASCII so the stream stays trivially
        // diffable in the failure artifact. Escape bursts go through
        // `pick_escape_burst` separately.
        let len = 1 + self
            .rng
            .next_bounded_u32(u32::from(self.config.max_write_bytes))
            as usize;
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            // Range 0x20..=0x7E (printable ASCII).
            let c = 0x20 + self.rng.next_bounded_u32(0x5F) as u8;
            out.push(c);
        }
        out
    }

    fn pick_escape_burst(&mut self) -> Vec<u8> {
        // A small CSI sequence; cycles through cursor/erase/SGR
        // primitives. The fuzz lane does NOT want to drive the
        // parser into pathological territory in the foundation
        // slice; the integration bead can broaden this once the
        // parser baseline is captured.
        const TEMPLATES: &[&[u8]] = &[
            b"\x1b[H",      // cursor home
            b"\x1b[2J",     // erase display
            b"\x1b[K",      // erase to EOL
            b"\x1b[?25l",   // hide cursor
            b"\x1b[?25h",   // show cursor
            b"\x1b[31m",    // SGR red fg
            b"\x1b[39m",    // SGR default fg
            b"\x1b[1m",     // SGR bold
            b"\x1b[22m",    // SGR clear bold
            b"\x1b[?1049h", // alt screen on
            b"\x1b[?1049l", // alt screen off
        ];
        let idx = self.rng.next_bounded_u32(TEMPLATES.len() as u32) as usize;
        TEMPLATES[idx].to_vec()
    }

    fn pick_cell(&mut self) -> (u16, u16) {
        let col = self.rng.next_bounded_u32(u32::from(self.config.max_cols)) as u16;
        let row = self.rng.next_bounded_u32(u32::from(self.config.max_rows)) as u16;
        (col, row)
    }
}

impl Iterator for FuzzStream {
    type Item = FuzzInputEvent;

    fn next(&mut self) -> Option<Self::Item> {
        if self.emitted >= self.config.event_budget {
            return None;
        }

        // Distribution: writes are the common case; resize/scroll
        // are middling; selection/focus/escape/clear are rare. The
        // weights below sum to 100 to make the math obvious.
        let kind = self.rng.next_bounded_u32(100);
        let event = if kind < 55 {
            FuzzInputEvent::Write {
                bytes: self.pick_write_burst(),
            }
        } else if kind < 70 {
            let cols = self.pick_axis();
            let rows = self.pick_axis();
            FuzzInputEvent::Resize { cols, rows }
        } else if kind < 80 {
            let raw = self
                .rng
                .next_bounded_u32(u32::from(self.config.max_scroll_lines) * 2 + 1);
            let lines = raw as i32 - i32::from(self.config.max_scroll_lines);
            FuzzInputEvent::Scroll { lines }
        } else if kind < 88 {
            let (col, row) = self.pick_cell();
            FuzzInputEvent::SelectStart { col, row }
        } else if kind < 92 {
            let (col, row) = self.pick_cell();
            FuzzInputEvent::SelectExtend { col, row }
        } else if kind < 94 {
            FuzzInputEvent::SelectEnd
        } else if kind < 97 {
            FuzzInputEvent::EscapeBurst {
                bytes: self.pick_escape_burst(),
            }
        } else if kind < 99 {
            FuzzInputEvent::FocusToggle
        } else {
            FuzzInputEvent::Clear
        };

        self.emitted += 1;
        Some(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small_config() -> FuzzConfig {
        FuzzConfig {
            max_cols: 64,
            max_rows: 24,
            min_axis: 4,
            max_write_bytes: 16,
            max_scroll_lines: 10,
            event_budget: 256,
        }
    }

    #[test]
    fn seed_zero_does_not_lock_at_fixed_point() {
        // xorshift's all-zero state is a fixed point. FuzzSeed::new
        // must remap seed=0 to a non-zero state so the stream
        // actually advances.
        let mut a = FuzzSeed::new(0);
        let first = a.next_u64();
        let second = a.next_u64();
        assert_ne!(first, 0);
        assert_ne!(first, second);
    }

    #[test]
    fn seed_is_reproducible() {
        let a: Vec<FuzzInputEvent> = FuzzStream::new(0xCAFE_F00D, small_config()).collect();
        let b: Vec<FuzzInputEvent> = FuzzStream::new(0xCAFE_F00D, small_config()).collect();
        assert_eq!(a, b);
        assert_eq!(a.len(), 256);
    }

    #[test]
    fn different_seeds_diverge() {
        let a: Vec<FuzzInputEvent> = FuzzStream::new(1, small_config()).take(64).collect();
        let b: Vec<FuzzInputEvent> = FuzzStream::new(2, small_config()).take(64).collect();
        assert_ne!(a, b);
    }

    #[test]
    fn event_budget_is_honored() {
        let mut s = FuzzStream::new(42, small_config());
        let collected: Vec<_> = (&mut s).collect();
        assert_eq!(collected.len(), 256);
        assert_eq!(s.emitted(), 256);
        // Past the budget the stream is exhausted.
        assert!(s.next().is_none());
    }

    #[test]
    fn next_bounded_returns_in_range() {
        let mut rng = FuzzSeed::new(0xDEAD_BEEF);
        for _ in 0..10_000 {
            let v = rng.next_bounded_u64(17);
            assert!(v < 17, "next_bounded_u64(17) returned {}", v);
        }
        assert_eq!(rng.next_bounded_u64(0), 0);
    }

    #[test]
    fn distribution_covers_every_variant_under_modest_budget() {
        // With event_budget=2048 and the 100-weight rolling
        // distribution, every variant should appear at least once.
        let cfg = FuzzConfig {
            event_budget: 2048,
            ..small_config()
        };
        let events: Vec<_> = FuzzStream::new(7, cfg).collect();

        let mut saw_write = false;
        let mut saw_resize = false;
        let mut saw_scroll = false;
        let mut saw_select_start = false;
        let mut saw_select_extend = false;
        let mut saw_select_end = false;
        let mut saw_escape = false;
        let mut saw_focus = false;
        let mut saw_clear = false;

        for ev in &events {
            match ev {
                FuzzInputEvent::Write { .. } => saw_write = true,
                FuzzInputEvent::Resize { .. } => saw_resize = true,
                FuzzInputEvent::Scroll { .. } => saw_scroll = true,
                FuzzInputEvent::SelectStart { .. } => saw_select_start = true,
                FuzzInputEvent::SelectExtend { .. } => saw_select_extend = true,
                FuzzInputEvent::SelectEnd => saw_select_end = true,
                FuzzInputEvent::EscapeBurst { .. } => saw_escape = true,
                FuzzInputEvent::FocusToggle => saw_focus = true,
                FuzzInputEvent::Clear => saw_clear = true,
            }
        }

        assert!(saw_write, "Write absent under 2048-event budget");
        assert!(saw_resize, "Resize absent");
        assert!(saw_scroll, "Scroll absent");
        assert!(saw_select_start, "SelectStart absent");
        assert!(saw_select_extend, "SelectExtend absent");
        assert!(saw_select_end, "SelectEnd absent");
        assert!(saw_escape, "EscapeBurst absent");
        assert!(saw_focus, "FocusToggle absent");
        assert!(saw_clear, "Clear absent");
    }

    #[test]
    fn resize_axes_respect_min_and_max() {
        let cfg = small_config();
        let events: Vec<_> = FuzzStream::new(99, cfg).collect();
        for ev in &events {
            if let FuzzInputEvent::Resize { cols, rows } = ev {
                assert!(*cols >= cfg.min_axis, "cols={} below min_axis", cols);
                let span = cfg.max_cols.max(cfg.max_rows);
                assert!(*cols <= span, "cols={} above max span", cols);
                assert!(*rows >= cfg.min_axis, "rows={} below min_axis", rows);
                assert!(*rows <= span, "rows={} above max span", rows);
            }
        }
    }

    #[test]
    fn write_bursts_are_printable_ascii_and_bounded() {
        let cfg = small_config();
        let events: Vec<_> = FuzzStream::new(123, cfg).collect();
        for ev in &events {
            if let FuzzInputEvent::Write { bytes } = ev {
                assert!(!bytes.is_empty(), "Write burst was empty");
                assert!(
                    bytes.len() <= usize::from(cfg.max_write_bytes),
                    "Write burst {} exceeds max_write_bytes {}",
                    bytes.len(),
                    cfg.max_write_bytes,
                );
                for &b in bytes {
                    assert!(
                        (0x20..=0x7E).contains(&b),
                        "non-printable byte {:#x} in Write burst",
                        b,
                    );
                }
            }
        }
    }

    #[test]
    fn scroll_deltas_stay_in_signed_band() {
        let cfg = small_config();
        let events: Vec<_> = FuzzStream::new(456, cfg).collect();
        let max = i32::from(cfg.max_scroll_lines);
        for ev in &events {
            if let FuzzInputEvent::Scroll { lines } = ev {
                assert!(
                    lines.abs() <= max,
                    "scroll delta {} exceeds max {}",
                    lines,
                    max,
                );
            }
        }
    }

    #[test]
    fn escape_bursts_are_well_formed_csi() {
        let cfg = FuzzConfig {
            event_budget: 4096,
            ..small_config()
        };
        let events: Vec<_> = FuzzStream::new(789, cfg).collect();
        let mut count = 0;
        for ev in &events {
            if let FuzzInputEvent::EscapeBurst { bytes } = ev {
                count += 1;
                assert!(bytes.starts_with(b"\x1b"), "escape burst missing ESC");
                // CSI templates are short, well below any reasonable
                // bound; sanity-check the tail isn't garbage.
                assert!(bytes.len() <= 8);
            }
        }
        assert!(count > 0, "no EscapeBurst observed in 4096 events");
    }

    #[test]
    fn rng_state_advances_on_every_event() {
        // The integration bead serializes rng_state into the failure
        // artifact so reproductions land on the exact event index.
        // That only works if the state actually advances per event.
        let mut s = FuzzStream::new(0xAA55, small_config());
        let s0 = s.rng_state();
        let _ = s.next();
        let s1 = s.rng_state();
        let _ = s.next();
        let s2 = s.rng_state();

        assert_ne!(s0, s1);
        assert_ne!(s1, s2);
    }
}
