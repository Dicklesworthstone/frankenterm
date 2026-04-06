#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 2 || $# -gt 3 ]]; then
  echo "usage: $0 <ft-linux-binary> <output-dir> [run-id]" >&2
  exit 2
fi

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
COMPOSE_FILE="${ROOT_DIR}/tests/e2e/distributed-compose.yml"
HARNESS_FILE="${ROOT_DIR}/tests/e2e/distributed_compose_harness.py"
BINARY_PATH="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
OUTPUT_DIR="$2"
RUN_ID="${3:-$(date +%Y%m%d_%H%M%S)}"
PROJECT_NAME="ftdist${RUN_ID//[^a-zA-Z0-9]/}"
COMPOSE_LOG="${OUTPUT_DIR}/compose.log"
COMPOSE_PS="${OUTPUT_DIR}/compose.ps"
DOCKER_RESOURCES="${OUTPUT_DIR}/docker_resources.txt"
EXPORT_DIR="${OUTPUT_DIR}/export"
PLATFORM="${FT_E2E_COMPOSE_PLATFORM:-linux/amd64}"

mkdir -p "${OUTPUT_DIR}"
OUTPUT_DIR="$(cd "${OUTPUT_DIR}" && pwd)"

if [[ ! -x "${BINARY_PATH}" ]]; then
  echo "expected executable ft binary at ${BINARY_PATH}" >&2
  exit 1
fi

if [[ ! -f "${COMPOSE_FILE}" ]]; then
  echo "missing compose file ${COMPOSE_FILE}" >&2
  exit 1
fi

if [[ ! -f "${HARNESS_FILE}" ]]; then
  echo "missing harness file ${HARNESS_FILE}" >&2
  exit 1
fi

if ! command -v docker >/dev/null 2>&1; then
  echo "docker is required for distributed compose smoke" >&2
  exit 1
fi

if ! docker compose version >/dev/null 2>&1; then
  echo "docker compose is required for distributed compose smoke" >&2
  exit 1
fi

{
  echo "project=${PROJECT_NAME}"
  echo "platform=${PLATFORM}"
  docker info --format 'ncpus={{.NCPU}} mem_total={{.MemTotal}}'
} > "${DOCKER_RESOURCES}" || true

if docker info --format '{{.NCPU}} {{.MemTotal}}' >/tmp/ft_distributed_compose_resources.$$ 2>/dev/null; then
  read -r docker_cpus docker_mem_bytes < /tmp/ft_distributed_compose_resources.$$
  rm -f /tmp/ft_distributed_compose_resources.$$
  if [[ "${docker_cpus}" -lt 4 || "${docker_mem_bytes}" -lt 4294967296 ]]; then
    echo "warning: Docker Desktop resources below recommended minimum (cpu=${docker_cpus}, mem_bytes=${docker_mem_bytes})" \
      >> "${DOCKER_RESOURCES}"
  fi
fi

export FT_E2E_COMPOSE_BINARY="${BINARY_PATH}"
export FT_E2E_COMPOSE_HARNESS="${HARNESS_FILE}"
export FT_E2E_COMPOSE_PLATFORM="${PLATFORM}"
export FT_E2E_COMPOSE_MARKER="${FT_E2E_COMPOSE_MARKER:-DIST_COMPOSE_STREAM_MARKER}"
export FT_E2E_COMPOSE_TOKEN="${FT_E2E_COMPOSE_TOKEN:-compose-secret}"

collect_artifacts() {
  docker compose -f "${COMPOSE_FILE}" --project-name "${PROJECT_NAME}" logs --no-color > "${COMPOSE_LOG}" 2>&1 || true
  docker compose -f "${COMPOSE_FILE}" --project-name "${PROJECT_NAME}" ps --all > "${COMPOSE_PS}" 2>&1 || true
  mkdir -p "${EXPORT_DIR}"
  docker run --rm --platform "${PLATFORM}" \
    -v "${PROJECT_NAME}_shared_artifacts:/from" \
    -v "${OUTPUT_DIR}:/to" \
    python:3.12-slim \
    /bin/sh -lc 'mkdir -p /to/export && cp -a /from/. /to/export/' >/dev/null 2>&1 || true
  docker compose -f "${COMPOSE_FILE}" --project-name "${PROJECT_NAME}" down --remove-orphans >/dev/null 2>&1 || true
}

trap collect_artifacts EXIT

docker compose -f "${COMPOSE_FILE}" --project-name "${PROJECT_NAME}" --profile distributed \
  up --abort-on-container-exit --exit-code-from test-runner
collect_artifacts
trap - EXIT

required_files=(
  "${EXPORT_DIR}/aggregator.log"
  "${EXPORT_DIR}/aggregator_log.json"
  "${EXPORT_DIR}/agent_log.json"
  "${EXPORT_DIR}/security_log.json"
  "${EXPORT_DIR}/db_snapshot.json"
  "${EXPORT_DIR}/db_snapshot.sqlite"
  "${EXPORT_DIR}/query_visibility.json"
  "${EXPORT_DIR}/compose_summary.json"
)

for required in "${required_files[@]}"; do
  if [[ ! -f "${required}" ]]; then
    echo "missing expected compose artifact ${required}" >&2
    exit 1
  fi
done

echo "[distributed-compose-smoke] PASS"
echo "Artifacts: ${EXPORT_DIR}"
