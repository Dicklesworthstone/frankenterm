#!/bin/sh
# Native FT-vs-FT A/Bs of two runtime kill switches on one release-perf
# ingest_throughput build (term lane), arms interleaved three times:
#   ft-yg1lt  FT_PRESENTATION_VS_SKIP=0 (old for_grapheme path) vs default
#   ft-951ev  FT_LINE_COMPRESS_SLICE=0  (old compress_for_scrollback) vs default
# Corpora: a 64 MiB slice of the operator's T0 file (~/color-emoji-random.bin)
# and the generated 64 MiB seq_lines corpus.
set -u
SRC=/private/tmp/claude-501/-Users-jemanuel-projects/62f910a5-5c5c-4f7b-925b-c76d31c3007c/scratchpad/ftb/frankenterm
R=/private/tmp/claude-501/-Users-jemanuel-projects/62f910a5-5c5c-4f7b-925b-c76d31c3007c/scratchpad/killswitch-ab
export RCH_CARGO_WRAPPER_BYPASS=1
cd "$SRC" || exit 1
echo "HEAD $(git rev-parse --short HEAD)"
CARGO_TARGET_DIR="$SRC/target" nice -n 10 cargo build -j 12 -p frankenterm-term \
  --profile release-perf --example ingest_throughput > "$R/build.log" 2>&1 \
  || { echo "build failed"; tail -5 "$R/build.log"; exit 1; }
cp "$SRC/target/release-perf/examples/ingest_throughput" "$R/ingest"

head -c 67108864 "$HOME/color-emoji-random.bin" > "$R/t0-64m.bin"
export FT_CORPUS_DIR="$R/corpus"
"$R/ingest" --gen-only --corpus seq_lines > "$R/gen.json"
seq=$(sed -n 's/.*"corpus_path":"\([^"]*\)".*/\1/p' "$R/gen.json")
[ -f "$seq" ] || { echo "no seq corpus: $(cat "$R/gen.json")"; exit 1; }
echo "t0 $(shasum -a 256 "$R/t0-64m.bin" | cut -c1-16) seq $(shasum -a 256 "$seq" | cut -c1-16)"

run() { # label corpus env...
  label=$1; corpus=$2; shift 2
  load=$(uptime | sed 's/.*averages: //')
  env "$@" "$R/ingest" --lane term --from-file "$corpus" > "$R/run-$label.json" 2> "$R/run-$label.err"
  echo "$label rc=$? load=[$load] $(python3 -c "import json,sys; d=json.load(open('$R/run-$label.json')); print('MiB/s=%.1f' % d['mib_per_s'], 'sanity=%s' % d.get('sanity'), 'fp=%s' % d.get('state_fingerprint'))" 2>/dev/null || tail -1 "$R/run-$label.err")"
}

for i in 1 2 3; do
  run "yg1lt-t0-old-$i" "$R/t0-64m.bin" FT_PRESENTATION_VS_SKIP=0
  run "yg1lt-t0-new-$i" "$R/t0-64m.bin" FT_BENCH_ARM=default
  run "951ev-t0-old-$i" "$R/t0-64m.bin" FT_LINE_COMPRESS_SLICE=0
  run "951ev-t0-new-$i" "$R/t0-64m.bin" FT_BENCH_ARM=default
  run "951ev-seq-old-$i" "$seq" FT_LINE_COMPRESS_SLICE=0
  run "951ev-seq-new-$i" "$seq" FT_BENCH_ARM=default
done
echo DONE
