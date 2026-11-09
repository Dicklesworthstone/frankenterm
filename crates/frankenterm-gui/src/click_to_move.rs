//! A click in a command line moves its cursor there (`cursor_click_to_move`).
//!
//! The terminal cannot move a program's cursor itself: only the program can,
//! so the click becomes the arrow keys that move it, as if typed. Which cells
//! form the command line, and how many presses separate two of them, is
//! settled here from the rows themselves, without the window. It lives in the
//! library, as the window's other pure rules do, so that `cargo test --lib`
//! runs its tests: the window's own module graph is the binary's, which has
//! no test target.

use std::ops::Range;
use wezterm_term::{KeyCode, Line, SemanticType, SemanticZone, StableRowIndex};

/// More presses than this are not sent: a click that far from the cursor is
/// not one a person aims at a cell.
const MAX_PRESSES: usize = 4096;

/// What a click asks of the program: `presses` of `key`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CursorMove {
    pub key: KeyCode,
    pub presses: usize,
}

/// Which rules may find a command line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClickToMove {
    /// The input a shell marks with OSC 133, cursor included.
    pub shell_integration: bool,
    /// The cursor's own row, when no marked input holds the cursor.
    pub cursor_row: bool,
}

/// A cell, as (row, column), so that cells order as they read.
type Cell = (StableRowIndex, usize);

/// The move from `cursor` to the cell `click` landed on, if the click is in
/// the cursor's command line. `zones` are the pane's semantic zones; `lines`
/// reads the rows it is given, as `Pane::get_lines` does.
pub fn cursor_move_for_click(
    rules: ClickToMove,
    zones: &[SemanticZone],
    lines: impl FnOnce(Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>),
    cursor: Cell,
    click: Cell,
) -> Option<CursorMove> {
    let input = zones.iter().find(|zone| {
        zone.semantic_type == SemanticType::Input
            && (zone.start_y, zone.start_x) <= cursor
            && cursor <= (zone.end_y, zone.end_x.saturating_add(1))
    });
    let (start, last_row, kind) = match input {
        Some(zone) if rules.shell_integration => (
            (zone.start_y, zone.start_x),
            zone.end_y,
            Some(SemanticType::Input),
        ),
        _ if rules.cursor_row => ((cursor.0, 0), cursor.0, None),
        _ => return None,
    };
    // Not before the command line, nor below it.
    if click < start || click.0 > last_row {
        return None;
    }
    let (first, rows) = lines(start.0..last_row.checked_add(1)?);
    let row = |y: StableRowIndex| rows.get(usize::try_from(y.checked_sub(first)?).ok()?);
    // Past its text, the click means its end: one past the last character
    // written there, or the cursor if it is further.
    let end = text_end(&row, start, last_row, kind).max(cursor);
    let target = click.min(end);
    let (key, from, to) = match target.cmp(&cursor) {
        std::cmp::Ordering::Equal => return None,
        std::cmp::Ordering::Greater => (KeyCode::RightArrow, cursor, target),
        std::cmp::Ordering::Less => (KeyCode::LeftArrow, target, cursor),
    };
    let presses = presses_between(&row, from, to)?;
    (1..=MAX_PRESSES)
        .contains(&presses)
        .then_some(CursorMove { key, presses })
}

/// One past the last non-blank cell of the command line (of `kind`, when
/// given) from `start` through `last_row`, or `start` if it holds none.
fn text_end<'a>(
    row: &impl Fn(StableRowIndex) -> Option<&'a Line>,
    start: Cell,
    last_row: StableRowIndex,
    kind: Option<SemanticType>,
) -> Cell {
    let mut end = start;
    for y in start.0..=last_row {
        let Some(line) = row(y) else { continue };
        for cell in line.visible_cells() {
            let x = cell.cell_index();
            if (y, x) < start
                || kind.is_some_and(|kind| cell.attrs().semantic_type() != kind)
                || cell.str().trim().is_empty()
            {
                continue;
            }
            end = end.max((y, x + cell.width().max(1)));
        }
    }
    end
}

/// How many arrow presses move a cursor from `from` to `to` (`from < to`):
/// one a character, a wide one included, across rows only where a row wraps
/// onto the next. A hard line break (a multi-line command) is not crossed.
fn presses_between<'a>(
    row: &impl Fn(StableRowIndex) -> Option<&'a Line>,
    from: Cell,
    to: Cell,
) -> Option<usize> {
    let mut presses = 0usize;
    for y in from.0..=to.0 {
        let line = row(y)?;
        let low = if y == from.0 { from.1 } else { 0 };
        let high = if y == to.0 { to.1 } else { usize::MAX };
        let mut written_to = 0;
        for cell in line.visible_cells() {
            let x = cell.cell_index();
            written_to = x + cell.width().max(1);
            if x >= low && x < high {
                presses += 1;
            }
        }
        if y == to.0 {
            // Unwritten cells before the target still take a press each,
            // as typed spaces would.
            presses += high.saturating_sub(written_to.max(low));
        } else if !line.last_cell_was_wrapped() {
            return None;
        }
    }
    Some(presses)
}

#[cfg(test)]
mod tests {
    use super::*;
    use SemanticType::{Input, Output, Prompt};
    use wezterm_term::{Cell as TermCell, CellAttributes};

    fn row(parts: &[(&str, SemanticType)], wrapped: bool) -> Line {
        let mut cells: Vec<TermCell> = Vec::new();
        for (text, semantic) in parts {
            let mut attrs = CellAttributes::default();
            attrs.set_semantic_type(*semantic);
            let mut line = Line::from_text(text, &attrs, 0, None);
            cells.extend(line.cells_mut().iter().cloned());
        }
        let mut line = Line::from_cells(cells, 0);
        line.set_last_cell_was_wrapped(wrapped, 0);
        line
    }

    fn zone(start: Cell, end: Cell, semantic_type: SemanticType) -> SemanticZone {
        SemanticZone {
            start_y: start.0,
            start_x: start.1,
            end_y: end.0,
            end_x: end.1,
            semantic_type,
        }
    }

    fn left(presses: usize) -> Option<CursorMove> {
        Some(CursorMove {
            key: KeyCode::LeftArrow,
            presses,
        })
    }

    fn right(presses: usize) -> Option<CursorMove> {
        Some(CursorMove {
            key: KeyCode::RightArrow,
            presses,
        })
    }

    const SHELL: ClickToMove = ClickToMove {
        shell_integration: true,
        cursor_row: false,
    };

    /// A pane whose rows start at 10, read as `Pane::get_lines` reads them.
    fn pane(rows: &[Line]) -> impl FnOnce(Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
        let rows = rows.to_vec();
        move |range| {
            let start = usize::try_from(range.start - 10).unwrap();
            let end = usize::try_from(range.end - 10).unwrap().min(rows.len());
            (range.start, rows[start..end].to_vec())
        }
    }

    /// "$ <input>" on row 10, as a shell with OSC 133 leaves it.
    fn prompt_and(input: &str) -> (Vec<SemanticZone>, Vec<Line>) {
        let zones = vec![
            zone((10, 0), (10, 1), Prompt),
            zone((10, 2), (10, 1 + input.chars().count()), Input),
        ];
        (zones, vec![row(&[("$ ", Prompt), (input, Input)], false)])
    }

    #[test]
    fn a_click_in_the_command_line_moves_its_cursor_there() {
        // "$ echo hello", the cursor at its end, column 12.
        let (zones, rows) = prompt_and("echo hello");
        let click = |x| cursor_move_for_click(SHELL, &zones, pane(&rows), (10, 12), (10, x));
        assert_eq!(click(7), left(5));
        assert_eq!(click(2), left(10));
        // At the cursor, past the text, or on the prompt: nothing to move.
        assert_eq!(click(12), None);
        assert_eq!(click(40), None);
        assert_eq!(click(0), None);
        assert_eq!(
            cursor_move_for_click(SHELL, &zones, pane(&rows), (10, 2), (10, 7)),
            right(5)
        );
    }

    #[test]
    fn a_click_past_the_text_stops_at_its_end_not_at_a_right_prompt() {
        let (zones, rows) = prompt_and("ls -la");
        assert_eq!(
            cursor_move_for_click(SHELL, &zones, pane(&rows), (10, 2), (10, 50)),
            right(6)
        );
        // Spaces typed after the text are the command line's too: a click
        // past them leaves the cursor after them.
        let (zones, rows) = prompt_and("ls  ");
        assert_eq!(
            cursor_move_for_click(SHELL, &zones, pane(&rows), (10, 6), (10, 40)),
            None
        );
        // zsh's right prompt shares the row: "$ ls" ... "~/src" at 30.
        let zones = vec![
            zone((10, 0), (10, 1), Prompt),
            zone((10, 2), (10, 3), Input),
            zone((10, 30), (10, 34), Prompt),
        ];
        let gap = " ".repeat(26);
        let rows = vec![row(
            &[
                ("$ ", Prompt),
                ("ls", Input),
                (&gap, Output),
                ("~/src", Prompt),
            ],
            false,
        )];
        assert_eq!(
            cursor_move_for_click(SHELL, &zones, pane(&rows), (10, 2), (10, 32)),
            right(2)
        );
    }

    #[test]
    fn a_wide_character_takes_one_press() {
        // "日本" fills columns 7 through 10; the cursor at the end, column 13.
        let (zones, rows) = prompt_and("echo 日本 x");
        let zones = vec![zones[0], zone((10, 2), (10, 12), Input)];
        assert_eq!(
            cursor_move_for_click(SHELL, &zones, pane(&rows), (10, 13), (10, 7)),
            left(4)
        );
    }

    #[test]
    fn a_wrapped_command_line_is_one_line_but_a_hard_break_is_not() {
        let zones = vec![
            zone((10, 0), (10, 1), Prompt),
            zone((10, 2), (11, 2), Input),
        ];
        let rows = |wrapped| {
            vec![
                row(&[("$ ", Prompt), ("abcdefgh", Input)], wrapped),
                row(&[("ijk", Input)], false),
            ]
        };
        // The cursor after "k" (11, 3), the click on "c" (10, 4): k j i h g f e d c.
        assert_eq!(
            cursor_move_for_click(SHELL, &zones, pane(&rows(true)), (11, 3), (10, 4)),
            left(9)
        );
        assert_eq!(
            cursor_move_for_click(SHELL, &zones, pane(&rows(false)), (11, 3), (10, 4)),
            None
        );
    }

    #[test]
    fn output_and_other_rows_are_not_the_command_line() {
        let zones = vec![
            zone((9, 0), (9, 4), Output),
            zone((10, 0), (10, 1), Prompt),
            zone((10, 2), (10, 4), Input),
        ];
        let rows = vec![row(&[("$ ", Prompt), ("pwd", Input)], false)];
        // The output above.
        assert_eq!(
            cursor_move_for_click(SHELL, &zones, pane(&rows), (10, 5), (9, 2)),
            None
        );
        // The cursor out of the input, as while a command runs.
        assert_eq!(
            cursor_move_for_click(SHELL, &zones, pane(&rows), (11, 0), (10, 3)),
            None
        );
    }

    #[test]
    fn the_command_line_is_the_one_holding_the_cursor_not_an_earlier_one() {
        // "$ ls" ran on row 10 and printed row 11; "$ echo hi" is on row 12,
        // the cursor at its end, column 9.
        let zones = vec![
            zone((10, 0), (10, 1), Prompt),
            zone((10, 2), (10, 3), Input),
            zone((11, 0), (11, 3), Output),
            zone((12, 0), (12, 1), Prompt),
            zone((12, 2), (12, 8), Input),
        ];
        let rows = vec![
            row(&[("$ ", Prompt), ("ls", Input)], false),
            row(&[("a.rs", Output)], false),
            row(&[("$ ", Prompt), ("echo hi", Input)], false),
        ];
        assert_eq!(
            cursor_move_for_click(SHELL, &zones, pane(&rows), (12, 9), (12, 7)),
            left(2)
        );
        // The earlier command's line is not this one.
        assert_eq!(
            cursor_move_for_click(SHELL, &zones, pane(&rows), (12, 9), (10, 3)),
            None
        );
    }

    #[test]
    fn the_cursor_row_rule_is_opt_in_and_keeps_to_the_cursors_row() {
        // An input box a program drew, with no shell integration.
        let rows = vec![row(&[("> fix the bug", Output)], false)];
        let both = ClickToMove {
            shell_integration: true,
            cursor_row: true,
        };
        assert_eq!(
            cursor_move_for_click(SHELL, &[], pane(&rows), (10, 13), (10, 6)),
            None
        );
        assert_eq!(
            cursor_move_for_click(both, &[], pane(&rows), (10, 13), (10, 6)),
            left(7)
        );
        assert_eq!(
            cursor_move_for_click(both, &[], pane(&rows), (10, 13), (11, 6)),
            None
        );
        // With the shell rule off, a shell's command line takes the row rule.
        let (zones, rows) = prompt_and("echo hello");
        let row_only = ClickToMove {
            shell_integration: false,
            cursor_row: true,
        };
        assert_eq!(
            cursor_move_for_click(row_only, &zones, pane(&rows), (10, 12), (10, 7)),
            left(5)
        );
    }

    #[test]
    fn a_click_too_far_away_is_not_sent() {
        let long = "x".repeat(MAX_PRESSES + 10);
        let zones = vec![
            zone((10, 0), (10, 1), Prompt),
            zone((10, 2), (10, 1 + long.len()), Input),
        ];
        let rows = vec![row(&[("$ ", Prompt), (long.as_str(), Input)], false)];
        assert_eq!(
            cursor_move_for_click(SHELL, &zones, pane(&rows), (10, 2 + long.len()), (10, 2)),
            None
        );
    }
}
