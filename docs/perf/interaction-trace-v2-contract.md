# Interaction trace v2 contract

**Bead:** `ft-interactive-systems-performance-4tenz.2.1`

**Wire schema:** `ft.interaction-trace.v2`

**Rust authority:** `frankenterm_core::interaction_trace_v2`

**JSON schema:** `docs/perf/interaction-trace-v2.schema.json`

This contract is the content-free identity and evidence boundary for the
keypress and resize/zoom paths in the sustained mux/renderer campaign. It
freezes what producers must emit and what validators may infer. It does not
wire those producers, replace the bounded flight recorder, or qualify any
hardware target.

## Identity

`run_id` is an opaque 128-bit process-epoch nonce. It changes whenever the
producing process restarts. It is not a timestamp, PID, hostname, build ID, key
hash, or pane-content hash. `trace_id` is `(run_id, sequence)`, where sequence
is strictly increasing within one run. Zero and `u64::MAX` are reserved. The
allocator fail-stops after `u64::MAX - 1`; it never wraps.

Every event repeats its exact schema and trace identity so a standalone JSONL
row is attributable. `event_ordinal` starts at zero and is contiguous within a
trace. `span_id` is non-zero and unique; `parent_span_id`, when present, must
name an earlier span. A run envelope rejects mixed run IDs, duplicate trace
sequences, and regressing trace sequences. Reusing sequence one after a process
restart is valid only under a new run ID.

Producer identity is the opaque tuple `(host_id, process_id,
process_generation, thread_id, connection_generation)`. The connection
generation is mandatory on mux/transport stages. Each event also carries exact
numeric window, tab, and pane IDs. Human-readable titles, commands, current
directories, key data, and pane data are forbidden.

The window/tab/pane tuple is invariant for the lifetime of one trace. A
producer that follows an operation onto a different topology owner must start a
new trace or record an explicit correlation in a higher-level artifact; it may
not stitch the two owners into one apparently stable interaction.

## Closed stage inventories

The contract reuses the catalog-owned closed enums rather than creating a
second spelling:

- keypress: `RendererKeypressTraceStage::ALL`, K0 through K13;
- resize/zoom: `RendererResizeTraceStage::ALL`, R0 through R25.

The canonical meanings remain frozen in
`docs/perf/mux-long-session-performance-campaign.md` section 5 and
`docs/design/renderer-scenario-contract.md`. Structural validation accepts a
strict prefix so an interrupted trace remains diagnostic. Qualification
requires the entire path in exact order, with no duplicate stage, ordinal gap,
or recorder loss. “Viewport ready” and “display presented” remain different
stages.

Each stage also carries an explicit outcome: `performed`, `no_op`,
`not_applicable`, `superseded`, `cancelled`, or `failed`. Conditional resize
work therefore retains its closed stage slot without inventing execution.
No-op, inapplicable, and superseded slots have zero duration. Until the
scenario catalog freezes a stage-specific optionality map, only `performed`
stages qualify; every other outcome remains honest diagnostic evidence.

## Clocks and duration arithmetic

Every timestamp contains:

- an opaque `clock_domain = (host_id, process_generation, clock_id)`;
- monotonic nanoseconds used for duration arithmetic; and
- optional Unix wall time retained only as metadata.

The clock host/process labels must match the event producer. A stage interval
fails on clock-domain mismatch or monotonic regression. The public subtraction
helper accepts only the exact same clock-domain tuple. In particular:

- never subtract Mac and remote-host timestamps;
- never subtract two process-local clocks merely because they are on the same
  host;
- never use wall time for a duration;
- measure a wire RTT from two markers on one clock; and
- admit a cross-host interval only through the later calibration/synchronization
  authority, with its retained error bound. This v2 contract intentionally has
  no method that performs that subtraction.

Within one trace, stage start timestamps sharing the exact same clock domain
must also be nondecreasing by event ordinal. Overlapping stage intervals remain
legal; the validator intentionally compares start to prior start, not start to
prior completion. This catches a regressing producer clock without falsely
rejecting concurrent resize/render work.

## Causality and claim boundaries

Each receipt carries one closed correlation class:

| Class | Required authority | Maximum standalone claim |
|---|---|---|
| `exact_protocol` | Non-zero protocol token and protocol generation | Causal software path at the named protocol boundary |
| `exact_echo_fixture` | Non-zero fixture token and expected terminal generation | Exact path for that controlled fixture only |
| `causal_candidate` | Non-zero candidate window | Diagnostic candidate; not exact attribution |
| `uncorrelated` | None | Aggregate-only observation |

`InputSerial` is not renamed or treated as a trace ID. It can be mentioned by a
producer only at the protocol boundary it actually proves; it is not PTY echo,
parser application, presentation, or photon authority. A complete trace takes
the weakest claim boundary carried by any event.

The `observation_boundary` vocabulary is `internal_state`, `software_present`,
`metal_drawable`, `display_presented`, and `photon`. Only K13/R25 may claim
display completion. `photon` additionally requires non-zero physical detector
and calibration IDs. GPU submission or a drawable present request cannot
impersonate display completion or photons.

## Required counters and generations

Every event has explicit queue depth and oldest age, generic work/byte/row
counts, allocations/copies, RPCs, deltas, dirty rows, full viewport clones,
cursor-row duplicates, paints, frames, and cumulative dropped/overwritten
event counts. Zero means observed zero unless the corresponding boolean in the
fixed-shape `counter_availability` object is `true`. Unavailable fields must
carry the zero placeholder, unknown fields fail deserialization, and any
unavailable counter makes the trace diagnostic rather than qualifying. The
fixed shape bounds deserialization and preserves the distinction between a
measured zero and missing authority without putting free-form text into the
trace.

Terminal, snapshot, and frame generations are explicit optional values. Zero
is invalid. The validator requires terminal generation at K7, snapshot
generation at K8 and R13, and frame generation at K13/R25. These anchors keep
an identically numbered but stale terminal/snapshot/frame from satisfying the
trace.

`INTERACTION_TRACE_V2_METRIC_MAP` is the executable schema lint. It contains
exactly one producer and maximum claim boundary for every metric. The linter
rejects omissions, duplicates, and physical-photon authority assigned to
anything except the physical detector.

## Privacy review

The DTOs use only closed enums, numeric counters, numeric opaque identities,
the exact schema string, and optional wall-clock metadata. All wire structs
deny unknown fields. There is no string slot for raw keys, composed text, pane
contents, titles, commands, paths, hostnames, or user labels. The negative
privacy overlay plants `raw_key` and `pane_text` onto the otherwise-valid good
fixture; schema validity returns only when those two fields are removed.
Nested decode negatives and a serialization test independently check that the
forbidden vocabulary cannot enter or leave the typed DTO.

Opaque IDs must be generated independently of sensitive content. Hashing raw
input into an ID is not permitted: a low-entropy key or title remains
dictionary-recoverable even if the original bytes are absent.

## Fixture and validation matrix

The retained corpus is `fixtures/perf/interaction-trace-v2/`:

- `good-keypress-v2.json`: complete lossless K0-K13 trace with a local/remote
  clock split and an actual display-presented K13 boundary;
- `old-keypress-v1.json`: old-version shape retained to prove fail-closed
  version rejection; and
- `bad-raw-content-v2.json`: forbidden raw-content overlay applied to the valid
  v2 fixture so the planted fields are the only rejection cause.

Inline tests additionally cover sequence exhaustion, duplicate/regressing IDs,
process restart, missing stages, topology changes, conditional stage outcomes,
explicit counter unavailability, cross-clock arithmetic, clock regression,
round trip, sampling loss, metric-map completeness, and submit-versus-photon
authority. A Draft 2020-12 validator compiles the committed JSON schema,
accepts the committed keypress fixture plus typed keypress and resize
roundtrips, rejects the retained old version, and rejects the good fixture only
after the raw-content overlay is applied.

## Non-claims and downstream work

This change establishes a schema and semantic validator. It does not prove
that K0-K13 or R0-R25 producers are wired, that the recorder is bounded or
low-overhead, that any live trace is complete, that any clock registry is
externally bound, or that any M4/M5/Threadripper latency target is met. Those
remain the responsibilities of the production recorder, producer-wiring,
isolated lab, and target-qualification beads. No live FrankenTerm process or
operator session is needed or admissible for this contract-only proof.
