#!/usr/bin/env bash
# Validate E2E artifact bundles against docs/test-logging-contract.md.

set -euo pipefail

usage() {
    cat >&2 <<'EOF'
Usage: scripts/validate_artifacts.sh <artifacts_dir>

Checks run-level manifest.json, scenario test_artifacts_manifest.json files,
listed artifact paths, sha256 values when present, JSON syntax, and obvious
unredacted secret patterns in text artifacts.
EOF
}

failures=0

error() {
    printf 'ERROR: %s\n' "$*" >&2
    failures=$((failures + 1))
}

require_tool() {
    if ! command -v "$1" >/dev/null 2>&1; then
        printf 'ERROR: required tool not found: %s\n' "$1" >&2
        exit 2
    fi
}

sha256_file() {
    local path="$1"
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$path" | awk '{print $1}'
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$path" | awk '{print $1}'
    else
        printf 'ERROR: neither shasum nor sha256sum is available\n' >&2
        exit 2
    fi
}

validate_json() {
    local path="$1"
    if [[ ! -f "$path" ]]; then
        error "Manifest schema validation failed: missing JSON file $path"
        return
    fi
    if ! jq empty "$path" >/dev/null 2>&1; then
        error "Manifest schema validation failed: invalid JSON in $path"
    fi
}

is_safe_relative_path() {
    local rel="$1"
    [[ -n "$rel" ]] || return 1
    [[ "$rel" != /* ]] || return 1
    [[ "$rel" != *".."* ]] || return 1
    return 0
}

validate_listed_path() {
    local base="$1"
    local rel="$2"
    local label="$3"
    local target

    if ! is_safe_relative_path "$rel"; then
        error "Required artifact has unsafe path for $label: $rel"
        return
    fi

    target="$base/$rel"
    if [[ ! -f "$target" && ! -d "$target" ]]; then
        error "Required artifact not found: $target"
    fi
}

validate_artifact_entry() {
    local base="$1"
    local rel="$2"
    local expected_sha="$3"
    local target
    local actual_sha

    validate_listed_path "$base" "$rel" "artifact.path"
    target="$base/$rel"

    if [[ -n "$expected_sha" && "$expected_sha" != "null" && -f "$target" ]]; then
        if [[ ! "$expected_sha" =~ ^[0-9a-fA-F]{64}$ ]]; then
            error "Manifest schema validation failed: invalid sha256 for $target"
            return
        fi
        actual_sha="$(sha256_file "$target")"
        if [[ "$actual_sha" != "$expected_sha" ]]; then
            error "Checksum mismatch: $target"
        fi
    fi
}

validate_test_artifacts_manifest() {
    local manifest="$1"
    local base
    local schema_version
    local artifact_count
    local outcome

    validate_json "$manifest"
    [[ -f "$manifest" ]] || return

    base="$(cd "$(dirname "$manifest")" && pwd)"
    schema_version="$(jq -r '.schema_version // empty' "$manifest")"
    if [[ "$schema_version" != "wa.test_artifacts.v1" ]]; then
        error "Manifest schema validation failed: unsupported test artifact schema in $manifest"
        return
    fi

    if ! jq -e '.run_id and .generated_at_ms and .outcome and .correlation.test_case_id' "$manifest" >/dev/null; then
        error "Manifest schema validation failed: missing required fields in $manifest"
    fi

    artifact_count="$(jq -r '(.artifacts // []) | length' "$manifest")"
    if [[ "$artifact_count" -eq 0 ]]; then
        error "Manifest schema validation failed: no artifacts listed in $manifest"
    fi

    while IFS=$'\t' read -r rel expected_sha; do
        [[ -n "$rel" ]] || continue
        validate_artifact_entry "$base" "$rel" "$expected_sha"
    done < <(jq -r '.artifacts[]? | [(.path // ""), (.sha256 // "")] | @tsv' "$manifest")

    outcome="$(jq -r '.outcome // empty' "$manifest")"
    if [[ "$outcome" != "passed" ]]; then
        for required_kind in trace_bundle frame_histogram failure_signature; do
            if ! jq -e --arg kind "$required_kind" '.artifacts[]? | select(.kind == $kind)' "$manifest" >/dev/null; then
                error "Required artifact missing: $required_kind in $manifest"
            fi
        done
    fi
}

validate_run_manifest() {
    local root="$1"
    local manifest="$root/manifest.json"
    local schema_version

    if [[ ! -f "$manifest" ]]; then
        error "Test artifacts missing manifest: $manifest"
        return
    fi

    validate_json "$manifest"
    [[ -f "$manifest" ]] || return

    schema_version="$(jq -r '.schema_version // .format // empty' "$manifest")"
    case "$schema_version" in
        wa.e2e.summary.v2)
            if ! jq -e '.scenarios and .results and .files' "$manifest" >/dev/null; then
                error "Manifest schema validation failed: missing E2E summary fields in $manifest"
            fi

            while IFS= read -r rel; do
                [[ -n "$rel" && "$rel" != "null" ]] || continue
                validate_listed_path "$root" "$rel" "manifest.files"
            done < <(jq -r '.files | objects | to_entries[] | .value | select(type == "string")' "$manifest")

            while IFS= read -r rel; do
                [[ -n "$rel" && "$rel" != "null" ]] || continue
                validate_listed_path "$root" "$rel" "scenario.artifacts_dir"
            done < <(jq -r '.scenarios[]? | .artifacts_dir // empty' "$manifest")

            while IFS= read -r rel; do
                [[ -n "$rel" && "$rel" != "null" ]] || continue
                validate_listed_path "$root" "$rel" "scenario.test_artifacts_manifest"
                if is_safe_relative_path "$rel" && [[ -f "$root/$rel" ]]; then
                    validate_test_artifacts_manifest "$root/$rel"
                fi
            done < <(jq -r '.scenarios[]? | .test_artifacts_manifest // empty' "$manifest")
            ;;
        ft-test-manifest)
            while IFS=$'\t' read -r rel expected_sha; do
                [[ -n "$rel" ]] || continue
                validate_artifact_entry "$root" "$rel" "$expected_sha"
            done < <(jq -r '.artifacts[]? | [(.path // ""), (.sha256 // "")] | @tsv' "$manifest")
            ;;
        wa.test_artifacts.v1)
            validate_test_artifacts_manifest "$manifest"
            ;;
        *)
            error "Manifest schema validation failed: unsupported manifest schema in $manifest"
            ;;
    esac
}

scan_for_secrets() {
    local root="$1"
    local found=0
    while IFS= read -r path; do
        if LC_ALL=C grep -Eq 'sk-(proj-)?[A-Za-z0-9_-]{20,}|AKIA[0-9A-Z]{16}|Authorization:[[:space:]]*Bearer[[:space:]]+[A-Za-z0-9._~+/-]{20,}=*|OPENAI_API_KEY[[:space:]]*=[[:space:]]*sk-[A-Za-z0-9_-]{20,}|password[[:space:]]*[=:][[:space:]]*[A-Za-z0-9_./+-]{12,}' "$path"; then
            error "Secret pattern found in $path"
            found=1
        fi
    done < <(find "$root" -type f \( \
        -name '*.json' -o -name '*.jsonl' -o -name '*.log' -o -name '*.txt' -o \
        -name '*.out' -o -name '*.err' -o -name '*.stderr' -o -name '*.stdout' \
    \) | LC_ALL=C sort)
    return "$found"
}

main() {
    if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
        usage
        exit 0
    fi
    if [[ $# -ne 1 ]]; then
        usage
        exit 2
    fi

    require_tool jq
    require_tool find
    require_tool grep

    local root="$1"
    if [[ ! -d "$root" ]]; then
        printf 'ERROR: artifact directory not found: %s\n' "$root" >&2
        exit 1
    fi
    root="$(cd "$root" && pwd)"

    validate_run_manifest "$root"
    scan_for_secrets "$root" || true

    if [[ "$failures" -ne 0 ]]; then
        printf 'Artifact validation failed: %s (%s issue(s))\n' "$root" "$failures" >&2
        exit 1
    fi

    printf 'Artifact validation passed: %s\n' "$root"
}

main "$@"
