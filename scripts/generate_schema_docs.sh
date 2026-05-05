#!/bin/bash
# Generate a deterministic Markdown index for committed JSON Schema contracts.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

SCHEMA_DIR="${SCHEMA_DIR:-$PROJECT_ROOT/docs/json-schema}"
OUT_PATH=""
DRY_RUN=false

usage() {
    cat <<'USAGE'
Usage: scripts/generate_schema_docs.sh [--dry-run] [--schema-dir DIR] [--out PATH]

Options:
  --dry-run        Print generated Markdown to stdout. Does not write files.
  --schema-dir DIR Read schemas from DIR (default: docs/json-schema).
  --out PATH       Write generated Markdown to PATH.
  -h, --help       Show this help.
USAGE
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --dry-run)
            DRY_RUN=true
            shift
            ;;
        --schema-dir)
            SCHEMA_DIR="$2"
            shift 2
            ;;
        --out)
            OUT_PATH="$2"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if ! command -v jq >/dev/null 2>&1; then
    echo "jq is required to generate schema docs" >&2
    exit 1
fi

if [[ ! -d "$SCHEMA_DIR" ]]; then
    echo "schema directory not found: $SCHEMA_DIR" >&2
    exit 1
fi

SCHEMAS=()
while IFS= read -r schema; do
    SCHEMAS+=("$schema")
done < <(find "$SCHEMA_DIR" -maxdepth 1 -type f -name '*.json' | sort)
if [[ ${#SCHEMAS[@]} -eq 0 ]]; then
    echo "no JSON schemas found in $SCHEMA_DIR" >&2
    exit 1
fi

generate_markdown() {
    local schema
    local schema_count="${#SCHEMAS[@]}"

    cat <<EOF
# JSON Schema Index

Generated from \`docs/json-schema/*.json\`.

Schema count: $schema_count

| Schema | Title | Type | Required | Properties | Description |
| --- | --- | --- | ---: | ---: | --- |
EOF

    for schema in "${SCHEMAS[@]}"; do
        jq -e . "$schema" >/dev/null
        local file
        file="$(basename "$schema")"
        jq -r --arg file "$file" '
            def markdown_cell:
                tostring
                | gsub("\\r?\\n"; " ")
                | gsub("\\|"; "\\|")
                | if length > 160 then .[0:157] + "..." else . end;
            [
                "`" + $file + "`",
                ((.title // "(untitled)") | markdown_cell),
                ((if (.type | type) == "array" then (.type | join(", ")) else (.type // "unspecified") end) | markdown_cell),
                (((.required // []) | length) | tostring),
                (((.properties // {}) | length) | tostring),
                ((.description // "") | markdown_cell)
            ] | "| " + join(" | ") + " |"
        ' "$schema"
    done
}

if [[ "$DRY_RUN" == "true" || -z "$OUT_PATH" ]]; then
    generate_markdown
else
    mkdir -p "$(dirname "$OUT_PATH")"
    generate_markdown > "$OUT_PATH"
    echo "wrote schema docs: $OUT_PATH"
fi
