#!/usr/bin/env bash
# Shared shell helpers for static attestation verifier scripts.

if [[ -n "${_STATIC_ATTESTATION_HELPERS_SH_LOADED:-}" ]]; then
  return 0 2>/dev/null || exit 0
fi
_STATIC_ATTESTATION_HELPERS_SH_LOADED=1

static_attestation_repo_root() {
  if [[ -n "${FRANKENTERM_REPO_ROOT:-}" ]]; then
    printf '%s\n' "${FRANKENTERM_REPO_ROOT}"
  else
    cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd
  fi
}

static_attestation_fail() {
  printf 'static attestation: %s\n' "$*" >&2
  exit 1
}

static_attestation_require_command() {
  local command_name="${1-}"

  [[ -n "${command_name}" ]] || static_attestation_fail "command name is empty"
  command -v "${command_name}" >/dev/null 2>&1 || static_attestation_fail "missing command: ${command_name}"
}

static_attestation_require_repo_relative_path() {
  local path="${1-}"

  [[ -n "${path}" ]] || static_attestation_fail "path is empty"
  [[ "${path}" != /* ]] || static_attestation_fail "absolute path is forbidden: ${path}"

  local part
  IFS='/' read -r -a _static_attestation_path_parts <<<"${path}"
  for part in "${_static_attestation_path_parts[@]}"; do
    [[ "${part}" != ".." ]] || static_attestation_fail "parent traversal is forbidden: ${path}"
  done
}

static_attestation_require_file() {
  local path="${1-}"
  local root

  static_attestation_require_repo_relative_path "${path}"
  root="$(static_attestation_repo_root)"
  [[ -f "${root}/${path}" ]] || static_attestation_fail "missing file: ${path}"
}

static_attestation_require_executable_script() {
  local path="${1-}"
  local root

  static_attestation_require_command ruby
  static_attestation_require_repo_relative_path "${path}"
  root="$(static_attestation_repo_root)"
  FRANKENTERM_REPO_ROOT="${root}" ruby -I "${root}/tests/scripts" -r static_attestation_helpers -e \
    'StaticAttestation.require_direct_exec_script!(ARGV.fetch(0))' "${path}" \
    || static_attestation_fail "script shape check failed: ${path}"
}

static_attestation_run_ruby() {
  local root

  static_attestation_require_command ruby
  root="$(static_attestation_repo_root)"
  FRANKENTERM_REPO_ROOT="${root}" ruby -I "${root}/tests/scripts" -r static_attestation_helpers "$@"
}
