#!/usr/bin/env bash
# Static verifier for the deferred proof replay decision harness (ft-zbnz4.6).
#
# The harness projects retained receipts into a queue, classifies each one
# fail-closed, selects at most one next candidate, and records a dry-run replay
# decision. It NEVER mints material remote proof: a live replay still requires a
# real remote RCH worker (ft-zbnz4.4 -> ft-5xwsu.3). This verifier proves the
# queue semantics without depending on live RCH recovery, while the tamper
# corpus locks the fail-closed invariants:
#   * a non-remote (local cargo / missing RCH_REQUIRE_REMOTE) command never runs,
#   * a stale command shape never runs,
#   * a selected-worker topology-preflight failure is a distinct blocker
#     (not no-admissible-workers, not worker-pressure, not code failure) and is
#     never green, and
#   * local Cargo output is never classified as successful proof.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

SCHEMA="docs/json-schema/ft-deferred-proof-replay-harness.json"
DOC="docs/robot-contracts/deferred-proof-replay-harness.md"
MANIFEST="fixtures/deferred-proof-replay/replay-harness/manifest.json"
INPUT="fixtures/deferred-proof-replay/replay-harness/input-receipts.v1.json"
TAMPER="fixtures/deferred-proof-replay/replay-harness/tamper-cases.v1.json"
EXPECTED="fixtures/deferred-proof-replay/replay-harness/expected/decisions.v1.jsonl"
PROVENANCE="docs/json-schema/PROVENANCE.md"

fail() {
  printf 'deferred proof replay harness contract: %s\n' "$*" >&2
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
require_file "${INPUT}"
require_file "${TAMPER}"
require_file "${EXPECTED}"
require_file "${PROVENANCE}"

jq empty "${SCHEMA}" "${MANIFEST}" "${INPUT}" "${TAMPER}"
jq -c empty "${EXPECTED}" >/dev/null

ruby <<'RUBY'
require "json"
require "set"

SCHEMA = "docs/json-schema/ft-deferred-proof-replay-harness.json"
DOC = "docs/robot-contracts/deferred-proof-replay-harness.md"
MANIFEST = "fixtures/deferred-proof-replay/replay-harness/manifest.json"
INPUT = "fixtures/deferred-proof-replay/replay-harness/input-receipts.v1.json"
TAMPER = "fixtures/deferred-proof-replay/replay-harness/tamper-cases.v1.json"
EXPECTED = "fixtures/deferred-proof-replay/replay-harness/expected/decisions.v1.jsonl"
PROVENANCE = "docs/json-schema/PROVENANCE.md"

DECISION_CONTRACT_ID = "ft.deferred_proof_replay_harness.decision.v1"
INPUT_CONTRACT_ID = "ft.deferred_proof_replay_harness.input.v1"
TAMPER_CONTRACT_ID = "ft.deferred_proof_replay_harness.tamper.v1"

# Canonical, currently-runnable command shapes. Anything else is stale and must
# never run, no matter what eligibility the receipt forges.
CANONICAL_SHAPES = %w[rch-no-self-healing-v1 static-verifier-v1].freeze

# The full decision vocabulary. The harness is dry-run only, so there is no
# green/proof_complete value anywhere in the space.
DECISIONS = %w[
  run_static_now
  would_run_remote
  defer_remote_blocked
  defer_dirty_overlap
  defer_prerequisite
  cancelled
  reject_stale_command
  reject_non_remote_command
  request_triage
].freeze

def fail!(message)
  warn "deferred proof replay harness contract: #{message}"
  exit 1
end

def read_json(path)
  JSON.parse(File.read(path))
rescue JSON::ParserError => error
  fail!("#{path} does not parse as JSON: #{error.message}")
end

def read_jsonl(path)
  File.readlines(path, chomp: true).reject(&:empty?).map { |line| JSON.parse(line) }
rescue JSON::ParserError => error
  fail!("#{path} does not parse as JSONL: #{error.message}")
end

def env_value(command, name)
  (command["env"] || []).each do |item|
    return item["value"] if item["name"] == name
  end
  nil
end

# A remote-only command shape: argv must be `rch ... --no-self-healing ... exec
# -- ...`, the RCH_REQUIRE_REMOTE and RCH_NO_SELF_HEALING env flags must both be
# "1" (otherwise a local fallback is reachable and it is not remote-only proof),
# and the CARGO_TARGET_DIR must be pinned in argv when a target_dir is declared.
def remote_only_command?(command)
  argv = command["argv"] || []
  return false unless argv.first == "rch"

  exec_index = argv.index("exec")
  return false unless exec_index && argv[exec_index + 1] == "--"
  return false unless argv[1...exec_index].include?("--no-self-healing")
  return false unless env_value(command, "RCH_REQUIRE_REMOTE") == "1"
  return false unless env_value(command, "RCH_NO_SELF_HEALING") == "1"

  target = command["target_dir"]
  return true if target.nil?

  argv.include?("CARGO_TARGET_DIR=#{target}")
end

# Map a raw RCH admission state to a coarse blocked classification plus the
# precise blocker code. A selected-worker topology-preflight failure keeps its
# own blocker and is never collapsed into worker-pressure / no-admissible.
def admission_blocked(state)
  case state
  when "topology_preflight_failed"
    "rch.topology_preflight_failed"
  when "critical_pressure", "no_admissible_workers", "blocked_worker_pressure",
       "insufficient_slots", "telemetry_gap", "active_project_exclusion", "wait_rch"
    "rch.worker_pressure"
  else
    nil
  end
end

# Authoritative fail-closed classifier. The receipt's declared eligibility,
# replay_allowed, and evidence_classification are untrusted: only structural and
# coordination facts decide. First match wins; the ordering is the fail-closed
# guarantee.
def classify(receipt)
  command = receipt["command"] || {}
  coordination = receipt["coordination"] || {}
  paths = receipt["paths"] || {}
  proof = receipt["proof"] || {}
  argv = command["argv"] || []
  material = command["material_remote_required"] == true

  # (a) An operator's explicit cancellation overrides everything; never auto-run.
  return ["cancelled", ["operator.cancelled"]] if coordination["operator_cancelled"] == true

  # (b) A non-canonical command shape never runs, even when admission is
  #     admitted and the receipt forges replay_allowed=true.
  unless CANONICAL_SHAPES.include?(command["command_shape_version"])
    return ["reject_stale_command", ["command.stale_shape"]]
  end

  # (c) Structurally empty / ambiguous receipts go to human triage.
  return ["request_triage", ["triage.ambiguous"]] if argv.empty?
  return ["request_triage", ["triage.ambiguous"]] if proof["evidence_classification"] == "ambiguous"

  unless material
    # (d) Static verifier: runnable right now without any RCH worker.
    return ["run_static_now", []] if command["command_shape_version"] == "static-verifier-v1"

    return ["request_triage", ["triage.ambiguous"]]
  end

  # (e) Material remote receipts.
  # Command must be a real remote-only shape; a bare/local cargo or a
  # command missing RCH_REQUIRE_REMOTE can fall back locally and is rejected.
  return ["reject_non_remote_command", ["command.not_remote_only"]] unless remote_only_command?(command)

  # Coordination blockers that are independent of RCH liveness.
  if (paths["dirty_paths_at_capture"] || []).any?
    return ["defer_dirty_overlap", ["overlap.dirty_paths"]]
  end
  if (coordination["prerequisite_beads"] || []).any?
    return ["defer_prerequisite", ["prereq.bead_open"]]
  end

  # RCH admission blockers. Topology-preflight keeps its distinct blocker.
  blocker = admission_blocked(coordination["rch_admission_state"])
  return ["defer_remote_blocked", [blocker]] if blocker

  # Admitted, canonical, clean: this is the receipt we WOULD replay remotely
  # once a worker is live. The harness still records no material proof.
  return ["would_run_remote", []] if coordination["rch_admission_state"] == "admitted"

  # Anything else (e.g. unknown admission) is ambiguous, fail closed.
  ["request_triage", ["triage.ambiguous"]]
end

def note_for(decision)
  case decision
  when "run_static_now"
    "Runnable now: static verifier needs no RCH worker."
  when "would_run_remote"
    "Eligible remote replay; deferred until a live remote RCH worker is admitted. No material proof minted here."
  when "defer_remote_blocked"
    "RCH admission blocked at capture; defer until remote RCH recovers."
  when "defer_dirty_overlap"
    "Dirty-tree overlap at capture; resolve ownership before replay."
  when "defer_prerequisite"
    "Prerequisite bead still open; complete it before replay."
  when "cancelled"
    "Operator cancelled this replay; never auto-queue."
  when "reject_stale_command"
    "Non-canonical command shape; refresh before any replay."
  when "reject_non_remote_command"
    "Command is not remote-only; local fallback is reachable so it is never proof."
  when "request_triage"
    "Ambiguous receipt; request human triage."
  else
    fail!("no note for decision #{decision}")
  end
end

def decision_record(receipt)
  command = receipt["command"] || {}
  coordination = receipt["coordination"] || {}
  decision, blockers = classify(receipt)
  {
    "schema_version" => 1,
    "contract_id" => DECISION_CONTRACT_ID,
    "bead_id" => receipt.fetch("bead_id"),
    "receipt_id" => receipt.fetch("receipt_id"),
    "command_shape_version" => command["command_shape_version"],
    "decision" => decision,
    "blockers" => blockers,
    "material_remote_required" => command["material_remote_required"] == true,
    "target_dir" => command["target_dir"],
    # Only carried for selected-worker remote failures, so a topology-preflight
    # failure is never confused with no-admissible-workers admission.
    "selected_worker" => coordination["selected_worker"],
    # Always null: a non-null remote exit may only come from a real RCH replay.
    "remote_exit_status" => nil,
    "dry_run" => true,
    # Material proof is never produced by the harness, so only static verifiers
    # are runnable right now.
    "replay_allowed_now" => decision == "run_static_now",
    "note" => note_for(decision)
  }
end

# Every emitted record must conform to the PUBLISHED schema, so the verifier and
# the schema cannot silently drift apart. Required keys, the decision enum, and
# the blocker enum are all read from the schema file itself (not re-hardcoded).
def assert_schema_conformant!(record, schema, where)
  required = schema.fetch("required")
  fail!("#{where}: record keys #{record.keys.sort.inspect} != schema required #{required.sort.inspect}") unless record.keys.sort == required.sort
  decision_enum = schema.dig("properties", "decision", "enum")
  fail!("#{where}: decision #{record["decision"]} not in schema enum") unless decision_enum.include?(record["decision"])
  blocker_enum = schema.dig("properties", "blockers", "items", "enum")
  record.fetch("blockers").each do |blocker|
    fail!("#{where}: blocker #{blocker} not in schema enum") unless blocker_enum.include?(blocker)
  end
  fail!("#{where}: contract_id drift") unless record["contract_id"] == schema.dig("properties", "contract_id", "const")
  fail!("#{where}: schema_version drift") unless record["schema_version"] == schema.dig("properties", "schema_version", "const")
end

# Cross-cutting fail-closed invariants every emitted record must satisfy.
def assert_fail_closed!(record, schema, where)
  assert_schema_conformant!(record, schema, where)
  fail!("#{where}: unknown decision #{record["decision"]}") unless DECISIONS.include?(record["decision"])
  fail!("#{where}: dry_run must be true") unless record["dry_run"] == true
  fail!("#{where}: remote_exit_status must be null") unless record["remote_exit_status"].nil?
  if record["replay_allowed_now"] && record["decision"] != "run_static_now"
    fail!("#{where}: only run_static_now may be replay_allowed_now")
  end
  if record["material_remote_required"] && record["replay_allowed_now"]
    fail!("#{where}: a material remote receipt is never runnable now (no local proof)")
  end
end

# Mapping from a receipt's self-declared eligibility.state to the decision the
# authoritative classifier must independently derive. This proves the honest
# input corpus is internally consistent without trusting its eligibility fields.
EXPECTED_BY_ELIGIBILITY = {
  "eligible" => %w[run_static_now would_run_remote],
  "wait_rch" => %w[defer_remote_blocked],
  "dirty_overlap" => %w[defer_dirty_overlap],
  "prerequisite_blocked" => %w[defer_prerequisite],
  "operator_cancelled" => %w[cancelled],
  "stale_command" => %w[reject_stale_command],
  "ambiguous" => %w[request_triage]
}.freeze

schema = read_json(SCHEMA)
manifest = read_json(MANIFEST)
input = read_json(INPUT)
tamper = read_json(TAMPER)
expected = read_jsonl(EXPECTED)
doc = File.read(DOC)
provenance = File.read(PROVENANCE)

# --- Schema sanity -------------------------------------------------------
fail!("schema id drifted") unless schema["$id"]&.end_with?("/ft-deferred-proof-replay-harness.json")
fail!("schema contract const drifted") unless schema.dig("properties", "contract_id", "const") == DECISION_CONTRACT_ID
fail!("schema decision enum drifted") unless schema.dig("properties", "decision", "enum").sort == DECISIONS.sort
fail!("schema forbids non-null remote exit") unless schema.dig("properties", "remote_exit_status", "type") == "null"
fail!("schema forces dry_run const") unless schema.dig("properties", "dry_run", "const") == true

# --- Manifest sanity -----------------------------------------------------
fail!("manifest contract drifted") unless manifest["contract_id"] == DECISION_CONTRACT_ID
fail!("manifest schema path drifted") unless manifest["schema_path"] == SCHEMA
fail!("manifest input path drifted") unless manifest["input_receipts"] == INPUT
fail!("manifest tamper path drifted") unless manifest["tamper_cases"] == TAMPER
fail!("manifest expected jsonl path drifted") unless manifest["expected_jsonl"] == EXPECTED
fail!("manifest input contract drifted") unless manifest.dig("golden_summary", "input_contract_id") == INPUT_CONTRACT_ID
fail!("manifest tamper contract drifted") unless manifest.dig("golden_summary", "tamper_contract_id") == TAMPER_CONTRACT_ID

# --- Honest corpus: classify and compare against the golden JSONL --------
fail!("input contract drifted") unless input["contract_id"] == INPUT_CONTRACT_ID
receipts = input.fetch("receipts")
fail!("input corpus is empty") if receipts.empty?
receipt_ids = receipts.map { |r| r.fetch("receipt_id") }
fail!("input receipt ids are not unique") unless receipt_ids.uniq.length == receipt_ids.length

actual = receipts.map { |r| decision_record(r) }
actual.each { |rec| assert_fail_closed!(rec, schema, "input #{rec["receipt_id"]}") }

actual_lines = actual.map { |rec| JSON.generate(rec) }
expected_lines = expected.map { |rec| JSON.generate(rec) }
unless actual_lines == expected_lines
  fail!("generated decisions drifted\nexpected:\n#{expected_lines.join("\n")}\nactual:\n#{actual_lines.join("\n")}")
end
# Determinism: a second classification of the same bytes is byte-identical.
fail!("classification is not deterministic") unless receipts.map { |r| JSON.generate(decision_record(r)) } == actual_lines

# The honest corpus must declare an eligibility consistent with the derived
# decision (the classifier and the corpus agree without trusting the corpus).
receipts.each_with_index do |receipt, idx|
  declared = receipt.dig("eligibility", "state")
  allowed = EXPECTED_BY_ELIGIBILITY[declared]
  fail!("unknown declared eligibility #{declared.inspect} for #{receipt["receipt_id"]}") unless allowed
  got = actual[idx]["decision"]
  fail!("#{receipt["receipt_id"]} declares #{declared} but classifier derived #{got}") unless allowed.include?(got)
  # replay_allowed must never disagree with the derived runnability.
  declared_replay = receipt.dig("eligibility", "replay_allowed")
  derived_static = got == "run_static_now"
  if declared_replay == true && !(got == "would_run_remote" || derived_static)
    fail!("#{receipt["receipt_id"]} declares replay_allowed but classifier blocks it (#{got})")
  end
end

# The honest corpus must exercise every decision the harness can emit EXCEPT
# reject_non_remote_command, which is a pure adversarial outcome (a well-formed
# honest receipt never carries a non-remote material command). That decision is
# locked by the tamper corpus below.
HONEST_DECISIONS = (DECISIONS - %w[reject_non_remote_command]).freeze
present = actual.map { |rec| rec["decision"] }.uniq.sort
fail!("honest corpus does not exercise every decision: missing #{(HONEST_DECISIONS - present).inspect}") unless present == HONEST_DECISIONS.sort
fail!("honest corpus unexpectedly emitted reject_non_remote_command") if present.include?("reject_non_remote_command")

# Topology-preflight must keep its distinct blocker and selected worker, and
# must never collapse into worker-pressure / no-admissible-workers.
topo = actual.find { |rec| rec["blockers"].include?("rch.topology_preflight_failed") }
fail!("no topology-preflight decision present") unless topo
fail!("topology decision must defer") unless topo["decision"] == "defer_remote_blocked"
fail!("topology decision lost its selected worker") if topo["selected_worker"].nil?
fail!("topology decision collapsed into worker pressure") if topo["blockers"].include?("rch.worker_pressure")

# --- Next candidate selection -------------------------------------------
# Prefer something runnable now (static) over a remote replay that still needs a
# live RCH worker; break ties deterministically by bead id. At most one.
def select_next_candidate(records)
  %w[run_static_now would_run_remote].each do |tier|
    pick = records.select { |rec| rec["decision"] == tier }.min_by { |rec| rec["bead_id"] }
    return pick if pick
  end
  nil
end

next_candidate = select_next_candidate(actual)
fail!("expected a next candidate in the honest corpus") unless next_candidate
expected_nc = manifest.dig("golden_summary", "next_candidate")
fail!("next candidate drifted: got #{next_candidate["bead_id"]}, manifest says #{expected_nc}") unless next_candidate["bead_id"] == expected_nc
fail!("next candidate must be runnable now (static, RCH down)") unless next_candidate["decision"] == "run_static_now"
fail!("next candidate must be replay_allowed_now") unless next_candidate["replay_allowed_now"]

# Metamorphic checks on the selection branches the honest corpus cannot exercise
# (it carries exactly one run_static_now and no static ties):
#  - tier fallback: with no static receipt, the next candidate must fall through
#    to a would_run_remote one (not to a deferred/rejected receipt, and not null
#    while a remote-runnable one exists);
#  - deterministic tie-break: among same-tier candidates the smallest bead_id
#    wins regardless of input order;
#  - empty/all-blocked input yields no candidate.
without_static = actual.reject { |rec| rec["decision"] == "run_static_now" }
fallback = select_next_candidate(without_static)
remote_runnable = without_static.select { |rec| rec["decision"] == "would_run_remote" }
if remote_runnable.empty?
  fail!("fallback should be nil when nothing is runnable") unless fallback.nil?
else
  fail!("fallback must pick a would_run_remote receipt") unless fallback && fallback["decision"] == "would_run_remote"
  fail!("fallback must be the smallest-bead_id remote receipt") unless fallback["bead_id"] == remote_runnable.map { |r| r["bead_id"] }.min
end

tie_a = { "bead_id" => "ft-zzz", "decision" => "run_static_now" }
tie_b = { "bead_id" => "ft-aaa", "decision" => "run_static_now" }
fail!("tie-break must be order-independent (forward)") unless select_next_candidate([tie_a, tie_b])["bead_id"] == "ft-aaa"
fail!("tie-break must be order-independent (reversed)") unless select_next_candidate([tie_b, tie_a])["bead_id"] == "ft-aaa"

only_blocked = actual.reject { |rec| %w[run_static_now would_run_remote].include?(rec["decision"]) }
fail!("all-blocked input must yield no candidate") unless select_next_candidate(only_blocked).nil?
fail!("empty input must yield no candidate") unless select_next_candidate([]).nil?

# Manifest decision histogram reconciles with the classifier output.
counts = Hash.new(0)
actual.each { |rec| counts[rec["decision"]] += 1 }
DECISIONS.each do |decision|
  declared = manifest.dig("golden_summary", "decision_counts", decision) || 0
  fail!("manifest decision_counts[#{decision}] drift: got #{counts[decision]}, manifest #{declared}") unless declared == counts[decision]
end
fail!("manifest receipt_count drift") unless manifest.dig("golden_summary", "receipt_count") == receipts.length

# --- Tamper corpus: fail-closed invariants ------------------------------
fail!("tamper contract drifted") unless tamper["contract_id"] == TAMPER_CONTRACT_ID
cases = tamper.fetch("cases")
fail!("tamper corpus is empty") if cases.empty?
tamper_ids = cases.map { |c| c.fetch("case_id") }
fail!("tamper case ids are not unique") unless tamper_ids.uniq.length == tamper_ids.length

REQUIRED_TAMPER = %w[
  local-cargo-direct-never-runnable
  missing-require-remote-env-never-runnable
  topology-preflight-never-green
  topology-preflight-not-no-worker
  stale-shape-never-runnable
  forged-green-without-remote-exit
  dirty-overlap-forged-eligible
  prerequisite-forged-eligible
  cancelled-overrides-admitted-eligible
].freeze
fail!("tamper coverage drifted: #{tamper_ids.sort.inspect}") unless tamper_ids.sort == REQUIRED_TAMPER.sort
fail!("manifest tamper_case_count drift") unless manifest.dig("golden_summary", "tamper_case_count") == cases.length

cases.each do |kase|
  receipt = kase.fetch("receipt")
  record = decision_record(receipt)
  assert_fail_closed!(record, schema, "tamper #{kase["case_id"]}")
  decision = record["decision"]
  expected_decision = kase.fetch("expected_decision")
  fail!("tamper #{kase["case_id"]} expected #{expected_decision} but got #{decision}") unless decision == expected_decision

  (kase["forbidden_decisions"] || []).each do |forbidden|
    fail!("tamper #{kase["case_id"]} emitted forbidden decision #{forbidden}") if decision == forbidden
  end
  # A would_run_remote / run_static_now decision must never be reached by a
  # local-cargo, stale, or topology-failed receipt: the runnable decisions are
  # globally forbidden whenever the case lists them as forbidden.
  (kase["must_include_blocker"] || []).each do |blocker|
    fail!("tamper #{kase["case_id"]} missing required blocker #{blocker}") unless record["blockers"].include?(blocker)
  end
  (kase["must_exclude_blocker"] || []).each do |blocker|
    fail!("tamper #{kase["case_id"]} must exclude blocker #{blocker}") if record["blockers"].include?(blocker)
  end
  if kase["forbid_green_proof_state"]
    # The harness has no green state; concretely, a forged-green material
    # receipt must stay would_run_remote with no minted proof.
    fail!("tamper #{kase["case_id"]} minted a remote exit status") unless record["remote_exit_status"].nil?
    fail!("tamper #{kase["case_id"]} marked a material receipt runnable now") if record["replay_allowed_now"]
  end
end

# A would_run_remote tamper receipt must never be the next candidate ahead of a
# real static one, and crucially must never be reclassified as runnable now.
tamper_records = cases.map { |kase| decision_record(kase.fetch("receipt")) }
fail!("a tamper receipt was classified run_static_now") if tamper_records.any? { |rec| rec["decision"] == "run_static_now" }
# The non-remote rejection is adversarial-only; the tamper corpus must lock it.
unless tamper_records.any? { |rec| rec["decision"] == "reject_non_remote_command" }
  fail!("tamper corpus does not exercise reject_non_remote_command")
end

# --- Doc + provenance ----------------------------------------------------
[
  "ft.deferred_proof_replay_harness.decision.v1",
  "ft.deferred_proof_replay_harness.input.v1",
  "ft.deferred_proof_replay_harness.tamper.v1",
  "run_static_now",
  "would_run_remote",
  "defer_remote_blocked",
  "reject_non_remote_command",
  "reject_stale_command",
  "rch.topology_preflight_failed",
  "RCH_REQUIRE_REMOTE=1",
  "RCH_NO_SELF_HEALING=1",
  "remote_exit_status",
  "ft-zbnz4.4",
  "fixtures/deferred-proof-replay/replay-harness/"
].each do |term|
  fail!("doc missing contract term #{term}") unless doc.include?(term)
end

fail!("provenance missing replay-harness row") unless provenance.include?("`ft-deferred-proof-replay-harness.json`")
fail!("provenance row missing verifier") unless provenance.include?("bash tests/e2e/test_deferred_proof_replay_harness_contract.sh")

emitted_remote = actual.count { |rec| rec["decision"] == "would_run_remote" }
puts "deferred proof replay harness contract: static verifier passed " \
     "(#{actual.length} receipts, next=#{next_candidate["bead_id"]}/#{next_candidate["decision"]}, " \
     "#{emitted_remote} remote-deferred, #{cases.length} rejected tamper cases)"
RUBY
