#!/usr/bin/env bash
# Run TLC for one docs/specs TLA+ module and emit a normalized JSON summary.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

usage() {
  cat <<'USAGE'
Usage: scripts/run-tlc.sh [options] docs/specs/<spec>.tla

Options:
  --jar <path>            tla2tools.jar path (default: $TLA_TOOLS_JAR or /tmp/tla2tools.jar)
  --cfg <path>            TLC config path (default: sibling docs/specs/<spec>.cfg)
  --timeout-secs <secs>   Time budget for TLC (default: 300)
  --workers <value>       TLC worker count (default: auto)
  --out-dir <path>        Output directory (default: target/tlc/<spec>)
  --dry-run               Emit the command and schema without running TLC
  -h, --help              Show this help
USAGE
}

json_escape() {
  printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'
}

json_string_or_null() {
  if [[ -n "$1" ]]; then
    printf '"%s"' "$(json_escape "$1")"
  else
    printf 'null'
  fi
}

json_number_or_null() {
  if [[ -n "$1" ]]; then
    printf '%s' "$1"
  else
    printf 'null'
  fi
}

join_command() {
  local out=""
  local part
  for part in "$@"; do
    if [[ -n "$out" ]]; then
      out+=" "
    fi
    out+="$part"
  done
  printf '%s' "$out"
}

invariant_results_json() {
  local log_file="$1"
  local first=1
  local name

  printf '['
  while IFS= read -r name; do
    [[ -n "$name" ]] || continue
    if [[ "$first" -eq 0 ]]; then
      printf ','
    fi
    first=0
    printf '{"name":"%s","status":"pass"}' "$(json_escape "$name")"
  done < <(sed -nE 's/.*Invariant ([A-Za-z0-9_]+) is true.*/\1/p' "$log_file" | sort -u)

  while IFS= read -r name; do
    [[ -n "$name" ]] || continue
    if [[ "$first" -eq 0 ]]; then
      printf ','
    fi
    first=0
    printf '{"name":"%s","status":"fail"}' "$(json_escape "$name")"
  done < <(sed -nE 's/.*The invariant ([A-Za-z0-9_]+) is violated.*/\1/p' "$log_file" | sort -u)
  printf ']'
}

extract_states() {
  local log_file="$1"
  awk '
    {
      line = $0
      gsub(/,/, "", line)
      n = split(line, parts, /[[:space:]]+/)
      for (i = 2; i < n; i++) {
        if (parts[i] == "states" && parts[i + 1] == "generated" && parts[i - 1] ~ /^[0-9]+$/) {
          value = parts[i - 1]
        }
      }
    }
    END { if (value != "") print value }
  ' "$log_file"
}

extract_distinct_states() {
  local log_file="$1"
  awk '
    {
      line = $0
      gsub(/,/, "", line)
      n = split(line, parts, /[[:space:]]+/)
      for (i = 2; i < n - 1; i++) {
        if (parts[i] == "distinct" && parts[i + 1] == "states" && parts[i + 2] == "found" && parts[i - 1] ~ /^[0-9]+$/) {
          value = parts[i - 1]
        }
      }
    }
    END { if (value != "") print value }
  ' "$log_file"
}

emit_result() {
  local ok="$1"
  local exit_code="$2"
  local spec="$3"
  local cfg="$4"
  local jar="$5"
  local command_text="$6"
  local time_budget="$7"
  local timeout_enforced="$8"
  local timed_out="$9"
  local stdout_path="${10}"
  local stderr_path="${11}"
  local state_count="${12}"
  local distinct_state_count="${13}"
  local invariant_json="${14}"

  cat <<JSON
{
  "ok": ${ok},
  "exit-code": ${exit_code},
  "spec": $(json_string_or_null "$spec"),
  "cfg": $(json_string_or_null "$cfg"),
  "jar": $(json_string_or_null "$jar"),
  "command": $(json_string_or_null "$command_text"),
  "state-count": $(json_number_or_null "$state_count"),
  "distinct-state-count": $(json_number_or_null "$distinct_state_count"),
  "time-budget": {
    "seconds": ${time_budget},
    "enforced": ${timeout_enforced},
    "timed-out": ${timed_out}
  },
  "invariant-results": ${invariant_json},
  "artifacts": {
    "stdout": $(json_string_or_null "$stdout_path"),
    "stderr": $(json_string_or_null "$stderr_path")
  }
}
JSON
}

jar="${TLA_TOOLS_JAR:-/tmp/tla2tools.jar}"
timeout_secs=300
workers="${TLC_WORKERS:-auto}"
out_dir=""
cfg_override=""
dry_run=0
spec=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --jar)
      jar="$2"
      shift 2
      ;;
    --cfg)
      cfg_override="$2"
      shift 2
      ;;
    --timeout-secs)
      timeout_secs="$2"
      shift 2
      ;;
    --workers)
      workers="$2"
      shift 2
      ;;
    --out-dir)
      out_dir="$2"
      shift 2
      ;;
    --dry-run)
      dry_run=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    -*)
      printf 'error: unknown option: %s\n' "$1" >&2
      usage >&2
      exit 2
      ;;
    *)
      if [[ -n "$spec" ]]; then
        printf 'error: multiple spec paths supplied\n' >&2
        usage >&2
        exit 2
      fi
      spec="$1"
      shift
      ;;
  esac
done

if [[ -z "$spec" ]]; then
  usage >&2
  exit 2
fi

if [[ "$spec" != /* ]]; then
  spec="${PROJECT_ROOT}/${spec}"
fi
spec="$(cd "$(dirname "$spec")" && pwd)/$(basename "$spec")"
if [[ -n "$cfg_override" ]]; then
  cfg="$cfg_override"
  if [[ "$cfg" != /* ]]; then
    cfg="${PROJECT_ROOT}/${cfg}"
  fi
  cfg="$(cd "$(dirname "$cfg")" && pwd)/$(basename "$cfg")"
else
  cfg="${spec%.tla}.cfg"
fi
base="$(basename "$spec" .tla)"

if [[ -z "$out_dir" ]]; then
  out_dir="${PROJECT_ROOT}/target/tlc/${base}"
elif [[ "$out_dir" != /* ]]; then
  out_dir="${PROJECT_ROOT}/${out_dir}"
fi

stdout_path="${out_dir}/tlc.stdout.log"
stderr_path="${out_dir}/tlc.stderr.log"
combined_path="${out_dir}/tlc.combined.log"
summary_path="${out_dir}/summary.json"

tlc_spec="$spec"
tlc_cfg="$cfg"
module_name=""
if [[ -f "$spec" ]]; then
  module_name="$(sed -nE 's/^-+ MODULE ([A-Za-z][A-Za-z0-9]*) -+$/\1/p' "$spec" | head -n 1)"
fi
if [[ -n "$module_name" && "$base" != "$module_name" ]]; then
  tlc_module_dir="${out_dir}/module"
  tlc_spec="${tlc_module_dir}/${module_name}.tla"
  tlc_cfg="${tlc_module_dir}/${module_name}.cfg"
fi

cmd=(java -cp "$jar" tlc2.TLC -deadlock -workers "$workers" -config "$tlc_cfg" "$tlc_spec")
command_text="$(join_command "${cmd[@]}")"

if [[ "$dry_run" -eq 1 ]]; then
  emit_result true 0 "$spec" "$cfg" "$jar" "$command_text" "$timeout_secs" false false "" "" "" "" "[]"
  exit 0
fi

if [[ ! -f "$spec" ]]; then
  emit_result false 2 "$spec" "$cfg" "$jar" "$command_text" "$timeout_secs" false false "" "" "" "" "[]" >&2
  exit 2
fi
if [[ ! -f "$cfg" ]]; then
  emit_result false 2 "$spec" "$cfg" "$jar" "$command_text" "$timeout_secs" false false "" "" "" "" "[]" >&2
  exit 2
fi
if [[ ! -f "$jar" ]]; then
  emit_result false 2 "$spec" "$cfg" "$jar" "$command_text" "$timeout_secs" false false "" "" "" "" "[]" >&2
  exit 2
fi

mkdir -p "$out_dir"
if [[ "$tlc_spec" != "$spec" ]]; then
  mkdir -p "$(dirname "$tlc_spec")"
  cp "$spec" "$tlc_spec"
  cp "$cfg" "$tlc_cfg"
fi

timeout_bin=""
if command -v timeout >/dev/null 2>&1; then
  timeout_bin="timeout"
elif command -v gtimeout >/dev/null 2>&1; then
  timeout_bin="gtimeout"
fi

timeout_enforced=false
timed_out=false
set +e
if [[ -n "$timeout_bin" ]]; then
  timeout_enforced=true
  "$timeout_bin" "${timeout_secs}s" "${cmd[@]}" >"$stdout_path" 2>"$stderr_path"
  rc=$?
  if [[ "$rc" -eq 124 ]]; then
    timed_out=true
  fi
else
  "${cmd[@]}" >"$stdout_path" 2>"$stderr_path"
  rc=$?
fi
set -e

{
  printf '### stdout\n'
  cat "$stdout_path"
  printf '\n### stderr\n'
  cat "$stderr_path"
} >"$combined_path"

state_count="$(extract_states "$combined_path")"
distinct_state_count="$(extract_distinct_states "$combined_path")"
invariants="$(invariant_results_json "$combined_path")"

if [[ "$rc" -eq 0 ]]; then
  ok=true
else
  ok=false
fi

emit_result "$ok" "$rc" "$spec" "$cfg" "$jar" "$command_text" "$timeout_secs" "$timeout_enforced" "$timed_out" "$stdout_path" "$stderr_path" "$state_count" "$distinct_state_count" "$invariants" | tee "$summary_path"
exit "$rc"
