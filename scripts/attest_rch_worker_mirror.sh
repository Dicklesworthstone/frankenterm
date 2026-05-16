#!/usr/bin/env bash
# Read-only selected-worker source mirror attestation for RCH proof lanes.
#
# This intentionally does not run Cargo. It checks that a named worker has a
# reachable source mirror at the expected HEAD with required tracked files
# present, then emits a stable JSON/JSONL record suitable for proof-ledger
# attachment.

set -euo pipefail

SCRIPT_NAME="$(basename "$0")"
REPO_ROOT=""
WORKER_ID=""
BEAD_ID=""
OUTPUT_MODE="json"
REMOTE_BASE="${RCH_MIRROR_ATTEST_REMOTE_BASE:-}"
WORKERS_TOML="${RCH_MIRROR_ATTEST_WORKERS_TOML:-${HOME}/.config/rch/workers.toml}"
WORKERS_JSON_OVERRIDE="${RCH_MIRROR_ATTEST_WORKERS_JSON:-}"
SSH_BIN="${RCH_MIRROR_ATTEST_SSH_BIN:-ssh}"
CONNECT_TIMEOUT_SECS="${RCH_MIRROR_ATTEST_CONNECT_TIMEOUT_SECS:-8}"
COMMAND_TEXT=""
TARGET_DIR="not_applicable"
REQUIRED_PATHS=()
REMOTE_PROJECT_ROOTS=()
INCLUDE_WORKSPACE_MEMBER_ROOTS="false"

usage() {
    cat <<USAGE
Usage: ${SCRIPT_NAME} --worker <id> (--path <repo-relative-file> | --workspace-member-roots) [options]

Options:
  --worker <id>              Required RCH worker id to attest.
  --path <file>              Required tracked repo-relative file. Repeatable.
  --workspace-member-roots   Require every Cargo workspace member manifest plus
                             declared lib/bin/build source roots that exist locally.
  --bead <id>                Bead id to include in output.
  --command <text>           Intended command context; not executed.
  --target-dir <path>        Intended remote target dir context; not checked.
  --remote-base <path>       Worker remote_base override.
  --remote-project-root <p>  Remote repo root candidate. Repeatable.
  --repo-root <path>         Local repo root override.
  --workers-toml <path>      RCH workers.toml override.
  --json                    Emit JSON object (default).
  --jsonl                   Emit one JSON object as JSONL.
  -h, --help                Show this help.

Environment overrides:
  RCH_MIRROR_ATTEST_WORKERS_JSON   JSON from 'rch workers list --json' or a fixture.
  RCH_MIRROR_ATTEST_SSH_BIN        SSH executable or test double.
  RCH_MIRROR_ATTEST_REMOTE_BASE    Remote base path override.
USAGE
}

fatal_usage() {
    printf 'FATAL: %s\n' "$1" >&2
    usage >&2
    exit 64
}

command_exists() {
    command -v "$1" >/dev/null 2>&1
}

file_sha256() {
    local path="$1"
    if command_exists shasum; then
        shasum -a 256 -- "${path}" | awk '{print $1}'
    elif command_exists sha256sum; then
        sha256sum -- "${path}" | awk '{print $1}'
    else
        return 1
    fi
}

json_bool() {
    if [[ "$1" == "true" ]]; then
        printf 'true\n'
    else
        printf 'false\n'
    fi
}

timestamp_utc() {
    date -u '+%Y-%m-%dT%H:%M:%SZ'
}

jq_escape_array_append() {
    local current_json="$1"
    local value="$2"
    jq -cn --argjson current "${current_json}" --arg value "${value}" \
        '$current + [$value]'
}

add_required_path() {
    local path="$1"
    local existing
    for existing in "${REQUIRED_PATHS[@]}"; do
        [[ "${existing}" == "${path}" ]] && return 0
    done
    REQUIRED_PATHS+=("${path}")
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --worker)
            [[ $# -ge 2 ]] || fatal_usage "--worker requires a value"
            WORKER_ID="$2"
            shift 2
            ;;
        --path)
            [[ $# -ge 2 ]] || fatal_usage "--path requires a value"
            add_required_path "$2"
            shift 2
            ;;
        --workspace-member-roots)
            INCLUDE_WORKSPACE_MEMBER_ROOTS="true"
            shift
            ;;
        --bead)
            [[ $# -ge 2 ]] || fatal_usage "--bead requires a value"
            BEAD_ID="$2"
            shift 2
            ;;
        --command)
            [[ $# -ge 2 ]] || fatal_usage "--command requires a value"
            COMMAND_TEXT="$2"
            shift 2
            ;;
        --target-dir)
            [[ $# -ge 2 ]] || fatal_usage "--target-dir requires a value"
            TARGET_DIR="$2"
            shift 2
            ;;
        --remote-base)
            [[ $# -ge 2 ]] || fatal_usage "--remote-base requires a value"
            REMOTE_BASE="$2"
            shift 2
            ;;
        --remote-project-root)
            [[ $# -ge 2 ]] || fatal_usage "--remote-project-root requires a value"
            REMOTE_PROJECT_ROOTS+=("$2")
            shift 2
            ;;
        --repo-root)
            [[ $# -ge 2 ]] || fatal_usage "--repo-root requires a value"
            REPO_ROOT="$2"
            shift 2
            ;;
        --workers-toml)
            [[ $# -ge 2 ]] || fatal_usage "--workers-toml requires a value"
            WORKERS_TOML="$2"
            shift 2
            ;;
        --json)
            OUTPUT_MODE="json"
            shift
            ;;
        --jsonl)
            OUTPUT_MODE="jsonl"
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            fatal_usage "unknown argument: $1"
            ;;
    esac
done

[[ -n "${WORKER_ID}" ]] || fatal_usage "--worker is required"
command_exists jq || {
    printf 'FATAL: jq is required for %s\n' "${SCRIPT_NAME}" >&2
    exit 69
}
command_exists python3 || {
    printf 'FATAL: python3 is required for %s\n' "${SCRIPT_NAME}" >&2
    exit 69
}

if [[ -z "${REPO_ROOT}" ]]; then
    REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || true)"
fi
[[ -n "${REPO_ROOT}" && -d "${REPO_ROOT}/.git" ]] || {
    printf 'FATAL: could not resolve local git repo root\n' >&2
    exit 69
}

cd "${REPO_ROOT}"

discover_workspace_member_required_paths() {
    local root="$1"
    python3 - "${root}" <<'PY'
import glob
import sys
from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError:
    print("tomllib is required to parse Cargo.toml", file=sys.stderr)
    raise SystemExit(69)

root = Path(sys.argv[1]).resolve()
manifest = root / "Cargo.toml"
try:
    data = tomllib.loads(manifest.read_text())
except Exception as exc:
    print(f"failed to parse {manifest}: {exc}", file=sys.stderr)
    raise SystemExit(69)

members = data.get("workspace", {}).get("members", [])
if not isinstance(members, list):
    print("Cargo.toml [workspace].members must be a list", file=sys.stderr)
    raise SystemExit(64)

seen = set()

def emit(path: Path) -> None:
    if not path.is_file():
        return
    try:
        rel = path.relative_to(root).as_posix()
    except ValueError:
        print(f"workspace member source escaped repo root: {path}", file=sys.stderr)
        raise SystemExit(64)
    if rel not in seen:
        print(rel)
        seen.add(rel)

for member in members:
    if not isinstance(member, str) or not member:
        print("Cargo.toml workspace member paths must be non-empty strings", file=sys.stderr)
        raise SystemExit(64)
    member_path = Path(member)
    if member_path.is_absolute() or ".." in member_path.parts:
        print(f"invalid workspace member path: {member}", file=sys.stderr)
        raise SystemExit(64)

    pattern = str(root / member)
    matches = sorted(glob.glob(pattern)) or [pattern]
    for matched in matches:
        matched_path = Path(matched)
        try:
            rel = matched_path.relative_to(root).as_posix()
        except ValueError:
            print(f"workspace member escaped repo root: {matched_path}", file=sys.stderr)
            raise SystemExit(64)
        cargo_toml = matched_path / "Cargo.toml"
        emit(cargo_toml)

        try:
            member_data = tomllib.loads(cargo_toml.read_text())
        except Exception as exc:
            print(f"failed to parse {cargo_toml}: {exc}", file=sys.stderr)
            raise SystemExit(69)

        package = member_data.get("package", {})
        build_script = package.get("build")
        if isinstance(build_script, str) and build_script:
            emit(matched_path / build_script)
        elif build_script is not False:
            emit(matched_path / "build.rs")

        lib = member_data.get("lib")
        if isinstance(lib, dict):
            emit(matched_path / lib.get("path", "src/lib.rs"))
        elif lib is not False:
            emit(matched_path / "src/lib.rs")

        bins = member_data.get("bin", [])
        if isinstance(bins, dict):
            bins = [bins]
        for bin_target in bins:
            if isinstance(bin_target, dict) and isinstance(bin_target.get("path"), str):
                emit(matched_path / bin_target["path"])

        emit(matched_path / "src/main.rs")
PY
}

if [[ "${INCLUDE_WORKSPACE_MEMBER_ROOTS}" == "true" ]]; then
    workspace_member_path_list="$(discover_workspace_member_required_paths "${REPO_ROOT}")"
    while IFS= read -r workspace_member_path; do
        [[ -n "${workspace_member_path}" ]] || continue
        add_required_path "${workspace_member_path}"
    done <<<"${workspace_member_path_list}"
fi

[[ "${#REQUIRED_PATHS[@]}" -gt 0 ]] || fatal_usage "at least one --path or --workspace-member-roots is required"

load_workers_json() {
    if [[ -n "${WORKERS_JSON_OVERRIDE}" ]]; then
        if [[ -f "${WORKERS_JSON_OVERRIDE}" ]]; then
            cat "${WORKERS_JSON_OVERRIDE}"
        else
            printf '%s\n' "${WORKERS_JSON_OVERRIDE}"
        fi
        return 0
    fi

    rch workers list --json
}

load_worker_toml_json() {
    local worker_id="$1"
    local workers_toml="$2"

    if [[ ! -f "${workers_toml}" ]]; then
        printf '{}\n'
        return 0
    fi

    python3 - "${workers_toml}" "${worker_id}" <<'PY'
import json
import sys
from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError:
    print("{}")
    raise SystemExit(0)

path = Path(sys.argv[1]).expanduser()
worker_id = sys.argv[2]
try:
    data = tomllib.loads(path.read_text())
except Exception:
    print("{}")
    raise SystemExit(0)

for worker in data.get("workers", []):
    if str(worker.get("id", "")) == worker_id:
        allowed = {
            "id",
            "host",
            "user",
            "identity_file",
            "port",
            "total_slots",
            "priority",
            "tags",
        }
        print(json.dumps({k: v for k, v in worker.items() if k in allowed}))
        break
else:
    print("{}")
PY
}

load_remote_base() {
    if [[ -n "${REMOTE_BASE}" ]]; then
        printf '%s\n' "${REMOTE_BASE}"
        return 0
    fi

    local config_json
    config_json="$(rch config show --json 2>/dev/null || true)"
    if [[ -n "${config_json}" ]]; then
        jq -r '.data.transfer.remote_base // .transfer.remote_base // empty' <<<"${config_json}" 2>/dev/null || true
    fi
}

add_candidate_root() {
    local candidate="$1"
    local existing
    [[ -n "${candidate}" ]] || return 0
    for existing in "${REMOTE_PROJECT_ROOTS[@]}"; do
        [[ "${existing}" == "${candidate}" ]] && return 0
    done
    REMOTE_PROJECT_ROOTS+=("${candidate}")
}

populate_candidate_roots() {
    local repo_name="$1"
    local remote_base="$2"

    if [[ -n "${remote_base}" ]]; then
        add_candidate_root "${remote_base%/}/${repo_name}/*"
        add_candidate_root "${remote_base%/}/${repo_name}"
        add_candidate_root "${remote_base%/}/projects/${repo_name}/*"
        add_candidate_root "${remote_base%/}/projects/${repo_name}"
    fi
    add_candidate_root "/data/projects/${repo_name}"
    add_candidate_root "/dp/${repo_name}"
}

candidate_roots_json() {
    local roots_json="[]"
    local root
    for root in "${REMOTE_PROJECT_ROOTS[@]}"; do
        roots_json="$(jq_escape_array_append "${roots_json}" "${root}")"
    done
    printf '%s\n' "${roots_json}"
}

build_required_files_json() {
    local files_json="[]"
    local path tracked local_present local_status local_sha256

    for path in "${REQUIRED_PATHS[@]}"; do
        case "${path}" in
            ""|/*|*..*|*$'\n'*|*$'\t'*)
                jq -cn --argjson current "${files_json}" --arg path "${path}" \
                    '$current + [{path:$path, tracked:false, local_present:false, local_status:"invalid_required_path", local_sha256:null, remote_status:"not_checked", remote_sha256:null, hash_matches:false}]'
                return 0
                ;;
        esac

        tracked="false"
        local_present="false"
        local_status="untracked"
        local_sha256=""
        if git ls-files --error-unmatch -- "${path}" >/dev/null 2>&1; then
            tracked="true"
            if [[ -f "${path}" ]]; then
                local_present="true"
                local_status="present"
                local_sha256="$(file_sha256 "${path}" || true)"
            else
                local_status="missing_local"
            fi
        fi

        files_json="$(jq -cn \
            --argjson current "${files_json}" \
            --arg path "${path}" \
            --argjson tracked "$(json_bool "${tracked}")" \
            --argjson local_present "$(json_bool "${local_present}")" \
            --arg local_status "${local_status}" \
            --arg local_sha256 "${local_sha256}" \
            '$current + [{
                path:$path,
                tracked:$tracked,
                local_present:$local_present,
                local_status:$local_status,
                local_sha256:(if $local_sha256 == "" then null else $local_sha256 end),
                remote_status:"not_checked",
                remote_sha256:null,
                hash_matches:false
            }]')"
    done

    printf '%s\n' "${files_json}"
}

emit_result() {
    local status="$1"
    local reason_code="$2"
    local failure_domain="$3"
    local confidence="$4"
    local detail="$5"
    local worker_json="$6"
    local candidate_roots_json="$7"
    local required_files_json="$8"
    local remote_project_path="$9"
    local remote_head="${10}"
    local local_head="${11}"
    local remote_stdout_file="${12:-}"
    local remote_stderr_file="${13:-}"
    local ssh_exit_code="${14:-}"
    local generated_at branch dirty identity_configured head_matches

    generated_at="$(timestamp_utc)"
    branch="$(git branch --show-current 2>/dev/null || true)"
    if [[ -n "$(git status --short 2>/dev/null || true)" ]]; then
        dirty="true"
    else
        dirty="false"
    fi
    if [[ "$(jq -r '.identity_file // empty' <<<"${worker_json}")" != "" ]]; then
        identity_configured="true"
    else
        identity_configured="false"
    fi
    if [[ -n "${remote_head}" && "${remote_head}" == "${local_head}" ]]; then
        head_matches="true"
    else
        head_matches="false"
    fi

    jq -cn \
        --argjson schema_version 1 \
        --arg kind "rch_selected_worker_mirror_attestation" \
        --arg output_mode "${OUTPUT_MODE}" \
        --arg generated_at "${generated_at}" \
        --arg status "${status}" \
        --arg reason_code "${reason_code}" \
        --arg failure_domain "${failure_domain}" \
        --arg confidence "${confidence}" \
        --arg detail "${detail}" \
        --arg bead_id "${BEAD_ID}" \
        --arg command "${COMMAND_TEXT}" \
        --arg target_dir "${TARGET_DIR}" \
        --arg worker_id "${WORKER_ID}" \
        --arg worker_host "$(jq -r '.host // empty' <<<"${worker_json}")" \
        --arg worker_user "$(jq -r '.user // "ubuntu"' <<<"${worker_json}")" \
        --argjson identity_configured "$(json_bool "${identity_configured}")" \
        --arg repo_root "${REPO_ROOT}" \
        --arg branch "${branch}" \
        --arg local_head "${local_head}" \
        --argjson local_worktree_dirty "$(json_bool "${dirty}")" \
        --arg remote_base "${REMOTE_BASE}" \
        --arg remote_project_path "${remote_project_path}" \
        --arg remote_head "${remote_head}" \
        --argjson head_matches "$(json_bool "${head_matches}")" \
        --argjson candidate_roots "${candidate_roots_json}" \
        --argjson required_files "${required_files_json}" \
        --arg remote_stdout_file "${remote_stdout_file}" \
        --arg remote_stderr_file "${remote_stderr_file}" \
        --arg ssh_exit_code "${ssh_exit_code}" \
        '{
            schema_version:$schema_version,
            kind:$kind,
            output_mode:$output_mode,
            generated_at:$generated_at,
            status:$status,
            reason_code:$reason_code,
            failure_domain:$failure_domain,
            confidence:$confidence,
            detail:$detail,
            bead_id:$bead_id,
            command_context:$command,
            target_dir:$target_dir,
            worker:{
                id:$worker_id,
                host:$worker_host,
                user:$worker_user,
                identity_configured:$identity_configured
            },
            local:{
                repo_root:$repo_root,
                branch:$branch,
                head:$local_head,
                worktree_dirty:$local_worktree_dirty
            },
            remote:{
                remote_base:$remote_base,
                candidate_roots:$candidate_roots,
                project_path:$remote_project_path,
                head:$remote_head,
                head_matches:$head_matches,
                required_files:$required_files
            },
            checks:{
                worker_reachable:(
                    $reason_code != "rch_mirror.worker_unreachable"
                    and $reason_code != "rch_mirror.invalid_worker"
                    and $reason_code != "rch_mirror.config_unavailable"
                ),
                project_path_present:($remote_project_path != ""),
                head_available:($remote_head != ""),
                tracked_files_present:($remote_project_path != "" and ($required_files | all(.remote_status != "missing"))),
                tracked_file_hashes_match:($remote_project_path != "" and ($required_files | all(.hash_matches == true))),
                compiler_or_test_executed:false,
                scheduler_queue_checked:false
            },
            artifacts:{
                remote_stdout_file:$remote_stdout_file,
                remote_stderr_file:$remote_stderr_file
            },
            transport:{
                ssh_exit_code:$ssh_exit_code
            }
        }'
}

workers_json="$(load_workers_json 2>/dev/null || true)"
[[ -n "${workers_json}" ]] || {
    empty_worker="$(jq -cn --arg id "${WORKER_ID}" '{id:$id}')"
    roots_json="[]"
    files_json="$(build_required_files_json)"
    emit_result "failed" "rch_mirror.config_unavailable" "selected_worker" "inconclusive_worker_evidence" \
        "could not load RCH worker inventory" "${empty_worker}" "${roots_json}" "${files_json}" "" "" "$(git rev-parse HEAD)" "" "" ""
    exit 2
}

worker_from_json="$(jq -c --arg id "${WORKER_ID}" '
    (if (.data | type) == "array" then .data
     elif (.data.workers | type) == "array" then .data.workers
     elif (.workers | type) == "array" then .workers
     else [] end)
    | map(select(.id == $id))
    | .[0] // {}
' <<<"${workers_json}")"
worker_from_toml="$(load_worker_toml_json "${WORKER_ID}" "${WORKERS_TOML}")"
worker_json="$(jq -cn --argjson a "${worker_from_json}" --argjson b "${worker_from_toml}" '$a * $b')"

local_head="$(git rev-parse HEAD)"
files_json="$(build_required_files_json)"
REMOTE_BASE="$(load_remote_base | head -n 1)"
repo_name="$(basename "${REPO_ROOT}")"
populate_candidate_roots "${repo_name}" "${REMOTE_BASE}"
roots_json="$(candidate_roots_json)"

if [[ "$(jq -r '.host // empty' <<<"${worker_json}")" == "" ]]; then
    emit_result "failed" "rch_mirror.invalid_worker" "selected_worker" "inconclusive_worker_evidence" \
        "selected worker is not present in RCH inventory" "${worker_json}" "${roots_json}" "${files_json}" "" "" "${local_head}" "" "" ""
    exit 2
fi

invalid_local_reason="$(jq -r '
    if any(.[]; .local_status == "invalid_required_path") then "rch_mirror.invalid_required_path"
    elif any(.[]; .tracked == false) then "rch_mirror.untracked_required_file"
    elif any(.[]; .local_status == "missing_local") then "rch_mirror.local_required_file_missing"
    else "" end
' <<<"${files_json}")"
if [[ -n "${invalid_local_reason}" ]]; then
    emit_result "failed" "${invalid_local_reason}" "input" "inconclusive_worker_evidence" \
        "required path is not a present tracked local file" "${worker_json}" "${roots_json}" "${files_json}" "" "" "${local_head}" "" "" ""
    exit 2
fi

payload=""
payload+=$'H\t'"${local_head}"$'\n'
for root in "${REMOTE_PROJECT_ROOTS[@]}"; do
    payload+=$'R\t'"${root}"$'\n'
done
for path in "${REQUIRED_PATHS[@]}"; do
    local_sha="$(jq -r --arg path "${path}" '.[] | select(.path == $path) | .local_sha256 // ""' <<<"${files_json}")"
    payload+=$'F\t'"${path}"$'\t'"${local_sha}"$'\n'
done
payload_b64="$(printf '%s' "${payload}" | base64 | tr -d '\n')"

host="$(jq -r '.host' <<<"${worker_json}")"
user="$(jq -r '.user // "ubuntu"' <<<"${worker_json}")"
port="$(jq -r '.port // 22' <<<"${worker_json}")"
identity_file="$(jq -r '.identity_file // empty' <<<"${worker_json}")"
ssh_target="${user}@${host}"
ssh_opts=(
    -o BatchMode=yes
    -o StrictHostKeyChecking=accept-new
    -o ConnectTimeout="${CONNECT_TIMEOUT_SECS}"
    -o ControlMaster=no
    -o ControlPath=none
    -p "${port}"
)
if [[ -n "${identity_file}" ]]; then
    ssh_opts+=(-i "${identity_file}")
fi

remote_stdout="$(mktemp "${TMPDIR:-/tmp}/rch-mirror-attest-stdout.XXXXXX")"
remote_stderr="$(mktemp "${TMPDIR:-/tmp}/rch-mirror-attest-stderr.XXXXXX")"
set +e
"${SSH_BIN}" "${ssh_opts[@]}" "${ssh_target}" "bash -s -- '${payload_b64}'" >"${remote_stdout}" 2>"${remote_stderr}" <<'REMOTE_SCRIPT'
set -euo pipefail

file_sha256() {
    local path="$1"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum -- "${path}" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 -- "${path}" | awk '{print $1}'
    else
        return 1
    fi
}

payload_b64="$1"
payload="$(printf '%s' "${payload_b64}" | base64 -d 2>/dev/null || true)"
local_head=""
roots=()
paths=()
local_hashes=()

while IFS=$'\t' read -r kind value extra; do
    case "${kind}" in
        H) local_head="${value}" ;;
        R) roots+=("${value}") ;;
        F)
            paths+=("${value}")
            local_hashes+=("${extra:-}")
            ;;
    esac
done <<<"${payload}"

selected_root=""
first_existing_root=""
first_all_present_root=""
first_all_hash_match_root=""
best_present_root=""
best_present_count=-1
best_manifest_present=0
best_hash_match_root=""
best_hash_match_count=-1
best_hash_present_count=-1
best_hash_manifest_present=0
shopt -s nullglob
for root in "${roots[@]}"; do
    expanded_roots=()
    case "${root}" in
        *"*"*|*"?"*|*"["*)
            # Intentionally expand operator-controlled remote root globs.
            # shellcheck disable=SC2206
            expanded_roots=(${root})
            ;;
        *)
            expanded_roots=("${root}")
            ;;
    esac

    for expanded_root in "${expanded_roots[@]}"; do
        [[ -d "${expanded_root}" ]] || continue
        if [[ -z "${first_existing_root}" ]]; then
            first_existing_root="${expanded_root}"
        fi

        all_present="true"
        all_hash_match="true"
        present_count=0
        hash_match_count=0
        manifest_present=0
        if [[ -f "${expanded_root}/Cargo.toml" ]]; then
            manifest_present=1
        fi
        for path_index in "${!paths[@]}"; do
            path="${paths[${path_index}]}"
            local_hash="${local_hashes[${path_index}]:-}"
            if [[ -f "${expanded_root}/${path}" ]]; then
                present_count=$((present_count + 1))
                if [[ -n "${local_hash}" ]] \
                    && remote_hash="$(file_sha256 "${expanded_root}/${path}" 2>/dev/null)" \
                    && [[ "${remote_hash}" == "${local_hash}" ]]; then
                    hash_match_count=$((hash_match_count + 1))
                else
                    all_hash_match="false"
                fi
            else
                all_present="false"
                all_hash_match="false"
            fi
        done

        if [[ "${hash_match_count}" -gt "${best_hash_match_count}" ]] \
            || [[ "${hash_match_count}" -eq "${best_hash_match_count}" && "${present_count}" -gt "${best_hash_present_count}" ]] \
            || [[ "${hash_match_count}" -eq "${best_hash_match_count}" && "${present_count}" -eq "${best_hash_present_count}" && "${manifest_present}" -gt "${best_hash_manifest_present}" ]]; then
            best_hash_match_count="${hash_match_count}"
            best_hash_present_count="${present_count}"
            best_hash_manifest_present="${manifest_present}"
            best_hash_match_root="${expanded_root}"
        fi

        if [[ "${present_count}" -gt "${best_present_count}" ]] \
            || [[ "${present_count}" -eq "${best_present_count}" && "${manifest_present}" -gt "${best_manifest_present}" ]]; then
            best_present_count="${present_count}"
            best_manifest_present="${manifest_present}"
            best_present_root="${expanded_root}"
        fi

        if [[ "${all_present}" == "true" && -z "${first_all_present_root}" ]]; then
            first_all_present_root="${expanded_root}"
        fi

        if [[ "${all_present}" == "true" && "${all_hash_match}" == "true" && -z "${first_all_hash_match_root}" ]]; then
            first_all_hash_match_root="${expanded_root}"
        fi

        if [[ "${all_present}" == "true" && "${all_hash_match}" == "true" ]] \
            && [[ -n "${local_head}" ]] \
            && git -C "${expanded_root}" rev-parse --verify HEAD >/dev/null 2>&1 \
            && [[ "$(git -C "${expanded_root}" rev-parse HEAD)" == "${local_head}" ]]; then
            selected_root="${expanded_root}"
            break 2
        fi
    done
done
shopt -u nullglob

if [[ -z "${selected_root}" ]]; then
    selected_root="${first_all_hash_match_root:-${best_hash_match_root:-${first_all_present_root:-${best_present_root:-${first_existing_root}}}}}"
fi

if [[ -z "${selected_root}" ]]; then
    printf 'STATUS\tproject_path_absent\n'
    exit 0
fi

printf 'STATUS\tok\n'
printf 'ROOT\t%s\n' "${selected_root}"
if git -C "${selected_root}" rev-parse --verify HEAD >/dev/null 2>&1; then
    printf 'HEAD\t%s\n' "$(git -C "${selected_root}" rev-parse HEAD)"
else
    printf 'HEAD\t\n'
fi

for path in "${paths[@]}"; do
    if [[ -f "${selected_root}/${path}" ]]; then
        if remote_hash="$(file_sha256 "${selected_root}/${path}" 2>/dev/null)"; then
            printf 'FILE\t%s\tpresent\t%s\n' "${path}" "${remote_hash}"
        else
            printf 'FILE\t%s\tpresent\t\n' "${path}"
        fi
    else
        printf 'FILE\t%s\tmissing\t\n' "${path}"
    fi
done
REMOTE_SCRIPT
ssh_rc=$?
set -e

if [[ "${ssh_rc}" -ne 0 ]]; then
    emit_result "failed" "rch_mirror.worker_unreachable" "selected_worker" "inconclusive_worker_evidence" \
        "SSH transport to selected worker failed" "${worker_json}" "${roots_json}" "${files_json}" "" "" "${local_head}" \
        "${remote_stdout}" "${remote_stderr}" "${ssh_rc}"
    exit 2
fi

remote_status=""
remote_root=""
remote_head=""
while IFS=$'\t' read -r kind value extra rest; do
    case "${kind}" in
        STATUS) remote_status="${value}" ;;
        ROOT) remote_root="${value}" ;;
        HEAD) remote_head="${value}" ;;
        FILE)
            files_json="$(jq -cn \
                --argjson current "${files_json}" \
                --arg path "${value}" \
                --arg remote_status "${extra}" \
                --arg remote_sha256 "${rest}" \
                '$current | map(
                    if .path == $path then
                        . + {
                            remote_status:$remote_status,
                            remote_sha256:(if $remote_sha256 == "" then null else $remote_sha256 end),
                            hash_matches:(.local_sha256 != null and $remote_sha256 != "" and .local_sha256 == $remote_sha256)
                        }
                    else . end
                )')"
            ;;
    esac
done <"${remote_stdout}"

if [[ "${remote_status}" == "project_path_absent" ]]; then
    emit_result "failed" "rch_mirror.project_path_absent" "source_mirror" "inconclusive_worker_evidence" \
        "none of the remote project root candidates exists on the selected worker" "${worker_json}" "${roots_json}" "${files_json}" \
        "" "" "${local_head}" "${remote_stdout}" "${remote_stderr}" "${ssh_rc}"
    exit 2
fi

if [[ "${remote_status}" != "ok" || -z "${remote_root}" ]]; then
    emit_result "failed" "rch_mirror.remote_protocol_error" "source_mirror" "inconclusive_worker_evidence" \
        "selected worker returned an unrecognized mirror attestation response" "${worker_json}" "${roots_json}" "${files_json}" \
        "${remote_root}" "${remote_head}" "${local_head}" "${remote_stdout}" "${remote_stderr}" "${ssh_rc}"
    exit 2
fi

if jq -e 'any(.[]; .remote_status == "missing")' <<<"${files_json}" >/dev/null; then
    emit_result "failed" "rch_mirror.missing_tracked_file" "source_mirror" "inconclusive_worker_evidence" \
        "selected worker source mirror is missing at least one required tracked file" "${worker_json}" "${roots_json}" "${files_json}" \
        "${remote_root}" "${remote_head}" "${local_head}" "${remote_stdout}" "${remote_stderr}" "${ssh_rc}"
    exit 2
fi

if jq -e 'any(.[]; .local_sha256 == null or .remote_sha256 == null)' <<<"${files_json}" >/dev/null; then
    emit_result "failed" "rch_mirror.hash_unavailable" "source_mirror" "inconclusive_worker_evidence" \
        "selected worker source mirror could not hash at least one required tracked file" "${worker_json}" "${roots_json}" "${files_json}" \
        "${remote_root}" "${remote_head}" "${local_head}" "${remote_stdout}" "${remote_stderr}" "${ssh_rc}"
    exit 2
fi

if jq -e 'any(.[]; .hash_matches != true)' <<<"${files_json}" >/dev/null; then
    emit_result "failed" "rch_mirror.tracked_file_hash_mismatch" "source_mirror" "inconclusive_worker_evidence" \
        "selected worker source mirror required tracked file content differs from local snapshot" "${worker_json}" "${roots_json}" "${files_json}" \
        "${remote_root}" "${remote_head}" "${local_head}" "${remote_stdout}" "${remote_stderr}" "${ssh_rc}"
    exit 2
fi

if [[ -z "${remote_head}" ]]; then
    emit_result "passed" "rch_mirror.required_files_ok_head_unavailable" "none" "target_worker_mirror_attestation" \
        "selected worker required tracked files matched local content; Git HEAD could not be verified in the RCH rsync mirror" "${worker_json}" "${roots_json}" \
        "${files_json}" "${remote_root}" "${remote_head}" "${local_head}" "${remote_stdout}" "${remote_stderr}" "${ssh_rc}"
    exit 0
fi

if [[ "${remote_head}" != "${local_head}" ]]; then
    emit_result "passed" "rch_mirror.required_files_ok_head_mismatch" "none" "target_worker_mirror_attestation" \
        "selected worker required tracked files matched local content; Git metadata HEAD differs from the local snapshot" "${worker_json}" "${roots_json}" \
        "${files_json}" "${remote_root}" "${remote_head}" "${local_head}" "${remote_stdout}" "${remote_stderr}" "${ssh_rc}"
    exit 0
fi

emit_result "passed" "rch_mirror.ok" "none" "target_worker_mirror_attestation" \
    "selected worker source mirror matched local HEAD and required tracked files were present" "${worker_json}" "${roots_json}" \
    "${files_json}" "${remote_root}" "${remote_head}" "${local_head}" "${remote_stdout}" "${remote_stderr}" "${ssh_rc}"
