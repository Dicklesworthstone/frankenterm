#!/usr/bin/env bash
# Run or retain the resource-cockpit target-class proof gate.
#
# This wrapper never promotes reduced conformance proof to target-class proof.
# If the selected SKU predicate is absent, it writes a retained
# skipped_not_proven summary and exits successfully only when
# FT_TARGET_CLASS_ALLOW_SKIP=1.
#
# Modes (W9.4b):
#   dry-run (default on the dev host): FT_TARGET_CLASS_ALLOW_SKIP=1. Exercises
#     host detection, the predicate preflight, and the artifact shape; retains
#     skipped_not_proven and exits 0. Safe to run anywhere; produces no proof.
#   preflight-only: FT_TARGET_CLASS_PREFLIGHT_ONLY=1. Prints the predicate
#     report, writes a skipped artifact, and exits 0 only when the host is
#     target-class eligible (non-zero otherwise). Use to confirm a rented box
#     BEFORE paying for the full run.
#   run (rented target-class box): FT_TARGET_CLASS_ALLOW_SKIP=0. Fails fast with
#     a clear message if the host misses the SKU floor; on a conforming host it
#     runs the W9.3 rehearsal, benchmark budget lane, and cockpit conformance
#     suite, then emits a NON-skipped, signable summary.json
#     (ready_to_sign=true only when all required lanes pass).
#
# Operator command for the rented 64-CPU/256-GiB box (see
# docs/perf/target-class-hardware.md):
#   FT_TARGET_CLASS_SKU=linux-x86_64-high-core FT_TARGET_CLASS_PREFLIGHT_ONLY=1 \
#     scripts/run-target-class-cockpit.sh   # confirm eligibility, then:
#   FT_TARGET_CLASS_SKU=linux-x86_64-high-core FT_TARGET_CLASS_ALLOW_SKIP=0 \
#     scripts/run-target-class-cockpit.sh   # run + emit the signable artifact
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BEAD_ID="ft-tf6g3.14"
SCENARIO_ID="resource_cockpit_target_class"
RUN_ID="${FT_TARGET_CLASS_RUN_ID:-$(date -u +"%Y%m%dT%H%M%SZ")}"
SKU_ID="${FT_TARGET_CLASS_SKU:-linux-x86_64-high-core}"
ALLOW_SKIP="${FT_TARGET_CLASS_ALLOW_SKIP:-1}"

case "${SKU_ID}" in
  macos-apple-silicon-dev)
    MAJOR_SKU="macos"
    SKU_DESCRIPTION="Apple Silicon macOS operator workstation"
    REQUIRED_OS_FAMILY="Darwin"
    REQUIRED_ARCH="arm64"
    REQUIRED_LOGICAL_CPUS=14
    REQUIRED_MEMORY_GIB=64
    REQUIRED_DISK_FREE_GIB=50
    REQUIRED_KERNEL_MAJOR=24
    REQUIRED_OS_VERSION_MAJOR=15
    HIGH_SCALE_SKU=false
    ;;
  linux-x86_64-high-core)
    MAJOR_SKU="linux"
    SKU_DESCRIPTION="Linux x86_64 high-core swarm host"
    REQUIRED_OS_FAMILY="Linux"
    REQUIRED_ARCH="x86_64"
    REQUIRED_LOGICAL_CPUS=64
    REQUIRED_MEMORY_GIB=256
    REQUIRED_DISK_FREE_GIB=200
    REQUIRED_KERNEL_MAJOR=6
    REQUIRED_OS_VERSION_MAJOR=0
    HIGH_SCALE_SKU=true
    ;;
  *)
    printf 'error: unknown FT_TARGET_CLASS_SKU=%s\n' "${SKU_ID}" >&2
    printf 'known SKUs: macos-apple-silicon-dev, linux-x86_64-high-core\n' >&2
    exit 2
    ;;
esac

ARTIFACT_DIR="${ROOT_DIR}/tests/e2e/artifacts/target-class/${SKU_ID}/${RUN_ID}"
SUMMARY_FILE="${ARTIFACT_DIR}/summary.json"
HOST_FILE="${ARTIFACT_DIR}/host.json"
COMMANDS_FILE="${ARTIFACT_DIR}/commands.txt"
CONFORMANCE_LOG="${ARTIFACT_DIR}/resource-cockpit-conformance.log"
HIGH_SCALE_REHEARSAL_DIR="${ARTIFACT_DIR}/high-scale-rehearsal"
HIGH_SCALE_REHEARSAL_LOG="${ARTIFACT_DIR}/high-scale-rehearsal.log"
HIGH_SCALE_REHEARSAL_VERIFY_LOG="${ARTIFACT_DIR}/high-scale-rehearsal-verify.log"
BENCH_BUDGET_LOG="${ARTIFACT_DIR}/bench-budget.log"
BENCH_BUDGET_REPORT="${ARTIFACT_DIR}/bench-budget-report.json"
BENCH_BUDGET_TARGET_DIR="${FT_TARGET_CLASS_BENCH_TARGET_DIR:-target/ft-target-class-bench-${RUN_ID}}"
RCH_CAPABILITIES_FILE="${ARTIFACT_DIR}/rch-worker-capabilities.json"
RCH_CAPABILITIES_STDERR="${ARTIFACT_DIR}/rch-worker-capabilities.stderr.log"
mkdir -p "${ARTIFACT_DIR}"
: >"${COMMANDS_FILE}"
: >"${CONFORMANCE_LOG}"

is_uint() {
  [[ "${1:-}" =~ ^[0-9]+$ ]]
}

bool_from_test() {
  if "$@"; then
    printf 'true'
  else
    printf 'false'
  fi
}

ge_uint() {
  local observed="$1"
  local required="$2"
  is_uint "${observed}" && [[ "${observed}" -ge "${required}" ]]
}

record_command() {
  printf '%s\n' "$*" >>"${COMMANDS_FILE}"
}

OS_FAMILY="$(uname -s)"
ARCH="$(uname -m)"
KERNEL_RELEASE="$(uname -r)"
KERNEL_MAJOR="${KERNEL_RELEASE%%.*}"
OS_NAME="${OS_FAMILY}"
OS_VERSION=""
OS_BUILD=""
OBSERVED_LOGICAL_CPUS=0
OBSERVED_MEMORY_GIB=0

if [[ "${OS_FAMILY}" == "Darwin" ]]; then
  OS_NAME="$(sw_vers -productName 2>/dev/null || printf 'macOS')"
  OS_VERSION="$(sw_vers -productVersion 2>/dev/null || printf '')"
  OS_BUILD="$(sw_vers -buildVersion 2>/dev/null || printf '')"
  OBSERVED_LOGICAL_CPUS="$(sysctl -n hw.logicalcpu)"
  memory_bytes="$(sysctl -n hw.memsize)"
  OBSERVED_MEMORY_GIB="$((memory_bytes / 1073741824))"
elif [[ "${OS_FAMILY}" == "Linux" ]]; then
  if [[ -r /etc/os-release ]]; then
    # shellcheck disable=SC1091
    . /etc/os-release
    OS_NAME="${PRETTY_NAME:-Linux}"
    OS_VERSION="${VERSION_ID:-}"
    OS_BUILD="${BUILD_ID:-}"
  fi
  OBSERVED_LOGICAL_CPUS="$(getconf _NPROCESSORS_ONLN 2>/dev/null || nproc 2>/dev/null || printf '0')"
  memory_kib="$(awk '/^MemTotal:/ {print $2}' /proc/meminfo 2>/dev/null || printf '0')"
  OBSERVED_MEMORY_GIB="$((memory_kib / 1048576))"
fi

disk_free_kib="$(df -Pk "${ROOT_DIR}" | awk 'NR == 2 {print $4}')"
OBSERVED_DISK_FREE_GIB="$((disk_free_kib / 1048576))"
OS_VERSION_MAJOR="${OS_VERSION%%.*}"
if [[ -z "${OS_VERSION_MAJOR}" || "${OS_VERSION_MAJOR}" == "${OS_VERSION}" && ! "${OS_VERSION_MAJOR}" =~ ^[0-9]+$ ]]; then
  OS_VERSION_MAJOR=0
fi

if [[ "${OS_FAMILY}" == "${REQUIRED_OS_FAMILY}" ]]; then
  OS_FAMILY_OK=true
else
  OS_FAMILY_OK=false
fi
if [[ "${ARCH}" == "${REQUIRED_ARCH}" ]]; then
  ARCH_OK=true
else
  ARCH_OK=false
fi
CPU_OK="$(bool_from_test ge_uint "${OBSERVED_LOGICAL_CPUS}" "${REQUIRED_LOGICAL_CPUS}")"
MEMORY_OK="$(bool_from_test ge_uint "${OBSERVED_MEMORY_GIB}" "${REQUIRED_MEMORY_GIB}")"
DISK_OK="$(bool_from_test ge_uint "${OBSERVED_DISK_FREE_GIB}" "${REQUIRED_DISK_FREE_GIB}")"
KERNEL_OK="$(bool_from_test ge_uint "${KERNEL_MAJOR}" "${REQUIRED_KERNEL_MAJOR}")"
if [[ "${REQUIRED_OS_VERSION_MAJOR}" -eq 0 ]]; then
  OS_VERSION_OK=true
else
  OS_VERSION_OK="$(bool_from_test ge_uint "${OS_VERSION_MAJOR}" "${REQUIRED_OS_VERSION_MAJOR}")"
fi

TARGET_SKU_MATCHED=false
if [[ "${OS_FAMILY_OK}" == "true" \
   && "${ARCH_OK}" == "true" \
   && "${CPU_OK}" == "true" \
   && "${MEMORY_OK}" == "true" \
   && "${DISK_OK}" == "true" \
   && "${KERNEL_OK}" == "true" \
   && "${OS_VERSION_OK}" == "true" ]]; then
  TARGET_SKU_MATCHED=true
fi

HIGH_SCALE_PREDICATE_MET=false
if [[ "${TARGET_SKU_MATCHED}" == "true" && "${HIGH_SCALE_SKU}" == "true" ]]; then
  HIGH_SCALE_PREDICATE_MET=true
fi

PROOF_STATUS="skipped_not_proven"
TARGET_HARDWARE_STATUS="skipped_not_proven"
HIGH_SCALE_CLAIM_ALLOWED=false
if [[ "${HIGH_SCALE_PREDICATE_MET}" == "true" ]]; then
  PROOF_STATUS="proven_predicate_met"
  TARGET_HARDWARE_STATUS="target_hardware"
  HIGH_SCALE_CLAIM_ALLOWED=true
fi

jq -n \
  --arg os_family "${OS_FAMILY}" \
  --arg arch "${ARCH}" \
  --arg kernel_release "${KERNEL_RELEASE}" \
  --arg os_name "${OS_NAME}" \
  --arg os_version "${OS_VERSION}" \
  --arg os_build "${OS_BUILD}" \
  --argjson logical_cpus "${OBSERVED_LOGICAL_CPUS}" \
  --argjson memory_gib "${OBSERVED_MEMORY_GIB}" \
  --argjson disk_free_gib "${OBSERVED_DISK_FREE_GIB}" \
  '{
    os_family: $os_family,
    arch: $arch,
    kernel_release: $kernel_release,
    os_name: $os_name,
    os_version: $os_version,
    os_build: $os_build,
    logical_cpus: $logical_cpus,
    memory_gib: $memory_gib,
    disk_free_gib: $disk_free_gib
  }' >"${HOST_FILE}"

RCH_CAPABILITIES_STATUS="not_run"
RCH_MAX_LOGICAL_CPUS=0
if [[ "${FT_TARGET_CLASS_SKIP_RCH_CAPABILITIES:-0}" != "1" ]] && command -v rch >/dev/null 2>&1; then
  record_command "rch --json workers capabilities"
  set +e
  rch --json workers capabilities >"${RCH_CAPABILITIES_FILE}" 2>"${RCH_CAPABILITIES_STDERR}"
  rch_capabilities_rc=$?
  set -e
  if [[ "${rch_capabilities_rc}" -eq 0 ]] && jq -e . "${RCH_CAPABILITIES_FILE}" >/dev/null 2>&1; then
    RCH_CAPABILITIES_STATUS="passed"
    RCH_MAX_LOGICAL_CPUS="$(jq -r '[.data.workers[]?.capabilities.num_cpus // 0] | max // 0' "${RCH_CAPABILITIES_FILE}")"
  else
    RCH_CAPABILITIES_STATUS="unavailable"
  fi
fi

HIGH_SCALE_REHEARSAL_STATUS="not_run"
HIGH_SCALE_REHEARSAL_SUMMARY_PATH=""
BENCH_BUDGET_STATUS="not_run"
BENCH_BUDGET_SUMMARY_PATH=""

write_summary() {
  local status="$1"
  local conformance_status="$2"
  local conformance_summary_path="$3"
  local exit_code="$4"

  jq -n \
    --arg schema_version "1.0.0" \
    --arg bead_id "${BEAD_ID}" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg run_id "${RUN_ID}" \
    --arg status "${status}" \
    --arg sku_id "${SKU_ID}" \
    --arg major_sku "${MAJOR_SKU}" \
    --arg sku_description "${SKU_DESCRIPTION}" \
    --arg artifact_dir "${ARTIFACT_DIR}" \
    --arg proof_status "${PROOF_STATUS}" \
    --arg target_hardware_status "${TARGET_HARDWARE_STATUS}" \
    --arg conformance_status "${conformance_status}" \
    --arg conformance_summary_path "${conformance_summary_path}" \
    --arg high_scale_rehearsal_status "${HIGH_SCALE_REHEARSAL_STATUS}" \
    --arg high_scale_rehearsal_summary_path "${HIGH_SCALE_REHEARSAL_SUMMARY_PATH}" \
    --arg bench_budget_status "${BENCH_BUDGET_STATUS}" \
    --arg bench_budget_summary_path "${BENCH_BUDGET_SUMMARY_PATH}" \
    --arg bench_budget_target_dir "${BENCH_BUDGET_TARGET_DIR}" \
    --arg rch_capabilities_status "${RCH_CAPABILITIES_STATUS}" \
    --argjson exit_code "${exit_code}" \
    --arg os_family "${OS_FAMILY}" \
    --arg arch "${ARCH}" \
    --arg kernel_release "${KERNEL_RELEASE}" \
    --arg os_name "${OS_NAME}" \
    --arg os_version "${OS_VERSION}" \
    --arg os_build "${OS_BUILD}" \
    --arg required_os_family "${REQUIRED_OS_FAMILY}" \
    --arg required_arch "${REQUIRED_ARCH}" \
    --argjson required_logical_cpus "${REQUIRED_LOGICAL_CPUS}" \
    --argjson required_memory_gib "${REQUIRED_MEMORY_GIB}" \
    --argjson required_disk_free_gib "${REQUIRED_DISK_FREE_GIB}" \
    --argjson required_kernel_major "${REQUIRED_KERNEL_MAJOR}" \
    --argjson required_os_version_major "${REQUIRED_OS_VERSION_MAJOR}" \
    --argjson observed_logical_cpus "${OBSERVED_LOGICAL_CPUS}" \
    --argjson observed_memory_gib "${OBSERVED_MEMORY_GIB}" \
    --argjson observed_disk_free_gib "${OBSERVED_DISK_FREE_GIB}" \
    --argjson high_scale_sku "${HIGH_SCALE_SKU}" \
    --argjson rch_max_logical_cpus "${RCH_MAX_LOGICAL_CPUS}" \
    --argjson os_family_ok "${OS_FAMILY_OK}" \
    --argjson arch_ok "${ARCH_OK}" \
    --argjson cpu_ok "${CPU_OK}" \
    --argjson memory_ok "${MEMORY_OK}" \
    --argjson disk_ok "${DISK_OK}" \
    --argjson kernel_ok "${KERNEL_OK}" \
    --argjson os_version_ok "${OS_VERSION_OK}" \
    --argjson target_sku_matched "${TARGET_SKU_MATCHED}" \
    --argjson high_scale_predicate_met "${HIGH_SCALE_PREDICATE_MET}" \
    --argjson high_scale_claim_allowed "${HIGH_SCALE_CLAIM_ALLOWED}" \
    '{
      schema_version: $schema_version,
      bead_id: $bead_id,
      scenario_id: $scenario_id,
      run_id: $run_id,
      status: $status,
      exit_code: $exit_code,
      ready_to_sign: (
        $status == "passed"
        and $proof_status == "proven_predicate_met"
        and $conformance_status == "passed"
        and $bench_budget_status == "passed"
        and $high_scale_rehearsal_status == "passed"
      ),
      artifact_dir: $artifact_dir,
      sku: {
        id: $sku_id,
        major_sku: $major_sku,
        description: $sku_description,
        high_scale_sku: $high_scale_sku,
        required: {
          os_family: $required_os_family,
          arch: $required_arch,
          logical_cpus: $required_logical_cpus,
          memory_gib: $required_memory_gib,
          disk_free_gib: $required_disk_free_gib,
          kernel_major: $required_kernel_major,
          os_version_major: $required_os_version_major
        }
      },
      observed_host: {
        os_family: $os_family,
        arch: $arch,
        kernel_release: $kernel_release,
        os_name: $os_name,
        os_version: $os_version,
        os_build: $os_build,
        logical_cpus: $observed_logical_cpus,
        memory_gib: $observed_memory_gib,
        disk_free_gib: $observed_disk_free_gib
      },
      target_sku_predicate: {
        met: $target_sku_matched,
        checks: {
          os_family: $os_family_ok,
          arch: $arch_ok,
          logical_cpus: $cpu_ok,
          memory_gib: $memory_ok,
          disk_free_gib: $disk_ok,
          kernel_major: $kernel_ok,
          os_version_major: $os_version_ok
        }
      },
      hardware_predicate: {
        required_logical_cpus: 64,
        required_memory_gib: 256,
        observed_logical_cpus: $observed_logical_cpus,
        observed_memory_gib: $observed_memory_gib,
        target_class: $high_scale_predicate_met,
        proof_status: $proof_status,
        failure_reasons: [
          (if $high_scale_predicate_met then empty else "target_class.high_scale_predicate_absent" end),
          (if $cpu_ok then empty else "target_class.cpu_below_sku_floor" end),
          (if $memory_ok then empty else "target_class.memory_below_sku_floor" end),
          (if $os_family_ok then empty else "target_class.os_family_mismatch" end),
          (if $arch_ok then empty else "target_class.arch_mismatch" end),
          (if $disk_ok then empty else "target_class.disk_free_below_sku_floor" end),
          (if $kernel_ok then empty else "target_class.kernel_below_sku_floor" end),
          (if $os_version_ok then empty else "target_class.os_version_below_sku_floor" end)
        ]
      },
      evidence: {
        target_hardware: $target_hardware_status,
        skipped_not_proven: ($proof_status == "skipped_not_proven"),
        high_scale_claim_allowed: $high_scale_claim_allowed,
        conformance: $conformance_status,
        conformance_summary: (if $conformance_summary_path == "" then null else $conformance_summary_path end),
        high_scale_rehearsal: $high_scale_rehearsal_status,
        high_scale_rehearsal_summary: (if $high_scale_rehearsal_summary_path == "" then null else $high_scale_rehearsal_summary_path end),
        bench_budget: $bench_budget_status,
        bench_budget_report: (if $bench_budget_summary_path == "" then null else $bench_budget_summary_path end),
        rch_capabilities: {
          status: $rch_capabilities_status,
          max_worker_logical_cpus: $rch_max_logical_cpus
        }
      },
      benches: [
        {
          id: "criterion_budget_gate",
          kind: "criterion_budget",
          synthetic: false,
          status: $bench_budget_status,
          command: "scripts/check_bench_budgets.sh",
          report: (if $bench_budget_summary_path == "" then null else $bench_budget_summary_path end),
          log: "bench-budget.log",
          cargo_target_dir: $bench_budget_target_dir,
          gates_ready_to_sign: true
        }
      ],
      rehearsals: [
        {
          id: "w9_3_high_scale_rehearsal",
          synthetic: true,
          status: $high_scale_rehearsal_status,
          command: "scripts/high-scale-rehearsal.sh",
          summary: (if $high_scale_rehearsal_summary_path == "" then null else $high_scale_rehearsal_summary_path end),
          log: "high-scale-rehearsal.log",
          verify_log: "high-scale-rehearsal-verify.log",
          gates_ready_to_sign: true
        }
      ],
      release_bundle_gate: {
        requires_artifact_per_major_sku: true,
        major_sku: $major_sku,
        artifact_path_pattern: "tests/e2e/artifacts/target-class/<sku>/<run_id>/summary.json"
      },
      artifacts: {
        host: "host.json",
        commands: "commands.txt",
        rch_worker_capabilities: "rch-worker-capabilities.json",
        rch_worker_capabilities_stderr: "rch-worker-capabilities.stderr.log",
        conformance_log: "resource-cockpit-conformance.log",
        high_scale_rehearsal_log: "high-scale-rehearsal.log",
        high_scale_rehearsal_verify_log: "high-scale-rehearsal-verify.log",
        high_scale_rehearsal_summary: "high-scale-rehearsal/rehearsal-summary.json",
        bench_budget_log: "bench-budget.log",
        bench_budget_report: "bench-budget-report.json"
      }
    }' >"${SUMMARY_FILE}"
}

run_high_scale_rehearsal() {
  record_command "scripts/high-scale-rehearsal.sh --out-dir ${HIGH_SCALE_REHEARSAL_DIR} --run-id ${RUN_ID}"
  set +e
  "${ROOT_DIR}/scripts/high-scale-rehearsal.sh" \
    --out-dir "${HIGH_SCALE_REHEARSAL_DIR}" \
    --run-id "${RUN_ID}" >"${HIGH_SCALE_REHEARSAL_LOG}" 2>&1
  local rehearsal_rc=$?
  set -e
  HIGH_SCALE_REHEARSAL_SUMMARY_PATH="${HIGH_SCALE_REHEARSAL_DIR}/rehearsal-summary.json"
  if [[ "${rehearsal_rc}" -ne 0 ]]; then
    HIGH_SCALE_REHEARSAL_STATUS="failed"
    return "${rehearsal_rc}"
  fi

  record_command "scripts/high-scale-rehearsal.sh --verify ${HIGH_SCALE_REHEARSAL_DIR}"
  set +e
  "${ROOT_DIR}/scripts/high-scale-rehearsal.sh" \
    --verify "${HIGH_SCALE_REHEARSAL_DIR}" >"${HIGH_SCALE_REHEARSAL_VERIFY_LOG}" 2>&1
  local verify_rc=$?
  set -e
  if [[ "${verify_rc}" -ne 0 ]]; then
    HIGH_SCALE_REHEARSAL_STATUS="failed"
    return "${verify_rc}"
  fi

  HIGH_SCALE_REHEARSAL_STATUS="passed"
  return 0
}

run_bench_budget_lane() {
  record_command "FT_BENCH_BUDGETS_TARGET_DIR=${BENCH_BUDGET_TARGET_DIR} scripts/check_bench_budgets.sh"
  set +e
  FT_BENCH_BUDGETS_TARGET_DIR="${BENCH_BUDGET_TARGET_DIR}" \
    "${ROOT_DIR}/scripts/check_bench_budgets.sh" >"${BENCH_BUDGET_LOG}" 2>&1
  local bench_rc=$?
  set -e

  local generated_report="${ROOT_DIR}/${BENCH_BUDGET_TARGET_DIR}/criterion/ft-budget-report.json"
  if [[ -f "${generated_report}" ]]; then
    cp "${generated_report}" "${BENCH_BUDGET_REPORT}"
    BENCH_BUDGET_SUMMARY_PATH="${BENCH_BUDGET_REPORT}"
  fi

  if [[ "${bench_rc}" -ne 0 ]]; then
    BENCH_BUDGET_STATUS="failed"
    return "${bench_rc}"
  fi
  if [[ -z "${BENCH_BUDGET_SUMMARY_PATH}" ]]; then
    BENCH_BUDGET_STATUS="failed_missing_report"
    return 1
  fi
  if ! jq -e '.format == "ft-budget-report" and .total > 0 and .fail == 0' \
    "${BENCH_BUDGET_SUMMARY_PATH}" >/dev/null 2>&1
  then
    BENCH_BUDGET_STATUS="failed"
    return 1
  fi

  BENCH_BUDGET_STATUS="passed"
  return 0
}

record_command "scripts/run-target-class-cockpit.sh FT_TARGET_CLASS_SKU=${SKU_ID}"

# Human-readable preflight: observed vs required predicate and per-check verdict.
# Printed to stderr so the machine-readable `summary=` line on stdout stays clean.
print_preflight_report() {
  {
    printf '== target-class preflight: SKU=%s ==\n' "${SKU_ID}"
    printf '  observed: %s/%s, %s logical CPU, %s GiB RAM, %s GiB free, kernel %s\n' \
      "${OS_FAMILY}" "${ARCH}" "${OBSERVED_LOGICAL_CPUS}" "${OBSERVED_MEMORY_GIB}" \
      "${OBSERVED_DISK_FREE_GIB}" "${KERNEL_RELEASE}"
    printf '  required: %s/%s, >=%s logical CPU, >=%s GiB RAM, >=%s GiB free, kernel >=%s\n' \
      "${REQUIRED_OS_FAMILY}" "${REQUIRED_ARCH}" "${REQUIRED_LOGICAL_CPUS}" \
      "${REQUIRED_MEMORY_GIB}" "${REQUIRED_DISK_FREE_GIB}" "${REQUIRED_KERNEL_MAJOR}"
    printf '  checks: os_family=%s arch=%s cpu=%s memory=%s disk=%s kernel=%s os_version=%s\n' \
      "${OS_FAMILY_OK}" "${ARCH_OK}" "${CPU_OK}" "${MEMORY_OK}" "${DISK_OK}" \
      "${KERNEL_OK}" "${OS_VERSION_OK}"
    if [[ "${HIGH_SCALE_PREDICATE_MET}" == "true" ]]; then
      printf '  verdict: CONFORMING — eligible to emit a signable target-class artifact\n'
    elif [[ "${TARGET_SKU_MATCHED}" == "true" ]]; then
      printf '  verdict: SKU matched but not a high-scale SKU — no target-class claim\n'
    else
      printf '  verdict: NOT CONFORMING — would retain skipped_not_proven (NOT target-class proof)\n'
    fi
  } >&2
}

print_preflight_report

# Preflight-only gate: let the operator confirm a rented box BEFORE the long run.
# Writes a complete (skipped) artifact, then exits 0 when eligible, non-zero when not.
if [[ "${FT_TARGET_CLASS_PREFLIGHT_ONLY:-0}" == "1" ]]; then
  HIGH_SCALE_REHEARSAL_STATUS="not_run_preflight_only"
  BENCH_BUDGET_STATUS="not_run_preflight_only"
  write_summary "skipped_not_proven" "not_run_preflight_only" "" 0
  printf 'summary=%s\n' "${SUMMARY_FILE}"
  if [[ "${HIGH_SCALE_PREDICATE_MET}" == "true" ]]; then
    printf 'preflight: host is target-class eligible; rerun without FT_TARGET_CLASS_PREFLIGHT_ONLY to run the suite and emit the signable artifact\n' >&2
    exit 0
  fi
  printf 'preflight: host is NOT target-class eligible; see checks above\n' >&2
  exit 3
fi

if [[ "${TARGET_SKU_MATCHED}" != "true" ]]; then
  HIGH_SCALE_REHEARSAL_STATUS="not_run_predicate_absent"
  BENCH_BUDGET_STATUS="not_run_predicate_absent"
  write_summary "skipped_not_proven" "not_run_predicate_absent" "" 0
  printf 'summary=%s\n' "${SUMMARY_FILE}"
  if [[ "${ALLOW_SKIP}" == "1" ]]; then
    printf 'dry-run: predicate absent on this host; retained skipped_not_proven (NOT target-class proof).\n' >&2
    printf 'dry-run: to produce a signable artifact, run on a conforming %s host (>=%s logical CPU, >=%s GiB RAM) with FT_TARGET_CLASS_ALLOW_SKIP=0.\n' \
      "${REQUIRED_OS_FAMILY}" "${REQUIRED_LOGICAL_CPUS}" "${REQUIRED_MEMORY_GIB}" >&2
    exit 0
  fi
  printf 'error: target-class predicate not met and FT_TARGET_CLASS_ALLOW_SKIP=0 — refusing to emit a non-proven artifact.\n' >&2
  printf 'error: this host does not meet the %s SKU floor (>=%s logical CPU, >=%s GiB RAM, >=%s GiB free); see preflight checks above.\n' \
    "${SKU_ID}" "${REQUIRED_LOGICAL_CPUS}" "${REQUIRED_MEMORY_GIB}" "${REQUIRED_DISK_FREE_GIB}" >&2
  exit 3
fi

record_command "tests/e2e/test_ft_rz0eb_4_resource_cockpit_conformance.sh"
set +e
"${ROOT_DIR}/tests/e2e/test_ft_rz0eb_4_resource_cockpit_conformance.sh" >"${CONFORMANCE_LOG}" 2>&1
conformance_rc=$?
set -e

conformance_summary_path="$(grep '^summary=' "${CONFORMANCE_LOG}" | tail -1 | sed 's/^summary=//')"
conformance_status="failed"
if [[ -n "${conformance_summary_path}" && -f "${conformance_summary_path}" ]]; then
  conformance_status="$(jq -r '.status // "failed"' "${conformance_summary_path}" 2>/dev/null || printf 'failed')"
fi

rehearsal_rc=0
bench_rc=0
if [[ "${HIGH_SCALE_SKU}" == "true" ]]; then
  run_high_scale_rehearsal || rehearsal_rc=$?
  run_bench_budget_lane || bench_rc=$?
else
  HIGH_SCALE_REHEARSAL_STATUS="not_required_for_non_high_scale_sku"
  BENCH_BUDGET_STATUS="not_required_for_non_high_scale_sku"
fi

if [[ "${conformance_rc}" -eq 0 \
   && "${conformance_status}" == "passed" \
   && "${rehearsal_rc}" -eq 0 \
   && "${bench_rc}" -eq 0 ]]; then
  write_summary "passed" "passed" "${conformance_summary_path}" 0
  printf 'summary=%s\n' "${SUMMARY_FILE}"
  exit 0
fi

failure_rc="${conformance_rc}"
if [[ "${failure_rc}" -eq 0 && "${rehearsal_rc}" -ne 0 ]]; then
  failure_rc="${rehearsal_rc}"
fi
if [[ "${failure_rc}" -eq 0 && "${bench_rc}" -ne 0 ]]; then
  failure_rc="${bench_rc}"
fi
if [[ "${failure_rc}" -eq 0 ]]; then
  failure_rc=1
fi

write_summary "failed" "${conformance_status}" "${conformance_summary_path}" "${failure_rc}"
printf 'summary=%s\n' "${SUMMARY_FILE}"
exit "${failure_rc}"
