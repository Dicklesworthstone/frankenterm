# Cold-Tier Write-Path Pipeline Contracts

**Bead:** [BR-TERM-EMULATOR-UPLIFT-2.13.cont] / `ft-tfb64`
**Status:** Foundation slice shipped — typed-state pipeline
+ disk-path layout + metadata index DDL + structured-log
row + key-handle shape + 22 lib tests. The actual
integration (zstd compression, asupersync I/O, AES-256-GCM
crypto, SQLite metadata index, retention cleanup task,
search-bridge integration) is the integration follow-on
that consumes these contracts.

Cold-tier policy substrate already lives at
`scrollback_cold_tier.rs` (`b3e8a6845`) — eviction gates,
retention purge, LRU select, dedup. This module ships the
**integration substrate** so the integration cannot
accidentally break the bead's "DO NOT BREAK" invariants.

## Headline rule

> **Privacy: redactor MUST apply before disk write.**
> The substrate enforces this at the **type level**: the
> integration cannot construct a `ChunkBytes<Compressed>`
> without first consuming a `ChunkBytes<Redacted>`, which
> can only come from a `ChunkBytes<Raw>` redaction transition
> such as `redact_with`, `redact_with_evidence`, or
> `redact_with_streaming`.
> Skipping the redactor is a compile error.

## Typed-state pipeline (DO NOT BREAK rule)

`ChunkBytes<Stage>` is a phantom-typed wrapper. Each stage admits only the
next valid transition set. Production chunked persistence uses the
evidence-bearing streaming transition; the simple `redact_with` helper remains
for tests and non-streaming adapters:

```
ChunkBytes<Raw>          -> redact_with(redactor)        -> ChunkBytes<Redacted>
ChunkBytes<Raw>          -> redact_with_evidence(...)    -> (ChunkBytes<Redacted>, RedactionEvidence)
ChunkBytes<Raw>          -> redact_with_streaming(...)   -> (ChunkBytes<Redacted>, RedactionEvidence)
ChunkBytes<Redacted>     -> compress_with(zstd)          -> ChunkBytes<Compressed>
ChunkBytes<Compressed>   -> encrypt_with(key, cipher)    -> ChunkBytes<Encrypted>
                         -> skip_encryption()            -> ChunkBytes<Encrypted>  (operator opt-out)
ChunkBytes<Encrypted>    -> mark_written()               -> ChunkBytes<Written>
```

Compile-time invariants (verified by absence — these
patterns *cannot* compile):

- `ChunkBytes<Raw>::compress_with(...)` — `Raw` has no
  `compress_with`.
- `ChunkBytes<Raw>::mark_written()` — `Raw` has no
  `mark_written`.
- Constructing a `ChunkBytes<Compressed>` directly without
  going through `Redacted` first.

## Disk-path layout (sub-task 5)

`ColdTierDiskPath::render(cache_root) -> PathBuf`:

- Layout: `<cache_root>/scrollback/<pane_id>/<chunk_id>.zst[.enc]`
- File mode constant: `FILE_MODE = 0o600`.
- `matches_layout(path)` validates a path against the
  contract — used by tests to pin layout, by integration
  to reject malformed paths in the migration framework.

The `matches_layout_*` test family pins six edge cases:
well-formed, encrypted, non-numeric chunk id, non-numeric
pane id, missing `.zst` suffix.

## Metadata index schema (sub-task 6)

`MetadataIndexRow` carries the bead's column list:

| Column | Source |
|---|---|
| `chunk_id` | bead's "id" |
| `pane_id` | scope |
| `byte_start, byte_end` | bead's "byte_range" |
| `line_start, line_end` | bead's "line_range" |
| `content_hash` | bead's "content_hash" |
| `written_ts_ms` | bead's "written_ts" |
| `last_access_ts_ms` | bead's "last_access_ts" |
| `tier_slug` | bead's "tier" |
| `redaction_slug` | bead's "redaction" |
| `encryption_slug` | bead's "encryption" |

`TABLE_DDL` and `INDEX_DDL` are idempotent (`IF NOT
EXISTS`); the `metadata_index_has_all_bead_required_columns`
test pins the column list.

`validate(row)` returns the first invariant violation:

- `byte_start > byte_end`
- `line_start > line_end`
- `last_access_ts < written_ts`

## Key handle (sub-task 4)

`ColdTierKeyHandle { key_id, mmap_key_slug }` —
opaque key material handle. Production wraps a
`chacha20poly1305::Key` or platform key-store handle;
foundation slice carries the shape + cap-bag.

`mmap_key_slug` is the bead's "shared key with `ft-2okh0.5`
mmap-backed scrollback" coupling — same slug = same key
handle.

`EncryptionMode` enum (Disabled / Aes256Gcm) drives the
pipeline's `encrypt_with` vs `skip_encryption` branch.

## Structured logging (sub-task 9)

`StructuredLogRow` enum (tagged):

- `ChunkWrite { ts_ms, chunk_id, pane_id, bytes_in,
  bytes_out, redaction_applied, encryption_mode,
  latency_ns }`
- `ChunkRead { ts_ms, chunk_id, pane_id, bytes_out,
  decompress_ns, decrypt_ns, total_latency_ns }`
- `EvictionCycle { ts_ms, chunks_evicted, bytes_freed }`
- `RetentionPurge { ts_ms, chunks_purged,
  retention_window_ms }`

`render_log_jsonl` / `parse_log_jsonl` are bidirectionally
clean.

## Pipeline health

`PipelineHealth`:
- `chunks_written_total` / `chunks_read_total` — lifetime
  counters.
- `redactions_applied_total` — privacy invariant
  observability. It must equal `chunks_written_total`.
- `chunks_written_without_redactor` — fail-closed counter for writes where
  the integration explicitly reported that the redactor was not applied. It
  must remain 0.
- `encryptions_applied_total` / `encryption_skipped_total`
  — operator opt-in/out split.
- `evictions_total` / `retention_purges_total` — cleanup
  observability.
- `stage_failures` — per-stage failure counter
  (`compress_zstd`, `encrypt_aes256_gcm`, `write_disk`).
- `is_safe()`: `redactions_applied_total == chunks_written_total` and
  `chunks_written_without_redactor == 0` (privacy invariant).

## Tests (22)

- 8 disk-path tests (render, matches_layout edges, mode
  constant).
- 2 typed-state pipeline tests (full path, skip-encryption).
- 6 metadata index tests (DDL idempotency, column
  coverage, 3 validation cases, valid passes, serde
  roundtrip).
- 1 structured-log JSONL roundtrip.
- 3 health tests (baseline-safe, write-with-redactor-
  invariant, stage-failures).
- 1 headline scenario:
  `full_write_path_scenario_with_audit_trail`.

## Bead acceptance status (foundation slice → integration)

| Sub-task | Status |
|---|---|
| 1 — zstd compression at write | ✓ pipeline shape (`compress_with`); ⏳ zstd integration |
| 2 — Async I/O via asupersync | ⏳ integration follow-on |
| 3 — Redactor pre-write call | ✓ structurally enforced (`redact_with` is the only path to `Redacted`) |
| 4 — AES-256-GCM with operator opt-in | ✓ `encrypt_with` / `skip_encryption` + `ColdTierKeyHandle` shape |
| 5 — Disk file layout | ✓ `ColdTierDiskPath` + 0o600 mode constant |
| 6 — Metadata index | ✓ DDL + row schema + validation |
| 7 — Cleanup task (weekly cron) | ⏳ integration follow-on (substrate has `should_purge_by_retention`) |
| 8 — Search integration (cold-tier read) | ⏳ integration follow-on |
| 9 — Structured JSONL logging | ✓ `StructuredLogRow` |
| Per-release attestation entry | ⏳ depends on `ft-syqcz.1` |

## Cross-references

- Substrate: `scrollback_cold_tier.rs` (eviction policies,
  4-tier cascade, dedup decision).
- Sibling: `ft-2okh0.5` (mmap-backed scrollback — sub-task
  4 shares key handle), `redactor.rs` (sub-task 3 callee),
  `compression_dictionary.rs` (sub-task 1 — dict-based
  zstd).
- Attestation: `ft-syqcz.1`.
