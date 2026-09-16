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
- [ ] Execute strict RCH guardian/mux custody tests on the committed source.
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
- [ ] Run guardian, mux, client and portable-pty libraries on the successor.
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
- [ ] Commit coherent RC25 metadata, refreshed vendored provenance and tracker
  export after the PTY correction. Complete every static gate before tagging
  and admitting the replacement candidate; obtain all owner freeze acknowledgments.
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
