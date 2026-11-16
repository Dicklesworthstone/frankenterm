# Audit Ledger Replication via Reed-Solomon Erasure Coding

**Bead:** [BR-RC-SAFETY-PROOFS.G11.1] / `ft-x0666.5`
**Status:** Foundation slice shipped. Reference implementation
+ contract layer + recovery proof + audit doc all live;
production wiring (integrating with the existing
`policy_decision_log` + `policy_audit_chain` modules) and the
ft doctor warning surface are integration follow-ons. **P3 /
optional alien-artifact uplift** — only relevant for
multi-aggregator distributed deployments.

## Why this matters

When distributed mode is enabled, the policy-denial audit
chain currently replicates **1:1** across aggregators. A
single host loss takes its slice of audit history with it.
For a production deployment running multiple aggregators, the
audit ledger should survive any single host failure without
data loss.

**Reed-Solomon k-of-n erasure encoding** is the standard
solution: encode each audit row into n shards distributed
across n aggregators; any k surviving shards reconstruct the
original row.

## Default parameters

| Parameter | Value | Rationale |
|---|---|---|
| `k` (data shards) | **3** | Minimum that gives meaningful redundancy without ballooning storage |
| `n` (total shards) | **5** | Typical 5-aggregator deployment topology |
| Parity (`n - k`) | **2** | Survives any 2 host losses |
| Storage overhead | **n / k = 1.67×** | vs 5× for full replication |
| Spec max `n` | 32 | Sanity bound; no real distributed audit deployment uses ≥32 aggregators |

Tied to deployment topology (the bead's action #1): operators
override via the `[audit_replication]` config block; the
contract module's `ErasureConfig::new(k, n)` validates the
override.

## Reference implementation

`crates/frankenterm-core/src/audit_erasure_spec.rs` ships a
**dependency-free** Reed-Solomon implementation over `GF(2^8)`
using primitive polynomial `0x11d` (the same field as
zfec, raptorq, and ISA-L's erasure module). Why ship the
reference impl rather than depend on `reed-solomon-erasure`?

- The bead is a **spec** (action #3). The reference impl is
  the authoritative semantics — operators integrating with
  zfec / raptorq / a future hardware-accelerated lane verify
  their output against this module's `encode_row`.
- Audit-row encoding is one row per write — not in any hot
  path. Performance optimization is irrelevant.
- `forbid(unsafe_code)` in `frankenterm-core` is preserved.

## Encoding contract

```rust
let cfg = ErasureConfig::default();          // (3, 5)
let shards = encode_row(cfg, audit_row_bytes);
//   shards.len() == 5
//   shards[0..3] are data; shards[3..5] are parity

// Distribute one shard per aggregator.
// On read: collect any 3 surviving shards.
let surviving: Vec<ErasureShard> = ...;
let reconstructed = reconstruct(cfg, &surviving)?;
assert_eq!(reconstructed, audit_row_bytes);
```

The encoding embeds a **4-byte little-endian length prefix**
into the data so `reconstruct` can recover the original
length without external metadata. This makes shards self-
describing — operators can re-shuffle and re-parameterize
without losing the row boundary.

## Properties (proven)

The contract module's tests verify these on every CI run:

| Property | Test | Verdict |
|---|---|---|
| MDS — any k of n reconstructs | `any_three_of_five_reconstructs` | ✓ all C(5,3)=10 subsets |
| Round-trip — `reconstruct(encode(x)) == x` | `round_trip_preserves_data` | ✓ for empty, 1-byte, 12-byte, 43-byte, 256-byte inputs |
| Varied (k, n) parameters | `varied_config_round_trip` | ✓ for k ∈ 1..=5, n ∈ k..=8 |
| Single-host loss recoverable | `single_host_loss_recoverable_at_default` | ✓ at default (3, 5) |
| Insufficient shards rejected | `fewer_than_k_shards_fails` | ✓ |
| Duplicate shard rejected | `duplicate_shard_index_rejected` | ✓ |
| Out-of-range index rejected | `shard_index_out_of_range_rejected` | ✓ |
| Inconsistent shard size rejected | `inconsistent_shard_size_rejected` | ✓ |
| GF(256) inverse correctness | `gf_arithmetic_basic` | ✓ for all 255 nonzero elements |
| Serde roundtrip | `shard_serde_roundtrip` | ✓ |
| Parity flag correctness | `parity_flag_correctness` | ✓ |

The MDS property follows from the **Vandermonde structure**
of the generator matrix: parity rows have entries
`G[i][j] = (i - k + 1)^j` over GF(256) for evaluation points
1..=parity, distinct from each other. Any k rows of the
(n × k) matrix have a nonzero determinant (Vandermonde
determinant identity over a field), so the corresponding k
shards form an invertible system and reconstruct the
original.

## ft doctor warning

`AuditErasureHealth` is the snapshot the operator's `ft
doctor` reads:

```text
config                    : currently active (k, n)
distributed_mode_on       : whether replication is meaningful
effective_replication     : 1 = single-copy; n - k + 1 = full
rows_encoded_total        : process-lifetime counter
reconstructions_total     : count of host-loss recoveries
should_warn()             : distributed_mode_on && effective == 1
```

The bead's action #4: "doctor check that warns when
distributed mode is on but audit replication is single-copy."
The predicate `should_warn()` is the trigger; doctor wires it
to a `WARN`-level message: *"Distributed mode is active but
audit replication is single-copy. Configure
`[audit_replication]` (default k=3, n=5) to survive host
losses."*

## Integration plan

The bead's action #2 — wiring into the existing
`policy_decision_log` + `policy_audit_chain` modules — is the
integration follow-on. Sketch:

1. The audit-row writer calls
   `audit_erasure_spec::encode_row(cfg, row_bytes)` to produce
   n shards.
2. Each shard is shipped to one aggregator (mapping operator-
   supplied via the `[distributed.audit_shards]` table in
   `frankenterm.toml`).
3. On read (e.g., `ft audit query --row-id <id>`), the reader
   collects any k surviving shards via the distributed query
   and calls `reconstruct(cfg, &surviving)`.
4. If reconstruction fails (fewer than k shards survive), the
   reader reports `AuditUnavailable` to the caller and
   increments a counter the doctor surfaces.

## Property test (always-on)

`tests/audit_erasure_property.rs` is a follow-on slot — the
contract module's `varied_config_round_trip` test is the
always-on regression net for the spec; a richer property test
sweeping random (k, n) pairs and random data lengths can plug
into proptest later.

## Re-running

```bash
CARGO_TARGET_DIR=/tmp/ft-pane3-target \
CC=/opt/homebrew/opt/llvm/bin/clang CXX=/opt/homebrew/opt/llvm/bin/clang++ \
cargo test -p frankenterm-core --lib audit_erasure_spec:: \
    --features asupersync-runtime --no-default-features
# → 19 passed (incl. exhaustive C(5,3) subset reconstruction)
```

## Bead acceptance status

| Item | Status |
|---|---|
| Spec doc shipped | ✓ this doc |
| k-of-n parameter envelope | ✓ `ErasureConfig::new(k, n)` with validation |
| Reference implementation | ✓ `audit_erasure_spec` module — pure Rust, dependency-free, `forbid(unsafe_code)` clean |
| Property test: any single-host loss preserves audit history | ✓ `any_three_of_five_reconstructs` + `single_host_loss_recoverable_at_default` |
| Doctor warning surface | ✓ `AuditErasureHealth::should_warn()` predicate; doctor wiring is integration follow-on |
| Reference impl in distributed.rs | ⏳ wiring follow-on |
| Per-release attestation entry | ⏳ depends on `ft-syqcz.1` |

## Cross-references

- **Sibling foundation fixtures** (same `*Health` /
  spec-module pattern):
  `a11y_tree`, `color_management`, `ime_caret`,
  `atlas_stability`, `triple_buffer`, `live_resize`,
  `grid_reflow`, `render_quality`, `snap_back_fuzz`,
  `wayland_frame_pacing`, `bidi_correctness`,
  `tx_killswitch_model`, `passive_watch_invariant`,
  `wire_dedup_model`, `redactor_coverage_matrix`,
  `tui_parity_oracle`, `robot_checkpoint_state_machine`,
  `robot_work_state_machine`, `robot_fleet_state_machine`,
  `wayland_compositor_matrix`.
- **Related field arithmetic:** the GF(256) tables in this
  module match those used by zfec / raptorq / ISA-L (same
  primitive polynomial `0x11d`). Operator integrators using
  any of those libraries can verify shard equivalence
  byte-for-byte against this reference.
- **Distributed mode parent:**
  `crates/frankenterm-core/src/distributed.rs` (6.1k LOC) —
  the audit-erasure wiring follow-on lives here.
- **Wire protocol cross-reference:** `ft-x0666.3` /
  `wire_dedup_model` — the audit-row erasure layer is *above*
  the wire-protocol dedup layer; both are independent safety
  proofs.
- **Attestation cross-link:** `BR-RC-FOUNDATION.G3.1`
  (`ft-syqcz.1`).
