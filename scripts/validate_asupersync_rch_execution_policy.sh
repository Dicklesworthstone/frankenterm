#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCHEMA_FILE="${ROOT_DIR}/docs/asupersync-rch-evidence-schema.json"
SCHEMA_VERSION=2
POLICY_VERSION="2.0.0"

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
  local trimmed
  trimmed="$(printf '%s' "${cmd}" | sed -E 's/^[[:space:]]+//')"
  [[ "${trimmed}" =~ ^([A-Za-z_][A-Za-z0-9_]*=[^[:space:]]+[[:space:]]+)*(env[[:space:]]+([A-Za-z_][A-Za-z0-9_]*=[^[:space:]]+[[:space:]]+)*)?rch[[:space:]]+exec[[:space:]]+--([[:space:]]|$) ]]
}

has_rch_cargo_wrapper() {
  local cmd="$1"
  local trimmed
  trimmed="$(printf '%s' "${cmd}" | sed -E 's/^[[:space:]]+//')"
  [[ "${trimmed}" =~ ^([A-Za-z_][A-Za-z0-9_]*=[^[:space:]]+)*run_rch_cargo_logged(_with_timeout)?[[:space:]] ]]
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

  if [[ "${normalized}" =~ (^|[[:space:]])cargo[[:space:]]+(check|build|test|clippy|bench|run|install)([[:space:]]|$) ]]; then
    return 0
  fi

  return 1
}

classify_command_json() {
  local cmd="$1"
  local command_class="light"
  local heavy="false"
  local used_rch="false"
  local requires_rch="false"

  if is_heavy_command "${cmd}"; then
    command_class="heavy"
    heavy="true"
    requires_rch="true"
  fi
  if has_rch_prefix "${cmd}" || has_rch_cargo_wrapper "${cmd}"; then
    used_rch="true"
  fi

  jq -cn \
    --arg command "${cmd}" \
    --arg command_class "${command_class}" \
    --argjson is_heavy "${heavy}" \
    --argjson used_rch "${used_rch}" \
    --argjson requires_rch "${requires_rch}" \
    '{
      command: $command,
      command_class: $command_class,
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

bead_id_is_valid() {
  local bead_id="$1"
  [[ "${bead_id}" =~ ^ft-[[:alnum:]][[:alnum:]-]*(\.[[:alnum:]][[:alnum:]-]*)*$ ]]
}

artifact_path_exists() {
  local path="$1"
  if [[ "${path}" = /* ]]; then
    [[ -e "${path}" ]]
  else
    [[ -e "${ROOT_DIR}/${path}" ]]
  fi
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

  jq -e --argjson schema_version "${SCHEMA_VERSION}" '.schema_version == $schema_version' "${evidence_file}" >/dev/null || {
    echo "schema_version must be ${SCHEMA_VERSION}" >&2
    return 1
  }

  local bead_id
  bead_id="$(jq -r '.bead_id // ""' "${evidence_file}")"
  if ! bead_id_is_valid "${bead_id}"; then
    echo "bead_id must be a normal FrankenTerm ft-* bead id" >&2
    return 1
  fi

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
    local run cmd declared_command_class declared_is_heavy declared_used_rch worker_context target_dir target_dir_lifecycle elapsed exit_status
    local fallback_reason fallback_approved execution_mode validation_status

    run="$(jq -c ".runs[${i}]" "${evidence_file}")"
    cmd="$(jq -r '.command' <<<"${run}")"
    declared_command_class="$(jq -r '.command_class // ""' <<<"${run}")"
    declared_is_heavy="$(jq -r 'if has("is_heavy") then (.is_heavy | tostring) else "" end' <<<"${run}")"
    declared_used_rch="$(jq -r '.used_rch' <<<"${run}")"
    worker_context="$(jq -r '.worker_context' <<<"${run}")"
    target_dir="$(jq -r '.target_dir // ""' <<<"${run}")"
    target_dir_lifecycle="$(jq -r '.target_dir_lifecycle // ""' <<<"${run}")"
    elapsed="$(jq -r '.elapsed_seconds' <<<"${run}")"
    exit_status="$(jq -r '.exit_status' <<<"${run}")"
    fallback_reason="$(jq -r '.fallback_reason_code // ""' <<<"${run}")"
    fallback_approved="$(jq -r '.fallback_approved_by // ""' <<<"${run}")"
    execution_mode="$(jq -r '.execution_mode // ""' <<<"${run}")"
    validation_status="$(jq -r '.validation_status // ""' <<<"${run}")"

    [[ -n "${cmd}" ]] || {
      echo "run[$i] command must be non-empty" >&2
      return 1
    }
    [[ "${declared_command_class}" =~ ^(heavy|light)$ ]] || {
      echo "run[$i] command_class must be heavy or light" >&2
      return 1
    }
    [[ "${declared_is_heavy}" =~ ^(true|false)$ ]] || {
      echo "run[$i] is_heavy must be boolean" >&2
      return 1
    }
    [[ "${declared_used_rch}" =~ ^(true|false)$ ]] || {
      echo "run[$i] used_rch must be boolean" >&2
      return 1
    }
    [[ "${execution_mode}" =~ ^(remote_rch|local_light|approved_local_fallback)$ ]] || {
      echo "run[$i] execution_mode must be remote_rch, local_light, or approved_local_fallback" >&2
      return 1
    }
    [[ "${validation_status}" =~ ^(valid|approved_fallback)$ ]] || {
      echo "run[$i] validation_status must be valid or approved_fallback" >&2
      return 1
    }

    jq -e '.artifact_paths | type == "array" and length > 0' <<<"${run}" >/dev/null || {
      echo "run[$i] artifact_paths must be non-empty array" >&2
      return 1
    }
    local artifact_count j artifact_path
    artifact_count="$(jq '.artifact_paths | length' <<<"${run}")"
    for ((j = 0; j < artifact_count; j++)); do
      artifact_path="$(jq -r ".artifact_paths[${j}]" <<<"${run}")"
      [[ -n "${artifact_path}" ]] || {
        echo "run[$i] artifact_paths[$j] must be non-empty" >&2
        return 1
      }
      artifact_path_exists "${artifact_path}" || {
        echo "run[$i] artifact_paths[$j] does not exist: ${artifact_path}" >&2
        return 1
      }
    done
    jq -e '.residual_risk_notes | type == "string"' <<<"${run}" >/dev/null || {
      echo "run[$i] residual_risk_notes must be string" >&2
      return 1
    }
    [[ -n "${worker_context}" ]] || {
      echo "run[$i] worker_context must be non-empty" >&2
      return 1
    }
    [[ -n "${target_dir}" ]] || {
      echo "run[$i] target_dir must be non-empty" >&2
      return 1
    }
    [[ "${target_dir_lifecycle}" =~ ^(not_applicable|retained|inventory_only|cleanup_approved)$ ]] || {
      echo "run[$i] target_dir_lifecycle must be not_applicable, retained, inventory_only, or cleanup_approved" >&2
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

    local classified expected_command_class expected_heavy expected_used_rch
    classified="$(classify_command_json "${cmd}")"
    expected_command_class="$(jq -r '.command_class' <<<"${classified}")"
    expected_heavy="$(jq -r '.is_heavy' <<<"${classified}")"
    expected_used_rch="$(jq -r '.used_rch' <<<"${classified}")"

    if [[ "${declared_command_class}" != "${expected_command_class}" ]]; then
      echo "run[$i] command_class mismatch: declared=${declared_command_class}, expected=${expected_command_class}" >&2
      return 1
    fi
    if [[ "${declared_is_heavy}" != "${expected_heavy}" ]]; then
      echo "run[$i] is_heavy mismatch: declared=${declared_is_heavy}, expected=${expected_heavy}" >&2
      return 1
    fi
    if [[ "${declared_used_rch}" != "${expected_used_rch}" ]]; then
      echo "run[$i] used_rch mismatch: declared=${declared_used_rch}, expected=${expected_used_rch}" >&2
      return 1
    fi

    local expected_execution_mode expected_validation_status
    expected_execution_mode="local_light"
    expected_validation_status="valid"

    if [[ "${declared_command_class}" == "heavy" ]]; then
      if [[ "${target_dir}" == "not_applicable" || "${target_dir_lifecycle}" == "not_applicable" ]]; then
        echo "run[$i] heavy run requires target_dir and target_dir_lifecycle evidence" >&2
        return 1
      fi

      local heavy_execution_context_is_local="false"
      if worker_context_is_local "${worker_context}"; then
        heavy_execution_context_is_local="true"
      fi

      if [[ "${declared_used_rch}" == "false" || "${heavy_execution_context_is_local}" == "true" ]]; then
        [[ -n "${fallback_reason}" && -n "${fallback_approved}" ]] || {
          echo "run[$i] heavy run requires fallback_reason_code and fallback_approved_by when executed without remote rch confirmation" >&2
          return 1
        }
        expected_execution_mode="approved_local_fallback"
        expected_validation_status="approved_fallback"
      else
        expected_execution_mode="remote_rch"
      fi
    elif [[ "${declared_used_rch}" == "true" ]]; then
      if ! worker_context_is_local "${worker_context}"; then
        expected_execution_mode="remote_rch"
      fi
    elif [[ "${target_dir_lifecycle}" == "not_applicable" && "${target_dir}" != "not_applicable" ]]; then
      echo "run[$i] target_dir must be not_applicable when target_dir_lifecycle is not_applicable" >&2
      return 1
    fi

    if [[ "${execution_mode}" != "${expected_execution_mode}" ]]; then
      echo "run[$i] execution_mode mismatch: declared=${execution_mode}, expected=${expected_execution_mode}" >&2
      return 1
    fi
    if [[ "${validation_status}" != "${expected_validation_status}" ]]; then
      echo "run[$i] validation_status mismatch: declared=${validation_status}, expected=${expected_validation_status}" >&2
      return 1
    fi
  done

  echo "Evidence policy validation passed: ${evidence_file}"
}

run_self_test() {
  local out

  out="$(classify_command_json "cargo test --workspace")"
  [[ "$(jq -r '.command_class' <<<"${out}")" == "heavy" ]] || {
    echo "self-test failed: cargo test should classify as heavy" >&2
    return 1
  }
  [[ "$(jq -r '.is_heavy' <<<"${out}")" == "true" ]] || {
    echo "self-test failed: cargo test should be heavy" >&2
    return 1
  }
  [[ "$(jq -r '.policy_violation' <<<"${out}")" == "true" ]] || {
    echo "self-test failed: heavy command without rch should be violation" >&2
    return 1
  }

  out="$(classify_command_json "cargo install --locked --path crates/frankenterm")"
  [[ "$(jq -r '.is_heavy' <<<"${out}")" == "true" ]] || {
    echo "self-test failed: cargo install should be heavy" >&2
    return 1
  }
  [[ "$(jq -r '.policy_violation' <<<"${out}")" == "true" ]] || {
    echo "self-test failed: cargo install without rch should be violation" >&2
    return 1
  }

  out="$(classify_command_json "rch exec -- cargo test --workspace")"
  [[ "$(jq -r '.policy_violation' <<<"${out}")" == "false" ]] || {
    echo "self-test failed: rch-wrapped heavy command should not be violation" >&2
    return 1
  }

  out="$(classify_command_json "TMPDIR=/tmp rch exec -- cargo test --workspace")"
  [[ "$(jq -r '.used_rch' <<<"${out}")" == "true" ]] || {
    echo "self-test failed: env-prefixed rch command should still count as rch usage" >&2
    return 1
  }
  [[ "$(jq -r '.policy_violation' <<<"${out}")" == "false" ]] || {
    echo "self-test failed: env-prefixed rch heavy command should not be violation" >&2
    return 1
  }

  out="$(classify_command_json "run_rch_cargo_logged target/proof.log env CARGO_TARGET_DIR=target/rch-proof cargo test --workspace")"
  [[ "$(jq -r '.used_rch' <<<"${out}")" == "true" ]] || {
    echo "self-test failed: shared rch cargo wrapper should count as rch usage" >&2
    return 1
  }
  [[ "$(jq -r '.policy_violation' <<<"${out}")" == "false" ]] || {
    echo "self-test failed: shared rch cargo wrapper should not be violation" >&2
    return 1
  }

  out="$(classify_command_json "run_rch_cargo_logged target/proof.log env CARGO_TARGET_DIR=target/rch-proof cargo install --locked --path crates/frankenterm")"
  [[ "$(jq -r '.used_rch' <<<"${out}")" == "true" ]] || {
    echo "self-test failed: shared rch cargo wrapper should count for cargo install" >&2
    return 1
  }
  [[ "$(jq -r '.policy_violation' <<<"${out}")" == "false" ]] || {
    echo "self-test failed: wrapped cargo install should not be violation" >&2
    return 1
  }

  out="$(classify_command_json "run_rch_cargo_logged_with_timeout 120 target/proof.log env CARGO_TARGET_DIR=target/rch-proof cargo test --workspace")"
  [[ "$(jq -r '.used_rch' <<<"${out}")" == "true" ]] || {
    echo "self-test failed: shared timeout rch cargo wrapper should count as rch usage" >&2
    return 1
  }
  [[ "$(jq -r '.policy_violation' <<<"${out}")" == "false" ]] || {
    echo "self-test failed: shared timeout rch cargo wrapper should not be violation" >&2
    return 1
  }

  out="$(classify_command_json "cargo fmt --check")"
  [[ "$(jq -r '.command_class' <<<"${out}")" == "light" ]] || {
    echo "self-test failed: cargo fmt --check should classify as light" >&2
    return 1
  }
  [[ "$(jq -r '.is_heavy' <<<"${out}")" == "false" ]] || {
    echo "self-test failed: cargo fmt --check should be light" >&2
    return 1
  }

  out="$(classify_command_json "bash -lc 'echo rch exec -- cargo test; cargo test --workspace'")"
  [[ "$(jq -r '.is_heavy' <<<"${out}")" == "true" ]] || {
    echo "self-test failed: shell wrapper with local cargo should be heavy" >&2
    return 1
  }
  [[ "$(jq -r '.used_rch' <<<"${out}")" == "false" ]] || {
    echo "self-test failed: shell wrapper that only mentions rch must not count as rch usage" >&2
    return 1
  }

  local tmp_dir tmp_evidence tmp_artifact
  tmp_dir="${ROOT_DIR}/target/rch-policy-self-test/$(date -u +%Y%m%d_%H%M%S)-$$"
  mkdir -p "${tmp_dir}"
  tmp_artifact="${tmp_dir}/mock.jsonl"
  printf '{"mock":true}\n' > "${tmp_artifact}"
  tmp_evidence="${tmp_dir}/valid-evidence.json"

  cat > "${tmp_evidence}" <<JSON
{
  "schema_version": ${SCHEMA_VERSION},
  "bead_id": "ft-emjsg",
  "policy_version": "${POLICY_VERSION}",
  "runs": [
    {
      "timestamp": "2026-02-25T00:00:00Z",
      "command": "rch exec -- cargo test --workspace",
      "command_class": "heavy",
      "is_heavy": true,
      "used_rch": true,
      "worker_context": "worker=mock-1",
      "execution_mode": "remote_rch",
      "target_dir": "/tmp/ft-emjsg-rch-target",
      "target_dir_lifecycle": "retained",
      "artifact_paths": ["${tmp_artifact}"],
      "elapsed_seconds": 12.2,
      "exit_status": 0,
      "residual_risk_notes": "",
      "validation_status": "valid"
    },
    {
      "timestamp": "2026-02-25T00:01:00Z",
      "command": "cargo fmt --check",
      "command_class": "light",
      "is_heavy": false,
      "used_rch": false,
      "worker_context": "local",
      "execution_mode": "local_light",
      "target_dir": "not_applicable",
      "target_dir_lifecycle": "not_applicable",
      "artifact_paths": ["${tmp_artifact}"],
      "elapsed_seconds": 0.4,
      "exit_status": 0,
      "residual_risk_notes": "",
      "validation_status": "valid"
    }
  ]
}
JSON

  validate_evidence_file "${tmp_evidence}" >/dev/null

  local tmp_fail_open tmp_fail_open_recovered tmp_sync_chatter tmp_shell_wrapper
  local tmp_missing_artifact tmp_missing_is_heavy tmp_malformed_bead tmp_stale_schema
  tmp_fail_open="${tmp_dir}/fail-open.json"
  tmp_fail_open_recovered="${tmp_dir}/fail-open-recovered.json"
  tmp_sync_chatter="${tmp_dir}/sync-chatter.json"
  tmp_shell_wrapper="${tmp_dir}/shell-wrapper.json"
  tmp_missing_artifact="${tmp_dir}/missing-artifact.json"
  tmp_missing_is_heavy="${tmp_dir}/missing-is-heavy.json"
  tmp_malformed_bead="${tmp_dir}/malformed-bead.json"
  tmp_stale_schema="${tmp_dir}/stale-schema.json"

  jq '.runs[0].worker_context = "local_fallback" | .runs[0].execution_mode = "remote_rch"' \
    "${tmp_evidence}" > "${tmp_fail_open}"

  if validate_evidence_file "${tmp_fail_open}" >/dev/null 2>&1; then
    echo "self-test failed: heavy local execution after rch wrapper must require fallback metadata" >&2
    return 1
  fi

  jq '.runs[0].fallback_reason_code = "RCH-LOCAL-FALLBACK" |
      .runs[0].fallback_approved_by = "human-operator" |
      .runs[0].execution_mode = "approved_local_fallback" |
      .runs[0].validation_status = "approved_fallback"' \
    "${tmp_fail_open}" > "${tmp_fail_open_recovered}"
  validate_evidence_file "${tmp_fail_open_recovered}" >/dev/null

  jq '.runs[0].command = "rch status && cargo test --workspace" |
      .runs[0].used_rch = true |
      .runs[0].execution_mode = "remote_rch"' \
    "${tmp_evidence}" > "${tmp_sync_chatter}"
  if validate_evidence_file "${tmp_sync_chatter}" >/dev/null 2>&1; then
    echo "self-test failed: RCH setup chatter must not count as remote cargo proof" >&2
    return 1
  fi

  jq '.runs[0].command = "bash -lc '\''echo rch exec -- cargo test; cargo test --workspace'\''" |
      .runs[0].used_rch = true |
      .runs[0].execution_mode = "remote_rch"' \
    "${tmp_evidence}" > "${tmp_shell_wrapper}"
  if validate_evidence_file "${tmp_shell_wrapper}" >/dev/null 2>&1; then
    echo "self-test failed: shell wrapper that only mentions RCH must not validate as RCH proof" >&2
    return 1
  fi

  jq --arg missing "${tmp_dir}/missing.jsonl" '.runs[0].artifact_paths = [$missing]' \
    "${tmp_evidence}" > "${tmp_missing_artifact}"
  if validate_evidence_file "${tmp_missing_artifact}" >/dev/null 2>&1; then
    echo "self-test failed: missing artifact paths must be rejected" >&2
    return 1
  fi

  jq 'del(.runs[0].is_heavy)' "${tmp_evidence}" > "${tmp_missing_is_heavy}"
  if validate_evidence_file "${tmp_missing_is_heavy}" >/dev/null 2>&1; then
    echo "self-test failed: missing is_heavy must be rejected" >&2
    return 1
  fi

  jq '.bead_id = "wa-old.1"' "${tmp_evidence}" > "${tmp_malformed_bead}"
  if validate_evidence_file "${tmp_malformed_bead}" >/dev/null 2>&1; then
    echo "self-test failed: malformed/non-ft bead id must be rejected" >&2
    return 1
  fi

  jq '.schema_version = 1' "${tmp_evidence}" > "${tmp_stale_schema}"
  if validate_evidence_file "${tmp_stale_schema}" >/dev/null 2>&1; then
    echo "self-test failed: stale schema_version must be rejected" >&2
    return 1
  fi

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
