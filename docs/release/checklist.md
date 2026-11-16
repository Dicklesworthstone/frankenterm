# Release Checklist

Run this list **before tagging a major release** (`vX.Y.0`). The
items marked **mandatory** must be done; **recommended** items
catch common omissions.

## Mandatory

1. **Bump the workspace version.** Edit `Cargo.toml`'s
   `[workspace.package].version`; the per-crate `Cargo.toml`s
   inherit via `version.workspace = true`.
2. **Refresh CHANGELOG.md.** Move the `[Unreleased]` section into a
   new `[X.Y.0] — YYYY-MM-DD` section; add a fresh `[Unreleased]`
   stub above it.
3. **Stamp README/AGENTS counts.** Run
   `bash scripts/stamp-readme-counts.sh --source=head` so every documented
   workspace count matches the committed release tree (ft-tf6g3.2), then refresh the
   release-attestation snapshot with
   `bash scripts/stamp-readme-counts.sh --source=head --json > docs/attestations/doctrine/agents-md-counts.json`.
   Retain the stamper output in the DSR release record and block the DSR tag
   step if the committed tree diverges from the refreshed snapshot.
4. **Regenerate the vendored-fork provenance manifest.** Run
   `python3 scripts/regen-provenance.py` so
   `frankenterm/PROVENANCE.json` reflects the latest fork-side
   commits (ft-i2eni.6). Run `scripts/check-provenance.sh`, retain its output
   in the DSR release record, and refuse the DSR tag step when the manifest is
   stale.
5. **Run the bench-stats verdict in enforce mode.** Ensure
   `bash scripts/check_bench_stats.sh` (with
   `BENCH_STATS_MODE=enforce` set) returns clean against the
   `origin/main` baseline (ft-9zzkg).
6. **Re-record both README demo GIFs.**
   - Tour: `vhs scripts/demo.tape` → `assets/demo.gif` (ft-jjvxg).
     Synthetic; renders against any built `ft` binary.
   - Full scenario: stage the 10-pane NTM swarm per ft-xl2kc.1's
     runbook (4× cc / 3× cod / 3× agy, drive ~3 min so it hits
     ≥1 real rate limit + ≥1 workflow auto-detect), then
     `vhs scripts/demo-full.tape` → `assets/demo-full.gif`
     (ft-xl2kc). The recording reads live `ft status` /
     `ft robot events` / `ft search` output from the staged swarm
     — no mocks. Eyeball both GIFs for stale CLI options or
     missing flags before committing.
7. **Run the attestation closure checklist.** Walk
   [`docs/release/attestation-checklist.md`](attestation-checklist.md)
   pre-flight: confirm every required-category producing bead
   is closed, then `scripts/attestation-build.sh` +
   `scripts/attestation-verify.sh` + the smoke test.
   Block tagging if the bundle is partial or any verification
   step fails (ft-187kv).
   For robot-contract updates, confirm `proofs/robot-contracts`
   covers the checked-in live profile/fleet mutation receipt matrix
   and failure envelopes as repository evidence, not as a production
   deployment claim.
8. **Run the context-horizon truth gate when release claims mention it.**
   If README, release notes, attestation text, or operator docs claim
   context-horizon behavior, verify the contract, schema, fixture, and
   read surfaces as repository evidence before tagging. At minimum run:
   ```bash
   RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- env CARGO_TARGET_DIR=/tmp/ft-release-context-horizon-docs \
     cargo test -p frankenterm --test docs_smoke \
     context_horizon_contract_docs_truth_gate -- --nocapture
   RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- env CARGO_TARGET_DIR=/tmp/ft-release-context-horizon-core \
     cargo test -p frankenterm-core --lib context_horizon -- --nocapture
   RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- env CARGO_TARGET_DIR=/tmp/ft-release-context-horizon-ft \
     cargo test -p frankenterm --bin ft context_horizon -- --nocapture
   ```
   Retain the exact commands, selected worker/runtime reachability,
   target dirs, artifact paths, and failure classification. These checks
   prove the v1 contract, privacy posture, deterministic fixtures, robot
   JSON/TOON behavior, and doctor embedding; they do not prove provider
   token availability, live pane mutation safety, or 64 CPU / 256 GiB
   high-scale behavior. Any high-scale context-horizon wording must stay
   caveated or `target_hardware_skipped` unless the high-scale evidence
   gate below also retains a target-class artifact.
9. **Run the high-scale release evidence gate through RCH.** Ensure
   `RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- env CARGO_TARGET_DIR=/tmp/ft-release-evidence cargo test -p frankenterm-core --test large_swarm_replay_corpus release_evidence --no-default-features`
   passes before publishing any 64-core / 256 GiB swarm-performance
   claim. Synthetic/local smoke manifests must render
   `SKIPPED_NOT_PROVEN`; only a real-hardware proof-gauntlet manifest
   with linked replay artifacts may render as proven. For resource-cockpit
   claims, the retained
   `tests/e2e/artifacts/goal-line/ft-rz0eb.4/resource_cockpit_conformance/20260513T172634Z/summary.json`
   artifact is remote-reduced schema/runtime evidence only; target hardware
   remains `skipped_not_proven` until a target-class live artifact is retained.
   Run `scripts/run-target-class-cockpit.sh` once per documented major SKU from
   [`docs/perf/target-class-hardware.md`](../perf/target-class-hardware.md);
   a release bundle that publishes resource-cockpit high-scale wording must
   retain at least one target-class summary for `macos` and one for `linux`.
   Only a summary with `hardware_predicate.proof_status = "proven_predicate_met"`
   satisfies high-scale wording.
   For capture-lag or capture-fairness claims, cite the retained `ft-n447z.5`
   200-pane reduced RCH artifact and keep target-class wording blocked unless
   the same run also retains a passing high-core hardware predicate.
10. **Verify artifact-specific panic profiles.** Every standalone CLI archive
    and every `FrankenTerm.app` executable must come from
    `target/<triple>/release-interactive/`, carry the same profile-specific
    atomic identity for that target, and declare `panic.*=unwind`. The ordinary
    `release` profile is also unwind-safe but is not the release identity. Never
    package the explicitly aborting `release-abort-probe`; run that negative
    control and the shipped-profile arm through the panic-contract subprocess
    proof with strict remote RCH. Build every packaged artifact with `--locked`;
    unit-profile catch tests do not prove shipped recovery.
11. **Release only through DSR.** GitHub Actions is not a FrankenTerm release
    mechanism and must not be invoked, queried, or treated as fallback proof.
    From the exact clean release commit, run `dsr quality --tool frankenterm`
    and require a passing receipt whose before/after source identities match.
    Then use `dsr version tag frankenterm` (add `--push` only when publication
    is authorized), `dsr build frankenterm --version X.Y.Z`, the DSR signing,
    checksum, SBOM, and SLSA commands required by the release contract, and
    `dsr release frankenterm X.Y.Z --verify-tag` followed by
    `dsr release verify frankenterm X.Y.Z`. Add `--prerelease` for release
    candidates. Retain the DSR manifests, signatures, checksums, provenance,
    upload receipt, and public verification output as the release evidence.

## Recommended

- **Run the headline-claim benches through RCH.**
  ```bash
  RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- env CARGO_TARGET_DIR=/tmp/ft-release-headline-benches \
    cargo bench -p frankenterm-core
  ```
  Then review `target/criterion/wa-bench-distributions.jsonl` for any
  unexplained drift in the 5 SLO claims (ft-syqcz.3 manifest). Retain
  the selected worker, target dir, artifact path, exit code, and any
  failure classification if RCH does not reach the bench binary.
- **Sanity-check the doctrine guards.** The strict remote lint lane in the DSR
  quality receipt is authoritative. A local
  `python3 scripts/check_runtime_proof_coverage.py --summary` spot-check can
  confirm 0 uncovered before tagging, but does not replace that DSR/RCH proof.
- **Smoke-test the installer entrypoint without mutating the host.** Run
  `bash -n install.sh` and `bash install.sh --help >/dev/null` against the
  exact release source to catch shell-syntax and argument-parser drift before
  users do. The installer does not provide a `--dry-run` option.
- **Test the e2e workflow trigger through RCH.**
  ```bash
  RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- env CARGO_TARGET_DIR=/tmp/ft-release-e2e-workflow \
    cargo test -p frankenterm --test e2e_workflow_trigger -- --nocapture
  ```
  Run this against the renamed binaries to confirm end-to-end paths still
  resolve, and retain the selected worker, target dir, artifact path, exit
  code, and failure classification.

## Cross-references

- `docs/release/sub-crate-publish-order.md` — `cargo publish`
  topological order for the 13+ sub-crates.
- `~/.config/dsr/repos.d/frankenterm.yaml` and the `frankenterm` quality entry
  in `~/.config/dsr/repos.yaml` — the sole release build, artifact-contract,
  signing, and strict remote quality authority.
- `scripts/stamp-readme-counts.sh` — count-drift stamper
  (ft-tf6g3.2).
- `scripts/regen-provenance.py` + `scripts/check-provenance.sh`
  — vendored-fork provenance manifest (ft-i2eni.6).
- `scripts/demo.tape` — tour VHS recording script (ft-jjvxg).
- `scripts/demo-full.tape` — full-scenario VHS recording script
  consuming the live 10-pane swarm staged per ft-xl2kc.1 (ft-xl2kc).
