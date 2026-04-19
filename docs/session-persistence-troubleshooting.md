# Session Persistence — Troubleshooting

This guide covers the most common snapshot/restore failure modes and how to diagnose them quickly.

Session persistence is a shipped `ft` feature, but live snapshot/restore still depends on the current WezTerm-backed mux interop boundary. Start with `ft doctor`, `ft status --health`, and `ft session doctor` before assuming the failure is in the snapshot data itself.

## 1) `ft snapshot save`: “No panes found” / “Failed to list panes”

**Symptoms**

- `ft snapshot save` exits non-zero
- JSON output shows `"ok": false`

**Likely causes**

- The current live mux interop boundary (WezTerm today) isn’t running
- `wezterm` CLI is not available in `PATH` or can’t talk to the mux server
- Pane filters exclude everything

**What to do**

1) Verify the current mux interop CLI:
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

`ft snapshot restore <checkpoint_id>` is wired for manual restores. `ft watch` remains the restore-on-startup path when you want ft to detect unclean shutdowns automatically.

**What to do**

1) For restore-on-startup, run the watcher:
   ```bash
   ft watch
   ```
2) Re-check operator health surfaces:
   ```bash
   ft doctor
   ft status --health
   ft session doctor
   ```
3) For a direct/manual restore, list recent checkpoints and restore one explicitly:
   ```bash
   ft snapshot list --limit 10
   ft snapshot restore <checkpoint_id>
   ```
4) If you only want the split/tab/window layout and do not want scrollback replay:
   ```bash
   ft snapshot restore <checkpoint_id> --layout-only
   ```
5) Check whether ft sees unclean sessions:
   ```bash
   ft session doctor
   ft session list
   ```
6) Inspect the latest checkpoint for a session:
   ```bash
   ft session show <session_id>
   ```

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
- Another `ft snapshot restore` or `ft restart` is already holding the restart-operation lock

**What to do**

1) Check watcher status:
   ```bash
   ft status
   ```
2) Stop the watcher if needed:
   ```bash
   ft stop
   ```
3) If the error mentions another restore or restart already being in progress, wait for that operation to finish before retrying
4) Re-run snapshot/session commands and see if the lock clears
5) If migrations are involved:
   ```bash
   ft db migrate --status
   ft db migrate
   ```

## 5) Scrollback fidelity surprises (TUIs, alt-screen, partial replay)

**What to expect**

- Scrollback restore is best-effort and may not perfectly reproduce interactive TUIs.
- Alt-screen content is inherently less stable for capture and replay.

**What to do**

- Prefer relying on layout restoration first (splits/tabs/windows)
- Use `ft snapshot inspect <id>` to confirm the pane’s captured terminal state (size, alt-screen)
- If you need a reproducible artifact, consider `ft record` / `ft reproduce` instead of scrollback replay

## Minimal “what do I run?” checklist

```bash
ft status
ft snapshot save -f json
ft snapshot list -f json --limit 10
ft snapshot inspect <id> -f json
ft session doctor -f json
```
