#!/usr/bin/env bash
# Static verifier for the EvidenceLifecycleReceipt delimiter-injection golden.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

FIXTURE="fixtures/incident-bundles/evidence-lifecycle-receipt/cases.v1.json"
SOURCE="crates/frankenterm-core/src/incident_bundle.rs"

fail() {
  printf 'incident evidence_lifecycle receipt canonicalization: %s\n' "$*" >&2
  exit 1
}

command -v jq >/dev/null 2>&1 || fail "missing command: jq"
command -v ruby >/dev/null 2>&1 || fail "missing command: ruby"
[[ -f "${FIXTURE}" ]] || fail "missing fixture: ${FIXTURE}"
[[ -f "${SOURCE}" ]] || fail "missing source: ${SOURCE}"

jq empty "${FIXTURE}"

FIXTURE="${FIXTURE}" SOURCE="${SOURCE}" ruby <<'RUBY'
require "digest"
require "json"

FIXTURE = ENV.fetch("FIXTURE")
SOURCE = ENV.fetch("SOURCE")
FIXTURE_CONTRACT_ID = "ft.incident_bundle.evidence_lifecycle_receipt_canonicalization.fixtures.v1"
PRODUCING_BEAD = "ft-cjmla"
RELATED_BEAD = "ft-womif"
RECEIPT_CONTRACT_ID = "ft.evidence_lifecycle.v1"
RECEIPT_SCHEMA_VERSION = 1
RECEIPT_ID_DOMAIN = "ft.evidence_lifecycle.receipt_id.v2"
RECEIPT_ID_PREFIX = "evidence:lifecycle:"
RECEIPT_ID_DIGEST_HEX_CHARS = 32
REQUIRED_CASES = ["delimiter-injection-collision"].freeze
CANONICAL_FIELD_ORDER = [
  "domain",
  "contract_id",
  "schema_version",
  "generated_at_ms",
  "decisions_len",
  "decision.evidence_id",
  "decision.kind",
  "decision.action",
  "decision.reason",
  "decision.expiry_tag",
  "decision.expiry_value_when_some",
].freeze

def fail!(message)
  warn "incident evidence_lifecycle receipt canonicalization: #{message}"
  exit 1
end

def read_json(path)
  JSON.parse(File.read(path))
rescue JSON::ParserError => error
  fail!("#{path} does not parse as JSON: #{error.message}")
end

def sha256_hex(bytes)
  Digest::SHA256.hexdigest(bytes)
end

def legacy_payload(receipt_contract, generated_at_ms, decisions)
  payload = +"#{receipt_contract.fetch("contract_id")}\n#{receipt_contract.fetch("schema_version")}\n#{generated_at_ms}"
  decisions.each do |decision|
    payload << "\n"
    payload << decision.fetch("evidence_id")
    payload << "|"
    payload << decision.fetch("kind")
    payload << "|"
    payload << decision.fetch("action")
    payload << "|"
    payload << decision.fetch("reason")
    payload << "|"
    expiry = decision.fetch("effective_expires_at_ms")
    payload << expiry.to_s unless expiry.nil?
  end
  payload.b
end

def append_receipt_id_field(payload, field)
  bytes = field.to_s.b
  payload << bytes.bytesize.to_s
  payload << ":"
  payload << bytes
  payload << ";"
end

def framed_payload(receipt_contract, generated_at_ms, decisions, header_order: nil)
  fields_by_name = {
    "domain" => receipt_contract.fetch("receipt_id_domain"),
    "contract_id" => receipt_contract.fetch("contract_id"),
    "schema_version" => receipt_contract.fetch("schema_version").to_s,
    "generated_at_ms" => generated_at_ms.to_s,
    "decisions_len" => decisions.length.to_s,
  }
  order = header_order || %w[domain contract_id schema_version generated_at_ms decisions_len]

  payload = +"".b
  order.each { |name| append_receipt_id_field(payload, fields_by_name.fetch(name)) }
  decisions.each do |decision|
    append_receipt_id_field(payload, decision.fetch("evidence_id"))
    append_receipt_id_field(payload, decision.fetch("kind"))
    append_receipt_id_field(payload, decision.fetch("action"))
    append_receipt_id_field(payload, decision.fetch("reason"))
    expiry = decision.fetch("effective_expires_at_ms")
    if expiry.nil?
      append_receipt_id_field(payload, "none")
    else
      append_receipt_id_field(payload, "some")
      append_receipt_id_field(payload, expiry.to_s)
    end
  end
  payload
end

def verify_projection!(case_id, side, receipt_contract, generated_at_ms, projection)
  legacy = legacy_payload(receipt_contract, generated_at_ms, projection.fetch("decisions"))
  framed = framed_payload(receipt_contract, generated_at_ms, projection.fetch("decisions"))
  framed_digest = sha256_hex(framed)

  fail!("#{case_id}/#{side}: legacy payload hex drifted") unless projection.fetch("legacy_payload_hex") == legacy.unpack1("H*")
  fail!("#{case_id}/#{side}: legacy payload digest drifted") unless projection.fetch("legacy_payload_sha256") == sha256_hex(legacy)
  fail!("#{case_id}/#{side}: framed payload hex drifted") unless projection.fetch("framed_payload_hex") == framed.unpack1("H*")
  fail!("#{case_id}/#{side}: framed payload digest drifted") unless projection.fetch("framed_payload_sha256") == framed_digest

  expected_receipt_id = RECEIPT_ID_PREFIX + framed_digest[0, RECEIPT_ID_DIGEST_HEX_CHARS]
  fail!("#{case_id}/#{side}: receipt_id drifted") unless projection.fetch("receipt_id") == expected_receipt_id
end

fixture = read_json(FIXTURE)

fail!("schema_version must be 1") unless fixture.fetch("schema_version") == 1
fail!("contract_id drifted") unless fixture.fetch("contract_id") == FIXTURE_CONTRACT_ID
fail!("producing_bead drifted") unless fixture.fetch("producing_bead") == PRODUCING_BEAD
fail!("related_bead drifted") unless fixture.fetch("related_bead") == RELATED_BEAD
fail!("purpose missing") unless fixture.fetch("purpose").is_a?(String) && !fixture.fetch("purpose").empty?

golden_confidence = fixture.fetch("golden_confidence")
fail!("golden artifact_kind drifted") unless golden_confidence.fetch("artifact_kind") == "structural_json_golden"
fail!("golden must be deterministic") unless golden_confidence.fetch("deterministic") == true
fail!("golden must be platform-independent") unless golden_confidence.fetch("platform_dependent") == false
fail!("golden volatility must stay low") unless (1..2).cover?(golden_confidence.fetch("volatility"))

receipt_contract = fixture.fetch("receipt_contract")
fail!("receipt contract_id drifted") unless receipt_contract.fetch("contract_id") == RECEIPT_CONTRACT_ID
fail!("receipt schema_version drifted") unless receipt_contract.fetch("schema_version") == RECEIPT_SCHEMA_VERSION
fail!("receipt_id_prefix drifted") unless receipt_contract.fetch("receipt_id_prefix") == RECEIPT_ID_PREFIX
fail!("receipt_id_domain drifted") unless receipt_contract.fetch("receipt_id_domain") == RECEIPT_ID_DOMAIN
fail!("framing contract drifted") unless receipt_contract.fetch("framing") == "decimal-byte-length-colon-field-semicolon"
fail!("digest contract drifted") unless receipt_contract.fetch("digest") == "sha256"
fail!("receipt id digest truncation drifted") unless receipt_contract.fetch("receipt_id_digest_hex_chars") == RECEIPT_ID_DIGEST_HEX_CHARS

cases = fixture.fetch("cases")
case_ids = cases.map { |entry| entry.fetch("case_id") }.sort
fail!("case set drifted: #{case_ids.inspect}") unless case_ids == REQUIRED_CASES.sort
fail!("required_cases drifted") unless fixture.fetch("required_cases").sort == REQUIRED_CASES.sort

cases.each do |entry|
  case_id = entry.fetch("case_id")
  generated_at_ms = entry.fetch("generated_at_ms")
  control = entry.fetch("control")
  attack = entry.fetch("attack")
  expected = entry.fetch("expected")

  verify_projection!(case_id, "control", receipt_contract, generated_at_ms, control)
  verify_projection!(case_id, "attack", receipt_contract, generated_at_ms, attack)

  control_legacy = legacy_payload(receipt_contract, generated_at_ms, control.fetch("decisions"))
  attack_legacy = legacy_payload(receipt_contract, generated_at_ms, attack.fetch("decisions"))
  control_framed = framed_payload(receipt_contract, generated_at_ms, control.fetch("decisions"))
  attack_framed = framed_payload(receipt_contract, generated_at_ms, attack.fetch("decisions"))

  fail!("#{case_id}: test case does not contain distinct decision sets") if JSON.generate(control.fetch("decisions")) == JSON.generate(attack.fetch("decisions"))
  fail!("#{case_id}: expected legacy collision no longer collides") unless control_legacy == attack_legacy
  fail!("#{case_id}: expected framed payload split did not happen") if control_framed == attack_framed
  fail!("#{case_id}: legacy digest collision drifted") unless control.fetch("legacy_payload_sha256") == attack.fetch("legacy_payload_sha256")
  fail!("#{case_id}: framed digest collision still present") if control.fetch("framed_payload_sha256") == attack.fetch("framed_payload_sha256")
  fail!("#{case_id}: receipt_id collision still present") if control.fetch("receipt_id") == attack.fetch("receipt_id")

  fail!("#{case_id}: expected legacy_payloads_collide must be true") unless expected.fetch("legacy_payloads_collide") == true
  fail!("#{case_id}: expected framed_payloads_collide must be false") unless expected.fetch("framed_payloads_collide") == false
  fail!("#{case_id}: expected receipt_ids_collide must be false") unless expected.fetch("receipt_ids_collide") == false
  fail!("#{case_id}: expected receipt_id_prefix drifted") unless expected.fetch("receipt_id_prefix") == RECEIPT_ID_PREFIX
  fail!("#{case_id}: canonical field order drifted") unless expected.fetch("canonical_field_order") == CANONICAL_FIELD_ORDER

  wrong_header = %w[domain schema_version contract_id generated_at_ms decisions_len]
  wrong_order_digest = sha256_hex(framed_payload(receipt_contract, generated_at_ms, control.fetch("decisions"), header_order: wrong_header))
  fail!("#{case_id}: canonical digest guard does not reject header reordering") if wrong_order_digest == control.fetch("framed_payload_sha256")
end

source = File.read(SOURCE)
impl_start = source.index("impl EvidenceLifecycleReceipt")
helper_start = source.index("fn append_receipt_id_field")
fail!("source missing EvidenceLifecycleReceipt impl") if impl_start.nil?
fail!("source missing length-prefixed framing helper") if helper_start.nil?
receipt_impl = source[impl_start...helper_start]

fail!("source missing v2 receipt-id domain separator") unless receipt_impl.include?(RECEIPT_ID_DOMAIN)
fail!("source no longer calls append_receipt_id_field enough to cover the receipt payload") if receipt_impl.scan("append_receipt_id_field(&mut payload").length < 10
fail!("source reintroduced delimiter push in receipt hashing impl") if receipt_impl.include?("payload.push('|')")
fail!("source reintroduced write!-assembled receipt payload") if receipt_impl.include?("write!(&mut payload")

puts "incident evidence_lifecycle receipt canonicalization: static verifier passed (#{cases.length} golden case)"
RUBY
