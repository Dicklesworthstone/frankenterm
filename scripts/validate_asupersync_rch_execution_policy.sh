#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCHEMA_FILE="${ROOT_DIR}/docs/asupersync-rch-evidence-schema.json"
SCHEMA_VERSION=3
POLICY_VERSION="3.2.0"
REDACTION_POLICY_VERSION="frankenterm.redactor.v1"

usage() {
  cat <<'EOF'
Usage:
  validate_asupersync_rch_execution_policy.sh --classify "<command>"
  validate_asupersync_rch_execution_policy.sh --redact-text "<text>"
  validate_asupersync_rch_execution_policy.sh --validate-evidence <path>
  validate_asupersync_rch_execution_policy.sh --aggregate-ledger <jsonl-path> [jsonl-path ...]
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

is_rch_diagnose_dry_run() {
  local cmd="$1"
  local trimmed
  local normalized

  trimmed="$(printf '%s' "${cmd}" | sed -E 's/^[[:space:]]+//')"
  normalized="$(printf '%s' "${trimmed}" | tr '[:upper:]' '[:lower:]')"
  [[ "${normalized}" =~ ^([a-z_][a-z0-9_]*=[^[:space:]]+[[:space:]]+)*(env[[:space:]]+([a-z_][a-z0-9_]*=[^[:space:]]+[[:space:]]+)*)?rch([[:space:]]+--[a-z0-9_-]+)*[[:space:]]+diagnose[[:space:]]+--dry-run([[:space:]]|$) ]]
}

is_heavy_command() {
  local cmd="$1"
  local normalized

  if is_rch_diagnose_dry_run "${cmd}"; then
    return 1
  fi

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

optional_worker_text_is_safe() {
  local label="$1"
  local text="$2"

  [[ -z "${text}" || "${text}" == "null" ]] && return 0
  require_no_sensitive_text "${label}" "${text}" || return 1
}

missing_worker_value() {
  local value="$1"
  [[ -z "${value}" || "${value}" == "null" || "${value}" == "unknown" || "${value}" == "not_applicable" ]]
}

repo_snapshot_head_is_valid() {
  local value="$1"
  [[ "${value}" =~ ^[a-f0-9]{40}$ ]]
}

artifact_path_exists() {
  local path="$1"
  if [[ "${path}" = /* ]]; then
    [[ -e "${path}" ]]
  else
    [[ -e "${ROOT_DIR}/${path}" ]]
  fi
}

fingerprint_text() {
  local text="$1"
  local digest

  if command -v shasum >/dev/null 2>&1; then
    digest="$(printf '%s' "${text}" | shasum -a 256 | awk '{print $1}')"
  elif command -v sha256sum >/dev/null 2>&1; then
    digest="$(printf '%s' "${text}" | sha256sum | awk '{print $1}')"
  else
    echo "sha256 tool not found; install shasum or sha256sum" >&2
    return 1
  fi

  printf 'sha256:%s' "${digest}"
}

fingerprint_artifact_paths() {
  local run="$1"
  local canonical
  canonical="$(jq -c '.artifact_paths' <<<"${run}")"
  fingerprint_text "${canonical}"
}

fingerprint_is_valid() {
  [[ "$1" =~ ^sha256:[a-f0-9]{64}$ ]]
}

redact_proof_ledger_text() {
  local text="$1"

  perl -0pe '
    s/-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----/[REDACTED]/gs;
    s/\bsk-ant-[A-Za-z0-9_-]{40,}/[REDACTED]/g;
    s/\bsk-(?:proj-|svcacct-|admin-)?[A-Za-z0-9_-]{16,}/[REDACTED]/g;
    s/\bgh[pousr]_[A-Za-z0-9]{20,}/[REDACTED]/g;
    s/\bgithub_pat_[A-Za-z0-9_]{20,}/[REDACTED]/g;
    s/\bAKIA[0-9A-Z]{16}\b/[REDACTED]/g;
    s/\bAIza[0-9A-Za-z_-]{20,}/[REDACTED]/g;
    s/\bya29\.[0-9A-Za-z_-]{20,}/[REDACTED]/g;
    s/\bBearer\s+[A-Za-z0-9._\/+=-]{16,}/Bearer [REDACTED]/gi;
    s#([?&](?:access_token|code|token)=)[^&\s"\x27]+#${1}[REDACTED]#gi;
    s#(?i)\b(?:postgres|mysql|mongodb|redis)(?:ql)?://[^\s:]+:[^@\s]+@#[REDACTED]#g;
    s/\b(aws_secret_access_key|api[_-]?key|apikey|access[_-]?token|token|password|secret)\s*[:=]\s*["\x27]?[^\s"\x27]{8,}/$1=[REDACTED]/gi;
    s#(^|[^A-Za-z0-9_.-])(?:/Users|/home)/[^\s"\x27]+#${1}[REDACTED:filesystem_path]#g;
    s#(^|[^A-Za-z0-9_.-])~/(?:\.ssh|\.config|\.aws|\.cargo)/[^\s"\x27]+#${1}[REDACTED:filesystem_path]#g;
    s#(^|[^A-Za-z0-9_.-])/[^[:space:]"\x27]*/\.ssh/[^\s"\x27]+#${1}[REDACTED:filesystem_path]#g;
    s#(^|[^A-Za-z0-9_.-])/[^[:space:]"\x27]*/id_(?:rsa|ed25519|ecdsa)(?:[^[:alnum:]_]|\z)#${1}[REDACTED:filesystem_path]#g;
  ' <<<"${text}"
}

redact_text_json() {
  local text="$1"
  local redacted fingerprint

  redacted="$(redact_proof_ledger_text "${text}")"
  fingerprint="$(fingerprint_text "${text}")"

  jq -cn \
    --arg redaction_policy_version "${REDACTION_POLICY_VERSION}" \
    --arg redacted "${redacted}" \
    --arg fingerprint "${fingerprint}" \
    '{
      redaction_policy_version: $redaction_policy_version,
      redacted: $redacted,
      fingerprint: $fingerprint
    }'
}

contains_sensitive_text() {
  local text="$1"
  local redacted

  redacted="$(redact_proof_ledger_text "${text}")"
  if [[ "${redacted}" != "${text}" ]]; then
    return 0
  fi

  return 1
}

require_no_sensitive_text() {
  local label="$1"
  local text="$2"

  if contains_sensitive_text "${text}"; then
    echo "${label} contains unredacted sensitive text; use redacted text plus fingerprint $(fingerprint_text "${text}")" >&2
    return 1
  fi
}

require_fingerprint() {
  local label="$1"
  local actual="$2"
  local expected="$3"

  fingerprint_is_valid "${actual}" || {
    echo "${label} must be sha256:<64 lowercase hex chars>" >&2
    return 1
  }
  [[ "${actual}" == "${expected}" ]] || {
    echo "${label} mismatch: declared=${actual}, expected=${expected}" >&2
    return 1
  }
}

require_public_text_fingerprint() {
  local label="$1"
  local actual="$2"
  local public_text="$3"

  fingerprint_is_valid "${actual}" || {
    echo "${label} must be sha256:<64 lowercase hex chars>" >&2
    return 1
  }

  if [[ "${public_text}" != *"[REDACTED"* && "${actual}" != "$(fingerprint_text "${public_text}")" ]]; then
    echo "${label} mismatch for non-redacted public text" >&2
    return 1
  fi
}

validate_worker_evidence_fields() {
  local run_index="$1"
  local run="$2"
  local command_class="$3"
  local used_rch="$4"
  local execution_mode="$5"
  local validation_status="$6"

  local confidence intended_worker selected_worker worker_queue_state repo_snapshot_head
  local source_mirror_status source_mirror_reason_code remote_cargo_reached
  local remote_rustc_reached test_binary_reached

  jq -e '
    (if has("worker_evidence_confidence") then (.worker_evidence_confidence == null or (.worker_evidence_confidence | type == "string")) else true end) and
    (if has("intended_worker_id") then (.intended_worker_id == null or (.intended_worker_id | type == "string")) else true end) and
    (if has("selected_worker_id") then (.selected_worker_id == null or (.selected_worker_id | type == "string")) else true end) and
    (if has("worker_queue_state") then (.worker_queue_state == null or (.worker_queue_state | type == "string")) else true end) and
    (if has("repo_snapshot_head") then (.repo_snapshot_head == null or (.repo_snapshot_head | type == "string")) else true end) and
    (if has("source_mirror_status") then (.source_mirror_status == null or (.source_mirror_status | type == "string")) else true end) and
    (if has("source_mirror_reason_code") then (.source_mirror_reason_code == null or (.source_mirror_reason_code | type == "string")) else true end) and
    (if has("remote_cargo_reached") then (.remote_cargo_reached == null or (.remote_cargo_reached | type == "boolean")) else true end) and
    (if has("remote_rustc_reached") then (.remote_rustc_reached == null or (.remote_rustc_reached | type == "boolean")) else true end) and
    (if has("test_binary_reached") then (.test_binary_reached == null or (.test_binary_reached | type == "boolean")) else true end)
  ' <<<"${run}" >/dev/null || {
    echo "run[$run_index] worker evidence fields must use strings, booleans, or nulls with their documented types" >&2
    return 1
  }

  confidence="$(jq -r '.worker_evidence_confidence // ""' <<<"${run}")"
  intended_worker="$(jq -r '.intended_worker_id // ""' <<<"${run}")"
  selected_worker="$(jq -r '.selected_worker_id // ""' <<<"${run}")"
  worker_queue_state="$(jq -r '.worker_queue_state // ""' <<<"${run}")"
  repo_snapshot_head="$(jq -r '.repo_snapshot_head // ""' <<<"${run}")"
  source_mirror_status="$(jq -r '.source_mirror_status // ""' <<<"${run}")"
  source_mirror_reason_code="$(jq -r '.source_mirror_reason_code // ""' <<<"${run}")"
  remote_cargo_reached="$(jq -r 'if has("remote_cargo_reached") then (.remote_cargo_reached | tostring) else "" end' <<<"${run}")"
  remote_rustc_reached="$(jq -r 'if has("remote_rustc_reached") then (.remote_rustc_reached | tostring) else "" end' <<<"${run}")"
  test_binary_reached="$(jq -r 'if has("test_binary_reached") then (.test_binary_reached | tostring) else "" end' <<<"${run}")"

  optional_worker_text_is_safe "run[$run_index] worker_evidence_confidence" "${confidence}" || return 1
  optional_worker_text_is_safe "run[$run_index] intended_worker_id" "${intended_worker}" || return 1
  optional_worker_text_is_safe "run[$run_index] selected_worker_id" "${selected_worker}" || return 1
  optional_worker_text_is_safe "run[$run_index] worker_queue_state" "${worker_queue_state}" || return 1
  optional_worker_text_is_safe "run[$run_index] repo_snapshot_head" "${repo_snapshot_head}" || return 1
  optional_worker_text_is_safe "run[$run_index] source_mirror_status" "${source_mirror_status}" || return 1
  optional_worker_text_is_safe "run[$run_index] source_mirror_reason_code" "${source_mirror_reason_code}" || return 1

  [[ -z "${confidence}" || "${confidence}" =~ ^(target_worker_remote_proof|target_worker_mirror_attestation|scheduler_selected_remote_proof|worker_self_test_only|sync_or_transfer_only|inconclusive_worker_evidence|legacy_unknown_worker_evidence)$ ]] || {
    echo "run[$run_index] worker_evidence_confidence has unsupported value: ${confidence}" >&2
    return 1
  }
  [[ -z "${worker_queue_state}" || "${worker_queue_state}" =~ ^(ready|busy_wait|unhealthy|unsupported_worker_selection|queue_timeout|unknown|not_applicable)$ ]] || {
    echo "run[$run_index] worker_queue_state must be ready, busy_wait, unhealthy, unsupported_worker_selection, queue_timeout, unknown, or not_applicable" >&2
    return 1
  }
  [[ -z "${repo_snapshot_head}" || "${repo_snapshot_head}" == "unknown" || "${repo_snapshot_head}" == "not_applicable" || "${repo_snapshot_head}" =~ ^[a-f0-9]{40}$ ]] || {
    echo "run[$run_index] repo_snapshot_head must be a 40-character lowercase git SHA, unknown, or not_applicable" >&2
    return 1
  }
  [[ -z "${source_mirror_status}" || "${source_mirror_status}" =~ ^(present|missing|stale|unreachable|not_checked|unknown|not_applicable)$ ]] || {
    echo "run[$run_index] source_mirror_status must be present, missing, stale, unreachable, not_checked, unknown, or not_applicable" >&2
    return 1
  }

  case "${confidence}" in
    target_worker_remote_proof)
      missing_worker_value "${intended_worker}" && {
        echo "run[$run_index] target_worker_remote_proof requires intended_worker_id" >&2
        return 1
      }
      missing_worker_value "${selected_worker}" && {
        echo "run[$run_index] target_worker_remote_proof requires selected_worker_id" >&2
        return 1
      }
      [[ "${intended_worker}" == "${selected_worker}" ]] || {
        echo "run[$run_index] target_worker_remote_proof requires intended_worker_id to match selected_worker_id" >&2
        return 1
      }
      [[ "${used_rch}" == "true" && "${execution_mode}" == "remote_rch" && "${validation_status}" == "valid" ]] || {
        echo "run[$run_index] target_worker_remote_proof requires valid remote_rch execution" >&2
        return 1
      }
      repo_snapshot_head_is_valid "${repo_snapshot_head}" || {
        echo "run[$run_index] target_worker_remote_proof requires repo_snapshot_head" >&2
        return 1
      }
      [[ "${source_mirror_status}" == "present" ]] || {
        echo "run[$run_index] target_worker_remote_proof requires source_mirror_status=present" >&2
        return 1
      }
      if [[ "${command_class}" == "heavy" && "${remote_cargo_reached}" != "true" ]]; then
        echo "run[$run_index] target_worker_remote_proof for heavy commands requires remote_cargo_reached=true" >&2
        return 1
      fi
      ;;
    scheduler_selected_remote_proof)
      missing_worker_value "${selected_worker}" && {
        echo "run[$run_index] scheduler_selected_remote_proof requires selected_worker_id" >&2
        return 1
      }
      [[ "${used_rch}" == "true" && "${execution_mode}" == "remote_rch" && "${validation_status}" == "valid" ]] || {
        echo "run[$run_index] scheduler_selected_remote_proof requires valid remote_rch execution" >&2
        return 1
      }
      repo_snapshot_head_is_valid "${repo_snapshot_head}" || {
        echo "run[$run_index] scheduler_selected_remote_proof requires repo_snapshot_head" >&2
        return 1
      }
      [[ "${source_mirror_status}" == "present" ]] || {
        echo "run[$run_index] scheduler_selected_remote_proof requires source_mirror_status=present" >&2
        return 1
      }
      if [[ "${command_class}" == "heavy" && "${remote_cargo_reached}" != "true" ]]; then
        echo "run[$run_index] scheduler_selected_remote_proof for heavy commands requires remote_cargo_reached=true" >&2
        return 1
      fi
      ;;
    target_worker_mirror_attestation)
      if missing_worker_value "${intended_worker}" && missing_worker_value "${selected_worker}"; then
        echo "run[$run_index] target_worker_mirror_attestation requires intended_worker_id or selected_worker_id" >&2
        return 1
      fi
      [[ "${source_mirror_status}" =~ ^(present|missing|stale|unreachable)$ ]] || {
        echo "run[$run_index] target_worker_mirror_attestation requires source_mirror_status present, missing, stale, or unreachable" >&2
        return 1
      }
      if [[ "${source_mirror_status}" == "present" ]]; then
        repo_snapshot_head_is_valid "${repo_snapshot_head}" || {
          echo "run[$run_index] target_worker_mirror_attestation with present source mirror requires repo_snapshot_head" >&2
          return 1
        }
      elif [[ -z "${source_mirror_reason_code}" || "${source_mirror_reason_code}" == "null" ]]; then
        echo "run[$run_index] target_worker_mirror_attestation with failed mirror requires source_mirror_reason_code" >&2
        return 1
      fi
      [[ "${remote_cargo_reached}" != "true" && "${remote_rustc_reached}" != "true" && "${test_binary_reached}" != "true" ]] || {
        echo "run[$run_index] target_worker_mirror_attestation must not claim remote Cargo, rustc, or test execution" >&2
        return 1
      }
      ;;
    worker_self_test_only|sync_or_transfer_only)
      [[ "${remote_cargo_reached}" != "true" && "${remote_rustc_reached}" != "true" && "${test_binary_reached}" != "true" ]] || {
        echo "run[$run_index] ${confidence} must not claim remote Cargo, rustc, or test execution" >&2
        return 1
      }
      ;;
  esac
}

validate_evidence_file() {
  local evidence_file="$1"

  if [[ ! -f "${evidence_file}" && ! -r "${evidence_file}" ]]; then
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
    local run cmd command_fingerprint declared_command_class declared_is_heavy declared_used_rch
    local worker_context worker_context_fingerprint target_dir target_dir_fingerprint
    local target_dir_lifecycle artifact_paths_fingerprint elapsed exit_status
    local residual_risk_notes residual_risk_notes_fingerprint
    local fallback_reason fallback_approved execution_mode validation_status

    run="$(jq -c ".runs[${i}]" "${evidence_file}")"
    cmd="$(jq -r '.command' <<<"${run}")"
    command_fingerprint="$(jq -r '.command_fingerprint // ""' <<<"${run}")"
    declared_command_class="$(jq -r '.command_class // ""' <<<"${run}")"
    declared_is_heavy="$(jq -r 'if has("is_heavy") then (.is_heavy | tostring) else "" end' <<<"${run}")"
    declared_used_rch="$(jq -r '.used_rch' <<<"${run}")"
    worker_context="$(jq -r '.worker_context' <<<"${run}")"
    worker_context_fingerprint="$(jq -r '.worker_context_fingerprint // ""' <<<"${run}")"
    target_dir="$(jq -r '.target_dir // ""' <<<"${run}")"
    target_dir_fingerprint="$(jq -r '.target_dir_fingerprint // ""' <<<"${run}")"
    target_dir_lifecycle="$(jq -r '.target_dir_lifecycle // ""' <<<"${run}")"
    artifact_paths_fingerprint="$(jq -r '.artifact_paths_fingerprint // ""' <<<"${run}")"
    elapsed="$(jq -r '.elapsed_seconds' <<<"${run}")"
    exit_status="$(jq -r '.exit_status' <<<"${run}")"
    residual_risk_notes="$(jq -r '.residual_risk_notes // ""' <<<"${run}")"
    residual_risk_notes_fingerprint="$(jq -r '.residual_risk_notes_fingerprint // ""' <<<"${run}")"
    fallback_reason="$(jq -r '.fallback_reason_code // ""' <<<"${run}")"
    fallback_approved="$(jq -r '.fallback_approved_by // ""' <<<"${run}")"
    execution_mode="$(jq -r '.execution_mode // ""' <<<"${run}")"
    validation_status="$(jq -r '.validation_status // ""' <<<"${run}")"

    [[ -n "${cmd}" ]] || {
      echo "run[$i] command must be non-empty" >&2
      return 1
    }
    require_no_sensitive_text "run[$i] command" "${cmd}" || return 1
    require_no_sensitive_text "run[$i] worker_context" "${worker_context}" || return 1
    require_no_sensitive_text "run[$i] target_dir" "${target_dir}" || return 1
    require_no_sensitive_text "run[$i] residual_risk_notes" "${residual_risk_notes}" || return 1
    require_public_text_fingerprint "run[$i] command_fingerprint" "${command_fingerprint}" "${cmd}" || return 1
    require_public_text_fingerprint "run[$i] worker_context_fingerprint" "${worker_context_fingerprint}" "${worker_context}" || return 1
    require_public_text_fingerprint "run[$i] target_dir_fingerprint" "${target_dir_fingerprint}" "${target_dir}" || return 1
    require_public_text_fingerprint "run[$i] residual_risk_notes_fingerprint" "${residual_risk_notes_fingerprint}" "${residual_risk_notes}" || return 1
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
      require_no_sensitive_text "run[$i] artifact_paths[$j]" "${artifact_path}" || return 1
      artifact_path_exists "${artifact_path}" || {
        echo "run[$i] artifact_paths[$j] does not exist: $(redact_proof_ledger_text "${artifact_path}")" >&2
        return 1
      }
    done
    require_fingerprint "run[$i] artifact_paths_fingerprint" "${artifact_paths_fingerprint}" "$(fingerprint_artifact_paths "${run}")" || return 1
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
    validate_worker_evidence_fields \
      "${i}" \
      "${run}" \
      "${declared_command_class}" \
      "${declared_used_rch}" \
      "${execution_mode}" \
      "${validation_status}" || return 1
  done

  echo "Evidence policy validation passed: ${evidence_file}"
}

aggregate_reason_for_validation_error() {
  local error_text="$1"

  if [[ "${error_text}" == *"artifact_paths["* && "${error_text}" == *"does not exist"* ]]; then
    printf '%s\n' "aggregate.missing_artifact"
  elif [[ "${error_text}" == *"heavy run requires fallback_reason_code"* ]]; then
    printf '%s\n' "aggregate.rejected_local_heavy"
  else
    printf '%s\n' "aggregate.malformed"
  fi
}

aggregate_category_for_reason() {
  case "$1" in
    aggregate.missing_artifact) printf '%s\n' "missing_artifact" ;;
    aggregate.rejected_local_heavy) printf '%s\n' "rejected_local_heavy" ;;
    *) printf '%s\n' "malformed" ;;
  esac
}

aggregate_valid_run_category() {
  local run="$1"
  local execution_mode validation_status command_class used_rch residual_risk_notes
  local worker_evidence_confidence

  execution_mode="$(jq -r '.execution_mode // ""' <<<"${run}")"
  validation_status="$(jq -r '.validation_status // ""' <<<"${run}")"
  command_class="$(jq -r '.command_class // ""' <<<"${run}")"
  used_rch="$(jq -r '.used_rch // false' <<<"${run}")"
  residual_risk_notes="$(jq -r '.residual_risk_notes // ""' <<<"${run}")"
  worker_evidence_confidence="$(jq -r '.worker_evidence_confidence // ""' <<<"${run}")"

  if [[ "${execution_mode}" == "approved_local_fallback" || "${validation_status}" == "approved_fallback" ]]; then
    printf '%s\n' "approved_fallback"
  elif [[ -n "${residual_risk_notes}" ]]; then
    printf '%s\n' "residual_risk_only"
  elif [[ "${command_class}" == "heavy" && "${used_rch}" == "true" && "${execution_mode}" == "remote_rch" && "${worker_evidence_confidence}" =~ ^(target_worker_remote_proof|scheduler_selected_remote_proof)$ ]]; then
    printf '%s\n' "proven_remote"
  elif [[ "${command_class}" == "light" && "${execution_mode}" == "local_light" ]]; then
    printf '%s\n' "light_local"
  elif [[ "${used_rch}" == "true" && "${execution_mode}" == "remote_rch" && "${worker_evidence_confidence}" =~ ^(target_worker_remote_proof|scheduler_selected_remote_proof)$ ]]; then
    printf '%s\n' "proven_remote"
  else
    printf '%s\n' "residual_risk_only"
  fi
}

aggregate_worker_evidence_category() {
  local run="$1"
  local confidence source_mirror_status

  confidence="$(jq -r '.worker_evidence_confidence // "legacy_unknown_worker_evidence"' <<<"${run}" 2>/dev/null || printf 'legacy_unknown_worker_evidence')"
  source_mirror_status="$(jq -r '.source_mirror_status // ""' <<<"${run}" 2>/dev/null || printf '')"

  case "${confidence}" in
    target_worker_remote_proof) printf '%s\n' "target_worker_remote" ;;
    scheduler_selected_remote_proof) printf '%s\n' "scheduler_selected_remote" ;;
    target_worker_mirror_attestation)
      if [[ "${source_mirror_status}" == "present" ]]; then
        printf '%s\n' "mirror_attested"
      else
        printf '%s\n' "mirror_failed"
      fi
      ;;
    worker_self_test_only) printf '%s\n' "worker_self_test_only" ;;
    sync_or_transfer_only) printf '%s\n' "sync_or_transfer_only" ;;
    inconclusive_worker_evidence) printf '%s\n' "inconclusive_worker_evidence" ;;
    *) printf '%s\n' "legacy_unknown_worker_evidence" ;;
  esac
}

aggregate_reason_for_valid_category() {
  case "$1" in
    proven_remote) printf '%s\n' "aggregate.proven_remote" ;;
    light_local) printf '%s\n' "aggregate.light_local" ;;
    approved_fallback) printf '%s\n' "aggregate.approved_fallback" ;;
    residual_risk_only) printf '%s\n' "aggregate.residual_risk_only" ;;
    *) printf '%s\n' "aggregate.residual_risk_only" ;;
  esac
}

aggregate_run_entry_json() {
  local ledger_file="$1"
  local line_no="$2"
  local run_index="$3"
  local evidence="$4"
  local category="$5"
  local reason_code="$6"
  local reason_detail="${7:-}"
  local validation_status="${8:-valid}"
  local run bead_id scenario_id command worker_context artifact_paths artifact_path
  local worker_evidence_confidence worker_evidence_category intended_worker_id selected_worker_id
  local worker_queue_state repo_snapshot_head source_mirror_status source_mirror_reason_code
  local remote_cargo_reached remote_rustc_reached test_binary_reached

  run="$(jq -c ".runs[${run_index}]" <<<"${evidence}" 2>/dev/null || printf '{}')"
  bead_id="$(jq -r '.bead_id // "unknown"' <<<"${evidence}" 2>/dev/null || printf 'unknown')"
  scenario_id="$(jq -r '.scenario_id // "unknown"' <<<"${evidence}" 2>/dev/null || printf 'unknown')"
  command="$(jq -r '.command // "unknown"' <<<"${run}" 2>/dev/null || printf 'unknown')"
  worker_context="$(jq -r '.worker_context // "unknown"' <<<"${run}" 2>/dev/null || printf 'unknown')"
  artifact_paths="$(jq -c 'if (.artifact_paths | type) == "array" then .artifact_paths else [] end' <<<"${run}" 2>/dev/null || printf '[]')"
  artifact_path="$(jq -r 'if (.artifact_paths | type) == "array" and (.artifact_paths | length) > 0 then .artifact_paths[0] else "unknown" end' <<<"${run}" 2>/dev/null || printf 'unknown')"
  worker_evidence_confidence="$(jq -r '.worker_evidence_confidence // "legacy_unknown_worker_evidence"' <<<"${run}" 2>/dev/null || printf 'legacy_unknown_worker_evidence')"
  worker_evidence_category="$(aggregate_worker_evidence_category "${run}")"
  intended_worker_id="$(jq -r '.intended_worker_id // ""' <<<"${run}" 2>/dev/null || printf '')"
  selected_worker_id="$(jq -r '.selected_worker_id // ""' <<<"${run}" 2>/dev/null || printf '')"
  worker_queue_state="$(jq -r '.worker_queue_state // ""' <<<"${run}" 2>/dev/null || printf '')"
  repo_snapshot_head="$(jq -r '.repo_snapshot_head // ""' <<<"${run}" 2>/dev/null || printf '')"
  source_mirror_status="$(jq -r '.source_mirror_status // ""' <<<"${run}" 2>/dev/null || printf '')"
  source_mirror_reason_code="$(jq -r '.source_mirror_reason_code // ""' <<<"${run}" 2>/dev/null || printf '')"
  remote_cargo_reached="$(jq -r 'if has("remote_cargo_reached") then .remote_cargo_reached else null end' <<<"${run}" 2>/dev/null || printf 'null')"
  remote_rustc_reached="$(jq -r 'if has("remote_rustc_reached") then .remote_rustc_reached else null end' <<<"${run}" 2>/dev/null || printf 'null')"
  test_binary_reached="$(jq -r 'if has("test_binary_reached") then .test_binary_reached else null end' <<<"${run}" 2>/dev/null || printf 'null')"

  jq -cn \
    --arg ledger_path "${ledger_file}" \
    --argjson line_no "${line_no}" \
    --argjson run_index "${run_index}" \
    --arg bead_id "${bead_id}" \
    --arg scenario_id "${scenario_id}" \
    --arg command "${command}" \
    --arg worker_context "${worker_context}" \
    --arg artifact_path "${artifact_path}" \
    --argjson artifact_paths "${artifact_paths}" \
    --arg category "${category}" \
    --arg reason_code "${reason_code}" \
    --arg reason_detail "${reason_detail}" \
    --arg validation_status "${validation_status}" \
    --arg worker_evidence_confidence "${worker_evidence_confidence}" \
    --arg worker_evidence_category "${worker_evidence_category}" \
    --arg intended_worker_id "${intended_worker_id}" \
    --arg selected_worker_id "${selected_worker_id}" \
    --arg worker_queue_state "${worker_queue_state}" \
    --arg repo_snapshot_head "${repo_snapshot_head}" \
    --arg source_mirror_status "${source_mirror_status}" \
    --arg source_mirror_reason_code "${source_mirror_reason_code}" \
    --argjson remote_cargo_reached "${remote_cargo_reached}" \
    --argjson remote_rustc_reached "${remote_rustc_reached}" \
    --argjson test_binary_reached "${test_binary_reached}" \
    '{
      ledger_path: $ledger_path,
      line_no: $line_no,
      run_index: $run_index,
      bead_id: $bead_id,
      scenario_id: $scenario_id,
      command: $command,
      worker_context: $worker_context,
      artifact_path: $artifact_path,
      artifact_paths: $artifact_paths,
      category: $category,
      reason_code: $reason_code,
      reason_detail: $reason_detail,
      validation_status: $validation_status,
      worker_evidence_confidence: $worker_evidence_confidence,
      worker_evidence_category: $worker_evidence_category,
      intended_worker_id: (if $intended_worker_id == "" then null else $intended_worker_id end),
      selected_worker_id: (if $selected_worker_id == "" then null else $selected_worker_id end),
      worker_queue_state: (if $worker_queue_state == "" then null else $worker_queue_state end),
      repo_snapshot_head: (if $repo_snapshot_head == "" then null else $repo_snapshot_head end),
      source_mirror_status: (if $source_mirror_status == "" then null else $source_mirror_status end),
      source_mirror_reason_code: (if $source_mirror_reason_code == "" then null else $source_mirror_reason_code end),
      remote_cargo_reached: $remote_cargo_reached,
      remote_rustc_reached: $remote_rustc_reached,
      test_binary_reached: $test_binary_reached
    }'
}

aggregate_malformed_line_json() {
  local ledger_file="$1"
  local line_no="$2"
  local reason_detail="$3"

  jq -cn \
    --arg ledger_path "${ledger_file}" \
    --argjson line_no "${line_no}" \
    --arg reason_detail "${reason_detail}" \
    '{
      ledger_path: $ledger_path,
      line_no: $line_no,
      run_index: null,
      bead_id: "unknown",
      scenario_id: "unknown",
      command: "unknown",
      worker_context: "unknown",
      artifact_path: "unknown",
      artifact_paths: [],
      category: "malformed",
      reason_code: "aggregate.malformed",
      reason_detail: $reason_detail,
      validation_status: "invalid"
    }'
}

aggregate_ledger_file() {
  local ledger_file="$1"

  if [[ ! -f "${ledger_file}" ]]; then
    echo "ledger file not found: ${ledger_file}" >&2
    return 1
  fi

  local rows="" line_no=0 line compact runs_count run_index single single_file validation_error
  local category reason_code reason_detail validation_status
  local ledger_display_path="${ledger_file}"
  local validation_dir="${ledger_file}.aggregate-validation"

  mkdir -p "${validation_dir}"

  while IFS= read -r line || [[ -n "${line}" ]]; do
    line_no=$((line_no + 1))
    [[ -n "${line}" ]] || continue

    if ! compact="$(jq -c . <<<"${line}" 2>/dev/null)"; then
      rows+=$(aggregate_malformed_line_json "${ledger_display_path}" "${line_no}" "line is not valid JSON")
      rows+=$'\n'
      continue
    fi

    if ! runs_count="$(jq -r 'if (.runs | type) == "array" then (.runs | length) else -1 end' <<<"${compact}" 2>/dev/null)"; then
      rows+=$(aggregate_malformed_line_json "${ledger_display_path}" "${line_no}" "runs must be an array")
      rows+=$'\n'
      continue
    fi
    if [[ ! "${runs_count}" =~ ^[0-9]+$ || "${runs_count}" -le 0 ]]; then
      rows+=$(aggregate_malformed_line_json "${ledger_display_path}" "${line_no}" "runs must be a non-empty array")
      rows+=$'\n'
      continue
    fi

    for ((run_index = 0; run_index < runs_count; run_index++)); do
      single="$(jq -c --argjson idx "${run_index}" '. as $root | $root + {runs: [$root.runs[$idx]]}' <<<"${compact}")"
      single_file="${validation_dir}/entry_${line_no}_${run_index}.json"
      printf '%s\n' "${single}" >"${single_file}"
      validation_error=""
      if validation_error="$(validate_evidence_file "${single_file}" 2>&1 >/dev/null)"; then
        category="$(aggregate_valid_run_category "$(jq -c '.runs[0]' <<<"${single}")")"
        reason_code="$(aggregate_reason_for_valid_category "${category}")"
        reason_detail=""
        validation_status="valid"
      else
        reason_code="$(aggregate_reason_for_validation_error "${validation_error}")"
        category="$(aggregate_category_for_reason "${reason_code}")"
        reason_detail="$(redact_proof_ledger_text "${validation_error}")"
        validation_status="invalid"
      fi

      rows+=$(aggregate_run_entry_json \
        "${ledger_display_path}" \
        "${line_no}" \
        "${run_index}" \
        "${single}" \
        "${category}" \
        "${reason_code}" \
        "${reason_detail}" \
        "${validation_status}")
      rows+=$'\n'
    done
  done <"${ledger_file}"

  if [[ -z "${rows}" ]]; then
    rows="$(aggregate_malformed_line_json "${ledger_display_path}" 0 "ledger file had no JSONL entries")"$'\n'
  fi

  printf '%s' "${rows}" | jq -s \
    --arg schema_version "1" \
    --arg generated_at "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
    --arg ledger_path "${ledger_file}" \
    --arg validation_dir "${validation_dir}" \
    'def countcat($name): map(select(.category == $name)) | length;
     def countworker($name): map(select(.worker_evidence_category == $name)) | length;
     . as $entries
     | {
        schema_version: ($schema_version | tonumber),
        generated_at: $generated_at,
        ledger_path: $ledger_path,
        ledger_paths: [$ledger_path],
        validation_dir: $validation_dir,
        validation_dirs: [$validation_dir],
        entries: $entries,
        counts: {
          proven_remote: countcat("proven_remote"),
          light_local: countcat("light_local"),
          approved_fallback: countcat("approved_fallback"),
          rejected_local_heavy: countcat("rejected_local_heavy"),
          malformed: countcat("malformed"),
          missing_artifact: countcat("missing_artifact"),
          residual_risk_only: countcat("residual_risk_only")
        },
        worker_evidence_counts: {
          target_worker_remote: countworker("target_worker_remote"),
          scheduler_selected_remote: countworker("scheduler_selected_remote"),
          mirror_attested: countworker("mirror_attested"),
          mirror_failed: countworker("mirror_failed"),
          worker_self_test_only: countworker("worker_self_test_only"),
          sync_or_transfer_only: countworker("sync_or_transfer_only"),
          inconclusive_worker_evidence: countworker("inconclusive_worker_evidence"),
          legacy_unknown_worker_evidence: countworker("legacy_unknown_worker_evidence")
        }
      }
      | .blocking_failure_count = (
          .counts.rejected_local_heavy + .counts.malformed + .counts.missing_artifact
        )
      | .risk_count = (.counts.approved_fallback + .counts.residual_risk_only)
      | .quality_gate_passed = (.blocking_failure_count == 0)
      | .overall_verdict = (
          if .blocking_failure_count > 0 then "failed"
          elif .risk_count > 0 then "partial_risk"
          else "passed"
          end
        )'
}

aggregate_ledger_files() {
  if [[ $# -lt 1 ]]; then
    echo "at least one ledger file is required" >&2
    return 1
  fi

  if [[ $# -eq 1 ]]; then
    aggregate_ledger_file "$1"
    return $?
  fi

  local rows="" ledger_paths_json="[]" validation_dirs_json="[]"
  local ledger_file report validation_dir

  for ledger_file in "$@"; do
    if [[ ! -f "${ledger_file}" ]]; then
      echo "ledger file not found: ${ledger_file}" >&2
      return 1
    fi
    report="$(aggregate_ledger_file "${ledger_file}")"
    rows+="$(jq -c '.entries[]' <<<"${report}")"
    rows+=$'\n'
    ledger_paths_json="$(jq -c --arg path "${ledger_file}" '. + [$path]' <<<"${ledger_paths_json}")"
    validation_dir="$(jq -r '.validation_dir' <<<"${report}")"
    validation_dirs_json="$(jq -c --arg path "${validation_dir}" '. + [$path]' <<<"${validation_dirs_json}")"
  done

  printf '%s' "${rows}" | jq -s \
    --arg schema_version "1" \
    --arg generated_at "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
    --argjson ledger_paths "${ledger_paths_json}" \
    --argjson validation_dirs "${validation_dirs_json}" \
    'def countcat($name): map(select(.category == $name)) | length;
     def countworker($name): map(select(.worker_evidence_category == $name)) | length;
     . as $entries
     | {
        schema_version: ($schema_version | tonumber),
        generated_at: $generated_at,
        ledger_path: null,
        ledger_paths: $ledger_paths,
        validation_dir: null,
        validation_dirs: $validation_dirs,
        entries: $entries,
        counts: {
          proven_remote: countcat("proven_remote"),
          light_local: countcat("light_local"),
          approved_fallback: countcat("approved_fallback"),
          rejected_local_heavy: countcat("rejected_local_heavy"),
          malformed: countcat("malformed"),
          missing_artifact: countcat("missing_artifact"),
          residual_risk_only: countcat("residual_risk_only")
        },
        worker_evidence_counts: {
          target_worker_remote: countworker("target_worker_remote"),
          scheduler_selected_remote: countworker("scheduler_selected_remote"),
          mirror_attested: countworker("mirror_attested"),
          mirror_failed: countworker("mirror_failed"),
          worker_self_test_only: countworker("worker_self_test_only"),
          sync_or_transfer_only: countworker("sync_or_transfer_only"),
          inconclusive_worker_evidence: countworker("inconclusive_worker_evidence"),
          legacy_unknown_worker_evidence: countworker("legacy_unknown_worker_evidence")
        }
      }
      | .blocking_failure_count = (
          .counts.rejected_local_heavy + .counts.malformed + .counts.missing_artifact
        )
      | .risk_count = (.counts.approved_fallback + .counts.residual_risk_only)
      | .quality_gate_passed = (.blocking_failure_count == 0)
      | .overall_verdict = (
          if .blocking_failure_count > 0 then "failed"
          elif .risk_count > 0 then "partial_risk"
          else "passed"
          end
        )'
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

  out="$(classify_command_json "rch exec -- cargo test diagnose --dry-run")"
  [[ "$(jq -r '.is_heavy' <<<"${out}")" == "true" ]] || {
    echo "self-test failed: dry-run words after rch exec must not hide a cargo test" >&2
    return 1
  }
  [[ "$(jq -r '.used_rch' <<<"${out}")" == "true" ]] || {
    echo "self-test failed: dry-run words after rch exec should still count as rch usage" >&2
    return 1
  }
  [[ "$(jq -r '.policy_violation' <<<"${out}")" == "false" ]] || {
    echo "self-test failed: rch exec cargo test with dry-run words should remain policy-compliant" >&2
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

  local tmp_dir tmp_evidence tmp_artifact tmp_artifact_rel
  tmp_dir="${ROOT_DIR}/target/rch-policy-self-test/$(date -u +%Y%m%d_%H%M%S)-$$"
  mkdir -p "${tmp_dir}"
  tmp_artifact="${tmp_dir}/mock.jsonl"
  tmp_artifact_rel="${tmp_artifact#"${ROOT_DIR}"/}"
  printf '{"mock":true}\n' > "${tmp_artifact}"
  tmp_evidence="${tmp_dir}/valid-evidence.json"
  local remote_cmd remote_worker remote_target light_cmd_value light_worker light_target
  local remote_cmd_fp remote_worker_fp remote_target_fp light_cmd_fp light_worker_fp light_target_fp
  local artifact_paths_fp empty_fp remote_repo_snapshot
  remote_cmd="rch exec -- cargo test --workspace"
  remote_worker="worker=mock-1"
  remote_target="/tmp/ft-kvs1e-rch-target"
  light_cmd_value="cargo fmt --check"
  light_worker="local"
  light_target="not_applicable"
  remote_repo_snapshot="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  remote_cmd_fp="$(fingerprint_text "${remote_cmd}")"
  remote_worker_fp="$(fingerprint_text "${remote_worker}")"
  remote_target_fp="$(fingerprint_text "${remote_target}")"
  light_cmd_fp="$(fingerprint_text "${light_cmd_value}")"
  light_worker_fp="$(fingerprint_text "${light_worker}")"
  light_target_fp="$(fingerprint_text "${light_target}")"
  artifact_paths_fp="$(fingerprint_text "$(jq -cn --arg path "${tmp_artifact_rel}" '[$path]')")"
  empty_fp="$(fingerprint_text "")"

  cat > "${tmp_evidence}" <<JSON
{
  "schema_version": ${SCHEMA_VERSION},
  "bead_id": "ft-kvs1e",
  "policy_version": "${POLICY_VERSION}",
  "runs": [
    {
      "timestamp": "2026-02-25T00:00:00Z",
      "command": "${remote_cmd}",
      "command_fingerprint": "${remote_cmd_fp}",
      "command_class": "heavy",
      "is_heavy": true,
      "used_rch": true,
      "worker_context": "${remote_worker}",
      "worker_context_fingerprint": "${remote_worker_fp}",
      "worker_evidence_confidence": "scheduler_selected_remote_proof",
      "intended_worker_id": null,
      "selected_worker_id": "mock-1",
      "worker_queue_state": "ready",
      "repo_snapshot_head": "${remote_repo_snapshot}",
      "source_mirror_status": "present",
      "source_mirror_reason_code": null,
      "remote_cargo_reached": true,
      "remote_rustc_reached": null,
      "test_binary_reached": null,
      "execution_mode": "remote_rch",
      "target_dir": "${remote_target}",
      "target_dir_fingerprint": "${remote_target_fp}",
      "target_dir_lifecycle": "retained",
      "artifact_paths": ["${tmp_artifact_rel}"],
      "artifact_paths_fingerprint": "${artifact_paths_fp}",
      "elapsed_seconds": 12.2,
      "exit_status": 0,
      "residual_risk_notes": "",
      "residual_risk_notes_fingerprint": "${empty_fp}",
      "validation_status": "valid"
    },
    {
      "timestamp": "2026-02-25T00:01:00Z",
      "command": "${light_cmd_value}",
      "command_fingerprint": "${light_cmd_fp}",
      "command_class": "light",
      "is_heavy": false,
      "used_rch": false,
      "worker_context": "${light_worker}",
      "worker_context_fingerprint": "${light_worker_fp}",
      "worker_evidence_confidence": "legacy_unknown_worker_evidence",
      "intended_worker_id": null,
      "selected_worker_id": null,
      "worker_queue_state": "not_applicable",
      "repo_snapshot_head": "not_applicable",
      "source_mirror_status": "not_applicable",
      "source_mirror_reason_code": null,
      "remote_cargo_reached": false,
      "remote_rustc_reached": false,
      "test_binary_reached": false,
      "execution_mode": "local_light",
      "target_dir": "${light_target}",
      "target_dir_fingerprint": "${light_target_fp}",
      "target_dir_lifecycle": "not_applicable",
      "artifact_paths": ["${tmp_artifact_rel}"],
      "artifact_paths_fingerprint": "${artifact_paths_fp}",
      "elapsed_seconds": 0.4,
      "exit_status": 0,
      "residual_risk_notes": "",
      "residual_risk_notes_fingerprint": "${empty_fp}",
      "validation_status": "valid"
    }
  ]
}
JSON

  validate_evidence_file "${tmp_evidence}" >/dev/null

  local redaction_probe redaction_probe_json redaction_probe_text redaction_probe_fingerprint
  redaction_probe="API_KEY=sk-proj-abcdefghijklmnopqrstuvwxyz012345 cargo test -p frankenterm-core --manifest-path crates/frankenterm/Cargo.toml --header 'Authorization: Bearer abcdefghijklmnopqrstuvwxyz012345' --ssh '-----BEGIN OPENSSH PRIVATE KEY-----abc-----END OPENSSH PRIVATE KEY-----' --path /Users/jemanuel/.ssh/id_ed25519 --safe crates/frankenterm"
  redaction_probe_json="$(redact_text_json "${redaction_probe}")"
  redaction_probe_text="$(jq -r '.redacted' <<<"${redaction_probe_json}")"
  redaction_probe_fingerprint="$(jq -r '.fingerprint' <<<"${redaction_probe_json}")"
  [[ "${redaction_probe_text}" == *"cargo test -p frankenterm-core"* ]] || {
    echo "self-test failed: redaction must preserve non-sensitive command structure" >&2
    return 1
  }
  [[ "${redaction_probe_text}" == *"crates/frankenterm"* ]] || {
    echo "self-test failed: redaction must preserve innocuous repo-relative paths" >&2
    return 1
  }
  [[ "${redaction_probe_text}" == *"[REDACTED]"* && "${redaction_probe_text}" == *"[REDACTED:filesystem_path]"* ]] || {
    echo "self-test failed: redaction must mark secrets and sensitive filesystem paths" >&2
    return 1
  }
  [[ "${redaction_probe_text}" != *"sk-proj-"* && "${redaction_probe_text}" != *"Bearer abcdef"* && "${redaction_probe_text}" != *"/Users/jemanuel"* ]] || {
    echo "self-test failed: redaction leaked a fixture secret" >&2
    return 1
  }
  fingerprint_is_valid "${redaction_probe_fingerprint}" || {
    echo "self-test failed: redaction helper must emit a stable sha256 fingerprint" >&2
    return 1
  }

  local tmp_fail_open tmp_fail_open_recovered tmp_sync_chatter tmp_shell_wrapper
  local tmp_missing_artifact tmp_missing_is_heavy tmp_secret_command tmp_secret_path
  local tmp_malformed_bead tmp_stale_schema tmp_missing_worker_id tmp_missing_repo_snapshot
  local tmp_bad_target_mirror_status tmp_mirror_failed_attestation
  tmp_fail_open="${tmp_dir}/fail-open.json"
  tmp_fail_open_recovered="${tmp_dir}/fail-open-recovered.json"
  tmp_sync_chatter="${tmp_dir}/sync-chatter.json"
  tmp_shell_wrapper="${tmp_dir}/shell-wrapper.json"
  tmp_missing_artifact="${tmp_dir}/missing-artifact.json"
  tmp_missing_is_heavy="${tmp_dir}/missing-is-heavy.json"
  tmp_secret_command="${tmp_dir}/secret-command.json"
  tmp_secret_path="${tmp_dir}/secret-path.json"
  tmp_malformed_bead="${tmp_dir}/malformed-bead.json"
  tmp_stale_schema="${tmp_dir}/stale-schema.json"
  tmp_missing_worker_id="${tmp_dir}/missing-worker-id.json"
  tmp_missing_repo_snapshot="${tmp_dir}/missing-repo-snapshot.json"
  tmp_bad_target_mirror_status="${tmp_dir}/bad-target-mirror-status.json"
  tmp_mirror_failed_attestation="${tmp_dir}/mirror-failed-attestation.json"

  local local_fallback_worker local_fallback_worker_fp
  local_fallback_worker="local_fallback"
  local_fallback_worker_fp="$(fingerprint_text "${local_fallback_worker}")"
  jq --arg worker "${local_fallback_worker}" \
    --arg worker_fp "${local_fallback_worker_fp}" \
    '.runs[0].worker_context = $worker |
      .runs[0].worker_context_fingerprint = $worker_fp |
      .runs[0].execution_mode = "remote_rch"' \
    "${tmp_evidence}" > "${tmp_fail_open}"

  if validate_evidence_file "${tmp_fail_open}" >/dev/null 2>&1; then
    echo "self-test failed: heavy local execution after rch wrapper must require fallback metadata" >&2
    return 1
  fi

  jq '.runs[0].fallback_reason_code = "RCH-LOCAL-FALLBACK" |
      .runs[0].fallback_approved_by = "human-operator" |
      .runs[0].execution_mode = "approved_local_fallback" |
      .runs[0].validation_status = "approved_fallback" |
      .runs[0].worker_evidence_confidence = "inconclusive_worker_evidence" |
      .runs[0].selected_worker_id = null |
      .runs[0].worker_queue_state = "unsupported_worker_selection" |
      .runs[0].source_mirror_status = "not_checked" |
      .runs[0].remote_cargo_reached = false |
      .runs[0].remote_rustc_reached = false |
      .runs[0].test_binary_reached = false' \
    "${tmp_fail_open}" > "${tmp_fail_open_recovered}"
  validate_evidence_file "${tmp_fail_open_recovered}" >/dev/null

  local sync_chatter_cmd sync_chatter_cmd_fp
  sync_chatter_cmd="rch status && cargo test --workspace"
  sync_chatter_cmd_fp="$(fingerprint_text "${sync_chatter_cmd}")"
  jq --arg cmd "${sync_chatter_cmd}" \
    --arg cmd_fp "${sync_chatter_cmd_fp}" \
    '.runs[0].command = $cmd |
      .runs[0].command_fingerprint = $cmd_fp |
      .runs[0].used_rch = true |
      .runs[0].execution_mode = "remote_rch"' \
    "${tmp_evidence}" > "${tmp_sync_chatter}"
  if validate_evidence_file "${tmp_sync_chatter}" >/dev/null 2>&1; then
    echo "self-test failed: RCH setup chatter must not count as remote cargo proof" >&2
    return 1
  fi

  local shell_wrapper_cmd shell_wrapper_cmd_fp
  shell_wrapper_cmd="bash -lc 'echo rch exec -- cargo test; cargo test --workspace'"
  shell_wrapper_cmd_fp="$(fingerprint_text "${shell_wrapper_cmd}")"
  jq --arg cmd "${shell_wrapper_cmd}" \
    --arg cmd_fp "${shell_wrapper_cmd_fp}" \
    '.runs[0].command = $cmd |
      .runs[0].command_fingerprint = $cmd_fp |
      .runs[0].used_rch = true |
      .runs[0].execution_mode = "remote_rch"' \
    "${tmp_evidence}" > "${tmp_shell_wrapper}"
  if validate_evidence_file "${tmp_shell_wrapper}" >/dev/null 2>&1; then
    echo "self-test failed: shell wrapper that only mentions RCH must not validate as RCH proof" >&2
    return 1
  fi

  local missing_artifact_path missing_artifact_fp
  missing_artifact_path="${tmp_artifact_rel%/*}/missing.jsonl"
  missing_artifact_fp="$(fingerprint_text "$(jq -cn --arg path "${missing_artifact_path}" '[$path]')")"
  jq --arg missing "${missing_artifact_path}" \
    --arg artifact_fp "${missing_artifact_fp}" \
    '.runs[0].artifact_paths = [$missing] |
      .runs[0].artifact_paths_fingerprint = $artifact_fp' \
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

  local secret_cmd secret_cmd_fp secret_path secret_path_fp
  secret_cmd="API_KEY=sk-proj-abcdefghijklmnopqrstuvwxyz rch exec -- cargo test --workspace"
  secret_cmd_fp="$(fingerprint_text "${secret_cmd}")"
  jq --arg cmd "${secret_cmd}" \
    --arg cmd_fp "${secret_cmd_fp}" \
    '.runs[0].command = $cmd |
      .runs[0].command_fingerprint = $cmd_fp' \
    "${tmp_evidence}" > "${tmp_secret_command}"
  local secret_error
  if secret_error="$(validate_evidence_file "${tmp_secret_command}" 2>&1 >/dev/null)"; then
    echo "self-test failed: unredacted secret-bearing command must be rejected" >&2
    return 1
  fi
  [[ "${secret_error}" != *"sk-proj-"* ]] || {
    echo "self-test failed: validator error leaked the raw command secret" >&2
    return 1
  }

  secret_path="${tmp_dir}/.ssh/id_ed25519"
  secret_path_fp="$(fingerprint_text "${secret_path}")"
  jq --arg path "${secret_path}" \
    --arg path_fp "${secret_path_fp}" \
    '.runs[0].target_dir = $path |
      .runs[0].target_dir_fingerprint = $path_fp' \
    "${tmp_evidence}" > "${tmp_secret_path}"
  if validate_evidence_file "${tmp_secret_path}" >/dev/null 2>&1; then
    echo "self-test failed: SSH-style secret path must be rejected" >&2
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

  jq '.runs[0].worker_evidence_confidence = "target_worker_remote_proof" |
      .runs[0].intended_worker_id = "mock-1" |
      .runs[0].selected_worker_id = null' \
    "${tmp_evidence}" > "${tmp_missing_worker_id}"
  if validate_evidence_file "${tmp_missing_worker_id}" >/dev/null 2>&1; then
    echo "self-test failed: target-worker proof without selected worker id must be rejected" >&2
    return 1
  fi

  jq '.runs[0].worker_evidence_confidence = "scheduler_selected_remote_proof" |
      .runs[0].repo_snapshot_head = null' \
    "${tmp_evidence}" > "${tmp_missing_repo_snapshot}"
  if validate_evidence_file "${tmp_missing_repo_snapshot}" >/dev/null 2>&1; then
    echo "self-test failed: scheduler-selected proof without repo snapshot must be rejected" >&2
    return 1
  fi

  jq '.runs[0].worker_evidence_confidence = "target_worker_remote_proof" |
      .runs[0].intended_worker_id = "mock-1" |
      .runs[0].selected_worker_id = "mock-1" |
      .runs[0].source_mirror_status = "missing" |
      .runs[0].source_mirror_reason_code = "rch_mirror.missing_tracked_file"' \
    "${tmp_evidence}" > "${tmp_bad_target_mirror_status}"
  if validate_evidence_file "${tmp_bad_target_mirror_status}" >/dev/null 2>&1; then
    echo "self-test failed: target-worker remote proof with missing source mirror must be rejected" >&2
    return 1
  fi

  local mirror_cmd mirror_cmd_fp mirror_worker mirror_worker_fp mirror_residual mirror_residual_fp
  mirror_cmd="bash scripts/attest_rch_worker_mirror.sh --worker mock-1 --workspace-member-roots --path Cargo.toml --json > ${tmp_artifact_rel}"
  mirror_cmd_fp="$(fingerprint_text "${mirror_cmd}")"
  mirror_worker="worker=mock-1"
  mirror_worker_fp="$(fingerprint_text "${mirror_worker}")"
  mirror_residual="mirror attestation showed the named worker is missing a tracked file; no material Cargo proof ran"
  mirror_residual_fp="$(fingerprint_text "${mirror_residual}")"
  jq --arg cmd "${mirror_cmd}" \
    --arg cmd_fp "${mirror_cmd_fp}" \
    --arg worker "${mirror_worker}" \
    --arg worker_fp "${mirror_worker_fp}" \
    --arg target "${light_target}" \
    --arg target_fp "${light_target_fp}" \
    --arg residual "${mirror_residual}" \
    --arg residual_fp "${mirror_residual_fp}" \
    '.runs[0].command = $cmd |
      .runs[0].command_fingerprint = $cmd_fp |
      .runs[0].command_class = "light" |
      .runs[0].is_heavy = false |
      .runs[0].used_rch = false |
      .runs[0].worker_context = $worker |
      .runs[0].worker_context_fingerprint = $worker_fp |
      .runs[0].execution_mode = "local_light" |
      .runs[0].target_dir = $target |
      .runs[0].target_dir_fingerprint = $target_fp |
      .runs[0].target_dir_lifecycle = "not_applicable" |
      .runs[0].residual_risk_notes = $residual |
      .runs[0].residual_risk_notes_fingerprint = $residual_fp |
      .runs[0].worker_evidence_confidence = "target_worker_mirror_attestation" |
      .runs[0].intended_worker_id = "mock-1" |
      .runs[0].selected_worker_id = "mock-1" |
      .runs[0].worker_queue_state = "not_applicable" |
      .runs[0].repo_snapshot_head = "unknown" |
      .runs[0].source_mirror_status = "missing" |
      .runs[0].source_mirror_reason_code = "rch_mirror.missing_tracked_file" |
      .runs[0].remote_cargo_reached = false |
      .runs[0].remote_rustc_reached = false |
      .runs[0].test_binary_reached = false' \
    "${tmp_evidence}" > "${tmp_mirror_failed_attestation}"
  validate_evidence_file "${tmp_mirror_failed_attestation}" >/dev/null

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
  --redact-text)
    shift
    if [[ $# -ne 1 ]]; then
      usage
      exit 1
    fi
    redact_text_json "$1"
    ;;
  --validate-evidence)
    shift
    if [[ $# -ne 1 ]]; then
      usage
      exit 1
    fi
    validate_evidence_file "$1"
    ;;
  --aggregate-ledger)
    shift
    if [[ $# -lt 1 ]]; then
      usage
      exit 1
    fi
    aggregate_report="$(aggregate_ledger_files "$@")"
    printf '%s\n' "${aggregate_report}"
    if [[ "$(jq -r '.quality_gate_passed' <<<"${aggregate_report}")" != "true" ]]; then
      exit 1
    fi
    ;;
  --self-test)
    run_self_test
    ;;
  *)
    usage
    exit 1
    ;;
esac
