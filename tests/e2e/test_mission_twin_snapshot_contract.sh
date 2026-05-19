#!/usr/bin/env bash
# Static JSONL verifier for the mission-twin snapshot contract fixtures.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

DOC="docs/robot-contracts/mission-twin-snapshot.md"
SCHEMA="docs/json-schema/ft-mission-twin-snapshot.json"
INVALID="fixtures/mission-twin/snapshot/invalid/fragments.v1.json"
VALID_DIR="fixtures/mission-twin/snapshot/valid"

fail() {
  printf '{"event":"mission_twin_snapshot.error","status":"fail","message":%s}\n' "$(ruby -rjson -e 'print JSON.generate(ARGV.fetch(0))' "$*")" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "missing command: $1"
}

require_file() {
  [[ -f "$1" ]] || fail "missing file: $1"
}

require_command jq
require_command ruby
require_file "${DOC}"
require_file "${SCHEMA}"
require_file "${INVALID}"

for fixture in "${VALID_DIR}"/*.json; do
  require_file "${fixture}"
done

jq empty "${SCHEMA}" "${INVALID}" "${VALID_DIR}"/*.json

ruby <<'RUBY'
require "json"
require "set"

DOC = "docs/robot-contracts/mission-twin-snapshot.md"
SCHEMA = "docs/json-schema/ft-mission-twin-snapshot.json"
INVALID = "fixtures/mission-twin/snapshot/invalid/fragments.v1.json"
VALID = {
  "healthy" => "fixtures/mission-twin/snapshot/valid/healthy.json",
  "agent-mail-red" => "fixtures/mission-twin/snapshot/valid/agent-mail-red.json",
  "rch-critical-pressure-5" => "fixtures/mission-twin/snapshot/valid/rch-critical-pressure-5.json",
  "active-owner" => "fixtures/mission-twin/snapshot/valid/active-owner.json",
  "dirty-overlap" => "fixtures/mission-twin/snapshot/valid/dirty-overlap.json",
  "no-ready-work" => "fixtures/mission-twin/snapshot/valid/no-ready-work.json"
}.freeze
EXPECTED_FORBIDDEN = %w[
  agent_mail_service_repair_restart
  rch_service_repair_restart
  worker_mutation
  build_cancellation
  file_deletion
  destructive_git
  local_cargo_proof
  pane_mutation
  raw_pane_content_storage
  beads_mutation
].freeze
EXPECTED_INVALID = %w[
  raw-pane-content-stored
  unsafe-artifact-path
  destructive-action-hint
  ambiguous-timestamp
  missing-forbidden-action
  unredacted-source
].freeze
SOURCE_KEYS = %w[
  beads
  rch
  agent_mail
  git
  reservations
  operating_envelope
].freeze

def emit(event, fields = {})
  puts JSON.generate({ "event" => event }.merge(fields))
end

def fail!(message, fields = {})
  warn JSON.generate({ "event" => "mission_twin_snapshot.error", "status" => "fail", "message" => message }.merge(fields))
  exit 1
end

def read_json(path)
  JSON.parse(File.read(path))
rescue JSON::ParserError => error
  fail!("#{path} does not parse as JSON", "error" => error.message)
end

def safe_repo_relative_path?(path)
  return false unless path.is_a?(String) && !path.empty?
  return false if path == "." || path == ".."
  return false if path.start_with?("/", "./", "../", "~")
  return false if path.end_with?("/")
  return false if path.include?("\\") || path.include?("://")

  segments = path.split("/", -1)
  return false if segments.any?(&:empty?)
  return false if segments.any? { |segment| segment == "." || segment == ".." || segment == ".git" }

  true
end

def require_safe_existing_artifact(path, owner)
  fail!("unsafe artifact path", "owner" => owner, "path" => path) unless safe_repo_relative_path?(path)
  fail!("artifact path does not resolve", "owner" => owner, "path" => path) unless File.file?(path)
end

def require_epoch_ms(value, owner, field)
  fail!("timestamp must be positive epoch ms", "owner" => owner, "field" => field, "value" => value) unless value.is_a?(Integer) && value.positive?
end

def reject_ambiguous_time_keys!(object, owner, path = [])
  case object
  when Hash
    object.each do |key, value|
      if key.end_with?("_at") || key == "generated_at" || key == "collected_at"
        fail!("ambiguous timestamp key present", "owner" => owner, "path" => (path + [key]).join("."))
      end
      reject_ambiguous_time_keys!(value, owner, path + [key])
    end
  when Array
    object.each_with_index { |value, index| reject_ambiguous_time_keys!(value, owner, path + [index.to_s]) }
  end
end

def validate_source!(snapshot_id, source_name, source)
  fail!("missing source", "snapshot_id" => snapshot_id, "source" => source_name) unless source.is_a?(Hash)
  fail!("source_id drifted", "snapshot_id" => snapshot_id, "source" => source_name) unless source["source_id"] == source_name
  fail!("source is not redacted", "snapshot_id" => snapshot_id, "source" => source_name) unless source["redacted"] == true
  fail!("source stores raw pane content", "snapshot_id" => snapshot_id, "source" => source_name) unless source["raw_pane_content_stored"] == false
  if source["status"] != "unavailable"
    require_epoch_ms(source["collected_at_ms"], snapshot_id, "#{source_name}.collected_at_ms")
  end
  source.fetch("artifact_paths").each { |path| require_safe_existing_artifact(path, "#{snapshot_id}.#{source_name}") }
end

def validate_valid_snapshot!(snapshot_id, path, snapshot)
  fail!("snapshot_id drifted", "expected" => snapshot_id, "actual" => snapshot["snapshot_id"]) unless snapshot["snapshot_id"] == snapshot_id
  fail!("schema version drifted", "snapshot_id" => snapshot_id) unless snapshot["schema_version"] == 1
  fail!("contract id drifted", "snapshot_id" => snapshot_id) unless snapshot["contract_id"] == "ft.mission_twin_snapshot.v1"
  fail!("source bead drifted", "snapshot_id" => snapshot_id) unless snapshot["source_bead"] == "ft-u7r37.1"
  require_epoch_ms(snapshot["generated_at_ms"], snapshot_id, "generated_at_ms")
  reject_ambiguous_time_keys!(snapshot, snapshot_id)
  fail!("envelope stores raw pane content", "snapshot_id" => snapshot_id) unless snapshot["raw_pane_content_stored"] == false
  fail!("forbidden action set drifted", "snapshot_id" => snapshot_id) unless snapshot.fetch("forbidden_actions").sort == EXPECTED_FORBIDDEN.sort
  fail!("forbidden action set has duplicates", "snapshot_id" => snapshot_id) unless snapshot.fetch("forbidden_actions").uniq.length == EXPECTED_FORBIDDEN.length

  snapshot.fetch("artifact_paths").each { |artifact| require_safe_existing_artifact(artifact, snapshot_id) }

  sources = snapshot.fetch("sources")
  fail!("source key coverage drifted", "snapshot_id" => snapshot_id) unless sources.keys.sort == SOURCE_KEYS.sort
  SOURCE_KEYS.each { |source_name| validate_source!(snapshot_id, source_name, sources.fetch(source_name)) }

  validation = snapshot.fetch("validation")
  fail!("valid fixture must be accepted", "snapshot_id" => snapshot_id) unless validation["validation_state"] == "accepted"
  fail!("destructive hints must be empty", "snapshot_id" => snapshot_id) unless validation.fetch("destructive_action_hints").empty?
  fail!("ambiguous timestamps must be rejected", "snapshot_id" => snapshot_id) unless validation["ambiguous_timestamps_rejected"] == true

  case snapshot_id
  when "agent-mail-red"
    agent_mail = sources.fetch("agent_mail")
    fail!("agent-mail-red must mark mail red", "snapshot_id" => snapshot_id) unless agent_mail["availability_state"] == "red"
    fail!("agent-mail-red must be unavailable", "snapshot_id" => snapshot_id) unless agent_mail["status"] == "unavailable"
    fail!("agent-mail-red must retain fallback reasons", "snapshot_id" => snapshot_id) if agent_mail.fetch("fallback_reason_codes").empty?
  when "rch-critical-pressure-5"
    rch = sources.fetch("rch")
    fail!("rch pressure fixture must have critical_pressure_count=5", "snapshot_id" => snapshot_id) unless rch["critical_pressure_count"] == 5
    fail!("rch pressure fixture must be not_ready", "snapshot_id" => snapshot_id) unless rch["admission_state"] == "not_ready"
  when "active-owner"
    owners = sources.fetch("beads").fetch("owner_states")
    fail!("active-owner fixture lacks active owner", "snapshot_id" => snapshot_id) unless owners.any? { |owner| owner["owner_state"] == "active" }
  when "dirty-overlap"
    git = sources.fetch("git")
    fail!("dirty-overlap fixture lacks dirty owned path", "snapshot_id" => snapshot_id) unless git.fetch("dirty_paths").any? { |entry| entry["overlaps_owned_path"] == true }
    fail!("dirty-overlap fixture lacks overlap_paths", "snapshot_id" => snapshot_id) if git.fetch("overlap_paths").empty?
  when "no-ready-work"
    beads = sources.fetch("beads")
    fail!("no-ready-work fixture must have zero ready beads", "snapshot_id" => snapshot_id) unless beads["ready_count"] == 0
    fail!("no-ready-work reason missing", "snapshot_id" => snapshot_id) unless beads.fetch("reason_codes").include?("mission_twin.no_ready_work")
  end

  emit("mission_twin_snapshot.valid", "status" => "ok", "snapshot_id" => snapshot_id, "path" => path)
end

schema = read_json(SCHEMA)
doc = File.read(DOC)
invalid = read_json(INVALID)

fail!("schema id drifted") unless schema["$id"]&.end_with?("/ft-mission-twin-snapshot.json")
fail!("schema contract const drifted") unless schema.dig("properties", "contract_id", "const") == "ft.mission_twin_snapshot.v1"
fail!("schema source bead const drifted") unless schema.dig("properties", "source_bead", "const") == "ft-u7r37.1"
fail!("schema raw pane const missing") unless schema.dig("properties", "raw_pane_content_stored", "const") == false
fail!("schema forbidden enum drifted") unless schema.dig("$defs", "forbidden_action", "enum").sort == EXPECTED_FORBIDDEN.sort
fail!("schema validation timestamp policy missing") unless schema.dig("$defs", "validation_summary", "properties", "ambiguous_timestamps_rejected", "const") == true
emit("mission_twin_snapshot.schema", "status" => "ok", "path" => SCHEMA)

%w[
  ft.mission_twin_snapshot.v1
  docs/json-schema/ft-mission-twin-snapshot.json
  crates/frankenterm-core/src/mission_twin_snapshot.rs
  fixtures/mission-twin/snapshot/valid/
  raw pane text
  destructive
].each do |needle|
  fail!("doc missing required contract text", "needle" => needle) unless doc.include?(needle)
end
emit("mission_twin_snapshot.doc", "status" => "ok", "path" => DOC)

VALID.each do |snapshot_id, path|
  validate_valid_snapshot!(snapshot_id, path, read_json(path))
end

fail!("invalid schema version drifted") unless invalid["schema_version"] == 1
fail!("invalid contract id drifted") unless invalid["contract_id"] == "ft.mission_twin_snapshot.invalid_fragments.v1"
fail!("invalid source bead drifted") unless invalid["source_bead"] == "ft-u7r37.1"
fail!("invalid fixture list drifted") unless invalid.fetch("valid_fixtures").sort == VALID.values.sort

invalid_cases = invalid.fetch("cases")
case_ids = invalid_cases.map { |entry| entry.fetch("case_id") }
fail!("invalid case coverage drifted", "cases" => case_ids.sort) unless case_ids.sort == EXPECTED_INVALID.sort
fail!("invalid case ids are not unique") unless case_ids.uniq.length == case_ids.length

by_id = invalid_cases.to_h { |entry| [entry.fetch("case_id"), entry] }
raw = by_id.fetch("raw-pane-content-stored").fetch("invalid_fragment")
fail!("raw pane case drifted") unless raw["raw_pane_content_stored"] == true && raw.dig("sources", "beads", "raw_pane_content_stored") == true

unsafe_paths = by_id.fetch("unsafe-artifact-path").dig("invalid_fragment", "artifact_paths")
fail!("unsafe path fixture lacks coverage") unless unsafe_paths.any? { |path| path.start_with?("/") } &&
  unsafe_paths.any? { |path| path.start_with?("../") } &&
  unsafe_paths.any? { |path| path.start_with?(".git/") } &&
  unsafe_paths.any? { |path| path.include?("://") } &&
  unsafe_paths.any? { |path| path.end_with?("/") }
unsafe_paths.each do |path|
  fail!("unsafe path accepted by predicate", "path" => path) if safe_repo_relative_path?(path)
end

destructive = by_id.fetch("destructive-action-hint").dig("invalid_fragment", "validation", "destructive_action_hints")
fail!("destructive hint case drifted") unless destructive == ["file_deletion"]

ambiguous = by_id.fetch("ambiguous-timestamp").fetch("invalid_fragment")
fail!("ambiguous timestamp fixture drifted") unless ambiguous["generated_at"].is_a?(String) &&
  ambiguous.dig("validation", "ambiguous_timestamps_rejected") == false &&
  ambiguous.dig("sources", "git", "collected_at").is_a?(String)

missing = by_id.fetch("missing-forbidden-action").fetch("invalid_fragment")
fail!("missing forbidden action case drifted") unless missing["omitted_forbidden_action"] == "local_cargo_proof" &&
  !missing.fetch("forbidden_actions").include?("local_cargo_proof")

unredacted = by_id.fetch("unredacted-source").dig("invalid_fragment", "sources", "git")
fail!("unredacted source case drifted") unless unredacted && unredacted["redacted"] == false

EXPECTED_INVALID.each do |case_id|
  emit("mission_twin_snapshot.invalid_fragment", "status" => "ok", "case_id" => case_id)
end

emit(
  "mission_twin_snapshot.summary",
  "status" => "ok",
  "valid_fixtures" => VALID.length,
  "invalid_cases" => EXPECTED_INVALID.length,
  "forbidden_actions" => EXPECTED_FORBIDDEN.length
)
RUBY
