#!/usr/bin/env bash
# ft-xxfwy.19: /stream/events filters and redaction, over the watcher's own bus.
#
# `ft watch --web` serves the read-only API from the watcher process and shares
# its EventBus, and `ft event --from-uservar` publishes onto that bus through
# the watcher's IPC socket. That pairing gives a deterministic live-event test
# with no mux server and no storage-tail poll latency: publish once, assert what
# each subscriber saw.
#
# What this proves:
#   - the event reaches an unfiltered subscriber
#   - `?pane_id=<its pane>` admits it and `?pane_id=<other>` does not
#     (the planted negative: a filter that admitted everything would fail here)
#   - `?channel=signals` carries it and `?channel=deltas` does not
#   - a secret in the payload is redacted in BOTH views the frame carries: the
#     decoded `event_data` and the base64 `value` it was decoded from. The
#     second one is the interesting half -- it read as clean to a grep while
#     decoding to the secret in full until the redaction walk learned to look
#     inside base64.
#
# What this does NOT prove: lag frames (needs a slow reader against a busy
# publisher), Last-Event-ID resume (not implemented), or the storage-tail path
# used by a standalone `ft web` (see test_web_sse_live_events.sh for that).
#
# Usage: tests/e2e/test_web_sse_filters_and_redaction.sh [BIN_DIR] [PORT]
#   BIN_DIR defaults to target/debug and needs `ft` built with the `web` feature.
# Writes receipt.json and every stream body into a fresh workspace dir.
set -u
BIN_DIR="${1:-target/debug}"
PORT="${2:-18993}"
FT="$BIN_DIR/ft"
[ -x "$FT" ] || { echo "missing binary: $FT" >&2; exit 2; }
command -v jq >/dev/null 2>&1 || { echo "missing jq" >&2; exit 2; }
HERE=$(cd "$(dirname "$0")/../.." && pwd)

D=$(mktemp -d "${TMPDIR:-/tmp}/ft-sse-filters-XXXXXX")
mkdir -p "$D/.ft"; chmod 700 "$D/.ft"
printf '[storage]\ndb_path = "ft.db"\n' > "$D/ft.toml"; chmod 600 "$D/ft.toml"
echo "workspace: $D"
PANE=7
OTHER=8
SECRET='sk-livetest1234567890abcdefghij'

cleanup() {
  for pid in ${C_ALL:-} ${C_PANE:-} ${C_OTHER:-} ${C_SIGNALS:-} ${C_DELTAS:-}; do
    kill "$pid" 2>/dev/null
  done
  if [ -n "${WATCH:-}" ]; then
    kill "$WATCH" 2>/dev/null
    for _ in $(seq 1 25); do kill -0 "$WATCH" 2>/dev/null || break; sleep 0.4; done
    kill -0 "$WATCH" 2>/dev/null && kill -9 "$WATCH" 2>/dev/null
  fi
  return 0
}
trap cleanup EXIT

FT_WORKSPACE="$D" "$FT" -c "$D/ft.toml" watch --foreground --web --web-port "$PORT" \
  --poll-interval 5000 > "$D/watch.log" 2>&1 &
WATCH=$!
for _ in $(seq 1 150); do
  curl -fsS --max-time 2 "http://127.0.0.1:$PORT/health" >/dev/null 2>&1 && break
  kill -0 "$WATCH" 2>/dev/null || break
  sleep 0.5
done
curl -fsS --max-time 2 "http://127.0.0.1:$PORT/health" >/dev/null 2>&1 || {
  echo "FAIL: the in-process web API never answered on $PORT" >&2
  tail -12 "$D/watch.log" >&2; exit 1; }
for _ in $(seq 1 60); do [ -S "$D/.ft/ipc.sock" ] && break; sleep 0.5; done
[ -S "$D/.ft/ipc.sock" ] || { echo "FAIL: watcher IPC socket never appeared" >&2; exit 1; }

curl -s -N --max-time 25 "http://127.0.0.1:$PORT/stream/events" > "$D/all.txt" 2>/dev/null &
C_ALL=$!
curl -s -N --max-time 25 "http://127.0.0.1:$PORT/stream/events?pane_id=$PANE" > "$D/pane.txt" 2>/dev/null &
C_PANE=$!
curl -s -N --max-time 25 "http://127.0.0.1:$PORT/stream/events?pane_id=$OTHER" > "$D/other.txt" 2>/dev/null &
C_OTHER=$!
curl -s -N --max-time 25 "http://127.0.0.1:$PORT/stream/events?channel=signals" > "$D/signals.txt" 2>/dev/null &
C_SIGNALS=$!
curl -s -N --max-time 25 "http://127.0.0.1:$PORT/stream/events?channel=deltas" > "$D/deltas.txt" 2>/dev/null &
C_DELTAS=$!
sleep 2

VALUE=$(printf '{"kind":"note","message":"a token %s trails here"}' "$SECRET" | base64)
FT_WORKSPACE="$D" "$FT" -c "$D/ft.toml" event --from-uservar --pane "$PANE" --name ft_event \
  --value "$VALUE" > "$D/inject.log" 2>&1
INJECT=$?
sleep 3
for pid in $C_ALL $C_PANE $C_OTHER $C_SIGNALS $C_DELTAS; do kill "$pid" 2>/dev/null; done
C_ALL=""; C_PANE=""; C_OTHER=""; C_SIGNALS=""; C_DELTAS=""
sleep 1

hits() { local n; n=$(grep -c 'user_var_received' "$1" 2>/dev/null); echo "${n:-0}"; }
ALL_HITS=$(hits "$D/all.txt")
PANE_HITS=$(hits "$D/pane.txt")
OTHER_HITS=$(hits "$D/other.txt")
SIGNAL_HITS=$(hits "$D/signals.txt")
DELTA_HITS=$(hits "$D/deltas.txt")

# Redaction has to be judged on what a client can READ, not on what a grep can
# see: decode every base64 `value` the streams served and look for the secret
# in the decoded text too.
DECODED_LEAK=$(cat "$D"/*.txt 2>/dev/null | sed -n 's/^data: //p' | jq -r '
    .. | objects | select(has("value")) | .value // empty' 2>/dev/null \
  | while read -r encoded; do printf '%s' "$encoded" | base64 -d 2>/dev/null; echo; done \
  | grep -c "$SECRET")
DECODED_LEAK=${DECODED_LEAK:-0}
RAW_LEAK=$(cat "$D"/*.txt 2>/dev/null | grep -c "$SECRET"); RAW_LEAK=${RAW_LEAK:-0}

STATUS=pass
fail() { echo "FAIL: $1" >&2; STATUS=fail; }
[ "$INJECT" -eq 0 ] || fail "ft event --from-uservar exited $INJECT"
[ "$ALL_HITS" -ge 1 ] || fail "the injected event never reached the unfiltered stream"
[ "$PANE_HITS" -ge 1 ] || fail "pane_id=$PANE dropped its own pane's event"
[ "$OTHER_HITS" -eq 0 ] || fail "pane_id=$OTHER admitted another pane's event"
[ "$SIGNAL_HITS" -ge 1 ] || fail "channel=signals did not carry the user-var event"
[ "$DELTA_HITS" -eq 0 ] || fail "channel=deltas admitted a signal event"
[ "$RAW_LEAK" -eq 0 ] || fail "the secret was served verbatim"
[ "$DECODED_LEAK" -eq 0 ] || fail "the secret survived inside a base64 field"

jq -n --arg schema "ft.e2e.web-sse-filters-redaction.v1" \
  --arg generated_at "$(date -u +%FT%TZ)" \
  --arg commit "$(git -C "$HERE" rev-parse --short HEAD 2>/dev/null || echo unknown)" \
  --arg cli_version "$("$FT" --version 2>/dev/null | head -1)" \
  --arg status "$STATUS" --arg workspace "$D" \
  --argjson inject_exit "$INJECT" \
  --argjson unfiltered_hits "$ALL_HITS" --argjson pane_hits "$PANE_HITS" \
  --argjson other_pane_hits "$OTHER_HITS" --argjson signal_hits "$SIGNAL_HITS" \
  --argjson delta_hits "$DELTA_HITS" \
  --argjson raw_secret_occurrences "$RAW_LEAK" \
  --argjson decoded_secret_occurrences "$DECODED_LEAK" \
  '{schema:$schema,generated_at:$generated_at,commit:$commit,cli_version:$cli_version,
    status:$status,workspace:$workspace,inject_exit:$inject_exit,
    filters:{unfiltered_hits:$unfiltered_hits,pane_hits:$pane_hits,
             other_pane_hits:$other_pane_hits,signal_hits:$signal_hits,delta_hits:$delta_hits},
    redaction:{raw_secret_occurrences:$raw_secret_occurrences,
               decoded_secret_occurrences:$decoded_secret_occurrences}}' \
  > "$D/receipt.json"

echo "hits: unfiltered=$ALL_HITS pane($PANE)=$PANE_HITS other($OTHER)=$OTHER_HITS signals=$SIGNAL_HITS deltas=$DELTA_HITS"
echo "secret occurrences: raw=$RAW_LEAK decoded-from-base64=$DECODED_LEAK"
echo "receipt: $D/receipt.json (status=$STATUS)"
[ "$STATUS" = pass ] || { tail -6 "$D/watch.log"; exit 1; }
echo "PASS: /stream/events honored pane and channel filters and redacted both views of the payload"
