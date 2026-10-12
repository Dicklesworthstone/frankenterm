#!/bin/sh
# Native measurements, sequential (OpusSession5351). Gates relaxed after the
# host held load 70-120 for 45 minutes; the facts bench enforces its own CV
# admissibility, and the GUI A/Bs compare counts as well as time.
# 1. ft-yccm0.2.2.1 AC4: render_facts_publication ABBA, 32 MiB x 20 pairs.
# 2. ft-yccm0.2.2.3 owner data: T0 capture budget 4 ms (ft) vs 1 ms (ft_baseline).
# 3. ft-yccm0.2.6 AC3: first atlas 1024 px (ft) vs 128 px (ft_baseline).
# GUI: release-perf build of a4d155502's code (5351e), copied to capture-ab/.
set -u
load1() { uptime | sed 's/.*averages: //' | cut -d' ' -f1; }
wait_load() { # ceiling max_seconds
  w=0
  while [ "$(load1 | cut -d. -f1)" -ge "$1" ] && [ "$w" -lt "$2" ]; do sleep 60; w=$((w + 60)); done
  echo "  waited ${w}s for load < $1, now $(load1)"
}
export RCH_CARGO_WRAPPER_BYPASS=1
E=$HOME/ft-build/frankenterm-5351e
OUT=$HOME/ft-build/capture-ab
FT=$HOME/ft-staging/5351e/app/FrankenTerm.app/Contents/MacOS/ft

echo "1 facts bench $(date +%H:%M:%S)"
wait_load 70 7200
( cd "$HOME/ft-build/frankenterm-5351g" && FT_RENDER_FACTS_CORPUS_BYTES=33554432 FT_RENDER_FACTS_PAIRS=20 \
  CARGO_TARGET_DIR=$HOME/cargo-targets/ft-5351-bench ~/.cargo/bin/cargo bench -p mux \
  --features bench-hooks --bench render_facts_publication -- abba-only-no-criterion-match ) \
  > "$OUT/facts-bench2.log" 2>&1
echo "  bench rc=$? $(date +%H:%M:%S) load=$(load1)"
grep -E '^\[BENCH\]' "$OUT/facts-bench2.log"

echo "2 capture A/B $(date +%H:%M:%S)"
wait_load 90 7200
( cd "$E" && bash scripts/mac-gui-throughput.sh --gui-bin "$OUT/frankenterm-gui" --ft-bin "$FT" \
  --corpus-file "$HOME/color-emoji-random.bin" --no-fps --no-beachball-probe \
  --baseline ft --baseline-env FRANKENTERM_RENDER_CAPTURE_WAIT_MS=1 --reps 3 \
  --out "$OUT/harness" ) > "$OUT/harness.log" 2>&1
echo "  capture harness rc=$? $(date +%H:%M:%S) load=$(load1)"

echo "3 atlas A/B $(date +%H:%M:%S)"
wait_load 90 7200
( cd "$E" && bash scripts/mac-gui-throughput.sh --gui-bin "$OUT/frankenterm-gui" --ft-bin "$FT" \
  --corpus-file "$HOME/color-emoji-random.bin" --no-fps --no-beachball-probe \
  --baseline ft --baseline-ft-lua 'config.glyph_atlas_initial_size = 128' --reps 3 \
  --out "$OUT/atlas-harness" ) > "$OUT/atlas-harness.log" 2>&1
echo "  atlas harness rc=$? $(date +%H:%M:%S) load=$(load1)"
echo done
