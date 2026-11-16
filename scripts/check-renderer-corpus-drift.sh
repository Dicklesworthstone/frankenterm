#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/check-renderer-corpus-drift.sh [options]

Options:
  --corpus-root <path>  Corpus root (default: tests/fixtures/renderer-corpus)
  --base-ref <ref>      Git base ref for changed-file pairing checks
  --all                 Validate every committed corpus PNG and sidecar
  --changed             Check changed PNGs have changed sibling metadata
  --help                Show this help

With no mode flags, both --all and --changed are enabled. The changed-file
check is skipped when Git diff context is unavailable; full hash validation
still catches PNG byte changes with stale metadata.
EOF
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

CORPUS_ROOT="tests/fixtures/renderer-corpus"
BASE_REF="${RENDERER_CORPUS_BASE_REF:-}"
RUN_ALL=false
RUN_CHANGED=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --corpus-root)
            CORPUS_ROOT="${2:-}"
            shift 2
            ;;
        --base-ref)
            BASE_REF="${2:-}"
            shift 2
            ;;
        --all)
            RUN_ALL=true
            shift
            ;;
        --changed)
            RUN_CHANGED=true
            shift
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *)
            echo "[renderer-corpus-drift] unknown argument: $1" >&2
            usage >&2
            exit 64
            ;;
    esac
done

if [[ "$RUN_ALL" == false && "$RUN_CHANGED" == false ]]; then
    RUN_ALL=true
    RUN_CHANGED=true
fi

cd "$PROJECT_ROOT"

if ! command -v jq >/dev/null 2>&1; then
    echo "[renderer-corpus-drift] jq is required" >&2
    exit 69
fi

sha256_file() {
    local path="$1"
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$path" | awk '{print $1}'
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$path" | awk '{print $1}'
    else
        echo "[renderer-corpus-drift] shasum or sha256sum is required" >&2
        exit 69
    fi
}

error_count=0

record_error() {
    echo "[renderer-corpus-drift] ERROR: $*" >&2
    error_count=$((error_count + 1))
}

validate_frame() {
    local png_path="$1"
    local metadata_path="${png_path%.png}.json"
    local rel group scenario frame_file frame extra expected_hash actual_hash

    if [[ ! -f "$metadata_path" ]]; then
        record_error "missing sidecar for $png_path"
        return
    fi

    rel="${png_path#"$CORPUS_ROOT"/}"
    IFS='/' read -r group scenario frame_file extra <<< "$rel"
    frame="${frame_file%.png}"

    if [[ -z "${group:-}" || -z "${scenario:-}" || -z "${frame:-}" || -n "${extra:-}" || "$frame_file" == "$frame" ]]; then
        record_error "invalid corpus frame layout: $png_path"
        return
    fi

    if ! jq -e \
        --arg group "$group" \
        --arg scenario "$scenario" \
        --arg frame "$frame" \
        '
          .schema_version == "renderer-corpus-frame.v1"
          and .group == $group
          and .scenario == $scenario
          and .frame == $frame
          and (.viewport | type == "object")
          and (.viewport.width_px | type == "number")
          and (.viewport.height_px | type == "number")
          and (.viewport.scale_factor | type == "number")
          and (.monitors | type == "array" and length > 0)
          and has("cursor")
          and has("selection")
          and (.png_compression | type == "object")
          and (.png_compression.color_type == "rgba8")
          and (.png_compression.bit_depth == 8)
          and (.png_compression.interlace == "none")
          and (.content_hash | test("^sha256:[a-f0-9]{64}$"))
        ' "$metadata_path" >/dev/null; then
        record_error "invalid metadata contract: $metadata_path"
        return
    fi

    expected_hash="sha256:$(sha256_file "$png_path")"
    actual_hash="$(jq -r '.content_hash' "$metadata_path")"
    if [[ "$actual_hash" != "$expected_hash" ]]; then
        record_error "content_hash mismatch for $png_path (metadata $actual_hash, actual $expected_hash)"
    fi
}

resolve_base_ref() {
    if [[ -n "$BASE_REF" ]]; then
        if git rev-parse --verify --quiet "$BASE_REF" >/dev/null; then
            git rev-parse "$BASE_REF"
            return 0
        fi
        echo "[renderer-corpus-drift] base ref not found: $BASE_REF" >&2
        return 1
    fi

    if [[ "${GITHUB_EVENT_NAME:-}" == "pull_request" && -n "${GITHUB_BASE_REF:-}" ]]; then
        local pr_base="origin/${GITHUB_BASE_REF}"
        if git rev-parse --verify --quiet "$pr_base" >/dev/null; then
            git rev-parse "$pr_base"
            return 0
        fi
    fi

    if git rev-parse --verify --quiet "HEAD^" >/dev/null; then
        git rev-parse "HEAD^"
        return 0
    fi

    return 1
}

run_all_check() {
    if [[ ! -d "$CORPUS_ROOT" ]]; then
        record_error "corpus root does not exist: $CORPUS_ROOT"
        return
    fi

    local png_count=0
    while IFS= read -r png_path; do
        [[ -z "$png_path" ]] && continue
        png_count=$((png_count + 1))
        validate_frame "$png_path"
    done < <(find "$CORPUS_ROOT" -type f -name '*.png' | LC_ALL=C sort)

    while IFS= read -r metadata_path; do
        [[ -z "$metadata_path" ]] && continue
        if [[ ! -f "${metadata_path%.json}.png" ]]; then
            record_error "orphan metadata sidecar without sibling PNG: $metadata_path"
        fi
    done < <(find "$CORPUS_ROOT" -type f -name '*.json' | LC_ALL=C sort)

    echo "[renderer-corpus-drift] validated $png_count PNG frame(s)"
}

run_changed_check() {
    local base_ref
    if ! base_ref="$(resolve_base_ref)"; then
        echo "[renderer-corpus-drift] Git diff base unavailable; skipped changed-file pairing check"
        return
    fi

    local changed_files
    changed_files="$(git diff --name-only "$base_ref"...HEAD -- "$CORPUS_ROOT" || true)"
    if [[ -z "$changed_files" ]]; then
        echo "[renderer-corpus-drift] no changed corpus files"
        return
    fi

    local changed_lookup
    changed_lookup="$(printf '%s\n' "$changed_files")"

    while IFS= read -r changed_path; do
        [[ -z "$changed_path" ]] && continue
        case "$changed_path" in
            "$CORPUS_ROOT"/*.png|"$CORPUS_ROOT"/*/*.png|"$CORPUS_ROOT"/*/*/*.png)
                local sidecar="${changed_path%.png}.json"
                if ! grep -Fx -- "$sidecar" <<< "$changed_lookup" >/dev/null; then
                    record_error "changed PNG requires changed sidecar in same diff: $changed_path -> $sidecar"
                fi
                ;;
        esac
    done <<< "$changed_files"

    echo "[renderer-corpus-drift] checked changed corpus files against base $base_ref"
}

if [[ "$RUN_ALL" == true ]]; then
    run_all_check
fi

if [[ "$RUN_CHANGED" == true ]]; then
    run_changed_check
fi

if [[ "$error_count" -gt 0 ]]; then
    exit 1
fi

echo "[renderer-corpus-drift] renderer corpus contract holds"
