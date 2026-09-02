#!/usr/bin/env bash
# Live SSE e2e (ft-xxfwy.19): a standalone `ft web` must republish a detection
# that a SEPARATE watcher process persisted, on /stream/events, within a few
# seconds. Reuses the headless observe smoke as the event producer so the event
# comes from the real capture -> detect pipeline, not a synthetic insert.
#
# This script is the acceptance test for ft-xxfwy.19 (both stream modes:
# /stream/events via the storage tail and /stream/deltas via DB scans). It was
# red until ft-xxfwy.38 landed: `ft web` answered no request at all (fastapi
# accept timeouts born expired), and then the first SSE client aborted the
# process (handler spawn from fastapi's own connection task). Both are fixed
# and regression-tested in frankenterm-core::web_framework; on the dev build
# this script passes with the ready frame plus the live detection frame.
#
# Usage: tests/e2e/test_web_sse_live_events.sh [BIN_DIR] [PORT]
#   BIN_DIR defaults to target/debug (needs ft + frankenterm-mux-server, built
#   with the `web` feature); PORT defaults to 18777.
# Writes receipt.json + web.log + stream.txt into a fresh workspace dir.
set -u
BIN_DIR="${1:-target/debug}"
PORT="${2:-18777}"
FT="$BIN_DIR/ft"
[ -x "$FT" ] || { echo "missing binary: $FT" >&2; exit 2; }
HERE=$(cd "$(dirname "$0")/../.." && pwd)
SMOKE="$HERE/scripts/smoke/headless-mux-observe.sh"
[ -x "$SMOKE" ] || { echo "missing smoke script: $SMOKE" >&2; exit 2; }

D=$(mktemp -d "${TMPDIR:-/tmp}/ft-sse-XXXXXX")
mkdir -p "$D/.ft"; chmod 700 "$D/.ft"
printf '[storage]\ndb_path = "ft.db"\n' > "$D/ft.toml"; chmod 600 "$D/ft.toml"
echo "workspace: $D"

export FT_WORKSPACE="$D"
"$FT" -c "$D/ft.toml" web --port "$PORT" > "$D/web.log" 2>&1 &
WEB=$!
# Readiness: the server logs "ft web listening on" once bound (a debug build can
# take several seconds to open storage first). Never probe with an unbounded curl.
for _ in $(seq 1 150); do grep -q 'ft web listening on' "$D/web.log" 2>/dev/null && break; sleep 0.2; done
grep -q 'ft web listening on' "$D/web.log" || { echo "ft web never listened: $(tail -3 "$D/web.log")" >&2; kill "$WEB" 2>/dev/null; exit 1; }
# Subscribe before the producer runs; the tail republishes rows persisted after it started.
curl -s -N --max-time 60 "http://127.0.0.1:$PORT/stream/events" > "$D/stream.txt" 2> "$D/curl.err" &
CURL=$!
# Second mode: /stream/deltas is DB-backed (periodic segment scans, all panes
# when no pane_id is given), so the producer's captured output must show up as
# `delta` frames on a standalone ft web too.
curl -s -N --max-time 60 "http://127.0.0.1:$PORT/stream/deltas" > "$D/deltas.txt" 2> "$D/curl-deltas.err" &
CURL_DELTAS=$!
sleep 1

# Producer: the real pipeline (mux server + watcher + send + detect) in the SAME workspace.
"$SMOKE" "$BIN_DIR" "$D" > "$D/producer.log" 2>&1
PRODUCER=$?

# Give the tail one poll interval plus slack, then stop.
sleep 3
kill "$CURL" 2>/dev/null; wait "$CURL" 2>/dev/null
kill "$CURL_DELTAS" 2>/dev/null; wait "$CURL_DELTAS" 2>/dev/null
kill "$WEB" 2>/dev/null

FRAMES=$(grep -c '^data:' "$D/stream.txt" 2>/dev/null); FRAMES=${FRAMES:-0}
HIT=$(grep -c 'codex.usage.reached' "$D/stream.txt" 2>/dev/null); HIT=${HIT:-0}
DELTA_FRAMES=$(grep -c '^event: delta' "$D/deltas.txt" 2>/dev/null); DELTA_FRAMES=${DELTA_FRAMES:-0}
STATUS=fail
if [ "$PRODUCER" -eq 0 ] && [ "$HIT" -ge 1 ] && [ "$DELTA_FRAMES" -ge 1 ]; then STATUS=pass; fi
jq -n --arg schema "ft.e2e.web-sse-live-events.v2" --arg generated_at "$(date -u +%FT%TZ)" \
  --arg commit "$(git -C "$HERE" rev-parse --short HEAD 2>/dev/null || echo unknown)" \
  --arg cli_version "$("$FT" --version 2>/dev/null | head -1)" --arg status "$STATUS" \
  --argjson producer_exit "$PRODUCER" --argjson frames "$FRAMES" --argjson detection_frames "$HIT" \
  --argjson delta_frames "$DELTA_FRAMES" \
  '{schema:$schema,generated_at:$generated_at,commit:$commit,cli_version:$cli_version,status:$status,producer_exit:$producer_exit,sse_frames:$frames,detection_frames:$detection_frames,delta_frames:$delta_frames}' \
  > "$D/receipt.json"
echo "producer exit=$PRODUCER sse frames=$FRAMES detection frames=$HIT delta frames=$DELTA_FRAMES"
echo "receipt: $D/receipt.json (status=$STATUS)"
[ "$STATUS" = pass ] || { echo "stream head:"; head -c 600 "$D/stream.txt"; echo; echo "deltas head:"; head -c 400 "$D/deltas.txt"; echo; tail -5 "$D/web.log"; exit 1; }
echo "PASS: a watcher-persisted detection reached /stream/events and captured output reached /stream/deltas on a standalone ft web"
