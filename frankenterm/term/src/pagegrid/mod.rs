//! PageGrid: the packed page grid for the terminal model (ft-yccm0.3.3).
//! The design and its invariants are in
//! `docs/proposals/mac-render-pagegrid-adr.md`.
//!
//! This module holds the page (B3.2): 8-byte packed cells with palette
//! styles inline, row headers, and the per-page rich-style table, grapheme
//! arena, hyperlink table and image map. The page list (B3.3) pools,
//! recycles and seals pages. The views (B3.4, ahead of B3.5) turn a row
//! into a legacy `Line` and back. The engine switch (B3.4) builds on all
//! three.

pub mod cell;
pub mod grapheme;
mod hash;
pub mod links;
pub mod list;
#[cfg(test)]
mod list_tests;
pub mod native;
pub mod page;
pub mod row;
pub mod rows;
pub mod style;
#[cfg(test)]
mod tests;
pub mod view;

pub use cell::{CellStyle, Glyph, InlineStyle, PackedCell, StyleClass};
pub use list::{OverCap, PageList, RangeFloor, RowRef, Scrolled, MAX_POOLED_PAGES, SEAL_DISTANCE};
pub use page::{rows_per_page, CellWrite, Page, StyleSpec, STD_PAGE_CELLS};
pub use row::RowHeader;
pub use style::{CachedRichId, RichStyle, RichStyleTable};
