# Session Persistence — Troubleshooting

This guide covers the most common snapshot/restore failure modes and how to diagnose them quickly.

## 1) `ft snapshot save`: “No panes found” / “Failed to list panes”

**Symptoms**

- `ft snapshot save` exits non-zero
- JSON output shows `"ok": false`

**Likely causes**

- WezTerm isn’t running
- `wezterm` CLI is not available in `PATH` or can’t talk to the mux server
- Pane filters exclude everything

**What to do**

1) Verify WezTerm CLI:
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

**Important**: `ft snapshot restore` is not fully wired yet; the supported restore path is via `ft watch` on startup.

**What to do**

1) Run the watcher:
   ```bash
   ft watch
   ```
2) Check whether ft sees unclean sessions:
   ```bash
   ft session doctor
   ft session list
   ```
3) Inspect the latest checkpoint for a session:
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

**What to do**

1) Check watcher status:
   ```bash
   ft status
   ```
2) Stop the watcher if needed:
   ```bash
   ft stop
   ```
3) Re-run snapshot/session commands and see if the lock clears
4) If migrations are involved:
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

