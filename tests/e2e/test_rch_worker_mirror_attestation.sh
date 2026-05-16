#!/usr/bin/env bash
# Static fixtures for selected-worker RCH source mirror attestation.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SCRIPT="${ROOT_DIR}/scripts/attest_rch_worker_mirror.sh"
RUN_ID="${RUN_ID:-$(date -u +%Y%m%d_%H%M%S)}"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs/rch_worker_mirror_attestation_${RUN_ID}"
mkdir -p "${LOG_DIR}"

TOTAL=0
PASS=0
FAIL=0

record_result() {
    local name="$1"
    local ok="$2"
    local detail="${3:-}"

    TOTAL=$((TOTAL + 1))
    if [[ "${ok}" == "true" ]]; then
        PASS=$((PASS + 1))
        printf 'PASS %s\n' "${name}"
    else
        FAIL=$((FAIL + 1))
        printf 'FAIL %s %s\n' "${name}" "${detail}"
    fi
}

fixture_sha256() {
    local path="$1"
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 -- "${path}" | awk '{print $1}'
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum -- "${path}" | awk '{print $1}'
    else
        printf 'missing-hash-tool\n'
    fi
}

HEAD_SHA="$(git -C "${ROOT_DIR}" rev-parse HEAD)"
CARGO_TOML_SHA="$(fixture_sha256 "${ROOT_DIR}/Cargo.toml")"
CORE_LIB_SHA="$(fixture_sha256 "${ROOT_DIR}/crates/frankenterm-core/src/lib.rs")"
WORKERS_JSON="${LOG_DIR}/workers.json"
FAKE_SSH="${LOG_DIR}/fake_ssh.sh"

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${ROOT_DIR}/tests/e2e/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "fixture" "rch_worker_mirror_attestation" "${ROOT_DIR}"

cat >"${WORKERS_JSON}" <<JSON
{
  "success": true,
  "data": [
    {
      "id": "worker-a",
      "host": "203.0.113.10",
      "user": "ubuntu",
      "identity_file": "${LOG_DIR}/fixture_key"
    }
  ]
}
JSON

cat >"${FAKE_SSH}" <<'SH'
#!/usr/bin/env bash
set -euo pipefail

fixture_sha256() {
    local path="$1"
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 -- "${path}" | awk '{print $1}'
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum -- "${path}" | awk '{print $1}'
    else
        printf '\n'
    fi
}

requested_paths() {
    local command_text payload_b64 payload
    command_text="${!#}"
    payload_b64="$(printf '%s\n' "${command_text}" | sed -n "s/^.*-- '\([^']*\)'.*$/\1/p")"
    [[ -n "${payload_b64}" ]] || return 0
    payload="$(printf '%s' "${payload_b64}" | base64 -d 2>/dev/null || true)"
    while IFS=$'\t' read -r kind value _hash; do
        [[ "${kind}" == "F" && -n "${value}" ]] || continue
        printf '%s\n' "${value}"
    done <<<"${payload}"
}

emit_files() {
    local mode="$1"
    shift
    local path full_path hash
    while IFS= read -r path; do
        case "${mode}:${path}" in
          missing_file:crates/frankenterm-core/src/lib.rs)
            printf 'FILE\t%s\tmissing\t\n' "${path}"
            continue
            ;;
          missing_workspace_member:crates/frankenterm-core-replay-types/src/lib.rs)
            printf 'FILE\t%s\tmissing\t\n' "${path}"
            continue
            ;;
          hash_mismatch:crates/frankenterm-core/src/lib.rs)
            printf 'FILE\t%s\tpresent\t0000000000000000000000000000000000000000000000000000000000000000\n' "${path}"
            continue
            ;;
        esac

        full_path="${FAKE_RCH_MIRROR_REPO_ROOT}/${path}"
        if [[ -f "${full_path}" ]]; then
            hash="$(fixture_sha256 "${full_path}")"
            printf 'FILE\t%s\tpresent\t%s\n' "${path}" "${hash}"
        else
            printf 'FILE\t%s\tmissing\t\n' "${path}"
        fi
    done < <(requested_paths "$@")
}

mode="${FAKE_RCH_MIRROR_MODE:-success}"
case " $* " in
  *"203.0.113.11"*)
    mode="${FAKE_RCH_MIRROR_MODE_WORKER_B:-${mode}}"
    ;;
esac

case "${mode}" in
  unreachable)
    printf 'fixture ssh unreachable\n' >&2
    exit 255
    ;;
  project_absent)
    printf 'STATUS\tproject_path_absent\n'
    ;;
  stale_head)
    printf 'STATUS\tok\n'
    printf 'ROOT\t/data/projects/frankenterm\n'
    printf 'HEAD\t%s\n' "${FAKE_RCH_MIRROR_REMOTE_HEAD:-0000000000000000000000000000000000000000}"
    emit_files stale_head "$@"
    ;;
  head_unavailable)
    printf 'STATUS\tok\n'
    printf 'ROOT\t/data/projects/frankenterm\n'
    printf 'HEAD\t\n'
    emit_files head_unavailable "$@"
    ;;
  hash_mismatch)
    printf 'STATUS\tok\n'
    printf 'ROOT\t/data/projects/frankenterm\n'
    printf 'HEAD\t%s\n' "${FAKE_RCH_MIRROR_REMOTE_HEAD}"
    emit_files hash_mismatch "$@"
    ;;
  missing_file)
    printf 'STATUS\tok\n'
    printf 'ROOT\t/data/projects/frankenterm\n'
    printf 'HEAD\t%s\n' "${FAKE_RCH_MIRROR_REMOTE_HEAD}"
    emit_files missing_file "$@"
    ;;
  missing_workspace_member)
    printf 'STATUS\tok\n'
    printf 'ROOT\t/data/projects/frankenterm\n'
    printf 'HEAD\t%s\n' "${FAKE_RCH_MIRROR_REMOTE_HEAD}"
    emit_files missing_workspace_member "$@"
    ;;
  success)
    printf 'STATUS\tok\n'
    printf 'ROOT\t/data/projects/frankenterm\n'
    printf 'HEAD\t%s\n' "${FAKE_RCH_MIRROR_REMOTE_HEAD}"
    emit_files success "$@"
    ;;
  *)
    printf 'unknown fake mode: %s\n' "${mode}" >&2
    exit 2
    ;;
esac
SH
chmod +x "${FAKE_SSH}"

run_fixture() {
    local mode="$1"
    local output_file="$2"
    local remote_head="${FAKE_RCH_MIRROR_REMOTE_HEAD:-${HEAD_SHA}}"
    shift 2

    set +e
    FAKE_RCH_MIRROR_MODE="${mode}" \
    FAKE_RCH_MIRROR_REMOTE_HEAD="${remote_head}" \
    FAKE_RCH_MIRROR_CARGO_TOML_SHA="${CARGO_TOML_SHA}" \
    FAKE_RCH_MIRROR_CORE_LIB_SHA="${CORE_LIB_SHA}" \
    FAKE_RCH_MIRROR_REPO_ROOT="${ROOT_DIR}" \
    RCH_MIRROR_ATTEST_WORKERS_JSON="${WORKERS_JSON}" \
    RCH_MIRROR_ATTEST_SSH_BIN="${FAKE_SSH}" \
    "${SCRIPT}" \
      --worker worker-a \
      --bead ft-5hkdw \
      --remote-project-root /data/projects/frankenterm \
      --path Cargo.toml \
      --path crates/frankenterm-core/src/lib.rs \
      --json \
      "$@" >"${output_file}"
    local rc=$?
    set -e
    return "${rc}"
}

SUCCESS_JSON="${LOG_DIR}/success.json"
if run_fixture success "${SUCCESS_JSON}"; then
    if jq -e '
        .status == "passed"
        and .reason_code == "rch_mirror.ok"
        and .confidence == "target_worker_mirror_attestation"
        and .checks.compiler_or_test_executed == false
        and .checks.scheduler_queue_checked == false
        and (.remote.required_files | all(.remote_status == "present" and .hash_matches == true))
    ' "${SUCCESS_JSON}" >/dev/null; then
        record_result "success_fixture" "true"
    else
        record_result "success_fixture" "false" "unexpected JSON shape"
    fi
else
    record_result "success_fixture" "false" "script failed"
fi

WORKSPACE_ROOTS_JSON="${LOG_DIR}/workspace_member_roots.json"
if run_fixture success "${WORKSPACE_ROOTS_JSON}" --workspace-member-roots; then
    if jq -e '
        .status == "passed"
        and .reason_code == "rch_mirror.ok"
        and ([.remote.required_files[].path] | index("crates/frankenterm-core-replay-types/Cargo.toml") != null)
        and ([.remote.required_files[].path] | index("crates/frankenterm-core-replay-types/src/lib.rs") != null)
        and ([.remote.required_files[].path] | index("crates/frankenterm-topo/Cargo.toml") != null)
        and ([.remote.required_files[].path] | index("crates/frankenterm-topo/src/lib.rs") != null)
        and ([.remote.required_files[].path] | index("frankenterm/config/derive/Cargo.toml") != null)
        and (.remote.required_files | length > 50)
        and (.remote.required_files | all(.remote_status == "present" and .hash_matches == true))
    ' "${WORKSPACE_ROOTS_JSON}" >/dev/null; then
        record_result "workspace_member_roots_fixture" "true"
    else
        record_result "workspace_member_roots_fixture" "false" "unexpected JSON shape"
    fi
else
    record_result "workspace_member_roots_fixture" "false" "script failed"
fi

WORKSPACE_MISSING_JSON="${LOG_DIR}/workspace_member_missing.json"
if run_fixture missing_workspace_member "${WORKSPACE_MISSING_JSON}" --workspace-member-roots; then
    record_result "workspace_member_missing_fixture" "false" "script unexpectedly passed"
else
    if jq -e '
        .status == "failed"
        and .reason_code == "rch_mirror.missing_tracked_file"
        and .failure_domain == "source_mirror"
        and (.remote.required_files[] | select(.path == "crates/frankenterm-core-replay-types/src/lib.rs") | .remote_status == "missing")
    ' "${WORKSPACE_MISSING_JSON}" >/dev/null; then
        record_result "workspace_member_missing_fixture" "true"
    else
        record_result "workspace_member_missing_fixture" "false" "unexpected JSON shape"
    fi
fi

MISSING_JSON="${LOG_DIR}/missing_file.json"
if run_fixture missing_file "${MISSING_JSON}"; then
    record_result "missing_file_fixture" "false" "script unexpectedly passed"
else
    if jq -e '
        .status == "failed"
        and .reason_code == "rch_mirror.missing_tracked_file"
        and .failure_domain == "source_mirror"
        and (.remote.required_files[] | select(.path == "crates/frankenterm-core/src/lib.rs") | .remote_status == "missing")
    ' "${MISSING_JSON}" >/dev/null; then
        record_result "missing_file_fixture" "true"
    else
        record_result "missing_file_fixture" "false" "unexpected JSON shape"
    fi
fi

STALE_JSON="${LOG_DIR}/stale_head.json"
if FAKE_RCH_MIRROR_REMOTE_HEAD="0000000000000000000000000000000000000000" run_fixture stale_head "${STALE_JSON}"; then
    if jq -e '
        .status == "passed"
        and .reason_code == "rch_mirror.required_files_ok_head_mismatch"
        and .failure_domain == "none"
        and .remote.head_matches == false
        and (.remote.required_files | all(.hash_matches == true))
    ' "${STALE_JSON}" >/dev/null; then
        record_result "stale_head_fixture" "true"
    else
        record_result "stale_head_fixture" "false" "unexpected JSON shape"
    fi
else
    record_result "stale_head_fixture" "false" "script failed despite matching required-file hashes"
fi

HEAD_UNAVAILABLE_JSON="${LOG_DIR}/head_unavailable.json"
if run_fixture head_unavailable "${HEAD_UNAVAILABLE_JSON}"; then
    if jq -e '
        .status == "passed"
        and .reason_code == "rch_mirror.required_files_ok_head_unavailable"
        and .failure_domain == "none"
        and .checks.head_available == false
        and (.remote.required_files | all(.hash_matches == true))
    ' "${HEAD_UNAVAILABLE_JSON}" >/dev/null; then
        record_result "head_unavailable_fixture" "true"
    else
        record_result "head_unavailable_fixture" "false" "unexpected JSON shape"
    fi
else
    record_result "head_unavailable_fixture" "false" "script failed despite matching required-file hashes"
fi

HASH_MISMATCH_JSON="${LOG_DIR}/hash_mismatch.json"
if run_fixture hash_mismatch "${HASH_MISMATCH_JSON}"; then
    record_result "hash_mismatch_fixture" "false" "script unexpectedly passed"
else
    if jq -e '
        .status == "failed"
        and .reason_code == "rch_mirror.tracked_file_hash_mismatch"
        and .failure_domain == "source_mirror"
        and (.remote.required_files[] | select(.path == "crates/frankenterm-core/src/lib.rs") | .hash_matches == false)
    ' "${HASH_MISMATCH_JSON}" >/dev/null; then
        record_result "hash_mismatch_fixture" "true"
    else
        record_result "hash_mismatch_fixture" "false" "unexpected JSON shape"
    fi
fi

ABSENT_JSON="${LOG_DIR}/project_absent.json"
if run_fixture project_absent "${ABSENT_JSON}"; then
    record_result "project_absent_fixture" "false" "script unexpectedly passed"
else
    if jq -e '
        .status == "failed"
        and .reason_code == "rch_mirror.project_path_absent"
        and .failure_domain == "source_mirror"
        and .checks.project_path_present == false
    ' "${ABSENT_JSON}" >/dev/null; then
        record_result "project_absent_fixture" "true"
    else
        record_result "project_absent_fixture" "false" "unexpected JSON shape"
    fi
fi

UNREACHABLE_JSON="${LOG_DIR}/unreachable.json"
if run_fixture unreachable "${UNREACHABLE_JSON}"; then
    record_result "unreachable_worker_fixture" "false" "script unexpectedly passed"
else
    if jq -e '
        .status == "failed"
        and .reason_code == "rch_mirror.worker_unreachable"
        and .failure_domain == "selected_worker"
        and .checks.worker_reachable == false
    ' "${UNREACHABLE_JSON}" >/dev/null; then
        record_result "unreachable_worker_fixture" "true"
    else
        record_result "unreachable_worker_fixture" "false" "unexpected JSON shape"
    fi
fi

UNTRACKED_JSON="${LOG_DIR}/untracked_path.json"
if run_fixture success "${UNTRACKED_JSON}" --path docs/this-file-is-not-tracked-for-attestation-fixture.txt; then
    record_result "untracked_required_path_fixture" "false" "script unexpectedly passed"
else
    if jq -e '
        .status == "failed"
        and .reason_code == "rch_mirror.untracked_required_file"
        and .failure_domain == "input"
        and .confidence == "inconclusive_worker_evidence"
    ' "${UNTRACKED_JSON}" >/dev/null; then
        record_result "untracked_required_path_fixture" "true"
    else
        record_result "untracked_required_path_fixture" "false" "unexpected JSON shape"
    fi
fi

write_preflight_probe_fixture() {
    local output_file="$1"
    local status="$2"

    cat >"${output_file}" <<JSON
{
  "success": true,
  "data": {
    "results": [
      {
        "id": "worker-a",
        "host": "203.0.113.10",
        "status": "${status}",
        "latency_ms": 12
      }
    ],
    "summary": {
      "total": 1,
      "healthy": 1,
      "unhealthy": 0,
      "failed": 0
    }
  }
}
JSON
}

write_preflight_queue_fixture() {
    local output_file="$1"
    local mode="$2"

    case "${mode}" in
      ready)
        cat >"${output_file}" <<'JSON'
{
  "success": true,
  "data": {
    "queue_depth": 0,
    "queued_builds": [],
    "active_builds": [],
    "slots_available": 4,
    "slots_total": 8,
    "workers_available": 1,
    "workers_busy": 0,
    "workers_healthy": 1,
    "workers_offline": 0,
    "workers_total": 1
  }
}
JSON
        ;;
      busy)
        cat >"${output_file}" <<'JSON'
{
  "success": true,
  "data": {
    "queue_depth": 2,
    "queued_builds": [{"id": 1}, {"id": 2}],
    "active_builds": [{"id": 3}],
    "slots_available": 0,
    "slots_total": 8,
    "workers_available": 0,
    "workers_busy": 1,
    "workers_healthy": 1,
    "workers_offline": 0,
    "workers_total": 1
  }
}
JSON
        ;;
      unhealthy)
        cat >"${output_file}" <<'JSON'
{
  "success": true,
  "data": {
    "queue_depth": 0,
    "queued_builds": [],
    "active_builds": [],
    "slots_available": 0,
    "slots_total": 8,
    "workers_available": 0,
    "workers_busy": 0,
    "workers_healthy": 0,
    "workers_offline": 1,
    "workers_total": 1
  }
}
JSON
        ;;
      unsupported)
        printf '%s\n' 'not-json' >"${output_file}"
        ;;
      *)
        printf 'unknown preflight queue fixture mode: %s\n' "${mode}" >&2
        return 2
        ;;
    esac
}

run_preflight_fixture() {
    local name="$1"
    local probe_status="$2"
    local queue_mode="$3"
    local remote_required="$4"
    local expected_status="$5"
    local expected_reason="$6"
    local expected_queue_state="$7"
    local probe_file="${LOG_DIR}/${name}_probe.json"
    local queue_file="${LOG_DIR}/${name}_queue.json"
    local output_file="${LOG_DIR}/${name}_preflight.json"
    local queue_rc=0

    write_preflight_probe_fixture "${probe_file}" "${probe_status}"
    write_preflight_queue_fixture "${queue_file}" "${queue_mode}"
    if [[ "${queue_mode}" == "unsupported" ]]; then
        queue_rc=2
    fi

    RCH_REQUIRE_REMOTE="${remote_required}" \
      rch_write_remote_preflight_json "${probe_file}" "${queue_file}" "${queue_rc}" "${output_file}"

    if jq -e \
        --arg status "${expected_status}" \
        --arg reason "${expected_reason}" \
        --arg queue_state "${expected_queue_state}" \
        '.status == $status
         and .reason_code == $reason
         and .worker_queue_state == $queue_state
         and .checks.local_fallback_allowed == false
         and .checks.heavy_cargo_started == false' \
        "${output_file}" >/dev/null; then
        record_result "${name}" "true"
    else
        record_result "${name}" "false" "unexpected preflight JSON shape"
    fi
}

run_preflight_fixture "remote_preflight_ready_fixture" "ok" "ready" "1" \
    "passed" "remote_ready" "ready"
run_preflight_fixture "remote_preflight_busy_fixture" "ok" "busy" "1" \
    "blocked" "remote_busy_wait" "busy_wait"
run_preflight_fixture "remote_preflight_unhealthy_fixture" "failed" "unhealthy" "1" \
    "blocked" "no_healthy_workers" "unhealthy"
run_preflight_fixture "remote_preflight_unsupported_fixture" "ok" "unsupported" "1" \
    "warning" "unsupported_worker_selection" "unsupported_worker_selection"
run_preflight_fixture "remote_preflight_local_fallback_fixture" "ok" "ready" "0" \
    "blocked" "local_fallback_forbidden" "unsupported_worker_selection"

POOL_WORKERS_JSON="${LOG_DIR}/pool_workers.json"
cat >"${POOL_WORKERS_JSON}" <<JSON
{
  "success": true,
  "data": [
    {
      "id": "worker-a",
      "host": "203.0.113.10",
      "user": "ubuntu",
      "identity_file": "${LOG_DIR}/fixture_key"
    },
    {
      "id": "worker-b",
      "host": "203.0.113.11",
      "user": "ubuntu",
      "identity_file": "${LOG_DIR}/fixture_key"
    }
  ]
}
JSON

write_pool_probe_fixture() {
    local output_file="$1"

    cat >"${output_file}" <<'JSON'
{
  "success": true,
  "data": {
    "results": [
      {
        "id": "worker-a",
        "host": "203.0.113.10",
        "status": "ok"
      },
      {
        "id": "worker-b",
        "host": "203.0.113.11",
        "status": "ok"
      }
    ]
  }
}
JSON
}

write_pool_scheduler_fixture() {
    local output_file="$1"

    cat >"${output_file}" <<'JSON'
{
  "success": true,
  "data": {
    "daemon": {
      "workers": [
        {
          "id": "worker-a",
          "status": "healthy"
        },
        {
          "id": "worker-b",
          "status": "healthy"
        }
      ]
    }
  }
}
JSON
}

run_rch() {
    if [[ "$*" == "--json status --workers" ]]; then
        cat "${FAKE_RCH_STATUS_JSON}"
        return 0
    fi

    printf 'unexpected fake run_rch invocation: %s\n' "$*" >&2
    return 2
}

FAKE_DIAGNOSE_WORKER="worker-b"
FAKE_SELECTED_WORKER_SYNC_RC=0
run_rch_logged_with_timeout() {
    local timeout_secs="$1"
    local output_file="$2"
    shift 2
    case "$*" in
      *"--json status --workers"*)
        cat "${FAKE_RCH_STATUS_JSON}" >"${output_file}"
        return 0
        ;;
      *"--json diagnose"*)
        jq -cn \
          --arg worker "${FAKE_DIAGNOSE_WORKER}" \
          --arg timeout_secs "${timeout_secs}" \
          '{
            success: true,
            data: {
              decision: { would_intercept: true },
              worker_selection: {
                worker: { id: $worker },
                reason: "fixture_selected_worker"
              },
              timeout_secs: ($timeout_secs | tonumber)
            }
        }' >"${output_file}"
        return 0
        ;;
      *"exec -- env CARGO_TARGET_DIR="*" cargo check --help"*)
        if [[ "${FAKE_SELECTED_WORKER_SYNC_RC}" -ne 0 ]]; then
          printf 'selected-worker source materialization smoke failed\n' >"${output_file}"
          return "${FAKE_SELECTED_WORKER_SYNC_RC}"
        fi
        {
          printf '[RCH] remote %s: selected-worker source materialization\n' "${FAKE_DIAGNOSE_WORKER}"
          printf 'Sync complete: fixture\n'
          printf 'selected-worker source materialized in /data/projects/frankenterm\n'
          printf 'Remote command finished: exit=0 in 1ms\n'
        } >"${output_file}"
        return 0
        ;;
      *)
        printf 'unexpected fake run_rch_logged_with_timeout invocation: %s\n' "$*" >&2
        return 2
        ;;
    esac
}

run_pool_mirror_fixture() {
    local name="$1"
    local mode_a="$2"
    local mode_b="$3"
    local min_passing="$4"
    local require_all_checked="$5"
    local block_on_stale="$6"
    local expected_rc="$7"
    local expected_status="$8"
    local expected_reason="$9"
    local probe_file="${LOG_DIR}/${name}_probe.json"
    local scheduler_file="${LOG_DIR}/${name}_scheduler.json"
    local output_file="${LOG_DIR}/${name}_mirror_preflight.json"
    local remote_head="${HEAD_SHA}"
    local rc

    if [[ "${mode_a}" == "stale_head" || "${mode_b}" == "stale_head" ]]; then
        remote_head="0000000000000000000000000000000000000000"
    fi

    write_pool_probe_fixture "${probe_file}"
    write_pool_scheduler_fixture "${scheduler_file}"

    _RCH_PROBE_LOG="${probe_file}"
    _RCH_MIRROR_PREFLIGHT_LOG="${output_file}"
    _RCH_SCHEDULER_WORKERS_LOG="${LOG_DIR}/${name}_scheduler_capture.json"

    set +e
    (
        export RCH_MIRROR_ATTEST_WORKERS_JSON="${POOL_WORKERS_JSON}"
        export RCH_MIRROR_ATTEST_SSH_BIN="${FAKE_SSH}"
        export FAKE_RCH_STATUS_JSON="${scheduler_file}"
        export FAKE_RCH_MIRROR_MODE="${mode_a}"
        export FAKE_RCH_MIRROR_MODE_WORKER_B="${mode_b}"
        export FAKE_RCH_MIRROR_REMOTE_HEAD="${remote_head}"
        export FAKE_RCH_MIRROR_REPO_ROOT="${ROOT_DIR}"
        RCH_MIRROR_REQUIRED_PATHS="Cargo.toml,crates/frankenterm-core/src/lib.rs"
        RCH_MIRROR_REQUIRE_WORKSPACE_MEMBER_ROOTS=0
        RCH_MIRROR_MIN_PASSING_WORKERS="${min_passing}"
        RCH_MIRROR_REQUIRE_ALL_CHECKED_WORKERS="${require_all_checked}"
        RCH_MIRROR_BLOCK_ON_STALE_HEAD="${block_on_stale}"
        ensure_rch_mirror_preflight
    ) >"${LOG_DIR}/${name}.stdout" 2>"${LOG_DIR}/${name}.stderr"
    rc=$?
    set -e

    if [[ "${rc}" -ne "${expected_rc}" ]]; then
        record_result "${name}" "false" "expected rc ${expected_rc}, got ${rc}"
        return
    fi

    if jq -e \
        --arg status "${expected_status}" \
        --arg reason "${expected_reason}" \
        '.status == $status
         and .reason_code == $reason
         and .scheduler_filter_active == true
         and .total_workers_checked == 2' \
        "${output_file}" >/dev/null; then
        record_result "${name}" "true"
    else
        record_result "${name}" "false" "unexpected mirror preflight JSON shape"
    fi
}

run_pool_mirror_fixture "mirror_pool_partial_allowed_fixture" \
    "success" "hash_mismatch" "1" "0" "0" "0" \
    "passed" "source_mirror_minimum_ready"

run_pool_mirror_fixture "mirror_pool_require_all_checked_blocks_fixture" \
    "success" "hash_mismatch" "1" "1" "0" "1" \
    "blocked" "source_mirror_checked_workers_blocked"

run_pool_mirror_fixture "mirror_pool_strict_stale_head_blocks_fixture" \
    "stale_head" "success" "2" "0" "1" "1" \
    "blocked" "source_mirror_blocked"

run_pinned_mirror_fixture() {
    local name="$1"
    local pinned_worker="$2"
    local mode_a="$3"
    local mode_b="$4"
    local require_all_checked="$5"
    local expected_rc="$6"
    local expected_status="$7"
    local expected_reason="$8"
    local probe_file="${LOG_DIR}/${name}_probe.json"
    local scheduler_file="${LOG_DIR}/${name}_scheduler.json"
    local output_file="${LOG_DIR}/${name}_mirror_preflight.json"
    local rc

    write_pool_probe_fixture "${probe_file}"
    write_pool_scheduler_fixture "${scheduler_file}"

    _RCH_PROBE_LOG="${probe_file}"
    _RCH_MIRROR_PREFLIGHT_LOG="${output_file}"
    _RCH_SCHEDULER_WORKERS_LOG="${LOG_DIR}/${name}_scheduler_capture.json"

    set +e
    (
        export RCH_MIRROR_ATTEST_WORKERS_JSON="${POOL_WORKERS_JSON}"
        export RCH_MIRROR_ATTEST_SSH_BIN="${FAKE_SSH}"
        export FAKE_RCH_STATUS_JSON="${scheduler_file}"
        export FAKE_RCH_MIRROR_MODE="${mode_a}"
        export FAKE_RCH_MIRROR_MODE_WORKER_B="${mode_b}"
        export FAKE_RCH_MIRROR_REMOTE_HEAD="${HEAD_SHA}"
        export FAKE_RCH_MIRROR_REPO_ROOT="${ROOT_DIR}"
        RCH_WORKER="${pinned_worker}"
        RCH_MIRROR_REQUIRED_PATHS="Cargo.toml,crates/frankenterm-core/src/lib.rs"
        RCH_MIRROR_REQUIRE_WORKSPACE_MEMBER_ROOTS=0
        RCH_MIRROR_MIN_PASSING_WORKERS="1"
        RCH_MIRROR_REQUIRE_ALL_CHECKED_WORKERS="${require_all_checked}"
        RCH_MIRROR_BLOCK_ON_STALE_HEAD=0
        ensure_rch_mirror_preflight
    ) >"${LOG_DIR}/${name}.stdout" 2>"${LOG_DIR}/${name}.stderr"
    rc=$?
    set -e

    if [[ "${rc}" -ne "${expected_rc}" ]]; then
        record_result "${name}" "false" "expected rc ${expected_rc}, got ${rc}"
        return
    fi

    if jq -e \
        --arg status "${expected_status}" \
        --arg reason "${expected_reason}" \
        --arg worker "${pinned_worker}" \
        '.status == $status
         and .reason_code == $reason
         and .total_workers_checked == 1
         and .worker_results[0].worker.id == $worker' \
        "${output_file}" >/dev/null; then
        record_result "${name}" "true"
    else
        record_result "${name}" "false" "unexpected pinned mirror preflight JSON shape"
    fi
}

run_pinned_mirror_fixture "mirror_pool_pinned_worker_only_fixture" \
    "worker-b" "hash_mismatch" "success" "1" "0" \
    "passed" "source_mirror_ready"

run_pinned_mirror_fixture "mirror_pool_pinned_worker_missing_blocks_fixture" \
    "worker-b" "success" "missing_file" "0" "1" \
    "blocked" "source_mirror_blocked"

run_selected_worker_mirror_fixture() {
    local name="$1"
    local mode_b="$2"
    local expected_rc="$3"
    local output_file="${LOG_DIR}/${name}.log"
    local mirror_file="${LOG_DIR}/${name}.selected_worker_mirror.json"
    local selected=""
    local rc

    set +e
    selected="$(
      {
        export RCH_MIRROR_ATTEST_WORKERS_JSON="${POOL_WORKERS_JSON}"
        export RCH_MIRROR_ATTEST_SSH_BIN="${FAKE_SSH}"
        export FAKE_SELECTED_WORKER_SYNC_RC=0
        export FAKE_RCH_MIRROR_MODE="success"
        export FAKE_RCH_MIRROR_MODE_WORKER_B="${mode_b}"
        export FAKE_RCH_MIRROR_REMOTE_HEAD="${HEAD_SHA}"
        export FAKE_RCH_MIRROR_REPO_ROOT="${ROOT_DIR}"
        RCH_SELECTED_WORKER_MIRROR_PREFLIGHT=1
        RCH_MIRROR_REQUIRED_PATHS="Cargo.toml,crates/frankenterm-core/src/lib.rs"
        RCH_MIRROR_REQUIRE_WORKSPACE_MEMBER_ROOTS=0
        rch_attest_selected_worker_before_cargo "${output_file}" \
          env CARGO_TARGET_DIR=target/rch-fixture cargo test -p frankenterm-gui terminal_state
      } 2>"${LOG_DIR}/${name}.stderr"
    )"
    rc=$?
    set -e

    if [[ "${rc}" -ne "${expected_rc}" ]]; then
        record_result "${name}" "false" "expected rc ${expected_rc}, got ${rc}"
        return
    fi

    if [[ "${expected_rc}" -eq 0 ]]; then
        if [[ "${selected}" == "worker-b" ]] \
          && jq -e '.status == "passed" and .worker.id == "worker-b"' "${mirror_file}" >/dev/null; then
            record_result "${name}" "true"
        else
            record_result "${name}" "false" "selected worker was not pinned to passing mirror fixture"
        fi
    else
        if jq -e '.status == "failed" and .reason_code == "rch_mirror.missing_tracked_file" and .worker.id == "worker-b"' "${mirror_file}" >/dev/null; then
            record_result "${name}" "true"
        else
            record_result "${name}" "false" "failing selected-worker mirror fixture did not retain expected JSON"
        fi
    fi
}

run_selected_worker_mirror_fixture "selected_worker_preflight_passes_fixture" \
    "success" "0"

run_selected_worker_mirror_fixture "selected_worker_preflight_missing_blocks_fixture" \
    "missing_file" "1"

run_selected_worker_sync_fixture() {
    local name="$1"
    local sync_rc="$2"
    local expected_rc="$3"
    local output_file="${LOG_DIR}/${name}.log"
    local sync_file="${LOG_DIR}/${name}.selected_worker_sync.log"
    local rc

    set +e
    (
        export RCH_MIRROR_ATTEST_WORKERS_JSON="${POOL_WORKERS_JSON}"
        export RCH_MIRROR_ATTEST_SSH_BIN="${FAKE_SSH}"
        export FAKE_SELECTED_WORKER_SYNC_RC="${sync_rc}"
        export FAKE_RCH_MIRROR_MODE="success"
        export FAKE_RCH_MIRROR_REMOTE_HEAD="${HEAD_SHA}"
        export FAKE_RCH_MIRROR_REPO_ROOT="${ROOT_DIR}"
        RCH_SELECTED_WORKER_MIRROR_PREFLIGHT=1
        RCH_MIRROR_REQUIRED_PATHS="Cargo.toml,crates/frankenterm-core/src/lib.rs"
        RCH_MIRROR_REQUIRE_WORKSPACE_MEMBER_ROOTS=0
        rch_attest_selected_worker_before_cargo "${output_file}" \
          env CARGO_TARGET_DIR=target/rch-fixture cargo test -p frankenterm-gui terminal_state
    ) >"${LOG_DIR}/${name}.stdout" 2>"${LOG_DIR}/${name}.stderr"
    rc=$?
    set -e

    if [[ "${rc}" -ne "${expected_rc}" ]]; then
        record_result "${name}" "false" "expected rc ${expected_rc}, got ${rc}"
        return
    fi

    if [[ "${expected_rc}" -eq 0 ]]; then
        record_result "${name}" "true"
    elif [[ -f "${sync_file}" ]] && grep -Fq "selected-worker source materialization smoke failed" "${sync_file}"; then
        record_result "${name}" "true"
    else
        record_result "${name}" "false" "selected-worker sync failure did not retain expected log"
    fi
}

run_selected_worker_sync_fixture "selected_worker_sync_failure_blocks_fixture" \
    "42" "1"

jq -cn \
  --arg test "rch_worker_mirror_attestation" \
  --arg log_dir "${LOG_DIR}" \
  --argjson total "${TOTAL}" \
  --argjson pass "${PASS}" \
  --argjson fail "${FAIL}" \
  '{test:$test,log_dir:$log_dir,total:$total,pass:$pass,fail:$fail}'

[[ "${FAIL}" -eq 0 ]]
