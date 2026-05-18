#!/usr/bin/env bash
# Static verifier for bundled demo-lab fixture manifest and retained goldens.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

MANIFEST="fixtures/demo-lab/manifest.v1.json"
REQUIRED_SCENARIOS=(quickstart usage_limit compaction)
REQUIRED_DEGRADATIONS=(
  "agent_mail_unavailable"
  "disabled_feature"
  "rch_proof_unavailable"
  "unsupported_platform"
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

mapfile -t scenario_paths < <(jq -r '.scenarios[].scenario_path' "${MANIFEST}")
mapfile -t json_golden_paths < <(jq -r '.scenarios[].expected_artifacts[] | select(.kind == "golden_json") | .path' "${MANIFEST}")
mapfile -t toon_golden_paths < <(jq -r '.scenarios[].expected_artifacts[] | select(.kind == "golden_toon") | .path' "${MANIFEST}")

all_json=("${MANIFEST}" "${json_golden_paths[@]}")
for path in "${all_json[@]}" "${scenario_paths[@]}" "${toon_golden_paths[@]}"; do
  require_repo_relative_path "${path}"
done

jq empty "${all_json[@]}"
ruby -ryaml -e 'ARGV.each { |path| YAML.safe_load(File.read(path), permitted_classes: [], aliases: false); }' \
  "${scenario_paths[@]}"

jq -e --argjson required "$(printf '%s\n' "${REQUIRED_SCENARIOS[@]}" | jq -R . | jq -s .)" '
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
    [.kind, .path, (.max_bytes | tostring)] | @tsv
  ' "${MANIFEST}")

  for row in "${artifact_rows[@]}"; do
    IFS=$'\t' read -r kind path max_bytes <<<"${row}"
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

if rg -n --hidden --glob '!*.md' \
  '(sk-[A-Za-z0-9]{20,}|AKIA[0-9A-Z]{16}|ghp_[A-Za-z0-9]{20,}|xox[baprs]-[A-Za-z0-9-]{20,}|Bearer [A-Za-z0-9._-]{20,}|BEGIN (RSA|OPENSSH|EC) PRIVATE KEY)' \
  fixtures/demo-lab >/tmp/ft-demo-lab-secret-scan.txt; then
  cat /tmp/ft-demo-lab-secret-scan.txt >&2
  fail "secret-shaped strings found in demo-lab fixtures"
fi

printf 'demo-lab fixture manifest: static verifier passed (%d scenarios, %d json goldens, %d toon goldens)\n' \
  "${#REQUIRED_SCENARIOS[@]}" "${#json_golden_paths[@]}" "${#toon_golden_paths[@]}"
