#!/bin/sh
# ft-3floy native A/B: seq_lines term-lane throughput and profile before
# (parent of a32ff4f42: byte-wise String::zeroize) and after (a32ff4f42:
# word-wise wipe), same corpus bytes, interleaved, plus one 4 s sample each.
# Follows evidence/mac-render-perf/2026-10-12/seq-gapfill-70s4z/70s4z-ab.sh.
set -u
SRC=$SCRATCH/ftb/frankenterm
R=$SCRATCH/e2e/recycle
mkdir -p "$R"
export RCH_CARGO_WRAPPER_BYPASS=1
cd "$SRC" || exit 1
[ -z "$(git status --porcelain)" ] || { echo "tree dirty"; exit 1; }
true

build() {
  echo "build $1 $(date +%H:%M:%S)"
  CARGO_TARGET_DIR="$SRC/target" nice -n 10 cargo build -j 12 -p frankenterm-term \
    --profile release-perf --example ingest_throughput > "$R/build-$1.log" 2>&1 \
    || { echo "build $1 failed"; tail -5 "$R/build-$1.log"; return 1; }
  cp "$SRC/target/release-perf/examples/ingest_throughput" "$R/ingest-$1"
}

build after || exit 1
git checkout -q HEAD^ -- frankenterm/surface/src/line/line.rs frankenterm/surface/src/line/clusterline.rs frankenterm/term/src/screen.rs
build before; rc=$?
git checkout -q HEAD -- frankenterm/surface/src/line/line.rs frankenterm/surface/src/line/clusterline.rs frankenterm/term/src/screen.rs
[ -z "$(git status --porcelain)" ] || { echo "tree not restored"; git status --short; exit 1; }
[ $rc = 0 ] || exit 1

export FT_CORPUS_DIR="$R/corpus"
"$R/ingest-after" --gen-only --corpus seq_lines > "$R/gen.json"
corpus=$(sed -n 's/.*"corpus_path":"\([^"]*\)".*/\1/p' "$R/gen.json")
[ -f "$corpus" ] || { echo "no corpus: $(cat "$R/gen.json")"; exit 1; }
echo "corpus $corpus $(shasum -a 256 "$corpus" | cut -c1-16)"

for i in 1 2 3; do
  for name in before after; do
    load=$(uptime | sed 's/.*averages: //')
    FT_BENCH_GIT_SHA=$name "$R/ingest-$name" --lane term --from-file "$corpus" \
      > "$R/run-$name-$i.json" 2> "$R/run-$name-$i.err"
    echo "run $i $name rc=$? load=[$load] $(grep BENCH "$R/run-$name-$i.err" | tail -1 | sed 's/.*secs=/secs=/')"
  done
done

for name in before after; do
  "$R/ingest-$name" --lane term --from-file "$corpus" > "$R/prof-$name.json" 2> "$R/prof-$name.err" &
  pid=$!
  sleep 1
  /usr/bin/sample "$pid" 4 -file "$R/sample-$name.txt" > /dev/null 2>&1
  wait "$pid"
  echo "sampled $name load=[$(uptime | sed 's/.*averages: //')]"
done
echo done
