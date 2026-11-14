# Agent Quirks & Drift Catalog

Captures undocumented behaviors and output drift for Codex, Claude Code, and
Gemini that affect detection rules. Each entry links to the corpus fixture
that demonstrates the behavior.

## False Positive Report

Rules whose anchors fire on unrelated text from other programs.

| Rule | Anchor | FP Trigger | Fixture | Severity |
|------|--------|-----------|---------|----------|
| `wezterm.mux.connection_lost` | `connection lost` | "SSH connection lost: remote host" | `connection_lost_fp_ssh` | High: fires on any disconnection |

Fixed 2026-11-14: `claude_code.error.network`, `.timeout` and `.overloaded`
used to fire on any "timed out", "connection closed" or "overloaded" in prose
and tool output. They also missed every real Claude Code API error, because
their lowercase, case-sensitive regexes did not match Claude Code's wording.
All three now require Claude Code's `API Error: ` framing. The old triggers
are kept as fixtures that must not match: `overloaded_fp_database`,
`timeout_error_fp_test_suite`, `error_rules_fp_prose` and
`overloaded_fp_rate_limit`.

Fixed 2026-11-14: `claude_code.auth.api_key_error` fired on any "API key" or
"ANTHROPIC_API_KEY": 187 of its 191 hits in 461k Claude Code messages were
prose. It drives `handle_auth_required`, which types a recovery prompt into
the pane. It now needs Claude Code's own wording, and the old triggers are
kept in `api_key_error_fp_prose`.

### Mitigation

ft's domain filter (agent-scoped pane assignment) limits exposure: a pane
assigned to `claude_code` won't run Codex rules. Panes without an assigned
agent type (shells, and agents ft does not recognize) run every rule, so a
rule's own text must be specific to its agent.

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
| API failures framed as "API Error: <message>" | `claude_code.error.network`, `.timeout`, `.overloaded` | Claude Code 2.1.201..2.1.322 session logs: "Connection lost mid-response", "Connection refused — ... (ECONNREFUSED)", "Can't reach the API server — ... (ENOTFOUND)", "No response from API (waited 3m, ...)", "The response stopped arriving", "529 Overloaded". The TUI shows the same text under a `⎿` gutter. | `network_error`, `network_error_v2`, `timeout_error`, `timeout_error_v2`, `overloaded`, `overloaded_v2` |
| Limit messages change wording | `claude_code.usage.reached`, `.usage.warning` | 2.1.312 renamed the session limit "5-hour limit". A weekly reset names its day ("resets Nov 12 at 2pm (America/New_York)", "resets Oct 12, 9am"), so `reset_time` keeps the date. Model limits ("You've reached your Fable 5 limit") and "You're out of usage credits" name no reset. | `usage_reached_5hour_cli_2026_11`, `usage_reached_weekly_date_cli_2026_11`, `usage_reached_model_cli_2026_11`, `usage_reached_model_v2_cli_2026_11`, `usage_reached_credits_cli_2026_11`, `usage_warning_5hour_cli_2026_11` |
| Auth failures framed by Claude Code | `claude_code.auth.api_key_error`, `.login_required` | "Invalid API key · Fix external API key", "... for Claude Code · Use an Anthropic API key instead ...", "Failed to authenticate: OAuth session expired and could not be refreshed", "Failed to authenticate. API Error: 401 OAuth access token has expired ..." and "... 403 Attention Required! \| Cloudflare". Other tools also print "Failed to authenticate <what>", so only these framings count. | `api_key_error`, `api_key_error_v2`, `auth_failed_oauth_expired_cli_2026_11`, `auth_failed_401_cli_2026_11`, `auth_failed_403_cli_2026_11` |

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
| 2026-11-14 | Live fleet scan + Claude Code session logs | `claude_code.error.network`, `.timeout`, `.overloaded` | Require the "API Error: " framing. Before: 0 of 9 real API errors detected, 7 of 7 prose samples fired. Synthetic positive fixtures replaced with real text; their old text now must not match. | `error_rules_fp_prose.txt`, `overloaded_fp_rate_limit.txt` |
| 2026-11-14 | Claude Code session logs | `claude_code.usage.reached`, `.usage.warning` | Match the 5-hour, model and credit limit messages, which matched no rule, and keep a weekly reset's day. Before, "resets Nov 12 at 2pm" was read as the next 2pm, up to six days early. | `usage_reached_*_cli_2026_11.txt`, `usage_warning_5hour_cli_2026_11.txt` |
| 2026-11-14 | Claude Code session logs + prose sweep | `claude_code.auth.api_key_error`, `.login_required` | api_key_error: only Claude Code's "Invalid API key ·" and "Use an Anthropic API key instead" (was any "API key"; 187 of 191 hits were prose). login_required: also "Failed to authenticate: OAuth" and "Failed to authenticate. API Error:", which matched no rule. | `api_key_error_fp_prose.txt`, `auth_failed_*_cli_2026_11.txt` |

## Coverage Summary

- 37 rules across 5 packs
- 158 corpus fixtures
- All 3 agents have quirks documented
- 4 confirmed false positives cataloged with fixtures; 3 of them (the
  Claude Code error and API-key rules) fixed 2026-11-14
- Pack linter: 0 errors, 0 warnings
