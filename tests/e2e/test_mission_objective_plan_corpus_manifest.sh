#!/usr/bin/env bash
# Static verifier for the mission objective-plan golden corpus manifest.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

PLAN_SCHEMA="docs/json-schema/ft-mission-objective-plan.json"
SOURCES_SCHEMA="docs/json-schema/ft-mission-objective-sources.json"
MANIFEST="fixtures/mission-planner/objective-plan-corpus/manifest.json"
INPUT_DIR="fixtures/mission-planner/objective-plan-corpus/inputs"
PLAN_ARTIFACT_DIR="fixtures/mission-planner/objective-plan"
REQUIRED_CASES=(
  "agent-mail-unavailable"
  "blocked-proof-lane"
  "clean-ready-queue"
  "dirty-overlapping-paths"
  "no-ready-fallback"
  "rch-degraded-no-worker"
  "stale-in-progress-candidate"
)
REQUIRED_SCRUB_FIELDS=(
  "elapsed_ms"
  "generated_at_ms"
  "machine_id"
  "worker_id"
  "workspace_root"
)
REQUIRED_FORBIDDEN_ACTIONS=(
  "destructive_filesystem"
  "pane_mutation"
  "service_mutation"
)
REQUIRED_INVALID_PLAN_ARTIFACTS=(
  "fixtures/mission-planner/objective-plan/invalid-raw-pane-content.json"
)

fail() {
  printf 'mission objective-plan corpus manifest: %s\n' "$*" >&2
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
  [[ "${path}" != "." ]] || fail "bare dot path is forbidden: ${path}"
  [[ "${path}" != ".." ]] || fail "parent-relative path is forbidden: ${path}"
  [[ "${path}" != ./* ]] || fail "dot-prefixed path is forbidden: ${path}"
  [[ "${path}" != ../* ]] || fail "parent-relative path is forbidden: ${path}"
  [[ "${path}" != */../* ]] || fail "parent-relative path is forbidden: ${path}"
  [[ "${path}" != */./* ]] || fail "embedded dot segment is forbidden: ${path}"
  [[ "${path}" != */.. ]] || fail "trailing parent-relative segment is forbidden: ${path}"
  [[ "${path}" != */. ]] || fail "trailing dot segment is forbidden: ${path}"
  [[ "${path}" != ".git" ]] || fail ".git path is forbidden: ${path}"
  [[ "${path}" != .git/* ]] || fail ".git path is forbidden: ${path}"
  [[ "${path}" != */.git/* ]] || fail ".git path is forbidden: ${path}"
}

sha256_file() {
  shasum -a 256 "$1" | awk '{print $1}'
}

require_command jq
require_command rg
require_command shasum

require_file "${PLAN_SCHEMA}"
require_file "${SOURCES_SCHEMA}"
require_file "${MANIFEST}"

mapfile -t manifest_input_paths < <(jq -r '.cases[].input_path' "${MANIFEST}" | sort)
mapfile -t actual_input_paths < <(find "${INPUT_DIR}" -type f -name '*.json' | sort)
mapfile -t retained_artifact_paths < <(jq -r '.cases[].retained_artifacts[].artifact_path' "${MANIFEST}" | sort -u)
mapfile -t retained_negative_artifact_paths < <(jq -r '.retained_negative_artifacts[]?.artifact_path' "${MANIFEST}" | sort -u)
mapfile -t plan_artifact_paths < <(find "${PLAN_ARTIFACT_DIR}" -type f -name '*.json' | sort)

all_json=(
  "${PLAN_SCHEMA}"
  "${SOURCES_SCHEMA}"
  "${MANIFEST}"
  "${manifest_input_paths[@]}"
  "${retained_artifact_paths[@]}"
  "${retained_negative_artifact_paths[@]}"
  "${plan_artifact_paths[@]}"
)

for path in "${all_json[@]}"; do
  require_repo_relative_path "${path}"
  require_file "${path}"
done

jq empty "${all_json[@]}"

diff -u <(printf '%s\n' "${manifest_input_paths[@]}") <(printf '%s\n' "${actual_input_paths[@]}") \
  >/dev/null || fail "manifest case input paths do not match ${INPUT_DIR}/*.json"

jq -e --argjson required "$(printf '%s\n' "${REQUIRED_CASES[@]}" | jq -R . | jq -s .)" \
  --arg plan_schema "${PLAN_SCHEMA}" '
  def repo_relative_path_ok:
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
  def planner_input_path_ok:
    repo_relative_path_ok
    and startswith("fixtures/mission-planner/objective-plan-corpus/inputs/")
    and endswith(".json");
  def objective_plan_artifact_path_ok:
    repo_relative_path_ok
    and startswith("fixtures/mission-planner/objective-plan/")
    and endswith(".json");

  .schema_version == 1
  and .contract_id == "ft.mission_objective_plan.golden_corpus.v1"
  and ([.cases[].case_id] | sort) == ($required | sort)
  and (.scrub_rules | type == "array" and length >= 5)
  and all(.scrub_rules[];
    (.field | type == "string" and length > 0)
    and (.replacement | type == "string" and length > 0)
    and (.reason | type == "string" and length > 0)
  )
  and (.retained_negative_artifacts | type == "array" and length >= 1)
  and all(.retained_negative_artifacts[];
    (.artifact_path | objective_plan_artifact_path_ok)
    and (.artifact_kind == "objective_plan_json_expected_invalid")
    and (.schema_path == $plan_schema)
    and (.validation_expectation == "schema_rejects_raw_pane_content")
    and (.expected_failure == "raw_pane_content_stored_const_false")
    and (.fixture_sha256 | test("^[0-9a-f]{64}$"))
  )
  and (. as $manifest | all(.retained_negative_artifacts[];
    .artifact_path as $negative_path |
    all($manifest.cases[].retained_artifacts[]; .artifact_path != $negative_path)
  ))
  and all(.cases[];
    (.case_id | type == "string" and length > 0)
    and (.input_path | planner_input_path_ok)
    and (.retained_artifacts | type == "array" and length >= 1)
    and all(.retained_artifacts[];
      (.artifact_kind | IN("objective_plan_json", "planner_input_json"))
      and (
        if .artifact_kind == "planner_input_json" then
          (.artifact_path | planner_input_path_ok)
        else
          (.artifact_path | objective_plan_artifact_path_ok)
        end
      )
      and (.source_command | type == "string" and length > 0)
      and (.exit_code | type == "number" and . >= 0 and . <= 255)
      and (.fixture_sha256 | test("^[0-9a-f]{64}$"))
    )
    and (.expected.plan_status | IN(
      "actionable",
      "degraded",
      "dirty_overlap",
      "no_ready_work",
      "rch_substrate_blocked",
      "waiting_owner"
    ))
    and (.expected.risk_level | IN("low", "medium", "high", "blocked"))
    and (.expected.top_step_candidate_id | type == "string" and length > 0)
    and (.expected.top_step_action_kind | IN("choose_ready_bead", "create_bead", "inspect_artifact", "wait_for_owner"))
    and (.expected.top_step_status | IN("actionable", "dirty_overlap", "no_ready_work", "rch_substrate_blocked", "waiting_owner"))
    and (.expected.top_step_proof_lane | IN("blocked", "not_required"))
    and (.expected.reason_codes_include | type == "array" and length >= 1)
    and all(.expected.reason_codes_include[]; type == "string" and length > 0)
  )
' "${MANIFEST}" >/dev/null || fail "manifest metadata or case entries are incomplete"

for field in "${REQUIRED_SCRUB_FIELDS[@]}"; do
  jq -e --arg field "${field}" '
    any(.scrub_rules[];
      .field == $field
      and (.replacement | type == "string" and length > 0)
      and (.reason | type == "string" and length > 0)
    )
  ' "${MANIFEST}" >/dev/null || fail "manifest missing scrub rule for ${field}"
done

for artifact_path in "${REQUIRED_INVALID_PLAN_ARTIFACTS[@]}"; do
  jq -e --arg artifact_path "${artifact_path}" '
    any(.retained_negative_artifacts[];
      .artifact_path == $artifact_path
      and .artifact_kind == "objective_plan_json_expected_invalid"
      and .validation_expectation == "schema_rejects_raw_pane_content"
    )
  ' "${MANIFEST}" >/dev/null || fail "manifest missing retained negative artifact ${artifact_path}"
done

while IFS=$'\t' read -r artifact_path schema_path expected_failure fixture_sha256; do
  require_repo_relative_path "${artifact_path}"
  require_repo_relative_path "${schema_path}"
  require_file "${artifact_path}"
  require_file "${schema_path}"

  actual_sha256="$(sha256_file "${artifact_path}")"
  [[ "${actual_sha256}" == "${fixture_sha256}" ]] \
    || fail "retained negative artifact hash drift for ${artifact_path}: expected ${fixture_sha256}, got ${actual_sha256}"

  [[ "${expected_failure}" == "raw_pane_content_stored_const_false" ]] \
    || fail "${artifact_path} uses unsupported retained negative expectation ${expected_failure}"

  jq -e '
    .properties.raw_pane_content_stored.const == false
    and .["$defs"].source_snapshot.properties.redacted.const == true
    and .["$defs"].source_snapshot.properties.raw_pane_content_stored.const == false
    and .["$defs"].redaction_policy.properties.raw_pane_content_allowed.const == false
  ' "${schema_path}" >/dev/null || fail "${schema_path} no longer fail-closes raw pane content redaction fields"

  jq -e '
    .contract_id == "ft.mission_objective_plan.v1"
    and .raw_pane_content_stored == true
    and .redaction_policy.raw_pane_content_allowed == false
    and any(.source_snapshots[];
      .redacted == false
      and .raw_pane_content_stored == true
      and .redaction_posture.pane_content == "raw_forbidden"
    )
    and any(.proof_requirements[];
      .proof_kind == "static_schema"
      and (.artifact_paths | index("docs/json-schema/ft-mission-objective-plan.json"))
      and (.artifact_paths | index("fixtures/mission-planner/objective-plan/invalid-raw-pane-content.json"))
    )
  ' "${artifact_path}" >/dev/null || fail "${artifact_path} no longer exercises the raw pane content schema rejection"
done < <(jq -r '
  .retained_negative_artifacts[] |
  [.artifact_path, .schema_path, .expected_failure, .fixture_sha256] | @tsv
' "${MANIFEST}")

for case_id in "${REQUIRED_CASES[@]}"; do
  input_path="$(jq -r --arg case_id "${case_id}" '.cases[] | select(.case_id == $case_id) | .input_path' "${MANIFEST}")"
  expected_status="$(jq -r --arg case_id "${case_id}" '.cases[] | select(.case_id == $case_id) | .expected.plan_status' "${MANIFEST}")"
  expected_proof_lane="$(jq -r --arg case_id "${case_id}" '.cases[] | select(.case_id == $case_id) | .expected.top_step_proof_lane' "${MANIFEST}")"

  require_file "${input_path}"

  jq -e --arg case_id "${case_id}" --arg expected_status "${expected_status}" '
    .source == "mission_objective_plan.golden_corpus"
    and (.generated_at_ms | type == "number" and . >= 0)
    and (.objective | type == "string" and length > 0)
    and (.strictness | type == "string" and length > 0)
    and (.dirty_paths | type == "array")
    and (.source_snapshots | type == "array" and length >= 1)
    and all(.source_snapshots[];
      (.source_id | type == "string" and length > 0)
      and (.kind | IN("agent_mail", "beads", "git", "rch"))
      and (.state | IN("available", "blocked", "degraded", "stale", "unavailable"))
      and (.freshness_state | IN("fresh", "stale", "unknown", "not_collected"))
      and (.redaction_posture | type == "string" and contains("redacted"))
      and (.reason_codes | type == "array" and length >= 1)
      and all(.reason_codes[]; type == "string" and length > 0)
      and (.evidence | type == "array" and length >= 1)
      and all(.evidence[];
        (.category | type == "string" and length > 0)
        and (.summary | type == "string" and length > 0)
        and (.reason_codes | type == "array" and length >= 1)
        and all(.reason_codes[]; type == "string" and length > 0)
      )
    )
    and (.candidates | type == "array")
    and (
      if $expected_status == "no_ready_work" then
        (.candidates | length) == 0
      else
        (.candidates | length) >= 1
        and all(.candidates[];
          (.candidate_id | type == "string" and length > 0)
          and (.title | type == "string" and length > 0)
          and (.readiness | IN("ready_bead", "stale_reopen_candidate"))
          and (.priority | type == "number")
          and (.target_bead_id | type == "string" and length > 0)
          and (.owned_paths | type == "array" and length >= 1)
          and all(.owned_paths[]; type == "string" and length > 0)
          and (.stale_after_seconds | type == "number" and . >= 0)
          and (.dependency_ready | type == "boolean")
          and (.proof_availability | IN("available", "blocked", "not_required", "unavailable"))
          and (.capacity_posture | type == "string" and length > 0)
          and (.reason_codes | type == "array" and length >= 1)
          and all(.reason_codes[]; type == "string" and length > 0)
        )
      end
    )
  ' "${input_path}" >/dev/null || fail "${case_id} planner input is incomplete"

  if [[ "${expected_proof_lane}" == "blocked" ]]; then
    jq -e '
      any(.source_snapshots[].reason_codes[]?; startswith("rch.") or . == "local_cargo_forbidden")
      or any(.candidates[].reason_codes[]?; startswith("rch.") or . == "local_cargo_forbidden")
    ' "${input_path}" >/dev/null || fail "${case_id} blocked proof case lacks RCH/local-Cargo reason codes"
  fi

  while IFS=$'\t' read -r artifact_kind artifact_path exit_code fixture_sha256; do
    require_repo_relative_path "${artifact_path}"
    require_file "${artifact_path}"

    actual_sha256="$(sha256_file "${artifact_path}")"
    [[ "${actual_sha256}" == "${fixture_sha256}" ]] \
      || fail "${case_id} hash drift for ${artifact_path}: expected ${fixture_sha256}, got ${actual_sha256}"

    if ((exit_code != 0)); then
      [[ "${expected_status}" == "rch_substrate_blocked" && "${expected_proof_lane}" == "blocked" ]] \
        || fail "${case_id} non-zero retained exit code is only allowed for blocked RCH proof cases"
    fi

    case "${artifact_kind}" in
      objective_plan_json)
        ((exit_code == 0)) || fail "${case_id} objective_plan_json retained artifact must have exit_code 0"
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
          def retained_artifact_path_ok:
            repo_relative_artifact_path_ok
            and (
              . == "docs/json-schema/ft-mission-objective-plan.json"
              or (startswith("fixtures/mission-planner/objective-plan/") and endswith(".json"))
            );

          .schema_version == 1
          and .contract_id == "ft.mission_objective_plan.v1"
          and (.generated_at_ms | type == "number" and . >= 0)
          and (.plan_status | IN(
            "actionable",
            "blocked",
            "degraded",
            "dirty_overlap",
            "no_ready_work",
            "rch_substrate_blocked",
            "unavailable",
            "waiting_external",
            "waiting_owner"
          ))
          and .raw_pane_content_stored == false
          and .redaction_policy.raw_pane_content_allowed == false
          and .redaction_policy.secret_material_allowed == false
          and (.source_snapshots | type == "array" and length >= 1)
          and all(.source_snapshots[];
            .redacted == true
            and .raw_pane_content_stored == false
            and (.reason_codes | type == "array" and length >= 1)
            and (.evidence | type == "array" and length >= 1)
          )
          and (.forbidden_actions | type == "array" and length >= 1)
          and (.artifact_paths | type == "array" and length >= 1)
          and all(.artifact_paths[]; retained_artifact_path_ok)
        ' "${artifact_path}" >/dev/null || fail "${artifact_path} retained objective-plan JSON is unsafe or incomplete"

        for action_class in "${REQUIRED_FORBIDDEN_ACTIONS[@]}"; do
          jq -e --arg action_class "${action_class}" '
            any(.forbidden_actions[]; .action_class == $action_class)
          ' "${artifact_path}" >/dev/null || fail "${artifact_path} missing forbidden action ${action_class}"
        done
        ;;
      planner_input_json)
        # Validate retained planner input artifacts with the same input contract used by case inputs.
        jq -e '
          .source == "mission_objective_plan.golden_corpus"
          and (.source_snapshots | type == "array" and length >= 1)
          and all(.source_snapshots[];
            (.redaction_posture | type == "string" and contains("redacted"))
            and (.reason_codes | type == "array" and length >= 1)
            and (.evidence | type == "array" and length >= 1)
          )
          and (.dirty_paths | type == "array")
          and (.candidates | type == "array")
        ' "${artifact_path}" >/dev/null || fail "${artifact_path} retained planner input is incomplete"
        ;;
      *)
        fail "${case_id} unknown retained artifact kind: ${artifact_kind}"
        ;;
    esac
  done < <(jq -r --arg case_id "${case_id}" '
    .cases[] | select(.case_id == $case_id) | .retained_artifacts[] |
    [.artifact_kind, .artifact_path, (.exit_code | tostring), .fixture_sha256] | @tsv
  ' "${MANIFEST}")
done

if rg -n --hidden --glob '!*.md' \
  '(sk-[A-Za-z0-9]{20,}|AKIA[0-9A-Z]{16}|ghp_[A-Za-z0-9]{20,}|xox[baprs]-[A-Za-z0-9-]{20,}|Bearer [A-Za-z0-9._-]{20,}|BEGIN (RSA|OPENSSH|EC) PRIVATE KEY)' \
  fixtures/mission-planner >/tmp/ft-mission-objective-plan-secret-scan.txt; then
  cat /tmp/ft-mission-objective-plan-secret-scan.txt >&2
  fail "secret-shaped strings found in mission-planner fixtures"
fi

printf 'mission objective-plan corpus manifest: static verifier passed (%d cases, %d retained artifacts, %d retained negative artifacts)\n' \
  "${#REQUIRED_CASES[@]}" "${#retained_artifact_paths[@]}" "${#retained_negative_artifact_paths[@]}"
