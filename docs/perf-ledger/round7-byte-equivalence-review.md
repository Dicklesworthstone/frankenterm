# Round-7 Byte-Equivalence / Correctness Review (cc_3)

> Reviewer notes for epic **ft-yjihu**. Mandate: every round-7 promotion + new flag must be
> byte-equivalent to baseline (optimization changes internal work, never observable output).
> Each promoted flag must get its OWN default-`true` gate fn; NEVER flip a shared env-helper
> default. Flag any promotion lacking a byte-equivalence proof, and any gate restructure that
> changes behavior when the flag is OFF. Read-only; this file is my own review artifact.

## Verdict table (as of working-tree review, HEAD d1f462a52)

| Flag | Pane / bead | Gate-structure | OFF-path preserved | Byte-equiv proof | Verdict |
|---|---|---|---|---|---|
| EV4 set-based FTS | cod_3 / ft-uvjfr | ✅ own fn `fts_insert_select_batch_enabled_from_env` default-on; shared helper untouched | ✅ `=0`/`false`/`off` → off, == baseline | ✅ `round7_fts_promote.rs` (oracle sound; fresh solo RCH green pending) | **PASS (landed 8318c5514)** |
| .13 clustered-ASCII | cod_2 / ft-97g96 | ✅ `moonshot_recommended_enabled() && !moonshot_env_falsey(ENV)` | ✅ `=0` → off, == baseline | ✅ `..ascii_cluster_run_append_matches_per_cell_materialization` (cluster_hits>0) | **ENDORSE (landed 5c2d995eb, RCH green)** |
| EV1 bulk-ASCII | cod_2 / ft-97g96 | ✅ same pattern, BULK_ASCII_ROW_WRITE_ENV | ✅ `=0` → off | ✅ `..bulk_ascii_row_write_matches_scalar..` (bulk_hits>0) | **ENDORSE (landed 5c2d995eb, RCH green)** |
| D1 print-batching | cod_2 / ft-97g96 | ✅ `default_print_batching()` default-on, dual falsey hatch | ✅ `=0` → off, == old default-off | ✅ chunked all-splits + escape-parser `parser_print_batching_equivalence.rs` | **ENDORSE (landed 5c2d995eb, RCH green)** |
| adaptive-M4 CDC (RSS) | cod_1 / ft-ykde4 | ⏳ not landed (scrollback_tiers.rs unmodified; CDC default still `Off`) | — | — | **AWAITING** |

No promotion is missing a byte-equivalence proof. No gate restructure changes OFF behavior.

## EV4 FTS — full review (ENDORSE)
- **Gate (storage.rs):** new `fts_insert_select_batch_enabled_from_env()` honors `FT_MOONSHOT_ALL`,
  reads `FT_MOONSHOT_FTS_INSERT_SELECT_BATCH` with `.unwrap_or(true)`. The `_ =>` arm of
  `fts_insert_select_batch_enabled()` was repointed from `storage_env_flag_enabled(...)` to this fn.
- **CRITICAL guard satisfied:** `storage_env_flag_enabled()` itself is UNCHANGED — its other three
  callers (`FT_MOONSHOT_GROUP_COMMIT_EVENTS`, `..WRITER_BLOCKING_RECV`, `..GROUP_COMMIT_ADAPTIVE`,
  storage.rs:2102-2107) remain default-OFF. No over-promotion.
- **OFF semantics:** `=0/false/off/no` → `env_value_is_truthy`=false → off, identical to baseline;
  only the unset default flips false→true (the promotion).
- **Proof `round7_fts_promote.rs`:** process-isolated env arms via `Command::env` (workspace forbids
  `unsafe set_var`), `env_remove(FT_MOONSHOT_ALL)` to avoid master-switch contamination. Compares
  default-on vs `=0` (per-row oracle): sync shape, second-sync no-op, and FTS search projections
  including **`score.to_bits()`** (exact BM25 float bits) across all/pane/zone/pane+zone+time filters.
  Small-batch stress (`batch_size:2, max_batch_bytes:128` over 6 segments) forces multi-batch +
  byte-budget splitting. Non-vacuity guards: segments_indexed=6, panes_processed=2, searches.len()=4,
  second_sync.segments_indexed=0. Feature `asupersync-runtime` IS default-on → proof runs for real.
- cc_1's keep-ledger card (2026-06-21) matches this independent review.
- **LANDED 8318c5514 — verified against committed blobs. PASS.** Gate `_ =>` arm routes to the new
  fn; `storage_env_flag_enabled` byte-for-byte unchanged (single-line repoint, 1 deletion). Cargo.toml
  adds `[[test]] round7_fts_promote required-features=["asupersync-runtime"]` — correct (feature is
  default-on → runs under default `cargo test`; the entry only lets cargo skip it cleanly under
  `--no-default-features`). Oracle assertions intact (gate_enabled both arms, sync/second_sync/searches
  byte-equiv incl `score.to_bits()`, non-vacuity 6/2/0/4). **Caveat:** commit message marks the fresh
  full-core RCH re-proof "pending/optional" (deferred to avoid concurrent-core-build truncation, per
  [[concurrent-rch-core-builds-truncate-2026-06-14]]). Underlying EV4 path was RCH-green round-6
  (dc01bd950); the NEW round7_fts_promote oracle should get one SOLO fail-closed RCH green before final
  certification. Correctness PASS by inspection + sound oracle; green-run is a process follow-up, not a
  code defect.

## Term-render stack (.13 / EV1 / D1) — review (ENDORSE pending RCH green)
- **Oracle design (performer.rs inline #[cfg(test)]):** ON vs OFF rendered from the SAME parsed
  actions with exact `assert_eq!`, plus `hits>0` non-vacuity assertion (guards against false-green
  where ON silently falls back to OFF). Adversarial corpus: dense-ASCII, exact-right-edge wrap, attrs
  at edge, left/right margins, insert-mode fallback, charset fallback.
- **Toggle mechanism:** RAII override guards set a `#[cfg(test)]` atomic that the gate checks BEFORE
  the `LazyLock`, so both arms run in one process. EV1 test pins cluster off / varies bulk; .13 test
  pins bulk on / varies cluster. By transitivity of exact equality, all-three-on == scalar baseline.
- **D1:** `default_print_batching()` flipped default-off→on with dual falsey hatch
  (`FT_MOONSHOT_RECOMMENDED` set-wide + `FT_MOONSHOT_PARSER_PRINT_BATCHING` per-flag). `=0` reproduces
  old baseline. The oracle drives both arms explicitly via `set_print_batching`, so it is unaffected
  by the default flip. escape-parser `parser_print_batching_equivalence.rs` battery
  (UTF-8/incomplete/invalid/SWAR-boundary + conformance corpus) was only rustfmt-touched, NOT weakened.

## Cross-cutting observations
1. **`FT_MOONSHOT_RECOMMENDED` shared enabler — NOT a violation.** New set-wide const defined locally
   in screen.rs, performer.rs, escape-parser/parser/mod.rs (same string, independent symbols), gating
   ONLY the 3 promoted term-render flags. Each flag keeps its own per-flag falsey escape hatch. This
   is a shared *enable* with per-flag *override* preserved — distinct from flipping an existing shared
   default helper. Spirit of the keep-gate rule is satisfied.
2. **Falsey vocabularies consistent.** term/parser use `{empty,0,false,off,no}`; EV4 uses whitelist
   truthy `{1,true,yes,on}`. Default (unset→on) and all canonical disable values agree across flags.
   LOW-severity: a *garbage* value (`=banana`) flips EV4 off but term flags on — disable-semantics
   asymmetry only, NOT a byte-equivalence concern.
3. `moonshot_env_truthy` fully removed from screen.rs (no orphan → no `-D warnings` break).
4. csi.rs / osc.rs working-tree diffs are pure `mod test` rustfmt; no production behavior change. The
   `CSI::parse_fast` call reflow in parser/mod.rs is cosmetic and gated behind default-off D2.
5. **D1 dropped `cfg!(feature = "parser-print-batching")`** from `default_print_batching()`. The
   feature is now inert: ZERO source `cfg!` references, not in any default set, not enabled by any
   dependent. The only behavior delta (feature compiled-in via explicit `--features` AND env set
   falsey: old=force-on, new=env-off) is unreachable in any realistic build, and the new behavior
   (explicit env disable wins) is more correct. **Cleanup for cc_1:** the feature's Cargo.toml doc
   comment ("Default-OFF") is now stale; the feature decl could be removed. (The stray `n = []`
   feature in escape-parser/Cargo.toml is pre-existing, unrelated to round-7.)

## Landed-commit verification — 5c2d995eb (term-render promotion)
Confirmed against the committed blobs (not just working tree):
- All three production gates are per-flag default-true, NO shared-helper default flip:
  `ascii_cluster_run_append_enabled` (screen.rs:178), `default_print_batching` (parser/mod.rs:40),
  `bulk_ascii_row_write_enabled` (performer.rs:219). The `term_*_enabled` mirrors in the diff are
  bench-local (`term_parser_ab.rs`), not production. D2 table-dispatch stays opt-in (not promoted).
- **`parser_print_batching_equivalence` (6 passed):** `normalize()` coalesces Print/PrintString runs
  so the only permitted ON-vs-OFF difference is the intended coalescing; any swallowed control /
  dropped-or-mis-decoded codepoint / reordered action / desynced state machine diverges. Covers
  whole-buffer + corpus (guarded `>=6`), **all-split chunk-boundary fuzzing**, a non-vacuity toggle
  test (OFF never emits PrintString, ON does), and the adversarial C1-via-UTF-8 (`0xC2 0x9B`) case.
- **`csi_osc_dispatch_equivalence`:** raw `assert_eq!(off, on)` on `Vec<Action>` (no normalize — D2
  must be byte-identical) over battery + corpus + all chunk boundaries + a gate-off-by-default
  sanity test. Regression hygiene for the opt-in D2 path; intact, proves ON==OFF byte-for-byte.
- Render-level: 4 inline term tests (`parser_print_batching_*` in performer.rs) green (RCH
  vmi1152480) prove EV1/.13/D1 render byte-equivalence.
- **No gate changes behavior when its flag is OFF:** `=0`/`false`/`off`/`no` (or `FT_MOONSHOT_RECOMMENDED`
  falsey) maps each flag to the same path as the pre-promotion baseline; only the unset default flipped.

## Watch list
- cod_1 adaptive-M4 CDC: when it lands, require decode byte-equivalence proof (adaptive-CDC decode ==
  non-CDC decode; likely extends `proptest_scrollback_cdc_dedup.rs`). RSS is a memory metric — it does
  not substitute for a decode byte-equivalence proof. If promoted to default-on, watch
  `cdc_dedup_mode_from_env()` `.unwrap_or(CdcDedupMode::Off)` flip and confirm `=0`/`off` still maps
  to the deterministic non-CDC path.
- cod_5 scan_pipeline removal (ft-8cpho): dead-code removal — confirm zero live production callers
  before/after (round-6 established scan_pipeline.process was DEAD; verify the removal touches no live
  simd_scan/BOCPD path).
