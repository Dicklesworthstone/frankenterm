#!/usr/bin/env bash
# Static privacy/no-mutation guard for the swarm incident-bundle golden fixtures
# (crates/frankenterm-core/tests/fixtures/incident_bundle_goldens/{normal,
# degraded,sensitive_transcript}). The Rust consumer
# (tests/incident_bundle_golden_fixtures.rs) needs cargo/RCH; this verifier locks
# the security contract these goldens encode WITHOUT compilation, so the privacy
# doctrine stays enforced while the remote proof lane (ft-4tp7g) is down:
#   * incident collection never mutates state, never repairs Agent Mail, and only
#     ever uses read-only process samplers,
#   * redaction counts reconcile across the manifest, the redaction report, and
#     the source payloads (no over- or under-claimed redaction),
#   * pane text is gated (disabled => no pane-text source; otherwise summaries
#     only, partial redaction, with the redaction warning), and
#   * no source payload carries a raw pane/source-text key.
# Pure jq + ruby. Related contract: ft.swarm_incident_bundle.v1 (ft-1nqye shape).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

GOLDEN_ROOT="crates/frankenterm-core/tests/fixtures/incident_bundle_goldens"

fail() {
  printf 'incident bundle redaction goldens: %s\n' "$*" >&2
  exit 1
}

command -v jq >/dev/null 2>&1 || fail "missing command: jq"
command -v ruby >/dev/null 2>&1 || fail "missing command: ruby"
[[ -d "${GOLDEN_ROOT}" ]] || fail "missing golden root: ${GOLDEN_ROOT}"

GOLDEN_ROOT="${GOLDEN_ROOT}" ruby <<'RUBY'
require "json"

ROOT = ENV.fetch("GOLDEN_ROOT")
CONTRACT_ID = "ft.swarm_incident_bundle.v1"
REQUIRED_GOLDENS = %w[normal degraded sensitive_transcript].freeze
# Read-only process sampler modes. Anything else would let incident collection
# perturb the very swarm it is auditing.
SAMPLER_MODES = %w[disabled bounded_snapshot].freeze
# A raw pane/source-text payload key must never appear in any source file.
BANNED_RAW_KEYS = %w[source_text pane_text raw_pane_text raw_pane_content].freeze
REDACTION_MARKER = "[REDACTED]"
TRUNCATION_MARKER = "[PANE_TEXT_TRUNCATED]"

def fail!(message)
  warn "incident bundle redaction goldens: #{message}"
  exit 1
end

def read_json(path)
  JSON.parse(File.read(path))
rescue Errno::ENOENT
  fail!("missing file: #{path}")
rescue JSON::ParserError => error
  fail!("#{path} does not parse as JSON: #{error.message}")
end

def collect_keys(node, acc = [])
  case node
  when Hash
    node.each { |key, value| acc << key; collect_keys(value, acc) }
  when Array
    node.each { |item| collect_keys(item, acc) }
  end
  acc
end

dirs = Dir.children(ROOT).select { |d| File.directory?(File.join(ROOT, d)) }.sort
fail!("golden set drifted: #{dirs.inspect}") unless dirs == REQUIRED_GOLDENS.sort

REQUIRED_GOLDENS.each do |golden|
  base = File.join(ROOT, golden)
  manifest = read_json(File.join(base, "incident_manifest.json"))
  report = read_json(File.join(base, "redaction_report.json"))
  swarm = manifest.fetch("swarm")

  # --- Contract identity --------------------------------------------------
  fail!("#{golden}: contract_id drift #{swarm["contract_id"].inspect}") unless swarm["contract_id"] == CONTRACT_ID

  # --- No-mutation / no-repair doctrine ----------------------------------
  policy = swarm.fetch("collection_policy")
  fail!("#{golden}: mutating_actions_allowed must be false") unless policy["mutating_actions_allowed"] == false
  fail!("#{golden}: agent_mail_repair_allowed must be false") unless policy["agent_mail_repair_allowed"] == false
  fail!("#{golden}: process_sampler #{policy["process_sampler"].inspect} not a read-only mode") unless SAMPLER_MODES.include?(policy["process_sampler"])

  sources = swarm.fetch("sources")
  sources.each do |src|
    fail!("#{golden}: source #{src["name"].inspect} mutates state") unless src["mutates_state"] == false
  end

  # --- Manifest file-list integrity --------------------------------------
  files = manifest.fetch("files")
  files.each do |rel|
    fail!("#{golden}: manifest lists missing file #{rel}") unless File.file?(File.join(base, rel))
  end
  sources.each do |src|
    rel = src["file"]
    next if rel.nil? # in-manifest evidence, not a file on disk
    fail!("#{golden}: source file #{rel} missing on disk") unless File.file?(File.join(base, rel))
    fail!("#{golden}: source file #{rel} not in manifest files[]") unless files.include?(rel)
  end

  # --- Redaction reconciliation across manifest / report / payloads ------
  summary = swarm.fetch("redaction_summary")
  per_file = report.fetch("per_file")
  report_total = report.fetch("total_redactions")
  per_file_sum = per_file.sum { |entry| entry.fetch("count") }
  fail!("#{golden}: redaction_report total #{report_total} != sum of per_file #{per_file_sum}") unless report_total == per_file_sum
  fail!("#{golden}: manifest redaction total #{summary["total_redactions"]} != report total #{report_total}") unless summary["total_redactions"] == report_total
  fail!("#{golden}: manifest redacted_files #{summary["redacted_files"]} != report per_file count #{per_file.length}") unless summary["redacted_files"] == per_file.length
  per_file.each do |entry|
    fail!("#{golden}: redaction_report references missing file #{entry["file"]}") unless File.file?(File.join(base, entry.fetch("file")))
    fail!("#{golden}: redaction_report count for #{entry["file"]} must be >= 1") unless entry.fetch("count") >= 1
  end

  # --- Privacy: no raw pane/source-text key in any source payload --------
  source_files = files.select { |rel| rel.start_with?("sources/") && rel.end_with?(".json") }
  source_payloads = source_files.to_h do |rel|
    payload = read_json(File.join(base, rel))
    keys = collect_keys(payload)
    BANNED_RAW_KEYS.each do |banned|
      fail!("#{golden}: source #{rel} carries banned raw key #{banned}") if keys.include?(banned)
    end
    [rel, payload]
  end

  # --- Pane-text gating ---------------------------------------------------
  pane_sources = sources.select { |src| src["name"].to_s.include?("pane_text") || src["file"].to_s.include?("pane_text") }
  pane_allowed = policy["pane_text_allowed"]
  if pane_allowed == "disabled"
    fail!("#{golden}: pane_text disabled but a pane-text source is present") unless pane_sources.empty?
    fail!("#{golden}: pane_text disabled but a pane_text source file exists") if source_files.any? { |rel| rel.include?("pane_text") }
  else
    fail!("#{golden}: pane_text present but allowance #{pane_allowed.inspect} is not summaries_only") unless pane_allowed == "summaries_only"
    fail!("#{golden}: pane_text allowed but no pane-text source declared") if pane_sources.empty?
    pane_sources.each do |src|
      fail!("#{golden}: pane-text source must be partially redacted") unless src["redaction"] == "partial"
      fail!("#{golden}: pane-text source missing redaction warning id") unless (src["warning_ids"] || []).include?("pane_text.redacted")
    end

    # --- Pane summary payload privacy + count reconciliation -------------
    pane_sources.each do |src|
      rel = src.fetch("file")
      payload = source_payloads.fetch(rel) { read_json(File.join(base, rel)) }
      fail!("#{golden}: #{rel} provenance must not mutate state") unless payload.dig("provenance", "mutates_state") == false
      panes = payload.fetch("panes")
      pane_redactions = panes.sum { |p| p.fetch("redactions") }
      report_count = per_file.find { |e| e["file"] == rel }&.fetch("count")
      fail!("#{golden}: #{rel} pane redactions #{pane_redactions} != redaction_report count #{report_count.inspect}") unless report_count == pane_redactions
      panes.each do |pane|
        if pane.fetch("redactions") > 0
          fail!("#{golden}: pane #{pane["pane_id"]} claims redactions but summary lacks #{REDACTION_MARKER}") unless pane.fetch("summary").include?(REDACTION_MARKER)
        end
        if pane.fetch("summary").downcase.include?("clip") || pane.fetch("summary").downcase.include?("trunc")
          fail!("#{golden}: pane #{pane["pane_id"]} implies truncation but lacks #{TRUNCATION_MARKER}") unless pane.fetch("summary").include?(TRUNCATION_MARKER)
          fail!("#{golden}: truncated pane #{pane["pane_id"]} missing tail_lines") unless pane.key?("tail_lines")
        end
      end
    end
  end

  printf("incident bundle redaction goldens: %s ok (%d sources, %d redactions, pane_text=%s)\n",
         golden, sources.length, report_total, pane_allowed)
end

puts "incident bundle redaction goldens: static verifier passed (#{REQUIRED_GOLDENS.length} goldens)"
RUBY
