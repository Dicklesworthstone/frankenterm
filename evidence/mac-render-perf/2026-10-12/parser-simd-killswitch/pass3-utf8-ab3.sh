#!/bin/sh
# Third pass, FT_PARSER_SIMD_UTF8 only: pass 2 ran at load 58-107 (CV
# 27-51%), so this waits for facts-bench.sh to finish and the 1-minute load
# to fall below 40 (up to 3 h), then runs 10 ABBA pairs on unicode_mix
# (64 MiB) and 6 on color_emoji_random (512 MiB), same 15a2b1c46 binary.
set -u
R=$HOME/ft-build/70s4z
O=$HOME/ft-build/utf8-ab3
BIN=$R/ingest-head
mkdir -p "$O"
export FT_CORPUS_DIR=$R/corpus
load1() { uptime | sed 's/.*averages: //' | cut -d' ' -f1; }
while pgrep -f facts-bench.sh > /dev/null; do sleep 30; done
waited=0
while [ "$(load1 | cut -d. -f1)" -ge 40 ] && [ "$waited" -lt 10800 ]; do
  sleep 60
  waited=$((waited + 60))
done
echo "start $(date +%H:%M:%S) load=$(load1) waited=${waited}s"
corpus_path() {
  "$BIN" --gen-only --corpus "$1" --size "$2" | sed -n 's/.*"corpus_path":"\([^"]*\)".*/\1/p'
}
run() { # value path tag
  env "FT_PARSER_SIMD_UTF8=$1" "$BIN" --lane term --from-file "$2" > "$O/$3.json" 2> "$O/$3.err"
  rc=$?
  secs=$(sed -n 's/.*"secs":\([0-9.]*\).*/\1/p' "$O/$3.json")
  echo "$3 rc=$rc secs=$secs load=$(load1)"
}
for spec in unicode_mix:64MiB:10 color_emoji_random:512MiB:6; do
  corpus=${spec%%:*}
  rest=${spec#*:}
  size=${rest%%:*}
  pairs=${rest#*:}
  path=$(corpus_path "$corpus" "$size")
  [ -f "$path" ] || { echo "no corpus for $corpus"; continue; }
  echo "corpus $corpus $size $(shasum -a 256 "$path" | cut -c1-16)"
  i=0
  while [ "$i" -lt "$pairs" ]; do
    i=$((i + 1))
    if [ $((i % 2)) -eq 1 ]; then order="1 0"; else order="0 1"; fi
    for v in $order; do
      run "$v" "$path" "FT_PARSER_SIMD_UTF8-$corpus-$v-$i"
    done
  done
done
echo "done $(date +%H:%M:%S)"
