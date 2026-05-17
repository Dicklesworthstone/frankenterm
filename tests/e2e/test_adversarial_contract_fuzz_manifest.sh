#!/usr/bin/env bash
# Static verifier for the adversarial contract-fuzz manifest and CI matrix.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

MANIFEST="docs/security/adversarial-contract-fuzz.json"
WORKFLOW=".github/workflows/adversarial-contract-fuzz.yml"
FUZZ_CARGO="fuzz/Cargo.toml"

fail() {
  printf 'adversarial contract-fuzz manifest: %s\n' "$*" >&2
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

require_file "${MANIFEST}"
require_file "${WORKFLOW}"
require_file "${FUZZ_CARGO}"

mapfile -t referenced_schemas < <(jq -r '.targets[].schema' "${MANIFEST}" | sort -u)
for schema in "${referenced_schemas[@]}"; do
  require_file "${schema}"
done

jq empty "${MANIFEST}" "${referenced_schemas[@]}"

ruby <<'RUBY'
require "json"
require "yaml"

MANIFEST = "docs/security/adversarial-contract-fuzz.json"
WORKFLOW = ".github/workflows/adversarial-contract-fuzz.yml"
FUZZ_CARGO = "fuzz/Cargo.toml"

def fail!(message)
  warn "adversarial contract-fuzz manifest: #{message}"
  exit 1
end

def repo_relative_path!(path, field)
  fail!("#{field} is empty") unless path.is_a?(String) && !path.empty?
  fail!("#{field} must be repo-relative: #{path}") if path.start_with?("/")
  fail!("#{field} must not contain parent traversal: #{path}") if path.split("/").include?("..")
  path
end

def normalize_dir(path)
  path.sub(%r{/+\z}, "")
end

def parse_fuzz_bins
  bins = {}
  File.read(FUZZ_CARGO).split(/^\[\[bin\]\]\s*$/).drop(1).each do |section|
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

manifest = JSON.parse(File.read(MANIFEST))
workflow = YAML.load_file(WORKFLOW)
workflow_text = File.read(WORKFLOW)
fuzz_bins = parse_fuzz_bins

fail!("schema_version must be 1") unless manifest["schema_version"] == 1
fail!("unexpected contract_id") unless manifest["contract_id"] == "security.adversarial_contract_fuzz.v1"
fail!("status must be harness_and_ci_wired") unless manifest["status"] == "harness_and_ci_wired"
fail!("release_bundle_artifact drift") unless manifest["release_bundle_artifact"] == "security/adversarial-contract-fuzz.json"

privacy = manifest.fetch("privacy_invariant")
%w[raw_context_content_stored raw_pane_content_stored raw_content_allowed].each do |field|
  fail!("privacy_invariant.#{field} must be false") unless privacy[field] == false
end
fail!("privacy assertion is empty") unless privacy["assertion"].is_a?(String) && privacy["assertion"].include?("raw_")

targets = manifest.fetch("targets")
fail!("must retain at least five contract fuzz targets") unless targets.length >= 5

families = targets.map { |target| target["family"] }
target_names = targets.map { |target| target["cargo_fuzz_target"] }
fail!("duplicate target families") unless families.uniq.length == families.length
fail!("duplicate cargo_fuzz_target names") unless target_names.uniq.length == target_names.length

manifest_pairs = []
targets.each do |target|
  family = target.fetch("family")
  cargo_target = target.fetch("cargo_fuzz_target")
  path = repo_relative_path!(target.fetch("path"), "#{cargo_target}.path")
  schema = repo_relative_path!(target.fetch("schema"), "#{cargo_target}.schema")
  seed_corpus = normalize_dir(repo_relative_path!(target.fetch("seed_corpus"), "#{cargo_target}.seed_corpus"))
  entry_points = target.fetch("production_entry_points")

  fail!("#{cargo_target} family is malformed") unless family.match?(/\A[a-z][a-z0-9_]*\z/)
  fail!("#{cargo_target} must use contract_ family prefix") unless cargo_target == "contract_#{family}"
  fail!("#{cargo_target} target file missing: #{path}") unless File.file?(path)
  fail!("#{cargo_target} schema missing: #{schema}") unless File.file?(schema)
  fail!("#{cargo_target} seed corpus missing: #{seed_corpus}") unless File.directory?(seed_corpus)
  fail!("#{cargo_target} seed corpus is empty") if Dir.children(seed_corpus).empty?
  fail!("#{cargo_target} production entry points empty") unless entry_points.is_a?(Array) && entry_points.any?
  fail!("#{cargo_target} production entry point is malformed") unless entry_points.all? { |entry| entry.is_a?(String) && entry.include?("::") }

  cargo_bin = fuzz_bins[cargo_target]
  fail!("#{cargo_target} missing [[bin]] in #{FUZZ_CARGO}") if cargo_bin.nil?
  fail!("#{cargo_target} Cargo.toml path mismatch") unless File.join("fuzz", cargo_bin[:path]) == path
  fail!("#{cargo_target} must require core-fuzz-targets") unless cargo_bin[:section].include?('"core-fuzz-targets"')

  source = File.read(path)
  fail!("#{cargo_target} target must use libfuzzer fuzz_target!") unless source.include?("fuzz_target!")
  fail!("#{cargo_target} target must compile its declared schema") unless source.include?("compile_schema") && source.include?(schema)
  fail!("#{cargo_target} target must assert no raw content flags") unless source.include?("assert_no_raw_content_flags")
  fail!("#{cargo_target} target must share contract_fuzz_common") unless source.include?("../contract_fuzz_common.rs")

  schema_doc = JSON.parse(File.read(schema))
  fail!("#{cargo_target} schema id must end with schema file") unless schema_doc.fetch("$id").end_with?("/#{File.basename(schema)}")

  manifest_pairs << [cargo_target, seed_corpus]
end

ci = manifest.fetch("ci")
fail!("ci.workflow must point at workflow file") unless ci["workflow"] == WORKFLOW
fail!("PR fuzz seconds must remain 1800") unless ci["pull_request_seconds_per_target"] == 1800
fail!("release fuzz seconds must remain 86400") unless ci["release_seconds_per_target"] == 86_400

workflow_events = workflow["on"] || workflow[true]
fail!("workflow event block missing") unless workflow_events.is_a?(Hash)
paths = workflow_events.fetch("pull_request").fetch("paths")
[
  "crates/frankenterm-core/**",
  "docs/json-schema/**",
  MANIFEST,
  "fuzz/**",
  WORKFLOW,
].each do |path|
  fail!("workflow pull_request paths missing #{path}") unless paths.include?(path)
end

jobs = workflow.fetch("jobs")
{
  "pr-fuzz" => 1800,
  "release-fuzz" => 86_400,
}.each do |job_name, expected_seconds|
  job = jobs.fetch(job_name)
  matrix = job.fetch("strategy").fetch("matrix").fetch("include")
  matrix_pairs = matrix.map { |row| [row.fetch("target"), normalize_dir(row.fetch("corpus"))] }.sort
  fail!("#{job_name} matrix does not match manifest targets") unless matrix_pairs == manifest_pairs.sort
  fail!("#{job_name} timeout command missing max_total_time=#{expected_seconds}") unless workflow_text.include?("-max_total_time=#{expected_seconds}")
end

local_proof = manifest.fetch("local_proof")
fail!("local_proof must require RCH for Cargo") unless local_proof["cargo_execution_policy"] == "run through rch only"
rch_command = local_proof.fetch("commands").map { |row| row["command"] }.find { |command| command.to_s.include?("rch exec") }
fail!("local_proof missing RCH compile command") if rch_command.nil?
target_names.each do |target_name|
  fail!("local_proof RCH command missing #{target_name}") unless rch_command.include?("--bin #{target_name}")
end

puts "adversarial contract-fuzz manifest: static verifier passed (#{targets.length} targets)"
RUBY
