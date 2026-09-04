#!/usr/bin/env bash
# Exercise the actual selector with isolated script fixtures; no Cargo runs.
# Fixtures and logs are retained for diagnosis, in accordance with AGENTS.md.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
RUN_DIR=$(mktemp -d "${TMPDIR:-/tmp}/ft-release-selection.XXXXXXXX")
FIXTURE="$RUN_DIR/fixture"
mkdir -p "$FIXTURE/scripts" "$FIXTURE/docs/security"
cp "$ROOT/scripts/release-gates.sh" "$FIXTURE/scripts/release-gates.sh"
count=0

expect_exit() {
  local expected="$1" label="$2" actual=0
  shift 2
  count=$((count + 1))
  bash "$FIXTURE/scripts/release-gates.sh" "$@" >"$RUN_DIR/$count.log" 2>&1 || actual=$?
  if [[ "$actual" -ne "$expected" ]]; then
    printf 'FAIL %s expected=%s actual=%s log=%s\n' "$label" "$expected" "$actual" "$RUN_DIR/$count.log" >&2
    cat "$RUN_DIR/$count.log" >&2
    exit 1
  fi
  printf 'PASS %s exit=%s\n' "$label" "$actual"
}

expect_exit 2 unknown --only nonexistent-gate
expect_exit 2 missing-name --only
expect_exit 2 empty-name --only ''
expect_exit 2 cargo-excluded --only 'workspace cycles'
expect_exit 1 missing-required --only 'asupersync test-only doctrine'
expect_exit 1 missing-interpreted --only 'release panic contract (profiles)'

cat >"$FIXTURE/scripts/check_asupersync_test_only.sh" <<'SH'
#!/usr/bin/env bash
printf 'ran\n' >> ran-gate
exit 0
SH
expect_exit 1 non-executable --only 'asupersync test-only doctrine'
chmod +x "$FIXTURE/scripts/check_asupersync_test_only.sh"
expect_exit 2 mixed-unknown --only 'asupersync test-only doctrine' --only nonexistent-gate
[[ ! -e "$FIXTURE/ran-gate" ]] || { echo 'mixed-invalid selection executed a gate' >&2; exit 1; }
expect_exit 1 mixed-missing --only 'asupersync test-only doctrine' --only 'runtime_compat residuals'
[[ ! -e "$FIXTURE/ran-gate" ]] || { echo 'missing prerequisite partially executed gates' >&2; exit 1; }
expect_exit 0 list --list --only 'asupersync test-only doctrine'
[[ ! -e "$FIXTURE/ran-gate" ]] || { echo 'listing executed a gate' >&2; exit 1; }
expect_exit 0 positive --only 'asupersync test-only doctrine'
[[ $(wc -l <"$FIXTURE/ran-gate") -eq 1 ]] || exit 1
expect_exit 0 duplicate-selection --only 'asupersync test-only doctrine' --only 'asupersync test-only doctrine'
[[ $(wc -l <"$FIXTURE/ran-gate") -eq 2 ]] || exit 1

cat >"$FIXTURE/scripts/check_runtime_compat_residuals.sh" <<'SH'
#!/usr/bin/env bash
echo 'intentional gate failure' >&2
exit 7
SH
chmod +x "$FIXTURE/scripts/check_runtime_compat_residuals.sh"
expect_exit 1 gate-failure --only 'runtime_compat residuals'
for duration in 0 -1 abc 1.5 ''; do
  expect_exit 2 invalid-fuzz-duration --fuzz-campaign "$duration"
done
expect_exit 2 fuzz-excluded --only 'asupersync test-only doctrine' --fuzz-campaign 1
expect_exit 1 missing-fuzz-manifest --cargo --only 'asupersync test-only doctrine' --fuzz-campaign 1
printf '{"targets":[]}\n' >"$FIXTURE/docs/security/adversarial-contract-fuzz.json"
expect_exit 1 empty-fuzz-manifest --cargo --only 'asupersync test-only doctrine' --fuzz-campaign 1
printf '{malformed\n' >"$FIXTURE/docs/security/adversarial-contract-fuzz.json"
expect_exit 1 malformed-fuzz-manifest --cargo --only 'asupersync test-only doctrine' --fuzz-campaign 1
printf '{"targets":[{"cargo_fuzz_target":"safe_target","seed_corpus":"fuzz/missing/corpus/"}]}\n' >"$FIXTURE/docs/security/adversarial-contract-fuzz.json"
expect_exit 1 missing-fuzz-corpus --cargo --only 'asupersync test-only doctrine' --fuzz-campaign 1
[[ $(wc -l <"$FIXTURE/ran-gate") -eq 2 ]] || { echo 'invalid fuzz request executed a gate' >&2; exit 1; }

# A production static gate is the positive control, beyond selector fixtures.
bash "$ROOT/scripts/release-gates.sh" --only 'release panic contract (profiles)' >"$RUN_DIR/production.log" 2>&1
rg -q '^PASS release panic contract' "$RUN_DIR/production.log"
printf 'RELEASE_GATE_SELECTION_SUCCESS cases=%s production=passed artifacts=%s\n' "$count" "$RUN_DIR"
