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
   `bash scripts/stamp-readme-counts.sh` so every documented
   workspace count matches HEAD (ft-i2eni.5). CI's drift check
   will block a divergent release.
4. **Regenerate the vendored-fork provenance manifest.** Run
   `python3 scripts/regen-provenance.py` so
   `frankenterm/PROVENANCE.json` reflects the latest fork-side
   commits (ft-i2eni.6). CI's
   `Vendored-fork provenance manifest check` step refuses a
   release with stale data.
5. **Run the bench-stats verdict in enforce mode.** Ensure
   `bash scripts/check_bench_stats.sh` (with
   `BENCH_STATS_MODE=enforce` set) returns clean against the
   `origin/main` baseline (ft-9zzkg).
6. **Re-record the README demo GIF.** Run
   `vhs scripts/demo.tape` to refresh `assets/demo.gif`
   (ft-jjvxg). Eyeball the rendered GIF for stale CLI options or
   missing flags. The tape itself is intentionally a "tour" — the
   full multi-agent demo lands under a follow-up bead and gets
   re-recorded against the same release.
7. **Run the attestation closure checklist.** Walk
   [`docs/release/attestation-checklist.md`](attestation-checklist.md)
   pre-flight: confirm every required-category producing bead
   is closed, then `scripts/attestation-build.sh` +
   `scripts/attestation-verify.sh` + the smoke test.
   Block tagging if the bundle is partial or any verification
   step fails (ft-187kv).
8. **Tag and push.** `git tag vX.Y.0 && git push origin vX.Y.0`.
   The release workflow at `.github/workflows/release.yml`
   handles the rest (binaries, checksums, GitHub release notes,
   sigstore-signed attestation bundle).

## Recommended

- **Run the headline-claim benches.** `cargo bench -p
  frankenterm-core` followed by reviewing
  `target/criterion/wa-bench-distributions.jsonl` for any
  unexplained drift in the 5 SLO claims (ft-syqcz.3 manifest).
- **Sanity-check the doctrine guards.** The CI lint job is
  authoritative, but a local
  `python3 scripts/check_runtime_proof_coverage.py --summary`
  spot-check confirms 0 uncovered before tagging.
- **Smoke-test the installer.** `bash install.sh --dry-run`
  against `vX.Y.0` (the tagged version) to catch `curl|bash` shape
  drift before users do.
- **Test the e2e workflow trigger.** `cargo test --test
  e2e_workflow_trigger` against the renamed binaries to confirm
  end-to-end paths still resolve.

## Cross-references

- `docs/release/sub-crate-publish-order.md` — `cargo publish`
  topological order for the 13+ sub-crates.
- `.github/workflows/release.yml` — automated artifact build +
  upload on tag push.
- `scripts/stamp-readme-counts.sh` — count-drift stamper
  (ft-i2eni.5).
- `scripts/regen-provenance.py` + `scripts/check-provenance.sh`
  — vendored-fork provenance manifest (ft-i2eni.6).
- `scripts/demo.tape` — VHS recording script (ft-jjvxg).
