# Proposal: `CODEC_VERSION_MIN_SUPPORTED` window for additive PDU rolling upgrade

**Bead:** [ft-k6jco](../../.beads/issues.jsonl) (track B of ft-kuxho)
**Status:** draft
**Related:** ft-kuxho (parent epic), ft-8smkj (track A: CI guard against silent CODEC_VERSION bumps), ft-e1emx (tail-padding conformance harness — proven landing pad for this design)

## 1. Current invariant

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
yet). Future commits that introduce **strictly additive** PDU changes
bump `CODEC_VERSION` *without* bumping `CODEC_VERSION_MIN_SUPPORTED`,
opening a backward-compat window.

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
- **Cheap to maintain.** Two `pub const`s and one handshake helper
  versus a per-PDU registry.
- **Honest about scope.** Removed/modified fields still require an
  atomic redeploy. The window only buys headroom for the *additive*
  cases — which empirically dominate. A scan of the last 20 codec
  changes (`git log --oneline frankenterm/codec/src/lib.rs`) shows
  ~80% are additive (new PDU variant, new field with a default). The
  window addresses the common case without pretending to address
  removal/rename/typechange.

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
| add field with `#[serde(default)]` at the **end** of a struct | yes                  | no         |
| remove a PDU variant               | yes                  | yes        |
| remove a field                     | yes                  | yes        |
| change a field's type              | yes                  | yes        |
| reorder fields                     | yes                  | yes        |
| add a field in the *middle* of a struct | yes             | yes        |

The rule "additive only at the end" is the same constraint that makes
the existing `#[serde(default)]` workaround sound (see
`GetCodecVersionResponse::config_file_path` at lib.rs:840 — added at
the end of the struct, default `None`, decodes cleanly from older
peers).

## 4. Interaction with the ft-e1emx tail-padding harness

The ft-e1emx conformance harness (commit 211f3b8a) already verifies the
property this proposal depends on:

- **`tail-padded decode robustness`** — extra bytes after the canonical
  frame must not corrupt the decoded PDU. This is exactly what happens
  during a rolling upgrade in the additive case: the new build emits
  `N+k` bytes, the old build decodes the first `N` and ignores the
  trailing `k` because the length prefix tells it to stop.
- **`Option<T>` tag-byte regression guard** — skips `skip_serializing_if`
  reintroductions, which would silently misalign older decoders even
  for nominally-additive changes.

What ft-e1emx does *not* cover (and what this proposal needs as
follow-up):

- **End-of-struct addition with default** — the existing harness exercises
  encode/decode roundtrips at one version. We need a parameterized
  harness that takes a "decode-as-version" parameter and verifies that a
  v47 frame containing an end-of-struct addition decodes cleanly as v46
  (the end-bytes simply land in the consumed-but-ignored tail).
- **Cross-version handshake matrix** — `(local_min, local_max, remote)`
  triples and the resulting CompatDecision. This is integration-level,
  not codec-unit-level.

Both gaps become child beads (§6).

## 5. Three-step rollout plan

| step | scope | success signal |
| ---- | ----- | -------------- |
| 1    | Land the `CODEC_VERSION_MIN_SUPPORTED` const and the `check_compat` helper. Initial value equals `CODEC_VERSION`. No behavior change on the wire. Pre-existing handshake call sites migrate from `==` to the helper. | All existing PDU roundtrip tests pass. New unit test exercises `check_compat` against `(min, max, remote ∈ {min-1, min, max, max+1})`. |
| 2    | Extend the ft-e1emx harness with a `decode_as_version` parameter. Add a fixture that encodes a synthetic "future" PDU variant (extra trailing bytes representing a v+1 end-of-struct field) and asserts the v decoder consumes only the canonical prefix. | The harness fails on a deliberate "field added in the middle" mutation; passes on "field added at end with default". |
| 3    | First production-driven `CODEC_VERSION` bump that *exercises* the window. Add a single end-of-struct field to one PDU; bump `CODEC_VERSION` from 46 → 47; leave `MIN` at 46. Roll the server first, then clients incrementally. The handshake warn-line must fire and be observed in production logs at v46/v47 mixed steady state. | Rolling deploy completes with zero hard handshake failures. Logs show the warn-line for the duration of the mixed window and stop firing once all clients are at v47. |

## 6. Open questions

1. **What's the GetCodecVersionResponse wire format change cost?** The
   existing PDU has `codec_vers: usize`. We may want to add `min_supported`
   to it so peers can negotiate symmetrically. Adding a field is itself a
   PDU change — the bootstrap problem. Solvable by piggybacking on the
   existing `#[serde(default)]` at-end pattern, but the order matters
   (this becomes step 1's first commit).
2. **Does this need a per-PDU "frame version" too?** No, per §2: per-PDU
   versioning costs a positional shift on every PDU and the two-scalar
   window covers the empirically-dominant additive case. We should
   revisit if a real-world removal is needed and the atomic-redeploy
   cost is unacceptable for that specific PDU.
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
2. `ft-kuxho.B.2` — extend the ft-e1emx conformance harness with a
   `decode_as_version` parameter and the synthetic "future PDU" fixture
   that proves the additive end-of-struct case roundtrips across the
   compat window. Step 2.
3. `ft-kuxho.B.3` — add a `GetCodecVersionResponse.min_supported` field
   (end-of-struct, `#[serde(default)]`, defaults to the same value as
   `codec_vers` for backward compat). Resolves open question §6.1 and
   prerequisites the symmetric handshake. Order-sensitive; lands before
   step 3.
