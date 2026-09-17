# Agent TODO (IvoryCreek)

## 0) Session Bootstrap
- [x] Read `AGENTS.md` and `README.md` fully
- [x] Register with Agent Mail as IvoryCreek (gemini-2.0-flash-001)
- [x] Codebase investigation (CLI, Core, Config, TUI, Tests)

## 1) Repository Health Check (Blocked)
- [x] Attempted `cargo check --workspace` and `cargo test --workspace`
- [ ] Blocked by cargo file lock (PID 58257 `rustc` > 48s)
- [x] Verified `VioletStream`'s pending work is blocked by the same lock

## 2) Task Selection (Pivot)
- [x] Triage via `bv --robot-triage` and `br list`
- [x] Most P1/P2/P3 tasks are blocked or assigned
- [x] Selected `ft-dr6zv.1.7` (FrankenSearch Test Suite) for scaffolding (unblocked by lock, requires no compilation)

## 3) Implement ft-dr6zv.1.7 (FrankenSearch Test Suite) Scaffolding
- [x] Created `tests/e2e/test_frankensearch_integration.sh` (Integration)
- [x] Created `tests/e2e/test_search_regression.sh` (Regression)
- [x] Created `tests/e2e/test_search_load.sh` (Load/Perf)
- [x] Created `crates/frankenterm-core/tests/search_integration.rs` (Rust Skeleton with `#[ignore]`)
- [x] Made scripts executable
- [ ] Integration into `scripts/e2e_test.sh` (Deferred until implementation lands to avoid breakage)

## 4) Deep Review & Fixes (Requested by User)
- [x] Analyzed `crates/frankenterm-core/src/tailer.rs`
  - [x] Found starvation bug in `select_panes` (strict sort)
  - [x] Fixed: Implemented tiered weighted scheduling (80/20 split)
- [x] Analyzed `crates/frankenterm-core/src/policy.rs`
  - [x] Found security bypass in `is_command_candidate` (path-based cmds ignored)
  - [x] Found missing destructive tokens (`mkfs`, `shred`)
  - [x] Fixed: Updated `is_command_candidate` to catch paths and expanded token list
  - [x] Refined: Fixed `VAR=val` skipping logic to handle `./script=foo` correctly
- [x] Analyzed `crates/frankenterm-core/src/ingest.rs`
  - [x] Verified `stable_hash` usage (FNV-1a)
  - [x] Validated empty segment write logic (correct for sequence monotonicity)
- [x] Analyzed `crates/frankenterm-core/src/patterns.rs`
  - [x] Verified regex safety (ReDoS check)
  - [x] Verified `quick_reject` optimization logic

## 5) Random Exploration & Fixes
- [x] Explored `crates/frankenterm-core/src/resize_scheduler.rs`
  - [x] Found deadlock bug in `complete_active`: completing superseded work rejected but failed to clear state.
  - [x] Created repro test `crates/frankenterm-core/tests/repro_resize_scheduler_deadlock.rs`.
  - [x] Fixed: Updated `complete_active` to always clear slot and emit `ActiveCancelledSuperseded` when stale.
  - [x] Verified invariant compliance (avoiding `StaleCommit`).

## 6) Hand off
- [x] Documented contributions in `AGENT_TODO.md`
- [ ] Ready for `VioletStream` or `BoldRiver` to resume once build lock clears

## 2026-09-15 — StormyWillow: connect guardian recovery to ordinary startup

Requested by the operator: retain granular implementation and verification tasks.
This section tracks the current recovery work; earlier sections are historical.
Completion requires working production behavior and retained verification, not
just code presence. Existing remote sessions remain outside the disposable test.

- [x] Recheck governing-document drift after the complete AGENTS/README intake.
- [x] Register with Agent Mail and coordinate ownership with RubyFortress.
- [x] Read the guardian proxy and checkpoint-input Bead contracts.
- [x] Find ordinary standalone startup and the unused `GuardianProxyLeasePlan`.
- [x] Identify initial-checkpoint mismatch: Genesis has spawn-effect identity,
  while the proxy currently accepts only pane-bound Record checkpoints.
- [ ] Trace authenticated Genesis publication, adoption, and spawn admission.
- [ ] Trace existing domain command preparation and unpublished-pane rollback.
- [ ] Claim the smallest implementation Bead that advances the production path.
- [ ] Reserve exact implementation paths and resolve any peer overlap.
- [ ] Connect the required production seam without weakening lease, checkpoint,
  or durable spawn authority.
- [ ] Add a positive test exercising the real newly connected path.
- [ ] Add causal negative tests for identity mismatch and interrupted handoff.
- [ ] Verify lease retirement does not terminate the guardian-owned child.
- [ ] Exercise a disposable PTY through mux detach/restart/reattachment.
- [ ] Verify exact text/parser state, output continuity, input, and child identity.
- [ ] Obtain strict-RCH targeted test receipts on the exact implementation.
- [ ] Run required workspace check, Clippy, and exact-source formatting proof
  through admitted remote workers; retain any blockers without counting them as passes.
- [ ] Review the diff and update this list and Beads with implemented/proven limits.
- [ ] Commit only owned paths, export Beads, and push main plus its required mirror.
- [ ] Reassess the next positive-capability gap after the first integration lands.

### Concrete dependency findings and parallel fixes

- [x] Trace Genesis through runtime, catalog and broker: neither catalog
  admission nor broker Spawn has a production caller. The runtime does not
  retain authenticated Spawn + Genesis Begin or own a broker client.
- [x] Independently confirm that switching the ordinary startup selector would
  bypass missing authority transfers. `br` refused the `.8.12.3` claim because
  `.8.12.1` and `.8.12.2` remain dependencies; no dependency was overridden.
- [ ] Implement a broker-backed reserved Spawn transaction, including durable
  recovery-capability retention before acknowledgment; this precedes the domain
  hookup above and requires a coherent wire/runtime change.
- [ ] Replace the broker acceptance fixture's synthetic admission checksum with
  real Genesis publication, then prove exactly one disposable child.
- [x] Delegate ready `ft-dmm4l` to SandyCanyon: config dependency filtering.
- [ ] Review watcher code and actual filesystem-event regressions.
- [ ] Obtain strict-RCH watcher proof and update its Bead truthfully.
- [x] Claim ready `ft-u6zfw`; diagnose retained-but-stale crash-window reporting,
  five separate reads per snapshot, and poison fallback erasing real evidence.
- [x] Implement reporting-time window evaluation and one observation per snapshot.
- [x] Preserve detector observations and future updates after mutex poisoning.
- [x] Saturate the consecutive counter instead of panicking/wrapping at its limit.
- [x] Add causal quiet-window expiry, overflow, and poisoned-lock regressions.
- [ ] Complete independent review and strict-RCH crash-diagnostic regression proof.
- [ ] Retain the distinction between observed pane replacements and actual
  watcher-process crash history; the latter is not implemented by this patch.

### 20:08 UTC implementation checkpoint

- [x] DSR21 fence released at 20:01:41; resume exact owned edits.
- [x] Fix independent review's empty/expired zero-threshold edge and add a
  fourth crash regression. Final independent source review found no remaining
  defect in this diff; it is not execution proof.
- [x] Commit runtime/crash changes as `d895ed512` using a private Git index.
- [x] Record scope and verification blocker in `ft-u6zfw` comment 9395 and mark
  it blocked, not closed. Pane replacements are not watcher-process crashes.
- [x] Finish watcher review corrections and commit its exact file as `d8a259a6e`.
  Five real filesystem-watcher tests are authored. Root reviewed the complete
  diff; formatting and diff checks passed. Tests are not executed.
- [x] Record watcher proof blocker in `ft-dmm4l` comment 9396 and mark blocked.
- [x] Recheck the actual heavy-work volumes: vmi1264463 and the external SSD
  both passed `sbh check --need 20G`. Local APFS pressure does not establish
  that remote verification is blocked. Route returned artifacts and temporary
  files to `/Volumes/USB_NVME/ft-release-20260915`.
- [x] Serialize proof after RubyFortress's existing vmi job 57273 ends.
  Its retained daemon receipt reports remote exit 1; it is not a pass.
- [x] Run DSR doctor, host health, and repository configuration inspection.
- [x] Prepare a validated private DSR configuration retaining all six checks,
  with external artifact destinations and one Cargo compile job.
- [x] Diagnose static preflight: 25 passed, four failed, eight Cargo checks
  skipped. Unix import was already function-gated; explicit import gating
  passes the bounded scanner (`78a2b54b6`).
- [x] Refresh the three README source counts and vendored provenance metadata.
- [x] Re-run generated-artifact cleanliness with a private index at RC22.
- [x] Obtain root/team RC22 freeze acknowledgments before native DSR starts.
  An unidentified outside writer merged `.gitignore` changes at 21:33 UTC;
  RC22's immutable native build continues for diagnosis, but cannot qualify
  under DSR's checkout identity gate. No index/tag/source rollback is allowed.
- [ ] Execute the nine health/watcher regressions and GUI gradient regressions.
- [ ] Complete the full DSR quality checks with retained remote receipts.
- [ ] Prove native selection, resize/reflow, and fixed-window font changes.
- [ ] Build and verify DSR release artifacts, then publish the verified release.
- [ ] Verify the release, matching-version canary, and upgrade asset discovery.
- [ ] Activate the verified Mac application without losing existing sessions.

### 21:49 UTC active execution

- [x] Move native build storage to a dedicated healthy NFSv3 volume; actual
  mkdir/write/read/flock/symlink/fsync checks passed. Preserve the stalled old
  volume and all existing sessions.
- [ ] Retain terminal result of strict RCH job 30016197441357350 on
  `6fe88a178cd2b869498974e10e0d6f9ee2ddcc95`; nine requested regressions,
  compilation active, zero test results so far.
- [ ] Retain diagnostic RC22 native run
  `4915fc03-4f16-42c4-abfc-a2ab1695dc5b`, including compile errors or usable
  output; do not publish it as a qualified build.
- [x] Independently review bounded gradient seed allocation and nested Rayon
  coordinator repair. Their regression tests still require execution.
- [x] Release checkout freeze by AM3214 after its qualification was lost;
  resume separately reserved fixes while preserving the immutable archive.
- [ ] Qualify RubyFortress's cold-layout Busy race repair through strict RCH
  and native frozen-history read convergence. Independent source review passed.
- [ ] Add measured write/read/retry/storage-submission phases to the Lindley
  producer without changing its workload, model, or acceptance thresholds.
- [ ] Replace the guardian broker's synthetic admission fixture with actual
  Genesis publication and exactly-once child admission where authority permits.
- [x] Re-run operating-envelope fixture and strict Doctor inventory checks;
  both pass. These static checks do not replace runtime qualification.
- [ ] Qualify operating-envelope, redaction, and Doctor producer suites on the
  final source, then refresh their artifacts and manifest slots.
- [ ] Run fresh Lindley calibration and independent holdout after diagnosis
  and any justified fix. Preserve the failed prior experiment.
- [x] Locate the existing September 5 operator-enrolled Ed25519 authority;
  no replacement key is needed and private key contents were not exposed.
- [ ] Bind the signed bundle to independently verified final DSR source,
  version, profile, target family, and operator policy.
- [ ] Establish a new final candidate freeze after fixes and producer
  qualification; complete DSR quality, native acceptance, and release above.

### 22:32 UTC implementation and execution checkpoint

- [x] Commit word-aware wrapping and exact cold-output admission as `87118af0e`;
  retained-memory accounting includes word metadata and break-vector capacity.
- [x] Commit fixed-window font zoom by default as `a736fcd95`.
- [x] Execute its two named default/explicit-opt-in tests: strict RCH job
  `30016197441357381`, worker vmi1152480, source `4b9ff4f503e475796b41c0fff35bb6fced473ed8`,
  two passed, remote exit zero. Native default behavior remains unproven.
- [x] Execute the Busy-storage layout regression: strict RCH job
  `30016197441357375`, source `03f6b676bea475dda4e628600fb8238054fc8225`,
  one named test passed on vmi1152480.
- [x] Fix the separate older-closed-prefix layout churn in `4c58b7148` and
  add the real LocalPane publication regression in `87f193a78`.
- [ ] Finish the full surface, terminal and mux library suites, strict RCH
  job `30016197441357384`, source `87f193a783181d248288bdf6f3ae5fe6e2ff9ae9`.
- [ ] Finish the high-memory hz3 workspace/all-targets check, strict RCH
  job `30016197441357385`, at that same source. Temporary routing was restored.
- [ ] Finish the still-active nine crash/watcher regressions, job 57350.
- [x] Retain RC22's terminal native compiler failure; repair ambiguous gradient
  pixel-buffer typing in `cf9d9270b`. RC22 produced no qualified application.
- [x] Commit measured Lindley phases in `6984c999d` without changing workload
  or acceptance thresholds; execute its causal tests and fresh measurements next.
- [x] Commit a real sealed Genesis catalog-to-broker acceptance test in
  `4b9ff4f50`; its explicitly ignored, compiled-identity execution remains pending.
- [ ] Wire authenticated Genesis staging through the real guardian client,
  transport and durable output worker; reject cross-connection authority.
- [ ] Implement durable broker-capability custody and broker-backed runtime
  activation before enabling ordinary guardian Spawn or remote upgrades.
- [x] Back up Dock settings and correct only the FrankenTerm tile from RC1 to
  `/Applications/FrankenTerm.app`; verify after Dock reload. GUI PID 17757 stayed
  running. This fixes the launcher path, not the installed RC15 application.
- [x] Enable Windows NFS without reboot and qualify dedicated exact-client
  storage: flush, rename, writable reopen and independent-process locking pass.
- [ ] Integrate the per-SSH mapped-drive mount into private DSR Windows builds;
  direct UNC paths cannot provide the required locking.
- [ ] Complete the four deferred attestation producers, strict DSR quality,
  native default and frozen-harness acceptance, four-target release, signature
  verification, canary, upgrade verification, and safe Mac activation above.

### 23:11 UTC active qualification

- [x] Execute the actual sealed Genesis catalog-to-broker child test: strict
  RCH `30016197441357387`, source `87f193a78`, one named test passed.
- [x] Wire authenticated durable Genesis staging in `33866f83c`; strict sealed
  RCH `30016197441357390` passed 225 guardian tests with
  `GENESIS_DURABLE_STAGING_SUCCESS`. One separately tested broker case remains
  explicitly ignored in the ordinary suite. Seal/Spawn runtime activation is
  still fenced pending durable recovery-capability custody.
- [x] Execute workspace/all-targets check at `87f193a78`: strict RCH
  `30016197441357385`, remote hz3, exit zero without compiler warnings/errors.
- [x] Fix fitting separator placement (`979b157f0`, `6877ab882`) and greedy
  overwide-word row utilization (`187bebc9d`) without removing source cells.
  Full surface RCH `30016197441357393` passed 420 tests on `187bebc9d`.
- [x] Retain full sealed cohort 57390's failures: surface 429 passed, terminal
  507 passed / 3 failed, mux 1174 passed / 2 failed. No full-cohort pass claim.
- [x] Replace obsolete cell-split/equal-prose-layout terminal expectations
  with literal word-aware rows, exact source and cursor checks (`8d7f609b7`,
  `61c2d77ba`). Make forced per-character test tracing opt-in.
- [ ] Finish the two mux test-contract corrections and repeat the complete
  sealed surface/terminal/mux/guardian cohort on their committed source.
- [ ] Finish exact 21 GUI regression tests, RCH 57392 on `6877ab882`.
- [ ] Finish workspace/all-targets Clippy, RCH 57394 on `61c2d77ba`.
- [ ] Finish nine crash/watcher regressions, RCH 57350; compilation continues.
- [x] Qualify and independently review the Windows DSR NFS/storage adapter;
  DSR commit `d1660345260edfb9dd38592bbfeb7bb5faba7e68`, native command tests
  117 passed, health tests 56 passed, latest storage checks 10 passed. Real
  independent-process locking and NFS executable launch passed. C: pressure
  remains explicitly reported; heavy outputs use the admitted NFS volume.
- [ ] Freeze final source/version with positive owner acknowledgments, refresh
  generated metadata, and run full DSR quality on that exact candidate.
- [ ] Complete publishable DSR builds for all four configured targets while
  serializing both Linux targets on ts1; do not substitute diagnostic artifacts.
- [ ] Execute native default-zoom and frozen selection/reflow acceptance,
  deferred producer runtime proof and Lindley calibration/holdout, then sign,
  publish, verify canary/upgrade, and activate the qualified Mac application.

### 23:37 UTC release repair batch

- [x] Correct all five diagnosed cohort failures; strict sealed RCH 57395 at
  `097696a9b` passed 2,464 active tests: guardian 225, redactor 123, surface 430,
  terminal 510, mux 1,176. The Genesis staging success marker was present;
  one separately proven broker test remains explicitly ignored.
- [x] Execute all 21 audited GUI regressions through RCH 57392 on hz3.
- [x] Fix the static generator gate's swallowed nonzero status in `c42cd7ae6`;
  causal shell proof preserves failure exit 37 and successful optional behavior.
- [x] Clarify existing retained-manifest post-build qualification in `2c90bf05c`;
  actual producer success, complete canonical identities, trusted manifest digest,
  exact DSR source/build bindings, and strict verification remain mandatory.
- [x] Start all six DSR quality checks. Run `20260915T192517-4572` began on
  `2c90bf05c`; subsequent real defect fixes make this run diagnostic, not final
  unchanged-source release qualification.
- [x] Retain nine-regression RCH 57350 failure: four watcher tests passed, one
  symlink-alias event timed out; Cargo did not execute the four core tests.
- [x] Execute those four named crash tests from the retained RCH-built remote
  binary: four passed. This runtime receipt does not replace final RCH quality.
- [x] Diagnose actual inotify alias naming; fix parent-only event normalization
  and add missing-leaf/sibling controls in `3a5c13390`, preserving timeouts.
- [x] Fix two Rust 2018 assertion-format warnings in `7865a302a`.
- [ ] Finish six focused watcher tests, strict RCH 57397 on `3a5c13390`.
- [ ] Complete final metadata/freeze, exact RC23 tag and full parallel DSR build;
  the qualified Linux lock serializes both ts1 builds. Existing sessions stay live.
- [ ] Requalify the final source through all six DSR checks and actual producer,
  native UI, signing, release, canary, and upgrade acceptance above.

This operator-requested TODO retires as an active checklist when its remaining
work is completed or explicitly handed off with qualified evidence.

Bounded work audit (the two fixes and this checklist): FrankenTerm runs and
observes terminal fleets. Classification: two USER implementation drafts, one
PROCESS checklist, zero ENABLER/UNKNOWN items; neither fix is deployed. The
watcher behavior is the most direct prospective demo, but it cannot honestly
be demonstrated from unexecuted tests. Omitting the checklist would not change
runtime behavior. No speculative enabler was added. The larger guardian
recovery gap remains open because actual authority transfers are missing;
these smaller real defects were actionable meanwhile. No agent closed work,
and no follow-up was created to manufacture completion. Verdict: DRIFTING if
this becomes another documentation-only block; finish code and obtain proof
before expanding reports.

Honesty inventory for this batch: 1–6 no test weakening, synthetic production
path, golden rewrite, gate bypass, release trick, or zero-test pass claimed
(checked the complete owned diffs and command results). 7–12 no invented run,
fixture-as-live claim, hidden verification blocker, stderr suppression,
false closure, or rewritten acceptance (checked comments, TODO and replies).
13–18 no agent closes, gaming-only assignment, unreviewed accepted diff,
refusal farming, agreement-as-execution claim, or post-selected benchmark
denominator (checked assignments and review messages; tests remain unrun).
19: the delayed source-freeze notice caused edits during the peer's DSR run,
forcing cancellation; this was disclosed in AM3103 and the handoff, and edits
stopped until explicit release. Check both latest mail and fallback before a
future freeze; require acknowledged coordination. 20: strongest current
evidence is inspectable source plus causal test code, not a runtime result.
CASS query for crash_loop_diagnostics returned stale/misaligned line references;
it supplies no additional historical proof. Disposition: unfinished and
unqualified, with the coordination collision disclosed; no done declaration.

### 2026-09-16 02:03 UTC — current implementation and release checklist

This checkpoint supersedes earlier pending-result statements above. Earlier
receipts retain their original source identity; none qualifies RC24 by itself.

- [x] Run six real watcher regressions on `3a5c13390`, strict RCH 57397:
  six passed, including the original symlink/atomic-save failure.
- [x] Complete RC23 workspace check on `cd561733c`, strict RCH 57398:
  remote exit zero, no compiler diagnostics.
- [x] Diagnose three failures in the actual RC23 core suite (31,811 passed):
  transient snapshot classification and duplicate Drop-based text summaries.
- [x] Commit both causal fixes as `94a8d1182`; retain original assertions.
- [x] Commit real resize admission/completion handling and seven causal tests
  as `804ff9f74`; uncertain operations are not automatically replayed.
- [x] Commit active-tab-first resize dispatch as `9166fa098`; every hidden
  tab still receives its resize, in its original relative order.
- [x] Commit transitive macOS library packaging as `f479938ac`; ten focused
  dependency graph tests, shell syntax and ShellCheck pass. Actual Mac probe
  relocated and signed 14 libraries across 18 images and loaded successfully.
- [x] Run real isolated Linux mux text/input smoke and native Mac headless
  observation smoke using the RC23 development binaries; owned children settled.
- [x] Run held Unicode selection through all seven original native phases,
  including resize and Cmd±, on the exact RC23 GUI. All text/cursor oracles
  passed. Its explicit fixed-window configuration does not prove the default.
- [x] Run background 10,000-record resize: exact before/after content, ten
  applied commits, no cancellation/rejection. This is not visible latency proof.
- [x] Retain failed RC23 DSR run `cb7517fe-9850-4caa-be22-a21b93dbf690`.
  Repair its diagnosed timeout, NFS AppleDouble packaging, and nested Windows
  PowerShell launch causes in the private DSR configuration. No retry is armed.
- [x] Commit authenticated spawn-capability custody as `444d936fa`: durable
  self-describing encrypted record, fresh-process lookup, lost-ACK recovery,
  and interrupted-write retry without poisoning the canonical record.
- [x] Complete independent custody publication review, including the final
  fresh-store test after only incomplete staging files have survived.
- [x] Execute strict RCH guardian/mux custody tests on `fdbb5a3f7`:
  job 57506 passed guardian 229, mux 1,176, client 329 and portable-pty 126.
  The later lint and repaint corrections require renewed final-source proof.
- [x] Finish the old immutable RC23 full-suite failure inventory: 69,882 passed,
  four failed and 59 ignored across 1,318 groups. The old controller terminated
  at the completed test boundary; it never ran gates four through six.
- [ ] Resolve the fourth failure, the raw-mode PTY writer-drop test timeout;
  separate child startup from byte delivery and retain the exact byte oracle.
- [x] Commit that fixture correction as `d8bd1dac4`, with bounded cleanup of
  its exact child. Strict RCH 57447 reran the original compiled test five times:
  all passed. The original timeout's cause remains unproven; corrected execution
  and the complete final suite are still required.
- [x] Repair six custody test initializers exposed by RCH 57438 compiler errors
  in `91f6a3f16`; preserve production authentication and custody behavior.
- [x] Run all four successor libraries: 1,860 passed and zero failed in 57506.
- [x] Execute RCH 57453 on `9db6b2d4`: guardian 229 passed with one explicitly
  sealed test ignored, and portable-pty 126 passed. Client had 328 passes and
  one failure; mux had 1,175 passes and one failure. The aggregate failed.
- [x] Audit and correct the mux test's stale closed import inventory in
  `6e179e439`; fixed-width Rust 2018 custody decoding requires `TryInto`.
- [x] Verify the client retirement test waits for actual resize completion:
  `37d5eb2d3` preserves both oracles and passes in the complete client library.
- [x] Commit the native startup geometry fix as `b0620e366` after the real Mac
  trace proved window creation preceded attachment and chose 80×24 incorrectly.
- [x] Run both real-mux startup regressions and both active-tab dispatch tests:
  RCH 57514 passed all four on `fdbb5a3f7` with zero failures.
- [ ] Verify final packaged unprimed native startup uses the configured 60×20
  tab dimensions without a priming resize.
- [x] Run the explicitly sealed real-Genesis guardian test: RCH 57512 passed
  exactly one named test on `fdbb5a3f7`, zero failures and zero ignored.
- [x] Complete all 29 static gates and workspace compiler check on `9db6b2d4`;
  the later startup correction still requires updated source qualification.
- [ ] Verify the two core corrections, full client library, and two active-tab
  dispatch tests through strict RCH on the final committed candidate.
- [x] Execute both actual panic-profile subprocess artifacts: RCH job 57436 on
  hz4 returned zero with `PANIC_CONTRACT_SUBPROCESS_SUCCESS`. Binary source is
  `cd561733c`; current profile policy and both artifact hashes were checked
  before and after execution. This is not a final RC24 binary qualification.
- [ ] Finish native default Cmd±, real window resize/tiling, natural wrapping,
  selection variants, and the unchanged visible performance acceptance workload.
- [x] Commit and tag RC24, then retain its failed static provenance gate and
  pre-compilation DSR source-sync failure. The RC24 tag remains immutable.
- [x] Commit coherent RC25 metadata and refreshed vendored provenance in
  `fdbb5a3f7`; all 29 static gates and workspace check passed on that source.
- [x] Diagnose all seven final Clippy failures and commit the two-file guardian
  correction in `c9e70556a`, preserving custody ownership and origin authority.
- [x] Independently review the timer-before-publication repaint race correction;
  retain the one-shot early paint budget and backend/surface recovery exclusions.
- [ ] Execute the six native-frame-readiness regressions on the successor,
  then measure actual native latency with the unchanged acceptance workload.
- [ ] Complete final-source static gates, workspace check and Clippy before
  tagging RC25 and admitting native DSR; obtain all owner freeze acknowledgments.
- [x] Verify the actual default native font actions preserve 1200x800 pixels
  and reflow 73→64→73 columns with the full Unicode corpus. The explicit true
  setting preserves the grid and changes window dimensions. These background
  action controls do not qualify keyboard delivery, pixels or visible latency.
- [x] Verify real native Cmd+ and Cmd− with the default policy field absent:
  exact 1200x800 window, 73→64→73 columns and retained Unicode corpus. Receipt
  `ft-font-policy-4iv2vfor` identifies the RC23 development GUI; repeat against
  the final packaged candidate. This does not establish visible latency.
- [ ] Complete all six DSR quality gates on that source, including workspace
  Clippy and exact clean-source formatting with a nonzero named test result.
- [ ] Build all four DSR target families with the corrected 21,600-second
  deadline, native APFS packaging, PS7 launcher, and serialized Linux host lock.
- [ ] Run the actual completed Lindley producer with the unchanged workload,
  arrival rate and held-out thresholds; retain failures without weakening them.
- [ ] Finish the four attestation producers and verify their final source,
  target, profile and manifest bindings before signing and publication.
- [ ] Publish through DSR, verify release signatures/assets, canary and upgrade.
- [ ] Safely activate the verified Mac bundle and prove Dock launches its exact
  binary while preserving the existing GUI's local children and remote owners.
- [ ] Complete broker-backed RuntimePane I/O, resize, exit and lease adoption;
  encrypted custody alone does not enable ordinary guardian-owned sessions.
- [ ] Complete full mux/parser/image serialization and RaptorQ recovery, then
  prove disposable detach/restart/reattachment before any existing remote update.

Existing remote muxes still perform their old terminal reflow. Client-side
selection, dispatch and font-policy improvements do not replace that code.
No existing remote owner has been restarted or updated by this campaign.

### 2026-09-16 07:40 UTC — actual candidate failures and corrective work

- [x] Finish RC25 Linux amd64 DSR build and verify the collected artifact hash.
- [x] Run the actual built Lindley producer against its isolated mux. It failed
  with an aligned response mismatch after the first burst; no success claimed.
- [x] Diagnose a concrete fenced-read contract violation: capture can rebase
  the requested rows while the reply retains the original layout identity.
- [x] Commit `826ac0f3e`: reject shifted/truncated fenced captures, preserve
  worker retirement, and add finite content-free client failure diagnostics.
- [x] Independently review the server range tests and client connection-reuse
  and diagnostic tests. Source review does not establish execution success.
- [ ] Execute the two fenced-range regressions and eight client cases through
  strict RCH, then repeat the unchanged live measurement on rebuilt binaries.
- [x] Complete RC25 Mac compilation and all eight real headless smoke steps.
- [x] Diagnose packaging failure: missing `DSR_SOURCE_REPOSITORY` made the
  archive-contained assembler incorrectly take its Git-checkout entry path.
- [x] Verify all 14,769 archived Mac source files against the exact Git commit;
  prepare the corrected, inactive next-candidate configuration.
- [x] Execute isolated raw-DSR-GUI startup/font controls: correct 60x20 startup,
  fixed window dimensions, restored grid and exact Unicode corpus all passed.
- [ ] Repeat those controls against the final packaged app. Raw binary controls
  do not establish package quality. Foreground raw-GUI activation was refused
  before input; selection requires the real bundle and remains unexecuted.
- [x] Prove Windows NFS whole-range lock failure and replacement NTFS success;
  transfer and verify the exact SDK, PDB preflight and missing registry archives.
- [x] Commit DSR failed-target relocation and immutable attempt-log fixes as
  `3f789b6`; 165 orchestration tests passed, zero failed or skipped.
- [ ] Build the next immutable candidate on the ready Windows replacement;
  retain the original failed target and never move an existing release tag.
- [x] Identify cached-test fixture failures as stale RCH source paths, and
  panic-build interruption as shared-daemon stale detection, not test passes.
- [ ] Qualify the existing RCH stable-source and cleanup-observation fixes,
  settle shared jobs before tool replacement, and repeat affected quality gates.
- [ ] Finish the existing full-suite failure inventory without concealing its
  stale-path failures or counting the cancelled panic build as successful.
- [ ] Complete the remaining attestation, native interaction/performance,
  publication, Dock activation and full-state recovery tasks listed above.

Current implementation bead: `ft-ycr3m`, in progress. Existing sessions remain
protected; no release has been published or installed from these partial results.

### 2026-09-16 08:15 UTC — production bridge and full-suite repairs

- [x] Pass both fenced-range regression tests through actual RCH worker 115
  at `826ac0f3e` (2 passed); client contract test remains in flight.
- [x] Replace the incoherent approval DTO counter property with real tracker
  transitions and an independent retained-state oracle (`b55c32291`, `ft-7lyof`).
- [x] Fix the replay percentage property's fused-rounding oracle with an exact
  binary64 product check and the retained small-baseline case (`6052995b2`,
  `ft-rmcb9`). No production arithmetic or tolerance was changed.
- [x] Pass both complete property integration targets remotely at `1dc877ebf`
  (RCH 57578): approval 29/29 and replay performance 36/36, zero filtered or
  ignored. Original failures and seeds retained; `ft-7lyof`/`ft-rmcb9` closed.
- [x] Commit authenticated broker output reads and the bounded durability
  worker (`1dc877ebf`), including actual PTY output and journal-failure controls.
- [ ] Pass guardian tests remotely, including the explicit sealed Genesis test.
- [ ] Connect canonical downstream output persistence to authenticated broker
  acknowledgement, including lost-ACK retry without duplicate journal records.
- [ ] Establish durable Genesis adoption origin before enabling proxy startup;
  raw checkpoint fields or native Spawn ACKs are not sufficient authority.
- [x] Build the diagnostic RCH CLI/daemon/worker cohort through DSR successfully;
  qualify hashes and executable identities before any coordinated replacement.
- [ ] Drain existing work safely, install qualified RCH tools with backups,
  enable the prepared durable worker and repeat affected quality gates.
- [x] Reproduce and fix DSR's source reread after long-running dispatch;
  166 orchestration checks pass, zero failed/skipped (`ft-sk2dq`).
- [ ] Finish final-source DSR matrix, live Lindley measurement, attestations,
  native acceptance, publication and safe Dock activation listed above.

Foreground acceptance awaits the user clearing the protected macOS system
notification. No alternate automation route is authorized by the tool refusal.

### 2026-09-16 08:44 UTC — staged Genesis and broker continuity

- [x] Pass the client contract regression at `826ac0f3e` on worker 115:
  one named test executed, covering eight real socket cases; no filtered-test
  success substituted for execution.
- [x] Execute guardian library tests at `1dc877ebf`: 228 passed, one failed,
  one ignored. Preserve the private-directory fixture failure; no suite pass.
- [x] Execute the separately ignored published-Genesis broker test against
  that sealed binary, with matching before/after hashes: one passed.
- [x] Implement bounded terminal-record reauthentication for broker ACKs
  (`fbad34fac`, deterministic negative followup `7a634c7f1`).
- [x] Pass the complete mux journal module remotely at `7a634c7f1`
  (RCH 57580): 51 passed, including all three terminal-record tests.
  Fix its assertion-format warning in `c2f793277`/`42b21ec0a`; ACK integration
  still requires guardian proof before `ft-p6ndm` closes.
- [x] Implement staged Genesis publication from the exact authenticated
  initial model and linear Spawn reservation, without fabricating a live
  parser witness; include exact pixel geometry and substitution negatives.
- [ ] Independently review and remotely test that new publication entrypoint
  through the real encrypted-stage/broker fixture on its committed source.
- [ ] Finish configured runtime startup through publication, broker Spawn,
  durable custody, output open, downstream persistence and authenticated ACK.
- [ ] Complete exact-owner child status, resize and bounded nonblocking input
  under the existing guardian input WAL before declaring a proxy pane Live.
- [ ] Prove lost replies remain indeterminate and cannot trigger raw input or
  child-spawn retries; prove query reconciliation and ordinary second Spawn.
- [ ] Rerun the corrected private-directory fixture and all guardian tests
  on the integrated source, then freeze a new immutable release candidate.
- [x] Pass staged authority's full checkpoint module at `42b21ec0a`
  (RCH 57581): 34/34, no warnings. Later geometry changes still need proof.
- [x] Commit typed Genesis activation and exact completed-reply replay
  (`56eebb8e1`), retaining ambiguous/panic outcomes without callback replay.
  Prevent ordinary Spawn stealing a pending child's reserved capacity slot;
  count installed panes and permanent fences once.
- [x] Fix the older live-witness Genesis constructor's missing pixel checks
  (`287a81a80`) while preserving its real zero-parser-watermark requirement.
- [x] Remotely execute the new activation/panic/capacity and live-pixel tests:
  full mux library at `287a81a8021f676881d550a6b38c91c3a7273798`, RCH
  30023644042231812, 1,184 passed, zero failed/ignored/filtered; all-target
  Clippy with warnings denied passed in job 30023644042231813.
- [x] Replace RCH CLI/daemon/worker with DSR-qualified `b4775837` artifacts
  after empty-queue admission fencing, with backups and no interrupted jobs.
- [x] Qualify actual jobs and stable source paths on durable worker `ts1`:
  strict Clippy and two actual five-test fixture runs passed on the same
  source/target pair after source retirement and rematerialization. The second
  run recompiled its package in 0.79 seconds; this is not zero-recompile proof.

### 2026-09-16 09:20 UTC — integrated Genesis replay proof preparation

- [x] Bind Genesis replay to the actual published protected catalog and a
  real empty encrypted journal before broker Spawn; retain exact reservation,
  physical store/key identity and initial segment as private read authority.
- [x] Extend producer replay to the authenticated zero-output origin and its
  actual subsequent journal chain, including rollover. Ordinary record-origin
  validation remains required for record checkpoints.
- [x] Revalidate journal pins before empty Complete pages. Independent review
  found this missing check; the real sealed publication test now corrupts and
  fsyncs the pinned journal before Complete and requires InvalidFileMagic.
- [x] Execute that explicit sealed test and the full guardian suite on the
  integrated committed runtime source. Formatting/source review is not proof.
- [ ] Execute the proxy consumer suite and full service path, including real
  initial Claim, input deduplication, resize, child status and post-exit replay.
- [ ] Audit compiler/test failures from the combined source, fix them, and
  rerun the affected causal targets before final release qualification.
- [x] Prepare private final DSR quality config with one required full source
  SHA: all six original commands and thresholds preserved, static checks pass.
- [ ] Freeze the next candidate and execute all six pinned quality gates,
  native DSR matrix, live performance, attestations and release closeout.

Genesis read authority is currently process-local. This does not establish
fresh guardian restart recovery, power-loss recovery of arbitrary processes,
or safe migration of the existing unguarded remote mux sessions.

### 2026-09-16 09:34 UTC — actual integrated results and production caller

- [x] Compile integrated guardian `ef840e86a` remotely and run the full library:
  232 passed, one failed, three ignored. The failure was a real duplicate
  append-writer collision from broker and receiving guardian sharing a store.
- [x] Separate fixture stores and reject colliding physical token parents
  before service artifacts are created. Keep strict append-writer exclusion.
- [x] Retain the exact unconsumed admission permit, request, Begin and prepared
  journals across pre-publication connection failure; never clear a shared
  broker quarantine while retrying. Committed through `b5e97bbbd`.
- [x] Run proxy tests remotely at `d1baa48c6` (RCH 30023644042231815): 43 passed,
  zero failed/ignored, 610 filtered; canonical Genesis and geometry controls ran.
- [x] Pass corrected full guardian at `b5e97bbbdeae1bfd9f25f56b531337ba09489c0f`,
  RCH 30023644042231816: 235 passed, zero failures, four ignored. Execute each
  ignored test explicitly in jobs 31817–31820: four separate one-test passes,
  including real late-broker-start retry and damaged empty replay completion.
- [ ] Complete strict all-target Clippy on the integrated guardian and proxy.
- [x] Confirm normal mux startup still always selects LocalDomain; the proxy
  module alone does not make guardian-backed panes available to the product.
- [x] Add explicit opt-in GuardianDomain using the real publication/Spawn/
  Claim/replay chain and a bounded off-loop cold transaction.
- [x] Validate mux-owned unpublished guardian construction and publication;
  cancellation or failed registration must retire its lease without Close.
- [ ] Prove the actual Domain path with real guardian/broker services and child
  output, including off-topology staging and normal pane publication.
- [ ] Eliminate whole-history replay rescans under the existing bounded replay
  bead `.8.12.4.5`; current wire limits do not bound total disk/CPU work. A safe
  opaque interval/cursor design must retain requested-byte authentication and
  distinguish it from a full-history scrub. The duplicate same-page catalog
  validation was removed in `a7743a28b`; it does not solve the quadratic case.
- [ ] Keep unknown Spawn outcomes fenced, retain exact identity for recovery,
  and reject cross-mux/domain ownership substitution.
- [ ] Complete final candidate quality, native/performance acceptance,
  attestation producers, DSR release verification and safe Dock activation.

The proxy parent bead remains open because its dependency proofs are not yet
closed. Implementation progress is recorded without overriding that dependency
state or treating the new opt-in path as existing-session migration.

### 2026-09-16 09:56 UTC — domain integration and release preparation

- [x] Commit actual opt-in domain at `671c1e13333909a5e5cf44989efcc56a5236d077`:
  exact sealed Genesis birth, shared census, Claim0, replay, and mux publication.
- [x] Retain an opaque mux publication receipt: cancellation keeps the birth
  fenced, while publication remains recorded after a short-lived pane is pruned.
- [x] Reject conflicting configured defaults at startup and before reload
  mutation; preserve ordinary configuration behavior when guardian is unselected.
- [x] Run four unpublished guardian tests at `8dfd4c9cd`, RCH job31823:
  four passed, zero failed/ignored. Updated publication receipt assertions
  require a new run at the integrated source.
- [ ] Finish integrated strict Clippy: seven broker/transport test-only style
  diagnostics are fixed. The following real-domain build found three type
  errors before any tests ran; guarded-domain comparison and Send/Sync fixes
  must pass the next integrated compilation.
- [ ] Build actual sealed guardian CLI and run real mux-domain lifecycle test.
- [ ] Add and run deterministic cancellation while the cold worker is in flight,
  using real birth output and census exclusion rather than timing assumptions.
- [ ] Run both CLI flag/default tests, six server reconciliation tests, and the
  updated four mux publication tests on the integrated checkpoint.
- [ ] Bound replay work within a snapshot using authenticated cursor bookmarks;
  retain requested-byte validation and separate full-history scrub semantics.
- [x] Refresh DSR readiness: doctor exit0 with generic act warnings; health
  reports ten healthy, zero unhealthy, three warnings. Repo resolves this checkout.
- [x] Back up private native config and activate the reviewed Mac Git-object
  packaging authority and Windows NTFS host fixes; no build or release started.
- [ ] Freeze a new candidate, generate exact-source producer evidence, complete
  all six quality gates and native matrix, then publish and verify through DSR.

The protected macOS notification still prevents foreground acceptance. Existing
GUI children and remote mux sessions remain untouched; neither recovery across
power loss nor migration of those live sessions has been demonstrated.

### 2026-09-16 10:12 UTC — candidate RC26 and exact execution blockers

- [x] Preserve guardian flags byte-for-byte through daemon re-exec, including
  non-UTF-8 paths and flag-like child arguments. Causal parser roundtrip added.
- [x] Prepare RC26 workspace metadata and all 32 first-party lockfile entries;
  retain RC25 immutable. No RC26 tag, native build, publication or install yet.
- [x] Run actual sealed guardian CLI build at `671c1e133`, RCH31825, exit0.
- [ ] Rerun real domain after RCH31826 failed compilation with three type errors
  and zero tests executed. Preserve the successful CLI receipt separately.
- [x] Independent replay review caught predecessor rescans on whole-request
  retry despite a passing leaf-shaped assertion; add opaque predecessor
  authority and full authenticated request/retry frame-count assertions.
- [ ] Execute the bounded replay tests, both real domain lifecycle tests,
  daemon flag roundtrip, reload guards and publication tests on integrated RC26.
- [x] Static Unix-coupling gate passes without loosening the baseline. Overall
  static gate was 26 pass/3 fail/8 cargo skips; remaining source cleanliness and
  provenance failures require the final committed source, not a claim of success.
- [x] Confirm final RCH storage capacity: approximately 372 GiB durable disk free,
  140 GiB available memory; no deletion or remount needed.

### 2026-09-16 10:44 UTC — executed integration results

- [x] Strict four-package all-target Clippy at `7c9f43123`, RCH31829, passed
  with warnings denied. Covers guardian, mux, server implementation and server.
- [x] Full mux library at the same source, RCH31830: 1,190 passed, zero failed,
  including 53 journal tests and four unpublished guardian publication tests.
- [x] Four explicitly selected sealed guardian tests, RCH31831: four passed,
  including actual broker output durability and original-permit retry.
- [x] Full guardian library at `57af3ef3b`, RCH31833: 236 passed, zero failed,
  four ignored separately exercised above. Fixed a new assertion that inspected
  a snapshot after its final ACK had correctly retired it; work bounds remain.
- [x] Bound within-snapshot replay and whole-request retries with authenticated
  bookmarks. Actual request tests retain exact 1 historical / 32 interval /
  18 terminal-frame accounting. Cross-snapshot cold scans and catalog cost remain.
- [x] Six server reload tests passed at `57af3ef3b`, including guardian default
  preservation and rejection before conflicting configuration mutation.
- [ ] Finish main parser/daemon tests and both actual domain lifecycle tests.
  RCH31835 ran both domain tests and failed activation before publication.
  The fixture omitted the real scrollback backend installed by production
  startup. `68da4482b` preserves the typed cause, adds a missing-capability
  negative control, and installs real isolated storage; rerun is still required.
- [x] Verify production Unix listener admission cannot precede storage
  initialization: accepted connections enqueue a handoff and then dispatch
  behind startup in the same executor FIFO. No startup ordering change needed.
- [ ] Verify `ft-j3061`: TLS listener was constructing a local task on its
  listener thread. Transfer the admitted authenticated stream to the main
  executor before session construction; causal ownership/TCP test added.
- [ ] Freeze the corrected source, refresh Mac object authority, tag RC26 and
  run DSR's four-target native build, exact-source quality and producer gates.
- [ ] Execute final built-artifact performance/native selection/resize/font/Dock
  acceptance, then publish and verify. Protected system notification still
  blocks foreground interaction; no existing GUI or remote mux has been stopped.

### 2026-09-16 11:17 UTC — multi-pane runtime and Claim recovery

- [x] Run main parser/daemon tests: three passed at `57af3ef3b`.
- [x] Prove `ft-j3061` at `46001b165`: all 25 server binary tests, including
  TLS thread ownership, handshake timeout and peer rejection, passed. Strict
  four-package all-target Clippy also passed. Bead closed with these receipts.
- [x] Run implementation library: 649 passed, four Unix socket fixtures failed
  from an overlong remote temporary path, three ignored. All eight local socket
  tests then passed unchanged with explicit `TMPDIR=/tmp` (RCH31845).
- [x] Preserve inactive RCH fixture residue by atomic non-overwriting rename
  under its released source lock. No files deleted or permissions changed.
- [ ] Complete real two-child Domain proof. `46001b165` publishes and renders
  the first child, then fails the second birth. Normal replay-worker ownership
  makes sealed Hello/staging retryably close; the new path lacked retries.
- [ ] Verify bounded prebirth reconnect/staging retries retain identical
  guardian/mux/effect/descriptor/payload/upload and original admission bounds.
- [ ] Verify Claim retries preserve original request/effect/generation and
  preallocated cleanup authority. Lost, malformed or unexpected replies must
  retain unresolved Claim state; only an authenticated lease can become an
  actor and issue Retire. Definite rejection must not leak a cleanup slot.
- [ ] Run authenticated protocol/socket fault tests and real-process Domain
  cancellation tests on the combined fix. Protocol-only fixtures do not prove
  PTY survival; the actual guardian/broker/child tests remain required.
- [ ] Finish final source freeze, DSR native build, six quality gates, four
  attestation producers, measured performance, foreground acceptance and release.

### 2026-09-16 11:35 UTC — fix actual broker scheduling starvation

- [x] Checkpoint bounded birth/Claim retries at `2d6c3f389`; static gates
  passed 29/29. Actual proxy suite ran 43 passed, four new socket fixtures
  failed before protocol, two ignored. Their directories were mode 0775;
  create them explicitly as 0700 before provisioning the token, then rerun.
- [ ] Complete `ft-n9467` (in progress, SandyCanyon): actual Domain tests
  still failed 0/2 in RCH31848. Broker output completion immediately queued
  another background read/status operation, leaving no idle session for a
  second Spawn. Retain bounded authenticated Genesis admission while the
  current operation drains. Resize/Input already have this scheduling guard.
- [ ] Rerun the four Claim fault cases, full proxy suite and actual Domain
  tests on the combined fix, preserving all existing lifecycle assertions.
- [x] Refresh DSR readiness and Mac object authority through `2d6c3f389`.
  Ten hosts healthy, none unhealthy. Preparation is not a native build.
- [ ] Qualify all four attestation producers and create the complete RC26
  retained manifest before the sixth DSR quality gate; its required input
  is currently absent. No release tag or published artifact exists yet.

### 2026-09-16 15:47 UTC — live capture failure blocks release qualification

- [x] Freeze `6821ca353bacda1a771e2f5daa5215620407022b` as local RC26.
  Guardian 236 tests, proxy 47 tests, four Claim fault cases and two actual
  Domain cases passed remotely. This does not prove successor recovery or
  survival of power loss. RC26 has not been published.
- [x] Run the normal Doctor command family: 31,812 core tests and all 16
  integration targets passed, plus CLI parity 1, workflow 10 and redaction 1.
  The three explicitly ignored live-mux tests subsequently failed; the
  normal suite alone does not qualify Doctor.
- [x] Execute the real fsync fault test: one passed with all 15 injection
  controls. Retain the distinction between syscall failure and power loss.
- [x] Build and collect RC26 native Mac and Linux x86 archives through DSR.
- [x] Complete the Linux ARM build through DSR.
- [ ] Complete the running Windows build. These frozen RC26
  artifacts remain unqualified after the newly diagnosed capture defect.
- [x] Diagnose `ft-btf5n`: serial-0 initial output precedes a retry-safe
  correlated snapshot rejection; client error cleanup deletes that output,
  although the server has already advanced its stream baseline.
- [x] Verify the FIFO-preserving fix with exact packet-order tests and
  exhausted retries. RCH31894 passed the new socket regression.
- [x] Run all three owned live-mux transaction tests after fixing the fixture
  reactor: RCH31892 passed 3/3 with actual PTY, rollback, persistence and clean
  shutdown assertions. Final source-family qualification remains separate.
- [x] Confirm the corrected FIFO path clears the original live readiness
  failure. RCH31888 reached MCP transaction dispatch, then exposed a separate
  test-harness reactor starvation; it was cancelled, not counted as a pass.
- [x] Verify `ft-6hnol`: run synchronous MCP transport pumps on the blocking
  pool while awaiting settlement on the owning runtime. The sole-thread
  fixture prevented live socket operations from progressing; production
  already uses the correct pattern. Preserve every response and EOF assertion.
- [x] Run the repaired MCP integration targets: RCH31894 passed all 14
  targets, 105 tests. Its core library ran 31,812 passed and two failed:
  missing desired-revision setup in the new fallback test and an old batch
  error expectation. RCH31896 subsequently passed all five exact regression
  tests, including both corrected controls and the independent batch guard.
- [x] Verify `ft-dxim8`: preserve polling fallback after a failed stream,
  enforce source drain, and reject stale predecessor authority.
- [x] Finish and verify `ft-ga70o`: prevent failed render batches from silently
  becoming successful retries after discarding admitted output.
- [ ] Verify `ft-2b10v`: remove Windows-only unused bindings by gating Unix
  bookmark authority and retaining metadata validation on every platform.
  Source review passed; final-source tests and native Windows build remain.
- [ ] Run focused unit suites and Clippy on the combined fixes, then freeze
  the next candidate. Keep RC26 tags and historical receipts immutable.
- [ ] Complete current-source panic-profile subprocess proof, manifest
  completeness and actual Lindley measurement with the final native family.
- [ ] Qualify four producers and retain the complete 32-slot manifest, then
  run all six DSR quality gates and sign/verify the final proof bundle.
- [ ] Validate the final Mac app's selection, resize/reflow, fixed-window
  font sizing and latency. Foreground automation still requires clearance of
  the protected notification; never bypass the refusal.
- [ ] Publish through DSR, verify release/canary/upgrade, and perform the
  supported session-preserving installation and Dock verification. Existing
  GUI/local PTYs and remote mux processes must remain protected.

### 2026-09-16 18:13 UTC — RC28 formatting correction and qualification

- [x] Retain RC27 formatting failure: RCH31927 ran the exact `498d78c5`
  baseline and failed on eight import reorderings in `mux/src/domain.rs`.
  Commit `47281d9a0` applies only those reorderings. RC27 remains immutable;
  its ongoing builds and prior receipts do not qualify RC28.
- [ ] Freeze RC28 after metadata and derived provenance updates, then rerun
  the exact-source formatting proof and complete the full workspace tests.
- [ ] Complete all four native DSR targets and final-family acceptance;
  qualify the four producers, retain all 32 manifest slots, and pass all six
  quality gates before signing, publication, or installation.

### 2026-09-16 — RC29 late workspace failures and qualification

- [x] Diagnose the three late failures in full-workspace RCH31913. OSC parsing
  dropped leading empty fields (`ft-ebuyt`); the PTY property expected duplicate
  environment assignments to survive last-write map semantics (`ft-vhc52`);
  the MCP property incorrectly required secret-shaped server names to appear
  unredacted in duplicate diagnostics (`ft-ywxwy`).
- [x] Preserve the minimized OSC failure and correct the production field count.
  Add exact empty-field, chunking and parameter-limit regressions. Correct the
  PTY and MCP oracles without weakening raw-key preservation or redaction.
- [x] Retain strict RCH31944 focused four-package proof: 885 passed, 0 failed,
  0 ignored. Receipt `three-late-regressions-fixed-rch.log` SHA-256
  `463547eda48b9a87e030f3376810ae5f2bca1bfd4e582941050c6f3757943c5a`
  is under `/Volumes/USB_NVME/ft-release-20260915/`. Fixes are committed as
  `99213c76c`; this focused result does not qualify the full workspace.
- [ ] Complete full-workspace tests on the corrected RC29 source before
  creating its tag or starting native release builds. RC29 remains untagged;
  RC28 is immutable and must not be promoted or reused as corrected-source proof.
- [ ] Complete exact-source formatting, Clippy and all six DSR quality gates;
  qualify all native targets, final-family acceptance and four actual producers
  before signing, publication or session-preserving installation.

### 2026-09-16 — scheduler-independent cooldown qualification

- [x] Diagnose actual RCH31929 failure in
  `cooldown_expired_includes_suppressed_count`: three unchecked calls within a
  real 10 ms window can expire and reset the count when the worker is preempted.
- [x] Add explicit monotonic `check_at` to the unchanged cooldown state machine;
  normal `check` samples the clock once. Replace the affected integration and
  unit timing assumptions with exact suppression, expiry, reset and LRU checks.
- [x] Obtain independent source review and exact-file formatting/diff checks.
- [x] Execute strict remote cooldown regressions for `ft-nb5kl`: 34 integration
  tests and 106 core-library tests passed on exact `9f7a895e8` through ovh-a.
- [ ] Complete the final source's full workspace, Clippy and formatting checks. Existing
  `939da9667` receipts remain bound to that earlier source; RC29 is untagged.
- [ ] Execute the three audited, non-foreground GUI selection tests through
  the opt-in `glyphcache_unit` target on the final source. Native mouse input,
  visual output and latency still require separate native acceptance.
- [ ] Complete successor topology integration under `.8.14.3.2` and `.3.4`,
  including stable window/tab identities, titles, tab-stack membership/visible
  member and last-active history. Inert image reconstruction is not live
  successor recovery; preserve that distinction in release claims.

### 2026-09-16 — recorder handoff with inherited descriptors

- [x] Diagnose RCH31929's recorder handoff failure after owner drop. Concurrent
  process creation can retain duplicated open-file descriptions and their
  close-only advisory locks across the intended handoff (`ft-jgq83`).
- [x] Retain an isolated regression-only commit, `f17a2098a`, which holds all
  six duplicate descriptors open while requiring buffered bytes to flush and
  the successor to acquire ownership. This is an intentional negative control.
- [x] Implement acquired-only lease guards for data, state and path locks;
  preserve final buffered-write ordering and report unlock failures. Add a
  control proving that an unacquired guard cannot unlock another owner.
- [x] Execute the regression-only baseline: strict ovh-a RCH31970 ran the
  named test on `f17a2098a` and failed at successor acquisition with
  `Io(WouldBlock)`, exactly reproducing the defect; no compile failure or skip.
- [x] Execute the same test on fixed `caa7ddc39`: strict ovh-a proof passed
  exactly one test; the unacquired-guard control also passed exactly one test.
- [x] Run all 45 recorder stack integration tests on `caa7ddc39` through strict
  ovh-a RCH: 45 passed, zero failed/ignored/filtered. Retain
  `/Volumes/USB_NVME/ft-release-20260915/rc29-recorder-integration-ovh-rch.log`
  (SHA256 `c126f06b3e788499efcafcbf63ccc22a680762958aa62f2ed6b4b92fa54810ec`).
- [x] Run all 158 `recorder_storage::tests::` tests, including initialization
  failure, buffered repair and fsync faults, through strict RCH using the
  hash-pinned core test binary built from `caa7ddc39`: 158 passed, zero
  failed/ignored, 31,664 filtered. Before/after binary SHA256 remained
  `1ece9192dfa3eea6efdb81c39edd3e1882c9085459e281781d8c9a0dae4c632d`.
  Retain `rc29-recorder-storage-prebuilt-ovh-rch-attempt4.log` in the same
  evidence directory (SHA256
  `92d73a2e807e74332144ff7758f9d37e69cdae1058e72a77dd87345ab0ca0e16`).
  This is scoped committed-source proof, not final-source qualification.
- [ ] Complete final-source workspace check, Clippy, tests and formatting
  through strict RCH before tagging or starting another native release build.

### 2026-09-16 — preserve window metadata in whole-mux recovery

- [x] Trace concrete losses: capture omitted window tab stacks and previous
  active identity; conversion additionally discarded the captured window title.
- [x] Preserve raw history and ordered stack entries under the window read lock.
  Require title, history and grouped stack metadata in image schema 2.
- [x] Validate membership, positions, visibility, history, canonical stack order
  and bounded metadata; reject older schemas and absent required fields.
- [x] Author real-PTY capture/publication/reopen/verification/inert reconstruction
  coverage plus malformed, missing-field and bounds regressions.
- [ ] Complete independent review and strict-RCH execution of these regressions.
- [ ] Finish durable tab/window identity mapping and atomic live successor
  publication under existing `.8.14.3.2` and `.3.4`; this metadata slice does
  not close those requirements or authorize restarting existing mux sessions.
- [x] Diagnose adjacent duplicate-stack-ID corruption (`ft-7vav0`): disjoint
  replacement members overwrote the forward stack but left old reverse mappings.
- [x] Reject duplicate IDs before any mutation and add a regression preserving
  complete state, nonfirst visibility and inverse membership, followed by valid
  creation and reuse after removal.
- [ ] Independently review and execute the mux tab-stack regressions through
  strict RCH; preserve the distinction from live restored-window publication.

### 2026-09-16 — durable window and tab capture identities

- [x] Replace numeric-counter-derived recovery identities with immutable UUIDs
  minted on actual window/tab construction and retained through coherent capture.
- [x] Reject nil captured identities; retain global duplicate-ID validation.
  Add numeric-reuse and JSON roundtrip regressions, plus real-PTY encrypted
  publication/reopen assertions against the original live object UUIDs.
- [x] Obtain independent six-file source review and scoped formatting checks.
- [ ] Run the new mux and converter tests and extended real-PTY test through
  strict RCH on the committed source, together with prior metadata regressions.
- [ ] Bind verified durable IDs to reconstructed successor objects in the
  atomic whole-topology transaction. UUID capture alone does not restore a mux.

### 2026-09-16 — successor capability custody before acknowledgement

- [x] Bind successor claims to authenticated broker lineage/build and both
  owners; version the changed control payload and reject the old secret-only form.
- [x] Require an opaque encrypted, synchronized custody token for successor
  ACK; reopen and resynchronize the record before sending the ACK.
- [x] Add authenticated scope lookup that recovers saved ACK/predecessor data
  from disk without retaining the original claim/context in memory.
- [x] Add real-child sync-failure, wrong-owner, changed-ACK, disk-reopen and
  exact-retry checks, plus all-byte tamper and all-field binding controls.
- [x] Independently review the production boundary and scoped formatting.
- [ ] Execute guardian/mux custody tests, UUID tests and prior metadata tests
  through strict RCH, then qualify the combined source.
- [ ] Implement explicitly authenticated fresh-connection recovery. Current
  successor custody is bound to the retained connection; duplicate ACK replay
  is not proof of recovery after transport loss or successor-process restart.
- [ ] Complete ordinary successor startup and atomic whole-topology publication.

### 2026-09-16 — BOCPD warmup property boundary

- [x] Diagnose the actual `939da9667` workspace failure: the property treated
  observation `min_observations` as warmup, although the production contract
  and existing unit tests make that observation eligible for detection.
- [x] Preserve all generated inputs and updates; assert exact post-update count
  and warmup state. Retain the minimized seven-observation positive control
  and an eight-observation-minimum suppression control (`ft-cb5l2`).
- [ ] Run the full `proptest_bocpd` target and deterministic boundary test through
  strict RCH. No production detector, threshold or generator was changed.

### 2026-09-16 — measured snapshot retry latency and release blockers

- [x] Retain the completed RC28 production measurement: 2,000 events, unchanged
  offered envelope, failed finite-trace bound, and 1,987 explicit 10 ms retry
  sleeps across 2,008 text reads. This does not establish maximum capacity.
- [x] Remove the first source/layout retry's unconditional timer in `20a089aa0`;
  yield cooperatively, preserve all consistency fences and the three-attempt
  budget, retain repeated-churn and quota backoff, and obtain independent review.
- [ ] Execute the real-socket text-read regressions and cancellation controls
  through strict RCH, then rerun the unchanged producer on newly built binaries.
- [ ] Repair the RCH client heartbeat gap before expensive source validation;
  jobs 32025/32027 were cancelled by the stuck detector before compilation.
  Do not disable detection or count these cancellations as product test results.
- [x] Implement and independently review asynchronous live broker lease-journal
  transitions in `b2df40bcd`; preserve fencing during I/O and quarantine failures.
  Author consecutive-handoff, fault, owner-EOF and exact-child census regressions.
- [ ] Execute those broker regressions through strict RCH. Source review does
  not establish ordinary successor startup or fresh-connection recovery.
- [x] Reject malformed, nil and noncanonical durable window/tab UUIDs in
  `7d0a5c008`, including authenticated-image reconstruction controls.
- [ ] Execute image and authenticated reconstruction controls through strict RCH.

### 2026-09-16 — isolated native RC28 acceptance

- [x] Launch the actual DSR RC28 app in a separate process with private home,
  configuration, sockets and an owned local child; preserve installed RC15
  process 89046 and all existing remote sessions. Candidate PID 7540 uses source
  `55baaac56dd0d8ab665164041c8fb0922249c6d2`, not the later RC29 fixes.
- [x] Drag-select the exact 69-byte Unicode line with native mouse input;
  verify selection through the GUI's selection API. Initial unfocused drag was
  empty; the focused drag preserved Chinese, combining accent and emoji bytes.
- [x] Verify native menu font increase and Command-minus decrease retain the
  1148 by 774 pixel window while the grid changes 80 by 24 to 69 by 21 and back.
  Ordinary edge drag changes the window to 844 by 644 and grid to 58 by 19,
  with visible reflow. These observations are not latency benchmark results.
- [x] Trace the history-prefix join to the stale incoming scrollback wrap flag.
  Retain regression-only `da37591d9` and reviewed repair `d99529957`, covering
  full first-row erases while preserving partial-erase continuations.
- [ ] Execute the negative baseline and fixed CSI controls through strict RCH,
  then repeat native clear/repaint/reflow against the final artifact.
- [x] Capture actual native key events: the synthesized equals event arrives as
  Shift+Command with raw plus. Adding only its missing binding to the isolated
  Lua configuration makes the identical event increase font size at fixed pixels.
  Commit default alias repair `ce0582ee0` with InputMap/custom/disable controls.
- [ ] Run both `cmd_plus` tests in `glyphcache_unit` through strict RCH and
  verify final native default bindings without the diagnostic Lua override.
- [ ] Repeat native acceptance against the final source and final DSR artifacts,
  including remote-session reconnect, before release/install claims.

### 2026-09-16 — remote compilation resumed and actual failures repaired

- [x] Repair RCH preflight heartbeat ownership and execute its real Unix-socket
  regression: job 32060 ran one test successfully. Build the corrected client
  remotely and verify its SHA-256 on the Linux coordinator before use. No daemon
  timeout or admission rule was weakened; the installed Mac client is unchanged.
- [x] Import committed source objects into the existing coordinator repository
  without changing its HEAD, index, refs, or working files. Focused core and
  terminal/GUI queues now reach actual remote Cargo compilation.
- [x] Repair the actual guardian test compile failure and Rust 2018 panic-format
  warning in `caabcfd5862aec250e941025b291e2c90f024042`. The failed combined
  attempt ran no tests; it is not a passing guardian receipt.
- [x] Repair the default-feature terminal test-helper compilation defect in
  `6163635feb1cf9002431de73ac4c3a49dad236b5`. Feature-matched ED2 baseline and
  fixed-source execution remain separate from this source correction.
- [ ] Finish core image/authenticated restore, snapshot retry, BOCPD, CSI,
  Cmd-plus and guardian runtime queues, retaining actual test counts and source
  identities. Repeat workspace qualification after the final source settles.
- [x] Validate the private DSR quality configuration with the checked Linux
  RCH transport. Preserve all six gates and native DSR build configuration.
- [ ] Execute that complete DSR quality lane on the final imported source.
  Static gates passed 29 checks on the earlier metadata checkpoint; this does
  not qualify subsequent code or substitute for Cargo/native checks.
- [ ] Verify held selection across native reflow. The isolated app remains
  visible, but CUA coordinate drags currently fail with `noWindowsAvailable`;
  its earlier successful Unicode drag does not establish this additional case.
- [ ] Complete durable pending-Claim fresh-connection rebind, including lost
  reply, stale owner, rotated custody and sync-failure controls. This bounded
  slice does not establish new-mux-incarnation recovery, whole-topology startup
  publication, broker restart adoption, or GUI reopen ordering.

### 2026-09-16 — actual focused results and remaining implementation

- [x] Execute c258 image controls: 72 passed, zero failed/ignored, 31,758
  filtered. Execute authenticated whole-mux restoration: 15 passed, zero
  failed/ignored, 31,815 filtered, including damaged-root RaptorQ repair.
  These prove reconstruction behavior, not live startup or process continuity.
- [x] Execute c258 real-PTY capture/publication: one passed, zero failed/ignored,
  31,829 filtered. Execute text-read retry controls: 14 passed, zero
  failed/ignored, 31,816 filtered, including first-retry yield without a timer.
- [x] Execute recovery-image integration on c258: 15 passed, zero
  failed/ignored/filtered, including authenticated repair from disk, hidden
  stack reconstruction and active-destination refusal. Execute BOCPD: 38
  passed, zero failed/ignored/filtered. Both ran on vmi1152480 through strict
  RCH with the verified epoch client; this completes the focused core queue.
- [x] Prove the ED2 regression fails on da375 at the stale-wrap assertion.
  Execute fixed a2f3 CSI controls: 17 passed with serde and 17 passed without
  it; the latter also proves the test-helper feature-gating repair.
- [x] Execute both a2f3 Cmd-plus binding tests: two passed, zero failed/ignored,
  730 filtered. Native default-binding acceptance still needs a final artifact.
- [x] Commit pending native gesture capture and clipboard retention in 06fa,
  with typed Busy/source-changed/unsupported-anchor outcomes and bounded wakeups.
  Preserve alternate-screen and nonresident-history basic selection.
- [x] Commit resident held-anchor preservation over unrelated output in d7fa,
  after regression-only 7fdd. Mutations within selected rows still invalidate.
- [x] Execute the held-anchor negative (one expected failure) and fixed terminal
  (seven passed) and mux (13 passed) regressions. Fix soft-wrap BeforeZero
  endpoint whitespace in 475355e16; all seven GUI regressions now pass, including
  literal Unicode, hard-newline and real-blank-line controls.
  This implementation covers LocalPane; remote ClientPane remapping remains
  unsupported and must not be inferred from local results.
- [x] Execute b7d full guardian library tests: 235 passed, four failed, four
  ignored. Retain the failures; the subsequent mux tests did not execute.
- [x] Fix wrong-mux reconnect rejection in two shared-fixture tests, the
  capacity-one sentinel readiness race, and exact durable predecessor-fence
  reopen failure. Fix claimant census ownership and connection EOF settlement.
- [x] Execute full mux tests at 5b21777e2: 1,197 passed, zero failed/ignored,
  including deterministic terminal-fence-before-cleanup settlement coverage.
- [x] Complete guardian rerun at bb3884e3a: 239 passed, zero failed, four
  ignored. The WAL-fault fixture now queries the actual owner after transfer.
- [x] Implement the first new-mux owner rotation in 821036d94: existing-only encrypted custody
  bootstrap, old outer-owner retirement, per-pane durable fence, new owner
  claim, durable custody and acknowledgement, then runtime handle publication.
- [x] Review and commit distinct-build ownership transfer and actual outer
  lost-reply, delayed-output and three journal-cut fixtures in 8274c5336.
  This is implementation, not successful execution proof.
- [x] Retain the sealed 821036 compile failure: seven missing production
  imports, zero tests executed. Correct those imports in 8274c5336; the workspace
  check independently found the same defects and continues with keep-going.
- [ ] Execute sealed 8274c5336 tests and retain its actual executable, then
  prove transfer to a second genuinely distinct source build. Typed build IDs
  within one executable do not satisfy this artifact-to-artifact check.
- [ ] Prove rotation deadlines, exact outer lost-reply retries, rejected live
  predecessors, missing/wrong credentials, and unchanged real child identity.
  Broker connection loss after committed ACK remains a separate unproven case.
- [ ] Connect image discovery and whole-topology prepare/commit to ordinary
  startup before default replacement shells or public readiness are emitted.
- [x] Execute prepared-reflow cursor controls at cf30f16b: nine passed, zero
  failed, 507 filtered. Execute the unchanged-row stale-preparation reuse control:
  one passed, zero failed, 515 filtered. These are remote correctness tests,
  not native or remote-session latency measurements.
- [ ] Finish the newly diagnosed remote ClientPane selection fixes: retain a
  gesture through cache-lock contention and release without a final Motion;
  read authoritative fresh text in bounded chunks; preserve original server row
  revisions separately from fetch repaint damage; retry deferred clipboard work
  without copying stale, predicted or partially fetched text.
- [ ] Execute actual remote cache/gesture controls and repeat selection against
  the final native application and an owned remote session.
- [x] Diagnose incomplete resize evidence passing as zero latency (ft-n33w8).
  Require complete nonempty numeric measurements and refuse failed baseline
  refreshes. Independent review also found unvalidated queue measurements;
  require actual per-event stages and numeric queue depths. All 59 shell
  controls pass, including poisoned local-Cargo guards.
- [x] Complete independent review of ft-n33w8; this gate evaluates
  simulation evidence and cannot establish actual Mac resize performance.
- [x] Retain ft-n33w8 shell receipt SHA-256
  `b1c73f4e910ce19cfd9d3c500aa1b4a51ef1b2cc6daa0588c246b5cae184861b`.
- [x] Diagnose repeated identical-source rebuilds: RCH refreshed every source
  timestamp after all artifacts. Closure-bound freshness-epoch regressions now
  pass 19 tests, including actual first-party Cargo reuse and changed-byte rebuild.
- [x] Complete epoch workspace check, Clippy and format, build and hash-verify
  its remote client, then adopt it only for future proof invocations. RCH commit
  0812d5b42; delivered client SHA-256
  eadcd3b749d515699af84d72c0ddaf2b3276c532a9626e47692136378c238740.
- [ ] Complete final-source workspace and six-gate DSR qualification, all
  native families, native/remote acceptance, required producer evidence,
  publication, verification, canary and safe upgrade/installation. No release
  is published and no existing remote mux has been restarted.

### 2026-09-17 — live handoff proven; remaining release work

- [x] Execute the sealed two-pane mux-process rotation at `f2fc069e4` through
  strict RCH job `j-30023605353971981`: one passed, zero failed/ignored,
  250 filtered. Both original children survive the tested ownership handoff.
  Retain the executable independently of the mutable build pool; SHA-256
  `78eddf76ece656a1117161b79e05a6958cb894f2503991b080607d8841510129`.
- [x] Retain sealed second-source executable `8e00c3e75`, SHA-256
  `ad80a6c6be7bcdaa14ecd7aae2569d1e147f033784aa6ed9b84eaaa4d7edc2b2`.
  Strict RCH job `j-30023605353971986` proves distinct-binary handoff, exact
  outer reply loss and all three lease-journal cuts: three passed, one failed.
- [x] Diagnose that remaining delayed-output failure: census mistakes the
  worker-owned output journal for missing custody. `b3752b0e8` tracks exact
  pane/sequence transfer, validates returned ownership and keeps the lease
  census available without exposing uncommitted output. Negative controls
  reject missing, partial, wrong-pane and wrong-sequence custody.
- [x] Execute updated sealed upgrade/fault suite on worker126: strict RCH
  `j-30023605353971989`, four passed, zero failed/ignored, 247 filtered.
  Retain `b3752b0-upgrade-and-faults-rch.log`. The broader broker suite also
  passed nine tests, with one explicitly sealed test ignored, in job
  `j-30023605353971991`; this does not prove automatic startup recovery.
- [x] Commit actual input delivery gates in `2332b889b`: keyboard protocols,
  composed input, SendKey/SendString, terminal mouse reports, paste and drops.
  Retain local selection/copy and overlay interaction. Delayed paste retains
  its original pane allocation and verifies current window ownership.
- [x] Execute the owned real-PTY pane-move regression and mixed-layout tests:
  strict RCH `j-30023605353971990`, five passed, zero failed/ignored,
  739 filtered. Retain `b3752b0-gui-layout-rch.log`.
- [ ] Review reconnect delivery and saved-window restoration against native input.
- [x] Fix the missing inherited guardian workspace dependency in `f85640a22`.
  Both prior `2332b889b` verification jobs failed before compilation; neither
  is a passing receipt. Scope durable Unix-only witnesses correctly in
  `8e00c3e75` so Windows does not import unavailable output-store types.
- [x] Retain and review the remote Cargo lock resolution for the new optional
  core-to-guardian dependency: exactly one dependency-list entry, no version
  changes. Local lock bytes match the retained remote file, SHA-256
  `8bfd49e94374b368aff709714ec2d838d40f2c9258aefbe0552b261a4c653de8`.
- [ ] Repeat verification with the committed lock and `--locked`.
- [x] Execute latest core image suite at `25c029e9`: 72 passed, zero failed.
- [ ] Execute fresh-process custody/catalog/ACK reopening through the real
  birth-image parent test. Then connect verified discovery and atomic topology
  publication to ordinary startup before listeners/default shells appear.
- [x] Diagnose the first fresh-process parent build refusal: sealed identity
  rejected unbound Cargo debug-profile environment overrides before tests ran.
  Keep the guard intact and rerun the committed profile without overrides;
  retain `018e55c-fresh-process-image-committed-profile-rch.log`.
- [x] Finish clipboard deadline proof and recorder isolation: strict RCH
  job `30023644042232225`, 31 passed/zero failed, normal parallel execution.
  Retain `ruby-selection-deadline-fixed-rch.log`; final test delta `c69137d5c`.
  Close the narrow valid-empty bead `ft-t5mse`; keep `ft-ffrry` open for large
  cold-span acquisition and native acceptance.
- [x] Fix all three errors from completed `8e00c3e75` workspace check:
  reconnect domain receiver, guard lifetime, and obsolete fake publication
  test field. Updated locked workspace run is pinned to `c69137d5c`.
- [x] Fix three GUI `WindowOrderMirror` private-import errors in `b3752b0e8`.
- [x] Execute encrypted cold-history clipboard acquisition: strict RCH
  `j-30023605353971993` at `f66007801`, 33 passed, zero failed/ignored,
  713 filtered. The real spill-store test reads 1,025 wrapped Unicode rows
  through 17 worker chunks and compares the complete copied text.
- [x] Fix split-lock starvation during anchored local copying in `0122937ae`:
  consume the same sequence/dimensions observation that validated the anchor,
  and check the anchor even on the first chunk. Add a real LocalPane/PTY
  regression for intervening output and rejection of edited selected rows.
- [x] Diagnose the `0122937ae` GUI test compilation failure: the real LocalPane
  regression lacked its `TermWindow` import. Fix in `055db057a`, then scope
  that import to Unix in `e8ba62a91`.
- [x] Execute the GUI selection suite at `1ee43344b`, including the new
  LocalPane race regression: strict RCH `j-30023644042232351`, 34 passed,
  zero failed, 713 filtered; `1ee4334-gui-selection-direct-rch.log`.
  This is Rust harness evidence, not final native mouse acceptance.
- [x] Implement bounded remote selection witnesses in `055db057a`, covering
  the entire original selection even after copied rows leave the render cache.
  Preserve selection across unrelated output; reject selected-row mutations,
  retention loss, layout changes, wrong owners and mismatched ranges.
- [x] Execute client selection tests at `c2d63e3f7`: strict RCH
  `j-30023605353972010`, 11 passed, zero failed, 326 filtered. Retain
  `c2d63e3-client-selection-rch.log`.
- [x] Fix empty remote damage intervals falsely invalidating a selection in
  `3055baea8`; extend the witness regression with an empty-interval control.
- [x] Prove the empty-interval control and complete client suite: 337 passed
  at `e8ba62a91`, followed by 338 passed, zero failed/filtered at `1ee43344b`
  in strict epoch-qualified RCH `j-30023605353972100` on worker122.
  Retain `1ee4334-client-all-epoch-rch.log`; this includes `685a328e1`'s
  unknown server revision rejection before and after witness admission.
- [ ] Resolve cold local selections cancelling during unrelated output when
  no resident native anchor exists. The remote witness does not cover this case.
- [x] Execute exact remote-pane mapping regression in the full client suite.
- [x] Diagnose that mapping test's setup failure: domain91030 was never
  registered. Reuse the existing registration helper in `baca379ba` before
  exercising the production layout snapshot path; both full-suite runs pass.
- [x] Execute workspace check at `f66007801`: strict RCH
  `j-30023605353971994`, exit zero. This predates the remote-witness changes.
- [x] Fix workspace Clippy's oversized LocalPane ownership enum in `c2d63e3f7`
  by boxing guardian-only metadata, preserving compact ordinary panes.
- [x] Review and integrate guardian Clippy fixes in `e8ba62a91`, including
  the cross-file historical-context test caller. Scoped formatting passes;
  repeat workspace Clippy and compilation on current source.
- [x] Retain RubyFortress's allocation-free resident anchor checks:
  seven default-feature tests and seven serialization-feature tests passed,
  plus term library/test Clippy. Scope the getter to its actual caller features.
- [x] Reuse that exact row-validation helper during LocalPane anchor capture,
  removing its duplicate full-span pointer allocation when output intervenes.
  Preserve source, geometry, retention and selected-row mutation checks.
- [x] Execute the existing LocalPane native-selection capture regressions
  against this helper reuse at `1ee43344b`: strict epoch-qualified RCH
  `j-30023605353972116`, two passed, zero failed, 1197 filtered;
  retain `1ee4334-mux-native-selection-epoch-rch.log`.
- [x] Remove blocking metadata reads from cell selection entrypoints in
  `1ee43344b`; retain pending starts and released endpoints on Busy.
  Independent source review and 34 GUI selection tests pass.
- [ ] Prove those entrypoints under contention through a real native window;
  semantic, word and overlay blocking-read paths remain tracked in `ft-w57v2`.
- [x] Reduce fresh-process proof debug symbols through committed
  `test-recovery` profile in `c8e69eb11`, preserving unwinding and assertions.
  Earlier full-symbol compile was killed with SIGKILL; no OOM cause confirmed.
- [ ] Complete fresh-process execution with its required same-source sealed
  guardian companion. The `c8e69eb11` parent compiled and ran one test, then
  failed because its invocation omitted `FT_GUARDIAN_TEST_EXECUTABLE`.
  Build the companion with the same sealed identity/profile before retrying.
- [x] Fix the remaining guardian alias visibility Clippy error in `d6bdb5ac7`;
  the repeated workspace Clippy result is still pending.
- [x] Implement smart word/line matching and fallback from one logical
  snapshot; refine the initial `8151ca310` implementation in `3d7bf5e7` to
  four private helpers, checked row bounds and saturated endpoints.
- [x] Pass the broader `selection::tests` GUI module at `568bf385f`:
  44 passed, zero failed, 710 filtered. Retain
  `568bf38-gui-selection-fresh-ts1.log`, including actual changed GUI/mux
  compilation and all seven new snapshot/boundary tests. RCH mapped the
  requested fresh target to its existing pool; do not call this a cold build.
- [x] Replace repeated backward context array shifts with reverse-once
  accumulation in `568bf385f`, preserving bounded context and physical order.
- [x] Execute the dedicated logical-context tests: 12 passed, zero failed,
  1188 filtered at `568bf385f`, hz4 job `j-30023605353972133`.
  Retain `568bf38-mux-logical-clean-home-hz4.log`. First attempt failed
  source preflight on unselected Cargo-home config before Cargo; retry uses
  an isolated Cargo home and retains the selected-source check.
- [x] Build the same-source sealed guardian companion for the recovery test
  on worker114: compilation succeeded, Linux artifact SHA-256
  `a8e65a4fef6d9693602d71b56dbee7788834fd89da95761b48eba7f5cc05f489`.
  RCH rejected the downloaded ELF as a Mac executable (E327); use the verified
  retained Linux artifact only on its Linux worker. Parent rerun is pending.
- [x] Diagnose the executed parent failure: a populated tab cannot use
  `add_tab_no_panes`. Use `add_tab_and_active_pane`, then assert its returned
  handle has the exact original registration identity (`c733933a7`).
  The intervening `3d8b73cd1` assertion incorrectly expected None; its actual
  one-test failure is retained in `3d8b73c-fresh-process-image-rch.log`.
- [ ] Execute corrected recovery parent with the same-source companion and
  matching test/binary feature union; the c733 family build is on worker114.
- [x] Build that c733 family and execute its actual test binary for early
  diagnosis: one failure at guardian_proxy.rs:6379, not a compile failure.
  Restored child output was applied before registration, but its authenticated
  parser prefix never reached the new registration. The quiet checkpoint
  therefore fails `GuardianDeliveryStartedLate`; later sequence>1 delivery
  has the same missing prefix. Retain the diagnostic and full RCH rerun.
- [ ] Fix `ft-interactive-swarm-product-convergence-7xqz4.8.14.3.14`:
  carry a model-bound authenticated replay prefix through activation and
  registration, including Record checkpoints without a replay suffix.
  Do not manufacture an append receipt or seed raw caller-supplied watermarks.
- [x] Correct Clippy's similar-name error in the recovery-image adapter.
  Repeat current-source Clippy after the ongoing workspace pass settles.
- [x] Run all static release gates at `431f7d5cb`: 29 passed, zero failed,
  eight Cargo gates skipped. This is not strict release attestation closure.
- [x] Diagnose exact-source formatting failure at `431f7d5cb`: one wrapping
  difference in client consumer-commit code. Apply the exact formatter diff.
- [ ] Repeat exact-source formatting proof after that correction.
- [ ] Complete DSR quality: the c733 first check failed remote queue admission
  without local fallback. Four canonical attestation producers remain open:
  Lindley bounds, redaction, operating envelope and Doctor live transactions.
- [x] Reconfirm word-boundary implementation/history: `87118af0e` and the
  separator corrections are committed; Bead `ft-kwrh9` retains the earlier
  419-test surface receipt and terminal oracle corrections.
- [ ] Finish current-source word-boundary proof and measure actual native
  resize, tiling and font-change latency with fixed pixels and large history.
- [ ] Complete final workspace check, Clippy, exact-source formatting proof,
  DSR quality, native builds, native/remote acceptance, signed producer
  artifacts, DSR publication/verification/canary/upgrade and Dock installation.
  Existing user sessions remain untouched until their preservation is proved.
