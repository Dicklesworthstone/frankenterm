#!/usr/bin/env bash
# Browser fleet viewer e2e: `ft web --ui-dir` serves the WASM viewer, and a
# real headless Chrome loads it, follows /stream/events, and shows a detection
# produced by the real capture -> detect pipeline
# (scripts/smoke/headless-mux-observe.sh) plus the captured pane. No synthetic
# rows: the producer is the same headless smoke test_web_sse_live_events.sh uses.
#
# The smoke owns a fresh, empty workspace (it refuses a non-empty one). This
# harness waits for the smoke's watcher to create that workspace's database,
# then starts a read-only `ft web` on the same workspace and opens the viewer.
# Depending on timing the detection reaches the browser live (storage tail ->
# SSE) or with the history fetch; this script does not distinguish the two.
#
# Usage: tests/e2e/test_browser_fleet_viewer.sh UI_DIR [BIN_DIR] [PORT]
#   UI_DIR   package site dir from scripts/build-browser-ui.sh (has manifest.json)
#   BIN_DIR  defaults to target/debug; needs ft + frankenterm-mux-server (web feature)
#   PORT     defaults to 18778
# Needs node >= 22 (built-in WebSocket) and Chrome with WebGPU; CHROME overrides
# the macOS default path. Writes receipt.json, web.log, browser.json and
# viewer.png into a fresh harness dir. A run is a dev signal unless DSR runs
# it on the release binaries.
set -u
UI_DIR="${1:?usage: test_browser_fleet_viewer.sh UI_DIR [BIN_DIR] [PORT]}"
BIN_DIR="${2:-target/debug}"
PORT="${3:-18778}"
FT="$BIN_DIR/ft"
CHROME="${CHROME:-/Applications/Google Chrome.app/Contents/MacOS/Google Chrome}"
HERE=$(cd "$(dirname "$0")/../.." && pwd)
SMOKE="$HERE/scripts/smoke/headless-mux-observe.sh"
DRIVER="$HERE/tests/e2e/browser_fleet_viewer_cdp.mjs"
[ -x "$FT" ] || { echo "missing binary: $FT" >&2; exit 2; }
[ -f "$UI_DIR/manifest.json" ] || { echo "not a UI package: $UI_DIR" >&2; exit 2; }
[ -x "$SMOKE" ] || { echo "missing smoke script: $SMOKE" >&2; exit 2; }
[ -x "$CHROME" ] || { echo "missing Chrome: $CHROME" >&2; exit 2; }
command -v node >/dev/null || { echo "missing node" >&2; exit 2; }

H=$(mktemp -d "${TMPDIR:-/tmp}/ft-viewer-e2e-XXXXXX")   # harness files
W=$(mktemp -d "${TMPDIR:-/tmp}/ft-viewer-ws-XXXXXX")    # the smoke's workspace
echo "harness: $H"
echo "workspace: $W"

# Producer: the real pipeline (mux server + watcher + send + detect).
( "$SMOKE" "$BIN_DIR" "$W" > "$H/producer.log" 2>&1; echo $? > "$H/producer.exit" ) &
PRODUCER=$!

for _ in $(seq 1 300); do [ -f "$W/.ft/ft.db" ] && break; sleep 0.2; done
if [ ! -f "$W/.ft/ft.db" ]; then
  echo "the producer never created its database: $(tail -3 "$H/producer.log")" >&2
  wait "$PRODUCER"
  exit 1
fi

FT_SKIP_BUNDLED_FONT_INSTALL=1 "$FT" -c "$W/ft.toml" --workspace "$W" web --port "$PORT" \
  --ui-dir "$UI_DIR" > "$H/web.log" 2>&1 &
WEB=$!
# Readiness: "ft web listening on" once bound; a debug build opens storage first.
for _ in $(seq 1 150); do grep -q 'ft web listening on' "$H/web.log" 2>/dev/null && break; sleep 0.2; done
if ! grep -q 'ft web listening on' "$H/web.log"; then
  echo "ft web never listened: $(tail -3 "$H/web.log")" >&2
  kill "$WEB" 2>/dev/null
  wait "$PRODUCER"
  exit 1
fi

jq -n --arg url "http://127.0.0.1:$PORT/ui/" --arg chrome "$CHROME" \
  --arg profile "$H/chrome-profile" --arg shot "$H/viewer.png" --arg result "$H/browser.json" \
  --arg exitfile "$H/producer.exit" --argjson port "$((PORT + 1000))" \
  '{url:$url, chrome:$chrome, profileDir:$profile, debugPort:$port, screenshot:$shot,
    resultPath:$result, producerExitFile:$exitfile, expectDetection:"codex.usage.reached"}' \
  > "$H/browser-config.json"
node "$DRIVER" "$H/browser-config.json"
BROWSER=$?
kill "$WEB" 2>/dev/null
wait "$WEB" 2>/dev/null
wait "$PRODUCER" 2>/dev/null

STATUS=fail
[ "$BROWSER" -eq 0 ] && STATUS=pass
jq -n --arg schema "ft.e2e.browser-fleet-viewer.v1" --arg generated_at "$(date -u +%FT%TZ)" \
  --arg commit "$(git -C "$HERE" rev-parse --short HEAD 2>/dev/null || echo unknown)" \
  --arg cli_version "$("$FT" --version 2>/dev/null | head -1)" --arg status "$STATUS" \
  --arg package "$(shasum -a 256 "$UI_DIR/manifest.json" | awk '{ print $1 }')" \
  --arg producer_exit "$(cat "$H/producer.exit" 2>/dev/null || echo missing)" \
  --slurpfile browser "$H/browser.json" \
  '{schema:$schema, generated_at:$generated_at, commit:$commit, cli_version:$cli_version,
    status:$status, ui_manifest_sha256:$package, producer_exit:$producer_exit,
    checks:$browser[0].checks}' > "$H/receipt.json"
echo "receipt: $H/receipt.json (status=$STATUS)"
[ "$STATUS" = pass ] || { tail -5 "$H/web.log"; tail -5 "$H/producer.log"; exit 1; }
echo "PASS: the browser viewer served by ft web showed a pipeline-produced detection and pane"
