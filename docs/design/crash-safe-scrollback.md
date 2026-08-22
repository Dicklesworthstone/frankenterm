# Crash-Safe Scrollback — File Format + Recovery Doctrine

**Bead:** [BR-TERM-EMULATOR-UPLIFT-2.5] / `ft-2okh0.5` (decomposed into 5 sub-beads).
**Subsets shipped:** epic decomposition + this design doc + all 5 sub-beads at substrate scope (see cross-reference table).
**Wired-pass tracking:** see "Cross-references" section.

## Bead-ID cross-reference

The crash-safe-scrollback work has two parallel decompositions:
the canonical `ft-2okh0.5.x` hierarchy (in the bead system from
the start) and the `ft-kscfg / ft-5te6x / ft-hs5f6 / ft-53zsr /
ft-0ulxc` set (filed during the 2026-05-01 session as a parallel
breakdown). Both decompose the same underlying work; this table
maps them so future operators can reach either via the cross-link.

| Canonical (ft-2okh0.5.x) | Session decomposition | Scope |
| ------------------------ | --------------------- | ----- |
| `ft-2okh0.5.1` mmap-backed scrollback (page-aligned, kill-9 survivable) | `ft-kscfg` mmap scrollback file format + write path | Format substrate at `crates/frankenterm-core/src/scrollback_mmap_format.rs` (256-byte header and tagged-length records). `append_captured_segment_to_mmap_scrollback` exists and is exercised by tests, but the production runtime capture path does not currently call it; this file must not be represented as a production-wide scrollback guarantee. Encryption-at-rest remains separate follow-up work. |
| `ft-2okh0.5.2` recovery protocol on launch | `ft-5te6x` recovery protocol — discover orphan scrollback + session-restore prompt | Orphan scanner at `crates/frankenterm-core/src/scrollback_mmap_recovery.rs` (OrphanState taxonomy, LockProbe trait, production `FlockLockProbe`). CLI commands respect live-writer locks. `ft session recover` exports the exact redacted UTF-8 prefix to a private transcript and never writes archived output into a live PTY. Automatic launch recovery and pane attachment are not shipped. |
| `ft-2okh0.5.3` native tmux control protocol speaker | `ft-hs5f6` native tmux control-protocol speaker (Tier-1 RPC subset) | Wire-format substrate at `crates/frankenterm-core/src/tmux_control_protocol.rs` (TmuxCommand enum, parse_command + TmuxResponse encoder, 29 unit tests). Wired-pass blocker: `ft-l4cef`; current daemon slice probes tmux line protocol, supports live `list-sessions`, `list-windows`, `capture-pane -p -t %<pane_id>`, and `send-keys -t %<pane_id>`, and returns explicit typed `%error` frames for Tier-2 `pipe-pane` / `copy-mode`, while topology/client-lifecycle commands and the notification stream remain pending. `ft-2h56m` closed only the socket-lock/listener slice. |
| `ft-2okh0.5.4` tmux compatibility test corpus | `ft-53zsr` tmux compatibility matrix verification | Compatibility matrix doc at `docs/term-emulator/tmux-compat-matrix.md` with substrate-pass / wired-pass taxonomy. |
| `ft-2okh0.5.5` crash-recovery adversarial fuzz — kill-9 stress test corpus | `ft-0ulxc` crash-recovery test fixture: kill -9 mid-session integrity | Substrate invariant tests at `crates/frankenterm-core/tests/crash_recovery_kill9.rs` cover pre-msync byte safety, pane UUID continuity, bounded loss, torn metadata, and edge cases. The Unix subprocess harness `session_recover_exports_sigkill_orphan_without_mux_mutation` kills a writer with SIGKILL, invokes `ft session recover`, and verifies the durable prefix in a private transcript without starting or mutating a mux. |

This document is the foundational decision record that unblocks
all sub-bead implementations under both decompositions. It pins
the file format, the on-disk layout, the redaction-and-encryption
strategy, and the recovery-flow contract so each sub-bead can
land independently without re-litigating the architecture.

## Why file-based mmap rather than SQLite

`storage.rs` already runs SQLite in WAL mode, with cascade
retention and FTS5 indexing. Why not just write scrollback there?

1. **Crash semantics**. SQLite's WAL gives durable commits, but a
   `kill -9` between commits loses the in-memory journal. Per-pane
   mmap with explicit `msync` lets ft trade write throughput for
   crash-window-zero on the bytes already flushed.
2. **Read amplification**. Scrollback is sequential append + random
   seek by line number. SQLite imposes a row + index per cell;
   mmap'd ring is one byte per cell. The ratio matters at 50MB
   per-pane caps.
3. **Recovery latency**. After a crash, mmap recovery reads a
   single file per pane. SQLite recovery walks the WAL, reapplies
   committed transactions, and may rebuild FTS indices.
4. **Failure independence**. If the SQLite DB corrupts (rare but
   not zero — `db_check_repair` exists), scrollback survives in
   the separate file. The mmap files act as a second source of
   truth.

The trade-off is operational complexity: two storage substrates,
two retention policies, two recovery flows. The bead's "drop-in
tmux replacement" framing is what justifies the cost — tmux's own
storage is per-pane file-based, and matching that shape lets ft
inherit tmux operator habits.

## On-disk layout

```
~/.local/share/ft/
├── scrollback/
│   ├── <pane_uuid>.bin       0600 perms, mmap'd
│   └── <pane_uuid>.bin.lock  flock advisory; 0600
└── scrollback.key            0400 perms; AES-256 key (opt-in encryption)
```

- **Path keyed on `pane_uuid`, not `pane_id`.** `pane_id` is a
  short integer reused after pane closure; `pane_uuid` is a
  globally unique identifier that survives across ft restarts and
  prevents accidental ABA on recovery.
- **`.lock` file** uses `flock(LOCK_EX | LOCK_NB)` so a second
  ft instance can't accidentally write to the same file. On
  recovery, an unlocked `.bin` whose `.lock` is gone is
  unambiguously orphaned.
- **`scrollback.key`** is created on first encrypted-mode write,
  mode 0400, owner-readable only. The bead's "encryption-at-rest"
  is feature-gated behind `--features scrollback-encryption`
  because key management adds operational complexity that not
  every operator wants by default.

## File format (v1)

The `.bin` file is a **fixed-header + ring buffer** layout:

```
 byte offset    field                        notes
 0..4           "FTSB"                       magic
 4..6           u16  format_version          starts at 1
 6..8           u16  flags                   bit 0 = encrypted
 8..16          u64  capacity_bytes          ring size; page-aligned
16..24          u64  write_cursor_bytes      offset into the ring
24..56          [u8; 32]  pane_uuid          binary form
56..64          u64  created_at_epoch_ms
64..72          u64  last_msync_at_epoch_ms  last successful msync
72..80          u64  redactions_applied      counter
80..88          u64  total_bytes_written     monotone; for histograms
88..256         [u8; 168]  reserved          zero-filled
256..end       <ring buffer>                 capacity_bytes long
```

- **256-byte header** is 4 cache lines wide; aligned for atomic
  updates to `write_cursor_bytes` + `last_msync_at_epoch_ms` via
  the existing `compact_bitset` style of CAS.
- **Ring buffer entries** are tagged-length records:
  `u32 record_len | u8 record_kind | record_payload`. Kinds are
  the existing `OutputSegment` variants (text / OSC / CSI / cursor
  movement / clear). Bumps to record_kind require a
  format_version bump (the v1 line above).

## Substrate write path contract

The following is the contract implemented by the mmap writer/helper substrate.
It is not currently a claim about every production capture because the live
runtime does not call the helper.

1. A caller may write a captured segment through
   `append_captured_segment_to_mmap_scrollback`, which delegates to
   `MmapScrollback::append`. Current direct callers are tests.
2. `append` runs the **redactor** (ft-x0666 G10 surface) before
   the mmap write. Secrets never reach disk.
3. `append` writes into the ring at `write_cursor_bytes`, wraps
   on overrun, and calls `msync(MS_ASYNC)` opportunistically (each
   pane's `append` returns without blocking on disk).
4. **Flush boundary**: every N appends OR every M ms, the
   write-side helper calls `msync(MS_SYNC)` and bumps
   `last_msync_at_epoch_ms`. This is the durability boundary —
   bytes before the last sync survive `kill -9`.

The in-memory ring stays as the read-side fast path. mmap is for
durability, not access.

## Current production gaps

The standalone mux installs a separate tiered-scrollback spill sink backed by
`MmapScrollbackStore` under `~/.local/share/ft/scrollback-lines/`. That path is
useful but does not satisfy this document's complete crash-recovery goal:

- it receives only lines evicted from the terminal hot tier, not the current
  viewport/hot rows;
- its file key is derived partly from the mux process ID, so a restarted server
  cannot authoritatively associate remnants with a stable pane identity;
- ordinary line appends flush userspace buffers but do not establish an fsync
  boundary for every admitted line; and
- there is no production manifest/hydration path that reconstructs terminal
  parser/render state or reattaches live PTY descriptors after mux death.

`ft session dump` closes the immediate forensic/pre-upgrade content gap by
capturing all currently readable pane text plus topology metadata. It does not
close these hot-state or PTY-lifetime gaps.

## Manual orphan-export flow

When an operator invokes the session orphan commands (automatic launch recovery
is not wired):

1. Scan `~/.local/share/ft/scrollback/` for `.bin` files whose
   `.lock` is absent or whose `flock` succeeds (no live owner).
2. For each orphan:
   - Validate the magic + version.
   - Reapply the redactor (defense in depth — the secrets ledger
     might have grown since the file was written).
   - Surface the orphan via `ft session list-orphans`.
3. The CLI lets the operator `recover` or `discard`. Recover reads the mmap
   file's linear record prefix, reapplies redaction, reports
   record/byte/chunk counts, skips non-UTF-8 records with explicit diagnostics,
   and exports the exact admitted UTF-8 bytes to a new private transcript. It
   never creates a pane and never sends historical output through PTY input.

## Redaction + encryption boundary

- **Redactor** runs *before* every mmap write. Single source of
  truth at the secrets-ledger level (existing); the mmap path is
  one consumer of that surface.
- **Encryption** runs *after* the redactor, immediately before the
  syscall. The order matters: redacted plaintext is what gets
  ciphered; the encryption key is *not* a substitute for redaction
  (an attacker with the key can read the plaintext, so secrets
  still need to be redacted before they reach the file at all).

## Tested invariants (sub-bead ft-0ulxc)

The kill-9 substrate fixture under `ft-0ulxc` proves:

1. **Pre-msync byte safety**: bytes that *did* hit `MS_SYNC`
   before the kill survive byte-for-byte across restart.
2. **Post-msync data loss is bounded**: bytes between the last
   `MS_SYNC` and the kill are best-effort; the test asserts the
   loss window is ≤ N appends or M ms (whichever the write-path
   helper enforces).
3. **Identity continuity**: `pane_uuid` matches across the kill/restart
   boundary and a downstream `ft session recover` resolves the right orphan
   artifact. No live pane identity continuity is claimed.

The fixture is `cfg(unix)` because SIGKILL semantics are
POSIX-specific. Windows uses a `TerminateProcess` analog under a
separate fixture (deferred, not in scope for v1).

The Unix E2E harness under `ft-rlvsz` uses isolated `FT_WORKSPACE`,
`XDG_DATA_HOME`, and `HOME`, waits for a child mmap writer to cross a sync
boundary, sends SIGKILL to that child, runs the actual `ft session recover
<pane_uuid> --format json` binary, and verifies the durable pre-kill text prefix
in the exported transcript. It intentionally starts no mux process.

## Compatibility constraints

- **Disk usage cap**: per-pane file size cap is configurable;
  default 50 MB. When the ring fills, older entries are
  overwritten in-place (FIFO).
- **Privacy**: the existing redactor (BR-RC-SAFETY-PROOFS.G10)
  applies before write. No secret bytes ever land in the mmap.
- **A11Y**: the recovery prompt is keyboard-navigable and
  screen-reader-accessible. The picker reuses the existing
  `crate::ui` prompt machinery.
- **session_restore module integration**: the recovery flow
  composes with the existing `session_restore` module rather
  than duplicating its logic.

## Cross-references

- Parent epic: `ft-2okh0.5` (decomposed into 5 sub-beads).
- Sub-beads: ft-kscfg (.1 write path) / ft-5te6x (.2 recovery
  protocol) / ft-hs5f6 (.3 tmux speaker) / ft-53zsr (.4 tmux
  compat matrix) / ft-0ulxc (.5 kill-9 fixture).
- Sibling: `crates/frankenterm-core/src/session_restore.rs` —
  composes with the recovery flow.
- Sibling: `crates/frankenterm-core/src/redactor.rs` (under
  ft-x0666.G10) — the secrets ledger the write-path consults.
- Sibling: `crates/frankenterm-core/src/storage.rs` —
  retention-cleanup integration (mmap files purge alongside
  segment expiry).
