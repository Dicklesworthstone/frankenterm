# Recovery campaign reality check — 2026-09-15

Six-hour checkpoint: 2026-09-14 22:12 UTC through 2026-09-15 04:12 UTC.
Scope: full mux-state recovery with authenticated RaptorQ protection, and
the concurrently reviewed native rendering/reconnect changes. This is a
source and retained-test assessment, not a release attestation.

## User outcome

**Not ready to restart an existing remote mux without risking its sessions.**
The campaign produced working recovery components and passing fresh-process
storage/reconstruction tests. Ordinary startup, live guardian reattachment,
independent-host disaster recovery, and native interactive performance have
not reached their acceptance criteria. No remote mux was restarted or
upgraded by this campaign. In particular, hz1 remains on its existing legacy
mux; it was not used as a destructive recovery experiment.

## What now has executable evidence

The implementation captures parser-ground terminal state and mux topology,
publishes authenticated encrypted recovery objects and repair records, verifies
their closure, and reconstructs an inert mux image. Repair uses the existing
RaptorQ implementation through `runtime_async::raptorq`. Tests exercise the
real filesystem, parser checkpoints, encryption, repair decoder, verifier,
and a fresh child process. They do not merely compare saved screen text.

| Evidence | Exact source | Actual remote result |
| --- | --- | --- |
| Whole-mux integration, including fresh-process predecessor fallback | `e1fc277d2b7fb29532d72a9238101eab502bb1dd` | hz4 job `30016197441357072`: integration target 14 passed, 0 failed, 0 filtered; the separate core target failed, so the overall job exited 101 |
| Client, guardian, surface, terminal, mux | `39f53f6e04df6d405263770a608278ca69455806` | hz4 job `30016197441357098`: 2,652 passed, 0 failed, terminal exit 0 |
| Changed recovery modules and two shutdown regressions | `39f53f6e04df6d405263770a608278ca69455806` | hz4 job `30016197441357096`: 165 passed, 0 failed, 27,521 filtered, terminal exit 0 |

The 2,652 count comprises client 322, guardian library 223, guardian binary 3,
surface 425, terminal 506, and mux 1,173. These are package tests, not 2,652
independent disaster journeys. Integration negatives include wrong keys,
ciphertext/manifest corruption, spoofed locators or guardian authority,
missing objects, insufficient repair rank, admission pressure, and refusal
to overwrite an active destination.

Production fixes include catalog-backed checkpoint acknowledgements, exact
ordered stack-slot binding, semantic row-mutation fencing, preserving the
checkpoint generation on failed activation, mux-incarnation-bound reconnects,
and a separate bounded optional layout-binding lookup before transport
bootstrap. Independent GUI review also found cancellation and native-window
ownership bugs; those fixes require their own final native qualification.

## What remains unproven or incomplete

1. **Normal startup to guardian recovery.** `GuardianProxyLeasePlan` has no
   ordinary startup consumer. The existing local-domain path still creates
   direct local panes; the proxy replay boundary rejects Genesis. Unit tests
   of the guardian worker do not prove authenticated socket/PTY survival and
   activation. Tracked under `.8.14.3.1` and `.8.12.3` of the product-convergence
   epic.
2. **All terminal state.** Graphics/image checkpointing is explicitly rejected
   rather than silently dropped. Canonical image bytes and placements,
   protected storage, and fresh-process equality are tracked in
   `ft-interactive-swarm-product-convergence-7xqz4.8.14.5`.
3. **Independent failure domains.** Replication, rollback/freshness witnesses,
   and fresh-host key/catalog bootstrap remain open under `.8.14.2.11`,
   `.8.14.2.14`, and `.8.14.2.15`. Local error correction cannot recover a
   completely lost storage device without another surviving copy.
4. **Real disaster journeys.** `.8.14.4.3` still requires real mux/guardian/
   client/PTY crash, upgrade, reconnect, and host-loss tests. The failure
   contract distinguishes preserved live processes when the guardian survives
   from terminal reconstruction and policy-controlled process replacement
   after host loss. Serialized mux state does not preserve arbitrary process
   memory or external resources through power loss.
5. **Intermittent shutdown failures.** The broader core run passed 1,118 of
   1,128 selected tests. Eight obsolete fixtures were corrected; two terminal
   checkpoint/clean-mark failures passed the narrower rerun without a behavior
   fix. New finite diagnostics preserve the failure class. Load-sensitive
   transaction timeout is a hypothesis, not a confirmed cause; `.8.14.3.11`
   retains the reproduction and next diagnostic run.
6. **Native usability and release.** RC18 is a diagnostic candidate, not an
   installed or published recovery release. Native timing runs were invalidated
   by host load, capture, or focus failures. No dramatic rendering speedup,
   final font-resize latency, or complete remote-tab reopening is claimed.

## Qualification still in progress at the checkpoint

Source `a761bf1a3734cd5dd81e65bd48a6f817964a09d2` is under strict remote
workspace check (hz4 job `30016197441357126`) and formatting proof (hz4 job
`30016197441357127`). Neither admission nor compilation progress is a pass.
Workspace Clippy, final GUI tests, and macOS-only compilation/runtime checks
remain pending. Further native-owner fixes discovered by independent review
will need qualification at their own exact source revision.

The recovery subtree currently has 51 open, 9 in-progress, 1 blocked, and 8
previously closed beads. No recovery bead was closed by this campaign on the
strength of component tests alone. The next useful milestone is a verified
ordinary-startup guardian path and a disposable real crash/reattachment
journey, followed by an explicitly authorized canary on an existing host.

## Follow-up qualification at 04:53 UTC

- Formatting job `30016197441357127` passed the exact `a761bf1a3` contract:
  one named test, one pass, zero filtered, and the source-bound success marker.
- Broader core job `30016197441357133` on that source passed 1,127 of 1,128
  tests. Both earlier shutdown cases passed, but the event-bridge test observed
  startup persistence without an event checkpoint inside ten seconds, before
  shutdown began. Its separate 14-test recovery integration target passed.
  The worker exited 101; subsequent RCH stuck-detector cancellation of the
  lingering wrapper is separately retained. This is not an overall pass.
- `ec3cefcc3` adds test-only trigger/capture counters and task status to that
  timeout, with unchanged deadlines. Job `30016197441357160` is investigating
  the broader selection with those diagnostics; no behavioral fix is claimed.
- Workspace check `30016197441357126` and GUI test job `30016197441357147`
  both failed to compile the newly included journal-binding library test.
  `91e96f2b3` supplies its missing `CompatRuntime` import. Full workspace Clippy
  job `30016197441357168` is running on that corrected revision.
- Independent review accepted the native callback-registration and allocation-
  owner fixes (`b8e3a26f2`, `7e532d1f7`). RC19 DSR run `9c0b3470` failed before
  Cargo because the build host lacked the tagged Git object; the object was
  subsequently transferred without changing its checkout. No native compile,
  installation, or release success follows from that attempt.
- UBS static scanning remains nonzero. Review of its displayed findings found
  fixture panics, bounded operations, binary headers mistaken for HTTP headers,
  and integer task/reactor tokens mistaken for credentials. Its sampled output
  does not clear the undisplayed aggregate findings or establish a clean gate.

## Follow-up qualification at 17:12 UTC

- Core job `30016197441357160` ended with 1,127 passes and one failure. The
  idle-bridge test did not observe a startup checkpoint within ten seconds;
  it had not reached shutdown. The event-bridge case passed. This does not
  establish a causal fix for either intermittent failure.
- GUI/window job `30016197441357169` passed its actual 3 library, 7 binary,
  and 1 window tests on `91e96f2b3`. This is Linux component evidence, not
  macOS compilation, native timing, or installed-app acceptance.
- Workspace Clippy job `30016197441357168` failed with 27 diagnostics. The
  fixes are in `747c75cbd13dd3e1089cbdcd821a23000aef426f`: explicit hasher
  parameters, simpler bindings/patterns, equivalent sorting/error construction,
  and repaired documentation formatting. Startup diagnostics retain the same
  deadline. No lint suppressions or relaxed recovery assertions were added.
- That commit's message overstates the generation-conflict change: the old
  `else` already returned the error; removing it preserves behavior. The report
  also remains on disk and in HEAD despite the unrelated stale normal index.
- Strict RCH jobs `30016197441357178` (workspace Clippy) and
  `30016197441357179` (focused runtime snapshot tests) were admitted on hz4
  against that exact source. They are pending, not passes. Final formatting
  proof must cover the corrected source as well.
- The campaign's six-hour checkpoint above remains the measured work window;
  the elapsed pause afterward is not claimed as continuous swarm supervision.
- Native RC18 comparison 4 matched all 18 action frames and preserved 10,000
  ordered records, but its observed compositor upper bounds ranged from
  59.461 to 310.563 ms, with seven actions above the 100 ms ceiling. The
  retained receipt is `/tmp/ft-native-profile-rch-0912/rc18-primary-comparison4-receipts.json`.
  These are scaled-capture matches, not native-resolution text-quality proof;
  live dragging was excluded and clean-host performance remains unqualified.
