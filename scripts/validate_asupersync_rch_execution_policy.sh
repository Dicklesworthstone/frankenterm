#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCHEMA_FILE="${ROOT_DIR}/docs/asupersync-rch-evidence-schema.json"

usage() {
  cat <<'EOF'
Usage:
  validate_asupersync_rch_execution_policy.sh --classify "<command>"
  validate_asupersync_rch_execution_policy.sh --validate-evidence <path>
  validate_asupersync_rch_execution_policy.sh --self-test
EOF
}

has_rch_prefix() {
  local cmd="$1"
  [[ "${cmd}" =~ (^|[[:space:]])rch[[:space:]]+exec[[:space:]]+-- ]]
}

is_heavy_command() {
  local cmd="$1"
  local normalized

  normalized="$(echo "${cmd}" | tr '[:upper:]' '[:lower:]')"
  if [[ ! "${normalized}" =~ (^|[[:space:]])cargo([[:space:]]|$) ]]; then
    return 1
  fi

  if [[ "${normalized}" =~ (^|[[:space:]])cargo[[:space:]]+(fmt|metadata|locate-project)([[:space:]]|$) ]]; then
    return 1
  fi

  if [[ "${normalized}" =~ (^|[[:space:]])cargo[[:space:]]+(check|build|test|clippy|bench|run)([[:space:]]|$) ]]; then
    return 0
  fi

  return 1
}

classify_command_json() {
  local cmd="$1"
  local heavy="false"
  local used_rch="false"
  local requires_rch="false"

  if is_heavy_command "${cmd}"; then
    heavy="true"
    requires_rch="true"
  fi
  if has_rch_prefix "${cmd}"; then
    used_rch="true"
  fi

  jq -cn \
    --arg command "${cmd}" \
    --argjson is_heavy "${heavy}" \
    --argjson used_rch "${used_rch}" \
    --argjson requires_rch "${requires_rch}" \
    '{
      command: $command,
      is_heavy: $is_heavy,
      used_rch: $used_rch,
      requires_rch: $requires_rch,
      policy_violation: ($requires_rch and ($used_rch | not))
    }'
}

worker_context_is_local() {
  local worker_context="$1"
  local normalized
  normalized="$(echo "${worker_context}" | tr '[:upper:]' '[:lower:]')"
  [[ "${normalized}" == *local* || "${normalized}" == *fallback* ]]
}

validate_evidence_file() {
  local evidence_file="$1"

  if [[ ! -f "${evidence_file}" ]]; then
    echo "evidence file not found: ${evidence_file}" >&2
    return 1
  fi
  if [[ ! -f "${SCHEMA_FILE}" ]]; then
    echo "schema file not found: ${SCHEMA_FILE}" >&2
    return 1
  fi

  jq -e '.schema_version == 1' "${evidence_file}" >/dev/null || {
    echo "schema_version must be 1" >&2
    return 1
  }
  jq -e '.bead_id | test("^ft-e34d9\\.10\\..+")' "${evidence_file}" >/dev/null || {
    echo "bead_id must target ft-e34d9.10.* scope" >&2
    return 1
  }
  jq -e '.policy_version | type == "string" and length > 0' "${evidence_file}" >/dev/null || {
    echo "policy_version must be a non-empty string" >&2
    return 1
  }
  jq -e '.runs | type == "array" and length > 0' "${evidence_file}" >/dev/null || {
    echo "runs must be a non-empty array" >&2
    return 1
  }

  local runs_count
  runs_count="$(jq '.runs | length' "${evidence_file}")"

  local i
  for ((i = 0; i < runs_count; i++)); do
    local run cmd declared_is_heavy declared_used_rch worker_context elapsed exit_status
    local fallback_reason fallback_approved

    run="$(jq -c ".runs[${i}]" "${evidence_file}")"
    cmd="$(jq -r '.command' <<<"${run}")"
    declared_is_heavy="$(jq -r '.is_heavy' <<<"${run}")"
    declared_used_rch="$(jq -r '.used_rch' <<<"${run}")"
    worker_context="$(jq -r '.worker_context' <<<"${run}")"
    elapsed="$(jq -r '.elapsed_seconds' <<<"${run}")"
    exit_status="$(jq -r '.exit_status' <<<"${run}")"
    fallback_reason="$(jq -r '.fallback_reason_code // ""' <<<"${run}")"
    fallback_approved="$(jq -r '.fallback_approved_by // ""' <<<"${run}")"

    jq -e '.artifact_paths | type == "array" and length > 0' <<<"${run}" >/dev/null || {
      echo "run[$i] artifact_paths must be non-empty array" >&2
      return 1
    }
    jq -e '.residual_risk_notes | type == "string"' <<<"${run}" >/dev/null || {
      echo "run[$i] residual_risk_notes must be string" >&2
      return 1
    }
    [[ -n "${worker_context}" ]] || {
      echo "run[$i] worker_context must be non-empty" >&2
      return 1
    }
    [[ "${elapsed}" =~ ^[0-9]+([.][0-9]+)?$ ]] || {
      echo "run[$i] elapsed_seconds must be numeric >= 0" >&2
      return 1
    }
    [[ "${exit_status}" =~ ^-?[0-9]+$ ]] || {
      echo "run[$i] exit_status must be integer" >&2
      return 1
    }

    local classified expected_heavy expected_used_rch
    classified="$(classify_command_json "${cmd}")"
    expected_heavy="$(jq -r '.is_heavy' <<<"${classified}")"
    expected_used_rch="$(jq -r '.used_rch' <<<"${classified}")"

    if [[ "${declared_is_heavy}" != "${expected_heavy}" ]]; then
      echo "run[$i] is_heavy mismatch: declared=${declared_is_heavy}, expected=${expected_heavy}" >&2
      return 1
    fi
    if [[ "${declared_used_rch}" != "${expected_used_rch}" ]]; then
      echo "run[$i] used_rch mismatch: declared=${declared_used_rch}, expected=${expected_used_rch}" >&2
      return 1
    fi

    if [[ "${declared_is_heavy}" == "true" ]]; then
      local heavy_execution_context_is_local="false"
      if worker_context_is_local "${worker_context}"; then
        heavy_execution_context_is_local="true"
      fi

      if [[ "${declared_used_rch}" == "false" || "${heavy_execution_context_is_local}" == "true" ]]; then
        [[ -n "${fallback_reason}" && -n "${fallback_approved}" ]] || {
          echo "run[$i] heavy run requires fallback_reason_code and fallback_approved_by when executed without remote rch confirmation" >&2
          return 1
        }
      fi
    fi
  done

  echo "Evidence policy validation passed: ${evidence_file}"
}

run_self_test() {
  local out

  out="$(classify_command_json "cargo test --workspace")"
  [[ "$(jq -r '.is_heavy' <<<"${out}")" == "true" ]] || {
    echo "self-test failed: cargo test should be heavy" >&2
    return 1
  }
  [[ "$(jq -r '.policy_violation' <<<"${out}")" == "true" ]] || {
    echo "self-test failed: heavy command without rch should be violation" >&2
    return 1
  }

  out="$(classify_command_json "rch exec -- cargo test --workspace")"
  [[ "$(jq -r '.policy_violation' <<<"${out}")" == "false" ]] || {
    echo "self-test failed: rch-wrapped heavy command should not be violation" >&2
    return 1
  }

  out="$(classify_command_json "cargo fmt --check")"
  [[ "$(jq -r '.is_heavy' <<<"${out}")" == "false" ]] || {
    echo "self-test failed: cargo fmt --check should be light" >&2
    return 1
  }

  local tmp_evidence
  tmp_evidence="$(mktemp)"

  cat > "${tmp_evidence}" <<'JSON'
{
  "schema_version": 1,
  "bead_id": "ft-e34d9.10.1.4",
  "policy_version": "1.0.0",
  "runs": [
    {
      "timestamp": "2026-02-25T00:00:00Z",
      "command": "rch exec -- cargo test --workspace",
      "is_heavy": true,
      "used_rch": true,
      "worker_context": "worker=mock-1",
      "artifact_paths": ["tests/e2e/logs/mock.jsonl"],
      "elapsed_seconds": 12.2,
      "exit_status": 0,
      "residual_risk_notes": ""
    },
    {
      "timestamp": "2026-02-25T00:01:00Z",
      "command": "cargo fmt --check",
      "is_heavy": false,
      "used_rch": false,
      "worker_context": "local",
      "artifact_paths": ["tests/e2e/logs/mock.jsonl"],
      "elapsed_seconds": 0.4,
      "exit_status": 0,
      "residual_risk_notes": ""
    }
  ]
}
JSON

  validate_evidence_file "${tmp_evidence}" >/dev/null

  local tmp_fail_open tmp_fail_open_recovered
  tmp_fail_open="$(mktemp)"
  tmp_fail_open_recovered="$(mktemp)"

  cat > "${tmp_fail_open}" <<'JSON'
{
  "schema_version": 1,
  "bead_id": "ft-e34d9.10.1.4",
  "policy_version": "1.0.0",
  "runs": [
    {
      "timestamp": "2026-02-25T00:02:00Z",
      "command": "rch exec -- cargo check --workspace --all-targets",
      "is_heavy": true,
      "used_rch": true,
      "worker_context": "local_fallback",
      "artifact_paths": ["tests/e2e/logs/mock.jsonl"],
      "elapsed_seconds": 4.2,
      "exit_status": 0,
      "residual_risk_notes": ""
    }
  ]
}
JSON

  if validate_evidence_file "${tmp_fail_open}" >/dev/null 2>&1; then
    echo "self-test failed: heavy local execution after rch wrapper must require fallback metadata" >&2
    rm -f "${tmp_evidence}" "${tmp_fail_open}" "${tmp_fail_open_recovered}"
    return 1
  fi

  jq '.runs[0].fallback_reason_code = "RCH-LOCAL-FALLBACK" | .runs[0].fallback_approved_by = "human-operator"' "${tmp_fail_open}" > "${tmp_fail_open_recovered}"
  validate_evidence_file "${tmp_fail_open_recovered}" >/dev/null

  rm -f "${tmp_evidence}" "${tmp_fail_open}" "${tmp_fail_open_recovered}"
  echo "Self-test passed"
}

if [[ $# -lt 1 ]]; then
  usage
  exit 1
fi

case "$1" in
  --classify)
    shift
    if [[ $# -ne 1 ]]; then
      usage
      exit 1
    fi
    classify_command_json "$1"
    ;;
  --validate-evidence)
    shift
    if [[ $# -ne 1 ]]; then
      usage
      exit 1
    fi
    validate_evidence_file "$1"
    ;;
  --self-test)
    run_self_test
    ;;
  *)
    usage
    exit 1
    ;;
esac
