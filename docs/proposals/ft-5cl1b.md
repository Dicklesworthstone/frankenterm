# ft-5cl1b Replay Determinism Audit

Audit date: 2026-04-28
Owner: cod_4

## Scope

This audit covers replay and fleet surfaces that can affect byte-equivalent replay
artifacts across Apple Silicon, Intel macOS, and Linux x86_64:

- replay decision graph construction, diffing, reports, provenance, artifact registry,
  merge ordering, performance ledgers, and asciicast export
- fleet launch and memory-pressure decisions when those decisions are captured into
  replay artifacts

Deterministic replay here means the same logical input produces the same canonical
decision graph, diff result, report bytes, and artifact hashes regardless of host
architecture, process hash seed, allocator layout, or wall-clock timing.

## Summary

Replay is partially deterministic today: many externally serialized collections use
`BTreeMap`, `BTreeSet`, explicit sorting, and virtual timestamps. The audit still found
several gaps that can produce divergent artifacts or hide divergence:

1. `DecisionEvent::new` uses `DefaultHasher` for replay hashes.
2. `PaneMergeResolver` derives equal-key ordering from `HashMap` drain order.
3. Decision graph and diff keys omit an event ordinal, so duplicate timestamp/pane/rule
   events inherit input order or overwrite each other.
4. Report/provenance hashes include run metadata and wall-clock fields without a
   separate deterministic-equivalence projection.
5. Performance and asciicast outputs use `f64` for persisted or emitted values.
6. Artifact registry outputs preserve manifest insertion order.
7. Fleet allocation uses floating-point Hamilton apportionment; acceptable operationally,
   but not as a canonical replay decision primitive.

## Findings

### 1. Unstable Replay Hashes

`DecisionEvent::new` imports `std::collections::hash_map::DefaultHasher` and uses it for
`definition_hash`, `input_hash`, and `output_hash`
(`crates/frankenterm-core-replay-types/src/replay_decision_graph.rs:177`). The helper
hashes strings through `DefaultHasher::new()` and formats the result as hex
(`replay_decision_graph.rs:181`).

That is not a replay artifact hash contract. `DefaultHasher` is intentionally an
implementation detail, and it should not be used for persisted or cross-host comparable
replay outputs. The output hash also starts from `serde_json::to_string(&output)`
(`replay_decision_graph.rs:186`), so object key construction order can leak into the hash
unless the value is canonicalized first.

Required follow-up: replace these hashes with a documented stable digest over canonical
bytes, preferably SHA-256 using a canonical JSON or TOON projection. Add a golden fixture
that records exact hash bytes for representative decision events.

### 2. Pane Merge Equal-Key Ordering Depends On HashMap Iteration

`PaneMergeResolver` stores pane streams in a `HashMap<u64, Vec<MergeEvent>>`
(`crates/frankenterm-core-replay/src/replay_merge.rs:97`). During `merge()`, it drains
that map directly into `pane_cursors`
(`replay_merge.rs:153`) and uses the vector index as a heap tie-breaker
(`replay_merge.rs:163`).

The module-level contract says the same input streams should always produce the same
merged sequence regardless of add order, but equal merge keys can currently be ordered by
process-local `HashMap` iteration. That can diverge across processes, platforms, or Rust
versions.

Required follow-up: make the cursor order deterministic before heap insertion. Either
store `pane_streams` in a `BTreeMap`, or sort drained cursors by pane id and use
`(merge_key, pane_id, source_position)` as the heap ordering key.

### 3. Duplicate Decision Keys Collapse Or Inherit Input Order

`DecisionNode::canonical_key()` is `(timestamp_ms, pane_id, rule_id)`
(`crates/frankenterm-core-replay-types/src/replay_decision_graph.rs:97`). Graph
construction sorts by that key (`replay_decision_graph.rs:338`), so two decisions with
the same timestamp, pane, and rule remain ordered by the input slice.

The diff layer uses the same key as a one-to-one map
(`crates/frankenterm-core-replay/src/replay_decision_diff.rs:200`). Later duplicate keys
overwrite earlier entries in `base_exact` and `cand_exact`
(`replay_decision_diff.rs:213`, `replay_decision_diff.rs:220`), while
`matched_cand` is only a set of keys (`replay_decision_diff.rs:247`).

This is deterministic only if upstream event order is already canonical and the key is
unique. Neither condition is enforced locally, and duplicate decisions can be silently
lost during exact diffing.

Required follow-up: add a stable event ordinal or source offset to canonical keys, and
store duplicate-key matches as vectors instead of overwriting. Add tests with two
decisions at the same timestamp/pane/rule but different output hashes.

### 4. Wall-Clock Metadata Is Mixed With Replay Hashes And Reports

`ReportMeta` includes `replay_run_id`, paths, duration, event count, and an ISO timestamp
(`crates/frankenterm-core-replay/src/replay_report.rs:34`). JSON reports serialize the
timestamp directly (`replay_report.rs:264`). Provenance hashes serialize the full
`ProvenanceEntry`, including `wall_clock_ms`
(`crates/frankenterm-core-replay/src/replay_provenance.rs:91`,
`replay_provenance.rs:103`). Replay audit hashes include `started_at_ms` and
`completed_at_ms` (`replay_provenance.rs:442`, `replay_provenance.rs:462`).

Those hashes are valid audit-chain hashes, but they should not be reused as deterministic
replay equivalence hashes. Two identical logical replays run at different times will
necessarily produce different report/provenance bytes.

Required follow-up: define two projections explicitly:

- audit projection: includes run id, actor, wall-clock timestamps, and chain linkage
- deterministic replay projection: excludes wall-clock/run-local fields and hashes only
  canonical virtual-timeline state

### 5. Floating-Point Values Can Leak Into Artifact Bytes

Replay performance classification uses `f64` samples, budgets, regression fractions, and
relative-spread checks
(`crates/frankenterm-core-replay/src/replay_performance.rs:84`,
`replay_performance.rs:224`, `replay_performance.rs:359`). Those values are performance
telemetry, so they are acceptable if they remain outside replay equivalence.

The core replay asciicast exporter converts millisecond timestamps to `f64` seconds and
serializes them as JSON array values (`crates/frankenterm-core/src/replay.rs:911`,
`replay.rs:916`, `replay.rs:928`, `replay.rs:940`). The playback path also computes
speed-adjusted delays through `f64` (`replay.rs:617`, `replay.rs:699`), which affects
wall-clock scheduling but not canonical replay state unless downstream captures are
fed back into an artifact.

Required follow-up: keep performance outputs out of deterministic replay hashes. For
persisted/exported replay artifacts, prefer integer milliseconds or a fixed decimal
formatter with documented rounding.

### 6. Artifact Registry Order Follows Manifest Insertion Order

The replay artifact registry lists active artifacts in manifest order
(`crates/frankenterm-core-replay/src/replay_artifact_registry.rs:459`), prunes in manifest
order (`replay_artifact_registry.rs:596`), and renders JSON from the current vector order
(`replay_artifact_registry.rs:708`). That is deterministic for a single manifest file,
but any bulk registration that depends on filesystem traversal can make cross-host output
order drift.

Required follow-up: define registry output order as part of the artifact contract. Sort
externally visible lists by `(path, kind, version)` or another documented stable key
before rendering.

### 7. Fleet Decisions Are Mostly Operational, Not Canonical Replay State

Fleet memory pressure records decisions into an audit trail with monotonic sequence
numbers and bounded `VecDeque` retention
(`crates/frankenterm-core/src/fleet_memory_controller.rs:347`). That is deterministic if
the input `signals` stream is deterministic.

Fleet launch allocation uses `f64` quotas and `partial_cmp` to distribute remainders
(`crates/frankenterm-core/src/fleet_launcher.rs:973`,
`fleet_launcher.rs:989`). Phase grouping uses a `HashMap`, but the rendered phases are
sorted by phase index (`fleet_launcher.rs:1028`, `fleet_launcher.rs:1036`). The current
planner therefore has stable phase ordering, but the fractional allocation method should
not be used as a canonical replay primitive without integer apportionment and explicit
tie-breaking.

Required follow-up if fleet launch plans become replay artifacts: replace floating-point
quota comparison with integer remainder comparison and stable ties by mix index/profile
id.

## Proposed Contract

Add a replay determinism contract with these rules:

1. Canonical replay artifacts must not use `DefaultHasher`, process hash seeds, memory
   addresses, allocator order, wall-clock timestamps, or filesystem traversal order.
2. Canonical collection output must use `BTreeMap`/`BTreeSet` or explicit sorting with a
   documented total key.
3. Canonical event keys must include a stable event ordinal whenever timestamp/pane/rule
   is not unique.
4. Floating-point values are allowed in telemetry, but deterministic artifacts must store
   integer units or fixed decimal strings.
5. Audit-chain hashes and deterministic replay-equivalence hashes must be separate API
   surfaces with separate tests.

## Follow-Up Beads Filed

- `ft-5jqed`: replace `DefaultHasher` with a stable canonical digest.
- `ft-57rag`: remove `HashMap` iteration from equal-key pane merge output ordering.
- `ft-cjdwn`: preserve and compare duplicate timestamp/pane/rule decisions.
- `ft-ayydc`: add a cross-architecture replay determinism golden matrix.
