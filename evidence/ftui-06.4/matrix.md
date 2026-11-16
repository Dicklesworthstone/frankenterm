# Text-Input Editing Edge-Case Matrix (FTUI-06.4.a)

**Bead:** wa-2yhi (FTUI-06.4.a)
**Date:** 2026-02-09

## Matrix

| id | category | description | expected outcome | test | status |
|----|----------|-------------|-----------------|------|--------|
| E-TI-001 | multibyte | Insert and delete 2-byte UTF-8 chars (é, ñ) | Cursor advances by char_len_utf8, delete removes full char | `edge_multibyte_insert_and_delete` | pass |
| E-TI-002 | multibyte | Cursor movement over 2-byte chars | Left/right skip full char (not byte) | `edge_multibyte_cursor_movement` | pass |
| E-TI-003 | multibyte | Insert and navigate 4-byte emoji (🦀) | Cursor advances by 4, movement skips full codepoint | `edge_emoji_insert_and_navigate` | pass |
| E-TI-004 | multibyte | CJK characters (3-byte UTF-8) | Insert between CJK chars works, cursor on char boundary | `edge_cjk_characters` | pass |
| E-TI-005 | deletion | Delete forward at end of text | No-op, text and cursor unchanged | `edge_delete_forward_at_end_is_noop` | pass |
| E-TI-006 | deletion | Delete back at start of text | No-op, text and cursor unchanged | `edge_delete_back_at_start_is_noop` | pass |
| E-TI-007 | deletion | Delete all chars one by one from end | Empty string, cursor at 0, extra delete safe | `edge_delete_all_chars_one_by_one` | pass |
| E-TI-008 | deletion | Delete forward from middle position | Correct char removed, cursor stays | `edge_delete_forward_from_middle` | pass |
| E-TI-009 | cursor | Rapid left past boundary (10x on 1-char) | Clamped at 0 | `edge_rapid_left_right_at_boundaries` | pass |
| E-TI-010 | cursor | Rapid right past boundary (10x on 1-char) | Clamped at len | `edge_rapid_left_right_at_boundaries` | pass |
| E-TI-011 | cursor | Home on empty string | Cursor stays at 0 | `edge_home_on_empty` | pass |
| E-TI-012 | cursor | End on empty string | Cursor stays at 0 | `edge_end_on_empty` | pass |
| E-TI-013 | sequence | Home then insert prefix | Text prepended, cursor after prefix | `edge_home_then_insert` | pass |
| E-TI-014 | sequence | Home then delete_forward all | Text cleared from front | `edge_home_then_delete_forward_all` | pass |
| E-TI-015 | sequence | Interleaved insert/delete mid-text | Correct state after each operation | `edge_interleaved_insert_delete` | pass |
| E-TI-016 | lifecycle | Clear then rebuild text | Fresh start, cursor tracks correctly | `edge_clear_then_rebuild` | pass |
| E-TI-017 | lifecycle | set_text then edit | Cursor at end after set, insert works | `edge_set_text_then_edit` | pass |
| E-TI-018 | stress | 200-character input | Correct length, home/end work | `edge_long_input_200_chars` | pass |
| E-TI-019 | stress | Navigate entire 6-char string step by step | Cursor correct at every position | `edge_navigate_entire_string` | pass |
| E-TI-020 | lifecycle | Single char: full operation cycle | insert/left/right/home/end/delete all correct | `edge_single_char_full_lifecycle` | pass |

## Summary

- Total rows: 20
- Pass: 20
- Fail: 0

## Categories

| Category | Count | Coverage |
|----------|-------|----------|
| multibyte | 4 | 2-byte (accented), 3-byte (CJK), 4-byte (emoji), mixed |
| deletion | 4 | Forward at end, back at start, exhaust all, forward from middle |
| cursor | 4 | Rapid boundary clamp, home/end on empty |
| sequence | 3 | Home+insert, home+delete_forward, interleaved |
| lifecycle | 3 | Clear+rebuild, set_text+edit, single char full cycle |
| stress | 2 | 200 chars, full string walk |

## Test Helper

```rust
fn assert_ti(ti: &TextInput, text: &str, cursor: usize, label: &str)
```

Reusable assertion that checks both text content and cursor position with
descriptive labels for failure diagnostics.
