# Policy Risk Scoring Model Design

**Status:** Draft
**Bead:** wa-upg.6.1
**Author:** RubyCat
**Date:** 2026-01-28

## Overview

This document defines the deterministic risk scoring model used by `PolicyEngine` to make
nuanced authorization decisions. Risk scoring provides granular control between the binary
Allow/Deny choices, enabling:

- Graduated responses (low-risk → allow, medium → require-approval, high → deny)
- Transparent decision-making (users can understand why something is risky)
- Configurable thresholds (operators can tune for their risk tolerance)

## Design Goals

1. **Deterministic**: Same inputs always produce same risk score
2. **Explainable**: Each contributing factor has human-readable explanation
3. **Configurable**: Weights and thresholds can be tuned via config
4. **Composable**: Factors combine additively with clear semantics
5. **Stable**: Factor IDs never change; new factors are additions only

## Risk Score Range

Risk scores are integers from 0-100:

| Range | Meaning | Default Behavior |
|-------|---------|------------------|
| 0-20  | Low risk | Allow |
| 21-50 | Medium risk | Allow (log as advisory) |
| 51-70 | Elevated risk | RequireApproval |
| 71-100 | High risk | Deny |

## Risk Factors

### Factor Registry

Each factor has:
- **ID**: Stable string identifier (snake_case, never changes)
- **Category**: Grouping for display (state, action, context, content)
- **Base Weight**: Default contribution to risk score
- **Explanation**: Human-readable description

### State Factors

Factors derived from pane/session state:

| Factor ID | Category | Base Weight | Description |
|-----------|----------|-------------|-------------|
| `state.alt_screen` | state | 60 | Pane is in alternate screen mode (vim, less, etc.) |
| `state.alt_screen_unknown` | state | 40 | Cannot determine if pane is in alt-screen |
| `state.command_running` | state | 25 | A command is currently executing |
| `state.no_prompt` | state | 20 | No active prompt detected |
| `state.recent_gap` | state | 35 | Recent capture gap (state uncertainty) |
| `state.is_reserved` | state | 50 | Pane is reserved by another workflow |
| `state.reserved_by_other` | state | 55 | Pane reserved by different workflow than actor |

### Action Factors

Factors derived from the action being requested:

| Factor ID | Category | Base Weight | Description |
|-----------|----------|-------------|-------------|
| `action.is_mutating` | action | 10 | Action modifies pane state |
| `action.is_destructive` | action | 25 | Action could be destructive (close, Ctrl-C/D) |
| `action.send_control` | action | 15 | Sending control character |
| `action.spawn_split` | action | 20 | Creating new pane (resource allocation) |
| `action.browser_auth` | action | 30 | Browser-based authentication flow |
| `action.workflow_start` | action | 15 | Starting automated workflow |

### Context Factors

Factors derived from request context:

| Factor ID | Category | Base Weight | Description |
|-----------|----------|-------------|-------------|
| `context.actor_untrusted` | context | 15 | Actor is not human (robot/mcp/workflow) |
| `context.broadcast_target` | context | 35 | Action targets multiple panes |
| `context.no_workflow_id` | context | 10 | Mutating action outside workflow context |
| `context.rate_limit_near` | context | 20 | Approaching rate limit threshold |

### Content Factors

Factors derived from command content analysis (SendText only):

| Factor ID | Category | Base Weight | Description |
|-----------|----------|-------------|-------------|
| `content.destructive_tokens` | content | 40 | Contains destructive tokens (rm -rf, DROP, etc.) |
| `content.sudo_elevation` | content | 30 | Contains sudo/doas/run0 |
| `content.multiline_complex` | content | 15 | Multi-line command (heredoc, compound) |
| `content.pipe_chain` | content | 10 | Piped command chain |
| `content.looks_like_password` | content | 25 | Input appears to be password/secret |

## Scoring Algorithm

```rust
/// Calculate total risk score from applicable factors
pub fn calculate_risk_score(factors: &[RiskFactor], config: &RiskConfig) -> RiskScore {
    let mut total: u32 = 0;
    let mut applied: Vec<AppliedFactor> = Vec::new();

    for factor in factors {
        let weight = config.get_weight(&factor.id).unwrap_or(factor.base_weight);
        if weight > 0 {
            total = total.saturating_add(weight);
            applied.push(AppliedFactor {
                id: factor.id.clone(),
                weight,
                explanation: factor.explanation.clone(),
            });
        }
    }

    // Cap at 100
    let score = total.min(100) as u8;

    RiskScore {
        score,
        factors: applied,
        summary: risk_summary(score),
    }
}

fn risk_summary(score: u8) -> String {
    match score {
        0..=20 => "Low risk".to_string(),
        21..=50 => "Medium risk".to_string(),
        51..=70 => "Elevated risk".to_string(),
        71..=100 => "High risk".to_string(),
        _ => unreachable!(),
    }
}
```

## Configuration

### Config Schema

```toml
[policy.risk]
# Enable risk scoring (default: true)
enabled = true

# Thresholds for decision mapping
[policy.risk.thresholds]
allow_max = 50          # Allow if score <= this
require_approval_max = 70  # Require approval if score <= this (else deny)

# Weight overrides (factor_id -> weight)
[policy.risk.weights]
"state.alt_screen" = 80        # Increase alt-screen risk
"content.sudo_elevation" = 10  # Decrease sudo risk for trusted environments

# Disable specific factors
[policy.risk.disabled]
factors = ["content.multiline_complex"]  # Don't penalize multiline commands

# Hard overrides (factor_id -> decision)
# These bypass scoring entirely when factor is present
[policy.risk.overrides]
"state.alt_screen" = "deny"           # Always deny in alt-screen
"state.reserved_by_other" = "deny"    # Always deny if reserved by other workflow
```

### Factor Weight Range

Weights must be in range 0-100. Setting weight to 0 effectively disables the factor.

### Hard Overrides

Hard overrides bypass the scoring system entirely:
- If any hard-override factor is present, that decision is returned immediately
- Useful for "always deny in alt-screen" type rules
- Overrides are evaluated before scoring

## Decision Mapping

```rust
/// Map risk score to policy decision
pub fn risk_to_decision(
    risk: &RiskScore,
    config: &RiskConfig,
    input: &PolicyInput,
) -> PolicyDecision {
    // Check hard overrides first
    for factor in &risk.factors {
        if let Some(override_decision) = config.get_override(&factor.id) {
            return override_decision.with_context(risk, input);
        }
    }

    // Map score to decision
    match risk.score {
        s if s <= config.allow_max => PolicyDecision::Allow {
            rule_id: Some("risk.score_allow".to_string()),
            context: Some(build_context(risk, input)),
        },
        s if s <= config.require_approval_max => PolicyDecision::RequireApproval {
            reason: format!(
                "Action has elevated risk score of {} (threshold: {}). Factors: {}",
                risk.score,
                config.allow_max,
                format_factors(&risk.factors),
            ),
            rule_id: Some("risk.score_approval".to_string()),
            approval: Some(build_approval_request(input)),
            context: Some(build_context(risk, input)),
        },
        _ => PolicyDecision::Deny {
            reason: format!(
                "Action has high risk score of {} (threshold: {}). Factors: {}",
                risk.score,
                config.require_approval_max,
                format_factors(&risk.factors),
            ),
            rule_id: Some("risk.score_deny".to_string()),
            context: Some(build_context(risk, input)),
        },
    }
}
```

## Explainability

### Factor Explanations

Each factor has a template for human-readable explanations:

```rust
static FACTOR_EXPLANATIONS: LazyLock<HashMap<&str, FactorExplanation>> = LazyLock::new(|| {
    let mut m = HashMap::new();

    m.insert("state.alt_screen", FactorExplanation {
        short: "Pane is in alternate screen mode",
        long: "The target pane is running a full-screen application (like vim, less, \
               or htop) that uses the alternate screen buffer. Sending text input to \
               such applications can cause unpredictable behavior or data corruption.",
        remediation: "Wait for the application to exit, or use the application's \
                     native input method.",
    });

    m.insert("state.recent_gap", FactorExplanation {
        short: "Recent capture discontinuity",
        long: "There was a gap in output capture, meaning some pane activity may have \
               been missed. This creates uncertainty about the current pane state.",
        remediation: "Wait for the pane to reach a known state (e.g., shell prompt) \
                     before taking action.",
    });

    m.insert("content.destructive_tokens", FactorExplanation {
        short: "Command contains destructive patterns",
        long: "The command text contains patterns commonly associated with destructive \
               operations (like 'rm -rf', 'DROP TABLE', 'git reset --hard').",
        remediation: "Review the command carefully. Consider using --dry-run if available, \
                     or manually confirm the operation is intended.",
    });

    // ... more explanations
    m
});
```

### Risk Summary in Output

Risk information appears in policy outputs:

```json
{
  "decision": "require_approval",
  "reason": "Action has elevated risk score of 65 (threshold: 50)",
  "rule_id": "risk.score_approval",
  "risk": {
    "score": 65,
    "summary": "Elevated risk",
    "factors": [
      {
        "id": "state.alt_screen_unknown",
        "weight": 40,
        "explanation": "Cannot determine if pane is in alternate screen mode"
      },
      {
        "id": "action.is_mutating",
        "weight": 10,
        "explanation": "Action modifies pane state"
      },
      {
        "id": "context.actor_untrusted",
        "weight": 15,
        "explanation": "Actor is not human (robot/mcp/workflow)"
      }
    ]
  },
  "approval": {
    "allow_once_code": "XK7P2M",
    "summary": "Allow SendText to pane 3 (risk: 65)",
    "command": "wa approve XK7P2M"
  }
}
```

## Data Structures

### Rust Types

```rust
/// Risk scoring configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskConfig {
    /// Enable risk scoring
    pub enabled: bool,
    /// Maximum score for automatic allow
    pub allow_max: u8,
    /// Maximum score for require-approval (above this = deny)
    pub require_approval_max: u8,
    /// Weight overrides by factor ID
    pub weights: HashMap<String, u8>,
    /// Disabled factor IDs
    pub disabled: HashSet<String>,
    /// Hard decision overrides by factor ID
    pub overrides: HashMap<String, PolicyRuleDecision>,
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_max: 50,
            require_approval_max: 70,
            weights: HashMap::new(),
            disabled: HashSet::new(),
            overrides: HashMap::new(),
        }
    }
}

/// A risk factor with its evaluation result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskFactor {
    /// Stable factor ID
    pub id: String,
    /// Factor category
    pub category: RiskCategory,
    /// Base weight (0-100)
    pub base_weight: u8,
    /// Human-readable explanation
    pub explanation: String,
}

/// Risk factor categories
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskCategory {
    State,
    Action,
    Context,
    Content,
}

/// Calculated risk score with contributing factors
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskScore {
    /// Total risk score (0-100)
    pub score: u8,
    /// Factors that contributed to the score
    pub factors: Vec<AppliedFactor>,
    /// Human-readable summary
    pub summary: String,
}

/// A factor that was applied to the risk calculation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppliedFactor {
    /// Factor ID
    pub id: String,
    /// Weight that was applied
    pub weight: u8,
    /// Human-readable explanation
    pub explanation: String,
}
```

## Integration Points

### PolicyEngine Integration

```rust
impl PolicyEngine {
    pub fn authorize(&self, input: &PolicyInput) -> PolicyDecision {
        // 1. Check explicit rules first (they may override risk scoring)
        if let Some(decision) = self.evaluate_explicit_rules(input) {
            return decision;
        }

        // 2. Calculate risk score
        let factors = self.collect_risk_factors(input);
        let risk = calculate_risk_score(&factors, &self.config.risk);

        // 3. Map risk to decision
        risk_to_decision(&risk, &self.config.risk, input)
    }

    fn collect_risk_factors(&self, input: &PolicyInput) -> Vec<RiskFactor> {
        let mut factors = Vec::new();

        // State factors
        if let Some(caps) = &input.capabilities {
            if caps.alt_screen == Some(true) {
                factors.push(FACTORS.get("state.alt_screen").unwrap().clone());
            } else if caps.alt_screen.is_none() {
                factors.push(FACTORS.get("state.alt_screen_unknown").unwrap().clone());
            }
            if caps.command_running {
                factors.push(FACTORS.get("state.command_running").unwrap().clone());
            }
            // ... etc
        }

        // Action factors
        if input.action.is_mutating() {
            factors.push(FACTORS.get("action.is_mutating").unwrap().clone());
        }
        // ... etc

        // Content factors (for SendText)
        if let Some(text) = &input.text {
            factors.extend(analyze_content_risk(text));
        }

        factors
    }
}
```

### wa why Integration

```rust
// In wa why output
pub fn explain_risk_decision(decision: &PolicyDecision) -> String {
    let Some(context) = decision.context() else {
        return "No decision context available".to_string();
    };

    let Some(risk) = &context.risk else {
        return "Risk scoring was not applied".to_string();
    };

    let mut output = String::new();
    writeln!(output, "Risk Score: {} ({})", risk.score, risk.summary);
    writeln!(output);
    writeln!(output, "Contributing Factors:");

    for factor in &risk.factors {
        let explanation = get_factor_explanation(&factor.id);
        writeln!(output, "  - {} (+{})", explanation.short, factor.weight);
        writeln!(output, "    {}", explanation.long);
        if let Some(remediation) = &explanation.remediation {
            writeln!(output, "    To fix: {}", remediation);
        }
    }

    output
}
```

## Testing Requirements

### Unit Tests

1. **Determinism**: Same factors always produce same score
2. **Scoring math**: Verify additive scoring with cap at 100
3. **Config overrides**: Weight changes affect output
4. **Hard overrides**: Bypass scoring entirely
5. **Factor collection**: Correct factors derived from input

### Integration Tests

1. **End-to-end scoring**: Full policy flow with risk
2. **Config loading**: TOML config correctly applied
3. **Output stability**: Risk JSON schema matches specification

### Fixture-Based Tests

Add to corpus:
- `risk_low.txt` + `risk_low.expect.json` - low-risk scenario
- `risk_elevated.txt` + `risk_elevated.expect.json` - elevated risk
- `risk_high.txt` + `risk_high.expect.json` - high risk denial

## Migration Notes

### Backward Compatibility

Risk scoring is opt-in initially:
- Default `enabled = true` but can be disabled
- Existing explicit rules take precedence
- No behavior change for currently-allowed actions

### Gradual Rollout

1. Deploy with `enabled = true`, high thresholds (allow most)
2. Monitor risk scores in logs
3. Gradually lower thresholds as confidence builds
4. Add content analysis factors incrementally

## Open Questions

1. **Factor interactions**: Should some factor combinations have non-additive effects?
   - e.g., `alt_screen + destructive_tokens` = extra penalty?

2. **Learning/adaptation**: Should we track false positive/negative rates?
   - Could inform threshold tuning suggestions

3. **Per-pane config**: Should panes be able to have different risk tolerances?
   - e.g., "production" panes have lower thresholds

## References

- wa-upg.6 (parent epic)
- wa-4vx.8.1 (policy model - dependency)
- wa-2ep (explainability epic)
