#!/usr/bin/env bash
# Pinned Ghostty incumbent vs FrankenTerm headless ingest head-to-head
# (ft-yccm0.1.3, plan M.2).
#
# The contract is docs/perf/incumbents/ghostty.md. Its ```ghostty-h2h-pins
# block is the single source of truth: Ghostty commit, zig version and
# tarball SHA-256, build flags, ghostty-bench action and flags, Ghostty.app
# version, FrankenTerm arm settings and the gates. Any drift fails closed
# (exit 3) before anything is measured.
#
# Arms, on identical bytes and dimensions:
#   ghostty      ghostty-bench +terminal-stream --data=F --terminal-rows=R --terminal-cols=C
#   frankenterm  ingest_throughput --lane term --from-file F --rows R --cols C
#                --scrollback S --chunk K   (frankenterm-term example, release-perf)
# Each (corpus, geometry) row runs N rounds of one hyperfine invocation with
# one timed run per arm, alternating AB, BA, AB, ... (ABBA), so neither arm is
# always second in a thermal window. hyperfine runs without a shell (-N).
#
# Exit codes: 0 every row admitted a verdict; 5 the receipt was written but
# some row is NO_ADMISSIBLE_RATIO; 3 pin drift, nothing measured; 4 an arm,
# build or input failed; 2 usage; 1 internal error.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
HELPER="${SCRIPT_DIR}/ghostty_h2h.py"
PROBE_SRC="${SCRIPT_DIR}/ghostty-h2h-probe"
# AGENTS.md: every web request carries this user agent.
USER_AGENT="OpenAI File Downloader, XaiImageApiFetch/1.0"

CONTRACT="${REPO_ROOT}/docs/perf/incumbents/ghostty.md"
GHOSTTY_SRC="${GHOSTTY_SRC:-${HOME}/projects/ghostty}"
GHOSTTY_APP=""
ZIG_TARBALL=""
FETCH_ZIG=0
WORK_BASE="${TMPDIR:-/tmp}"
WORK="${WORK_BASE%/}/ft-ghostty-h2h"
FT_BIN=""
BUILD_FT=0
FT_PROFILE=""
FT_SCROLLBACK=""
FT_CHUNK=""
CORPUS=""
CORPUS_FILES=()
SIZE="64MiB"
SEED=""
GEOMETRY=""
ROUNDS=""
WARMUP=""
MAX_CV=""
MAX_LOAD=""
OUT=""
MODE="measure"
CHECK_PINS=0

usage() {
    cat <<'EOF'
Usage: scripts/ghostty-headless-h2h.sh [options]

Pinned Ghostty incumbent vs FrankenTerm headless ingest, ABBA-interleaved with
hyperfine (ft-yccm0.1.3). Contract: docs/perf/incumbents/ghostty.md.

Modes:
  (default)              measure every corpus x geometry row, write the receipt
  --check-pins           verify every pin, build or verify ghostty-bench and the
                         width probe, then stop
  --self-test            one tiny corpus through both real arms (2 rounds);
                         checks the receipt schema and that the runs gate refuses

Incumbent:
  --contract PATH        contract file (default docs/perf/incumbents/ghostty.md)
  --ghostty-src DIR      pinned Ghostty checkout, only ever read
                         (default $GHOSTTY_SRC, else ~/projects/ghostty)
  --ghostty-app PATH     Ghostty.app to verify (default: the contract's app_path)
  --zig-tarball PATH     the pinned zig tarball; verified, then extracted under
                         --work (default WORK/dl/<pinned file name>)
  --fetch-zig            download the pinned tarball into a fresh directory
                         under WORK/dl when it is not there yet

FrankenTerm arm:
  --ft-bin PATH          ingest_throughput, in place in its cargo target dir:
                         <target>/<profile>/examples/ingest_throughput
  --build-ft             build it natively into WORK/ft-target first
  --ft-profile NAME      release-perf | release-interactive (default: contract)
  --ft-scrollback N      FrankenTerm scrollback lines (default: contract)
  --ft-chunk BYTES       bytes per feed call (default: contract, = Ghostty's read size)

Workload:
  --corpus LIST          comma-separated ingest_throughput corpora, all, or none
                         (default all; none when only --corpus-file is given)
  --corpus-file PATH     also measure PATH, e.g. the operator's
                         color-emoji-random.bin (repeatable)
  --size SIZE            generated corpus size (default 64MiB)
  --seed N               generator seed (default: the generator's)
  --geometry LIST        comma-separated ROWSxCOLS (default: contract)

Gates (overrides may only tighten the contract):
  --rounds N             ABBA rounds = timed runs per arm (>= contract min_runs)
  --warmup N             hyperfine warmup runs per arm before round 1
  --max-cv PCT           refuse a verdict above this coefficient of variation
  --max-load L           refuse a verdict above this 1-minute load average

Output:
  --work DIR             scratch: toolchain, builds, corpora
                         (default ${TMPDIR:-/tmp}/ft-ghostty-h2h)
  --out DIR              receipt directory, must be new or empty
                         (default WORK/receipts/<UTC time>)
EOF
}

ts() { date -u +%Y-%m-%dT%H:%M:%SZ; }
log() { printf '%s [h2h] %s\n' "$(ts)" "$*" >&2; }
die() {
    local code="$1"
    shift
    log "ERROR: $*"
    exit "$code"
}
drift() {
    log "PIN DRIFT: $*"
    log "nothing was measured. Re-pin deliberately in ${CONTRACT} (section Re-pinning), never by flag."
    exit 3
}
h2h() { python3 -I "$HELPER" "$@"; }
sha256() { shasum -a 256 "$1" | awk '{print $1}'; }
need_value() { [[ $# -ge 2 && -n $2 ]] || die 2 "$1 needs a value"; }
realdir() { (cd -- "$1" && pwd -P); }
# Resolves a path that may not exist yet, so a check can run before mkdir.
realpath_any() { python3 -I -c 'import os, sys; print(os.path.realpath(sys.argv[1]))' "$1"; }
outside_checkout() {
    local flag="$1" path
    path="$(realpath_any "$2")"
    case "$path/" in
        "$GHOSTTY_SRC/"*) die 2 "$flag $path is inside the Ghostty checkout; it must stay outside" ;;
    esac
}

while (($#)); do
    case "$1" in
        --contract) need_value "$@"; CONTRACT="$2"; shift 2 ;;
        --ghostty-src) need_value "$@"; GHOSTTY_SRC="$2"; shift 2 ;;
        --ghostty-app) need_value "$@"; GHOSTTY_APP="$2"; shift 2 ;;
        --zig-tarball) need_value "$@"; ZIG_TARBALL="$2"; shift 2 ;;
        --fetch-zig) FETCH_ZIG=1; shift ;;
        --ft-bin) need_value "$@"; FT_BIN="$2"; shift 2 ;;
        --build-ft) BUILD_FT=1; shift ;;
        --ft-profile) need_value "$@"; FT_PROFILE="$2"; shift 2 ;;
        --ft-scrollback) need_value "$@"; FT_SCROLLBACK="$2"; shift 2 ;;
        --ft-chunk) need_value "$@"; FT_CHUNK="$2"; shift 2 ;;
        --corpus) need_value "$@"; CORPUS="$2"; shift 2 ;;
        --corpus-file) need_value "$@"; CORPUS_FILES+=("$2"); shift 2 ;;
        --size) need_value "$@"; SIZE="$2"; shift 2 ;;
        --seed) need_value "$@"; SEED="$2"; shift 2 ;;
        --geometry) need_value "$@"; GEOMETRY="$2"; shift 2 ;;
        --rounds) need_value "$@"; ROUNDS="$2"; shift 2 ;;
        --warmup) need_value "$@"; WARMUP="$2"; shift 2 ;;
        --max-cv) need_value "$@"; MAX_CV="$2"; shift 2 ;;
        --max-load) need_value "$@"; MAX_LOAD="$2"; shift 2 ;;
        --work) need_value "$@"; WORK="$2"; shift 2 ;;
        --out) need_value "$@"; OUT="$2"; shift 2 ;;
        --check-pins) CHECK_PINS=1; shift ;;
        --self-test) MODE="self-test"; shift ;;
        -h | --help) usage; exit 0 ;;
        *) usage >&2; die 2 "unknown argument: $1" ;;
    esac
done

if ((CHECK_PINS)) && [[ $MODE == self-test ]]; then
    die 2 "--check-pins and --self-test are separate modes"
fi
for tool in python3 git shasum tar awk; do
    command -v "$tool" >/dev/null 2>&1 || die 2 "missing required tool: $tool"
done
if ! ((CHECK_PINS)); then
    command -v hyperfine >/dev/null 2>&1 || die 2 "missing required tool: hyperfine"
fi
[[ -d $GHOSTTY_SRC ]] || drift "the Ghostty checkout $GHOSTTY_SRC does not exist"
GHOSTTY_SRC="$(realdir "$GHOSTTY_SRC")"

if [[ $MODE == self-test ]]; then
    CORPUS="color_emoji_random"
    CORPUS_FILES=()
    SIZE="64KiB"
    ROUNDS="${ROUNDS:-2}"
    WARMUP="${WARMUP:-1}"
fi
if [[ -z $CORPUS ]]; then
    if ((${#CORPUS_FILES[@]})); then CORPUS="none"; else CORPUS="all"; fi
fi

outside_checkout --work "$WORK"
mkdir -p "$WORK"
WORK="$(realdir "$WORK")"
STAMP_UTC="$(date -u +%Y%m%dT%H%M%SZ)"
if [[ -z $OUT ]]; then
    if [[ $MODE == self-test ]]; then
        OUT="$WORK/self-test/$STAMP_UTC-$$"
    elif ((CHECK_PINS)); then
        OUT="$WORK/check-pins/$STAMP_UTC-$$"
    else
        OUT="$WORK/receipts/$STAMP_UTC-$$"
    fi
fi
outside_checkout --out "$OUT"
if [[ -d $OUT ]] && [[ -n "$(find "$OUT" -mindepth 1 -maxdepth 1 -print -quit)" ]]; then
    die 2 "--out $OUT already holds files; receipts are never overwritten"
fi
mkdir -p "$OUT"
OUT="$(realdir "$OUT")"
exec > >(tee -a "$OUT/run.log") 2> >(tee -a "$OUT/run.log" >&2)

FACTS="$OUT/facts.json"
PINS="$OUT/pins.json"
GATES="$OUT/gates.json"
fact() { h2h fact "$FACTS" "$@"; }
pin() { h2h pin "$PINS" "$1"; }

log "ft-yccm0.1.3 Ghostty headless head-to-head: mode=$MODE out=$OUT work=$WORK"
fact started_utc "$(ts)"

# ---------------------------------------------------------------------------
# 1. Contract and gates.

[[ -f $CONTRACT ]] || drift "the contract $CONTRACT does not exist"
h2h pins "$CONTRACT" >"$PINS" || drift "the contract's pin block is invalid (see above)"
fact contract.path "$CONTRACT"
fact contract.sha256 "$(sha256 "$CONTRACT")"
log "contract $CONTRACT sha256=$(sha256 "$CONTRACT")"

FT_PROFILE="${FT_PROFILE:-$(pin ft_profile)}"
FT_SCROLLBACK="${FT_SCROLLBACK:-$(pin ft_scrollback)}"
FT_CHUNK="${FT_CHUNK:-$(pin ft_chunk_bytes)}"
GEOMETRY="${GEOMETRY:-$(pin geometries)}"
GEOMETRY="${GEOMETRY//,/ }"
PRIMARY="$(pin primary_corpus)"
case "$FT_PROFILE" in
    release-perf | release-interactive) ;;
    *) die 2 "--ft-profile $FT_PROFILE: measure release-perf or release-interactive, never release (opt-level z) or debug" ;;
esac
[[ $FT_SCROLLBACK =~ ^[0-9]+$ ]] || die 2 "--ft-scrollback must be a non-negative integer"
[[ $FT_CHUNK =~ ^[1-9][0-9]*$ ]] || die 2 "--ft-chunk must be a positive integer number of bytes"
for geometry in $GEOMETRY; do
    [[ $geometry =~ ^[1-9][0-9]*x[1-9][0-9]*$ ]] || die 2 "bad geometry $geometry; expected ROWSxCOLS"
done
if [[ $FT_CHUNK != "$(pin bench_read_chunk_bytes)" ]]; then
    log "WARNING: FrankenTerm feeds $FT_CHUNK bytes per call; ghostty-bench reads $(pin bench_read_chunk_bytes)"
fi

gate_args=(--max-cv "$MAX_CV" --max-load "$MAX_LOAD" --rounds "$ROUNDS" --warmup "$WARMUP")
if [[ $MODE == self-test ]]; then gate_args+=(--self-test); fi
h2h gates "$PINS" "${gate_args[@]}" >"$GATES" || die 2 "invalid gate overrides (see above)"
ROUNDS="$(h2h get "$GATES" rounds)"
WARMUP="$(h2h get "$GATES" warmup)"
log "gates: rounds=$ROUNDS warmup=$WARMUP max_cv=$(h2h get "$GATES" max_cv_pct)% max_load_1m=$(h2h get "$GATES" max_load_1m) min_runs=$(h2h get "$GATES" min_runs)"

# ---------------------------------------------------------------------------
# 2. The Ghostty checkout: pinned commit, clean, matching version.

checkout_status() {
    git -C "$GHOSTTY_SRC" --no-optional-locks status --porcelain --untracked-files=all
}

verify_checkout() {
    local when="$1" head dirty version min_zig
    git -C "$GHOSTTY_SRC" rev-parse --git-dir >/dev/null 2>&1 || drift "$GHOSTTY_SRC is not a git checkout"
    head="$(git -C "$GHOSTTY_SRC" rev-parse HEAD)"
    [[ $head == "$(pin ghostty_commit)" ]] || drift "the Ghostty checkout is at $head, pinned $(pin ghostty_commit)"
    dirty="$(checkout_status)"
    if [[ -n $dirty ]]; then
        printf '%s\n' "$dirty" | head -20 >&2
        drift "the Ghostty checkout has local changes $when"
    fi
    version="$(sed -n 's/^[[:space:]]*\.version = "\(.*\)",[[:space:]]*$/\1/p' "$GHOSTTY_SRC/build.zig.zon" | head -1)"
    [[ $version == "$(pin ghostty_version)" ]] || drift "build.zig.zon says version $version, pinned $(pin ghostty_version)"
    min_zig="$(sed -n 's/^[[:space:]]*\.minimum_zig_version = "\(.*\)",[[:space:]]*$/\1/p' "$GHOSTTY_SRC/build.zig.zon" | head -1)"
    [[ $min_zig == "$(pin zig_version)" ]] || drift "build.zig.zon wants zig $min_zig, pinned $(pin zig_version)"
}

verify_checkout "before the build"
COMMIT="$(pin ghostty_commit)"
log "Ghostty checkout $GHOSTTY_SRC at $COMMIT, clean, version $(pin ghostty_version)"

# ---------------------------------------------------------------------------
# 3. zig: the pinned official tarball, verified before extraction.

ZIG_VERSION="$(pin zig_version)"
ZIG_NAME="$(pin zig_tarball)"
ZIG_SHA="$(pin zig_tarball_sha256)"
ZIG_TOP="$(pin zig_tarball_top_dir)"

fetch_zig() {
    local dir="$WORK/dl/fetch-$STAMP_UTC-$$" got
    mkdir -p "$WORK/dl"
    mkdir "$dir"
    log "downloading $(pin zig_tarball_url) into the fresh directory $dir"
    curl --fail --location --proto '=https' --tlsv1.2 --user-agent "$USER_AGENT" \
        --output "$dir/$ZIG_NAME" "$(pin zig_tarball_url)" || die 4 "the zig download failed"
    got="$(sha256 "$dir/$ZIG_NAME")"
    [[ $got == "$ZIG_SHA" ]] || drift "the downloaded zig tarball has SHA-256 $got, pinned $ZIG_SHA (kept at $dir)"
    if [[ ! -e "$WORK/dl/$ZIG_NAME" ]]; then
        mv "$dir/$ZIG_NAME" "$WORK/dl/$ZIG_NAME"
        ZIG_TARBALL="$WORK/dl/$ZIG_NAME"
    else
        ZIG_TARBALL="$dir/$ZIG_NAME"
    fi
}

if [[ -z $ZIG_TARBALL ]]; then ZIG_TARBALL="$WORK/dl/$ZIG_NAME"; fi
if [[ ! -f $ZIG_TARBALL ]]; then
    if ((FETCH_ZIG)); then
        fetch_zig
    else
        die 2 "no zig tarball at $ZIG_TARBALL; pass --zig-tarball PATH or --fetch-zig"
    fi
fi
ZIG_TARBALL_SHA="$(sha256 "$ZIG_TARBALL")"
[[ $ZIG_TARBALL_SHA == "$ZIG_SHA" ]] || drift "the zig tarball $ZIG_TARBALL has SHA-256 $ZIG_TARBALL_SHA, pinned $ZIG_SHA"
log "zig tarball $ZIG_TARBALL sha256=$ZIG_TARBALL_SHA (pinned)"

# Each extraction gets a fresh directory and a digest of every file it holds;
# a tree that no longer matches its digest is never used again (cleanup
# sweeps for `target` directories also hit zig's lib/std/Target on a
# case-insensitive file system). Nothing is deleted.
TOOLCHAINS="$WORK/toolchain"
ZIG_CURRENT="$TOOLCHAINS/zig-$ZIG_VERSION-${ZIG_SHA:0:12}.current"
mkdir -p "$TOOLCHAINS"
ZIG_HOME=""
if [[ -f $ZIG_CURRENT ]]; then
    candidate="$(head -1 "$ZIG_CURRENT")"
    if [[ -d $candidate && -f $candidate.digest ]]; then
        if [[ "$(h2h tree-digest "$candidate")" == "$(cat "$candidate.digest")" ]]; then
            ZIG_HOME="$candidate"
        else
            log "WARNING: the extracted toolchain $candidate no longer matches its digest (files missing or changed); extracting afresh"
        fi
    fi
fi
if [[ -z $ZIG_HOME ]]; then
    ZIG_HOME="$TOOLCHAINS/zig-$ZIG_VERSION-${ZIG_SHA:0:12}-$STAMP_UTC-$$"
    mkdir "$ZIG_HOME"
    log "extracting the verified tarball into $ZIG_HOME"
    tar -xJf "$ZIG_TARBALL" -C "$ZIG_HOME" || die 4 "extracting $ZIG_TARBALL failed"
    [[ -x "$ZIG_HOME/$ZIG_TOP/zig" ]] || drift "the tarball does not hold $ZIG_TOP/zig"
    h2h tree-digest "$ZIG_HOME" >"$ZIG_HOME.digest"
    printf '%s\n' "$ZIG_HOME" >"$ZIG_CURRENT"
fi
ZIG="$ZIG_HOME/$ZIG_TOP/zig"
ZIG_REPORTED="$("$ZIG" version)"
[[ $ZIG_REPORTED == "$ZIG_VERSION" ]] || drift "zig reports version $ZIG_REPORTED, pinned $ZIG_VERSION"
log "zig $ZIG_REPORTED at $ZIG ($(cat "$ZIG_HOME.digest"))"
fact zig.version "$ZIG_REPORTED"
fact zig.tarball "$ZIG_TARBALL"
fact zig.tarball_sha256 "$ZIG_TARBALL_SHA"
fact zig.path "$ZIG"

# ---------------------------------------------------------------------------
# 4. Ghostty.app (the GUI rows' incumbent): pinned version and binary.

GHOSTTY_APP="${GHOSTTY_APP:-$(pin app_path)}"
h2h app-info "$PINS" "$GHOSTTY_APP" >"$OUT/ghostty-app.json" || drift "$GHOSTTY_APP does not match the contract's Ghostty.app pins (see above)"
fact app "$OUT/ghostty-app.json" --type json
log "Ghostty.app $GHOSTTY_APP $(pin app_short_version) build $(pin app_build) (pinned)"

# ---------------------------------------------------------------------------
# 5. ghostty-bench, built outside the checkout (--prefix/--cache-dir in WORK).

read -r -a BUILD_FLAGS <<<"$(pin build_flags)"
GB_DIR="$WORK/ghostty-${COMMIT:0:12}-zig$ZIG_VERSION"
GB_BIN="$GB_DIR/out/$(pin bench_binary)"
GB_STAMP="$GB_DIR/stamp.json"
GB_KEY="$(printf '%s\n' "$COMMIT" "$ZIG_SHA" "${BUILD_FLAGS[*]}" | shasum -a 256 | awk '{print $1}')"

stamp_matches() {
    local stamp="$1" key="$2" binary="$3"
    [[ -f $stamp && -x $binary ]] || return 1
    [[ "$(h2h get "$stamp" build_key 2>/dev/null)" == "$key" ]] || return 1
    [[ "$(h2h get "$stamp" binary_sha256 2>/dev/null)" == "$(sha256 "$binary")" ]]
}

zig_build() {
    local what="$1" tree="$2" prefix="$3" cache="$4" logfile marker written
    shift 4
    logfile="$(dirname "$prefix")/build-$STAMP_UTC.log"
    # git status cannot see writes under ignored paths (.zig-cache, zig-out,
    # macos/GhosttyKit.xcframework), so also look for anything in the
    # checkout newer than a marker taken just before the build.
    marker="$(dirname "$prefix")/build-$STAMP_UTC.marker"
    : >"$marker"
    sleep 1
    log "building $what: (cd $tree && zig build $* --prefix $prefix --cache-dir $cache)"
    if ! (cd -- "$tree" && "$ZIG" build "$@" --prefix "$prefix" --cache-dir "$cache") >"$logfile" 2>&1; then
        tail -40 "$logfile" >&2
        die 4 "building $what failed; full log: $logfile"
    fi
    written="$(find "$GHOSTTY_SRC" -path "$GHOSTTY_SRC/.git" -prune -o -newer "$marker" ! -name .DS_Store -print | head -20)"
    if [[ -n $written ]]; then
        printf '%s\n' "$written" >&2
        drift "building $what wrote into the Ghostty checkout"
    fi
}

mkdir -p "$GB_DIR"
if stamp_matches "$GB_STAMP" "$GB_KEY" "$GB_BIN"; then
    log "reusing ghostty-bench $GB_BIN (stamp matches the pins)"
else
    zig_build ghostty-bench "$GHOSTTY_SRC" "$GB_DIR/out" "$GB_DIR/cache" "${BUILD_FLAGS[@]}"
    verify_checkout "after building ghostty-bench (the build must only read it)"
    [[ -x $GB_BIN ]] || die 4 "the build did not produce $GB_BIN"
    stamp_tmp="$GB_STAMP.$$"
    h2h fact "$stamp_tmp" build_key "$GB_KEY"
    h2h fact "$stamp_tmp" commit "$COMMIT"
    h2h fact "$stamp_tmp" zig_tarball_sha256 "$ZIG_SHA"
    h2h fact "$stamp_tmp" build_flags "${BUILD_FLAGS[*]}"
    h2h fact "$stamp_tmp" binary_sha256 "$(sha256 "$GB_BIN")"
    h2h fact "$stamp_tmp" built_utc "$(ts)"
    mv "$stamp_tmp" "$GB_STAMP"
fi
GB_SHA="$(sha256 "$GB_BIN")"
cp "$GB_STAMP" "$OUT/ghostty-bench-stamp.json"
log "ghostty-bench $GB_BIN sha256=$GB_SHA"

# ---------------------------------------------------------------------------
# 6. The width probe: ghostty-vt from the same checkout, same zig.

PROBE_FILES=(build.zig build.zig.zon.in src/main.zig)
PROBE_DIGEST="$(cd -- "$PROBE_SRC" && cat "${PROBE_FILES[@]}" | shasum -a 256 | awk '{print $1}')"
PROBE_DIR="$WORK/probe-${COMMIT:0:12}-zig$ZIG_VERSION-${PROBE_DIGEST:0:12}"
PROBE_BIN="$PROBE_DIR/out/bin/ghostty-h2h-probe"
PROBE_STAMP="$PROBE_DIR/stamp.json"
PROBE_KEY="$(printf '%s\n' "$COMMIT" "$ZIG_SHA" "$PROBE_DIGEST" | shasum -a 256 | awk '{print $1}')"
if stamp_matches "$PROBE_STAMP" "$PROBE_KEY" "$PROBE_BIN"; then
    log "reusing the width probe $PROBE_BIN"
else
    tree="$PROBE_DIR/tree-$STAMP_UTC-$$"
    mkdir -p "$tree/src"
    cp "$PROBE_SRC/build.zig" "$tree/build.zig"
    cp "$PROBE_SRC/src/main.zig" "$tree/src/main.zig"
    rel="$(python3 -I -c 'import os, sys; print(os.path.relpath(sys.argv[1], sys.argv[2]))' "$GHOSTTY_SRC" "$tree")"
    case "$rel" in *\"* | *\\* | *\|*) die 2 "cannot embed the checkout path $rel in build.zig.zon" ;; esac
    sed "s|@GHOSTTY_PATH@|$rel|" "$PROBE_SRC/build.zig.zon.in" >"$tree/build.zig.zon"
    zig_build ghostty-h2h-probe "$tree" "$PROBE_DIR/out" "$PROBE_DIR/cache" -Doptimize=ReleaseFast
    verify_checkout "after building the width probe (the build must only read it)"
    [[ -x $PROBE_BIN ]] || die 4 "the probe build did not produce $PROBE_BIN"
    stamp_tmp="$PROBE_STAMP.$$"
    h2h fact "$stamp_tmp" build_key "$PROBE_KEY"
    h2h fact "$stamp_tmp" probe_source_sha256 "$PROBE_DIGEST"
    h2h fact "$stamp_tmp" binary_sha256 "$(sha256 "$PROBE_BIN")"
    h2h fact "$stamp_tmp" built_utc "$(ts)"
    mv "$stamp_tmp" "$PROBE_STAMP"
fi
PROBE_SHA="$(sha256 "$PROBE_BIN")"
log "width probe $PROBE_BIN sha256=$PROBE_SHA"

fact ghostty.src "$GHOSTTY_SRC"
fact ghostty.commit "$COMMIT"
fact ghostty.describe "$(git -C "$GHOSTTY_SRC" describe --tags --always 2>/dev/null || echo unknown)"
fact ghostty.version "$(pin ghostty_version)"
fact ghostty.checkout_clean true --type bool
fact ghostty.build_flags "${BUILD_FLAGS[*]}"
fact binaries.ghostty_bench.path "$GB_BIN"
fact binaries.ghostty_bench.sha256 "$GB_SHA"
fact binaries.ghostty_probe.path "$PROBE_BIN"
fact binaries.ghostty_probe.sha256 "$PROBE_SHA"
fact binaries.ghostty_probe.source_sha256 "$PROBE_DIGEST"

if ((CHECK_PINS)); then
    log "PINS OK: Ghostty $COMMIT, zig $ZIG_VERSION ($ZIG_SHA), Ghostty.app $(pin app_short_version), ghostty-bench $GB_SHA"
    exit 0
fi

# ---------------------------------------------------------------------------
# 7. The FrankenTerm arm.

if ((BUILD_FT)); then
    FT_TARGET="$WORK/ft-target"
    ft_log="$WORK/ft-build-$STAMP_UTC.log"
    log "building ingest_throughput natively: cargo build -p frankenterm-term --profile $FT_PROFILE --example ingest_throughput (CARGO_TARGET_DIR=$FT_TARGET)"
    # RCH_CARGO_WRAPPER_BYPASS: the binary must run on this host, so the
    # build must not be offloaded to a remote worker.
    if ! (cd -- "$REPO_ROOT" && RCH_CARGO_WRAPPER_BYPASS=1 CARGO_TARGET_DIR="$FT_TARGET" \
        cargo build -p frankenterm-term --profile "$FT_PROFILE" --example ingest_throughput) >"$ft_log" 2>&1; then
        tail -40 "$ft_log" >&2
        die 4 "building ingest_throughput failed; full log: $ft_log"
    fi
    FT_BIN="$FT_TARGET/$FT_PROFILE/examples/ingest_throughput"
fi
[[ -n $FT_BIN ]] || die 2 "pass --ft-bin PATH or --build-ft"
[[ -x $FT_BIN ]] || die 2 "$FT_BIN is not an executable"
FT_BIN="$(realdir "$(dirname "$FT_BIN")")/$(basename "$FT_BIN")"
ft_examples_dir="$(dirname "$FT_BIN")"
ft_profile_dir="$(dirname "$ft_examples_dir")"
if [[ "$(basename "$ft_examples_dir")" != examples || "$(basename "$ft_profile_dir")" != "$FT_PROFILE" ]]; then
    die 4 "$FT_BIN must sit at <target>/$FT_PROFILE/examples/ingest_throughput; the bench reports its profile from that path"
fi
FT_SHA="$(sha256 "$FT_BIN")"
FT_HEAD="$(git -C "$REPO_ROOT" rev-parse HEAD)"
if [[ -n "$(git -C "$REPO_ROOT" --no-optional-locks status --porcelain --untracked-files=no)" ]]; then
    FT_DIRTY=true
else
    FT_DIRTY=false
fi
fact binaries.frankenterm.path "$FT_BIN"
fact binaries.frankenterm.sha256 "$FT_SHA"
fact binaries.frankenterm.profile "$FT_PROFILE"
fact frankenterm.git_head "$FT_HEAD"
fact frankenterm.tree_dirty "$FT_DIRTY" --type bool
fact frankenterm.lane term
fact frankenterm.scrollback "$FT_SCROLLBACK" --type int
fact frankenterm.chunk_bytes "$FT_CHUNK" --type int
log "FrankenTerm ingest_throughput $FT_BIN sha256=$FT_SHA profile=$FT_PROFILE head=$FT_HEAD dirty=$FT_DIRTY"

# ---------------------------------------------------------------------------
# 8. Inputs: identical bytes for both arms, hashed independently.

GEN_JSONL="$OUT/corpora.jsonl"
: >"$GEN_JSONL"
if [[ $CORPUS != none ]]; then
    gen_args=(--gen-only --corpus "$CORPUS" --size "$SIZE" --corpus-dir "$WORK/corpus")
    if [[ -n $SEED ]]; then gen_args+=(--seed "$SEED"); fi
    log "generating corpora: ingest_throughput ${gen_args[*]}"
    "$FT_BIN" "${gen_args[@]}" >"$GEN_JSONL" || die 4 "corpus generation failed"
fi
input_args=(--gen-jsonl "$GEN_JSONL" --primary "$PRIMARY" --out "$OUT/inputs.jsonl")
for file in ${CORPUS_FILES[@]+"${CORPUS_FILES[@]}"}; do
    [[ -f $file ]] || die 2 "--corpus-file $file does not exist"
    input_args+=(--file "$file")
done
h2h inputs "${input_args[@]}" >"$OUT/inputs.tsv" || die 4 "the inputs failed their identity checks (see above)"

IN_CORPUS=()
IN_PATH=()
IN_SHA=()
while IFS=$'\t' read -r corpus path sha; do
    case "$path" in *"'"*) die 2 "input path $path contains a single quote" ;; esac
    IN_CORPUS+=("$corpus")
    IN_PATH+=("$path")
    IN_SHA+=("$sha")
    log "input $corpus $path sha256=$sha"
done <"$OUT/inputs.tsv"
((${#IN_CORPUS[@]})) || die 2 "no inputs to measure"

h2h fingerprint --zig "$ZIG" >"$OUT/fingerprint.json"
fact fingerprint "$OUT/fingerprint.json" --type json
log "host: $(h2h get "$OUT/fingerprint.json" model) $(h2h get "$OUT/fingerprint.json" cpu), macOS $(h2h get "$OUT/fingerprint.json" os_version), load $(h2h loadavg)"

# ---------------------------------------------------------------------------
# 9. Emoji width parity: final grid geometry of both engines, untimed.

PARITY_ARGS=()
for index in "${!IN_CORPUS[@]}"; do
    [[ ${IN_CORPUS[$index]} == "$PRIMARY" ]] || continue
    for geometry in $GEOMETRY; do
        rows="${geometry%x*}"
        cols="${geometry#*x}"
        pdir="$OUT/parity/${PRIMARY}@$geometry"
        mkdir -p "$pdir"
        log "width parity $PRIMARY @ $geometry: ghostty-h2h-probe"
        "$PROBE_BIN" --data="${IN_PATH[$index]}" --terminal-rows="$rows" --terminal-cols="$cols" \
            >"$pdir/ghostty-probe.json" 2>"$pdir/ghostty-probe.stderr" || die 4 "the width probe failed; see $pdir"
        total_rows="$(h2h get "$pdir/ghostty-probe.json" total_rows)"
        # Room for every row FrankenTerm could produce if it wraps up to twice
        # as often as Ghostty; a saturated cap is reported as undecided.
        parity_scrollback=$((2 * total_rows + 4 * rows))
        log "width parity $PRIMARY @ $geometry: ingest_throughput --scrollback $parity_scrollback"
        "$FT_BIN" --lane term --from-file "${IN_PATH[$index]}" --rows "$rows" --cols "$cols" \
            --scrollback "$parity_scrollback" --chunk "$FT_CHUNK" \
            >"$pdir/ft-probe.jsonl" 2>"$pdir/ft-probe.stderr" || die 4 "the FrankenTerm width run failed; see $pdir"
        h2h parity --corpus "$PRIMARY" --corpus-file "${IN_PATH[$index]}" \
            --ghostty "$pdir/ghostty-probe.json" --ft "$pdir/ft-probe.jsonl" \
            --rows "$rows" --cols "$cols" --ft-scrollback "$parity_scrollback" \
            --out "$pdir/parity.json" >&2 || die 4 "comparing the width runs failed"
        PARITY_ARGS+=(--parity "$pdir/parity.json")
    done
done

# ---------------------------------------------------------------------------
# 10. Timed rows: hyperfine, one run per arm per round, ABBA.

sample_load() { printf '%s %s\n' "$(ts)" "$(h2h loadavg || echo unknown)"; }

ROW_ARGS=()
for index in "${!IN_CORPUS[@]}"; do
    corpus="${IN_CORPUS[$index]}"
    path="${IN_PATH[$index]}"
    for geometry in $GEOMETRY; do
        rows="${geometry%x*}"
        cols="${geometry#*x}"
        tag="${corpus//[^A-Za-z0-9_.-]/_}@$geometry"
        rdir="$OUT/rows/$tag"
        mkdir -p "$rdir"
        ghostty_cmd="'$GB_BIN' $(pin bench_action) --data='$path' --terminal-rows=$rows --terminal-cols=$cols"
        ft_cmd="'$FT_BIN' --lane term --from-file '$path' --rows $rows --cols $cols --scrollback $FT_SCROLLBACK --chunk $FT_CHUNK"
        ft_argv=("$FT_BIN" --lane term --from-file "$path" --rows "$rows" --cols "$cols"
            --scrollback "$FT_SCROLLBACK" --chunk "$FT_CHUNK")
        log "row $tag: untimed FrankenTerm admission run (sanity, profile, final state)"
        "${ft_argv[@]}" >"$rdir/ft-admission-pre.jsonl" 2>"$rdir/ft-admission-pre.stderr" ||
            die 4 "the FrankenTerm arm failed its admission run for $tag; see $rdir"
        : >"$rdir/load.txt"
        for ((round = 1; round <= ROUNDS; round++)); do
            nn="$(printf '%02d' "$round")"
            sample_load >>"$rdir/load.txt"
            hf=(hyperfine -N --runs 1 --style basic --export-json "$rdir/round-$nn.json")
            if ((round == 1 && WARMUP > 0)); then hf+=(--warmup "$WARMUP"); fi
            if ((round % 2 == 1)); then
                hf+=(-n ghostty -n frankenterm "$ghostty_cmd" "$ft_cmd")
                order="ghostty,frankenterm"
            else
                hf+=(-n frankenterm -n ghostty "$ft_cmd" "$ghostty_cmd")
                order="frankenterm,ghostty"
            fi
            log "row $tag round $nn/$ROUNDS order=$order"
            "${hf[@]}" >>"$rdir/hyperfine.log" 2>&1 ||
                die 4 "hyperfine failed in $tag round $nn (an arm exited non-zero); see $rdir/hyperfine.log"
        done
        sample_load >>"$rdir/load.txt"
        "${ft_argv[@]}" >"$rdir/ft-admission-post.jsonl" 2>"$rdir/ft-admission-post.stderr" ||
            die 4 "the FrankenTerm arm failed its post-round run for $tag; see $rdir"
        h2h row --dir "$rdir" --rel-dir "rows/$tag" --gates "$GATES" \
            --corpus "$corpus" --input-sha256 "${IN_SHA[$index]}" --rows "$rows" --cols "$cols" \
            --ft-scrollback "$FT_SCROLLBACK" --ft-chunk "$FT_CHUNK" --ft-profile "$FT_PROFILE" \
            --ghostty-cmd "$ghostty_cmd" --ft-cmd "$ft_cmd" >&2 || die 1 "summarizing $tag failed"
        ROW_ARGS+=(--row "$rdir/row.json")
    done
done

# ---------------------------------------------------------------------------
# 11. Receipt, validation, manifest.

fact finished_utc "$(ts)"
h2h receipt --mode "$MODE" --facts "$FACTS" --pins "$PINS" --gates "$GATES" \
    --inputs "$OUT/inputs.jsonl" "${ROW_ARGS[@]}" ${PARITY_ARGS[@]+"${PARITY_ARGS[@]}"} \
    --out "$OUT/receipt.json" || die 1 "assembling the receipt failed"
h2h validate "$OUT/receipt.json" >&2 || die 1 "the receipt failed schema validation"

verdict_status=0
h2h verdicts "$OUT/receipt.json" || verdict_status=$?

# The manifest covers every file in the receipt directory except itself and
# the live log, which tee is still writing.
(cd -- "$OUT" && find . -type f ! -name SHA256SUMS ! -name run.log | LC_ALL=C sort | sed 's|^\./||' |
    while IFS= read -r file; do shasum -a 256 "$file"; done) >"$OUT/SHA256SUMS"
log "receipt $OUT/receipt.json; manifest $OUT/SHA256SUMS (verify: cd $OUT && shasum -a 256 -c SHA256SUMS)"

if [[ $MODE == self-test ]]; then
    verdict="$(h2h get "$OUT/receipt.json" verdict)"
    case "$verdict" in
        "NO_ADMISSIBLE_RATIO (runs: "*) ;;
        *) die 1 "self-test: $ROUNDS rounds must be refused by the runs gate, got: $verdict" ;;
    esac
    parity_count="$(h2h get "$OUT/receipt.json" emoji_width_parity | python3 -I -c 'import json, sys; print(len(json.load(sys.stdin)))')"
    [[ $parity_count -ge 1 ]] || die 1 "self-test: the receipt holds no width-parity check"
    log "SELF-TEST OK: both arms ran, the receipt validates, the runs gate refused ($verdict)"
    exit 0
fi
exit "$verdict_status"
