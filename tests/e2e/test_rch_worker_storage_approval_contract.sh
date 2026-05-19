#!/usr/bin/env bash
# Static verifier for RCH worker storage approval schema and fixtures.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

SCHEMA="docs/json-schema/ft-rch-worker-storage-approval.json"
DOC="docs/robot-contracts/rch-worker-storage-approval.md"
MANIFEST="fixtures/rch-worker-storage-approval/manifest.json"
PROVENANCE="docs/json-schema/PROVENANCE.md"
README="README.md"

fail() {
  printf 'rch worker storage approval contract: %s\n' "$*" >&2
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
require_file "${DOC}"
require_file "${MANIFEST}"
require_file "${PROVENANCE}"
require_file "${README}"

jq empty "${SCHEMA}" "${MANIFEST}" fixtures/rch-worker-storage-approval/valid/*.json

ruby <<'RUBY'
require "json"
require "set"

SCHEMA = "docs/json-schema/ft-rch-worker-storage-approval.json"
DOC = "docs/robot-contracts/rch-worker-storage-approval.md"
MANIFEST = "fixtures/rch-worker-storage-approval/manifest.json"
PROVENANCE = "docs/json-schema/PROVENANCE.md"
README = "README.md"
EXPECTED_FIXTURE_IDS = %w[
  approved-candidate
  expired-approval
  path-mismatch
  protected-path
  missing-evidence-hash
  live-use-unknown
].freeze
EXPECTED_DECISIONS = {
  "approved-candidate" => "approved",
  "expired-approval" => "expired",
  "path-mismatch" => "path_mismatch",
  "protected-path" => "protected_path",
  "missing-evidence-hash" => "missing_evidence_hash",
  "live-use-unknown" => "live_use_unknown"
}.freeze
EXPECTED_EVIDENCE_CONTRACT_ID = "ft.rch_worker_storage_inventory.v1"
EXPECTED_FORBIDDEN = %w[
  delete_unlisted_path
  wildcard_path_expansion
  rm_rf_parent
  protected_path_cleanup
  live_unknown_cleanup
  stale_evidence_cleanup
  missing_hash_cleanup
  expired_approval_cleanup
  restart_agent_mail
  repair_agent_mail_db
  restart_rch_daemon
  mutate_worker_mirror
  cancel_build
  run_local_cargo_as_proof
  destructive_git
  proceed_without_human_approval
].freeze
EXPECTED_PROTECTED_GLOBS = %w[
  /Users/jemanuel/projects/frankenterm/**
  /Users/jemanuel/projects/frankenterm/.git/**
  /Users/jemanuel/projects/frankenterm/crates/frankenterm-core/**
  /var/run/**
  /private/tmp/agent-mail/**
  /tmp/am-*/**
].freeze
EVIDENCE_ARTIFACT_PATH_PATTERN = "^(?!/)(?!.*(?:^|/)\\.\\.?/)(?!.*(?:^|/)\\.\\.?$)(?!.*//)(?!.*\\\\)(?!.*(?:^|/)\\.git(?:/|$))(?:artifacts/rch-worker-pressure|tests/e2e/artifacts/retained/ft-5xwsu\\.1/rch-worker-pressure)/.+\\.json$".freeze
EVIDENCE_ARTIFACT_ROOTS = %w[
  artifacts/rch-worker-pressure/
  tests/e2e/artifacts/retained/ft-5xwsu.1/rch-worker-pressure/
].freeze
SAFE_EVIDENCE_ARTIFACT_PATH_POSITIVES = %w[
  artifacts/rch-worker-pressure/20260518T030000Z/healthy-complete.json
  tests/e2e/artifacts/retained/ft-5xwsu.1/rch-worker-pressure/healthy-complete/vmi1227854-df.json
].freeze
SAFE_EVIDENCE_ARTIFACT_PATH_NEGATIVES = [
  nil,
  "",
  "/tmp/rch-worker-pressure/healthy-complete.json",
  "./artifacts/rch-worker-pressure/healthy-complete.json",
  "../artifacts/rch-worker-pressure/healthy-complete.json",
  "fixtures/rch-worker-pressure/valid/healthy-complete.json",
  "artifacts//rch-worker-pressure/healthy-complete.json",
  "artifacts/rch-worker-pressure/20260518T030000Z/../healthy-complete.json",
  "artifacts/rch-worker-pressure/20260518T030000Z/./healthy-complete.json",
  "artifacts/rch-worker-pressure/20260518T030000Z/.",
  "artifacts/rch-worker-pressure/20260518T030000Z/..",
  "artifacts/rch-worker-pressure/.git/config.json",
  "tests/e2e/artifacts/retained/ft-5xwsu.1/rch-worker-pressure/.git/config.json",
  "artifacts/rch-worker-pressure/20260518T030000Z/healthy-complete.txt",
  "artifacts\\rch-worker-pressure\\healthy-complete.json"
].freeze
EVIDENCE_ARTIFACT_REGEX = Regexp.new(EVIDENCE_ARTIFACT_PATH_PATTERN)
SHA256 = /\A[0-9a-f]{64}\z/

def fail!(message)
  warn "rch worker storage approval contract: #{message}"
  exit 1
end

def read_json(path)
  JSON.parse(File.read(path))
rescue JSON::ParserError => error
  fail!("#{path} does not parse as JSON: #{error.message}")
end

def safe_evidence_artifact_path?(path)
  return false unless path.is_a?(String)
  return false if path.empty? || path.start_with?("/") || path.include?("\\")
  return false unless path.end_with?(".json")

  parts = path.split("/")
  return false if parts.empty? || parts.any?(&:empty?)
  return false if parts.any? { |part| part == "." || part == ".." || part == ".git" }

  EVIDENCE_ARTIFACT_ROOTS.any? { |root| path.start_with?(root) && path.length > root.length }
end

schema = read_json(SCHEMA)
manifest = read_json(MANIFEST)
doc = File.read(DOC)
provenance = File.read(PROVENANCE)
readme = File.read(README)

fail!("schema id drifted") unless schema["$id"]&.end_with?("/ft-rch-worker-storage-approval.json")
fail!("contract id const missing") unless schema.dig("properties", "contract_id", "const") == "ft.rch_worker_storage_approval.v1"
fail!("evidence contract id const missing") unless schema.dig("properties", "evidence_contract_id", "const") == EXPECTED_EVIDENCE_CONTRACT_ID
fail!("root evidence path schema not shared") unless schema.dig("properties", "evidence_artifact_path", "$ref") == "#/$defs/evidence_artifact_path"
fail!("per-path evidence path schema not shared") unless schema.dig("$defs", "path_request", "properties", "inventory_evidence_path", "$ref") == "#/$defs/evidence_artifact_path"
fail!("evidence path schema pattern drifted") unless schema.dig("$defs", "evidence_artifact_path", "pattern") == EVIDENCE_ARTIFACT_PATH_PATTERN
fail!("explicit human approval const missing") unless schema.dig("properties", "explicit_human_approval_required", "const") == true
fail!("schema missing destructive recovery field") unless schema.fetch("required").include?("destructive_recovery_allowed")
fail!("approval decision enum drifted") unless schema.dig("$defs", "approval_decision", "enum").sort.include?("approved")
fail!("forbidden-operation enum drifted") unless schema.dig("$defs", "forbidden_operation", "enum").sort == EXPECTED_FORBIDDEN.sort
SAFE_EVIDENCE_ARTIFACT_PATH_POSITIVES.each do |path|
  fail!("safe evidence artifact path rejected: #{path}") unless safe_evidence_artifact_path?(path)
  fail!("schema evidence artifact pattern rejected safe path: #{path}") unless EVIDENCE_ARTIFACT_REGEX.match?(path)
end
SAFE_EVIDENCE_ARTIFACT_PATH_NEGATIVES.each do |path|
  fail!("unsafe evidence artifact path accepted: #{path.inspect}") if safe_evidence_artifact_path?(path)
  fail!("schema evidence artifact pattern accepted unsafe path: #{path.inspect}") if path.is_a?(String) && EVIDENCE_ARTIFACT_REGEX.match?(path)
end

fail!("manifest schema_version drifted") unless manifest["schema_version"] == 1
fail!("manifest contract id drifted") unless manifest["contract_id"] == "ft.rch_worker_storage_approval.fixture_manifest.v1"
fail!("manifest bead drifted") unless manifest["bead"] == "ft-5xwsu.2"
fail!("manifest schema pointer drifted") unless manifest["schema"] == SCHEMA
fail!("manifest contract pointer drifted") unless manifest["contract"] == DOC
fail!("manifest verifier missing") unless manifest.fetch("verification").include?("bash tests/e2e/test_rch_worker_storage_approval_contract.sh")

fixture_paths = manifest.fetch("valid")
fixture_ids = fixture_paths.map { |path| File.basename(path, ".json") }
fail!("fixture ids drifted: #{fixture_ids.sort.inspect}") unless fixture_ids.sort == EXPECTED_FIXTURE_IDS.sort
fixture_paths.each { |path| fail!("manifest references missing fixture #{path}") unless File.file?(path) }

payloads = fixture_paths.map { |path| [File.basename(path, ".json"), read_json(path)] }.to_h
payloads.each do |fixture_id, payload|
  fail!("#{fixture_id} schema_version drifted") unless payload["schema_version"] == 1
  fail!("#{fixture_id} contract id drifted") unless payload["contract_id"] == "ft.rch_worker_storage_approval.v1"
  fail!("#{fixture_id} evidence contract id drifted") unless payload["evidence_contract_id"] == EXPECTED_EVIDENCE_CONTRACT_ID
  fail!("#{fixture_id} source bead drifted") unless payload["source_bead"] == "ft-5xwsu.2"
  fail!("#{fixture_id} decision drifted") unless payload["approval_decision"] == EXPECTED_DECISIONS.fetch(fixture_id)
  fail!("#{fixture_id} explicit human approval flag drifted") unless payload["explicit_human_approval_required"] == true
  fail!("#{fixture_id} evidence artifact path is unsafe: #{payload.fetch("evidence_artifact_path")}") unless safe_evidence_artifact_path?(payload.fetch("evidence_artifact_path"))
  fail!("#{fixture_id} forbidden operations drifted") unless payload.fetch("forbidden_operations").sort == EXPECTED_FORBIDDEN.sort

  policy = payload.fetch("protected_path_policy")
  fail!("#{fixture_id} policy must fail closed") unless policy["fail_closed"] == true
  fail!("#{fixture_id} policy must require exact paths") unless policy["exact_path_match_required"] == true
  fail!("#{fixture_id} policy must require hashes") unless policy["hash_required"] == true
  fail!("#{fixture_id} policy must require expiration") unless policy["approval_expiration_required"] == true
  fail!("#{fixture_id} live-use policy must deny unknowns") unless policy["live_use_unknown_action"] == "deny"
  fail!("#{fixture_id} protected globs drifted") unless policy.fetch("protected_globs").sort == EXPECTED_PROTECTED_GLOBS.sort

  expiration = payload.fetch("expiration")
  fail!("#{fixture_id} expiration max_age invalid") unless expiration.fetch("max_age_ms") > 0
  verification = payload.fetch("post_action_verification")
  fail!("#{fixture_id} post-action verification not required") unless verification["required"] == true
  fail!("#{fixture_id} post-action verification must require remote RCH") unless verification["remote_required_rch"] == true
  fail!("#{fixture_id} verification lacks ft-5xwsu.3") unless verification.fetch("beads_to_update").include?("ft-5xwsu.3")
  fail!("#{fixture_id} verification lacks ft-4tp7g") unless verification.fetch("beads_to_update").include?("ft-4tp7g")
  fail!("#{fixture_id} missing rollback/restore notes") if payload.fetch("rollback_or_restore_notes").empty?

  paths = payload.fetch("requested_paths")
  fail!("#{fixture_id} missing requested paths") if paths.empty?
  paths.each do |path|
    fail!("#{fixture_id} path contains wildcard: #{path.fetch("path")}") if path.fetch("path").include?("*")
    fail!("#{fixture_id} path hash invalid") unless SHA256.match?(path.fetch("path_sha256"))
    fail!("#{fixture_id} path evidence artifact path is unsafe: #{path.fetch("inventory_evidence_path")}") unless safe_evidence_artifact_path?(path.fetch("inventory_evidence_path"))
    evidence_hash = path["inventory_evidence_sha256"]
    fail!("#{fixture_id} path evidence hash invalid") if evidence_hash && !SHA256.match?(evidence_hash)
  end

  if fixture_id == "approved-candidate"
    record = payload.fetch("approval_record")
    fail!("approved fixture missing approver/reference") unless record["approver_identity"] || record["approval_reference"]
    fail!("approved fixture missing approval text hash") unless SHA256.match?(record.fetch("approval_text_sha256"))
    fail!("approved fixture must not be expired") unless expiration["expired"] == false
    fail!("approved fixture expiration order invalid") unless record.fetch("expires_at_ms") > record.fetch("approved_at_ms")
    fail!("approved fixture evidence hash invalid") unless SHA256.match?(payload.fetch("evidence_artifact_sha256"))
    fail!("approved fixture path-set hashes differ") unless payload.fetch("requested_path_set_sha256") == payload.fetch("approved_path_set_sha256")
    fail!("approved fixture allowed ops drifted") unless payload.fetch("allowed_operations") == ["move_to_quarantine"]
    fail!("approved fixture recovery flag drifted") unless payload["destructive_recovery_allowed"] == true
    paths.each do |path|
      fail!("approved fixture path classification drifted") unless path["classification"] == "approved_candidate"
      fail!("approved fixture path approval match drifted") unless path["approval_match"] == "exact"
      fail!("approved fixture path live-use drifted") unless path["live_use_state"] == "inactive"
      fail!("approved fixture protected reason drifted") unless path["protected_reason"] == "none"
      fail!("approved fixture path operation drifted") unless path["requested_operation"] == "move_to_quarantine"
    end
  else
    fail!("#{fixture_id} must fail closed") unless payload["destructive_recovery_allowed"] == false
    fail!("#{fixture_id} must not allow operations") unless payload.fetch("allowed_operations").empty?
  end
end

expired = payloads.fetch("expired-approval")
fail!("expired fixture not expired") unless expired.dig("expiration", "expired") == true
fail!("expired fixture approval did not expire before check") unless expired.dig("expiration", "expires_at_ms") < expired.dig("expiration", "checked_at_ms")

mismatch = payloads.fetch("path-mismatch")
fail!("path mismatch fixture path-set hashes should differ") unless mismatch.fetch("requested_path_set_sha256") != mismatch.fetch("approved_path_set_sha256")
fail!("path mismatch fixture lacks mismatch row") unless mismatch.fetch("requested_paths").any? { |path| path["approval_match"] == "mismatch" && path["classification"] == "path_mismatch" }

protected = payloads.fetch("protected-path")
fail!("protected fixture lacks source checkout reason") unless protected.fetch("requested_paths").any? { |path| path["protected_reason"] == "source_checkout" && path["classification"] == "protected_path" }
fail!("protected fixture should have no approved path-set hash") unless protected["approved_path_set_sha256"].nil?

missing_hash = payloads.fetch("missing-evidence-hash")
fail!("missing-hash fixture root hash should be null") unless missing_hash["evidence_artifact_sha256"].nil?
fail!("missing-hash fixture lacks missing evidence row") unless missing_hash.fetch("requested_paths").any? { |path| path["inventory_evidence_sha256"].nil? && path["classification"] == "missing_evidence_hash" }

unknown = payloads.fetch("live-use-unknown")
fail!("live-use fixture lacks unknown state") unless unknown.fetch("requested_paths").any? { |path| path["live_use_state"] == "unknown" && path["protected_reason"] == "unknown_live_use" }

fail!("doc missing schema path") unless doc.include?(SCHEMA)
fail!("doc missing canonical evidence contract id") unless doc.include?("`ft.rch_worker_storage_inventory.v1`")
fail!("doc still endorses pressure-named inventory id") if doc.include?("ft.rch_worker_pressure.inventory.v1")
fail!("doc missing exact path rule") unless doc.include?("exact requested and approved path-set hashes")
fail!("doc missing live-use fail-closed rule") unless doc.include?("live-use unknown must set")
fail!("provenance missing approval schema row") unless provenance.include?("`ft-rch-worker-storage-approval.json`")
fail!("provenance row missing static verifier") unless provenance.include?("bash tests/e2e/test_rch_worker_storage_approval_contract.sh")

git_ls_files = IO.popen(["git", "ls-files", "tests/e2e"], &:read)
fail!("failed to enumerate tracked E2E scripts") unless $?.success?
live_e2e = git_ls_files.lines.count { |path| path.chomp.end_with?(".sh") }
fail!("README stamped E2E count stale") unless readme.include?("<!--count:e2e_scripts-->#{live_e2e}<!--/count-->")
fail!("README tree E2E count stale") unless readme.include?("# #{live_e2e} shell E2E scripts")

puts "rch worker storage approval contract: static verifier passed (#{fixture_paths.length} fixtures, #{EXPECTED_FORBIDDEN.length} forbidden operations, #{live_e2e} E2E scripts)"
RUBY
