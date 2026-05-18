#!/usr/bin/env bash
# Static verifier for the adversarial contract-fuzz manifest and CI matrix.
set -euo pipefail

ROOT="${FRANKENTERM_REPO_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"
export FRANKENTERM_REPO_ROOT="$ROOT"
cd "$ROOT"

source "tests/scripts/static_attestation_helpers.sh"

static_attestation_require_executable_script "tests/e2e/test_adversarial_contract_fuzz_manifest.sh"

static_attestation_run_ruby - <<'RUBY'
require "yaml"

MANIFEST = "docs/security/adversarial-contract-fuzz.json"
WORKFLOW = ".github/workflows/adversarial-contract-fuzz.yml"
FUZZ_CARGO = "fuzz/Cargo.toml"

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

def checked_repo_relative_path(path, field, check:)
  StaticAttestation.repo_relative_path!(path, field: field, check: check)
end

def normalize_dir(path)
  path.sub(%r{/+\z}, "")
end

def require_directory!(path, check:)
  candidate = StaticAttestation.repo_path(path)
  actual = if File.directory?(candidate)
    "directory"
  elsif File.exist?(candidate)
    "not_directory"
  else
    "missing"
  end
  assert_ok(
    File.directory?(candidate),
    "missing directory: #{path}",
    check: check,
    input_path: path,
    expected: "directory",
    actual: actual,
  )
  candidate
end

def parse_fuzz_bins(toml)
  bins = {}
  toml.split(/^\[\[bin\]\]\s*$/).drop(1).each do |section|
    name = section[/^\s*name\s*=\s*"([^"]+)"/, 1]
    path = section[/^\s*path\s*=\s*"([^"]+)"/, 1]
    next if name.nil? || path.nil?

    bins[name] = {
      path: path,
      section: section,
    }
  end
  bins
end

def validate_privacy!(privacy, manifest_path)
  %w[raw_context_content_stored raw_pane_content_stored raw_content_allowed].each do |field|
    expect_equal(
      privacy[field],
      false,
      "privacy_invariant.#{field} must be false",
      check: "adversarial.privacy.#{field}",
      input_path: manifest_path,
    )
  end
  assertion = privacy["assertion"]
  assert_ok(
    assertion.is_a?(String) && assertion.include?("raw_"),
    "privacy assertion is empty",
    check: "adversarial.privacy.assertion",
    input_path: manifest_path,
    expected: "string containing raw_",
    actual: assertion,
  )
end

def validate_target!(target, fuzz_bins)
  family = target.fetch("family")
  cargo_target = target.fetch("cargo_fuzz_target")
  path = checked_repo_relative_path(
    target.fetch("path"),
    "#{cargo_target}.path",
    check: "adversarial.target.path",
  )
  schema = checked_repo_relative_path(
    target.fetch("schema"),
    "#{cargo_target}.schema",
    check: "adversarial.target.schema",
  )
  seed_corpus = normalize_dir(checked_repo_relative_path(
    target.fetch("seed_corpus"),
    "#{cargo_target}.seed_corpus",
    check: "adversarial.target.seed_corpus",
  ))
  entry_points = target.fetch("production_entry_points")

  assert_ok(
    family.match?(/\A[a-z][a-z0-9_]*\z/),
    "#{cargo_target} family is malformed",
    check: "adversarial.target.family",
    input_path: MANIFEST,
    expected: "lower_snake_case",
    actual: family,
  )
  expect_equal(
    cargo_target,
    "contract_#{family}",
    "#{cargo_target} must use contract_ family prefix",
    check: "adversarial.target.name",
    input_path: MANIFEST,
  )

  StaticAttestation.require_file!(path, check: "adversarial.target.file")
  StaticAttestation.require_file!(schema, check: "adversarial.target.schema_file")
  seed_path = require_directory!(seed_corpus, check: "adversarial.target.seed_dir")
  assert_ok(
    !Dir.children(seed_path.to_s).empty?,
    "#{cargo_target} seed corpus is empty",
    check: "adversarial.target.seed_dir_nonempty",
    input_path: seed_corpus,
    expected: "non-empty",
    actual: Dir.children(seed_path.to_s).length,
  )
  assert_ok(
    entry_points.is_a?(Array) && entry_points.any?,
    "#{cargo_target} production entry points empty",
    check: "adversarial.target.entry_points",
    input_path: MANIFEST,
    expected: "non-empty array",
    actual: entry_points,
  )
  assert_ok(
    entry_points.all? { |entry| entry.is_a?(String) && entry.include?("::") },
    "#{cargo_target} production entry point is malformed",
    check: "adversarial.target.entry_point_shape",
    input_path: MANIFEST,
    expected: "Rust path strings",
    actual: entry_points,
  )

  cargo_bin = fuzz_bins[cargo_target]
  assert_ok(
    !cargo_bin.nil?,
    "#{cargo_target} missing [[bin]] in #{FUZZ_CARGO}",
    check: "adversarial.target.cargo_bin",
    input_path: FUZZ_CARGO,
    expected: cargo_target,
    actual: fuzz_bins.keys,
  )
  expect_equal(
    File.join("fuzz", cargo_bin.fetch(:path)),
    path,
    "#{cargo_target} Cargo.toml path mismatch",
    check: "adversarial.target.cargo_path",
    input_path: FUZZ_CARGO,
  )
  StaticAttestation.require_terms!(
    cargo_bin.fetch(:section),
    StaticAttestation.expected_strings('"core-fuzz-targets"'),
    source: "#{FUZZ_CARGO}:#{cargo_target}",
    check: "adversarial.target.cargo_features",
  )

  source = StaticAttestation.read_text!(path, check: "adversarial.target.source")
  StaticAttestation.require_terms!(
    source,
    StaticAttestation.expected_strings(
      "fuzz_target!",
      "compile_schema",
      schema,
      "assert_no_raw_content_flags",
      "../contract_fuzz_common.rs",
    ),
    source: path,
    check: "adversarial.target.source_terms",
  )

  schema_doc = StaticAttestation.read_json!(schema, check: "adversarial.target.schema_json")
  assert_ok(
    schema_doc.fetch("$id").end_with?("/#{File.basename(schema)}"),
    "#{cargo_target} schema id must end with schema file",
    check: "adversarial.target.schema_id",
    input_path: schema,
    expected: File.basename(schema),
    actual: schema_doc.fetch("$id"),
  )

  [cargo_target, seed_corpus]
end

def validate_local_proof!(local_proof, target_names, manifest_path)
  expect_equal(
    local_proof["cargo_execution_policy"],
    "run through rch only",
    "local_proof must require RCH for Cargo",
    check: "adversarial.local_proof.cargo_policy",
    input_path: manifest_path,
  )
  commands = local_proof.fetch("commands")
  assert_ok(
    commands.is_a?(Array),
    "local_proof.commands must be an array",
    check: "adversarial.local_proof.commands_shape",
    input_path: manifest_path,
    expected: "array",
    actual: commands,
  )
  rch_command = commands.map { |row| row["command"] }.find { |command| command.to_s.include?("rch exec") }
  assert_ok(
    !rch_command.nil?,
    "local_proof missing RCH compile command",
    check: "adversarial.local_proof.rch_command",
    input_path: manifest_path,
    expected: "rch exec command",
    actual: commands,
  )
  target_names.each do |target_name|
    StaticAttestation.require_terms!(
      rch_command,
      StaticAttestation.expected_strings("--bin #{target_name}"),
      source: "#{manifest_path}:local_proof.commands",
      check: "adversarial.local_proof.rch_targets",
    )
  end
end

StaticAttestation.require_direct_exec_script!(
  "tests/e2e/test_adversarial_contract_fuzz_manifest.sh",
  check: "adversarial.verifier_shape",
)

manifest = StaticAttestation.read_json!(MANIFEST, check: "adversarial.manifest_json")
workflow_text = StaticAttestation.read_text!(WORKFLOW, check: "adversarial.workflow_text")
workflow = YAML.safe_load(workflow_text, aliases: true)
fuzz_toml = StaticAttestation.read_text!(FUZZ_CARGO, check: "adversarial.fuzz_cargo")
fuzz_bins = parse_fuzz_bins(fuzz_toml)

expect_equal(manifest["schema_version"], 1, "schema_version must be 1", check: "adversarial.schema_version", input_path: MANIFEST)
expect_equal(manifest["contract_id"], "security.adversarial_contract_fuzz.v1", "unexpected contract_id", check: "adversarial.contract_id", input_path: MANIFEST)
expect_equal(manifest["status"], "harness_and_ci_wired", "status must be harness_and_ci_wired", check: "adversarial.status", input_path: MANIFEST)
expect_equal(manifest["release_bundle_artifact"], "security/adversarial-contract-fuzz.json", "release_bundle_artifact drift", check: "adversarial.release_bundle", input_path: MANIFEST)

privacy = manifest.fetch("privacy_invariant")
validate_privacy!(privacy, MANIFEST)
StaticAttestation.expect_failure!("raw-content privacy negative fixture", check: "adversarial.negative.privacy") do
  validate_privacy!(privacy.merge("raw_content_allowed" => true), MANIFEST)
end

targets = manifest.fetch("targets")
assert_ok(
  targets.length >= 5,
  "must retain at least five contract fuzz targets",
  check: "adversarial.targets.count",
  input_path: MANIFEST,
  expected: ">= 5",
  actual: targets.length,
)

families = targets.map { |target| target["family"] }
target_names = targets.map { |target| target["cargo_fuzz_target"] }
expect_equal(families.uniq.length, families.length, "duplicate target families", check: "adversarial.targets.unique_families", input_path: MANIFEST)
expect_equal(target_names.uniq.length, target_names.length, "duplicate cargo_fuzz_target names", check: "adversarial.targets.unique_names", input_path: MANIFEST)

manifest_pairs = []
targets.each do |target|
  manifest_pairs << validate_target!(target, fuzz_bins)
end
StaticAttestation.expect_failure!("absolute target path negative fixture", check: "adversarial.negative.target_path") do
  validate_target!(targets.first.merge("path" => "/tmp/contract.rs"), fuzz_bins)
end

ci = manifest.fetch("ci")
expect_equal(ci["workflow"], WORKFLOW, "ci.workflow must point at workflow file", check: "adversarial.ci.workflow", input_path: MANIFEST)
expect_equal(ci["pull_request_seconds_per_target"], 1800, "PR fuzz seconds must remain 1800", check: "adversarial.ci.pr_seconds", input_path: MANIFEST)
expect_equal(ci["release_seconds_per_target"], 86_400, "release fuzz seconds must remain 86400", check: "adversarial.ci.release_seconds", input_path: MANIFEST)

workflow_events = workflow["on"] || workflow[true]
assert_ok(
  workflow_events.is_a?(Hash),
  "workflow event block missing",
  check: "adversarial.workflow.events",
  input_path: WORKFLOW,
  expected: "hash",
  actual: workflow_events,
)
paths = workflow_events.fetch("pull_request").fetch("paths")
[
  "crates/frankenterm-core/**",
  "docs/json-schema/**",
  MANIFEST,
  "fuzz/**",
  WORKFLOW,
].each do |path|
  assert_ok(
    paths.include?(path),
    "workflow pull_request paths missing #{path}",
    check: "adversarial.workflow.pr_paths",
    input_path: WORKFLOW,
    expected: path,
    actual: paths,
  )
end

jobs = workflow.fetch("jobs")
{
  "pr-fuzz" => 1800,
  "release-fuzz" => 86_400,
}.each do |job_name, expected_seconds|
  job = jobs.fetch(job_name)
  matrix = job.fetch("strategy").fetch("matrix").fetch("include")
  matrix_pairs = matrix.map { |row| [row.fetch("target"), normalize_dir(row.fetch("corpus"))] }.sort
  expect_equal(
    matrix_pairs,
    manifest_pairs.sort,
    "#{job_name} matrix does not match manifest targets",
    check: "adversarial.workflow.#{job_name}.matrix",
    input_path: WORKFLOW,
  )
  StaticAttestation.require_terms!(
    workflow_text,
    StaticAttestation.expected_strings("-max_total_time=#{expected_seconds}"),
    source: WORKFLOW,
    check: "adversarial.workflow.#{job_name}.timeout",
  )
end

local_proof = manifest.fetch("local_proof")
validate_local_proof!(local_proof, target_names, MANIFEST)
StaticAttestation.expect_failure!("local proof policy negative fixture", check: "adversarial.negative.local_proof_policy") do
  validate_local_proof!(local_proof.merge("cargo_execution_policy" => "run locally"), target_names, MANIFEST)
end

puts "adversarial contract-fuzz manifest: static-only verifier passed (#{targets.length} targets; RCH compile proof remains separately required)"
RUBY
