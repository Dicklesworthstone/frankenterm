#!/usr/bin/env bash
set -euo pipefail

# End-to-end bootstrap validation for frankenterm-gui.
# Exit codes:
#   0 = pass
#   1 = fail
#   2 = all steps skipped (for example, dry-run / unavailable environment)

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

RCH_BIN="${RCH_BIN:-rch}"
GUI_TARGET_DIR="${GUI_TARGET_DIR:-$PROJECT_ROOT/target/e2e-gui-bootstrap}"
BUILD_PROFILE="${BUILD_PROFILE:-release}"
GUI_TARGET_DIR_REL=""
GUI_TARGET_DIR_IN_REPO=0
GUI_BIN=""
LOG_DIR="${LOG_DIR:-$PROJECT_ROOT/target/e2e/gui-bootstrap}"
BUNDLE_OUTPUT_DIR="${BUNDLE_OUTPUT_DIR:-$LOG_DIR/bundle-${BUILD_PROFILE}-$$}"
LAUNCH_TIMEOUT_SECS="${LAUNCH_TIMEOUT_SECS:-3}"
RUN_GUI_LAUNCH="${RUN_GUI_LAUNCH:-0}"
RCH_PROBE_LOG="${RCH_PROBE_LOG:-$LOG_DIR/rch-workers-probe.json}"
DRY_RUN=0
SKIP_BUILD=0
SKIP_BUNDLE=0

step_index=0
pass_count=0
fail_count=0
skip_count=0
LAST_SKIP_REASON=""
BUILD_STEP_STATUS="not_run"
BUILD_STEP_DETAIL=""

mkdir -p "$LOG_DIR"

usage() {
  cat <<'EOF'
Usage: scripts/e2e_gui_bootstrap.sh [options]

Options:
  --dry-run       Print actions without executing them
  --skip-build    Skip build step (expects existing binary)
  --skip-bundle   Skip macOS bundle validation step
  -h, --help      Show help

Environment:
  RCH_BIN              rch executable (default: rch)
  GUI_TARGET_DIR       Cargo target dir for build artifacts
  BUILD_PROFILE        Cargo profile (default: release)
  RUN_GUI_LAUNCH       Set to 1 to run GUI launch smoke step
  LAUNCH_TIMEOUT_SECS  GUI smoke timeout seconds (default: 3)
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run) DRY_RUN=1; shift ;;
    --skip-build) SKIP_BUILD=1; shift ;;
    --skip-bundle) SKIP_BUNDLE=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown option: $1" >&2; usage; exit 1 ;;
  esac
done

now_ms() {
  python3 - <<'PY'
import time
print(int(time.time() * 1000))
PY
}

log() {
  printf '%s\n' "$*"
}

emit_step_json() {
  local status="$1"
  local name="$2"
  local duration_ms="$3"
  local detail="$4"
  printf '{"step":%d,"name":"%s","status":"%s","duration_ms":%d,"detail":"%s"}\n' \
    "$step_index" "$name" "$status" "$duration_ms" "${detail//\"/\\\"}" >&2
}

mark_skip() {
  LAST_SKIP_REASON="$1"
  return 0
}

skip_if_build_failed() {
  local dependent_surface="$1"
  if [[ "$BUILD_STEP_STATUS" == "failed" ]]; then
    local detail="${BUILD_STEP_DETAIL:-build step failed}"
    mark_skip "${detail}; ${dependent_surface}"
    return 125
  fi
  return 0
}

run_cmd() {
  if [[ "$DRY_RUN" -eq 1 ]]; then
    log "[DRY-RUN] $*"
    return 0
  fi
  "$@"
}

resolve_project_path_info() {
  python3 - "$PROJECT_ROOT" "$1" <<'PY'
import os
import sys

root = os.path.realpath(sys.argv[1])
value = sys.argv[2]
abs_path = os.path.realpath(value if os.path.isabs(value) else os.path.join(root, value))
rel_path = os.path.relpath(abs_path, root)
in_repo = rel_path == "." or (rel_path != ".." and not rel_path.startswith(f"..{os.sep}"))

print(abs_path)
print(rel_path)
print("1" if in_repo else "0")
PY
}

normalize_gui_target_dir() {
  local -a info
  mapfile -t info < <(resolve_project_path_info "$GUI_TARGET_DIR")
  GUI_TARGET_DIR="${info[0]}"
  GUI_TARGET_DIR_REL="${info[1]}"
  GUI_TARGET_DIR_IN_REPO="${info[2]}"
  GUI_BIN="$GUI_TARGET_DIR/$BUILD_PROFILE/frankenterm-gui"
}

require_remote_safe_gui_target_dir() {
  if [[ "$GUI_TARGET_DIR_IN_REPO" == "1" ]]; then
    return 0
  fi

  BUILD_STEP_STATUS="failed"
  BUILD_STEP_DETAIL="build step failed (GUI_TARGET_DIR must stay under repo root for rch offload)"
  log "GUI_TARGET_DIR '$GUI_TARGET_DIR' is outside project root '$PROJECT_ROOT'."
  log "Use a repo-relative target dir (for example target/e2e-gui-bootstrap) when offloading via rch."
  return 1
}

run_rch_gui_build() {
  (
    cd "$PROJECT_ROOT"
    run_cmd "$RCH_BIN" exec -- env CARGO_TARGET_DIR="$GUI_TARGET_DIR_REL" \
      cargo build --profile "$BUILD_PROFILE" --bin frankenterm-gui --manifest-path Cargo.toml
  )
}

run_rch_gui_prereq_check() {
  (
    cd "$PROJECT_ROOT"
    "$RCH_BIN" exec -- sh -lc '
      case "$(uname -s)" in
        Linux)
          if ! command -v pkg-config >/dev/null 2>&1; then
            echo "FT_GUI_REMOTE_PREREQ_MISSING:pkg-config" >&2
            exit 41
          fi
          if ! pkg-config --exists x11; then
            echo "FT_GUI_REMOTE_PREREQ_MISSING:x11" >&2
            pkg-config --print-errors --exists x11 || true
            exit 42
          fi
          ;;
      esac
    '
  )
}

ensure_remote_gui_prereqs() {
  local preflight_log="$LOG_DIR/gui-build-preflight.log"
  : > "$preflight_log"

  if run_rch_gui_prereq_check > >(tee "$preflight_log") 2> >(tee -a "$preflight_log" >&2); then
    return 0
  fi

  if grep -q 'FT_GUI_REMOTE_PREREQ_MISSING:x11' "$preflight_log"; then
    BUILD_STEP_STATUS="failed"
    BUILD_STEP_DETAIL="build step failed (remote worker missing pkg-config x11 / x11.pc)"
    log "Remote worker is missing X11 development metadata required for frankenterm-gui."
    log "frankenterm/window has a hard x11 dependency on Linux; provision x11 dev packages on the RCH workers."
    log "See $preflight_log for the remote preflight output."
    return 1
  fi

  if grep -q 'FT_GUI_REMOTE_PREREQ_MISSING:pkg-config' "$preflight_log"; then
    BUILD_STEP_STATUS="failed"
    BUILD_STEP_DETAIL="build step failed (remote worker missing pkg-config)"
    log "Remote worker is missing pkg-config, so frankenterm-gui Linux prerequisites cannot be checked."
    log "See $preflight_log for the remote preflight output."
    return 1
  fi

  BUILD_STEP_STATUS="failed"
  BUILD_STEP_DETAIL="build step failed (remote GUI prerequisite check failed)"
  log "Remote GUI prerequisite check failed before cargo build."
  log "See $preflight_log for the remote preflight output."
  return 1
}

normalize_gui_target_dir

run_step() {
  local name="$1"
  shift
  step_index=$((step_index + 1))
  local start_ms end_ms duration_ms
  start_ms="$(now_ms)"

  if "$@"; then
    end_ms="$(now_ms)"
    duration_ms=$((end_ms - start_ms))
    pass_count=$((pass_count + 1))
    log "[PASS] ${step_index}. ${name} (${duration_ms}ms)"
    emit_step_json "pass" "$name" "$duration_ms" "ok"
    return 0
  else
    local rc=$?
    end_ms="$(now_ms)"
    duration_ms=$((end_ms - start_ms))

    if [[ "$rc" -eq 125 ]]; then
      skip_count=$((skip_count + 1))
      local detail="${LAST_SKIP_REASON:-skipped}"
      LAST_SKIP_REASON=""
      log "[SKIP] ${step_index}. ${name} (${detail})"
      emit_step_json "skip" "$name" "$duration_ms" "$detail"
      return 0
    fi

    fail_count=$((fail_count + 1))
    log "[FAIL] ${step_index}. ${name} (${duration_ms}ms)"
    emit_step_json "fail" "$name" "$duration_ms" "failed"
    return 1
  fi
}

require_rch() {
  command -v "$RCH_BIN" >/dev/null 2>&1
}

probe_rch_workers() {
  local probe_json
  probe_json="$("$RCH_BIN" workers probe --json --all)"
  printf '%s\n' "$probe_json" > "$RCH_PROBE_LOG"

  python3 - <<'PY' "$RCH_PROBE_LOG"
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as handle:
    payload = json.load(handle)

for worker in payload.get("data", []):
    status = str(worker.get("status", "")).strip().lower()
    if status and not status.endswith("_failed") and status not in {
        "connection_failed",
        "error",
        "failed",
        "unreachable",
    }:
        sys.exit(0)

sys.exit(1)
PY
}

build_gui() {
  if [[ "$SKIP_BUILD" -eq 1 ]]; then
    BUILD_STEP_STATUS="skipped"
    BUILD_STEP_DETAIL="build step skipped"
    mark_skip "--skip-build set"
    return 125
  fi
  if ! require_remote_safe_gui_target_dir; then
    return 1
  fi
  if [[ "$DRY_RUN" -eq 1 ]]; then
    BUILD_STEP_STATUS="dry-run"
    BUILD_STEP_DETAIL="dry-run"
    run_rch_gui_build
    return 0
  fi
  if ! require_rch; then
    BUILD_STEP_STATUS="failed"
    BUILD_STEP_DETAIL="build step failed (rch not found)"
    log "rch not found at '$RCH_BIN'"
    return 1
  fi
  if ! probe_rch_workers; then
    BUILD_STEP_STATUS="failed"
    BUILD_STEP_DETAIL="build step failed (no reachable RCH workers)"
    log "No reachable RCH workers detected; refusing local cargo fallback."
    log "See $RCH_PROBE_LOG for probe details."
    return 1
  fi
  if ! ensure_remote_gui_prereqs; then
    return 1
  fi
  if run_rch_gui_build; then
    BUILD_STEP_STATUS="success"
    BUILD_STEP_DETAIL=""
    return 0
  fi

  BUILD_STEP_STATUS="failed"
  BUILD_STEP_DETAIL="build step failed (rch cargo build returned non-zero)"
  return 1
}

verify_binary_exists() {
  if [[ "$DRY_RUN" -eq 1 ]]; then
    mark_skip "dry-run (binary not materialized)"
    return 125
  fi
  skip_if_build_failed "GUI binary unavailable" || return $?
  [[ -x "$GUI_BIN" ]]
}

verify_binary_format() {
  if [[ "$DRY_RUN" -eq 1 ]]; then
    mark_skip "dry-run (binary format check skipped)"
    return 125
  fi
  skip_if_build_failed "GUI binary unavailable" || return $?
  local file_info
  file_info="$(file "$GUI_BIN")"
  log "$file_info"
  if [[ "$(uname -s)" == "Darwin" ]]; then
    [[ "$file_info" == *"Mach-O"* ]] || return 1
    if [[ "$(uname -m)" == "arm64" ]]; then
      [[ "$file_info" == *"arm64"* ]] || return 1
    fi
  fi
  return 0
}

verify_help_output() {
  skip_if_build_failed "GUI binary unavailable" || return $?
  run_cmd "$GUI_BIN" --help >/dev/null
}

verify_version_output() {
  skip_if_build_failed "GUI binary unavailable" || return $?
  local out
  out="$(run_cmd "$GUI_BIN" --version)"
  [[ -n "$out" ]]
}

validate_macos_bundle() {
  if [[ "$(uname -s)" != "Darwin" ]]; then
    mark_skip "non-macOS host"
    return 125
  fi
  if [[ "$SKIP_BUNDLE" -eq 1 ]]; then
    mark_skip "--skip-bundle set"
    return 125
  fi
  if [[ ! -x "$PROJECT_ROOT/scripts/create-macos-bundle.sh" ]]; then
    mark_skip "create-macos-bundle.sh missing"
    return 125
  fi
  if [[ ! -d "/Applications/WezTerm.app" ]]; then
    mark_skip "/Applications/WezTerm.app missing"
    return 125
  fi
  if [[ "$DRY_RUN" -eq 1 ]]; then
    mark_skip "dry-run (bundle validation skipped)"
    return 125
  fi
  skip_if_build_failed "bundle inputs unavailable" || return $?

  mkdir -p "$BUNDLE_OUTPUT_DIR"
  run_cmd "$PROJECT_ROOT/scripts/create-macos-bundle.sh" --skip-build --output "$BUNDLE_OUTPUT_DIR"

  local app="$BUNDLE_OUTPUT_DIR/FrankenTerm.app"
  [[ -d "$app" ]] || return 1
  [[ -f "$app/Contents/Info.plist" ]] || return 1
  [[ -f "$app/Contents/PkgInfo" ]] || return 1
  [[ -f "$app/Contents/Resources/ft.icns" ]] || return 1
  [[ -f "$app/Contents/MacOS/ft" ]] || return 1
  [[ -f "$app/Contents/MacOS/wezterm-gui" || -f "$app/Contents/MacOS/frankenterm-gui" ]] || return 1

  if command -v codesign >/dev/null 2>&1; then
    run_cmd codesign --verify --deep "$app"
  fi
}

launch_gui_smoke() {
  if [[ "$RUN_GUI_LAUNCH" != "1" ]]; then
    mark_skip "RUN_GUI_LAUNCH!=1"
    return 125
  fi

  local gui_log="$LOG_DIR/gui-launch.log"
  local gui_pid=""
  if [[ "$DRY_RUN" -eq 1 ]]; then
    log "[DRY-RUN] $GUI_BIN --skip-config start --always-new-process > $gui_log 2>&1 &"
    return 0
  fi
  skip_if_build_failed "GUI binary unavailable" || return $?

  "$GUI_BIN" --skip-config start --always-new-process >"$gui_log" 2>&1 &
  gui_pid="$!"

  sleep "$LAUNCH_TIMEOUT_SECS"
  if ! kill -0 "$gui_pid" 2>/dev/null; then
    log "GUI exited before timeout; tail follows:"
    tail -n 50 "$gui_log" || true
    return 1
  fi

  kill -TERM "$gui_pid" 2>/dev/null || true
  wait "$gui_pid" 2>/dev/null || true
  return 0
}

main() {
  log "e2e_gui_bootstrap: starting"
  log "project_root=$PROJECT_ROOT"
  log "target_dir=$GUI_TARGET_DIR"
  log "profile=$BUILD_PROFILE"
  log "gui_bin=$GUI_BIN"

  run_step "build frankenterm-gui via rch" build_gui || true
  run_step "verify GUI binary exists" verify_binary_exists || true
  run_step "verify GUI binary format" verify_binary_format || true
  run_step "verify --help" verify_help_output || true
  run_step "verify --version" verify_version_output || true
  run_step "macOS bundle structure" validate_macos_bundle || true
  run_step "GUI launch smoke" launch_gui_smoke || true

  log ""
  log "Summary: pass=${pass_count} fail=${fail_count} skip=${skip_count} total=${step_index}"
  if [[ "$fail_count" -gt 0 ]]; then
    exit 1
  fi
  if [[ "$pass_count" -eq 0 && "$skip_count" -gt 0 ]]; then
    exit 2
  fi
  exit 0
}

main "$@"
