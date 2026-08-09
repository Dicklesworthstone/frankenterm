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
  not strand unrelated valid work. Revision `4022f395e` adds transaction-wide,
  serializer-grounded encoded-byte admission and exact lineage isolation;
  revision `939662b1e` adds literal-limit and all-maxima adversarial fixtures
  without weakening the final physical encoder guard.
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
  authoritative cross-process reorder protocol. Revision `939662b1e` now
  constructs the real 4 MiB conservative admission boundary, rejects its
  deterministic one-byte successor as `EncodedQuota`, and constructs one
  all-maxima composite spanning maximally escaped workspace keys, maximum-width
  bindings, overlays, remote slots, and tombstones. Exact-source strict-remote
  boundary job `j-29958204528001155` passed on `vmi1152480`, and GUI all-target
  check `j-29958204528001154` passed on `vmi1293453`; all-maxima job
  `j-29958204528001156` passed on `vmi1153651`. Revision `fde6e012b` adds a
  deterministic controlled-worker race at one byte below the ceiling: an
  intervening writer consumes that byte after the worker freezes a mixed
  batch, then the worker reloads under the file lock, commits the independent
  overlay lineage, rejects only the stale workspace growth, and reports the
  typed semantic failure to its real flush waiter. The same revision retries
  that partitioned batch across all five injected slot-write crash points and
  emits admitted-versus-physical byte evidence under `--nocapture`. Its focused
  exact-source job `j-29958204528001184`, GUI check
  `j-29958204528001186` remain active. GUI all-target Clippy
  `j-29958204528001187` passed on `vmi1149989` with warnings denied; revision
  `8024ee3ec` still needs exact-current Clippy because it adds test-process
  code in the same module.
  Workspace format proof `j-29958204528001185` failed on broad committed-tree
  rustfmt drift in unrelated shared client/server files and is tracked by
  `ft-teo0x`; it is not a formatting PASS for this work. Initial admission on
  `vmi1167313` failed closed under `RCH-I002`; two same-worker follow-ups failed
  closed under `RCH-I005`. No local result substitutes for those refusals. The
  conservative authority is exactly 4 MiB, while the
  hash-dependent physical encoding is only asserted to remain at or below that
  ceiling, not to equal it byte-for-byte. The new deterministic writer
  interposition exercises authority reload and file-lock admission but is not
  an operating-system process-scheduling or target-filesystem power-loss
  artifact. Revision `8024ee3ec` corrects that evidence boundary by replacing
  the intervening call with a dedicated remote test-helper process and a
  child-written revision marker that makes a zero-test filter fail closed.
  Focused job `j-29958204528001191` and GUI all-target check
  `j-29958204528001192` remain active, so literal process interposition is still
  under proof rather than credited. Target-filesystem power-loss remains an
  explicit nonclaim. Final-source proof remains pending, and the `.5.2.1`
  selector bound remains separate.
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
  `j-29958204528001135` passed the affected all-target check on `vmi1227854`.
  Older focused/Clippy jobs `j-29958204528001130` and
  `j-29958204528001129` target the rejected revision and failed with the same
  retained inference defect; they cannot qualify the correction. Corrected
  focused render job `j-29958204528001148` passed on `vmi1227854` at exact
  revision `97f7cee14`; that supports the specialized state-machine tests but
  does not qualify its active recovery/deadline successor. Corrected
  warnings-denied Clippy `j-29958204528001150` reached
  `97f7cee14` on `vmi1264463` but rejected its wildcard arm for the sole
  remaining `MuxPoolError::Pool(_)` variant under
  `clippy::match_wildcard_for_single_variants`. That real lint is corrected in
  the active recovery/deadline patch, but fresh exact-source Clippy remains
  required after commit. Further admissions failed closed under `RCH-I005` or
  `RCH-I002`; no local result is substituted.
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
  `wezterm_term` crate. Exact focused retry `j-29958204528001124` then passed
  the full-dispatch regression on `vmi1167313` (1 passed, 232 filtered): a
  rejected `SetClipboard` records zero activity while an accepted
  `WriteToPane` records one. Earlier server/check/Clippy admission attempts
  failed closed under `RCH-I005`, `RCH-I002`, `RCH-I003`, or `RCH-I001`; no
  local result is substituted. This focused pass does not supply the missing
  exhaustive route, affected all-target check, Clippy, or formatting proof.
  The generated route matrix must subsequently validate `ClientRequest` before
  any activity accounting so a future classification mistake cannot recreate
  the ordering defect.
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
- **Rejected structural partial:** revision `6c86687df` made one exhaustive
  `ProtocolErrorKind` table authoritative and routed the initial pool/fallback
  consumers through it. Exact-source strict-remote focused job
  `j-29958204528001144` passed its exhaustive-authority test, and affected
  all-target check `j-29958204528001145` passed. Those gates prove the partial
  compiles and its table test executes; they do not prove the policy is sound.
  Independent review found that the three labels still collapsed distinct
  retry and connection-disposition decisions, unstructured server
  `RemoteError` was retried and discarded even for deterministic failures,
  acquisition errors bypassed the state machine, subscription read timeout
  retried a poisoned client, cancellation was not a typed classifier input,
  and the non-Unix variant lacked the canonical surface.
- **Rejected integration candidate:** a second static pass found that
  `write_to_pane` and `send_paste` still used the replaying generic recovery
  loop, while spawn, split, write, and paste all exposed post-invocation
  failures to generic CLI fallback. A lost response could therefore execute a
  keypress, paste, spawn, or split once over direct mux and again through
  recovery or CLI. The same pass found that pipelined render retry and
  sequential fallback each reset their attempt budget and counters, bounding
  elapsed time but not work or traffic.
- **Correction under proof:** revision `3d69c22ae` replaces the single label
  with a total decision carrying independent error kind, retry authority,
  connection reuse/discard, and cancellation axes; routes acquisition,
  subscription, circuit, and fallback consumers through that decision;
  represents an invoked mutation with unknown outcome as a typed
  non-replayable error; routes every input/topology mutation through
  acquisition-retry-only execution; and binds retry plus fallback to one
  elapsed-time and logical-attempt budget. Its exact-source strict-remote
  `frankenterm-core` all-target check `j-29958204528001173` passed on `ovh-a`.
  Focused mutation no-replay job `j-29958204528001174` passed on
  `vmi1293453` (1/1, 0 failures), and shared render-attempt job
  `j-29958204528001175` passed on `ovh-b` (1/1, 0 failures).
  Canonical-decision job `j-29958204528001189` and later-source core Clippy
  remain active. Workspace format job `j-29958204528001185` is retained as a
  failure: the exact committed tree carries broad rustfmt drift across shared
  client/server files, tracked by `ft-teo0x`; it is not a formatting PASS for
  this lane. Full generic post-write settle-or-poison proof remains owned by
  `.5.9`, and the render deadline remains separately gated by `.8.4`.
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
  `ft-interactive-systems-performance-4tenz.5.5.3.5.8.5.5`, with native and
  replay-egress closure split into `.8.5.5.1` and `.8.5.5.2`
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
- **Resolved design boundary:** the non-optional stamp must use a checked
  runtime-monotonic pane incarnation rather than the saturating, restartable
  `PaneEntry.generation`, plus source kind and a checked source epoch. Producer
  admission must cover cursor/bridge mutation, replay egress, and enqueue;
  persistence must acquire an increment-and-recheck lease guard after dequeue
  and hold it through its complete asynchronous side-effect chain. Revocation
  closes the old lease to new guards and waits in-flight guards to zero before
  promotion. Relay-only validation races revocation, while persistence-only
  validation leaves stale producer-side cursor and replay effects.
- **Native extension:** concurrent native connections currently merge bare
  events, discard `Hello`, and coalesce only by pane ID. Connection epoch and
  sequence must be stamped before that merge, readiness must be explicit, and
  stale output, state, destroy, and user-variable events must pass the same
  authority gate. Connection loss or queue loss is not pane removal and must
  enter the sticky durable-gap/full-resync machine instead.
- **Structural partial retained:** revision `d5e8b04ee` still fixes global versus
  shard-local identity, generation-aware task replacement, mismatch handling,
  and stale-exit removal. Those improvements are kept without upgrading the
  stronger stale-output claim.
- **Decision:** reject generation-fenced persistence, exact restart, and
  no-duplicate claims from task ownership alone.
- **Primary retry condition:**
  > Promote generation replacement only after deterministic barriers enqueue old-source output on both sides of revocation, commit a new producer epoch, and prove every stale envelope is rejected before all persistence side effects while the new generation emits one bounded resync and converges exactly.

### IS-N058 — Reweighting after serialization is not an allocator or RSS bound

- **Classification:** pre-allocation admission rejection; capacity-limited writer
  required
- **Bead:** `ft-interactive-systems-performance-4tenz.5.5.3.5.5`
- **Structural candidate retained:** `a32a47834803f0c7949725a464664b7b9db3ca77`
  charges the encoded frame's retained capacity to the connection-wide outbound
  budget and fails the connection generation closed when reweighting exceeds
  the topology ceiling.
- **Rejected candidate:** serialize a typed PDU into a newly allocated `Vec`,
  then use its capacity to decide whether the allocation was admissible.
- **Rejected inference:** post-allocation accounting bounds retained ownership
  after the decision, but the allocator has already serviced the `Vec` growth.
  Serialization and compression can therefore create an instantaneous
  allocation peak before the budget rejects and releases the frame.
- **Required correction:** reserve a checked upper bound before allocation, or
  serialize through a capacity-limited writer whose growth consumes the exact
  connection reservation before each allocation. Preserve exact transfer and
  release across typed, encoded, compressed, deferred, partial-write, failure,
  cancellation, and teardown states.
- **Decision:** keep the retained-byte accounting, but reject allocator-envelope
  and RSS claims from post-allocation reweighting alone.
- **Primary retry condition:**
  > Promote the outbound allocation envelope only after q2/q20/q200/q4096 and oversized-frame barriers prove that every serializer and compressor allocation is pre-admitted, peak live capacity remains within one frozen connection budget, and every terminal path returns the reservation to zero.

### IS-N059 — Reserved control capacity is not control scheduling priority

- **Classification:** service-order rejection; class-aware fairness required
- **Bead:** `ft-interactive-systems-performance-4tenz.5.1`
- **Structural candidate retained:** `a32a47834803f0c7949725a464664b7b9db3ca77`
  reserves 64 connection slots for control traffic while bounding bulk
  admission independently.
- **Rejected candidate:** append admitted control and bulk frames to the same
  FIFO and infer low control latency because bulk cannot consume the reserved
  control slots.
- **Rejected inference:** admission headroom prevents control rejection, but a
  newly admitted control frame can still wait behind every older bulk frame.
  The reserve establishes capacity, not a service deadline, burst bound, or
  starvation theorem.
- **Required correction:** schedule explicit traffic classes with a bounded
  priority or deficit policy, an executable maximum bulk burst, per-class age
  telemetry, and a fairness rule that also prevents bulk starvation.
- **Decision:** keep the control reserve, but reject keypress/control latency
  claims until service order is independently bounded and measured.
- **Primary retry condition:**
  > Promote control-path scheduling only after deterministic saturated-bulk barriers and retained q2/q20/q200/q4096 traces prove the declared control service gap and bulk starvation bound from admission through socket progress.

### IS-N060 — Two pending-request ledgers cannot both own retirement

- **Classification:** split-authority rejection; one request-lineage owner
  required
- **Bead:** `ft-interactive-systems-performance-4tenz.5.5.3`
- **Rejected candidate:** add a server-dispatch pending-request ledger beside
  the client's authoritative `PendingReplies` serial-to-waiter map so both
  sides attempt to classify response, cancellation, and transport teardown.
- **Rejected inference:** duplicate bookkeeping does not strengthen exact-once
  retirement. It creates two independently mutable authorities that can
  diverge across cancellation, reconnect, serial reuse, or a response/teardown
  race, producing double retirement, an orphaned waiter, or an ABA match.
- **Required correction:** keep `PendingReplies` as the sole owner of client
  request admission and waiter retirement. Server dispatch may own only its
  generation-scoped response obligations, with explicit handoff at the wire
  boundary and no shadow serial authority.
- **Decision:** reject the duplicate ledger. Retain the client teardown
  regression that proves one live and one abandoned waiter settle exactly once
  under the existing authority.
- **Primary retry condition:**
  > Reconsider end-to-end request accounting only with one non-duplicated lineage capability whose admission, response, cancellation, teardown, reconnect, and serial-reuse transitions have a single linearization point and deterministic exact-once proofs.

### IS-N061 — A validated trace schema is not production latency evidence

- **Classification:** evidence-contract boundary; producer wiring and retained
  target measurement required
- **Bead:** `ft-interactive-systems-performance-4tenz.2.1`
- **Structural contracts retained:** `28f2729d4e38512beacf30050a5535c321235244`
  freezes Trace Contract v2, and
  `edf8e36b257cb8e9ff85276edee9852b738611db` rejects same-clock start
  regression and executes the committed Draft 2020-12 schema against positive
  and negative fixtures.
- **Rejected candidate:** treat successful DTO validation, schema conformance,
  synthetic K0-K13/R0-R25 completeness, or fixture round trips as evidence that
  production mux, terminal, renderer, display, or photon producers emit those
  stages with acceptable latency.
- **Rejected inference:** `InputSerial` remains dispatch acknowledgement and is
  not the trace ID, application echo, or photon authority. Wall-clock metadata
  and uncalibrated cross-host monotonic clocks remain non-subtractable. A
  complete software trace cannot promote display or photon claims without the
  contract's detector and calibration authority, and a schema-valid synthetic
  trace proves neither live producer wiring nor Apple-silicon, AMD, LAN, aged-
  session, or observer-effect performance.
- **Required correction:** wire each frozen stage at its named production
  boundary, retain sampling-loss and generation receipts, calibrate any
  cross-clock or photon measurement under the declared authority, and capture
  same-window target-class artifacts with correctness and observer-effect
  controls.
- **Decision:** keep the portable, fail-closed evidence contract while rejecting
  every live latency, hardware, scale, and user-visible responsiveness claim
  from contract or fixture proof alone.
- **Primary retry condition:**
  > Promote a latency claim only after exact-revision production producers emit the required complete loss-accounted trace on the declared target and workload, every interval uses one exact clock domain or retained calibration authority, and the corresponding correctness, visual, resource, and observer-effect gates pass.

### IS-N062 — A discarded atomic counter is not global replay order

- **Classification:** false-authority and hot-path-work rejection; structural
  correction retained without a latency claim
- **Bead:** `ft-interactive-systems-performance-4tenz.2.13.1`
- **Rejected candidate:** allocate `EgressEvent.global_sequence` from a relaxed
  atomic for every normal and overflow-gap capture, then discard it when the
  event becomes RecorderEvent v1.
- **Rejected inference:** each `TailerSupervisor` and `CaptureAdapter` created
  its own counter at zero, the sharing setter had no callers, and neither
  RecorderEvent v1, `event_id.v1`, nor `RecorderMergeKey` represented the
  value. The counter therefore supplied neither process-wide authority nor
  durable replay order while adding shared atomic work to the capture path.
- **Decision:** remove the transient counter, field, setter, and allocations;
  retain authoritative per-pane sequence exhaustion and Recorder v1's
  five-part merge key. This is a structural optimization only: no keypress,
  resize, zoom, Apple-silicon, AMD, or aged-session latency improvement is
  claimed without campaign measurement.
- **Primary retry condition:**
  > Add a global ordering identity only through a versioned durable schema, one authority shared by every producer, deterministic concurrent and multi-supervisor fixtures, and retained same-window performance evidence.

### IS-N063 — Atomic API modernization may not silently raise the workspace MSRV

- **Classification:** toolchain-contract rejection; explicit checked CAS retained
- **Beads:** `ft-interactive-systems-performance-4tenz.2.13` and
  `ft-interactive-systems-performance-4tenz.2.20`
- **Rejected candidate:** replace the pinned toolchain's deprecated
  `AtomicU64::fetch_update` calls with `AtomicU64::try_update` in recorder
  counters and the mux handler-ID allocator.
- **Negative evidence:** strict remote all-target Clippy job
  `j-29959181985382549` rejected the recorder candidate because
  `try_update` was stabilized in Rust 1.95 while the workspace contract is
  Rust 1.85. Reverting only to `fetch_update` would satisfy that minimum but
  retain a warning denied by the current pinned nightly. Neither choice is an
  admissible workspace-wide fix.
- **Decision:** commits `5b42730fb8747f458c2bd2b708666bf03d25e5c6`
  and `ac067dff432be76f43173ff7c0d4eba31437a05d` use explicit
  `compare_exchange_weak` retry loops with checked successor arithmetic. The
  mux allocator remains sticky at exhaustion; recorder identity allocation
  fails before reuse; diagnostic counters saturate; and deterministic
  contention/exhaustion tests cover the boundary. This is a compatibility and
  correctness repair, not target-hardware performance evidence.
- **Primary retry condition:**
  > Replace an atomic helper on a campaign hot path only after the candidate compiles warning-free at both the declared MSRV and pinned nightly, preserves exact overflow and memory-order semantics, and passes deterministic contention and exhaustion tests.

### IS-N064 — Version-window overlap without exhaustive PDU admission is unsafe

- **Classification:** unguarded rolling-interop rejection; exhaustive static
  registry and DirectMux generation-bound gates retained
- **Bead:** `ft-interactive-systems-performance-4tenz.5.5.3.5.12`
- **Rejected candidate:** replace DirectMux's strict codec-version equality
  check with `check_compat`, accept an overlapping window, and then continue
  emitting PDUs through call sites that do not retain the agreed dialect or
  enforce direction, serial role, and established capabilities.
- **Rejected inference:** overlap arithmetic establishes only that a common
  dialect exists. It does not make an unguarded v51 PDU legal on an agreed-v50
  connection, prevent a newer unilateral server family from reaching an older
  client, bind capability state to the current connection generation, or stop
  serial allocation and socket writes before a permanent incompatibility is
  discovered. The earlier partial client change was therefore rejected. The
  retained DirectMux retry activates overlap only together with generation-
  bound dialect, direction, role, and capability gates; it is not authority to
  activate overlap in the ordinary client, server, or any other transport.
- **Historical ambiguity retained:** commit `5bb3372e26` assigned PDU 75/76
  while `CODEC_VERSION` still reported 46. Version 47 is the first unambiguous
  dialect containing those identifiers, so the exhaustive registry assigns
  both a conservative minimum of 47. Static call-site inspection found the
  request/reply implementation in server dispatch but no production ordinary-
  client or DirectMux emitter, so this conservative floor does not disable a
  current production request path.
- **Structural correction retained:** the codec's single `pdu!` declaration
  now requires every future variant to declare its minimum dialect, exact
  producer/serial-role authority tuples, and capability use. Unknown IDs and
  historical gaps have no specification. The ordered-window and reorder-CAS
  capability bits remain deliberately absent from `SERVER_SUPPORTED`.
- **Decision:** retain the exhaustive policy registry, impossible-window
  rejection, and the paired DirectMux gates. Do not claim system-wide rolling
  interop, connection safety, or latency improvement from either codec metadata
  or a single transport implementation alone.
- **Primary retry condition:**
  > Activate overlap only after ordinary client, DirectMux, server dispatch, and pooling retain generation-bound dialect and capability state; reject every outbound PDU before serial allocation and first write; reject inbound above-dialect or wrong-authority frames fail-closed; clear state on reconnect; classify permanent incompatibility as non-retryable; and pass exact current/previous, current/current-plus-one, legacy-sentinel, disjoint-window, capability-disabled, and no-write barriers on one retained revision.

### IS-N065 — An identical reorder CAS is not a free no-op under the frozen protocol

- **Classification:** semantic-shortcut and false-hot-path-win rejection; no
  candidate code retained
- **Beads:** `ft-interactive-swarm-product-convergence-7xqz4.8.10.3` and
  `ft-interactive-swarm-product-convergence-7xqz4.8.10.3.3`
- **Rejected candidate:** detect that a reorder request's desired tab vector and
  active identity already equal the live window and return success without
  reserving window/topology revisions or publishing the frozen order event.
- **Negative evidence:** the current v1 authority defines a first successful
  compare-and-set as `Applied`: one new window revision, one new topology
  revision, one frozen event, and one bounded replay receipt. Exact retries must
  replay that same terminal commit without republication, while the same
  mutation identity with a different canonical digest must remain
  equivocation. A special identical-order fast path would therefore either
  make `Applied` mean two observably different things or require a new terminal
  outcome. Skipping or shrinking its receipt would also change bounded ledger
  admission and eviction behavior, so a retry could cease to replay the
  original decision after later mutations.
- **Review result:** the speculative fast path was fully reverted before
  commit `0ebc32d0912201786ef38c17f638b4f4883e62f6`. The retained code keeps the
  single authoritative apply path. Static review is not latency evidence, and
  no identical-order performance improvement is claimed.
- **Decision:** reject the shortcut under v1. Preserve one meaning for
  `Applied`, exact replay/equivocation behavior, monotonic revision authority,
  and the existing receipt budget until a versioned protocol change proves a
  different contract end to end.
- **Primary retry condition:**
  > Optimize identical-order CAS only through an explicit versioned outcome whose server, ordinary client, reconnect, event, revision, replay, equivocation, receipt-budget, and mixed-version semantics pass deterministic tests and whose retained same-window A/B shows a material target-class benefit without weakening tab-order convergence.

### IS-N066 — Removing only the aggregate tab-ID cap does not fix receipt retention

- **Classification:** boundedness shortcut and false-contract-fix rejection;
  no candidate code retained
- **Beads:** `ft-interactive-swarm-product-convergence-7xqz4.8.10.3.4`
  and `ft-interactive-swarm-product-convergence-7xqz4.8.10.3.5`
- **Rejected candidate:** delete `MAX_WINDOW_ORDER_RECEIPT_TAB_IDS` while
  retaining one copied tab-ID vector per outcome and retained
  `FrozenWindowOrder` graphs.
- **Negative evidence:** the current 65,536-ID guard retains 4,096 q1
  outcomes but only 16 q4096 outcomes, contradicting the accepted count-based
  server replay contract. Deleting only that guard would make 4,096 distinct
  q4096 ID vectors consume 128 MiB before receipt metadata and duplicated
  projections, while retained frozen orders can additionally pin Tab, pane,
  terminal, and scrollback graphs. It restores the count but not the memory,
  sharing, accounting, or latency contract.
- **Decision:** replace the guard only together with a fixed 4,096-terminal
  coordinator, shared compact immutable order state, graph-free retained
  notifications, and separate Pending bounds. Do not claim bounded memory or
  target performance from deleting the guard.
- **Primary retry condition:**
  > Retry aggregate-cap removal only when q1/q20/q50/q200/q4096 tests prove count-independent 4,096/4,097 eviction, one shared order allocation across receipt/event/response owners, zero retained live-terminal graphs, exact unique/logical byte accounting, and isolated allocator-active/RSS evidence.

### IS-N067 — Dual FIFO queues with stale tombstones are not bounded O(1) eviction

- **Classification:** data-structure boundedness rejection; no candidate code
  retained
- **Bead:** `ft-interactive-swarm-product-convergence-7xqz4.8.10.3.4`
- **Rejected candidate:** maintain global and per-namespace `VecDeque` order,
  remove from only one queue on eviction, and lazily skip stale entries in the
  other queue.
- **Negative evidence:** namespace-local churn can create stale global entries
  faster than the global head reaches them, so metadata grows without the
  terminal-receipt count growing. Eager `VecDeque::remove` instead makes
  eviction O(n). Sharding maps without one global terminal sequencer also
  cannot identify deterministic terminal receipt 4,097.
- **Decision:** use one preallocated terminal-slot set with intrusive O(1)
  links for every contract-authorized ordering relation, one global insertion
  sequencer, and exact map/list/free-slot invariants. Pending decisions remain
  outside terminal eviction.
- **Primary retry condition:**
  > Retry a multi-order receipt queue only when deterministic adversarial churn proves every metadata structure remains bounded, global oldest-terminal eviction is exact at 4,097, every unlink is O(1), Pending entries are never evicted, and map/list/free-slot invariants survive cancellation and teardown.

### IS-N068 — Client-chosen namespaces cannot provide server-side fairness

- **Classification:** unauthenticated quota-authority rejection; no candidate
  code retained
- **Beads:** `ft-interactive-swarm-product-convergence-7xqz4.8.10.3.4`
  and `ft-interactive-swarm-product-convergence-7xqz4.8.10.4`
- **Rejected candidate:** evict server receipts within a client-provided
  mutation namespace at receipt 1,025 and describe that limit as fair sharing.
- **Negative evidence:** a client can rotate arbitrary namespace IDs and evade
  such a server quota unless one namespace is registered and authenticated to
  the immutable connection binding. The accepted v1 contract instead gives
  the server one 4,096-terminal session FIFO and makes the 1,024-terminal
  bound a client-side property of one live mutation namespace. Applying 1,024
  on the server would silently shorten promised replay for a single namespace.
- **Decision:** keep server eviction global and count-based. Bound old client
  namespaces and their Pending intents in client authority. Any future
  server-side fairness policy requires a versioned authenticated registration
  contract rather than trusting request bytes.
- **Primary retry condition:**
  > Reconsider server namespace fairness only when a versioned protocol registers a bounded namespace lifecycle to authenticated `DomainBindingId` authority, defines reconnect and rotation semantics, preserves or explicitly renegotiates replay guarantees, and passes malicious-rotation plus mixed-version tests.

### IS-N069 — Core-count-sized reorder workers do not remove the mux critical path

- **Classification:** topology-blind parallelism rejection; no candidate code
  retained
- **Beads:** `ft-interactive-swarm-product-convergence-7xqz4.8.10.3.2`
  and `ft-interactive-swarm-product-convergence-7xqz4.8.10.3.4`
- **Rejected candidate:** offload every large permutation to a worker count
  derived directly from Apple-silicon or Threadripper logical CPU count while
  retaining the global window-registry, topology, and receipt critical
  sections.
- **Negative evidence:** PDU88 currently enters the mux main thread and holds
  global authority across O(q) validation, freezing, and receipt work.
  Additional workers cannot make disjoint windows progress through those
  locks; they can multiply q4096 scratch memory, cache traffic, migrations,
  and wakeups. A 64-core/128-thread host does not turn one serialized commit
  lane into 128 independent authorities.
- **Decision:** first add bounded window-local logical mutation lanes, short
  scalar commit sections, Pending count and tab-weight backpressure, and
  per-stage probes. Tune a persistent worker pool from same-window evidence,
  not reported CPU count.
- **Primary retry condition:**
  > Reconsider worker-count scaling only after disjoint-window barriers prove q-sized preparation is outside global locks, same-window and cross-window mutation lanes remain correct, aggregate scratch is bounded, and isolated M4, M5, and Threadripper sweeps show improved p95/p99 without worse keypress tails, RSS, or energy.

### IS-N070 — Eager per-subscriber order projection multiplies q-sized fanout

- **Classification:** topology-blind fanout rejection; candidate corrected before
  commit
- **Beads:** `ft-interactive-swarm-product-convergence-7xqz4.8.10.2.2`
  and `ft-interactive-swarm-product-convergence-7xqz4.8.10.3.5`
- **Rejected candidate:** convert every shared `FrozenWindowOrder` notification
  into a fresh per-connection `OrderedWindowStateV1` before checking whether
  that connection negotiated ordered delivery.
- **Negative evidence:** the mux publishes one shared frozen order, but eager
  projection traverses and allocates q tab IDs once per subscriber. Legacy and
  coherent-only clients would pay this cost even while ordered capabilities
  are dormant, so a q4096 reorder with many attached clients multiplies work
  and allocation by fanout precisely on the interaction path being optimized.
  Moving that multiplication outside the coordinator mutex fixes lock hold
  time but does not fix total CPU, allocator pressure, or callback latency.
- **Decision:** keep legacy, coherent-only, dormant, and partially supported
  generations on an O(1) fallback carrying only window/resync identity. Permit
  q-sized projection only for an exact ordered fence or established ordered
  generation, outside the coordinator lock, followed by a generation and
  revision recheck. Activation still waits for source-shared compact order
  state so multiple ordered subscribers do not rebuild the same vector.
- **Primary retry condition:**
  > Reconsider per-subscriber projection only when q1/q20/q200/q4096 fanout sweeps prove total conversions and allocations remain independent of legacy/coherent subscriber count, ordered generations preserve exact PDU90 state, callback tails remain bounded, and shared compact source state cannot be reused.

### IS-N071 — Post-copy reweighting does not bound retained batch allocation

- **Classification:** transient-memory and accounting-order rejection;
  structural correction retained pending remote proof
- **Beads:** `ft-interactive-swarm-product-convergence-7xqz4.8.10.2.2`
  and `ft-interactive-systems-performance-4tenz.5.5.14.1.2.3.2.4`
- **Rejected candidate:** concatenate two already-accounted topology or ordered-
  snapshot frames into the first frame's `Vec`, then reweight the combined
  reservation from the resulting capacities.
- **Negative evidence:** `Vec::extend_from_slice` may allocate and copy the
  combined target before the replacement charge is admitted. During that
  transition the process owns both source frames and the new target allocation;
  a later budget rejection therefore limits retained state only after the
  allocator work and peak have occurred. Same-class batching also copies bytes
  that already have a complete wire representation.
- **Decision:** retain exact frame allocations as separate flush epochs and
  batch only unaccounted control/bulk frames. A future segmented or vectored
  writer may recover syscall amortization, but it must reserve aggregate iovec
  metadata and preserve each frame's existing exact byte charge before any new
  allocation.
- **Primary retry condition:**
  > Retry retained-frame batching only when a pre-admitted segmented or vectored implementation proves no payload copy, exact live-capacity accounting through every partial-write and terminal path, unchanged PDU87-before-PDU90 flush order, and a material isolated M4/M5/Threadripper p95/p99 benefit without higher peak allocation or keypress tails.

### IS-N072 — A clamped serde size hint is not schema count admission

- **Classification:** hostile-input and false-before-allocation rejection;
  zero-wire decoder chokepoint retained pending remote proof
- **Bead:** `ft-interactive-swarm-product-convergence-7xqz4.8.10.2.2`
- **Rejected candidate:** read a hostile PDU87 sequence/map prefix through the
  generic one-million-item decoder, expose only `min(length, 4096)` as serde's
  size hint, and rely on a custom visitor's maximum check.
- **Negative evidence:** the visitor cannot distinguish a declared 16,385-item
  collection from a 4,096-item hint. It rejects only after decoding the 16,385th
  pane/title or 4,097th map entry, including nested strings and pane graphs.
  The clamp limits one eager capacity request but does not enforce the closed
  PDU87 schema before element allocation.
- **Decision:** use zero-wire named-newtype markers to install exact collection
  admissions at the varbincode length-prefix chokepoint, restore the enclosing
  admission after each field, and expose the admitted exact q=16,384 hint so a
  maximum legal snapshot allocates once. This is structural boundedness, not
  RSS or latency evidence.
- **Primary retry condition:**
  > Replace schema-scoped prefix admission only when every compressed and uncompressed max-plus-one collection fails from a prefix-only fixture before EOF or element decode, every exact boundary roundtrips, golden bytes remain unchanged, and retained target-class allocation evidence shows an alternative is both equally bounded and materially faster.

### IS-N073 — A top-level pane-tree count does not bound recursive PDU87 graphs

- **Classification:** incomplete resource-envelope rejection; capability stays
  dormant
- **Bead:** `ft-interactive-swarm-product-convergence-7xqz4.8.10.2.2.2`
- **Rejected inference:** limiting `ListPanesResponse.tabs` to 16,384 values and
  the PDU87 body to 16 MiB bounds pane snapshot decoding and destruction.
- **Negative evidence:** each `PaneNode` is a recursively derived Split/Leaf
  tree. A compact deeply nested Split chain can exhaust the Rust stack during
  deserialize or drop, while total boxed nodes and leaves are not bounded by
  the number of top-level tab trees. A byte ceiling limits input volume but is
  not a proof of finite recursion depth or bounded Box amplification.
- **Decision:** keep ordered-window capability activation blocked on an
  iterative/flat, depth/node/leaf-bounded PDU87 representation or an equally
  strong seeded-decoder proof. Do not infer authority safety, hostile-input
  safety, memory bounds, or q4096 readiness from the new top-level collection
  caps alone.
- **Primary retry condition:**
  > Activate PDU87 pane snapshots only after prefix-only, maximum-depth, depth-plus-one, maximum-node, node-plus-one, broad-tree, malformed-index, compressed, truncated, and drop-path tests prove finite iterative admission and conversion with bounded allocation and no recursive stack growth.

### IS-N074 — Replacing dormant PDU87 does not harden live recursive PDU4/PDU82

- **Classification:** hidden live-surface and incomplete-remediation rejection;
  separate P0 hardening bead opened
- **Beads:** `ft-interactive-swarm-product-convergence-7xqz4.8.10.2.2.2`
  and `ft-interactive-systems-performance-4tenz.5.5.14.1.2.3.2.3.1.1`
- **Rejected inference:** replacing PDU87's provisional recursive pane payload
  with a flat arena removes the mux protocol's recursive pane-tree admission
  risk.
- **Negative evidence:** live PDU4 `ListPanesResponse` and authority-bearing
  PDU82 `CoherentPaneSnapshot` also embed the recursively derived `PaneNode`.
  They deserialize before client application and remain exposed to unbounded
  depth, Box amplification, recursive validation/serialization, and recursive
  drop even while PDU87 capability stays dormant.
- **Decision:** redesign dormant PDU87 directly as a bounded flat arena, but
  independently harden the byte-compatible PDU4/PDU82 field with seeded depth,
  node, and leaf admission plus iterative producer preflight. Ordered-window
  activation now also depends on the live-family hardening bead.
- **Primary retry condition:**
  > Treat recursive pane transport as bounded only after PDU4 and PDU82 golden bytes remain identical, hostile depth/node max-plus-one frames fail before unsafe construction in both compression modes, producer preflight is iterative, and every admitted value has a proven finite drop depth.

### IS-N075 — The server's local codec version is not a peer-agreed dialect

- **Classification:** false protocol-authority rejection; tentative local-only
  threading removed before commit
- **Beads:** `ft-interactive-swarm-product-convergence-7xqz4.8.10.2.2`,
  `ft-interactive-systems-performance-4tenz.5.5.3.5.12.4`, and
  `ft-interactive-systems-performance-4tenz.5.5.3.5.12.5`
- **Rejected candidate:** initialize the server dispatch coordinator with its
  own `codec::CODEC_VERSION` and retain that value as the connection's
  `agreed_codec_version` for PDU86-PDU90 authority.
- **Negative evidence:** PDU26 is empty and PDU27 advertises only the server's
  version window. The client computes and retains the overlap result, but the
  ordinary server never observes the client window and therefore cannot know
  the peer-agreed dialect. A local value of 53 cannot distinguish a v52 peer,
  a stale connection generation, or a forged v53-only request. The `None` in
  `decode_async_with_selector` is a maximum-serial option, not a dialect gate.
- **Decision:** make the ordered fence depend on symmetric codec-window
  registration and exact per-generation server wire authority. Once that
  authority exists, retain its immutable generation and agreed dialect in the
  PDU86 fence, PDU87/PDU90 permits, established stream, and PDU88 token; reject
  insufficient or stale dialects before body allocation.
- **Primary retry condition:**
  > Claim an agreed server dialect only after a symmetric nonce-bound registration proves both version windows and the exact overlap, a generation-bound pre-body selector rejects v52 PDU86 without q allocation, v53 establishes and retains exact authority, and stale-generation or forged permits cannot encode, queue, or mutate.

### IS-N076 — A logical serializer ceiling does not prevent geometric capacity growth

- **Classification:** producer peak-allocation rejection; exact-increment
  containment retained pending allocator evidence
- **Bead:** `ft-interactive-swarm-product-convergence-7xqz4.8.10.2.2`
- **Rejected inference:** rejecting a `BoundedSerializeBuffer` write when its
  logical length exceeds the wire limit also bounds the backing `Vec` to that
  limit.
- **Negative evidence:** `Vec::try_reserve` uses amortized growth. A small legal
  write near the ceiling can therefore grow capacity geometrically before the
  completed frame is charged, while the typed graph and compression buffers
  are still live. The byte count is valid but the transient allocation claim
  is false.
- **Decision:** reserve each admitted increment with `try_reserve_exact`, while
  retaining the checked logical ceiling before reservation. This removes the
  collection's geometric growth policy; it does not claim allocator size-class
  bytes, RSS, or that typed, uncompressed, compressed, and final owners never
  overlap.
- **Primary retry condition:**
  > Claim a hard producer allocation envelope only after allocator-visible peak-live-byte counters cover the typed graph, exact-increment body, compressor workspace/output, final frame, and deallocation order at q1/q20/q200/q4096 on every supported target class.

### IS-N077 — HashMap iteration is not canonical snapshot authority

- **Classification:** nondeterministic wire-authority rejection; dormant schema
  replacement required
- **Bead:** `ft-interactive-swarm-product-convergence-7xqz4.8.10.2.2.2`
- **Rejected candidate:** serialize PDU87 `window_titles` directly from a Rust
  `HashMap` and treat the resulting bytes as a stable authoritative snapshot.
- **Negative evidence:** semantically equal maps can iterate in different
  orders across processes or runs. A single-entry golden fixture cannot expose
  that ambiguity, and byte-level digests, replay comparison, and compression
  results can diverge even when window/title state is identical.
- **Decision:** the flat PDU87 replacement must encode window titles as one
  sorted canonical sequence keyed by wire window identity. Capability stays
  dormant; current bytes are not promoted as canonical.
- **Primary retry condition:**
  > Treat PDU87 bytes as canonical only after multi-entry permutation tests, repeated fresh-map construction, compressed and uncompressed golden fixtures, and decode-reencode identity prove one byte sequence for every equivalent admitted snapshot.

### IS-N078 — Admission that cannot fund encoding is not a live queue contract

- **Classification:** retained-memory liveness rejection; fail-closed
  containment is insufficient for service
- **Beads:** `ft-interactive-systems-performance-4tenz.5.5.14.1.2.3.2.4`
  and `ft-interactive-systems-performance-4tenz.5.5.3.5.5.6`
- **Rejected candidate:** admit a typed topology payload up to the complete
  retained-byte ceiling, then require its typed owner and encoded allocation to
  coexist under that same ceiling during drain.
- **Negative evidence:** an accepted near-ceiling payload can have no remaining
  headroom for its encoded representation. Later conversion must terminate the
  connection even though initial admission reported success. This is memory
  safe but converts a supposedly serviceable queue entry into a deterministic
  liveness failure.
- **Decision:** closure requires worst-case encode headroom at initial
  admission, a lower typed ceiling, or move/stream/vectored encoding whose live
  ownership is charged before allocation. Do not describe current fail-closed
  conversion as guaranteed progress.
- **Primary retry condition:**
  > Claim queue liveness only when every admitted boundary payload reaches flush or a pre-admission typed rejection, never a post-admission capacity failure, with exact live-byte release under partial writes, cancellation, reconnect, and teardown.

### IS-N079 — Claimed frame metadata is not proof of encoded byte identity

- **Classification:** latent authority-binding rejection; private constructors
  reduce reachability but do not prove the invariant
- **Bead:** `ft-interactive-systems-performance-4tenz.5.5.3.5.5.7`
- **Rejected inference:** validating an `EncodedPduAuthority` family, serial,
  and emission tag proves its separately stored byte vector encodes that same
  header and family.
- **Negative evidence:** the current pre-encoded boundary trusts metadata
  captured alongside bytes rather than deriving final authority from an opaque
  encode-only owner or revalidating the actual header. Private constructors
  make cross-wiring harder today, but future call sites can silently weaken the
  invariant without a production source guard.
- **Decision:** retain the current private typed permits as containment, but
  block the complete claim on the no-bypass encoder census and an opaque owner
  whose authoritative header identity cannot be paired with unrelated bytes.
- **Primary retry condition:**
  > Promote encoded-frame authority only after every production constructor is classified, raw construction is test-only, final write admission proves the actual encoded family and serial, and cross-wired metadata/byte fixtures fail before queue or network effects.

### IS-N080 — EventBus arrival order is not a durable resume-cursor authority

- **Classification:** out-of-order delivery and false-authority rejection;
  persisted watch records reduced to wakeups
- **Beads:** `ft-7h5da.4.8`, `ft-ubuw2`, and `ft-1vca6`
- **Rejected candidate:** emit each persisted `EventBus` record directly and
  advance the consumer resume cursor to the record's database ID.
- **Negative evidence:** durable rows can commit and publish from independent
  producers in a different order. Receiving ID 11 before ID 10 and exposing
  cursor 11 makes a disconnect or later `id > cursor` scan omit ID 10. A
  bounded-broadcast lag marker detects dropped channel records, but it cannot
  turn channel arrival order or payload identity into SQLite ordering
  authority. Dedupe conflicts can also associate a current attempted payload
  with an older stored ID.
- **Decision:** the robot watch follower now treats cursor-bearing IPC records
  only as wakeups and fetches exact payloads in ascending SQLite-ID order.
  Cursorless lifecycle signals stay distinctly best-effort. The shared
  `FilteredEventStream` remains unqualified until `ft-7h5da.4.8` gives it the
  same storage-owned scan/acknowledgment state machine.
- **Primary retry condition:**
  > Permit a persisted bus record to advance a resume cursor only after deterministic out-of-order, subscribe-before-drain, lag, reconnect, filtered-row, claim-contention, output-failure, and retention-expiry proofs show that every durable row is scanned in storage order and no unacknowledged row can be skipped.

### IS-N081 — Insert-then-publish adjacency is not an atomic delivery boundary

- **Classification:** crash-window and false-completion rejection; durable
  outbox follow-on required
- **Bead:** `ft-7h5da.4.9`
- **Rejected inference:** a successful event insert followed immediately by an
  in-memory `EventBus::publish` is effectively atomic because both operations
  occur in one producer function.
- **Negative evidence:** process death, cancellation, panic, or runtime failure
  can occur after SQLite commit and before publication. The event then exists
  durably but workflow and connector consumers may never receive a wakeup.
  Publishing first is also invalid because consumers could act on a row whose
  transaction later fails. The new `EventRecordOutcome` correctly suppresses
  duplicate live publication, but it cannot close this two-commit crash seam.
- **Decision:** keep insert outcome as dedupe authority and do not fake atomic
  publication. `ft-7h5da.4.9` requires an outbox intent committed in the event
  transaction, bounded token-CAS replay, a frozen per-event delivery plan, and
  idempotent per-consumer receipts.
- **Primary retry condition:**
  > Treat durable insert as complete downstream delivery only after crash injection at every post-commit boundary proves startup and periodic replay conserve every required sink effect, duplicate attempts converge through stable receipts, and ambiguous external effects fail closed rather than being silently retried or dropped.

### IS-N082 — A single unkeyed response-delivery slot cannot survive concurrent MCP

- **Classification:** concurrency and cross-request authority rejection;
  sequential containment retained
- **Beads:** `ft-7h5da.4.7` and `ft-7h5da.4.10`
- **Rejected candidate:** remove FastMCP's per-connection request serialization
  while retaining one prepared/armed delivery-action slot that finalizes after
  the next response flush.
- **Negative evidence:** two requests completing out of order can arm, replace,
  finalize, or release the wrong event leases. The current sequential transport
  makes the single-flight coordinator safe, but a long `wa.await_event` also
  prevents the reader from receiving later work and the very
  `notifications/cancelled` frame intended to stop that request.
- **Decision:** retain truthful sequential delivery acknowledgment now and
  document the head-of-line/cancellation limitation. Concurrent dispatch is
  blocked on `ft-7h5da.4.10`: dedicated reader, bounded structured request
  tasks, one frame writer, and delivery actions keyed by JSON-RPC ID plus an
  unforgeable request generation.
- **Primary retry condition:**
  > Enable concurrent per-connection tool execution only after out-of-order response, duplicate-ID, ID-reuse, cancellation/completion race, partial-write, flush-failure, connection-close, and saturation models prove at most one response per request generation and no lease action can cross request boundaries.

### IS-N083 — Structural correctness work is not M4/M5/Threadripper performance proof

- **Classification:** wrong-evidence-pipeline rejection; improvements retained
  without native latency claims
- **Beads:** `ft-interactive-systems-performance-4tenz`,
  `ft-interactive-swarm-product-convergence-7xqz4.8.10`, and
  `ft-interactive-systems-performance-4tenz.13`
- **Rejected inference:** sharded route maps, bounded lease scans, batched parser
  policy, ordered durable drains, and reduced hot-loop allocation necessarily
  establish improved keypress, resize, zoom, or long-session responsiveness on
  recent Apple silicon and the 128-core Threadripper host.
- **Negative evidence:** these changes have static and focused remote
  correctness evidence only. The ordered-window path remains capability-fenced,
  durable tab-order restoration remains incomplete, and the retained
  target-class resource-cockpit artifact is still `skipped_not_proven`. No
  current-source native key-to-photon, continuous-resize, visual-equivalence,
  long-soak, thermal, energy, or NUMA result exists for M4, M5, or `trj`.
- **Decision:** retain the structural changes because they close concrete
  correctness and boundedness defects, but make no target-performance claim.
  Qualification must use isolated fixtures and named target artifacts without
  launching, attaching to, or perturbing a user's live FrankenTerm session.
- **Primary retry condition:**
  > Promote any Apple-silicon or Threadripper performance claim only after exact-source baseline/candidate A/B runs retain target identity, workload and font/config hashes, p50/p95/p99/p999 with uncertainty, visual/state equivalence, RSS and allocation slopes, thermal/energy context, and rollback evidence for quiet plus q20/q50/q200 long-session workloads.

### IS-N084 — A deadline check before blocking stdio is not a response timeout

- **Classification:** false timeout-authority rejection; upstream transport
  blocker remains open
- **Bead:** `ft-bd3vr`
- **Rejected inference:** forwarding `McpClientConfig.timeout_ms` into the
  pinned FastMCP `ClientBuilder` proves that outbound tool calls return within
  that duration.
- **Negative evidence:** FastMCP revision
  `1038dd4e64cc7df8ea8122dbfb8806b0b04a7130` computes a response deadline and
  checks it before each receive attempt, but the synchronous stdio transport
  then calls blocking `BufRead::read_line`. A silent subprocess can therefore
  remain inside one receive beyond the configured deadline, and a caller's
  `Cx` cannot interrupt that read. The previous `connect_timeout_ms` naming and
  “bounded by” rustdoc promoted configuration into authority it did not have.
- **Decision:** rename the mirrored value as configured response-timeout
  telemetry, state explicitly that it is not a hard wall-clock bound, and
  reopen `ft-bd3vr`. Do not add a wrapper parameter that FastMCP cannot enforce.
  A real fix requires cancellation-safe transport I/O plus caller-specific
  deadline propagation and teardown of a timed-out subprocess/client.
- **Primary retry condition:**
  > Claim an outbound MCP response timeout only after a silent-subprocess fixture proves the exact call returns by the caller deadline, cancels or closes the blocked transport without leaking its reader thread or child process, and leaves later calls in a deterministic usable-or-closed state.

### IS-N085 — A shaping generation bump is not cache invalidation

- **Classification:** stale-render-state rejection; structural fix retained
- **Bead:** `ft-interactive-systems-performance-4tenz.8`
- **Rejected inference:** incrementing the GUI shaping-input generation is
  sufficient to invalidate every line-shaping result after a scale, font, or
  configuration transition.
- **Negative evidence:** the generation changed, but the HarfBuzz shape cache
  and `line_to_ele_shape_cache` still retained entries computed from the prior
  shaping inputs. Repeated resize/zoom/font transitions could therefore retain
  stale work and grow LFU state even though the glyph atlas remained reusable.
- **Decision:** route shaping-input transitions through one saturating helper
  that advances the generation and clears both shaping-result caches while
  preserving atlas-only reuse. Do not claim a resize/zoom latency win from the
  invalidation correction alone.
- **Primary retry condition:**
  > Keep a more selective shaping-cache reuse policy only after scale, font, fallback-font, DPI, configuration, resize-storm, and generation-exhaustion tests prove byte/visual equivalence and retained M4/M5 timing shows a statistically meaningful win without stale entries or unbounded cache growth.

### IS-N086 — Receiver limits do not bound producer traversal and callbacks

- **Classification:** wrong-boundary rejection; bounded producer preflight
  retained
- **Rejected inference:** validating the final ordered pane arena against wire
  depth and node ceilings is enough to bound snapshot production.
- **Negative evidence:** the producer could recursively census a malformed or
  over-limit tree, traverse hidden stacks/floating/zoom carriers, allocate
  observation state, and invoke `Pane::pane_id` before the codec rejected the
  result. A unique-identity cap also failed to bound repeated raw carriers, and
  coupling census capacity to remaining aggregate arena space made acceptance
  depend on tab position.
- **Decision:** perform an iterative callback-free preflight with independent
  tree-node/depth and stable raw-carrier budgets, exact ownership validation,
  fallible geometric allocation, active-pane capture, bounded retries, and
  rollback before any pane callback or arena append.
- **Primary retry condition:**
  > Relax or remove producer preflight only after adversarial depth, node, repeated-Arc, empty-stack, hidden/floating/zoom alias, malformed split, invalid-active, callback-mutation, allocation-failure, and q4096 tests prove every rejected topology consumes bounded work and performs zero pane callbacks or caller-arena mutation.

### IS-N087 — A complete shard snapshot is not authoritative without a generation fence

- **Classification:** stale-publication rejection; linearized route snapshot
  retained
- **Beads:** `ft-interactive-systems-performance-4tenz.5.5.3.5.8.5.1` and
  `ft-interactive-systems-performance-4tenz.5.5.13.1.1`
- **Rejected inference:** replacing the global route cache after a full shard
  discovery pass is safe because the pass observed every backend.
- **Negative evidence:** a concurrent route insert/remove can commit after the
  discovery pass starts and before its replacement publication. Without a
  generation fence, the older full snapshot overwrites that newer delta.
  Wrapping the generation counter would eventually let an ancient snapshot
  appear current again.
- **Decision:** publish only against the exact captured generation, linearize
  delta updates with snapshot publication, and fail closed at the saturated
  generation sentinel rather than wrapping.
- **Primary retry condition:**
  > Replace the generation fence only after a deterministic insert/remove/full-scan interleaving model plus generation-exhaustion tests prove no committed newer route can be lost, resurrected, or misrouted and global window/tab/pane identities remain collision-free across every shard.

### IS-N088 — Global `MIN(id)` is not retention-gap authority

- **Classification:** false no-gap inference; transactional deletion evidence
  required
- **Bead:** `ft-7h5da.4.8`
- **Rejected inference:** if a resume cursor is not below the oldest surviving
  event ID, no retention deletion intersects the requested history.
- **Negative evidence:** cleanup deletes by timestamp and tier predicates, not
  by an ID prefix. With ID 1 retained, ID 2 tier-expired, and ID 3 retained, a
  cursor at 1 sees global minimum 1 and then advances to 3, silently losing ID
  2. Filtered or unhandled-only streams naturally have sparse IDs, so probing
  global minimum on every discontinuity also adds pathological extra SQLite
  work while confusing filtering with pruning.
- **Decision:** do not infer deletion from page sparsity or global minimum.
  Every resumable deletion must atomically record exact coalesced ID evidence;
  resume validation must carry a cursor epoch and reconcile the requested
  interval against that evidence in the storage snapshot.
- **Primary retry condition:**
  > Claim no-silent-gaps only after fresh and upgraded databases prove epoch mismatch and legacy uncertainty fail closed, every flat/tiered/interior/full-prune deletion commits exact non-overlapping intervals atomically, sparse filtered pages add no false probe, and concurrent cleanup/resume cannot skip a deleted or reuse-ambiguous ID.

### IS-N089 — Caller cancellation cannot own post-delivery lease settlement

- **Classification:** cancellation-authority rejection; bounded completion
  capability retained
- **Bead:** `ft-7h5da.4.8`
- **Rejected inference:** using the request `Cx` for reserve, output, release,
  and finalization is uniformly cancel-correct.
- **Negative evidence:** after reservation succeeds, the writer can cancel the
  caller. Reusing that cancelled `Cx` makes release/finalize fail their
  preflight, stranding the lease until TTL; after a successful flush it can
  also leave delivered output unhandled and eligible for redelivery.
- **Decision:** keep acquisition caller-cancellable, checkpoint before starting
  synchronous output, then give release/finalize one independent finite
  completion capability after ownership has been acquired.
- **Primary retry condition:**
  > Change the completion boundary only after deterministic cancellation before write and inside successful, closed-pipe, and failed writers proves every acquired lease is finalized exactly after acknowledged flush or released promptly after known failure, with bounded settlement time and no cross-request ownership.

### IS-N090 — A Cx checkpoint cannot preempt a blocked synchronous output pipe

- **Classification:** false cancellation/backpressure bound; architectural
  follow-on required
- **Beads:** `ft-7h5da.4.8` and `ft-7h5da.4.10`
- **Rejected inference:** checking cancellation immediately before
  `StdoutLock::write`/flush makes watch-event output bounded and cancellable.
- **Negative evidence:** a full but still-open downstream pipe can block the
  async worker inside synchronous stdio indefinitely. No later checkpoint,
  heartbeat, lease finalization, or shutdown observation can run, and a
  delivery lease can outlive its intended settlement window.
- **Decision:** retain pre/post checkpoints as correctness guards for ready
  I/O, but make no hard output-timeout claim. The durable fix needs one bounded
  dedicated/asynchronous writer, explicit queue/backpressure policy, flush
  acknowledgments coupled to claim finalization, cancellation-aware teardown,
  and deterministic stalled-consumer behavior.
- **Primary retry condition:**
  > Claim bounded watch output only after a stalled-but-open pipe fixture proves queue admission, cancellation, shutdown, writer teardown, lease release/finalization, memory growth, and cursor behavior remain bounded under partial writes, flush stalls, pipe closure, and sustained producer overload.

### IS-N091 — A zero-test GUI library command is not renderer proof

- **Classification:** false test-topology authority; explicit opt-in crate-root
  harness retained
- **Bead:** `ft-interactive-systems-performance-4tenz.6.8.1`
- **Rejected inference:** an exit-zero `cargo test -p frankenterm-gui --lib`
  exercises binary-owned glyph-cache, `TermWindow`, background, and paint code.
- **Negative evidence:** the library target excludes those production modules;
  prior invocations could run zero relevant tests while reporting success. The
  explicit `glyphcache_unit` target compiles `src/main.rs` through a distinct
  crate-root wrapper under the generated Rust test harness, with the
  application `main` cfg-disabled, but remains an opt-in feature-gated target
  rather than ordinary workspace test authority. Disabling `main` prevents
  automatic startup but does not prove that arbitrary included test bodies
  never call frontend or window constructors.
- **Decision:** retain the explicit harness as focused structural evidence and
  require exact named-test counts. Qualify only exact statically audited test
  filters as nonlaunching; do not infer GUI launch, window-system, visual,
  presentation, or native-target proof from the target as a whole.
- **Primary retry condition:**
  > Claim ordinary renderer-test coverage only after the production binary-owned module graph participates in the normal fail-closed test gate, the retained transcript names and runs the intended tests with nonzero counts, and an authorized isolated native lane separately proves window and presentation behavior without touching an operator session.

### IS-N092 — Off-thread decode is not off-thread paint when frames remain blobs

- **Classification:** hidden GUI-thread I/O/copy rejection; immutable shared
  frame storage retained
- **Bead:** `ft-interactive-systems-performance-4tenz.8`
- **Rejected inference:** decoding animation frames on a bounded worker means
  the GUI upload path performs no blocking file I/O or full-frame copy.
- **Negative evidence:** decoded worker frames were stored as blob leases. A
  later paint called `get_data`, which could reopen/read a temporary file and
  materialize another full pixel vector synchronously before atlas upload.
- **Decision:** retain encoded source leases, but publish decoded frames as
  immutable `Arc<Vec<u8>>` values with dimensions, duration, and hash. A
  borrowed frame handle implements `BitmapImage`, so normal paint neither
  reopens the blob nor clones its pixels.
- **Primary retry condition:**
  > Reintroduce blob-backed decoded frames only after a deterministic slow-storage fixture proves every ordinary and animated paint/upload path performs zero file opens, reads, and full-frame copies on the GUI thread while preserving exact pixels, duration order, revision fencing, cancellation, and bounded memory.

### IS-N093 — Object provenance is not revision-bound image validation authority

- **Classification:** stale-trust and mismatched-ceiling rejection; private
  revision witness retained
- **Bead:** `ft-interactive-systems-performance-4tenz.8`
- **Rejected inference:** once an `ImageData` object has passed validation, any
  later clone/rebuild or fast path can trust it without rechecking its content
  revision and trust class.
- **Negative evidence:** rebuilding validated content could discard authority,
  while provenance-only trust could survive mutation or conflate the 64 MiB
  untrusted-wire ceiling with the separate 256 MiB trusted-local decoded
  ceiling. That creates either repeated expensive validation or stale/overbroad
  admission.
- **Decision:** make the validation authority private and serde-skipped, bind it
  to the exact content revision and summary, clear it around mutable access,
  preserve it only when retaining the same object, and check it under the data
  lock before the trusted fast path.
- **Primary retry condition:**
  > Replace revision-bound authority only after mutation-before/during/after-validation, serde round-trip, object rebuild, cache ABA, 64 MiB boundary, 256 MiB boundary, and concurrent fast-path models prove no stale validation is accepted and no valid trusted-local frame is spuriously routed through untrusted fallback.

### IS-N094 — Cache eviction does not bound active background decoded payloads

- **Classification:** wrong ownership/accounting boundary; unique logical
  decoded-pixel budget retained
- **Bead:** `ft-interactive-systems-performance-4tenz.8`
- **Rejected inference:** bounding the decoded background cache also bounds the
  images retained by the active window background list.
- **Negative evidence:** active layers hold escaped strong `Arc`s after cache
  eviction, repeated references to one image must count once, and distinct
  images must count separately. An unbounded layer list also exhausted the
  negative `i8` z range, causing later backgrounds to saturate/spill into the
  content plane.
- **Decision:** cap active negative-z layers at 127 and admit at most 256 MiB of
  exact unique active decoded-pixel vector lengths. The budget owns a strong
  identity witness and verifies `Arc::ptr_eq`, preventing raw-pointer ABA while
  deduplicating shared layers. This does not bound vector capacity, metadata,
  allocator overhead, encoded/cache/in-flight copies, GPU residency, or process
  RSS.
- **Primary retry condition:**
  > Claim a resident-memory bound only after shared-Arc, distinct-Arc, pointer-reuse, cache-eviction, replace/remove, 127/128-layer, vector capacity, metadata, allocator, encoded/cache/in-flight, GPU, animation, and long-resize models reconcile the logical payload ledger with retained process and device-memory measurements without dropping an admitted unique image silently.

### IS-N095 — Off-main decode does not remove synchronous atlas scale fallback

- **Classification:** remaining paint-thread latency path; coherent fix tracked
- **Bead:** `ft-interactive-systems-performance-4tenz.8.8.1`
- **Rejected inference:** moving validation and animation decode to workers
  eliminates all image-sized CPU and allocation work from a paint.
- **Negative evidence:** atlas-capacity recovery promotes `AllowImage::Yes`
  through scale factors and calls `Atlas::allocate_with_padding`. Its scale
  branch allocates a full-resolution 4WH image, copies the complete source, and
  performs a high-quality resize synchronously. Re-entering paint can repeat
  the fallback unless pressure state and scaled variants are retained.
- **Decision:** do not install a smaller synchronous helper and call the path
  fixed. The existing Bead owns direct fallible source scaling, typed/atomic
  failure, bounded cache/accounting, pixel oracle/readback, latency/byte
  telemetry, and named-target A/B evidence; an async extension must also own
  job de-duplication, byte permits, cancellation, repaint, revision/upload
  fencing, and atlas-recreation semantics.
- **Primary retry condition:**
  > Claim nonblocking scaled-image recovery only after exact-source q1/q20/q200 and atlas-pressure fixtures prove the GUI thread performs bounded dimension-independent work, duplicate paints create at most one bounded job per image revision and scale, stale jobs cannot upload after mutation or atlas recreation, pixels match the oracle, and retained M4 plus Threadripper p95/p99 evidence passes with M5 explicitly proven or skipped_not_proven.

### IS-N096 — An f64 tile quotient below 2^53 does not preserve sub-tile phase

- **Classification:** floating-point cancellation rejection; exact
  binary-rational reduction retained
- **Bead:** `ft-interactive-systems-performance-4tenz.8`
- **Rejected inference:** converting long-scroll background arithmetic from
  f32 to f64 and rejecting quotients at 2^53 preserves both mirror parity and
  fractional tile phase.
- **Negative evidence:** integer precision protects the whole-tile witness but
  not the division remainder. With an exactly represented scroll distance of
  2^54 pixels and an exactly represented 3-pixel step, the true truncated
  quotient is below 2^53 with a one-pixel remainder; f64 division rounds the
  quotient to an integer-valued float, so `fract() * step` produces a false
  zero phase. A first exact-rational implementation also used
  `u128::checked_shl` as an overflow check; that API rejects an excessive shift
  count but silently discards significant high bits shifted past bit 127. It
  then retained a 2^53 whole-tile cap and a wider successor retained a u64 cap,
  even though the quotient is never exposed or converted back to f64 and is
  used only for integer mirror parity.
- **Decision:** decompose the finite distance and f32-derived step into exact
  binary significand/exponent pairs. For nonnegative exponent deltas, compute
  the aligned numerator modulo twice the denominator with fast modular
  exponentiation: the modulus identifies quotient parity and exact remainder
  without materializing a whole-tile count. For negative deltas,
  `distance >= step` bounds the aligned denominator by the distance
  significand, so checked materialization is finite. Carry the exact signed
  remainder into origin normalization. Admit the row-pixel/f32-factor product
  according to its exact combined odd-significand width, not a fixed
  row-magnitude ceiling, so large powers of two remain representable without
  admitting rounded inputs.
- **Primary retry condition:**
  > Replace exact binary-rational reduction only after positive/negative scroll, odd/even mirror parity, non-power-of-two steps, quotients around and above 2^24, 2^53, u64, and u128, exact nonzero remainders rounded to integral floating quotients, exponent alignments too wide to materialize, invalid/nonfinite inputs, and backward viewport extension all prove identical phase without an arbitrary quotient-type ceiling.

### IS-N097 — A prepared legacy baseline is not delivered-output authority

- **Classification:** speculative-state publication race; exact enqueue-phase
  settlement retained
- **Bead:** `ft-interactive-systems-performance-4tenz.5.5.2`
- **Rejected inference:** once a legacy pane delta has been prepared and its
  new baseline installed, another render attempt may safely diff from that
  baseline before the first delta's transport enqueue has settled.
- **Negative evidence:** the baseline was advanced before `send_bulk` or
  `send_control`. A reentrant/simultaneous attempt could therefore treat
  possibly unsent bytes as delivered, while send failure, guard drop, panic,
  or same-numeric-pane-ID replacement complicated rollback ownership.
- **Decision:** give each exact per-pane state an `Idle`, revision-bearing
  `InFlight`, or fail-closed `Closed` legacy enqueue phase. Exclude competing
  legacy and transactional preparations while in flight; acknowledge only the
  exact installed revision after successful queue admission; and roll back
  only that revision on failure/drop. Settlement ambiguity retires the state
  closed and dirty, and detached push failures remain visible. Queue admission
  still is not a client application acknowledgement.
- **Primary retry condition:**
  > Remove the legacy enqueue phase only after concurrent preparation, reentrant send, queue rejection, guard drop, acknowledgement panic, recovery panic, revision exhaustion, old-registration/new-registration same-ID ABA, and detached-task error fixtures prove no unsent baseline becomes authoritative; claim end-to-end delivery only after a protocol application ACK binds the exact session, pane registration, and delta revision.

### IS-N098 — `async fn` syntax is not proof of call-time side effects

- **Classification:** proof-doctrine semantic regression; exact eager
  settled-future category retained
- **Bead:** `ft-3kv6e` (the original closed public-async census did not cover
  this newly identified synchronous-returning-Future contract)
- **Rejected inference:** converting every public Cx-aware function that
  returns a future into `async fn` preserves cancellation, preflight, channel
  observation, and completed-outcome telemetry semantics.
- **Negative evidence:** an async body does not execute until first poll. For
  the three audited APIs, an immediately dropped unpolled future would defer or
  erase work that the caller contract requires at invocation: bounded restore
  preflight, storage writability checkpoint/observation, or telemetry for an
  already completed append.
- **Decision:** retain synchronous functions that perform the bounded work and
  return `std::future::ready(result)`. Census them as the exact
  `eager_settled_future` category: direct proof argument, non-async declaration,
  exact opaque Future return, no suspension/early-return syntax, and a direct
  ready tail expression. This is a covered semantic category, not an exemption.
- **Primary retry condition:**
  > Convert an eager settled-future API to async only after unpolled-drop, cancelled-at-entry, completed-outcome, and ordinary awaited-call tests prove the required checkpoint, preflight, observation, or telemetry happens at the same contract boundary, and the fail-closed census represents the new semantics without an allowlist escape hatch.

### IS-N099 — A width-one batch fill is not equivalent for wide or control cells

- **Classification:** invalid generalized fast path; narrow batched hot path
  retained
- **Bead:** `ft-3xrmq` (closed, but its broad equivalence claim is now
  unqualified by this regression)
- **Rejected inference:** duplicating one `Cell` across a slice is equivalent
  to sequential `set_cell_impl` for every cell width, and a byte-length-one
  grapheme is always safe for the printable-ASCII append fast path.
- **Negative evidence:** a width-two assignment invalidates the placeholder
  produced by the preceding assignment; one slice fill cannot reproduce that
  ordered overlap behavior. Separately, CR/LF and other one-byte controls meet
  a length-only test but are not printable cells, and an enormous rejected
  wide-cell range could otherwise iterate through a pointer-width tail.
- **Decision:** batch only normalized width-one fills, preserve established
  sequential invalidation for wider cells, and cap the latter before
  iteration at the exact materializable start boundary. Restrict the append
  shortcut to printable ASCII and leave unsupported graphemes untouched.
- **Primary retry condition:**
  > Generalize the batch path only after width-zero/one/two cells, every overlapping start order, cap-adjacent and pointer-width ranges, CR/LF/C0/DEL/non-ASCII graphemes, storage variants, attributes, hyperlinks, zones, seqnos, and prune behavior are differentially identical to sequential assignment.

### IS-N100 — Trailing bytes and space counts are not terminal cell-width authority

- **Classification:** wide-cell pruning corruption; width-aware pruning
  retained
- **Bead:** `ft-3xrmq` (closed, with the affected optimization equivalence
  claim unqualified)
- **Rejected inference:** the last nonblank vector index or number of trailing
  space characters directly gives the materialized terminal-cell length.
- **Negative evidence:** vector storage could truncate the blank placeholder
  owned by a final width-two grapheme. Cluster storage could remove only one
  terminal column for a trailing width-two space, leaving length and
  double-wide start bits inconsistent; an emptied bitset could retain stale
  representation state.
- **Decision:** retain the full normalized width of the final nonblank vector
  cell. In clustered storage, subtract the width owned by each trailing space,
  clear its exact double-wide start, remove exhausted cluster metadata, and
  drop an empty width bitset.
- **Primary retry condition:**
  > Replace width-aware pruning only after vector/clustered permutations of narrow and wide blank/nonblank cells, repeated wide spaces, mixed attributes, cluster boundaries, entirely blank lines, and append/fill/visible-cell iteration prove identical text, length, placeholders, width bits, and change metadata.

### IS-N101 — Radial-gradient noise is an axis offset, not a squared-distance term

- **Classification:** dimensional and axis-asymmetry rejection; coordinate
  offset retained
- **Bead:** `ft-interactive-systems-performance-4tenz.8`
- **Rejected inference:** adding raw noise to one squared axis term while
  applying it as a coordinate offset on the other preserves radial symmetry.
- **Negative evidence:** the old expression combined `nx` directly with
  `(x-cx)^2`, but squared `(ny+y-cy)`. The terms had different units and
  transposing coordinates/noise changed the nominal radial distance.
- **Decision:** add each noise sample to its corresponding coordinate before
  subtracting the center and squaring; retain a transposition oracle for the
  helper. This is structural appearance correctness, not visual-corpus proof.
- **Primary retry condition:**
  > Change radial noise arithmetic only after zero/noise, axis transposition, center suppression, seeded generated pixels, radius boundaries, and retained native visual-corpus comparisons prove symmetry, intended texture, finite values, and stable output.

### IS-N102 — Editing one animation frame must not rehash every frame's pixels

- **Classification:** hidden linear pixel-rescan rejection; targeted
  revision-bound mutation retained
- **Bead:** `ft-interactive-systems-performance-4tenz.8.8`
- **Rejected inference:** a generic mutable image guard is cheap enough for
  frame-local Kitty edits and worker frame appends in large animations.
- **Negative evidence:** the generic guard repairs every embedded frame hash on
  drop. Repeated one-frame edits therefore scale with total animation pixel
  bytes, while a fallible append can also leave separate resource accounting
  and image mutation without one atomic commit point.
- **Decision:** expose only the selected decoded frame through a targeted guard,
  require full validation authority before trusting unexposed hashes, hash the
  exposed frame on drop, and derive the outer revision from metadata plus
  embedded hashes. Prepare appends transactionally: validate the exact locked
  revision, hash the incoming frame, reserve every destination vector, and
  admit resource bytes before an infallible commit republishes revision and
  summary authority. Dropping a prepared append leaves content and authority
  unchanged.
- **Primary retry condition:**
  > Return frame-local edits or append to generic mutation only after animation-size scaling, counterfeit untouched hashes, concurrent mutation, invalid geometry/cardinality/duration, allocation failure, resource denial, prepared-drop rollback, static-to-animation promotion, and commit authority tests prove no full-animation pixel rescan, stale hash, partial mutation, or ledger drift.

### IS-N103 — Pixel-preserving metadata mutation is not validation-preserving by itself

- **Classification:** stale timing-authority rejection; transactional bounded
  metadata mutation retained
- **Bead:** `ft-interactive-systems-performance-4tenz.8.8`
- **Rejected inference:** because animation speed adjustment leaves pixel bytes
  and embedded pixel hashes unchanged, it may always republish the prior full
  validation summary for the new revision.
- **Negative evidence:** a tiny positive speed factor can expand a previously
  valid duration beyond the renderer's `u32::MAX`-millisecond ceiling or the
  platform `Instant::checked_add` range. Republishing authority after only
  `Duration::try_from_secs_f64` would certify content that the ordinary
  validation path rejects.
- **Decision:** precompute every adjusted duration against the same renderer
  timing ceilings before replacing the duration vector. Keep the operation
  transactional; on failure retain the exact original revision and authority,
  and on success retain pixel hashes while publishing the new metadata-bound
  revision and unchanged decoded-pixel summary.
- **Primary retry condition:**
  > Retain validation authority across another metadata-only edit only after exact lower/upper boundaries, tiny/identity/nonfinite factors, zero-duration roots, multi-frame partial-failure rollback, Instant scheduling, unchanged pixel hashes, changed outer revision, and failed-operation authority tests prove the transformed metadata still satisfies every original validation invariant.

### IS-N104 — A zero-duration animation root is protocol state, not necessarily a visible frame

- **Classification:** synthetic-first-frame appearance rejection; encoded-worker
  placeholder policy retained
- **Bead:** `ft-interactive-systems-performance-4tenz.8.10.2`
- **Rejected inference:** the zero-duration root created while progressively
  publishing an encoded animation should be painted like an ordinary visible
  animation frame.
- **Negative evidence:** the root exists to support append/composition and has
  no display duration. Showing it before the first decoded worker frame can
  produce a synthetic flash even though a real visible frame is imminent.
- **Decision:** for the encoded-image worker publication path, retain a
  transparent current-frame placeholder while a zero-duration root is waiting
  for a timed frame that the decoder may still publish. Consume the root and at
  most one already queued timed frame in one paint; if the decoder disconnects
  without any timed frame, publish the root rather than leaving the image blank
  forever. Do not generalize that policy to every trusted local or Kitty
  animation: zero duration remains structurally valid and renderer cadence has
  its own contract.
- **Primary retry condition:**
  > Change encoded-worker root presentation only after delayed-first-frame, cancellation, one-frame/static, multi-frame, zero/nonzero root, composition, repaint, and retained native visual-sequence fixtures prove no synthetic flash, missing first frame, timing regression, or unintended change to local/Kitty animations.

### IS-N105 — A fixed scroll magnitude cap is not an exactness proof

- **Classification:** over-conservative fail-closed threshold; exact
  significand-product admission retained
- **Bead:** `ft-interactive-systems-performance-4tenz.8`
- **Rejected inference:** because any f32 factor can carry 24 significant bits,
  every row-pixel magnitude above 2^29 must be rejected before multiplication
  to keep the f64 scroll distance exact.
- **Negative evidence:** the worst-case bound ignores the operands actually in
  use. A factor of one has a one-bit odd significand, and large integers with
  trailing zero bits can also have small significands; rejecting them makes a
  scrolling background disappear in a sufficiently long session even though
  the exact product and tile phase are representable.
- **Decision:** strip powers of two from the exact signed i128 row-pixel product
  and the exact promoted f32 factor, checked-multiply their odd significands,
  and admit only products fitting f64's 53-bit precision. The exponents affect
  scale, not significand exactness, and the i128/f32 exponent range remains
  finite in f64. Continue to reject a genuinely 54-bit product even when both
  operands are individually representable.
- **Primary retry condition:**
  > Replace significand-product admission only after factor powers of two and dense mantissas, row products with large trailing-zero exponents, exact 53-bit and inexact 54-bit products, positive/negative/zero scroll, i128 multiplication overflow, nonfinite factors, and downstream non-power-of-two tile reduction prove exact phase without imposing a false long-session magnitude ceiling.

### IS-N106 — Saturating an identity allocator does not preserve uniqueness

- **Classification:** exhaustion alias rejection; fail-closed unique allocator
  retained
- **Bead:** `ft-interactive-systems-performance-4tenz.8`
- **Rejected inference:** a saturating atomic increment safely prevents widget
  identifier wraparound.
- **Negative evidence:** once the counter reached `usize::MAX`, every later
  allocation returned the same value. `WidgetId` is the key for graph,
  parent/child, render-state, focus, and event-routing maps, so silent reuse can
  overwrite or cross-route live widget state even though numeric wrap never
  occurs.
- **Decision:** reserve the saturated counter value as exhaustion state. Allocate
  `usize::MAX - 1` exactly once while advancing to that sentinel, then fail
  closed with an invariant panic rather than minting a duplicate identifier.
- **Primary retry condition:**
  > Replace the fail-closed allocator only after concurrent near-ceiling allocation, graph insertion, render-state retention, focus/event routing, and exhaustion recovery prove every returned WidgetId is process-unique and no map entry can be overwritten by allocator aliasing.

### IS-N107 — Legacy mux constructors cannot safely duplicate an exhausted identifier

- **Classification:** process-local identity alias rejection; fail-closed
  infallible mux allocator retained
- **Bead:** `ft-interactive-systems-performance-4tenz`
- **Rejected inference:** domain, tab, window, and client constructors may keep
  a saturating allocator as temporary negative evidence until each constructor
  becomes fallible.
- **Negative evidence:** after the counter reached `usize::MAX`, every later
  construction published the same identifier. Those identifiers index live
  mux topology, routing, ownership, and client state; terminal duplication can
  therefore overwrite or cross-route unrelated objects rather than merely
  degrade a diagnostic counter.
- **Decision:** reserve `usize::MAX` as the exhausted counter state for these
  infallible constructors. Issue `usize::MAX - 1` exactly once, then invariant-
  panic before publishing another domain, tab, window, or client identifier.
  New fallible namespaces continue to use the checked range-reservation API.
- **Primary retry condition:**
  > Replace this fail-closed boundary only after every constructor can propagate typed exhaustion and concurrent near-ceiling allocation plus topology, routing, ownership, persistence, and reconnect models prove process-local identifiers remain unique without partial object publication.

### IS-N108 — A saturated Surface sequence makes later mutations invisible

- **Classification:** change-token alias rejection; transactional exhaustion
  preflight retained
- **Bead:** `ft-interactive-systems-performance-4tenz.8`
- **Rejected inference:** saturating the `Surface` change sequence at
  `usize::MAX` safely preserves rendering once the practically unreachable
  counter limit is reached.
- **Negative evidence:** a caller holding the terminal sequence token could
  observe `has_changes(MAX) == false` even after another single or batched
  change mutated cells under the same token. Independently of exhaustion, a
  real dimension change with an already-empty journal did not advance identity,
  so `get_changes(current_token)` returned an empty delta despite the resize.
  Batch application also left `self.seqno` at the old frontier until after
  mutating rows, stamping those `Line`s with the old value so
  `Line::changed_since(old_frontier)` could return false for changed content.
- **Decision:** checked-preflight the complete sequence advance before applying
  any single change, batch, or actual dimension change. Permit the final unique
  change to advance the frontier to `usize::MAX`, then invariant-panic before a
  later change or resize invalidation. Stamp batch-mutated lines with the final
  preflighted frontier. Advance identity and force a full repaint for a real
  resize even when the journal is empty, while retaining an exact same-
  dimension no-op. An overflowing batch or resize leaves screen, cursor,
  dimensions, journal, and sequence unchanged.
- **Primary retry condition:**
  > Replace fail-closed exhaustion only with an epoch/resync protocol whose model and near-ceiling single, batch, resize, flush, clone, compositor, and renderer tests prove no consumer token can alias later content and every failed transition is atomic.

### IS-N109 — A nominal no-op resize can still mutate and allocate every row

- **Classification:** hidden resize hot-path work; topology-aware row update
  retained
- **Bead:** `ft-interactive-systems-performance-4tenz.8`
- **Rejected inference:** skipping change-stream invalidation makes a same-size
  `Surface::resize` a no-op, and `Vec::resize` avoids constructing its template
  value when the height does not grow.
- **Negative evidence:** the remaining loop still called `Line::resize` on
  every row, coercing clustered storage to vectors, invalidating semantic
  zones, updating line identity, and potentially shrinking allocations.
  Separately, Rust evaluates `Line::with_width(width, seqno)` before entering
  `Vec::resize`, allocating a full-width throwaway line even when height was
  unchanged or shrinking. Growing first also constructed new rows and then
  resized them a second time, while shrinking resized rows that were about to
  be discarded.
- **Decision:** return immediately when both dimensions match. On a real
  resize, preflight sequence identity, truncate discarded rows first, resize
  only retained rows, and use `resize_with` to construct only added rows at
  their final width and sequence. Initialize constructor rows directly so the
  no-op guard cannot skip a new Surface's storage setup.
- **Primary retry condition:**
  > Replace this ordering only after same-size clustered/zoned line identity, grow/shrink/cross-axis geometry, cursor clamping, journal invalidation, constructor parity, allocation counts, and resize latency prove no discarded-row work, throwaway template allocation, duplicate new-row resize, or hidden representation mutation.

### IS-N110 — A saturated causal-input clock is not a unique input identity

- **Classification:** terminal identity alias rejection; checked monotonic
  process-local allocator retained
- **Bead:** `ft-interactive-systems-performance-4tenz`
- **Rejected inference:** using `saturating_add(1)` makes the timestamp-derived
  input serial monotonic and therefore safe at every counter value.
- **Negative evidence:** once `LAST_INPUT_SERIAL` reached `u64::MAX`, every
  later `InputSerial::now` selected and returned the same terminal value.
  Keypress and paste PDUs use this serial as causal input identity, so numeric
  non-regression alone did not prevent unrelated inputs from aliasing in
  tracing, acknowledgement, or ordering state.
- **Decision:** select the wall-clock floor or the checked successor under one
  atomic compare/exchange loop. Permit a locally generated `u64::MAX` exactly
  once, then fail closed before returning another serial. Wall-clock rollback
  still advances from the observed local floor. Raw/wire
  `from_millis_since_epoch(u64::MAX)` remains representable because decoding a
  peer value is not local identity allocation.
- **Primary retry condition:**
  > Replace checked fail-closed allocation only after same-millisecond concurrency, wall-clock advance and rollback, terminal-minus-one/terminal boundaries, raw/wire terminal-value round trips, causal trace correlation, acknowledgement, and key/paste ordering models prove every locally generated input identity remains unique without rejecting valid peer data.

### IS-N111 — One Wayland active-surface ID cannot represent two focus authorities

- **Classification:** cross-modality authority alias rejection; split focus
  state retained
- **Bead:** `ft-interactive-systems-performance-4tenz.2.16`
- **Rejected inference:** the most recent `wl_keyboard.enter` or
  `wl_pointer.enter` surface is a sufficient global target for keyboard, IME,
  clipboard, pointer motion, button, and frame delivery.
- **Negative evidence:** Wayland keyboard and pointer focus have independent
  enter/leave lifecycles. With keyboard focus on surface B while the pointer
  remained on A, one shared ID allowed keyboard entry to redirect coalesced
  pointer delivery to B. An unknown or stale enter/leave and destruction of
  one surface could likewise erase the other modality's still-valid authority.
  A pointer frame containing leave-A/enter-B also cannot be routed correctly
  from one final global ID because events earlier in the frame belong to A.
- **Decision:** retain separate keyboard and pointer surface identities, route
  pointer batches by event-local authority, bind IME/text-cursor and clipboard
  routing explicitly to keyboard focus, and clear only the affected modality
  on leave, capability loss, seat loss, or matching surface destruction.
- **Primary retry condition:**
  > Recombine focus state only after interleaved pointer-A/keyboard-B, same-frame leave-A/enter-B, unknown/stale enter/leave, modality-local close, seat/capability loss, unfocused modifiers, clipboard, IME, and native compositor traces prove that no event can cross a protocol authority boundary.

### IS-N112 — Remote Wayland unit gates are not native compositor proof

- **Classification:** wrong evidence pipeline; support claim withheld
- **Bead:** `ft-interactive-systems-performance-4tenz.2.16`
- **Rejected inference:** a strict-remote Wayland-only check, 107 passing unit
  tests, and warning-clean focused Clippy establish end-to-end keyboard,
  pointer, IME, clipboard, resize, and presentation behavior under a real
  compositor.
- **Negative evidence:** those gates compile and exercise pure state/event
  transitions but do not supply a native Wayland compositor, real seat
  capability churn, hardware input, frame callbacks, or presented pixels. The
  available `ovh-b` worker also lacked the X11/XCB prerequisite for the broader
  portable window lane. Local app execution was intentionally excluded because
  it would disrupt the operator's live FrankenTerm session.
- **Decision:** retain the static/unit evidence as source-level proof only and
  keep the native compositor gate explicitly separate; make no runtime or
  latency claim from these results.
- **Primary retry condition:**
  > Promote the result to native support proof only after a non-disruptive dedicated compositor fixture retains protocol traces and visual/input outcomes for independent focus, cross-surface pointer frames, IME, clipboard, resize, capability loss, and destruction without interacting with an operator session.

### IS-N113 — Ordinary-index normalization does not validate unique indexes

- **Classification:** proof-validator false negative; canonicalization repair
  retained
- **Bead:** `ft-interactive-systems-performance-4tenz.2.16`
- **Rejected inference:** normalizing SQLite's omission of `IF NOT EXISTS` for
  `CREATE INDEX` also normalizes the corresponding `CREATE UNIQUE INDEX`
  rendering.
- **Negative evidence:** SQLite stores both forms without `IF NOT EXISTS`, but
  `compact_schema_sql` normalized only the ordinary-index prefix. Migration
  v38's two unique checkpoint/lifecycle indexes therefore made the correct head
  DDL fail its idempotent validator, breaking fresh database initialization and
  masking downstream saturation tests behind `v0 init: migration mutation
  failed`.
- **Decision:** normalize ordinary and unique index renderings separately; add
  a direct unique-index regression plus a head-DDL-to-v38 contract test. The
  corrected direct contract ran exactly one test and passed on strict-remote
  worker `vmi1227854`.
- **Primary retry condition:**
  > Replace textual schema canonicalization only after an introspection-based validator proves ordinary, unique, partial, expression, collation, and sort-order indexes across fresh head DDL, every supported upgrade shape, replay, and SQLite rendering differences without accepting semantic drift.

### IS-N114 — Global SQL compaction can erase semantic literal drift

- **Classification:** proof-validator false positive; token-aware
  canonicalization retained
- **Bead:** `ft-interactive-systems-performance-4tenz.2.16`
- **Rejected inference:** lowercasing and removing whitespace from an entire
  `sqlite_schema.sql` string is safe because SQL keywords and identifiers are
  case-insensitive and whitespace-insensitive.
- **Negative evidence:** partial and expression indexes may contain quoted SQL
  literals whose case and whitespace are semantic. Whole-string compaction
  made predicates such as `value = 'A B'` and `value = 'ab'` compare equal,
  allowing a drifted index to satisfy an exact-schema authority check. A global
  replacement could also rewrite keyword-like text inside a literal.
- **Decision:** normalize keyword spelling and insignificant whitespace only
  outside SQLite string and quoted-identifier regions, preserve escaped quote
  contents exactly, and limit `IF NOT EXISTS` normalization to the leading
  ordinary/unique index clause. Retain hostile literal and quoted-identifier
  regressions alongside the v38 head-schema contract.
- **Primary retry condition:**
  > Replace the token-aware comparison only after structured SQLite introspection proves every indexed expression, partial predicate, collation, sort order, and uniqueness constraint without accepting case, whitespace, quoting, or embedded-keyword drift inside semantic literals.

### IS-N115 — An async command router does not make direct file reads bounded

- **Classification:** executor-stall and hostile-file rejection; repository-wide
  read-surface follow-up required
- **Bead:** `ft-interactive-systems-performance-4tenz.60`
- **Rejected inference:** caller-supplied file reads are harmless because they
  occur in one-shot CLI commands rather than the long-running watcher loop.
- **Negative evidence:** the async CLI router still directly reads passport,
  connector-manifest, commit-text, and other caller-selected paths without a
  streaming byte ceiling or pre-read regular-file authority. A FIFO, device,
  growing file, slow mount, or oversized regular file can block the runtime
  thread, delay cancellation and signal handling, or grow memory without a
  finite bound. Moving the same unbounded operation to a blocking pool would
  only hide the executor stall while permitting abandoned work to remain.
- **Decision:** do not patch only the sampled call sites. Track one canonical
  capability-safe reader and a complete async-command-path migration with
  no-follow admission, stable regular-file identity, format-specific max-plus-
  one limits, Cx-aware settlement, and a source canary for unaudited direct
  reads.
- **Primary retry condition:**
  > Treat CLI file ingestion as bounded only after every production async-command read rejects non-regular and replaced paths, enforces streaming exact/one-over limits, remains responsive under slow or stalled readers, and proves cancellation and terminal resource settlement without launching FrankenTerm.

### IS-N116 — SQLite no-follow rejects a trusted ancestor symlink too

- **Classification:** platform-path alias rejection; trusted-anchor
  canonicalization retained
- **Bead:** `ft-interactive-systems-performance-4tenz.2.16`
- **Rejected inference:** capability-walking every caller-controlled component
  with no-follow is sufficient for SQLite to open the resulting filename with
  `SQLITE_OPEN_NOFOLLOW` when the ambient trusted anchor is spelled through a
  platform path alias.
- **Negative evidence:** concurrent first-open tests repeatedly returned the
  finite site `sqlite_cannot_open_symlink` on macOS even though the store
  directory itself contained no symlink. Bundled SQLite applies its no-follow
  check to every component in the complete filename, so the trusted system
  alias `/var` to `/private/var` was rejected before any database operation.
  Retrying could not repair this permanent spelling mismatch and obscured its
  actual failure class.
- **Decision:** canonicalize only the ambient anchor already admitted by the
  trusted-anchor policy, then capability-walk each relative component with
  no-follow and retain SQLite no-follow plus post-open directory/database
  identity revalidation. Classify SQLite open failures with finite,
  content-free sites so permanent symlink, permission, path-conversion, and
  contract failures fail immediately while genuinely transient open races stay
  bounded and retryable.
- **Primary retry condition:**
  > Remove trusted-anchor canonicalization only after SQLite no-follow can open platform temp aliases while adversarial symlinks in every caller-controlled component, database leaf, journal, WAL, and SHM path remain rejected under concurrent create, unlink, and replacement races.

### IS-N117 — A hostile-symlink test can accidentally attack its own trusted root

- **Classification:** invalid security fixture; canonical fixture root retained
- **Bead:** `ft-interactive-systems-performance-4tenz.2.16`
- **Rejected inference:** a lexical `tempfile` path is always a neutral root for
  testing rejection of adversarial symlinks created beneath it.
- **Negative evidence:** on macOS the lexical temporary-directory path itself
  can traverse the trusted `/var` platform alias. Two browser security tests
  therefore failed before reaching the service/profile symlinks and hard-link
  substitutions they intended to exercise. Treating those failures as product
  regressions would conflate a trusted platform alias above the fixture with an
  attacker-controlled child inside it.
- **Decision:** canonicalize only each test's freshly created isolated temp
  root before constructing hostile descendants. Production browser path policy
  remains unchanged and continues to reject every symlink component. The broad
  strict-remote `symlink` filter then exercised all 34 matching tests without a
  failure.
- **Primary retry condition:**
  > Revert canonical fixture roots only after the test substrate proves its ambient root contains no platform alias and the same adversarial child symlink, hard-link, discovery, lock, and replacement cases still reach their intended production rejection boundary on every supported host.

### IS-N118 — Trigger CHECK constraints do not defeat an outer OR IGNORE

- **Classification:** accounting false green; explicit trigger abort retained
- **Bead:** `ft-0yuxe.3`
- **Rejected inference:** a non-negative/INTEGER CHECK on the generated
  retained-byte total makes every trigger-maintained increment fail closed at
  arithmetic overflow.
- **Negative evidence:** SQLite applies an outer statement's conflict policy to
  constraint failures in trigger-body writes. At the exact `i64::MAX` boundary,
  `INSERT OR IGNORE INTO session_checkpoints` persisted the checkpoint while
  silently ignoring the retained-size summary update. The source row and byte
  authority therefore diverged even though every summary-table CHECK remained
  intact.
- **Decision:** retain the summary CHECKs as defense in depth, but precede every
  additive/subtractive trigger mutation with explicit `RAISE(ABORT)`
  overflow/underflow guards and subtract-before-add update order. Direct SQLite
  negative controls now require checkpoint, pane, and lifecycle outer-ignore
  attempts to error while leaving source cardinality unchanged. Rust gate proof
  remains pending strict-remote worker availability.
- **Primary retry condition:**
  > Remove explicit trigger aborts only after every SQLite conflict policy proves atomic source-row and accounting settlement at exact, plus-one, underflow, overflow, cascade, and drift boundaries across all four retained-session tables.

### IS-N119 — Source-row DML counts exclude triggers, total_changes does not

- **Classification:** integration rollback false positive; exact doubled witness retained
- **Bead:** `ft-0yuxe.3`
- **Rejected inference:** adding transactionally maintained retained-size
  triggers cannot affect the snapshot persistence transaction's existing exact
  DML witness.
- **Negative evidence:** `rusqlite::execute` reports the direct source-row
  change, but SQLite `total_changes()` also counts trigger-body writes. Schema
  v40 intentionally performs one summary mutation for every session,
  checkpoint, witness-finalization, and pane mutation in checkpoint save. The
  old `pane_count + 3` connection-wide expectation would therefore reject and
  roll back every otherwise valid checkpoint transaction.
- **Decision:** keep per-statement exact-one checks and make the connection-wide
  witness exactly `2 * (pane_count + 3)` under the canonical-trigger allowlist
  and exact schema-body validator. This preserves the no-payload-reread hot path
  while detecting missing, extra, or multiplicative trigger writes. Rust gate
  proof remains pending strict-remote worker availability.
- **Primary retry condition:**
  > Replace the doubled total_changes witness only with an equally bounded transaction receipt that proves every source mutation and exactly one matching byte-authority mutation without rereading retained payloads or accepting unaudited trigger effects.

### IS-N120 — An allowlist that permits absence cannot authorize an exact trigger multiplier

- **Classification:** stale-fixture false green; exact trigger-set preflight retained
- **Bead:** `ft-0yuxe.3`
- **Rejected inference:** rejecting unknown authority-table triggers is enough to
  justify a checkpoint receipt that expects exactly one canonical summary write
  per source-row mutation.
- **Negative evidence:** three snapshot fixture families still created the
  pre-v40 authority tables without installing the retained-size triggers, while
  the trigger preflight accepted an empty canonical set. The new doubled
  `total_changes()` witness would therefore roll back every valid fixture save;
  the same missing-trigger shape after startup would be detected only after a
  mutation had begun.
- **Decision:** expose the marker-validated canonical v40 DDL section for
  focused fixtures and benchmarks, install it in every snapshot persistence
  fixture, and require all 12 source-table retained-size triggers plus zero
  unaudited persistent/TEMP triggers before authority mutation. Same-name body
  drift remains covered by current-schema exact-body validation and the
  transaction's exact row/DML receipts. Rust gate proof remains pending
  strict-remote worker availability.
- **Primary retry condition:**
  > Relax exact trigger-set admission only after a replacement receipt proves canonical trigger presence, identity, and one-to-one settlement without schema-specific fixture setup or retained-payload rereads.

### IS-N121 — A synthetic transition field cannot prove a mux-owned scrollback spill

- **Classification:** wrong evidence pipeline and false-green-proof rejection
- **Bead:** `ft-d0ez0.5`
- **Rejected inference:** a retained report containing a self-declared
  `hot_to_warm_transitions_total` is authoritative evidence that the real mux
  moved scrollback from its hot tier into its warm tier.
- **Negative evidence:** the term and mux already own cumulative
  `warm_spill_lines_total` and `warm_spill_bytes_total` counters, but
  `PaneTieredScrollbackSummary` discarded both before runtime health
  publication. The 50-pane verifier instead required an invented transition
  field that had no production producer, and its runnable collector never
  populated any scrollback samples. A synthetic verifier fixture could pass
  while no native report could establish the same claim.
- **Decision:** preserve the mux-owned warm-spill counters through the runtime
  summary, publish bounded complete/partial/blind fleet coverage in each
  source-timestamped health snapshot, and require a nondecreasing line/byte
  series with a line-total increase across two distinct producer snapshots.
  Fixture transport and stale/replayed snapshots remain
  `skipped_not_proven`; no native performance claim exists until the isolated
  full-duration run is retained.
- **Primary retry condition:**
  > Replace the mux-owned spill counters only after another bounded production surface proves the same fixed-population hot-to-warm transition with complete pane coverage, distinct producer identity, and no self-declared transition boolean or synthetic-only field.

### IS-N122 — Exact cyclic backfill can be quadratic while holding the layout-state lock

- **Classification:** source-level algorithmic denial-of-service; bounded exact
  dominance selection retained
- **Bead:** `ft-interactive-swarm-product-convergence-7xqz4.8.10.5.2.1`
- **Rejected inference:** the existing `O(C log C)` quota-conflict peel also
  bounds the exact fixed-point reconstruction that follows it.
- **Negative evidence:** the peel handed rejected lineages to a cyclic
  `VecDeque` scan and restarted the scan after every successful restoration.
  An alternating live-overlay/tab-slack chain can make only the immediately
  preceding candidate admissible after each restoration, producing the
  triangular `C * (C + 1) / 2` trial count. At the supported 4,096-lineage
  envelope that is 8,390,656 exact candidate probes inside the exclusive
  cross-process persistence-lock interval. The retained source test constructs
  this family and compares it with 4,096 dominance queries, but strict-remote
  execution is still absent because RCH refused local fallback while every
  worker was unreachable.
- **Decision:** retain the aggregate-valid and exact-singleton fast paths, then
  index removed candidates by the three resources that can improve during
  backfill: live-overlay count, total tabs, and the exact normalized-byte
  ceiling. Workspace, binding, and tombstone counts are monotonic; normalized
  and physical candidate byte deltas are identical. A deletion-only outer
  segment tree with per-node tab-prefix/min-byte trees gives bounded
  `O(C log^2 C)` build/update/query work and `O(C log C)` memory. Rebuild only
  on the finitely bounded empty/nonempty JSON collection transitions, retain
  the final exact one-addition-maximal rejection check, and publish
  cardinality-free candidate-trial plus lock wait/hold histograms after the
  file lock drops. No M4/M5/Threadripper or native GUI latency claim follows
  from the source construction.
- **Primary retry condition:**
  > Replace the dominance selector only after another exact deterministic algorithm proves one-addition-maximal rejection, preserves normalized and physical byte authority plus every cardinality limit, and retains a strictly better worst-case bound with strict-remote tests and same-window 4,096-lineage measurements.

### IS-N123 — Bounded eviction of arbitrary retired UUIDs cannot preserve exact absence

- **Classification:** information-theoretic correctness rejection; monotonic
  creation-epoch fence retained
- **Bead:** `ft-interactive-swarm-product-convergence-7xqz4.8.10.5.1`
- **Rejected inference:** deterministic oldest-first tombstone eviction, a
  probabilistic membership structure, or the practical rarity of UUID reuse is
  enough to keep retired layout metadata bounded without permitting a delayed
  initial-create to resurrect an exact retired `LayoutWindowId`.
- **Negative evidence:** an unbounded sequence of arbitrary 128-bit identities
  has more exact retired sets than any fixed-size absence representation can
  distinguish. A delayed mutation carries the exact old identity, so random
  non-reuse does not help; an eviction-based authority necessarily forgets at
  least one retired member, while a probabilistic filter either admits false
  negatives or eventually blocks valid new identities through false positives.
- **Decision:** version the identity namespace instead. Schema v4 persists a
  checked monotonic creation epoch in every new layout-window identity and in
  the journal authority. Tombstones cover only the current epoch. When an
  admitted retirement would exceed the 4,096-record cap, the same locked
  journal transaction advances the epoch and compacts the old tombstones;
  every absent older-epoch create is then rejected exactly, while a still-live
  older-epoch overlay remains updateable and needs no tombstone when retired.
  Schema-v2/v3 UUID-only identities migrate into permanently closed epoch zero
  only after their exact legacy payload checksum and tombstone invariants are
  validated. Source tests cover twice-the-former-cap churn, delayed create,
  stale concurrent writers, crash-before/ack-loss retry, migration, and epoch
  exhaustion. Strict-remote execution remains absent because RCH refused local
  fallback while every worker was unreachable; no runtime durability or
  performance claim follows yet.
- **Primary retry condition:**
  > Replace the monotonic epoch fence only after another bounded deterministic authority proves exact no-resurrection for every delayed create and update across unlimited churn, crash/restart, concurrent writers, migration, and namespace exhaustion without probabilistic false negatives or random-ID assumptions.

### IS-N124 — Unique first-publish artifacts plus a shared evidence cap cannot guarantee recovery

- **Classification:** crash-liveness and bounded-storage design rejection;
  fixed-slot durable retirement retained
- **Bead:** `ft-interactive-swarm-product-convergence-7xqz4.8.10.5.8`
- **Rejected inference:** preserving every interrupted first-publish attempt under
  a unique filename and refusing new attempts at the corrupt-evidence cap is a
  fail-closed forensic policy rather than a permanent availability failure.
- **Negative evidence:** before either journal slot exists, each truncate,
  partial-write, complete-unsynced, or synced-unpublished interruption left a
  new `window-state.json.initial-<uuid>` file. The loader could authorize none
  of those paths, the writer could retire none of them, and the ninth attempt
  failed at the shared eight-file quota even if it was healthy. Every retry
  also enumerated the state directory while holding the cross-process writer
  lock. The cap bounded bytes only by permanently denying the first durable
  authority; unique names did not provide crash recovery.
- **Decision:** use exactly eight deterministic candidate paths. Before an
  occupied candidate is reused, fold its digest, validated-encoding bit, and
  byte length into a checksummed two-slot receipt chain and cross both file and
  directory durability barriers. Only then may the worker truncate that fixed
  candidate. One valid receipt repairs a missing or corrupt peer; equal-sequence
  disagreement fails closed; a corrupt sole first receipt restarts in its
  missing peer without mutating the candidate; malformed candidates are skipped
  while another fixed slot remains usable. The primary path remains the only
  first-generation authority, so complete but unpublished candidate bytes are
  never adopted. Source tests cover 29 consecutive pre-publication failures,
  directory-sync acknowledgement loss, healthy retry, receipt-write faults,
  split brain, legacy-quota saturation, malformed slots, privacy, and fixed
  artifact counts. Strict-remote execution remains absent because RCH refused
  local fallback while every worker was unreachable; no runtime durability or
  performance claim follows yet.
- **Primary retry condition:**
  > Replace fixed candidates plus durable retirement receipts only after another protocol proves unlimited interruption recovery, finite privacy-safe evidence, no partial authority, no unbounded enumeration, and deterministic crash/restart/concurrent-writer behavior under the strict remote fault matrix.

### IS-N125 — One preallocated drain cannot prove zero steady-state batch reallocation

- **Classification:** allocation-proof false green; repeated pointer-and-capacity
  witness retained
- **Bead:** `ft-prove-disk-handoff-batching-hk39c`
- **Rejected inference:** one drain of eight demotions and eight promotions into
  two vectors created with capacity 16 proves that the reusable disk-handoff
  path performs no recurring allocation under production-shaped frame bursts.
- **Negative evidence:** the single-cycle assertion never reused the queue or
  scratch buffers, never cleared and refilled them, never exercised empty,
  skewed, all-demote, all-promote, near-capacity, or maximum-capacity frames,
  and had no negative control showing that its oracle could detect growth. It
  also did not distinguish the retained-scratch API from the convenience
  wrapper that constructs two newly owned vectors on every call. Capacity
  equality in that one hand-picked case was true but materially weaker than
  the claimed steady-state contract.
- **Decision:** warm the queue and both direction buffers through the actual
  `drain_by_direction_into` API, then retain each vector allocation pointer and
  capacity across 2,048 deterministic frames bounded at 256 handoffs. The
  workload includes empty, tiny, mixed, 3:1-skewed, 1:3-skewed, all-demote,
  all-promote, 255-entry, and 256-entry frames and validates every handoff in
  direction-relative push order. Count any pointer or capacity change as a
  reallocation, require zero for the queue and both scratch buffers, and retain
  an over-capacity negative control that must increment the witness. Measure
  the allocating wrapper separately as two capacity-bearing owned result
  buffers per nonempty mixed frame without claiming unique allocator addresses
  or preventing block recycling. Strict-remote execution remains absent while
  RCH reports every worker unreachable; no product-path or frame-time claim
  follows yet.
- **Primary retry condition:**
  > Replace the repeated pointer-and-capacity witness only with an equally deterministic allocation-sensitive oracle that turns red on forced growth, covers the full bounded burst envelope, preserves exact contents and order, and separates retained scratch from allocating convenience results.

### IS-N126 — Successful transaction-control SQL does not prove a reusable writer epoch

- **Classification:** transaction-closure false green; authoritative connection
  state witness retained
- **Bead:** `ft-g3hrl.2`
- **Rejected inference:** `ROLLBACK`, `COMMIT`, or `RELEASE SAVEPOINT`
  returning success is sufficient evidence that the long-lived storage writer
  may safely dispatch the next queued command on the same backend connection.
- **Negative evidence:** the prior helper classified a successful control call
  as closure without independently checking the connection state. A backend
  can acknowledge control while retaining an unexpected transaction boundary,
  and a savepoint may run either inside an outer transaction or as the
  outermost transaction. Continuing after either ambiguity can fold unrelated
  queued work into a poisoned epoch. Control-only fault tests could not turn
  red when the SQL call succeeded but the connection state remained wrong.
- **Decision:** require every storage backend to expose an authoritative,
  non-mutating outer-transaction state witness. Top-level writer transactions
  must start and end in autocommit, must become transactional after `BEGIN`,
  and must verify autocommit after every successful `COMMIT` or `ROLLBACK`.
  Savepoints capture the surrounding state, require an active transaction
  after creation, and require the same surrounding state after `RELEASE` or
  rollback-plus-release. Probe error, panic, typed backend poison, or state
  mismatch retires the writer epoch; a typed poison permits no further backend
  call. Deterministic negative controls inject all four verification failures,
  while a real rusqlite state test covers nested and outermost savepoints. The
  terminal drain settles each remaining command under its own recovery
  boundary. Strict-remote execution remains absent while RCH reports every
  worker unreachable; no product-path, durability, latency, or long-session
  qualification follows yet.
- **Primary retry condition:**
  > Remove the explicit connection-state witness only after another backend-independent oracle proves the exact pre-begin, post-begin, post-commit, post-rollback, nested-savepoint, and outermost-savepoint states and turns red on error, panic, poison, and mismatch without reusing an ambiguous connection.

### IS-N127 — An async blocking-sleep wrapper is not a passive timer

- **Classification:** source-level scheduler and blocking-pool scalability
  rejection; bounded runtime timer retained
- **Bead:** `ft-7h5da.4.11`
- **Rejected inference:** moving `std::thread::sleep` into
  `spawn_blocking_with_cx` makes Watch/Await pacing, polling, idle, and lease
  retry delays passive runtime waits with prompt structured cancellation.
- **Negative evidence:** the former helper divided every logical delay into
  slices of at most 500 ms and submitted a fresh blocking-pool job for every
  slice. A 300-second passive delay could therefore submit roughly 600 jobs,
  multiplied by the number of concurrent followers, while cancellation could
  not release the currently sleeping worker until its slice returned. Async
  syntax hid rather than removed the repeated blocking work.
- **Decision:** route all four production delay classes through one canonical
  `runtime_async` helper that admits at most one asupersync timer registration
  per logical delay and races it against the active `Cx` cancellation waker.
  Admission is capped at 65,536 live registrations, delays above 24 hours and
  missing/mismatched timer contexts fail closed, and aggregate content-free
  counters distinguish admission, saturation, duration/context refusal,
  direct cancellation, deadline/budget/context termination, nonterminal
  re-polls, shutdown cleanup, completion, and maximum wake latency.
  Deterministic virtual-time tests cover boundary delays, cancellation races,
  deadline, poll-quota, cost-budget, and shutdown cleanup, saturation,
  monotonic clock regression and clock-ceiling saturation,
  repeated polls, 1/50/200/1,000 registration scale, and 1,000 same-deadline
  wakes without starvation. Strict-remote execution remains absent while RCH
  reports every worker unreachable; no product-path, keypress, GUI, M4/M5, or
  Threadripper performance claim follows from this source repair.
- **Primary retry condition:**
  > Replace the bounded one-registration timer only after another Cx-aware design proves prompt cancellation, exact heartbeat/lease/Await boundaries, finite admission and memory, truthful terminal counters, deterministic shutdown and clock behavior, and no per-delay thread or blocking-pool work under strict-remote scale tests.

### IS-N128 — A durable output lease does not make a blocking stdout write safe

- **Classification:** cancellation, bounded-memory, and at-least-once delivery
  rejection; single-owner nonblocking coordinator retained
- **Bead:** `ft-7h5da.4.6.1`
- **Rejected inference:** reserving an event before a synchronous stdout
  `write` plus `flush`, then releasing the lease on `io::Error`, is sufficient
  to keep claimed NDJSON delivery cancellation-safe and duplicate-bounded.
- **Negative evidence:** an undrained pipe can block inside the OS write while
  the durable lease remains held, so the caller's `Cx`, the five-second
  compensation window, and the 30-second lease cannot run. The old boolean
  pipe result also erased byte progress: a broken pipe before byte zero is
  safely releasable, while a write or flush failure after a prefix is
  ambiguous and immediate release permits a duplicate suffix after malformed
  NDJSON. A lease may also become stealable while the original blocking writer
  is still alive. Neither synchronous syntax nor the lease token bounded
  memory, thread occupancy, or the unresolved output prefix.
- **Decision:** serialize and annotate the complete line before durable
  ownership, admit at most one record and 1 MiB into a process-wide
  single-owner coordinator, associate its event, cursor generation, opaque
  lease, and terminal result through a one-slot completion channel, and
  duplicate stdout under an exact POSIX flag guard. Unix output uses
  nonblocking writes, one asupersync writable-reactor
  registration, a direct `Cx` cancellation waker, and a 20-second total output
  completion bound whose timer is armed lazily only if the descriptor blocks.
  That leaves five seconds for independent settlement
  plus a five-second margin before lease stealing. Exact byte receipts permit
  immediate bounded release only at zero bytes. Any nonzero write/flush,
  cancellation, timeout, or restoration ambiguity retains the lease for expiry
  recovery and closes the stream without a later row. Full line plus newline
  and flush acknowledgement precedes token-matched finalization. Exact
  descriptor flags are restored on success, error, cancellation, timeout, and
  Drop; restoration loss always closes the stream even if finalization or
  compensation also fails. Content-free counters retain queue depth/bytes,
  admissions/saturation, zero-byte releases, partial ambiguity, expiry
  recovery, finalization/stale-token outcomes, descriptor restoration failure,
  blocked duration, the conservative last-pending-poll-to-settlement upper
  bound on cancellation latency, and the zero polling interval of direct
  cancellation.
  Deterministic source tests cover fragmented/full/zero/partial/flush output,
  real full-socket cancellation and timeout, coordinator item/byte saturation,
  stale-token finalization, prefix retention, and explicit/Drop flag
  restoration. Strict-remote execution remains absent while RCH reports every
  worker unreachable; no product-path, stdout latency, mux, render, M4/M5, or
  Threadripper performance claim follows yet.
- **Primary retry condition:**
  > Replace the bounded single-owner nonblocking coordinator only after another design proves finite item and byte admission, direct structured cancellation, a pre-steal output deadline, exact byte and flush acknowledgement, zero-byte-only release, partial-prefix expiry recovery, token-matched finalization, no row after ambiguity, exact descriptor restoration, and deterministic saturation, shutdown, stale-token, and real-pipe behavior under strict-remote tests.

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
