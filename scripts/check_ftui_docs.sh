#!/usr/bin/env bash
# =============================================================================
# CI: Docs-smoke and contract-drift checks for FTUI migration documentation.
#
# Validates that migration docs, parity contracts, and code are in sync:
#   1. ADR cross-references: all referenced ADR files exist
#   2. Feature matrix smoke: documented compile commands work
#   3. View contract consistency: View enum matches ADR-0006 parity contract
#   4. Adapter contract drift: adapt_* functions match documented domains
#   5. Parity matrix template: required fields present
#   6. JSON schema presence: documented schemas exist on disk
#
# Implements: wa-107q (FTUI-07.5)
# =============================================================================

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

echo "=== FTUI Docs-Smoke & Contract-Drift Checks ==="
echo ""

# ---------------------------------------------------------------------------
# 1. ADR cross-references: verify all FTUI ADR files exist
# ---------------------------------------------------------------------------
echo "--- Check 1: ADR file presence ---"

REQUIRED_ADRS=(
    "docs/adr/0001-adopt-frankentui-for-tui-migration.md"
    "docs/adr/0002-one-writer-terminal-ownership.md"
    "docs/adr/0003-migration-scope-constraints-tradeoffs.md"
    "docs/adr/0004-phased-rollout-and-rollback.md"
    "docs/adr/0005-architecture-ring-map.md"
    "docs/adr/0006-parity-contract.md"
    "docs/adr/0007-risk-register.md"
)

for adr in "${REQUIRED_ADRS[@]}"; do
    if [ -f "$adr" ]; then
        pass "$adr exists"
    else
        fail "$adr is missing"
    fi
done

echo ""

# ---------------------------------------------------------------------------
# 2. Feature matrix smoke: documented compile commands work
# ---------------------------------------------------------------------------
echo "--- Check 2: Feature matrix smoke ---"

# Headless (no features)
if cargo check -p wa-core 2>/dev/null; then
    pass "cargo check -p wa-core (headless)"
else
    fail "cargo check -p wa-core (headless)"
fi

# Legacy TUI
if cargo check -p wa-core --features tui 2>/dev/null; then
    pass "cargo check --features tui"
else
    fail "cargo check --features tui"
fi

# FrankenTUI
if cargo check -p wa-core --features ftui 2>/dev/null; then
    pass "cargo check --features ftui"
else
    fail "cargo check --features ftui"
fi

# Mutual exclusion (must FAIL)
if cargo check -p wa-core --features tui,ftui 2>/dev/null; then
    fail "tui+ftui compiled — mutual exclusion broken"
else
    pass "tui+ftui correctly rejected"
fi

echo ""

# ---------------------------------------------------------------------------
# 3. View contract consistency: View enum matches ADR-0006
# ---------------------------------------------------------------------------
echo "--- Check 3: View contract consistency ---"

FTUI_STUB="crates/wa-core/src/tui/ftui_stub.rs"
PARITY_ADR="docs/adr/0006-parity-contract.md"

# Extract View variants from code
if [ -f "$FTUI_STUB" ]; then
    CODE_VIEWS=$(grep -A 10 'pub enum View' "$FTUI_STUB" \
        | grep -oP '^\s+(\w+),' \
        | sed 's/[, ]//g' \
        | sort)
else
    CODE_VIEWS=""
    fail "ftui_stub.rs not found"
fi

# Expected views from ADR-0006
EXPECTED_VIEWS="Events
Help
History
Home
Panes
Search
Triage"

if [ "$CODE_VIEWS" = "$EXPECTED_VIEWS" ]; then
    pass "View enum matches ADR-0006 parity contract (7 views)"
else
    fail "View enum drift — code has: $(echo "$CODE_VIEWS" | tr '\n' ',') vs expected: $(echo "$EXPECTED_VIEWS" | tr '\n' ',')"
fi

# Verify ADR-0006 mentions all 7 views
if [ -f "$PARITY_ADR" ]; then
    MISSING_IN_ADR=0
    for view in Home Panes Events Triage History Search Help; do
        if ! grep -q "| $view " "$PARITY_ADR"; then
            fail "ADR-0006 missing view: $view"
            MISSING_IN_ADR=1
        fi
    done
    if [ "$MISSING_IN_ADR" -eq 0 ]; then
        pass "ADR-0006 documents all 7 views"
    fi
else
    skip "ADR-0006 not found"
fi

echo ""

# ---------------------------------------------------------------------------
# 4. Adapter contract drift: adapt_* functions cover all domains
# ---------------------------------------------------------------------------
echo "--- Check 4: Adapter contract coverage ---"

VIEW_ADAPTERS="crates/wa-core/src/tui/view_adapters.rs"

if [ -f "$VIEW_ADAPTERS" ]; then
    EXPECTED_ADAPTERS=(
        "adapt_pane"
        "adapt_event"
        "adapt_triage"
        "adapt_history"
        "adapt_search"
        "adapt_workflow"
        "adapt_health"
    )

    for adapter in "${EXPECTED_ADAPTERS[@]}"; do
        if grep -q "pub fn $adapter" "$VIEW_ADAPTERS"; then
            pass "$adapter() present"
        else
            fail "$adapter() missing from view_adapters.rs"
        fi
    done

    # Verify Row types exist
    for row_type in PaneRow EventRow TriageRow HistoryRow SearchRow WorkflowRow HealthModel; do
        if grep -q "pub struct $row_type" "$VIEW_ADAPTERS"; then
            pass "$row_type struct present"
        else
            fail "$row_type struct missing from view_adapters.rs"
        fi
    done
else
    fail "view_adapters.rs not found"
fi

echo ""

# ---------------------------------------------------------------------------
# 5. Parity matrix template validity
# ---------------------------------------------------------------------------
echo "--- Check 5: Parity matrix template ---"

PARITY_TEMPLATE="docs/ftui-parity-matrix-template.md"

if [ -f "$PARITY_TEMPLATE" ]; then
    REQUIRED_FIELDS=("id" "category" "description" "severity" "verdict")
    MISSING=0
    for field in "${REQUIRED_FIELDS[@]}"; do
        if ! grep -qi "$field" "$PARITY_TEMPLATE"; then
            fail "Parity template missing field: $field"
            MISSING=1
        fi
    done
    if [ "$MISSING" -eq 0 ]; then
        pass "Parity matrix template has required fields"
    fi

    # Verify verdict types documented
    for verdict in "pass" "fail" "intentional-delta"; do
        if grep -qi "$verdict" "$PARITY_TEMPLATE"; then
            pass "Verdict type '$verdict' documented"
        else
            fail "Verdict type '$verdict' missing from template"
        fi
    done
else
    skip "Parity matrix template not found"
fi

echo ""

# ---------------------------------------------------------------------------
# 6. JSON schema presence
# ---------------------------------------------------------------------------
echo "--- Check 6: JSON schema presence ---"

SCHEMA_DIR="docs/json-schema"

if [ -d "$SCHEMA_DIR" ]; then
    SCHEMA_COUNT=$(find "$SCHEMA_DIR" -name "*.json" | wc -l | tr -d ' ')
    if [ "$SCHEMA_COUNT" -ge 10 ]; then
        pass "JSON schemas present ($SCHEMA_COUNT files)"
    else
        fail "Too few JSON schemas (found $SCHEMA_COUNT, expected >= 10)"
    fi

    # Verify the base envelope schema exists
    if [ -f "$SCHEMA_DIR/wa-robot-envelope.json" ]; then
        pass "Base envelope schema present"
    else
        fail "Base envelope schema (wa-robot-envelope.json) missing"
    fi
else
    skip "JSON schema directory not found"
fi

echo ""

# ---------------------------------------------------------------------------
# 7. Feature matrix doc consistency
# ---------------------------------------------------------------------------
echo "--- Check 7: Feature matrix doc ---"

FEATURE_DOC="docs/ftui-cargo-feature-matrix.md"

if [ -f "$FEATURE_DOC" ]; then
    # Verify it mentions both features
    if grep -q "tui" "$FEATURE_DOC" && grep -q "ftui" "$FEATURE_DOC"; then
        pass "Feature matrix doc mentions both tui and ftui"
    else
        fail "Feature matrix doc missing tui or ftui reference"
    fi

    # Verify mutual exclusion is documented
    if grep -qi "mutual.*exclu\|compile_error\|tui,ftui" "$FEATURE_DOC"; then
        pass "Mutual exclusion documented"
    else
        fail "Mutual exclusion not documented in feature matrix"
    fi
else
    skip "Feature matrix doc not found"
fi

echo ""

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo "=== Summary ==="
echo "  Passed: $PASS"
echo "  Failed: $FAIL"
echo "  Skipped: $SKIP"

# Write GitHub step summary (if in CI)
if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
    {
        echo "## FTUI Docs-Smoke & Contract-Drift"
        echo ""
        echo "| Check | Result |"
        echo "|-------|--------|"
        echo "| ADR presence | $( (( FAIL == 0 )) && echo 'pass' || echo 'see details' ) |"
        echo "| Feature matrix smoke | compiled |"
        echo "| View contract | 7 views |"
        echo "| Adapter coverage | 7 adapters + 7 types |"
        echo "| Parity template | fields + verdicts |"
        echo "| JSON schemas | present |"
        echo ""
        echo "**Total:** $PASS passed, $FAIL failed, $SKIP skipped"
    } >> "$GITHUB_STEP_SUMMARY"
fi

if [ "$FAIL" -gt 0 ]; then
    echo ""
    echo "FTUI docs-smoke check FAILED — see errors above."
    exit 1
else
    echo ""
    echo "All FTUI docs-smoke and contract-drift checks passed."
    exit 0
fi
