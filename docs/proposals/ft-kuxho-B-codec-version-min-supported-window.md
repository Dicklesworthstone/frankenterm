# Proposal: `CODEC_VERSION_MIN_SUPPORTED` window for additive PDU rolling upgrade

**Bead:** [ft-k6jco](../../.beads/issues.jsonl) (track B of ft-kuxho)
**Status:** implemented, with the 2026-07-31 schema-evolution erratum below
**Related:** ft-kuxho (parent epic), ft-8smkj (track A: CI guard against silent CODEC_VERSION bumps), ft-e1emx (tail-padding conformance harness — proven landing pad for this design)

## 2026-07-31 implementation erratum

The original proposal correctly introduced a two-scalar compatibility
window, but its positional-field evolution proof was wrong.

- Bytes appended **after a complete frame** are outside that frame's declared
  length. Their being ignored proves only that `Pdu::decode` can read one frame
  from a larger stream buffer.
- A new field serialized **inside** the frame changes the positional schema.
  An old decoder can ignore the new payload suffix, but a new derived decoder
  reading an old payload still requests the new field and fails at EOF.
  `#[serde(default)]` does not turn that I/O error into a missing field.
- Therefore a new PDU identifier is the default additive evolution mechanism.
  Reusing an identifier across schemas is allowed only with an explicit,
  bounded dual-schema decoder and canonical old/current frames that prove both
  directions and corruption rejection.

Strict-remote job `j-29953507796713785` supplied the decisive negative
evidence while testing render protocol v2. Codec v50 consequently retains
render-v1 schemas at IDs 79/80 and assigns v2 IDs 84/85. PDU 27 has the
explicit dual-schema decoder needed for its bootstrap role. This erratum
supersedes every contrary tail-field claim in the historical design discussion
below.

## Current source boundary (2026-09-04)

The window is implemented in `frankenterm/codec/src/lib.rs` and the native
client handshake. Read `CODEC_VERSION` and `CODEC_VERSION_MIN_SUPPORTED`
there for current values. The v46 code and strict-equality analysis below
are the original proposal baseline, not the running protocol. Negotiated
wire compatibility does not prove lossless live mux/server replacement.

## 1. Original invariant (before the window implementation)

`frankenterm/codec/src/lib.rs:650`:

```rust
pub const CODEC_VERSION: usize = 46;
```

The codec exposes a single global scalar version. Combined with two
load-bearing facts:

1. **varbincode is a positional binary format.** Per `MEMORY.md`'s
   `varbincode-skip-serializing-if-bug` and the comments at
   `codec/src/lib.rs:1204-1228`, any field add/remove shifts all
   subsequent field offsets. `skip_serializing_if = "Option::is_none"`
   on an `Option<T>` field is a known footgun because it elides the tag
   byte and corrupts every downstream field's offset.
2. **The handshake is single-valued.** `GetCodecVersion` /
   `GetCodecVersionResponse` (PDUs 26/27) returns one number; the
   client compares with `==` and aborts on mismatch.

The combination produces a hard invariant: **every PDU change requires
an atomic client/server redeploy.** Long-lived TLS mux sessions
(see ft-nyvyl) cannot rolling-upgrade across a CODEC_VERSION bump
because:

- A client at v46 talking to a server at v47 sees a different field
  layout for any modified PDU and either decodes garbage or hits a
  `failed to fill whole buffer` error.
- The "distributed" feature is on by default (see ft-kuxho F1), so this
  invariant is in effect for production deployments — not just a
  development concern.
- Operators cannot drain a session graceful-handoff style; the only
  protocol-correct response to a version skew is to disconnect.

## 2. Proposal — `CODEC_VERSION_MIN_SUPPORTED` window

Add a second scalar:

```rust
/// Highest codec version this build emits / accepts as canonical.
pub const CODEC_VERSION: usize = 46;

/// Lowest codec version this build can decode wire frames from.
/// A peer announcing a version `v` such that
/// `CODEC_VERSION_MIN_SUPPORTED <= v <= CODEC_VERSION` is interop-safe;
/// anything outside that window is a hard incompatibility.
pub const CODEC_VERSION_MIN_SUPPORTED: usize = 46;
```

`CODEC_VERSION_MIN_SUPPORTED` starts equal to `CODEC_VERSION` (no window
yet). Future commits that introduce **strictly additive** PDU changes—normally
new PDU identifiers—bump `CODEC_VERSION` *without* bumping
`CODEC_VERSION_MIN_SUPPORTED`, opening a backward-compat window.

The handshake gate becomes:

```rust
fn check_compat(local: usize, remote: usize) -> CompatDecision {
    let local_min = CODEC_VERSION_MIN_SUPPORTED;
    let local_max = CODEC_VERSION;
    if remote >= local_min && remote <= local_max {
        if remote < local_max {
            warn!(local=local_max, remote=remote, "peer is older but inside compat window");
        }
        CompatDecision::Compatible { agreed: remote.min(local_max) }
    } else {
        CompatDecision::Incompatible { local_min, local_max, remote }
    }
}
```

Both sides agree on `min(remote, local_max)` and serialize at that
version going forward for the session. (See §4 for the constraint that
makes this safe in the additive case.)

### Why a two-scalar window beats per-PDU versioning

The parent epic ft-kuxho mentions per-PDU version fields as one of the
options. We prefer the two-scalar approach because:

- **No wire format change.** Per-PDU version fields would require a
  positional shift on every PDU — exactly the change varbincode forbids
  without a CODEC_VERSION bump (chicken-and-egg).
- **Bounded reasoning.** "Is `v45` interop-safe?" becomes a single
  inequality check, not a combinatorial sweep across N PDU types.
- **Cheap to maintain.** Two `pub const`s and one handshake helper, plus
  explicit capability gates for newly assigned PDU identifiers.
- **Honest about scope.** Removed/modified fields still require an
  atomic redeploy. The window only buys headroom for the *additive*
  cases — which empirically dominate. A scan of the last 20 codec
  changes (`git log --oneline frankenterm/codec/src/lib.rs`) shows
  ~80% were classified as additive. Under the erratum, a new PDU identifier
  is additive by construction; a new field is additive only when explicit
  dual-schema decoding or negotiated directional emission proves it.

## 3. Policy — when does `CODEC_VERSION_MIN_SUPPORTED` advance?

**Bumping `CODEC_VERSION_MIN_SUPPORTED` is a breaking change.** It says
"clients at versions `[old_min, new_min)` will be rejected starting at
this release". The bar is high.

Required artifacts when `MIN` advances:

1. **A row in `CHANGELOG.md`** under a `Codec compatibility` section,
   citing the PDU change that forced the bump and the previous minimum.
   Example: `min 46 → 50; reason: removed UnusedClipboardField from SetClipboard (PDU 20).`
2. **A handshake-time `tracing::warn!`** for the *full* release cycle
   *before* the bump. Operators get one release of advance notice that
   their old clients are about to be incompatible.
3. **A `WARN`-level entry** in the GetCodecVersionResponse path that
   logs `local_min`, `local_max`, `remote` triples whenever
   `remote < local_min` is rejected, with stable structured fields so
   alerting can trigger on rollout.
4. **CI guard alignment with ft-8smkj.** The track-A CI guard rejects
   silent `CODEC_VERSION` bumps; extend it to also reject silent
   `CODEC_VERSION_MIN_SUPPORTED` bumps. Both must be paired with a
   release-note row.

Concrete rule of thumb:

| change kind                        | bump `CODEC_VERSION` | bump `MIN` |
| ---------------------------------- | -------------------- | ---------- |
| add new PDU variant (new ident)    | yes                  | no         |
| add a field under a reused positional PDU identifier | yes | yes, unless an explicit dual-schema decoder and real old/current frames prove the retained window |
| remove a PDU variant               | yes                  | yes        |
| remove a field                     | yes                  | yes        |
| change a field's type              | yes                  | yes        |
| reorder fields                     | yes                  | yes        |
| add a field in the *middle* of a struct | yes             | yes        |

Field position still matters—middle insertion also corrupts older decoders—but
tail position alone proves only the new-sender/old-decoder direction.
`GetCodecVersionResponse` now uses an explicit legacy/current decoder because
the reverse direction is required during bootstrap.

## 4. Interaction with the ft-e1emx tail-padding harness

The ft-e1emx conformance harness (commit 211f3b8a) already verifies the
property this proposal depends on:

- **`tail-padded decode robustness`** — extra bytes after the canonical
  frame must not corrupt that decoded PDU. This models another frame already
  buffered by the stream reader; it does not model bytes added inside the
  frame's declared payload length.
- **`Option<T>` tag-byte regression guard** — skips `skip_serializing_if`
  reintroductions, which would silently misalign older decoders even
  for nominally-additive changes.

What ft-e1emx does *not* cover (and what this proposal needs as
follow-up):

- **Actual old/current schemas** — compatibility proof must serialize each
  complete schema inside its real frame boundary. A reused identifier needs
  an explicit dual-schema decoder; otherwise the change uses a new identifier.
- **Cross-version handshake matrix** — `(local_min, local_max, remote)`
  triples and the resulting CompatDecision. This is integration-level,
  not codec-unit-level.

Both gaps become child beads (§6).

## 5. Three-step rollout plan

| step | scope | success signal |
| ---- | ----- | -------------- |
| 1    | Land the `CODEC_VERSION_MIN_SUPPORTED` const and the `check_compat` helper. Initial value equals `CODEC_VERSION`. No behavior change on the wire. Pre-existing handshake call sites migrate from `==` to the helper. | All existing PDU roundtrip tests pass. New unit test exercises `check_compat` against `(min, max, remote ∈ {min-1, min, max, max+1})`. |
| 2    | Extend the conformance harness with actual previous/current payload structs framed under their assigned identifiers. | New identifiers remain unknown-but-skippable to old peers; any reused identifier decodes canonical old/current payloads and rejects corruption. |
| 3    | First production-driven `CODEC_VERSION` bump that exercises the window through a new PDU identifier and negotiated emission gate. | Rolling deploy completes with zero hard handshake failures. Logs show the warn-line for the duration of the mixed window and stop firing once all clients are current. |

## 6. Open questions

1. **What's the GetCodecVersionResponse wire format change cost?** Resolved:
   PDU 27 keeps its identifier and uses an explicit bounded decoder for the
   canonical four-field legacy and five-field current schemas. Legacy decode
   produces the conservative `min_supported = 0` sentinel.
2. **Does this need a per-PDU "frame version" too?** Not globally. New PDU
   identifiers plus negotiated capabilities are the default. A reused
   identifier owns its explicit schema dispatcher.
3. **What about TLS-tunnelled long-lived sessions (ft-nyvyl)?** This
   proposal doesn't fix in-flight sessions — it fixes the *connect-time*
   gate. A v46 session that started before a server upgrade to v47 stays
   on v46 (because the negotiated version is sticky); the server only
   speaks v46 to that session. This is the correct behavior for
   additive changes.

## 7. Acceptance criteria for this proposal

- [ ] Reviewer signs off on the two-scalar approach over per-PDU
      versioning.
- [ ] Three child beads filed under `ft-kuxho` (one per rollout step).
- [ ] Open questions §6.1 (GetCodecVersionResponse field ordering)
      resolved before step-1 implementation lands.

## 8. Child beads to file

The next commit on this thread should be the bead-creation pass.
Proposed beads:

1. `ft-kuxho.B.1` — implement `CODEC_VERSION_MIN_SUPPORTED` const +
   `check_compat` helper + handshake-site migration. Step 1 of the
   rollout. Includes the unit test against
   `(min, max, remote)` triples.
2. `ft-kuxho.B.2` — historical harness bead. Its synthetic
   bytes-after-frame fixture is retained only as a framing guard; real schema
   fixtures supersede its compatibility claim.
3. `ft-kuxho.B.3` — historical `GetCodecVersionResponse.min_supported` bead.
   Its derived-default assumption is superseded by PDU 27's explicit
   legacy/current decoder.
