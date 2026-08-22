# Codec version-window and atomic-redeploy constraint

**Bead:** [ft-og9bi](../.beads/issues.jsonl) (track A doc of ft-kuxho)
**Related:** [ft-kuxho](../.beads/issues.jsonl) parent epic, ft-6vfeq
(v0.15.1 stranded-remote release repair),
[`docs/codec-versions.md`](codec-versions.md),
[`docs/proposals/ft-kuxho-B-codec-version-min-supported-window.md`](proposals/ft-kuxho-B-codec-version-min-supported-window.md),
ft-nyvyl (slow-loris TLS handshake fix, ed413bd2)

## TL;DR

`CODEC_VERSION_MIN_SUPPORTED` is live. Each peer advertises an inclusive
`min_supported..=codec_vers` window, the handshake rejects disjoint or
impossible windows, and a successful connection retains the lower peer's
canonical version as its agreed dialect.

- An **additive** change uses a new PDU identity and advances only
  `CODEC_VERSION`. It is rolling-upgrade safe when both inbound and outbound
  admission gate that PDU on its minimum dialect. Old peers continue at the
  previously agreed dialect and never receive the new PDU.
- A **breaking** positional-schema change advances both `CODEC_VERSION` and
  `CODEC_VERSION_MIN_SUPPORTED`. Its windows are intentionally disjoint from
  the old release, so it still requires the maintenance-window runbook below.

Do not infer the live window from this prose: read `CODEC_VERSION` and
`CODEC_VERSION_MIN_SUPPORTED` in `frankenterm/codec/src/lib.rs`. The v0.15.1
incident window is `61..=63`; remote mux servers left at codec 46 have no
overlap and are rejected before topology publication. PDU93/94 was additive in
the historical v56/v57 window and therefore unavailable, without wire
emission, on a negotiated v56 connection.

## 1. The remaining failure mode

The version window prevents a new peer from emitting a new PDU to an old peer;
it does not make arbitrary schema edits compatible. Varbincode is a positional
binary format. Field offsets are
   computed by serial position. Adding, removing, reordering, or
   type-changing any field shifts every subsequent field's offset in
   the encoded payload. The known-bad pattern documented in
   `frankenterm/codec/src/lib.rs` is `skip_serializing_if`
   on `Option<T>`: it elides the tag byte for `None`, misaligning the
   decoder for every downstream field.

An old peer decoding a modified positional schema under an incorrectly shared
dialect can therefore:

- decode garbage values and proceed — silent corruption, very rare
  because length prefixes catch most cases;
- hit a "failed to fill whole buffer" or
  "tag byte out of range" decode error — the common case;
- hit a buffer-overrun panic on a defensive `assert!` — the worst case
  when the misalignment lands inside a length-prefixed `Vec`.

`serde(default)` does not repair canonical old varbincode bytes that end at EOF;
the newer decoder still asks for the missing field. A changed schema therefore
needs a new PDU identity, an explicit dual-schema decoder proven against real
old and new frames, or a simultaneous minimum-version advance.

## 2. The long-lived TLS mux session caveat

Production mux deployments use long-lived TLS sessions (per
`crates/frankenterm-mux-server/src/ossl.rs`; the slow-loris handshake
window was bounded in ft-nyvyl / ed413bd2 but the *post-handshake*
session is unlimited by design). Sessions stay open for hours; some
agent panes hold a single connection for the lifetime of a workflow.

This means:

- **A connection's agreed dialect is immutable.** A v57/v56 pair speaks v56
  for that connection's lifetime. It cannot begin using a v57-only PDU after a
  binary elsewhere is upgraded.
- **Additive rollout is safe, not magical.** Restarting a server still drops
  its own sockets, but mixed-version peers can reconnect in either order and
  negotiate the shared dialect. A load-balanced rolling restart need not create
  a fleet-wide maintenance window for an additive bump.
- **Breaking rollout remains atomic.** Advancing the minimum makes old/new
  windows disjoint by design; those peers reject the handshake until they are
  brought to a common version.
- **A transport-only connection drop does not kill mux-owned PTYs.** The
  recorder/checkpoint subsystem is independent of the wire protocol, and the
  reconnect path can re-list and reattach panes after a compatible successor
  connection is established. A mux process exit is categorically different:
  the current mux owns each PTY master and child handle, so its shutdown or
  crash can terminate the child and cannot be described as workflow survival.
  Client-side caches and pending RPCs are generation-local in either case.

## 3. Operator runbook for a codec bump

### 3a. Pre-deploy checklist

- [ ] Classify the change as additive or breaking before deployment.
- [ ] Confirm the intended `CODEC_VERSION` and
  `CODEC_VERSION_MIN_SUPPORTED` values in
  `frankenterm/codec/src/lib.rs`.
- [ ] Confirm the corresponding row exists in [`docs/codec-versions.md`](codec-versions.md). The CI guard `scripts/check_codec_version_release_notes.sh` (ft-8smkj) blocks merges that miss this, so reaching the deploy stage means it's there — but spot-check it in case of merge artifacts.
- [ ] For every new PDU identity, confirm the registry declares its exact minimum
  dialect and producer/serial authority, and retain a negative test proving an
  older agreed dialect rejects it before serial allocation or wire emission.
- [ ] For a breaking change, schedule a maintenance window and confirm the Unix
  release archive contains both `ft` and `frankenterm-mux-server`, covered by
  the same atomic component manifest and sealed build ID.
- [ ] Pre-position the new CLI with `--no-app`; do not launch the new desktop
  client until every remote mux has been staged, drained, and restarted.

### 3b. Additive deploy

When only `CODEC_VERSION` advances and the old minimum is retained:

1. Roll servers or clients in either order using the ordinary bounded restart
   procedure.
2. Verify mixed-version handshakes agree on the lower canonical version.
3. Verify the new PDU is admitted only after both peers negotiate its minimum
   dialect; capability absence must be a healthy, aligned result rather than a
   transport failure.
4. Complete the rollout, then verify new/new connections use the new dialect.

### 3c. Breaking deploy — server first, then clients

When both the maximum and minimum advance, use the maintenance window:

1. **Capture and verify the currently compatible live mux state.** Before
   replacing the client that can still speak to every old server, export all
   reachable pane text and topology metadata:

   ```bash
   NEW_TAG=vX.Y.Z
   DUMP_PATH="$HOME/.local/share/ft/mux-dumps/pre-upgrade-${NEW_TAG}-$(date +%s).json"
   ~/.local/bin/ft session dump --output "$DUMP_PATH" --format json
   ~/.local/bin/ft session verify-dump "$DUMP_PATH" --format json
   ```

   Both commands must succeed, the dump must report `complete: true`, and the
   verifier must report `capture_complete: true`. This is a sequential
   redacted-content/topology safety artifact, not process continuity: it cannot
   keep mux-owned PTYs or agents alive.

   The v0.15.1 stranded-remote incident predates this dump command. If the
   installed compatible client does not recognize `session dump`, there is no
   complete automated artifact to promote: keep the old mux running, capture
   critical pane text manually through that compatible client where possible,
   and record the legacy gap. Never describe that fallback as a verified dump.

2. **Install the candidate CLI/process-family bytes without replacing the
   running desktop app.** Use the installer from the exact release source
   revision. The installer transactionally publishes `ft` plus its matched mux
   server and preserves the prior binaries; `--no-app` leaves the already-running
   codec-compatible GUI process untouched:

   ```bash
   NEW_TAG=vX.Y.Z
   bash install.sh --version "$NEW_TAG" --no-app
   ```

3. **Stage each remote process family without restarting its mux.** Repeat for
   every configured domain host using the candidate CLI:

   ```bash
   SSH_HOST=trj
   ~/.local/bin/ft setup --apply remote "$SSH_HOST" --yes \
     --install-ft --ft-version "$NEW_TAG"
   ```

   For a live mux, setup downloads and verifies the release pair into a unique
   cache directory and publishes both files only as matching `pending-*` paths.
   Before that download, it also asks the currently installed remote CLI to
   create and verify a complete host-local dump when that legacy release
   supports the command; a supported dump failure stops staging.
   It deliberately leaves `~/.local/bin/ft`, the service `ExecStart`, the active
   mux inode, and its PTYs untouched. An inactive host may transactionally
   activate the pair immediately, restoring the prior pair if publication
   fails. A client-only or mixed-build archive fails closed.
4. **Drain live PTYs and new connections.** For a single mux, finish or move all
   sessions it owns. For a load-balanced fleet, drain one server at a time.
   Do not restart a mux while it owns work that cannot be recreated.
5. **Activate the staged pair only after the host is drained.** The current
   release does not yet provide the guardian-backed activation transaction, so
   pending paths are a non-activating safety boundary, not an instruction to
   overwrite files ad hoc. Stop the now-empty mux, then rerun the same setup
   command. The inactive-host branch verifies and transactionally publishes the
   matched pair with rollback on partial publication, updates the unit, and
   enables the new mux. Active client sockets drop at the stop; the old desktop
   may reject the new disjoint window until the client rollout completes:

   ```bash
   ssh "$SSH_HOST" 'systemctl --user stop frankenterm-mux-server'
   ~/.local/bin/ft setup --apply remote "$SSH_HOST" --yes \
     --install-ft --ft-version "$NEW_TAG"
   ssh "$SSH_HOST" 'systemctl --user status frankenterm-mux-server --no-pager'
   ```

6. **Verify the new server, then update the desktop app.** Confirm the server
   identity through its service status first. Only after every target server is
   activated should the exact installer replace the still-running compatible
   GUI bundle:

   ```bash
   bash install.sh --version "$NEW_TAG"
   ```

7. **Verify automatic domain reconnect and zero old-dialect peers.** Check the
   domain connection UI/toast and structured handshake logs for both inclusive
   codec windows. A failure must display the local and remote windows plus the
   safe server-upgrade/desktop-rollback choices; it must never look like a
   no-op.

### 3d. Cross-deploy coordination

- **ft-zoxxq mux server** and **ft-nyvyl client** binaries must both be
  bumped to the same `CODEC_VERSION` in the same release. The
  ft-zoxxq.4 CI guard
  (`scripts/check_mux_interface_imports.sh`) is independent of this
  contract — it codifies the wezterm-fork stance for the trait
  surface, not the wire version.
- **Recorder/replay artifacts** are not affected. The recorder log
  format is independent of the codec wire format; old recordings
  remain decodable across `CODEC_VERSION` bumps.
- **Workflows mid-execution** can survive transport-only connection drops while
  their mux stays alive. Operators should expect a visible stall followed by
  reattachment after a compatible reconnect. Do not extend that claim to a mux
  restart or crash until PTY ownership has moved into the durable guardian:
  recorder/checkpoint rows preserve evidence, not live process descriptors.

`ft setup remote --install-ft` fences the same split-brain at deployment time.
With a live mux, either a locally supplied exact process family or a verified
release-tag family is written to unique `pending-*` paths and the active `ft`
plus mux-server bytes remain unchanged. Release-tag bytes are first verified in
a unique non-active cache directory. This prevents a newly activated client
from becoming unable to speak to the still-running old mux; it does not claim
to perform the later lossless mux handoff.

### 3e. Rollback

For an additive bump, roll components back within the retained compatibility
window and verify they negotiate the lower dialect. For a breaking bump, reverse
the breaking-deploy sequence: drain and stop the new server, restore the
timestamped `frankenterm-mux-server.previous-*` binary and service unit, start
the old server, and restore the preserved desktop app/CLI until both sides again
share a window. Never restart either server generation until its live PTYs have
been drained.

## 4. Window invariants

- A peer must advertise `min_supported <= codec_vers`; impossible windows fail
  closed.
- The two inclusive windows must overlap.
- The agreed dialect is `min(local.codec_vers, remote.codec_vers)` after the
  overlap check, and remains fixed for the connection generation.
- Every outbound PDU is checked against that agreed dialect before serial
  allocation and first write. Every inbound PDU is checked against the same
  dialect and its producer/role authority.
- A new PDU identity can be additive. A changed positional schema is not
  additive merely because a new Rust field has `serde(default)`.
- `CODEC_VERSION_MIN_SUPPORTED` advances only with a deliberately breaking
  release and its maintenance-window evidence.

## 5. Cross-references

- [`docs/codec-versions.md`](codec-versions.md) — the version-history
  release-note file enforced by ft-8smkj.
- [`docs/proposals/ft-kuxho-B-codec-version-min-supported-window.md`](proposals/ft-kuxho-B-codec-version-min-supported-window.md) — the design and proof history for the live rolling-upgrade window.
- `scripts/check_codec_version_release_notes.sh` — the CI guard that
  prevents silent CODEC_VERSION bumps.
- `frankenterm/codec/src/lib.rs` — codec/minimum constants, wire registry,
  handshake compatibility, and positional-format hazard commentary.
