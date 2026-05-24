#!/usr/bin/env bash
# Family-integrity guard for the deferred RCH proof replay contract family
# (ft-zbnz4). The five contracts (receipt, comment-extractor, ownership-gate,
# queue-surface, replay-harness) each carry a manifest, a JSON schema, a robot
# contract doc, fixtures, and a static verifier. This guard fences the drift
# class that left the verifiers silently unrun for a while: it asserts that
#   * every manifest's referenced files actually exist on disk,
#   * every family verifier is executable AND wired into CI (ci.yml), and
#   * every family JSON schema has a PROVENANCE.md row.
# Pure bash + jq; no compilation, no RCH. Runs green while the remote proof lane
# (ft-4tp7g) is down.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

CI_WORKFLOW=".github/workflows/ci.yml"
PROVENANCE="docs/json-schema/PROVENANCE.md"
MANIFEST_GLOB="fixtures/deferred-proof-replay"
SCHEMA_GLOB="docs/json-schema"

fail() {
  printf 'deferred proof family integrity: %s\n' "$*" >&2
  exit 1
}

command -v jq >/dev/null 2>&1 || fail "missing command: jq"
[[ -f "${CI_WORKFLOW}" ]] || fail "missing CI workflow: ${CI_WORKFLOW}"
[[ -f "${PROVENANCE}" ]] || fail "missing provenance: ${PROVENANCE}"

# --- A. Every family verifier is executable and wired into CI ------------
mapfile -t verifiers < <(find tests/e2e -maxdepth 1 -name 'test_deferred_proof_*.sh' | sort)
[[ "${#verifiers[@]}" -ge 5 ]] || fail "expected >=5 family verifiers, found ${#verifiers[@]}"

for v in "${verifiers[@]}"; do
  [[ -f "${v}" ]] || fail "verifier missing: ${v}"
  [[ -x "${v}" ]] || fail "verifier not executable: ${v}"
  base="$(basename "${v}")"
  grep -Fq "${base}" "${CI_WORKFLOW}" || fail "verifier not wired into CI: ${base} (add it to ${CI_WORKFLOW})"
done

# --- B. Every manifest's referenced files exist --------------------------
mapfile -t manifests < <(find "${MANIFEST_GLOB}" -name manifest.json | sort)
[[ "${#manifests[@]}" -ge 5 ]] || fail "expected >=5 manifests, found ${#manifests[@]}"

for m in "${manifests[@]}"; do
  jq empty "${m}" || fail "manifest does not parse: ${m}"
  cid="$(jq -r '.contract_id // empty' "${m}")"
  [[ -n "${cid}" ]] || fail "manifest missing contract_id: ${m}"

  # Any string value that looks like a repo path (a slash, a known extension,
  # and no whitespace) must resolve to a real file. Heterogeneous key spellings
  # across the family (schema vs schema_path, contract vs contract_doc) are
  # handled by not caring about the key name — only the value shape. The
  # whitespace exclusion skips embedded command strings (e.g. a `verification`
  # field holding `jq empty a.json b.json`), which are not file paths.
  while IFS= read -r path; do
    [[ -f "${path}" ]] || fail "manifest ${m} references missing file: ${path}"
  done < <(jq -r '.. | strings | select(test("^[^[:space:]]+$") and test("/") and test("\\.(json|jsonl|md|sh)$"))' "${m}")
done

# --- C. Every family schema has a PROVENANCE row -------------------------
mapfile -t schemas < <(find "${SCHEMA_GLOB}" -maxdepth 1 -name 'ft-deferred-proof-*.json' | sort)
[[ "${#schemas[@]}" -ge 5 ]] || fail "expected >=5 family schemas, found ${#schemas[@]}"

for s in "${schemas[@]}"; do
  jq empty "${s}" || fail "schema does not parse: ${s}"
  base="$(basename "${s}")"
  id="$(jq -r '."$id" // empty' "${s}")"
  [[ "${id}" == *"/${base}" ]] || fail "schema \$id does not end with its filename: ${s} (\$id=${id})"
  grep -Fq "\`${base}\`" "${PROVENANCE}" || fail "schema has no PROVENANCE row: ${base}"
done

printf 'deferred proof family integrity: passed (%d verifiers wired, %d manifests resolved, %d schemas provenanced)\n' \
  "${#verifiers[@]}" "${#manifests[@]}" "${#schemas[@]}"
