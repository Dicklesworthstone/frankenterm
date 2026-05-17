#!/usr/bin/env bash
# Static verifier for the operating-envelope fixture manifest.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

SCHEMA="docs/json-schema/ft-operating-envelope.json"
MANIFEST="fixtures/operating-envelope/manifest.json"
CONTRACT_DOC="docs/robot-contracts/operating-envelope.md"
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

fail() {
  printf 'operating-envelope fixture manifest: %s\n' "$*" >&2
  exit 1
}

require_file() {
  local path="$1"
  [[ -f "${path}" ]] || fail "missing file: ${path}"
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

mapfile -t valid_paths < <(jq -r '.valid_fixtures[].path' "${MANIFEST}")
mapfile -t invalid_paths < <(jq -r '.invalid_fixtures[].path' "${MANIFEST}")

((${#valid_paths[@]} >= 6)) || fail "manifest must retain at least 6 valid fixtures"
((${#invalid_paths[@]} >= 4)) || fail "manifest must retain at least 4 invalid fixtures"

all_json=("${SCHEMA}" "${MANIFEST}" "${valid_paths[@]}" "${invalid_paths[@]}")
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
  and all(.static_checks[]; type == "string" and length > 0)
  and all(.valid_fixtures[]; (.coverage | type == "array") and (.coverage | length > 0) and all(.coverage[]; type == "string" and length > 0))
  and all(.invalid_fixtures[]; (.expected_failure | type == "string") and (.expected_failure | length > 0))
' "${MANIFEST}" >/dev/null || fail "manifest metadata or coverage is incomplete"

jq -e '
  all(.;
    .schema_version == 1
    and .contract_id == "ft.operating_envelope.v1"
    and .raw_pane_content_stored == false
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
' "${valid_paths[@]}" >/dev/null || fail "valid fixture safety policy is not fail-closed"

for action_class in "${REQUIRED_FORBIDDEN_ACTION_CLASSES[@]}"; do
  jq -e --arg action_class "${action_class}" '
    all(.;
      all(.admission_windows[]; (.forbidden_action_classes | index($action_class) != null))
    )
  ' "${valid_paths[@]}" >/dev/null || fail "valid fixture admission_windows must forbid ${action_class}"
done

jq -e '
  all(.;
    all(.artifact_paths[]?; (startswith("/") | not) and ((contains("../") or startswith("../")) | not))
  )
' "${valid_paths[@]}" >/dev/null || fail "valid fixture artifact_paths must stay repo-relative"

jq -e '
  all(.invalid_fixtures[];
    (.path | type == "string" and length > 0)
    and (.expected_failure | type == "string" and length > 0)
  )
' "${MANIFEST}" >/dev/null || fail "invalid fixture entries need expected_failure text"

printf 'operating-envelope fixture manifest: static verifier passed (%d valid, %d invalid)\n' \
  "${#valid_paths[@]}" "${#invalid_paths[@]}"
