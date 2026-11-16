#!/usr/bin/env bash
# Select a pull-request Criterion subset from docs/perf/bench-coverage-matrix.json.
#
# The selector is a deterministic greedy set cover: each iteration chooses the
# bench with the largest count of newly covered required features per estimated
# second. The selected subset is written as JSON for CI and for audit artifacts.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_ROOT"

MATRIX_FILE="docs/perf/bench-coverage-matrix.json"
OUTPUT_FILE="target/ci-bench-subset.json"
BUDGET_SECONDS=""

usage() {
    cat <<'USAGE'
Usage: scripts/select-pr-bench-subset.sh [options]

Options:
  --matrix PATH          Coverage matrix JSON (default: docs/perf/bench-coverage-matrix.json)
  --output PATH          Output JSON path (default: target/ci-bench-subset.json)
  --budget-seconds N     Estimated runtime budget (default: matrix time_budget_seconds or 300)
  -h, --help             Show this help
USAGE
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --matrix)
            MATRIX_FILE="${2:?missing value for --matrix}"
            shift 2
            ;;
        --output)
            OUTPUT_FILE="${2:?missing value for --output}"
            shift 2
            ;;
        --budget-seconds)
            BUDGET_SECONDS="${2:?missing value for --budget-seconds}"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "[bench-subset] ERROR: unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if ! command -v jq >/dev/null 2>&1; then
    echo "[bench-subset] ERROR: jq is required" >&2
    exit 1
fi

if [[ ! -f "$MATRIX_FILE" ]]; then
    echo "[bench-subset] ERROR: coverage matrix not found: $MATRIX_FILE" >&2
    exit 1
fi

jq empty "$MATRIX_FILE"

if [[ -z "$BUDGET_SECONDS" ]]; then
    BUDGET_SECONDS="$(jq -r '.time_budget_seconds // 300' "$MATRIX_FILE")"
fi

if ! [[ "$BUDGET_SECONDS" =~ ^[0-9]+$ ]] || (( BUDGET_SECONDS <= 0 )); then
    echo "[bench-subset] ERROR: --budget-seconds must be a positive integer" >&2
    exit 1
fi

mapfile -t REQUIRED_FEATURES < <(
    jq -r '
      [
        (.required_categories // [] | map("proof_category:" + tostring))[],
        (.required_claims // [] | map("claim:" + .))[],
        (.required_contract_families // [] | map("contract_family:" + .))[]
      ] | unique | .[]
    ' "$MATRIX_FILE"
)

if (( ${#REQUIRED_FEATURES[@]} == 0 )); then
    echo "[bench-subset] ERROR: matrix has no required features" >&2
    exit 1
fi

mapfile -t BENCH_ROWS < <(jq -c '.benches[]' "$MATRIX_FILE")
if (( ${#BENCH_ROWS[@]} == 0 )); then
    echo "[bench-subset] ERROR: matrix has no benches" >&2
    exit 1
fi

declare -A REQUIRED=()
declare -A COVERED=()
declare -A SELECTED=()
declare -A SELECT_REASON=()
SELECTED_ORDER=()

for feature in "${REQUIRED_FEATURES[@]}"; do
    REQUIRED["$feature"]=1
done

features_for_row() {
    jq -r '
      [
        (.proof_categories // [] | map("proof_category:" + tostring))[],
        (.claims // [] | map("claim:" + .))[],
        (.contract_families // [] | map("contract_family:" + .))[]
      ] | unique | .[]
    ' <<<"$1"
}

all_required_covered() {
    local feature
    for feature in "${REQUIRED_FEATURES[@]}"; do
        if [[ -z "${COVERED[$feature]:-}" ]]; then
            return 1
        fi
    done
    return 0
}

SPENT_SECONDS=0

while ! all_required_covered; do
    best_idx=""
    best_gain=0
    best_cost=0
    best_score=-1
    best_name=""

    for idx in "${!BENCH_ROWS[@]}"; do
        if [[ -n "${SELECTED[$idx]:-}" ]]; then
            continue
        fi

        row="${BENCH_ROWS[$idx]}"
        cost="$(jq -r '.estimated_seconds // 1' <<<"$row")"
        if ! [[ "$cost" =~ ^[0-9]+$ ]] || (( cost <= 0 )); then
            echo "[bench-subset] ERROR: invalid estimated_seconds for row $idx" >&2
            exit 1
        fi
        if (( SPENT_SECONDS + cost > BUDGET_SECONDS )); then
            continue
        fi

        gain=0
        while IFS= read -r feature; do
            [[ -n "${REQUIRED[$feature]:-}" ]] || continue
            [[ -z "${COVERED[$feature]:-}" ]] || continue
            gain=$((gain + 1))
        done < <(features_for_row "$row")

        if (( gain == 0 )); then
            continue
        fi

        score=$((gain * 1000000 / cost))
        name="$(jq -r '.bench' <<<"$row")"
        if (( score > best_score )) ||
           { (( score == best_score )) && (( gain > best_gain )); } ||
           { (( score == best_score )) && (( gain == best_gain )) && (( cost < best_cost || best_cost == 0 )); } ||
           { (( score == best_score )) && (( gain == best_gain )) && (( cost == best_cost )) && [[ "$name" < "$best_name" ]]; }; then
            best_idx="$idx"
            best_gain="$gain"
            best_cost="$cost"
            best_score="$score"
            best_name="$name"
        fi
    done

    if [[ -z "$best_idx" ]]; then
        break
    fi

    SELECTED["$best_idx"]=1
    SELECT_REASON["$best_idx"]="greedy_gain_${best_gain}_cost_${best_cost}"
    SELECTED_ORDER+=("$best_idx")
    SPENT_SECONDS=$((SPENT_SECONDS + best_cost))

    while IFS= read -r feature; do
        [[ -n "${REQUIRED[$feature]:-}" ]] || continue
        COVERED["$feature"]=1
    done < <(features_for_row "${BENCH_ROWS[$best_idx]}")
done

missing_features=()
covered_features=()
for feature in "${REQUIRED_FEATURES[@]}"; do
    if [[ -n "${COVERED[$feature]:-}" ]]; then
        covered_features+=("$feature")
    else
        missing_features+=("$feature")
    fi
done

selected_json_lines=()
for idx in "${SELECTED_ORDER[@]}"; do
    row="${BENCH_ROWS[$idx]}"
    reason="${SELECT_REASON[$idx]}"
    enriched="$(
        jq -c --arg reason "$reason" '
          . + {
            selection_reason: $reason,
            features: ([
              (.proof_categories // [] | map("proof_category:" + tostring))[],
              (.claims // [] | map("claim:" + .))[],
              (.contract_families // [] | map("contract_family:" + .))[]
            ] | unique)
          }
        ' <<<"$row"
    )"
    selected_json_lines+=("$enriched")
done

json_array_from_json_lines() {
    if (( $# == 0 )); then
        echo "[]"
    else
        printf '%s\n' "$@" | jq -s '.'
    fi
}

json_array_from_strings() {
    if (( $# == 0 )); then
        echo "[]"
    else
        printf '%s\n' "$@" | jq -R . | jq -s 'sort'
    fi
}

selected_json="$(json_array_from_json_lines "${selected_json_lines[@]}")"
covered_json="$(json_array_from_strings "${covered_features[@]}")"
missing_json="$(json_array_from_strings "${missing_features[@]}")"
required_json="$(json_array_from_strings "${REQUIRED_FEATURES[@]}")"

mkdir -p "$(dirname "$OUTPUT_FILE")"
jq -n \
    --arg generated_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    --arg matrix "$MATRIX_FILE" \
    --argjson budget "$BUDGET_SECONDS" \
    --argjson spent "$SPENT_SECONDS" \
    --argjson benches "$selected_json" \
    --argjson required "$required_json" \
    --argjson covered "$covered_json" \
    --argjson missing "$missing_json" \
    '{
      schema_version: 1,
      generated_at: $generated_at,
      matrix: $matrix,
      budget_seconds: $budget,
      spent_estimated_seconds: $spent,
      selected_count: ($benches | length),
      coverage: {
        required_count: ($required | length),
        covered_count: ($covered | length),
        missing_count: ($missing | length),
        required: $required,
        covered: $covered,
        missing: $missing
      },
      benches: $benches,
      cargo_commands: ($benches | map(
        if .package == "frankenterm-core" then
          "cargo bench -p frankenterm-core --bench " + .bench + " -- --noplot"
        else
          "cargo bench -p " + .package + " --bench " + .bench + " -- --noplot"
        end
      ))
    }' > "$OUTPUT_FILE"

echo "[bench-subset] wrote $OUTPUT_FILE"
echo "[bench-subset] selected=${#SELECTED_ORDER[@]} spent=${SPENT_SECONDS}s budget=${BUDGET_SECONDS}s covered=${#covered_features[@]}/${#REQUIRED_FEATURES[@]}"

if (( ${#missing_features[@]} > 0 )); then
    printf '[bench-subset] missing feature: %s\n' "${missing_features[@]}" >&2
    exit 1
fi
