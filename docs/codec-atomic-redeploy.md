# Codec version-window and atomic-redeploy constraint

**Bead:** [ft-og9bi](../.beads/issues.jsonl) (track A doc of ft-kuxho)
**Related:** [ft-kuxho](../.beads/issues.jsonl) parent epic, [`docs/codec-versions.md`](codec-versions.md), [`docs/proposals/ft-kuxho-B-codec-version-min-supported-window.md`](proposals/ft-kuxho-B-codec-version-min-supported-window.md), ft-nyvyl (slow-loris TLS handshake fix, ed413bd2)

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

The current release is codec v57 with a v56 floor. PDU93/94 is additive and
therefore unavailable, without wire emission, on a negotiated v56 connection.

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
- **In-flight workflow state survives connection drops** (the
  recorder + checkpoint subsystem is independent of the wire
  protocol), but any client-side cache of pane state, scrollback, or
  pending RPCs is lost.

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
- [ ] For a breaking change, schedule a maintenance window and pre-position the
  new client binaries on every workstation that will reconnect.

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

1. **Drain new connections to the old servers.** If you run multiple mux servers behind a load balancer, drain them one at a time. If single-server, skip to step 2.
2. **Stop the old server.** Active sessions terminate; clients receive a connection drop.
3. **Start the new server.** Verify it accepts a same-version handshake.
4. **Roll clients.** Each client reconnects and resumes. Old clients reject the
   disjoint handshake until upgraded; that fail-closed state is expected.
5. **Verify zero old-dialect clients remain.** Check structured handshake logs
   using the exact old and new version numbers for this release.

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
- **Workflows mid-execution** survive connection drops via the
  recorder checkpoint subsystem. Operators should expect every
  in-flight workflow to pause on the connection drop and resume on
  reconnect; there is no data loss but there is a visible stall.

### 3e. Rollback

For an additive bump, roll components back within the retained compatibility
window and verify they negotiate the lower dialect. For a breaking bump, reverse
the breaking-deploy sequence: stop the new server, start the old server, and
roll clients back until both sides again share a window.

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
