#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="ft-gv7y2.4"
SCENARIO_ID="e2e_shell_script_contract"
RUN_ID="${FT_GV7Y2_4_RUN_ID:-$(date -u +"%Y%m%dT%H%M%SZ")}"
ARTIFACT_DIR="${FT_GV7Y2_4_ARTIFACT_DIR:-${ROOT_DIR}/tests/e2e/artifacts/static-proof/${BEAD_ID}/${SCENARIO_ID}/${RUN_ID}}"
STRUCTURED_LOG="${ARTIFACT_DIR}/static-attestation.jsonl"
SUMMARY_FILE="${ARTIFACT_DIR}/summary.json"
COMMANDS_FILE="${ARTIFACT_DIR}/commands.txt"

mkdir -p "${ARTIFACT_DIR}"
: >"${STRUCTURED_LOG}"
: >"${COMMANDS_FILE}"

record_command() {
    printf '%s\n' "$*" >>"${COMMANDS_FILE}"
}

record_command "bash -n tests/e2e/test_e2e_shell_script_contract.sh"
bash -n "${BASH_SOURCE[0]}"

# shellcheck source=tests/scripts/static_attestation_helpers.sh
source "${ROOT_DIR}/tests/scripts/static_attestation_helpers.sh"

record_command "ruby static E2E shell script contract verifier"
static_attestation_run_ruby - "${STRUCTURED_LOG}" "${SUMMARY_FILE}" "${ROOT_DIR}" <<'RUBY'
structured_log_path = ARGV.fetch(0)
summary_path = ARGV.fetch(1)
root = ARGV.fetch(2)

log_io = File.open(structured_log_path, "a")
StaticAttestation.configure(log_io: log_io, log_enabled: true)

begin
  git_ls_files = IO.popen(["git", "ls-files", "tests/e2e"], &:read)
  StaticAttestation.assert!(
    $?.success?,
    "failed to enumerate tracked E2E scripts",
    check: "e2e_script_contract.git_ls_files",
    input_path: "tests/e2e",
    expected: "success",
    actual: $?.exitstatus,
  )
  scripts = git_ls_files.lines.map(&:chomp).select { |path| path.end_with?(".sh") }.sort
  readme = StaticAttestation.read_text!("README.md", check: "e2e_script_contract.readme")

  exceptions = {
    "tests/e2e/lib_rch_guards.sh" => {
      "direct_exec" => false,
      "strict_mode" => false,
      "reason" => "source-only RCH guard library; sourced by executable harnesses",
    },
    "tests/e2e/test_frankensearch_integration.sh" => {
      "strict_mode" => false,
      "reason" => "legacy aggregate-failure harness keeps set -u plus pipefail while tallying assertion and infrastructure failures",
    },
    "tests/e2e/test_search_load.sh" => {
      "strict_mode" => false,
      "reason" => "legacy load harness keeps set -u plus pipefail while accumulating per-query failure metrics",
    },
    "tests/e2e/test_search_regression.sh" => {
      "strict_mode" => false,
      "reason" => "legacy regression harness keeps set -u plus pipefail while aggregating query assertions",
    },
  }

  StaticAttestation.assert!(
    readme.include?("<!--count:e2e_scripts-->#{scripts.length}<!--/count-->"),
    "README stamped E2E script count is stale",
    check: "e2e_script_contract.readme_marker_count",
    input_path: "README.md",
    expected: scripts.length,
    actual: readme[/<!--count:e2e_scripts-->(\d+)<!--\/count-->/, 1],
  )
  StaticAttestation.assert!(
    readme.include?("# #{scripts.length} shell E2E scripts"),
    "README tree E2E script count is stale",
    check: "e2e_script_contract.readme_tree_count",
    input_path: "README.md",
    expected: "#{scripts.length} shell E2E scripts",
    actual: readme.include?("# #{scripts.length} shell E2E scripts") ? "present" : "missing",
  )

  unknown_exceptions = exceptions.keys - scripts
  StaticAttestation.assert!(
    unknown_exceptions.empty?,
    "E2E script contract exception list contains unknown paths",
    check: "e2e_script_contract.exceptions_known",
    input_path: "tests/e2e",
    expected: [],
    actual: unknown_exceptions,
  )

  direct_exec_required = 0
  direct_exec_exceptions = 0
  strict_required = 0
  strict_exceptions = 0

  scripts.each do |path|
    exception = exceptions.fetch(path, {})
    text = StaticAttestation.read_text!(path, check: "e2e_script_contract.read_script")
    candidate = StaticAttestation.repo_path(path)
    first_line = text.lines.first.to_s.strip
    executable = File.executable?(candidate)
    strict = text.include?("set -euo pipefail")

    StaticAttestation.assert!(
      first_line.start_with?("#!"),
      "#{path} missing shebang",
      check: "e2e_script_contract.shebang",
      input_path: path,
      expected: "#!",
      actual: first_line.empty? ? "empty" : first_line,
    )

    if exception.fetch("direct_exec", true)
      direct_exec_required += 1
      StaticAttestation.assert!(
        executable,
        "#{path} is not directly executable",
        check: "e2e_script_contract.direct_exec",
        input_path: path,
        expected: "executable",
        actual: executable ? "executable" : "not_executable",
      )
    else
      direct_exec_exceptions += 1
      StaticAttestation.log_check(
        "e2e_script_contract.direct_exec_exception",
        input_path: path,
        expected: "not_required",
        actual: executable ? "executable" : "not_executable",
        status: "pass",
      )
    end

    if exception.fetch("strict_mode", true)
      strict_required += 1
      StaticAttestation.assert!(
        strict,
        "#{path} missing set -euo pipefail",
        check: "e2e_script_contract.strict_mode",
        input_path: path,
        expected: "set -euo pipefail",
        actual: strict ? "present" : "missing",
      )
    else
      strict_exceptions += 1
      StaticAttestation.assert!(
        !strict,
        "#{path} strict-mode exception should be removed once strict mode is present",
        check: "e2e_script_contract.strict_mode_exception",
        input_path: path,
        expected: "known_exception_without_set_e",
        actual: strict ? "present" : "missing",
      )
    end

    unless exception.empty?
      StaticAttestation.log_check(
        "e2e_script_contract.known_exception",
        input_path: path,
        expected: exception.fetch("reason"),
        actual: {
          "direct_exec" => executable ? "executable" : "not_executable",
          "strict_mode" => strict ? "present" : "missing",
        },
        status: "pass",
      )
    end
  end

  summary = {
    "bead_id" => "ft-gv7y2.4",
    "scenario_id" => "e2e_shell_script_contract",
    "status" => "passed",
    "script_count" => scripts.length,
    "shebang_checked_count" => scripts.length,
    "direct_exec_required_count" => direct_exec_required,
    "direct_exec_exception_count" => direct_exec_exceptions,
    "strict_mode_required_count" => strict_required,
    "strict_mode_exception_count" => strict_exceptions,
    "known_exceptions" => exceptions.map do |path, detail|
      {
        "path" => path,
        "direct_exec" => detail.fetch("direct_exec", true),
        "strict_mode" => detail.fetch("strict_mode", true),
        "reason" => detail.fetch("reason"),
      }
    end,
    "artifacts" => {
      "structured_log" => structured_log_path.delete_prefix("#{root}/"),
      "summary" => summary_path.delete_prefix("#{root}/"),
    },
  }

  File.write(summary_path, JSON.pretty_generate(summary) + "\n")
  StaticAttestation.log_check(
    "e2e_script_contract.summary",
    input_path: "tests/e2e",
    expected: "passed",
    actual: summary,
    status: "pass",
  )
  puts "E2E shell script contract: passed (#{scripts.length} scripts, #{direct_exec_exceptions} direct-exec exceptions, #{strict_exceptions} strict-mode exceptions)"
ensure
  log_io.close
end
RUBY
