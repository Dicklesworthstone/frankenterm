#!/usr/bin/env bash
# Static verifier for the operating-envelope fixture manifest.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

OUTPUT_FORMAT="text"
if [[ "${1:-}" == "--json" ]]; then
  OUTPUT_FORMAT="json"
  shift
fi
(($# == 0)) || {
  printf 'operating-envelope fixture manifest: unexpected arguments: %s\n' "$*" >&2
  exit 1
}

SCHEMA="docs/json-schema/ft-operating-envelope.json"
MANIFEST="fixtures/operating-envelope/manifest.json"
CONTRACT_DOC="docs/robot-contracts/operating-envelope.md"
PROOF_CALENDAR_SCHEMA="docs/json-schema/ft-operating-envelope-proof-calendar.json"
PROOF_CALENDAR_FIXTURES="fixtures/operating-envelope/proof-calendar/cases.v1.json"
PROOF_CALENDAR_INVALID_FIXTURES="fixtures/operating-envelope/proof-calendar/invalid/cases.v1.json"
PROVENANCE="docs/json-schema/PROVENANCE.md"
REQUIRED_FORBIDDEN_ACTION_CLASSES=(
  "agent_mail_repair"
  "build_cancellation"
  "destructive_filesystem"
  "destructive_git"
  "local_cargo_proof"
  "raw_pane_content"
  "raw_pane_content_capture"
  "rch_daemon_restart"
  "service_mutation"
)
PROOF_CALENDAR_REQUIRED_CASES=(
  "rch-unavailable"
  "no-admissible-workers"
  "static-only-ready"
  "local-closed-not-published"
  "dirty-overlap"
  "stale-proof-artifact"
  "target-hardware-unavailable"
)
PROOF_CALENDAR_REQUIRED_WORK_CLASSES=(
  "static_docs_fixture_verifier"
  "shell_jq_contract"
  "rch_required_unit_integration"
  "target_class_hardware_proof"
  "operator_only_recovery"
  "forbidden_mutation"
)
PROOF_CALENDAR_REQUIRED_FORBIDDEN=(
  "agent_mail_service_repair"
  "build_cancellation"
  "delete_files"
  "destructive_filesystem"
  "destructive_git"
  "local_cargo_proof"
  "local_heavy_cargo_fallback"
  "rch_daemon_restart"
  "rch_service_repair"
  "service_mutation"
  "worker_mutation"
)
PROOF_CALENDAR_REQUIRED_SOURCE_KINDS=(
  "beads"
  "rch"
  "agent_mail"
  "git"
  "proof_artifact"
)
PROOF_CALENDAR_REQUIRED_INVALID_CASES=(
  "local-cargo-fallback-allowed"
  "raw-pane-content-allowed"
  "absolute-artifact-path"
  "missing-required-forbidden-action"
  "toon-row-width-mismatch"
  "service-mutation-permitted"
)

fail() {
  if [[ "${OUTPUT_FORMAT}" == "json" ]] && command -v jq >/dev/null 2>&1; then
    jq -n --arg error "$*" '{ok: false, error: $error}' >&2
  else
    printf 'operating-envelope fixture manifest: %s\n' "$*" >&2
  fi
  exit 1
}

require_file() {
  local path="$1"
  [[ -f "${path}" ]] || fail "missing file: ${path}"
}

require_tracked_file() {
  local path="$1"
  require_repo_relative_path "${path}"
  require_file "${path}"
  git ls-files --error-unmatch -- "${path}" >/dev/null 2>&1 \
    || fail "referenced artifact is not tracked by git: ${path}"
}

require_repo_relative_path() {
  local path="$1"

  [[ -n "${path}" ]] || fail "empty fixture path"
  [[ "${path}" != /* ]] || fail "absolute path is forbidden: ${path}"
  [[ "${path}" != *'..'* ]] || fail "parent-relative path is forbidden: ${path}"
}

require_file "${SCHEMA}"
require_file "${MANIFEST}"
require_file "${CONTRACT_DOC}"
require_file "${PROOF_CALENDAR_SCHEMA}"
require_file "${PROOF_CALENDAR_FIXTURES}"
require_file "${PROOF_CALENDAR_INVALID_FIXTURES}"
require_file "${PROVENANCE}"

mapfile -t valid_paths < <(jq -r '.valid_fixtures[].path' "${MANIFEST}")
mapfile -t invalid_paths < <(jq -r '.invalid_fixtures[].path' "${MANIFEST}")
mapfile -t root_alias_paths < <(find fixtures/operating-envelope -maxdepth 1 -type f -name '*.json' ! -name 'manifest.json' | sort)

((${#valid_paths[@]} >= 6)) || fail "manifest must retain at least 6 valid fixtures"
((${#invalid_paths[@]} >= 4)) || fail "manifest must retain at least 4 invalid fixtures"
((${#root_alias_paths[@]} >= 6)) || fail "root alias fixtures must be retained and validated"

current_fixture_paths=("${valid_paths[@]}" "${root_alias_paths[@]}")
all_json=(
  "${SCHEMA}"
  "${MANIFEST}"
  "${PROOF_CALENDAR_SCHEMA}"
  "${PROOF_CALENDAR_FIXTURES}"
  "${PROOF_CALENDAR_INVALID_FIXTURES}"
  "${valid_paths[@]}"
  "${invalid_paths[@]}"
  "${root_alias_paths[@]}"
)
for path in "${all_json[@]}"; do
  require_repo_relative_path "${path}"
  require_file "${path}"
done

jq empty "${all_json[@]}"

mapfile -t actual_valid_paths < <(find fixtures/operating-envelope/valid -type f -name '*.json' | sort)
mapfile -t actual_invalid_paths < <(find fixtures/operating-envelope/invalid -type f -name '*.json' | sort)

diff -u <(printf '%s\n' "${valid_paths[@]}" | sort) <(printf '%s\n' "${actual_valid_paths[@]}") \
  >/dev/null || fail "manifest valid_fixtures does not match fixtures/operating-envelope/valid/*.json"

diff -u <(printf '%s\n' "${invalid_paths[@]}" | sort) <(printf '%s\n' "${actual_invalid_paths[@]}") \
  >/dev/null || fail "manifest invalid_fixtures does not match fixtures/operating-envelope/invalid/*.json"

jq -e '
  .schema_version == 1
  and .contract_id == "ft.operating_envelope.fixture_manifest.v1"
  and .schema_path == "docs/json-schema/ft-operating-envelope.json"
  and .contract_doc == "docs/robot-contracts/operating-envelope.md"
  and (.valid_fixtures | length >= 6)
  and (.invalid_fixtures | length >= 4)
  and (.static_checks | type == "array")
  and (.static_checks | length >= 3)
  and (.static_checks | index("bash tests/e2e/test_operating_envelope_fixture_manifest.sh") != null)
  and all(.static_checks[]; type == "string" and length > 0)
  and all(.valid_fixtures[]; (.coverage | type == "array") and (.coverage | length > 0) and all(.coverage[]; type == "string" and length > 0))
  and all(.invalid_fixtures[]; (.expected_failure | type == "string") and (.expected_failure | length > 0))
' "${MANIFEST}" >/dev/null || fail "manifest metadata or coverage is incomplete"

jq -e '
  all(.;
    .schema_version == 1
    and .contract_id == "ft.operating_envelope.v1"
    and .raw_pane_content_stored == false
    and (.controller_mode | type == "string" and length > 0)
    and (.target_class | type == "object")
    and (.input_domains | type == "object")
    and (.admission_windows | type == "array" and length > 0)
    and (.fail_closed_policy | type == "object")
    and (.fail_closed_policy.reason_codes | type == "array" and length > 0)
    and (.reason_codes | type == "array" and length > 0)
    and (.artifact_paths | type == "array" and length > 0)
    and .side_effect_policy.dry_run_only == true
    and .side_effect_policy.raw_pane_content_allowed == false
    and .side_effect_policy.pane_mutation_allowed == false
    and .side_effect_policy.service_mutation_allowed == false
    and .side_effect_policy.destructive_actions_allowed == false
    and .side_effect_policy.local_cargo_proof_allowed == false
    and .redaction_policy.raw_pane_content_allowed == false
    and .redaction_policy.secret_material_allowed == false
    and all(.input_domains[]?; .raw_pane_content_stored == false)
  )
' "${current_fixture_paths[@]}" >/dev/null || fail "valid/root fixture safety policy is not fail-closed"

for action_class in "${REQUIRED_FORBIDDEN_ACTION_CLASSES[@]}"; do
  jq -e --arg action_class "${action_class}" '
    all(.;
      all(.admission_windows[]; (.forbidden_action_classes | index($action_class) != null))
    )
  ' "${current_fixture_paths[@]}" >/dev/null || fail "valid/root fixture admission_windows must forbid ${action_class}"
done

jq -e '
  def repo_relative_artifact_path_ok:
    type == "string"
    and length > 0
    and . != "."
    and . != ".."
    and (startswith("/") | not)
    and (startswith("./") | not)
    and (startswith("../") | not)
    and (contains("/../") | not)
    and (contains("/./") | not)
    and (endswith("/..") | not)
    and (endswith("/.") | not)
    and . != ".git"
    and (startswith(".git/") | not)
    and (contains("/.git/") | not);

  all(.;
    all(.artifact_paths[]?; repo_relative_artifact_path_ok)
  )
' "${current_fixture_paths[@]}" >/dev/null || fail "valid/root fixture artifact_paths must stay repo-relative"

while IFS= read -r artifact_path; do
  require_tracked_file "${artifact_path}"
done < <(jq -r '.artifact_paths[]?' "${current_fixture_paths[@]}" | sort -u)

jq -e '
  all(.invalid_fixtures[];
    (.path | type == "string" and length > 0)
    and (.expected_failure | type == "string" and length > 0)
  )
' "${MANIFEST}" >/dev/null || fail "invalid fixture entries need expected_failure text"

jq -e '
  .schema_version == "ft.operating_envelope.proof_calendar.fixtures.v1"
  and .contract_id == "ft.operating_envelope.proof_calendar.fixture_manifest.v1"
  and .schema_path == "docs/json-schema/ft-operating-envelope-proof-calendar.json"
  and .contract_doc == "docs/robot-contracts/operating-envelope.md"
  and .source_bead == "ft-booek.8"
  and (.verification | index("bash tests/e2e/test_operating_envelope_fixture_manifest.sh") != null)
  and (.toon_columns | length >= 6)
  and (.cases | length >= 7)
' "${PROOF_CALENDAR_FIXTURES}" >/dev/null || fail "proof-calendar fixture metadata is incomplete"

jq -e '
  .schema_version == "ft.operating_envelope.proof_calendar.invalid_fixtures.v1"
  and .contract_id == "ft.operating_envelope.proof_calendar.invalid_fixture_manifest.v1"
  and .schema_path == "docs/json-schema/ft-operating-envelope-proof-calendar.json"
  and .valid_fixture == "fixtures/operating-envelope/proof-calendar/cases.v1.json"
  and .contract_doc == "docs/robot-contracts/operating-envelope.md"
  and .source_bead == "ft-booek.9"
  and (.verification | index("bash tests/e2e/test_operating_envelope_fixture_manifest.sh") != null)
  and (.cases | length >= 6)
  and all(.cases[];
    (.case_id | type == "string" and length > 0)
    and (.expected_failure | type == "string" and length > 0)
    and (.reason_codes | type == "array" and length > 0)
    and all(.reason_codes[]; type == "string" and contains("."))
    and (.invalid_fragment | type == "object")
  )
' "${PROOF_CALENDAR_INVALID_FIXTURES}" >/dev/null || fail "proof-calendar invalid fixture metadata is incomplete"

jq -e '
  .["$id"] == "https://frankenterm.dev/schemas/ft-operating-envelope-proof-calendar.json"
  and .properties.contract_id.const == "ft.operating_envelope.proof_calendar.v1"
  and .properties.source_bead.const == "ft-booek.8"
  and .properties.dry_run.const == true
  and .properties.read_only.const == true
' "${PROOF_CALENDAR_SCHEMA}" >/dev/null || fail "proof-calendar schema root contract drifted"

jq -e '
  def unique_values:
    length == (unique | length);

  ([.cases[].case_id] | unique_values)
  and all(.cases[];
    ([.artifact.calendar_entries[].entry_id] | unique_values)
    and ([.artifact.source_snapshots[].source_kind] | unique_values)
    and ([.artifact.work_classes[].work_class] | unique_values)
  )
' "${PROOF_CALENDAR_FIXTURES}" >/dev/null || fail "proof-calendar case ids and per-case artifact ids must be unique"

for case_id in "${PROOF_CALENDAR_REQUIRED_CASES[@]}"; do
  jq -e --arg case_id "${case_id}" '
    any(.cases[]; .case_id == $case_id)
  ' "${PROOF_CALENDAR_FIXTURES}" >/dev/null || fail "proof-calendar missing case ${case_id}"
  [[ "$(cat "${CONTRACT_DOC}")" == *"${case_id}"* ]] || fail "contract doc missing proof-calendar case ${case_id}"
done

for case_id in "${PROOF_CALENDAR_REQUIRED_INVALID_CASES[@]}"; do
  jq -e --arg case_id "${case_id}" '
    any(.cases[]; .case_id == $case_id)
  ' "${PROOF_CALENDAR_INVALID_FIXTURES}" >/dev/null || fail "proof-calendar missing invalid case ${case_id}"
  [[ "$(cat "${CONTRACT_DOC}")" == *"${case_id}"* ]] || fail "contract doc missing proof-calendar invalid case ${case_id}"
done

jq -e '
  def case($id): .cases[] | select(.case_id == $id);

  ([.cases[].case_id] | length == (unique | length))
  and (case("local-cargo-fallback-allowed")
    | .expected_failure == "local_cargo_fallback_allowed_must_be_false"
    and (.reason_codes | index("proof.local_cargo_fallback_forbidden") != null)
    and .invalid_fragment.proof_policy.local_cargo_fallback_allowed == true)
  and (case("raw-pane-content-allowed")
    | .expected_failure == "raw_pane_content_must_not_be_stored_or_allowed"
    and (.reason_codes | index("redaction.raw_pane_content_forbidden") != null)
    and .invalid_fragment.redaction_policy.raw_pane_content_allowed == true
    and any(.invalid_fragment.source_snapshots[]; .raw_pane_content_stored == true))
  and (case("absolute-artifact-path")
    | .expected_failure == "artifact_paths_must_be_repo_relative"
    and (.reason_codes | index("artifact.absolute_path_forbidden") != null)
    and any(.invalid_fragment.artifact_paths[]; startswith("/")))
  and (case("missing-required-forbidden-action")
    | .expected_failure == "all_required_forbidden_actions_must_be_retained"
    and (.reason_codes | index("policy.required_forbidden_action_missing") != null)
    and .invalid_fragment.missing_forbidden_action == "worker_mutation"
    and (.invalid_fragment.forbidden_actions | index("worker_mutation") == null))
  and (case("toon-row-width-mismatch")
    | .expected_failure == "toon_rows_must_match_declared_columns"
    and (.reason_codes | index("toon.row_width_mismatch") != null)
    and (.invalid_fragment.toon_projection.columns | length) as $width
    | any(.invalid_fragment.toon_projection.rows[]; length != $width))
  and (case("service-mutation-permitted")
    | .expected_failure == "service_mutation_allowed_must_be_false"
    and (.reason_codes | index("policy.service_mutation_forbidden") != null)
    and .invalid_fragment.side_effect_policy.service_mutation_allowed == true)
' "${PROOF_CALENDAR_INVALID_FIXTURES}" >/dev/null || fail "proof-calendar invalid fixtures do not cover required fail-closed cases"

for work_class in "${PROOF_CALENDAR_REQUIRED_WORK_CLASSES[@]}"; do
  jq -e --arg work_class "${work_class}" '
    (.required_work_classes | index($work_class) != null)
    and all(.cases[]; ([.artifact.work_classes[].work_class] | index($work_class) != null))
  ' "${PROOF_CALENDAR_FIXTURES}" >/dev/null || fail "proof-calendar missing work class ${work_class}"
done

for action_class in "${PROOF_CALENDAR_REQUIRED_FORBIDDEN[@]}"; do
  jq -e --arg action_class "${action_class}" '
    (.required_forbidden_actions | index($action_class) != null)
    and all(.cases[];
      (.artifact.forbidden_actions | index($action_class) != null)
      and all(.artifact.calendar_entries[]; (.forbidden_action_classes | index($action_class) != null))
    )
  ' "${PROOF_CALENDAR_FIXTURES}" >/dev/null || fail "proof-calendar must forbid ${action_class}"
  [[ "$(cat "${CONTRACT_DOC}")" == *"${action_class}"* ]] || fail "contract doc missing proof-calendar forbidden action ${action_class}"
done

for source_kind in "${PROOF_CALENDAR_REQUIRED_SOURCE_KINDS[@]}"; do
  jq -e --arg source_kind "${source_kind}" '
    all(.cases[]; ([.artifact.source_snapshots[].source_kind] | index($source_kind) != null))
  ' "${PROOF_CALENDAR_FIXTURES}" >/dev/null || fail "proof-calendar missing source snapshot ${source_kind}"
done

jq -e '
  def repo_relative_artifact_path_ok:
    type == "string"
    and length > 0
    and . != "."
    and . != ".."
    and (startswith("/") | not)
    and (startswith("./") | not)
    and (startswith("../") | not)
    and (contains("/../") | not)
    and (contains("/./") | not)
    and (endswith("/..") | not)
    and (endswith("/.") | not)
    and . != ".git"
    and (startswith(".git/") | not)
    and (contains("/.git/") | not);

  .toon_columns as $toon_columns
  | all(.cases[];
    .case_id as $case_id
    | .required_reason_code as $required_reason
    |
    (.artifact.schema_version == 1)
    and (.artifact.contract_id == "ft.operating_envelope.proof_calendar.v1")
    and (.artifact.source_bead == "ft-booek.8")
    and (.artifact.dry_run == true)
    and (.artifact.read_only == true)
    and (.artifact.input_summary | has("beads_state") and has("rch_state") and has("agent_mail_state") and has("git_state") and has("proof_artifact_state") and has("publication_state") and has("target_hardware_state"))
    and (.artifact.calendar_entries | length >= 3)
    and ([.artifact.calendar_entries[].sort_key] == ([.artifact.calendar_entries[].sort_key] | sort))
    and (["now", "next", "wait"] - [.artifact.calendar_entries[].lane] | length == 0)
    and (.expected_now_recommendation as $expected | any(.artifact.calendar_entries[]; .lane == "now" and .recommendation == $expected))
    and (.artifact.proof_policy.rch_sync_chatter_counts_as_remote_proof == false)
    and (.artifact.proof_policy.dry_run_interception_counts_as_remote_proof == false)
    and (.artifact.proof_policy.local_shell_success_counts_as_remote_cargo_proof == false)
    and (.artifact.proof_policy.local_cargo_fallback_allowed == false)
    and (.artifact.proof_policy.remote_cargo_proof_requires_retained_artifact == true)
    and (.artifact.side_effect_policy.dry_run_only == true)
    and (.artifact.side_effect_policy.read_only == true)
    and (.artifact.side_effect_policy.service_mutation_allowed == false)
    and (.artifact.side_effect_policy.worker_mutation_allowed == false)
    and (.artifact.side_effect_policy.deletion_allowed == false)
    and (.artifact.side_effect_policy.local_heavy_cargo_allowed == false)
    and (.artifact.redaction_policy.raw_pane_content_allowed == false)
    and (.artifact.redaction_policy.mail_body_storage_allowed == false)
    and (.artifact.redaction_policy.secret_material_allowed == false)
    and (.artifact.reason_codes | index($required_reason) != null)
    and all(.artifact.artifact_paths[]; repo_relative_artifact_path_ok)
    and (.artifact.artifact_paths | index("docs/json-schema/ft-operating-envelope-proof-calendar.json") != null)
    and (.artifact.artifact_paths | index("fixtures/operating-envelope/proof-calendar/cases.v1.json") != null)
    and all(.artifact.source_snapshots[];
      .raw_pane_content_stored == false
      and (.artifact_paths | length > 0)
      and all(.artifact_paths[]; repo_relative_artifact_path_ok)
    )
    and (if .artifact.input_summary.rch_state != "available"
      then all(.artifact.calendar_entries[]; (.requires_rch == false) or (.recommendation != "run_now"))
      else true end)
    and (if .artifact.input_summary.publication_state == "local_closed_not_published"
      then any(.artifact.calendar_entries[]; .lane == "now" and .recommendation == "wait")
      else true end)
    and (if .artifact.input_summary.git_state == "blocked"
      then any(.artifact.calendar_entries[]; .lane == "now" and .recommendation == "wait")
      else true end)
    and (.artifact.toon_projection.columns == $toon_columns)
    and ((.artifact.toon_projection.columns | length) as $toon_width
      | all(.artifact.toon_projection.rows[]; length == $toon_width and .[0] == $case_id))
  )
' "${PROOF_CALENDAR_FIXTURES}" >/dev/null || fail "proof-calendar fixture artifacts are not fail-closed"

while IFS= read -r artifact_path; do
  require_tracked_file "${artifact_path}"
done < <(
  jq -r '
    .cases[].artifact.artifact_paths[]?,
    .cases[].artifact.source_snapshots[].artifact_paths[]?
  ' "${PROOF_CALENDAR_FIXTURES}" | sort -u
)

for doc_text in \
  "ft.operating_envelope.proof_calendar.v1" \
  "ft-operating-envelope-proof-calendar.json" \
  "fixtures/operating-envelope/proof-calendar/cases.v1.json" \
  "rch_sync_chatter_counts_as_remote_proof" \
  "local_shell_success_counts_as_remote_cargo_proof"; do
  [[ "$(cat "${CONTRACT_DOC}")" == *"${doc_text}"* ]] || fail "contract doc missing ${doc_text}"
done

[[ "$(cat "${PROVENANCE}")" == *"ft-operating-envelope-proof-calendar.json"* ]] \
  || fail "schema provenance missing proof-calendar row"
[[ "$(cat "${PROVENANCE}")" == *"test_operating_envelope_fixture_manifest.sh"* ]] \
  || fail "schema provenance missing operating-envelope verifier"

proof_calendar_case_count="$(jq '.cases | length' "${PROOF_CALENDAR_FIXTURES}")"
proof_calendar_invalid_case_count="$(jq '.cases | length' "${PROOF_CALENDAR_INVALID_FIXTURES}")"

if [[ "${OUTPUT_FORMAT}" == "json" ]]; then
  jq -n \
    --arg contract_id "ft.operating_envelope.fixture_manifest.v1" \
    --arg proof_calendar_contract_id "ft.operating_envelope.proof_calendar.v1" \
    --argjson valid_count "${#valid_paths[@]}" \
    --argjson invalid_count "${#invalid_paths[@]}" \
    --argjson root_alias_count "${#root_alias_paths[@]}" \
    --argjson proof_calendar_case_count "${proof_calendar_case_count}" \
    --argjson proof_calendar_invalid_case_count "${proof_calendar_invalid_case_count}" \
    '{
      ok: true,
      contract_id: $contract_id,
      proof_calendar_contract_id: $proof_calendar_contract_id,
      summary: {
        valid_fixture_count: $valid_count,
        invalid_fixture_count: $invalid_count,
        root_alias_count: $root_alias_count,
        proof_calendar_case_count: $proof_calendar_case_count,
        proof_calendar_invalid_case_count: $proof_calendar_invalid_case_count
      }
    }'
else
  printf 'operating-envelope fixture manifest: static verifier passed (%d valid, %d invalid, %d root aliases, %d proof-calendar cases, %d proof-calendar invalid cases)\n' \
    "${#valid_paths[@]}" "${#invalid_paths[@]}" "${#root_alias_paths[@]}" "${proof_calendar_case_count}" "${proof_calendar_invalid_case_count}"
fi
