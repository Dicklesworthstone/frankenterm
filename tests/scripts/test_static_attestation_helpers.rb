#!/usr/bin/env ruby
# frozen_string_literal: true

require "json"
require "open3"
require "shellwords"
require "stringio"

root = File.expand_path("../..", __dir__)
ENV["FRANKENTERM_REPO_ROOT"] = root

require_relative "static_attestation_helpers"

def check(name)
  yield
  puts "ok - #{name}"
rescue StandardError => error
  warn "not ok - #{name}: #{error.class}: #{error.message}"
  raise
end

StaticAttestation.configure(log_io: StringIO.new, log_enabled: true)

run_shell_helper = lambda do |body|
  script = <<~BASH
    set -euo pipefail
    source tests/scripts/static_attestation_helpers.sh
    #{body}
  BASH
  Open3.capture3({ "FRANKENTERM_REPO_ROOT" => root }, "bash", "-c", script, chdir: root)
end

check("repo-relative path guard rejects absolute, parent traversal, empty, and NUL paths") do
  StaticAttestation.repo_relative_path!("docs/security/passive-watch-attestation.json")
  StaticAttestation.expect_failure!("absolute path guard", check: "test.negative.absolute_path") do
    StaticAttestation.repo_relative_path!("/tmp/nope")
  end
  StaticAttestation.expect_failure!("parent traversal guard", check: "test.negative.parent_traversal") do
    StaticAttestation.repo_relative_path!("docs/../secret")
  end
  StaticAttestation.expect_failure!("empty path guard", check: "test.negative.empty_path") do
    StaticAttestation.repo_relative_path!("")
  end
  StaticAttestation.expect_failure!("NUL path guard", check: "test.negative.nul_path") do
    StaticAttestation.repo_relative_path!("docs/security/passive-watch-attestation.json\0suffix")
  end
end

check("multi-word expected strings remain whole strings") do
  terms = StaticAttestation.expected_strings(
    "Zero outbound mutating IPC",
    "Zero non-capture storage",
  )
  raise "expected two whole phrases" unless terms == ["Zero outbound mutating IPC", "Zero non-capture storage"]
  StaticAttestation.expect_failure!("split words do not satisfy whole phrase", check: "test.negative.split_phrase") do
    StaticAttestation.require_terms!(
      "Zero\noutbound\nmutating\nIPC\nZero\nnon-capture\nstorage\n",
      ["Zero outbound mutating IPC"],
      source: "inline-regression",
    )
  end
end

check("structured logs contain check input expected actual status and reason") do
  log_io = StringIO.new
  StaticAttestation.configure(log_io: log_io, log_enabled: true)
  StaticAttestation.require_terms!(
    "alpha beta",
    ["alpha beta"],
    source: "inline-log-source",
    check: "log_shape",
  )
  record = JSON.parse(log_io.string.lines.last)
  %w[check input_path expected actual status].each do |field|
    raise "missing log field #{field}" unless record.key?(field)
  end
  raise "unexpected check" unless record["check"] == "log_shape"
  raise "unexpected expected" unless record["expected"] == "alpha beta"
  raise "unexpected status" unless record["status"] == "pass"
end

check("negative expectation helper records expected failures") do
  log_io = StringIO.new
  StaticAttestation.configure(log_io: log_io, log_enabled: true)
  error = StaticAttestation.expect_failure!("missing phrase fixture", check: "test.expect_failure") do
    StaticAttestation.require_terms!(
      "alpha beta",
      ["gamma delta"],
      source: "inline-negative-fixture",
      check: "test.expect_failure.inner",
    )
  end
  raise "wrong error type" unless error.is_a?(StaticAttestation::Failure)

  record = JSON.parse(log_io.string.lines.last)
  raise "expected pass record" unless record["status"] == "pass"
  raise "wrong check" unless record["check"] == "test.expect_failure"
  raise "missing failure reason" unless record["failure_reason"].to_s.include?("gamma delta")
end

check("structured failure logs redact sensitive reason text") do
  log_io = StringIO.new
  StaticAttestation.configure(log_io: log_io, log_enabled: true)
  StaticAttestation.expect_failure!("secret failure reason", check: "test.secret_reason") do
    StaticAttestation.fail!(
      "api key leaked in synthetic fixture",
      check: "test.secret_reason.inner",
      input_path: "inline-secret-fixture",
      expected: "no secret",
      actual: "sk-ant-secret123456789012345",
      reason: "token sk-ant-secret123456789012345 must not reach logs",
    )
  end

  records = log_io.string.lines.map { |line| JSON.parse(line) }
  inner_record = records.find { |record| record["check"] == "test.secret_reason.inner" }
  raise "missing inner failure record" unless inner_record
  serialized = JSON.generate(inner_record)
  raise "secret leaked into structured log" if serialized.include?("sk-ant-secret")
  reason = inner_record.fetch("failure_reason")
  raise "failure reason was not redacted" unless reason.fetch("redacted").start_with?("sha256:")
end

check("passive-watch source documents and multi-word audit phrases are preserved") do
  attestation = StaticAttestation.read_json!("docs/security/passive-watch-attestation.json")
  StaticAttestation.require_source_documents!(attestation.fetch("source_documents"))
  StaticAttestation.require_file_terms!(
    "docs/security/passive-watch-attestation.md",
    [
      "Zero outbound mutating IPC",
      "Zero non-capture storage",
      "cargo-fuzz target",
      "docs/security/passive-watch-attestation.json",
    ],
  )
end

check("passive-watch seed corpus names and byte sizes match the attestation") do
  attestation = StaticAttestation.read_json!("docs/security/passive-watch-attestation.json")
  seed_section = attestation.fetch("seed_corpus")
  summary = StaticAttestation.require_seed_corpus!(
    "fuzz/corpus/passive_watch_invariant",
    seeds: seed_section.fetch("seeds"),
  )
  raise "seed count drifted" unless summary.fetch(:seed_count) == seed_section.fetch("seed_count")
  raise "total bytes drifted" unless summary.fetch(:total_bytes) == seed_section.fetch("total_bytes")
end

check("direct-exec script helper pins shebang executable bit and strict mode") do
  StaticAttestation.require_direct_exec_script!("tests/e2e/test_passive_watch_attestation_manifest.sh")
end

check("shell helper exposes the expected sourceable API") do
  shell_helper = StaticAttestation.read_text!("tests/scripts/static_attestation_helpers.sh")
  StaticAttestation.require_terms!(
    shell_helper,
    [
      "static_attestation_require_command",
      "static_attestation_require_file",
      "static_attestation_require_repo_relative_path",
      "static_attestation_require_executable_script",
      "static_attestation_run_ruby",
    ],
    source: "tests/scripts/static_attestation_helpers.sh",
  )
end

check("shell helper path guards fail cleanly under strict mode") do
  [
    ["static_attestation_require_command", "", "command name is empty"],
    ["static_attestation_require_repo_relative_path", "", "path is empty"],
    ["static_attestation_require_repo_relative_path", "/tmp/nope", "absolute path is forbidden"],
    ["static_attestation_require_repo_relative_path", "docs/../secret", "parent traversal is forbidden"],
    ["static_attestation_require_file", "", "path is empty"],
    ["static_attestation_require_executable_script", "", "path is empty"],
  ].each do |function_name, argument, expected_error|
    _stdout, stderr, status = run_shell_helper.call("#{function_name} #{argument.shellescape}")
    raise "#{function_name} unexpectedly passed" if status.success?
    raise "#{function_name} raised Bash unbound-variable noise" if stderr.include?("unbound variable")
    raise "#{function_name} did not report #{expected_error.inspect}: #{stderr}" unless stderr.include?(expected_error)
  end
end

puts "static-attestation helpers: self-test passed"
