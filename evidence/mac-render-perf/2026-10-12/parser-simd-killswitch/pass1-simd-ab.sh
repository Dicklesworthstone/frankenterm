#!/bin/sh
# ft-yccm0.3.2.2 / ft-yccm0.3.2.3 native kill-switch ABBA on one binary
# (ingest_throughput release-perf at 15a2b1c46, --lane term), identical
# corpus bytes per corpus. Waits for the host's 1-minute load to fall
# below 60 (up to 2 h) and records the load beside every run.
set -u
R=$HOME/ft-build/70s4z
O=$HOME/ft-build/simd-ab
BIN=$R/ingest-head
mkdir -p "$O"
export FT_CORPUS_DIR=$R/corpus

load1() { uptime | sed 's/.*averages: //' | cut -d' ' -f1; }
waited=0
while [ "$(load1 | cut -d. -f1)" -ge 60 ] && [ "$waited" -lt 7200 ]; do
  sleep 60
  waited=$((waited + 60))
done
echo "start $(date +%H:%M:%S) load=$(load1) waited=${waited}s"

corpus_path() {
  "$BIN" --gen-only --corpus "$1" | sed -n 's/.*"corpus_path":"\([^"]*\)".*/\1/p'
}

run() { # env-name value corpus-name path tag
  env "$1=$2" "$BIN" --lane term --from-file "$4" > "$O/$5.json" 2> "$O/$5.err"
  rc=$?
  secs=$(sed -n 's/.*"secs":\([0-9.]*\).*/\1/p' "$O/$5.json")
  echo "$5 rc=$rc secs=$secs load=$(load1)"
}

for pair in FT_PARSER_SIMD:long_lines FT_PARSER_SIMD:seq_lines FT_PARSER_SIMD:color_random \
  FT_PARSER_SIMD:color_emoji_random FT_PARSER_SIMD_UTF8:unicode_mix \
  FT_PARSER_SIMD_UTF8:color_emoji_random; do
  var=${pair%%:*}
  corpus=${pair#*:}
  path=$(corpus_path "$corpus")
  [ -f "$path" ] || { echo "no corpus for $corpus"; continue; }
  for i in 1 2 3 4 5 6; do
    if [ $((i % 2)) -eq 1 ]; then order="1 0"; else order="0 1"; fi
    for v in $order; do
      run "$var" "$v" "$corpus" "$path" "$var-$corpus-$v-$i"
    done
  done
done
echo "done $(date +%H:%M:%S)"
