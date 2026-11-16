#!/usr/bin/env bash
# Static verifier for Agent Mail failover snapshot schema and fixtures.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

SCHEMA="docs/json-schema/ft-agent-mail-failover-snapshot.json"
DOC="docs/robot-contracts/agent-mail-failover.md"
CORPUS="fixtures/agent-mail-failover/snapshots.v1.json"
PROVENANCE="docs/json-schema/PROVENANCE.md"

fail() {
  printf 'agent-mail failover snapshots: %s\n' "$*" >&2
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
require_file "${CORPUS}"
require_file "${PROVENANCE}"

jq empty "${SCHEMA}" "${CORPUS}"

ruby <<'RUBY'
require "json"
require "set"

SCHEMA = "docs/json-schema/ft-agent-mail-failover-snapshot.json"
DOC = "docs/robot-contracts/agent-mail-failover.md"
CORPUS = "fixtures/agent-mail-failover/snapshots.v1.json"
PROVENANCE = "docs/json-schema/PROVENANCE.md"
EXPECTED_FIXTURES = %w[
  healthy-agent-mail
  unavailable-after-retry
  transient-database-recovery
  no-in-progress-beads
  stale-candidates-present
  dirty-tracked-overlap
  untracked-review-required
].freeze
EXPECTED_COVERAGE = %w[
  healthy_agent_mail
  unavailable_after_retry
  transient_database_recovery_message
  no_in_progress_beads
  stale_candidates_present
  dirty_tracked_overlap
  untracked_review_required
].freeze
EXPECTED_FORBIDDEN = [
  "am service restart",
  "am service stop",
  "am doctor fix",
  "am doctor repair",
  "am doctor reconstruct",
  "kill am/serve-http/mcp-agent-mail",
  "delete files",
  "git reset --hard",
  "git clean -fd",
  "run local Cargo as proof",
  "mutate RCH worker",
  "cancel RCH build"
].freeze

def fail!(message)
  warn "agent-mail failover snapshots: #{message}"
  exit 1
end

def read_json(path)
  JSON.parse(File.read(path))
rescue JSON::ParserError => error
  fail!("#{path} does not parse as JSON: #{error.message}")
end

schema = read_json(SCHEMA)
corpus = read_json(CORPUS)
doc = File.read(DOC)
provenance = File.read(PROVENANCE)

fail!("schema id drifted") unless schema["$id"]&.end_with?("/ft-agent-mail-failover-snapshot.json")
fail!("schema root type drifted") unless schema["type"] == "object"
fail!("mode enum drifted") unless schema.dig("properties", "mode", "enum").sort == %w[
  agent_mail_healthy
  agent_mail_transient_database_recovery
  agent_mail_unavailable_beads_only
].sort
fail!("forbidden action enum drifted") unless schema.dig("$defs", "forbidden_action", "enum").sort == EXPECTED_FORBIDDEN.sort
fail!("stale default const missing") unless schema.dig("$defs", "stale_reopen", "properties", "default_action", "const") == "do_not_reopen"
fail!("dirty overlap recommendation const missing") unless schema.dig("$defs", "dirty_overlap", "properties", "recommendation", "const") == "do_not_reopen_related_beads_until_owner_clear"

fail!("corpus version drifted") unless corpus["schema_version"] == 1
fail!("corpus contract id drifted") unless corpus["contract_id"] == "ft.agent_mail_failover.snapshot.fixture_corpus.v1"
fail!("corpus bead drifted") unless corpus["source_bead"] == "ft-5lsqo.1"
fail!("corpus schema pointer drifted") unless corpus["schema"] == SCHEMA
fail!("corpus contract pointer drifted") unless corpus["contract"] == DOC
fail!("corpus verifier missing") unless corpus.fetch("verification").include?("bash fixtures/agent-mail-failover/verify-fallback-snapshots.sh")

fixtures = corpus.fetch("fixtures")
fixture_ids = fixtures.map { |fixture| fixture.fetch("id") }
fail!("fixture ids drifted: #{fixture_ids.sort.inspect}") unless fixture_ids.sort == EXPECTED_FIXTURES.sort
coverage = fixtures.flat_map { |fixture| fixture.fetch("covers") }.uniq.sort
fail!("fixture coverage drifted: #{coverage.inspect}") unless coverage == EXPECTED_COVERAGE.sort

fixtures.each do |fixture|
  id = fixture.fetch("id")
  snapshot = fixture.fetch("snapshot")

  %w[ts session mode agent_mail beads git next_actions proof_doctor].each do |field|
    fail!("#{id} missing root field #{field}") unless snapshot.key?(field)
  end
  fail!("#{id} proof doctor must disclaim proof") unless snapshot.fetch("proof_doctor").include?("no Cargo/RCH proof lane claimed")

  mail = snapshot.fetch("agent_mail")
  fail!("#{id} forbidden actions drifted") unless mail.fetch("forbidden_actions").sort == EXPECTED_FORBIDDEN.sort
  fail!("#{id} missing retry_count") unless mail.key?("retry_count")
  if mail["status"] == "healthy"
    fail!("#{id} healthy mail should have retry_count 0") unless mail["retry_count"] == 0
  else
    fail!("#{id} unavailable/recovery mail should record retry") unless mail["retry_count"] >= 1
    fail!("#{id} marker must forbid repair or restart") unless mail.fetch("marker").match?(/do not repair|do not repair\/restart/i)
  end

  beads = snapshot.fetch("beads")
  fail!("#{id} in_progress count mismatch") unless beads.fetch("in_progress_count") == beads.fetch("in_progress").length
  fail!("#{id} ready count mismatch") unless beads.fetch("ready_count") == beads.fetch("ready").length
  stale = beads.fetch("stale_reopen")
  fail!("#{id} default stale action drifted") unless stale["default_action"] == "do_not_reopen"
  fail!("#{id} threshold should be at least two hours") unless stale.fetch("threshold_seconds") >= 7200
  fail!("#{id} missing dirty tree guard") unless stale.fetch("dirty_tree_guard").include?("Do not reopen")
  fail!("#{id} manual checks missing fallback command") unless stale.fetch("manual_checks").any? { |check| check.include?("swarm-tick.sh --agent-mail-fallback") }

  git = snapshot.fetch("git")
  fail!("#{id} dirty count mismatch") unless git.fetch("dirty_count") == git.fetch("dirty_paths").length
  fail!("#{id} tracked count mismatch") unless git.fetch("tracked_dirty_count") == git.fetch("dirty_paths").count { |path| path.fetch("status") != "??" }
  fail!("#{id} untracked count mismatch") unless git.fetch("untracked_dirty_count") == git.fetch("dirty_paths").count { |path| path.fetch("status") == "??" }
  fail!("#{id} high-risk count mismatch") unless git.fetch("high_risk_count") == git.fetch("dirty_paths").count { |path| path.fetch("severity") == "high" }

  case id
  when "healthy-agent-mail"
    fail!("healthy fixture mode drifted") unless snapshot["mode"] == "agent_mail_healthy"
    fail!("healthy fixture status drifted") unless mail["status"] == "healthy"
  when "unavailable-after-retry"
    fail!("unavailable fixture status drifted") unless mail["status"] == "unavailable"
    fail!("unavailable fixture should have no in-progress beads") unless beads["in_progress_count"] == 0
  when "transient-database-recovery"
    fail!("transient fixture status drifted") unless mail["status"] == "transient_database_recovery"
    fail!("transient fixture missing database message") unless mail.fetch("last_error").include?("database")
  when "no-in-progress-beads"
    fail!("no-in-progress fixture count drifted") unless beads["in_progress_count"] == 0
    fail!("no-in-progress fixture should keep active agents empty") unless beads.fetch("active_agents").empty?
  when "stale-candidates-present"
    fail!("stale fixture lacks stale candidate") unless stale.fetch("candidates").any? { |candidate| candidate["recommendation"] == "status_check_before_reopen" && candidate["age_seconds"] >= stale["threshold_seconds"] }
  when "dirty-tracked-overlap"
    fail!("dirty tracked fixture risk drifted") unless git["risk_level"] == "high"
    fail!("dirty tracked fixture missing high overlap") unless stale.fetch("dirty_overlap_unknown").any? { |path| path["category"] == "tracked_overlap_risk" && path["severity"] == "high" }
  when "untracked-review-required"
    fail!("untracked fixture risk drifted") unless git["risk_level"] == "medium"
    fail!("untracked fixture missing review-required path") unless stale.fetch("dirty_overlap_unknown").any? { |path| path["category"] == "untracked_review_required" && path["severity"] == "medium" }
  end
end

forbidden_text = [
  doc,
  corpus.to_s
].join("\n")
%w[
  "am service restart"
  "am doctor fix"
  "am doctor repair"
  "am doctor reconstruct"
  "git reset --hard"
  "git clean -fd"
].each do |forbidden|
  fail!("forbidden action #{forbidden} must appear only as a forbidden item") unless forbidden_text.include?(forbidden)
end
fail!("doc missing schema path") unless doc.include?(SCHEMA)
fail!("doc missing fallback script") unless doc.include?("scripts/swarm-tick.sh --agent-mail-fallback")
fail!("doc missing do_not_reopen") unless doc.include?("do_not_reopen")
fail!("provenance missing schema row") unless provenance.include?("`ft-agent-mail-failover-snapshot.json`")
fail!("provenance missing verifier") unless provenance.include?("bash fixtures/agent-mail-failover/verify-fallback-snapshots.sh")

puts "agent-mail failover snapshots: static verifier passed (#{fixtures.length} fixtures, #{coverage.length} coverage cases)"
RUBY
