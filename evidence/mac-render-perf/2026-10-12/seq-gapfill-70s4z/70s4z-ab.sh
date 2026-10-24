#!/bin/sh
# ft-70s4z native A/B: seq_lines term-lane throughput before (93e9176b9) and
# after (5e4a9b3d7) the gap-fill fix, plus HEAD, on identical corpus bytes,
# interleaved; then one /usr/bin/sample of each of before and head.
set -u
B=$HOME/ft-build
E=$B/frankenterm-5351e
R=$B/70s4z
mkdir -p "$R"
export RCH_CARGO_WRAPPER_BYPASS=1

build() {
  name=$1
  dir=$2
  t=$HOME/cargo-targets/ft-5351-70s4z-$name
  echo "build $name $(date +%H:%M:%S)"
  (cd "$dir" && CARGO_TARGET_DIR=$t ~/.cargo/bin/cargo build -p frankenterm-term \
    --profile release-perf --example ingest_throughput) > "$R/build-$name.log" 2>&1 \
    || { echo "build $name failed"; exit 1; }
  cp "$t/release-perf/examples/ingest_throughput" "$R/ingest-$name"
}

for pair in before:93e9176b9 after:5e4a9b3d7; do
  name=${pair%%:*}
  sha=${pair#*:}
  W=$B/wt-70s4z-$name
  [ -d "$W" ] || git -C "$E" worktree add --detach "$W" "$sha" > /dev/null 2>&1 \
    || { echo "worktree $name failed"; exit 1; }
  [ "$(git -C "$W" rev-parse --short=9 HEAD)" = "$sha" ] || { echo "worktree $name wrong sha"; exit 1; }
  build "$name" "$W"
done
build head "$B/frankenterm-5351g"

export FT_CORPUS_DIR=$R/corpus
"$R/ingest-head" --gen-only --corpus seq_lines > "$R/gen.json"
corpus=$(sed -n 's/.*"corpus_path":"\([^"]*\)".*/\1/p' "$R/gen.json")
[ -f "$corpus" ] || { echo "no corpus: $(cat "$R/gen.json")"; exit 1; }
echo "corpus $corpus $(shasum -a 256 "$corpus" | cut -c1-16)"

for i in 1 2 3; do
  for name in before after head; do
    load=$(uptime | sed 's/.*averages: //')
    s=$(date +%s)
    FT_BENCH_GIT_SHA=$name "$R/ingest-$name" --lane term --from-file "$corpus" \
      > "$R/run-$name-$i.json" 2> "$R/run-$name-$i.err"
    rc=$?
    e=$(date +%s)
    echo "run $i $name rc=$rc wall=$((e - s))s load=[$load] $(grep BENCH "$R/run-$name-$i.err" | tail -1)"
  done
done

for name in before head; do
  "$R/ingest-$name" --lane term --from-file "$corpus" > "$R/prof-$name.json" 2> "$R/prof-$name.err" &
  pid=$!
  sleep 1
  /usr/bin/sample "$pid" 4 -file "$R/sample-$name.txt" > /dev/null 2>&1
  wait "$pid"
  echo "sampled $name load=[$(uptime | sed 's/.*averages: //')]"
done
echo done
