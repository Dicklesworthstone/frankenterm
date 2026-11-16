#!/usr/bin/env bash
# scripts/test-lindley-bounds-tolerance-gate.sh — synthetic input/gate controls.
# Runs the real RCH producer wrapper and checks actual JSON plus exit status.
# Per-case stdout, stderr, RCH logs, JSON and exit codes are retained. These
# cases verify diagnostic behavior, not measured performance or release proof.
#
# Usage:
#   bash scripts/test-lindley-bounds-tolerance-gate.sh
#
# Exits 0 only when all controls pass; 1 for any failure or unexecuted control.
#
# Bead: br-ft-jfyz7.1 (tolerance-gate substrate).
# Substrate: scripts/lindley-bounds-build.sh +
#   crates/frankenterm-core/examples/lindley_bounds_build.rs (br-ft-43x69).
# Run manually in an authorized RCH lane or through DSR; no workflow operations.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT" || exit 1

BUILDER="$REPO_ROOT/scripts/lindley-bounds-build.sh"
[[ -x "$BUILDER" ]] || {
  echo "test-lindley-bounds-tolerance-gate: $BUILDER not executable" >&2
  exit 1
}

failures=0
RUN_ID="${RUN_ID:-$(date -u +"%Y%m%dT%H%M%SZ")-$$}"
LOG_DIR="${FT_LINDLEY_TOLERANCE_ARTIFACT_DIR:-$REPO_ROOT/target/lindley-tolerance/$RUN_ID}"
SHARED_TARGET="${FT_LINDLEY_BOUNDS_RCH_TARGET_DIR:-target/rch-lindley-tolerance-gate}"
mkdir -p "$LOG_DIR"
command -v python3 >/dev/null || { echo "python3 is required for JSON assertions" >&2; exit 1; }

run_case() {
  local label="$1" expected_exit="$2" expected_empirical="$3" expected_error="$4"
  shift 4
  local case_dir="$LOG_DIR/$label" actual_exit validation_exit
  mkdir -p "$case_dir"
  echo "case: $label (expected exit $expected_exit; logs $case_dir)"
  env -u FT_RELEASE_VERSION -u FT_LINDLEY_STAGE_TELEMETRY_JSON \
    -u FT_LINDLEY_STAGE_TELEMETRY_PATH -u FT_LINDLEY_EMPIRICAL_P99_MS \
    -u FT_LINDLEY_INPUT_SHA256 -u FT_LINDLEY_INPUT_ORIGIN \
    "FT_LINDLEY_BOUNDS_ARTIFACT_DIR=$case_dir/producer" \
    "FT_LINDLEY_BOUNDS_RCH_TARGET_DIR=$SHARED_TARGET" "RUN_ID=$label" \
    "$@" bash "$BUILDER" --no-write \
    >"$case_dir/stdout.log" 2>"$case_dir/stderr.log"
  actual_exit=$?
  printf '%s\n' "$actual_exit" >"$case_dir/exit-code"
  python3 - "$case_dir" "$label" "$actual_exit" "$expected_exit" \
    "$expected_empirical" "$expected_error" >"$case_dir/assertions.log" 2>&1 <<'PY'
import hashlib
import json
import math
import pathlib
import sys

directory, label, actual, expected, empirical, expected_error = sys.argv[1:]
directory = pathlib.Path(directory)
assert int(actual) == int(expected), f"exit {actual}, expected {expected}"
body = (directory / "stdout.log").read_text()
if int(expected) == 2:
    assert not body.strip(), "invalid input must not emit successful diagnostic JSON"
    logs = (directory / "stderr.log").read_text()
    for path in (directory / "producer").glob("*.rch.log"):
        logs += path.read_text(errors="replace")
    assert expected_error in logs, f"missing actual input rejection: {expected_error}"
    if label.startswith("telemetry_"):
        assert not list((directory / "producer").glob("*.rch*")), "invalid telemetry reached RCH preflight"
else:
    def reject_constant(value):
        raise AssertionError(f"invalid JSON numeric constant: {value}")
    row = json.loads(body, parse_constant=reject_constant)
    retained = json.loads((directory / "producer/lindley-bounds.json").read_text(),
                          parse_constant=reject_constant)
    assert row == retained, "stdout must agree with retained diagnostic on exit 0 and 1"
    assert row["within_tolerance"] is (int(expected) == 0)
    assert row["empirical_p99_ms"] == float(empirical)
    assert math.isfinite(row["empirical_p99_ms"]) and row["empirical_p99_ms"] >= 0
    assert row["exceeds_analytical_bound"] is (row["empirical_p99_ms"] > row["analytical_bound_ms"])
    capture = next(item for item in row["coverage_status"]
                   if item["claim_surface"] == "capture_4kb_overlap_benchmark")
    assert capture["status"] == "modeled_pending_empirical"
    provenance = row["input_provenance"]
    assert provenance["release_ready"] is False
    assert provenance["measurement_provenance_verified"] is False
    assert provenance["payload_encoding"] == "serde-json-lindley-inputs-v1"
    payload_bytes = provenance["payload_json"].encode("utf-8")
    digest = "sha256:" + hashlib.sha256(payload_bytes).hexdigest()
    assert digest == provenance["input_sha256"], "independent digest must bind exact payload bytes"
    payload = json.loads(payload_bytes, parse_constant=reject_constant)
    assert set(payload) == {"telemetry_model", "empirical_p99_ms"}
    assert payload["empirical_p99_ms"] == row["empirical_p99_ms"]
    historical_empirical = label in {"absent_default", "telemetry_at_limit_file",
                                     "telemetry_at_limit_inline"}
    expected_empirical_source = ("historical_8_5_ms_reference" if historical_empirical
                                 else "caller_supplied")
    assert provenance["empirical_source"] == expected_empirical_source
    if label == "release_bound_inputs":
        assert row["release_version"] == "1.2.3-input-binding-test"
        assert provenance["status"] == "supplied_inputs_unverified"
        assert provenance["input_binding_verified"] is True
        assert provenance["declared_input_sha256"] == digest
        assert provenance["declared_external_origin"] == "synthetic-tolerance-harness"
        assert provenance["model_source"] == "caller_supplied_json"
    else:
        assert row["release_version"] == "0.0.0-substrate"
        assert provenance["status"] == "historical_diagnostic"
        assert provenance["input_binding_verified"] is False
        expected_model_source = ("caller_supplied_json" if label.startswith("telemetry_at_limit_")
                                 else "historical_documented_default")
        assert provenance["model_source"] == expected_model_source
        assert "HISTORICAL" in "".join(path.read_text(errors="replace")
                                       for path in (directory / "producer").glob("*.rch.log"))
print(f"PASS: {label}; actual exit and diagnostic predicates verified")
PY
  validation_exit=$?
  cat "$case_dir/assertions.log"
  if [[ $validation_exit -ne 0 ]]; then
    cat "$case_dir/stderr.log" >&2
    echo "FAIL: $label; full retained evidence: $case_dir" >&2
    failures=$((failures + 1))
  fi
}

run_case absent_default 0 8.5 ""
run_case explicit_reference 0 8.5 "" FT_LINDLEY_EMPIRICAL_P99_MS=8.5
run_case outside_tolerance 1 999.0 "" FT_LINDLEY_EMPIRICAL_P99_MS=999.0
run_case zero_empirical 1 0 "" FT_LINDLEY_EMPIRICAL_P99_MS=0
for invalid in empty malformed negative nan infinity negative_infinity overflow; do
  case "$invalid" in
    empty) value="" ;;
    malformed) value="not-a-number" ;;
    negative) value="-1" ;;
    nan) value="NaN" ;;
    infinity) value="inf" ;;
    negative_infinity) value="-inf" ;;
    overflow) value="1e999" ;;
  esac
  run_case "$invalid" 2 "" "FT_LINDLEY_EMPIRICAL_P99_MS must be a finite nonnegative number" \
    "FT_LINDLEY_EMPIRICAL_P99_MS=$value"
done
run_case empty_model 2 "" "invalid FT_LINDLEY_STAGE_TELEMETRY_JSON" FT_LINDLEY_STAGE_TELEMETRY_JSON=
run_case malformed_model 2 "" "invalid FT_LINDLEY_STAGE_TELEMETRY_JSON" 'FT_LINDLEY_STAGE_TELEMETRY_JSON={'
run_case release_defaults_refused 2 "" "a release version requires explicit telemetry" \
  FT_RELEASE_VERSION=1.2.3-input-binding-test

# Reuse the actual diagnostic's exact payload to test binding. Independently
# compute the digest from its bytes, then alter each bound input as negatives.
if python3 - "$LOG_DIR" <<'PY'
import hashlib
import json
import pathlib
import sys
directory = pathlib.Path(sys.argv[1])
row = json.loads((directory / "absent_default/stdout.log").read_text())
payload_json = row["input_provenance"]["payload_json"]
payload = json.loads(payload_json)
model_json = json.dumps(payload["telemetry_model"])
(directory / "binding-model.json").write_text(model_json)
(directory / "at-limit-model.json").write_bytes(model_json.encode().ljust(64 * 1024, b" "))
(directory / "binding-digest").write_text("sha256:" + hashlib.sha256(payload_json.encode()).hexdigest())
payload["telemetry_model"]["arrival_burst_events"] += 1
(directory / "changed-binding-model.json").write_text(json.dumps(payload["telemetry_model"]))
PY
then
  model="$(cat "$LOG_DIR/binding-model.json")"
  changed_model="$(cat "$LOG_DIR/changed-binding-model.json")"
  digest="$(cat "$LOG_DIR/binding-digest")"
  binding_inputs=(FT_RELEASE_VERSION=1.2.3-input-binding-test
    "FT_LINDLEY_STAGE_TELEMETRY_JSON=$model" FT_LINDLEY_EMPIRICAL_P99_MS=8.5
    FT_LINDLEY_INPUT_ORIGIN=synthetic-tolerance-harness)
  run_case release_missing_digest 2 "" "a release version requires explicit telemetry" "${binding_inputs[@]}"
  run_case release_wrong_digest 2 "" "FT_LINDLEY_INPUT_SHA256 does not match" \
    "${binding_inputs[@]}" FT_LINDLEY_INPUT_SHA256=sha256:0000000000000000000000000000000000000000000000000000000000000000
  run_case release_bound_inputs 0 8.5 "" "${binding_inputs[@]}" "FT_LINDLEY_INPUT_SHA256=$digest"
  run_case changed_empirical 2 "" "FT_LINDLEY_INPUT_SHA256 does not match" \
    "${binding_inputs[@]}" "FT_LINDLEY_INPUT_SHA256=$digest" FT_LINDLEY_EMPIRICAL_P99_MS=8.6
  run_case changed_model 2 "" "FT_LINDLEY_INPUT_SHA256 does not match" \
    "${binding_inputs[@]}" "FT_LINDLEY_INPUT_SHA256=$digest" "FT_LINDLEY_STAGE_TELEMETRY_JSON=$changed_model"
  run_case telemetry_at_limit_file 0 8.5 "" \
    "FT_LINDLEY_STAGE_TELEMETRY_PATH=$LOG_DIR/at-limit-model.json"
  at_limit_json="$(cat "$LOG_DIR/at-limit-model.json")"
  run_case telemetry_at_limit_inline 0 8.5 "" "FT_LINDLEY_STAGE_TELEMETRY_JSON=$at_limit_json"
else
  echo "FAIL: input-binding controls could not run without a real diagnostic payload" >&2
  failures=$((failures + 1))
fi

# These planted negatives reach the actual wrapper before any RCH admission.
# Keep their exact bytes; NUL and invalid UTF-8 must never be shell-substituted.
if python3 - "$LOG_DIR" <<'PY'
import pathlib
import sys
directory = pathlib.Path(sys.argv[1])
limit = 64 * 1024
(directory / "oversized-model.json").write_bytes(b"{}" + b" " * (limit - 1))
(directory / "invalid-utf8-model.json").write_bytes(b"{}\xff")
(directory / "nul-model.json").write_bytes(b"{}\x00")
PY
then
  run_case telemetry_oversized_file 2 "" "telemetry exceeds 65536-byte limit" \
    "FT_LINDLEY_STAGE_TELEMETRY_PATH=$LOG_DIR/oversized-model.json"
  oversized_json="$(cat "$LOG_DIR/oversized-model.json")"
  run_case telemetry_oversized_inline 2 "" "telemetry exceeds 65536-byte limit" \
    "FT_LINDLEY_STAGE_TELEMETRY_JSON=$oversized_json"
  run_case telemetry_invalid_utf8_file 2 "" "telemetry must be valid UTF-8" \
    "FT_LINDLEY_STAGE_TELEMETRY_PATH=$LOG_DIR/invalid-utf8-model.json"
  run_case telemetry_nul_file 2 "" "telemetry must not contain NUL bytes" \
    "FT_LINDLEY_STAGE_TELEMETRY_PATH=$LOG_DIR/nul-model.json"
else
  echo "FAIL: bounded-input controls could not create their retained fixtures" >&2
  failures=$((failures + 1))
fi

if [[ "$failures" -eq 0 ]]; then
  echo
  echo "test-lindley-bounds-tolerance-gate: all diagnostic controls passed; logs $LOG_DIR"
  exit 0
else
  echo
  echo "test-lindley-bounds-tolerance-gate: $failures case(s) failed" >&2
  exit 1
fi
