# Agent Quirks & Drift Catalog

Captures undocumented behaviors and output drift for Codex, Claude Code, and
Gemini that affect detection rules. Each entry links to the corpus fixture
that demonstrates the behavior.

## False Positive Report

Rules whose anchors fire on unrelated text from other programs.

| Rule | Anchor | FP Trigger | Fixture | Severity |
|------|--------|-----------|---------|----------|
| `claude_code.error.overloaded` | `overloaded` | "Database overloaded: connection pool exhausted" | `overloaded_fp_database` | High: fires on any use of "overloaded" |
| `claude_code.error.timeout` | `timed out` | "Test suite timed out after 30 seconds" | `timeout_error_fp_test_suite` | High: fires on any timeout message |
| `wezterm.mux.connection_lost` | `connection lost` | "SSH connection lost: remote host" | `connection_lost_fp_ssh` | High: fires on any disconnection |

### Mitigation

These rules use single-word or two-word anchors that are common in
infrastructure logs. ft's domain filter (agent-scoped pane assignment) limits
exposure: a pane assigned to `claude_code` won't run Codex rules. Still, for
panes without an assigned agent type, all rules run and FPs are possible.

Recommended future work:
- Add context-window requirements (e.g., require preceding Claude Code banner).
- Increase anchor specificity where possible.
- Scope low-confidence matches by requiring multiple anchor hits.

## Codex Quirks

| Symptom | Rule Affected | Detail | Fixture |
|---------|--------------|--------|---------|
| Usage warning percentage format varies | `codex.usage.warning_*` | Can appear as "less than 5%" or "5% remaining" | `usage_warning_5_v2` |
| Resume hint UUID format | `codex.session.resume_hint` | Always RFC-4122 UUID; if Codex changes to short IDs, rule breaks | `resume_hint_v2` |
| Device code format `XXXX-XXXXX` | `codex.auth.device_code_prompt` | Hardcoded 4-5 alphanumeric; if OpenAI changes length, rule fails | `device_code_v2`, `device_code_v3` |
| Token usage field order | `codex.session.token_usage` | Regex expects `total=... input=... output=...` order | `token_usage`, `no_reasoning`, `no_cached` |

## Claude Code Quirks

| Symptom | Rule Affected | Detail | Fixture |
|---------|--------------|--------|---------|
| Banner format split | `claude_code.banner` | Two formats: "Claude Code v1.2.3" and "claude-code/2.0.0" | `banner` (v prefix), `banner_v2` (slash) |
| Model name evolution | `claude_code.model.selected` | Regex hardcodes `claude-(opus\|sonnet\|haiku)` — new families won't match | `model_selected_v2` |
| Tool name list static | `claude_code.tool_use` | Extraction regex enumerates known tools; new tools are unextracted | `tool_use_v2`, `tool_use_v3` |
| Compaction token format | `claude_code.compaction` | Expects numeric `[\d,]+` tokens; abbreviated "12.3K" won't match | `compaction_v2`, `compaction_v3` |
| "Thinking" anchor generic | `claude_code.thinking` | "Thinking" is common English; FP risk in verbose logs | `thinking_near_miss`, `thinking_near_miss_v2` |

## Gemini Quirks

| Symptom | Rule Affected | Detail | Fixture |
|---------|--------------|--------|---------|
| "Pro models" tier name hardcoded | `gemini.usage.warning` | Regex requires "Pro models quota"; tier rename breaks rule | `usage_warning_v2` |
| Usage reached exact phrasing | `gemini.usage.reached` | Three literal anchor variants; any wording change breaks detection | `usage_reached_v2` |
| Session summary field order | `gemini.session.summary` | Multiline regex assumes Session ID → Tool Calls → Tokens order | `session_summary_tokens` |

## WezTerm Quirks

| Symptom | Rule Affected | Detail | Fixture |
|---------|--------------|--------|---------|
| "connection lost" generic | `wezterm.mux.connection_lost` | Fires on any "connection lost" text (SSH, VPN, database) | `connection_lost_fp_ssh` |
| "process exited" generic | `wezterm.pane.exited` | Anchor could match non-WezTerm process exits | `pane_exited_near_miss` |

## Drift Log

| Date | Source | Rule | Change | Test Added |
|------|--------|------|--------|------------|
| 2026-01-30 | Corpus audit | `claude_code.banner` | Added base fixture (was missing — only v2 existed) | `banner.txt` |
| 2026-01-30 | Corpus audit | `claude_code.error.overloaded` | Documented FP on "Database overloaded" | `overloaded_fp_database.txt` |
| 2026-01-30 | Corpus audit | `claude_code.error.timeout` | Documented FP on "Test suite timed out" | `timeout_error_fp_test_suite.txt` |
| 2026-01-30 | Corpus audit | `wezterm.mux.connection_lost` | Documented FP on "SSH connection lost" | `connection_lost_fp_ssh.txt` |
| 2026-01-30 | Corpus audit | `claude_code.thinking` | Added second near-miss (natural language "thinking") | `thinking_near_miss_v2.txt` |

## Coverage Summary

- 30 rules across 4 packs
- 98 corpus fixtures (up from 88)
- All 3 agents have quirks documented
- 3 confirmed false positives cataloged with fixtures
- Pack linter: 0 errors, 0 warnings
