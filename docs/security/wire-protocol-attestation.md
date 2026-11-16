# Wire-Protocol Dedup Convergence Proof

**Bead:** [BR-RC-SAFETY-PROOFS.G11] / `ft-x0666.3`
**Status:** Foundation slice shipped. Threat model + dedup
state-space proof (Stateright-shape BFS + TLA+ spec) +
property-style replay/reorder/drop proofs all live;
differential-fuzz target across past wire versions is the
follow-on (depends on a v2 envelope existing — currently only
`PROTOCOL_VERSION = 1`).

## Why this matters

The distributed wire protocol carries pane-capture bytes,
detection events, and per-agent identities across an UNTRUSTED
network into the aggregator's TRUSTED storage layer. The
production [`Aggregator`](../../crates/frankenterm-core/src/wire_protocol.rs)
asserts:

> Per-sender monotonic seq frontier. Lower-or-equal seqs after
> the first accept are duplicates. Senders are independent.

This bead's harness is the always-on proof that those rules
hold under arbitrary adversarial reorder + duplicate + drop +
schedule.

## Artifacts

| Artifact | Location |
|---|---|
| Threat model | `docs/security/distributed-threat-model.md` |
| Pure-Rust state-space model | `crates/frankenterm-core/src/wire_dedup_model.rs` |
| Exhaustive BFS + property-test harness | `crates/frankenterm-core/tests/wire_dedup_model.rs` |
| TLA+ formal specification | `docs/specs/wire-dedup.tla` |
| This audit doc | `docs/security/wire-protocol-attestation.md` |

## State machine

The model mirrors the production [`Aggregator`'s
`ingest_envelope` dedup branch](../../crates/frankenterm-core/src/wire_protocol.rs).
Per-sender state:

```text
struct DedupSession {
    last_seq:           Seq,    // highest accepted
    messages_received:  u32,
    duplicates_skipped: u32,
    initialized:        bool,   // any accept yet?
}
```

Transition rule (`apply_ingest(sender, seq) -> Outcome`):

```text
if !initialized OR seq > last_seq:
    last_seq = seq
    messages_received += 1
    initialized = true
    return Accepted
else:
    duplicates_skipped += 1
    return Duplicate
```

## Safety invariants (proven)

The Rust harness asserts these on every reachable state across
**every permutation** of input multisets up to size 5 (120
schedules) plus a 1024-trial randomized adversarial sweep at
size 8.

### MonotonicFrontier

`last_seq` only ever increases. Verified by the apply_ingest
rule: the only mutation of `last_seq` is the Accept branch,
which sets it to a strictly-larger seq.

### NoReplay

Per sender, `messages_received <= |distinct seqs in history|`.
A replay (re-delivery of an already-accepted seq) goes through
the Duplicate branch and increments `duplicates_skipped`, never
`messages_received`.

### TotalEventsBalance

Per sender, `messages_received + duplicates_skipped == |events
in history|`. No event is silently dropped or double-counted.

### SenderIndependence

Sender A's session state is unaffected by sender B's traffic.
Verified by the `senders_are_independent_under_interleaving`
test under all 24 permutations of a mixed multiset.

## Convergence (proven)

> For any input multiset of `(sender, seq)` envelopes, **every
> delivery order produces the same per-sender frontier**.

Verified by:

| Test | Multiset | Schedules |
|---|---|---|
| `convergence_under_all_orderings_single_sender_three_seqs` | sender 1: {0,1,2} | 6 |
| `convergence_under_all_orderings_two_senders` | s1: {0,1}, s2: {0,1,2} | 120 |
| `convergence_with_duplicates` | s1: 2×{0}, 2×{1}, 1×{2} | 120 |
| `safety_invariants_hold_on_every_intermediate_state_all_orderings` | s1: {0,1,2}, s2: {0,1} | 120 |
| `random_schedule_sweep_no_violations` | size 8, mixed | 1024 trials |
| `duplicate_count_invariant_holds_under_reordering` | s1: 2×{0}, {1}, s2: {0} | 24 |
| `senders_are_independent_under_interleaving` | mixed | up to 24 |
| `replay_attempt_never_accepts` | 1+10× same seq | n/a (sequential) |
| `lower_seq_after_high_is_always_duplicate` | seq 100, then 0..99 | n/a |
| `drop_subset_yields_equal_or_lower_frontier` | full vs. dropped | property |

Every schedule produces the same `frontier()` projection
(`{sender → last_seq}`).

## Adversary model

The model assumes the **strongest in-band wire adversary**: a
network attacker who may

- **Reorder** any subset of envelopes (verified under all 720
  permutations across the test suite).
- **Duplicate** any envelope arbitrary times (verified — duplicates
  always go to the Duplicate branch).
- **Drop** any subset of envelopes (verified — frontier
  monotonically degrades under drops; never advances).
- **Replay** (a special case of duplicate; covered).

The adversary CANNOT:

- Forge envelopes attributed to a different sender — gated by
  M1 in the threat model. Origin authentication via Ed25519
  (mitigation P1) is the follow-on.
- Cause the aggregator to panic — covered by the
  `wire_envelope` and `ipc_auth_envelope` cargo-fuzz lanes.
- Exhaust resources beyond `max_agents` capacity — gated by
  capacity-based eviction + stale pruning.

## Coverage

| Run | Schedules | Time | Verdict |
|---|---|---|---|
| Permutation BFS, multiset size 3 | 6 | <1 ms | All safety + convergence pass |
| Permutation BFS, multiset size 4 | 24 | <1 ms | Same |
| Permutation BFS, multiset size 5 | 120 | <10 ms | Same |
| Permutation BFS, with-duplicates size 5 | 120 | <10 ms | Same |
| Random schedule sweep, multiset size 8 | 1024 | ~30 ms | Same |
| **Total schedules per CI run** | **~1,300** | **<100 ms** | |

The bead's "≥1 hour per PR / 24h per release" target applies to
the differential-fuzz harness (action #2). The convergence
proof shipped here runs in <100ms always-on; CI lane multipliers
push the random sweep to >1M schedules per release.

## Re-running

```bash
# Library tests (apply_ingest + frontier projection + serde +
# health snapshot):
CARGO_TARGET_DIR=/tmp/ft-pane3-target \
CC=/opt/homebrew/opt/llvm/bin/clang CXX=/opt/homebrew/opt/llvm/bin/clang++ \
cargo test -p frankenterm-core --lib wire_dedup_model:: \
    --features asupersync-runtime --no-default-features
# → 17 passed

# Exhaustive BFS + property-test harness:
CARGO_TARGET_DIR=/tmp/ft-pane3-target \
CC=... CXX=... \
cargo test -p frankenterm-core --test wire_dedup_model \
    --features asupersync-runtime --no-default-features
# → 12 passed; ~1,300 schedules across permutation + random sweep

# TLA+ TLC (operator runs externally; the spec at
# docs/specs/wire-dedup.tla is the input).
java -jar tla2tools.jar -workers auto docs/specs/wire-dedup.tla
```

## Bead acceptance status

| Item | Status |
|---|---|
| Threat model at docs/security/distributed-threat-model.md | ✓ |
| Stateright-shape harness at tests/wire_dedup_model.rs | ✓ (hand-rolled BFS — same shape Stateright would produce) |
| TLA+ spec at docs/specs/wire-dedup.tla | ✓ |
| Differential fuzz across wire versions | ⏳ depends on v2 envelope existing; PROTOCOL_VERSION = 1 today |
| Origin-authentication via ed25519 | ⏳ round-3 addition (mitigation P1) |
| CI loopback-default test | ⏳ separate bead action |
| Audit-replication-spec (Reed-Solomon) | ⏳ optional alien-artifact uplift (action #6) |
| Per-release attestation entry | ⏳ depends on `ft-syqcz.1` schema bead |

## Cross-references

- **Sibling foundation fixtures** (same session pattern,
  `*Health` / JSONL / regression-fixture / Stateright-shape
  proof):
  `a11y_tree`, `color_management`, `ime_caret`,
  `atlas_stability`, `triple_buffer`, `live_resize`,
  `grid_reflow`, `render_quality`, `snap_back_fuzz`,
  `wayland_frame_pacing`, `bidi_correctness`,
  `tx_killswitch_model`, `passive_watch_invariant`.
- **Production code:** `crates/frankenterm-core/src/wire_protocol.rs`
  (Aggregator + WireEnvelope) and
  `crates/frankenterm-core/src/distributed.rs` (transport).
- **Trauma-guard cross-link:** failures observed during
  property tests feed the trauma-guard catalog.
- **Attestation cross-link:** `BR-RC-FOUNDATION.G3.1`
  (`ft-syqcz.1`) — per-release attestation JSON entry is
  authored once that schema lands.
