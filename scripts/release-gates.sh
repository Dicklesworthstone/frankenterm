#!/usr/bin/env bash
# scripts/release-gates.sh — the repo-side static release gate.
#
# FrankenTerm releases through DSR only (AGENTS.md Rule 0.1); the GitHub
# workflow files that used to run these checks were retired on 2026-09-01
# (ft-xxfwy.16). This script is the single place the static verifiers are
# wired so `dsr quality frankenterm` (or an operator) can run them all with one
# command and so wiring tests have one file to grep.
#
# Scope: static, repo-local checks only. Cargo builds/tests/benches stay on the
# remote-required RCH lanes configured in the DSR quality checks; nothing here
# is a substitute for that proof.
#
# Usage:
#   scripts/release-gates.sh              # run every gate, report, exit non-zero on any failure
#   scripts/release-gates.sh --list       # print gate names and commands without running
#   scripts/release-gates.sh --only NAME  # run one gate (repeatable)
#   scripts/release-gates.sh --fuzz-campaign SECONDS
#                                         # additionally run the adversarial contract-fuzz
#                                         # campaign from docs/security/adversarial-contract-fuzz.json
#                                         # for SECONDS per target (needs cargo-fuzz; use the
#                                         # manifest's pull_request/release seconds)
set -u
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 1

LIST_ONLY=0
WITH_CARGO=0
FUZZ_SECONDS=""
declare -a ONLY=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --list) LIST_ONLY=1 ;;
    --cargo) WITH_CARGO=1 ;;
    --only) shift; ONLY+=("${1:?--only needs a gate name}") ;;
    --fuzz-campaign) shift; FUZZ_SECONDS="${1:?--fuzz-campaign needs seconds}" ;;
    -h|--help) sed -n '2,22p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

declare -a GATE_NAMES=() GATE_CMDS=() GATE_KINDS=()
# gate NAME CMD            -> static: grep/jq/python over the tree, safe anywhere
# cargo_gate NAME CMD      -> invokes cargo check/test; runs only with --cargo,
#                             on a host where Cargo execution is admissible
#                             (an RCH worker or the DSR quality lane), never as
#                             a local substitute for remote proof.
gate() { GATE_NAMES+=("$1"); GATE_CMDS+=("$2"); GATE_KINDS+=("static"); }
cargo_gate() { GATE_NAMES+=("$1"); GATE_CMDS+=("$2"); GATE_KINDS+=("cargo"); }

# --- Source doctrine -------------------------------------------------------
gate "asupersync test-only doctrine"            "scripts/check_asupersync_test_only.sh"
gate "runtime_compat residuals"                  "scripts/check_runtime_compat_residuals.sh"
gate "runtime proof coverage"                    "python3 scripts/check_runtime_proof_coverage.py"
gate "runtime proof soundness (Lean)"            "scripts/check-runtime-proof-soundness.sh"
gate "loom skeleton coverage"                    "scripts/check_loom_skeleton_coverage.sh"
gate "lua api no promise block_on"               "bash scripts/check_lua_api_no_promise_block_on.sh"
gate "mux interface imports"                     "scripts/check_mux_interface_imports.sh"
cargo_gate "workspace cycles"                    "scripts/check_workspace_cycles.sh"
cargo_gate "feature flag matrix"                 "bash scripts/check_feature_flag_matrix.sh"
gate "release panic contract (profiles)"         "bash scripts/check-release-panic-contract.sh --profiles-only"
gate "windows/unix coupling ratchet"             "bash scripts/check_windows_unix_coupling.sh"
cargo_gate "finish-line guards"                  "bash scripts/check_finish_line_guards.sh"
gate "spec conventions"                          "scripts/check-spec-conventions.sh"
# --- Generated artifacts and docs ------------------------------------------
gate "generated artifacts"                       "scripts/check_generated_artifacts.sh"
gate "renderer corpus drift"                     "scripts/check-renderer-corpus-drift.sh"
gate "codec version release notes"               "scripts/check_codec_version_release_notes.sh"
gate "readme counts"                             "bash scripts/stamp-readme-counts.sh --check"
gate "vendored provenance"                       "bash scripts/check-provenance.sh"
cargo_gate "ftui guardrails"                     "scripts/check_ftui_guardrails.sh"
cargo_gate "ftui tests"                          "scripts/check_ftui_tests.sh"
cargo_gate "ftui docs"                           "scripts/check_ftui_docs.sh"
gate "reality-check bead structure"              "scripts/check-reality-check-bead-structure.sh"
# --- Attestation and contract verifiers ------------------------------------
# Rebuilds the dev-channel bundle from the current tree (it is a generated
# artifact, like PROVENANCE.json) and verifies it. A stale committed bundle
# therefore surfaces as generated-artifact drift, not as a hash mismatch
# against history. The first signed release bundle is ft-xxfwy.15.
gate "attestation dev bundle build+verify"       "bash scripts/attestation-build.sh --version 0.0.0-dev --channel dev --sign unsigned --allow-partial >/dev/null && bash scripts/attestation-verify.sh docs/attestations/0.0.0-dev.json"
gate "deferred proof family integrity"           "tests/e2e/test_deferred_proof_family_integrity.sh"
gate "deferred proof family conformance"         "tests/e2e/test_deferred_proof_family_conformance.sh"
gate "deferred proof comment extractor contract" "tests/e2e/test_deferred_proof_comment_extractor_contract.sh"
gate "deferred proof ownership gate"             "tests/e2e/test_deferred_proof_ownership_gate.sh"
gate "deferred proof queue surface contract"     "tests/e2e/test_deferred_proof_queue_surface_contract.sh"
gate "deferred proof receipt contract"           "tests/e2e/test_deferred_proof_receipt_contract.sh"
gate "deferred proof replay harness contract"    "tests/e2e/test_deferred_proof_replay_harness_contract.sh"
gate "adversarial contract-fuzz manifest"        "tests/e2e/test_adversarial_contract_fuzz_manifest.sh"
gate "Robot/MCP Contract Doctor static verdict"  "bash scripts/check-contract-doctor-coverage.sh"
# The doctor slot may be populated (producer closed) or explicitly deferred to
# its producer bead; either is an honest manifest state. A silently missing
# slot is the failure.
gate "Robot/MCP Contract Doctor attestation slot" "jq -e '.slots[] | select(.category == \"proofs/robot-contracts\") | select((.path == \"docs/attestations/proofs/robot-contract-doctor.json\" and .produced_by_bead == \"ft-7h5da.13.7\") or (.path == null and .deferred_to_bead == \"ft-7h5da.13.7\")) | select(.proof_categories | index(4))' docs/attestations/manifest.json"
gate "Robot/MCP Contract Doctor verdict contract" "tests/e2e/test_robot_contract_doctor_verdict_contract.sh"
cargo_gate "Robot/MCP Contract Doctor cargo verdict" "cargo test -p frankenterm-core --lib robot_api_contracts -- --nocapture"

if [[ $LIST_ONLY -eq 1 ]]; then
  for i in "${!GATE_NAMES[@]}"; do
    printf '%-7s %-46s %s\n' "[${GATE_KINDS[$i]}]" "${GATE_NAMES[$i]}" "${GATE_CMDS[$i]}"
  done
  exit 0
fi

selected() {
  [[ ${#ONLY[@]} -eq 0 ]] && return 0
  local want
  for want in "${ONLY[@]}"; do
    [[ "$want" == "$1" ]] && return 0
  done
  return 1
}

pass=0 fail=0 skip=0
declare -a FAILED=()
for i in "${!GATE_NAMES[@]}"; do
  name="${GATE_NAMES[$i]}"; cmd="${GATE_CMDS[$i]}"
  selected "$name" || continue
  if [[ "${GATE_KINDS[$i]}" == cargo && $WITH_CARGO -eq 0 ]]; then
    printf 'SKIP %-46s (cargo gate; rerun with --cargo on an admissible host)\n' "$name"
    skip=$((skip + 1))
    continue
  fi
  first_word="${cmd%% *}"
  if [[ "$first_word" == scripts/* || "$first_word" == tests/* ]] && [[ ! -x "$first_word" ]]; then
    printf 'SKIP %-46s (missing or not executable: %s)\n' "$name" "$first_word"
    skip=$((skip + 1))
    continue
  fi
  start=$(date +%s)
  if bash -c "$cmd" >"/tmp/release-gate.$$.log" 2>&1; then
    printf 'PASS %-46s (%ss)\n' "$name" "$(( $(date +%s) - start ))"
    pass=$((pass + 1))
  else
    printf 'FAIL %-46s (%ss) — last lines:\n' "$name" "$(( $(date +%s) - start ))"
    tail -n 8 "/tmp/release-gate.$$.log" | sed 's/^/     | /'
    fail=$((fail + 1))
    FAILED+=("$name")
  fi
  rm -f "/tmp/release-gate.$$.log"
done

# --- Optional adversarial contract-fuzz campaign ---------------------------
# Runs each manifest target with cargo-fuzz for the requested wall-clock
# budget. The manifest declares 1800s (pull-request) and 86400s (release)
# per target; pass the value for the campaign you are running.
if [[ -n "$FUZZ_SECONDS" ]]; then
  manifest=docs/security/adversarial-contract-fuzz.json
  if ! command -v cargo-fuzz >/dev/null 2>&1 && ! cargo fuzz --version >/dev/null 2>&1; then
    echo "FAIL adversarial contract-fuzz campaign (cargo-fuzz not installed)"
    fail=$((fail + 1)); FAILED+=("adversarial contract-fuzz campaign")
  else
    while IFS=$'\t' read -r target corpus; do
      name="fuzz:${target}"
      start=$(date +%s)
      if (cd fuzz && cargo fuzz run "$target" "${corpus#fuzz/}" -- -max_total_time="$FUZZ_SECONDS") >"/tmp/release-gate.$$.log" 2>&1; then
        printf 'PASS %-46s (%ss)\n' "$name" "$(( $(date +%s) - start ))"; pass=$((pass + 1))
      else
        printf 'FAIL %-46s (%ss) — last lines:\n' "$name" "$(( $(date +%s) - start ))"
        tail -n 8 "/tmp/release-gate.$$.log" | sed 's/^/     | /'
        fail=$((fail + 1)); FAILED+=("$name")
      fi
      rm -f "/tmp/release-gate.$$.log"
    done < <(jq -r '.targets[] | [.cargo_fuzz_target, .corpus] | @tsv' "$manifest")
  fi
fi

echo "release gates: ${pass} passed, ${fail} failed, ${skip} skipped"
if [[ $fail -gt 0 ]]; then
  printf '  failed: %s\n' "${FAILED[@]}"
  exit 1
fi
exit 0
