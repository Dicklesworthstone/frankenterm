# Codec versions

Single source of truth for `CODEC_VERSION` history.

The CI guard `scripts/check_codec_version_release_notes.sh` (track A
of ft-kuxho, ft-8smkj) reads the current `CODEC_VERSION` constant from
`frankenterm/codec/src/lib.rs` and **fails CI** if no row in this
file documents that version. Bumping `CODEC_VERSION` without adding a
row here is a silent protocol change and is rejected at CI time.

When you bump `CODEC_VERSION` in `frankenterm/codec/src/lib.rs`, add a
row at the top of the table below in the same commit. Each row records:

- **version** — the new `CODEC_VERSION` value (matches the constant exactly)
- **date** — `YYYY-MM-DD` of the commit that bumped the version
- **kind** — `additive` (rolling upgrade safe per ft-kuxho/B) or `breaking`
  (atomic redeploy required and must advance `CODEC_VERSION_MIN_SUPPORTED`)
- **change** — short summary; reference the PDU id(s) and the bead/commit

`CODEC_VERSION_MIN_SUPPORTED` is live. Peers may negotiate any version in the
inclusive `MIN_SUPPORTED..=CODEC_VERSION` window. A new, separately negotiated
PDU may advance only `CODEC_VERSION`; a breaking change must advance the
minimum in the same commit. Adding a field to an existing varbincode PDU is not
additive because old payloads end at EOF and do not synthesize a missing field.

Every assigned identifier in the codec's `pdu!` declaration must also declare
its minimum dialect, exact producer/serial-role authorities, and topology-
capability use. The generated `PduWireSpec` registry is the source of truth for
transport admission; variant names such as `Response` are not direction
authority. Unknown identifiers and the historical gaps 5-7, 15-19, and 21 have
no specification and must fail closed after handshake.

`GetCodecVersionResponse` now carries `min_supported`. Its dual-schema decoder
maps a canonical legacy response to the sentinel zero; handshake code must
conservatively clamp that sentinel to the peer's `codec_vers`. `check_compat`
rejects impossible `min_supported > codec_vers` windows rather than repairing
peer input. Computing an overlap does not by itself activate rolling interop:
each connection generation must retain the agreed dialect and gate every PDU
before serial allocation and the first write, while inbound frames are checked
against the same dialect, producer/role, and established capabilities.

**Operator note:** rows marked additive are rolling-upgrade safe only when the
new PDU or behavior is guarded by the negotiated version/capability. Rows
marked breaking require the maintenance-window procedure in
[`docs/codec-atomic-redeploy.md`](codec-atomic-redeploy.md).

## History

| version | date       | kind     | change |
| ------- | ---------- | -------- | ------ |
| 56      | 2026-08-07 | breaking | adds an exact client-generated `input_serial` dispatch identity to positional varbincode PDU13 `SendPaste`, allowing paste-only speculative echo to receive the same causal terminal-sequence fence as key input. The minimum supported dialect advances atomically to 56 so a v55 peer can neither omit nor misdecode the new field. See the ft-interactive-systems-performance-4tenz mux/sessionhandler fresh-eyes tranche. |
| 55      | 2026-08-07 | breaking | changes PDU23/PDU47 image hydration authority from stable `ImageData` object identity to the current mutable pixel revision; introduces bounded image-byte serde for encoded, lease-backed, RGBA, and animated frame payloads; and applies a schema-specific 65 MiB decompressed admission ceiling to PDU47. Old peers would otherwise silently request or cache the wrong Kitty revision, so the minimum supported dialect advances atomically to 55 and mixed v54/v55 pairs are rejected. See the ft-interactive-systems-performance-4tenz.6 image-pipeline fresh-eyes tranche. |
| 54      | 2026-08-04 | additive | replaces the never-advertised recursive PDU87 pane-tree bytes with a canonical bounded flat arena: per-tab descriptors, contiguous preorder node indices, sorted window titles, and explicit depth/node/leaf ceilings. PDU86-90 admission advances atomically to dialect 54; older dialects remain usable for their other registered PDUs and cannot negotiate ordered-window authority. The minimum supported dialect remains 46. See ft-interactive-swarm-product-convergence-7xqz4.8.10.2.2.2. |
| 53      | 2026-08-03 | additive | replaces the never-advertised ordered-window PDU86/87 bootstrap bytes with a required nonzero client `DomainBindingId` and exact PDU87 echo, raises PDU86-90 admission to dialect 53, and establishes immutable stream/session/binding/capability authority before any reorder request. Dialects 51/52 remain usable for their other registered PDUs but cannot negotiate ordered-window authority. The minimum supported dialect remains 46. See ft-interactive-swarm-product-convergence-7xqz4.8.10.2.2.1. |
| 52      | 2026-08-03 | additive | reserves exact render-delivery request/response PDUs 91/92 with explicit connection, pane-generation, delivery-generation, request, baseline, and settlement authority; bounded exact deltas; immutable force-full manifests/chunks; receiver and aggregate caps; and a codec-owned fixed-width persisted-text projection plus domain-separated digests. `EXACT_RENDER_DELIVERY_V1` remains deliberately unadvertised and both endpoints remain inactive until the server retention, client pre-body admission/application, and convergence-oracle children are complete. The minimum supported dialect remains 46. See ft-interactive-systems-performance-4tenz.5.5.3.5.8.2.1. |
| 51      | 2026-08-03 | additive | reserves negotiated ordered-window snapshot, reorder-CAS, and causal order-event PDUs 86-90 with stable `u64` wire identities, exact bounded decoding, and capability bits that remain deliberately unadvertised until server/client authority is implemented. The codec window continues to include version 50; live rolling interop additionally requires the connection handshake to retain and enforce the agreed dialect. See ft-interactive-swarm-product-convergence-7xqz4.8.10.2. |
| 50      | 2026-07-30 | additive | adds authoritative render-application-v2 PDUs 84/85 while permanently retaining v1 schemas at 79/80, preventing numeric generation aliasing across reconnect, restart, and route failover. Also gives PDU 27 an explicit canonical legacy/current decoder after real old-frame testing disproved the prior `serde(default)`-at-EOF assumption. See ft-interactive-systems-performance-4tenz.5.5.4. |
| 49      | 2026-07-30 | additive | adds negotiated coherent `ListPanesCoherent` / `ListPanesCoherentResponse` and stamped `TopologyEvent` PDUs (81/82/83), with exact stream/session identity and typed contention, exhaustion, and unsupported-capability outcomes for ft-interactive-systems-performance-4tenz.5.5.14.1.2.3.2. |
| 48      | 2026-07-30 | additive | adds fail-closed `RenderApplicationUpdate` / `RenderApplicationResult` PDUs (79/80) with exact generation, scheduler, ledger, base/result-state, complete atomic surface components, hard resource bounds, original-deadline-bounded retry/resync, and typed ACK/NACK authority for ft-interactive-systems-performance-4tenz.5.5.5. |
| 47      | 2026-06-07 | additive | adds `GetSemanticZones` / `GetSemanticZonesResponse` PDUs (77/78) carrying live SemanticZone coordinates, zone text, and retained OSC 133 exit status for ft-7h5da.2.1 robot DOM queries. PDU 75/76 were historically added by `5bb3372e26` while the constant still reported v46; the exhaustive registry therefore assigns them a conservative minimum dialect of 47, the first unambiguous version containing them. |
| 46      | 2026-02-10 | initial  | starting value at fork import from wezterm @ `05343b387085842b434d267f91b6b0ec157e4331`. See `frankenterm/PROVENANCE.md`. |
