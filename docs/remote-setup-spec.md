# Remote Setup Spec (ft setup remote)

## Summary
A guided, idempotent, non-destructive workflow to bootstrap a remote host for compatibility-backend mux usage (current bridge: WezTerm + `frankenterm-mux-server`):
- verify SSH connectivity
- detect OS + package manager
- install WezTerm bridge components if missing
- inspect and preserve any existing `frankenterm-mux-server` systemd user
  service without rewriting or starting it through an unproved shell transaction
- enable linger so the mux survives logout
- optionally install the exact-build `ft` + `frankenterm-mux-server` process
  family on the remote

The source also contains the systemd contract for the independent
`frankenterm-pty-guardian` and the future one-way mux start gate. Guardian
publication and activation are deliberately withheld at this revision: the
guardian binary now has source-level bounded authenticated probe and pane-aware
guarded-stop commands, but their current-source mock-free proof, durable
acknowledgment replay, and external-effect atomicity are incomplete. Those
commands therefore do not yet satisfy the activation gate. `ft setup remote`
prints that boundary and continues with read-only service inspection; automatic
unit publication, daemon reload, enable, and start are withheld. It does not
imply guardian-backed continuity.

Default behavior is dry-run with a full plan preview. No destructive actions are allowed.

---

## Goals
- Make remote compatibility-backend domains reliable and repeatable.
- Provide a single command to bootstrap a host safely.
- Keep output clear, auditable, and deterministic.
- Make the future guardian lifecycle structurally independent of mux
  install, stop, restart, crash, and upgrade transactions.

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
- `--install-ft`: stage the matching `ft` and `frankenterm-mux-server` binaries
  on the remote as one immutable pending generation. A negative point-in-time
  mux census is not an activation lease, so this revision never changes
  `current`, rewrites a unit, or starts a mux from that candidate.
- `--ft-path <path>` and `--mux-server-path <path>`: target-compatible local
  binaries from one exact build; both are required together. Their embedded
  sealed build IDs, targets, profiles, and versions must match before any remote
  step runs, then they are copied into one immutable content-derived generation.
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
- Never delete or overwrite legacy binaries, published generations, current
  generation bytes, transaction stages, or displaced selector artifacts.
- Never publish or activate the guardian unit until an authenticated probe can
  traverse its bounded pane census and an independently authenticated stop
  transaction can refuse a non-empty census.
- Never use `PartOf=`, `BindsTo=`, `Requires=`, `PropagatesStopTo=`, or an
  ordinary mux `ExecStop=` to couple guardian lifetime to the mux.
- Never treat an `inactive` ownership probe as authority to switch `current` or
  start a mux. Every launcher must participate in one lifetime lease before a
  negative census can authorize activation.
- Never publish a systemd unit through path-based `cat`/`mv` compensation. Unit
  publication requires descriptor-confined create-new, exact readback, atomic
  no-replace/exchange, parent-directory synchronization, and replayable outcome
  authority; until that substrate lands, leave the unit unchanged.

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

When `--install-ft` is selected, both binaries are one process family. Local
uploads and release-cache sources retain exact size/SHA-256 receipts. The
verified `ft` and mux candidates are opened first on retained shell descriptors,
re-hashed through `/proc/self/fd/<n>`, and matched back to their verified named
inodes. Their owner-only executable modes are then applied through those exact
descriptor paths, followed by descriptor and named-path identity/hash
revalidation. The candidate `ft` is executed through its descriptor path, so a
substitution of an ambient upload or release-cache pathname cannot redirect the
publisher bootstrap or its preparatory chmod side effects. The hidden
target-Linux publisher reads both sealed build markers, requires identical
build/target/profile/version identity, and records the compiled codec version.
It never publishes either binary as a standalone canonical pathname.

The publication root is
`~/.local/share/frankenterm/process-family`. Ordinary `HOME`, `.local`, and
`share` ancestors are traversed without following symlinks but are not required
to have mode 0700. The owned publication root and its `generations` directory
must be current-user mode 0700 directories on one pinned filesystem. A
nonblocking, nofollow-opened, mode-0600, single-link lock serializes cooperating
publishers. After acquiring that lock, the publisher revalidates the named lock
against the locked handle. It also repeatedly requires the root's named
`generations` entry to have the same device/inode as the pinned generations
descriptor; a detached dirfd can never validate a root-relative selector.

The publisher derives one stable generation ID from canonical JSON containing
the frozen schema, codec version, and each component's filename, exact length,
SHA-256, owner-only mode, and sealed build identity. The generation manifest
adds that ID and is stored with one canonical pretty-JSON encoding and final
newline. It contains exactly:

```
generations/<content-derived-id>/
  ft                         # mode 0500
  frankenterm-mux-server     # mode 0500
  manifest.json              # mode 0400
```

Cross-filesystem sources are never renamed into place. They are opened without
following the leaf, required to be current-user regular mode-0500 files with
`nlink=1`, hashed before copying, copied to a transaction-unique mode-0700 stage
inside `generations`, and re-hashed/re-stat'ed after copying. Each destination
file is admitted through matching pathname/handle device+inode, owner, exact
mode, `nlink=1`, bounded length, and SHA-256 checks, then synchronized. The mux
candidate is hash-checked on a pinned descriptor before its bounded `--version`
probe, executed through a retained non-close-on-exec `/proc/self/fd/<n>`
duplicate of that exact descriptor, and checked again through the same handle
and nofollow named entry afterward. The mode-0400 canonical manifest is
synchronized and read back exactly; then the generation directory is
synchronized, changed to mode 0500 through its pinned handle, and revalidated
as an exact three-entry directory.

Generation publication uses Linux `renameat2(RENAME_NOREPLACE)` relative to the
pinned `generations` descriptor, followed by parent-directory synchronization.
An acknowledgement-loss retry may encounter an existing content-derived name;
it succeeds only after exact nofollow manifest, file, owner, mode, link-count,
hash, and pathname/handle device+inode revalidation. A conflicting existing
object fails closed. Transaction stages are never removed or overwritten, even
after a failed or concurrent attempt. A crash before the final generation
rename can therefore leave `.stage-<generation>-<transaction>` behind. Reusing
that exact transaction ID fails closed on the collision; a fresh transaction
may revalidate the sources and publish the same content-derived generation, but
the stale stage remains. This is not durable same-transaction recovery and can
accumulate residue. Bounded residue census plus a synchronized transaction
journal that can prove and resume an exact prepared state remain required before
claiming crash recovery; setup never guesses that a stale stage is safe to
delete.

Merely replacing the client while the old mux remains alive is forbidden
because a codec-window mismatch makes all configured domains unusable even
though their reconnect loop is running.

The live-owner census checks both the user service and manually launched mux
executables, including preserved generation names. Its result is observational,
not authorizing: a domain reconnect, human, or systemd can start a mux after an
`inactive` result. Setup therefore publishes every install candidate as pending,
never changes `current`, never rewrites a service unit, and never uses
`enable --now`. The
process-name expression is assembled by the remote shell from split literals;
the probe command line therefore cannot satisfy its own `pgrep -f`/`ps` match
and falsely classify every inactive host as active. `pgrep` and `grep` exit 1
are the only accepted no-match results; probe execution/inventory errors fail
the ownership fence instead of being collapsed into an inactive authorization.

Before any receipt-authorized step, setup requires one exact stdout marker from
the SSH command channel. Login/profile banners or other stdout contamination
fail before ownership checks or mutation, so a successful rename cannot later
be reported as ambiguous merely because its activation receipt was polluted.
Remote stdout and stderr diagnostics are terminal-sanitized, secret-redacted,
and bounded before verbose display.

Each local-component upload requires an exact stdout-clean nonexistence receipt
for its randomized remote stage before streaming the already-open no-follow
source descriptor over SSH, then rejects anything except the
expected regular, non-symlink file with the retained byte length and SHA-256.
The upload paths and release-cache paths remain source artifacts; generation
publication never moves, truncates, deletes, or overwrites them.

The stored process-family selector is the relative symlink `current`, whose
target must be the normalized path `generations/<content-derived-id>` with no
parent component. An already-current exact retry synchronizes and revalidates
that selector. For a change, the publisher first validates the existing
`current` selector and its complete generation, creates and synchronizes a
transaction-unique candidate selector, and atomically exchanges it with
`renameat2(RENAME_EXCHANGE)`. The displaced old selector remains under the
transaction's `.selector-rollback-<uuid>` name. An initial selector uses
`RENAME_NOREPLACE`. Stale selector artifacts are never removed or overwritten.
The selector primitive remains source-owned for the future lease-authorized
handoff and its direct fixture proofs; the remote publisher command exposed by
setup has no activation flag, and `ft setup remote` does not call this
primitive. A crash that leaves a transaction selector without a replayable
journal makes the same transaction fail closed rather than infer whether a
switch committed. Those residues have no automatic recovery in this revision.

One atomic selector makes readers see one complete old or new generation rather
than a mixed ft/mux pair. Atomic visibility is not crash-outcome knowledge: an
SSH acknowledgement loss or power loss around the switch still requires the
durable transaction journal/replay layer to determine and replay the terminal
result. This revision does not call activation crash-atomic and does not infer a
result from the absence of an acknowledgement.

The top-level installer's already-current shortcut is serialized by its
installer lock and requires the embedded sealed build marker to match across
the CLI and mux-server roles. Equal `--version` output from different builds is
not an installed process-family receipt and falls through to verified install.

Setup does not install a current-bound systemd unit in this revision. The former
shell transaction moved the old unit to a backup before moving the candidate,
did not synchronize the candidate or unit directory, and emitted no replayable
receipt; a crash could therefore leave the canonical unit absent even after an
apparently successful step. That path is removed rather than described as
durable. The replacement path performs only a read-only shape observation; it
does not consume unit contents. A typed marker admits only a missing path or a
regular non-symlink file; symlinks, directories, devices, and FIFOs fail
unopened. Publication remains withheld
until a target-side descriptor-confined transaction can create and synchronize
the candidate, verify exact bytes/owner/mode/link identity, use atomic
`RENAME_NOREPLACE` or `RENAME_EXCHANGE`, synchronize the parent, retain the old
identity, and replay an exact terminal receipt after acknowledgement loss.

This selector is not yet the launcher used by every SSH-domain reconnect.
`client.rs::wezterm_bin_path` still executes each domain's configured
`remote_wezterm_path`, or the legacy `wezterm` default. Existing domain configs
that point at `~/.local/bin/ft`, another FrankenTerm pathname, or `wezterm` do
not automatically resolve through `process-family/current`, and this setup
flow neither rewrites those configs nor republishes a stable proxy launcher at
their old path. Consequently this revision automates immutable pending-
generation publication, but does **not** automate selector activation, systemd
next-start selection, a mux upgrade, or restoration of domain reconnects.
Redirecting a live domain proxy remains withheld until the lifetime-lease and
readiness handoff lane can prove that it will not split a client from its
PTY-owning mux.

Before staging against a live owner, setup invokes the currently installed
remote `~/.local/bin/ft`, which is the client most likely to remain compatible
with that mux. If it supports `session dump`, both a complete dump and
`verify-dump` must succeed before any candidate bytes are downloaded. A legacy
client that predates the dump command emits an explicit unavailable warning and
may continue to non-activating staging, but the workflow records that no
verified content artifact exists and never promotes manual capture to proof.

Pending bytes are not an automated live upgrade. The command deliberately has
no activation step until the guardian owns PTYs and every mux launcher shares a
lifetime lease. Draining and stopping an empty mux is necessary but does not
make rerunning setup an activation command in this revision: rerun only
revalidates/publishes the same pending generation. The eventual handoff must
select the verified generation and start its exact process under one lease and
authenticated readiness transaction. Rollback is another lease-authorized,
validated selector switch; no operator should rename one component by hand.

An applied setup repeats the combined systemd/process mux-owner census after
staging. If an owner existed before staging and disappears, setup fails instead
of reporting preservation. An initially inactive host may remain inactive; that
is the expected safe result while activation/start are withheld. If an owner
appears concurrently, the summary reports it as unbound rather than treating it
as candidate activation. Neither census binds a PID to a stored generation or
holds a lifetime lease. A selector is stored authority, not proof that a
specific process is executing those bytes.

Before any deliberate restart, run the still-compatible client-side
`ft session dump --format json`, then run `ft session verify-dump <path>
--format json`; require both `complete: true` and `capture_complete: true`.
The dump preserves sequential redacted observable pane text and bounded
topology metadata only. It does not preserve mux-owned PTY descriptors, process
memory, or running-agent continuity, so it is an additional safety gate rather
than permission to restart an undrained mux.

### 7) Inspect the systemd user service for mux (publication withheld)

Expected future service file path:
```
~/.config/systemd/user/frankenterm-mux-server.service
```
Required future content shape (template only; this setup revision does not
publish it):
```
[Unit]
Description=FrankenTerm Mux Server
After=network.target

[Service]
Type=simple
ExecStart=/home/USER/.local/share/frankenterm/process-family/current/frankenterm-mux-server --daemonize=false
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

The future mux unit intentionally has no guardian ordering claim.
Once the guardian control surface exists and passes its mock-free service proof,
a future lease-authorized unit publisher may use the following one-way start
contract:

```ini
[Unit]
Wants=frankenterm-pty-guardian.service
After=network.target frankenterm-pty-guardian.service

[Service]
ExecStartPre=/absolute/path/frankenterm-pty-guardian probe \
  --socket-path %t/frankenterm/guardian/guardian.sock \
  --token-path %S/frankenterm/guardian/guardian.token
ExecStart=/absolute/path/frankenterm-mux-server --daemonize=false
```

`Wants=` requests guardian startup without acquiring stop propagation,
`After=` orders the jobs, and the authenticated `ExecStartPre=` probe prevents
mux execution unless the already-started guardian proves readiness. There is
no `Requires=`, `PartOf=`, or `BindsTo=` edge: stopping or restarting the mux
must not enqueue a guardian stop or restart.

#### Guardian unit scaffold (not activated yet)

The source-owned unit contract is:

```ini
[Unit]
Description=FrankenTerm PTY Guardian
Documentation=https://github.com/Dicklesworthstone/frankenterm
Before=frankenterm-mux-server.service
RefuseManualStop=yes

[Service]
Type=simple
UMask=0077
RuntimeDirectory=frankenterm/guardian
RuntimeDirectoryMode=0700
RuntimeDirectoryPreserve=yes
StateDirectory=frankenterm/guardian
StateDirectoryMode=0700
ExecStart=/absolute/path/frankenterm-pty-guardian serve \
  --socket-path %t/frankenterm/guardian/guardian.sock \
  --token-path %S/frankenterm/guardian/guardian.token
KillMode=control-group
Restart=no

[Install]
WantedBy=default.target
```

`RefuseManualStop=yes` is a manager-level guard against the ordinary
`systemctl --user stop/restart` path; it is not the pane-aware stop transaction
and is not treated as one. Dependency-driven unit shutdown and privileged
process termination remain operating-system authorities, not authenticated
guardian stop transactions. `RuntimeDirectoryPreserve=yes` prevents systemd
from deleting the retained guardian socket directory behind the guardian's
fail-closed lifecycle policy. `Restart=no` is intentional at this revision:
guardian process death does not preserve its live PTY descriptors, and the
current service refuses to unlink a retained socket implicitly. Claiming
automatic restart would therefore create both a false continuity guarantee and
a restart storm. `KillMode=control-group` makes forced unit termination
explicitly destructive and leak-free: this revision cannot recover the
guardian-owned PTY handles, so leaving apparently live but unowned child
processes behind would be a false survival guarantee. The authenticated
guarded-stop path reaches systemd exit only after proving the pane census is
empty. Its current in-memory response-flush handshake is not yet a durable stop
receipt: acknowledgment loss or a guardian crash can leave no authority from
which an exact retry can learn the terminal result. Publication additionally
requires a stable request/effect identity, a synchronized prepared/committed
stop record, exact replay after disconnect, and injected partial-write/crash
cuts proving that an authenticated success can never coexist with a running
replacement guardian.

Before this scaffold may be published, setup must provision the state and
runtime directories as current-user owner-only directories, create an exact
32-byte mode-0600 single-link token without following symlinks, and revalidate
the opened token identity. The guardian repeats those checks at startup. None
of those future steps authorizes replacing, unlinking, or truncating an
existing token or socket.

Read-only inspection commands:
```
ssh <host> 'service="$HOME/.config/systemd/user/frankenterm-mux-server.service"; if test ! -e "$service" && test ! -L "$service"; then printf "FT_REMOTE_SERVICE_STATE_V1=missing\n"; elif test -f "$service" && test ! -L "$service"; then printf "FT_REMOTE_SERVICE_STATE_V1=regular\n"; else printf "FT_REMOTE_SERVICE_STATE_V1=unsafe_shape\n"; fi'
ssh <host> "systemctl --user is-active frankenterm-mux-server || true"
```

No unit content is read and no unit write, `daemon-reload`, enable, or start
command is issued. The current SSH shell surface cannot prove
descriptor-confined create-new publication, exact readback, atomic
`RENAME_NOREPLACE`/`RENAME_EXCHANGE`, synchronized file and parent-directory
state, retained old identity, and an exact replayable receipt. Setup therefore
fails closed by withholding unit mutation rather than emulating durability with
path-based shell redirection or two `mv` operations.

### 8) Enable linger (mux survives logout)
Command (requires sudo):
```
ssh <host> "sudo loginctl enable-linger $USER"
```
- If sudo denied, print remediation steps; do not retry silently.

### 9) Inspect service state
```
ssh <host> "systemctl --user status frankenterm-mux-server"
```
- Parse status; report active/inactive. This is an observation, not a lease or
  proof of which generation a process executes.

### 10) Verify published generation

- The content-derived generation contains exactly the canonical manifest and
  matching `ft`/mux bytes. The mux `--version` probe occurs only after its
  destination hash is admitted and is followed by another exact hash check.
- Release-tag installs are checksum- and generation-manifest-verified before
  immutable pending publication. This setup revision leaves `current`
  unchanged.
- An already-active mux continues running its prior inode until the operator
  explicitly drains the hosted PTYs and restarts it.
- If a future lease-authorized transaction has created `current`,
  `current/ft --version` and `current/frankenterm-mux-server --version` identify
  the selected stored pair; they do not prove which generation a live mux runs.
- `ft doctor` / `ft doctor --json` emit diagnostics immediately, but current builds will report backend-prerequisite errors until WezTerm CLI is installed and `wezterm cli list --format json` can reach a running WezTerm GUI/mux.

---

## Idempotency Rules
- If WezTerm already installed, skip install.
- Observe an existing service-unit path without rewriting it. A missing,
  different, or unproved unit remains unchanged; setup does not claim exact unit
  identity through the current path-based shell surface.
- An exact existing content-derived generation is an idempotent retry only after
  complete nofollow revalidation. Conflicting bytes under that ID fail closed.
- Every install candidate remains pending until a cross-launcher startup lease
  exists. Setup does not change `current`, rewrite a current-bound unit, issue
  `daemon-reload`, enable the service, or start it.
- If linger already enabled, skip.
- Treat `ft` and `frankenterm-mux-server` as one versioned process family;
  never update or claim one without the other.
- Never restart an active mux as an installation side effect. Report the
  pending-vs-running distinction; this revision does not offer an unleased
  manual activation shortcut after drain.
- Installing, updating, stopping, or restarting the mux never stops, restarts,
  disables, or rewrites the guardian service. Guardian activation remains
  unavailable until the authenticated control boundary above lands.

---

## Observability
- Each step logs:
  - command string (redacted where needed)
  - duration
  - stdout/stderr (redacted)
  - status (ok/warn/error)
- Final summary includes:
  - what changed
  - immutable pending generations created (service backups are not created)
  - next steps

---

## Rollback Plan
- Never run `disable --now`, `stop`, `restart`, or kill the mux while it owns
  any pane PTY. The current mux is the only owner of those live descriptors;
  stopping it destroys running pane continuity, and a forensic content dump
  is not process restoration.
- While a mux is active, rollback may only stage the previous exact `ft` +
  `frankenterm-mux-server` process-family generation. It remains pending even
  after drain in this revision because setup has no startup/publication lease.
  Disabling future automatic starts, if desired, is the non-stopping operation:
```
ssh <host> "systemctl --user disable frankenterm-mux-server"
```
- Only a future lease-authorized transaction, after an independently verified
  empty pane/PTY census, may stop the mux and atomically exchange `current` to a
  fully revalidated prior generation. The displaced selector retained under
  `.selector-rollback-<transaction-uuid>` is evidence and a rollback input, not
  permission to bypass validation. The current setup command does not yet
  automate or certify the live handoff.
- Setup does not mutate the service file, so it has no service-file rollback
  step. Do not emulate one with a path-based `mv`; any future archive operation
  needs the same descriptor-confined durable transaction as publication.
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
- Do not add guardian stop/disable instructions to the mux rollback. A future
  guardian shutdown is a separate authenticated transaction and must refuse
  while any owned pane remains.

---

## Acceptance Criteria
- A reviewer can implement remote setup without re-reading PLAN.md.
- The spec enumerates commands, files, flags, logging, and rollback steps.
- Exact-generation retry is idempotent after complete nofollow revalidation;
  activation and service mutation are withheld by default. Full crash-cut
  behavior remains an executable acceptance boundary below.
- Source tests pin canonical content-derived manifests, exact receipt parsing,
  same-filesystem immutable publication, exact EEXIST retry versus conflict,
  descriptor-pinned version execution, bootstrap descriptor publication,
  nofollow/link/mode rejection, named-`generations` rebinding rejection, bounded
  collision-preserving rollback selectors, pending-only setup commands, absence
  of service-unit mutation/start commands, and preservation of legacy bytes.
  Full power-loss crash-cut and real-service identity proof remain separate
  executable acceptance gates rather than claims inferred from source.
- Source tests pin private guardian directories, ordinary-stop refusal,
  absence of mux lifecycle coupling, and authenticated probe ordering.
- Production guardian installation/start remains unclaimed until a mock-free
  service test proves the authenticated census/stop boundary and verifies that
  mux restart leaves guardian PID, census, children, and logs unchanged.
