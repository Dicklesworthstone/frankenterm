#!/usr/bin/env bash
set -euo pipefail

ROOT="${FRANKENTERM_REPO_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"
export FRANKENTERM_REPO_ROOT="$ROOT"
cd "$ROOT"

source "tests/scripts/static_attestation_helpers.sh"

static_attestation_require_executable_script "tests/e2e/test_passive_watch_attestation_manifest.sh"

static_attestation_run_ruby - <<'RUBY'
def assert_ok(condition, message, check:, input_path: nil, expected: true, actual: condition)
  StaticAttestation.assert!(
    condition,
    message,
    check: check,
    input_path: input_path,
    expected: expected,
    actual: actual,
  )
end

def expect_equal(actual, expected, message, check:, input_path: nil)
  assert_ok(
    actual == expected,
    message,
    check: check,
    input_path: input_path,
    expected: expected,
    actual: actual,
  )
end

att_path = "docs/security/passive-watch-attestation.json"
contract_path = "crates/frankenterm-core/src/passive_watch_invariant.rs"
fuzz_path = "fuzz/fuzz_targets/passive_watch_invariant.rs"
audit_path = "docs/security/passive-watch-attestation.md"
corpus_dir = "fuzz/corpus/passive_watch_invariant"
script_path = "tests/e2e/test_passive_watch_attestation_manifest.sh"

StaticAttestation.require_direct_exec_script!(script_path, check: "passive_watch.verifier_shape")

att = StaticAttestation.read_json!(att_path, check: "passive_watch.attestation_json")
manifest = StaticAttestation.read_json!("docs/attestations/manifest.json", check: "passive_watch.manifest_json")
contract = StaticAttestation.read_text!(contract_path, check: "passive_watch.contract_source")
fuzz = StaticAttestation.read_text!(fuzz_path, check: "passive_watch.fuzz_source")
toml = StaticAttestation.read_text!("fuzz/Cargo.toml", check: "passive_watch.fuzz_toml")
audit = StaticAttestation.read_text!(audit_path, check: "passive_watch.audit_doc")
readme = StaticAttestation.read_text!("README.md", check: "passive_watch.readme")

expect_equal(att["category"], "security/passive-watch", "wrong category", check: "passive_watch.category", input_path: att_path)
expect_equal(att["schema_version"], "ft.security.passive_watch_attestation.v1", "wrong schema version", check: "passive_watch.schema_version", input_path: att_path)
expect_equal(att["produced_by_bead"], "ft-e87u6.11", "wrong recovery producer", check: "passive_watch.produced_by_bead", input_path: att_path)
expect_equal(att["source_bead"], "ft-x0666.1", "wrong source bead", check: "passive_watch.source_bead", input_path: att_path)

slot = manifest.fetch("slots").find { |s| s["category"] == "security/passive-watch" }
assert_ok(!slot.nil?, "manifest missing passive-watch slot", check: "passive_watch.manifest_slot", input_path: "docs/attestations/manifest.json")
expect_equal(slot["path"], att_path, "manifest path mismatch", check: "passive_watch.manifest_path", input_path: "docs/attestations/manifest.json")
expect_equal(slot["media_type"], "application/json", "manifest media type mismatch", check: "passive_watch.manifest_media_type", input_path: "docs/attestations/manifest.json")
expect_equal(slot["produced_by_bead"], "ft-x0666.1", "manifest producer mismatch", check: "passive_watch.manifest_producer", input_path: "docs/attestations/manifest.json")

headline = att.fetch("headline_rule")
expect_equal(headline.fetch("hard_invariants"), [
  "zero outbound mutating IPC per fuzz input",
  "zero non-capture storage writes per fuzz input",
], "headline hard invariants changed", check: "passive_watch.hard_invariants", input_path: att_path)
expect_equal(headline.fetch("allowed_actions"), %w[capture pattern_detection watch_metadata_write], "allowed actions changed", check: "passive_watch.allowed_actions", input_path: att_path)
expect_equal(headline.fetch("forbidden_actions"), %w[outbound_send outbound_spawn outbound_close non_capture_storage_write], "forbidden actions changed", check: "passive_watch.forbidden_actions", input_path: att_path)

basis = att.fetch("artifact_basis")
{
  "status" => "foundation_slice_attested",
  "contract_module" => contract_path,
  "fuzz_target" => fuzz_path,
  "seed_corpus_dir" => "#{corpus_dir}/",
  "audit_doc" => audit_path,
  "release_fuzz_status" => "not_run_in_recovery_artifact",
}.each do |key, value|
  expect_equal(basis[key], value, "artifact_basis #{key} mismatch", check: "passive_watch.artifact_basis.#{key}", input_path: att_path)
end
expect_equal(basis["release_fuzz_duration_seconds"], 0, "release fuzz duration must stay zero", check: "passive_watch.release_fuzz_duration", input_path: att_path)

health = att.fetch("passive_watch_health_contract")
expect_equal(health["type"], "PassiveWatchHealth", "health type mismatch", check: "passive_watch.health_type", input_path: att_path)
expect_equal(health["fields"], %w[
  iterations_total captures_total detections_total metadata_writes_total
  mutating_violations_total unclassified_other_total
], "health fields changed", check: "passive_watch.health_fields", input_path: att_path)
expect_equal(health["safe_condition"], "iterations_total > 0 && mutating_violations_total == 0", "health safe condition changed", check: "passive_watch.health_safe_condition", input_path: att_path)
expect_equal(health["cold_baseline_safe"], false, "cold baseline must be unsafe", check: "passive_watch.cold_baseline_safe", input_path: att_path)

clean = att.fetch("representative_clean_health_snapshot")
expect_equal(clean["iterations_total"], 1, "clean snapshot iterations mismatch", check: "passive_watch.clean.iterations", input_path: att_path)
expect_equal(clean["mutating_violations_total"], 0, "clean snapshot mutation count mismatch", check: "passive_watch.clean.mutations", input_path: att_path)
expect_equal(clean["unclassified_other_total"], 0, "clean snapshot other count mismatch", check: "passive_watch.clean.other", input_path: att_path)
expect_equal(clean["is_safe"], true, "clean snapshot must be safe", check: "passive_watch.clean.is_safe", input_path: att_path)

seed_section = att.fetch("seed_corpus")
seed_corpus = StaticAttestation.require_seed_corpus!(corpus_dir, seeds: seed_section.fetch("seeds"), check: "passive_watch.seed_corpus")
expect_equal(seed_section["seed_count"], 10, "seed count changed", check: "passive_watch.attested_seed_count", input_path: att_path)
expect_equal(seed_section["total_bytes"], 14_516, "total seed bytes changed", check: "passive_watch.attested_seed_bytes", input_path: att_path)
expect_equal(seed_corpus[:seed_count], seed_section["seed_count"], "seed count does not match corpus files", check: "passive_watch.live_seed_count", input_path: corpus_dir)
expect_equal(seed_corpus[:total_bytes], seed_section["total_bytes"], "seed byte total does not match corpus files", check: "passive_watch.live_seed_bytes", input_path: corpus_dir)
expect_equal(
  seed_section["kinds"].sort,
  seed_section.fetch("seeds").map { |s| s.fetch("kind") }.uniq.sort,
  "seed kinds are not fully covered",
  check: "passive_watch.seed_kinds",
  input_path: att_path,
)

fuzz_contract = att.fetch("fuzz_target_contract")
expect_equal(fuzz_contract["target_name"], "passive_watch_invariant", "fuzz target name changed", check: "passive_watch.fuzz_target_name", input_path: att_path)
expect_equal(fuzz_contract["input_cap_bytes"], 262_144, "fuzz input cap changed", check: "passive_watch.fuzz_input_cap", input_path: att_path)
expect_equal(fuzz_contract["parser_surface"], "scan_pipeline::quick_scan", "parser surface changed", check: "passive_watch.parser_surface", input_path: att_path)
StaticAttestation.require_terms!(
  fuzz_contract.fetch("assertions").join("\n"),
  StaticAttestation.expected_strings(
    "check_invariants(&obs) returns empty",
    "PassiveWatchHealth::is_safe() is true after folding the observation",
  ),
  source: "#{att_path}:fuzz_target_contract.assertions",
  check: "passive_watch.fuzz_assertions",
)

unit = att.fetch("unit_test_contract")
assert_ok(unit["test_count"] >= 15, "unit test count regressed", check: "passive_watch.unit_test_count_floor", input_path: att_path, expected: ">= 15", actual: unit["test_count"])
StaticAttestation.require_terms!(
  unit.fetch("covered_shapes").join("\n"),
  StaticAttestation.expected_strings("corpus slugs match the documented values"),
  source: "#{att_path}:unit_test_contract.covered_shapes",
  check: "passive_watch.unit_covered_shapes",
)
contract_test_count = contract.scan(/^\s*#\[test\]\s*$/).length
assert_ok(contract_test_count >= unit["test_count"], "contract has fewer tests than attested", check: "passive_watch.contract_test_count", input_path: contract_path, expected: unit["test_count"], actual: contract_test_count)

validation = att.fetch("targeted_validation")
StaticAttestation.require_terms!(
  validation.fetch("command"),
  StaticAttestation.expected_strings("rch exec", "passive_watch_invariant::tests"),
  source: "#{att_path}:targeted_validation.command",
  check: "passive_watch.targeted_validation_command",
)
expect_equal(validation["status"], "blocked", "recovery artifact must not claim green validation", check: "passive_watch.validation_status", input_path: att_path)
expect_equal(validation["error_code"], "RCH-E104", "targeted validation blocker changed", check: "passive_watch.validation_error_code", input_path: att_path)

StaticAttestation.require_source_documents!(att.fetch("source_documents"), check: "passive_watch.source_documents")

StaticAttestation.require_terms!(
  contract,
  StaticAttestation.expected_strings(
    "adversarial_seed_catalog",
    "PassiveWatchHealth",
    "check_invariants",
    "NoOutboundMutatingIpc",
    "NoNonCaptureStorageWrite",
    "self.iterations_total > 0 && self.mutating_violations_total == 0",
    "exhaustive `WatchAction::is_mutating` match",
  ),
  source: contract_path,
  check: "passive_watch.contract_terms",
)
StaticAttestation.require_terms!(
  fuzz,
  StaticAttestation.expected_strings(
    "quick_scan",
    "check_invariants",
    "fold_observation",
    "PassiveWatchHealth::baseline",
    "health.is_safe",
    "256 * 1024",
  ),
  source: fuzz_path,
  check: "passive_watch.fuzz_terms",
)

bin = toml[/\[\[bin\]\]\s*name = "passive_watch_invariant".*?(?=\n\[\[bin\]\]|\z)/m]
assert_ok(!bin.nil?, "fuzz Cargo.toml missing passive_watch_invariant bin", check: "passive_watch.fuzz_bin", input_path: "fuzz/Cargo.toml")
StaticAttestation.require_terms!(
  bin,
  StaticAttestation.expected_strings(
    "path = \"fuzz_targets/passive_watch_invariant.rs\"",
    "required-features = [\"core-fuzz-targets\"]",
  ),
  source: "fuzz/Cargo.toml:passive_watch_invariant",
  check: "passive_watch.fuzz_bin_terms",
)

StaticAttestation.require_terms!(
  audit,
  StaticAttestation.expected_strings(
    "Zero outbound mutating IPC",
    "Zero non-capture storage",
    "fuzz/corpus/passive_watch_invariant/",
    "cargo-fuzz target",
    "docs/security/passive-watch-attestation.json",
    "docs/attestations/manifest.json",
    "`is_safe()` returns `iterations_total > 0 && mutating_violations_total == 0`",
    script_path,
    "tests/scripts/static_attestation_helpers.rb",
  ),
  source: audit_path,
  check: "passive_watch.audit_terms",
)
{
  "authored once that schema lands" => "audit doc still says attestation entry is future-only",
  "depends on `ft-syqcz.1`" => "audit doc still says JSON artifact depends on retired schema bead",
  "`is_safe()` returns `mutating_violations_total == 0`" => "audit doc still claims cold baseline is safe",
  "every_action_kind_has_a_classification" => "audit doc names nonexistent action-classification test",
}.each do |term, message|
  assert_ok(!audit.include?(term), message, check: "passive_watch.audit_absence", input_path: audit_path, expected: "absent", actual: audit.include?(term) ? "present" : "absent")
end
assert_ok(!contract.include?("every_action_kind_has_a_classification"), "contract comments name nonexistent action-classification test", check: "passive_watch.contract_absence", input_path: contract_path, expected: "absent", actual: contract.include?("every_action_kind_has_a_classification") ? "present" : "absent")

live_e2e = Dir.glob("tests/e2e/**/*.sh").length
StaticAttestation.require_terms!(
  readme,
  StaticAttestation.expected_strings(
    "<!--count:e2e_scripts-->#{live_e2e}<!--/count-->",
    "# #{live_e2e} shell E2E scripts",
  ),
  source: "README.md",
  check: "passive_watch.readme_e2e_count",
)

puts "passive-watch attestation verifier: passed (#{seed_section["seed_count"]} seeds, #{seed_section["total_bytes"]} bytes, #{live_e2e} E2E scripts)"
RUBY
