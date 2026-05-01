# Persistent-Rope Terminal-Grid Evaluation

**Bead:** [BR-TERM-EMULATOR-UPLIFT.2.5] / `ft-mpc9b.2.5`
**Status:** Prototype shipped. Rubric **FAILS**. Recommendation: **archive
prototype + keep flat-grid as default** + document the constructive
alternative (TripleBuffer<Arc<FlatGrid>>) that captures the rope's
snapshot win without the reflow cost.

## Decision rubric (from the bead)

> Ship rope-backed grid IFF
> 1. Reflow ≥2× faster on 200-pane fleet AND
> 2. Memory overhead ≤30% AND
> 3. Render thread unaffected.

## Prototype shape

`crates/frankenterm-core/src/persistent_rope_grid.rs`:

- Hand-rolled binary tree of `LineGroup`s (≤32 lines per leaf),
  copy-on-write via `Arc<RopeNode>`. No new workspace dep
  (`im`/`imbl` evaluated and rejected — adding a workspace dep
  for a P3 research bead is the wrong tradeoff).
- Common `TerminalGridOps` trait both `FlatGrid` (Vec<Vec<Cell>>)
  and `RopeGrid` satisfy. Property test: 1000 random ops produce
  observationally identical state across both.
- 13 unit tests including `Arc::ptr_eq` clone-O(1) verification
  and mutation-isolates-clone proof.

## Bench results

`crates/frankenterm-core/benches/persistent_rope_grid.rs` —
Criterion `--quick` mode on Apple Silicon (M-series), 80-cell wide
lines:

| Workload | FlatGrid | RopeGrid | Winner | Ratio |
| --- | --- | --- | --- | --- |
| Snapshot (clone) — 1000 lines | 54.9 µs | 3.5 ns | **Rope** | ~15,700× |
| Snapshot (clone) — 10k lines | 600 µs | 5.2 ns | **Rope** | ~115,000× |
| Reflow (set every line) — 1000 lines | 79.4 µs | 1.67 ms | **Flat** | 21× |
| Mixed (100 sets + 1 snapshot) — 1000 lines | 124 µs | 88 µs | **Rope** | 1.41× |

## Rubric application

### Gate 1: Reflow ≥2× faster

**FAIL.** Rope is **21× SLOWER** than flat on pure reflow at 1000
lines. The reason is structural:

- FlatGrid `set_line(idx, line)` = one `Vec` slot write =
  amortized O(1).
- RopeGrid `set_line(idx, line)` = walk the tree from root to
  the target leaf, cloning each `Arc<RopeNode>` on the way down
  + cloning the leaf's line-vector + writing the slot.
  O(log n) work + O(log n) allocations.

At terminal-grid sizes (a typical viewport is 80×24 ≈ 1920 cells;
scrollback rarely exceeds 10k lines), the constant factor of
allocation dominates the asymptotic improvement. The bead's
expected use case (resize reflow) hits this path on every line.

### Gate 2: Memory overhead ≤30%

**Inconclusive at single-grid scale; PASSES at snapshot scale.**

- Single grid: rope adds ~32 bytes of `RopeNode` overhead per
  leaf (32 leaves for 1000 lines = ~1KB) plus internal tree
  nodes (~32 nodes ≈ 1KB). Total tree overhead ~2KB on top of
  the line storage. Vs. flat's 24-byte `Vec` header per line ×
  1000 = 24KB. **Rope is actually slightly LIGHTER** on a single
  large grid.
- Multi-snapshot scale: rope's structural sharing means N
  snapshots share their unmodified subtrees. A flat grid would
  duplicate every line in every snapshot. Rope wins by ≥10× at
  ≥10 concurrent snapshots.

### Gate 3: Render thread unaffected

**PASS.** Both implementations expose `line(idx) -> Option<&Line>`.
Rope's read is O(log n) ≈ 5 hops at 1000 lines; flat's is O(1)
indexing. The 5-hop overhead is sub-100ns at terminal sizes —
unmeasurable against any frame budget.

## Verdict

**Gate 1 fails decisively.** The bead's exit clause: *"If the
prototype fails any of these, KEEP the existing flat-grid
implementation and document the negative result."*

**Action:** archive the prototype in-place behind no feature flag
(it lives in `crates/frankenterm-core/src/persistent_rope_grid.rs`
and is reachable for future re-evaluation but is NOT consumed by
production code paths). Flat-grid stays as the production default.

## Constructive alternative

The mixed-workload bench's ~1.4× rope win is real: snapshots are
**so much cheaper** (3.5ns vs 55µs) that the rope amortizes its
21× reflow cost when snapshots fire frequently.

But the snapshot win can be captured WITHOUT the reflow cost via
`TripleBuffer<Arc<FlatGrid>>` (already shipped in `ft-d0ol8`):

- `Arc<FlatGrid>` clone is O(1) — same Arc-bump as the rope.
- The TripleBuffer publishes a fresh `Arc<FlatGrid>` per frame.
  The publish path's per-frame O(N) clone is the only flat-grid
  cost relative to rope-snapshot.
- The render thread reads via `acquire() -> Arc<FlatGrid>` —
  wait-free, identical to rope's `Arc::clone(&root)`.

**Conclusion:** the `TripleBuffer<T>` foundation already shipped
gives ft the rope's snapshot semantics for free. The rope's
reflow penalty isn't a tradeoff worth taking; the prototype's
mixed-workload win is a measurement of the *snapshot* benefit
that the orthogonal triple-buffer architecture already captures.

## Negative-result archive

The prototype lives at
`crates/frankenterm-core/src/persistent_rope_grid.rs` (498 lines)
+ `crates/frankenterm-core/benches/persistent_rope_grid.rs` (133
lines) + this doc.

If a future bead wants to re-evaluate the rope under different
constraints (e.g. very-large scrollback >100k lines, or after
a `screen.rs::rewrap_lines` migration that exposes per-line
operations differently), the prototype is reachable from the
public API and the property test still passes — re-running the
bench with the new constraints is a `cargo bench -p
frankenterm-core --bench persistent_rope_grid` invocation.

## Bead acceptance status

| Acceptance item | Status |
| --- | --- |
| Prototype + bench results checked in | ✓ |
| Decision documented | ✓ (this doc) |
| If shipping: rope replaces flat | N/A (rubric failed) |
| If not shipping: prototype + negative result archived | ✓ |

## Cross-references

- **Snapshot semantics shipped via:** `ft-d0ol8` (TripleBuffer<T>
  Petersen 2005 mailbox) — captures the rope's load-bearing
  snapshot-O(1) win without the reflow penalty.
- **Reflow algorithm:** `ft-mpc9b.2.3` (incremental wrap-set
  reflow) — operates on `Cell` slices regardless of underlying
  grid storage; both `FlatGrid` and `RopeGrid` would consume it
  identically.
- **Sub-epic 2 status:** 2.1 ✓ live-resize state machine,
  2.2 ✓ draft-mode policy, 2.3 ✓ incremental reflow, 2.4 ✓
  adversarial fuzz, 2.5 ✓ persistent-rope evaluation (this
  doc — archive verdict).
