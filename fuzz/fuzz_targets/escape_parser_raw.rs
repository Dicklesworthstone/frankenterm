#![no_main]
//! Crash detector for raw bytes → `frankenterm_escape_parser::Parser` (ft-1p752).
//!
//! The crate's existing fuzz targets are structure-aware:
//!   * `termwiz_csi_parser.rs` — CSI grammar
//!   * `osc_marker_parser.rs`  — OSC markers
//!
//! Neither exercises SOS/PM/APC, Sixel, nested DCS, or pathological ESC
//! chains as plain byte streams. This target feeds arbitrary bytes (bounded
//! to 64 KiB per iteration) into the full `Parser::parse` state machine
//! with a counting callback and pins three invariants on top of the crash
//! oracle:
//!
//! 1. **Parser::new produces a fresh state every iteration** (no
//!    cross-input bleed).
//! 2. **`parse` + `parse_as_vec` agree on Action counts** — the two
//!    public entry points must produce the same number of Actions for
//!    the same bytes. A divergence means the state machine has a
//!    forked code path that the structured fuzzers would miss.
//! 3. **Action count is bounded by input length × small constant** —
//!    the state machine never emits more Actions than there are bytes
//!    × the max Actions-per-byte the grammar permits (we use 8 as a
//!    loose upper bound; tighter pinning is a follow-up). Catches any
//!    regression that turns parse into quadratic/explosive output.

use frankenterm_escape_parser::{Action, parser::Parser};
use libfuzzer_sys::fuzz_target;

/// Loose upper bound on Actions-per-byte. Real grammar emits at most
/// one Action per state-machine terminal transition; giving headroom
/// so this check catches true explosion, not close-to-bound traffic.
const MAX_ACTIONS_PER_BYTE: usize = 8;

fuzz_target!(|data: &[u8]| {
    // Bound input size — beyond 64 KiB the iteration-latency cost
    // dominates the marginal coverage gain.
    if data.len() > 64 * 1024 {
        return;
    }

    // Entry point 1: streaming callback form. `Parser::parse` drives the
    // state machine, invoking the callback for each emitted Action.
    let mut parse_callback_count: usize = 0;
    let mut parser = Parser::new();
    parser.parse(data, |_action: Action| {
        parse_callback_count += 1;
    });

    // Entry point 2: batched form. Should produce the same Action count
    // for the same bytes on a fresh parser.
    let mut parser2 = Parser::new();
    let vec_actions = parser2.parse_as_vec(data);

    // Invariant: callback count matches batched count. If the two
    // forms ever diverge, there's an emit-vs-collect bug that the
    // structured fuzzers would miss because they only check one.
    assert_eq!(
        parse_callback_count,
        vec_actions.len(),
        "parse() emitted {} actions via callback but parse_as_vec() produced {} \
         for input of {} bytes — the two entry points must agree",
        parse_callback_count,
        vec_actions.len(),
        data.len()
    );

    // Invariant: output is linear-bounded. data.len() * MAX_ACTIONS_PER_BYTE
    // is a loose ceiling; any regression that blows through it means the
    // state machine is emitting duplicate / recursive Actions.
    let ceiling = data.len().saturating_mul(MAX_ACTIONS_PER_BYTE).saturating_add(8);
    assert!(
        vec_actions.len() <= ceiling,
        "parse_as_vec emitted {} actions on {} input bytes — exceeds linear \
         ceiling of {} (MAX_ACTIONS_PER_BYTE = {}). Possible output explosion.",
        vec_actions.len(),
        data.len(),
        ceiling,
        MAX_ACTIONS_PER_BYTE
    );
});
