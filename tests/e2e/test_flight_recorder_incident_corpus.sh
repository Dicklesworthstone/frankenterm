#!/usr/bin/env bash
# Static verifier for ft-ogr3n.6 retained flight-recorder incident corpus.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

MANIFEST="fixtures/flight-recorder/incident-corpus/manifest.v1.json"
INVALID_FIXTURES="fixtures/flight-recorder/incident-corpus/invalid/fragments.v1.json"
PROOF_LEDGER="fixtures/flight-recorder/incident-corpus/proof/proof-ledger.v1.jsonl"
PROOF_SUMMARY="fixtures/flight-recorder/incident-corpus/proof/summary.v1.json"
REQUIRED_SCENARIOS=(
  "rch_mirror_before_cargo"
  "remote_cargo_pass"
  "dirty_tree_contaminated"
  "agent_mail_outage_beads_fallback"
  "bead_closeout_mirror"
)
REQUIRED_NEGATIVE_CASES=(
  "missing-source-set-artifact"
  "duplicate-scenario-id"
  "absolute-artifact-path"
  "parent-relative-artifact-path"
  "local-cargo-counted"
  "agent-mail-service-repair"
  "remote-pass-missing-worker"
)

fail() {
  printf 'flight-recorder incident corpus: %s\n' "$*" >&2
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
require_command rg
require_command shasum
require_file "${MANIFEST}"
require_file "${INVALID_FIXTURES}"
require_file "${PROOF_LEDGER}"
require_file "${PROOF_SUMMARY}"

jq empty "${MANIFEST}" "${INVALID_FIXTURES}" "${PROOF_SUMMARY}"
jq -s 'length > 0' "${PROOF_LEDGER}" >/dev/null || fail "proof ledger has no entries"

manifest_hash="$(sha256_file "${MANIFEST}")"
invalid_hash="$(sha256_file "${INVALID_FIXTURES}")"
ledger_hash="$(sha256_file "${PROOF_LEDGER}")"

jq -e \
  --argjson required "$(printf '%s\n' "${REQUIRED_SCENARIOS[@]}" | jq -R . | jq -s .)" \
  --argjson negative "$(printf '%s\n' "${REQUIRED_NEGATIVE_CASES[@]}" | jq -R . | jq -s .)" '
    .schema_version == "ft.flight_recorder.incident_corpus.v1"
    and .contract_id == "ft.flight_recorder.incident_corpus.v1"
    and .bead_id == "ft-ogr3n.6"
    and (.proof_boundary | contains("Retained source-set replay fixtures"))
    and .verification_command == "bash tests/e2e/test_flight_recorder_incident_corpus.sh"
    and .rust_verification_filter == "cargo test -p frankenterm-core-replay replay_incidents --lib -- --nocapture"
    and .proof_ledger_path == "fixtures/flight-recorder/incident-corpus/proof/proof-ledger.v1.jsonl"
    and .proof_summary_path == "fixtures/flight-recorder/incident-corpus/proof/summary.v1.json"
    and .invalid_fragments_path == "fixtures/flight-recorder/incident-corpus/invalid/fragments.v1.json"
    and (.required_scenarios | sort) == ($required | sort)
    and (.required_negative_cases | sort) == ($negative | sort)
    and ([.scenarios[].id] | length == (unique | length))
    and ([.scenarios[].id] | sort) == ($required | sort)
    and all(.scenarios[];
      (.title | type == "string" and length > 0)
      and (.source_set.path | type == "string" and length > 0)
      and (.source_set.sha256 | test("^[0-9a-f]{64}$"))
      and (.golden_json.path | type == "string" and length > 0)
      and (.golden_json.sha256 | test("^[0-9a-f]{64}$"))
      and (.golden_toon.path | type == "string" and length > 0)
      and (.golden_toon.sha256 | test("^[0-9a-f]{64}$"))
      and (.expected_outcome | IN("source_pass", "infrastructure_block", "contaminated_proof_attempt", "source_unavailable", "proof_incomplete"))
      and (.expected_proof_admissible | type == "boolean")
      and (.material_remote_rch_metadata | type == "boolean")
    )
    and .side_effects_forbidden.local_cargo_counted == true
    and .side_effects_forbidden.agent_mail_service_restarted == true
  ' "${MANIFEST}" >/dev/null || fail "manifest contract mismatch"

jq -e \
  --arg manifest "${MANIFEST}" \
  --argjson negative "$(printf '%s\n' "${REQUIRED_NEGATIVE_CASES[@]}" | jq -R . | jq -s .)" '
    .schema_version == "ft.flight_recorder.incident_corpus.invalid_fragments.v1"
    and .contract_id == "ft.flight_recorder.incident_corpus.invalid_fragments.v1"
    and .bead_id == "ft-ogr3n.6"
    and .manifest_path == $manifest
    and .verification_command == "bash tests/e2e/test_flight_recorder_incident_corpus.sh"
    and ([.cases[].case_id] | sort) == ($negative | sort)
    and ([.cases[].case_id] | length == (unique | length))
    and all(.cases[];
      (.expected_failure | type == "string" and length > 0)
      and (.reason_codes | type == "array" and length > 0)
      and all(.reason_codes[]; type == "string" and startswith("incident_corpus."))
      and (.invalid_fragment | type == "object")
    )
  ' "${INVALID_FIXTURES}" >/dev/null || fail "invalid fragments contract mismatch"

for case_id in "${REQUIRED_NEGATIVE_CASES[@]}"; do
  jq -e --arg case_id "${case_id}" 'any(.cases[]; .case_id == $case_id)' \
    "${INVALID_FIXTURES}" >/dev/null || fail "missing negative case ${case_id}"
done

mapfile -t manifest_paths < <(jq -r '
  .proof_ledger_path,
  .proof_summary_path,
  .invalid_fragments_path,
  (.scenarios[].source_set.path),
  (.scenarios[].golden_json.path),
  (.scenarios[].golden_toon.path)
' "${MANIFEST}")

for path in "${manifest_paths[@]}"; do
  require_repo_relative_path "${path}"
  require_file "${path}"
done

for scenario_id in "${REQUIRED_SCENARIOS[@]}"; do
  source_path="$(jq -r --arg id "${scenario_id}" '.scenarios[] | select(.id == $id) | .source_set.path' "${MANIFEST}")"
  source_sha="$(jq -r --arg id "${scenario_id}" '.scenarios[] | select(.id == $id) | .source_set.sha256' "${MANIFEST}")"
  golden_json="$(jq -r --arg id "${scenario_id}" '.scenarios[] | select(.id == $id) | .golden_json.path' "${MANIFEST}")"
  golden_json_sha="$(jq -r --arg id "${scenario_id}" '.scenarios[] | select(.id == $id) | .golden_json.sha256' "${MANIFEST}")"
  golden_toon="$(jq -r --arg id "${scenario_id}" '.scenarios[] | select(.id == $id) | .golden_toon.path' "${MANIFEST}")"
  golden_toon_sha="$(jq -r --arg id "${scenario_id}" '.scenarios[] | select(.id == $id) | .golden_toon.sha256' "${MANIFEST}")"
  expected_outcome="$(jq -r --arg id "${scenario_id}" '.scenarios[] | select(.id == $id) | .expected_outcome' "${MANIFEST}")"
  expected_admissible="$(jq -r --arg id "${scenario_id}" '.scenarios[] | select(.id == $id) | .expected_proof_admissible' "${MANIFEST}")"
  material_remote="$(jq -r --arg id "${scenario_id}" '.scenarios[] | select(.id == $id) | .material_remote_rch_metadata' "${MANIFEST}")"

  [[ "$(sha256_file "${source_path}")" == "${source_sha}" ]] || fail "${source_path} sha256 drifted"
  [[ "$(sha256_file "${golden_json}")" == "${golden_json_sha}" ]] || fail "${golden_json} sha256 drifted"
  [[ "$(sha256_file "${golden_toon}")" == "${golden_toon_sha}" ]] || fail "${golden_toon} sha256 drifted"

  jq empty "${source_path}" "${golden_json}"
  jq -e '
    .schema_version == 1
    and .contract_id == "ft.swarm.source_adapters.v1"
    and (.source_set_id | type == "string" and length > 0)
    and all([
      (.pane_runtime[]?.artifact_paths[]?),
      (.beads[]?.artifact_paths[]?),
      (.rch[]?.artifact_paths[]?),
      (.git[]?.artifact_paths[]?),
      (.agent_mail[]?.artifact_paths[]?)
    ] | flatten[]; (startswith("/") | not) and (contains("..") | not))
  ' "${source_path}" >/dev/null || fail "${source_path} source-set contract mismatch"

  jq -e \
    --arg id "${scenario_id}" \
    --arg source_path "${source_path}" \
    --arg outcome "${expected_outcome}" \
    --argjson admissible "${expected_admissible}" '
      .schema_version == "ft.flight_recorder.incident_golden.v1"
      and .contract_id == "ft.flight_recorder.incident_golden.v1"
      and .bead_id == "ft-ogr3n.6"
      and .scenario_id == $id
      and .source_set_path == $source_path
      and .surface_contract_id == "ft.swarm.incident_surfaces.v1"
      and .expected.outcome == $outcome
      and .expected.proof_admissible == $admissible
      and (.expected.frame_count | type == "number" and . > 0)
      and (.expected.required_sources | type == "array" and length > 0)
      and (.expected.required_causal_classes | type == "array" and length > 0)
      and (.expected.required_artifact_uris | type == "array" and length > 0)
      and (.expected.forbidden_substrings | index("ghp_") != null)
    ' "${golden_json}" >/dev/null || fail "${golden_json} semantic golden mismatch"

  rg -q "^schema_version: ft\\.flight_recorder\\.incident_golden\\.v1$" "${golden_toon}" || fail "${golden_toon} missing schema"
  rg -q "^scenario_id: ${scenario_id}$" "${golden_toon}" || fail "${golden_toon} missing scenario id"
  rg -q "^outcome: ${expected_outcome}$" "${golden_toon}" || fail "${golden_toon} missing outcome"
  rg -q "^proof_admissible: ${expected_admissible}$" "${golden_toon}" || fail "${golden_toon} missing proof admissibility"
  rg -q "^local_cargo_counted: false$" "${golden_toon}" || fail "${golden_toon} must forbid local Cargo proof"

  mapfile -t required_source_substrings < <(jq -r '.expected.required_source_set_substrings[]' "${golden_json}")
  for required in "${required_source_substrings[@]}"; do
    rg -F -q "${required}" "${source_path}" || fail "${source_path} missing required source substring: ${required}"
  done

  entry_count="$(jq -r --arg id "${scenario_id}" 'select(.scenario_id == $id) | .scenario_id' "${PROOF_LEDGER}" | wc -l | tr -d ' ')"
  [[ "${entry_count}" == "1" ]] || fail "${scenario_id} must have exactly one proof-ledger row"
  jq -e \
    --arg id "${scenario_id}" \
    --arg manifest "${MANIFEST}" \
    --arg manifest_hash "${manifest_hash}" \
    --arg source_path "${source_path}" \
    --arg source_sha "${source_sha}" \
    --arg golden_json "${golden_json}" \
    --arg golden_json_sha "${golden_json_sha}" \
    --arg golden_toon "${golden_toon}" \
    --arg golden_toon_sha "${golden_toon_sha}" \
    --arg outcome "${expected_outcome}" \
    --argjson admissible "${expected_admissible}" \
    --argjson material_remote "${material_remote}" '
      select(.scenario_id == $id)
      | .schema_version == "ft.flight_recorder.incident_corpus.proof_log.v1"
        and .contract_id == "ft.flight_recorder.incident_corpus.proof_log.v1"
        and .bead_id == "ft-ogr3n.6"
        and .manifest.path == $manifest
        and .manifest.sha256 == $manifest_hash
        and .source_set.path == $source_path
        and .source_set.sha256 == $source_sha
        and any(.goldens[]; .kind == "json" and .path == $golden_json and .sha256 == $golden_json_sha)
        and any(.goldens[]; .kind == "toon" and .path == $golden_toon and .sha256 == $golden_toon_sha)
        and any(.steps[]; .source == "replay" and .outcome == $outcome)
        and .expected.outcome == $outcome
        and .expected.proof_admissible == $admissible
        and (.expected.frame_count | type == "number" and . > 0)
        and .proof.local_cargo_counted == false
        and .side_effects.live_panes_mutated == false
        and .side_effects.external_services_called == false
        and .side_effects.agent_mail_repair_attempted == false
        and .side_effects.agent_mail_service_restarted == false
        and .side_effects.rch_worker_mutated == false
        and .side_effects.file_deleted == false
        and (if $material_remote then
          .proof.remote_cargo_reached == true
          and (.proof.worker_id | type == "string" and startswith("vmi"))
          and (.proof.build_id | type == "string" and startswith("j-"))
          and .proof.exit_code == 0
        else
          .proof.local_cargo_counted == false
        end)
    ' "${PROOF_LEDGER}" >/dev/null || fail "${scenario_id} proof ledger mismatch"
done

jq -e \
  --arg manifest "${MANIFEST}" \
  --arg manifest_hash "${manifest_hash}" \
  --arg invalid "${INVALID_FIXTURES}" \
  --arg invalid_hash "${invalid_hash}" \
  --arg ledger "${PROOF_LEDGER}" \
  --arg ledger_hash "${ledger_hash}" \
  --argjson scenarios "$(printf '%s\n' "${REQUIRED_SCENARIOS[@]}" | jq -R . | jq -s .)" \
  --argjson negative "$(printf '%s\n' "${REQUIRED_NEGATIVE_CASES[@]}" | jq -R . | jq -s .)" '
    .schema_version == "ft.flight_recorder.incident_corpus.proof_summary.v1"
    and .contract_id == "ft.flight_recorder.incident_corpus.proof_summary.v1"
    and .bead_id == "ft-ogr3n.6"
    and .manifest.path == $manifest
    and .manifest.sha256 == $manifest_hash
    and .invalid_fragments.path == $invalid
    and .invalid_fragments.sha256 == $invalid_hash
    and .invalid_fragments.entries == ($negative | length)
    and .proof_ledger.path == $ledger
    and .proof_ledger.sha256 == $ledger_hash
    and .proof_ledger.entries == ($scenarios | length)
    and (.scenario_ids | sort) == ($scenarios | sort)
    and (.negative_coverage | sort) == ($negative | sort)
    and (.proof_state.remote_metadata_scenarios | sort) == (["bead_closeout_mirror", "dirty_tree_contaminated", "remote_cargo_pass"] | sort)
    and (.proof_state.pre_cargo_block_scenarios | index("rch_mirror_before_cargo") != null)
    and (.proof_state.agent_mail_fallback_scenarios | index("agent_mail_outage_beads_fallback") != null)
    and .proof_state.local_cargo_counted == false
    and .proof_state.live_services_called == false
    and .side_effects.live_panes_mutated == false
    and .side_effects.external_services_called == false
    and .side_effects.agent_mail_repair_attempted == false
    and .side_effects.agent_mail_service_restarted == false
    and .side_effects.rch_worker_mutated == false
    and .side_effects.file_deleted == false
    and (.operator_summary | contains("does not count local Cargo"))
  ' "${PROOF_SUMMARY}" >/dev/null || fail "proof summary mismatch"

if rg -n --hidden --glob '!*.md' \
  '(sk-[A-Za-z0-9]{20,}|AKIA[0-9A-Z]{16}|ghp_[A-Za-z0-9]{20,}|xox[baprs]-[A-Za-z0-9-]{20,}|Bearer [A-Za-z0-9._-]{20,}|BEGIN (RSA|OPENSSH|EC) PRIVATE KEY)' \
  fixtures/flight-recorder/incident-corpus >/tmp/ft-flight-recorder-incident-corpus-secret-scan.txt; then
  cat /tmp/ft-flight-recorder-incident-corpus-secret-scan.txt >&2
  fail "secret-shaped strings found in incident corpus fixtures"
fi

ledger_entry_count="$(jq -s 'length' "${PROOF_LEDGER}")"
invalid_case_count="$(jq '.cases | length' "${INVALID_FIXTURES}")"
printf 'flight-recorder incident corpus: static verifier passed (%d scenarios, %d proof-ledger entries, %d invalid cases)\n' \
  "${#REQUIRED_SCENARIOS[@]}" "${ledger_entry_count}" "${invalid_case_count}"
