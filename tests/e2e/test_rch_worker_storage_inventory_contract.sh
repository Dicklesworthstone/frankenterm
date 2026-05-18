#!/usr/bin/env bash
# Static verifier for retained RCH worker storage inventory schema and fixtures.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

SCHEMA="docs/json-schema/ft-rch-worker-storage-inventory.json"
MANIFEST="fixtures/rch-worker-pressure/manifest.json"
PROVENANCE="docs/json-schema/PROVENANCE.md"
README="README.md"

fail() {
  printf 'rch worker storage inventory contract: %s\n' "$*" >&2
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
require_file "${SCHEMA}"
require_file "${MANIFEST}"
require_file "${PROVENANCE}"
require_file "${README}"

jq empty "${SCHEMA}" "${MANIFEST}" fixtures/rch-worker-pressure/valid/*.json

ruby <<'RUBY'
require "json"
require "set"

SCHEMA = "docs/json-schema/ft-rch-worker-storage-inventory.json"
MANIFEST = "fixtures/rch-worker-pressure/manifest.json"
PROVENANCE = "docs/json-schema/PROVENANCE.md"
README = "README.md"
EXPECTED_FIXTURE_IDS = %w[
  healthy-complete
  partial-timeout
  telemetry-gap
].freeze
EXPECTED_FORBIDDEN = %w[
  run_local_cargo_as_proof
  restart_agent_mail
  repair_agent_mail_db
  restart_rch_daemon
  mutate_rch_worker
  cancel_other_agent_build
  delete_files_without_approval
  treat_dry_run_as_compile_proof
  treat_inventory_as_cleanup_authorization
].freeze
EXPECTED_SIDE_EFFECT_FLAGS = %w[
  agent_mail_repaired
  agent_mail_restarted
  build_cancelled
  files_deleted
  local_cargo_counted_as_proof
  rch_daemon_restarted
  read_only
  worker_mutated
].freeze
FORBIDDEN_COMMAND_FRAGMENTS = [
  "am service restart",
  "am service stop",
  "am doctor fix",
  "am doctor repair",
  "am doctor reconstruct",
  "kill am",
  "kill mcp-agent-mail",
  "kill rch",
  "rch daemon restart",
  "rch daemon stop",
  "rch workers disable",
  "rch workers enable",
  "rch workers clean",
  "cancel build",
  "git reset --hard",
  "git clean -fd",
  "rm -rf"
].freeze

def fail!(message)
  warn "rch worker storage inventory contract: #{message}"
  exit 1
end

def read_json(path)
  JSON.parse(File.read(path))
rescue JSON::ParserError => error
  fail!("#{path} does not parse as JSON: #{error.message}")
end

def scan_records(payload)
  payload.fetch("worker_inventories").flat_map do |worker|
    worker.fetch("df_samples") + worker.fetch("shallow_scans") + worker.fetch("project_du_samples")
  end
end

schema = read_json(SCHEMA)
manifest = read_json(MANIFEST)
provenance = File.read(PROVENANCE)
readme = File.read(README)

fail!("schema id drifted") unless schema["$id"]&.end_with?("/ft-rch-worker-storage-inventory.json")
fail!("contract id const missing") unless schema.dig("properties", "contract_id", "const") == "ft.rch_worker_storage_inventory.v1"
fail!("read_only const missing") unless schema.dig("properties", "read_only", "const") == true
fail!("schema source bead pattern missing") unless schema.dig("properties", "source_bead", "pattern")

schema_forbidden = schema.dig("$defs", "forbidden_action", "enum")
fail!("schema forbidden-action enum drifted") unless schema_forbidden.sort == EXPECTED_FORBIDDEN.sort
%w[
  complete
  partial
  telemetry_gap
  blocked
].each do |status|
  fail!("schema missing inventory status #{status}") unless schema.dig("$defs", "inventory_status", "enum").include?(status)
end
%w[
  df_samples
  shallow_scans
  project_du_samples
  artifact_paths
].each do |field|
  fail!("schema worker inventory missing #{field}") unless schema.dig("$defs", "worker_inventory", "required").include?(field)
end

fail!("manifest schema_version drifted") unless manifest["schema_version"] == 1
fail!("manifest contract id drifted") unless manifest["contract_id"] == "ft.rch_worker_storage_inventory.fixture_manifest.v1"
fail!("manifest bead drifted") unless manifest["bead"] == "ft-5xwsu.1"
fail!("manifest schema pointer drifted") unless manifest["schema"] == SCHEMA
fail!("manifest verifier missing") unless manifest.fetch("verification").include?("bash tests/e2e/test_rch_worker_storage_inventory_contract.sh")

fixture_paths = manifest.fetch("valid")
fail!("fixture path count drifted") unless fixture_paths.length == EXPECTED_FIXTURE_IDS.length
fixture_paths.each { |path| fail!("manifest references missing fixture #{path}") unless File.file?(path) }

payloads = fixture_paths.map { |path| [path, read_json(path)] }
fixture_ids = payloads.map { |path, _| File.basename(path, ".json") }
fail!("fixture ids drifted: #{fixture_ids.sort.inspect}") unless fixture_ids.sort == EXPECTED_FIXTURE_IDS.sort

payloads.each do |path, payload|
  fixture_id = File.basename(path, ".json")
  fail!("#{fixture_id} schema_version drifted") unless payload["schema_version"] == 1
  fail!("#{fixture_id} contract id drifted") unless payload["contract_id"] == "ft.rch_worker_storage_inventory.v1"
  fail!("#{fixture_id} source bead drifted") unless payload["source_bead"] == "ft-5xwsu.1"
  fail!("#{fixture_id} is not read-only") unless payload["read_only"] == true
  fail!("#{fixture_id} forbidden actions drifted") unless payload.fetch("forbidden_actions").sort == EXPECTED_FORBIDDEN.sort
  fail!("#{fixture_id} missing notes") if payload.fetch("notes").empty?

  side_effects = payload.dig("collection_scope", "side_effect_policy")
  fail!("#{fixture_id} side-effect keys drifted") unless side_effects.keys.sort == EXPECTED_SIDE_EFFECT_FLAGS.sort
  side_effects.each do |flag, value|
    expected = flag == "read_only"
    fail!("#{fixture_id} side-effect flag #{flag} drifted") unless value == expected
  end

  artifact_paths = payload.fetch("artifact_paths")
  fail!("#{fixture_id} has no top-level artifact paths") if artifact_paths.empty?
  artifact_set = artifact_paths.to_set
  payload.fetch("worker_inventories").each do |worker|
    worker_id = worker.fetch("worker_id")
    fail!("#{fixture_id} worker #{worker_id} has no retained artifacts") if worker.fetch("artifact_paths").empty?
    worker.fetch("artifact_paths").each do |artifact|
      fail!("#{fixture_id} worker #{worker_id} artifact not in top-level set: #{artifact}") unless artifact_set.include?(artifact)
    end
    %w[df_samples shallow_scans project_du_samples].each do |field|
      fail!("#{fixture_id} worker #{worker_id} missing #{field}") unless worker.key?(field)
    end
  end

  commands = payload.dig("collection_scope", "commands") + scan_records(payload).map { |record| record.fetch("source_command") }
  commands.each do |command|
    fail!("#{fixture_id} records direct local Cargo proof command: #{command}") if command.start_with?("cargo ")
    FORBIDDEN_COMMAND_FRAGMENTS.each do |fragment|
      if command.downcase.include?(fragment.downcase)
        fail!("#{fixture_id} command contains forbidden fragment #{fragment}: #{command}")
      end
    end
  end

  scan_records(payload).each do |record|
    %w[source_command sampled_at_ms artifact_path pressure_reason notes].each do |field|
      fail!("#{fixture_id} retained scan record missing #{field}") unless record.key?(field)
    end
    fail!("#{fixture_id} retained scan record artifact not in top-level set: #{record.fetch("artifact_path")}") unless artifact_set.include?(record.fetch("artifact_path"))
  end
end

healthy = payloads.assoc("fixtures/rch-worker-pressure/valid/healthy-complete.json").last
fail!("healthy-complete status drifted") unless healthy["inventory_status"] == "complete"
fail!("healthy-complete records critical pressure") unless healthy.dig("summary", "workers_with_critical_pressure") == 0
fail!("healthy-complete records telemetry gap") unless healthy.dig("summary", "workers_with_telemetry_gap") == 0

partial = payloads.assoc("fixtures/rch-worker-pressure/valid/partial-timeout.json").last
fail!("partial-timeout status drifted") unless partial["inventory_status"] == "partial"
partial_records = scan_records(partial)
fail!("partial-timeout lacks timeout marker") unless partial_records.any? { |record| record["timeout_state"] == "partial_timeout" }
fail!("partial-timeout lacks partial_output marker") unless partial_records.any? { |record| record["partial_output"] == true }
fail!("partial-timeout summary missing partial count") unless partial.dig("summary", "workers_with_partial_output") > 0

telemetry_gap = payloads.assoc("fixtures/rch-worker-pressure/valid/telemetry-gap.json").last
fail!("telemetry-gap status drifted") unless telemetry_gap["inventory_status"] == "telemetry_gap"
fail!("telemetry-gap summary missing gap count") unless telemetry_gap.dig("summary", "workers_with_telemetry_gap") > 0
gap_records = telemetry_gap.fetch("worker_inventories").flat_map do |worker|
  [worker.fetch("telemetry_status")] + scan_records(telemetry_gap).map { |record| record.fetch("freshness") }
end
fail!("telemetry-gap fixture lacks stale or missing freshness") unless (gap_records & %w[stale missing]).any?

fail!("provenance missing worker storage schema row") unless provenance.include?("`ft-rch-worker-storage-inventory.json`")
fail!("provenance row missing static verifier") unless provenance.include?("bash tests/e2e/test_rch_worker_storage_inventory_contract.sh")

live_e2e = Dir.glob("tests/e2e/**/*.sh").length
fail!("README stamped E2E count stale") unless readme.include?("<!--count:e2e_scripts-->#{live_e2e}<!--/count-->")
fail!("README tree E2E count stale") unless readme.include?("# #{live_e2e} shell E2E scripts")

puts "rch worker storage inventory contract: static verifier passed (#{fixture_paths.length} fixtures, #{EXPECTED_FORBIDDEN.length} forbidden actions, #{live_e2e} E2E scripts)"
RUBY
