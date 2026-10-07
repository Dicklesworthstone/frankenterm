//! Multi-pane window frames (ft-yccm0.4.6), the platform-independent half.
//!
//! A window frame draws every visible pane of a window in one command
//! buffer and one render pass. Each [`PaneScene`] has its own cell and
//! glyph grids, uniforms and scissor rectangle. Its GPU buffers and the
//! rows they hold follow the pane's key from frame to frame, so a pane
//! whose rows did not change uploads nothing (ft-yccm0.4.2.4).
//! [`SolidRect`]s fill split borders.

use crate::cell_bg::{BackgroundUniforms, CellBgGrid};
use crate::cell_text::{CellTextGrid, TextUniforms};
use crate::{ClearColor, FrameError};

/// A rectangle of drawable pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PixelRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl PixelRect {
    #[must_use]
    pub fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// The part of the rectangle inside a `width x height` drawable;
    /// `None` when nothing of it is.
    #[must_use]
    pub fn within(&self, width: u32, height: u32) -> Option<Self> {
        let right = self.x.saturating_add(self.width).min(width);
        let bottom = self.y.saturating_add(self.height).min(height);
        (self.x < right && self.y < bottom).then(|| Self {
            x: self.x,
            y: self.y,
            width: right - self.x,
            height: bottom - self.y,
        })
    }

    /// Whether pixel `(x, y)` lies inside.
    #[must_use]
    pub fn contains(&self, x: u32, y: u32) -> bool {
        x >= self.x
            && y >= self.y
            && x < self.x.saturating_add(self.width)
            && y < self.y.saturating_add(self.height)
    }
}

/// One pane of a window frame.
pub struct PaneScene<'a> {
    /// Names the pane from frame to frame: its GPU buffers, and the rows
    /// they hold, follow the key.
    pub key: u64,
    /// Where the pane is drawn, in drawable pixels. Nothing it draws, not
    /// even a glyph's overhang, leaves this rectangle.
    pub rect: PixelRect,
    pub cells: &'a CellBgGrid,
    /// Its cell geometry, ring offset, cursor and tints. `grid_origin` is
    /// in drawable pixels: normally the rectangle's top-left corner plus
    /// padding.
    pub background: BackgroundUniforms,
    /// Its glyph instances, which must belong to `cells`.
    pub text: Option<(&'a CellTextGrid, TextUniforms)>,
    /// The pane's default background color, premultiplied.
    pub clear: ClearColor,
    /// Inactive-pane dimming: multipliers of hue, saturation and brightness.
    pub hsb: Option<[f32; 3]>,
}

/// A rectangle filled with one color: a split border, for example.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SolidRect {
    pub rect: PixelRect,
    pub color: ClearColor,
}

/// Everything a window frame draws.
pub struct WindowFrame<'a> {
    /// What shows where no pane or fill draws: padding and gaps.
    pub clear: ClearColor,
    pub panes: &'a [PaneScene<'a>],
    pub fills: &'a [SolidRect],
}

impl WindowFrame<'_> {
    /// Every pane's glyph grid belongs to its cell grid
    /// ([`FrameError::TextGridMismatch`]) and no two panes share a key
    /// ([`FrameError::DuplicatePane`]).
    pub fn check(&self) -> Result<(), FrameError> {
        for (index, pane) in self.panes.iter().enumerate() {
            if let Some((text, _)) = pane.text {
                let (cells, glyphs) = (pane.cells.extent(), text.extent());
                if cells != glyphs || pane.cells.row_offset() != text.row_offset() {
                    return Err(FrameError::TextGridMismatch {
                        cells,
                        text: glyphs,
                        cells_row_offset: pane.cells.row_offset(),
                        text_row_offset: text.row_offset(),
                    });
                }
            }
            if self.panes[..index]
                .iter()
                .any(|other| other.key == pane.key)
            {
                return Err(FrameError::DuplicatePane { key: pane.key });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GridExtent;

    #[test]
    fn a_rect_is_clipped_to_the_drawable() {
        let rect = PixelRect::new(10, 20, 100, 50);
        assert_eq!(rect.within(1000, 1000), Some(rect));
        assert_eq!(rect.within(60, 40), Some(PixelRect::new(10, 20, 50, 20)));
        assert_eq!(rect.within(10, 1000), None);
        assert!(rect.contains(10, 20) && rect.contains(109, 69));
        assert!(!rect.contains(110, 20) && !rect.contains(10, 70));
    }

    #[test]
    fn a_frame_rejects_duplicate_keys_and_foreign_glyph_grids() {
        let cells = CellBgGrid::new(GridExtent::new(2, 3));
        let other = CellTextGrid::new(GridExtent::new(2, 4));
        let pane = |key, text| PaneScene {
            key,
            rect: PixelRect::new(0, 0, 10, 10),
            cells: &cells,
            background: BackgroundUniforms::default(),
            text,
            clear: ClearColor::from_srgba(0.0, 0.0, 0.0, 1.0),
            hsb: None,
        };
        let panes = [pane(1, None), pane(2, None)];
        let frame = |panes| WindowFrame {
            clear: ClearColor::from_srgba(0.0, 0.0, 0.0, 1.0),
            panes,
            fills: &[],
        };
        assert_eq!(frame(&panes).check(), Ok(()));
        let duplicate = [pane(1, None), pane(1, None)];
        assert_eq!(
            frame(&duplicate).check(),
            Err(FrameError::DuplicatePane { key: 1 })
        );
        let foreign = [pane(3, Some((&other, TextUniforms::default())))];
        assert!(matches!(
            frame(&foreign).check(),
            Err(FrameError::TextGridMismatch { .. })
        ));
    }
}
