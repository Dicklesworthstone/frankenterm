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
//! with a collecting callback and pins four invariants on top of the crash
//! oracle:
//!
//! 1. **Parser::new produces a fresh state every iteration** (no
//!    cross-input bleed).
//! 2. **`parse` + `parse_as_vec` agree exactly on Actions** — the two
//!    public entry points must produce the same Actions for the same
//!    bytes. A divergence means the state machine has a forked code path
//!    that the structured fuzzers would miss.
//! 3. **Streaming chunk boundaries are transparent** — feeding the same
//!    bytes in deterministic small chunks must emit the same Actions as
//!    one-shot parsing. This catches parser state bugs when escape
//!    sequences are split across reads.
//! 4. **Action count is bounded by input length × small constant** —
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

const CHUNK_SCHEDULE: &[usize] = &[1, 2, 3, 5, 8, 13, 21, 34];

fn parse_chunked(data: &[u8]) -> Vec<Action> {
    let mut parser = Parser::new();
    let mut actions = Vec::new();
    let mut offset = 0;
    let mut schedule_idx = 0;

    while offset < data.len() {
        let chunk_len =
            CHUNK_SCHEDULE[schedule_idx % CHUNK_SCHEDULE.len()].min(data.len() - offset);
        parser.parse(&data[offset..offset + chunk_len], |action| {
            actions.push(action);
        });
        offset += chunk_len;
        schedule_idx += 1;
    }

    actions
}

fuzz_target!(|data: &[u8]| {
    // Bound input size — beyond 64 KiB the iteration-latency cost
    // dominates the marginal coverage gain.
    if data.len() > 64 * 1024 {
        return;
    }

    // Entry point 1: streaming callback form. `Parser::parse` drives the
    // state machine, invoking the callback for each emitted Action.
    let mut parse_callback_actions = Vec::new();
    let mut parser = Parser::new();
    parser.parse(data, |action: Action| {
        parse_callback_actions.push(action);
    });

    // Entry point 2: batched form. Should produce the same Actions
    // for the same bytes on a fresh parser.
    let mut parser2 = Parser::new();
    let vec_actions = parser2.parse_as_vec(data);

    // Invariant: callback actions match batched actions. If the two
    // forms ever diverge, there's an emit-vs-collect bug that the
    // structured fuzzers would miss because they only check one.
    assert_eq!(
        parse_callback_actions,
        vec_actions,
        "parse() callback Actions diverged from parse_as_vec() Actions \
         for input of {} bytes; the two entry points must agree",
        data.len()
    );

    // Invariant: splitting input across transport read boundaries does
    // not change the emitted terminal actions.
    let chunked_actions = parse_chunked(data);
    assert_eq!(
        chunked_actions,
        vec_actions,
        "chunked parse Actions diverged from one-shot parse_as_vec() Actions \
         for input of {} bytes; parser state must survive split escape sequences",
        data.len()
    );

    // Invariant: output is linear-bounded. data.len() * MAX_ACTIONS_PER_BYTE
    // is a loose ceiling; any regression that blows through it means the
    // state machine is emitting duplicate / recursive Actions.
    let ceiling = data
        .len()
        .saturating_mul(MAX_ACTIONS_PER_BYTE)
        .saturating_add(8);
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
