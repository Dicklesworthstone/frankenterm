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

HEAD_SHA="$(git -C "${ROOT_DIR}" rev-parse HEAD)"
WORKERS_JSON="${LOG_DIR}/workers.json"
FAKE_SSH="${LOG_DIR}/fake_ssh.sh"

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

case "${FAKE_RCH_MIRROR_MODE:-success}" in
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
    printf 'FILE\tCargo.toml\tpresent\n'
    printf 'FILE\tcrates/frankenterm-core/src/lib.rs\tpresent\n'
    ;;
  missing_file)
    printf 'STATUS\tok\n'
    printf 'ROOT\t/data/projects/frankenterm\n'
    printf 'HEAD\t%s\n' "${FAKE_RCH_MIRROR_REMOTE_HEAD}"
    printf 'FILE\tCargo.toml\tpresent\n'
    printf 'FILE\tcrates/frankenterm-core/src/lib.rs\tmissing\n'
    ;;
  success)
    printf 'STATUS\tok\n'
    printf 'ROOT\t/data/projects/frankenterm\n'
    printf 'HEAD\t%s\n' "${FAKE_RCH_MIRROR_REMOTE_HEAD}"
    printf 'FILE\tCargo.toml\tpresent\n'
    printf 'FILE\tcrates/frankenterm-core/src/lib.rs\tpresent\n'
    ;;
  *)
    printf 'unknown fake mode: %s\n' "${FAKE_RCH_MIRROR_MODE:-}" >&2
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
        and (.remote.required_files | all(.remote_status == "present"))
    ' "${SUCCESS_JSON}" >/dev/null; then
        record_result "success_fixture" "true"
    else
        record_result "success_fixture" "false" "unexpected JSON shape"
    fi
else
    record_result "success_fixture" "false" "script failed"
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
    record_result "stale_head_fixture" "false" "script unexpectedly passed"
else
    if jq -e '
        .status == "failed"
        and .reason_code == "rch_mirror.head_mismatch"
        and .failure_domain == "source_mirror"
        and .remote.head_matches == false
    ' "${STALE_JSON}" >/dev/null; then
        record_result "stale_head_fixture" "true"
    else
        record_result "stale_head_fixture" "false" "unexpected JSON shape"
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

jq -cn \
  --arg test "rch_worker_mirror_attestation" \
  --arg log_dir "${LOG_DIR}" \
  --argjson total "${TOTAL}" \
  --argjson pass "${PASS}" \
  --argjson fail "${FAIL}" \
  '{test:$test,log_dir:$log_dir,total:$total,pass:$pass,fail:$fail}'

[[ "${FAIL}" -eq 0 ]]
