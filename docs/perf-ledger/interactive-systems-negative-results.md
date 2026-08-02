# Interactive Systems Performance Negative-Evidence Ledger

**Campaign:** `ft-interactive-systems-performance-4tenz`
**Contract:** `docs/perf/mux-long-session-performance-campaign.md`
**Opened:** 2026-07-27

This ledger prevents the mux/renderer/long-session campaign from repeating
wrong-pipeline measurements, retired micro-optimizations, and attractive but
falsified explanations. The detailed round 4-9 ledgers remain authoritative
for their experiments; this file is the cross-round index for the real
interactive-systems campaign.

## Rules

Every rejected candidate records:

- source revision and candidate revision;
- exact workload, host/topology, profile, configuration, and transport;
- focused and broad commands/artifacts from the same measurement window;
- sample count, distribution, confidence/noise, and `cv_pct`;
- semantic/byte/state and visual-equivalence results;
- the measured outcome and decision; and
- exactly one primary retry predicate using one of the eight repository-owned
  canonical forms in
  [`round4-negative-results.md`](round4-negative-results.md#the-8-retry-condition-forms-every-entry-closes-with-exactly-one-no-anti-vocabulary).

A candidate with incomplete identity or noisy evidence remains an open
experiment. It is not converted into a flattering keep or a durable rejection.

## Closed evidence-boundary entries

### IS-N001 — The RQ-S1 dirty-row loop is not live 200-pane resize proof

- **Classification:** wrong evidence pipeline for the live claim
- **Source revision/artifact:** retained
  `docs/attestations/tui/resize-fps-rq-s1-20260606T2351Z.jsonl`;
  `crates/frankenterm-core/benches/resize_storm.rs`
- **Observed result:** the retained artifact contains 20 rows and reports a
  maximum p99 frame time of `106us`.
- **Code-path result:** the timed loop iterates pane IDs and dirty-row tuples.
  It does not enter `TermWindow`, mux resize, panes, scrollback reflow, shaping,
  WebGPU, or display presentation. A separate assertion resizes one terminal
  with 512 lines.
- **Decision:** retain the benchmark as a core dirty-work regression substrate.
  Reject its use as evidence of presented GUI FPS or resize appearance.
- **Primary retry condition:**
  > Do not retry from a cold read; use the real TermWindow/CAMetalLayer 20/50/200-pane live-resize rig in `ft-interactive-systems-performance-4tenz.3` instead.

### IS-N002 — The current RQ-S2 bench is not physical input-to-photon

- **Classification:** wrong evidence pipeline for the live claim
- **Source revision/artifacts:** 2026-07-16 retained macOS run;
  `docs/attestations/tui/input-to-photon-rq-s2-macos-20260716T140705Z.jsonl`;
  Criterion estimates beside it
- **Observed result:** cold sample `351.65ms`; Criterion steady-state mean
  `33.30ms`, median `33.54ms`, standard deviation `2.72ms`; target `<16ms`;
  `within_target=false`.
- **Code-path result:** the bench renders a synthetic headless fixture, creates
  GPU resources, forces GPU-to-CPU readback, and uses four fixed pre-render
  stage durations. It does not receive an `NSEvent`, traverse the client/server
  mux and PTY, invalidate a production window, or measure display scan-out.
- **Decision:** preserve the result as an honest native Metal proxy and its
  over-target status. Its confounders point in both directions: host load and
  GPU-to-CPU readback add work, while NSEvent, mux/PTY, production invalidation,
  scan-out, and photons are omitted. It is not an upper or lower bound. Reject
  any physical input-to-photon claim from this substrate.
- **Evidence-authority hardening:** new rows use
  `ft.renderer.input-to-photon.v2`, carry the closed `proxy_only` claim scope,
  retain only a closed input class and encoded byte count, and cannot serialize
  raw key content. Mixed schema/claim/workload/platform/GPU-adapter identity,
  duplicate sample IDs, incomplete proxy metadata, or any invalid member
  invalidate the entire summary. Proxy percentiles remain available for
  regression diagnosis, but `within_target` is always absent. The retained
  2026-07-16 v1 row remains immutable historical negative evidence; it is not
  reinterpreted as v2.
- **Primary retry condition:**
  > Do not retry from a cold read; use the correlated NSEvent-to-LAN-to-PTY-to-CAMetalDrawable trace and physical/display timing in `ft-interactive-systems-performance-4tenz.2` instead.

### IS-N003 — RQ-S6 does not measure a key under a live 50-pane burst

- **Classification:** wrong evidence pipeline for the live claim
- **Source artifact:** `crates/frankenterm-core/benches/heavy_burst.rs`
- **Code-path result:** the bench drives `SustainedBurstHarness` deferred
  operations and asserts a modeled per-frame p95 budget. It does not send a key
  through the connection FIFO, socket, server dispatch, mux main thread,
  terminal/PTY, or renderer.
- **Decision:** retain it as a deferred-operation regression substrate. Reject
  it as live heavy-burst input evidence.
- **Primary retry condition:**
  > Do not retry from a cold read; use the 50-pane 1MB/s production workload with the stage-correlated keypress trace in `ft-interactive-systems-performance-4tenz.2` instead.

### IS-N004 — A short core simulation is not an aged-session soak

- **Classification:** unsupported scope upgrade
- **Source artifact:** `crates/frankenterm-core/tests/e2e_swarm_stress_core.rs`
- **Code-path result:** the test uses simulated `TieredScrollback` panes and
  identifies `metric_source=core_simulation`. It does not drive the GUI, mux
  transport, PTYs, actual storage/search pipeline, reconnects, or GPU.
- **Decision:** preserve the deterministic core test. Reject “50/100/200-pane
  long-haul” or resource-stability claims based solely on this run.
- **Primary retry condition:**
  > Do not retry from a cold read; use the retained 4h/24h/72h live mux, PTY, network, GUI, storage, and renderer soak in `ft-interactive-systems-performance-4tenz.4` instead.

### IS-N005 — “Use all Threadripper CPUs” is not a latency strategy

- **Classification:** architectural explanation rejected
- **Source revision inspected:** `5e23496785fa02018ad76a6534a1fb6588221e58`
- **Observed target:** `trj` is a Ryzen Threadripper PRO 5995WX with 64
  physical cores and 128 logical CPUs.
- **Code-path result:** one remote connection task, server dispatch lane, mux
  main queue, terminal mutex, and GUI main thread remain serialized. The reflow
  heuristic can select 64 workers for a 64-line batch and then reject
  parallelism because it requires eight lines per selected worker. Pane resize
  can simultaneously create many OS workers.
- **Decision:** reject indiscriminate worker-count increases and the claim that
  aggregate core count alone should remove keypress lag. Test bounded useful
  work and locality.
- **Primary retry condition:**
  > Reconsider only inside the broader topology-aware persistent resize/reflow scheduler redesign tracked as `ft-interactive-systems-performance-4tenz.7`.

### IS-N006 — Direct-LAN routing is not yet the primary lag explanation

- **Classification:** orientation measurement; primary-cause hypothesis
  rejected pending a real transport trace
- **Host/date:** local Mac to `trj`, 2026-07-27
- **Observed result:** direct `10.10.10.1` ping averaged about `0.257ms` with
  `0.861ms` maximum over 30 samples; tailnet `100.91.120.17` averaged about
  `0.994ms` with `1.559ms` maximum. The live SSH process used the tailnet
  endpoint.
- **Decision:** keep route/transport in the matrix because direct LAN is lower
  latency and jitter. Reject the roughly `0.74ms` ping delta as a sufficient
  explanation for conspicuous multi-frame sluggishness.
- **Primary retry condition:**
  > Retry only if the correlated production trace attributes a clearly-above-noise share to transport or route selection on the 50-pane burst or 200-pane resize-concurrent workload.

### IS-N007 — Generic Apple Metal SoA/vertex-bandwidth work is retired

- **Classification:** measured performance rejection
- **Source:** `docs/perf-ledger/round6-negative-results.md`
- **Observed result:** the SoA instanced-glyph experiment did not produce a
  realistic Apple Metal win; GPU readback/wait dominated while vertex work was
  below the ledger's meaningful attribution threshold.
- **Decision:** do not repeat buffer-layout or generic vertex-bandwidth changes
  from source inspection. Per-frame bind-group/buffer reuse remains a distinct
  profile-gated hypothesis.
- **Primary retry condition:**
  > Retry only if a profiler attributes a clearly-above-noise share of at least 0.5% of live frame time to vertex create, bind, or upload work on the real M4/M5 resize workload.

### IS-N008 — Persistent-rope reflow is retired as a standalone design

- **Classification:** measured algorithm rejection
- **Source:** `docs/perf/persistent-rope-evaluation.md`
- **Observed result:** the evaluated persistent-rope design was approximately
  `21x` slower on reflow than the incumbent representation.
- **Decision:** preserve the negative. The campaign can change publication,
  cancellation, batching, snapshotting, and scheduler topology without
  reviving this representation.
- **Primary retry condition:**
  > Not worth retrying as a standalone patch.

### IS-N009 — COW snapshots do not solve a sub-microsecond scrollback lock

- **Classification:** measured opportunity rejection
- **Source:** round 4-9 performance ledgers
- **Observed result:** measured lock p95 was roughly `250-333ns`, while clone
  holds were roughly `15-47us`.
- **Decision:** reject copy-on-write solely to evade that measured lock. This
  does not reject a coherent short-lock render snapshot when a live profile
  attributes millisecond-scale paint/parser contention.
- **Primary retry condition:**
  > Worth reconsidering when the campaign's real live-session terminal-lock p95 reaches 50us.

### IS-N010 — Stable GPU allocation and `device.poll` were red herrings for the progressive slowdown

- **Classification:** historical causal rejection
- **Source revision:** root-cause/fix commit
  `1d9e3b9e67cc43910c4c138d9e2ee03b7742435f`
- **Observed incident:** render CPU rose from roughly 30% toward 70% over about
  40 minutes as glyph diversity accumulated. The stable GPU allocation did not
  explain the slope.
- **Root cause/result:** atlas overflow cleared the shape cache and repeatedly
  reintroduced full-screen HarfBuzz work. Decoupling atlas-invariant shape
  information reported roughly 56% fewer HarfBuzz calls per rebuild and about
  31% lower render CPU with byte-identical re-resolution.
- **Decision:** preserve the root cause and the failed GPU-memory/
  `device.poll` theories. Do not restart generic GPU-memory work to explain a
  recurrence without new attribution.
- **Primary retry condition:**
  > Do not retry from a cold read; use the 4h/24h/72h time series for atlas generations, HarfBuzz calls, CPU, GPU residency, and cache hit rates instead.

### IS-N011 — Per-op micro-mining is not the opening move for this campaign

- **Classification:** campaign-level evidence boundary
- **Source:** round 4-9 keep and negative ledgers
- **Observed history:** custom maps/vectors, broad prefilters/caches, serial
  replacements for vectorized code, CSI/OSC lookup changes, overlap changes,
  Bloom/MPHF/fingerprint/Teddy ideas, and several other micro-levers were
  neutral, noisy, or regressions. Adaptive CDC, the scrollback prefix index,
  and dense ASCII paths already captured the proven workload-specific wins.
- **Decision:** the new campaign starts with end-to-end queue, lock, reflow,
  frame, and resource attribution.
- **Primary retry condition:**
  > Retry only if a profiler attributes a clearly-above-noise share to the exact retired operation on the real keypress, live-resize, or aged-session workload.

### IS-N012 — Detached-test executors and fabricated protocol states are not production I/O failures

- **Classification:** wrong test execution model / harness-state rejection
- **Candidate revision:** `23c25e0254932660d15c717e18bffc0d02f4d21b`
  (developed from base `d5c4a89c7ff08918365a11ba411c8bbe39287770`)
- **Observed result:** strict-remote `fmd` job `j-29953507796713727`
  reported 642/645 mux tests passing, with three explicit-detach timeouts.
  The deterministic `ScopedExecutor` fixture does not pump the detached
  supervisor tasks used by the production I/O lane. Targeted job
  `j-29953507796713728` then passed the new explicit-detach case and exposed
  a synchronization race in an older clean-exit fixture. After the fixtures
  drove the admitted sender and clean-exit barriers, canonical job
  `j-29953507796713730` passed 646/646 and job
  `j-29953507796713732` passed Clippy with warnings denied.
- **Second rejection:** after first-claim lifecycle authority was added,
  `j-29953507796713735` passed 646/649. The three failures manually installed
  `WaitingForResponse` and `in_flight` state without the lifecycle I/O lease
  that every production admission installs. They therefore exercised an
  impossible partial state rather than a response/deadline defect.
- **Canonical result:** fixtures were corrected to install the same
  generation-tagged command lease as production. Targeted job
  `j-29953507796713737` passed 11/11 guarded-response tests; full job
  `j-29953507796713740` passed 649/649; and job
  `j-29953507796713742` passed Clippy with warnings denied.
- **Decision:** preserve both failed runs as negative evidence. Test through
  admitted sender/protocol callbacks and model the complete lifecycle lease;
  do not weaken the production state machine to accommodate an executor that
  does not run its detached tasks or a fixture that fabricates an unreachable
  subset of protocol state.
- **Primary retry condition:**
  > Do not retry either rejected harness model; retry only after a fixture drives the production admission path or installs the complete generation-tagged lifecycle lease and explicit synchronization barriers.

### IS-N013 — Nested positional fields are not additive wire tails

- **Classification:** protocol-design rejection and wrong test-state rejection
- **Bead:** `ft-interactive-systems-performance-4tenz.5.5.4`
- **Baseline revision:** `5f3384cd146f8c0a4b55b0e24e5c03acef1dfab2`
- **Rejected design:** the first render-protocol-v2 draft appended
  `RenderConnectionIdentity` to the nested `RenderApplicationToken`. That
  token is followed by pane and state fields inside
  `RenderApplicationIdentity`, which is itself followed by payload fields in
  PDUs 79 and 80. In varbincode's positional format, the change was therefore
  a middle insertion, not an additive tail, and could misalign every following
  field across versions.
- **Intermediate correction:** connection authority was moved to the literal
  final field of each top-level render PDU. That removed the middle insertion,
  but strict-remote job `j-29953507796713785` subsequently proved that a newer
  varbincode schema still cannot synthesize the absent field at legacy EOF.
  See IS-N014 for the final distinct-PDU correction.
- **Superseded proof:** strict-remote `fmd` job
  `j-29953507796713783` passed 163/163 codec tests and then failed the new
  client lifecycle test because the fixture captured a ready-only RPC scope
  before successor readiness publication. Production establishes render
  identity during coherent bootstrap, so the corrected test uses the exact
  bootstrap scope, then separately verifies preservation into a ready scope.
  The failed assertion does not justify weakening readiness fencing.
- **Decision:** reject both the nested-field layout and ready-only pre-ready
  fixture. Preserve stage-accurate scope authority; do not mistake literal tail
  placement for bidirectional positional-schema compatibility.
- **Primary retry condition:**
  > Do not add authority inside a nested positional wire struct or model bootstrap with a ready-only scope; use distinct PDU identifiers or an explicit dual-schema decoder with real legacy/current frames, plus the exact scope for each lifecycle stage.

### IS-N014 — `serde(default)` does not fill a missing positional varbincode tail at EOF

- **Classification:** protocol-design and evidence-pipeline rejection
- **Bead:** `ft-interactive-systems-performance-4tenz.5.5.4`
- **Baseline revision:** `5f3384cd146f8c0a4b55b0e24e5c03acef1dfab2`
- **Rejected design:** the second render-v2 draft put
  `RenderConnectionIdentity` at the literal tail of PDUs 79 and 80 and marked
  it `serde(default)`. Existing tests had treated arbitrary bytes appended
  *after a complete framed PDU* as if they were a future field inside the
  frame's length-delimited varbincode payload.
- **Observed result:** strict-remote `fmd` job
  `j-29953507796713785` failed
  `render_application_v2_connection_authority_is_a_real_wire_tail_extension`
  with `failed to fill whole buffer` while a v50 decoder read a canonical
  v48/v49 payload. The newer positional schema requests all of its fields;
  reaching EOF while reading the new field is an error before serde can apply
  the struct default. Bytes outside the frame prove only that a single-frame
  decoder stops at its declared length.
- **Correction:** PDU IDs 79/80 remain permanently bound to explicit v1
  schemas; authoritative v2 uses new IDs 84/85 and is capability-gated at
  codec 50. PDU 27, whose older four-field response genuinely must remain
  readable during handshake, now has an explicit bounded dual-schema decoder
  with canonical legacy/current frame tests in both compression modes.
- **Decision:** reject positional tail fields as automatically
  bidirectionally additive. New PDU identifiers are the default evolution
  mechanism; a reused identifier requires an explicit decoder for every
  accepted schema and real frames in both directions.
- **Primary retry condition:**
  > Retry a same-identifier schema extension only with a bounded explicit dual-schema decoder, canonical old/new payload fixtures inside the declared frame length, corruption rejection, and negotiated emission authority.

### IS-N015 — Forced `InputSerial` is dispatch acknowledgement, not PTY/application echo

- **Classification:** causal-boundary and correctness rejection
- **Bead:** `ft-interactive-systems-performance-4tenz.2.11`
- **Baseline revision:** `38e7f830fdb7507f6b08e10d5c68a764f84bf13b`
- **Candidate code revision:** `80487657ba8086214b689ed185e28f4dfd174d57`
- **Rejected inference:** the forced `GetPaneRenderChangesResponse` emitted
  immediately after `pane.key_down` was treated as evidence that the PTY or
  application had echoed every prediction through its returned `InputSerial`.
  The client retired those overlays at the serial boundary even when the
  terminal had not changed.
- **Code-grounded result:** the server samples the terminal sequence and forces
  a response after key dispatch, before any required PTY read, parser apply, or
  application echo. A serial-only response therefore measures dispatch RTT,
  not K7 echo latency. A newer terminal sequence without the predicted row is
  also missing evidence, and an unchanged row whose line sequence does not
  cross the dispatch fence cannot validate the prediction.
- **Correction:** each prediction now records the terminal-sequence fence of
  its dispatch acknowledgement. Settlement requires later authoritative row
  state whose terminal and line sequences cross that fence. A reordered stale
  response may contribute its serial/fence but not its surface content; the
  outer unilateral filter preserves that metadata without paying hydration
  work, and cached reconciliation accepts only a fresh `LineEntry::Line`.
  Missing rows remain pending, no-echo predictions expire with RTT-scaled
  confidence degradation, authoritative reconnect snapshots reset prediction
  state, and `InputSerial::now` has a process-local monotonic floor.
- **Boundedness correction:** retained prediction state is capped at 4,096
  cells per pane. Paste prediction performs a bounded all-or-nothing preflight
  before `textwrap::fill`, so an arbitrarily large paste cannot allocate or
  traverse a second unbounded speculative representation and cannot leave a
  partially admitted overlay.
- **Canonical proof:** strict-remote job `j-29953507796713895` on
  `vmi1264463` ran `cargo test -p frankenterm-client --lib` at the candidate
  revision: 117 passed, 0 failed, and one pre-existing harness test remained
  explicitly ignored. Strict-remote job `j-29953507796713890` on `hz1` ran
  `cargo clippy -p frankenterm-client --all-targets -- -D warnings` and passed.
  Unchanged codec/server slices retain earlier strict-remote coverage from
  `j-29953507796713871`, `j-29953507796713874`, and
  `j-29953507796713878`.
- **Retained failed evidence:** job `j-29953507796713886` correctly rejected
  the preflight's nested `if` under warnings-denied Clippy; the exact equivalent
  collapse is in the candidate. Test jobs `j-29953507796713891` and
  `j-29953507796713893` failed before Cargo because worker `vmi1227854` could
  not create its remote target directory. An additional pinned retry failed
  closed with `RCH-I006` rather than falling back locally. These are lint and
  infrastructure evidence, not behavioral test failures.
- **Formatting boundary:** RCH rejected remote `cargo fmt --check` as a
  non-compilation command (`RCH-E301`). `git diff --check` passed; no local
  formatter was run and no remote formatting proof is claimed.
- **Decision:** reject every latency or correctness claim that interprets
  `InputSerial::elapsed_millis` as PTY/application echo. It is dispatch RTT
  only. Later matching row state is safe overlay reconciliation, not a renamed
  K7 measurement.
- **Primary retry condition:**
  > Reclassify a protocol marker as PTY/application echo only after it is emitted from a causally downstream PTY-read/parser/application boundary, is retained in a stage trace, and passes delayed, no-echo, reordered, reconnect, and boundedness regressions.

### IS-N016 — Legacy `InputLatency` summaries are caller-labelled proxy diagnostics, not replayable production latency evidence

- **Classification:** wrong-evidence-pipeline and false-green-gate rejection
- **Bead:** `ft-interactive-systems-performance-4tenz.2.9`
- **Baseline revision:** `c286309f5fe3c450158155e49b92d38b6c98c42a`
- **Semantic candidate revision:**
  `9fb37eb5bd8448042587e68bb4abbfa05f6b5a57`
- **Clippy-corrected implementation revision:**
  `1cfb2212b89c67f26588841e4faf833084233f75`
- **Latest fully test/check-reconfirmed source descendant:**
  `8706d327b5f4902e2596ca80f70b5e69f004c93f`. Later source changes repair
  warnings in documentation, PID evidence, benchmarks, profiling/test paths,
  an unrelated production mux row-range request, and unrelated compile-only
  integration-test targets. The only later `input_latency` edits are
  `#[cfg(test)]` singleton-budget constructors in
  `1856d87fbdbd62b7c4f93302b05b6b96278be5c5`; its production implementation is
  unchanged from the Clippy-corrected candidate. Current repair descendant
  `637083428c61b52eb91c1829fca65e7e88ff4255` has not yet earned a replacement
  all-target warnings-denied verdict because the committed renderer-scenario
  contract is still being made compile-coherent; that unrelated compile blocker
  is not latency evidence.
- **Rejected inference:** a green legacy `InputLatencyReport` or
  `BudgetCheckResult` was treated as replayable production
  keypress-to-present evidence. This framework is not wired into the live
  AppKit, mux transport, remote PTY, parser, renderer, drawable, display, or
  photon path. Its producer and clock-domain IDs are caller assertions, not
  verified identity or synchronization.
- **Rejected false-green paths:** the prior surface could admit an empty
  collector through zero summaries, silently overwrite duplicate stages,
  collapse duplicate wire-map keys, filter invalid samples, compute partial
  spans, retain duplicate IDs, normalize zero capacity, advance an unchecked
  sequence through exhaustion, accept empty or ambiguous budgets, ignore stage
  budgets, expose mutable or deserializable derived verdicts, and round an
  authority-bearing `f64` threshold into a different gate. Approximate ratios
  could also display `1.0` while the exact integer comparison failed.
- **Authority correction:** every report and verdict is permanently
  `proxy_only`. `PtyRead` denotes a PTY master/reader read boundary and does not
  by itself establish causal application echo. `GpuPresent` is a
  caller-recorded operation marker, not scanout or photons. Producer and clock
  labels require future external authority; no registry, evidence-bundle
  schema/content binding, or clock calibration is implemented or verified by
  this module. Cross-host calibration and production authority remain trace-v2
  responsibilities.
- **Structural correction:** every admitted measurement contains all six unique
  stages under one asserted clock-domain label with non-regressing adjacent
  timestamps; the retained ring separately requires unique non-reserved
  measurement IDs. This is per-measurement label consistency, not cross-sample
  clock calibration. Partial measurements remain available as diagnostics but
  are not admitted as complete latency evidence. Duplicate writes preserve the
  original and leave a sticky fault. Unknown fields in collector, measurement,
  timestamp, stage-budget, and budget wire structs; duplicate authority-bearing
  map keys; unsupported collector schema versions; invalid capacities;
  malformed allocator frontiers; reserved/unallocated IDs; and oversized
  sequences are rejected. Decoding retains at most 65,536 samples and five
  adjacent stage budgets; element `MAX + 1` is probed with `IgnoredAny` rather
  than materialized as an unbounded value.
- **Gate correction:** ID zero and `u64::MAX` are reserved,
  `u64::MAX - 1` is last usable, and an attempted terminal allocation
  permanently taints the collector. Empty, incomplete, duplicate,
  clock-mismatched, regressing, invalid-capacity, or exhausted evidence yields
  zero admitted samples, no percentile summaries, and a non-pass verdict. A
  malformed or noncanonical budget wire is rejected before a verdict exists. A
  decoded or in-memory semantically inadmissible budget produces a non-pass
  verdict with a typed budget error but does not erase summaries derived from
  otherwise valid evidence. A late stage-breakdown failure clears aggregate
  and stage summaries.
  `InputLatencyReport`, `BudgetCheckResult`, and
  `BudgetCheckDetail` are public DTO types whose derived fields are
  private/getter-only; they implement `Serialize`, not `Deserialize`. Only
  report/verdict envelopes carry `proxy_only`; a standalone detail is not
  authority.
- **Numeric and replay correction:** retained property jobs
  `j-29955720610840590` and `j-29955720610840593` each passed all four
  synthetic latency-watchdog integration tests but failed
  `budget_serde_roundtrip` after 40 of 41 properties because decimal JSON moved
  the threshold `0.9088411463024019` by one ULP, from bits
  `4606361332840929359` to `4606361332840929360`. Enabling `serde_json`'s
  `float_roundtrip` feature did not repair that authority failure. The budget
  wire now requires canonical `0x` plus 16
  lowercase hexadecimal IEEE-754 digits; decimal, numeric, uppercase, and
  malformed encodings fail closed, while canonically encoded but semantically
  inadmissible IEEE-754 payloads retain their bits until typed validation.
  Positive finite thresholds are scaled with exact
  significand-and-exponent integer arithmetic, preventing false passes above
  `2^53`; non-positive, negative-zero, NaN, infinity, subnormal, and overflow
  cases are handled explicitly. Approximate ratio fields were removed.
- **Canonical focused proof:** strict-remote job
  `j-29955720610840594` on `yto` tested the semantic candidate with
  `cargo test -p frankenterm-core --test proptest_input_latency --test integration_latency_watchdog_scheduler`:
  41/41 property tests and 4/4 synthetic integration tests passed. Job
  `j-29955720610840596` on `yto` passed 52/52 library tests selected by
  `cargo test -p frankenterm-core --lib input_latency`, and job
  `j-29955720610840601` on `yto` passed all four compile-fail doctests selected
  by `cargo test -p frankenterm-core --doc input_latency`. All exited zero
  through an identified remote worker; none is a local Cargo result. At source
  descendant `045bfc65c989ecb8964fdcf6c668540b41b52c90`, job
  `j-29955720610840622` on `hz2` reconfirmed 52/52 library tests, job
  `j-29955720610840625` on `vmi1152480` reconfirmed 41/41 property tests and
  4/4 synthetic integration tests, and job `j-29955720610840623` on
  `vmi1149989` passed package-scoped `cargo check -p frankenterm-core
  --all-targets`. These were also strict remote executions with no local
  fallback.
- **Warnings-denied proof trail:** job `j-29955720610840606` found two
  `input_latency` Clippy diagnostics at the Serde adapter/lifetime boundary;
  `1cfb2212b89c67f26588841e4faf833084233f75` corrected them without changing
  behavior. Job `j-29955720610840607` then progressed beyond `input_latency`
  and found an unrelated lazy-continuation documentation lint, corrected in
  `bb4abef4c`. Job `j-29955720610840608` progressed farther and found fourteen
  diagnostics in the unrelated fleet-memory PID certificate. After that repair,
  job `j-29955720610840610` reached four `large_futures` diagnostics in the
  unrelated `compression_bypass` benchmark. Revision
  `351b05410f8334a0396ed27eeffa3213e5a862d5` boxes the two oversized mux-connect
  futures at their source; strict-remote job `j-29955720610840611` on `yto`
  passed
  `cargo clippy -p frankenterm-core --bench compression_bypass -- -D warnings`.
  Package-scoped all-target retry `j-29955720610840612` on `yto`, running
  `cargo clippy -p frankenterm-core --all-targets -- -D warnings`, progressed
  past those repairs and found two `ref_option` diagnostics in
  `m6_search_while_streaming`; revision
  `22ae0a04e2ea97c0bd05994c67f7d27a814dfc61` changes the helpers to
  `Option<&Distribution>` without changing their lookup or NaN fallback.
  Strict-remote job `j-29955720610840613` on `hz1` passed
  `cargo clippy -p frankenterm-core --bench m6_search_while_streaming -- -D warnings`.
  A static same-pattern sweep then found four remaining compile-reachable
  oversized `DirectMuxClient::connect` calls; revision
  `4cfd6f4a770f9fa286339901ef3ea7b15ec2b540` boxes those source futures in the
  PDU-pipelining, mux-client-operations, and mux-pool-scaling benchmarks.
  Strict-remote job `j-29955720610840614` on `hz2` passed warnings-denied
  Clippy for all three repaired benchmarks. Package-scoped all-target retry
  `j-29955720610840615` on `hz1`, running the same command, then reached a
  `needless_collect` diagnostic in `round6_profile_realistic_workloads`; revision
  `04ca88652f79741a5d9d7f3b5f1e602e508014f3` preserves its fail-closed gate
  assertion with `Iterator::any` and removes the unnecessary allocation. The
  parallel package-scoped all-target retry `j-29955720610840616` on `hz2`,
  running the same command, independently reached three
  `doc_lazy_continuation` diagnostics in the pattern-detection benchmark.
  Revision `e888301408731f06ef6fc705df25904a9e24ac83` inserts the missing
  paragraph boundary without changing its evidence explanation.
  Strict-remote job `j-29955720610840617` on `hz2` then passed the focused
  round-six test target with `-D warnings`, and job `j-29955720610840619` on
  `vmi1149989` passed the focused pattern-detection benchmark target with
  `-D warnings`. The longer-running all-target jobs continued to provide useful
  negative evidence: `j-29955720610840615` on `hz1` exposed seven singleton-map
  constructors in the `input_latency` test module plus explicit-match,
  visibility, range, duration, and singleton-range diagnostics in unrelated
  test paths; `j-29955720610840618` on `vmi1152480` exposed two remaining
  similar-name diagnostics in the PID certificate; and
  `j-29955720610840620` on `vmi1153651` reached a manual `Result` fallback in
  the round-five pattern benchmark. Revisions
  `966962cebf61aa14d62de09d234b9b21ef73454f`,
  `1856d87fbdbd62b7c4f93302b05b6b96278be5c5`,
  `43a7f709558743f86b5fa43f4f69145ce267b134`, and
  `045bfc65c989ecb8964fdcf6c668540b41b52c90` remove the diagnosed and
  same-pattern test allocations/lints without changing production
  `input_latency` behavior. Revision
  `9422312e71a80fb2b8aeee9fa937aaa52fd32fa5` replaces the final manual fallback
  with its exact standard-library equivalent. Static same-pattern sweeps found
  no remaining high-confidence instances of the previously diagnosed
  count-only or manual-fallback patterns. All-target job
  `j-29955720610840621` on `hz1` at revision
  `045bfc65c989ecb8964fdcf6c668540b41b52c90`, all-target job
  `j-29955720610840633` on `vmi1153651` at revision
  `9422312e71a80fb2b8aeee9fa937aaa52fd32fa5` (exit 101), and focused-benchmark
  job `j-29955720610840632` on `yto` at the same `9422312e7` revision then
  proved that `Vec::from([range])` did not escape
  `single_range_in_vec_init`: each stopped at the production
  `get_lines_with_cx` request before later targets could be credited. Revision
  `590336e38d1667b0235d0655ac88a8ff42bdbe27` uses
  `std::iter::once(range).collect()` at all 17 same-pattern production/test
  sites, preserving a vector with exactly one range rather than enumerating the
  range's scalar values. Strict-remote job `j-29955720610840634` on `yto`
  passed warnings-denied library Clippy at that revision. All-target job
  `j-29955720610840635` on `vmi1149989` then progressed beyond the range repair
  and found five diagnostics in the unrelated round-seven RSS harness. Revisions
  `d344ae2dafdef4d3682fe0b450ab95d9e9eaa3db` and
  `059b735db64cdfaf24446b9acc88678cf9bb9abc` preserve the harness calculations
  while repairing its documentation, checked division, option predicate, and
  equality assertion. Focused strict-remote job `j-29955720610840636` on
  `vmi1149989` passed warnings-denied Clippy for that test target, and job
  `j-29955720610840638` on `vmi1152480` passed both round-seven tests. Exact
  `059b735db64cdfaf24446b9acc88678cf9bb9abc` job
  `j-29955720610840640` on `vmi1149989` independently passed the same two
  tests. Exact
  `059b735db64cdfaf24446b9acc88678cf9bb9abc` all-target job
  `j-29955720610840639` on `yto` then exposed nine over-indented Rustdoc lines
  in the unrelated operator-cockpit harness and one oversized future in the
  compile-only subprocess-smoke target. Revision
  `91dc37aaf73f0c9cdaa762d32273728047dba77e` rewrites that prose without a
  Markdown list and boxes the named future at its source; it neither changes
  production `input_latency` nor executes the subprocess fixture. Focused
  strict-remote job `j-29955720610840643` on `vmi1152480` passed
  warnings-denied Clippy for both repaired test targets. The earlier parallel
  all-target job `j-29955720610840637` on `vmi1153651` then exposed five more
  oversized `MuxPool::list_panes` futures in the socket-disappearance test and
  three manual `repeat().take()` constructions in the delta-extraction bench.
  Revision `3a0838e1e4b8fadc2a54fc54470acd830c5cce45` boxes the five futures and
  uses `repeat_n` in that benchmark plus two exact same-pattern property-test
  generators. Focused jobs `j-29955720610840647` and
  `j-29955720610840649` failed before Cargo because workers `vmi1227854` and
  `vmi1293453` could not create their remote target directories; they are
  infrastructure non-proof. Exact `91dc37aaf73f0c9cdaa762d32273728047dba77e`
  all-target job `j-29955720610840641` on `hz2` independently progressed to
  twelve additional oversized real-mux snapshot-test futures. Revision
  `5d9cd3db79dae96a61c8d545f8f3bef5737a5b38` boxes every diagnosed
  `list_panes`, `split_pane`, and `send_text_with_options` future at its source;
  this compile-only repair does not execute or contact a mux process.
  Exact-source revision `8706d327b5f4902e2596ca80f70b5e69f004c93f`
  then received additional strict-remote evidence: job
  `j-29955720610840668` on `vmi1153651` passed its focused vendored-async
  warnings-denied lane after the 32 diagnosed futures were boxed; job
  `j-29955720610840669` on `hz2` passed focused escape-parser Clippy; job
  `j-29955720610840671` on `vmi1152480` passed focused term library/test
  Clippy; and job `j-29955720610840672` on `hz2` passed
  `cargo check -p frankenterm-core --all-targets`. Job
  `j-29955720610840674` on `vmi1152480` reconfirmed 41/41 property tests and
  the four synthetic integration tests; job `j-29955720610840673` on
  `vmi1153651` reconfirmed all 52 selected library tests; and job
  `j-29955720610840683` on `vmi1149989` reconfirmed all four selected
  `input_latency` compile-fail doctests. Exact workspace job
  `j-29955720610840686` on `vmi1152480` also passed
  `cargo check --workspace --all-targets` after 28 minutes. All reached an
  identified remote worker with no local fallback. These are static,
  package/focused proxy-module, and workspace compilation proofs, not live
  latency evidence.

  Exact `8706d327b5f4902e2596ca80f70b5e69f004c93f` all-target Clippy job
  `j-29955720610840682` on `hz2` remained useful negative evidence: it reached
  one similar-name diagnostic, one boolean-to-integer diagnostic, and one
  manual midpoint in unrelated benchmark/test targets. Revision
  `fd8da9f3c48c72c6ff025095a78dfc64052cdb74` repairs those and the two exact
  same-pattern midpoint sites without changing `input_latency`. The next
  package all-target job `j-29955720610840688` on `yto` failed at the separately
  committed, still-in-progress renderer-scenario contract before it could
  provide a package verdict. Workspace Clippy job
  `j-29955720610840687` independently found `chunks_exact(2)` in the unrelated
  escape-parser hex decoder; revision
  `637083428c61b52eb91c1829fca65e7e88ff4255` replaces it with exact array
  chunks and an even-length debug assertion. A clean descendant all-target
  warnings-denied rerun remains required after the renderer contract is
  compile-coherent. Remote formatter admission still fails with `RCH-E301`,
  and no local formatter is authorized or claimed. Therefore `.2.9` remains
  open despite the focused green evidence.
  <!-- IS-N016-CLIPPY-RESULT -->
- **Adjacent false-green evidence repaired:** investigation of the PID
  diagnostics proved that its gain-margin phase detector was unreachable, its
  synthetic plant-identification wording overclaimed authority, its closed-loop
  test reimplemented rather than exercised the shipped controller, and several
  stall, missing-plan, invalid-input, reset, and byte-identity checks could pass
  incorrectly. Revision `405a667459051710849ac1a3523011f1a2807c95` repairs
  those defects. Strict-remote job `j-29955720610840609` on `yto` passed all
  9/9 focused `fleet_memory_pid_dampening_cert` tests. This adjacent repair is
  required repository hygiene, not latency evidence.
- **Broad-proof boundary:** workspace check job `j-29955720610840602` and
  workspace Clippy job `j-29955720610840603` failed on `hz2` because
  `xcb-util.pc` was unavailable; job `j-29955720610840604` failed on `yto`
  because `x11-xcb.pc` was unavailable; and automatic retry
  `j-29955720610840605` failed on `fmd` for lack of disk space. Those are
  retained remote-environment failures, not successful workspace proof and not
  behavioral failures in this candidate.
- **Earlier retained diagnostics and infrastructure failures:** job
  `j-29953507796713904` completed an earlier check with warnings and was
  superseded. Job `j-29955676990078977` exposed malformed property-test
  diagnostics. Jobs `j-29955676990078978` and
  `j-29955676990078979` failed before Cargo because their remote target
  directories were not writable. Jobs `j-29955676990078981` and
  `j-29955676990078982` passed earlier revisions and are superseded. Job
  `j-29955720610840577` was cancelled after static audit superseded its
  revision. Job `j-29955720610840578` exposed test-only dead code, six malformed
  format strings, and an invalid `Result` equality assertion; it was cancelled
  after repair. Job `j-29955720610840580` exposed two missing type annotations
  and was cancelled after repair. A pinned `hz1` attempt failed closed before
  job creation with `RCH-I002` (`disk_critical_without_fresh_telemetry`). Job
  `j-29955720610840582` targeted a superseded candidate. None of these runs is
  promoted into canonical proof. Later all-target jobs
  `j-29955720610840626`/`j-29955720610840629` on `vmi1227854` and focused jobs
  `j-29955720610840628`/`j-29955720610840631` on `vmi1293453` failed before
  Cargo because their remote target directories could not be created. A pinned
  `yto` attempt was refused before job creation with `RCH-I005` active-project
  exclusion; a final-candidate pinned `hz1` attempt was refused with `RCH-I002`
  `memory_pressure_critical`. Both refused local fallback as required. These
  infrastructure outcomes are retained non-proof, not behavioral failures.
- **Workload and claim boundary:** this was a static correctness and evidence
  repair, not a performance experiment. No performance workload, measurement
  build profile/configuration, runtime topology/transport, or performance
  sample count existed. It collected no live Mac-to-trj keypresses and no
  AppKit, transport, PTY, application-echo, terminal-update, render, drawable,
  display, photon, observer-effect, distribution, confidence/noise, `cv_pct`,
  or visual-equivalence evidence. No active FrankenTerm session was used as a
  qualification rig.
- **Formatting boundary:** RCH rejected remote `cargo fmt --check` as a
  non-compilation command (`RCH-E301`). Final `git diff --check` passed; no
  local formatter or local Cargo command was run, and no remote formatting
  proof is claimed.
- **Decision:** keep the fail-closed proxy repair because it removes real
  false-green diagnostics, but reject every attempt to promote these summaries
  into production input-to-present evidence. This change cannot replace the
  isolated live `.2.7` matrix, cannot by itself unblock `.2.8`, and does not
  supersede trace v2 as production authority.
- **Primary retry condition:**
  > Do not retry from a cold read; use the trace-v2 identity/clock bundle and isolated live Mac-to-trj input rig in .2.1–.2.8 instead.

### IS-N017 — Retina logical DPI and backing scale are not two independent pixel-density multipliers

- **Classification:** wrong-model rejection; kept structural correction
- **Bead:** `ft-interactive-systems-performance-4tenz.3.1`
- **Rejected candidate:** an uncommitted catalog draft; it has no immutable
  revision or retained qualifying artifact.
- **Retained structural revision:**
  `fba4ecf0131fcca1f286fc7525c6b2d8011c8544`
- **Rejected inference:** representing a canonical macOS Retina move as logical
  `192_000` DPI plus backing scale `2_000` and multiplying both treated
  physical/backing density as two independent scale inputs. With the frozen
  8 px reference cell, that model produces 32 px—4x the reference—rather than
  the intended 16 px backing-pixel cell.
- **Structural correction:** `dpi_milli` is logical DPI, independent of
  physical panel DPI and backing scale. The canonical move retains logical
  `96_000` DPI and changes scale from `1_000` to `2_000`; the checked
  fixed-point derivation therefore maps 8 px to 16 px exactly. At the retained
  revision, all four `MoveToDisplay` payloads and all 32
  `display-retina-002` surface states carry `(96_000, 2_000)`. The checked-in
  catalog is 4,804,862 bytes with SHA-256
  `8d21fe3a26db8ca06e7def8746a7e22051dab980698438a574cbaf7df3a00b66`.
- **Proof boundary:** this is structural source, contract, and artifact
  evidence only. It collected no AppKit or `NSScreen` adapter trace,
  Apple-silicon render, presented-frame geometry, visual-equivalence result,
  or performance sample. It therefore cannot qualify native DPI handling,
  text appearance, resize/zoom latency, or M4/M5 performance.
- **Decision:** reject the double-scaling model and retain the corrected
  contract, without promoting any runtime or performance claim.
- **Primary retry condition:**
  > Reopen native qualification only when one retained same-window Apple-silicon display-move trace independently records logical DPI and backing scale at the platform adapter, proves the production render path applies each exactly once, and matches the predicted 8 px to 16 px cell geometry, exact residual padding, and native presented-frame visual oracle.

### IS-N018 — Bare numeric renderer-scenario `u64` seeds and mutation tests built on an invalid canonical schema are rejected evidence

- **Classification:** ambiguous wire identity and false-green schema-gate
  rejection; kept structural correction
- **Bead:** `ft-interactive-systems-performance-4tenz.3.1`
- **Rejected candidate revision:**
  `2f6a6f3c567bd0d20f51919c950aaab69c56dcbc`
- **Rejected inferences:** a bare JSON integer was treated as a portable 64-bit
  renderer-scenario identity even though IEEE-754-only consumers can round
  values above `2^53`. Separately, schema-mutation tests treated “some
  validation error exists” as proof that their mutation was rejected without
  first proving the canonical document was schema-valid. This result is scoped
  to `RendererScenarioDefinition.seed`; other generator-seed contracts require
  their own wire-identity audit.
- **Failed remote evidence:** job `j-29955720610840704` completed with 17 tests
  passing and 15 failing. The canonical schema incorrectly required at least
  one `comparator_policy_refs` entry, rejecting all 160 legitimate last-Draft
  provenance checkpoints whose exact semantic policy set is empty. That
  candidate also serialized the four distinct
  `renderer.dpi_display_move.{p001,p020,p050,p200}` seeds
  `0x4654525300070001`, `0x4654525300070014`, `0x4654525300070032`, and
  `0x46545253000700c8` as the same rounded decimal
  `5067765997134873000`. The schema contradiction and seed-identity collapse
  cascaded through schema, round-trip, and mutation checks; the 15 failures are
  not 15 independent defects. Job `j-29955720610840705` exited 101, but its
  diagnostics were unavailable, so it cannot classify a source failure or
  provide corrective proof. Exact-descendant job `j-29955720610840708` later
  isolated only two integration-test lint defects, `bool_to_int_with_if` and
  `type_complexity`, which led to the Clippy-only descendant below.
- **Structural correction:** revision
  `fba4ecf0131fcca1f286fc7525c6b2d8011c8544` changes all 32 seeds to exact
  `0x` plus 16 lowercase hexadecimal digits while retaining typed Rust `u64`
  values, rejects numeric/decimal/uppercase/short encodings, and binds source,
  schema, contract, artifact, and semantic validation to catalog revision `2`.
  It also permits structurally empty comparator arrays while the Rust validator
  enforces the exact role-specific zero/one/two policy set, and requires every
  schema mutation to begin from a schema-clean canonical document.
- **Clippy-only descendant:**
  `0eff8fe78ac8a495c14c5c3d9878c36a2d64c218` repairs integration-test lint
  findings without changing the corrected wire or validation semantics.
- **Corrected artifact:** `docs/design/renderer-scenario-catalog.v1.json` is
  4,804,862 bytes with SHA-256
  `8d21fe3a26db8ca06e7def8746a7e22051dab980698438a574cbaf7df3a00b66`;
  it contains exactly 32 unique string seeds and `catalog_revision: 2`.
- **Retained corrected proof:** strict-remote jobs `j-29955720610840706` and
  `j-29955720610840707` respectively completed the corrected leaf library's
  all-target check and warnings-denied library Clippy gate. Job
  `j-29955720610840709` completed the exact descendant's renderer
  integration-test target Clippy gate with warnings denied. Exact corrected
  descendant `24c43fd6db72fcdd12599cdbf0cb474053b7e74b` then passed all 34
  renderer-catalog tests in strict-remote job `j-29955720610840724` and passed
  the warnings-denied integration-test Clippy gate in job
  `j-29955720610840727`. Job `j-29955720610840726` ended in an RCH broken pipe
  before proof and is retained only as infrastructure-negative evidence.
- **Proof boundary:** these are contract, schema, serialization, and test-gate
  corrections. They do not cover a live FrankenTerm session, mux domain, PTY,
  AppKit path, renderer, presented frame, visual-equivalence workload, or
  performance sample.
- **Decision:** reject numeric-`u64` JSON as canonical seed identity and reject
  every mutation verdict obtained from a schema-invalid baseline. Keep the
  structural correction, but promote no runtime, visual, or performance claim.
- **Primary retry condition:**
  > Reintroduce a numeric `u64` wire only when every supported serializer, parser, query tool, and evidence consumer demonstrably preserves all 64 identity bits above `2^53` and a negative regression fails on any rounded representation; otherwise retain the fixed-width lowercase hexadecimal wire and require every mutation test to prove its canonical baseline schema-clean before mutation.

### IS-N019 — Replacing a held buffer with the same corpus is not a destructive negative control

- **Classification:** invalid negative-control rejection; kept test correction
- **Bead:** `ft-interactive-systems-performance-4tenz.3.1`
- **Rejected candidate revision:**
  `0eff8fe78ac8a495c14c5c3d9878c36a2d64c218`
- **Failed remote evidence:** strict-remote job `j-29955720610840712`
  reached the 34-test renderer-catalog suite and showed that
  `hold_through_rejects_early_alternate_exit_and_replacement` expected
  `RSC-STATE-001` from a mutation that validation correctly accepted. The
  remaining long-running tests were allowed to continue so their outcomes
  could not be hidden by the first failure; the final result was 33 passed and
  exactly this one failed.
- **Rejected inference:** changing an `EnterAlternateBuffer` step into
  `ReplaceActiveBuffer` was assumed to destroy the earlier hold-through effect.
  The mutation retained the exact same alternate-screen corpus identity, and
  the following typed-state materialization restored the same canonical buffer
  contents. It therefore did not violate the continuous hold promise and could
  not prove that the validator missed an early replacement.
- **Structural correction:** revision
  `24c43fd6db72fcdd12599cdbf0cb474053b7e74b` makes the replacement select a
  different corpus and requires the intended “does not survive continuously
  through promised checkpoint” diagnostic, rather than accepting any unrelated
  `InvalidState` result.
- **Corrected remote proof:** strict-remote job `j-29955720610840719` passed
  the exact corrected hold-through test. Job `j-29955720610840724` then passed
  the complete 34-test renderer-catalog suite at the same exact revision, and
  job `j-29955720610840727` passed its warnings-denied integration-test Clippy
  gate. Job `j-29955720610840726` suffered an RCH broken pipe and contributes
  no source verdict.
- **Proof boundary:** this repairs a semantic negative control only. It does
  not execute a live terminal, alternate-screen application, PTY, renderer, or
  presentation path and carries no visual or performance authority.
- **Decision:** reject the same-corpus replacement as evidence and retain the
  causally destructive mutation. The focused, full-suite, and warnings-denied
  structural gates are now green at the exact corrected revision; this does
  not promote any live terminal, visual, or performance claim.
- **Primary retry condition:**
  > Credit this negative control only when the replacement uses a corpus identity different from the held alternate-screen corpus and the exact corrected revision fails specifically because the original effect does not survive continuously through its promised checkpoint.

### IS-N020 — Deterministic tombstone eviction cannot preserve a never-reusable layout-window identity

- **Classification:** wrong-model rejection; kept fail-closed structural guard
- **Bead:** `ft-interactive-swarm-product-convergence-7xqz4.8.10.5.1`
- **Rejected candidate:** an uncommitted bounded-retention design; it has no
  retained candidate revision or qualifying runtime artifact.
- **Retained structural revision:**
  `55c63aa37f4c072cb4d05f7fa256474564c822ed`
- **Rejected inference:** evicting the oldest deterministic overlay tombstone
  was treated as safe because a later mutation carrying `base_revision =
  Some(_)` still conflicts with an absent live overlay. That does not cover a
  delayed or replayed create carrying `base_revision = None`: after eviction,
  the store can no longer distinguish that stale lineage from a genuinely new
  identity, so the retired window can be resurrected.
- **Structural correction:** a stable `LayoutWindowId` is currently
  never reusable. Its tombstone is therefore never pruned; the hard tombstone
  cap rejects a new distinct retirement before removing its live overlay and
  partitions that rejection from unrelated valid mutations. Existing
  tombstone replays remain idempotent at the cap.
- **Proof boundary:** this establishes only the fail-closed storage invariant.
  It does not provide safe unbounded retention, compaction, a durable identity
  generation, a public overlay durability receipt, or user-visible tab-order
  restoration. Safe reclamation remains owned by the named Bead.
- **Decision:** reject age-based, count-based, and deterministic-key eviction
  until a durable identity-generation or equivalent non-reuse proof exists.
- **Primary retry condition:**
  > Retry tombstone reclamation only when every stale base=None create is provably distinguishable from a fresh identity across process restart, journal recovery, reconnect, and delayed replay, with cap and cap-plus-one tests that cannot resurrect a retired layout window.

### IS-N021 — Independent item-count caps do not prove the aggregate 4 MiB journal envelope

- **Classification:** false resource-envelope rejection; retained bounded
  count guards
- **Bead:** `ft-interactive-swarm-product-convergence-7xqz4.8.10.5.2`
- **Rejected candidate:** the current count-only admission model; it is retained
  as defense in depth but has no authority for the aggregate byte claim.
- **Observed contradiction:** the existing independent ceilings allow 4,096
  workspaces with 1,024-byte names before accounting for JSON syntax, escaped
  expansion, window-state fields, domain bindings, overlays, tab slots,
  tombstones, or the checksum envelope. That one collection alone can consume
  4 MiB of raw string content, so the independent caps cannot imply that a
  complete encoded transaction stays within 4 MiB.
- **Retained structural revision:**
  `55c63aa37f4c072cb4d05f7fa256474564c822ed` keeps finite per-collection,
  per-string, tab-count, and waiter bounds, and partitions terminal workspace
  and domain-binding quota failures by lineage so one impossible request does
  not strand unrelated valid work.
- **Proof boundary:** finite item counts prevent unbounded cardinality, but do
  not establish the aggregate serialized-byte envelope, peak encoder memory,
  write amplification, or long-session RSS behavior. No live session or
  target-hardware memory measurement is claimed.
- **Decision:** retain the count bounds, reject any statement that they prove
  the 4 MiB aggregate envelope, and require transaction-wide encoded-byte
  admission before publication.
- **Primary retry condition:**
  > Claim the 4 MiB journal envelope only after a pre-mutation aggregate-byte budget accounts for canonical encoding and escaping across every collection plus envelope/checksum overhead, rejects the exact byte limit plus one before publication, and is covered by adversarial maximum-width and maximum-escape fixtures.

### IS-N022 — Ambiguous publication must resolve the exact frozen snapshot before any coalesced successor

- **Classification:** crash-consistency rejection; kept exact-retry and
  durability-barrier correction
- **Bead:** `ft-interactive-swarm-product-convergence-7xqz4.8.10.5`
- **Rejected candidates:** retrying the latest coalesced pending batch after an
  ambiguous I/O result, and treating parseable bytes visible through a file
  name as proof that the generation was durable.
- **Rejected inference:** after a full write, file sync, directory sync, or
  lost acknowledgement, the caller cannot know which generation is
  authoritative. Replacing that exact in-flight batch with a newer coalesced
  successor can skip its predecessor's CAS base, acknowledge the wrong
  mutation, or publish a delete/update whose prerequisite never became
  durable. Likewise, a valid checksum proves byte integrity, not that file and
  directory metadata crossed the durability barrier.
- **Structural correction:** revision
  `55c63aa37f4c072cb4d05f7fa256474564c822ed` retains the exact frozen batch on
  any I/O failure that may have published a generation and resolves that batch
  before taking a successor snapshot. Idempotent recovery syncs the selected
  authority and parent directory before success; `AfterDirectorySync` is a
  distinct injected boundary rather than an alias for file sync. Rejected
  snapshot lineages invalidate same-base descendants instead of allowing a
  revision-skipping successor.
- **Proof boundary:** the commit primitive and exact-retry helper have
  deterministic interruption coverage, but the production
  `persistence_worker` still invokes commits with `WriteInterruption::None`.
  Consequently this revision does not yet prove the real worker's wake token,
  retry slot, successor coalescing, flush/binding waiter transfer, disconnect,
  or shutdown behavior under an injected ambiguous commit. That missing proof
  is tracked by `ft-interactive-swarm-product-convergence-7xqz4.8.10.5.5`.
- **Decision:** reject latest-state retry and visibility-as-durability. Retain
  exact-snapshot replay and explicit file-plus-directory durability barriers,
  without promoting helper-level evidence into worker or end-to-end authority.
- **Primary retry condition:**
  > Credit end-to-end crash consistency only when a test-only deterministic seam drives the real persistence worker through each ambiguous publication epoch and proves the exact frozen batch resolves before update and live-to-delete successors, with bounded wake, waiter, disconnect, repeated-failure, and shutdown behavior and no sleeps.

## Open hypothesis register

These are not negative results. Each remains open until a retained same-window
A/B satisfies the campaign keep gate or creates a closed entry above.

| Hypothesis | Bead | Required first evidence |
|---|---|---|
| Input/control traffic needs bounded priority/fair scheduling | `ft-interactive-systems-performance-4tenz.5` | client and server queue depth/age plus socket-ready→decode and decode→mux-start tails |
| Key acknowledgement performs redundant viewport work | `ft-interactive-systems-performance-4tenz.6` | deltas, rows, bytes, compression, and paints per key |
| Render should use a coherent short-lock terminal snapshot | `ft-interactive-systems-performance-4tenz.6` | terminal-lock hold/wait and parser lag attributed during paint |
| Resize/reflow needs a persistent topology-aware pool | `ft-interactive-systems-performance-4tenz.7` | thread creation/join, context switch, migration, batch-size, and p95/p99 attribution |
| Same-grid resize can retain/reproject line quads | `ft-interactive-systems-performance-4tenz.8` | global quad-miss/rebuild counts and live visual timing |
| Display-link pacing improves phase and input tails | `ft-interactive-systems-performance-4tenz.8` | actual 60/120Hz frame-phase and present timing |
| Zoom warmup should be deadline-aware | `ft-interactive-systems-performance-4tenz.8` | scale-change stage trace and missing-glyph/SSIM oracle |
| True atomic viewport-first reflow improves first present | `ft-35zzw` plus `.8` | first coherent viewport, first present, and cold convergence as separate intervals |

## New entry template

```markdown
### IS-YYYYMMDD-NNN — <candidate>

- **Classification:** measured performance rejection | wrong evidence pipeline | kept structural
- **Bead:** <id>
- **Baseline revision:** <sha>
- **Candidate revision:** <sha>
- **Target identity:** <host, CPU/GPU/topology, OS, display, transport>
- **Workload identity:** <pane count, age, output, action, seed, config/font hashes>
- **Focused command/artifact:** <exact command and retained path>
- **Broad command/artifact:** <exact command and retained path>
- **Samples/statistics:** <n, p50/p95/p99/p999, CI, cv_pct>
- **Equivalence:** <byte/state/visual/cursor/IME/a11y results>
- **Measured result:** <baseline -> candidate and percent>
- **Decision:** rejected | kept durable infrastructure
- **Primary retry condition:**
  > <one verbatim canonical form, fully instantiated>
```
