#!/usr/bin/env bash
# Run or retain the resource-cockpit target-class proof gate.
#
# This wrapper never promotes reduced conformance proof to target-class proof.
# If the selected SKU predicate is absent, it writes a retained
# skipped_not_proven summary and exits successfully only when
# FT_TARGET_CLASS_ALLOW_SKIP=1.
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
        rch_capabilities: {
          status: $rch_capabilities_status,
          max_worker_logical_cpus: $rch_max_logical_cpus
        }
      },
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
        conformance_log: "resource-cockpit-conformance.log"
      }
    }' >"${SUMMARY_FILE}"
}

record_command "scripts/run-target-class-cockpit.sh FT_TARGET_CLASS_SKU=${SKU_ID}"

if [[ "${TARGET_SKU_MATCHED}" != "true" ]]; then
  write_summary "skipped_not_proven" "not_run_predicate_absent" "" 0
  printf 'summary=%s\n' "${SUMMARY_FILE}"
  if [[ "${ALLOW_SKIP}" == "1" ]]; then
    exit 0
  fi
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

if [[ "${conformance_rc}" -eq 0 && "${conformance_status}" == "passed" ]]; then
  write_summary "passed" "passed" "${conformance_summary_path}" 0
  printf 'summary=%s\n' "${SUMMARY_FILE}"
  exit 0
fi

write_summary "failed" "${conformance_status}" "${conformance_summary_path}" "${conformance_rc}"
printf 'summary=%s\n' "${SUMMARY_FILE}"
exit "${conformance_rc}"
