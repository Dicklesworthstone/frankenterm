#!/usr/bin/env bash
# Tests for scripts/ghostty-headless-h2h.sh and scripts/ghostty_h2h.py
# (ft-yccm0.1.3). Needs neither Ghostty nor zig: it builds a fake world
# (a git checkout, a zig tarball, Ghostty.app, ghostty-bench, the width probe
# and ingest_throughput, from tests/fixtures/ghostty-h2h) and drives the real
# runner through hyperfine against it:
#   - shellcheck of every shell file involved;
#   - the helper's unit tests;
#   - check-pins, stamp reuse, a measured receipt (ABBA order, primary corpus
#     first, SHA256SUMS), width disagreement, the self-test, gate overrides;
#   - every fail-closed case: commit, dirty checkout, a build writing into
#     the checkout, zig tarball and version, Ghostty.app, Ghostty version,
#     a broken contract, a damaged toolchain, the FT binary's layout and
#     sanity, corpus identity, receipt directory reuse, a failed download.
#
# Usage: scripts/test_ghostty_headless_h2h.sh   (H2H_TEST_KEEP=1 keeps the world)

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
RUNNER="$SCRIPT_DIR/ghostty-headless-h2h.sh"
HELPER="$SCRIPT_DIR/ghostty_h2h.py"
FIXTURES="$REPO_ROOT/tests/fixtures/ghostty-h2h"
CONTRACT="$REPO_ROOT/docs/perf/incumbents/ghostty.md"

PASS=0
FAIL=0
pass() { PASS=$((PASS + 1)); printf 'ok   %s\n' "$1"; }
fail() {
    FAIL=$((FAIL + 1))
    printf 'FAIL %s\n' "$1"
    if [[ -f ${T:-}/last.err ]]; then sed 's/^/     | /' "$T/last.err" | tail -15; fi
}
check() {
    local name="$1"
    shift
    if "$@"; then pass "$name"; else fail "$name"; fi
}
h2h() { python3 -I "$HELPER" "$@"; }

# --- static checks ----------------------------------------------------------

for tool in shellcheck hyperfine python3 git shasum tar; do
    command -v "$tool" >/dev/null 2>&1 || { echo "missing required tool: $tool" >&2; exit 2; }
done
check "shellcheck" shellcheck "$RUNNER" "${BASH_SOURCE[0]}" "$FIXTURES/fake-zig" "$FIXTURES/fake-ghostty-bench"
check "helper unit tests" python3 -I "$SCRIPT_DIR/test_ghostty_h2h.py"

# --- the fake world -----------------------------------------------------------

T="$(mktemp -d "${TMPDIR:-/tmp}/ghostty-h2h-test.XXXXXX")"
T="$(cd -- "$T" && pwd -P)"
cleanup() {
    if [[ ${H2H_TEST_KEEP:-} == 1 ]]; then
        echo "kept the test world at $T"
    else
        rm -rf -- "$T"
    fi
}
trap cleanup EXIT

export FAKE_BIN_DIR="$FIXTURES"
export FAKE_LOG="$T/zig-builds.log"
: >"$FAKE_LOG"

make_checkout() {
    local dir="$1"
    mkdir -p "$dir"
    git -C "$dir" init -q
    printf '.{\n    .name = .ghostty,\n    .version = "1.3.2-dev",\n    .minimum_zig_version = "0.16.0",\n}\n' >"$dir/build.zig.zon"
    printf '.zig-cache/\nzig-out/\n' >"$dir/.gitignore"
    git -C "$dir" add build.zig.zon .gitignore
    git -C "$dir" -c user.name=h2h-test -c user.email=h2h-test@invalid commit -q -m "fake ghostty"
}
make_checkout "$T/ghostty"
COMMIT="$(git -C "$T/ghostty" rev-parse HEAD)"
git clone -q "$T/ghostty" "$T/ghostty-dirty"
git clone -q "$T/ghostty" "$T/ghostty-writes"

mkdir -p "$T/zigsrc/zig-fake-0.16.0"
cp "$FIXTURES/fake-zig" "$T/zigsrc/zig-fake-0.16.0/zig"
tar -cJf "$T/zig-fake.tar.xz" -C "$T/zigsrc" zig-fake-0.16.0
ZIG_SHA="$(shasum -a 256 "$T/zig-fake.tar.xz" | awk '{print $1}')"

APP_VERSION="$(h2h pins "$CONTRACT" | python3 -I -c 'import json, sys; print(json.load(sys.stdin)["app_short_version"])')"
APP_BUILD="$(h2h pins "$CONTRACT" | python3 -I -c 'import json, sys; print(json.load(sys.stdin)["app_build"])')"
mkdir -p "$T/Ghostty.app/Contents/MacOS"
printf 'fake ghostty binary\n' >"$T/Ghostty.app/Contents/MacOS/ghostty"
python3 -I -c '
import plistlib, sys
with open(sys.argv[1], "wb") as handle:
    plistlib.dump({"CFBundleIdentifier": "com.mitchellh.ghostty", "CFBundleShortVersionString": sys.argv[2], "CFBundleVersion": sys.argv[3]}, handle)
' "$T/Ghostty.app/Contents/Info.plist" "$APP_VERSION" "$APP_BUILD"
APP_SHA="$(shasum -a 256 "$T/Ghostty.app/Contents/MacOS/ghostty" | awk '{print $1}')"

FT_BIN="$T/ft-target/release-perf/examples/ingest_throughput"
mkdir -p "$(dirname "$FT_BIN")"
cp "$FIXTURES/fake-ingest-throughput" "$FT_BIN"

# make_contract OUT [key=value ...]: the real contract with fake-world pins.
make_contract() {
    local out="$1"
    shift
    python3 -I -c '
import re, sys
text = open(sys.argv[1]).read()
for pair in sys.argv[3:]:
    key, _, value = pair.partition("=")
    if value == "<delete>":
        text, count = re.subn(rf"(?m)^{re.escape(key)} = .*\n", "", text)
    else:
        text, count = re.subn(rf"(?m)^{re.escape(key)} = .*$", f"{key} = {value}", text)
    assert count == 1, pair
open(sys.argv[2], "w").write(text)
' "$CONTRACT" "$out" \
        "ghostty_commit=$COMMIT" \
        "zig_tarball=zig-fake.tar.xz" \
        "zig_tarball_url=https://example.invalid/zig-fake.tar.xz" \
        "zig_tarball_sha256=$ZIG_SHA" \
        "zig_tarball_top_dir=zig-fake-0.16.0" \
        "app_path=$T/Ghostty.app" \
        "app_binary_sha256=$APP_SHA" \
        "geometries=4x10" \
        "min_runs=4" \
        "warmup=1" \
        "max_cv_pct=1000" \
        "max_load_1m=1000" \
        "$@"
}
C="$T/contract.md"
make_contract "$C"

BASE=(--contract "$C" --ghostty-src "$T/ghostty" --zig-tarball "$T/zig-fake.tar.xz")
MEASURE=(--ft-bin "$FT_BIN" --size 3KiB --rounds 4)

RC=0
run() {
    set +e
    "$RUNNER" "$@" >"$T/last.out" 2>"$T/last.err"
    RC=$?
    set -e
}
expect() {
    local name="$1" want="$2" pattern="$3"
    shift 3
    run "$@"
    if [[ $RC -ne $want ]]; then
        fail "$name (exit $RC, want $want)"
    elif [[ -n $pattern ]] && ! grep -q -- "$pattern" "$T/last.err" "$T/last.out"; then
        fail "$name (no '$pattern' in the output)"
    else
        pass "$name"
    fi
}
builds() { grep -c "^$1 " "$FAKE_LOG" || true; }
get() { h2h get "$1" "$2"; }

# --- pins and builds ------------------------------------------------------------

expect "check-pins verifies the world and builds both binaries" 0 "PINS OK" \
    "${BASE[@]}" --work "$T/w1" --check-pins
check "ghostty-bench was built once, from the checkout, with the pinned flags" \
    grep -q "^bench $T/ghostty -Demit-bench -Doptimize=ReleaseFast -Demit-macos-app=false" "$FAKE_LOG"
check "the probe was built once, against the checkout" test "$(builds probe)" -eq 1
expect "a second check-pins reuses both stamped binaries" 0 "reusing ghostty-bench" \
    "${BASE[@]}" --work "$T/w1" --check-pins
check "nothing was rebuilt" test "$(builds bench):$(builds probe)" = "1:1"
check "the checkout is still clean" test -z "$(git -C "$T/ghostty" status --porcelain --ignored)"

# --- a measured receipt -----------------------------------------------------------

R1="$T/w1/receipts/m1"
expect "a measured run admits a verdict for every row" 0 "VERDICT primary=color_emoji_random ft_faster" \
    "${BASE[@]}" --work "$T/w1" "${MEASURE[@]}" --corpus color_random,color_emoji_random --out "$R1"
check "the receipt validates" h2h validate "$R1/receipt.json"
check "the primary corpus is listed first although requested second" \
    test "$(get "$R1/receipt.json" inputs.0.corpus):$(get "$R1/receipt.json" rows.0.corpus)" = "color_emoji_random:color_emoji_random"
check "one row per corpus and geometry" test "$(get "$R1/receipt.json" rows.1.corpus)" = "color_random"
check "round 1 runs ghostty first" \
    test "$(get "$R1/rows/color_emoji_random@4x10/round-01.json" results.0.command)" = ghostty
check "round 2 runs frankenterm first (ABBA)" \
    test "$(get "$R1/rows/color_emoji_random@4x10/round-02.json" results.0.command)" = frankenterm
check "every arm has min_runs timed samples" test "$(get "$R1/receipt.json" rows.0.arms.ghostty.n)" -eq 4
check "the verdict follows the medians (fake ghostty sleeps 5x longer)" \
    python3 -I -c 'import json, sys; r = json.load(open(sys.argv[1]))["rows"][0]; sys.exit(0 if r["ft_speedup"] > 1.5 else 1)' "$R1/receipt.json"
check "both arms read the same bytes" \
    test "$(get "$R1/receipt.json" rows.0.input_sha256)" = "$(shasum -a 256 "$(get "$R1/receipt.json" inputs.0.path)" | awk '{print $1}')"
check "the width probes agree on the fake grid" test "$(get "$R1/receipt.json" emoji_width_parity.0.agree)" = true
sums_verify() { (cd -- "$1" && shasum -a 256 -c SHA256SUMS >/dev/null); }
check "SHA256SUMS verifies" sums_verify "$R1"
check "the receipt names the contract by hash" \
    test "$(get "$R1/receipt.json" contract.sha256)" = "$(shasum -a 256 "$C" | awk '{print $1}')"

R2="$T/w1/receipts/skew"
FAKE_FT_ROW_SKEW=3 expect "a width disagreement is recorded, not fatal" 0 "agree=False" \
    "${BASE[@]}" --work "$T/w1" "${MEASURE[@]}" --corpus color_emoji_random --out "$R2"
disagreement_mentions() { get "$1" emoji_width_parity.0.disagreements | grep -q -- "$2"; }
check "the disagreement names the rows written" disagreement_mentions "$R2/receipt.json" "rows written"

expect "a tightened load gate refuses the verdict (exit 5)" 5 "NO_ADMISSIBLE_RATIO (load: " \
    "${BASE[@]}" --work "$T/w1" "${MEASURE[@]}" --corpus color_emoji_random --max-load 0.0001
expect "the self-test runs both arms and the runs gate refuses" 0 "SELF-TEST OK" \
    "${BASE[@]}" --work "$T/w1" --ft-bin "$FT_BIN" --self-test

expect "a looser load gate is a usage error" 2 "may only tighten" \
    "${BASE[@]}" --work "$T/w1" "${MEASURE[@]}" --max-load 5000
expect "a looser CV gate is a usage error" 2 "may only tighten" \
    "${BASE[@]}" --work "$T/w1" "${MEASURE[@]}" --max-cv 2000
expect "fewer rounds than min_runs is a usage error" 2 "below the contract's min_runs" \
    "${BASE[@]}" --work "$T/w1" --ft-bin "$FT_BIN" --rounds 3

# --- fail closed ------------------------------------------------------------------

make_contract "$T/c-commit.md" "ghostty_commit=0000000000000000000000000000000000000000"
expect "a different checkout commit is drift" 3 "PIN DRIFT: the Ghostty checkout is at" \
    --contract "$T/c-commit.md" --ghostty-src "$T/ghostty" --zig-tarball "$T/zig-fake.tar.xz" --work "$T/w1" --check-pins
printf 'local edit\n' >"$T/ghostty-dirty/untracked.txt"
expect "a dirty checkout is drift" 3 "local changes before the build" \
    --contract "$C" --ghostty-src "$T/ghostty-dirty" --zig-tarball "$T/zig-fake.tar.xz" --work "$T/w1" --check-pins
FAKE_ZIG_WRITE_IGNORED=1 expect "a build writing into the checkout, even under .gitignore, is drift" 3 "wrote into the Ghostty checkout" \
    --contract "$C" --ghostty-src "$T/ghostty-writes" --zig-tarball "$T/zig-fake.tar.xz" --work "$T/w-writes" --check-pins
make_contract "$T/c-zigsha.md" "zig_tarball_sha256=$(printf '0%.0s' {1..64})"
expect "a zig tarball with another SHA-256 is drift" 3 "has SHA-256" \
    --contract "$T/c-zigsha.md" --ghostty-src "$T/ghostty" --zig-tarball "$T/zig-fake.tar.xz" --work "$T/w1" --check-pins
FAKE_ZIG_VERSION=0.15.2 expect "a zig reporting another version is drift" 3 "zig reports version 0.15.2" \
    "${BASE[@]}" --work "$T/w1" --check-pins
make_contract "$T/c-app.md" "app_build=99999"
expect "a Ghostty.app update is drift" 3 "build is '$APP_BUILD', pinned '99999'" \
    --contract "$T/c-app.md" --ghostty-src "$T/ghostty" --zig-tarball "$T/zig-fake.tar.xz" --work "$T/w1" --check-pins
make_contract "$T/c-version.md" "ghostty_version=9.9.9"
expect "a different Ghostty version in build.zig.zon is drift" 3 "build.zig.zon says version 1.3.2-dev, pinned 9.9.9" \
    --contract "$T/c-version.md" --ghostty-src "$T/ghostty" --zig-tarball "$T/zig-fake.tar.xz" --work "$T/w1" --check-pins
make_contract "$T/c-broken.md" "max_load_1m=<delete>"
expect "a contract missing a pin fails closed" 3 "missing pins: max_load_1m" \
    --contract "$T/c-broken.md" --ghostty-src "$T/ghostty" --zig-tarball "$T/zig-fake.tar.xz" --work "$T/w1" --check-pins

zig_home="$(head -1 "$T"/w1/toolchain/*.current)"
printf '# damaged\n' >>"$zig_home/zig-fake-0.16.0/zig"
expect "a damaged extracted toolchain is replaced from the verified tarball" 0 "no longer matches its digest" \
    "${BASE[@]}" --work "$T/w1" --check-pins
check "the damaged tree was left in place and a fresh one used" \
    test "$(find "$T/w1/toolchain" -mindepth 1 -maxdepth 1 -type d | wc -l | tr -d ' ')" -eq 2

mkdir -p "$T/elsewhere"
cp "$FT_BIN" "$T/elsewhere/ingest_throughput"
expect "an FT binary outside its cargo profile directory is refused" 4 "must sit at" \
    "${BASE[@]}" --work "$T/w1" --ft-bin "$T/elsewhere/ingest_throughput" --size 3KiB --rounds 4 --corpus color_emoji_random
FAKE_FT_SANITY="no visible text" expect "an FT run failing its sanity checks stops the run" 4 "failed" \
    "${BASE[@]}" --work "$T/w1" "${MEASURE[@]}" --corpus color_emoji_random
FAKE_FT_BAD_SHA=1 expect "a corpus whose bytes differ from the generator's claim is refused" 4 "differs from the generator" \
    "${BASE[@]}" --work "$T/w1" "${MEASURE[@]}" --corpus color_emoji_random
expect "a used receipt directory is never overwritten" 2 "receipts are never overwritten" \
    "${BASE[@]}" --work "$T/w1" "${MEASURE[@]}" --out "$R1"
expect "a work directory inside the checkout is refused before it is created" 2 "inside the Ghostty checkout" \
    "${BASE[@]}" --work "$T/ghostty/scratch" --check-pins
check "nothing was created inside the checkout" test ! -e "$T/ghostty/scratch"
expect "a missing tarball without --fetch-zig is a usage error" 2 "pass --zig-tarball PATH or --fetch-zig" \
    --contract "$C" --ghostty-src "$T/ghostty" --work "$T/w-fetch" --check-pins
expect "a failed download stops the run" 4 "the zig download failed" \
    --contract "$C" --ghostty-src "$T/ghostty" --work "$T/w-fetch" --fetch-zig --check-pins

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[[ $FAIL -eq 0 ]]
