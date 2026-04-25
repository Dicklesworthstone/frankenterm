# Proposal: Codec Versioning for Rolling Upgrades

**Status:** Draft
**Tracking bead:** ft-kuxho ([STRATEGIC/HIGH])
**Author:** strategic-analysis pane (cod), 2026-04-25
**Scope:** `frankenterm/codec/`, `frankenterm-mux-server*`, `frankenterm/client/`

## Problem

The mux wire protocol has two coupled rigidity sources that compound:

1. **`CODEC_VERSION` is a single global scalar.** `frankenterm/codec/src/lib.rs:650`
   declares `pub const CODEC_VERSION: usize = 46;`, and the handshake at
   `frankenterm/client/src/client.rs:1442` rejects connections with strict
   equality:

       Ok(info) if info.codec_vers == CODEC_VERSION => …
       Ok(info) => return Err(IncompatibleVersionError { … }.into())

   Any version drift between client and server fails the handshake. There is
   no negotiated downgrade, no minimum-supported version, no "I speak 44–46"
   range.

2. **`varbincode` is positional.** Per the existing memory note
   `feedback_varbincode_skip_serializing.md`, `#[serde(skip_serializing_if=…)]`
   breaks varbincode because the format is positional binary. Adding,
   removing, or reordering a single field in any of the ~95 PDU structs
   (`Pdu` enum at codec/lib.rs:656–~770) shifts every offset that follows it.

Combined: every PDU change is a coordinated atomic redeploy. The
`distributed` feature is on by default, and long-lived TLS mux sessions
(see ft-nyvyl slow-loris context) make atomic redeploy operationally
expensive — we'd have to terminate every active client, ship both halves
in lockstep, then reconnect. There is no rolling-upgrade path today.

## Why this matters now

* Wire-protocol changes will accelerate as the runtime, recorder, and
  workflow surfaces stabilise. Each `Pdu` addition currently bumps
  `CODEC_VERSION` and walls off old binaries.
* The mux topology is becoming multi-tenant (federated mux servers,
  per-zone connectors). A scalar global version forces the entire fleet
  to step in unison.
* Operators want to upgrade the server side without dropping every active
  pane. A strict-equality handshake makes that infeasible.

## Design space

Three positions, ordered by ambition:

### A. Document the constraint and harden the failure mode

Cheapest. We accept that the protocol is atomic-redeploy and:

* Add a doc page under `docs/architecture/` describing the constraint and
  the deploy procedure (drain → upgrade both halves → reopen).
* Improve `IncompatibleVersionError` to print the exact field-shape diff
  hint and a `--ignore-codec-version` escape hatch for read-only ops on
  mismatched fleets.
* Add a test that fails CI when `CODEC_VERSION` bumps without a release
  note row.

No protocol change. Rolling upgrades remain impossible.

### B. Scalar version + minimum-supported-version range

Moderate. Replace strict equality with an explicit window:

* `CODEC_VERSION_CURRENT: u32` (what we send)
* `CODEC_VERSION_MIN_SUPPORTED: u32` (oldest peer we accept)
* Handshake accepts when `peer.codec_vers >= MIN_SUPPORTED &&
  peer.codec_vers <= CURRENT`.
* Bumping `CURRENT` is cheap; bumping `MIN_SUPPORTED` becomes the explicit
  "drop old clients" event.
* Field-level changes still break varbincode positions, so the practical
  rolling window is "additions of fully new PDU types only" — the new
  PDUs simply aren't sent if the peer's version is below `CURRENT`.

This unlocks rolling upgrades for the most common kind of change (new
PDUs) at low cost. It does NOT solve "evolve an existing PDU's fields".

### C. Replace varbincode with a self-describing tail or per-PDU version

Largest. Break the positional constraint:

* **Tail-padded variant.** Add an explicit `Vec<u8>` tail to each PDU
  body that encodes optional fields as a length-prefixed sub-record.
  Old peers ignore the tail; new peers parse it. Compatible with
  varbincode's positional invariant because the new bytes always come
  after the existing ones.
* **Per-PDU version field.** Each PDU struct gains a leading
  `pdu_version: u16`. The handler dispatches on (`pdu_kind`, `pdu_version`)
  to a typed deserialiser. Fields are added by adding a new version, not
  by editing the existing struct.
* **Switch encoder.** Replace varbincode with `bincode 2.x` (which has
  explicit tail-padding semantics) or `postcard` (length-prefixed
  variants), gated by a feature flag during the cutover.

This unlocks evolving existing PDUs without breaking the wire. It is
also a multi-month project and is the hardest to land safely.

## Recommendation

Land **A immediately** (docs + better error + CI guard) so operators can
plan around the current constraint.

Then land **B as a follow-up** once two things are true: (i) we have a
release-note pipeline that surfaces `CODEC_VERSION` bumps, and (ii) we
have at least one concrete need for a rolling upgrade (e.g., a security
patch that must ship without dropping mux sessions).

Defer **C** until B's window-based rolling upgrades feel constraining —
which will most likely be when we want to add fields to an existing,
high-traffic PDU. Track that signal explicitly rather than pre-solving.

## Out of scope for this proposal

* Authentication / TLS rotation across rolling upgrades. Separate problem.
* The `IpcAuthToken` / `IpcConfig` rolling-upgrade story for the unix-socket
  surface. Different protocol, different file.
* Migrating `varbincode` to async-aware streaming codecs.

## Child beads (proposed)

To be filed under ft-kuxho once this proposal lands:

1. **Document atomic-redeploy constraint.** Add `docs/architecture/codec-deploy.md`
   covering CODEC_VERSION discipline, the strict-equality handshake,
   and the recommended drain/upgrade procedure. (Path A docs deliverable.)
2. **CI guard for CODEC_VERSION bumps.** A test that requires a release-notes
   row whenever `CODEC_VERSION` changes; rejects silent bumps.
3. **Improve IncompatibleVersionError.** Include the local + remote
   versions, a brief "what likely changed", and a pointer at the new
   docs page.
4. **Scope `CODEC_VERSION_MIN_SUPPORTED` (Path B).** Design + RFC: how
   to decide when to bump MIN, how to test the window, how to surface
   the matrix in `ft session doctor`.
5. **(Long-tail) Spike: tail-padded PDU variant.** Build one PDU
   with a length-prefixed optional-fields tail and round-trip it through
   the existing varbincode, to prove the Path C primitive.
