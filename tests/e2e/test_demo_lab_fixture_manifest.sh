#!/usr/bin/env bash
# Static verifier for bundled demo-lab fixture manifest and retained goldens.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

MANIFEST="fixtures/demo-lab/manifest.v1.json"
INVALID_FIXTURES="fixtures/demo-lab/invalid/manifest-fragments.v1.json"
PROOF_LEDGER="fixtures/demo-lab/proof/proof-ledger.v1.jsonl"
PROOF_SUMMARY="fixtures/demo-lab/proof/summary.v1.json"
REQUIRED_SCENARIOS=(quickstart usage_limit compaction)
REQUIRED_DEGRADATIONS=(
  "agent_mail_unavailable"
  "disabled_feature"
  "rch_proof_unavailable"
  "unsupported_platform"
)
REQUIRED_INVALID_CASES=(
  "unsupported-schema-version"
  "absolute-scenario-path"
  "parent-relative-artifact-path"
  "missing-degradation-reason"
  "duplicate-scenario-id"
  "target-class-proof-overclaim"
)

fail() {
  printf 'demo-lab fixture manifest: %s\n' "$*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "missing command: $1"
}

require_file() {
  local path="$1"
  [[ -f "${path}" ]] || fail "missing file: ${path}"
}

require_repo_relative_path() {
  local path="$1"

  [[ -n "${path}" ]] || fail "empty path"
  [[ "${path}" != /* ]] || fail "absolute path is forbidden: ${path}"
  [[ "${path}" != *'..'* ]] || fail "parent-relative path is forbidden: ${path}"
}

sha256_file() {
  shasum -a 256 "$1" | awk '{print $1}'
}

require_command jq
require_command ruby
require_command shasum
require_file "${MANIFEST}"
require_file "${INVALID_FIXTURES}"
require_file "${PROOF_LEDGER}"
require_file "${PROOF_SUMMARY}"

mapfile -t scenario_paths < <(jq -r '.scenarios[].scenario_path' "${MANIFEST}")
mapfile -t json_golden_paths < <(jq -r '.scenarios[].expected_artifacts[] | select(.kind == "golden_json") | .path' "${MANIFEST}")
mapfile -t toon_golden_paths < <(jq -r '.scenarios[].expected_artifacts[] | select(.kind == "golden_toon") | .path' "${MANIFEST}")
mapfile -t structured_log_paths < <(jq -r '.scenarios[].expected_artifacts[] | select(.kind == "structured_log") | .path' "${MANIFEST}")
mapfile -t proof_summary_paths < <(jq -r '.scenarios[].expected_artifacts[] | select(.kind == "proof_summary") | .path' "${MANIFEST}")

all_json=("${MANIFEST}" "${INVALID_FIXTURES}" "${PROOF_SUMMARY}" "${json_golden_paths[@]}")
for path in "${all_json[@]}" "${scenario_paths[@]}" "${toon_golden_paths[@]}" "${structured_log_paths[@]}" "${proof_summary_paths[@]}"; do
  require_repo_relative_path "${path}"
done

jq empty "${all_json[@]}"
jq -s 'length > 0' "${PROOF_LEDGER}" >/dev/null || fail "proof ledger has no JSONL entries"
ruby -ryaml -e 'ARGV.each { |path| YAML.safe_load(File.read(path), permitted_classes: [], aliases: false); }' \
  "${scenario_paths[@]}"

jq -e \
  --arg proof_ledger "${PROOF_LEDGER}" \
  --arg proof_summary "${PROOF_SUMMARY}" \
  --argjson required "$(printf '%s\n' "${REQUIRED_SCENARIOS[@]}" | jq -R . | jq -s .)" '
  .schema_version == "ft.demo.scenario-manifest.v1"
  and (.title | type == "string" and length > 0)
  and (.proof_boundary | type == "string" and contains("not target-class high-scale production capacity evidence"))
  and ([.scenarios[].id] | sort) == ($required | sort)
  and all(.scenarios[];
    (.title | type == "string" and length > 0)
    and (.purpose | type == "string" and length > 0)
    and (.scenario_path | type == "string" and length > 0)
    and (.deterministic_seed | type == "string" and length > 0)
    and (.required_features | type == "array" and length > 0)
    and (.supported_outputs | type == "array" and length > 0)
    and (.supported_outputs | index("jsonl") != null)
    and (.redaction_tier == "t1_standard")
    and (.proof_category | IN("conformance", "golden", "e2e"))
    and (.max_output_bytes | type == "number" and . > 0)
    and (.expected_artifacts | type == "array" and length > 0)
    and any(.expected_artifacts[];
      .id == "proof_ledger" and .kind == "structured_log" and .path == $proof_ledger)
    and any(.expected_artifacts[];
      .id == "proof_summary" and .kind == "proof_summary" and .path == $proof_summary)
    and all(.expected_artifacts[];
      (.id | type == "string" and length > 0)
      and (.kind | IN("manifest", "scenario_yaml", "golden_json", "golden_toon", "structured_log", "proof_summary"))
      and (.path | type == "string" and length > 0)
      and (.max_bytes | type == "number" and . > 0)
      and (.content_hash_required == true)
    )
    and (.degradation | type == "array" and length >= 4)
    and all(.degradation[];
      (.reason | type == "string" and length > 0)
      and (.status | type == "string" and length > 0)
      and (.operator_action | type == "string" and length > 0)
    )
  )
' "${MANIFEST}" >/dev/null || fail "manifest top-level contract is incomplete"

jq -e '
  .schema_version == "ft.demo.scenario-manifest.invalid-fragments.v1"
  and .contract_id == "ft.demo.scenario-manifest.invalid-fragments.v1"
  and .manifest_path == "fixtures/demo-lab/manifest.v1.json"
  and .contract_doc == "docs/demo-scenarios.md"
  and .source_bead == "ft-lecbn.8"
  and (.verification | index("bash tests/e2e/test_demo_lab_fixture_manifest.sh") != null)
  and (.cases | length >= 6)
  and all(.cases[];
    (.case_id | type == "string" and length > 0)
    and (.expected_failure | type == "string" and length > 0)
    and (.reason_codes | type == "array" and length > 0)
    and all(.reason_codes[]; type == "string" and contains("."))
    and (.invalid_fragment | type == "object")
  )
' "${INVALID_FIXTURES}" >/dev/null || fail "invalid fixture metadata is incomplete"

for case_id in "${REQUIRED_INVALID_CASES[@]}"; do
  jq -e --arg case_id "${case_id}" '
    any(.cases[]; .case_id == $case_id)
  ' "${INVALID_FIXTURES}" >/dev/null || fail "missing invalid case ${case_id}"
  [[ "$(cat docs/demo-scenarios.md)" == *"${case_id}"* ]] || fail "docs missing invalid case ${case_id}"
done

jq -e '
  def case($id): .cases[] | select(.case_id == $id);

  ([.cases[].case_id] | length == (unique | length))
  and (case("unsupported-schema-version")
    | .expected_failure == "unsupported_schema_version"
    and (.reason_codes | index("demo.unsupported_schema_version") != null)
    and .invalid_fragment.schema_version == "ft.demo.scenario-manifest.v2")
  and (case("absolute-scenario-path")
    | .expected_failure == "scenario_path_must_be_repo_relative"
    and (.reason_codes | index("demo.absolute_path_forbidden") != null)
    and (.invalid_fragment.scenario.scenario_path | startswith("/")))
  and (case("parent-relative-artifact-path")
    | .expected_failure == "artifact_path_must_not_escape_repo"
    and (.reason_codes | index("demo.parent_relative_path_forbidden") != null)
    and any(.invalid_fragment.expected_artifacts[]; (.path | contains("../") or startswith("../"))))
  and (case("missing-degradation-reason")
    | .expected_failure == "required_degradation_reason_missing"
    and (.reason_codes | index("demo.required_degradation_missing") != null)
    and .invalid_fragment.missing_reason == "rch_proof_unavailable"
    and ([.invalid_fragment.scenario.degradation[].reason] | index("rch_proof_unavailable") == null))
  and (case("duplicate-scenario-id")
    | .expected_failure == "scenario_ids_must_be_unique"
    and (.reason_codes | index("demo.duplicate_scenario_id") != null)
    and ([.invalid_fragment.scenarios[].id] | length != (unique | length)))
  and (case("target-class-proof-overclaim")
    | .expected_failure == "proof_boundary_must_not_claim_target_class_capacity"
    and (.reason_codes | index("demo.target_class_overclaim_forbidden") != null)
    and (.invalid_fragment.proof_boundary | test("target-class|200\\+ panes|64-core|256 GiB"; "i")))
' "${INVALID_FIXTURES}" >/dev/null || fail "invalid fixtures do not cover required fail-closed cases"

for scenario_id in "${REQUIRED_SCENARIOS[@]}"; do
  scenario_path="$(jq -r --arg id "${scenario_id}" '.scenarios[] | select(.id == $id) | .scenario_path' "${MANIFEST}")"
  seed="$(jq -r --arg id "${scenario_id}" '.scenarios[] | select(.id == $id) | .deterministic_seed' "${MANIFEST}")"
  proof_category="$(jq -r --arg id "${scenario_id}" '.scenarios[] | select(.id == $id) | .proof_category' "${MANIFEST}")"
  redaction_tier="$(jq -r --arg id "${scenario_id}" '.scenarios[] | select(.id == $id) | .redaction_tier' "${MANIFEST}")"
  require_file "${scenario_path}"

  for reason in "${REQUIRED_DEGRADATIONS[@]}"; do
    jq -e --arg id "${scenario_id}" --arg reason "${reason}" '
      any(.scenarios[] | select(.id == $id) | .degradation[]; .reason == $reason)
    ' "${MANIFEST}" >/dev/null || fail "${scenario_id} missing degradation reason ${reason}"
  done

  ruby -ryaml -e '
    path, scenario_id, seed, proof_category, redaction_tier = ARGV
    doc = YAML.safe_load(File.read(path), permitted_classes: [], aliases: false)
    meta = doc.fetch("metadata")
    abort("name mismatch") unless doc["name"] == scenario_id
    abort("metadata.scenario_id mismatch") unless meta["scenario_id"] == scenario_id
    abort("metadata.seed mismatch") unless meta["seed"] == seed
    abort("metadata.proof_category mismatch") unless meta["proof_category"] == proof_category
    abort("metadata.redaction_tier mismatch") unless meta["redaction_tier"] == redaction_tier
    abort("live_services must be none") unless meta["live_services"] == "none"
    abort("missing panes") unless doc["panes"].is_a?(Array) && !doc["panes"].empty?
    abort("missing events") unless doc["events"].is_a?(Array) && !doc["events"].empty?
    abort("missing expectations") unless doc["expectations"].is_a?(Array) && !doc["expectations"].empty?
  ' "${scenario_path}" "${scenario_id}" "${seed}" "${proof_category}" "${redaction_tier}" \
    || fail "${scenario_id} YAML metadata does not match manifest"

  scenario_hash="$(sha256_file "${scenario_path}")"
  mapfile -t artifact_rows < <(jq -r --arg id "${scenario_id}" '
    .scenarios[] | select(.id == $id) | .expected_artifacts[] |
    [.kind, .path, (.max_bytes | tostring), (.sha256 // "")] | @tsv
  ' "${MANIFEST}")

  for row in "${artifact_rows[@]}"; do
    IFS=$'\t' read -r kind path max_bytes expected_sha <<<"${row}"
    require_repo_relative_path "${path}"
    if [[ ! -f "${path}" ]]; then
      case "${kind}" in
        structured_log|proof_summary) continue ;;
        *) fail "${scenario_id} expected artifact missing: ${path}" ;;
      esac
    fi

    bytes="$(wc -c < "${path}" | tr -d ' ')"
    ((bytes <= max_bytes)) || fail "${path} exceeds max_bytes ${max_bytes}"

    case "${kind}" in
      manifest)
        ;;
      structured_log|proof_summary)
        if [[ -n "${expected_sha}" ]]; then
          [[ "${expected_sha}" =~ ^[0-9a-f]{64}$ ]] || fail "${path} has invalid manifest-pinned sha256"
          actual_sha="$(sha256_file "${path}")"
          [[ "${actual_sha}" == "${expected_sha}" ]] ||
            fail "${path} sha256 drifted: expected ${expected_sha}, got ${actual_sha}"
        fi
        ;;
      *)
        [[ "${expected_sha}" =~ ^[0-9a-f]{64}$ ]] || fail "${path} missing manifest-pinned sha256"
        actual_sha="$(sha256_file "${path}")"
        [[ "${actual_sha}" == "${expected_sha}" ]] ||
          fail "${path} sha256 drifted: expected ${expected_sha}, got ${actual_sha}"
        ;;
    esac

    case "${kind}" in
      golden_json)
        jq -e --arg id "${scenario_id}" --arg seed "${seed}" --arg scenario_path "${scenario_path}" --arg scenario_hash "${scenario_hash}" '
          .schema_version == "ft.demo.golden.v1"
          and .scenario_id == $id
          and .deterministic_seed == $seed
          and .status == "passed"
          and .redaction.tier == "t1_standard"
          and .redaction.raw_secrets_present == false
          and .scenario.path == $scenario_path
          and .scenario.sha256 == $scenario_hash
          and .degradation.rch_proof_unavailable == "proof_blocked_no_local_cargo_counted"
        ' "${path}" >/dev/null || fail "${path} golden JSON metadata mismatch"
        ;;
      golden_toon)
        rg -q "^schema_version: ft\\.demo\\.golden\\.v1$" "${path}" || fail "${path} missing TOON schema_version"
        rg -q "^scenario_id: ${scenario_id}$" "${path}" || fail "${path} missing TOON scenario_id"
        rg -q "^deterministic_seed: ${seed}$" "${path}" || fail "${path} missing TOON deterministic_seed"
        rg -q "^  path: ${scenario_path}$" "${path}" || fail "${path} missing TOON scenario path"
        rg -q "^  sha256: ${scenario_hash}$" "${path}" || fail "${path} missing TOON scenario hash"
        rg -q "^  rch_proof_unavailable: proof_blocked_no_local_cargo_counted$" "${path}" || fail "${path} missing TOON RCH degradation"
        ;;
    esac
  done
done

manifest_hash="$(sha256_file "${MANIFEST}")"
ledger_hash="$(sha256_file "${PROOF_LEDGER}")"
mapfile -t ledger_scenarios < <(jq -r '.scenario_id' "${PROOF_LEDGER}" | sort)
expected_scenarios="$(printf '%s\n' "${REQUIRED_SCENARIOS[@]}" | sort)"
actual_scenarios="$(printf '%s\n' "${ledger_scenarios[@]}")"
[[ "${actual_scenarios}" == "${expected_scenarios}" ]] || fail "proof ledger scenario ids mismatch"

for scenario_id in "${REQUIRED_SCENARIOS[@]}"; do
  scenario_path="$(jq -r --arg id "${scenario_id}" '.scenarios[] | select(.id == $id) | .scenario_path' "${MANIFEST}")"
  seed="$(jq -r --arg id "${scenario_id}" '.scenarios[] | select(.id == $id) | .deterministic_seed' "${MANIFEST}")"
  proof_category="$(jq -r --arg id "${scenario_id}" '.scenarios[] | select(.id == $id) | .proof_category' "${MANIFEST}")"
  scenario_hash="$(sha256_file "${scenario_path}")"
  pane_count="$(ruby -ryaml -e 'doc = YAML.safe_load(File.read(ARGV.fetch(0)), permitted_classes: [], aliases: false); puts doc.fetch("panes").length' "${scenario_path}")"
  event_count="$(ruby -ryaml -e 'doc = YAML.safe_load(File.read(ARGV.fetch(0)), permitted_classes: [], aliases: false); puts doc.fetch("events").length' "${scenario_path}")"
  expectation_count="$(ruby -ryaml -e 'doc = YAML.safe_load(File.read(ARGV.fetch(0)), permitted_classes: [], aliases: false); puts doc.fetch("expectations").length' "${scenario_path}")"
  entry_count="$(jq -r --arg id "${scenario_id}" 'select(.scenario_id == $id) | .scenario_id' "${PROOF_LEDGER}" | wc -l | tr -d ' ')"
  [[ "${entry_count}" == "1" ]] || fail "${scenario_id} must have exactly one proof ledger entry"

  jq -e \
    --arg id "${scenario_id}" \
    --arg manifest "${MANIFEST}" \
    --arg manifest_hash "${manifest_hash}" \
    --arg scenario_path "${scenario_path}" \
    --arg scenario_hash "${scenario_hash}" \
    --arg seed "${seed}" \
    --arg proof_category "${proof_category}" \
    --arg pane_count "${pane_count}" \
    --arg event_count "${event_count}" \
    --arg expectation_count "${expectation_count}" \
    --argjson required_invalid "$(printf '%s\n' "${REQUIRED_INVALID_CASES[@]}" | jq -R . | jq -s .)" '
      select(.scenario_id == $id)
      | .schema_version == "ft.demo-lab.proof-ledger.v1"
        and .contract_id == "ft.demo_lab.proof_ledger.v1"
        and .bead_id == "ft-lecbn.4"
        and .manifest.path == $manifest
        and .manifest.sha256 == $manifest_hash
        and .scenario.path == $scenario_path
        and .scenario.sha256 == $scenario_hash
        and .scenario.deterministic_seed == $seed
        and .scenario.proof_category == $proof_category
        and ([.commands[].kind] | sort) == (["demo_json", "demo_toon", "simulate_validate_json"] | sort)
        and all(.commands[]; .exit_code == 0 and .normalized_stderr.status == "empty")
        and any(.commands[];
          .kind == "simulate_validate_json"
          and .normalized_stdout.status == "scenario_contract_validated"
          and (.normalized_stdout.pane_count | tostring) == $pane_count
          and (.normalized_stdout.event_count | tostring) == $event_count
          and (.normalized_stdout.expectation_count | tostring) == $expectation_count)
        and .proof.execution_mode == "retained_static_fixture"
        and .proof.target_dir == "not_applicable_static_fixture"
        and .proof.worker_id == "not_applicable_static_fixture"
        and .proof.remote_cargo_reached == false
        and .proof.remote_rustc_reached == false
        and .proof.test_binary_reached == false
        and .proof.local_cargo_counted == false
        and .proof.rch_status == "not_run_static_fixture"
        and .side_effects.live_panes_mutated == false
        and .side_effects.external_services_called == false
        and .side_effects.agent_mail_repair_attempted == false
        and .side_effects.rch_worker_mutated == false
        and .side_effects.file_deleted == false
        and (.negative_coverage | sort) == ($required_invalid | sort)
    ' "${PROOF_LEDGER}" >/dev/null || fail "${scenario_id} proof ledger entry mismatch"
done

jq -e \
  --arg manifest "${MANIFEST}" \
  --arg manifest_hash "${manifest_hash}" \
  --arg proof_ledger "${PROOF_LEDGER}" \
  --arg ledger_hash "${ledger_hash}" \
  --argjson required "$(printf '%s\n' "${REQUIRED_SCENARIOS[@]}" | jq -R . | jq -s .)" \
  --argjson required_invalid "$(printf '%s\n' "${REQUIRED_INVALID_CASES[@]}" | jq -R . | jq -s .)" '
    .schema_version == "ft.demo-lab.proof-summary.v1"
    and .contract_id == "ft.demo_lab.proof_summary.v1"
    and .bead_id == "ft-lecbn.4"
    and .manifest.path == $manifest
    and .manifest.sha256 == $manifest_hash
    and .proof_ledger.path == $proof_ledger
    and .proof_ledger.sha256 == $ledger_hash
    and .proof_ledger.contract_id == "ft.demo_lab.proof_ledger.v1"
    and .proof_ledger.entries == ($required | length)
    and (.scenario_ids | sort) == ($required | sort)
    and (.commands_per_scenario | length) == 3
    and .proof_state.execution_mode == "retained_static_fixture"
    and .proof_state.remote_cargo_reached == false
    and .proof_state.remote_rustc_reached == false
    and .proof_state.test_binary_reached == false
    and .proof_state.local_cargo_counted == false
    and .proof_state.target_dir == "not_applicable_static_fixture"
    and .proof_state.worker_id == "not_applicable_static_fixture"
    and .side_effects.live_panes_mutated == false
    and .side_effects.external_services_called == false
    and .side_effects.agent_mail_repair_attempted == false
    and .side_effects.rch_worker_mutated == false
    and .side_effects.file_deleted == false
    and (.negative_coverage | sort) == ($required_invalid | sort)
    and (.operator_summary | contains("do not prove remote Cargo"))
  ' "${PROOF_SUMMARY}" >/dev/null || fail "proof summary metadata mismatch"

mapfile -t closeout_children < <(printf '%s\n' \
  "ft-lecbn.1" \
  "ft-lecbn.2" \
  "ft-lecbn.3" \
  "ft-lecbn.4" \
  "ft-lecbn.5" \
  "ft-lecbn.7" \
  "ft-lecbn.8")

jq -e \
  --arg manifest "${MANIFEST}" \
  --arg manifest_hash "${manifest_hash}" \
  --arg proof_ledger "${PROOF_LEDGER}" \
  --arg ledger_hash "${ledger_hash}" \
  --argjson required "$(printf '%s\n' "${REQUIRED_SCENARIOS[@]}" | jq -R . | jq -s .)" \
  --argjson closeout_children "$(printf '%s\n' "${closeout_children[@]}" | jq -R . | jq -s .)" '
    .convergence_closeout.bead_id == "ft-lecbn.6"
    and .convergence_closeout.parent_epic == "ft-lecbn"
    and .convergence_closeout.all_children_closed == true
    and .convergence_closeout.attestation_slot.status == "not_added_schema_category_absent"
    and .convergence_closeout.attestation_slot.manifest_path == "docs/attestations/manifest.json"
    and .convergence_closeout.attestation_slot.schema_path == "docs/attestations/schema.json"
    and .convergence_closeout.attestation_slot.manifest_mutated == false
    and .convergence_closeout.attestation_slot.readme_claim_graduated == false
    and ([.child_acceptance_evidence[].bead_id] | sort) == ($closeout_children | sort)
    and all(.child_acceptance_evidence[];
      .status == "closed"
      and (.acceptance | type == "string" and length > 0)
      and (.artifact_paths | type == "array" and length > 0)
      and (.proof_commands | type == "array" and length > 0)
      and all(.proof_commands[];
        (.command | type == "string" and length > 0)
        and (.worker_id | type == "string" and length > 0)
        and (.target_dir | type == "string" and length > 0)
        and (.exit_code == 0)
        and (.cargo_reached | type == "boolean")
        and (.test_reached | type == "boolean")
        and .local_cargo_counted == false
        and (.output_summary | type == "string" and length > 0)
      )
    )
    and any(.convergence_closeout.verification_commands[];
      .command == "bash tests/e2e/test_demo_lab_fixture_manifest.sh"
      and .exit_code == 0
      and .local_cargo_counted == false)
    and any(.convergence_closeout.verification_commands[];
      .command == "br dep cycles --json"
      and .exit_code == 0
      and .local_cargo_counted == false)
    and any(.convergence_closeout.verification_commands[];
      (.command | startswith("git diff --check -- "))
      and .exit_code == 0
      and .local_cargo_counted == false)
    and all(.convergence_closeout.forbidden_actions[]; . == false)
    and (.scenario_artifact_hashes | length) == ($required | length)
    and all(.scenario_artifact_hashes[];
      .scenario_id as $scenario_id
      | ($required | index($scenario_id) != null)
        and any(.shared_artifacts[];
          .kind == "manifest" and .path == $manifest and .sha256 == $manifest_hash)
        and any(.shared_artifacts[];
          .kind == "proof_ledger" and .path == $proof_ledger and .sha256 == $ledger_hash)
        and any(.shared_artifacts[];
          .kind == "proof_summary"
          and .path == "fixtures/demo-lab/proof/summary.v1.json"
          and .sha256 == "recorded_in_bead_closeout_comment_to_avoid_self_hash_cycle")
    )
  ' "${PROOF_SUMMARY}" >/dev/null || fail "proof summary convergence closeout mismatch"

for scenario_id in "${REQUIRED_SCENARIOS[@]}"; do
  scenario_path="$(jq -r --arg id "${scenario_id}" '.scenarios[] | select(.id == $id) | .scenario_path' "${MANIFEST}")"
  scenario_hash="$(sha256_file "${scenario_path}")"

  jq -e --arg id "${scenario_id}" --arg path "${scenario_path}" --arg hash "${scenario_hash}" '
    any(.scenario_artifact_hashes[];
      .scenario_id == $id and .scenario.path == $path and .scenario.sha256 == $hash)
  ' "${PROOF_SUMMARY}" >/dev/null || fail "${scenario_id} closeout scenario hash mismatch"

  mapfile -t closeout_golden_rows < <(jq -r --arg id "${scenario_id}" '
    .scenarios[] | select(.id == $id) | .expected_artifacts[] |
    select(.kind == "golden_json" or .kind == "golden_toon") |
    [.kind, .path, .sha256] | @tsv
  ' "${MANIFEST}")

  for row in "${closeout_golden_rows[@]}"; do
    IFS=$'\t' read -r kind path expected_sha <<<"${row}"
    jq -e --arg id "${scenario_id}" --arg kind "${kind}" --arg path "${path}" --arg hash "${expected_sha}" '
      any(.scenario_artifact_hashes[] | select(.scenario_id == $id) | .goldens[];
        .kind == $kind and .path == $path and .sha256 == $hash)
    ' "${PROOF_SUMMARY}" >/dev/null || fail "${scenario_id} closeout ${kind} hash mismatch"
  done
done

if rg -n --hidden --glob '!*.md' \
  '(sk-[A-Za-z0-9]{20,}|AKIA[0-9A-Z]{16}|ghp_[A-Za-z0-9]{20,}|xox[baprs]-[A-Za-z0-9-]{20,}|Bearer [A-Za-z0-9._-]{20,}|BEGIN (RSA|OPENSSH|EC) PRIVATE KEY)' \
  fixtures/demo-lab >/tmp/ft-demo-lab-secret-scan.txt; then
  cat /tmp/ft-demo-lab-secret-scan.txt >&2
  fail "secret-shaped strings found in demo-lab fixtures"
fi

invalid_case_count="$(jq '.cases | length' "${INVALID_FIXTURES}")"
ledger_entry_count="$(jq -s 'length' "${PROOF_LEDGER}")"

printf 'demo-lab fixture manifest: static verifier passed (%d scenarios, %d json goldens, %d toon goldens, %d invalid cases, %d proof-ledger entries)\n' \
  "${#REQUIRED_SCENARIOS[@]}" "${#json_golden_paths[@]}" "${#toon_golden_paths[@]}" "${invalid_case_count}" "${ledger_entry_count}"
