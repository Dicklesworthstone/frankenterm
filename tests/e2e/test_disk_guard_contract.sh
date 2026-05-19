#!/usr/bin/env bash
# Static verifier for the disk-guard retained contract fixtures.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

SCHEMA="docs/json-schema/ft-disk-guard.json"
DOC="docs/robot-contracts/disk-guard.md"
MANIFEST="fixtures/disk-guard/manifest.json"
README="README.md"
VALID_DIR="fixtures/disk-guard/valid"

fail() {
  printf 'disk guard contract: %s\n' "$*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "missing command: $1"
}

require_file() {
  [[ -f "$1" ]] || fail "missing file: $1"
}

require_dir() {
  [[ -d "$1" ]] || fail "missing directory: $1"
}

require_command jq
require_command ruby
require_file "${SCHEMA}"
require_file "${DOC}"
require_file "${MANIFEST}"
require_file "${README}"
require_dir "${VALID_DIR}"

jq empty "${SCHEMA}" "${MANIFEST}" "${VALID_DIR}"/*.json

ruby <<'RUBY'
require "json"

SCHEMA = "docs/json-schema/ft-disk-guard.json"
DOC = "docs/robot-contracts/disk-guard.md"
MANIFEST = "fixtures/disk-guard/manifest.json"
README = "README.md"
VALID_DIR = "fixtures/disk-guard/valid"
VERIFIER = "tests/e2e/test_disk_guard_contract.sh"

EXPECTED_VALID_FIXTURES = %w[
  fixtures/disk-guard/valid/current-eno-space.json
  fixtures/disk-guard/valid/cleanup-inventory.json
  fixtures/disk-guard/valid/preflight-surfaces.json
  fixtures/disk-guard/valid/healthy.json
  fixtures/disk-guard/valid/warning-low-space.json
  fixtures/disk-guard/valid/fatal-write-probe-failed.json
].freeze

EXPECTED_PROBES = %w[
  system_data_volume
  private_tmp
  repo_write_probe
  beads_db_writeability
  beads_jsonl_exportability
  agent_mail_db_open
  rch_cache_writeability
  external_scratch
].freeze

EXPECTED_FORBIDDEN_ACTIONS = %w[
  delete_file
  delete_directory
  clean_target
  repair_agent_mail
  restart_agent_mail
  restart_rch
  mutate_worker_mirror
  cancel_build
  run_local_cargo_proof
  destructive_git
].freeze

EXPECTED_PREFLIGHT_SURFACES = %w[
  patch_application
  static_verifier_creation
  beads_comment_export
  rch_proof_lane
].freeze

EXPECTED_DECISIONS = %w[
  block
  external_scratch_only
  proceed
  static_only
].freeze

def fail!(message)
  warn "disk guard contract: #{message}"
  exit 1
end

def read_json(path)
  JSON.parse(File.read(path))
rescue JSON::ParserError => error
  fail!("#{path} does not parse as JSON: #{error.message}")
end

schema = read_json(SCHEMA)
manifest = read_json(MANIFEST)
doc = File.read(DOC)
readme = File.read(README)

fail!("schema id drifted") unless schema["$id"]&.end_with?("/ft-disk-guard.json")
fail!("schema_version const drifted") unless schema.dig("properties", "schema_version", "const") == 1
fail!("contract_id const drifted") unless schema.dig("properties", "contract_id", "const") == "ft.disk_guard.v1"

root_required = schema.fetch("required")
%w[
  schema_version
  contract_id
  generated_at_ms
  guard_id
  workspace_root
  decision
  side_effect_policy
  probes
  reason_codes
  artifact_paths
].each do |field|
  fail!("schema missing required root field #{field}") unless root_required.include?(field)
end

probe_enum = schema.dig("$defs", "probe", "properties", "probe_id", "enum")
fail!("probe enum drifted") unless probe_enum.sort == EXPECTED_PROBES.sort
forbidden_enum = schema.dig("$defs", "side_effect_policy", "properties", "forbidden_actions", "items", "enum")
fail!("forbidden action enum drifted") unless forbidden_enum.sort == EXPECTED_FORBIDDEN_ACTIONS.sort
preflight_surface_enum = schema.dig("$defs", "preflight_result", "properties", "surface", "enum")
fail!("preflight surface enum drifted") unless preflight_surface_enum.sort == EXPECTED_PREFLIGHT_SURFACES.sort
cleanup_tier_enum = schema.dig("$defs", "cleanup_candidate", "properties", "risk_tier", "enum")
%w[low medium high protected unknown].each do |tier|
  fail!("cleanup risk tier missing #{tier}") unless cleanup_tier_enum.include?(tier)
end

fail!("manifest contract id drifted") unless manifest["contract_id"] == "ft.disk_guard.fixture_manifest.v1"
fail!("manifest schema pointer drifted") unless manifest["schema"] == SCHEMA
fail!("manifest doc pointer drifted") unless manifest["contract"] == DOC
fail!("manifest valid fixture list drifted") unless manifest.fetch("valid").sort == EXPECTED_VALID_FIXTURES.sort
fail!("manifest invalid list should remain empty until invalid fixtures exist") unless manifest.fetch("invalid") == []
verification = manifest.fetch("verification")
fail!("manifest missing jq verifier command") unless verification.any? { |entry| entry.include?("jq empty #{SCHEMA}") }
fail!("manifest missing static verifier command") unless verification.include?("bash #{VERIFIER}")

tracked_fixtures = Dir.glob("#{VALID_DIR}/*.json").sort
fail!("valid fixture directory drifted") unless tracked_fixtures == EXPECTED_VALID_FIXTURES.sort

decisions = []
fixtures_by_path = {}
fixtures = EXPECTED_VALID_FIXTURES.map do |path|
  fixture = read_json(path)
  fixtures_by_path[path] = fixture
  decisions << fixture.fetch("decision")
  fail!("#{path} schema_version drifted") unless fixture.fetch("schema_version") == 1
  fail!("#{path} contract_id drifted") unless fixture.fetch("contract_id") == "ft.disk_guard.v1"
  fail!("#{path} guard_id missing") if fixture.fetch("guard_id").strip.empty?
  fail!("#{path} generated_at_ms missing") unless fixture.fetch("generated_at_ms") > 0
  fail!("#{path} probes coverage drifted") unless fixture.fetch("probes").map { |probe| probe.fetch("probe_id") }.sort == EXPECTED_PROBES.sort
  policy = fixture.fetch("side_effect_policy")
  fail!("#{path} side_effect_policy.read_only must be true") unless policy.fetch("read_only") == true
  fail!("#{path} cleanup approval const drifted") unless policy.fetch("cleanup_requires_operator_approval") == true
  fail!("#{path} forbidden actions drifted") unless policy.fetch("forbidden_actions").sort == EXPECTED_FORBIDDEN_ACTIONS.sort
  fail!("#{path} emitted deletion command text") if fixture.to_json.downcase.include?("rm -rf")
  fail!("#{path} has no artifact path") if fixture.fetch("artifact_paths").empty?
  fail!("#{path} missing policy cleanup reason") unless fixture.fetch("reason_codes").include?("policy.cleanup_requires_operator_approval")
  fixture
end

fail!("fixture decision coverage drifted") unless decisions.uniq.sort == EXPECTED_DECISIONS.sort

cleanup = fixtures_by_path.fetch("fixtures/disk-guard/valid/cleanup-inventory.json")
fail!("cleanup inventory fixture missing") unless cleanup
candidate = cleanup.fetch("cleanup_candidates").first
fail!("cleanup fixture should have exactly one candidate") unless cleanup.fetch("cleanup_candidates").length == 1
fail!("cleanup candidate risk tier drifted") unless candidate.fetch("risk_tier") == "low"
fail!("cleanup candidate must require operator approval") unless candidate.fetch("operator_approval_required") == true
fail!("cleanup candidate must forbid automatic cleanup") unless candidate.fetch("automatic_cleanup_allowed") == false
fail!("cleanup candidate live-use drifted") unless candidate.fetch("live_use") == "not_referenced"
%w[
  cleanup_candidate.no_automatic_deletion
  cleanup_candidate.operator_approval_required
  cleanup_candidate.no_live_reference
].each do |code|
  fail!("cleanup candidate missing reason #{code}") unless candidate.fetch("reason_codes").include?(code)
end

preflight = fixtures_by_path.fetch("fixtures/disk-guard/valid/preflight-surfaces.json")
fail!("preflight fixture missing") unless preflight
preflight_results = preflight.fetch("preflight_results")
fail!("preflight surface coverage drifted") unless preflight_results.map { |row| row.fetch("surface") }.sort == EXPECTED_PREFLIGHT_SURFACES.sort
preflight_results.each do |row|
  fail!("preflight #{row.fetch("surface")} should allow in healthy fixture") unless row.fetch("action") == "allow"
  fail!("preflight #{row.fetch("surface")} should allow writes in healthy fixture") unless row.fetch("write_allowed") == true
  fail!("preflight #{row.fetch("surface")} should not require external scratch") unless row.fetch("external_scratch_required") == false
end

current_enospc = fixtures_by_path.fetch("fixtures/disk-guard/valid/current-eno-space.json")
fail!("current ENOSPC fixture missing") unless current_enospc
fail!("current ENOSPC decision drifted") unless current_enospc.fetch("decision") == "external_scratch_only"
fail!("current ENOSPC fixture missing external scratch reason") unless current_enospc.fetch("reason_codes").include?("external_scratch.available")

%w[
  ft.disk_guard.v1
  docs/json-schema/ft-disk-guard.json
  fixtures/disk-guard/manifest.json
  tests/e2e/test_disk_guard_contract.sh
  cleanup_candidates
  operator_approval_required
  automatic_cleanup_allowed
  static_verifier_creation
  rch_proof_lane
  external_scratch_only
].each do |term|
  fail!("doc missing #{term}") unless doc.include?(term)
end

git_ls_files = IO.popen(["git", "ls-files", "tests/e2e"], &:read)
fail!("failed to enumerate tracked E2E scripts") unless $?.success?
live_e2e = git_ls_files.lines.count { |path| path.chomp.end_with?(".sh") }
fail!("README stamped E2E count stale") unless readme.include?("<!--count:e2e_scripts-->#{live_e2e}<!--/count-->")
fail!("README tree E2E count stale") unless readme.include?("# #{live_e2e} shell E2E scripts")

puts "disk guard contract: static verifier passed (#{fixtures.length} fixtures, #{EXPECTED_PROBES.length} probes, #{EXPECTED_PREFLIGHT_SURFACES.length} preflight surfaces, #{live_e2e} E2E scripts)"
RUBY
