# Session Persistence — Troubleshooting

This guide covers the most common snapshot/restore failure modes and how to diagnose them quickly.

Snapshot capture, inspection, and doctor surfaces ship, while live restore is
only partially supported and depends on the current WezTerm-backed mux
boundary. Start with `ft doctor`, `ft status --health`, and `ft session doctor`
before assuming the failure is in the snapshot data itself.

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

`ft snapshot restore <checkpoint_id>` is wired for manual layout-only restores.
The `SessionRestorer` library can detect unclean sessions, but production
`ft watch` does not yet call that detector or offer a startup restore prompt.

**What to do**

1) Re-check operator health surfaces:
   ```bash
   ft doctor
   ft status --health
   ft session doctor
   ```
2) List recent checkpoints and restore one explicitly:
   ```bash
   ft snapshot list --limit 10
   ft snapshot restore <checkpoint_id>
   ```
3) State the currently supported layout-only mode explicitly:
   ```bash
   ft snapshot restore <checkpoint_id> --layout-only
   ```
4) Check whether ft sees unclean sessions:
   ```bash
   ft session doctor
   ft session list
   ```
5) Inspect the latest checkpoint for a session:
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
- Another `ft snapshot restore` is already holding the restore-operation lock

**What to do**

1) Check watcher status:
   ```bash
   ft status
   ```
2) Stop the watcher if needed:
   ```bash
   ft stop
   ```
3) If the error mentions another restore already being in progress, wait for that operation to finish before retrying
4) Re-run snapshot/session commands and see if the lock clears
5) If migrations are involved:
   ```bash
   ft db migrate --status
   ft db migrate
   ```

## 5) Historical scrollback is not replayed

**What to expect**

- Restore fails closed rather than sending captured historical output to a
  pane through PTY input.
- `output_segments` are arbitrary stream fragments, not authoritative logical
  terminal lines or a versioned render-state snapshot.
- Alt-screen state, interactive TUI buffers, images, cursor state, and reflow
  therefore are not reconstructed by the current layout-only path.

**What to do**

- Use layout restoration for its documented supported subset
- Use `ft snapshot inspect <id>` to confirm the pane’s captured terminal state (size, alt-screen)
- Use `ft record` / `ft reproduce` when you need a reproducible historical artifact
- Do not treat `--layout-only` as an optional performance mode: the CLI forces
  it until a mux-owned output or render-state restoration channel exists

## 6) Panes returned, but shells, agents, or interactive programs did not

This is the expected boundary of the current restore path. Recreating a pane
starts its default shell at the validated local working directory; it does not
resume the process, agent conversation, in-flight command, TUI state, job
control state, environment, or remote-domain attachment that occupied the old
pane. Process classification produces finite manual dispositions only.

**What to do**

- Treat every restored pane as a new process boundary, even if its layout and
  working directory match the snapshot
- Inspect the saved session metadata to determine what previously occupied the
  pane, then resume it through that program's own supported recovery mechanism
- Delete any historical `[snapshots.process_relaunch]` table. It is rejected
  with a migration error, and there is no replacement launch setting because
  process and agent restoration is unavailable
- Delete any historical `session.restore_max_lines` setting. It is rejected
  because scrollback replay is unavailable rather than silently pretending to
  constrain a replay path
- Do not interpret a completed layout restore as process, agent, scrollback,
  render-state, or full-session continuity

## Minimal “what do I run?” checklist

```bash
ft status
ft snapshot save -f json
ft snapshot list -f json --limit 10
ft snapshot inspect <id> -f json
ft session doctor -f json
```
