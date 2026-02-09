#!/usr/bin/env bash
# check_ftui_guardrails.sh — Build guardrails for the FTUI migration.
#
# Prevents accidental dual-stack drift by enforcing:
#   1. Feature exclusion: `--features tui,ftui` must fail to compile
#   2. Import isolation: ftui-only modules must not import ratatui/crossterm
#   3. Feature matrix: both `tui` and `ftui` compile independently
#
# Implements: wa-eutd (FTUI-02.4)
# Deletion: Remove when the `tui` feature is dropped (FTUI-09.3).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_ROOT"

PASS=0
FAIL=0
SKIP=0

pass() { PASS=$((PASS + 1)); echo "  PASS: $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  FAIL: $1" >&2; }
skip() { SKIP=$((SKIP + 1)); echo "  SKIP: $1"; }

echo "=== FTUI Migration Guardrails ==="
echo ""

# ---------------------------------------------------------------------------
# 1. Feature exclusion: tui + ftui must not compile together (unless rollout)
# ---------------------------------------------------------------------------
echo "--- Check 1: Mutual exclusion (tui + ftui) ---"

if cargo check -p wa-core --features tui,ftui >/dev/null 2>&1; then
    fail "tui + ftui compiled successfully — compile_error! guard is missing or broken"
else
    pass "tui + ftui correctly fails to compile"
fi

if cargo check -p wa-core --features rollout >/dev/null 2>&1; then
    pass "rollout (tui + ftui) compiles with runtime dispatch"
else
    fail "rollout feature does not compile"
fi

echo ""

# ---------------------------------------------------------------------------
# 2. Individual feature compilation
# ---------------------------------------------------------------------------
echo "--- Check 2: Individual feature compilation ---"

for feature in tui ftui; do
    if cargo check -p wa-core --features "$feature" >/dev/null 2>&1; then
        pass "--features $feature compiles"
    else
        fail "--features $feature does not compile"
    fi
done

if cargo check -p wa-core >/dev/null 2>&1; then
    pass "default (no features) compiles"
else
    fail "default (no features) does not compile"
fi

echo ""

# ---------------------------------------------------------------------------
# 3. Import isolation: ftui-only modules must not reference ratatui/crossterm
# ---------------------------------------------------------------------------
echo "--- Check 3: Import isolation ---"

# Migration-complete modules that MUST NOT reference ratatui or crossterm
# outside of cfg(feature = "tui") blocks.
#
# ALLOWLIST (files permitted to contain ratatui/crossterm):
#   - tui/ftui_compat.rs   — compatibility adapter with cfg-gated conversions
#   - tui/terminal_session.rs — CrosstermSession impl is cfg-gated
#   - tui/mod.rs           — conditional module imports
#   - tui/app.rs           — legacy ratatui backend (compiled only under tui)
#   - tui/views.rs         — legacy ratatui backend (compiled only under tui)
#
# To add an exception: add the file path to the allowlist above AND file a
# bead explaining why the exception is needed and when it expires.
#
# DEVELOPER GUIDANCE: If this check fails for your module:
#   1. Replace `ratatui::` types with equivalents from `tui::ftui_compat`
#   2. Replace `crossterm::` types with `tui::ftui_compat::InputEvent` etc.
#   3. Use `ftui::` directly for FrankenTUI-native code
#   4. If a conversion is genuinely needed, add it to ftui_compat.rs with
#      a `#[cfg(feature = "tui")]` gate

FTUI_AGNOSTIC_FILES=(
    "crates/wa-core/src/tui/query.rs"
    "crates/wa-core/src/tui/ftui_stub.rs"
    "crates/wa-core/src/tui/view_adapters.rs"
    "crates/wa-core/src/tui/keymap.rs"
    "crates/wa-core/src/tui/state.rs"
    "crates/wa-core/src/tui/command_handoff.rs"
    "crates/wa-core/src/tui/output_gate.rs"
)

# Forbidden patterns: bare (non-cfg-gated) references to ratatui or crossterm.
# Matches `use ratatui`, `use crossterm`, `ratatui::`, `crossterm::` but
# excludes lines that are inside cfg attributes or doc comments.
FORBIDDEN_PATTERNS='(use ratatui|use crossterm|ratatui::|crossterm::)'

for file in "${FTUI_AGNOSTIC_FILES[@]}"; do
    if [ ! -f "$file" ]; then
        skip "$file does not exist"
        continue
    fi

    basename=$(basename "$file")

    # Check for forbidden patterns, excluding cfg-gated lines and comments
    violations=$(grep -nE "$FORBIDDEN_PATTERNS" "$file" \
        | grep -v '#\[cfg' \
        | grep -v '^ *///' \
        | grep -v '^ *//' \
        || true)

    if [ -n "$violations" ]; then
        fail "$basename contains bare ratatui/crossterm reference (not cfg-gated):"
        echo "$violations" | while read -r line; do
            echo "    $line"
        done
    else
        pass "$basename is framework-agnostic"
    fi
done

echo ""

# ---------------------------------------------------------------------------
# 4. Clippy pass for both features
# ---------------------------------------------------------------------------
echo "--- Check 4: Clippy for both features ---"

for feature in tui ftui rollout; do
    if cargo clippy -p wa-core --features "$feature" -- -D warnings >/dev/null 2>&1; then
        pass "clippy --features $feature passes"
    else
        fail "clippy --features $feature has warnings/errors"
    fi
done

echo ""

# ---------------------------------------------------------------------------
# 5. Test presence: snapshot and E2E tests must exist in ftui_stub.rs
# Implements: wa-36xw (FTUI-07.4)
# ---------------------------------------------------------------------------
echo "--- Check 5: FTUI test presence ---"

FTUI_STUB="crates/wa-core/src/tui/ftui_stub.rs"
if [ -f "$FTUI_STUB" ]; then
    SNAPSHOT_FNS=$(grep -c 'fn snapshot_' "$FTUI_STUB" || true)
    E2E_FNS=$(grep -c 'fn e2e_' "$FTUI_STUB" || true)

    if [ "$SNAPSHOT_FNS" -ge 20 ]; then
        pass "Snapshot tests present ($SNAPSHOT_FNS functions)"
    else
        fail "Snapshot tests missing or below 20 (found $SNAPSHOT_FNS)"
    fi

    if [ "$E2E_FNS" -ge 10 ]; then
        pass "E2E tests present ($E2E_FNS functions)"
    else
        fail "E2E tests missing or below 10 (found $E2E_FNS)"
    fi
else
    fail "ftui_stub.rs not found"
fi

echo ""

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo "=== Summary ==="
echo "  Passed: $PASS"
echo "  Failed: $FAIL"
echo "  Skipped: $SKIP"

if [ "$FAIL" -gt 0 ]; then
    echo ""
    echo "FTUI guardrail check FAILED — see errors above."
    exit 1
else
    echo ""
    echo "All FTUI guardrails passed."
    exit 0
fi
