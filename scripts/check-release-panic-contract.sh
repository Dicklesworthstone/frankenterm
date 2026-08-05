#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PROFILES_ONLY=false
if [[ $# -eq 1 && "$1" == "--profiles-only" ]]; then
  PROFILES_ONLY=true
elif [[ $# -eq 2 ]]; then
  # Execute already-built artifacts only. Cargo/profile selection belongs to
  # the strict remote caller so this script cannot silently rebuild under
  # test/debug.
  INTERACTIVE_PROBE="$1"
  ABORT_PROBE="$2"
else
  echo "usage: $0 --profiles-only" >&2
  echo "   or: $0 <release-interactive-probe> <release-abort-probe>" >&2
  exit 2
fi

# Machine-check the complete profile contract rather than trusting comments or
# the behavior of one probe build. This also proves that no second explicitly
# aborting profile can accidentally become a packaging candidate.
python3 - "$REPO_ROOT/Cargo.toml" <<'PY'
import pathlib
import re
import sys

manifest_path = pathlib.Path(sys.argv[1])
manifest_text = manifest_path.read_text(encoding="utf-8")
try:
    import tomllib
except ModuleNotFoundError:
    # Apple still ships Python versions older than 3.11 on some supported
    # hosts. Parse only the scalar profile tables this contract owns rather
    # than turning a release-safety check into an ambient Python-package
    # dependency.
    profiles = {}
    current = None
    section_pattern = re.compile(r"^\[profile\.([A-Za-z0-9-]+)\]$")
    for raw_line in manifest_text.splitlines():
        line = raw_line.split("#", 1)[0].strip()
        if not line:
            continue
        section = section_pattern.fullmatch(line)
        if section:
            current = profiles.setdefault(section.group(1), {})
            continue
        if line.startswith("["):
            current = None
            continue
        if current is None or "=" not in line:
            continue
        key, raw_value = (part.strip() for part in line.split("=", 1))
        if raw_value.startswith('"') and raw_value.endswith('"'):
            value = raw_value[1:-1]
        elif raw_value in {"true", "false"}:
            value = raw_value == "true"
        elif re.fullmatch(r"[0-9]+", raw_value):
            value = int(raw_value)
        else:
            raise SystemExit(
                f"unsupported scalar in fallback Cargo profile parser: {line!r}"
            )
        current[key] = value
else:
    profiles = tomllib.loads(manifest_text).get("profile", {})

expected = {
    "release": {
        "opt-level": "z",
        "lto": True,
        "codegen-units": 1,
        "panic": "unwind",
        "strip": True,
        "debug": False,
    },
    "release-interactive": {
        "inherits": "release",
        "panic": "unwind",
    },
    "release-abort-probe": {
        "inherits": "release",
        "panic": "abort",
    },
    "release-perf": {
        "inherits": "release",
        "opt-level": 3,
        "lto": "thin",
        "debug": "line-tables-only",
        "panic": "unwind",
        "strip": "none",
    },
}

for profile_name, required in expected.items():
    actual = profiles.get(profile_name)
    if not isinstance(actual, dict):
        raise SystemExit(f"missing required Cargo profile: {profile_name}")
    for key, expected_value in required.items():
        actual_value = actual.get(key)
        if actual_value != expected_value:
            raise SystemExit(
                f"Cargo profile drift: profile.{profile_name}.{key}="
                f"{actual_value!r}, expected {expected_value!r}"
            )

aborting = sorted(
    name
    for name, settings in profiles.items()
    if isinstance(settings, dict) and settings.get("panic") == "abort"
)
if aborting != ["release-abort-probe"]:
    raise SystemExit(
        "release-abort-probe must be the only explicitly aborting profile; "
        f"observed {aborting!r}"
    )
PY

# shellcheck disable=SC2016
# This is a literal ERE: `$` anchors and backticks are intentionally not shell
# expansions.
STALE_SHIPPED_PROFILE_PATTERN='cargo (build|install).*((--release|--profile release-abort-probe).*(frankenterm-gui|frankenterm-mux-server|-p frankenterm([[:space:]`]|$)|--bin ft([[:space:]`]|$))|(frankenterm-gui|frankenterm-mux-server|-p frankenterm([[:space:]`]|$)|--bin ft([[:space:]`]|$)).*(--release|--profile release-abort-probe))|cargo build --release([[:space:]]+--features[[:space:]]+[^[:space:]#]+)?([[:space:]]*(#.*)?)?$|target/(release|release-abort-probe)/(ft|frankenterm-gui|frankenterm-mux-server)'
set +e
stale_shipped_profile_refs="$(
  grep -R -n -E --include='*.md' --include='*.tape' \
    "$STALE_SHIPPED_PROFILE_PATTERN" \
    "$REPO_ROOT/docs" \
    "$REPO_ROOT/AGENTS.md" \
    "$REPO_ROOT/README.md" \
    "$REPO_ROOT/PLAN.md" \
    "$REPO_ROOT/PLAN_CODEX.md" \
    "$REPO_ROOT/frankenterm_guide.md" \
    "$REPO_ROOT/scripts/demo.tape" \
    "$REPO_ROOT/scripts/demo-full.tape"
)"
stale_shipped_profile_status=$?
set -e
if [[ $stale_shipped_profile_status -eq 0 ]]; then
  echo "shipped-process build instructions bypass release-interactive identity:" >&2
  echo "$stale_shipped_profile_refs" >&2
  exit 1
fi
if [[ $stale_shipped_profile_status -ne 1 ]]; then
  echo "could not audit shipped-process profile instructions" >&2
  exit 1
fi

set +e
stale_packaging_script_refs="$(
  grep -n -E "$STALE_SHIPPED_PROFILE_PATTERN" \
    "$REPO_ROOT/install.sh" \
    "$REPO_ROOT/scripts/create-macos-bundle.sh"
)"
stale_packaging_script_status=$?
set -e
if [[ $stale_packaging_script_status -eq 0 ]]; then
  echo "packaging scripts bypass release-interactive identity:" >&2
  echo "$stale_packaging_script_refs" >&2
  exit 1
fi
if [[ $stale_packaging_script_status -ne 1 ]]; then
  echo "could not audit packaging-script profile selection" >&2
  exit 1
fi

STALE_RELEASE_WORKFLOW_PATTERN='panic\.cli=abort|target/\$\{\{ matrix\.target \}\}/(release|release-abort-probe)/\$\{\{ matrix\.artifact \}\}|cargo build --release --target'
set +e
stale_release_workflow_refs="$(
  grep -n -E "$STALE_RELEASE_WORKFLOW_PATTERN" \
    "$REPO_ROOT/.github/workflows/release.yml" \
    "$REPO_ROOT/.github/workflows/ci.yml"
)"
stale_release_workflow_status=$?
set -e
if [[ $stale_release_workflow_status -eq 0 ]]; then
  echo "release workflow still packages outside the release-interactive identity:" >&2
  echo "$stale_release_workflow_refs" >&2
  exit 1
fi
if [[ $stale_release_workflow_status -ne 1 ]]; then
  echo "could not audit shipped workflow profile selection" >&2
  exit 1
fi

if [[ "$PROFILES_ONLY" == true ]]; then
  echo "PANIC_PROFILE_STATIC_CONTRACT_SUCCESS"
  exit 0
fi

for probe in "$INTERACTIVE_PROBE" "$ABORT_PROBE"; do
  if [[ ! -x "$probe" ]]; then
    echo "panic-contract probe is not executable: $probe" >&2
    exit 2
  fi
done

interactive_profile="$(basename "$(dirname "$(dirname "$INTERACTIVE_PROBE")")")"
abort_profile="$(basename "$(dirname "$(dirname "$ABORT_PROBE")")")"
if [[ "$interactive_profile" != "release-interactive" ]]; then
  echo "interactive probe is not the release-interactive artifact: $INTERACTIVE_PROBE" >&2
  exit 2
fi
if [[ "$abort_profile" != "release-abort-probe" ]]; then
  echo "abort probe is not the release-abort-probe artifact: $ABORT_PROBE" >&2
  exit 2
fi

EVIDENCE_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/ft-panic-contract.XXXXXX")"
SECRET_SENTINEL="FT_PANIC_SECRET_SENTINEL_DO_NOT_REFLECT"

assert_empty_file() {
  local path="$1"
  if [[ -s "$path" ]]; then
    echo "expected empty file: $path" >&2
    sed -n '1,20p' "$path" >&2
    exit 1
  fi
}

assert_no_secret() {
  local path="$1"
  if grep -R -F -q "$SECRET_SENTINEL" "$path"; then
    echo "panic payload leaked into evidence: $path" >&2
    exit 1
  fi
}

# Execute a real std-printing EPIPE deterministically. The child receives the
# write end of a pipe after every read descriptor has been closed, so its first
# `println!` reaches Rust's `library/std/src/io/stdio.rs` failure path. A forged
# `panic_any` string is tested separately and must remain fatal.
run_with_closed_stdout() {
  local probe="$1"
  local scenario="$2"
  local crash_root="$3"
  local stderr_path="$4"
  local status_path="$5"
  python3 - "$probe" "$scenario" "$crash_root" "$stderr_path" "$status_path" <<'PY'
import os
import pathlib
import subprocess
import sys

probe, scenario, crash_root, stderr_path, status_path = sys.argv[1:]
read_fd, write_fd = os.pipe()
os.close(read_fd)
env = os.environ.copy()
env["FT_PANIC_PROBE_CRASH_DIR"] = crash_root
try:
    with open(stderr_path, "wb") as stderr:
        process = subprocess.Popen(
            [probe, scenario],
            stdout=write_fd,
            stderr=stderr,
            env=env,
            close_fds=True,
        )
finally:
    os.close(write_fd)
status = process.wait()
pathlib.Path(status_path).write_text(f"{status}\n", encoding="ascii")
PY
}

assert_one_sanitized_crash_bundle() {
  local crash_root="$1"
  python3 - "$crash_root" "$SECRET_SENTINEL" <<'PY'
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
secret = sys.argv[2]
bundles = sorted(path for path in root.glob("ft_crash_*") if path.is_dir())
if len(bundles) != 1:
    raise SystemExit(f"expected exactly one crash bundle, observed {len(bundles)}")
bundle = bundles[0]
manifest = json.loads((bundle / "manifest.json").read_text(encoding="utf-8"))
report = json.loads((bundle / "crash_report.json").read_text(encoding="utf-8"))
if manifest.get("files", []).count("crash_report.json") != 1:
    raise SystemExit("manifest must name exactly one crash report")
expected = "FrankenTerm: fatal internal error; diagnostic details were suppressed"
if report.get("message") != expected:
    raise SystemExit(f"unexpected fatal report message: {report.get('message')!r}")
for path in bundle.rglob("*"):
    if path.is_file() and secret in path.read_text(encoding="utf-8", errors="replace"):
        raise SystemExit(f"panic payload leaked into {path}")
print(f"fatal_bundle={bundle}")
PY
}

for site in mux-pane-callback mux-subscriber mux-pane-retirement storage-writer epipe-spoof; do
  case "$site" in
    mux-pane-callback) expected_label="mux.pane_callback" ;;
    mux-subscriber) expected_label="mux.subscriber" ;;
    mux-pane-retirement) expected_label="mux.pane_retirement" ;;
    storage-writer) expected_label="storage.writer" ;;
    epipe-spoof) expected_label="mux.pane_callback" ;;
  esac
  run_root="$EVIDENCE_ROOT/caught-$site"
  crash_root="$run_root/crashes"
  mkdir -p "$crash_root"
  FT_PANIC_PROBE_CRASH_DIR="$crash_root" \
    "$INTERACTIVE_PROBE" caught "$site" >"$run_root.stdout" 2>"$run_root.stderr"
  grep -F -x -q \
    "recovered site=$expected_label alive=true recovered_delta=1" \
    "$run_root.stdout"
  assert_empty_file "$run_root.stderr"
  if compgen -G "$crash_root/ft_crash_*" >/dev/null; then
    echo "caught panic created a fatal crash bundle for $site" >&2
    exit 1
  fi
  assert_no_secret "$run_root.stdout"
  assert_no_secret "$run_root.stderr"
done

# Exercise the GUI->base chain without starting a GUI or creating a window.
# A caller-controlled EPIPE-looking payload inside the audited marker must be
# contained, not misclassified as a real closed pipeline.
gui_spoof_root="$EVIDENCE_ROOT/gui-caught-epipe-spoof"
mkdir -p "$gui_spoof_root/crashes"
FT_PANIC_PROBE_CRASH_DIR="$gui_spoof_root/crashes" \
  "$INTERACTIVE_PROBE" gui-caught-epipe-spoof >"$gui_spoof_root.stdout" \
  2>"$gui_spoof_root.stderr"
grep -F -x -q \
  'recovered site=mux.pane_callback alive=true recovered_delta=1' \
  "$gui_spoof_root.stdout"
assert_empty_file "$gui_spoof_root.stderr"
if compgen -G "$gui_spoof_root/crashes/ft_crash_*" >/dev/null; then
  echo "GUI-contained EPIPE spoof created a fatal crash bundle" >&2
  exit 1
fi

payload_once_root="$EVIDENCE_ROOT/payload-drop-once"
mkdir -p "$payload_once_root/crashes"
FT_PANIC_PROBE_CRASH_DIR="$payload_once_root/crashes" \
  "$INTERACTIVE_PROBE" payload-drop-once >"$payload_once_root.stdout" \
  2>"$payload_once_root.stderr"
grep -F -x -q 'first-payload-drop-contained marker=false recovered_delta=2' \
  "$payload_once_root.stdout"
assert_empty_file "$payload_once_root.stderr"
if compgen -G "$payload_once_root/crashes/ft_crash_*" >/dev/null; then
  echo "contained payload destructor panic created a crash bundle" >&2
  exit 1
fi
assert_no_secret "$payload_once_root"

payload_twice_root="$EVIDENCE_ROOT/payload-drop-twice"
mkdir -p "$payload_twice_root/crashes"
set +e
FT_PANIC_PROBE_CRASH_DIR="$payload_twice_root/crashes" \
  "$INTERACTIVE_PROBE" payload-drop-twice >"$payload_twice_root.stdout" \
  2>"$payload_twice_root.stderr"
payload_twice_status=$?
set -e
if [[ $payload_twice_status -ne 134 ]]; then
  echo "poisoned repeated recovery exited $payload_twice_status instead of 134" >&2
  exit 1
fi
grep -F -x -q 'first-payload-drop-contained marker=false recovered_delta=2' \
  "$payload_twice_root.stdout"
grep -F -x -q \
  'FrankenTerm: fatal internal error; diagnostic details were suppressed' \
  "$payload_twice_root.stderr"
if [[ $(wc -l <"$payload_twice_root.stderr") -ne 1 ]]; then
  echo "poisoned repeated recovery must emit one generic fatal report" >&2
  exit 1
fi
if compgen -G "$payload_twice_root/crashes/ft_crash_*" >/dev/null; then
  echo "payload-disposal poison created a duplicate crash bundle" >&2
  exit 1
fi
assert_no_secret "$payload_twice_root"

# Rust cannot catch a second panic that begins while a thread is already
# unwinding. An outer recovery marker must therefore be overridden while a
# wrapped future is torn down, so the fatal destructor panic reaches the crash
# and base hooks before the runtime aborts.
nested_drop_root="$EVIDENCE_ROOT/nested-drop-panic"
mkdir -p "$nested_drop_root/crashes"
set +e
(
  ulimit -c 0
  exec env FT_PANIC_PROBE_CRASH_DIR="$nested_drop_root/crashes" \
    "$INTERACTIVE_PROBE" nested-drop-panic
) >"$nested_drop_root.stdout" 2>"$nested_drop_root.stderr"
nested_drop_status=$?
set -e
if [[ $nested_drop_status -ne 134 ]]; then
  echo "nested destructor panic exited $nested_drop_status instead of 134" >&2
  exit 1
fi
assert_empty_file "$nested_drop_root.stdout"
grep -F -q \
  'FrankenTerm: fatal internal error; diagnostic details were suppressed' \
  "$nested_drop_root.stderr"
assert_one_sanitized_crash_bundle "$nested_drop_root/crashes"
assert_no_secret "$nested_drop_root"

fatal_root="$EVIDENCE_ROOT/uncaught"
fatal_crash_root="$fatal_root/crashes"
mkdir -p "$fatal_crash_root"
set +e
FT_PANIC_PROBE_CRASH_DIR="$fatal_crash_root" \
  "$INTERACTIVE_PROBE" uncaught >"$fatal_root.stdout" 2>"$fatal_root.stderr"
fatal_status=$?
set -e
if [[ $fatal_status -ne 101 ]]; then
  echo "uncaught unwind panic exited $fatal_status instead of 101" >&2
  exit 1
fi
assert_empty_file "$fatal_root.stdout"
grep -F -x -q \
  'FrankenTerm: fatal internal error; diagnostic details were suppressed' \
  "$fatal_root.stderr"
if [[ $(wc -l <"$fatal_root.stderr") -ne 1 ]]; then
  echo "uncaught panic must emit exactly one generic base report" >&2
  exit 1
fi

assert_one_sanitized_crash_bundle "$fatal_crash_root"

# Exact payload text is insufficient to claim EPIPE. This panic originates in
# application code, so it must remain fatal even though the string is byte-for-
# byte identical to std's printing error.
spoof_fatal_root="$EVIDENCE_ROOT/uncaught-epipe-spoof"
spoof_fatal_crash_root="$spoof_fatal_root/crashes"
mkdir -p "$spoof_fatal_crash_root"
set +e
FT_PANIC_PROBE_CRASH_DIR="$spoof_fatal_crash_root" \
  "$INTERACTIVE_PROBE" uncaught-epipe-spoof >"$spoof_fatal_root.stdout" \
  2>"$spoof_fatal_root.stderr"
spoof_fatal_status=$?
set -e
if [[ $spoof_fatal_status -ne 101 ]]; then
  echo "unmarked EPIPE-string spoof exited $spoof_fatal_status instead of 101" >&2
  exit 1
fi
assert_empty_file "$spoof_fatal_root.stdout"
grep -F -x -q \
  'FrankenTerm: fatal internal error; diagnostic details were suppressed' \
  "$spoof_fatal_root.stderr"
if [[ $(wc -l <"$spoof_fatal_root.stderr") -ne 1 ]]; then
  echo "unmarked EPIPE-string spoof must emit one fatal report" >&2
  exit 1
fi
assert_one_sanitized_crash_bundle "$spoof_fatal_crash_root"

gui_fatal_root="$EVIDENCE_ROOT/gui-uncaught"
gui_fatal_crash_root="$gui_fatal_root/crashes"
mkdir -p "$gui_fatal_crash_root"
set +e
FT_PANIC_PROBE_CRASH_DIR="$gui_fatal_crash_root" \
  "$INTERACTIVE_PROBE" gui-uncaught >"$gui_fatal_root.stdout" \
  2>"$gui_fatal_root.stderr"
gui_fatal_status=$?
set -e
if [[ $gui_fatal_status -ne 101 ]]; then
  echo "GUI-chain unwind panic exited $gui_fatal_status instead of 101" >&2
  exit 1
fi
assert_empty_file "$gui_fatal_root.stdout"
grep -F -x -q 'PROBE_GUI_GENERIC_FATAL_REPORT' "$gui_fatal_root.stderr"
if [[ $(wc -l <"$gui_fatal_root.stderr") -ne 1 ]]; then
  echo "GUI-chain panic must emit exactly one GUI-layer report" >&2
  exit 1
fi
assert_one_sanitized_crash_bundle "$gui_fatal_crash_root"
assert_no_secret "$gui_fatal_root"

gui_spoof_fatal_root="$EVIDENCE_ROOT/gui-uncaught-epipe-spoof"
gui_spoof_fatal_crash_root="$gui_spoof_fatal_root/crashes"
mkdir -p "$gui_spoof_fatal_crash_root"
set +e
FT_PANIC_PROBE_CRASH_DIR="$gui_spoof_fatal_crash_root" \
  "$INTERACTIVE_PROBE" gui-uncaught-epipe-spoof \
  >"$gui_spoof_fatal_root.stdout" 2>"$gui_spoof_fatal_root.stderr"
gui_spoof_fatal_status=$?
set -e
if [[ $gui_spoof_fatal_status -ne 101 ]]; then
  echo "GUI-chain EPIPE-string spoof exited $gui_spoof_fatal_status instead of 101" >&2
  exit 1
fi
assert_empty_file "$gui_spoof_fatal_root.stdout"
grep -F -x -q 'PROBE_GUI_GENERIC_FATAL_REPORT' \
  "$gui_spoof_fatal_root.stderr"
if [[ $(wc -l <"$gui_spoof_fatal_root.stderr") -ne 1 ]]; then
  echo "GUI-chain EPIPE-string spoof must emit one GUI-layer report" >&2
  exit 1
fi
assert_one_sanitized_crash_bundle "$gui_spoof_fatal_crash_root"

epipe_root="$EVIDENCE_ROOT/epipe"
mkdir -p "$epipe_root/crashes"
: >"$epipe_root.stdout"
run_with_closed_stdout "$INTERACTIVE_PROBE" epipe "$epipe_root/crashes" \
  "$epipe_root.stderr" "$epipe_root.status"
epipe_status="$(<"$epipe_root.status")"
if [[ $epipe_status -ne 141 ]]; then
  echo "EPIPE probe exited $epipe_status instead of 141" >&2
  exit 1
fi
assert_empty_file "$epipe_root.stdout"
assert_empty_file "$epipe_root.stderr"
if compgen -G "$epipe_root/crashes/ft_crash_*" >/dev/null; then
  echo "EPIPE created a crash bundle" >&2
  exit 1
fi

gui_epipe_root="$EVIDENCE_ROOT/gui-epipe"
mkdir -p "$gui_epipe_root/crashes"
: >"$gui_epipe_root.stdout"
run_with_closed_stdout "$INTERACTIVE_PROBE" gui-epipe \
  "$gui_epipe_root/crashes" "$gui_epipe_root.stderr" \
  "$gui_epipe_root.status"
gui_epipe_status="$(<"$gui_epipe_root.status")"
if [[ $gui_epipe_status -ne 141 ]]; then
  echo "GUI-chain EPIPE probe exited $gui_epipe_status instead of 141" >&2
  exit 1
fi
assert_empty_file "$gui_epipe_root.stdout"
assert_empty_file "$gui_epipe_root.stderr"
if compgen -G "$gui_epipe_root/crashes/ft_crash_*" >/dev/null; then
  echo "GUI-chain EPIPE created a crash bundle" >&2
  exit 1
fi

abort_epipe_root="$EVIDENCE_ROOT/abort-epipe"
mkdir -p "$abort_epipe_root/crashes"
: >"$abort_epipe_root.stdout"
run_with_closed_stdout "$ABORT_PROBE" epipe "$abort_epipe_root/crashes" \
  "$abort_epipe_root.stderr" "$abort_epipe_root.status"
abort_epipe_status="$(<"$abort_epipe_root.status")"
if [[ $abort_epipe_status -ne 141 ]]; then
  echo "abort-profile EPIPE probe exited $abort_epipe_status instead of 141" >&2
  exit 1
fi
assert_empty_file "$abort_epipe_root.stdout"
assert_empty_file "$abort_epipe_root.stderr"
if compgen -G "$abort_epipe_root/crashes/ft_crash_*" >/dev/null; then
  echo "abort-profile EPIPE created a crash bundle" >&2
  exit 1
fi

FT_PANIC_PROBE_CRASH_DIR="$EVIDENCE_ROOT/marker-interactive" \
  "$INTERACTIVE_PROBE" marker >"$EVIDENCE_ROOT/marker-interactive.stdout" \
  2>"$EVIDENCE_ROOT/marker-interactive.stderr"
grep -F -x -q 'marker=true' "$EVIDENCE_ROOT/marker-interactive.stdout"
assert_empty_file "$EVIDENCE_ROOT/marker-interactive.stderr"

FT_PANIC_PROBE_CRASH_DIR="$EVIDENCE_ROOT/marker-abort" \
  "$ABORT_PROBE" marker >"$EVIDENCE_ROOT/marker-abort.stdout" \
  2>"$EVIDENCE_ROOT/marker-abort.stderr"
grep -F -x -q 'marker=false' "$EVIDENCE_ROOT/marker-abort.stdout"
assert_empty_file "$EVIDENCE_ROOT/marker-abort.stderr"

# The abort negative control must not suppress a panic merely because caller
# code used the recovery helper. Disable core files, then prove the hook emits
# one generic report and the operation never reaches its success marker.
abort_catch_root="$EVIDENCE_ROOT/abort-catch-negative-control"
mkdir -p "$abort_catch_root/crashes"
set +e
(
  ulimit -c 0
  exec env FT_PANIC_PROBE_CRASH_DIR="$abort_catch_root/crashes" \
    "$ABORT_PROBE" caught mux-pane-callback
) >"$abort_catch_root.stdout" 2>"$abort_catch_root.stderr"
abort_catch_status=$?
set -e
if [[ $abort_catch_status -ne 134 ]]; then
  echo "abort-profile catch probe exited $abort_catch_status instead of 134" >&2
  exit 1
fi
assert_empty_file "$abort_catch_root.stdout"
grep -F -x -q \
  'FrankenTerm: fatal internal error; diagnostic details were suppressed' \
  "$abort_catch_root.stderr"
if [[ $(wc -l <"$abort_catch_root.stderr") -ne 1 ]]; then
  echo "abort-profile catch probe must emit one generic fatal report" >&2
  exit 1
fi
assert_one_sanitized_crash_bundle "$abort_catch_root/crashes"

assert_no_secret "$EVIDENCE_ROOT"
echo "PANIC_CONTRACT_SUBPROCESS_SUCCESS evidence_root=$EVIDENCE_ROOT"
