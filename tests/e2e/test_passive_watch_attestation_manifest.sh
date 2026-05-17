#!/usr/bin/env bash
set -euo pipefail

ROOT="${FRANKENTERM_REPO_ROOT:-/Users/jemanuel/projects/frankenterm}"
cd "$ROOT"

ruby -rjson -e '
def assert(ok, msg)
  raise msg unless ok
end

att_path = "docs/security/passive-watch-attestation.json"
contract_path = "crates/frankenterm-core/src/passive_watch_invariant.rs"
fuzz_path = "fuzz/fuzz_targets/passive_watch_invariant.rs"
audit_path = "docs/security/passive-watch-attestation.md"
corpus_dir = "fuzz/corpus/passive_watch_invariant"

att = JSON.parse(File.binread(att_path))
manifest = JSON.parse(File.binread("docs/attestations/manifest.json"))
contract = File.binread(contract_path)
fuzz = File.binread(fuzz_path)
toml = File.binread("fuzz/Cargo.toml")
audit = File.binread(audit_path)
readme = File.binread("README.md")

assert(att["category"] == "security/passive-watch", "wrong category")
assert(att["schema_version"] == "ft.security.passive_watch_attestation.v1", "wrong schema version")
assert(att["produced_by_bead"] == "ft-e87u6.11", "wrong recovery producer")
assert(att["source_bead"] == "ft-x0666.1", "wrong source bead")

slot = manifest.fetch("slots").find { |s| s["category"] == "security/passive-watch" }
assert(slot, "manifest missing passive-watch slot")
assert(slot["path"] == att_path, "manifest path mismatch")
assert(slot["media_type"] == "application/json", "manifest media type mismatch")
assert(slot["produced_by_bead"] == "ft-x0666.1", "manifest producer mismatch")

headline = att.fetch("headline_rule")
assert(headline.fetch("hard_invariants") == [
  "zero outbound mutating IPC per fuzz input",
  "zero non-capture storage writes per fuzz input",
], "headline hard invariants changed")
assert(headline.fetch("allowed_actions") == %w[capture pattern_detection watch_metadata_write], "allowed actions changed")
assert(headline.fetch("forbidden_actions") == %w[outbound_send outbound_spawn outbound_close non_capture_storage_write], "forbidden actions changed")

basis = att.fetch("artifact_basis")
{
  "status" => "foundation_slice_attested",
  "contract_module" => contract_path,
  "fuzz_target" => fuzz_path,
  "seed_corpus_dir" => "#{corpus_dir}/",
  "audit_doc" => audit_path,
  "release_fuzz_status" => "not_run_in_recovery_artifact",
}.each { |k, v| assert(basis[k] == v, "artifact_basis #{k} mismatch") }
assert(basis["release_fuzz_duration_seconds"] == 0, "release fuzz duration must stay zero")

health = att.fetch("passive_watch_health_contract")
assert(health["type"] == "PassiveWatchHealth", "health type mismatch")
assert(health["fields"] == %w[
  iterations_total captures_total detections_total metadata_writes_total
  mutating_violations_total unclassified_other_total
], "health fields changed")
assert(health["safe_condition"] == "iterations_total > 0 && mutating_violations_total == 0", "health safe condition changed")
assert(health["cold_baseline_safe"] == false, "cold baseline must be unsafe")

clean = att.fetch("representative_clean_health_snapshot")
assert(clean["iterations_total"] == 1, "clean snapshot iterations mismatch")
assert(clean["mutating_violations_total"] == 0, "clean snapshot mutation count mismatch")
assert(clean["unclassified_other_total"] == 0, "clean snapshot other count mismatch")
assert(clean["is_safe"] == true, "clean snapshot must be safe")

seed_section = att.fetch("seed_corpus")
files = Dir.children(corpus_dir).sort
sizes = files.to_h { |name| [name, File.size(File.join(corpus_dir, name))] }
declared = seed_section.fetch("seeds").to_h { |seed| [seed.fetch("name"), seed.fetch("bytes")] }
assert(seed_section["seed_count"] == 10, "seed count changed")
assert(seed_section["total_bytes"] == 14_516, "total seed bytes changed")
assert(declared.keys.sort == files, "seed names do not match corpus files")
assert(declared == sizes, "seed byte counts do not match corpus files")
assert(seed_section["kinds"].sort == seed_section.fetch("seeds").map { |s| s.fetch("kind") }.uniq.sort, "seed kinds are not fully covered")

fuzz_contract = att.fetch("fuzz_target_contract")
assert(fuzz_contract["target_name"] == "passive_watch_invariant", "fuzz target name changed")
assert(fuzz_contract["input_cap_bytes"] == 262_144, "fuzz input cap changed")
assert(fuzz_contract["parser_surface"] == "scan_pipeline::quick_scan", "parser surface changed")
assert(fuzz_contract["assertions"].include?("check_invariants(&obs) returns empty"), "fuzz invariant assertion missing")
assert(fuzz_contract["assertions"].include?("PassiveWatchHealth::is_safe() is true after folding the observation"), "fuzz health assertion missing")

unit = att.fetch("unit_test_contract")
assert(unit["test_count"] >= 15, "unit test count regressed")
assert(unit["covered_shapes"].include?("corpus slugs match the documented values"), "corpus slug coverage missing")
assert(contract.scan(/^\s*#\[test\]\s*$/).length >= unit["test_count"], "contract has fewer tests than attested")

validation = att.fetch("targeted_validation")
assert(validation["command"].include?("rch exec"), "targeted validation must use rch")
assert(validation["command"].include?("passive_watch_invariant::tests"), "targeted validation target changed")
assert(validation["status"] == "blocked", "recovery artifact must not claim green validation")
assert(validation["error_code"] == "RCH-E104", "targeted validation blocker changed")

att.fetch("source_documents").each { |path| assert(File.file?(path), "missing source document #{path}") }

%w[adversarial_seed_catalog PassiveWatchHealth check_invariants NoOutboundMutatingIpc NoNonCaptureStorageWrite].each do |term|
  assert(contract.include?(term), "contract missing #{term}")
end
assert(contract.include?("self.iterations_total > 0 && self.mutating_violations_total == 0"), "cold-baseline guard changed")
%w[quick_scan check_invariants fold_observation].each { |term| assert(fuzz.include?(term), "fuzz target missing #{term}") }
assert(fuzz.include?("PassiveWatchHealth::baseline"), "fuzz target missing health baseline")
assert(fuzz.include?("health.is_safe"), "fuzz target missing health check")
assert(fuzz.include?("256 * 1024"), "fuzz target input cap changed")

bin = toml[/\[\[bin\]\]\s*name = "passive_watch_invariant".*?(?=\n\[\[bin\]\]|\z)/m]
assert(bin, "fuzz Cargo.toml missing passive_watch_invariant bin")
assert(bin.include?("path = \"fuzz_targets/passive_watch_invariant.rs\""), "fuzz bin path changed")
assert(bin.include?("required-features = [\"core-fuzz-targets\"]"), "fuzz bin feature changed")

["Zero outbound mutating IPC", "Zero non-capture storage", "fuzz/corpus/passive_watch_invariant/", "cargo-fuzz target"].each do |term|
  assert(audit.include?(term), "audit doc missing #{term}")
end
assert(audit.include?("docs/security/passive-watch-attestation.json"), "audit doc missing current JSON attestation path")
assert(audit.include?("docs/attestations/manifest.json"), "audit doc missing current manifest path")
assert(!audit.include?("authored once that schema lands"), "audit doc still says attestation entry is future-only")
assert(!audit.include?("depends on `ft-syqcz.1`"), "audit doc still says JSON artifact depends on retired schema bead")
assert(audit.include?("`is_safe()` returns `iterations_total > 0 && mutating_violations_total == 0`"), "audit doc missing cold-baseline-safe health condition")
assert(!audit.include?("`is_safe()` returns `mutating_violations_total == 0`"), "audit doc still claims cold baseline is safe")
assert(!audit.include?("every_action_kind_has_a_classification"), "audit doc names nonexistent action-classification test")
assert(!contract.include?("every_action_kind_has_a_classification"), "contract comments name nonexistent action-classification test")
assert(contract.include?("exhaustive `WatchAction::is_mutating` match"), "contract comments missing exhaustive match classification guard")

live_e2e = Dir.glob("tests/e2e/**/*.sh").length
assert(readme.include?("<!--count:e2e_scripts-->#{live_e2e}<!--/count-->"), "README stamped E2E count is stale; live=#{live_e2e}")
assert(readme.include?("# #{live_e2e} shell E2E scripts"), "README tree E2E count is stale; live=#{live_e2e}")

puts "passive-watch attestation verifier: passed (#{seed_section["seed_count"]} seeds, #{seed_section["total_bytes"]} bytes, #{live_e2e} E2E scripts)"
'
