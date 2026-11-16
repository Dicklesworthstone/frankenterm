#!/usr/bin/env bash
# Filter cargo/clippy JSONL diagnostics down to the files owned by one bead.
#
# This helper does not run cargo. Feed it retained `--message-format=json`
# output and the original cargo exit status so the result can say both:
#   - whether the full clippy command failed
#   - whether the owned file slice had diagnostics
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  filter-clippy-owned-files.sh --cargo-status N --owned-file PATH [--owned-file PATH ...] [--input FILE] [--format json|text]

Options:
  --cargo-status N      Exit status from the full cargo clippy command.
  --owned-file PATH     Project-relative file path owned by this bead. Repeatable.
  --owned-files FILE    Newline-delimited file containing owned paths.
  --input FILE          Cargo JSONL input. Defaults to stdin.
  --format json|text    Output format. Defaults to json.
  --repo-root PATH      Repo root used to normalize absolute diagnostics.

The output is attribution evidence only. An owned-files-clean verdict is not a
workspace-green clippy claim when cargo_status is non-zero.
EOF
}

cargo_status=""
input_file="-"
output_format="json"
repo_root="${REPO_ROOT:-$(pwd -P)}"
owned_files=()

while [ "$#" -gt 0 ]; do
  case "$1" in
    --cargo-status)
      [ "$#" -ge 2 ] || { echo "--cargo-status requires a value" >&2; exit 64; }
      cargo_status="$2"
      shift 2
      ;;
    --owned-file)
      [ "$#" -ge 2 ] || { echo "--owned-file requires a value" >&2; exit 64; }
      owned_files+=("$2")
      shift 2
      ;;
    --owned-files)
      [ "$#" -ge 2 ] || { echo "--owned-files requires a value" >&2; exit 64; }
      owned_file_list="$2"
      [ -f "$owned_file_list" ] || { echo "owned file list not found: $owned_file_list" >&2; exit 66; }
      while IFS= read -r owned_file; do
        [ -n "$owned_file" ] || continue
        owned_files+=("$owned_file")
      done < "$owned_file_list"
      shift 2
      ;;
    --input)
      [ "$#" -ge 2 ] || { echo "--input requires a value" >&2; exit 64; }
      input_file="$2"
      shift 2
      ;;
    --format)
      [ "$#" -ge 2 ] || { echo "--format requires a value" >&2; exit 64; }
      output_format="$2"
      shift 2
      ;;
    --repo-root)
      [ "$#" -ge 2 ] || { echo "--repo-root requires a value" >&2; exit 64; }
      repo_root="$2"
      shift 2
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 64
      ;;
  esac
done

case "$cargo_status" in
  ""|*[!0-9]*)
    echo "--cargo-status must be a non-negative integer" >&2
    exit 64
    ;;
esac

if [ "${#owned_files[@]}" -eq 0 ]; then
  echo "at least one --owned-file or --owned-files entry is required" >&2
  exit 64
fi

case "$output_format" in
  json|text) ;;
  *)
    echo "--format must be json or text" >&2
    exit 64
    ;;
esac

if [ "$input_file" != "-" ] && [ ! -f "$input_file" ]; then
  echo "input file not found: $input_file" >&2
  exit 66
fi

repo_root="${repo_root%/}"

owned_json=$(
  printf '%s\n' "${owned_files[@]}" |
    jq -R --arg repo_root "$repo_root" '
      def normalize_path:
        gsub("\\\\"; "/")
        | sub("^\\./"; "")
        | if ($repo_root | length) > 0 and startswith($repo_root + "/") then
            .[($repo_root | length + 1):]
          else
            .
          end
        | sub("^\\./"; "");

      select(length > 0) | normalize_path
    ' |
    jq -s 'unique'
)

run_filter() {
  jq -Rs \
    --argjson cargo_status "$cargo_status" \
    --argjson owned "$owned_json" \
    --arg repo_root "$repo_root" \
    '
    def normalize_path:
      gsub("\\\\"; "/")
      | sub("^\\./"; "")
      | if ($repo_root | length) > 0 and startswith($repo_root + "/") then
          .[($repo_root | length + 1):]
        else
          .
        end
      | sub("^\\./"; "");

    def parse_jsonl:
      split("\n")
      | map(select(length > 0) | (fromjson? // empty));

    def child_spans($message):
      [($message.children // [])[]? | (.spans // [])[]?];

    def message_spans($message):
      (($message.spans // []) + child_spans($message));

    def diagnostic_files($message):
      [message_spans($message)[]? | .file_name | select(. != null) | normalize_path] | unique;

    def owned_hit($files):
      any($files[]; . as $file | any($owned[]; . == $file));

    def diagnostic:
      .message as $message
      | (diagnostic_files($message)) as $files
      | {
          level: ($message.level // "unknown"),
          message: ($message.message // ""),
          code: ($message.code.code // null),
          rendered: ($message.rendered // ""),
          files: $files
        };

    (parse_jsonl
      | map(select(.reason == "compiler-message") | diagnostic)
      | map(select(owned_hit(.files)))) as $owned_diagnostics
    | ($owned_diagnostics | map(select(.level == "error")) | length) as $owned_errors
    | ($owned_diagnostics | map(select(.level == "warning")) | length) as $owned_warnings
    | {
        cargo_status: $cargo_status,
        full_command_failed: ($cargo_status != 0),
        workspace_green: ($cargo_status == 0),
        owned_files: $owned,
        owned_diagnostic_count: ($owned_diagnostics | length),
        owned_error_count: $owned_errors,
        owned_warning_count: $owned_warnings,
        attribution_verdict: (
          if $owned_errors > 0 then "owned_errors"
          elif ($owned_diagnostics | length) > 0 then "owned_non_error_diagnostics"
          else "owned_files_clean"
          end
        ),
        proof_note: "Filtered clean is not workspace green; cargo_status preserves the full command result.",
        owned_diagnostics: $owned_diagnostics
      }'
}

if [ "$input_file" = "-" ]; then
  json_output=$(run_filter)
else
  json_output=$(run_filter < "$input_file")
fi

case "$output_format" in
  json)
    printf '%s\n' "$json_output"
    ;;
  text)
    printf '%s\n' "$json_output" | jq -r '
      [
        "cargo_status=\(.cargo_status)",
        "full_command_failed=\(.full_command_failed)",
        "workspace_green=\(.workspace_green)",
        "owned_diagnostic_count=\(.owned_diagnostic_count)",
        "owned_error_count=\(.owned_error_count)",
        "owned_warning_count=\(.owned_warning_count)",
        "attribution_verdict=\(.attribution_verdict)",
        "owned_files=\(.owned_files | join(","))",
        "proof_note=\(.proof_note)"
      ] | .[]'
    ;;
esac
