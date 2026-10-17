//! The esctest-style cases, grouped by control family. Each expectation is
//! what xterm documents (ctlseqs) or the DEC VT510 reference manual
//! specifies; `reference` names the source. "ctlseqs" is
//! https://invisible-island.net/xterm/ctlseqs/ctlseqs.html and "VT510" is
//! https://vt100.net/docs/vt510-rm/.

use frankenterm_term::ClipboardSelection;

use super::esctest::{escape_bytes, Check, EscCase, Session};

const DIGITS: [&str; 5] = [
    "1111111111",
    "2222222222",
    "3333333333",
    "4444444444",
    "5555555555",
];

/// A 5x10 screen whose rows are all `1`s, `2`s, ... `5`s, cursor at 1,1.
fn digits(t: &mut Session) {
    t.fill(1, &DIGITS);
    t.feed("\x1b[H");
}

fn case(id: &'static str, reference: &'static str, run: fn(&mut Session) -> Check) -> EscCase {
    EscCase {
        id,
        reference,
        rows: 24,
        cols: 80,
        run,
    }
}

/// A 5x10 screen, for cases that check whole rows.
fn small(id: &'static str, reference: &'static str, run: fn(&mut Session) -> Check) -> EscCase {
    EscCase {
        id,
        reference,
        rows: 5,
        cols: 10,
        run,
    }
}

pub fn all() -> Vec<EscCase> {
    let mut cases = Vec::new();
    cases.extend(cursor_positioning());
    cases.extend(cursor_motion());
    cases.extend(line_feeds_and_tabs());
    cases.extend(erasing());
    cases.extend(inserting_and_deleting());
    cases.extend(margins_and_modes());
    cases.extend(save_restore());
    cases.extend(rendition());
    cases.extend(charsets_and_screen_ops());
    cases.extend(alternate_screen());
    cases.extend(reports());
    cases.extend(operating_system_commands());
    cases.extend(resets());
    cases.extend(utf8());
    cases
}

fn cursor_positioning() -> Vec<EscCase> {
    vec![
        case("cup/default-params-home", "ctlseqs CUP: default 1;1", |t| {
            t.feed("\x1b[5;5H\x1b[H");
            t.cursor(1, 1)
        }),
        case("cup/row-only", "ctlseqs CUP: missing column is 1", |t| {
            t.feed("\x1b[6H");
            t.cursor(6, 1)
        }),
        case("cup/column-only", "ctlseqs CUP: missing row is 1", |t| {
            t.feed("\x1b[;6H");
            t.cursor(1, 6)
        }),
        case("cup/zero-means-one", "VT510 CUP: 0 is treated as 1", |t| {
            t.feed("\x1b[3;3H\x1b[0;0H");
            t.cursor(1, 1)
        }),
        case(
            "cup/clamps-to-screen",
            "VT510 CUP: stops at the last line and column",
            |t| {
                t.feed("\x1b[999;999H");
                t.cursor(24, 80)
            },
        ),
        case(
            "cup/origin-mode-is-relative",
            "VT510 DECOM: CUP relative to the top margin",
            |t| {
                t.feed("\x1b[5;10r\x1b[?6h\x1b[2;3H");
                t.cursor(6, 3)
            },
        ),
        case(
            "cup/origin-mode-clamps-to-region",
            "VT510 DECOM: cursor cannot leave the region",
            |t| {
                t.feed("\x1b[5;10r\x1b[?6h\x1b[99;1H");
                t.cursor(10, 1)
            },
        ),
        case("hvp/same-as-cup", "ctlseqs HVP", |t| {
            t.feed("\x1b[7;9f");
            t.cursor(7, 9)
        }),
        small(
            "cup/cancels-pending-wrap",
            "xterm: cursor addressing clears the wrap flag",
            |t| {
                t.feed("0123456789\x1b[1;10HX");
                t.rows(1, &["012345678X", ""])?;
                t.cursor(1, 10)
            },
        ),
        case("cha/absolute-column", "ctlseqs CHA", |t| {
            t.feed("\x1b[3;5H\x1b[10G");
            t.cursor(3, 10)
        }),
        case("cha/clamps-to-last-column", "VT510 CHA", |t| {
            t.feed("\x1b[999G");
            t.cursor(1, 80)
        }),
        case("hpa/absolute-column", "ctlseqs HPA", |t| {
            t.feed("\x1b[3;5H\x1b[12`");
            t.cursor(3, 12)
        }),
        case("vpa/absolute-row", "ctlseqs VPA", |t| {
            t.feed("\x1b[3;5H\x1b[9d");
            t.cursor(9, 5)
        }),
        case(
            "vpa/origin-mode-is-relative",
            "xterm VPA honors DECOM",
            |t| {
                t.feed("\x1b[5;10r\x1b[?6h\x1b[3d");
                t.cursor(7, 1)
            },
        ),
    ]
}

fn cursor_motion() -> Vec<EscCase> {
    vec![
        case("cuu/default-one", "ctlseqs CUU", |t| {
            t.feed("\x1b[5;5H\x1b[A");
            t.cursor(4, 5)
        }),
        case("cuu/stops-at-top-line", "VT510 CUU", |t| {
            t.feed("\x1b[3;5H\x1b[99A");
            t.cursor(1, 5)
        }),
        case(
            "cuu/stops-at-top-margin",
            "VT510 CUU: stops at the top margin inside the region",
            |t| {
                t.feed("\x1b[5;10r\x1b[7;1H\x1b[99A");
                t.cursor(5, 1)
            },
        ),
        case(
            "cuu/above-region-reaches-line-1",
            "VT510 CUU: above the region, stops at line 1",
            |t| {
                t.feed("\x1b[5;10r\x1b[3;1H\x1b[99A");
                t.cursor(1, 1)
            },
        ),
        case("cud/stops-at-bottom-margin", "VT510 CUD", |t| {
            t.feed("\x1b[5;10r\x1b[7;1H\x1b[99B");
            t.cursor(10, 1)
        }),
        case("cud/below-region-reaches-last-line", "VT510 CUD", |t| {
            t.feed("\x1b[5;10r\x1b[12;1H\x1b[99B");
            t.cursor(24, 1)
        }),
        case("cuf/stops-at-right-edge", "VT510 CUF", |t| {
            t.feed("\x1b[1;5H\x1b[999C");
            t.cursor(1, 80)
        }),
        case("cuf/never-wraps", "VT510 CUF", |t| {
            t.feed("\x1b[1;79H\x1b[5C");
            t.cursor(1, 80)
        }),
        case("cub/stops-at-left-edge", "VT510 CUB", |t| {
            t.feed("\x1b[1;5H\x1b[999D");
            t.cursor(1, 1)
        }),
        case("cub/zero-means-one", "VT510 CUB", |t| {
            t.feed("\x1b[1;5H\x1b[0D");
            t.cursor(1, 4)
        }),
        case("cnl/down-to-column-1", "ctlseqs CNL", |t| {
            t.feed("\x1b[3;5H\x1b[2E");
            t.cursor(5, 1)
        }),
        case("cpl/up-to-column-1", "ctlseqs CPL", |t| {
            t.feed("\x1b[5;5H\x1b[2F");
            t.cursor(3, 1)
        }),
        case("hpr/relative-column", "ctlseqs HPR", |t| {
            t.feed("\x1b[3;5H\x1b[3a");
            t.cursor(3, 8)
        }),
        case("vpr/relative-row", "ctlseqs VPR", |t| {
            t.feed("\x1b[3;5H\x1b[3e");
            t.cursor(6, 5)
        }),
        case("bs/stops-at-left-edge", "VT510 BS", |t| {
            t.feed("\x1b[1;1H\x08");
            t.cursor(1, 1)
        }),
        small(
            "bs/from-pending-wrap",
            "xterm: BS from the wrap position goes to the next-to-last column",
            |t| {
                t.feed("0123456789\x08");
                t.cursor(1, 9)
            },
        ),
        case("cr/to-column-1", "VT510 CR", |t| {
            t.feed("\x1b[2;5H\r");
            t.cursor(2, 1)
        }),
    ]
}

fn line_feeds_and_tabs() -> Vec<EscCase> {
    vec![
        small("lf/scrolls-at-bottom", "VT510 LF", |t| {
            t.feed("\x1b[1;1Hfirst\x1b[5;1Hlast\n");
            t.screen(&["", "", "", "last", ""])?;
            t.cursor(5, 5)
        }),
        small("lf/scrolls-only-the-region", "VT510 DECSTBM", |t| {
            t.feed("\x1b[1;1Htop\x1b[3;1Hin3\x1b[4;1Hin4\x1b[3;4r\x1b[4;1H\n");
            t.screen(&["top", "", "in4", "", ""])?;
            t.cursor(4, 1)
        }),
        small(
            "lf/below-region-does-not-scroll",
            "VT510 LF: below the bottom margin",
            |t| {
                t.feed("\x1b[3;4r\x1b[5;1Hx\n");
                t.screen(&["", "", "", "", "x"])?;
                t.cursor(5, 2)
            },
        ),
        small("ind/scrolls-at-bottom", "VT510 IND", |t| {
            t.feed("\x1b[5;1Hx\x1bD");
            t.screen(&["", "", "", "x", ""])?;
            t.cursor(5, 2)
        }),
        small("ri/scrolls-down-at-top", "VT510 RI", |t| {
            t.feed("first\x1b[1;1H\x1bM");
            t.screen(&["", "first"])?;
            t.cursor(1, 1)
        }),
        small("ri/scrolls-only-the-region", "VT510 RI", |t| {
            t.fill(3, &["three", "four", "five"]);
            t.feed("\x1b[3;4r\x1b[3;1H\x1bM");
            t.screen(&["", "", "", "three", "five"])?;
            t.cursor(3, 1)
        }),
        case("nel/is-cr-lf", "VT510 NEL", |t| {
            t.feed("\x1b[2;5H\x1bE");
            t.cursor(3, 1)
        }),
        case("lnm/lf-also-returns", "VT510 LNM", |t| {
            t.feed("\x1b[20h\x1b[2;5H\n");
            t.cursor(3, 1)?;
            t.mode("newline_mode", "true")
        }),
        case("ht/default-stops-every-8", "VT510 HT", |t| {
            t.feed("\t");
            t.cursor(1, 9)?;
            t.feed("\t");
            t.cursor(1, 17)
        }),
        case("ht/stops-at-last-column", "VT510 HT", |t| {
            t.feed("\x1b[1;75H\t");
            t.cursor(1, 80)
        }),
        case("hts/sets-a-stop", "VT510 HTS", |t| {
            t.feed("\x1b[1;5H\x1bH\x1b[1;1H\t");
            t.cursor(1, 5)
        }),
        case("tbc/0-clears-the-stop-at-the-cursor", "VT510 TBC", |t| {
            t.feed("\x1b[1;9H\x1b[0g\x1b[1;1H\t");
            t.cursor(1, 17)
        }),
        case("tbc/3-clears-every-stop", "VT510 TBC", |t| {
            t.feed("\x1b[3g\x1b[1;1H\t");
            t.cursor(1, 80)
        }),
        case("cht/forward-n-stops", "ctlseqs CHT", |t| {
            t.feed("\x1b[2I");
            t.cursor(1, 17)
        }),
        case("cbt/back-n-stops", "ctlseqs CBT", |t| {
            t.feed("\x1b[1;20H\x1b[2Z");
            t.cursor(1, 9)
        }),
        case("cbt/stops-at-column-1", "ctlseqs CBT", |t| {
            t.feed("\x1b[1;5H\x1b[9Z");
            t.cursor(1, 1)
        }),
    ]
}

fn erasing() -> Vec<EscCase> {
    vec![
        small("ed/0-erases-from-the-cursor", "VT510 ED 0", |t| {
            digits(t);
            t.feed("\x1b[3;5H\x1b[J");
            t.screen(&[DIGITS[0], DIGITS[1], "3333"])?;
            t.cursor(3, 5)
        }),
        small("ed/1-erases-through-the-cursor", "VT510 ED 1", |t| {
            digits(t);
            t.feed("\x1b[3;5H\x1b[1J");
            t.screen(&["", "", "     33333", DIGITS[3], DIGITS[4]])
        }),
        small("ed/2-erases-all-and-keeps-the-cursor", "VT510 ED 2", |t| {
            digits(t);
            t.feed("\x1b[3;5H\x1b[2J");
            t.screen(&[])?;
            t.cursor(3, 5)
        }),
        small("ed/ignores-the-scroll-region", "VT510 ED", |t| {
            digits(t);
            t.feed("\x1b[2;3r\x1b[2;1H\x1b[J");
            t.screen(&[DIGITS[0]])
        }),
        small(
            "ed/erased-cells-take-the-background",
            "xterm bce: ED fills with the current background",
            |t| {
                digits(t);
                t.feed("\x1b[44m\x1b[3;1H\x1b[J");
                t.attrs(3, 1, "bg=4")?;
                t.attrs(5, 10, "bg=4")?;
                t.attrs(2, 10, "")
            },
        ),
        small(
            "ed/3-keeps-the-screen",
            "ctlseqs ED 3: erases saved lines only",
            |t| {
                digits(t);
                t.feed("\x1b[3;5H\x1b[3J");
                t.screen(&DIGITS)
            },
        ),
        small("el/0-erases-to-the-right", "VT510 EL 0", |t| {
            digits(t);
            t.feed("\x1b[3;5H\x1b[K");
            t.rows(2, &[DIGITS[1], "3333", DIGITS[3]])?;
            t.cursor(3, 5)
        }),
        small("el/1-erases-through-the-cursor", "VT510 EL 1", |t| {
            digits(t);
            t.feed("\x1b[3;5H\x1b[1K");
            t.row(3, "     33333")
        }),
        small("el/2-erases-the-line", "VT510 EL 2", |t| {
            digits(t);
            t.feed("\x1b[3;5H\x1b[2K");
            t.rows(2, &[DIGITS[1], "", DIGITS[3]])
        }),
        small(
            "el/erased-cells-take-the-background",
            "xterm bce: EL fills with the current background",
            |t| {
                digits(t);
                t.feed("\x1b[41m\x1b[3;5H\x1b[K");
                t.attrs(3, 4, "")?;
                t.attrs(3, 5, "bg=1")?;
                t.attrs(3, 10, "bg=1")
            },
        ),
        small("ech/erases-without-shifting", "VT510 ECH", |t| {
            digits(t);
            t.feed("\x1b[3;3H\x1b[4X");
            t.row(3, "33    3333")?;
            t.cursor(3, 3)
        }),
        small("ech/default-one", "VT510 ECH", |t| {
            digits(t);
            t.feed("\x1b[3;3H\x1b[X");
            t.row(3, "33 3333333")
        }),
        small(
            "ech/stops-at-the-right-edge",
            "VT510 ECH: does not wrap",
            |t| {
                digits(t);
                t.feed("\x1b[3;8H\x1b[99X");
                t.rows(3, &["3333333", DIGITS[3]])
            },
        ),
    ]
}

fn inserting_and_deleting() -> Vec<EscCase> {
    vec![
        small("il/inserts-at-the-cursor-row", "VT510 IL", |t| {
            digits(t);
            t.feed("\x1b[2;3H\x1b[L");
            t.screen(&[DIGITS[0], "", DIGITS[1], DIGITS[2], DIGITS[3]])
        }),
        small("il/inserts-n-lines", "VT510 IL", |t| {
            digits(t);
            t.feed("\x1b[2;1H\x1b[2L");
            t.screen(&[DIGITS[0], "", "", DIGITS[1], DIGITS[2]])
        }),
        small("il/stays-inside-the-region", "VT510 IL", |t| {
            digits(t);
            t.feed("\x1b[2;4r\x1b[3;1H\x1b[L");
            t.screen(&[DIGITS[0], DIGITS[1], "", DIGITS[2], DIGITS[4]])
        }),
        small("il/outside-the-region-does-nothing", "VT510 IL", |t| {
            digits(t);
            t.feed("\x1b[2;4r\x1b[5;1H\x1b[L");
            t.screen(&DIGITS)
        }),
        small("il/new-lines-take-the-background", "xterm bce: IL", |t| {
            digits(t);
            t.feed("\x1b[42m\x1b[2;1H\x1b[L");
            t.attrs(2, 1, "bg=2")?;
            t.attrs(2, 10, "bg=2")
        }),
        small("dl/deletes-at-the-cursor-row", "VT510 DL", |t| {
            digits(t);
            t.feed("\x1b[2;1H\x1b[M");
            t.screen(&[DIGITS[0], DIGITS[2], DIGITS[3], DIGITS[4], ""])
        }),
        small("dl/stays-inside-the-region", "VT510 DL", |t| {
            digits(t);
            t.feed("\x1b[2;4r\x1b[2;1H\x1b[M");
            t.screen(&[DIGITS[0], DIGITS[2], DIGITS[3], "", DIGITS[4]])
        }),
        small("dl/count-past-the-region-clears-it", "VT510 DL", |t| {
            digits(t);
            t.feed("\x1b[2;4r\x1b[3;1H\x1b[99M");
            t.screen(&[DIGITS[0], DIGITS[1], "", "", DIGITS[4]])
        }),
        small("ich/inserts-blanks-shifting-right", "VT510 ICH", |t| {
            t.feed("abcdefghij\x1b[1;3H\x1b[2@");
            t.row(1, "ab  cdefgh")?;
            t.cursor(1, 3)
        }),
        small("ich/default-one", "VT510 ICH", |t| {
            t.feed("abcdefghij\x1b[1;3H\x1b[@");
            t.row(1, "ab cdefghi")
        }),
        small(
            "ich/stays-inside-the-right-margin",
            "VT510 ICH with DECLRMM",
            |t| {
                t.feed("abcdefghij\x1b[?69h\x1b[1;8s\x1b[1;3H\x1b[2@");
                t.row(1, "ab  cdefij")
            },
        ),
        small("dch/deletes-shifting-left", "VT510 DCH", |t| {
            t.feed("abcdefghij\x1b[1;3H\x1b[2P");
            t.row(1, "abefghij")?;
            t.cursor(1, 3)
        }),
        small("dch/default-one", "VT510 DCH", |t| {
            t.feed("abcdefghij\x1b[1;3H\x1b[P");
            t.row(1, "abdefghij")
        }),
        small(
            "dch/stays-inside-the-right-margin",
            "VT510 DCH with DECLRMM",
            |t| {
                t.feed("abcdefghij\x1b[?69h\x1b[1;8s\x1b[1;3H\x1b[2P");
                t.row(1, "abefgh  ij")
            },
        ),
        small("irm/printing-inserts", "VT510 IRM", |t| {
            t.feed("abcdef\x1b[1;3H\x1b[4hXY\x1b[4l");
            t.row(1, "abXYcdef")?;
            t.mode("insert", "false")
        }),
        case("rep/repeats-the-last-character", "ctlseqs REP", |t| {
            t.feed("a\x1b[3b");
            t.row(1, "aaaa")
        }),
        small("su/scrolls-up", "ctlseqs SU", |t| {
            digits(t);
            t.feed("\x1b[2S");
            t.screen(&[DIGITS[2], DIGITS[3], DIGITS[4]])
        }),
        small("sd/scrolls-down", "ctlseqs SD", |t| {
            digits(t);
            t.feed("\x1b[2T");
            t.screen(&["", "", DIGITS[0], DIGITS[1], DIGITS[2]])
        }),
        small("su/stays-inside-the-region", "ctlseqs SU", |t| {
            digits(t);
            t.feed("\x1b[2;4r\x1b[S");
            t.screen(&[DIGITS[0], DIGITS[2], DIGITS[3], "", DIGITS[4]])
        }),
    ]
}

fn margins_and_modes() -> Vec<EscCase> {
    vec![
        case("decstbm/homes-the-cursor", "VT510 DECSTBM", |t| {
            t.feed("\x1b[5;5H\x1b[3;10r");
            t.cursor(1, 1)
        }),
        case(
            "decstbm/no-params-is-the-full-screen",
            "VT510 DECSTBM",
            |t| {
                t.feed("\x1b[3;10r\x1b[r");
                t.mode("top_and_bottom_margins", "0..24")
            },
        ),
        case(
            "decstbm/top-past-bottom-is-ignored",
            "VT510 DECSTBM: top must be less than bottom",
            |t| {
                t.feed("\x1b[3;10r\x1b[10;3r");
                t.mode("top_and_bottom_margins", "2..10")
            },
        ),
        case(
            "decstbm/one-line-region-is-ignored",
            "VT510 DECSTBM: the minimum region is two lines",
            |t| {
                t.feed("\x1b[5;5r");
                t.mode("top_and_bottom_margins", "0..24")
            },
        ),
        case("decom/homes-to-the-region", "VT510 DECOM", |t| {
            t.feed("\x1b[5;10r\x1b[?6h");
            t.cursor(5, 1)
        }),
        case("decom/reset-homes-to-the-screen", "VT510 DECOM", |t| {
            t.feed("\x1b[5;10r\x1b[?6h\x1b[?6l");
            t.cursor(1, 1)
        }),
        case("decom/cursor-stays-in-the-region", "VT510 DECOM", |t| {
            t.feed("\x1b[5;10r\x1b[?6h\x1b[99A");
            t.cursor(5, 1)?;
            t.feed("\x1b[99B");
            t.cursor(10, 1)
        }),
        small("decawm/wraps-by-default", "VT510 DECAWM", |t| {
            t.feed("0123456789AB");
            t.rows(1, &["0123456789", "AB"])?;
            t.wrapped(1, true)?;
            t.cursor(2, 3)
        }),
        small(
            "decawm/off-overwrites-the-last-column",
            "VT510 DECAWM",
            |t| {
                t.feed("\x1b[?7l0123456789AB");
                t.rows(1, &["012345678B", ""])?;
                t.cursor(1, 10)
            },
        ),
        small(
            "decawm/last-column-defers-the-wrap",
            "xterm: the wrap happens on the next printable",
            |t| {
                t.feed("0123456789");
                t.rows(1, &["0123456789", ""])?;
                t.cursor(1, 10)?;
                t.mode("wrap_next", "true")
            },
        ),
        small(
            "decawm/cr-cancels-the-deferred-wrap",
            "xterm wrap flag",
            |t| {
                t.feed("0123456789\rX");
                t.rows(1, &["X123456789", ""])
            },
        ),
        small("decawm/wrap-at-the-bottom-scrolls", "VT510 DECAWM", |t| {
            t.feed("\x1b[5;1H0123456789X");
            t.rows(4, &["0123456789", "X"])
        }),
        small(
            "decawm/wide-char-on-the-last-column-wraps-whole",
            "xterm charproc.c dotext: a wide character with one column left forces the wrap",
            |t| {
                t.feed("012345678\u{4e2d}");
                t.rows(1, &["012345678", "\u{4e2d}"])?;
                t.wrapped(1, true)?;
                t.cursor(2, 3)
            },
        ),
        small(
            "decawm/off-drops-a-wide-char-on-the-last-column",
            "xterm charproc.c dotext: without WRAPAROUND it is not written",
            |t| {
                t.feed("\x1b[?7l012345678\u{4e2d}");
                t.rows(1, &["012345678", ""])?;
                t.cursor(1, 10)
            },
        ),
        small(
            "decawm/off-a-later-mark-joins-the-last-columns-char",
            "Ghostty Terminal.zig print: without wraparound, the last column's text takes a continuation",
            |t| {
                t.feed("\x1b[?7l012345678e");
                t.feed("\u{301}");
                t.rows(1, &["012345678e\u{301}", ""])?;
                t.cursor(1, 10)
            },
        ),
        small(
            "declrmm/wide-char-on-the-right-margin-wraps-to-the-left-margin",
            "xterm charproc.c dotext: the wrap goes to the left margin",
            |t| {
                t.feed("\x1b[?69h\x1b[3;6s\x1b[1;3Habc\u{4e2d}");
                t.rows(1, &["  abc", "  \u{4e2d}"])?;
                t.cursor(2, 5)
            },
        ),
        small(
            "declrmm/text-wraps-at-the-right-margin",
            "VT510 DECSLRM",
            |t| {
                t.feed("\x1b[?69h\x1b[3;6s\x1b[1;3Habcdefg");
                t.rows(1, &["  abcd", "  efg"])
            },
        ),
        case("dectcem/hides-and-shows-the-cursor", "VT510 DECTCEM", |t| {
            t.feed("\x1b[?25l");
            t.cursor_visible(false)?;
            t.feed("\x1b[?25h");
            t.cursor_visible(true)
        }),
        case(
            "bracketed-paste/2004-sets-and-resets",
            "ctlseqs mode 2004",
            |t| {
                t.feed("\x1b[?2004h");
                t.mode("bracketed_paste", "true")?;
                t.feed("\x1b[?2004l");
                t.mode("bracketed_paste", "false")
            },
        ),
        case(
            "bracketed-paste/decrqm-reports-it",
            "ctlseqs DECRQM for mode 2004",
            |t| {
                t.feed("\x1b[?2004h\x1b[?2004$p");
                t.reply(b"\x1b[?2004;1$y")?;
                t.feed("\x1b[?2004l\x1b[?2004$p");
                t.reply(b"\x1b[?2004;2$y")
            },
        ),
    ]
}

fn save_restore() -> Vec<EscCase> {
    vec![
        case("decsc/restores-the-position", "VT510 DECSC/DECRC", |t| {
            t.feed("\x1b[3;4H\x1b7\x1b[10;10H\x1b8");
            t.cursor(3, 4)
        }),
        case(
            "decsc/restores-the-rendition",
            "VT510 DECSC saves SGR",
            |t| {
                t.feed("\x1b[1;31m\x1b7\x1b[0m\x1b8X");
                t.attrs(1, 1, "bold fg=1")
            },
        ),
        case(
            "decsc/restores-origin-mode",
            "VT510 DECSC saves DECOM",
            |t| {
                t.feed("\x1b[5;10r\x1b[?6h\x1b7\x1b[?6l\x1b8");
                t.mode("dec_origin_mode", "true")?;
                t.feed("\x1b[H");
                t.cursor(5, 1)
            },
        ),
        case(
            "decsc/restores-the-charset",
            "VT510 DECSC saves G0-G3",
            |t| {
                t.feed("\x1b(0\x1b7\x1b(B\x1b8q");
                t.row(1, "\u{2500}")
            },
        ),
        case(
            "decrc/without-decsc-homes",
            "xterm: restore with nothing saved homes the cursor",
            |t| {
                t.feed("\x1b[5;5H\x1b8");
                t.cursor(1, 1)
            },
        ),
        case("scosc/csi-s-and-csi-u", "ctlseqs SCOSC/SCORC", |t| {
            t.feed("\x1b[3;4H\x1b[s\x1b[10;10H\x1b[u");
            t.cursor(3, 4)
        }),
    ]
}

fn rendition() -> Vec<EscCase> {
    vec![
        case("sgr/1-bold", "ctlseqs SGR 1", |t| {
            t.feed("\x1b[1mX");
            t.attrs(1, 1, "bold")
        }),
        case("sgr/2-faint", "ctlseqs SGR 2", |t| {
            t.feed("\x1b[2mX");
            t.attrs(1, 1, "half")
        }),
        case("sgr/3-italic", "ctlseqs SGR 3", |t| {
            t.feed("\x1b[3mX");
            t.attrs(1, 1, "italic")
        }),
        case("sgr/4-underline", "ctlseqs SGR 4", |t| {
            t.feed("\x1b[4mX");
            t.attrs(1, 1, "underline=single")
        }),
        case(
            "sgr/21-double-underline",
            "ctlseqs SGR 21: doubly underlined",
            |t| {
                t.feed("\x1b[21mX");
                t.attrs(1, 1, "underline=double")
            },
        ),
        case("sgr/4-3-curly-underline", "kitty/xterm SGR 4:3", |t| {
            t.feed("\x1b[4:3mX");
            t.attrs(1, 1, "underline=curly")
        }),
        case("sgr/5-blink-6-rapid", "ctlseqs SGR 5, ECMA-48 SGR 6", |t| {
            t.feed("\x1b[5mX\x1b[0;6mY");
            t.attrs(1, 1, "blink=slow")?;
            t.attrs(1, 2, "blink=rapid")
        }),
        case("sgr/7-inverse", "ctlseqs SGR 7", |t| {
            t.feed("\x1b[7mX");
            t.attrs(1, 1, "reverse")
        }),
        case("sgr/8-invisible", "ctlseqs SGR 8", |t| {
            t.feed("\x1b[8mX");
            t.attrs(1, 1, "invisible")
        }),
        case("sgr/9-crossed-out", "ctlseqs SGR 9", |t| {
            t.feed("\x1b[9mX");
            t.attrs(1, 1, "strike")
        }),
        case("sgr/53-overline", "ECMA-48 SGR 53", |t| {
            t.feed("\x1b[53mX");
            t.attrs(1, 1, "overline")
        }),
        case("sgr/22-clears-bold-and-faint", "ctlseqs SGR 22", |t| {
            t.feed("\x1b[1m\x1b[2m\x1b[22mX");
            t.attrs(1, 1, "")
        }),
        case(
            "sgr/2x-and-55-clear-their-attributes",
            "ctlseqs SGR 23-29, ECMA-48 SGR 55",
            |t| {
                t.feed("\x1b[3;4;5;7;8;9;53m\x1b[23;24;25;27;28;29;55mX");
                t.attrs(1, 1, "")
            },
        ),
        case("sgr/0-resets-everything", "ctlseqs SGR 0", |t| {
            t.feed("\x1b[1;3;4;31;42m\x1b[0mX");
            t.attrs(1, 1, "")
        }),
        case(
            "sgr/no-params-resets-everything",
            "ctlseqs SGR: default 0",
            |t| {
                t.feed("\x1b[1;31m\x1b[mX");
                t.attrs(1, 1, "")
            },
        ),
        case("sgr/several-in-one-sequence", "ctlseqs SGR", |t| {
            t.feed("\x1b[1;4;31mX");
            t.attrs(1, 1, "bold underline=single fg=1")
        }),
        case("sgr/30-47-ansi-colors", "ctlseqs SGR 30-37, 40-47", |t| {
            t.feed("\x1b[31;42mX");
            t.attrs(1, 1, "fg=1 bg=2")
        }),
        case(
            "sgr/90-107-bright-colors",
            "ctlseqs SGR 90-97, 100-107",
            |t| {
                t.feed("\x1b[91;102mX");
                t.attrs(1, 1, "fg=9 bg=10")
            },
        ),
        case(
            "sgr/38-5-and-48-5-indexed",
            "ctlseqs SGR 38;5 and 48;5",
            |t| {
                t.feed("\x1b[38;5;200;48;5;17mX");
                t.attrs(1, 1, "fg=200 bg=17")
            },
        ),
        case("sgr/38-5-colon-form", "ctlseqs SGR 38:5", |t| {
            t.feed("\x1b[38:5:200mX");
            t.attrs(1, 1, "fg=200")
        }),
        case("sgr/38-2-direct-color", "ctlseqs SGR 38;2", |t| {
            t.feed("\x1b[38;2;10;20;30mX");
            t.attrs(1, 1, "fg=#0a141e")
        }),
        case(
            "sgr/38-2-colon-form-with-colorspace",
            "ITU T.416 / ctlseqs SGR 38:2::r:g:b",
            |t| {
                t.feed("\x1b[38:2::10:20:30mX");
                t.attrs(1, 1, "fg=#0a141e")
            },
        ),
        case("sgr/39-49-default-colors", "ctlseqs SGR 39, 49", |t| {
            t.feed("\x1b[31;42m\x1b[39;49mX");
            t.attrs(1, 1, "")
        }),
        case(
            "sgr/58-underline-color-and-59",
            "kitty/ctlseqs SGR 58, 59",
            |t| {
                t.feed("\x1b[4;58;5;3mX\x1b[59mY");
                t.attrs(1, 1, "underline=single ul=3")?;
                t.attrs(1, 2, "underline=single")
            },
        ),
        case("sgr/applies-to-later-cells-only", "ctlseqs SGR", |t| {
            t.feed("a\x1b[1mb\x1b[0mc");
            t.attrs(1, 1, "")?;
            t.attrs(1, 2, "bold")?;
            t.attrs(1, 3, "")
        }),
    ]
}

fn charsets_and_screen_ops() -> Vec<EscCase> {
    vec![
        case(
            "charset/dec-graphics-in-g0",
            "VT510 SCS: DEC Special Graphic",
            |t| {
                t.feed("\x1b(0lqk\x1b(Bq");
                t.row(1, "\u{250c}\u{2500}\u{2510}q")
            },
        ),
        case("charset/so-si-switch-to-g1", "VT510 SO/SI", |t| {
            t.feed("\x1b)0\x0eqx\x0fqx");
            t.row(1, "\u{2500}\u{2502}qx")
        }),
        case("charset/uk-national", "VT510 SCS: United Kingdom", |t| {
            t.feed("\x1b(A#\x1b(B#");
            t.row(1, "\u{a3}#")
        }),
        small("decaln/fills-with-e", "VT510 DECALN", |t| {
            t.feed("\x1b[3;3H\x1b#8");
            t.screen(&["EEEEEEEEEE"; 5])?;
            t.cursor(1, 1)
        }),
        small("decaln/resets-the-margins", "VT510 DECALN", |t| {
            t.feed("\x1b[2;3r\x1b#8");
            t.mode("top_and_bottom_margins", "0..5")
        }),
    ]
}

fn alternate_screen() -> Vec<EscCase> {
    vec![
        case(
            "altscreen/1049-switches-to-a-blank-screen",
            "ctlseqs mode 1049",
            |t| {
                t.feed("primary\x1b[2;3H\x1b[?1049h");
                t.mode("alt_screen_active", "true")?;
                t.row(1, "")?;
                t.cursor(2, 3)
            },
        ),
        case(
            "altscreen/1049-restores-screen-and-cursor",
            "ctlseqs mode 1049",
            |t| {
                t.feed("primary\x1b[2;3H\x1b[?1049halt\x1b[5;5H\x1b[?1049l");
                t.mode("alt_screen_active", "false")?;
                t.row(1, "primary")?;
                t.cursor(2, 3)
            },
        ),
        case(
            "altscreen/writes-never-reach-the-primary",
            "ctlseqs mode 1049",
            |t| {
                t.feed("primary\x1b[?1049h\x1b[2J\x1b[Hgarbage\x1b[?1049l");
                t.row(1, "primary")
            },
        ),
        case(
            "altscreen/47-keeps-the-alternate-contents",
            "ctlseqs mode 47: no clear on switch",
            |t| {
                t.feed("\x1b[?47hALT\x1b[?47l\x1b[?47h");
                t.row(1, "ALT")
            },
        ),
        case(
            "altscreen/1047-clears-on-leaving",
            "ctlseqs mode 1047",
            |t| {
                t.feed("\x1b[?1047hALT\x1b[?1047l\x1b[?1047h");
                t.row(1, "")
            },
        ),
        case(
            "altscreen/1048-saves-the-cursor",
            "ctlseqs mode 1048",
            |t| {
                t.feed("\x1b[3;4H\x1b[?1048h\x1b[10;10H\x1b[?1048l");
                t.cursor(3, 4)
            },
        ),
    ]
}

fn reports() -> Vec<EscCase> {
    vec![
        case("dsr/5-reports-ok", "VT510 DSR 5", |t| {
            t.feed("\x1b[5n");
            t.reply(b"\x1b[0n")
        }),
        case("dsr/6-reports-the-cursor", "VT510 CPR", |t| {
            t.feed("\x1b[3;7H\x1b[6n");
            t.reply(b"\x1b[3;7R")
        }),
        case(
            "dsr/6-is-relative-under-decom",
            "VT510 CPR: relative to the origin under DECOM",
            |t| {
                t.feed("\x1b[5;10r\x1b[?6h\x1b[3;4H\x1b[6n");
                t.reply(b"\x1b[3;4R")?;
                t.cursor(7, 4)
            },
        ),
        case(
            "da1/reports-a-vt-class",
            "VT510 DA1: CSI ? Pc ; Ps c with Pc 6x",
            |t| {
                t.feed("\x1b[c");
                let reply = t.take_replies();
                let text = String::from_utf8_lossy(&reply).into_owned();
                let ok = text.starts_with("\x1b[?6")
                    && text.ends_with('c')
                    && text[3..text.len() - 1]
                        .split(';')
                        .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
                if ok {
                    Ok(())
                } else {
                    Err(format!("DA1 reply {}", escape_bytes(&reply)))
                }
            },
        ),
        case("decrqm/decawm-is-set", "VT510 DECRQM", |t| {
            t.feed("\x1b[?7$p");
            t.reply(b"\x1b[?7;1$y")
        }),
        case("decrqm/decom-is-reset", "VT510 DECRQM", |t| {
            t.feed("\x1b[?6$p");
            t.reply(b"\x1b[?6;2$y")
        }),
        case(
            "decrqm/unknown-mode",
            "VT510 DECRQM: 0 means not recognized",
            |t| {
                t.feed("\x1b[?9999$p");
                t.reply(b"\x1b[?9999;0$y")
            },
        ),
        case("decrqm/ansi-irm", "VT510 DECRQM for ANSI modes", |t| {
            t.feed("\x1b[4h\x1b[4$p");
            t.reply(b"\x1b[4;1$y")
        }),
        case("decrqss/decstbm", "VT510 DECRQSS", |t| {
            t.feed("\x1b[5;10r\x1bP$qr\x1b\\");
            t.reply(b"\x1bP1$r5;10r\x1b\\")
        }),
        case("decrqss/sgr", "ctlseqs DECRQSS m", |t| {
            t.feed("\x1b[1;31m\x1bP$qm\x1b\\");
            t.reply(b"\x1bP1$r0;1;31m\x1b\\")
        }),
    ]
}

fn operating_system_commands() -> Vec<EscCase> {
    vec![
        case(
            "osc8/marks-the-linked-cells",
            "OSC 8 hyperlinks spec",
            |t| {
                t.feed("\x1b]8;;http://example.com/\x1b\\link\x1b]8;;\x1b\\ plain");
                t.attrs(1, 1, "link=http://example.com/")?;
                t.attrs(1, 4, "link=http://example.com/")?;
                t.attrs(1, 5, "")?;
                t.attrs(1, 6, "")
            },
        ),
        case(
            "osc8/keeps-the-id-parameter",
            "OSC 8 hyperlinks spec: id=",
            |t| {
                t.feed("\x1b]8;id=x1;http://a/\x07A\x1b]8;;\x07B");
                t.attrs(1, 1, "link=http://a/;id=x1")?;
                t.attrs(1, 2, "")
            },
        ),
        case("osc52/writes-the-clipboard", "ctlseqs OSC 52", |t| {
            t.feed("\x1b]52;c;aGVsbG8=\x07");
            t.clipboard(ClipboardSelection::Clipboard, Some("hello"))
        }),
        case(
            "osc52/primary-selection",
            "ctlseqs OSC 52: p is PRIMARY",
            |t| {
                t.feed("\x1b]52;p;d29ybGQ=\x1b\\");
                t.clipboard(ClipboardSelection::PrimarySelection, Some("world"))
            },
        ),
        case(
            "osc133/marks-prompt-input-and-output",
            "FinalTerm OSC 133 semantic prompts",
            |t| {
                t.feed("\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07out\r\n\x1b]133;D;0\x07");
                t.attrs(1, 1, "semantic=prompt")?;
                t.attrs(1, 3, "semantic=input")?;
                t.attrs(2, 1, "")
            },
        ),
        case("osc0/sets-the-title", "ctlseqs OSC 0", |t| {
            t.feed("\x1b]0;hello\x07");
            t.mode("title", "\"hello\"")
        }),
        case("osc2/sets-the-title", "ctlseqs OSC 2", |t| {
            t.feed("\x1b]2;world\x1b\\");
            t.mode("title", "\"world\"")
        }),
    ]
}

fn resets() -> Vec<EscCase> {
    vec![
        case("decstr/resets-modes-and-rendition", "VT510 DECSTR", |t| {
            t.feed("\x1b[4h\x1b[5;10r\x1b[?6h\x1b[1;31m\x1b[!pX");
            t.mode("insert", "false")?;
            t.mode("dec_origin_mode", "false")?;
            t.mode("top_and_bottom_margins", "0..24")?;
            let cursor = t.snapshot().cursor;
            let (row, col) = (cursor.y + 1, cursor.x);
            t.attrs(row as usize, col, "")
        }),
        case("ris/clears-and-homes", "VT510 RIS", |t| {
            t.feed("text\x1b[5;5H\x1b[1m\x1b[3;10r\x1bc");
            t.screen(&[])?;
            t.cursor(1, 1)?;
            t.mode("top_and_bottom_margins", "0..24")?;
            t.feed("X");
            t.attrs(1, 1, "")
        }),
    ]
}

/// Malformed input becomes one U+FFFD per maximal subpart (Unicode 15
/// section 3.9, "U+FFFD Substitution of Maximal Subparts", the practice the
/// W3C/WHATWG decoders follow). Nothing is dropped and no valid byte after a
/// broken sequence is swallowed.
fn utf8() -> Vec<EscCase> {
    const UNICODE: &str = "Unicode 15 section 3.9: maximal subparts";
    vec![
        case("utf8/multibyte-and-wide", "Unicode / UAX #11 widths", |t| {
            t.feed("a\u{e9}\u{4e2d}\u{1f600}b");
            t.row(1, "a\u{e9}\u{4e2d}\u{1f600}b")?;
            t.cursor(1, 8)
        }),
        case("utf8/split-across-reads", UNICODE, |t| {
            t.feed(b"a\xe4\xb8");
            t.feed(b"\xadb");
            t.row(1, "a\u{4e2d}b")
        }),
        case("utf8/lone-continuation-byte", UNICODE, |t| {
            t.feed(b"a\x80b");
            t.row(1, "a\u{fffd}b")
        }),
        case("utf8/truncated-lead-keeps-the-next-byte", UNICODE, |t| {
            t.feed(b"a\xc3b");
            t.row(1, "a\u{fffd}b")
        }),
        case("utf8/never-valid-bytes", UNICODE, |t| {
            t.feed(b"a\xc0\xc1\xf5\xffb");
            t.row(1, "a\u{fffd}\u{fffd}\u{fffd}\u{fffd}b")
        }),
        case("utf8/encoded-surrogate", UNICODE, |t| {
            t.feed(b"a\xed\xa0\x80b");
            t.row(1, "a\u{fffd}\u{fffd}\u{fffd}b")
        }),
        case("utf8/overlong-form", UNICODE, |t| {
            t.feed(b"a\xe0\x80\xafb");
            t.row(1, "a\u{fffd}\u{fffd}\u{fffd}b")
        }),
        case("utf8/truncated-four-byte-sequence", UNICODE, |t| {
            t.feed(b"a\xf0\x9f\x98b");
            t.row(1, "a\u{fffd}b")
        }),
        case("utf8/escape-interrupts-a-sequence", UNICODE, |t| {
            t.feed(b"a\xe4\x1b[1mb");
            t.row(1, "a\u{fffd}b")?;
            t.attrs(1, 3, "bold")
        }),
    ]
}
