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
- **Beads:** `ft-interactive-swarm-product-convergence-7xqz4.8.10.5` and
  `ft-interactive-swarm-product-convergence-7xqz4.8.10.5.5`
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
  revision-skipping successor. Revision
  `37a8820d7be2107cef94e8a12b2369d5114dd888` adds a test-only rendezvous seam
  around the real `persistence_worker`, while leaving the production
  `WriteInterruption::None` path unchanged. The controlled worker proves that
  ambiguous update and live-to-delete predecessors retry exactly before their
  post-snapshot successors; failures block for a later wake instead of
  spinning; semantic outcomes cross an explicit barrier once; and dropped or
  disconnected flush/binding receivers are drained without false durability
  acknowledgement or waiter leakage.
- **Proof boundary:** exact revision
  `37a8820d7be2107cef94e8a12b2369d5114dd888` passed the eight focused
  controlled-worker tests and all 77 module tests on remote worker
  `vmi1153651` (jobs `j-29957405445980188` and
  `j-29957405445980189`), `cargo check -p frankenterm-gui --all-targets
  --locked` on `vmi1264463` (job `j-29957405445980191`), and
  `cargo clippy -p frankenterm-gui --all-targets --locked -- -D warnings` on
  `vmi1153651` (job `j-29957405445980194`). These deterministic Linux tests
  establish the worker state-machine contract; they do not simulate power
  loss, prove a particular filesystem or storage device's durability, exercise
  the native GUI, or establish ordered-tab restoration end to end.
- **Decision:** reject latest-state retry and visibility-as-durability. Retain
  exact-snapshot replay and explicit file-plus-directory durability barriers,
  now with real-worker state-machine evidence but without promoting it into
  filesystem, native-GUI, or ordered-tab end-to-end authority.
- **Primary retry condition:**
  > Credit target-filesystem crash consistency and ordered-tab restoration only after retained power-loss or equivalent target-class evidence and a native end-to-end restore trial bind the same journal generation to the reconstructed mux-window order; the deterministic worker proof is necessary but not sufficient.

### IS-N023 — Later error classification cannot erase antecedent ambiguous publication

- **Classification:** crash-consistency rejection; kept exact retry-debt
  correction
- **Bead:** `ft-interactive-swarm-product-convergence-7xqz4.8.10.5.5`
- **Rejected candidate:** after an exact retry returns a definite non-I/O error,
  clear the frozen retry slot because the latest error cannot itself have
  published a generation.
- **Rejected inference:** retry debt describes uncertainty created by an
  earlier publication attempt, not the taxonomy of the most recent failure.
  Once a full write, file sync, directory sync, or lost acknowledgement may
  have published the predecessor, a later corruption, validation, transaction,
  or other definite failure supplies no evidence that the antecedent
  publication did not occur. Dropping the frozen batch at that point lets a
  coalesced successor bypass an unresolved CAS base.
- **Structural correction:** revision
  `37a8820d7be2107cef94e8a12b2369d5114dd888` restores the exact retry batch
  after every exact-retry failure, independent of the later error class. A
  deterministic production-worker test drives ambiguous directory-sync loss,
  repeated ambiguous retry, a definite `Corrupt` retry failure, recovery of
  the same frozen revision, and only then successor handling. Its causal
  before-wake gate also proves the retained debt blocks without a timer or
  retry spin.
- **Proof boundary:** the exact revision and remote jobs listed in IS-N022
  cover this transition and its package check/Clippy gates. The injected
  `Corrupt` result proves state-machine classification, not real media
  corruption recovery or storage-hardware behavior.
- **Decision:** retry debt is monotonic until the exact predecessor is durably
  resolved and acknowledged. Later failures may change the error reported to
  current waiters, but they may not erase that debt or admit a successor.
- **Primary retry condition:**
  > Reconsider only if a stronger authority protocol can prove that the original possibly published generation is impossible or durably superseded before the frozen retry slot is cleared; a later error code alone is never that proof.

### IS-N024 — An exact-filter command that runs zero tests is not focused proof

- **Classification:** verification rejection; retained command-shape evidence
- **Bead:** `ft-interactive-systems-performance-4tenz.8.5.1`
- **Rejected candidate:** count an exit-zero `cargo test --exact` invocation as
  execution of the max-FPS frame-interval regression.
- **Rejected inference:** successful compilation and a zero-test harness result do
  not establish that the named regression ran. Job `j-29957405445980196`
  selected the wrong target, and job `j-29957405445980199` selected the GUI
  binary but omitted the module-qualified test name; both exited successfully
  while executing zero tests. The same failure mode recurred for tab-byte
  admission in job `j-29958204528001040`: selecting
  `--bin frankenterm-gui window_state_persist::tests` compiled for 19 minutes
  but ran zero tests because that module belongs to the library target. Renderer
  job `j-29958204528001072` repeated the same failure mode: it compiled the GUI
  library successfully but its filter matched none of the 241 library tests.
- **Decision:** retain both jobs as negative evidence. Require the correct binary
  target, the fully qualified module path, and output showing exactly one
  executed passing test before crediting focused proof.
- **Primary retry condition:**
  > Credit the focused regression only when the remote harness reports one test run and one test passed for `glyphcache::tests::config_changed_refreshes_frame_interval_from_explicit_config`; compilation or a zero-test result remains non-evidence.

### IS-N025 — Static Wayland review cannot substitute for feature-enabled compilation

- **Classification:** verification rejection; kept compile- and lint-driven fixes
- **Bead:** `ft-interactive-systems-performance-4tenz.2.15`
- **Rejected candidate:** promote the Wayland repeat-lifecycle patch from static
  inspection and diff hygiene alone.
- **Rejected inference:** source review did not expose the old-edition
  `TryFrom` import requirement, non-`Send` repeat state held across an await, or
  Wayland-feature-only lint failures. Strict remote jobs
  `j-29957405445980200`, `j-29957405445980201`, and
  `j-29957405445980203` exposed those defects and therefore remain failures,
  not partial passes. After those corrections, job
  `j-29958204528001033` exposed one adjacent feature-only
  `bool_assert_comparison` test lint; it too remains a superseded failure, not
  evidence for the Wayland lane.
- **Structural correction:** revisions `f6e1a2a6f`, `daf0b82d5`, and
  `eb9edd437` repair the source/`Send` issue and every feature-only lint exposed
  by the failed jobs without weakening repeat cancellation or timing bounds.
- **Proof boundary:** exact source `eb9edd437` passed all 26 Wayland-filtered
  window library tests on remote worker `vmi1227854` in job
  `j-29958204528001036`, the Wayland-feature all-target check in job
  `j-29958204528001037`, and all-target Clippy with `-D warnings` in job
  `j-29958204528001042`.
- **Decision:** retain every superseded failure and promote the exact
  deterministic Wayland repeat-lifecycle source only. No native compositor,
  key-to-photon latency, user-visible responsiveness, or target-hardware claim
  follows from remote Linux compilation and pure tests.
- **Primary retry condition:**
  > Promote only one exact source SHA for which the strict remote Wayland test, check, and `-D warnings` Clippy lanes all pass; preserve every superseded failed job in the ledger.

### IS-N026 — A failed trait-object handoff is not mux lifecycle proof

- **Classification:** verification rejection; kept compile-driven correction
- **Bead:** `ft-interactive-systems-performance-4tenz.5.5.15.6`
- **Rejected candidate:** credit the transactional tmux preparation change from
  static review or from the first strict remote compile attempt.
- **Rejected inference:** job `j-29958204528001027` failed because the deferred
  command path passed `&Box<dyn TmuxCommand>` where the helper required
  `&dyn TmuxCommand`. The ownership and conditional-commit design could be
  reviewed statically, but that type error meant the executable mux surface
  had not compiled and no tests had run.
- **Structural correction:** revision `5085826fa` passes the boxed command via
  `as_ref()` at the trait-object boundary without changing ownership or retry
  semantics.
- **Proof boundary:** exact source revision `5085826fa` passed all 687 mux
  library tests on remote worker `vmi1264463` in job
  `j-29958204528001028`, the mux all-target check in job
  `j-29958204528001030`, and all-target Clippy with `-D warnings` in job
  `j-29958204528001035`.
- **Decision:** retain the initial compile failure and its narrow correction;
  promote deterministic command-lifecycle correctness only. These jobs do not
  exercise native tmux, fault injection outside the deterministic harness,
  detach behavior, saturation, LAN latency, long soak, or user-visible
  responsiveness.
- **Primary retry condition:**
  > Credit saturation and performance only after the `.15.7` workload matrix retains native tmux fault, detach, queue-pressure, latency, and soak artifacts for the exact promoted source; compile-clean deterministic tests are necessary but not sufficient.

### IS-N027 — One-pass byte-admission backfill can emit a false quota receipt

- **Classification:** correctness and complexity rejection; kept exact
  fixed-point correction with a bounded-performance follow-up
- **Beads:** `ft-interactive-swarm-product-convergence-7xqz4.8.10.5.2`
  and `.5.2.1`
- **Rejected candidate:** after the `O(C log C)` multi-resource peel, test each
  removed lineage exactly once in reverse removal order and classify every
  survivor as a quota rejection.
- **Rejected inference:** one accepted late reducer can make an earlier
  rejection admissible after that earlier lineage has already been tested. In
  the retained four-candidate construction, the base has encoded upper bound
  96 against maximum 100 and sits at the tab cap. Candidate deltas are
  `C0=(+3 bytes,-2 tabs)`, `C1=(+12,-3)`, `C2=(+15,0)`, and
  `C3=(-5,+2)`. The peel removes `C2,C0,C1,C3`; one reverse pass accepts only
  `C0`, although `C3` is then individually admissible and yields 94 bytes at
  the exact tab cap. The old caller could consequently manufacture
  `EncodedQuota { projected_upper_bound: 94, maximum: 100 }`.
- **Structural correction:** revision `4022f395e` cycles removed lineages in
  deterministic reverse-removal order until a complete pass makes no
  progress. The terminal pass proves every returned rejection is individually
  inadmissible against the final exact normalized/physical/count projection;
  the caller independently aborts if that invariant is ever violated. Forced
  candidate-free schema/recovery publication now participates in the physical
  byte check. Empty/nonempty JSON separator interactions are evaluated against
  the live projection rather than inferred from fixed base-relative deltas.
- **Rejected optimization shortcut:** scalar threshold parking does not restore
  a near-linear worst-case. Alternating mixed-sign resource slack can wake all
  parked candidates repeatedly, and JSON separator deltas change at collection
  boundaries. A threshold index can accelerate ordinary cases, but it cannot
  replace exact retry or the terminal invariant without false sleeps.
- **Proof boundary:** two independent static audits found the fixed-point and
  forced-publication semantics sound and the focused regressions coherent.
  Exact revision `4022f395e` passed the GUI all-target check in strict remote
  job `j-29958204528001038` on `vmi1264463` and all-target Clippy with
  `-D warnings` in job `j-29958204528001039` on `vmi1293453`. The first
  focused command, job `j-29958204528001040`, selected the binary harness and
  ran zero tests, so it is retained under IS-N024 and is not proof. Corrected
  strict remote library-test job `j-29958204528001045` on `vmi1264463` ran the
  real module: 97 of 99 tests passed and two failed. Both failures exposed
  reversed test expectations rather than selector behavior: a byte-only
  rejection correctly classified as `Oversized`, while a candidate that would
  exceed both the byte and workspace caps correctly followed count-quota
  precedence and classified as `Quota`. Job `j-29958204528001049` then ran 102
  tests with four failures: those two stale assertions plus two ownership tests
  whose one-slot setup was legitimately repaired before the rejected overlay,
  exhausting their manually seeded maximum revision. After correcting the
  assertions, job `j-29958204528001056` ran 102 tests with only the two degraded
  journal fixtures failing. Revision `903fcfa5b` made those fixtures establish a
  healthy two-slot journal before injecting the maximum revision. Exact remote
  job `j-29958204528001062` on `vmi1264463` then passed all 102
  `window_state_persist::tests`, including fixed-point byte admission, exact tab
  order, unavailable-slot position, ownership components, quota receipts, and
  journal recovery; affected all-target check job `j-29958204528001061` also
  passed. Later exact-source GUI library job `j-29958204528001109` on
  `vmi1152480` passed all 241 library tests at revision
  `f183ed55761ad8aedf1c6e25389521fd3b78e88a`, including the expanded
  persistence suite. That broader green run is supporting deterministic
  evidence; it does not supply the literal maximum-scale artifacts below.
  Every superseded failure remains negative evidence. The exact backfill
  still has an adversarial quadratic candidate-trial bound under the
  cross-process lock, tracked by `.5.2.1`; no lock-hold performance claim is
  made, and the deterministic persistence suite does not prove the separate
  authoritative cross-process reorder protocol. It also does not yet exercise
  a real 4 MiB exact boundary, one all-maxima escaped composite, cross-process
  byte growth near the physical ceiling, mixed byte rejection through worker
  receipts and injected crash points, or a retained maximum encoded/admitted
  byte artifact. The current lowered-limit boundary and read-side oversized
  fixture are not substitutes for those literal acceptance cases.
- **Decision:** reject one-pass backfill and false quota receipts. Keep exact
  one-addition-maximal rejection now. Keep `.5.2` open for its physical-ceiling,
  concurrent-growth, crash-receipt, and retained-size evidence, and require
  retained worst-case timing under `.5.2.1` before promoting the selector as
  bounded for maximum-scale batches.
- **Primary retry condition:**
  > Claim bounded large-batch admission only after `.5.2.1` retains an adversarial maximum-lineage generator, candidate-trial counts, lock-hold distributions, strict remote correctness gates, and a mechanically near-linear selector without weakening exact rejection truth.

### IS-N028 — Serial-only abandoned-body discard is not protocol proof

- **Classification:** correctness and security rejection; kept narrow Phase-1
  uncompressed path
- **Bead:** `ft-interactive-systems-performance-4tenz.5.5.3.7`
- **Rejected candidate:** after validating only the frame length and response
  serial, drain an abandoned RPC body through a fixed scratch buffer without
  materializing it.
- **Rejected inference:** a current pending serial proves correlation to an RPC
  attempt, but it does not prove that the frame carries the response type that
  attempt expects. A wrong or unknown PDU identifier could consume the
  tombstone before the legitimate reply. Raw discard of a compressed body also
  skips zstd integrity and typed-payload decoding that a live response receives.
- **Premise correction:** the current codec has an encoded
  `MAX_PDU_SIZE` limit and a decompressed `MAX_PDU_SIZE + 1` read cap. It does
  not have an explicit compressed-to-decompressed ratio limit or an
  application-frame checksum, so an optimization cannot truthfully claim to
  preserve either nonexistent mechanism.
- **Structural correction:** revisions `5fdfeeb36` through `5250b6be7` bind
  an admitted request to its exact allowed response identifier (plus
  `ErrorResponse`), restrict fast-path eligibility to an ordinary closed waiter
  in the active generation, validate the full header before body selection,
  drain only uncompressed eligible bodies through a 64 KiB logical read window,
  revalidate after drainage, and retire the tombstone only after complete
  consumption. Revisions `0525fbb14` and `b9ed09099` preserve allocation-free
  live decode while satisfying the large-enum lint without boxing every live
  frame.
- **Proof boundary:** strict remote focused jobs
  `j-29958204528001069`, `j-29958204528001075`, and
  `j-29958204528001076` passed the ordinary tombstone, large interleaved
  successor-alignment, and delayed-handshake cases. Codec job
  `j-29958204528001082` passed 171 tests; client job
  `j-29958204528001087` passed 123 tests with one ignored plus the separate
  `PaneWriter` guard; codec all-target check job `j-29958204528001088`, client
  all-target Clippy job `j-29958204528001090`, and codec all-target Clippy job
  `j-29958204528001096` passed remotely. These jobs span exact source
  revisions and establish the narrow deterministic path, not one parent
  closure bundle.
- **Remaining rejection:** compressed abandoned replies still take the full
  materializing decoder. Raw uncompressed drainage intentionally skips typed
  varbincode and end-of-payload validation. `Vec::try_reserve_exact` and a
  64 KiB requested read window do not prove physical allocator capacity or RSS.
  Moreover, the typed zstd decoder can stop after a valid prefix without
  universally consuming decompressed EOF, so the current read cap is not a
  complete-frame, trailing-data, ratio, checksum, or decompression-window
  policy. Direct duplicate/unmatched and wrong-generation body-unread
  regressions, allocator/RSS evidence, target hardware, and latency remain
  open.
- **Decision:** retain the exact-type uncompressed Phase-1 fast path but keep
  the P0 parent open. Compressed integrity/amplification, complete-frame and
  checksum policy, bounded schema validation, and allocator/RSS/latency proof
  remain P0 children `.7.1`, `.7.1.1`, `.7.1.2`, and `.7.1.3`. Missing
  protections are not evidence supplied by this optimization.
- **Primary retry condition:**
  > Retry zero-materialization discard only after deterministic tests prove exact response-ident admission, wrong/unknown/future/generation-mismatched fail-closed behavior, compressed truncation/corruption rejection, decompressed-cap enforcement, fixed scratch bounds, stream resynchronization, and tombstone retirement only after complete successful consumption.

### IS-N029 — Individually accepted overlay mutations do not prove atomic ownership transfer

- **Classification:** correctness rejection; kept component-wide correction
- **Bead:** `ft-interactive-swarm-product-convergence-7xqz4.8.10.5.2`
- **Rejected candidate:** form stable-tab ownership components from only the
  overlay mutations that individually passed revision/CAS preflight.
- **Rejected inference:** an individually rejected mutation can still be one
  half of an ownership transition. In the retained counterexample, source
  window `S` at revision 1 owns slot `X`; destination `D` at revision 1 is
  empty. One batch validly advances `S` to revision 2 while releasing `X`, but
  submits `D` with the wrong base revision while advancing it to revision 3
  and acquiring `X`. Pruning `D` before component formation makes `S` a
  singleton, so the release commits even though the acquisition is rejected.
  That is a torn transfer, not a valid partial commit.
- **Structural correction:** revision `16698a0ea` forms ownership components from every
  requested overlay mutation's old/new slot symmetric difference. If any
  member already has an individual preflight failure, reject the entire
  connected component using a deterministic failure precedence and window-ID
  tie break. Only then run ownership uniqueness; only mutations that actually
  change state proceed to byte admission. A disjoint ownership component must
  remain independently admissible.
- **Proof boundary:** deterministic regressions cover the wrong-base destination
  counterexample and two conflicting claimants alongside one disjoint valid
  claimant in both enqueue orders. The superseded remote sequence is retained
  in IS-N027. Exact job `j-29958204528001062` on `vmi1264463` passed all 102
  `window_state_persist::tests` at source containing revisions `16698a0ea` and
  `903fcfa5b`; affected all-target check job `j-29958204528001061` also passed.
  These deterministic storage tests prove atomic local overlay admission and
  preserved ordering, not a mux-authoritative cross-process reorder protocol,
  native reopen behavior, visual correctness, or user-visible latency. Exact
  current-source `-D warnings` Clippy remains required before closing the
  producing bead.
- **Decision:** reject preflight-pruned component formation and retain the
  component-wide correction. Keep `.5.2` open until the exact current-source
  Clippy lane passes; keep the broader ordered-window protocol beads open until
  their independent capability, RPC, CAS, reconnect, and native proof gates
  are satisfied.
- **Primary retry condition:**
  > Promote overlay admission only when strict remote tests prove a preflight-rejected ownership participant rejects its entire connected component, disjoint components still commit with exact receipts, and the same exact source passes GUI all-target check and Clippy with `-D warnings`.

### IS-N030 — A repaint timer is not a bounded or retry-safe presentation protocol

- **Classification:** correctness and retry-amplification rejection;
  implementation not promoted
- **Bead:** `ft-interactive-systems-performance-4tenz.8.2.1`
- **Rejected candidate:** propagate draw errors, retain dirty generations, and
  attach one exponentially delayed invalidation ticket to every recoverable
  renderer failure while leaving geometry construction and backend ownership
  otherwise unchanged.
- **Rejected inference:** coalescing timer creation does not coalesce backend
  attempts. Ordinary `NeedRepaint` events can still run expensive geometry while
  a retry deadline is pending, so a busy mux can bypass the backoff at event
  rate. The first implementation also constructed a `glium::Frame` before
  fallible geometry: an early return dropped the live frame without calling
  `finish` or `set_finish`, which glium 0.36 specifies as a panic. WebGPU
  `Lost`/`Outdated` recovery called same-dimension `resize`, whose equality
  fast path performs no `surface.configure`; occluded windows rebuilt geometry
  on a perpetual 250 ms self-wake; permanent paint/draw failures retried
  forever; success could reactivate an old retry ticket and inherit its stale
  deadline; and animation scheduling removed live deadline state while old
  sleepers remained able to invalidate.
- **Additional ownership rejection:** `TermWindow::drop` took the OS window
  before its `render_state` and `webgpu` `Rc`s were released. That ordering is
  incompatible with the unsafe WebGPU surface contract that the raw window and
  display handles outlive the surface, and becomes especially dangerous if
  `Lost` recovery recreates a surface from those handles.
- **Structural correction under proof:** admit a paint only through a bounded
  recovery state machine; acquire and repair the WebGPU surface before geometry;
  distinguish same-size force-configuration from surface recreation; construct
  and consume every glium frame inside the presenter; use generation-checked
  retry and animation tickets; park occlusion without a self-wake; open a
  circuit after bounded transient failures; and release renderer/GPU ownership
  before the OS window. Dirty generations remain retained until a matching
  synchronous presentation handoff succeeds.
- **Proof boundary:** exact revision
  `f183ed55761ad8aedf1c6e25389521fd3b78e88a` passed strict clean/no-overlay
  GUI all-target check job `j-29958204528001108` on `vmi1167313` and all 241
  GUI library tests in job `j-29958204528001109` on `vmi1152480`. Those Linux
  deterministic gates support the committed state-machine code; exact-source
  warnings-denied Clippy and workspace formatting are still absent.
  `Queue::submit` and `SurfaceTexture::present` are
  synchronously infallible in the current wgpu API; success at that seam does
  not prove asynchronous GPU completion, scanout, visual appearance, latency,
  or target-hardware behavior. No native macOS, visual, M4/M5 latency, or GPU
  completion artifact is claimed from the remote compile/test bundle.
- **Decision:** reject timer-only retry and same-size resize recovery. Do not
  promote renderer damage settlement until the corrected admission, ownership,
  repair, and stale-ticket sequence tests pass for one exact source revision.
- **Primary retry condition:**
  > Promote only after deterministic tests prove no geometry before successful surface acquisition, every constructed glium frame is consumed once, external repaint storms cannot bypass cooldown, same-size Outdated performs real configuration, Lost replaces the surface without splitting Rc ownership, occlusion creates no self-wake, permanent failures open a circuit, stale retry and animation callbacks are inert, GPU resources drop before the OS window, and the same exact source passes strict remote focused tests, all-target check, and Clippy with `-D warnings`.

### IS-N031 — A bounded enqueue count is not an end-to-end RPC retention bound

- **Classification:** architecture and memory-accounting rejection; replacement
  design required before implementation
- **Bead:** `ft-interactive-systems-performance-4tenz.5.5.3.5.1`
- **Rejected candidate:** put a semaphore or bounded channel around the existing
  `ReaderMessage::SendPdu` enqueue and describe that capacity as bounded RPC
  memory.
- **Rejected inference:** the retained request can live before first poll, in the
  outbound FIFO, during serial assignment and encoding, in the pending map, or
  as an abandoned-reply tombstone. A caller can also drop a queued future while
  `async_channel` retains the physical PDU because that queue has no keyed
  removal. Releasing a permit in that state creates an uncharged backlog;
  retaining it violates immediate pre-wire cancellation release. After reply
  correlation is removed, a bounded completion channel or caller future can
  still own the full decoded PDU. Independent `Client` and `DirectMuxClient`
  instances additionally defeat any process-wide inference from one ledger.
- **Decision:** require one exact-generation, non-clone settlement lease spanning
  reservation through response drainage or terminal teardown, plus an O(1)
  keyed-cancelable retained FIFO whose node owns the PDU. Charge finite logical
  retained bytes explicitly and call them retained bytes, not RSS. Track
  post-correlation response ownership, direct-client bypasses, and any
  process-global parent budget as explicit closure work rather than laundering
  them through a per-client count.
- **Primary retry condition:**
  > Claim end-to-end bounded admission only after deterministic phase/cancellation/teardown permutations prove physical queued removal, settle-once accounting, exact tombstone drainage, bounded outbound encoding, no unclassified send path, and retained process-level evidence that separately accounts for every client and response-completion owner.

### IS-N032 — A retirement drain is not a fence against late readiness completion

- **Classification:** exact-generation correctness rejection; kept stale-result
  classifier and deterministic regression
- **Bead:** `ft-interactive-systems-performance-4tenz.5.5.3.8`
- **Rejected candidate:** let every asynchronous `FinishReadyReplay` completion
  invoke the current readiness coordinator after predecessor retirement has
  drained its ordinary pending work.
- **Rejected inference:** a G1 replay can finish after the retirement drain and
  after G2 has installed its successor coordinator. Applying that late result to
  current state can fail or mutate G2 even though the work was valid only for
  G1. Queue drainage does not linearize asynchronous completion that has already
  escaped the drained collection.
- **Structural correction:** revision `aa4c912e1` carries the replay generation
  in a typed completion, ignores only generations strictly older than the
  current reader before touching successor state, and fails closed on a future
  generation. The deterministic stale/future/current accounting regression
  passed strict remote job `j-29958204528001077`; affected client all-target
  check job `j-29958204528001078` also passed. Broader client job
  `j-29958204528001087` passed 123 tests with one ignored plus the separate
  `PaneWriter` guard, and all-target `-D warnings` Clippy job
  `j-29958204528001090` passed on remote worker `vmi1227854`. Those jobs bind
  different exact source revisions and therefore are supporting evidence, not
  one closure bundle. Server-side barriers, the consumer-commit race, and the
  topology-fence dependency remain formally open.
- **Decision:** retain the late-G1 schedule as negative evidence and require
  every asynchronous lifecycle completion to carry and revalidate the exact
  generation whose authority created it. Do not promote the parent admission
  lease while its topology-fence prerequisite is unresolved.
- **Primary retry condition:**
  > Close generation admission only when one current exact SHA passes deterministic disconnect barriers at every queue, serial, encode, flush, response, consumer-commit, and late-readiness boundary plus strict remote client/server tests, check, and Clippy, with the topology-fence dependency closed rather than bypassed.

### IS-N033 — In-mirror Git commands are not RCH clean-source identity proof

- **Classification:** verification-pipeline rejection; retained failed remote
  transcript
- **Bead:** `ft-x3esq`
- **Rejected candidate:** have the remote formatting integration test run
  `git rev-parse HEAD` and `git status --porcelain` inside an RCH
  `--clean-overlay --no-overlay` checkout, then treat those subprocesses as the
  source-identity and cleanliness authority.
- **Rejected inference:** an immutable clean source mirror need not be a Git
  working tree. RCH resolves the requested base commit outside the build root
  and materializes its archive into a fresh remote directory; the directory
  intentionally has no `.git`. Absence of in-mirror Git metadata therefore
  cannot distinguish a bad source transfer from the expected clean-mirror
  topology, and a caller-supplied SHA environment value cannot independently
  attest what source RCH materialized.
- **Observed negative:** strict remote job `j-29958204528001089` on worker
  `hz1` compiled revision `4fd3bf769` and then failed with
  `fatal: not a git repository` when the wrapper invoked its in-mirror Git
  check. Independent job `j-29958204528001092` on worker `ovh-b` reproduced
  the same failure after selecting and running the one intended test. Both
  failures are retained as evidence that the initial wrapper contract was
  incompatible with RCH clean mirrors; neither is reformulated as a formatting
  result.
- **Canary-oracle negative:** strict remote job `j-29958204528001099` on
  `ovh-b` selected the corrected one-test wrapper at revision `e957362fa`,
  recorded the pinned toolchain, and then exposed a second invalid assumption:
  its nightly rustfmt printed the expected stdin `--check` diff for valid but
  unformatted Rust while returning exit zero. The job correctly failed the
  wrapper assertion. It is evidence that an exit-code-only stdin canary can
  false-pass on the actual toolchain, not a formatting result.
  Superseded exact-source job `j-29958204528001100` independently reproduced
  that same old-wrapper failure on `vmi1264463`; it likewise ran one intended
  test and is retained only as cross-worker negative evidence for removing the
  invalid status oracle.
- **Exact-source workspace negative:** strict clean/no-overlay job
  `j-29958204528001104` on `vmi1227854` selected exactly one intended wrapper
  test at revision `f183ed55761ad8aedf1c6e25389521fd3b78e88a`; the retained
  outer transcript and inner canaries bound the source and pinned cargo,
  rustc, and rustfmt identities. The nested workspace-wide
  `cargo fmt --all -- --check` then exited 1 and emitted broad committed-source
  diffs. This is a valid fail-closed result after 11m13s: it validates that the
  hardened wrapper reaches the actual workspace check, and it exposes current
  formatting debt. Independent strict clean/no-overlay job
  `j-29958204528001105` reproduced the same outcome on `vmi1153651` at the
  same exact revision after 35m56s: exactly one wrapper test ran, the composite
  source/tool and rustfmt canaries passed, and the nested workspace check
  failed on committed formatting diffs. These are cross-worker fail-closed
  negatives, not formatting passes, and neither satisfies a passing worker
  lane of the acceptance contract.
- **Replacement under proof:** make source identity a composite protocol. The
  retained outer command and RCH transcript must bind one full 40-hex
  `--base`, `--clean-overlay`, `--no-overlay`, an explicitly requested and
  selected remote worker, no local/fallback marker, and remote completion. The
  integration test treats its SHA and source-mode environment values only as
  consistency labels, records tool identities, proves rustfmt stdin mode by
  reproducing canonical input byte-for-byte, rewriting unformatted input to
  exact expected canonical bytes, and rejecting malformed input with a
  diagnostic, binds nested `cargo fmt` to that same rustfmt binary, and emits a
  stable success sentinel only after the workspace-wide check passes.
- **Zero-test rejection:** remote exit zero is still insufficient. Libtest can
  succeed when an exact filter selects nothing, as retained under IS-N024.
  Acceptance additionally requires `running 1 test`, the exact named test
  ending in `... ok`, `1 passed`, `0 filtered out`, and the final sentinel in
  each complete log.
- **Decision:** reject all in-mirror Git subprocesses and all environment-only
  source claims. Keep `ft-x3esq` open until the corrected composite wrapper
  passes at one exact committed revision on two distinct pinned workers with
  separate target directories and complete retained transcripts.
- **Primary retry condition:**
  > Promote exact-revision remote formatting only after two distinct pinned workers run the one named wrapper test from the same full RCH base with clean/no-overlay transfer, each transcript proves one test and zero filtered tests, all canaries and workspace formatting pass, the success sentinel matches the requested revision, and neither transcript contains local fallback or an unverified scheduler substitution.

### IS-N034 — Producer direction and accepted compatibility are different censuses

- **Classification:** protocol-classification rejection; corrected before code
- **Bead:** `ft-interactive-systems-performance-4tenz.5.5.3.5.5.1`
- **Rejected candidate:** infer outbound server direction from the Rust suffix
  `Response`, or collapse every frame a compatibility client accepts into the
  canonical first-party producer contract.
- **Rejected inference:** neither naming nor inbound tolerance is producer
  authority. The current server emits `GetPaneRenderChangesResponse` with
  serial zero and correlates `GetPaneRenderChanges` with `LivenessResponse`.
  `DirectMuxClient` deliberately also accepts a correlated render payload from
  older or synthetic peers. That acceptance preserves compatibility; it does
  not authorize new first-party servers to resume the superseded response
  shape.
- **Structural correction:** retain two explicit censuses. The canonical
  producer registry contains 77 concrete variants with 48 client-request, 18
  server-response, and 15 server-unilateral permissions; its four duals are
  `SetPalette`, `TabTitleChanged`, `WindowTitleChanged`, and
  `RenameWorkspace`. The accepted-client compatibility surface additionally
  tolerates correlated render payloads, but raw compatibility fixtures remain
  outside the first-party outbound-admission chokepoints.
- **Decision:** reject both name-derived direction and compatibility laundering.
  Generate all three canonical directions explicitly, classify render changes
  as unilateral-only for first-party outbound planning, and keep the legacy
  correlated branch explicit in the client rather than widening server
  authority.
- **Primary retry condition:**
  > Change any outbound direction only with a complete producer, dispatcher, typed-response, serial-semantics, and rolling-compatibility transcript that proves the new direction is intentional; a type suffix or isolated callsite is never sufficient authority.

### IS-N035 — Single render polling correctness does not imply batch correctness

- **Classification:** live-protocol correctness rejection; implementation under
  proof
- **Bead:** `ft-interactive-systems-performance-4tenz.5.5.3.5.8`
- **Rejected candidate:** specialize batched `GetPaneRenderChanges` by accepting
  only correlated `GetPaneRenderChangesResponse`, even though the generic batch
  loop already stashes serial-zero sidebands.
- **Rejected inference:** reusing a generic response pipeline does not reuse the
  single-call semantic resolver. The live server sends the delta as a
  serial-zero sideband and returns correlated `LivenessResponse`; the old batch
  postprocessor therefore rejected the canonical response after correctly
  stashing its payload. Correlated-response mocks hid the mismatch.
- **Rejected first correction:** feeding generic ordered batch responses through
  the single-pane resolver fixed the happy-path type mismatch but preserved
  deeper defects. Full serial-zero deltas remained staged until the entire
  generic batch ended, so batch-induced retention scaled with total panes even
  at depth one. Timeout, cancellation, read failure, or retention failure could
  leave issued serials outstanding. Duplicate pane IDs made sideband ownership
  ambiguous, and wrong/dead liveness or a wrong-pane legacy correlated payload
  could return without invalidating stale target state.
- **Structural correction under proof:** use a specialized ambient/explicit-Cx
  state machine. Reject duplicate panes before transport; own every issued
  serial in a drop guard; consume and resolve each sideband-plus-liveness pair
  as its correlated response arrives; stop admission after the first semantic
  failure while draining already-issued serials; and bind every canonical or
  legacy payload to its requested pane. Fully drained semantic errors clear all
  target state but preserve unrelated state and connection reuse. Any ambiguous
  post-write abandonment poisons the connection and releases every retained
  direct-client collection before a later operation can write.
- **Exact-source correction evidence:** revision `aa28390ef` retained that
  state machine after two static reviews found no concrete semantic blocker,
  but strict-remote all-target check `j-29958204528001128` reached the new code
  on `vmi1227854` and failed with six `E0282`/`E0283` type-inference errors in
  checked-retention folds. Revision `97f7cee14` supplies the four explicit
  `DirectMuxError` result types; exact-source retry
  `j-29958204528001135` is active. Older focused/Clippy jobs target the rejected
  revision and cannot qualify the correction. Further focused admissions failed
  closed under `RCH-I005` or `RCH-I002`; no local result is substituted.
- **Decision:** reject response-type matching duplicated outside the canonical
  render resolver and reject generic end-of-batch postprocessing for this
  sideband protocol. Keep the bead open until focused failure/cancellation/
  depth-bound tests, affected full tests, check, formatting, and Clippy pass
  remotely on one exact committed source.
- **Primary retry condition:**
  > Promote batched render polling only after ambient and explicit-Cx tests prove interleaved serial-zero deltas plus correlated liveness, out-of-order wire delivery, exact pane binding and caller order, retained-byte settlement, wrong-pane and missing-delta failure, and explicit legacy correlated compatibility.

### IS-N036 — A rejected server-unilateral PDU must not spoof client activity

- **Classification:** request-validation ordering rejection; correction under
  proof
- **Bead:** `ft-interactive-systems-performance-4tenz.5.5.3.5.5.1`
- **Rejected candidate:** include `SetClipboard` in `Pdu::is_user_input` even
  though the server accepts it only as a server-to-client unilateral.
- **Rejected inference:** a user-visible clipboard action is not necessarily a
  valid client request. `SessionHandler::process_one` records client activity
  before its exhaustive request match, so a client-supplied forbidden
  `SetClipboard` could refresh activity and only afterward receive an error.
- **Structural correction under proof:** revision
  `7743e0dde2a044955cee8c663d350dfb545df74b` removes the unilateral from
  client-input classification and adds an exact full-dispatch regression. Its
  strict clean/no-overlay codec job `j-29958204528001117` passed the one named
  classification test on `vmi1293453`. Focused server job
  `j-29958204528001118` then reached compilation on `vmi1227854` and exposed a
  test-only unresolved crate path for `ClipboardSelection`; it did not execute
  the regression. Combined all-target job `j-29958204528001119` independently
  reached the same exact compile failure on `vmi1264463`; it is not a check
  pass. Revision `3e5cb8377` corrects that reference to the linked
  `wezterm_term` crate, and exact focused retry `j-29958204528001124` is active
  on `vmi1167313`. Earlier server/check/Clippy admission attempts failed closed
  under `RCH-I005`, `RCH-I002`, `RCH-I003`, or `RCH-I001`. No local result is
  substituted. The
  generated route matrix must
  subsequently validate `ClientRequest` before any activity accounting so a
  future classification mistake cannot recreate the ordering defect.
- **Decision:** reject semantic-name inference before request validation. Route
  authority must precede activity, mutation, allocation, and response work.
- **Primary retry condition:**
  > Promote client-input activity accounting only after every canonical client-request variant is admitted before accounting, every response/unilateral/Invalid variant is rejected without refreshing activity, and the exhaustive route census and server regression pass on one exact remote source.

### IS-N037 — Dropping a direct-client future is not request settlement

- **Classification:** cancellation and correlation-lifecycle rejection; broader
  transport correction required
- **Bead:** `ft-interactive-systems-performance-4tenz.5.5.3.5.9`
- **Rejected candidate:** let an outer timeout or caller drop cancel a
  `DirectMuxClient` single or generic batch future after requests have been
  written, then reuse the same connection without an explicit drain or terminal
  transition.
- **Rejected inference:** Rust future drop releases the exclusive client borrow;
  it does not retire serials, remove retained responses, consume late frames, or
  prove that a timed-out write transferred zero bytes. The current direct client
  can therefore retain abandoned serials and buffered protocol state after
  `send_request`, `batch`, their explicit-Cx siblings, or an intermediate
  retention failure returns early. A later operation may consume leaked
  capacity or encounter a frame produced under abandoned authority.
- **Narrow correction under proof:** the specialized render-batch sibling
  `ft-interactive-systems-performance-4tenz.5.5.3.5.8` will own its issued
  serials in a drop guard. Before transport, cancellation remains reusable;
  after an ambiguous write or abandoned read, guard drop poisons the connection,
  releases all retained direct-client state, and makes later writes fail closed.
  That narrow correction does not settle ordinary single requests or generic
  PDU batches.
- **Decision:** reject borrow release, timeout return, and collection clearing as
  transport completion fences. Every direct-client operation needs one
  generation-bound settlement protocol that either drains its exact replies to
  a proved fence or makes the stream terminal before releasing retained state.
- **Primary retry condition:**
  > Promote reusable direct-client cancellation only after deterministic ambient and explicit-Cx schedules cover future drop, timeout, cancellation, EOF, partial-write ambiguity, out-of-order replies, and retention failure for singles and generic batches, proving exact settle-once drainage or a zero-retention terminal transition before any later operation can allocate a serial or write.

### IS-N038 — Bounded render-sideband retention can still be quadratic

- **Classification:** hot-path complexity rejection; indexed-retention follow-up
  required
- **Bead:** `ft-interactive-systems-performance-4tenz.5.5.3.5.8.1`
- **Rejected candidate:** retain serial-zero render deltas in one bounded
  `VecDeque`, then linearly find and indexed-remove the matching pane whenever
  a correlated liveness response arrives. Independently, retain issued
  `(request_index, serial)` pairs in another `VecDeque` and use the same linear
  search plus indexed removal for every correlated response.
- **Rejected inference:** a count and byte cap bounds memory, not work. With
  reversed or adversarial interleaving, resolving D pending pane deltas performs
  repeated linear searches and deque shifts in both queues, producing O(D
  squared) work in the render polling path even if only one is repaired. The
  current defaults permit hundreds of outstanding and pending entries, so this
  cannot be dismissed as a tiny fixed list.
- **Required correction:** use pane-keyed deterministic FIFO retention and
  serial-keyed request correlation, or separate exact indexes, so every match
  and removal is O(1) expected or O(log D). Preserve repeated-pane arrival
  semantics, caller-order output, aggregate count/byte/request caps, unrelated
  response stashing, connection identity, targeted invalidation, and
  fail-closed accounting.
- **Decision:** retain the specialized batch state-machine correction for its
  protocol and cancellation invariants, but make no maximum-scale complexity
  claim from bounded `VecDeque` storage alone. Keep the indexed-retention child
  open until adversarial operation counts or retained timing rule out quadratic
  behavior.
- **Primary retry condition:**
  > Promote large-session batch correlation only after maximum-admitted reversed and repeated-pane workloads preserve exact protocol results and byte/request accounting while retained operation counts or timing demonstrate non-quadratic sideband lookup, serial correlation, removal, and targeted invalidation.

### IS-N039 — A fixed snapshot cap is a pane-count failure cliff, not a scale policy

- **Classification:** operating-envelope and recovery-loop rejection; explicit
  resync design required
- **Bead:** `ft-interactive-systems-performance-4tenz.5.5.3.5.8.2`
- **Rejected candidate:** retain one full idle render snapshot for every pane,
  fail insertion after 512 distinct panes, and rely on ordinary retry,
  reconnect, or sequential fallback.
- **Rejected inference:** a hard memory cap does not make the supported pane
  envelope graceful. Canonical sideband-plus-liveness polling needs retained
  pane metadata when an unchanged pane returns liveness without a new delta.
  Once distinct live panes exceed the configured snapshot count, the same
  deterministic insertion can fail on every retry; sequential fallback does
  not reduce the number of live pane snapshots required.
- **Required correction:** define a compact snapshot and target-derived budget,
  plus explicit eviction/resynchronization semantics that never answer
  liveness from absent or stale authority. A force-full response or generation
  marker belongs in the protocol if exact bounded recovery cannot be expressed
  client-side.
- **Decision:** make no claim that the current direct mux render path supports
  more than 512 simultaneously retained pane snapshots. Raising the constant
  without RSS, latency, byte-accounting, resync, and target-class evidence is
  not an accepted correction.
- **Primary retry condition:**
  > Promote a large-pane operating envelope only after repeated polls above 512 panes prove exact unchanged-pane results, bounded eviction or resync, no futile reconnect loop, exact count and byte accounting, and retained RSS and latency evidence on the declared Apple-silicon and high-core-count AMD target classes.

### IS-N040 — A typed render delta should not traverse the codec three times

- **Classification:** allocation and codec-churn rejection; batch-local pairing
  optimization required
- **Bead:** `ft-interactive-systems-performance-4tenz.5.5.3.5.8.3`
- **Rejected candidate:** receive a typed serial-zero render delta, serialize it
  once as the pending full response and again as an idle snapshot, then locate
  and deserialize the full response when correlated liveness arrives.
- **Rejected inference:** exact byte-bounded retention is not automatically an
  efficient representation for immediately consumed in-flight data. In the
  canonical batch path, the guard already knows the issued pane set and depth;
  routing owned sidebands through the global serialized queue adds allocation,
  codec calls, byte copies, and the queue lookup rejected in IS-N038.
- **Required correction:** pair owned sidebands with their issued requests in a
  depth-bounded typed structure, return the typed payload directly, and publish
  only the compact persistent snapshot. Preserve globally bounded accounting
  for unrelated sidebands and every cancellation/error cleanup invariant.
- **Decision:** keep serialization as the authority for globally retained
  unowned traffic, but make no optimal-render-path claim while immediately
  consumed owned deltas still encode twice and decode once.
- **Primary retry condition:**
  > Promote the batch-local fast path only after allocation, serialized-byte, and codec-call counters prove zero full-payload round trips for owned canonical sidebands at depths 1, 32, and 256 while adversarial protocol tests preserve exact ordering, bounds, cleanup, and unrelated-sideband behavior.

### IS-N041 — Complete-frame decoding can still be quadratic after bytes arrive

- **Classification:** inbound codec complexity rejection; streaming-buffer
  migration required
- **Bead:** `ft-interactive-systems-performance-4tenz.5.5.3.5.10`
- **Rejected candidate:** after decoding one frame from a `Vec`, shift every
  remaining byte to offset zero with `copy_within` and repeat for each
  coalesced frame.
- **Rejected inference:** bounded frame size and safe overlapping-copy semantics
  do not bound aggregate decoder work. A burst of N small complete frames can
  repeatedly move nearly the whole suffix, producing quadratic byte movement
  before dispatch. Pipelined render sideband/liveness traffic makes that
  pattern relevant to the focused-input and resize path.
- **Required correction:** give the decoder an owned buffer with a checked unread
  cursor and explicit amortized compaction/reclamation policy. Preserve exact
  residual bytes and nonconsumption on malformed complete frames; do not trade
  byte shifting for unbounded dead-prefix capacity.
- **Decision:** the existing overlap-safe `copy_within` remains memory-safe, but
  it is rejected as a large-burst performance architecture. No decoder
  complexity claim follows from frame caps alone.
- **Primary retry condition:**
  > Promote streamed mux decoding only after arbitrarily fragmented and coalesced oracle tests preserve exact PDUs, errors, and residual bytes, while 32, 256, and 4096-frame bursts retain byte-move and allocation evidence demonstrating amortized linear work under a bounded-capacity policy.

### IS-N042 — A per-attempt timeout is not an interactive operation deadline

- **Classification:** tail-latency and retry-amplification rejection; shared
  deadline required
- **Bead:** `ft-interactive-systems-performance-4tenz.5.5.3.5.8.4`
- **Rejected candidate:** give each pipelined attempt the complete configured
  timeout, then start a new recovery loop for sequential fallback with the same
  complete timeout.
- **Rejected inference:** a five-second `pipeline_timeout` does not bound a
  public pool call when retry and fallback each restart it. With the default two
  recovery attempts, one call can consume roughly four timeout windows plus
  connection and backoff time before returning. A caller-provided `Cx` only
  supplies an end-to-end bound when that caller already carries a tighter
  deadline; the ambient entry point does not create one.
- **Required correction:** establish one absolute operation deadline at public
  admission and pass checked remaining budget through pool acquisition,
  connect, transport attempt, backoff, and any permitted fallback. Do not start
  work that cannot fit the remaining minimum budget.
- **Decision:** reject nominal per-attempt timeout values as interactive tail
  guarantees. Retry success rate and eventual completion cannot substitute for
  bounded input/render response time.
- **Primary retry condition:**
  > Promote render-poll timeout behavior only after deterministic ambient and explicit-Cx schedules prove acquisition, connect, retry, backoff, and fallback share one absolute budget, total elapsed stays within a frozen tolerance, and phase telemetry explains every exhausted or skipped attempt.

### IS-N043 — A correct unused batch API is not a production performance optimization

- **Classification:** production-topology and adoption rejection; live-path
  integration required
- **Bead:** `ft-interactive-systems-performance-4tenz.5.5.3.5.8.5`
- **Rejected candidate:** implement a bounded, cancellation-safe batched render
  request API, but leave the supported capture runtime constructing and polling
  an independent `DirectMuxClient` subscription for each pane.
- **Rejected inference:** protocol correctness, pipelining, and microbenchmarks
  in a library API do not reduce production socket count, task count, timer
  wakeups, CPU, RSS, or input-to-visible latency when the live call graph never
  invokes that API. Static source inspection found no production batch caller;
  `run_vendored_streaming_capture` constructs an individual direct client,
  `subscribe_pane_output_with_inherited_cx` starts an individual subscription,
  and `run_subscription_loop` polls one pane at the active or idle cadence.
- **Required correction:** move the live capture path to a domain/session-scoped
  bounded batched subscription with dynamic membership, generation identity,
  explicit fairness for focused and control traffic, adaptive cadence without
  timer herds, bounded queues and retained bytes, and deterministic
  gap/resynchronization semantics. Keep the connection set deliberately bounded
  rather than merely hiding per-pane clients behind another abstraction.
- **Decision:** retain the batch state machine as correctness infrastructure,
  but make no user-visible or resource-efficiency claim until production uses
  it. Static call-graph adoption is necessary but still insufficient for target
  promotion.
- **Primary retry condition:**
  > Promote batched render polling only after the supported production path has no per-pane socket/task/timer topology and retained q2/q20/q50/q200/above-512 target-class A/B artifacts demonstrate bounded connections, wakeups, CPU, RSS, fairness, exact deltas and gaps, and improved keypress plus resize/zoom tails on recent Apple silicon and high-core-count AMD over LAN.

### IS-N044 — Two recovery classifiers cannot both be retry authority

- **Classification:** divergent-control-policy rejection; canonical classifier
  required
- **Bead:** `ft-interactive-systems-performance-4tenz.5.5.3.5.11`
- **Rejected candidate:** independently classify `DirectMuxError` in its
  inherent `protocol_error_kind` method and again in
  `protocol_recovery::classify_mux_error`, then allow callers to use either.
- **Rejected inference:** matching enum names and three similarly named buckets
  does not make the policies equivalent. The two live tables disagree for
  timeouts, remote errors, socket discovery, and I/O error kinds, so the same
  failure can trigger reconnect, in-place retry, or terminal handling depending
  on the call path.
- **Required correction:** establish one exhaustive authority, including
  transport-boundary and cancellation context where needed, and route pool
  recovery, render fallback, circuit breaking, and telemetry through it. A new
  error variant must fail compilation or an exhaustive contract test until its
  decision is explicit.
- **Decision:** reject retry/fallback counts and tail guarantees produced under
  divergent classifiers. More retries are not resilience when the connection
  state and operation deadline do not authorize them.
- **Primary retry condition:**
  > Promote recovery behavior only after every DirectMuxError and representative I/O kind has one shared decision, cancellation never retries, ambiguous transport failures discard the connection, permanent failures stop, and deterministic schedules prove all consumers obey the same bounded deadline and telemetry outcome.

### IS-N045 — A transport-local pane number is not global capture identity

- **Classification:** sharding and generation-identity rejection; correctness
  repair required
- **Bead:** `ft-interactive-systems-performance-4tenz.5.5.3.5.8.5.1`
- **Rejected candidate:** key streaming tasks only by a packed global pane ID,
  discard `PaneEntry.generation`, decode that ID to a shard-local pane number
  for polling, and forward the resulting local-numbered `PaneDelta` directly
  into the global capture bridge.
- **Rejected inference:** a pane number is not self-identifying across shards or
  generations. Two shards can both own local pane 1, while a respawn can reuse
  one global number with new authority. `StreamingBridge` uses the delta's pane
  field to construct captured segments, so failing to remap at the adapter
  boundary can collide or misroute persisted output; failing to fence a new
  generation can admit stale state or late output.
- **Required correction:** carry one typed identity containing global pane,
  shard/socket, transport-local pane, and generation. Remap local events exactly
  once, and fence teardown, snapshots, sequence state, gaps, and new admission
  by generation.
- **Structural partial under proof:** revision `d5e8b04ee` carries the typed
  identity through task ownership and transport adaptation, remaps every delta
  variant before `StreamingBridge`, replaces same-ID tasks on generation
  change, and rejects stale generation/token exits. It deliberately does not
  claim a persistence fence: `CaptureEvent` still discards generation and task
  token after enqueue, so already-buffered old-generation output remains
  indistinguishable downstream. Fresh strict-remote focused/check/Clippy
  admission attempts failed closed under `RCH-I002`; no local Cargo output is
  substituted.
- **Decision:** reject sharded streaming and same-ID restart correctness claims
  while either identity dimension is implicit or discarded.
- **Primary retry condition:**
  > Promote capture identity only after equal local IDs on distinct shards persist under distinct global IDs and a same-global-ID generation transition proves old output cannot cross the fence, with explicit gap/resync and exact teardown evidence.

### IS-N046 — Updating a config snapshot does not reconfigure live pollers

- **Classification:** hot-reload effectiveness rejection; live versioned
  configuration required
- **Bead:** `ft-interactive-systems-performance-4tenz.5.5.3.5.8.5.2`
- **Rejected candidate:** update the capture loop's
  `vendored_subscription_config`, but leave every running subscription holding
  the immutable clone it received at spawn time.
- **Rejected inference:** changing the value used for future tasks does not
  change active/idle cadence, channel capacity, or load for a large ongoing
  session whose panes do not churn. The operator can observe accepted config
  while the actual timer and memory behavior remains old indefinitely.
- **Required correction:** deliver a versioned configuration to the live
  coordinator, or perform a fenced single-producer restart for fields that
  cannot change in place. Preserve pane/generation identity, explicit gaps,
  bounded resources, and one authorized timer/poller per member.
- **Decision:** reject configuration and resource-envelope claims derived from
  the latest control-plane value until applied-generation and lag evidence
  proves the data plane consumed it.
- **Primary retry condition:**
  > Promote hot-reloaded streaming controls only after deterministic virtual-time schedules prove faster and slower cadence plus capacity changes reach every existing pane within a bound, with no overlapping pollers, silent loss, timer multiplication, or generation ambiguity.

### IS-N047 — One failed pane must not erase a healthy fleet poll

- **Classification:** fault-isolation and availability rejection; per-pane
  outcome contract required
- **Bead:** `ft-interactive-systems-performance-4tenz.5.5.3.5.8.5.3`
- **Rejected candidate:** use an all-or-nothing finite batch API unchanged as a
  long-lived q50/q200 coordinator, where the first pane semantic error stops
  admission, invalidates all targets, and returns no healthy results.
- **Rejected inference:** draining an aligned connection safely is not the same
  as isolating member failure. Pane closure, removal, and resynchronization are
  routine in an ongoing session; turning one such event into whole-batch retry
  or fallback amplifies tail latency and can starve healthy panes.
- **Required correction:** return typed per-pane outcomes and continue bounded
  admission for target-local unavailable/resync states. Reserve connection-wide
  failure for correlation, direction, codec, frame, accounting, cancellation,
  deadline, or transport ambiguity that actually invalidates shared authority.
- **Decision:** reject fleet availability, fairness, and poll-age claims from an
  all-or-nothing coordinator contract, even when the underlying finite batch is
  protocol-correct.
- **Primary retry condition:**
  > Promote long-lived batched capture only after failures in the first, middle, and last member of q2/q20/q200 preserve an explicit outcome for every healthy pane, bound poll age and retry work, and prove transport-wide faults still terminate the shared connection safely.

### IS-N048 — A private bridge cursor cannot converge storage sequence authority

- **Classification:** sequence and lifecycle-authority rejection; shared cursor
  owner required
- **Bead:** `ft-interactive-systems-performance-4tenz.5.5.3.5.8.5.4`
- **Rejected candidate:** create a fresh `StreamingBridge` and private
  `StreamIngester` cursor at each subscription start while discovery and
  persistence maintain a separate storage-backed runtime cursor. On persistence
  mismatch, repair only the runtime cursor. Also translate every subscription
  `Ended` event into pane closure regardless of why transport ended.
- **Rejected inference:** a downstream correction cannot repair a producer
  counter it does not own or update. After nonzero history or reconnect, the
  private bridge can keep proposing stale sequence numbers and produce a
  discontinuity gap on every event. Likewise, socket disconnect, cancellation,
  and reconfiguration are not authoritative pane-closure events.
- **Required correction:** assign capture sequence through one
  generation-bound, storage-initialized cursor authority with explicit
  persistence feedback. Preserve it across transport reconnects or emit one
  bounded gap/resync before convergence, and separate transport lifecycle from
  authoritative pane removal.
- **Decision:** reject monotonic-capture, gap-rate, reconnect, and pane-lifecycle
  claims while bridge and persistence cursors can diverge or transport teardown
  can masquerade as pane closure.
- **Primary retry condition:**
  > Promote streaming continuity only after nonzero-history reconnect and polling fallback/return schedules converge with monotonic accepted segments, at most one gap per real discontinuity, no false PaneClosed event, and isolation across panes, shards, and generations.

### IS-N049 — A spawned stream task is not capture coverage

- **Classification:** readiness and source-authority rejection; transition fence
  required
- **Bead:** `ft-interactive-systems-performance-4tenz.5.5.3.5.8.5.5`
- **Rejected candidate:** remove a pane from polling fallback as soon as a stream
  task is inserted, before connect and handshake finish, while admitting late
  results from polling, native, or a replaced stream without a source epoch.
- **Rejected inference:** task existence neither proves a ready data path nor
  revokes already-issued work. This creates blind windows on slow connect and
  duplicate or stale admission during source transitions.
- **Required correction:** use an explicit streaming-ready commit plus one
  pane-generation source epoch. Keep polling authoritative until commit and
  reject every old-epoch result before persistence.
- **Decision:** reject continuous-coverage and no-duplicate claims until both
  directions of the streaming/fallback transition are fenced.
- **Primary retry condition:**
  > Promote source switching only after stalled/failed connect and in-flight old-source schedules retain uninterrupted coverage, one committed producer authority, and zero post-fence stale persistence.

### IS-N050 — A full queue cannot reliably carry its own loss notification

- **Classification:** backpressure and gap-delivery rejection; sticky recovery
  state required
- **Bead:** `ft-interactive-systems-performance-4tenz.5.5.3.5.8.5.6`
- **Rejected candidate:** after output `try_send` fails on a full bounded channel,
  immediately `try_send` a gap into that same channel and forget both failures.
- **Rejected inference:** best-effort gap enqueue does not satisfy no-silent-loss
  semantics. The condition that rejected the delta normally rejects the gap too.
- **Required correction:** retain a bounded sticky `GapPending`/`ResyncPending`
  state until one durable gap and authoritative resync complete; reserve
  downstream capacity before advancing the baseline where possible.
- **Decision:** reject losslessness and gap-rate evidence from the current
  double-`try_send` path.
- **Primary retry condition:**
  > Promote bounded streaming delivery only after capacity-one schedules prove every dropped baseline-advancing result yields exactly one durable gap and bounded full resync before later deltas resume.

### IS-N051 — Terminal mutation sequence and bonus lines are not delivery authority

- **Classification:** false-gap and inexact-delta rejection; explicit wire
  baseline required
- **Bead:** `ft-interactive-systems-performance-4tenz.5.5.3.5.8.2.1`
- **Rejected candidate:** treat terminal mutation `seqno` as a contiguous
  transport-delivery counter and treat render bonus lines as exact appended
  text.
- **Rejected inference:** several legitimate terminal mutations between polls
  can jump `seqno` without any missed response, while bonus lines are explicitly
  a best-effort render projection. `previous + 1` can also overflow.
- **Required correction:** carry connection/pane generation and an explicit
  delivery baseline with bounded exact delta, no-change, too-old, generation
  change, and force-full-resync outcomes. Exact capture text/state must not rely
  on bonus-line reconstruction.
- **Decision:** reject current gap counts, exact-delta claims, and resync
  convergence as protocol evidence.
- **Primary retry condition:**
  > Promote delivery continuity only after mutation-sequence jumps produce no false gap, lost baselines produce one justified gap, checked exhaustion never wraps, and exact bounded full resync converges text and terminal state.

### IS-N052 — Per-pane reconnect demand must not become a backend storm

- **Classification:** retry-herd and resource-amplification rejection;
  backend-level backoff required
- **Bead:** `ft-interactive-systems-performance-4tenz.5.5.3.5.8.5.7`
- **Rejected candidate:** retry every failed pane subscription on each discovery
  sync with independent connect and handshake work.
- **Rejected inference:** a bounded sync interval does not bound aggregate work.
  Hundreds of panes sharing one failed socket can synchronize attempts, logs,
  wakeups, and fallback churn.
- **Required correction:** coalesce demand into one checked backend reconnect
  state machine with deadline, capped backoff/jitter, circuit probing,
  cancellation, and connection-generation resync.
- **Decision:** reject recovery-rate and resource-envelope claims from per-pane
  retries.
- **Primary retry condition:**
  > Promote reconnect behavior only after q200 unavailable/flapping schedules perform at most one admitted backend probe per window, retain polling coverage, and recover through one generation change without a member herd.

### IS-N053 — Last-writer health is not fleet telemetry

- **Classification:** observability and evidence-authority rejection; aggregate
  ownership required
- **Bead:** `ft-interactive-systems-performance-4tenz.5.5.3.5.8.5.8`
- **Rejected candidate:** let each per-pane bridge overwrite one process-global
  `StreamingHealth` snapshot.
- **Rejected inference:** the most recent pane update is not an aggregate of
  active panes, events, gaps, dirty work, fallbacks, errors, or resource state.
- **Required correction:** coordinator/runtime-owned monotonic counters, exact
  gauges, and bounded histograms with conservation and reset semantics and no
  pane-cardinality labels.
- **Decision:** reject q50/q200 diagnosis and promotion decisions derived from
  the last-writer snapshot.
- **Primary retry condition:**
  > Promote streaming telemetry only after concurrent multi-backend conservation tests prove order-independent totals/gauges/histograms, bounded read cost and memory, and exact lifecycle reset behavior.

### IS-N054 — A per-frame cap is not a batch-result memory cap

- **Classification:** aggregate-memory rejection; incremental delivery required
- **Bead:** `ft-interactive-systems-performance-4tenz.5.5.3.5.8.3.1`
- **Rejected candidate:** cap each decoded frame but retain one full typed render
  response per target until returning a complete ordered batch vector.
- **Rejected inference:** pipeline depth bounds simultaneous requests, not the
  accumulated output vector. Legal large responses multiply by total pane
  count and can dominate transient RSS.
- **Required correction:** validate an aggregate count/byte envelope and deliver
  owned results incrementally or in bounded chunks with exact reservation,
  accounting, ordering identity, cancellation, and resync semantics.
- **Decision:** reject batch memory-envelope and >512 scale claims from a
  per-frame cap alone.
- **Primary retry condition:**
  > Promote batch result memory only after skewed near-limit q200/above-512 schedules keep peak live bytes within one frozen aggregate cap independent of total membership and downstream speed.

### IS-N055 — Full-map reconciliation is not a scale-neutral control plane

- **Classification:** membership-complexity rejection; delta-driven topology
  required
- **Bead:** `ft-interactive-systems-performance-4tenz.5.5.3.5.8.5.9`
- **Rejected candidate:** clone every observed pane on each sync, repeat one
  backend socket check per pane, and rebuild the full polling complement after
  each stream exit.
- **Rejected inference:** individual O(P) operations become harmless merely
  because P is bounded. Correlated q200 churn can repeat them per event and
  approach quadratic control work.
- **Required correction:** consume generation-bound membership deltas, group
  once by backend, update only affected state, and retain a bounded periodic
  full audit solely for repair.
- **Decision:** reject maximum-scale CPU/wakeup claims while hot-path membership
  work repeatedly scans the fleet.
- **Primary retry condition:**
  > Promote reconciliation only after operation/allocation counters demonstrate amortized work proportional to changed members plus affected backends under q200/above-512 correlated churn.

### IS-N056 — Hardware names and core counts are not tuning evidence

- **Classification:** target-profile and configuration-authority rejection;
  retained A/B required
- **Bead:** `ft-interactive-systems-performance-4tenz.5.5.3.5.8.5.10`
- **Rejected candidate:** hard-code larger pipelines or concurrency because a
  machine reports Apple M4/M5 or many AMD cores, without one validated global
  memory/deadline/fairness configuration and target artifacts.
- **Rejected inference:** more cores do not remove socket serialization,
  downstream backpressure, memory bandwidth, cache/NUMA effects, GPU/display
  pacing, or LAN tails. A marketing name is not a stable capability contract.
- **Required correction:** expose checked hot-reloadable coordinator budgets
  with portable defaults. Any target profile must use stable capabilities,
  exact build/config identity, retained workload A/B, and a safe fallback.
- **Decision:** reject hardware-specific superiority and operating-envelope
  claims from static heuristics.
- **Primary retry condition:**
  > Promote an Apple-silicon or high-core-count AMD profile only after exact q2/q20/q50/q200/above-512 target-class artifacts improve declared latency/resource metrics without correctness, fairness, visual, or memory regressions and portable fallback remains green.

### IS-N057 — Aborting a producer does not revoke capture already in shared queues

- **Classification:** generation-revocation and persistence-admission rejection;
  stamped source epoch required
- **Beads:** `ft-interactive-systems-performance-4tenz.5.5.3.5.8.5.1` and
  `ft-interactive-systems-performance-4tenz.5.5.3.5.8.5.5`
- **Rejected candidate:** bind one generation and unique token to each vendored
  stream task, abort the old handle when a same-ID generation appears, ignore
  stale exit notifications, and treat that as proof that no old output can
  cross the replacement fence.
- **Rejected inference:** handle abort prevents future producer progress but
  does not retract `CaptureEvent` values already accepted by the shared MPSC
  channel or relayed into the SPSC persistence ring. Those events carry only a
  `CapturedSegment`; generation, source epoch, and task token have been erased.
  Persistence therefore cannot distinguish an old-generation segment from the
  active generation after replacement.
- **Required correction:** carry a bounded stamped capture envelope through
  every queue and relay to one persistence admission check. Revoke the prior
  pane-generation/source epoch before new authority commits, reject every old
  envelope before storage, cursor, pattern, event, or metric side effects, and
  emit the one justified gap/resync transition required for convergence.
- **Structural partial retained:** revision `d5e8b04ee` still fixes global versus
  shard-local identity, generation-aware task replacement, mismatch handling,
  and stale-exit removal. Those improvements are kept without upgrading the
  stronger stale-output claim.
- **Decision:** reject generation-fenced persistence, exact restart, and
  no-duplicate claims from task ownership alone.
- **Primary retry condition:**
  > Promote generation replacement only after deterministic barriers enqueue old-source output on both sides of revocation, commit a new producer epoch, and prove every stale envelope is rejected before all persistence side effects while the new generation emits one bounded resync and converges exactly.

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
| Shipped GUI/mux code generation is penalized by the CLI size profile | `ft-interactive-systems-performance-4tenz.13` | exact-artifact `release` versus latency-profile A/B with input, resize/zoom, startup, resource, size, and target-class evidence |

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
