# Codec atomic-redeploy constraint

**Bead:** [ft-og9bi](../.beads/issues.jsonl) (track A doc of ft-kuxho)
**Related:** [ft-kuxho](../.beads/issues.jsonl) parent epic, [`docs/codec-versions.md`](codec-versions.md), [`docs/proposals/ft-kuxho-B-codec-version-min-supported-window.md`](proposals/ft-kuxho-B-codec-version-min-supported-window.md), ft-nyvyl (slow-loris TLS handshake fix, ed413bd2)

## TL;DR

Today, **every `CODEC_VERSION` bump requires you to take down all mux
servers and clients, then bring them back up at the new version, with
no version skew tolerated in between.** This is a deployment
constraint, not a transient bug — the wire format and the version
field are both flat scalars and the codec has no negotiation. Until
ft-kuxho/B's `CODEC_VERSION_MIN_SUPPORTED` window lands, schedule
codec-bump deploys as maintenance windows and follow the runbook
below.

## 1. The failure mode

Two facts compose into the constraint:

1. **`CODEC_VERSION` is a single scalar.** `frankenterm/codec/src/lib.rs:650`:

   ```rust
   pub const CODEC_VERSION: usize = 46;
   ```

   The handshake (`GetCodecVersion` / `GetCodecVersionResponse`, PDUs
   26/27) returns one number. Existing client code compares with `==`
   and aborts on mismatch — there is no version window, no
   negotiation, no fallback path.

2. **varbincode is a positional binary format.** Field offsets are
   computed by serial position. Adding, removing, reordering, or
   type-changing any field shifts every subsequent field's offset in
   the encoded payload. The known-bad pattern documented in
   `frankenterm/codec/src/lib.rs:1204-1228` is `skip_serializing_if`
   on `Option<T>`: it elides the tag byte for `None`, misaligning the
   decoder for every downstream field.

Combined: a v46 client decoding a v47 PDU sees a different field
layout for any modified PDU and either:

- decodes garbage values and proceeds — silent corruption, very rare
  because length prefixes catch most cases;
- hits a "failed to fill whole buffer" or
  "tag byte out of range" decode error — the common case;
- hits a buffer-overrun panic on a defensive `assert!` — the worst case
  when the misalignment lands inside a length-prefixed `Vec`.

There is no graceful fallback. The client either drops the PDU or
crashes the connection.

## 2. The long-lived TLS mux session caveat

Production mux deployments use long-lived TLS sessions (per
`crates/frankenterm-mux-server/src/ossl.rs`; the slow-loris handshake
window was bounded in ft-nyvyl / ed413bd2 but the *post-handshake*
session is unlimited by design). Sessions stay open for hours; some
agent panes hold a single connection for the lifetime of a workflow.

This means:

- **Rolling upgrades are not transparent.** Bumping the server to v47
  while v46 clients are still connected does not migrate those
  sessions — they continue to speak v46 because the handshake already
  completed, but every new PDU shape change after the bump corrupts
  their wire stream.
- **Connection drops are the *correct* response.** Operators must
  expect the server upgrade to terminate every active session.
  Clients reconnect, re-handshake at v47, and resume. There is no
  graceful-handoff path.
- **In-flight workflow state survives connection drops** (the
  recorder + checkpoint subsystem is independent of the wire
  protocol), but any client-side cache of pane state, scrollback, or
  pending RPCs is lost.

## 3. Operator runbook for a `CODEC_VERSION` bump

Until ft-kuxho/B's `CODEC_VERSION_MIN_SUPPORTED` window lands, follow
this sequence for any deploy that includes a `CODEC_VERSION` bump.

### 3a. Pre-deploy checklist

- [ ] Confirm the bump is in the build by running `git diff main -- frankenterm/codec/src/lib.rs` and verifying the new value.
- [ ] Confirm the corresponding row exists in [`docs/codec-versions.md`](codec-versions.md). The CI guard `scripts/check_codec_version_release_notes.sh` (ft-8smkj) blocks merges that miss this, so reaching the deploy stage means it's there — but spot-check it in case of merge artifacts.
- [ ] Schedule a maintenance window. Communicate the expected blast radius to operators of every workspace that has long-lived agent panes connected to a remote mux.
- [ ] Pre-position the new client binaries on every workstation that will reconnect. The shorter the gap between server flip and client roll, the shorter the window of unavailability.

### 3b. Deploy order — server first, then clients

The strictly-correct order is:

1. **Drain new connections to the old servers.** If you run multiple mux servers behind a load balancer, drain them one at a time. If single-server, skip to step 2.
2. **Stop the old server.** Active sessions terminate; clients receive a connection drop.
3. **Start the new server at v47.** Verify it accepts handshakes by attaching one v47 client.
4. **Roll clients to v47.** Each client reconnects, handshakes at v47, and resumes. Until clients are rolled, they are not actually offline — they are crash-looping at handshake (server says 47, client says 46, both abort). That is the expected steady state during the rollout.
5. **Verify zero v46 clients remain.** Check the mux server's structured logs for `codec_version=46` handshake-reject lines; they should taper to zero within the rollout window.

### 3c. Cross-deploy coordination

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

### 3d. Rollback

If the new build has a regression, roll back by reversing steps 3-5:
stop the v47 server, start a v46 server, let the v47 clients fail
their handshake until rolled back. Same pattern, opposite direction.

## 4. The future relief valve

[ft-kuxho/B](proposals/ft-kuxho-B-codec-version-min-supported-window.md)
proposes a `CODEC_VERSION_MIN_SUPPORTED` constant that creates a
backward-compat window: clients at any version
`CODEC_VERSION_MIN_SUPPORTED <= v <= CODEC_VERSION` can interop. Once
that lands:

- **Additive bumps** (new PDU variant; new field with `serde(default)`
  appended at the end of a struct — see ft-e1emx tail-padding
  conformance harness at `frankenterm/codec/src/lib.rs:3120+`) become
  rolling-upgrade safe. `CODEC_VERSION` advances; `MIN` does not.
  Old clients continue to decode the canonical prefix and ignore the
  trailing extra bytes.
- **Breaking bumps** still require the runbook above. Removing or
  reordering fields, changing types, or inserting fields in the
  middle of a struct all bump `MIN` and force atomic redeploy.

The runbook in §3 should be retained for breaking bumps; the
"maintenance window" overhead can be skipped for additive bumps once
the window mechanism is in place. Until then, treat all bumps as
breaking.

## 5. Cross-references

- [`docs/codec-versions.md`](codec-versions.md) — the version-history
  release-note file enforced by ft-8smkj.
- [`docs/proposals/ft-kuxho-B-codec-version-min-supported-window.md`](proposals/ft-kuxho-B-codec-version-min-supported-window.md) — the design for the rolling-upgrade window.
- `scripts/check_codec_version_release_notes.sh` — the CI guard that
  prevents silent CODEC_VERSION bumps.
- `frankenterm/codec/src/lib.rs:650` — the `CODEC_VERSION` constant.
- `frankenterm/codec/src/lib.rs:1204-1228` — comments documenting the
  varbincode positional-format hazard.
