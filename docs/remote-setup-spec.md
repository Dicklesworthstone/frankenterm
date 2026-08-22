# Remote Setup Spec (ft setup remote)

## Summary
A guided, idempotent, non-destructive workflow to bootstrap a remote host for compatibility-backend mux usage (current bridge: WezTerm + `frankenterm-mux-server`):
- verify SSH connectivity
- detect OS + package manager
- install WezTerm bridge components if missing
- install and enable `frankenterm-mux-server` as a systemd user service
- enable linger so the mux survives logout
- optionally install the exact-build `ft` + `frankenterm-mux-server` process
  family on the remote

Default behavior is dry-run with a full plan preview. No destructive actions are allowed.

---

## Goals
- Make remote compatibility-backend domains reliable and repeatable.
- Provide a single command to bootstrap a host safely.
- Keep output clear, auditable, and deterministic.

## Non-Goals
- Managing SSH keys (assumes SSH access already works).
- Managing sudo credentials beyond explicit prompts.

---

## CLI Contract

### Command
```
ft setup remote --host <ssh_host>
```

### Flags
- `--host <ssh_host>`: SSH alias from `~/.ssh/config` or explicit host.
- `--dry-run`: default; prints plan, does not modify remote.
- `--apply`: executes the plan (non-destructive). Requires explicit confirmation.
- `--install-ft`: include staging the matching `ft` and
  `frankenterm-mux-server` binaries on the remote. This never restarts an
  already-active mux service because that process owns the live PTYs.
- `--ft-path <path>` and `--mux-server-path <path>`: target-compatible local
  binaries from one exact build; both are required together. Their embedded
  sealed build IDs, targets, profiles, and versions must match before any remote
  step runs, then they are published as one recoverable update.
- `--ft-version <release-tag>`: install both binaries from the checksummed Unix
  archive for an immutable release tag. `git` is rejected because it cannot
  identify an atomic client/server build.
- `--yes`: skip interactive prompts (only allowed with `--apply`).
- `--timeout-secs <n>`: per-command timeout (default 30s).
- `--verbose`: emit step-by-step logs with timings and remote command outputs.

### Output Format
- Human output by default.
- If `FT_OUTPUT_FORMAT=json`, also emit machine-parsable JSON plan/results.

---

## Safety Requirements
- Default to `--dry-run`.
- Explicit confirmation before any remote change.
- All file mutations create backups or are additive.
- No deletion of remote user data.

---

## Step Plan (Dry Run + Apply)

### 1) Host Selection
- Accept `--host` or prompt from SSH config (if interactive).
- Resolve to `ssh` target and report effective user/hostname/port.

### 2) Connectivity Check
Run:
```
ssh <host> "true"
```
- If unreachable, abort with actionable error.

### 3) Detect OS / Package Manager
Run:
```
ssh <host> "command -v apt-get || command -v dnf || command -v yum || command -v pacman || true"
```
- Record package manager for later steps.

### 4) Detect WezTerm bridge
Run:
```
ssh <host> "command -v wezterm"
ssh <host> "wezterm --version"    # if wezterm exists
```
- If missing and `--apply`, proceed to install.

### 5) Install WezTerm bridge (If Missing)
Plan depends on package manager:

#### apt (Ubuntu/Debian)
```
ssh <host> "sudo apt-get update"
ssh <host> "sudo apt-get install -y wezterm"
```

#### dnf (Fedora)
```
ssh <host> "sudo dnf install -y wezterm"
```

#### yum (RHEL/CentOS)
```
ssh <host> "sudo yum install -y wezterm"
```

#### pacman (Arch)
```
ssh <host> "sudo pacman -Sy --noconfirm wezterm"
```

If no known manager:
- Stop and report unsupported OS. Provide guidance for manual install.

### 6) Stage the matching FrankenTerm process family (optional)

When `--install-ft` is selected, treat both binaries as one process family. The
release archive and its component manifest must contain both executables from
the same source revision and codec contract. If no mux is active, existing
bytes are renamed to timestamped `previous-*` paths and a publish failure
restores the previous pair. If a mux is active, locally supplied binaries are
placed at unique `pending-*` paths and the currently compatible client/server
pair is left untouched. Release-tag setup always downloads and verifies the
pair in a unique private non-active cache directory and executes both staged version
probes before publication. With a live owner it moves both components to
matching `pending-<tag>-<nonce>` paths and never points the service or canonical
client path at those bytes. With no live owner it publishes both canonical
names transactionally and restores the previous pair on partial publication.
Merely replacing the client while the old mux remains alive is forbidden
because a codec-window mismatch makes all configured domains unusable even
though their reconnect loop is running.

The live-owner fence checks both the user service and manually launched mux
executables, including `pending-*` or preserved generation names. When any mux
generation is already live, setup may enable the service for a future boot but
must not use `enable --now` or otherwise start a second mux generation.

Before staging against a live owner, setup invokes the currently installed
remote `~/.local/bin/ft`, which is the client most likely to remain compatible
with that mux. If it supports `session dump`, both a complete dump and
`verify-dump` must succeed before any candidate bytes are downloaded. A legacy
client that predates the dump command emits an explicit unavailable warning and
may continue to non-activating staging, but the workflow records that no
verified content artifact exists and never promotes manual capture to proof.

Pending bytes are not an automated live upgrade. The command deliberately has
no activation step until the guardian owns PTYs and can prove a fenced handoff;
today the operator must drain sessions, stop the empty mux, and rerun the same
`ft setup --apply remote <host> --yes --install-ft --ft-version <tag>` command.
The inactive-host branch transactionally publishes the verified pair, restores
the prior pair on partial publication, and enables the new service; no operator
should rename one pending component by hand.

Before any deliberate restart, run the still-compatible client-side
`ft session dump --format json`, then run `ft session verify-dump <path>
--format json`; require both `complete: true` and `capture_complete: true`.
The dump preserves sequential redacted observable pane text and bounded
topology metadata only. It does not preserve mux-owned PTY descriptors, process
memory, or running-agent continuity, so it is an additional safety gate rather
than permission to restart an undrained mux.

### 7) Install systemd user service for mux
Service file path:
```
~/.config/systemd/user/frankenterm-mux-server.service
```
Service content (template):
```
[Unit]
Description=FrankenTerm Mux Server
After=network.target

[Service]
Type=simple
ExecStart=/usr/bin/frankenterm-mux-server --daemonize=false
Restart=on-failure
RestartSec=2
# The mux owns every pane's PTY, so an oomd kill takes down every process
# hosted in those panes -- on a fleet box, the whole agent swarm at once.
# It is a small-RSS process and is only ever picked because it is a cheap
# candidate in a pressured slice, so remove it from oomd's candidate set.
ManagedOOMMemoryPressure=auto
ManagedOOMSwap=auto
ManagedOOMPreference=omit
MemoryHigh=400G

[Install]
WantedBy=default.target
```

Commands:
```
ssh <host> "mkdir -p ~/.config/systemd/user"
ssh <host> "cat > ~/.config/systemd/user/frankenterm-mux-server.service <<'EOF'\n...EOF"
ssh <host> "systemctl --user daemon-reload"
ssh <host> "systemctl --user enable --now frankenterm-mux-server"
```

### 8) Enable linger (mux survives logout)
Command (requires sudo):
```
ssh <host> "sudo loginctl enable-linger $USER"
```
- If sudo denied, print remediation steps; do not retry silently.

### 9) Verify service
```
ssh <host> "systemctl --user status frankenterm-mux-server"
```
- Parse status; report active/inactive.

### 10) Verify staged components

- `~/.local/bin/ft --version` and
  `~/.local/bin/frankenterm-mux-server --version` must both launch.
- Release-tag installs are checksum- and component-manifest-verified before
  either live path changes.
- An already-active mux continues running its prior inode until the operator
  explicitly drains the hosted PTYs and restarts it.
- `ft --version` should succeed immediately after install.
- `ft doctor` / `ft doctor --json` emit diagnostics immediately, but current builds will report backend-prerequisite errors until WezTerm CLI is installed and `wezterm cli list --format json` can reach a running WezTerm GUI/mux.

---

## Idempotency Rules
- If WezTerm already installed, skip install.
- If service file exists and matches expected content, skip rewrite.
- When `--install-ft` stages a new process family, preserve a differing service
  unit under a timestamped `previous-*` name and update `ExecStart` to the
  freshly staged mux-server path. `daemon-reload` changes only the next start;
  the currently active mux and its PTYs are not restarted.
- If service is enabled/active, skip enable step.
- If linger already enabled, skip.
- Treat `ft` and `frankenterm-mux-server` as one versioned process family;
  never update or claim one without the other.
- Never restart an active mux as an installation side effect. Report the
  staged-vs-running distinction and require an explicit drained restart.

---

## Observability
- Each step logs:
  - command string (redacted where needed)
  - duration
  - stdout/stderr (redacted)
  - status (ok/warn/error)
- Final summary includes:
  - what changed
  - backups created
  - next steps

---

## Rollback Plan
- Disable service:
```
ssh <host> "systemctl --user disable --now frankenterm-mux-server"
```
- Archive service file manually (not automated by ft):
```
ssh <host> "mv ~/.config/systemd/user/frankenterm-mux-server.service ~/.config/systemd/user/frankenterm-mux-server.service.disabled"
```
- Disable linger:
```
ssh <host> "sudo loginctl disable-linger $USER"
```
- Archive `ft` binary manually:
```
ssh <host> "mv ~/.local/bin/ft ~/.local/bin/ft.disabled"
```
- Archive `frankenterm-mux-server` alongside it; do not restart the service
  until the desired pair is staged and all mux-owned PTYs are drained.

---

## Acceptance Criteria
- A reviewer can implement remote setup without re-reading PLAN.md.
- The spec enumerates commands, files, flags, logging, and rollback steps.
- The flow is idempotent and safe by default.
