#!/usr/bin/env bash
# round4-bench-ab.sh — Round-4 Alien Optimization Gauntlet A/B bench driver.
#
# Compares ONE Criterion bench between a BASELINE arm and a CANDIDATE arm in
# the SAME run window, through RCH (remote-required, fail-closed), under the
# `release-perf` profile, then emits a KEEP/REJECT verdict card.
#
# This is the missing harness piece. The stats math already exists:
#   * crates/frankenterm-core/src/bench_stats.rs  — Distribution + Mann-Whitney
#     U + Empirical-Bernstein CI (Rust producer; emitted by bench_common.rs as
#     target/criterion/wa-bench-distributions.jsonl after every Criterion run).
#   * scripts/check_bench_stats.py — faithful Python port of the verdict tests;
#     this driver invokes it as the comparator (REUSED, not reinvented).
#
# What this driver adds (NEW):
#   * Per-arm RCH-remote bench execution under release-perf with per-arm
#     CARGO_TARGET_DIR isolation.
#   * Capture + snapshot of each arm's wa-bench-distributions.jsonl AND the raw
#     Criterion <group>/<bench_id>/new/sample.json into
#     target/round4-bench-ab/<bench>/<arm>/.
#   * Same-run-window provenance (git SHA, RCH worker host, ISO-8601 ts per arm)
#     with warnings if SHAs differ or timestamps drift > 120s.
#   * cv_pct per arm (stddev/mean from the Distribution row).
#   * Keep-gate decision card (machine JSON + human-readable, paste-ready into
#     docs/perf-ledger/round4-keep-ledger.md / round4-negative-results.md).
#
# RULES (AGENTS.md): no worktrees, no file deletion of pre-existing files,
# RCH-remote-required (never fall back to local cargo).
#
# ---------------------------------------------------------------------------
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
CHECK_STATS_PY="${SCRIPT_DIR}/check_bench_stats.py"

# ---------------------------------------------------------------------------
# Defaults / config
# ---------------------------------------------------------------------------
PACKAGE=""
BENCH=""
GROUP=""
ID=""
GATE=""
BASELINE_REF=""
CANDIDATE_REF=""
DRY_RUN=0
# Keep-gate thresholds (mirror round4-negative-results.md rules 8 + 10).
CV_MAX_PCT="${CV_MAX_PCT:-5.0}"
ALPHA="${ALPHA:-0.05}"
REGRESSION_PCT="${REGRESSION_PCT:-10.0}"
# Minimum mean speedup (%) to count as a KEEP-worthy win. Gauntlet quick-wins
# are often 2-8% (the SWAR moonshot was -2.5%), so the driver's own keep
# decision uses this small threshold rather than check_bench_stats.py's coarse
# symmetric --regression-threshold-pct status mapping.
IMPROVE_PCT="${IMPROVE_PCT:-2.0}"
# Same-run-window tolerance: warn if the two arms' bench timestamps drift more.
TS_DRIFT_WARN_SECS="${TS_DRIFT_WARN_SECS:-120}"
# release-perf wants frame pointers for attributable flamegraphs (keep-gate 9).
BENCH_RUSTFLAGS="${BENCH_RUSTFLAGS:--C force-frame-pointers=yes}"
OUT_ROOT="${REPO_ROOT}/target/round4-bench-ab"

RCH_PREFIX=(env RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 CARGO_NET_GIT_FETCH_WITH_CLI=true \
    rch --no-self-healing exec --)

usage() {
    cat <<'EOF'
round4-bench-ab.sh — same-run-window A/B bench driver for the round-4 gauntlet.

USAGE:
  scripts/round4-bench-ab.sh --package <crate> --bench <bench_name> \
      --group <criterion_group> --id <bench_id> --gate <spec> [options]

REQUIRED:
  --package <crate>     Cargo package owning the bench (e.g. frankenterm-term).
  --bench   <name>      Bench target name (cargo bench --bench <name>).
  --group   <group>     Criterion group string (c.benchmark_group("...")).
  --id      <bench_id>  Criterion bench id within the group.
  --gate    <spec>      One of:
      env:VAR=ON[/OFF]   ENV-GATED. Build the bench binary ONCE, then run it
                         twice back-to-back: VAR=OFF (baseline) then VAR=ON
                         (candidate). Same binary + same target dir = ideal
                         same-run-window A/B. OFF defaults to empty if omitted.
      feature:NAME       FEATURE-GATED. Build+run arm A without the feature
                         (baseline), then build+run arm B with --features NAME
                         (candidate). Two builds, two target dirs.
      default-active     The change is already default-on. Treated as an
                         env-disable gate unless --baseline-ref is given, in
                         which case a cross-commit git-ref A/B is performed.

OPTIONS:
  --baseline-ref <sha>  Cross-commit A/B: build/run baseline arm at <sha>.
  --candidate-ref <sha> Cross-commit A/B: build/run candidate arm at <sha>
                        (defaults to HEAD / working tree).
  --disable-var <VAR>   For `default-active` env-disable gate: the env var that
                        DISABLES the optimization (baseline sets VAR=1).
  --cv-max <pct>        Max coefficient-of-variation %% for a KEEP (default 5).
  --alpha <a>           Mann-Whitney significance level (default 0.05).
  --regression-pct <p>  check_bench_stats.py regression threshold (default 10).
  --improve-pct <p>     Min mean speedup %% to count as a KEEP win (default 2).
  --dry-run             Skip RCH/cargo entirely. Validate arg parsing and run
                        the comparator against synthetic distribution JSONLs so
                        the wiring can be smoke-tested with no remote workers.
  -h, --help            This help.

GATE CHOICE for byte_to_grid (the round-3 SWAR moonshot, ft-p8vls):
  The `bench-scalar-vte-scan` feature FORCES the scalar path. Default build =
  SWAR-optimized (candidate). So the A/B is feature-gated with the feature as
  the BASELINE arm. This driver's feature mode always builds the no-feature arm
  as baseline and the +feature arm as candidate; for byte_to_grid the win is
  proven by the inverse, so pass --gate feature:bench-scalar-vte-scan and read
  the card with that polarity in mind (or use env-gate benches for direct A/B).

OUTPUT:
  target/round4-bench-ab/<bench>/<arm>/  per-arm snapshots + meta.
  target/round4-bench-ab/<bench>/verdict.json  machine verdict.
  A paste-ready KEEP/REJECT card to stdout.

RCH is remote-required and fail-closed. If RCH cannot reach a remote worker the
driver exits non-zero with "RCH blocked — proof lane down" and NEVER runs a
local cargo build.
EOF
}

die() { echo "round4-bench-ab: ERROR: $*" >&2; exit 1; }
warn() { echo "round4-bench-ab: WARN: $*" >&2; }
info() { echo "round4-bench-ab: $*" >&2; }

iso_now() { date -u +%Y-%m-%dT%H:%M:%SZ; }

# ---------------------------------------------------------------------------
# Arg parsing
# ---------------------------------------------------------------------------
DISABLE_VAR=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --package) PACKAGE="${2:?--package needs a value}"; shift 2;;
        --bench)   BENCH="${2:?--bench needs a value}"; shift 2;;
        --group)   GROUP="${2:?--group needs a value}"; shift 2;;
        --id)      ID="${2:?--id needs a value}"; shift 2;;
        --gate)    GATE="${2:?--gate needs a value}"; shift 2;;
        --baseline-ref)  BASELINE_REF="${2:?}"; shift 2;;
        --candidate-ref) CANDIDATE_REF="${2:?}"; shift 2;;
        --disable-var)   DISABLE_VAR="${2:?}"; shift 2;;
        --cv-max)        CV_MAX_PCT="${2:?}"; shift 2;;
        --alpha)         ALPHA="${2:?}"; shift 2;;
        --regression-pct) REGRESSION_PCT="${2:?}"; shift 2;;
        --improve-pct) IMPROVE_PCT="${2:?}"; shift 2;;
        --dry-run) DRY_RUN=1; shift;;
        -h|--help) usage; exit 0;;
        *) die "unknown argument: $1 (try --help)";;
    esac
done

[[ -n "${PACKAGE}" ]] || die "--package is required"
[[ -n "${BENCH}"   ]] || die "--bench is required"
[[ -n "${GROUP}"   ]] || die "--group is required"
[[ -n "${ID}"      ]] || die "--id is required"
[[ -n "${GATE}"    ]] || die "--gate is required"
[[ -f "${CHECK_STATS_PY}" ]] || die "comparator missing: ${CHECK_STATS_PY}"
command -v python3 >/dev/null 2>&1 || die "python3 not found (needed for comparator + cv computation)"

# Parse gate spec into a mode.
GATE_MODE=""      # env | feature | default-active
GATE_VAR=""       # env var name (env / default-active disable)
GATE_ON=""        # env on value
GATE_OFF=""       # env off value
GATE_FEATURE=""   # feature name
case "${GATE}" in
    env:*)
        GATE_MODE="env"
        spec="${GATE#env:}"
        GATE_VAR="${spec%%=*}"
        rhs="${spec#*=}"
        [[ "${spec}" == *=* ]] || die "env gate needs VAR=ON[/OFF]: ${GATE}"
        GATE_ON="${rhs%%/*}"
        if [[ "${rhs}" == */* ]]; then GATE_OFF="${rhs#*/}"; else GATE_OFF=""; fi
        [[ -n "${GATE_VAR}" ]] || die "env gate needs a VAR name: ${GATE}"
        ;;
    feature:*)
        GATE_MODE="feature"
        GATE_FEATURE="${GATE#feature:}"
        [[ -n "${GATE_FEATURE}" ]] || die "feature gate needs a NAME: ${GATE}"
        ;;
    default-active)
        if [[ -n "${BASELINE_REF}" ]]; then
            GATE_MODE="gitref"
        else
            GATE_MODE="env"
            [[ -n "${DISABLE_VAR}" ]] || die "default-active without --baseline-ref needs --disable-var <VAR>"
            GATE_VAR="${DISABLE_VAR}"
            GATE_ON=""    # candidate: optimization active (disable var unset/empty)
            GATE_OFF="1"  # baseline: optimization disabled
        fi
        ;;
    *) die "unrecognized --gate spec: ${GATE} (env:VAR=on/off | feature:NAME | default-active)";;
esac
# Explicit cross-commit ref A/B overrides any gate mode when both refs imply it.
if [[ -n "${BASELINE_REF}" && "${GATE_MODE}" != "gitref" ]]; then
    GATE_MODE="gitref"
fi

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

# Path to a sample.json inside an arbitrary target dir for our group/id.
# Criterion sanitizes some characters but for the simple ids used in the
# gauntlet (alnum + _) the path is target/criterion/<group>/<id>/new/sample.json.
sample_rel_path() { echo "criterion/${GROUP}/${ID}/new/sample.json"; }
dist_rel_path()   { echo "criterion/wa-bench-distributions.jsonl"; }

git_head_sha() { git -C "${REPO_ROOT}" rev-parse --short=12 HEAD 2>/dev/null || echo "unknown"; }

tree_is_dirty() {
    [[ -n "$(git -C "${REPO_ROOT}" status --porcelain 2>/dev/null)" ]]
}

# Run one cargo invocation through RCH, fail closed if it never reached a
# remote worker. Captures combined output to $2 (a log file). $1 = arm label.
# Remaining args = the cargo command.
rch_cargo() {
    local arm="$1"; shift
    local logf="$1"; shift
    local target_dir="$1"; shift
    info "[${arm}] RCH-remote cargo: $* (CARGO_TARGET_DIR=${target_dir})"
    set +e
    "${RCH_PREFIX[@]}" env \
        CARGO_TARGET_DIR="${target_dir}" \
        RUSTFLAGS="${BENCH_RUSTFLAGS}" \
        "$@" >"${logf}" 2>&1
    local rc=$?
    set -e
    # Fail-closed RCH detection: any sign the work did NOT land on a remote
    # worker is a blocked proof lane, not a result.
    if grep -qiE '\[RCH\] local|running locally|no admissible workers|worker=null|local fallback|RCH_FORCE_LOCAL' "${logf}"; then
        echo "---- last 40 lines of ${logf} ----" >&2
        tail -40 "${logf}" >&2 || true
        die "RCH blocked — proof lane down (arm=${arm}); refusing to count local output as a result"
    fi
    if [[ ${rc} -ne 0 ]]; then
        echo "---- last 60 lines of ${logf} ----" >&2
        tail -60 "${logf}" >&2 || true
        die "cargo failed for arm=${arm} (rc=${rc})"
    fi
    return 0
}

# Extract the RCH worker host from a captured log (best-effort).
rch_worker_from_log() {
    local logf="$1"
    # Common RCH markers: "[RCH] remote vmiXXXX (Ns)" or "worker=<host>".
    grep -oE '\[RCH\] remote [^ ]+' "${logf}" 2>/dev/null | tail -1 | awk '{print $3}' && return 0 || true
    grep -oE 'worker=[^ ]+' "${logf}" 2>/dev/null | tail -1 | sed 's/worker=//' && return 0 || true
    echo "unknown"
}

# Pull the per-iteration sample.json OFF the remote worker into a local arm
# snapshot dir and DERIVE the canonical Distribution JSONL from it. RCH rewrites
# CARGO_TARGET_DIR to a worker-scoped path on the worker but syncs the criterion
# tree back to the local `target_dir` we requested, so we read from there.
#
# We do NOT rely on bench_common's wa-bench-distributions.jsonl: only benches
# wired through frankenterm-core's emit_bench_distributions write it. Criterion
# ALWAYS writes the raw sample.json (iters[] + times[]), so deriving the
# Distribution ourselves — with the SAME percentile/stddev conventions as
# bench_stats.rs / check_bench_stats.py — makes this driver work for any bench.
# If a bench_common row IS present we prefer it (byte-matches the Rust producer).
snapshot_arm() {
    local arm="$1"; local target_dir="$2"; local logf="$3"
    local arm_dir="${OUT_ROOT}/${BENCH}/${arm}"
    mkdir -p "${arm_dir}"
    # Clear stale snapshot files from any prior run so results can't be
    # contaminated (we only ever truncate/overwrite our OWN snapshot files;
    # AGENTS.md no-delete rule is about source/work files, not regenerated
    # per-run snapshots — but we use truncation, not rm, to be safe).
    : > "${arm_dir}/wa-bench-distributions.jsonl"
    local sample_src dist_src
    sample_src="${target_dir}/$(sample_rel_path)"
    dist_src="${target_dir}/$(dist_rel_path)"
    local copied_sample=0
    if [[ -f "${sample_src}" ]]; then
        cp -f "${sample_src}" "${arm_dir}/sample.json"; copied_sample=1
    else
        warn "[${arm}] sample.json not found at ${sample_src} (wrong group/id? criterion sanitization?)"
        : > "${arm_dir}/sample.json"
    fi
    [[ -f "${dist_src}" ]] || dist_src=""   # optional bench_common row
    # Record provenance + derive distribution for the same-run-window check.
    local worker; worker="$(rch_worker_from_log "${logf}")"
    python3 - "$arm_dir/meta.json" "$arm" "$(git_head_sha)" "$worker" "$copied_sample" \
        "${arm_dir}/sample.json" "${arm_dir}/wa-bench-distributions.jsonl" \
        "${dist_src}" "${GROUP}" "${ID}" "${BENCH}" "$(iso_now)" <<'PY'
import json, sys, pathlib, math, statistics

(meta_path, arm, sha, worker, copied_sample, sample_path, out_dist_path,
 bench_common_dist, group, bench_id, bench, ts) = sys.argv[1:13]

def percentile(sorted_s, q):
    if not sorted_s:
        return float("nan")
    if len(sorted_s) == 1:
        return sorted_s[0]
    pos = q * (len(sorted_s) - 1)
    lo, hi = math.floor(pos), math.ceil(pos)
    if lo == hi:
        return sorted_s[lo]
    frac = pos - lo
    return sorted_s[lo] * (1.0 - frac) + sorted_s[hi] * frac

dist = None
source = None

# 1) Prefer a bench_common-emitted row if one exists (exact Rust producer).
if bench_common_dist:
    p = pathlib.Path(bench_common_dist)
    if p.is_file():
        rows = []
        for line in p.read_text().splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                continue
            if obj.get("test_type") == "bench-distribution" \
               and obj.get("group") == group and obj.get("bench_id") == bench_id:
                rows.append(obj)
        if rows:
            rows.sort(key=lambda r: r.get("generated_at_ms", 0))
            dist = rows[-1]["distribution"]
            source = "bench_common"

# 2) Else derive from raw Criterion sample.json (iters[] + times[]).
if dist is None:
    sp = pathlib.Path(sample_path)
    if sp.is_file() and sp.stat().st_size > 0:
        try:
            raw = json.loads(sp.read_text())
        except json.JSONDecodeError:
            raw = {}
        iters = raw.get("iters") or []
        times = raw.get("times") or []
        per = [t / i for t, i in zip(times, iters) if i]  # per-iteration ns
        if per:
            per_sorted = sorted(per)
            n = len(per_sorted)
            mean = statistics.fmean(per_sorted)
            stddev = statistics.stdev(per_sorted) if n > 1 else 0.0  # Bessel n-1
            dist = {
                "sample_size": n,
                "mean": mean,
                "stddev": stddev,
                "min": per_sorted[0],
                "max": per_sorted[-1],
                "percentiles": [
                    {"q": 0.50, "value": percentile(per_sorted, 0.50)},
                    {"q": 0.95, "value": percentile(per_sorted, 0.95)},
                    {"q": 0.99, "value": percentile(per_sorted, 0.99)},
                    {"q": 0.999, "value": percentile(per_sorted, 0.999)},
                ],
            }
            source = "derived_from_sample_json"

mean = stddev = cv_pct = sample_size = None
if dist is not None:
    mean = dist.get("mean"); stddev = dist.get("stddev"); sample_size = dist.get("sample_size")
    if mean and mean > 0 and stddev is not None:
        cv_pct = stddev / mean * 100.0
    # Write the canonical wa-bench-distributions.jsonl row the comparator reads.
    row = {
        "test_type": "bench-distribution", "schema": "1", "bench": bench,
        "group": group, "bench_id": bench_id,
        "sample_source": sample_path, "generated_at_ms": 0,
        "distribution": dist,
    }
    pathlib.Path(out_dist_path).write_text(json.dumps(row) + "\n")

meta = {
    "arm": arm, "git_sha": sha, "rch_worker": worker, "captured_at": ts,
    "group": group, "bench_id": bench_id,
    "copied_sample": bool(int(copied_sample)),
    "distribution_source": source,
    "mean_ns": mean, "stddev_ns": stddev, "sample_size": sample_size,
    "cv_pct": cv_pct,
}
pathlib.Path(meta_path).write_text(json.dumps(meta, indent=2, sort_keys=True) + "\n")
print(json.dumps(meta))
PY
}

# ---------------------------------------------------------------------------
# DRY-RUN: validate parsing + comparator wiring against synthetic data.
# ---------------------------------------------------------------------------
if [[ "${DRY_RUN}" -eq 1 ]]; then
    info "DRY-RUN: validating arg parsing + comparator wiring (no RCH, no cargo)."
    info "  package=${PACKAGE} bench=${BENCH} group=${GROUP} id=${ID}"
    info "  gate_mode=${GATE_MODE} var='${GATE_VAR}' on='${GATE_ON}' off='${GATE_OFF}' feature='${GATE_FEATURE}'"
    base_dir="${OUT_ROOT}/${BENCH}/baseline"
    cand_dir="${OUT_ROOT}/${BENCH}/candidate"
    mkdir -p "${base_dir}" "${cand_dir}"
    # Fabricate a tiny synthetic A/B pair of Distribution JSONL rows: candidate
    # is ~7% faster than baseline (a clear "improved" so the comparator polarity
    # is exercised end-to-end).
    python3 - "${base_dir}" "${cand_dir}" "${GROUP}" "${ID}" "${BENCH}" <<'PY'
import json, pathlib, sys
base_dir, cand_dir, group, bench_id, bench = sys.argv[1:6]
def row(mean, stddev, gen_ms):
    return {
        "test_type": "bench-distribution", "schema": "1", "bench": bench,
        "group": group, "bench_id": bench_id,
        "sample_source": f"target/criterion/{group}/{bench_id}/new/sample.json",
        "generated_at_ms": gen_ms,
        "distribution": {
            "sample_size": 100, "mean": mean, "stddev": stddev,
            "min": mean - 2 * stddev, "max": mean + 2 * stddev,
            "percentiles": [
                {"q": 0.50, "value": mean},
                {"q": 0.95, "value": mean + 1.6 * stddev},
                {"q": 0.99, "value": mean + 2.3 * stddev},
                {"q": 0.999, "value": mean + 3.0 * stddev},
            ],
        },
    }
# baseline mean 1000ns; candidate 930ns (-7%); cv ~3% (≤5 → passes gate 8).
pathlib.Path(base_dir, "wa-bench-distributions.jsonl").write_text(
    json.dumps(row(1000.0, 30.0, 1)) + "\n")
pathlib.Path(cand_dir, "wa-bench-distributions.jsonl").write_text(
    json.dumps(row(930.0, 28.0, 2)) + "\n")
PY
    # Synthesize per-arm meta (cv computation path) so the verdict card renders.
    for arm in baseline candidate; do
        d="${OUT_ROOT}/${BENCH}/${arm}"
        python3 - "$d/meta.json" "$arm" "synthetic" "dry-run" 1 0 \
            "$d/wa-bench-distributions.jsonl" "${GROUP}" "${ID}" "$(iso_now)" <<'PY'
import json, sys, pathlib
(meta_path, arm, sha, worker, copied_dist, copied_sample,
 dist_path, group, bench_id, ts) = sys.argv[1:11]
mean = stddev = cv_pct = sample_size = None
for line in pathlib.Path(dist_path).read_text().splitlines():
    if not line.strip():
        continue
    obj = json.loads(line)
    if obj.get("group") == group and obj.get("bench_id") == bench_id:
        d = obj["distribution"]
        mean, stddev, sample_size = d["mean"], d["stddev"], d["sample_size"]
        cv_pct = stddev / mean * 100.0
meta = {"arm": arm, "git_sha": sha, "rch_worker": worker, "captured_at": ts,
        "group": group, "bench_id": bench_id,
        "copied_distributions": True, "copied_sample": False,
        "mean_ns": mean, "stddev_ns": stddev, "sample_size": sample_size,
        "cv_pct": cv_pct}
pathlib.Path(meta_path).write_text(json.dumps(meta, indent=2, sort_keys=True) + "\n")
PY
    done
    info "DRY-RUN: synthetic A/B pair written; running comparator + card."
fi

# ---------------------------------------------------------------------------
# LIVE: run the arms through RCH.
# ---------------------------------------------------------------------------
if [[ "${DRY_RUN}" -ne 1 ]]; then
    mkdir -p "${OUT_ROOT}/${BENCH}"
    BENCH_FILTER="${GROUP}/${ID}"
    if tree_is_dirty; then
        warn "working tree is DIRTY (a swarm may be editing src/). The git SHA recorded"
        warn "per arm reflects HEAD, not the exact dirty tree state. For ref A/B this is"
        warn "best-effort; prefer env/feature single-tree mode for a clean same-run window."
    fi

    case "${GATE_MODE}" in
        env)
            # Build ONCE, run twice (off then on) in the SAME target dir.
            target_dir="/tmp/ft-benchab-${BENCH}-env"
            info "ENV-GATED A/B on \$${GATE_VAR}: build once, run OFF='${GATE_OFF}' then ON='${GATE_ON}'."
            # Build the bench binary (no-run) once.
            build_log="${OUT_ROOT}/${BENCH}/build-env.log"
            rch_cargo "build" "${build_log}" "${target_dir}" \
                cargo bench --no-run -p "${PACKAGE}" --bench "${BENCH}" --profile release-perf
            # Baseline arm: VAR=OFF.
            base_log="${OUT_ROOT}/${BENCH}/baseline.log"
            "${RCH_PREFIX[@]}" env CARGO_TARGET_DIR="${target_dir}" RUSTFLAGS="${BENCH_RUSTFLAGS}" \
                "${GATE_VAR}=${GATE_OFF}" \
                cargo bench -p "${PACKAGE}" --bench "${BENCH}" --profile release-perf -- "${BENCH_FILTER}" \
                >"${base_log}" 2>&1 || { tail -60 "${base_log}" >&2; die "baseline bench run failed"; }
            grep -qiE '\[RCH\] local|no admissible workers|worker=null|running locally' "${base_log}" \
                && { tail -40 "${base_log}" >&2; die "RCH blocked — proof lane down (baseline)"; }
            snapshot_arm baseline "${target_dir}" "${base_log}" >/dev/null
            # Candidate arm: VAR=ON (same binary, same target dir, back-to-back).
            cand_log="${OUT_ROOT}/${BENCH}/candidate.log"
            "${RCH_PREFIX[@]}" env CARGO_TARGET_DIR="${target_dir}" RUSTFLAGS="${BENCH_RUSTFLAGS}" \
                "${GATE_VAR}=${GATE_ON}" \
                cargo bench -p "${PACKAGE}" --bench "${BENCH}" --profile release-perf -- "${BENCH_FILTER}" \
                >"${cand_log}" 2>&1 || { tail -60 "${cand_log}" >&2; die "candidate bench run failed"; }
            grep -qiE '\[RCH\] local|no admissible workers|worker=null|running locally' "${cand_log}" \
                && { tail -40 "${cand_log}" >&2; die "RCH blocked — proof lane down (candidate)"; }
            snapshot_arm candidate "${target_dir}" "${cand_log}" >/dev/null
            ;;
        feature)
            # Two builds, two target dirs. Baseline = no feature, candidate = +feature.
            info "FEATURE-GATED A/B on --features ${GATE_FEATURE}: baseline no-feature, candidate +feature."
            base_target="/tmp/ft-benchab-${BENCH}-baseline"
            cand_target="/tmp/ft-benchab-${BENCH}-candidate"
            base_log="${OUT_ROOT}/${BENCH}/baseline.log"
            cand_log="${OUT_ROOT}/${BENCH}/candidate.log"
            rch_cargo baseline "${base_log}" "${base_target}" \
                cargo bench -p "${PACKAGE}" --bench "${BENCH}" --profile release-perf -- "${BENCH_FILTER}"
            snapshot_arm baseline "${base_target}" "${base_log}" >/dev/null
            rch_cargo candidate "${cand_log}" "${cand_target}" \
                cargo bench -p "${PACKAGE}" --bench "${BENCH}" --features "${GATE_FEATURE}" \
                --profile release-perf -- "${BENCH_FILTER}"
            snapshot_arm candidate "${cand_target}" "${cand_log}" >/dev/null
            ;;
        gitref)
            # Cross-commit A/B is BEST-EFFORT (AGENTS.md forbids worktrees; src may
            # be dirty). We do NOT checkout (that would clobber sibling-agent WIP).
            die "git-ref A/B mode (--baseline-ref=${BASELINE_REF} --candidate-ref=${CANDIDATE_REF:-HEAD}) is documented but disabled: a checkout would clobber a possibly-dirty swarm working tree (AGENTS.md RULE 2: no worktrees, and no destructive checkout). Use env/feature single-tree mode instead."
            ;;
        *) die "internal: unhandled gate mode ${GATE_MODE}";;
    esac
fi

# ---------------------------------------------------------------------------
# Comparator: invoke check_bench_stats.py with candidate as current, baseline
# as baseline.
# ---------------------------------------------------------------------------
BASE_DIST="${OUT_ROOT}/${BENCH}/baseline/wa-bench-distributions.jsonl"
CAND_DIST="${OUT_ROOT}/${BENCH}/candidate/wa-bench-distributions.jsonl"
[[ -f "${CAND_DIST}" ]] || die "candidate distributions JSONL missing: ${CAND_DIST}"
[[ -f "${BASE_DIST}" ]] || die "baseline distributions JSONL missing: ${BASE_DIST}"

STATS_OUT="${OUT_ROOT}/${BENCH}/check_bench_stats.json"
info "running comparator: check_bench_stats.py (current=candidate, baseline=baseline)"
python3 "${CHECK_STATS_PY}" \
    --current "${CAND_DIST}" \
    --baseline "${BASE_DIST}" \
    --alpha "${ALPHA}" \
    --regression-threshold-pct "${REGRESSION_PCT}" \
    --mode advisory \
    --output "${STATS_OUT}" >/dev/null

# ---------------------------------------------------------------------------
# Build the verdict (KEEP/REJECT) + render card.
# ---------------------------------------------------------------------------
VERDICT_JSON="${OUT_ROOT}/${BENCH}/verdict.json"
python3 - \
    "${STATS_OUT}" \
    "${OUT_ROOT}/${BENCH}/baseline/meta.json" \
    "${OUT_ROOT}/${BENCH}/candidate/meta.json" \
    "${GROUP}" "${ID}" "${BENCH}" "${PACKAGE}" "${GATE}" \
    "${CV_MAX_PCT}" "${ALPHA}" "${TS_DRIFT_WARN_SECS}" \
    "${VERDICT_JSON}" "${DRY_RUN}" "${IMPROVE_PCT}" <<'PY'
import json, sys, pathlib, datetime

(stats_path, base_meta_path, cand_meta_path, group, bench_id, bench, package,
 gate, cv_max, alpha, ts_drift_warn, verdict_path, dry_run, improve_pct) = sys.argv[1:15]
cv_max = float(cv_max); alpha = float(alpha); ts_drift_warn = float(ts_drift_warn)
improve_pct = float(improve_pct)
dry_run = bool(int(dry_run))

stats = json.loads(pathlib.Path(stats_path).read_text())
base_meta = json.loads(pathlib.Path(base_meta_path).read_text())
cand_meta = json.loads(pathlib.Path(cand_meta_path).read_text())

# Find the per-bench verdict for our (group, bench_id).
v = None
for row in stats.get("verdicts", []):
    if row.get("group") == group and row.get("bench_id") == bench_id:
        v = row
        break
if v is None and stats.get("verdicts"):
    v = stats["verdicts"][0]
v = v or {}

status = v.get("status", "advisory_no_baseline")
delta_pct = v.get("delta_pct")          # (cur-base)/base; <0 = candidate faster
p_value_mw = v.get("p_value_mw")
ebci_upper = v.get("ebci_upper_current")

base_mean = base_meta.get("mean_ns")
cand_mean = cand_meta.get("mean_ns")
base_cv = base_meta.get("cv_pct")
cand_cv = cand_meta.get("cv_pct")

# Same-run-window provenance checks.
window_warnings = []
if base_meta.get("git_sha") != cand_meta.get("git_sha"):
    window_warnings.append(
        f"git SHA differs between arms (baseline={base_meta.get('git_sha')} "
        f"candidate={cand_meta.get('git_sha')})")
def parse_ts(s):
    try:
        return datetime.datetime.strptime(s, "%Y-%m-%dT%H:%M:%SZ")
    except (ValueError, TypeError):
        return None
bt, ct = parse_ts(base_meta.get("captured_at")), parse_ts(cand_meta.get("captured_at"))
ts_drift = None
if bt and ct:
    ts_drift = abs((ct - bt).total_seconds())
    if ts_drift > ts_drift_warn:
        window_warnings.append(
            f"arm timestamps drift {ts_drift:.0f}s > {ts_drift_warn:.0f}s "
            "(not a tight same-run window)")

# Keep-gate decision (driven by the raw stats, not the comparator's coarse
# symmetric status). KEEP iff ALL of:
#   * delta_pct <= -improve_pct          (candidate is meaningfully faster)
#   * p_value_mw < alpha                 (the shift is statistically significant)
#   * candidate cv_pct <= cv_max         (keep-gate rule 8: not noise)
#   * comparator did NOT flag a regression (defense in depth)
# Anything else → REJECT, with a precise reason.
reasons = list(v.get("reasons", []))
keep = True
if delta_pct is None:
    keep = False
    reasons.append("REJECT: no delta (missing baseline distribution)")
else:
    if delta_pct <= -improve_pct:
        reasons.append(f"speedup {delta_pct:+.2f}% ≤ -{improve_pct}% (meaningful win)")
    else:
        keep = False
        reasons.append(f"REJECT: speedup {delta_pct:+.2f}% does not reach -{improve_pct}% threshold")
    if p_value_mw is not None and p_value_mw < alpha:
        reasons.append(f"p_value_mw={p_value_mw:.4g} < α={alpha} (significant)")
    else:
        keep = False
        reasons.append(f"REJECT: p_value_mw={p_value_mw} not < α={alpha} (not significant)")
    if cand_cv is not None and cand_cv <= cv_max:
        reasons.append(f"candidate cv_pct={cand_cv:.2f} ≤ {cv_max} (gate rule 8 satisfied)")
    elif cand_cv is None:
        keep = False
        reasons.append(f"REJECT: candidate cv_pct unavailable (no distribution row) > {cv_max} (gate rule 8)")
    else:
        keep = False
        reasons.append(f"REJECT: candidate cv_pct={cand_cv:.2f} > {cv_max} (gate rule 8: noise)")
if status == "regressed":
    keep = False
    reasons.append("REJECT: comparator flagged REGRESSED vs baseline")
verdict = "KEEP" if keep else "REJECT"

out = {
    "schema": "round4-bench-ab.v1",
    "package": package, "bench": bench, "group": group, "bench_id": bench_id,
    "gate": gate,
    "dry_run": dry_run,
    "verdict": verdict,
    "comparator_status": status,
    "delta_pct": delta_pct,
    "p_value_mw": p_value_mw,
    "ebci_upper_current": ebci_upper,
    "baseline": {"mean_ns": base_mean, "cv_pct": base_cv,
                 "git_sha": base_meta.get("git_sha"),
                 "rch_worker": base_meta.get("rch_worker"),
                 "captured_at": base_meta.get("captured_at"),
                 "sample_size": base_meta.get("sample_size")},
    "candidate": {"mean_ns": cand_mean, "cv_pct": cand_cv,
                  "git_sha": cand_meta.get("git_sha"),
                  "rch_worker": cand_meta.get("rch_worker"),
                  "captured_at": cand_meta.get("captured_at"),
                  "sample_size": cand_meta.get("sample_size")},
    "same_run_window": {"ts_drift_secs": ts_drift, "warnings": window_warnings},
    "cv_max_pct": cv_max, "alpha": alpha, "improve_pct": improve_pct,
    "reasons": reasons,
}
pathlib.Path(verdict_path).write_text(json.dumps(out, indent=2, sort_keys=True) + "\n")

# ----- Human-readable, paste-ready card -----
def fmt(x, suff="", nd=1):
    if x is None:
        return "n/a"
    if isinstance(x, float):
        return f"{x:.{nd}f}{suff}"
    return f"{x}{suff}"

def ns(x):
    return "n/a" if x is None else f"{x:.1f}ns"

speedup = "n/a"
if base_mean and cand_mean and cand_mean > 0:
    speedup = f"{base_mean / cand_mean:.3f}x"

dpct = "n/a" if delta_pct is None else f"{delta_pct:+.2f}%"
window_line = "OK" if not window_warnings else " | ".join(window_warnings)

card = f"""
================================ ROUND-4 BENCH A/B CARD ================================
  {'[DRY-RUN — synthetic data, NOT a real proof]' if dry_run else ''}
  Bench:   {package} :: {bench} :: {group}/{bench_id}
  Gate:    {gate}
  Verdict: {verdict}   (comparator status: {status})

  Measurement (focused):  {ns(base_mean)} (before) -> {ns(cand_mean)} (after)  [{dpct}, {speedup}]
    candidate cv_pct = {fmt(cand_cv, '%', 2)}  (need <= {cv_max}%);  baseline cv_pct = {fmt(base_cv, '%', 2)}
    p_value_mw = {fmt(p_value_mw, '', 4) if p_value_mw is not None else 'n/a'}   ebci_upper = {ns(ebci_upper)}

  Same run window:
    baseline:  git={base_meta.get('git_sha')}  worker={base_meta.get('rch_worker')}  ts={base_meta.get('captured_at')}  n={base_meta.get('sample_size')}
    candidate: git={cand_meta.get('git_sha')}  worker={cand_meta.get('rch_worker')}  ts={cand_meta.get('captured_at')}  n={cand_meta.get('sample_size')}
    window check: {window_line}

  Reasons:
"""
for r in reasons:
    card += f"    - {r}\n"
card += f"""
  Paste into:  docs/perf-ledger/round4-{'keep-ledger' if verdict=='KEEP' else 'negative-results'}.md
  Artifacts:   target/round4-bench-ab/{bench}/  (verdict.json, per-arm sample.json + meta.json + logs)
=======================================================================================
"""
print(card)
PY

rc=$?
if [[ "${DRY_RUN}" -eq 1 ]]; then
    info "DRY-RUN complete. Live RCH smoke run is PENDING — re-run without --dry-run."
fi
exit "${rc}"
