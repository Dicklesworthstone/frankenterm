//! The render mirror (ft-yccm0.4.4): a render-side copy of a pane's visible
//! rows as compact per-cell data, refreshed under one short terminal lock by
//! copying only the rows that changed since the previous capture.
//!
//! A [`MirrorCell`] holds what drawing the cell needs, unresolved:
//! - its text (a range of its row's text) and width;
//! - its foreground, background and underline colors as the cell names them
//!   (default, palette index, or 8-bit RGBA, which is lossless for 8-bit
//!   output because a true color always wins over its palette fallback);
//! - packed attribute flags and a hyperlink id.
//!
//! Style and color resolution, selection, hyperlink hover and GPU instance
//! building all happen from the mirror after the lock is released. No
//! visible [`Line`] is cloned and no row is compared with a previous copy of
//! itself: change detection is by line sequence number.
//!
//! A row is recaptured when:
//! - its line's sequence number moved past the one it was captured at, or is
//!   `SEQ_ZERO`, which always counts as changed;
//! - it holds images, whose payloads change behind shared handles;
//! - it enters the viewport;
//! - it belongs to a logical line whose implicit hyperlinks were rescanned
//!   (a rescan can change a clean continuation row without a new seqno).
//!
//! Every row is recaptured when the screen's coordinate witness changes
//! (resize, reflow, scrollback erase, the other screen becoming active), the
//! alternate screen flips, the grid size or hyperlink rules change, or the
//! sequence number runs backwards. Ordinary output and scrolling keep the
//! witness, so a flood recaptures only the rows it writes.

use crate::pane::Pane;
use crate::renderable::{
    terminal_get_cursor_position, terminal_get_dimensions, RenderableDimensions,
    StableCursorPosition,
};
use frankenterm_term::screen::ScreenCoordinateWitness;
use frankenterm_term::{Line, StableRowIndex, Terminal};
use std::convert::TryFrom;
use std::ops::Range;
use std::sync::Arc;
use termwiz::cell::{Blink, CellAttributes, Intensity, Underline, VerticalAlign};
use termwiz::color::{ColorAttribute, SrgbaTuple};
use termwiz::hyperlink::{Hyperlink, Rule};
use termwiz::surface::{SequenceNo, SEQ_ZERO};

/// A cell color as the cell names it, before palette resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MirrorColor {
    /// The palette's default foreground or background.
    Default,
    Palette(u8),
    /// sRGB, straight alpha, 8 bits per channel.
    Rgba([u8; 4]),
}

/// A color component as the 8-bit value an 8-bit target stores.
// Clamped to 0.0..=255.0 before the cast.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn unorm8(component: f32) -> u8 {
    (component.clamp(0.0, 1.0) * 255.0).round() as u8
}

impl MirrorColor {
    fn from_attribute(color: ColorAttribute) -> Self {
        match color {
            ColorAttribute::Default => Self::Default,
            ColorAttribute::PaletteIndex(index) => Self::Palette(index),
            ColorAttribute::TrueColorWithPaletteFallback(color, _)
            | ColorAttribute::TrueColorWithDefaultFallback(color) => {
                let SrgbaTuple(red, green, blue, alpha) = color;
                Self::Rgba([unorm8(red), unorm8(green), unorm8(blue), unorm8(alpha)])
            }
        }
    }

    /// The color as a [`ColorAttribute`] a palette resolves. Resolving it
    /// gives the cell's color to 8-bit precision.
    pub fn to_attribute(self) -> ColorAttribute {
        match self {
            Self::Default => ColorAttribute::Default,
            Self::Palette(index) => ColorAttribute::PaletteIndex(index),
            Self::Rgba([red, green, blue, alpha]) => {
                ColorAttribute::TrueColorWithDefaultFallback(SrgbaTuple(
                    f32::from(red) / 255.0,
                    f32::from(green) / 255.0,
                    f32::from(blue) / 255.0,
                    f32::from(alpha) / 255.0,
                ))
            }
        }
    }
}

// MirrorCell::flags layout.
const INTENSITY_SHIFT: u16 = 0;
const UNDERLINE_SHIFT: u16 = 2;
const BLINK_SHIFT: u16 = 5;
const ITALIC: u16 = 1 << 7;
const REVERSE: u16 = 1 << 8;
const STRIKETHROUGH: u16 = 1 << 9;
const INVISIBLE: u16 = 1 << 10;
const OVERLINE: u16 = 1 << 11;
const VALIGN_SHIFT: u16 = 12;
const IMAGE: u16 = 1 << 14;

/// One visible cell (a wide character is one cell of width 2; its spacer is
/// not stored), in compact form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MirrorCell {
    /// The cell's text: `text_len` bytes of its row's text from `text_start`.
    text_start: u32,
    text_len: u8,
    col: u16,
    width: u8,
    flags: u16,
    /// 0 for none, else one past an index into the row's hyperlinks.
    link: u16,
    fg: MirrorColor,
    bg: MirrorColor,
    underline_color: MirrorColor,
}

impl MirrorCell {
    /// The cell's first column.
    pub fn col(&self) -> usize {
        usize::from(self.col)
    }

    pub fn width(&self) -> usize {
        usize::from(self.width)
    }

    pub fn fg(&self) -> MirrorColor {
        self.fg
    }

    pub fn bg(&self) -> MirrorColor {
        self.bg
    }

    pub fn underline_color(&self) -> MirrorColor {
        self.underline_color
    }

    pub fn intensity(&self) -> Intensity {
        match (self.flags >> INTENSITY_SHIFT) & 0b11 {
            1 => Intensity::Bold,
            2 => Intensity::Half,
            _ => Intensity::Normal,
        }
    }

    pub fn underline(&self) -> Underline {
        match (self.flags >> UNDERLINE_SHIFT) & 0b111 {
            1 => Underline::Single,
            2 => Underline::Double,
            3 => Underline::Curly,
            4 => Underline::Dotted,
            5 => Underline::Dashed,
            _ => Underline::None,
        }
    }

    pub fn blink(&self) -> Blink {
        match (self.flags >> BLINK_SHIFT) & 0b11 {
            1 => Blink::Slow,
            2 => Blink::Rapid,
            _ => Blink::None,
        }
    }

    pub fn italic(&self) -> bool {
        self.flags & ITALIC != 0
    }

    pub fn reverse(&self) -> bool {
        self.flags & REVERSE != 0
    }

    pub fn strikethrough(&self) -> bool {
        self.flags & STRIKETHROUGH != 0
    }

    pub fn invisible(&self) -> bool {
        self.flags & INVISIBLE != 0
    }

    pub fn overline(&self) -> bool {
        self.flags & OVERLINE != 0
    }

    pub fn vertical_align(&self) -> VerticalAlign {
        match (self.flags >> VALIGN_SHIFT) & 0b11 {
            1 => VerticalAlign::SuperScript,
            2 => VerticalAlign::SubScript,
            _ => VerticalAlign::BaseLine,
        }
    }

    /// Whether the cell has image attachments (drawn by an image pass, not
    /// by the cell passes).
    pub fn has_image(&self) -> bool {
        self.flags & IMAGE != 0
    }

    fn flags_of(attrs: &CellAttributes) -> u16 {
        let mut flags = ((attrs.intensity() as u16) << INTENSITY_SHIFT)
            | ((attrs.underline() as u16) << UNDERLINE_SHIFT)
            | ((attrs.blink() as u16) << BLINK_SHIFT)
            | ((attrs.vertical_align() as u16) << VALIGN_SHIFT);
        for (set, bit) in [
            (attrs.italic(), ITALIC),
            (attrs.reverse(), REVERSE),
            (attrs.strikethrough(), STRIKETHROUGH),
            (attrs.invisible(), INVISIBLE),
            (attrs.overline(), OVERLINE),
            (attrs.has_image_attachments(), IMAGE),
        ] {
            if set {
                flags |= bit;
            }
        }
        flags
    }
}

/// One captured row.
#[derive(Debug, Clone, PartialEq)]
pub struct MirrorRow {
    stable: StableRowIndex,
    /// The line's sequence number when it was captured.
    seqno: SequenceNo,
    /// The capture that last copied this row (see [`RenderMirror::generation`]).
    generation: u64,
    cells: Vec<MirrorCell>,
    /// Every cell's text, back to back.
    text: String,
    links: Vec<Arc<Hyperlink>>,
    has_images: bool,
}

impl MirrorRow {
    /// Compacts `line`, the row at `stable`.
    pub fn from_line(stable: StableRowIndex, line: &Line, generation: u64) -> Self {
        let mut row = Self {
            stable,
            seqno: line.current_seqno(),
            generation,
            cells: Vec::with_capacity(line.len()),
            text: String::with_capacity(line.len()),
            links: Vec::new(),
            has_images: false,
        };
        for cell in line.visible_cells() {
            let text = cell.str();
            // A grapheme is at most a few dozen bytes; anything longer is cut
            // at a char boundary rather than wrapping the length.
            let mut len = text.len().min(usize::from(u8::MAX));
            while !text.is_char_boundary(len) {
                len -= 1;
            }
            let text_start = u32::try_from(row.text.len()).unwrap_or(u32::MAX);
            row.text.push_str(&text[..len]);
            let attrs = cell.attrs();
            let link = match attrs.hyperlink() {
                None => 0,
                Some(link) => {
                    let index = row
                        .links
                        .iter()
                        .position(|known| Arc::ptr_eq(known, link) || **known == **link)
                        .unwrap_or_else(|| {
                            row.links.push(Arc::clone(link));
                            row.links.len() - 1
                        });
                    u16::try_from(index + 1).unwrap_or(u16::MAX)
                }
            };
            let flags = MirrorCell::flags_of(attrs);
            row.has_images |= flags & IMAGE != 0;
            row.cells.push(MirrorCell {
                text_start,
                text_len: u8::try_from(len).unwrap_or(u8::MAX),
                col: u16::try_from(cell.cell_index()).unwrap_or(u16::MAX),
                width: u8::try_from(cell.width()).unwrap_or(u8::MAX),
                flags,
                link,
                fg: MirrorColor::from_attribute(attrs.foreground()),
                bg: MirrorColor::from_attribute(attrs.background()),
                underline_color: MirrorColor::from_attribute(attrs.underline_color()),
            });
        }
        row
    }

    pub fn stable(&self) -> StableRowIndex {
        self.stable
    }

    pub fn seqno(&self) -> SequenceNo {
        self.seqno
    }

    /// The capture that last copied this row: a renderer rebuilds the rows
    /// whose generation is newer than the frame it built last.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn cells(&self) -> &[MirrorCell] {
        &self.cells
    }

    /// The cell's text (its grapheme).
    pub fn text(&self, cell: &MirrorCell) -> &str {
        let start = cell.text_start as usize;
        self.text
            .get(start..start + usize::from(cell.text_len))
            .unwrap_or("")
    }

    pub fn hyperlink(&self, cell: &MirrorCell) -> Option<&Arc<Hyperlink>> {
        usize::from(cell.link)
            .checked_sub(1)
            .and_then(|index| self.links.get(index))
    }

    pub fn has_images(&self) -> bool {
        self.has_images
    }

    /// Whether the row must be copied again although its seqno says clean.
    fn always_recapture(&self) -> bool {
        self.has_images
    }
}

/// What a capture copied: the counters prove that a capture does work
/// proportional to the rows that changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureStats {
    /// Viewport rows checked against the mirror.
    pub rows_examined: usize,
    /// Rows copied into the mirror.
    pub rows_captured: usize,
    /// Cells copied into the mirror.
    pub cells_captured: usize,
    /// Every row was copied (first capture, or an invalidation).
    pub full: bool,
}

/// The pane parameters of one capture.
#[derive(Debug, Clone, Copy)]
pub struct CaptureRequest<'a> {
    /// The first row of a scrolled-back viewport; `None` follows the live
    /// screen.
    pub viewport_top: Option<StableRowIndex>,
    /// Implicit hyperlink rules applied to the rows being copied.
    pub rules: &'a [Rule],
    /// Changes whenever `rules` does (the configuration generation).
    pub rules_generation: usize,
}

/// The mirror of a pane's viewport, refreshed by [`capture_terminal_rows`].
#[derive(Debug, Clone, Default)]
pub struct RenderMirror {
    rows: Vec<MirrorRow>,
    first: StableRowIndex,
    dimensions: Option<RenderableDimensions>,
    cursor: StableCursorPosition,
    seqno: SequenceNo,
    alt_screen: bool,
    rules_generation: Option<usize>,
    witness: Option<ScreenCoordinateWitness>,
    generation: u64,
    last: CaptureStats,
}

impl RenderMirror {
    pub fn new() -> Self {
        Self::default()
    }

    /// The viewport's rows, top first.
    pub fn rows(&self) -> &[MirrorRow] {
        &self.rows
    }

    /// The stable index of the viewport's first row.
    pub fn first(&self) -> StableRowIndex {
        self.first
    }

    /// The pane's dimensions at the last capture.
    pub fn dimensions(&self) -> Option<RenderableDimensions> {
        self.dimensions
    }

    /// The cursor at the last capture.
    pub fn cursor(&self) -> StableCursorPosition {
        self.cursor
    }

    /// Hides the cursor of the last capture (a tmux-controlled pane).
    pub fn hide_cursor(&mut self) {
        self.cursor.visibility = termwiz::surface::CursorVisibility::Hidden;
    }

    /// The terminal's sequence number at the last capture.
    pub fn seqno(&self) -> SequenceNo {
        self.seqno
    }

    /// Increments with every capture; rows record the one that copied them.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// What the last capture copied.
    pub fn last_capture(&self) -> CaptureStats {
        self.last
    }

    /// A mirror built from full copies of the viewport's lines, `lines[0]`
    /// being the row at `first`: the Line-based reference that incremental
    /// captures must match, and what a renderer that deep-clones every
    /// visible line each frame works from. Hyperlink rules are not applied.
    pub fn from_lines(
        dimensions: RenderableDimensions,
        cursor: StableCursorPosition,
        first: StableRowIndex,
        lines: &[Line],
    ) -> Self {
        let rows = lines
            .iter()
            .enumerate()
            .map(|(offset, line)| MirrorRow::from_line(first + offset as StableRowIndex, line, 1))
            .collect();
        Self {
            rows,
            first,
            dimensions: Some(dimensions),
            cursor,
            generation: 1,
            ..Self::default()
        }
    }

    /// Forgets every row, so the next capture copies all of them.
    pub fn invalidate(&mut self) {
        self.rows.clear();
        self.witness = None;
    }

    /// The mirror's row at `stable`, if it holds one.
    fn row_index(&self, stable: StableRowIndex) -> Option<usize> {
        let offset = usize::try_from(stable.checked_sub(self.first)?).ok()?;
        (self.rows.get(offset)?.stable == stable).then_some(offset)
    }
}

/// Refreshes `mirror` from `term`, which the caller has locked: copies only
/// the viewport rows that changed since the mirror's last capture. `None`
/// when the viewport reaches into cold (non-resident) scrollback, which this
/// capture never hydrates under the lock; use [`capture_pane_rows`] then.
pub fn capture_terminal_rows(
    term: &mut Terminal,
    mirror: &mut RenderMirror,
    request: &CaptureRequest<'_>,
) -> Option<CaptureStats> {
    capture_rows(term, mirror, request, None)
}

/// [`capture_terminal_rows`], optionally failing to copy one changed row:
/// the planted fault that proves the equality tests can fail.
fn capture_rows(
    term: &mut Terminal,
    mirror: &mut RenderMirror,
    request: &CaptureRequest<'_>,
    planted_skip: Option<StableRowIndex>,
) -> Option<CaptureStats> {
    let dimensions = terminal_get_dimensions(term);
    let seqno = term.current_seqno();
    let alt_screen = term.is_alt_screen_active();
    let witness_holds = mirror
        .witness
        .as_ref()
        .is_some_and(|witness| term.screen().matches_coordinate_witness(witness));
    let full = mirror.rows.is_empty()
        || !witness_holds
        || mirror
            .dimensions
            .map(|dims| (dims.cols, dims.viewport_rows))
            != Some((dimensions.cols, dimensions.viewport_rows))
        || mirror.alt_screen != alt_screen
        || mirror.rules_generation != Some(request.rules_generation)
        || seqno < mirror.seqno;

    let rows = dimensions.viewport_rows;
    let first = request
        .viewport_top
        .unwrap_or(dimensions.physical_top)
        .max(dimensions.scrollback_top)
        .min(dimensions.physical_top);
    let end = first.checked_add(StableRowIndex::try_from(rows).ok()?)?;
    if first < term.screen().phys_to_stable_row_index(0) {
        return None;
    }

    // Which viewport rows need a copy, by seqno, membership and images.
    let mut dirty = vec![full; rows];
    if !full {
        let screen = term.screen();
        let phys = screen.stable_range(&(first..end));
        let phys_first = screen.phys_to_stable_row_index(phys.start);
        screen.with_phys_lines(phys, |lines| {
            for (offset, line) in lines.iter().enumerate() {
                let stable = phys_first + offset as StableRowIndex;
                let Some(row) = usize::try_from(stable - first).ok() else {
                    continue;
                };
                let Some(flag) = dirty.get_mut(row) else {
                    continue;
                };
                *flag = match mirror.row_index(stable) {
                    None => true,
                    Some(index) => {
                        let held = &mirror.rows[index];
                        let current = line.current_seqno();
                        current == SEQ_ZERO || current > held.seqno || held.always_recapture()
                    }
                };
            }
        });
    }

    // Implicit hyperlinks: rescanning a logical line that holds a link
    // rewrites every row of it (with the line's highest seqno), so rescan
    // each logical line touching the viewport that holds a row being copied
    // or a row never scanned, inside the viewport or beyond it (new output
    // wrapped below a scrolled-back viewport), and copy all of its visible
    // rows.
    if !request.rules.is_empty() {
        for range in logical_lines_to_rescan(term.screen(), first..end, &dirty) {
            let mut widened: Vec<Range<StableRowIndex>> = Vec::new();
            term.screen_mut()
                .for_each_logical_line_in_stable_range_mut(range, |logical, lines| {
                    Line::apply_hyperlink_rules(request.rules, lines);
                    widened.push(logical);
                    true
                });
            for logical in widened {
                for stable in logical.start.max(first)..logical.end.min(end) {
                    dirty[(stable - first) as usize] = true;
                }
            }
        }
    }

    // Copy the dirty rows; move the clean ones over from the old mirror.
    let generation = mirror.generation.wrapping_add(1);
    let mut old = std::mem::take(&mut mirror.rows);
    let old_first = mirror.first;
    let mut next: Vec<Option<MirrorRow>> = (0..rows)
        .map(|row| {
            if dirty[row] {
                return None;
            }
            let stable = first + row as StableRowIndex;
            let offset = usize::try_from(stable.checked_sub(old_first)?).ok()?;
            let held = old.get_mut(offset)?;
            (held.stable == stable).then(|| std::mem::replace(held, placeholder_row()))
        })
        .collect();
    let mut stats = CaptureStats {
        rows_examined: rows,
        full,
        ..CaptureStats::default()
    };
    {
        let screen = term.screen();
        let phys = screen.stable_range(&(first..end));
        let phys_first = screen.phys_to_stable_row_index(phys.start);
        screen.with_phys_lines(phys, |lines| {
            for (offset, line) in lines.iter().enumerate() {
                let stable = phys_first + offset as StableRowIndex;
                let Some(row) = usize::try_from(stable - first).ok() else {
                    continue;
                };
                let Some(slot) = next.get_mut(row) else {
                    continue;
                };
                if slot.is_some() {
                    continue;
                }
                if planted_skip == Some(stable) {
                    // The planted fault: keep the stale copy, as a capture
                    // that missed this dirty row would.
                    let stale = usize::try_from(stable - old_first)
                        .ok()
                        .and_then(|offset| old.get_mut(offset))
                        .filter(|held| held.stable == stable)
                        .map(|held| std::mem::replace(held, placeholder_row()));
                    *slot = Some(stale.unwrap_or_else(|| MirrorRow {
                        stable,
                        ..placeholder_row()
                    }));
                    continue;
                }
                let copied = MirrorRow::from_line(stable, line, generation);
                stats.rows_captured += 1;
                stats.cells_captured += copied.cells.len();
                *slot = Some(copied);
            }
        });
    }
    // Rows the screen does not have (none for a resident viewport) stay
    // blank rather than leaving the mirror short.
    mirror.rows = next
        .into_iter()
        .enumerate()
        .map(|(row, slot)| {
            slot.unwrap_or_else(|| MirrorRow {
                stable: first + row as StableRowIndex,
                generation,
                ..placeholder_row()
            })
        })
        .collect();
    mirror.first = first;
    mirror.dimensions = Some(dimensions);
    mirror.cursor = terminal_get_cursor_position(term);
    mirror.seqno = seqno;
    mirror.alt_screen = alt_screen;
    mirror.rules_generation = Some(request.rules_generation);
    mirror.witness = Some(term.screen().capture_coordinate_witness());
    mirror.generation = generation;
    mirror.last = stats;
    Some(stats)
}

/// Rows grouped into one logical line at most, in cells: the cap
/// `Screen::for_each_logical_line_in_stable_range_mut` applies, mirrored so
/// both find the same logical lines.
const MAX_LOGICAL_LINE_LEN: usize = 1024;

/// What deciding a rescan needs from one row.
struct RowFacts {
    wrapped: bool,
    len: usize,
    scanned: bool,
}

/// Rows of a screen read on demand, in small chunks, without mutable
/// access, so clean page-engine rows stay native and a capture reads beyond
/// the viewport only while logical lines continue.
struct RowReader<'a> {
    screen: &'a frankenterm_term::screen::Screen,
    start: usize,
    rows: std::collections::VecDeque<RowFacts>,
    /// The screen has no rows past the last one read.
    at_end: bool,
}

impl<'a> RowReader<'a> {
    const CHUNK: usize = 16;

    fn new(screen: &'a frankenterm_term::screen::Screen, phys: Range<usize>) -> Self {
        let mut reader = Self {
            screen,
            start: phys.start,
            rows: std::collections::VecDeque::new(),
            at_end: false,
        };
        let wanted = phys.len();
        let rows = reader.read(phys);
        reader.at_end = rows.len() < wanted;
        reader.rows.extend(rows);
        reader
    }

    fn read(&self, phys: Range<usize>) -> Vec<RowFacts> {
        let mut rows = Vec::with_capacity(phys.len());
        self.screen.with_phys_lines(phys, |lines| {
            rows.extend(lines.iter().map(|line| RowFacts {
                wrapped: line.last_cell_was_wrapped(),
                len: line.len(),
                scanned: line.implicit_hyperlinks_are_scanned(),
            }));
        });
        rows
    }

    fn get(&mut self, phys: usize) -> Option<&RowFacts> {
        while phys < self.start {
            let from = self.start.saturating_sub(Self::CHUNK);
            let rows = self.read(from..self.start);
            if rows.is_empty() {
                return None;
            }
            for row in rows.into_iter().rev() {
                self.rows.push_front(row);
            }
            self.start = from;
        }
        while phys >= self.start + self.rows.len() {
            if self.at_end {
                return None;
            }
            let from = self.start + self.rows.len();
            let rows = self.read(from..from + Self::CHUNK);
            self.at_end = rows.len() < Self::CHUNK;
            if rows.is_empty() {
                return None;
            }
            self.rows.extend(rows);
        }
        self.rows.get(phys - self.start)
    }
}

/// The stable ranges of the logical lines touching `viewport` that need an
/// implicit-hyperlink rescan: those holding a row marked in `dirty` (indexed
/// from `viewport.start`) or a row never scanned, found the way
/// `Screen::for_each_logical_line_in_stable_range_mut` groups rows.
fn logical_lines_to_rescan(
    screen: &frankenterm_term::screen::Screen,
    viewport: Range<StableRowIndex>,
    dirty: &[bool],
) -> Vec<Range<StableRowIndex>> {
    let phys = screen.stable_range(&viewport);
    if phys.is_empty() {
        return Vec::new();
    }
    let mut reader = RowReader::new(screen, phys.clone());

    // Back to the start of the first logical line, as the screen walks.
    let mut start = phys.start;
    let mut back_len = 0usize;
    while start > 0 {
        let Some(prior) = reader.get(start - 1) else {
            break;
        };
        if !prior.wrapped || logical_len_exceeds(back_len, prior.len) {
            break;
        }
        back_len = back_len.saturating_add(prior.len);
        start -= 1;
    }

    let mut rescan = Vec::new();
    let mut row = start;
    while row < phys.end {
        if reader.get(row).is_none() {
            break;
        }
        let mut total = 0usize;
        let mut end_inclusive = row;
        let mut unscanned = false;
        let mut index = row;
        while let Some(line) = reader.get(index) {
            if total > 0 && logical_len_exceeds(total, line.len) {
                break;
            }
            end_inclusive = index;
            total = total.saturating_add(line.len);
            unscanned |= !line.scanned;
            if !line.wrapped {
                break;
            }
            index += 1;
        }
        let logical = screen.phys_to_stable_row_index(row)
            ..screen.phys_to_stable_row_index(end_inclusive + 1);
        let copied = (logical.start.max(viewport.start)..logical.end.min(viewport.end))
            .any(|stable| dirty.get((stable - viewport.start) as usize) == Some(&true));
        if unscanned || copied {
            rescan.push(logical);
        }
        row = end_inclusive + 1;
    }
    rescan
}

fn logical_len_exceeds(current: usize, additional: usize) -> bool {
    current
        .checked_add(additional)
        .is_none_or(|total| total > MAX_LOGICAL_LINE_LEN)
}

fn placeholder_row() -> MirrorRow {
    MirrorRow {
        stable: 0,
        seqno: SEQ_ZERO,
        generation: 0,
        cells: Vec::new(),
        text: String::new(),
        links: Vec::new(),
        has_images: false,
    }
}

/// [`capture_terminal_rows`] for any pane, through its public row API: for a
/// pane whose terminal this process does not hold (a mux client), and for a
/// local viewport in cold scrollback, whose rows `Pane::get_lines` hydrates.
/// Each call may lock the pane separately, so rows can come from consecutive
/// states, as such panes have always been rendered. Implicit hyperlink rules
/// are applied to the copied rows by logical line within each copied run.
pub fn capture_pane_rows(
    pane: &dyn Pane,
    mirror: &mut RenderMirror,
    request: &CaptureRequest<'_>,
) -> CaptureStats {
    let dimensions = pane.get_dimensions();
    let seqno = pane.get_current_seqno();
    let alt_screen = pane.render_facts().alt_screen_active;
    let full = mirror.rows.is_empty()
        || mirror
            .dimensions
            .map(|dims| (dims.cols, dims.viewport_rows))
            != Some((dimensions.cols, dimensions.viewport_rows))
        || mirror.alt_screen != alt_screen
        || mirror.rules_generation != Some(request.rules_generation)
        || seqno < mirror.seqno;
    let rows = dimensions.viewport_rows;
    let first = request
        .viewport_top
        .unwrap_or(dimensions.physical_top)
        .max(dimensions.scrollback_top)
        .min(dimensions.physical_top);
    let end = first.saturating_add(StableRowIndex::try_from(rows).unwrap_or(0));
    let changed = if full {
        rangeset::RangeSet::new()
    } else {
        pane.get_changed_since(first..end, mirror.seqno)
    };
    let dirty: Vec<bool> = (0..rows)
        .map(|row| {
            let stable = first + row as StableRowIndex;
            full || changed.contains(stable)
                || mirror
                    .row_index(stable)
                    .is_none_or(|index| mirror.rows[index].always_recapture())
        })
        .collect();

    let generation = mirror.generation.wrapping_add(1);
    let mut old = std::mem::take(&mut mirror.rows);
    let old_first = mirror.first;
    let mut next: Vec<Option<MirrorRow>> = (0..rows)
        .map(|row| {
            if dirty[row] {
                return None;
            }
            let stable = first + row as StableRowIndex;
            let offset = usize::try_from(stable.checked_sub(old_first)?).ok()?;
            let held = old.get_mut(offset)?;
            (held.stable == stable).then(|| std::mem::replace(held, placeholder_row()))
        })
        .collect();
    drop(old);
    let mut stats = CaptureStats {
        rows_examined: rows,
        full,
        ..CaptureStats::default()
    };
    let mut row = 0;
    while row < rows {
        if !dirty[row] {
            row += 1;
            continue;
        }
        let run_start = row;
        while row < rows && dirty[row] {
            row += 1;
        }
        let run = first + run_start as StableRowIndex..first + row as StableRowIndex;
        let (lines_first, mut lines) = pane.get_lines(run);
        if !request.rules.is_empty() {
            apply_rules_by_logical_line(request.rules, &mut lines);
        }
        for (offset, line) in lines.iter().enumerate() {
            let stable = lines_first + offset as StableRowIndex;
            let Some(slot) = usize::try_from(stable - first)
                .ok()
                .and_then(|index| next.get_mut(index))
            else {
                continue;
            };
            let copied = MirrorRow::from_line(stable, line, generation);
            stats.rows_captured += 1;
            stats.cells_captured += copied.cells.len();
            *slot = Some(copied);
        }
    }
    mirror.rows = next
        .into_iter()
        .enumerate()
        .map(|(row, slot)| {
            slot.unwrap_or_else(|| MirrorRow {
                stable: first + row as StableRowIndex,
                generation,
                ..placeholder_row()
            })
        })
        .collect();
    mirror.first = first;
    mirror.dimensions = Some(dimensions);
    mirror.cursor = pane.get_cursor_position();
    mirror.seqno = seqno;
    mirror.alt_screen = alt_screen;
    mirror.rules_generation = Some(request.rules_generation);
    mirror.witness = None;
    mirror.generation = generation;
    mirror.last = stats;
    stats
}

/// Applies `rules` to copied rows, one logical (wrapped) line at a time.
fn apply_rules_by_logical_line(rules: &[Rule], lines: &mut [Line]) {
    let mut start = 0;
    while start < lines.len() {
        let mut end = start;
        while end + 1 < lines.len() && lines[end].last_cell_was_wrapped() {
            end += 1;
        }
        let mut logical: Vec<&mut Line> = lines[start..=end].iter_mut().collect();
        Line::apply_hyperlink_rules(rules, &mut logical);
        start = end + 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frankenterm_term::color::ColorPalette;
    use frankenterm_term::{TerminalConfiguration, TerminalSize};
    use termwiz::cell::Cell;

    #[derive(Debug)]
    struct TestConfig;

    impl TerminalConfiguration for TestConfig {
        fn color_palette(&self) -> ColorPalette {
            ColorPalette::default()
        }
    }

    fn size(rows: usize, cols: usize) -> TerminalSize {
        TerminalSize {
            rows,
            cols,
            pixel_width: cols * 8,
            pixel_height: rows * 16,
            dpi: 96,
        }
    }

    fn terminal(rows: usize, cols: usize) -> Terminal {
        Terminal::new(
            size(rows, cols),
            Arc::new(TestConfig),
            "FrankenTerm",
            "render-mirror-test",
            Box::new(Vec::<u8>::new()),
        )
    }

    const LIVE: CaptureRequest<'static> = CaptureRequest {
        viewport_top: None,
        rules: &[],
        rules_generation: 0,
    };

    fn capture(term: &mut Terminal, mirror: &mut RenderMirror) -> CaptureStats {
        capture_terminal_rows(term, mirror, &LIVE).expect("resident viewport")
    }

    /// The Line-based reference: every viewport row copied afresh.
    fn fresh(term: &mut Terminal, request: &CaptureRequest<'_>) -> RenderMirror {
        let mut mirror = RenderMirror::new();
        let stats = capture_terminal_rows(term, &mut mirror, request).expect("resident viewport");
        assert!(stats.full);
        mirror
    }

    type RowContent<'a> = (
        StableRowIndex,
        SequenceNo,
        &'a [MirrorCell],
        &'a str,
        &'a [Arc<Hyperlink>],
        bool,
    );

    /// What a row shows, without the capture bookkeeping (its generation).
    fn content(row: &MirrorRow) -> RowContent<'_> {
        (
            row.stable,
            row.seqno,
            &row.cells,
            &row.text,
            &row.links,
            row.has_images,
        )
    }

    /// The first difference between what two mirrors show, if any.
    fn difference(mirror: &RenderMirror, reference: &RenderMirror) -> Option<String> {
        if (mirror.first, mirror.cursor, mirror.dimensions)
            != (reference.first, reference.cursor, reference.dimensions)
        {
            return Some(format!(
                "viewport {:?} vs {:?}",
                (mirror.first, mirror.cursor, mirror.dimensions),
                (reference.first, reference.cursor, reference.dimensions)
            ));
        }
        if mirror.rows.len() != reference.rows.len() {
            return Some(format!(
                "{} rows vs {}",
                mirror.rows.len(),
                reference.rows.len()
            ));
        }
        mirror
            .rows
            .iter()
            .zip(&reference.rows)
            .find(|(row, expected)| content(row) != content(expected))
            .map(|(row, expected)| format!("row {:?}\n vs {:?}", row, expected))
    }

    /// xorshift64*: deterministic, so a failing seed replays.
    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Self {
            Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
        }

        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound
        }

        fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
            &items[self.below(items.len() as u64) as usize]
        }
    }

    const WORDS: &[&str] = &[
        "alpha",
        "beta",
        "γάμμα",
        "你好",
        "世界",
        "e\u{301}",
        "👍",
        "👩\u{200d}💻",
        "❤\u{fe0f}",
        "🇯🇵",
        "x",
        "  ",
        "\t",
    ];

    const SGR: &[&str] = &[
        "0",
        "1",
        "2",
        "3",
        "4",
        "4:2",
        "4:3",
        "4:4",
        "4:5",
        "5",
        "7",
        "8",
        "9",
        "53",
        "22",
        "23",
        "24",
        "27",
        "38;5;196",
        "48;5;21",
        "38;2;10;200;30",
        "48;2;250;128;7",
        "58;5;46",
        "58;2;1;2;3",
        "39",
        "49",
        "59",
        "73",
        "74",
    ];

    /// Terminal output exercising text (wide, clusters, emoji), styles,
    /// cursor moves, scrolling, scroll regions with insert/delete/scroll,
    /// erases, insert/delete/erase characters, both screens, hyperlinks
    /// explicit and implicit, autowrap and resets.
    fn random_output(rng: &mut Rng, rows: usize, cols: usize) -> Vec<u8> {
        let mut out = String::new();
        let (rows, cols) = (rows as u64, cols as u64);
        match rng.below(18) {
            0..=4 => {
                for _ in 0..1 + rng.below(8) {
                    out.push_str(rng.pick(WORDS));
                    out.push(' ');
                }
            }
            5 | 6 => out.push_str(&format!("\x1b[{}m", rng.pick(SGR))),
            7 => out.push_str(&format!(
                "\x1b[{};{}H",
                1 + rng.below(rows + 2),
                1 + rng.below(cols + 2)
            )),
            8 => {
                for _ in 0..1 + rng.below(4) {
                    out.push_str("\r\n");
                }
            }
            9 => {
                let top = 1 + rng.below(rows);
                let bottom = top + rng.below(rows + 1 - top);
                out.push_str(&format!("\x1b[{top};{bottom}r\x1b[{top};1H"));
                out.push_str(rng.pick(&[
                    "\x1b[2L",
                    "\x1b[M",
                    "\x1b[3S",
                    "\x1b[T",
                    "line\r\n\r\n",
                    "\x1bM\x1bM",
                ]));
                out.push_str("\x1b[r");
            }
            10 => out.push_str(rng.pick(&[
                "\x1b[J", "\x1b[1J", "\x1b[2J", "\x1b[K", "\x1b[1K", "\x1b[2K",
            ])),
            11 => out.push_str(rng.pick(&["\x1b[3@", "\x1b[2P", "\x1b[4X"])),
            12 => out.push_str(rng.pick(&[
                "\x1b[?1049h",
                "\x1b[?1049l",
                "\x1b[?47h",
                "\x1b[?47l",
                "\x1b[?1047h",
                "\x1b[?1047l",
            ])),
            13 => out.push_str("\x1b]8;;https://example.com/a\x1b\\link\x1b]8;;\x1b\\"),
            14 | 15 => {
                out.push_str("see https://example.com/some/long/path/that/wraps/across/the/edge ")
            }
            16 => out.push_str(rng.pick(&["\x1b[3J", "\x1b[?7l", "\x1b[?7h"])),
            _ => {
                if rng.below(4) == 0 {
                    out.push_str("\x1bc");
                }
            }
        }
        out.into_bytes()
    }

    /// Feeds random output in steps, capturing incrementally now and then,
    /// sometimes into a scrolled-back viewport, and after a planted fault
    /// when `plant` is set. Returns how many captures differed from a fresh
    /// full capture.
    fn random_session(seed: u64, rules: &[Rule], plant: bool) -> usize {
        let mut rng = Rng::new(seed);
        let (mut rows, mut cols) = (8, 24);
        let mut term = terminal(rows, cols);
        let mut mirror = RenderMirror::new();
        let mut differences = 0;
        for step in 0..400 {
            term.advance_bytes(random_output(&mut rng, rows, cols));
            if rng.below(40) == 0 {
                rows = 4 + rng.below(9) as usize;
                cols = 10 + rng.below(31) as usize;
                term.resize(size(rows, cols));
            }
            if rng.below(3) != 0 {
                continue;
            }
            let dims = terminal_get_dimensions(&mut term);
            let viewport_top = (rng.below(4) == 0).then(|| {
                let back = dims.physical_top - dims.scrollback_top;
                dims.physical_top - rng.below(back as u64 + 1) as StableRowIndex
            });
            let request = CaptureRequest {
                viewport_top,
                rules,
                rules_generation: 0,
            };
            let planted = plant.then(|| terminal_get_cursor_position(&mut term).y);
            let stats =
                capture_rows(&mut term, &mut mirror, &request, planted).expect("resident viewport");
            assert!(stats.rows_captured <= stats.rows_examined);
            let reference = fresh(&mut term, &request);
            if let Some(difference) = difference(&mirror, &reference) {
                assert!(
                    plant,
                    "seed {} step {}: incremental capture differs from a full one: {}",
                    seed, step, difference
                );
                differences += 1;
                mirror.invalidate();
            }
        }
        differences
    }

    #[test]
    fn incremental_captures_equal_full_captures_under_random_output() {
        let rules = [Rule::new(r"https?://\S+", "$0").expect("rule")];
        for seed in 0..12 {
            let rules: &[Rule] = if seed % 2 == 0 { &[] } else { &rules };
            assert_eq!(random_session(seed, rules, false), 0);
        }
    }

    #[test]
    fn the_randomized_check_catches_a_capture_that_misses_changed_rows() {
        // Planted negative: each capture skips the cursor's row, the row
        // output most often changes; the check must notice.
        let caught: usize = (0..4).map(|seed| random_session(seed, &[], true)).sum();
        assert!(caught > 0, "a missed dirty row went unnoticed");
    }

    #[test]
    fn a_capture_that_misses_a_changed_row_fails_the_equality_check() {
        let mut term = terminal(6, 20);
        term.advance_bytes(b"\x1b[3;1Hbefore");
        let mut mirror = RenderMirror::new();
        capture(&mut term, &mut mirror);
        term.advance_bytes(b"\x1b[3;1Hafter!");
        let changed = mirror.first() + 2;
        let stats = capture_rows(&mut term, &mut mirror, &LIVE, Some(changed)).expect("resident");
        assert_eq!(
            stats.rows_captured, 0,
            "the fault skipped the only changed row"
        );
        let reference = fresh(&mut term, &LIVE);
        assert!(
            difference(&mirror, &reference).is_some(),
            "the check must catch the missed row"
        );
        // An honest capture repairs it: the stale row's seqno is behind.
        assert_eq!(capture(&mut term, &mut mirror).rows_captured, 1);
        assert_eq!(difference(&mirror, &fresh(&mut term, &LIVE)), None);
    }

    #[test]
    fn a_capture_copies_only_the_rows_that_changed() {
        let mut term = terminal(10, 40);
        term.advance_bytes(b"one\r\ntwo\r\nthree");
        let mut mirror = RenderMirror::new();
        let stats = capture(&mut term, &mut mirror);
        assert!(stats.full);
        assert_eq!(stats.rows_captured, 10);

        let stats = capture(&mut term, &mut mirror);
        assert_eq!(
            (stats.full, stats.rows_captured, stats.cells_captured),
            (false, 0, 0)
        );

        term.advance_bytes(b"\x1b[6;3Hx");
        let stats = capture(&mut term, &mut mirror);
        assert_eq!((stats.full, stats.rows_captured), (false, 1));
        let row = &mirror.rows()[5];
        assert_eq!(row.text(&row.cells()[2]), "x");

        term.advance_bytes(b"\x1b[2;1Hab\x1b[9;1Hcd");
        assert_eq!(capture(&mut term, &mut mirror).rows_captured, 2);

        // A scroll at the bottom: the written row and the new one. The rows
        // that only moved keep their stable index and are not copied.
        term.advance_bytes(b"\x1b[10;1Hlast\r\n");
        let stats = capture(&mut term, &mut mirror);
        assert_eq!((stats.full, stats.rows_captured), (false, 2));

        term.advance_bytes(b"\x1b[?1049h");
        let stats = capture(&mut term, &mut mirror);
        assert_eq!((stats.full, stats.rows_captured), (true, 10));
        assert_eq!(difference(&mirror, &fresh(&mut term, &LIVE)), None);
    }

    #[test]
    fn scrolling_the_viewport_copies_only_the_rows_it_exposes() {
        let mut term = terminal(5, 20);
        for row in 0..20 {
            term.advance_bytes(format!("row {row}\r\n"));
        }
        let top = terminal_get_dimensions(&mut term).physical_top;
        let at = |viewport_top| CaptureRequest {
            viewport_top: Some(viewport_top),
            ..LIVE
        };
        let mut mirror = RenderMirror::new();
        capture_terminal_rows(&mut term, &mut mirror, &at(top - 2)).expect("resident");
        let stats = capture_terminal_rows(&mut term, &mut mirror, &at(top - 3)).expect("resident");
        assert_eq!((stats.full, stats.rows_captured), (false, 1));
        assert_eq!(difference(&mirror, &fresh(&mut term, &at(top - 3))), None);
        let stats = capture_terminal_rows(&mut term, &mut mirror, &LIVE).expect("resident");
        assert_eq!((stats.full, stats.rows_captured), (false, 3));
        assert_eq!(difference(&mirror, &fresh(&mut term, &LIVE)), None);
    }

    #[test]
    fn switching_screens_recaptures_rows_whose_seqnos_did_not_move() {
        let mut term = terminal(4, 20);
        term.advance_bytes(b"primary");
        let mut mirror = RenderMirror::new();
        capture(&mut term, &mut mirror);
        // 47 switches without clearing: the alternate rows keep old seqnos.
        for switch in [
            &b"\x1b[?47h"[..],
            b"\x1b[?47l",
            b"\x1b[?1047h",
            b"\x1b[?1047l",
        ] {
            term.advance_bytes(switch);
            let stats = capture(&mut term, &mut mirror);
            assert!(stats.full);
            assert_eq!(difference(&mirror, &fresh(&mut term, &LIVE)), None);
        }
    }

    #[test]
    fn a_rescan_of_a_wrapped_link_recaptures_its_clean_continuation_row() {
        let rules = [Rule::new(r"https?://\S+", "$0").expect("rule")];
        let request = CaptureRequest {
            rules: &rules,
            ..LIVE
        };
        let mut term = terminal(4, 20);
        term.advance_bytes(b"https://example.com/abcdefghijklmnop");
        let mut mirror = RenderMirror::new();
        capture_terminal_rows(&mut term, &mut mirror, &request).expect("resident");
        // Change only the first row of the wrapped URL.
        term.advance_bytes(b"\x1b[1;1Hhttp ");
        let stats = capture_terminal_rows(&mut term, &mut mirror, &request).expect("resident");
        assert_eq!(stats.rows_captured, 2, "the continuation row is copied too");
        assert_eq!(difference(&mirror, &fresh(&mut term, &request)), None);
    }

    /// bv13 (seed 5, step 210), minimized: a wrapped logical line runs from
    /// the last row of a scrolled-back viewport into the row below it. Output
    /// there completes a URL across both rows; the rescan rewrites the
    /// visible row too, so the capture must copy it although nothing wrote to
    /// it.
    #[test]
    fn a_link_completed_below_a_scrolled_back_viewport_recaptures_the_visible_row() {
        let rules = [Rule::new(r"https?://\S+", "$0").expect("rule")];
        let mut term = terminal(4, 10);
        // Row 3 is "see https:" and wraps into row 4, "zzzz": no link yet.
        term.advance_bytes(b"a\r\nb\r\nc\r\nsee https:zzzz\r\n\r\n\r\n");
        let top = terminal_get_dimensions(&mut term).physical_top;
        assert_eq!(top, 4, "rows 4 to 7 are on screen");
        let request = CaptureRequest {
            viewport_top: Some(0),
            rules: &rules,
            rules_generation: 0,
        };
        let links_on_row_3 = |mirror: &RenderMirror| {
            let row = &mirror.rows()[3];
            assert_eq!(row.stable(), 3);
            row.cells()
                .iter()
                .filter(|cell| row.hyperlink(cell).is_some())
                .count()
        };
        let mut mirror = RenderMirror::new();
        capture_terminal_rows(&mut term, &mut mirror, &request).expect("resident");
        assert_eq!(links_on_row_3(&mirror), 0);

        // Row 4, the screen's first row and below the viewport, becomes
        // "//ab.cd": the logical line reads "see https://ab.cd".
        term.advance_bytes(b"\x1b[1;1H//ab.cd");
        let stats = capture_terminal_rows(&mut term, &mut mirror, &request).expect("resident");
        assert!(!stats.full);
        assert_eq!(
            stats.rows_captured, 1,
            "only the visible row of the rescanned line"
        );
        assert_eq!(difference(&mirror, &fresh(&mut term, &request)), None);
        assert_eq!(links_on_row_3(&mirror), "https:".len());
    }

    #[test]
    fn compaction_keeps_every_attribute_the_renderer_draws() {
        let palette = ColorPalette::default();
        let colors = [
            ColorAttribute::Default,
            ColorAttribute::PaletteIndex(3),
            ColorAttribute::PaletteIndex(200),
            ColorAttribute::TrueColorWithDefaultFallback(SrgbaTuple(0.1, 0.5, 0.9, 1.0)),
            ColorAttribute::TrueColorWithPaletteFallback(SrgbaTuple(1.0, 0.0, 0.25, 0.5), 9),
        ];
        let link = Arc::new(Hyperlink::new("https://example.com"));
        let eight =
            |SrgbaTuple(r, g, b, a): SrgbaTuple| [unorm8(r), unorm8(g), unorm8(b), unorm8(a)];
        let mut rng = Rng::new(7);
        for _ in 0..64 {
            let mut line = Line::new(5);
            let mut col = 0;
            while col < 40 {
                let mut attrs = CellAttributes::default();
                attrs
                    .set_intensity(*rng.pick(&[
                        Intensity::Normal,
                        Intensity::Bold,
                        Intensity::Half,
                    ]))
                    .set_underline(*rng.pick(&[
                        Underline::None,
                        Underline::Single,
                        Underline::Double,
                        Underline::Curly,
                        Underline::Dotted,
                        Underline::Dashed,
                    ]))
                    .set_blink(*rng.pick(&[Blink::None, Blink::Slow, Blink::Rapid]))
                    .set_italic(rng.below(2) == 1)
                    .set_reverse(rng.below(2) == 1)
                    .set_strikethrough(rng.below(2) == 1)
                    .set_invisible(rng.below(2) == 1)
                    .set_overline(rng.below(2) == 1)
                    .set_vertical_align(*rng.pick(&[
                        VerticalAlign::BaseLine,
                        VerticalAlign::SuperScript,
                        VerticalAlign::SubScript,
                    ]))
                    .set_foreground(*rng.pick(&colors))
                    .set_background(*rng.pick(&colors))
                    .set_underline_color(*rng.pick(&colors));
                if rng.below(3) == 0 {
                    attrs.set_hyperlink(Some(Arc::clone(&link)));
                }
                let text = *rng.pick(&["a", "Z", "你", "e\u{301}", "👩\u{200d}💻", "❤\u{fe0f}"]);
                let cell = Cell::new_grapheme(text, attrs, None);
                let width = cell.width().max(1);
                line.set_cell(col, cell, 5);
                col += width;
            }
            let row = MirrorRow::from_line(0, &line, 1);
            let originals: Vec<_> = line.visible_cells().collect();
            assert_eq!(row.cells().len(), originals.len());
            for (cell, original) in row.cells().iter().zip(originals) {
                let attrs = original.attrs();
                assert_eq!(row.text(cell), original.str());
                assert_eq!(
                    (cell.col(), cell.width()),
                    (original.cell_index(), original.width())
                );
                assert_eq!(cell.intensity(), attrs.intensity());
                assert_eq!(cell.underline(), attrs.underline());
                assert_eq!(cell.blink(), attrs.blink());
                assert_eq!(cell.vertical_align(), attrs.vertical_align());
                assert_eq!(
                    [
                        cell.italic(),
                        cell.reverse(),
                        cell.strikethrough(),
                        cell.invisible(),
                        cell.overline()
                    ],
                    [
                        attrs.italic(),
                        attrs.reverse(),
                        attrs.strikethrough(),
                        attrs.invisible(),
                        attrs.overline()
                    ]
                );
                assert_eq!(
                    eight(palette.resolve_fg(cell.fg().to_attribute())),
                    eight(palette.resolve_fg(attrs.foreground()))
                );
                assert_eq!(
                    eight(palette.resolve_bg(cell.bg().to_attribute())),
                    eight(palette.resolve_bg(attrs.background()))
                );
                assert_eq!(
                    eight(palette.resolve_fg(cell.underline_color().to_attribute())),
                    eight(palette.resolve_fg(attrs.underline_color()))
                );
                assert_eq!(row.hyperlink(cell), attrs.hyperlink());
                assert!(!cell.has_image());
            }
        }
    }
}
