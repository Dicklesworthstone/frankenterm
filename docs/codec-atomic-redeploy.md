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
   ~/.local/bin/ft session verify-dump "$DUMP_PATH" --require-complete --format json
   ```

   Both commands must succeed, the dump must report `complete: true`, and the
   verifier must report `capture_complete: true`. This is a sequential
   redacted-content/topology safety artifact, not process continuity: it cannot
   keep mux-owned PTYs or agents alive.

   The v0.15.1 stranded-remote incident predates this dump command. If the
   installed compatible client does not recognize `session dump`, keep the old
   client and mux running and continue with the side-by-side candidate staging
   below. The candidate has an explicit `compatible-client-dump` bridge for the
   exact v0.13.0 client; do not replace or launch the desktop app before that
   bridge has produced a complete verified artifact.

2. **Install the candidate CLI/process-family bytes without replacing the
   running desktop app.** Use the installer from the exact release source
   revision. The installer transactionally publishes `ft` plus its matched mux
   server and preserves the prior binaries; `--no-app` leaves the already-running
   codec-compatible GUI process untouched:

   ```bash
   NEW_TAG=vX.Y.Z
   bash install.sh --version "$NEW_TAG" --no-app
   ```

   When the live compatible process is v0.13.0, use the staged candidate to
   drive the exact retained v0.13.0 CLI through the live GUI mux socket. Pin the
   old executable by both SHA-256 and byte length and choose a new private
   output path:

   ```bash
   CANDIDATE_FT="$HOME/.local/bin/ft"
   COMPATIBLE_FT="/Applications/FrankenTerm.app/Contents/MacOS/ft"
   COMPATIBLE_SHA256="$(shasum -a 256 "$COMPATIBLE_FT" | cut -d ' ' -f 1)"
   COMPATIBLE_BYTES="$(stat -f %z "$COMPATIBLE_FT")"
   LIVE_MUX_SOCKET="$HOME/.local/share/frankenterm/frankenterm-gui-sock-<pid>"
   DUMP_PATH="$HOME/.local/share/ft/mux-dumps/pre-upgrade-${NEW_TAG}-$(date +%s).json"

   "$CANDIDATE_FT" session compatible-client-dump \
     --client "$COMPATIBLE_FT" \
     --expected-client-sha256 "$COMPATIBLE_SHA256" \
     --expected-client-bytes "$COMPATIBLE_BYTES" \
     --mux-socket "$LIVE_MUX_SOCKET" \
     --output "$DUMP_PATH" \
     --format json
   "$CANDIDATE_FT" session verify-dump "$DUMP_PATH" --require-complete --format json
   ```

   The bridge accepts only v0.13.0 build `3ebd60566`, pins the client and socket
   file identities, runs the old Robot API in a sterile private environment,
   batches bounded pane reads between two censuses, reapplies redaction, and
   publishes a schema-v2 artifact through the ordinary no-clobber dump
   verifier. Because the old Robot state may project one numeric pane into
   multiple window/tab rows, schema v2 separates unique `content_targets` from
   lossless `projections`: each unique numeric pane ID receives exactly one
   `get-text` request and one untruncated outcome, every distinct
   `(window_id, tab_id, pane_id)` row is retained, and duplicate or
   metadata-conflicting aliases fail closed. Without incarnation authority,
   that exact request/outcome accounting is not an ABA-exclusion claim.
   `pane_count` and verifier-derived domain counts therefore count unique
   content targets; `projection_count` reports the retained topology rows. The
   client subprocess batches run sequentially, but v0.13 may read panes within
   one batch concurrently, so the content remains bounded batch-concurrent,
   best-effort, and non-atomic. The topology remains explicitly limited to the
   v0.13 projection and does not invent workspace, geometry,
   active/zoom, stable incarnation, or authoritative domain identity.

   Retry with the exact same output path and request bounds after an ambiguous
   reply. The bridge first performs an offline Query/Ack: an existing complete
   schema-v2 artifact must match the expected client hash/length/version,
   mux-socket path digest, and all resource bounds before it is acknowledged
   without contacting the mux. A mismatch, incomplete artifact, or legacy v1
   artifact is retained and fails closed. Legacy v1 remains offline-verifiable
   for forensic use but cannot satisfy the current alias-aware capture contract.
   The exact output path and bound request tuple are the offline idempotency
   key. Query/Ack does not require the historical socket to remain present, so
   it still works if the mux crashed after publication; use a new output path
   to request a genuinely new capture. A reconciled receipt is therefore not
   current socket-incarnation or liveness evidence and cannot authorize
   activation.
   Offline verification also requires a whole-second batch timeout, recomputes
   the producer's batches-plus-two-censuses total deadline, and requires the
   tested 16-KiB minimum needed to fit the frozen v0.13 maximum-size empty
   batch control envelope. These checks reject known source-impossible bounds
   even when all JSON checksums were recomputed. The artifact does not retain
   per-batch raw stdout receipts, so this check does not claim the configured
   allowance equals the bytes each subprocess actually emitted. Fresh
   publication then re-locates the private recovery environment before saying
   it is retained; Query/Ack repeats that bounded check and may truthfully
   report it absent without invalidating the durable forensic artifact.
   The receipt includes verifier-derived `domain_pane_counts`; record and check
   the expected domain counts before admitting the artifact to any later
   activation transaction.
   Enforce those recorded counts offline with repeated
   `ft session verify-dump <artifact> --require-complete --expect-domain-panes DOMAIN=COUNT`
   arguments. Missing or mismatched domains fail closed.
   Success still means redacted forensic text/topology only; the receipt states
   `executable_restore_image: false` and `production_mux_activation: false`.

3. **Stage each remote process family without restarting its mux.** Repeat for
   every configured domain host using the candidate CLI:

   ```bash
   SSH_HOST=trj
   # Generate once, record it with the rollout, and reuse it after every retry.
   TRANSACTION_ID=0123456789abcdef0123456789abcdef
   ~/.local/bin/ft setup remote "$SSH_HOST" --apply --yes \
     --install-ft --ft-version "$NEW_TAG" \
     --transaction-id "$TRANSACTION_ID"
   ```

   Setup downloads and verifies the exact release triplet, copies all three components
   into one private destination-filesystem stage, synchronizes their bytes and
   manifest, and publishes one immutable content-derived generation. The stable
   caller transaction ID binds the exact component hashes, generation identity,
   committed authorization records, and replayable `PendingLiveOwner` receipt.
   Retrying that same ID revalidates or resumes the same claim; changing the
   payload under it fails closed. Local uploads likewise admit only a missing
   stage or an exact retained stage with the expected private regular-file
   identity and checksum.

   Before that download, it also asks the currently installed remote CLI to
   create and verify a complete host-local dump when that legacy release
   supports the command; a supported dump failure stops staging. Some legacy
   mux hosts have no remote `ft` CLI at all. The resulting
   `unavailable_no_compatible_cli` or `unavailable_legacy_client` marker permits
   immutable staging only; it is not dump evidence and cannot authorize a
   restart. In that topology, the local compatible GUI/client bridge above must
   capture the reachable remote-domain panes before any later activation.
   It deliberately leaves the `current` selector, `~/.local/bin/ft`, the service
   `ExecStart`, the active mux inode, and its PTYs untouched. An inactive host is
   also left pending: current source has no cross-launcher lifetime lease and
   therefore no production activation path. A client-only or mixed-build
   archive fails closed.
4. **Keep the compatible mux and its PTYs live.** Do not drain or stop work merely
   to run the current staging command. A future non-guardian migration may
   require an explicit drain, but no restart is safe while the mux owns work
   that cannot be recreated.
5. **Do not activate with this release.** Draining a host does not manufacture
   the missing lifetime lease, authenticated successor readiness proof, or
   rollback transaction. Rerunning setup with the same transaction ID only
   revalidates/replays the immutable pending generation; it does not change
   `current`, rewrite the unit, start a successor, or restore domain reconnects.
   Keep the compatible mux running until the guardian-backed activation command
   is implemented and independently proven. Do not stop the mux and overwrite
   either component by hand.

6. **Update the desktop app only after a separately proven server rollout.**
   Current setup cannot supply that activation proof. Once a future
   lease-authorized rollout has verified every exact running server generation,
   the exact installer may replace the still-running compatible GUI bundle:

   ```bash
   bash install.sh --version "$NEW_TAG"
   ```

7. **Verify automatic domain reconnect and zero old-dialect peers.** Check the
   domain connection UI/toast and structured handshake logs for both inclusive
   codec windows. A failure must display the local and remote windows plus the
   safe server-upgrade/desktop-rollback choices; it must never look like a
   no-op. One failing domain must not suppress later configured domains or
   ordinary GUI startup. Initially unavailable auto domains retry independently
   with bounded concurrency and jittered backoff; an explicit or default remote
   start opens a local recovery shell rather than leaving an inert window.

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
release-tag family is published as one immutable content-derived generation and
committed as `PendingLiveOwner`; the active selector and running bytes remain
unchanged. This prevents setup itself from splitting a newly selected client
from the still-running old mux. It does not perform the later lossless mux
handoff.

### 3e. Rollback

For an additive bump, roll components back within the retained compatibility
window and verify they negotiate the lower dialect. Current remote setup does
not activate its pending generation, so its safe rollback is simply to leave
the old selector, service and mux untouched. A future breaking-deploy rollback
must run under the same lifetime lease, revalidate the retained old immutable
generation, atomically switch the selector, authenticate the old server's
readiness, and preserve guardian-owned PTYs throughout. No current manual
pathname replacement is an authorized substitute.

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
