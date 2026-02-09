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
# 1. Feature exclusion: tui + ftui must not compile together
# ---------------------------------------------------------------------------
echo "--- Check 1: Mutual exclusion (tui + ftui) ---"

if cargo check -p wa-core --features tui,ftui >/dev/null 2>&1; then
    fail "tui + ftui compiled successfully — compile_error! guard is missing or broken"
else
    pass "tui + ftui correctly fails to compile"
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

# Files that are exclusively ftui (must not import ratatui or crossterm
# outside of cfg(feature = "tui") blocks).
#
# EXCEPTION LIST: These files are allowed to contain ratatui/crossterm
# imports because they are part of the compatibility layer:
#   - tui/ftui_compat.rs (the adapter itself, with cfg-gated impls)
#   - tui/terminal_session.rs (CrosstermSession is cfg-gated)
#   - tui/mod.rs (conditional module imports)
#   - tui/app.rs (legacy ratatui backend, only compiled under tui feature)
#   - tui/views.rs (legacy ratatui backend, only compiled under tui feature)
#
# This check targets modules that should be framework-agnostic:

FTUI_AGNOSTIC_FILES=(
    "crates/wa-core/src/tui/query.rs"
    "crates/wa-core/src/tui/ftui_stub.rs"
)

for file in "${FTUI_AGNOSTIC_FILES[@]}"; do
    if [ ! -f "$file" ]; then
        skip "$file does not exist"
        continue
    fi

    # Check for bare ratatui/crossterm imports (not inside cfg blocks)
    # We grep for use statements and direct type references
    if grep -n 'use ratatui' "$file" | grep -v '#\[cfg' >/dev/null 2>&1; then
        fail "$file contains bare ratatui import (not cfg-gated)"
    elif grep -n 'use crossterm' "$file" | grep -v '#\[cfg' >/dev/null 2>&1; then
        fail "$file contains bare crossterm import (not cfg-gated)"
    else
        pass "$file is framework-agnostic (no bare ratatui/crossterm imports)"
    fi
done

# Check that view_adapters.rs (if present) is also agnostic
if [ -f "crates/wa-core/src/tui/view_adapters.rs" ]; then
    if grep -n 'use ratatui' "crates/wa-core/src/tui/view_adapters.rs" | grep -v '#\[cfg' >/dev/null 2>&1; then
        fail "view_adapters.rs contains bare ratatui import"
    else
        pass "view_adapters.rs is framework-agnostic"
    fi
fi

echo ""

# ---------------------------------------------------------------------------
# 4. Clippy pass for both features
# ---------------------------------------------------------------------------
echo "--- Check 4: Clippy for both features ---"

for feature in tui ftui; do
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
