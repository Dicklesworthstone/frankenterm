#!/usr/bin/env bash
# Cross-contract conformance harness for the deferred-proof contract family
# (ft-zbnz4.{1,2,3,5,8}). Locks the vocabulary the family must share so editing one
# contract's enum and forgetting the others fails loudly. Static; no RCH needed.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

PROVENANCE="docs/json-schema/PROVENANCE.md"

fail() {
  printf 'deferred proof family conformance: %s\n' "$*" >&2
  exit 1
}

command -v jq >/dev/null 2>&1 || fail "missing command: jq"
command -v ruby >/dev/null 2>&1 || fail "missing command: ruby"

RECEIPT="docs/json-schema/ft-deferred-proof-receipt.json"
EXTRACT="docs/json-schema/ft-deferred-proof-comment-extraction.json"
SURFACE="docs/json-schema/ft-deferred-proof-queue-surface.json"
OWNGATE="docs/json-schema/ft-deferred-proof-ownership-gate.json"

for f in "${RECEIPT}" "${EXTRACT}" "${SURFACE}" "${OWNGATE}" "${PROVENANCE}"; do
  [[ -f "$f" ]] || fail "missing file: $f"
done

jq empty "${RECEIPT}" "${EXTRACT}" "${SURFACE}" "${OWNGATE}"

ruby <<'RUBY'
require "json"
require "set"

PROVENANCE = "docs/json-schema/PROVENANCE.md"

# contract file => [expected contract_id, e2e verifier]
CONTRACTS = {
  "docs/json-schema/ft-deferred-proof-receipt.json" =>
    ["ft.deferred_proof_receipt.v1", "tests/e2e/test_deferred_proof_receipt_contract.sh"],
  "docs/json-schema/ft-deferred-proof-comment-extraction.json" =>
    ["ft.deferred_proof_comment_extraction.v1", "tests/e2e/test_deferred_proof_comment_extractor_contract.sh"],
  "docs/json-schema/ft-deferred-proof-queue-surface.json" =>
    ["ft.deferred_proof_queue_surface.v1", "tests/e2e/test_deferred_proof_queue_surface_contract.sh"],
  "docs/json-schema/ft-deferred-proof-ownership-gate.json" =>
    ["ft.deferred_proof_ownership_gate.v1", "tests/e2e/test_deferred_proof_ownership_gate.sh"]
}.freeze

CANONICAL_COMMAND_SHAPE = %w[rch-no-self-healing-v1 static-verifier-v1].freeze
# The coarse, derived admission vocabulary the extractor projection and the queue
# surface share. Intentionally distinct from the receipt's richer *captured*
# vocabulary (admissible / critical_pressure / no_admissible_workers /
# telemetry_gap / topology_preflight_failed), which records raw RCH signals
# before they are coarsened.
COARSE_ADMISSION = %w[admitted blocked_worker_pressure not_required unknown].freeze
RICH_ADMISSION = %w[admissible critical_pressure no_admissible_workers telemetry_gap topology_preflight_failed not_required unknown].freeze
BANNED_RAW_KEYS = %w[source_text pane_text raw_pane_text raw_pane_content].freeze

def fail!(message)
  warn "deferred proof family conformance: #{message}"
  exit 1
end

def read_json(path)
  JSON.parse(File.read(path))
rescue JSON::ParserError => error
  fail!("#{path} does not parse as JSON: #{error.message}")
end

# Every enum array anywhere in the schema that contains `token`.
def enums_containing(node, token, acc = [])
  case node
  when Hash
    if node["enum"].is_a?(Array) && node["enum"].include?(token)
      acc << node["enum"]
    end
    node.each_value { |value| enums_containing(value, token, acc) }
  when Array
    node.each { |item| enums_containing(item, token, acc) }
  end
  acc
end

# Every object that declares `properties`, paired with that property map.
def property_maps(node, acc = [])
  case node
  when Hash
    acc << node["properties"] if node["properties"].is_a?(Hash)
    node.each_value { |value| property_maps(value, acc) }
  when Array
    node.each { |item| property_maps(item, acc) }
  end
  acc
end

# Every object-schema node (one that declares `properties`) and the trail to
# it, when it does NOT pin additionalProperties:false. This is strictly
# stronger than the named-raw-key ban below: additionalProperties:false
# rejects ANY unknown field, so a future edit cannot smuggle an unvalidated
# raw-content field under a name not yet on the ban list.
def object_nodes_missing_ap_false(node, trail = "<root>", acc = [])
  case node
  when Hash
    if node["properties"].is_a?(Hash) && node["additionalProperties"] != false
      acc << trail
    end
    node.each { |key, value| object_nodes_missing_ap_false(value, "#{trail}/#{key}", acc) }
  when Array
    node.each_with_index { |item, idx| object_nodes_missing_ap_false(item, "#{trail}[#{idx}]", acc) }
  end
  acc
end

schemas = CONTRACTS.keys.to_h { |path| [path, read_json(path)] }
provenance = File.read(PROVENANCE)

# 1. Contract identity, provenance, and verifier wiring.
CONTRACTS.each do |path, (contract_id, verifier)|
  schema = schemas.fetch(path)
  found = schema.dig("properties", "contract_id", "const")
  fail!("#{path} contract const #{found.inspect} != #{contract_id.inspect}") unless found == contract_id
  base = File.basename(path)
  fail!("PROVENANCE missing row for #{base}") unless provenance.include?("`#{base}`")
  fail!("PROVENANCE missing verifier #{verifier} for #{base}") unless provenance.include?("bash #{verifier}")
  fail!("verifier #{verifier} does not exist on disk") unless File.file?(verifier)
end

# 2. Privacy invariant: no contract may declare a raw source/pane text property,
#    and any raw_pane_content_stored flag must be pinned const false.
schemas.each do |path, schema|
  property_maps(schema).each do |props|
    BANNED_RAW_KEYS.each do |banned|
      fail!("#{path} declares a raw property #{banned}") if props.key?(banned)
    end
    if props.key?("raw_pane_content_stored")
      const = props["raw_pane_content_stored"]["const"]
      fail!("#{path} raw_pane_content_stored is not pinned false (#{const.inspect})") unless const == false
    end
  end

  # Defense-in-depth: every object node must reject unknown fields outright,
  # so no raw-content/secret field can be smuggled under an unbanned name.
  offenders = object_nodes_missing_ap_false(schema)
  fail!("#{path} object nodes missing additionalProperties:false: #{offenders.join(", ")}") unless offenders.empty?
end

# 3. Command-shape vocabulary: receipt and extractor must agree on the exact two
#    command_shape_version values; the queue surface does not carry it.
{
  "docs/json-schema/ft-deferred-proof-receipt.json" => 1,
  "docs/json-schema/ft-deferred-proof-comment-extraction.json" => 1,
  "docs/json-schema/ft-deferred-proof-queue-surface.json" => 0
}.each do |path, expected_count|
  hits = enums_containing(schemas.fetch(path), "rch-no-self-healing-v1")
  fail!("#{path} command_shape enum count #{hits.length} != #{expected_count}") unless hits.length == expected_count
  hits.each do |enum|
    fail!("#{path} command_shape enum drifted: #{enum.inspect}") unless enum.sort == CANONICAL_COMMAND_SHAPE.sort
  end
end

# 4. Coarse admission vocabulary: the extractor projection and the queue surface
#    share it exactly; the receipt deliberately does NOT (it keeps the rich
#    captured vocabulary). This guards the intentional split both ways.
[
  "docs/json-schema/ft-deferred-proof-comment-extraction.json",
  "docs/json-schema/ft-deferred-proof-queue-surface.json"
].each do |path|
  hits = enums_containing(schemas.fetch(path), "blocked_worker_pressure")
  fail!("#{path} is missing the coarse admission vocabulary") if hits.empty?
  hits.each do |enum|
    fail!("#{path} coarse admission enum drifted: #{enum.inspect}") unless enum.sort == COARSE_ADMISSION.sort
  end
end

receipt = schemas.fetch("docs/json-schema/ft-deferred-proof-receipt.json")
fail!("receipt unexpectedly adopted the coarse admission vocabulary") unless enums_containing(receipt, "blocked_worker_pressure").empty?
rich = enums_containing(receipt, "critical_pressure")
fail!("receipt lost its rich captured admission vocabulary") if rich.empty?
rich.each do |enum|
  fail!("receipt rich admission enum drifted: #{enum.inspect}") unless enum.sort == RICH_ADMISSION.sort
end

puts "deferred proof family conformance: passed (#{CONTRACTS.length} contracts, command-shape + privacy + admission vocab locked)"
RUBY
