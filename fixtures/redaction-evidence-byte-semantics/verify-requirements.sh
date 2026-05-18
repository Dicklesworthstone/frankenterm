#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
json_path="${1:-"$script_dir/requirements.v1.json"}"

if [[ ! -f "$json_path" ]]; then
  echo "missing requirements inventory: $json_path" >&2
  exit 1
fi

jq -e '
  .schema_version == 1
  and .contract_id == "ft.redaction_evidence.byte_semantics_conformance.v1"
  and .producing_bead == "ft-khxlh"
  and .source_design_bead == "ft-wjjkp.2"
  and .target_implementation_bead == "ft-wjjkp.3"
  and .proof_kind == "static-pre-code"
  and .runtime_conformance_claimed == false
' "$json_path" >/dev/null

jq -e '
  .privacy_policy
  | to_entries
  | all(.value == false)
' "$json_path" >/dev/null

jq -e --argjson expected '[
  "replacement_count",
  "original_input_bytes",
  "decoded_input_text_bytes",
  "redacted_output_bytes",
  "secret_input_bytes_replaced",
  "lossy_input_bytes",
  "lossy_replacement_count"
]' '.evidence_fields == $expected' "$json_path" >/dev/null

jq -e '
  (.derived_values | sort) == ([
    "decode_was_lossy",
    "original_to_output_delta",
    "text_length_delta"
  ] | sort)
  and (.evidence_fields | index("decode_was_lossy") | not)
  and (.evidence_fields | index("text_length_delta") | not)
  and (.evidence_fields | index("original_to_output_delta") | not)
' "$json_path" >/dev/null

jq -e '
  (.forbidden_legacy_names | index("bytes_replaced"))
  and (.forbidden_legacy_names | index("matches"))
  and (.evidence_fields | index("bytes_replaced") | not)
  and (.evidence_fields | index("matches") | not)
' "$json_path" >/dev/null

requirement_ids=(
  REQ-FIELD-001
  REQ-FIELD-002
  REQ-FIELD-003
  REQ-FIELD-004
  REQ-FIELD-005
  REQ-LOSSY-001
  REQ-LOSSY-002
  REQ-DERIVED-001
  REQ-STREAM-001
  REQ-STREAM-002
  REQ-MERGE-001
  REQ-COLD-001
  REQ-MMAP-001
  REQ-PRIV-001
  REQ-COMPAT-001
)

fixture_ids=(
  FIX-VALID-UTF8-ONE-REPLACEMENT
  FIX-VALID-UTF8-MARKER-LONGER
  FIX-INVALID-UTF8-NO-REPLACEMENT
  FIX-INVALID-UTF8-ADJACENT-REPLACEMENT
  FIX-INVALID-UTF8-SPLIT-STREAM
  FIX-STREAM-SECRET-SPLIT
  FIX-MERGE-OVERFLOW-EMISSIONS
  FIX-COLD-TIER-CONVERSION
  FIX-MMAP-APPEND-HEADER
  FIX-LEGACY-FIELD-ABSENT
)

for requirement_id in "${requirement_ids[@]}"; do
  jq -e --arg id "$requirement_id" '
    any(.requirements[]; .id == $id and .level == "MUST")
  ' "$json_path" >/dev/null
done

for fixture_id in "${fixture_ids[@]}"; do
  jq -e --arg id "$fixture_id" '
    any(.fixtures[]; .id == $id)
  ' "$json_path" >/dev/null
done

jq -e --argjson expected_count "${#requirement_ids[@]}" '
  ([.requirements[] | select(.level == "MUST")] | length) == $expected_count
  and .coverage_matrix.must_requirement_count == $expected_count
  and .coverage_matrix.must_requirement_fixture_covered_count == $expected_count
' "$json_path" >/dev/null

jq -e --argjson expected_count "${#fixture_ids[@]}" '
  (.fixtures | length) == $expected_count
  and .coverage_matrix.fixture_count == $expected_count
' "$json_path" >/dev/null

jq -e '
  ([.requirements[].id] | unique) as $requirement_ids
  | [.fixtures[].covers[]? | select(($requirement_ids | index(.)) | not)]
  | length == 0
' "$json_path" >/dev/null

jq -e '
  ([.fixtures[].covers[]?] | unique) as $covered
  | [
      .requirements[]
      | select(.level == "MUST")
      | .id as $id
      | select(($covered | index($id)) | not)
    ]
  | length == 0
' "$json_path" >/dev/null

jq -e '
  [
    .fixtures[].retained_material
    | to_entries[]
    | select(.value != false)
  ]
  | length == 0
' "$json_path" >/dev/null

jq -e '
  all(.fixtures[]; .implementation_status == "planned")
  and all(.fixtures[]; (.expected_relationships | length) > 0)
  and all(.fixtures[]; (.covers | length) > 0)
' "$json_path" >/dev/null

echo "redaction evidence byte-semantics conformance inventory: ok"
