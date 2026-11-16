//! Character protection (DECSCA, SPA/EPA) with the erases that honor it, SL
//! and SR, and DEC Special Graphics 0x5F. Every expectation follows xterm's
//! source, $XTermId charproc.c 1.2133, util.c 1.1027, screen.c 1.680 and
//! charsets.c 1.131:
//! - `CASE_DECSCA` sets the DEC protection mode, and protects what is printed
//!   next for Ps 1, not for 0 or 2. `CASE_SPA` sets the ISO mode and
//!   protects; `CASE_EPA` stops protecting, leaving the mode.
//! - `do_erase_char`, `do_erase_line` and `do_erase_display` honor protection
//!   in the ISO mode for every erase, and in the DEC mode only for DECSED and
//!   DECSEL. `ClearInLine2` then erases the unprotected cells only, and
//!   `ClearCells` leaves the erased cells unprotected.
//! - `do_erase_display` case 2 turns protection off when it keeps no
//!   protected cell; ED 0 from the home position and ED 1 from the last cell
//!   take that case.
//! - `resetRendition` (SGR 0) keeps PROTECTED; `ReallyReset` (DECSTR, RIS)
//!   clears it and turns protection off.
//! - `xtermScrollLR` (SL, SR) deletes or inserts n cells at the left margin
//!   of every row within the top and bottom margins, when the cursor is
//!   inside the margins (`xtermColScroll`); 0 counts as 1.
//! - `xtermCharSetIn` maps DEC Special Graphics 0x5F to the blank.

use super::*;
use crate::color::ColorAttribute;
use k9::assert_equal as assert_eq;

/// Row `y`'s text with trailing blanks dropped.
fn row(term: &TestTerm, y: usize) -> String {
    term.screen().visible_lines()[y]
        .as_str()
        .trim_end()
        .to_string()
}

fn rows(term: &TestTerm) -> Vec<String> {
    (0..term.screen().physical_rows)
        .map(|y| row(term, y))
        .collect()
}

/// Whether the cell at column `x` of row `y` is protected.
fn protected(term: &TestTerm, x: usize, y: usize) -> bool {
    term.screen().visible_lines()[y]
        .visible_cells()
        .find(|cell| cell.cell_index() == x)
        .is_some_and(|cell| cell.attrs().protected())
}

fn digits(term: &mut TestTerm) {
    term.print("0123456789\r\nabcdefghij\r\nklmnopqrst\r\nuvwxyz0123\r\nABCDEFGHIJ");
}

#[test]
fn decsca_cells_survive_decsel_and_decsed_but_not_ed() {
    let mut term = TestTerm::new(5, 10, 0);
    term.print("\x1b[1\"qAB\x1b[0\"qcd");
    assert!(protected(&term, 0, 0) && protected(&term, 1, 0));
    assert!(!protected(&term, 2, 0));

    term.print("\x1b[?2K");
    assert_eq!(row(&term, 0), "AB");

    term.print("\x1b[2;1H\x1b[1\"qXY\x1b[2\"qzz\x1b[?2J");
    assert_eq!(rows(&term), vec!["AB", "XY", "", "", ""]);

    // In the DEC mode, ED ignores protection.
    term.print("\x1b[2J");
    assert_eq!(rows(&term), vec!["", "", "", "", ""]);
}

#[test]
fn decsel_and_decsed_parts_keep_protected_cells() {
    let mut term = TestTerm::new(5, 10, 0);
    term.print("ab\x1b[1\"qPP\x1b[0\"qcd\r\nefgh");
    // DECSEL 0 from the second P: only "cd" goes.
    term.print("\x1b[1;4H\x1b[?0K");
    assert_eq!(row(&term, 0), "abPP");
    // DECSEL 1 from the same cell: "ab" goes, the Ps stay.
    term.print("\x1b[?1K");
    assert_eq!(row(&term, 0), "  PP");
    // DECSED 0 from the home position erases everything unprotected.
    term.print("\x1b[H\x1b[?J");
    assert_eq!(rows(&term), vec!["  PP", "", "", "", ""]);
}

#[test]
fn a_protected_wide_cell_survives_decsel_whole() {
    let mut term = TestTerm::new(5, 10, 0);
    term.print("x\x1b[1\"q\u{4e2d}\x1b[0\"qy\x1b[?2K");
    assert_eq!(row(&term, 0), " \u{4e2d}");
    assert!(protected(&term, 1, 0));
}

#[test]
fn sgr_0_keeps_protection_and_erased_cells_are_unprotected() {
    let mut term = TestTerm::new(5, 10, 0);
    term.print("\x1b[1\"q\x1b[1mA\x1b[0mB\x1b[0\"qC\x1b[?2K");
    assert_eq!(row(&term, 0), "AB");
    // ECH leaves an unprotected blank even with the pen protecting.
    term.print("\x1b[1\"q\x1b[1;5HZ\x1b[1;5H\x1b[0\"q\x1b[1X\x1b[1\"q\x1b[1;7H\x1b[1X");
    assert!(!protected(&term, 4, 0));
    assert!(!protected(&term, 6, 0));
}

#[test]
fn spa_cells_survive_ed_el_and_ech() {
    let mut term = TestTerm::new(5, 10, 0);
    term.print("\x1bV\x1b[2;3HPROT\x1bW\x1b[2;7Hfree\x1b[3;1Hmore");
    assert_eq!(row(&term, 1), "  PROTfree");
    term.print("\x1b[2;4H\x1b[K\x1b[1K\x1b[2;1H\x1b[10X");
    assert_eq!(row(&term, 1), "  PROT");
    term.print("\x1b[2J");
    assert_eq!(rows(&term), vec!["", "  PROT", "", "", ""]);

    // A later DECSCA selects the DEC mode, in which ED erases them.
    term.print("\x1b[0\"q\x1b[2J");
    assert_eq!(rows(&term), vec!["", "", "", "", ""]);
}

#[test]
fn a_whole_screen_erase_that_keeps_nothing_turns_protection_off() {
    let mut term = TestTerm::new(5, 10, 0);
    // SPA, then an ED 2 with nothing protected yet: the mode goes off, so
    // the cells printed after it, protected as they are, no longer survive
    // EL (util.c do_erase_display case 2, "reset the protected mode flag").
    term.print("\x1bV\x1b[2Jlost\x1b[2K");
    assert_eq!(row(&term, 0), "");

    // With a protected cell left, the mode stays.
    term.print("\x1bV\x1b[Hkept\x1b[2J\x1b[2K");
    assert_eq!(row(&term, 0), "kept");

    // ED 0 from the home position, and ED 1 from the last cell, erase the
    // whole screen too; from elsewhere they leave the mode.
    term.print("\x1b[0\"q\x1b[2J\x1bV\x1b[H\x1b[Jgone\x1b[2K");
    assert_eq!(row(&term, 0), "");
    term.print("\x1bV\x1b[5;10H\x1b[1J\x1b[Hgone\x1b[2K");
    assert_eq!(row(&term, 0), "");
    term.print("\x1bV\x1b[2;1H\x1b[J\x1b[Hstay\x1b[2K");
    assert_eq!(row(&term, 0), "stay");
}

#[test]
fn decstr_and_ris_turn_protection_off() {
    let mut term = TestTerm::new(5, 10, 0);
    term.print("\x1b[1\"qAB\x1b[!pcd");
    assert!(protected(&term, 0, 0));
    assert!(!protected(&term, 2, 0));
    // DECSEL with protection off erases protected cells too.
    term.print("\x1b[?2K");
    assert_eq!(row(&term, 0), "");

    term.print("\x1bV\x1b[1;1HAB\x1bc\x1b[1;1Hcd");
    assert_eq!(row(&term, 0), "cd");
    assert!(!protected(&term, 0, 0));
}

#[test]
fn sl_and_sr_move_the_columns_within_the_top_and_bottom_margins() {
    let mut term = TestTerm::new(5, 10, 0);
    digits(&mut term);
    term.print("\x1b[2;4r\x1b[3;5H\x1b[2 @");
    assert_eq!(
        rows(&term),
        vec![
            "0123456789",
            "cdefghij",
            "mnopqrst",
            "wxyz0123",
            "ABCDEFGHIJ"
        ]
    );
    assert_eq!(term.cursor_pos().x, 4);
    assert_eq!(term.cursor_pos().y, 2);

    // 0 and the default count as 1.
    term.print("\x1b[0 A\x1b[ A");
    assert_eq!(
        rows(&term),
        vec![
            "0123456789",
            "  cdefghij",
            "  mnopqrst",
            "  wxyz0123",
            "ABCDEFGHIJ"
        ]
    );

    // A count past the region's width clears it.
    term.print("\x1b[99 @");
    assert_eq!(rows(&term), vec!["0123456789", "", "", "", "ABCDEFGHIJ"]);
}

#[test]
fn sl_and_sr_stay_within_left_and_right_margins() {
    let mut term = TestTerm::new(5, 10, 0);
    digits(&mut term);
    term.print("\x1b[?69h\x1b[3;7s\x1b[2;4r\x1b[2;4H\x1b[ @");
    assert_eq!(
        rows(&term),
        vec![
            "0123456789",
            "abdefg hij",
            "klnopq rst",
            "uvxyz0 123",
            "ABCDEFGHIJ"
        ]
    );
    // Cells pushed past the right margin are lost; the columns after it stay.
    term.print("\x1b[2 A");
    assert_eq!(
        rows(&term),
        vec![
            "0123456789",
            "ab  defhij",
            "kl  noprst",
            "uv  xyz123",
            "ABCDEFGHIJ"
        ]
    );
}

/// The background of the cell at column `x` of row `y`.
fn background(term: &TestTerm, x: usize, y: usize) -> ColorAttribute {
    term.screen().visible_lines()[y]
        .visible_cells()
        .find(|cell| cell.cell_index() == x)
        .map_or(ColorAttribute::Default, |cell| cell.attrs().background())
}

#[test]
fn sl_and_sr_fill_with_the_current_colours() {
    let mut term = TestTerm::new(5, 10, 0);
    digits(&mut term);
    // SR inserts blanks as xterm's ScrnInsertChar clears them (ClearCells,
    // screen.c): with the current colours.
    term.print("\x1b[44m\x1b[1;1H\x1b[2 A");
    assert_eq!(row(&term, 0), "  01234567");
    assert_eq!(background(&term, 0, 0), ColorAttribute::PaletteIndex(4));
    assert_eq!(background(&term, 1, 0), ColorAttribute::PaletteIndex(4));
    assert_eq!(background(&term, 2, 0), ColorAttribute::Default);
    // SL's blanks at the right margin take them too.
    term.print("\x1b[41m\x1b[3 @");
    assert_eq!(row(&term, 0), "1234567");
    assert_eq!(background(&term, 6, 0), ColorAttribute::Default);
    assert_eq!(background(&term, 7, 0), ColorAttribute::PaletteIndex(1));
    assert_eq!(background(&term, 9, 0), ColorAttribute::PaletteIndex(1));
}

#[test]
fn sl_from_outside_the_margins_does_nothing() {
    let mut term = TestTerm::new(5, 10, 0);
    digits(&mut term);
    term.print("\x1b[2;4r\x1b[1;1H\x1b[ @\x1b[5;1H\x1b[ A");
    assert_eq!(
        rows(&term),
        vec![
            "0123456789",
            "abcdefghij",
            "klmnopqrst",
            "uvwxyz0123",
            "ABCDEFGHIJ"
        ]
    );
    term.print("\x1b[r\x1b[?69h\x1b[3;7s\x1b[2;1H\x1b[ @\x1b[2;9H\x1b[ A");
    assert_eq!(row(&term, 1), "abcdefghij");
}

#[test]
fn dec_special_graphics_0x5f_is_a_blank() {
    let mut term = TestTerm::new(5, 10, 0);
    term.print("\x1b(0^_`\x1b(B_");
    assert_eq!(row(&term, 0), "^ \u{25c6}_");
}
