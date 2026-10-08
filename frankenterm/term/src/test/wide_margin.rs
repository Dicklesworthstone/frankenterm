//! A wide grapheme arriving on the right margin's column (ft-b35o7).
//!
//! It is never clipped in that last column. Both references were read from
//! source:
//! - Ghostty e500d414f, `src/terminal/Terminal.zig` `print` (lines 1531-1567).
//!   With wraparound, the last column becomes an empty spacer head (a narrow
//!   blank when it is inside a right margin), the row wraps, and the wide
//!   character prints at the next row's left margin. Without it: "If we don't
//!   have wraparound enabled then we don't print this character at all and
//!   don't move the cursor. This is how xterm behaves." Its variation
//!   selector path (lines 1301-1365) wraps a narrow cell that VS16 widens on
//!   the last column the same way.
//! - xterm `charproc.c` 1.2133, `dotext`. A wide character with one column
//!   left backs off (`chars_chomped` drops to 0). Only
//!   `(xw->flags & WRAPAROUND)` then sets `force_wrap`, which wraps and writes
//!   the character at the next line's left margin. Without autowrap nothing
//!   is written and the cursor stays.
//!
//! FrankenTerm's spacer head is the empty end of the wrapped row: blank, and
//! not part of the row's text. So copying joins the next row's text without
//! a space, and reflow rewraps it like the underfull rows reflow leaves
//! before a wide grapheme itself.

use super::*;
use k9::assert_equal as assert_eq;

/// The bead's geometry: 120 columns (the T0 head-to-head's).
const COLS: usize = 120;
const EMOJI: &str = "\u{1f600}";

fn a_row() -> String {
    "A".repeat(COLS - 1)
}

/// The visible rows' graphemes: column, text and width.
fn cells(term: &TestTerm, y: usize) -> Vec<(usize, String, usize)> {
    term.screen().visible_lines()[y]
        .visible_cells()
        .map(|cell| (cell.cell_index(), cell.str().to_string(), cell.width()))
        .collect()
}

fn wrapped(term: &TestTerm, y: usize) -> bool {
    term.screen().visible_lines()[y].last_cell_was_wrapped()
}

/// The cursor's column and row. Printing moves the cursor without touching
/// its sequence number, so only the position is compared.
fn cursor_at(term: &TestTerm, x: usize, y: i64, reason: &str) {
    let cursor = term.cursor_pos();
    assert_eq!((cursor.x, cursor.y), (x, y), "{}", reason);
}

/// Unicode 14 widths (iTerm2's `UnicodeVersion`): emoji presentation, so
/// VS16 makes a text-presentation character two cells wide. The default,
/// Unicode 9, keeps U+2764 and "#" narrow with it.
const UNICODE_14: &str = "\x1b]1337;UnicodeVersion=14\x07";

/// The bead's first repro: 119 x "A", then U+1F600, at 120 columns. As in
/// Ghostty: the cursor at x=2 y=1, the emoji in row 1, columns 0-1, row 0
/// wrapped with nothing in its last column.
#[test]
fn a_wide_char_on_the_last_column_wraps_whole() {
    let mut term = TestTerm::new(4, COLS, 0);
    term.print(a_row());
    term.print(EMOJI);
    cursor_at(&term, 2, 1, "after the wrapped emoji");
    assert_visible_contents(&term, file!(), line!(), &[&a_row(), EMOJI, "", ""]);
    assert!(wrapped(&term, 0), "row 0 wraps into row 1");
    assert_eq!(
        cells(&term, 0).len(),
        COLS - 1,
        "the last column holds nothing"
    );
    assert_eq!(cells(&term, 1)[0], (0, EMOJI.to_string(), 2));
}

/// The bead's second repro: then "B". As in Ghostty: x=3 y=1, "B" right
/// after the emoji.
#[test]
fn text_after_the_wrapped_wide_char_follows_it() {
    let mut term = TestTerm::new(4, COLS, 0);
    term.print(format!("{}{EMOJI}B", a_row()));
    cursor_at(&term, 3, 1, "after B");
    let second = format!("{EMOJI}B");
    assert_visible_contents(&term, file!(), line!(), &[&a_row(), &second, "", ""]);
    assert_eq!(cells(&term, 1)[1], (2, "B".to_string(), 1));
    // Copying reads the rows' text and joins a wrapped row to the next with
    // nothing between: the spacer adds no space.
    let lines = term.screen().visible_lines();
    assert!(lines[0].last_cell_was_wrapped() && !lines[1].last_cell_was_wrapped());
    assert_eq!(
        format!("{}{}", lines[0].as_str(), lines[1].as_str()),
        format!("{}{EMOJI}B", a_row())
    );
}

/// The spacer is blank: what the last column held before is cleared, as
/// Ghostty's spacer head clears it.
#[test]
fn the_spacer_clears_the_last_column() {
    let mut term = TestTerm::new(4, COLS, 0);
    term.print(format!("{}\r{}{EMOJI}", "Z".repeat(COLS), a_row()));
    cursor_at(&term, 2, 1, "after the wrapped emoji");
    assert_visible_contents(&term, file!(), line!(), &[&a_row(), EMOJI, "", ""]);
}

/// With autowrap (DECAWM) off, as xterm and Ghostty do: the wide char is not
/// printed and the cursor stays on the last column, no wrap pending; the
/// next narrow char then prints there.
#[test]
fn without_autowrap_the_wide_char_is_not_printed() {
    let mut term = TestTerm::new(4, COLS, 0);
    term.print("\x1b[?7l");
    term.print(format!("{}{EMOJI}", a_row()));
    cursor_at(&term, COLS - 1, 0, "not moved");
    assert_visible_contents(&term, file!(), line!(), &[&a_row(), "", "", ""]);
    assert!(!wrapped(&term, 0));
    term.print("B");
    cursor_at(&term, COLS - 1, 0, "autowrap off");
    let first = format!("{}B", a_row());
    assert_visible_contents(&term, file!(), line!(), &[&first, "", "", ""]);
}

/// A variation selector that widens the last column's narrow cell wraps it
/// whole too, as Ghostty's VS16 path does. That holds whether U+2764 and VS16
/// arrive in one read (one wide grapheme) or in two, as a PTY can split them.
/// Without autowrap the selector is dropped and the cell stays narrow, as
/// Ghostty prints the base and then drops the selector. That is checked for
/// one read only: split, the selector meets ft-0b8ux (it joins the cell
/// before the cursor), a separate defect.
#[test]
fn a_cell_widened_on_the_last_column_wraps_whole() {
    let heart = "\u{2764}";
    let wide_heart = "\u{2764}\u{fe0f}";
    let feed = |term: &mut TestTerm, split: bool| {
        if split {
            term.print(format!("{}{heart}", a_row()));
            term.print("\u{fe0f}");
        } else {
            term.print(format!("{}{wide_heart}", a_row()));
        }
    };
    for split in [true, false] {
        let reason = format!("split: {split}");
        let mut term = TestTerm::new(4, COLS, 0);
        term.print(UNICODE_14);
        feed(&mut term, split);
        cursor_at(&term, 2, 1, &reason);
        assert_visible_contents(&term, file!(), line!(), &[&a_row(), wide_heart, "", ""]);
        assert!(wrapped(&term, 0));
        assert_eq!(cells(&term, 1)[0], (0, wide_heart.to_string(), 2));

        if split {
            continue;
        }
        let mut term = TestTerm::new(4, COLS, 0);
        term.print(UNICODE_14);
        term.print("\x1b[?7l");
        feed(&mut term, split);
        cursor_at(&term, COLS - 1, 0, &reason);
        let first = format!("{}{heart}", a_row());
        assert_visible_contents(&term, file!(), line!(), &[&first, "", "", ""]);
        assert_eq!(cells(&term, 0)[COLS - 1], (COLS - 1, heart.to_string(), 1));
    }
}

/// An ASCII keycap base on the last column widened by VS16 right after it in
/// the same read ("#" then U+FE0F is two cells wide). Written as a direct
/// ASCII run, the run's last cell is then the junction the selector joins.
/// It wraps whole like any other wide grapheme.
#[test]
fn a_keycap_widened_on_the_last_column_wraps_whole() {
    let mut term = TestTerm::new(4, COLS, 0);
    term.print(UNICODE_14);
    term.print(format!("{}#\u{fe0f}", "A".repeat(COLS - 1)));
    cursor_at(&term, 2, 1, "after the wrapped keycap");
    assert_visible_contents(&term, file!(), line!(), &[&a_row(), "#\u{fe0f}", "", ""]);
    assert_eq!(cells(&term, 1)[0], (0, "#\u{fe0f}".to_string(), 2));

    let mut term = TestTerm::new(4, COLS, 0);
    term.print(UNICODE_14);
    term.print("\x1b[?7l");
    term.print(format!("{}#\u{fe0f}", "A".repeat(COLS - 1)));
    cursor_at(&term, COLS - 1, 0, "not moved");
    let first = format!("{}#", a_row());
    assert_visible_contents(&term, file!(), line!(), &[&first, "", "", ""]);
}

/// On the bottom row of a scroll region (DECSTBM), the wrap scrolls the
/// region: the row with the spacer moves up within it, the emoji starts the
/// region's new bottom row, and the rows outside the region stay.
#[test]
fn a_wide_char_on_the_last_column_wraps_within_a_scroll_region() {
    let mut term = TestTerm::new(4, COLS, 0);
    term.print("r0\r\nr1\r\nr2\r\nr3");
    // Rows 2-3 (one-based) scroll; the cursor goes to row 3's last column.
    term.print(format!("\x1b[2;3r\x1b[3;{COLS}H{EMOJI}"));
    cursor_at(&term, 2, 2, "after the emoji, region bottom");
    assert_visible_contents(&term, file!(), line!(), &["r0", "r2", EMOJI, "r3"]);
    assert!(
        wrapped(&term, 1),
        "the scrolled-up row wraps into the emoji's"
    );
}

/// With left and right margins (DECLRMM, DECSLRM 1;10), the right margin's
/// column is cleared to a narrow blank (as Ghostty clears it inside a margin)
/// and the emoji goes to the next row's left margin. The columns past the
/// margin keep their text.
#[test]
fn a_wide_char_on_the_right_margin_wraps_to_the_left_margin() {
    let mut term = TestTerm::new(4, COLS, 0);
    term.print("Z".repeat(COLS));
    term.print(format!(
        "\x1b[?69h\x1b[1;10s\x1b[1;1H{}{EMOJI}",
        "A".repeat(9)
    ));
    cursor_at(&term, 2, 1, "after the emoji, left margin");
    let first = format!("{} {}", "A".repeat(9), "Z".repeat(COLS - 10));
    assert_visible_contents(&term, file!(), line!(), &[&first, EMOJI, "", ""]);
}

/// Reflow over the spacer neither duplicates nor loses a cell. Wider, the
/// rows join into one line of exactly what was printed; back at 120 columns
/// the emoji wraps whole again, after a row of 119 "A"s.
#[test]
fn reflow_over_the_spacer_keeps_every_cell_once() {
    let mut term = TestTerm::new(4, COLS, 0);
    term.print(format!("{}{EMOJI}B", a_row()));
    let printed = format!("{}{EMOJI}B", a_row());
    let size = |cols| TerminalSize {
        rows: 4,
        cols,
        ..Default::default()
    };

    term.resize(size(130));
    assert_visible_contents(&term, file!(), line!(), &[&printed, "", "", ""]);
    cursor_at(&term, COLS + 2, 0, "after B, one row");

    term.resize(size(COLS));
    let second = format!("{EMOJI}B");
    assert_visible_contents(&term, file!(), line!(), &[&a_row(), &second, "", ""]);
    assert!(wrapped(&term, 0));
    cursor_at(&term, 3, 1, "after B, wrapped again");
}
