# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-XX-XX

### Changed

- **BREAKING**: Removed Lua-based status update hook (`update-status`)
  - Dramatically improves WezTerm performance by eliminating high-frequency Lua callbacks
  - The `update-status` event was firing at ~60Hz, causing continuous Lua interpreter
    invocations, JSON serialization, and IPC overhead on every frame
  - Pane metadata now obtained via polling (`wezterm cli list`) only when needed
  - Alt-screen detection now via escape sequence parsing (more reliable, zero overhead)

### Removed

- `ft events --from-status` CLI option (internal, not public API)
- `StatusUpdate` IPC message type (internal)
- `STATUS_UPDATE_LUA` snippet from `ft setup` output

### Migration

If you previously ran `ft setup`, run it again to update your wezterm.lua:

```bash
ft setup --wezterm
```

This will automatically remove the deprecated Lua code from the ft-managed block.
Your wezterm.lua's ft-managed block should no longer contain (legacy markers from older versions may still use `wa_*`/`WA_*` names):
- `wezterm.on('update-status'`
- `wa_last_status_update`
- `WA_STATUS_UPDATE_INTERVAL_MS`

It should still contain:
- `wezterm.on('user-var-changed'` (for agent signaling)

## [0.1.0] - 2026-01-25

### Added

- Initial release
- Robot Mode API for AI-to-AI orchestration
- Pattern detection engine for agent state transitions (rate limits, errors, prompts)
- Full-text search across all captured pane output (FTS5)
- Delta extraction for efficient output capture
- Policy engine with capability gates and rate limiting
- Multi-pane observation via WezTerm CLI
- TOON output format for 40-60% token savings
