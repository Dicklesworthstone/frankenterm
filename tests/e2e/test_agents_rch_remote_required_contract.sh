#!/usr/bin/env bash
# Static verifier for AGENTS.md remote-required RCH proof examples.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

AGENTS="AGENTS.md"
README="README.md"
MAIN_RS="crates/frankenterm/src/main.rs"
LIVE_RCH_SURFACES=(
  "crates/frankenterm-core/src/mission_objective_plan.rs"
  "crates/frankenterm-core/src/resource_pressure_chaos_runner.rs"
  "crates/frankenterm-core/src/runbook_compiler.rs"
  "crates/frankenterm-core/src/swarm_failure_conformance.rs"
  "crates/frankenterm-core/src/test_artifacts.rs"
  "crates/frankenterm-core-replay/src/replay_decision_diff.rs"
  "crates/frankenterm-core-replay/src/replay_resource_digital_twin.rs"
  "crates/frankenterm-core-audit-types/src/proof_doctor.rs"
  "crates/frankenterm-core-audit-types/src/proof_lane.rs"
  "crates/frankenterm-core/src/workflows/handlers.rs"
  "crates/frankenterm-core/tests/fixtures/blocker_radar/claimability_cases.json"
  "crates/frankenterm-core/tests/fixtures/blocker_radar/conformance_cases.json"
  "crates/frankenterm-core/tests/fixtures/rehearsal_score_receipt_golden_matrix.json"
  "crates/frankenterm-core/tests/golden_robot_envelope/control_plane_golden_matrix.json"
  "crates/frankenterm-core/tests/scale_lab_smoke_harness.rs"
  "frankenterm/escape-parser/tests/terminal_conformance_corpus.rs"
  "tests/e2e/test_ft_782hw_4_proof_doctor_handoff.sh"
  "tests/e2e/test_proof_doctor_handoff_generation.sh"
  "tests/fixtures/terminal-conformance/manifest.json"
  "tests/fixtures/terminal-conformance/README.md"
  "tests/fixtures/terminal-conformance/minimized/tc-minimized-synthetic-failure-001.json"
)
ABSOLUTE_TARGET_DIR_HARNESSES=(
  "tests/e2e/test_agent_detection.sh"
  "tests/e2e/test_agent_detection_graceful.sh"
  "tests/e2e/test_agent_autoconfig.sh"
  "tests/e2e/test_ft_1i2ge_4_6.sh"
  "tests/e2e/test_ft_1i2ge_3_8.sh"
  "tests/e2e/test_ft_1i2ge_8_10.sh"
)
DOCS=(
  "docs/adr/0012-asupersync-runtime-doctrine.md"
  "docs/asupersync-migration-baseline.md"
  "docs/asupersync-migration-playbook.md"
  "docs/asupersync-migration-scoreboard.json"
  "docs/asupersync-migration-scoreboard.md"
  "docs/asupersync-rch-execution-policy.md"
  "docs/design/mmap_scrollback_store.md"
  "docs/design/ntm-fcp-convergence-architecture.md"
  "docs/ft-3681t-convergence-architecture.md"
  "docs/ft-xbnl0-verification-contract.md"
  "docs/gpu-harness-fixture-guide.md"
  "docs/high-core-swarm-runbook.md"
  "docs/high-scale-operator-rehearsals.md"
  "docs/latency-immunity-architecture-ft-1u90p.9.md"
  "docs/operator-playbook.md"
  "docs/operator-runbook.md"
  "docs/perf/swarm-capacity-baseline.md"
  "docs/proposals/ft-1grhq-storage-io-scheduler-contract.md"
  "docs/proposals/ft-luq3w-safe-auto-tuning-contract.md"
  "docs/rch-admission-contract.md"
  "docs/release/checklist.md"
  "docs/resource-pressure-cockpit-contract.md"
  "docs/terminal-conformance-contract.md"
  "docs/test-logging-contract.md"
  "docs/rio-analysis-synthesis.md"
  "docs/rio-implementation-validation-matrix.md"
  "docs/robot-contracts/api-surface-coverage.md"
  "docs/robot-contracts/checkpoint.md"
  "docs/robot-contracts/current-ntm-gap-dispatch.md"
  "docs/robot-contracts/fleet.md"
  "docs/robot-contracts/work.md"
  "docs/security/redaction-evidence-byte-semantics.md"
  "docs/spike/gpu-ci-linux-feasibility.md"
  "docs/tuning-reference.md"
)
DOCS_WITH_INTENTIONAL_INVALID_RCH_EXAMPLES=(
  "docs/proposals/ft-tn6cw-proof-lane-evidence-contract.md"
  "docs/proposals/ft-tn6cw-proof-lane-evidence-taxonomy.md"
  "docs/proposals/ft-wik9p-proof-doctor-verdict-schema.md"
)

fail() {
  printf 'agents rch remote-required contract: %s\n' "$*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "missing command: $1"
}

require_file() {
  [[ -f "$1" ]] || fail "missing file: $1"
}

require_command ruby
require_file "${AGENTS}"
require_file "${README}"
require_file "${MAIN_RS}"
for surface in "${LIVE_RCH_SURFACES[@]}"; do
  require_file "${surface}"
done
for surface in "${ABSOLUTE_TARGET_DIR_HARNESSES[@]}"; do
  require_file "${surface}"
done
for doc in "${DOCS[@]}"; do
  require_file "${doc}"
done
for doc in "${DOCS_WITH_INTENTIONAL_INVALID_RCH_EXAMPLES[@]}"; do
  require_file "${doc}"
done

ruby <<'RUBY'
AGENTS = "AGENTS.md"
README = "README.md"
MAIN_RS = "crates/frankenterm/src/main.rs"
LIVE_RCH_SURFACES = [
  "crates/frankenterm-core/src/mission_objective_plan.rs",
  "crates/frankenterm-core/src/resource_pressure_chaos_runner.rs",
  "crates/frankenterm-core/src/runbook_compiler.rs",
  "crates/frankenterm-core/src/swarm_failure_conformance.rs",
  "crates/frankenterm-core/src/test_artifacts.rs",
  "crates/frankenterm-core-replay/src/replay_decision_diff.rs",
  "crates/frankenterm-core-replay/src/replay_resource_digital_twin.rs",
  "crates/frankenterm-core-audit-types/src/proof_doctor.rs",
  "crates/frankenterm-core-audit-types/src/proof_lane.rs",
  "crates/frankenterm-core/src/workflows/handlers.rs",
  "crates/frankenterm-core/tests/fixtures/blocker_radar/claimability_cases.json",
  "crates/frankenterm-core/tests/fixtures/blocker_radar/conformance_cases.json",
  "crates/frankenterm-core/tests/fixtures/rehearsal_score_receipt_golden_matrix.json",
  "crates/frankenterm-core/tests/golden_robot_envelope/control_plane_golden_matrix.json",
  "crates/frankenterm-core/tests/scale_lab_smoke_harness.rs",
  "frankenterm/escape-parser/tests/terminal_conformance_corpus.rs",
  "tests/e2e/test_ft_782hw_4_proof_doctor_handoff.sh",
  "tests/e2e/test_proof_doctor_handoff_generation.sh",
  "tests/fixtures/terminal-conformance/manifest.json",
  "tests/fixtures/terminal-conformance/README.md",
  "tests/fixtures/terminal-conformance/minimized/tc-minimized-synthetic-failure-001.json"
]
ABSOLUTE_TARGET_DIR_HARNESSES = [
  "tests/e2e/test_agent_detection.sh",
  "tests/e2e/test_agent_detection_graceful.sh",
  "tests/e2e/test_agent_autoconfig.sh",
  "tests/e2e/test_ft_1i2ge_4_6.sh",
  "tests/e2e/test_ft_1i2ge_3_8.sh",
  "tests/e2e/test_ft_1i2ge_8_10.sh"
]
CONTRACT_DOCS = [
  "docs/adr/0012-asupersync-runtime-doctrine.md",
  "docs/asupersync-migration-baseline.md",
  "docs/asupersync-migration-playbook.md",
  "docs/asupersync-migration-scoreboard.json",
  "docs/asupersync-migration-scoreboard.md",
  "docs/asupersync-rch-execution-policy.md",
  "docs/design/mmap_scrollback_store.md",
  "docs/design/ntm-fcp-convergence-architecture.md",
  "docs/ft-3681t-convergence-architecture.md",
  "docs/ft-xbnl0-verification-contract.md",
  "docs/gpu-harness-fixture-guide.md",
  "docs/high-core-swarm-runbook.md",
  "docs/high-scale-operator-rehearsals.md",
  "docs/latency-immunity-architecture-ft-1u90p.9.md",
  "docs/operator-playbook.md",
  "docs/operator-runbook.md",
  "docs/perf/swarm-capacity-baseline.md",
  "docs/proposals/ft-1grhq-storage-io-scheduler-contract.md",
  "docs/proposals/ft-luq3w-safe-auto-tuning-contract.md",
  "docs/rch-admission-contract.md",
  "docs/release/checklist.md",
  "docs/resource-pressure-cockpit-contract.md",
  "docs/terminal-conformance-contract.md",
  "docs/test-logging-contract.md",
  "docs/rio-analysis-synthesis.md",
  "docs/rio-implementation-validation-matrix.md",
  "docs/robot-contracts/api-surface-coverage.md",
  "docs/robot-contracts/checkpoint.md",
  "docs/robot-contracts/current-ntm-gap-dispatch.md",
  "docs/robot-contracts/fleet.md",
  "docs/robot-contracts/work.md",
  "docs/security/redaction-evidence-byte-semantics.md",
  "docs/spike/gpu-ci-linux-feasibility.md",
  "docs/tuning-reference.md"
]
INTENTIONAL_INVALID_RCH_DOCS = [
  "docs/proposals/ft-tn6cw-proof-lane-evidence-contract.md",
  "docs/proposals/ft-tn6cw-proof-lane-evidence-taxonomy.md",
  "docs/proposals/ft-wik9p-proof-doctor-verdict-schema.md"
]

def fail!(message)
  warn "agents rch remote-required contract: #{message}"
  exit 1
end

text = File.read(AGENTS)
readme = File.read(README)
main_rs = File.read(MAIN_RS)

compiler = text[/## Compiler Checks \(CRITICAL\).*?---/m]
fail!("missing Compiler Checks section") unless compiler

manual = text[/## RCH — Remote Compilation Helper.*?### When rch is down/m]
fail!("missing RCH helper section") unless manual

weekly = text[/## Weekly WezTerm Upstream Backport Workflow.*?## Toolchain: Rust & Cargo/m]
fail!("missing Weekly WezTerm section") unless weekly

testing = text[/## Testing.*?## ast-grep vs ripgrep/m]
fail!("missing AGENTS Testing section") unless testing

readme_benchmarks = readme[/## Performance Benchmarks.*?## Testing/m]
fail!("missing README Performance Benchmarks section") unless readme_benchmarks

readme_testing = readme[/## Testing.*?## Troubleshooting/m]
fail!("missing README Testing section") unless readme_testing

required_snippets = [
  "RCH_REQUIRE_REMOTE=1",
  "RCH_NO_SELF_HEALING=1",
  "rch --no-self-healing exec --"
]

[
  ["compiler", compiler],
  ["manual", manual],
  ["weekly", weekly],
  ["testing", testing],
  ["README Performance Benchmarks", readme_benchmarks],
  ["README Testing", readme_testing]
].each do |name, section|
  required_snippets.each do |snippet|
    fail!("#{name} section missing #{snippet}") unless section.include?(snippet)
  end
  fail!("#{name} section still has bare rch exec") if section.match?(/(^|\s)rch exec --\s+env\s+CARGO_TARGET_DIR=/)
  fail!("#{name} section still omits --no-self-healing") if section.match?(/(^|\s)RCH_REQUIRE_REMOTE=1\s+rch exec --/)
end

CONTRACT_DOCS.each do |path|
  doc = File.read(path)
  required_snippets.each do |snippet|
    fail!("#{path} missing #{snippet}") unless doc.include?(snippet)
  end
  if doc.match?(/(^|\s)rch exec --\s+env\s+CARGO_TARGET_DIR=/)
    fail!("#{path} still has bare rch exec")
  end
  if doc.match?(/(^|\s)RCH_REQUIRE_REMOTE=1\s+rch exec --/)
    fail!("#{path} still omits --no-self-healing")
  end
end

INTENTIONAL_INVALID_RCH_DOCS.each do |path|
  doc = File.read(path)
  required_snippets.each do |snippet|
    fail!("#{path} missing #{snippet}") unless doc.include?(snippet)
  end
  if doc.match?(/(^|\s)RCH_REQUIRE_REMOTE=1\s+rch exec --/)
    fail!("#{path} still omits --no-self-healing")
  end
end

if main_rs.match?(/(^|\s)rch exec --\s+env\s+CARGO_TARGET_DIR=/)
  fail!("#{MAIN_RS} still has bare rch exec")
end
if main_rs.match?(/(^|\s)RCH_REQUIRE_REMOTE=1\s+rch exec --/)
  fail!("#{MAIN_RS} still omits --no-self-healing")
end

LIVE_RCH_SURFACES.each do |path|
  surface = File.read(path)
  if surface.match?(/(^|\s)rch exec --\s+(?:env\s+CARGO_TARGET_DIR=|cargo|bash\s+-lc)/)
    fail!("#{path} still has a fail-open live RCH command surface")
  end
  if surface.match?(/(^|\s)RCH_REQUIRE_REMOTE=1\s+rch exec --/)
    fail!("#{path} still omits --no-self-healing")
  end
  next unless surface.include?("rch")

  required_snippets.each do |snippet|
    fail!("#{path} missing #{snippet}") unless surface.include?(snippet)
  end
end

target_dir_rejection = /
  if \s+ \[\[ \s+ -n \s+ "\$\{(?<var>INHERITED_CARGO_TARGET_DIR|REQUESTED_TARGET_DIR|REQUESTED_CARGO_TARGET_DIR)\}" \s+
  && \s+ "\$\{\k<var>\}" \s+ != \s+ \/\* \s+ \]\] ; \s+ then
/x

ABSOLUTE_TARGET_DIR_HARNESSES.each do |path|
  surface = File.read(path)
  fail!("#{path} missing CARGO_TARGET_DIR selection") unless surface.include?("CARGO_TARGET_DIR")
  fail!("#{path} still rejects absolute CARGO_TARGET_DIR overrides") if surface.match?(target_dir_rejection)
end

%w[
  cargo\ check
  cargo\ clippy
  cargo\ fmt
].each do |command|
  fail!("compiler section missing #{command}") unless compiler.include?(command.tr("\\", ""))
end

%w[
  cargo\ build
  cargo\ test
  cargo\ clippy
].each do |command|
  fail!("manual section missing #{command}") unless manual.include?(command.tr("\\", ""))
end

fallback_needles = [
  "[RCH] local",
  "running locally",
  "no admissible workers",
  "worker=null",
  "local fallback",
  "blocked",
  "Do not count local Cargo output as proof"
]
fallback_needles.each do |needle|
  fail!("AGENTS.md missing fallback blocker wording: #{needle}") unless text.include?(needle)
end

puts "agents rch remote-required contract: static verifier passed"
RUBY
