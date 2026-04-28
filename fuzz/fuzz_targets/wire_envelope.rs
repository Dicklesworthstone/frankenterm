#![no_main]

//! [ft-h8v8v] Fuzz target for the distributed-mode wire-protocol surface.
//!
//! `WireEnvelope::from_json` and `Aggregator::ingest` are the entry
//! points where untrusted network bytes flow into frankenterm-core
//! when distributed mode is enabled. A panic, OOM, stack overflow, or
//! deserialization confusion on this boundary is a remote-DoS surface.
//!
//! The contract this harness pins:
//!
//! 1. **No panic on any byte sequence.** `WireEnvelope::from_json`
//!    and `Aggregator::ingest` must return `Ok(_)` or `Err(_)` for
//!    every input — never trip a debug-assert, integer overflow
//!    panic, or stack overflow on deeply-nested JSON.
//! 2. **Saturating-counter invariant.** The aggregator's
//!    `total_accepted` / `total_rejected` use `saturating_add`; this
//!    harness drives N consecutive ingests against one aggregator
//!    and asserts `agent_count() <= max_agents` and that reading
//!    the counters is panic-free.
//!
//! ## Modes (driven by Arbitrary)
//!
//! - **Single**: feed one byte slice into `WireEnvelope::from_json`
//!    AND `Aggregator::ingest` (which is `from_json` + state machine
//!    in one call). Hits the JSON parsing surface with no
//!    scaffolding — malformed UTF-8, deeply-nested arrays, integer
//!    overflow attempts on `version` / `seq`, adversarial payload
//!    variants.
//! - **Sequence**: drive a single Aggregator through up to N
//!    raw-bytes ingests, letting libFuzzer minimize toward inputs
//!    that hit the eviction + duplicate-seq + future-clock branches.
//!
//! ## Not fuzzed here
//!
//! - The TCP framing layer (bounds messages before they reach
//!   `from_json`).
//! - TLS handshakes / connection setup.
//! - The serialization side (`to_json`) — already round-tripped in
//!   `proptest_wire_protocol.rs`.

use arbitrary::Arbitrary;
use frankenterm_core::wire_protocol::{Aggregator, WireEnvelope, WireProtocolLimits};
use libfuzzer_sys::fuzz_target;

// Match the production wire-protocol caps so the harness reflects
// what the network code path actually accepts. Larger inputs are
// dropped at the framing layer before reaching from_json.
const MAX_BYTES: usize = 1024 * 1024; // 1 MiB
const MAX_SEQUENCE_LEN: usize = 32;
const MAX_AGENTS: usize = 16;

#[derive(Arbitrary, Debug)]
enum FuzzInput<'a> {
    /// Single ingest — feed one byte slice through both from_json
    /// (parser-only path) and Aggregator::ingest (parser + state).
    Single(&'a [u8]),
    /// Sequence — drive one Aggregator through up to MAX_SEQUENCE_LEN
    /// raw-bytes ingests with an advancing receive clock so stale-
    /// agent eviction can engage.
    Sequence {
        max_agents_choice: u8,
        ingests: Vec<&'a [u8]>,
        check_stats: bool,
    },
}

fn parse_only(bytes: &[u8], limits: WireProtocolLimits) {
    if bytes.len() > MAX_BYTES {
        return;
    }
    // Contract 1: from_json must return Ok or Err, never panic.
    let _: Result<WireEnvelope, _> = WireEnvelope::from_json_with_limits(bytes, limits);
}

fn parse_and_ingest(bytes: &[u8], agg: &mut Aggregator) {
    if bytes.len() > MAX_BYTES {
        return;
    }
    // Contract 1: ingest is from_json + state machine; must not panic.
    let _ = agg.ingest(bytes);
}

fuzz_target!(|input: FuzzInput| {
    let limits = WireProtocolLimits::default();
    match input {
        FuzzInput::Single(bytes) => {
            // Hit the parser-only path first (matches the wire frame
            // decode site that may peek before forwarding).
            parse_only(bytes, limits);
            // Then the parser+state path through Aggregator.
            let mut agg = Aggregator::with_limits(MAX_AGENTS, limits);
            parse_and_ingest(bytes, &mut agg);
        }
        FuzzInput::Sequence {
            max_agents_choice,
            ingests,
            check_stats,
        } => {
            let max_agents = (max_agents_choice as usize % MAX_AGENTS).max(1);
            let mut agg = Aggregator::with_limits(max_agents, limits);
            for bytes in ingests.into_iter().take(MAX_SEQUENCE_LEN) {
                parse_and_ingest(bytes, &mut agg);
            }
            if check_stats {
                // Contract 2: invariants the production saturating_add
                // semantics promise.
                assert!(
                    agg.agent_count() <= max_agents,
                    "agent_count {} exceeded max_agents {}",
                    agg.agent_count(),
                    max_agents
                );
                let _ = agg.total_accepted(); // panic-free read
                let _ = agg.total_rejected();
            }
        }
    }
});
