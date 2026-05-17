#!/usr/bin/env bash
# Static verifier for docs/json-schema provenance coverage.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

SCHEMA_DIR="docs/json-schema"
PROVENANCE="${SCHEMA_DIR}/PROVENANCE.md"

fail() {
  printf 'json-schema provenance: %s\n' "$*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "missing command: $1"
}

require_file() {
  local path="$1"
  [[ -f "${path}" ]] || fail "missing file: ${path}"
}

require_command jq
require_command ruby
require_file "${PROVENANCE}"

mapfile -t schema_paths < <(find "${SCHEMA_DIR}" -maxdepth 1 -type f -name '*.json' | sort)
((${#schema_paths[@]} > 0)) || fail "no schema files found under ${SCHEMA_DIR}"

jq empty "${schema_paths[@]}"

ruby <<'RUBY'
require "json"

SCHEMA_DIR = "docs/json-schema"
PROVENANCE = "#{SCHEMA_DIR}/PROVENANCE.md"

def fail!(message)
  warn "json-schema provenance: #{message}"
  exit 1
end

schema_files = Dir[File.join(SCHEMA_DIR, "*.json")].map { |path| File.basename(path) }.sort
fail!("no schema files found under #{SCHEMA_DIR}") if schema_files.empty?

rows = []
File.readlines(PROVENANCE, chomp: true).each_with_index do |line, index|
  next unless line =~ /^\|\s*`([^`]+\.json)`\s*\|/

  columns = line.split("|", -1).map(&:strip)
  fail!("line #{index + 1} is not a 4-column provenance row") if columns.length < 6

  schema_from_cell = columns[1][/`([^`]+\.json)`/, 1]
  schema_from_match = Regexp.last_match(1)
  fail!("line #{index + 1} schema cell is malformed") if schema_from_cell.nil?
  fail!("line #{index + 1} schema cell mismatch") unless schema_from_cell == schema_from_match

  rows << {
    schema: schema_from_cell,
    line: index + 1,
    source: columns[2],
    command: columns[3],
    version: columns[4],
  }
end

fail!("no schema provenance rows found") if rows.empty?

row_names = rows.map { |row| row[:schema] }
counts = Hash.new(0)
row_names.each { |name| counts[name] += 1 }
duplicates = counts.select { |_name, count| count > 1 }.keys.sort
fail!("duplicate provenance rows: #{duplicates.join(", ")}") unless duplicates.empty?

missing_from_provenance = schema_files - row_names
stale_rows = row_names - schema_files
fail!("schemas missing provenance rows: #{missing_from_provenance.join(", ")}") unless missing_from_provenance.empty?
fail!("provenance rows without schema files: #{stale_rows.join(", ")}") unless stale_rows.empty?

rows.each do |row|
  fail!("#{row[:schema]} line #{row[:line]} has empty source of truth") if row[:source].empty? || row[:source] == "-"
  fail!("#{row[:schema]} line #{row[:line]} has empty generator command") if row[:command].empty? || row[:command] == "-"
  fail!("#{row[:schema]} line #{row[:line]} command must include verification text") unless row[:command].include?("verify with")
  fail!("#{row[:schema]} line #{row[:line]} has malformed version") unless row[:version].match?(/\A\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?\z/)

  path = File.join(SCHEMA_DIR, row[:schema])
  schema = JSON.parse(File.read(path))

  ["$schema", "$id", "title", "description"].each do |key|
    value = schema[key]
    fail!("#{row[:schema]} missing root #{key}") unless value.is_a?(String) && !value.empty?
  end

  fail!("#{row[:schema]} root $schema must use JSON Schema") unless schema["$schema"].include?("json-schema.org")
  fail!("#{row[:schema]} root $id must end with schema file name") unless schema["$id"].end_with?("/#{row[:schema]}")

  type = schema["type"]
  fail!("#{row[:schema]} root type must be object or array") unless ["object", "array"].include?(type)
end

puts "json-schema provenance: static verifier passed (#{schema_files.length} schemas)"
RUBY
