#!/usr/bin/env bash
# W4.T (ft-7h5da.5.6) live e2e: dead-wire closure + wiring gate.
#
# Runs a real watcher against a private mux (isolated HOME/XDG, its own socket;
# it never dials the operator's running GUI) and checks, with ft-test-log
# phase markers and per-assertion PASS/FAIL:
#   1. shadow mode: `ft doctor --json` publishes the observe-only shadow-mode
#      contract with per-engine decision counts;
#   2. BOCPD: a pane that shifts from a slow trickle to a flood produces
#      `bocpd.change_point` events at INFO from the live watcher, bounded (no
#      storm);
#   3. connector reliability/governor consultation on dispatch (static
#      invariant chain, tests/e2e/test_connector_reliability_governor_consultation.sh);
#   4. the frankenterm-topo deadwire gate and the wiring-status attestation
#      (cargo test, routed through the RCH hook), plus a direct check that
#      every dormant attestation record carries a bead and an expiry that
#      matches the dormant manifest.
#
# Usage: tests/e2e/deadwire_and_governors.sh [BIN_DIR]
#   BIN_DIR holds same-commit `ft` and `frankenterm-mux-server` builds.
#   FT_E2E_SKIP_CARGO=1 skips step 4's cargo gate (the attestation check stays).
# Exit: 0 all PASS, 1 any FAIL, 2 missing input.
set -uo pipefail
umask 077

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="${1:-${FT_E2E_BIN:-}}"
if [[ -z "$BIN" || ! -x "$BIN/ft" || ! -x "$BIN/frankenterm-mux-server" ]]; then
  echo "FATAL: pass a BIN_DIR containing ft and frankenterm-mux-server" >&2
  exit 2
fi
command -v python3 > /dev/null || { echo "FATAL: python3 required" >&2; exit 2; }

D="$(mktemp -d /tmp/ft-w4t-XXXXXX)"
ARTIFACTS="$D/artifacts"
mkdir -p "$D/home" "$D/config" "$D/cache" "$D/data" "$D/state" "$D/runtime" "$D/tmp" "$ARTIFACTS"
chmod 700 "$D" "$D/runtime"
SOCK="$D/mux.sock"
FAILS=0
MUXPID=""
WATCHPID=""

phase() { echo "[ft-test-log] phase=$1"; }
pass() { echo "[ft-test-log] PASS $1"; }
fail() { echo "[ft-test-log] FAIL $1: $2"; FAILS=$((FAILS + 1)); }
teardown() {
  phase TEARDOWN
  [[ -n "$WATCHPID" ]] && kill "$WATCHPID" 2> /dev/null && wait "$WATCHPID" 2> /dev/null
  [[ -n "$MUXPID" ]] && kill "$MUXPID" 2> /dev/null && wait "$MUXPID" 2> /dev/null
  echo "[ft-test-log] artifacts=$ARTIFACTS"
}
trap teardown EXIT

phase SETUP
printf '[[unix_domains]]\nname = "w4t"\nsocket_path = "%s"\nno_serve_automatically = true\n' "$SOCK" > "$D/frankenterm.toml"
printf '[storage]\ndb_path = "ft.db"\n[vendored]\nmux_socket_path = "%s"\n' "$SOCK" > "$D/ft.toml"
ENVV=("PATH=$BIN:/usr/bin:/bin:/usr/sbin:/sbin" "LANG=C" "HOME=$D/home" "SHELL=/bin/zsh"
  "XDG_CONFIG_HOME=$D/config" "XDG_CACHE_HOME=$D/cache" "XDG_DATA_HOME=$D/data"
  "XDG_STATE_HOME=$D/state" "XDG_RUNTIME_DIR=$D/runtime" "TMPDIR=$D/tmp"
  "WEZTERM_UNIX_SOCKET=$SOCK" "FRANKENTERM_UNIX_SOCKET=$SOCK"
  "FRANKENTERM_CONFIG_FILE=$D/frankenterm.toml" "FT_WORKSPACE=$D"
  "FT_WEZTERM_CLI=$D/external-cli-disabled" "FT_METRICS_ENABLED=false")
ft() { env -i "${ENVV[@]}" "$BIN/ft" -c "$D/ft.toml" "$@"; }

env -i "${ENVV[@]}" "$BIN/frankenterm-mux-server" --config-file "$D/frankenterm.toml" \
  --daemonize=false --cwd "$D" -- /bin/zsh -f -c 'sleep 900' > "$ARTIFACTS/mux.log" 2>&1 &
MUXPID=$!
for _ in $(seq 1 150); do
  grep -q "pid=$MUXPID" "$SOCK.lock" 2> /dev/null && [[ -S "$SOCK" ]] && break
  sleep 0.2
done
[[ -S "$SOCK" ]] || { fail setup.mux "private mux socket never appeared"; exit 1; }
# Regime shift: one line a second through BOCPD warmup, then a flood.
ft robot profile create shift \
  --command 'for i in $(seq 1 40); do echo tick $i; sleep 1; done; seq 1 300000; sleep 600' \
  > "$ARTIFACTS/profile-create.json" 2>&1
ft robot profile apply shift --count 1 > "$ARTIFACTS/profile-apply.json" 2>&1
env -i "${ENVV[@]}" "$BIN/ft" -c "$D/ft.toml" watch --foreground --poll-interval 500 \
  > "$ARTIFACTS/watch.log" 2>&1 &
WATCHPID=$!

phase ACT
sleep 150

phase ASSERT
# 1. Shadow-mode contract.
ft doctor --json > "$ARTIFACTS/doctor.json" 2> "$ARTIFACTS/doctor.err"
if python3 - "$ARTIFACTS/doctor.json" << 'PY'
import json, sys
text = open(sys.argv[1]).read()
doc = json.loads(text[text.index("{"):])
shadow = doc["shadow_mode"]
assert shadow["observe_only"] is True, "observe_only"
assert shadow["live_mutation_allowed"] is False, "live_mutation_allowed"
assert shadow["production_behavior_changed"] is False, "production_behavior_changed"
engines = shadow["engines"]
assert engines, "no engine rows"
wired = {"bocpd_change_points", "connector_reliability_governor", "capacity_governor_admission"}
for row in engines:
    if row["engine_id"] in wired:
        assert row["feed_state"] != "dormant_not_wired", f"{row['engine_id']} reported dormant but is wired"
for row in engines:
    counts = row["counts"]
    for key in ("baseline_decisions", "shadow_decisions", "minor_divergences", "major_divergences"):
        assert isinstance(counts[key], int), f"{row['engine_id']}.{key}"
    print(f"  engine={row['engine_id']} feed_state={row['feed_state']} "
          f"shadow_decisions={counts['shadow_decisions']} "
          f"divergences={counts['minor_divergences'] + counts['major_divergences']}")
PY
then pass shadow_mode.doctor_contract; else fail shadow_mode.doctor_contract "see $ARTIFACTS/doctor.json"; fi

# 2. Live BOCPD change points.
DB="$D/.ft/ft.db"
BOCPD_ROWS="$(sqlite3 "$DB" "select count(*) from events where rule_id = 'core.bocpd:change_point' and event_type = 'bocpd.change_point'" 2> /dev/null || echo 0)"
BOCPD_NON_INFO="$(sqlite3 "$DB" "select count(*) from events where rule_id = 'core.bocpd:change_point' and severity != 'info'" 2> /dev/null || echo 0)"
echo "  bocpd.change_point events=$BOCPD_ROWS non_info=$BOCPD_NON_INFO"
if [[ "$BOCPD_ROWS" -ge 1 ]]; then pass bocpd.live_change_point; else fail bocpd.live_change_point "no bocpd.change_point event after the regime shift"; fi
if [[ "$BOCPD_NON_INFO" -eq 0 ]]; then pass bocpd.info_severity; else fail bocpd.info_severity "$BOCPD_NON_INFO events above info"; fi
# One pane over 150 s: a storm would be one change point per poll.
if [[ "$BOCPD_ROWS" -le 30 ]]; then pass bocpd.no_storm; else fail bocpd.no_storm "$BOCPD_ROWS change points in 150 s"; fi

# 3. Connector reliability/governor consultation on dispatch.
if bash "$ROOT/tests/e2e/test_connector_reliability_governor_consultation.sh" > "$ARTIFACTS/connector-consultation.log" 2>&1; then
  pass connector.governor_consulted_on_dispatch
else
  fail connector.governor_consulted_on_dispatch "see $ARTIFACTS/connector-consultation.log"
fi

# 4. Deadwire gate + wiring-status attestation.
if python3 - "$ROOT" << 'PY'
import json, sys, pathlib
root = pathlib.Path(sys.argv[1])
manifest = json.loads((root / "crates/frankenterm-topo/deadwire-dormant.json").read_text())
artifact = json.loads((root / "docs/attestations/doctrine/decision-api-wiring-status.json").read_text())
exempt = {e["symbol"]: e for e in manifest["exemptions"]}
for record in artifact["records"]:
    assert record["status"] in ("wired", "dormant"), f"{record['symbol']} is {record['status']}"
    if record["status"] == "dormant":
        entry = exempt.get(record["symbol"])
        assert entry, f"dormant {record['symbol']} has no manifest exemption"
        assert record["dormant_bead"] == entry["bead_id"], record["symbol"]
        assert record["dormant_expires_on"] == entry["expires_on"], record["symbol"]
dormant = {r["symbol"] for r in artifact["records"] if r["status"] == "dormant"}
assert set(exempt) <= dormant, f"stale exemptions: {set(exempt) - dormant}"
print(f"  records={len(artifact['records'])} dormant={sorted(dormant)}")
PY
then pass attestation.dormant_records_match_manifest; else fail attestation.dormant_records_match_manifest "manifest/attestation mismatch"; fi
if [[ "${FT_E2E_SKIP_CARGO:-0}" == "1" ]]; then
  echo "[ft-test-log] SKIP deadwire.workspace_gate (FT_E2E_SKIP_CARGO=1)"
elif (cd "$ROOT" && cargo test -p frankenterm-topo --test deadwire_gate_workspace) > "$ARTIFACTS/deadwire-gate.log" 2>&1; then
  pass deadwire.workspace_gate
else
  fail deadwire.workspace_gate "see $ARTIFACTS/deadwire-gate.log"
fi

echo "[ft-test-log] result=$([[ $FAILS -eq 0 ]] && echo PASS || echo FAIL) failures=$FAILS"
[[ $FAILS -eq 0 ]]
