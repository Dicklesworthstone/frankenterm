# Remote Setup Spec (ft setup remote)

## Summary
A guided, idempotent, non-destructive workflow to bootstrap a remote host for WezTerm mux usage:
- verify SSH connectivity
- detect OS + package manager
- install WezTerm if missing
- install and enable `wezterm-mux-server` as a systemd user service
- enable linger so the mux survives logout
- optionally install `ft` on the remote

Default behavior is dry-run with a full plan preview. No destructive actions are allowed.

---

## Goals
- Make remote WezTerm domains reliable and repeatable.
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
- `--install-ft`: include installing `ft` on the remote.
- `--ft-path <path>`: optional local path to `ft` binary for scp.
- `--ft-version <version|git>`: if not using scp, specify install source.
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

### 4) Detect WezTerm
Run:
```
ssh <host> "command -v wezterm"
ssh <host> "wezterm --version"    # if wezterm exists
```
- If missing and `--apply`, proceed to install.

### 5) Install WezTerm (If Missing)
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

### 6) Install systemd user service for mux
Service file path:
```
~/.config/systemd/user/wezterm-mux-server.service
```
Service content (template):
```
[Unit]
Description=WezTerm Mux Server
After=network.target

[Service]
Type=simple
ExecStart=/usr/bin/wezterm-mux-server --daemonize=false
Restart=on-failure
RestartSec=2

[Install]
WantedBy=default.target
```

Commands:
```
ssh <host> "mkdir -p ~/.config/systemd/user"
ssh <host> "cat > ~/.config/systemd/user/wezterm-mux-server.service <<'EOF'\n...EOF"
ssh <host> "systemctl --user daemon-reload"
ssh <host> "systemctl --user enable --now wezterm-mux-server"
```

### 7) Enable linger (mux survives logout)
Command (requires sudo):
```
ssh <host> "sudo loginctl enable-linger $USER"
```
- If sudo denied, print remediation steps; do not retry silently.

### 8) Verify service
```
ssh <host> "systemctl --user status wezterm-mux-server"
```
- Parse status; report active/inactive.

### 9) Optional: Install ft on remote
If `--install-ft`:
- Preferred: scp local binary
```
scp <local_ft> <host>:~/.local/bin/ft
ssh <host> "chmod +x ~/.local/bin/ft"
```
- Alternative (if no local binary):
```
ssh <host> "cargo install --git https://github.com/Dicklesworthstone/frankenterm.git ft"
```

---

## Idempotency Rules
- If WezTerm already installed, skip install.
- If service file exists and matches expected content, skip rewrite.
- If service is enabled/active, skip enable step.
- If linger already enabled, skip.
- If `ft` binary already present and matches version (if detectable), skip.

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
ssh <host> "systemctl --user disable --now wezterm-mux-server"
```
- Remove service file (manual; not automated by ft):
```
ssh <host> "rm ~/.config/systemd/user/wezterm-mux-server.service"
```
- Disable linger:
```
ssh <host> "sudo loginctl disable-linger $USER"
```
- Remove `ft` binary (manual):
```
ssh <host> "rm ~/.local/bin/ft"
```

---

## Acceptance Criteria
- A reviewer can implement remote setup without re-reading PLAN.md.
- The spec enumerates commands, files, flags, logging, and rollback steps.
- The flow is idempotent and safe by default.
