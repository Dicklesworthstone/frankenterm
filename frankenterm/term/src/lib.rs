#![allow(clippy::unused_io_amount)]
#![allow(
    clippy::boxed_local,
    clippy::drop_non_drop,
    clippy::enum_variant_names,
    clippy::get_first,
    clippy::len_zero,
    clippy::manual_clamp,
    clippy::manual_find,
    clippy::manual_map,
    clippy::manual_unwrap_or_default,
    clippy::match_like_matches_macro,
    clippy::match_ref_pats,
    clippy::needless_borrow,
    clippy::needless_borrows_for_generic_args,
    clippy::needless_range_loop,
    clippy::result_large_err,
    clippy::single_match,
    clippy::unneeded_struct_pattern,
    clippy::unnecessary_cast,
    clippy::unnecessary_min_or_max,
    clippy::unwrap_or_default,
    clippy::upper_case_acronyms,
    clippy::useless_conversion,
    clippy::manual_unwrap_or
)]
//! This crate provides the core of the virtual terminal emulator implementation
//! used by [wezterm](https://wezterm.org/).  The home for this
//! crate is in the wezterm repo and development is tracked at
//! <https://github.com/wezterm/wezterm/>.
//!
//! It is full featured, providing terminal escape sequence parsing, keyboard
//! and mouse input encoding, a model for the screen cells including scrollback,
//! sixel and iTerm2 image support, OSC 8 Hyperlinks and a wide range of
//! terminal cell attributes.
//!
//! This crate does not provide any kind of gui, nor does it directly
//! manage a PTY; you provide a `std::io::Write` implementation that
//! could connect to a PTY, and supply bytes to the model via the
//! `advance_bytes` method.
//!
//! The entrypoint to the crate is the [Terminal](terminal/struct.Terminal.html)
//! struct.
use anyhow::Error;
use frankenterm_dynamic::{FromDynamic, ToDynamic};
use frankenterm_surface::SequenceNo;
#[cfg(feature = "use_serde")]
use serde::{Deserialize, Serialize};
use std::ops::{Deref, DerefMut, Range};
use std::str;

pub mod config;
pub use config::TerminalConfiguration;

pub mod input;
pub use crate::input::*;

pub use frankenterm_cell::*;
pub use frankenterm_surface::line::*;

pub mod screen;
pub use crate::screen::*;

pub mod terminal;
pub use crate::terminal::*;

pub mod terminalstate;
pub use crate::terminalstate::*;

/// Represents the index into screen.lines.  Index 0 is the top of
/// the scrollback (if any).  The index of the top of the visible screen
/// depends on the terminal dimensions and the scrollback size.
pub type PhysRowIndex = usize;

/// Represents an index into the visible portion of the screen.
/// Value 0 is the first visible row.  `VisibleRowIndex` needs to be
/// resolved into a `PhysRowIndex` to obtain an actual row.  It is not
/// valid to have a negative `VisibleRowIndex` value so this type logically
/// should be unsigned, however, having a different sign is helpful to
/// have the compiler catch accidental arithmetic performed between
/// `PhysRowIndex` and `VisibleRowIndex`.  We could define our own type with
/// its own `Add` and `Sub` operators, but then we'd not be able to iterate
/// over `Ranges` of these types without also laboriously implementing an
/// iterator `Skip` trait that is currently only in unstable rust.
pub type VisibleRowIndex = i64;

/// Like `VisibleRowIndex` above, but can index backwards into scrollback.
/// This is deliberately a differently sized signed type to catch
/// accidentally blending together the wrong types of indices.
/// This is explicitly 32-bit rather than 64-bit as it seems unreasonable
/// to want to scroll back or select more than ~2billion lines of scrollback.
pub type ScrollbackOrVisibleRowIndex = i32;

/// Allows referencing a logical line in the scrollback, allowing for scrolling.
/// The StableRowIndex counts from the top of the scrollback, growing larger
/// as you move down through the display rows.
/// Initially the very first line as StableRowIndex==0.  If the scrollback
/// is filled and lines are purged (say we need to purge 5 lines), then whichever
/// line is first in the scrollback (PhysRowIndex==0) will now have StableRowIndex==5
/// which is the same value that that logical line had prior to data being purged
/// out of the scrollback.
///
/// As per ScrollbackOrVisibleRowIndex above, a StableRowIndex can never
/// legally be a negative number.  We're just using a differently sized type
/// to have the compiler assist us in detecting improper usage.
pub type StableRowIndex = isize;

/// Returns true if r1 intersects r2
pub fn intersects_range<T: Ord + Copy>(r1: Range<T>, r2: Range<T>) -> bool {
    use std::cmp::{max, min};
    let start = max(r1.start, r2.start);
    let end = min(r1.end, r2.end);

    end > start
}

/// Position allows referring to an absolute visible row number
/// or a position relative to some existing row number (typically
/// where the cursor is located).  Both of the cases are represented
/// as signed numbers so that the math and error checking for out
/// of range values can be deferred to the point where we execute
/// the request.
#[derive(Debug)]
pub enum Position {
    Absolute(VisibleRowIndex),
    Relative(i64),
}

/// Describes the location of the cursor in the visible portion
/// of the screen.
#[cfg_attr(feature = "use_serde", derive(Deserialize, Serialize))]
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
pub struct CursorPosition {
    pub x: usize,
    pub y: VisibleRowIndex,
    pub shape: frankenterm_surface::CursorShape,
    pub visibility: frankenterm_surface::CursorVisibility,
    pub seqno: SequenceNo,
}

#[cfg_attr(feature = "use_serde", derive(Deserialize, Serialize))]
#[derive(Debug, Clone, Copy, Eq, PartialEq, PartialOrd, Ord, FromDynamic, ToDynamic)]
pub struct SemanticZone {
    pub start_y: StableRowIndex,
    pub start_x: usize,
    pub end_y: StableRowIndex,
    pub end_x: usize,
    pub semantic_type: SemanticType,
}

pub mod color;

#[cfg(test)]
mod test;

pub const CSI: &str = "\x1b[";
pub const OSC: &str = "\x1b]";
pub const ST: &str = "\x1b\\";
pub const SS3: &str = "\x1bO";
pub const DCS: &str = "\x1bP";

#[cfg(test)]
mod lib_tests {
    use super::*;

    // ── intersects_range ────────────────────────────────────

    #[test]
    fn overlapping_ranges_intersect() {
        assert!(intersects_range(0..5, 3..8));
    }

    #[test]
    fn identical_ranges_intersect() {
        assert!(intersects_range(0..5, 0..5));
    }

    #[test]
    fn contained_range_intersects() {
        assert!(intersects_range(0..10, 3..7));
    }

    #[test]
    fn disjoint_ranges_do_not_intersect() {
        assert!(!intersects_range(0..5, 5..10));
    }

    #[test]
    fn adjacent_ranges_do_not_intersect() {
        assert!(!intersects_range(0..3, 3..6));
    }

    #[test]
    fn reversed_order_intersects() {
        assert!(intersects_range(3..8, 0..5));
    }

    #[test]
    fn empty_range_does_not_intersect() {
        assert!(!intersects_range(5..5, 0..10));
    }

    #[test]
    fn single_element_range_intersects() {
        assert!(intersects_range(3..4, 0..10));
    }

    // ── CursorPosition ─────────────────────────────────────

    #[test]
    fn cursor_position_default() {
        let pos = CursorPosition::default();
        assert_eq!(pos.x, 0);
        assert_eq!(pos.y, 0);
    }

    #[test]
    fn cursor_position_eq() {
        let a = CursorPosition::default();
        let b = CursorPosition::default();
        assert_eq!(a, b);
    }

    #[test]
    fn cursor_position_clone_copy() {
        let a = CursorPosition {
            x: 5,
            y: 10,
            ..Default::default()
        };
        let b = a;
        assert_eq!(a.x, b.x);
        assert_eq!(a.y, b.y);
    }

    // ── Position enum ───────────────────────────────────────

    #[test]
    fn position_absolute_debug() {
        let p = Position::Absolute(42);
        let debug = format!("{p:?}");
        assert!(debug.contains("Absolute"));
        assert!(debug.contains("42"));
    }

    #[test]
    fn position_relative_debug() {
        let p = Position::Relative(-3);
        let debug = format!("{p:?}");
        assert!(debug.contains("Relative"));
        assert!(debug.contains("-3"));
    }

    // ── SemanticZone ────────────────────────────────────────

    #[test]
    fn semantic_zone_eq() {
        let a = SemanticZone {
            start_y: 0,
            start_x: 0,
            end_y: 10,
            end_x: 80,
            semantic_type: SemanticType::default(),
        };
        let b = a;
        assert_eq!(a, b);
    }

    #[test]
    fn semantic_zone_ord() {
        let a = SemanticZone {
            start_y: 0,
            start_x: 0,
            end_y: 5,
            end_x: 10,
            semantic_type: SemanticType::default(),
        };
        let b = SemanticZone {
            start_y: 1,
            start_x: 0,
            end_y: 5,
            end_x: 10,
            semantic_type: SemanticType::default(),
        };
        assert!(a < b);
    }

    // ── escape sequence constants ───────────────────────────

    #[test]
    fn csi_constant() {
        assert_eq!(CSI, "\x1b[");
    }

    #[test]
    fn osc_constant() {
        assert_eq!(OSC, "\x1b]");
    }

    #[test]
    fn st_constant() {
        assert_eq!(ST, "\x1b\\");
    }

    #[test]
    fn ss3_constant() {
        assert_eq!(SS3, "\x1bO");
    }

    #[test]
    fn dcs_constant() {
        assert_eq!(DCS, "\x1bP");
    }
}
