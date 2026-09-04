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
#   scripts/release-gates.sh --cargo --fuzz-campaign SECONDS
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
    --only)
      [[ $# -ge 2 && -n "$2" && "$2" != --* ]] || { echo '--only needs a gate name' >&2; exit 2; }
      shift; ONLY+=("$1") ;;
    --fuzz-campaign)
      [[ $# -ge 2 && "$2" =~ ^[1-9][0-9]*$ ]] || { echo '--fuzz-campaign needs a positive integer number of seconds' >&2; exit 2; }
      shift; FUZZ_SECONDS="$1" ;;
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
gate "release gate selection"                    "bash tests/e2e/test_release_gate_selection.sh"
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
# --- Web API liveness ------------------------------------------------------
# v0.15.1 shipped an `ft web` that bound its port and never answered a single
# request (ft-xxfwy.38 / plan G80): fastapi-http's accept timeout was judged
# against a different clock than the runtime's, so once process uptime passed
# the 50 ms accept interval every accept future was born expired. This test
# starts the real FrameworkWebRuntime after the runtime clock has advanced
# past that interval and requires an HTTP 200 over a plain TCP socket.
cargo_gate "web api liveness after clock skew"  "cargo test -p frankenterm-core --lib web_framework::tests::server_started_after_runtime_clock_skew_answers_requests"

selected() {
  [[ ${#ONLY[@]} -eq 0 ]] && return 0
  local want
  for want in "${ONLY[@]}"; do
    [[ "$want" == "$1" ]] && return 0
  done
  return 1
}

# Validate the entire request before any gate can change generated artifacts.
# In particular, a valid name followed by a typo must not partially execute.
for want in "${ONLY[@]}"; do
  found=0
  for name in "${GATE_NAMES[@]}"; do
    [[ "$want" != "$name" ]] || found=1
  done
  [[ $found -eq 1 ]] || { printf 'unknown gate: %s\n' "$want" >&2; exit 2; }
done

if [[ $LIST_ONLY -eq 1 ]]; then
  for i in "${!GATE_NAMES[@]}"; do
    selected "${GATE_NAMES[$i]}" || continue
    printf '%-7s %-46s %s\n' "[${GATE_KINDS[$i]}]" "${GATE_NAMES[$i]}" "${GATE_CMDS[$i]}"
  done
  exit 0
fi

eligible=0
for i in "${!GATE_NAMES[@]}"; do
  name="${GATE_NAMES[$i]}"; cmd="${GATE_CMDS[$i]}"
  selected "$name" || continue
  if [[ "${GATE_KINDS[$i]}" == cargo && $WITH_CARGO -eq 0 ]]; then
    if [[ ${#ONLY[@]} -gt 0 ]]; then
      printf 'cargo gate requires --cargo on an admissible host: %s\n' "$name" >&2
      exit 2
    fi
    continue
  fi
  read -r first_word second_word rest <<< "$cmd"
  script=""
  needs_execute=0
  case "$first_word" in
    scripts/*|tests/*) script="$first_word"; needs_execute=1 ;;
    bash|python3) script="$second_word" ;;
  esac
  if [[ -n "$script" ]] && { [[ ! -f "$script" || ! -r "$script" ]] || { [[ $needs_execute -eq 1 && ! -x "$script" ]]; }; }; then
    printf 'required gate unavailable: %s (%s)\n' "$name" "$script" >&2
    exit 1
  fi
  if [[ $needs_execute -eq 0 ]] && ! command -v "$first_word" >/dev/null 2>&1; then
    printf 'required gate executable unavailable: %s (%s)\n' "$name" "$first_word" >&2
    exit 1
  fi
  eligible=$((eligible + 1))
done

FUZZ_TARGETS=""
if [[ -n "$FUZZ_SECONDS" ]]; then
  [[ $WITH_CARGO -eq 1 ]] || { echo 'fuzz campaign requires --cargo on an admissible host' >&2; exit 2; }
  manifest=docs/security/adversarial-contract-fuzz.json
  # Validate and capture in the parent shell: process substitution would hide
  # jq failure and could report a successful campaign with zero targets.
  if ! FUZZ_TARGETS=$(jq -er '
    .targets | select(type == "array" and length > 0)
    | select((map(.cargo_fuzz_target) | unique | length) == length)
    | select(all(.[];
        (.cargo_fuzz_target | type == "string" and test("^[a-z][a-z0-9_]*$")) and
        (.seed_corpus | type == "string" and test("^fuzz/([A-Za-z0-9_-]+/)+$"))))
    | .[] | [.cargo_fuzz_target, .seed_corpus] | @tsv
  ' "$manifest"); then
    echo 'invalid or empty adversarial contract-fuzz target manifest' >&2
    exit 1
  fi
  while IFS=$'\t' read -r target corpus; do
    [[ -d "$corpus" ]] || { printf 'missing fuzz seed corpus: %s\n' "$corpus" >&2; exit 1; }
  done <<< "$FUZZ_TARGETS"
  command -v cargo-fuzz >/dev/null 2>&1 || { echo 'cargo-fuzz not installed' >&2; exit 1; }
fi
[[ $eligible -gt 0 || -n "$FUZZ_TARGETS" ]] || { echo 'no executable gates selected' >&2; exit 1; }

LOG_DIR=$(mktemp -d "${TMPDIR:-/tmp}/ft-release-gates.XXXXXXXX") || exit 1
printf 'Retained gate logs: %s\n' "$LOG_DIR"
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
  log_file="$LOG_DIR/gate-$i.log"
  start=$(date +%s)
  if bash -c "$cmd" >"$log_file" 2>&1; then
    printf 'PASS %-46s (%ss)\n' "$name" "$(( $(date +%s) - start ))"
    pass=$((pass + 1))
  else
    printf 'FAIL %-46s (%ss) — last lines:\n' "$name" "$(( $(date +%s) - start ))"
    tail -n 8 "$log_file" | sed 's/^/     | /'
    fail=$((fail + 1))
    FAILED+=("$name")
  fi
done

# --- Optional adversarial contract-fuzz campaign ---------------------------
# Runs each manifest target with cargo-fuzz for the requested wall-clock
# budget. The manifest declares 1800s (pull-request) and 86400s (release)
# per target; pass the value for the campaign you are running.
if [[ -n "$FUZZ_SECONDS" ]]; then
    while IFS=$'\t' read -r target corpus; do
      name="fuzz:${target}"
      log_file="$LOG_DIR/fuzz-$target.log"
      start=$(date +%s)
      if (cd fuzz && cargo fuzz run "$target" "${corpus#fuzz/}" -- -max_total_time="$FUZZ_SECONDS") >"$log_file" 2>&1; then
        printf 'PASS %-46s (%ss)\n' "$name" "$(( $(date +%s) - start ))"; pass=$((pass + 1))
      else
        printf 'FAIL %-46s (%ss) — last lines:\n' "$name" "$(( $(date +%s) - start ))"
        tail -n 8 "$log_file" | sed 's/^/     | /'
        fail=$((fail + 1)); FAILED+=("$name")
      fi
    done <<< "$FUZZ_TARGETS"
fi

echo "release gates: ${pass} passed, ${fail} failed, ${skip} skipped"
if [[ $fail -gt 0 || $pass -eq 0 ]]; then
  printf '  failed: %s\n' "${FAILED[@]}"
  exit 1
fi
exit 0
