# Session Persistence — Troubleshooting

This guide covers the most common snapshot/restore failure modes and how to diagnose them quickly.

Snapshot capture, inspection, and doctor surfaces ship. Live restore and
restart execution do not: non-dry invocations fail closed before process or mux
effects. Restore dry-run is a bounded metadata-only reporting surface, not an
executable preflight. Start with `ft doctor`, `ft status --health`, and
`ft session doctor` before assuming the failure is in the snapshot data itself.

## 1) `ft snapshot save`: “No panes found” / “Failed to list panes”

**Symptoms**

- `ft snapshot save` exits non-zero
- JSON output shows `"ok": false`

**Likely causes**

- The configured exact mux endpoint is unavailable or cannot be authenticated
- Direct mux transport failed and the eligible `wezterm` CLI fallback is not
  available or cannot reach that same endpoint
- Pane filters exclude everything

**What to do**

1) If you deliberately use the compatibility fallback, verify it against the
   already-running exact endpoint. This is an external client connection, not
   a passive offline check, and it may exercise backend discovery/fallback:
   ```bash
   wezterm cli list
   ```
2) Verify ft can see panes:
   ```bash
   ft status
   ```
3) Retry with JSON to see structured error:
   ```bash
   ft snapshot save -f json
   ```

## 2) “Restore didn’t happen” after a crash/restart

No production path currently performs snapshot restore. The `SessionRestorer`
library can detect unclean sessions and its executable restorer is exercised as
library/test substrate, but production `ft watch` does not call that detector
or offer a startup restore prompt. Non-dry snapshot restore fails before
checkpoint/database resolution, subprocess launch, or mux mutation.

**What to do**

1) Re-check operator health surfaces:
   ```bash
   ft doctor
   ft status --health
   ft session doctor
   ```
2) List recent checkpoints and inspect the one you intend to use:
   ```bash
   ft snapshot list --limit 10
   ft snapshot inspect <checkpoint_id> -f json
   ```
3) Generate a metadata-only, non-executable recovery aid:
   ```bash
   ft snapshot restore <checkpoint_id> --layout-only --dry-run
   ```
4) Check whether ft sees unclean sessions:
   ```bash
   ft session doctor
   ft session list --limit 50 --offset 0
   ```
5) Inspect the latest checkpoint for a session:
   ```bash
   ft session show <session_id> --limit 50 --offset 0
   ```
   If either command reports more rows, advance `--offset` by the number
   returned. Each page is internally consistent, but separate page invocations
   are not pinned to one database snapshot, so a concurrently changing history
   can shift between pages.

## 3) Snapshots “disappeared” (list is empty)

**Likely causes**

- You’re pointing at a different database than you think (workspace vs global data dir)
- Retention pruned old checkpoints (`retention_count` / `retention_days`)

**What to do**

- Verify the active config and storage location:
  ```bash
  ft config show
  ```
- List recent snapshots:
  ```bash
  ft snapshot list --limit 50
  ```
- Confirm retention settings:
  ```toml
  [snapshots]
  retention_count = 10
  retention_days = 7
  ```

## 4) Database errors: “database is locked”, migration problems, or corruption

**Likely causes**

- Another watcher instance is running and holding locks
- A previous crash left the DB in a bad state (rare, but possible)

**What to do**

1) Check watcher status and identify the exact owner/instance:
   ```bash
   ft status
   ```
2) Do not stop a shared, active, or unidentified watcher, GUI, or mux process.
   If and only if the watcher is an explicitly owned disposable instance,
   coordinate with its operator and use the owner/supervisor's exact-instance
   normal lifecycle command. The generic command examples here do not identify
   a process incarnation and must not be used as proof of ownership.
3) Re-run snapshot/session commands and see if the lock clears
4) If migrations are involved, inspect status first:
   ```bash
   ft db migrate --status
   ```
   Applying migrations is a write operation. Back up the database, identify
   the exact workspace/writer, coordinate a maintenance window, and use the
   repository's current migration runbook before applying anything.

## 5) Historical scrollback is not replayed

**What to expect**

- Production restore is unavailable before it reaches any replay decision. In
  the library/test substrate, the scrollback capability check fails closed
  rather than sending captured historical output to a pane through PTY input.
- `output_segments` are arbitrary stream fragments, not authoritative logical
  terminal lines or a versioned render-state snapshot.
- Alt-screen state, interactive TUI buffers, images, cursor state, and reflow
  therefore are not reconstructed by the current layout-only path.

**What to do**

- Use snapshot inspection and the dry-run plan to guide manual reconstruction;
  production does not execute layout restoration
- Use `ft snapshot inspect <id>` to confirm the pane’s captured terminal state (size, alt-screen)
- Use `ft record` / `ft reproduce` when you need a reproducible historical artifact
- Do not treat `--layout-only` as an optional performance mode or as execution:
  only metadata-only `--dry-run` planning is available

## 6) Why a layout exercise cannot return a process or agent session

Production did not recreate any pane: executable restore is unavailable. In
the library/test substrate, recreating a pane starts its default shell at the
validated local working directory; it does not resume the process, agent
conversation, in-flight command, TUI state, job control state, environment, or
remote-domain attachment that occupied the old pane. Process classification
produces finite manual dispositions only.

**What to do**

- Treat every pane you reconstruct manually as a new process boundary, even if
  its layout and working directory match the snapshot
- Inspect the saved session metadata to determine what previously occupied the
  pane, then resume it through that program's own supported recovery mechanism
- Delete any historical `[snapshots.process_relaunch]` table. It is rejected
  with a migration error, and there is no replacement launch setting because
  process and agent restoration is unavailable
- Delete the entire historical top-level `[session]` table. It is unsupported
  and rejected, including `session.restore_max_lines`, because automatic
  startup restore and scrollback replay are unavailable.
- Do not interpret a dry-run plan or a library/test restore receipt as
  production process, agent, scrollback, render-state, or full-session
  continuity

## Minimal “what do I run?” checklist

```bash
ft status
ft snapshot list -f json --limit 10
ft snapshot inspect <id> -f json
ft session doctor -f json
```

`ft snapshot save` is intentionally omitted from this minimal diagnostic set:
it writes a new checkpoint and capture completion can run retention pruning.
Use it only when that mutation is the intended operation.
