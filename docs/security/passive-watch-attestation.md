# Passive-Watch Read-Only Fuzz Proof

**Bead:** [BR-RC-SAFETY-PROOFS.G9] / `ft-x0666.1`
**Status:** Foundation slice shipped. Contract module +
adversarial-corpus catalog + cargo-fuzz target +
seed corpus + invariant runner all live. The recovery JSON
attestation is published at
`docs/security/passive-watch-attestation.json` and wired through
`docs/attestations/manifest.json`; integration with the real
`ft watch` driver remains the follow-on. The 1h/PR + 24h/release
CI cadence is a separate CI configuration concern.

## Headline rule (the bead's strict-no)

> `ft watch` is read-only. Mutating actions must pass the
> Policy Engine.
>
> **Zero outbound mutating IPC. Zero non-capture storage
> writes per fuzz input.** Pattern detections OK;
> sends/spawns/closes NOT OK.

If a single randomly-crafted pane-output stream can drive `ft
watch` into emitting a `send_text`, `spawn`, `close`, or a
storage write to a non-capture table, every downstream "safe by
default" claim collapses. This bead's harness is the always-on
proof that no such input exists in the explored space.

## Artifacts

| Artifact | Location |
|---|---|
| Contract module | `crates/frankenterm-core/src/passive_watch_invariant.rs` |
| cargo-fuzz target | `fuzz/fuzz_targets/passive_watch_invariant.rs` |
| Seed corpus | `fuzz/corpus/passive_watch_invariant/` (10 hand-curated seeds) |
| JSON attestation | `docs/security/passive-watch-attestation.json` |
| Static verifier | `tests/e2e/test_passive_watch_attestation_manifest.sh` |
| This audit doc | `docs/security/passive-watch-attestation.md` |

## Action taxonomy

`WatchAction` enumerates every observable emission the watch
loop can produce:

| Variant | Mutating? |
|---|---|
| `Capture { pane_id, byte_count }` | ❌ read-only baseline |
| `PatternDetection { rule_id }` | ❌ inspection only |
| `WatchMetadataWrite { kind }` (`Heartbeat` / `Telemetry` / `CrashCheckpoint`) | ❌ self-only |
| `OutboundSend { target_pane_id, bytes_len }` | **✓ FORBIDDEN** |
| `OutboundSpawn { kind }` | **✓ FORBIDDEN** |
| `OutboundClose { target_pane_id }` | **✓ FORBIDDEN** |
| `NonCaptureStorageWrite { table }` | **✓ FORBIDDEN** |
| `Other { description }` | flagged as soft signal (review-but-not-fail) |

Adding a new action variant requires extending this taxonomy and
classifying it; the exhaustive `WatchAction::is_mutating`
switch pins that no variant stays unclassified.

## Adversarial corpus

`AdversarialCorpusKind` enumerates the five attack categories
named in the bead description. Each kind has at least one
hand-curated seed in
[`adversarial_seed_catalog`](../../crates/frankenterm-core/src/passive_watch_invariant.rs);
seed bytes are also written to disk under
`fuzz/corpus/passive_watch_invariant/` so libfuzzer's coverage-
guided engine has concrete inputs to mutate.

| Kind | Seed | Bytes | Targets |
|---|---|---|---|
| MutatedRealOutput | `shell_prompt_bitflip` | 50 | misclassification of a high-bit-injected real prompt as an OSC marker |
| CsiInjection | `csi_overlong_params` | 4100 | parser-buffer bounds; whether ridiculous params trigger a mutating action |
| CsiInjection | `csi_invalid_final` | 8 | error-recovery path doesn't escalate a malformed CSI |
| OscInjection | `osc_oversized_title` | 8197 | metadata-write path bounds; no spawn/send escalation |
| OscInjection | `osc_unterminated` | 20 | parser holds the buffer without ever emitting a mutating action |
| DcsInjection | `dcs_nested` | 14 | state-machine recovers without firing a side effect |
| DcsInjection | `dcs_sixel_like` | 2053 | image-data path is capture-only |
| PromptMimic | `fake_codex_usage_limit` | 51 | watch emits PatternDetection, NOT OutboundSend; workflow is the gated layer |
| PromptMimic | `fake_compacting` | 15 | Claude Code compaction pattern: detect, don't act |
| PromptMimic | `fake_interactive_prompt` | 8 | no auto-answer workflow fires from a passive watch |

## Invariants the harness enforces

`check_invariants(&PassiveWatchObservation)` returns
`Vec<PassiveWatchInvariant>`, with three named violations:

1. **NoOutboundMutatingIpc** — fires for any `OutboundSend`,
   `OutboundSpawn`, or `OutboundClose` action. **Hard fail.**
2. **NoNonCaptureStorageWrite** — fires for any
   `NonCaptureStorageWrite { table }` action. **Hard fail.**
3. **OtherActionUnclassified** — fires when an `Other` action
   appears, indicating the recorder couldn't classify an
   emission. Soft signal — flagged for human review (false
   positives would be worse than false negatives here).

The cargo-fuzz target asserts `violations.is_empty()` after
each iteration. Any violation is a panic and aborts the fuzz
run with the input's FNV-1a64 hash for reproducibility.

## Health snapshot

`PassiveWatchHealth` is the `ft doctor` counter surface,
matching the `*Health` shape used across this session
(`a11y_tree`, `color_management`, `atlas_stability`,
`triple_buffer`, `live_resize`, `render_quality`,
`snap_back_fuzz`, `wayland_frame_pacing`, `bidi_correctness`,
`tx_killswitch`):

| Counter | Meaning |
|---|---|
| `iterations_total` | fuzz iterations folded in |
| `captures_total` | Capture actions seen |
| `detections_total` | PatternDetection actions seen |
| `metadata_writes_total` | WatchMetadataWrite actions seen |
| `mutating_violations_total` | OutboundSend/Spawn/Close + NonCaptureStorageWrite |
| `unclassified_other_total` | Other actions (review queue) |

`is_safe()` returns `iterations_total > 0 && mutating_violations_total == 0`,
so a cold baseline with no iterations is not reported safe. The production
attestation surface dumps this struct verbatim in the recovery JSON artifact.

## Harness shape (current vs. follow-on)

The shipped target drives `scan_pipeline::quick_scan` — the
production parser the watch loop already uses to turn bytes
into "what to capture / what pattern matched." For each fuzz
input it synthesizes a `PassiveWatchObservation` containing:

- one `Capture` for the bytes consumed,
- one `PatternDetection` per non-zero `TriggerCategory` count,
- one `WatchMetadataWrite::Telemetry` for the counter dump,

then asserts no invariant fires and `health.is_safe()` holds.
This proves the parser surface itself has no path to a mutating
action — by construction. The integration follow-on bead (filed
by pane 2) wires the real `ft watch` driver into the same
harness so the dispatcher (not just the parser) is exercised
by libfuzzer's coverage-guided mutation.

## Re-running

```bash
# Build (nightly required for cargo-fuzz).
CARGO_TARGET_DIR=/tmp/ft-pane3-target-fuzz \
CC=/opt/homebrew/opt/llvm/bin/clang CXX=/opt/homebrew/opt/llvm/bin/clang++ \
cargo +nightly fuzz build passive_watch_invariant

# Run with the seeded adversarial corpus. -max_total_time=3600
# is the bead's "≥1 hour per PR" target; 86400 is the
# "≥24 hours per release" target.
cargo +nightly fuzz run passive_watch_invariant -- -max_total_time=3600
```

## CI cadence (follow-on)

Per-PR: `-max_total_time=3600` (1h). Per-release:
`-max_total_time=86400` (24h). Operator wires this into the
fuzz CI lane in the integration bead; the harness is the
contract that lane consumes.

## Cross-references

- **Sibling fixtures** (same session pattern, all `*Health` /
  JSONL row / regression harness shape):
  `a11y_tree`, `color_management`, `ime_caret`,
  `atlas_stability`, `triple_buffer`, `live_resize`,
  `grid_reflow`, `render_quality`, `snap_back_fuzz`,
  `wayland_frame_pacing`, `bidi_correctness`,
  `tx_killswitch_model`.
- **Production parser:** `scan_pipeline::quick_scan` in
  `crates/frankenterm-core/src/scan_pipeline.rs`.
- **Production trigger taxonomy:** `pattern_trigger::TriggerCategory`
  in `crates/frankenterm-core/src/pattern_trigger.rs`.
- **Attestation cross-link:** `docs/attestations/manifest.json`
  contains the `security/passive-watch` slot pointing at
  `docs/security/passive-watch-attestation.json`.
- **Static verifier helper:** `tests/e2e/test_passive_watch_attestation_manifest.sh`
  uses `tests/scripts/static_attestation_helpers.rb` so source-document,
  seed-corpus, direct-exec, and multi-word term checks share the common
  static attestation contract.
