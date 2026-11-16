#!/usr/bin/env bash
# Validate docs/specs TLA+ substrate conventions.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
SPEC_DIR="${PROJECT_ROOT}/docs/specs"

failures=0

fail() {
  local file="$1"
  local message="$2"
  printf 'error: %s: %s\n' "${file#"${PROJECT_ROOT}"/}" "$message" >&2
  failures=$((failures + 1))
}

metric_value() {
  local file="$1"
  local key="$2"
  awk -v want="$key" '
    /coverage-metric:/ { in_block = 1; next }
    in_block {
      line = $0
      if (line ~ /^[[:space:]]*(\\\*|\/\/)/) {
        sub(/^[[:space:]]*(\\\*|\/\/)[[:space:]]*/, "", line)
        if (line ~ /^[A-Za-z0-9_-]+:[[:space:]]*/) {
          pos = index(line, ":")
          k = substr(line, 1, pos - 1)
          v = substr(line, pos + 1)
          gsub(/^[ \t]+|[ \t]+$/, "", k)
          gsub(/^[ \t]+|[ \t]+$/, "", v)
          if (k == want) {
            print v
            exit
          }
        }
        next
      }
      if (line ~ /^[[:space:]]*$/) {
        next
      }
      exit
    }
  ' "$file"
}

require_pattern() {
  local file="$1"
  local pattern="$2"
  local message="$3"
  if ! grep -Eq "$pattern" "$file"; then
    fail "$file" "$message"
  fi
}

require_no_pattern() {
  local file="$1"
  local pattern="$2"
  local message="$3"
  if grep -Eq "$pattern" "$file"; then
    fail "$file" "$message"
  fi
}

validate_mapping() {
  local spec="$1"
  local base="$2"
  local mapping="${SPEC_DIR}/${base}-mapping.md"

  if [[ ! -f "$mapping" ]]; then
    fail "$spec" "missing mapping doc docs/specs/${base}-mapping.md"
    return
  fi

  require_pattern "$mapping" "\`${base}\.tla\`" "mapping doc must name ${base}.tla"
  require_pattern "$mapping" '^## Rust Correspondence$' "mapping doc missing Rust Correspondence section"
  require_pattern "$mapping" '^## Action Mapping$' "mapping doc missing Action Mapping section"
  require_pattern "$mapping" '^## Invariant Mapping$' "mapping doc missing Invariant Mapping section"
  require_pattern "$mapping" '^## TLC Configuration$' "mapping doc missing TLC Configuration section"
  require_pattern "$mapping" 'crates/[^`| ]+:[0-9]+' "mapping doc must cite Rust paths with line numbers"
}

validate_cfg() {
  local spec="$1"
  local base="$2"
  local cfg="${SPEC_DIR}/${base}.cfg"

  if [[ ! -f "$cfg" ]]; then
    fail "$spec" "missing TLC config docs/specs/${base}.cfg"
    return
  fi

  require_pattern "$cfg" '^SPECIFICATION[[:space:]]+Spec$' "cfg must use SPECIFICATION Spec"
  require_pattern "$cfg" '^CONSTANTS$' "cfg must declare deterministic constants"
  require_pattern "$cfg" '^INVARIANT[[:space:]]+SafetyInvariants$' "cfg must check SafetyInvariants"
  require_no_pattern "$cfg" 'TODO|FIXME|<[^>]+>' "cfg must not contain placeholders"
}

validate_coverage_cfg() {
  local spec="$1"
  local coverage_cfg
  coverage_cfg="$(metric_value "$spec" "coverage-cfg")"
  if [[ -z "$coverage_cfg" ]]; then
    return 0
  fi

  local coverage_path="$coverage_cfg"
  if [[ "$coverage_path" != /* ]]; then
    coverage_path="${PROJECT_ROOT}/${coverage_path}"
  fi

  if [[ ! -f "$coverage_path" ]]; then
    fail "$spec" "coverage-cfg does not exist: $coverage_cfg"
    return
  fi

  require_pattern "$coverage_path" '^SPECIFICATION[[:space:]]+Spec$' "coverage cfg must use SPECIFICATION Spec"
  require_pattern "$coverage_path" '^CONSTANTS$' "coverage cfg must declare deterministic constants"
  require_pattern "$coverage_path" '^INVARIANT[[:space:]]+SafetyInvariants$' "coverage cfg must check SafetyInvariants"
  require_no_pattern "$coverage_path" 'TODO|FIXME|<[^>]+>' "coverage cfg must not contain placeholders"
}

shopt -s nullglob
specs=("${SPEC_DIR}"/*.tla)

if [[ ${#specs[@]} -eq 0 ]]; then
  printf 'error: no TLA+ specs found under docs/specs\n' >&2
  exit 1
fi

for spec in "${specs[@]}"; do
  base="$(basename "$spec" .tla)"
  rel="${spec#"${PROJECT_ROOT}"/}"

  if [[ ! "$base" =~ ^[a-z0-9]+(-[a-z0-9]+)*$ ]]; then
    fail "$spec" "filename must be kebab-case"
  fi

  require_pattern "$spec" '^-+ MODULE [A-Za-z][A-Za-z0-9]* -+$' "missing TLA+ module header"
  require_pattern "$spec" 'Run with TLC' "missing TLC run note"
  require_pattern "$spec" '^CONSTANTS[[:space:]]+[A-Za-z0-9_,[:space:]]+$|^CONSTANTS$' "missing CONSTANTS declaration"
  require_pattern "$spec" '^VARIABLES[[:space:]]+' "missing VARIABLES block"
  require_pattern "$spec" '^vars[[:space:]]*==' "missing vars tuple"
  require_pattern "$spec" '^Init[[:space:]]*==' "missing Init definition"
  require_pattern "$spec" '^Next[[:space:]]*==' "missing Next definition"
  require_pattern "$spec" '^Spec[[:space:]]*==[[:space:]]*Init' "missing Spec definition rooted at Init"
  require_pattern "$spec" '\[\]\[Next\]_vars' "Spec definition must include [][Next]_vars"
  require_pattern "$spec" '^SafetyInvariants[[:space:]]*==' "missing SafetyInvariants block"
  require_pattern "$spec" 'Liveness' "missing liveness/progress block"
  require_pattern "$spec" 'coverage-metric:' "missing coverage-metric comment block"
  require_pattern "$spec" 'subsystem:' "coverage-metric block missing subsystem"
  require_pattern "$spec" 'declared-invariants:' "coverage-metric block missing declared-invariants"
  require_pattern "$spec" 'max-depth:' "coverage-metric block missing max-depth"
  require_pattern "$spec" 'branching-factor:' "coverage-metric block missing branching-factor"
  require_pattern "$spec" 'threshold-pct:' "coverage-metric block missing threshold-pct"

  validate_cfg "$spec" "$base"
  validate_coverage_cfg "$spec"
  validate_mapping "$spec" "$base"

  printf 'checked: %s\n' "$rel"
done

if [[ "$failures" -ne 0 ]]; then
  printf 'spec convention check failed: %d issue(s)\n' "$failures" >&2
  exit 1
fi

printf 'spec convention check passed: %d spec(s)\n' "${#specs[@]}"
