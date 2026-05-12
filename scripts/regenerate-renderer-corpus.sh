#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/regenerate-renderer-corpus.sh [options]

Options:
  --seed <path>      YAML seed file (default: tests/fixtures/renderer-corpus/seed.yaml)
  --output <path>    Corpus output root (default: seed output_root, then tests/fixtures/renderer-corpus)
  --check            Verify generated files match the seed without writing
  --dry-run          Print planned writes without writing
  --force            Overwrite changed existing corpus files
  --help             Show this help
EOF
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

SEED="tests/fixtures/renderer-corpus/seed.yaml"
OUTPUT=""
CHECK=false
DRY_RUN=false
FORCE=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --seed)
            SEED="${2:-}"
            shift 2
            ;;
        --output)
            OUTPUT="${2:-}"
            shift 2
            ;;
        --check)
            CHECK=true
            shift
            ;;
        --dry-run)
            DRY_RUN=true
            shift
            ;;
        --force)
            FORCE=true
            shift
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *)
            echo "[renderer-corpus] unknown argument: $1" >&2
            usage >&2
            exit 64
            ;;
    esac
done

if [[ -z "$SEED" ]]; then
    echo "[renderer-corpus] --seed must not be empty" >&2
    exit 64
fi

if ! command -v ruby >/dev/null 2>&1; then
    echo "[renderer-corpus] ruby is required to parse the YAML seed" >&2
    exit 69
fi

cd "$PROJECT_ROOT"

ruby - "$PROJECT_ROOT" "$SEED" "$OUTPUT" "$CHECK" "$DRY_RUN" "$FORCE" <<'RUBY'
require "digest"
require "fileutils"
require "json"
require "pathname"
require "yaml"

root = Pathname.new(ARGV.fetch(0)).realpath
seed_arg = ARGV.fetch(1)
output_arg = ARGV.fetch(2)
check = ARGV.fetch(3) == "true"
dry_run = ARGV.fetch(4) == "true"
force = ARGV.fetch(5) == "true"

def abort_with(message, code = 64)
  warn "[renderer-corpus] #{message}"
  exit code
end

def absolutize(root, path)
  candidate = Pathname.new(path.to_s)
  candidate = root.join(candidate) unless candidate.absolute?
  candidate.cleanpath
end

def relative_to_root(root, path)
  Pathname.new(path).relative_path_from(root).to_s
rescue ArgumentError
  path.to_s
end

def safe_id!(kind, value)
  unless value.is_a?(String) && value.match?(/\A[a-z0-9][a-z0-9-]*\z/)
    abort_with("#{kind} must be lowercase kebab-case, got #{value.inspect}")
  end
  value
end

def require_hash!(hash, key, context)
  value = hash[key]
  abort_with("#{context} missing #{key}") if value.nil?
  value
end

def normalize_json(value)
  JSON.pretty_generate(value) + "\n"
end

seed_path = absolutize(root, seed_arg)
abort_with("seed file does not exist: #{seed_path}", 66) unless seed_path.file?

seed = YAML.safe_load(seed_path.read, permitted_classes: [], aliases: false)
abort_with("seed must be a YAML mapping") unless seed.is_a?(Hash)

schema_version = seed["schema_version"]
abort_with("unsupported seed schema_version: #{schema_version.inspect}") unless schema_version == "renderer-corpus-seed.v1"

output_root_arg = output_arg.empty? ? (seed["output_root"] || "tests/fixtures/renderer-corpus") : output_arg
output_root = absolutize(root, output_root_arg)
unless output_root.to_s == root.to_s || output_root.to_s.start_with?(root.to_s + File::SEPARATOR)
  abort_with("output root must be inside the project: #{output_root}")
end

compression = require_hash!(seed, "png_compression", "seed")
abort_with("png_compression must be a mapping") unless compression.is_a?(Hash)

groups = require_hash!(seed, "groups", "seed")
abort_with("groups must be a non-empty array") unless groups.is_a?(Array) && !groups.empty?

planned = []
errors = []

groups.each do |group|
  abort_with("group entry must be a mapping") unless group.is_a?(Hash)
  group_id = safe_id!("group id", require_hash!(group, "id", "group"))
  scenarios = require_hash!(group, "scenarios", "group #{group_id}")
  abort_with("group #{group_id} scenarios must be a non-empty array") unless scenarios.is_a?(Array) && !scenarios.empty?

  scenarios.each do |scenario|
    abort_with("scenario entry must be a mapping") unless scenario.is_a?(Hash)
    scenario_id = safe_id!("scenario id", require_hash!(scenario, "id", "scenario"))
    revision = require_hash!(scenario, "revision", "scenario #{group_id}/#{scenario_id}")
    viewport = require_hash!(scenario, "viewport", "scenario #{group_id}/#{scenario_id}")
    monitors = require_hash!(scenario, "monitors", "scenario #{group_id}/#{scenario_id}")
    frames = require_hash!(scenario, "frames", "scenario #{group_id}/#{scenario_id}")

    abort_with("viewport must be a mapping for #{group_id}/#{scenario_id}") unless viewport.is_a?(Hash)
    abort_with("monitors must be a non-empty array for #{group_id}/#{scenario_id}") unless monitors.is_a?(Array) && !monitors.empty?
    abort_with("frames must be a non-empty array for #{group_id}/#{scenario_id}") unless frames.is_a?(Array) && !frames.empty?

    frames.each do |frame|
      abort_with("frame entry must be a mapping") unless frame.is_a?(Hash)
      frame_id = safe_id!("frame id", require_hash!(frame, "id", "frame"))
      source_rel = require_hash!(frame, "source_png", "frame #{group_id}/#{scenario_id}/#{frame_id}")
      source_path = absolutize(root, source_rel)
      abort_with("source PNG does not exist: #{source_path}", 66) unless source_path.file?

      png_hash = Digest::SHA256.file(source_path).hexdigest
      destination_dir = output_root.join(group_id, scenario_id)
      png_path = destination_dir.join("#{frame_id}.png")
      metadata_path = destination_dir.join("#{frame_id}.json")

      metadata = {
        "schema_version" => "renderer-corpus-frame.v1",
        "group" => group_id,
        "scenario" => scenario_id,
        "frame" => frame_id,
        "source_png" => relative_to_root(root, source_path),
        "viewport" => viewport,
        "monitors" => monitors,
        "cursor" => frame.key?("cursor") ? frame["cursor"] : nil,
        "selection" => frame.key?("selection") ? frame["selection"] : nil,
        "png_compression" => compression,
        "content_hash" => "sha256:#{png_hash}",
        "seed" => {
          "path" => relative_to_root(root, seed_path),
          "scenario_revision" => revision,
        },
      }
      metadata_json = normalize_json(metadata)

      planned << relative_to_root(root, png_path)
      planned << relative_to_root(root, metadata_path)

      if check
        if !png_path.file?
          errors << "missing generated PNG: #{relative_to_root(root, png_path)}"
        elsif Digest::SHA256.file(png_path).hexdigest != png_hash
          errors << "PNG drift: #{relative_to_root(root, png_path)}"
        end

        if !metadata_path.file?
          errors << "missing metadata sidecar: #{relative_to_root(root, metadata_path)}"
        elsif metadata_path.read != metadata_json
          errors << "metadata drift: #{relative_to_root(root, metadata_path)}"
        end
        next
      end

      if dry_run
        puts "[renderer-corpus] would write #{relative_to_root(root, png_path)}"
        puts "[renderer-corpus] would write #{relative_to_root(root, metadata_path)}"
        next
      end

      FileUtils.mkdir_p(destination_dir)

      if png_path.file? && Digest::SHA256.file(png_path).hexdigest != png_hash && !force
        errors << "refusing to overwrite changed PNG without --force: #{relative_to_root(root, png_path)}"
      else
        FileUtils.cp(source_path, png_path)
      end

      if metadata_path.file? && metadata_path.read != metadata_json && !force
        errors << "refusing to overwrite changed metadata without --force: #{relative_to_root(root, metadata_path)}"
      else
        metadata_path.write(metadata_json)
      end
    end
  end
end

if errors.any?
  errors.each { |error| warn "[renderer-corpus] ERROR: #{error}" }
  exit 1
end

if check
  puts "[renderer-corpus] seed matches #{planned.length / 2} frame(s)"
elsif !dry_run
  puts "[renderer-corpus] generated #{planned.length / 2} frame(s)"
end
RUBY
