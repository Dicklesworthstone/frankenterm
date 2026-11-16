#!/usr/bin/env bash
# Static integrity guard for the redactor recall-coverage report.
#
# docs/security/redactor-coverage.json is a SECURITY-ASSURANCE artifact: it
# states the redactor's measured recall/precision and drives the per-class
# sample-size derivation (recall_floor, zero_miss vector requirement). Agents
# hand-edit it as the catalog and corpus evolve, so a stale or internally
# inconsistent edit can make it CLAIM coverage it does not have — the project's
# anti-fabrication doctrine (honest proof artifacts) requires the report to be
# arithmetically self-consistent.
#
# This guard recomputes the report's headline metrics from its own primitives
# and rejects any inconsistency. It does NOT measure recall (that is the RCH
# corpus job, ft-tf6g3.35) — it only enforces that the numbers in the file agree
# with each other. Catalog↔report class-set sync is enforced separately by
# test_redactor_streaming_anchor_coverage.sh step 8.
#
# Invariants:
#   1. overall.tp == sum(by_pattern_class[].observed_positive_vectors)
#      (every true positive is attributed to exactly one class).
#   2. overall.recall    == tp / (tp + fn)   (within float tolerance).
#      overall.precision == tp / (tp + fp).
#   3. tp, fn, fp are non-negative; vectors_total >= tp (positives <= total).
#   4. every class shares the top-level recall_floor and the derivation's
#      zero_miss_positive_vectors_required_per_class.
#   5. a class with observed < required must keep an under-sampled status — it
#      cannot be silently flipped to a passing status while still under-sampled.
#
# Static; no compilation and no RCH worker needed. Run:
#   bash tests/e2e/test_redactor_coverage_report_integrity.sh
#   bash tests/e2e/test_redactor_coverage_report_integrity.sh --self-test
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

COVERAGE="docs/security/redactor-coverage.json"

fail() {
  printf 'redactor coverage integrity: %s\n' "$*" >&2
  exit 1
}

command -v ruby >/dev/null 2>&1 || fail "missing command: ruby"

SELF_TEST=0
case "${1:-}" in
  --self-test) SELF_TEST=1 ;;
  "") ;;
  -h|--help)
    sed -n '2,40p' "${BASH_SOURCE[0]}"
    exit 0
    ;;
  *) fail "unknown option: $1" ;;
esac

[[ "${SELF_TEST}" == "1" ]] || [[ -f "${COVERAGE}" ]] || fail "missing file: ${COVERAGE}"

COVERAGE_PATH="${COVERAGE}" SELF_TEST="${SELF_TEST}" ruby <<'RUBY'
require "json"

TOL = 1e-9

# Return an array of human-readable inconsistency strings ([] == consistent).
def verify(data)
  errors = []
  overall = data["overall"]
  classes = data["by_pattern_class"]
  floor = data["sample_size_floor"]

  return ["missing overall block"] unless overall.is_a?(Hash)
  return ["missing/empty by_pattern_class"] unless classes.is_a?(Hash) && !classes.empty?
  return ["missing sample_size_floor block"] unless floor.is_a?(Hash)

  tp = overall["tp"]
  fn = overall["fn"]
  fp = overall["fp"]
  errors << "overall.tp/fn/fp must be integers" unless [tp, fn, fp].all? { |v| v.is_a?(Integer) }
  return errors unless errors.empty?

  errors << "negative count in overall (tp=#{tp} fn=#{fn} fp=#{fp})" if [tp, fn, fp].any?(&:negative?)

  # 1. tp == sum of per-class observed positives.
  sum_observed = classes.values.sum { |c| c["observed_positive_vectors"].to_i }
  errors << "overall.tp=#{tp} != sum(observed_positive_vectors)=#{sum_observed}" unless tp == sum_observed

  # 2. recall / precision recompute from primitives.
  if (tp + fn) > 0
    recall_calc = tp.to_f / (tp + fn)
    stated = overall["recall"].to_f
    errors << "overall.recall=#{stated} != tp/(tp+fn)=#{recall_calc}" if (recall_calc - stated).abs > TOL
  end
  if (tp + fp) > 0
    precision_calc = tp.to_f / (tp + fp)
    stated = overall["precision"].to_f
    errors << "overall.precision=#{stated} != tp/(tp+fp)=#{precision_calc}" if (precision_calc - stated).abs > TOL
  end

  # 3. positives cannot exceed the total vector count.
  vectors_total = data["vectors_total"]
  if vectors_total.is_a?(Integer)
    errors << "vectors_total=#{vectors_total} < overall.tp=#{tp} (positives exceed total)" if vectors_total < tp
  else
    errors << "missing/invalid vectors_total"
  end

  # 4 & 5. per-class derivation consistency.
  top_floor = data["recall_floor"]
  req = floor["zero_miss_positive_vectors_required_per_class"]
  classes.each do |name, c|
    observed = c["observed_positive_vectors"]
    required = c["required_positive_vectors"]
    errors << "#{name}: observed_positive_vectors must be a non-negative integer" unless observed.is_a?(Integer) && observed >= 0
    errors << "#{name}: recall_floor=#{c["recall_floor"]} != top recall_floor=#{top_floor}" if top_floor && c["recall_floor"] != top_floor
    errors << "#{name}: required_positive_vectors=#{required} != derivation floor=#{req}" if req && required != req
    if observed.is_a?(Integer) && required.is_a?(Integer) && observed < required
      status = c["status"].to_s
      errors << "#{name}: observed (#{observed}) < required (#{required}) but status=#{status.inspect} is not under-sampled" unless status == "under_sampled"
    end
  end

  errors
end

if ENV.fetch("SELF_TEST") == "1"
  # A known-good minimal report must verify clean.
  good = {
    "overall" => { "tp" => 6, "fn" => 0, "fp" => 0, "recall" => 1.0, "precision" => 1.0 },
    "by_pattern_class" => {
      "alpha" => { "observed_positive_vectors" => 3, "required_positive_vectors" => 459, "recall_floor" => 0.99, "status" => "under_sampled" },
      "beta"  => { "observed_positive_vectors" => 3, "required_positive_vectors" => 459, "recall_floor" => 0.99, "status" => "under_sampled" }
    },
    "sample_size_floor" => { "zero_miss_positive_vectors_required_per_class" => 459 },
    "vectors_total" => 10,
    "recall_floor" => 0.99
  }
  ge = verify(good)
  unless ge.empty?
    warn "redactor coverage integrity: self-test FAILED — known-good report rejected: #{ge.inspect}"
    exit 1
  end

  # Each tamper must be rejected; key = label, value = mutation lambda.
  tampers = {
    "tp != sum"            => ->(d) { d["overall"]["tp"] = 5; d["overall"]["recall"] = 5.0 / 5 },
    "recall mismatch"      => ->(d) { d["overall"]["fn"] = 2; d["overall"]["recall"] = 1.0 },
    "precision mismatch"   => ->(d) { d["overall"]["fp"] = 3; d["overall"]["precision"] = 1.0 },
    "positives > total"    => ->(d) { d["vectors_total"] = 5 },
    "per-class floor drift" => ->(d) { d["by_pattern_class"]["alpha"]["required_positive_vectors"] = 100 },
    "status fabrication"   => ->(d) { d["by_pattern_class"]["alpha"]["status"] = "met" }
  }
  tampers.each do |label, mutate|
    d = JSON.parse(JSON.generate(good))
    mutate.call(d)
    if verify(d).empty?
      warn "redactor coverage integrity: self-test FAILED — tamper #{label.inspect} not rejected"
      exit 1
    end
  end
  puts "redactor coverage integrity: self-test passed (known-good report accepted; #{tampers.length} tampers all rejected)"
  exit 0
end

data = JSON.parse(File.read(ENV.fetch("COVERAGE_PATH")))
errors = verify(data)
unless errors.empty?
  warn "redactor coverage integrity: #{errors.length} inconsistency(ies) in #{ENV.fetch("COVERAGE_PATH")}:"
  errors.each { |e| warn "  - #{e}" }
  exit 1
end

tp = data["overall"]["tp"]
n = data["by_pattern_class"].length
puts "redactor coverage integrity: passed (tp=#{tp} == sum over #{n} classes; recall/precision recompute clean; per-class floors consistent)"
RUBY
