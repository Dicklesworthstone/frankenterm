#!/bin/sh
# Second pass of the SIMD kill-switch ABBA: 512 MiB corpora (runs of about
# 2 s, not 0.24 s) and 10 pairs, on the same 15a2b1c46 release-perf binary.
set -u
R=$HOME/ft-build/70s4z
O=$HOME/ft-build/simd-ab2
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

corpus_path() { # corpus size
  "$BIN" --gen-only --corpus "$1" --size "$2" | sed -n 's/.*"corpus_path":"\([^"]*\)".*/\1/p'
}

run() { # env-name value path tag
  env "$1=$2" "$BIN" --lane term --from-file "$3" > "$O/$4.json" 2> "$O/$4.err"
  rc=$?
  secs=$(sed -n 's/.*"secs":\([0-9.]*\).*/\1/p' "$O/$4.json")
  echo "$4 rc=$rc secs=$secs load=$(load1)"
}

for spec in FT_PARSER_SIMD:long_lines:512MiB FT_PARSER_SIMD:color_random:512MiB \
  FT_PARSER_SIMD:color_emoji_random:512MiB FT_PARSER_SIMD_UTF8:unicode_mix:64MiB \
  FT_PARSER_SIMD_UTF8:color_emoji_random:512MiB; do
  var=${spec%%:*}
  rest=${spec#*:}
  corpus=${rest%%:*}
  size=${rest#*:}
  path=$(corpus_path "$corpus" "$size")
  [ -f "$path" ] || { echo "no corpus for $corpus $size"; continue; }
  echo "corpus $corpus $size $(shasum -a 256 "$path" | cut -c1-16)"
  for i in 1 2 3 4 5 6 7 8 9 10; do
    if [ $((i % 2)) -eq 1 ]; then order="1 0"; else order="0 1"; fi
    for v in $order; do
      run "$var" "$v" "$path" "$var-$corpus-$v-$i"
    done
  done
done
echo "done $(date +%H:%M:%S)"
