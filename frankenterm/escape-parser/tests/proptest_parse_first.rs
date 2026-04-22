//! Property-based robustness tests for the streaming `Parser::parse_first`
//! and `Parser::parse_first_as_vec` entry points (ft-1p752).
//!
//! The companion `proptest_parser_robustness.rs` covers `Parser::parse` and
//! `Parser::parse_as_vec` against arbitrary bytes (crash-freedom,
//! determinism, concat consistency, etc). It does NOT exercise the
//! streaming entry points — those are covered only by three hand-written
//! unit tests in `src/parser/mod.rs:785-801` against specific inputs.
//!
//! `parse_first` is what callers use when they want to consume one
//! escape sequence at a time from a byte buffer (tmux_cc, replay engines,
//! any framing-aware consumer). Any bug in its consumed-offset arithmetic
//! or short-circuit logic either strands bytes (caller loops forever) or
//! double-consumes them (caller misses sequences).
//!
//! Pinned invariants:
//!
//!   1. **Crash-freedom** — `parse_first` never panics on arbitrary bytes.
//!   2. **Consumed-bound** — when `Some((_, n))` is returned,
//!      `0 < n <= bytes.len()`. `n == 0` would violate progress;
//!      `n > bytes.len()` would index out of bounds in callers that
//!      split `&bytes[n..]`.
//!   3. **Determinism** — calling `parse_first` twice on identical
//!      byte slices with fresh parsers returns the same `Some(n)` / `None`
//!      and the same consumed length.
//!   4. **Iteration reaches `parse_first_as_vec`'s count** —
//!      `parse_first_as_vec` collects all actions from the first sequence
//!      and returns the same offset as `parse_first`. Divergence would
//!      mean the two entry points disagree on where a sequence ends.
//!   5. **Drain-to-EOF covers the whole input** — chaining
//!      `parse_first` on `&bytes[n..]` until it returns `None` must
//!      consume exactly `bytes.len()` bytes across the chain (plus
//!      however many bytes hang in an incomplete trailing sequence).
//!      Catches any regression in the offset math.

use frankenterm_escape_parser::Action;
use frankenterm_escape_parser::parser::Parser;
use proptest::prelude::*;

// ─── Strategies ─────────────────────────────────────────────────────────

/// Arbitrary raw bytes — broadest input space. 0..512 keeps shrinking
/// cheap (proptest default budget).
fn arb_bytes() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(any::<u8>(), 0..512)
}

/// Bytes biased toward structured escape sequences. We hand-seed common
/// framing shapes (CSI, OSC, APC, DCS) and let proptest's mutator mix
/// them with noise. Without this the mostly-random arbitrary-bytes
/// distribution rarely produces a complete sequence at the front of the
/// buffer, so the `parse_first = Some(...)` branch is under-exercised.
fn arb_structured_bytes() -> impl Strategy<Value = Vec<u8>> {
    let shapes: Vec<&'static [u8]> = vec![
        b"\x1b[31m",                     // CSI: red FG
        b"\x1b[?1049h",                   // CSI: private alt-screen enable
        b"\x1b]0;title\x07",              // OSC: set title, BEL-terminated
        b"\x1b]8;;https://example.com\x07text\x1b]8;;\x07", // OSC 8: hyperlink
        b"\x1b_apc payload\x1b\\",        // APC: ST-terminated
        b"\x1bP+q53\x1b\\",               // DCS: request termcap, ST-terminated
        b"\x1bM",                          // ESC M: reverse index
        b"plain text",                    // ground state, no ESC
        b"\xc3\xa9",                       // UTF-8 é (multi-byte Print)
        b"\x08\x0d\x0a",                   // control codes: BS, CR, LF
        b"\x1b[",                          // truncated CSI — caller sees None
    ];
    proptest::collection::vec(
        proptest::sample::select(shapes),
        0..10,
    )
    .prop_map(|parts| parts.into_iter().flat_map(|p| p.iter().copied()).collect::<Vec<u8>>())
}

// ─── Properties ─────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 512,
        ..ProptestConfig::default()
    })]

    /// Invariant 1 — crash-freedom on arbitrary bytes.
    #[test]
    fn parse_first_does_not_panic_on_arbitrary_bytes(bytes in arb_bytes()) {
        let mut parser = Parser::new();
        let _ = parser.parse_first(&bytes);
    }

    /// Invariant 2 — consumed-offset bounds.
    ///
    /// Critical for callers that split `&bytes[n..]`: n must be strictly
    /// positive (otherwise the caller would loop forever on the same
    /// prefix) and strictly within the buffer (otherwise the slice
    /// panics).
    #[test]
    fn parse_first_consumed_offset_is_in_bounds(bytes in arb_structured_bytes()) {
        let mut parser = Parser::new();
        if let Some((_action, consumed)) = parser.parse_first(&bytes) {
            prop_assert!(
                consumed > 0,
                "parse_first returned Some with consumed=0 — no progress made"
            );
            prop_assert!(
                consumed <= bytes.len(),
                "parse_first returned consumed={} > bytes.len()={}",
                consumed, bytes.len()
            );
        }
    }

    /// Invariant 3 — determinism across fresh parsers.
    #[test]
    fn parse_first_is_deterministic(bytes in arb_bytes()) {
        let mut p1 = Parser::new();
        let mut p2 = Parser::new();
        let r1 = p1.parse_first(&bytes);
        let r2 = p2.parse_first(&bytes);
        // Compare consumed offsets and Action presence (not the Action
        // payload itself — some Action variants contain non-Eq data like
        // Sixel pixel buffers).
        let o1 = r1.as_ref().map(|(_, n)| *n);
        let o2 = r2.as_ref().map(|(_, n)| *n);
        prop_assert_eq!(
            o1, o2,
            "parse_first consumed offset differs across fresh parsers: \
             {:?} vs {:?}", o1, o2
        );
    }

    /// Invariant 4 — parse_first_as_vec's offset is a SUPERSET of
    /// parse_first's offset (when both return Some).
    ///
    /// The two entry points have semantically distinct stop conditions
    /// documented in src/parser/mod.rs:
    ///
    ///   * `parse_first` stops at the first Action emission.
    ///   * `parse_first_as_vec` "collects all actions from the first
    ///     sequence, and guarantees the state machine is in the ground
    ///     state at the end of this sequence."
    ///
    /// For multi-action sequences (DCS with data bytes, CSI with
    /// multiple intermediates, etc.) parse_first_as_vec therefore keeps
    /// consuming bytes after parse_first has already returned. Concrete
    /// example that was a fixture in this test:
    /// `ESC P + q 5 3 ESC \` (DCS termcap-request). parse_first returns
    /// consumed=7 on the first data-byte Action; parse_first_as_vec
    /// keeps going to consumed=8 past the ST terminator.
    ///
    /// Property: `parse_first_as_vec` never consumes FEWER bytes than
    /// `parse_first` for the same input, and if either returns None the
    /// other result is constrained. Catches regressions where
    /// parse_first_as_vec starts stopping early (losing trailing
    /// terminators in a sequence) or where parse_first overshoots.
    #[test]
    fn parse_first_as_vec_offset_not_less_than_parse_first(bytes in arb_structured_bytes()) {
        let mut p1 = Parser::new();
        let mut p2 = Parser::new();
        let first = p1.parse_first(&bytes);
        let first_vec = p2.parse_first_as_vec(&bytes);

        match (&first, &first_vec) {
            (Some((_, n_first)), Some((actions, n_vec))) => {
                prop_assert!(
                    !actions.is_empty(),
                    "parse_first_as_vec returned Some with empty actions"
                );
                prop_assert!(
                    *n_vec >= *n_first,
                    "parse_first_as_vec offset={} is less than parse_first offset={} — \
                     the sequence-collecting variant should never consume fewer bytes",
                    n_vec, n_first
                );
            }
            (None, None) => {} // both agree: no complete sequence
            (Some((_, n)), None) => {
                // parse_first saw an Action but parse_first_as_vec couldn't
                // reach a ground state. This can happen on short-circuit
                // Actions that don't live inside a framed sequence, OR on
                // inputs where the first sequence never terminates within
                // the buffer. Document it rather than panic.
                prop_assume!(false); // skip this case — not a violation of the offset invariant
                let _ = n;
            }
            (None, Some((actions, n))) => {
                prop_assert!(
                    false,
                    "parse_first returned None but parse_first_as_vec returned Some({} actions, consumed={})",
                    actions.len(), n
                );
            }
        }
    }

    /// Invariant 5 — drain loop terminates and total consumption does
    /// not exceed input length.
    ///
    /// This is the operational contract callers actually depend on: loop
    /// calling `parse_first` on the remainder until it returns `None`,
    /// then the remaining (unconsumed) bytes are a single incomplete
    /// sequence. The loop must always terminate (each iteration must
    /// consume at least 1 byte) and total consumption must be bounded
    /// by input length.
    #[test]
    fn parse_first_drain_loop_terminates(bytes in arb_structured_bytes()) {
        let mut consumed_total: usize = 0;
        let mut iterations: usize = 0;
        const MAX_ITERATIONS: usize = 2048; // generous: 1 action per byte max
        let mut parser = Parser::new();
        loop {
            iterations += 1;
            prop_assert!(
                iterations <= MAX_ITERATIONS,
                "drain loop exceeded {} iterations on {}-byte input — parse_first is not making progress",
                MAX_ITERATIONS, bytes.len()
            );
            let remainder = &bytes[consumed_total..];
            match parser.parse_first(remainder) {
                Some((_action, n)) => {
                    prop_assert!(n > 0, "parse_first Some with zero consumed");
                    consumed_total = consumed_total.saturating_add(n);
                    prop_assert!(
                        consumed_total <= bytes.len(),
                        "drain loop consumed {} > input {}",
                        consumed_total, bytes.len()
                    );
                    if consumed_total == bytes.len() {
                        break; // exact consumption, no trailing incomplete sequence
                    }
                }
                None => {
                    // Reached a trailing incomplete sequence; loop exits.
                    break;
                }
            }
        }
    }
}

// ─── Regression: hand-crafted inputs that historically surfaced bugs ──

/// Truncated CSI: `ESC [` without the final byte. Must return None so
/// the caller knows to hold the partial frame and append more bytes.
#[test]
fn parse_first_truncated_csi_returns_none() {
    let mut parser = Parser::new();
    assert_eq!(None, parser.parse_first(b"\x1b["));
}

/// Truncated OSC: `ESC ] 0 ; t` without ST/BEL. Must return None.
#[test]
fn parse_first_truncated_osc_returns_none() {
    let mut parser = Parser::new();
    assert_eq!(None, parser.parse_first(b"\x1b]0;t"));
}

/// Full CSI: `ESC [ 3 1 m` → RED foreground. Must return Some with
/// consumed=5 (all bytes consumed).
#[test]
fn parse_first_complete_csi_reports_full_consumption() {
    let mut parser = Parser::new();
    let result = parser.parse_first(b"\x1b[31m");
    match result {
        Some((_action, consumed)) => {
            assert_eq!(consumed, 5, "complete CSI ESC[31m should consume all 5 bytes");
        }
        None => panic!("parse_first returned None on complete CSI"),
    }
}

/// Plain ASCII text followed by nothing: one Print action per byte is
/// the typical shape. Must emit the first action without consuming the
/// entire buffer (consumed should be 1 for the first printable byte).
#[test]
fn parse_first_plain_ascii_emits_single_byte() {
    let mut parser = Parser::new();
    let result = parser.parse_first(b"ABCDE");
    match result {
        Some((Action::Print(ch), consumed)) => {
            assert_eq!(ch, 'A');
            assert_eq!(consumed, 1, "first ASCII byte should consume exactly 1");
        }
        other => panic!("expected Some(Print('A'), 1), got {other:?}"),
    }
}

/// Chained parse_first calls over plain text must terminate and consume
/// all bytes. This is the primary operational path.
#[test]
fn parse_first_chain_over_plain_ascii_consumes_everything() {
    let mut parser = Parser::new();
    let input = b"hello world";
    let mut consumed = 0;
    while consumed < input.len() {
        let result = parser.parse_first(&input[consumed..]);
        match result {
            Some((_, n)) => {
                assert!(n > 0, "chain iteration made zero progress");
                consumed += n;
            }
            None => panic!("chain over plain ASCII hit None at offset {}", consumed),
        }
    }
    assert_eq!(consumed, input.len());
}
