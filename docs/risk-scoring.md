# Risk Scoring Guide

This guide explains how wa's risk scoring system works and how to configure it for your environment.

## Overview

wa uses a risk scoring model to make nuanced authorization decisions. Instead of binary allow/deny, actions receive a risk score from 0-100 that determines the response:

| Score Range | Risk Level | Default Response |
|-------------|------------|------------------|
| 0-20 | Low | Allow automatically |
| 21-50 | Medium | Allow (logged for review) |
| 51-70 | Elevated | Require approval |
| 71-100 | High | Deny |

## Understanding Risk Factors

Risk scores are calculated by combining multiple factors. Each factor has an ID, category, and weight.

### State Factors

These reflect the current state of the target pane:

| Factor | Weight | When It Applies |
|--------|--------|-----------------|
| `state.alt_screen` | 60 | Pane is in vim, less, htop, etc. |
| `state.alt_screen_unknown` | 40 | Can't determine if alt-screen is active |
| `state.command_running` | 25 | A command is currently executing |
| `state.no_prompt` | 20 | No shell prompt detected |
| `state.recent_gap` | 35 | Recent capture gap (uncertain state) |
| `state.is_reserved` | 50 | Pane reserved by a workflow |

### Action Factors

These depend on what action is being performed:

| Factor | Weight | When It Applies |
|--------|--------|-----------------|
| `action.is_mutating` | 10 | Action modifies pane state (send text) |
| `action.is_destructive` | 25 | Ctrl-C, Ctrl-D, close pane |
| `action.send_control` | 15 | Sending control characters |
| `action.spawn_split` | 20 | Creating new pane |
| `action.browser_auth` | 30 | Browser authentication flow |

### Context Factors

These depend on who/what is requesting the action:

| Factor | Weight | When It Applies |
|--------|--------|-----------------|
| `context.actor_untrusted` | 15 | Actor is robot/MCP/workflow (not human) |
| `context.broadcast_target` | 35 | Action targets multiple panes |
| `context.no_workflow_id` | 10 | Mutating action outside workflow |

### Content Factors

These analyze the command text (SendText only):

| Factor | Weight | When It Applies |
|--------|--------|-----------------|
| `content.destructive_tokens` | 40 | Contains `rm -rf`, `DROP TABLE`, etc. |
| `content.sudo_elevation` | 30 | Contains `sudo`, `doas`, `run0` |
| `content.multiline_complex` | 15 | Heredocs, multi-line commands |
| `content.pipe_chain` | 10 | Complex pipe chains (2+ pipes) |

## Configuration

Configure risk scoring in `~/.config/wa/wa.toml` or `.wa/config.toml`:

### Basic Configuration

```toml
[policy.risk]
# Enable/disable risk scoring (default: true)
enabled = true

# Score thresholds for decisions
[policy.risk.thresholds]
allow_max = 50          # Allow if score <= 50
require_approval_max = 70  # Require approval if <= 70, deny above
```

### Adjusting Factor Weights

Override default weights for specific factors:

```toml
[policy.risk.weights]
# Reduce alt-screen penalty for trusted environments
"state.alt_screen" = 40

# Increase penalty for destructive commands
"content.destructive_tokens" = 60

# Trust automated actors more
"context.actor_untrusted" = 5
```

### Disabling Factors

Completely disable factors that don't apply to your environment:

```toml
[policy.risk.disabled]
factors = [
    "content.multiline_complex",  # Multi-line commands are normal here
    "content.pipe_chain",         # Pipes are fine
]
```

## Examples

### Example 1: Low-Risk Action

```
Action: Send "ls -la" to pane with active prompt
Actor: Robot

Factors:
  - action.is_mutating: +10
  - context.actor_untrusted: +15

Total: 25 (Medium risk - Allow)
```

### Example 2: Elevated-Risk Action

```
Action: Send "sudo rm -rf /tmp/cache" to pane
Actor: Robot

Factors:
  - action.is_mutating: +10
  - context.actor_untrusted: +15
  - content.sudo_elevation: +30
  - content.destructive_tokens: +40

Total: 95 -> capped at 100 (High risk - Deny)
```

### Example 3: Alt-Screen Risk

```
Action: Send text to pane running vim
Actor: Human

Factors:
  - action.is_mutating: +10
  - state.alt_screen: +60

Total: 70 (Elevated risk - Require Approval)
```

## Viewing Risk Information

### In Robot Mode Output

Risk information appears in policy decision JSON:

```json
{
  "decision": "require_approval",
  "context": {
    "risk": {
      "score": 65,
      "summary": "Elevated risk",
      "factors": [
        {"id": "state.alt_screen", "weight": 60, "explanation": "Pane is in alternate screen mode"},
        {"id": "action.is_mutating", "weight": 10, "explanation": "Action modifies pane state"}
      ]
    }
  }
}
```

### Using wa why

```bash
# Explain why an action was denied/required approval
wa why denied --pane 3
```

## Safety Guidelines

**Do not blindly lower all risk thresholds.** The defaults are designed to prevent accidents.

### Safe Adjustments

- Lower `state.alt_screen` if you frequently work in vim and understand the risks
- Disable `content.pipe_chain` if complex pipes are normal in your workflow
- Reduce `context.actor_untrusted` for trusted automation systems

### Dangerous Adjustments

- Setting `allow_max = 100` defeats all safety checks
- Disabling `content.destructive_tokens` removes protection against `rm -rf`
- Setting all weights to 0 makes risk scoring useless

### Recommended Approach

1. Start with defaults
2. Watch for false positives (legitimate actions blocked)
3. Adjust specific factors that cause false positives
4. Never disable factors wholesale without understanding implications

## Troubleshooting

### "Why was my action denied?"

1. Check the risk score and contributing factors in the response
2. Use `wa why denied` for detailed explanation
3. Consider if any factors are false positives for your use case

### "How do I allow this specific action?"

For one-time approval:
```bash
wa approve <code>  # Use the approval code from the denial response
```

For recurring actions, adjust the relevant factor weight or threshold.

### "Risk scoring seems broken"

1. Check if `enabled = true` in config
2. Verify factor weights haven't all been set to 0
3. Check for config file syntax errors

## Related Documentation

- [Risk Model Design](risk-model-design.md) - Technical implementation details
- [Policy Configuration](../AGENTS.md#configuration) - General wa configuration
