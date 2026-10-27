//! Projection of coherent native terminal state into the browser protocol.
//!
//! Callers authorize the authenticated host and exact native registration
//! before capture. This module does not grant access, open a listener, or
//! interpret ANSI text. Unrepresentable terminal features fail explicitly.

use std::collections::HashMap;
use std::io::{self, Write};

use codec::{PaneIdentityV1, PaneRenderMetadataV1};
use frankenterm_cell::{Blink, CellAttributes, Intensity, VerticalAlign};
use frankenterm_term::color::{ColorAttribute, SrgbaTuple};
use frankenterm_terminal_wire::{
    self as wire, BoundedVec, Cell, CellStyle, Color, Cursor, CursorShape, Dimensions,
    FrozenSnapshot, Hyperlink, Id, Line, PaneIdentity, Scope, TerminalModes, Text,
    Underline, WireI64, WireU64,
};
use termwiz::surface::{CursorShape as NativeCursorShape, CursorVisibility};

/// Content-free outcomes for the authenticated gateway boundary.
#[derive(Debug, thiserror::Error)]
pub enum TerminalProjectionError {
    #[error("native terminal backend unavailable")]
    BackendUnavailable,
    #[error("native terminal circuit open")]
    CircuitOpen,
    #[error("native terminal pool unavailable")]
    Pool(#[from] crate::pool::PoolError),
    #[error("native terminal request failed")]
    Native(#[from] crate::vendored::DirectMuxError),
    #[error("native peer cannot provide exact pane identity")]
    UnsupportedPeer,
    #[error("native pane identity unavailable")]
    IdentityUnavailable,
    #[error("native pane cannot provide coherent render metadata")]
    UnsupportedPane,
    #[error("native pane identity changed")]
    IdentityChanged,
    #[error("native terminal changed during the bounded snapshot transaction")]
    SnapshotChanged,
    #[error("native terminal snapshot is inconsistent")]
    InvalidSource,
    #[error("native terminal snapshot exceeds browser limits")]
    LimitExceeded,
    #[error("native terminal feature is not representable: {0}")]
    Unsupported(&'static str),
    #[error("terminal wire projection rejected")]
    Wire(#[from] wire::WireError),
}

/// Canonical mapping from mux-owned identity. The supplied host must be the
/// enrolled identity of the authenticated route, never a hostname or path.
pub fn scope_for_identity(
    host: Id,
    identity: &PaneIdentityV1,
    epoch: WireU64,
) -> Result<Scope, TerminalProjectionError> {
    identity.validate().map_err(|_| TerminalProjectionError::InvalidSource)?;
    let session = uuid::Uuid::from_bytes(identity.session_incarnation.as_bytes());
    let registration = uuid::Uuid::from_bytes(identity.pane_registration.as_bytes());
    Ok(Scope {
        identity: PaneIdentity {
            host,
            pane: Id::new(&uuid::Uuid::from_bytes(identity.durable_pane_id).to_string())?,
            incarnation: Id::new(&format!("{}.{}", session.simple(), registration.simple()))?,
        },
        epoch,
    })
}

pub(crate) fn projected_dimensions(
    metadata: &PaneRenderMetadataV1,
) -> Result<Dimensions, TerminalProjectionError> {
    metadata.validate().map_err(|_| TerminalProjectionError::InvalidSource)?;
    let native = metadata.dimensions;
    let dimensions = Dimensions {
        columns: u16::try_from(native.cols).map_err(|_| TerminalProjectionError::LimitExceeded)?,
        rows: u16::try_from(native.viewport_rows).map_err(|_| TerminalProjectionError::LimitExceeded)?,
        pixel_width: u32::try_from(native.pixel_width).map_err(|_| TerminalProjectionError::LimitExceeded)?,
        pixel_height: u32::try_from(native.pixel_height).map_err(|_| TerminalProjectionError::LimitExceeded)?,
    };
    dimensions.validate()?;
    Ok(dimensions)
}

fn projected_cursor(
    metadata: &PaneRenderMetadataV1,
    dimensions: &Dimensions,
) -> Result<Cursor, TerminalProjectionError> {
    let native = metadata.cursor;
    let row = native.y.checked_sub(metadata.dimensions.physical_top)
        .and_then(|row| u16::try_from(row).ok())
        .filter(|row| *row < dimensions.rows)
        .ok_or(TerminalProjectionError::InvalidSource)?;
    // Pending wrap is stored one column beyond the grid; native paint keeps
    // it at the final cell until the following printable input.
    let column = u16::try_from(native.x.min(metadata.dimensions.cols - 1))
        .map_err(|_| TerminalProjectionError::InvalidSource)?;
    let (shape, blink) = match native.shape {
        NativeCursorShape::Default | NativeCursorShape::SteadyBlock => (CursorShape::Block, false),
        NativeCursorShape::BlinkingBlock => (CursorShape::Block, true),
        NativeCursorShape::SteadyUnderline => (CursorShape::Underline, false),
        NativeCursorShape::BlinkingUnderline => (CursorShape::Underline, true),
        NativeCursorShape::SteadyBar => (CursorShape::Bar, false),
        NativeCursorShape::BlinkingBar => (CursorShape::Bar, true),
    };
    Ok(Cursor { column, row, shape, blink, visible: native.visibility == CursorVisibility::Visible })
}

fn projected_color(color: SrgbaTuple) -> Result<Color, TerminalProjectionError> {
    if ![color.0, color.1, color.2, color.3].iter().all(|channel| channel.is_finite())
        || color.3 != 1.0
    {
        return Err(TerminalProjectionError::Unsupported("nonopaque_color"));
    }
    let (r, g, b, _) = color.as_rgba_u8();
    Ok(Color::Rgb([r, g, b]))
}

fn projected_style(
    attrs: &CellAttributes,
    metadata: &PaneRenderMetadataV1,
) -> Result<CellStyle, TerminalProjectionError> {
    if attrs.vertical_align() != VerticalAlign::BaseLine {
        return Err(TerminalProjectionError::Unsupported("vertical_alignment"));
    }
    if attrs.blink() == Blink::Rapid {
        return Err(TerminalProjectionError::Unsupported("rapid_blink"));
    }
    if attrs.has_image_attachments() {
        return Err(TerminalProjectionError::Unsupported("image_placement"));
    }
    let foreground = metadata.palette.resolve_fg(attrs.foreground());
    let underline_color = match attrs.underline_color() {
        ColorAttribute::Default => foreground,
        color => metadata.palette.resolve_fg(color),
    };
    let mut attributes = match attrs.intensity() {
        Intensity::Normal => 0,
        Intensity::Bold => wire::STYLE_BOLD,
        Intensity::Half => wire::STYLE_DIM,
    };
    for (enabled, flag) in [
        (attrs.italic(), wire::STYLE_ITALIC),
        (attrs.blink() == Blink::Slow, wire::STYLE_BLINK),
        (attrs.reverse() ^ metadata.dimensions.reverse_video, wire::STYLE_REVERSE),
        (attrs.invisible(), wire::STYLE_INVISIBLE),
        (attrs.strikethrough(), wire::STYLE_STRIKETHROUGH),
        (attrs.overline(), wire::STYLE_OVERLINE),
    ] {
        if enabled { attributes |= flag; }
    }
    let underline = match attrs.underline() {
        frankenterm_cell::Underline::None => Underline::None,
        frankenterm_cell::Underline::Single => Underline::Single,
        frankenterm_cell::Underline::Double => Underline::Double,
        frankenterm_cell::Underline::Curly => Underline::Curly,
        frankenterm_cell::Underline::Dotted => Underline::Dotted,
        frankenterm_cell::Underline::Dashed => Underline::Dashed,
    };
    Ok(CellStyle {
        foreground: projected_color(foreground)?,
        background: projected_color(metadata.palette.resolve_bg(attrs.background()))?,
        underline_color: projected_color(underline_color)?,
        underline,
        attributes,
    })
}

struct SizeBudget(usize);

impl Write for SizeBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.checked_sub(bytes.len())
            .ok_or_else(|| io::Error::other("terminal_projection_limit"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

/// Project and release native chunks progressively. The complete native
/// screen is never retained alongside the complete browser image.
pub(crate) struct TerminalSnapshotBuilder {
    snapshot: FrozenSnapshot,
    metadata: PaneRenderMetadataV1,
    hyperlinks: HashMap<String, WireU64>,
    budget: SizeBudget,
}

impl TerminalSnapshotBuilder {
    pub(crate) fn new(
        scope: Scope,
        snapshot_id: WireU64,
        metadata: &PaneRenderMetadataV1,
        title: String,
        cwd: Option<String>,
    ) -> Result<Self, TerminalProjectionError> {
        let dimensions = projected_dimensions(metadata)?;
        let cursor = projected_cursor(metadata, &dimensions)?;
        let sequence = WireU64(u64::try_from(metadata.seqno)
            .map_err(|_| TerminalProjectionError::InvalidSource)?);
        let snapshot = FrozenSnapshot {
            scope, snapshot_id, sequence, dimensions, cursor,
            modes: TerminalModes {
                alternate_screen: metadata.alt_screen_active,
                mouse_tracking: metadata.mouse_grabbed,
                bracketed_paste: metadata.bracketed_paste,
            },
            title: Text::new(&title)?,
            cwd: cwd.as_deref().map(Text::new).transpose()?,
            lines: BoundedVec::new(), hyperlinks: BoundedVec::new(), images: BoundedVec::new(),
        };
        Ok(Self {
            snapshot, metadata: metadata.clone(), hyperlinks: HashMap::new(),
            budget: SizeBudget(wire::MAX_SNAPSHOT_BYTES as usize),
        })
    }

    fn hyperlink(&mut self, attrs: &CellAttributes) -> Result<Option<WireU64>, TerminalProjectionError> {
        let Some(link) = attrs.hyperlink() else { return Ok(None); };
        let uri = link.uri();
        if let Some(id) = self.hyperlinks.get(uri) { return Ok(Some(*id)); }
        if self.hyperlinks.len() == wire::MAX_SNAPSHOT_RESOURCES {
            return Err(TerminalProjectionError::LimitExceeded);
        }
        let id = WireU64(u64::try_from(self.hyperlinks.len() + 1)
            .map_err(|_| TerminalProjectionError::LimitExceeded)?);
        let resource = Hyperlink { id, uri: Text::new(uri)? };
        serde_json::to_writer(&mut self.budget, &resource)
            .map_err(|_| TerminalProjectionError::LimitExceeded)?;
        // Charge the dictionary's owned key too. Native hyperlink parameters
        // are not retained; only the bounded URI exists in the browser schema.
        self.budget.write_all(uri.as_bytes())
            .map_err(|_| TerminalProjectionError::LimitExceeded)?;
        self.snapshot.hyperlinks.try_push(resource)?;
        self.hyperlinks.insert(uri.to_string(), id);
        Ok(Some(id))
    }

    pub(crate) fn push_rows(
        &mut self,
        rows: Vec<(isize, frankenterm_term::Line)>,
    ) -> Result<(), TerminalProjectionError> {
        let columns = usize::from(self.snapshot.dimensions.columns);
        if rows.len() > usize::from(self.snapshot.dimensions.rows).saturating_sub(self.snapshot.lines.len()) {
            return Err(TerminalProjectionError::InvalidSource);
        }
        for (stable_row, native) in rows {
            let row = self.snapshot.lines.len();
            if self.metadata.dimensions.physical_top.checked_add(row as isize) != Some(stable_row) {
                return Err(TerminalProjectionError::InvalidSource);
            }
            if !native.is_single_width() {
                return Err(TerminalProjectionError::Unsupported("double_width_or_height"));
            }
            let (bidi_enabled, direction) = native.bidi_info();
            if bidi_enabled || direction != Default::default() {
                return Err(TerminalProjectionError::Unsupported("bidi_layout"));
            }
            if native.len() > columns { return Err(TerminalProjectionError::InvalidSource); }
            let mut cells = BoundedVec::new();
            for cell in native.visible_cells() {
                if cell.cell_index() != cells.len() || !matches!(cell.width(), 1 | 2)
                    || cell.cell_index().checked_add(cell.width()).is_none_or(|end| end > columns)
                {
                    return Err(TerminalProjectionError::InvalidSource);
                }
                let style = projected_style(cell.attrs(), &self.metadata)?;
                let hyperlink = self.hyperlink(cell.attrs())?;
                let column = u16::try_from(cells.len()).map_err(|_| TerminalProjectionError::InvalidSource)?;
                cells.try_push(Cell {
                    column, text: Text::new(cell.str())?, width: cell.width() as u8,
                    style: style.clone(), hyperlink, image: None,
                })?;
                if cell.width() == 2 {
                    cells.try_push(Cell {
                        column: column + 1, text: Text::new("")?, width: 0,
                        style, hyperlink, image: None,
                    })?;
                }
            }
            let blank = projected_style(&CellAttributes::default(), &self.metadata)?;
            while cells.len() < columns {
                cells.try_push(Cell {
                    column: cells.len() as u16, text: Text::new(" ")?, width: 1,
                    style: blank.clone(), hyperlink: None, image: None,
                })?;
            }
            let line = Line {
                row: row as u16,
                stable_row: WireI64(i64::try_from(stable_row).map_err(|_| TerminalProjectionError::InvalidSource)?),
                cells,
            };
            serde_json::to_writer(&mut self.budget, &line)
                .map_err(|_| TerminalProjectionError::LimitExceeded)?;
            self.snapshot.lines.try_push(line)?;
        }
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<FrozenSnapshot, TerminalProjectionError> {
        if self.snapshot.lines.len() != usize::from(self.snapshot.dimensions.rows) {
            return Err(TerminalProjectionError::InvalidSource);
        }
        Ok(self.snapshot)
    }
}

/// Project an owned native image. The caller supplies coherence and view
/// authorization; the direct client's live transaction provides native fences.
pub fn project_terminal_snapshot(
    scope: Scope,
    snapshot_id: WireU64,
    metadata: &PaneRenderMetadataV1,
    title: String,
    cwd: Option<String>,
    rows: Vec<(isize, frankenterm_term::Line)>,
) -> Result<FrozenSnapshot, TerminalProjectionError> {
    let mut builder = TerminalSnapshotBuilder::new(scope, snapshot_id, metadata, title, cwd)?;
    builder.push_rows(rows)?;
    builder.finish()
}
