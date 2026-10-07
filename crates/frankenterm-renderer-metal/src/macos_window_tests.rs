//! Native tests of multi-pane window frames (ft-yccm0.4.6): exact readback
//! of the real Metal pipeline, offscreen.
//!
//! The reference for a window frame is each pane rendered alone through
//! the single-pane path ([`MetalRenderer::snapshot_frame`], an independent
//! code path), copied into its rectangle over the window's clear color, with
//! the fills painted on top. Pane rectangles are exactly their grids, so a
//! single-pane render covers them completely.

use crate::{
    AtlasKind, AtlasSlot, BackgroundUniforms, CellBg, CellBgGrid, CellText, CellTextGrid,
    ClearColor, FrameScene, GridExtent, MetalRenderer, PaneScene, PixelRect, SolidRect,
    TextUniforms, WindowFrame, apply_hsb,
};

const CELL: [u32; 2] = [8, 16];
const TEXT: TextUniforms = TextUniforms {
    underline_position: 13.0,
    line_thickness: 1.0,
    strikethrough_position: 8.0,
};

fn renderer() -> MetalRenderer {
    MetalRenderer::offscreen().expect("this test needs an admitted Metal device")
}

fn clear() -> ClearColor {
    ClearColor::from_srgba(16.0 / 255.0, 20.0 / 255.0, 24.0 / 255.0, 1.0)
}

fn pane_clear() -> ClearColor {
    ClearColor::from_srgba(30.0 / 255.0, 30.0 / 255.0, 40.0 / 255.0, 1.0)
}

/// A 6x10 grayscale glyph with every coverage level its shape needs.
fn glyph(renderer: &MetalRenderer) -> AtlasSlot {
    let pixels: Vec<u8> = (0..60_u8).map(|i| i.wrapping_mul(41)).collect();
    renderer
        .insert_glyph(AtlasKind::Grayscale, 6, 10, &pixels)
        .expect("a glyph in the atlas")
}

struct Content {
    cells: CellBgGrid,
    text: CellTextGrid,
}

/// Content whose cells and glyphs all depend on `seed`.
fn content(extent: GridExtent, seed: u32, glyph: &AtlasSlot) -> Content {
    let mut cells = CellBgGrid::new(extent);
    let mut text = CellTextGrid::new(extent);
    for row in 0..extent.rows {
        for col in 0..extent.cols {
            if !(row + col + seed).is_multiple_of(3) {
                let byte = |n: u32| u8::try_from(n % 256).unwrap();
                cells.set(
                    row,
                    col,
                    CellBg::rgb(byte(seed * 53 + 40), byte(row * 31), byte(col * 17)),
                );
            }
            if (row * 7 + col + seed).is_multiple_of(4) {
                let col16 = u16::try_from(col).unwrap();
                text.push(
                    row,
                    CellText::new(col16, [240, 220, 120, 255]).with_glyph(glyph, [1, 2]),
                );
            }
        }
    }
    Content { cells, text }
}

/// The grid of `extent` cells at `(x, y)`.
fn rect_for(x: u32, y: u32, extent: GridExtent) -> PixelRect {
    PixelRect::new(x, y, extent.cols * CELL[0], extent.rows * CELL[1])
}

// Pixel coordinates are small; f32 holds them exactly.
#[allow(clippy::cast_precision_loss)]
fn background(rect: PixelRect) -> BackgroundUniforms {
    BackgroundUniforms {
        cell_size: [CELL[0] as f32, CELL[1] as f32],
        grid_origin: [rect.x as f32, rect.y as f32],
        ..BackgroundUniforms::default()
    }
}

fn pane(key: u64, rect: PixelRect, content: &Content, hsb: Option<[f32; 3]>) -> PaneScene<'_> {
    PaneScene {
        key,
        rect,
        cells: &content.cells,
        background: background(rect),
        text: Some((&content.text, TEXT)),
        clear: pane_clear(),
        hsb,
    }
}

/// The window frame as single-pane renders composited at their rectangles
/// over the clear color, with the fills painted on top. `shift` moves the
/// copied rectangle of pane `shift.0` right by `shift.1` pixels: a planted
/// placement error.
fn composite(
    renderer: &MetalRenderer,
    width: u32,
    height: u32,
    panes: &[(PixelRect, &Content)],
    fills: &[SolidRect],
    shift: (usize, u32),
) -> Vec<u8> {
    let mut out: Vec<u8> = (0..width * height)
        .flat_map(|_| clear().to_bgra8())
        .collect();
    for (index, (rect, content)) in panes.iter().enumerate() {
        let alone = renderer
            .snapshot_frame(
                width,
                height,
                &FrameScene {
                    cells: &content.cells,
                    background: background(*rect),
                    text: &content.text,
                    text_uniforms: TEXT,
                    clear: pane_clear(),
                },
            )
            .expect("a single-pane render");
        let dx = if shift.0 == index { shift.1 } else { 0 };
        for y in rect.y..rect.y + rect.height {
            for x in rect.x..rect.x + rect.width {
                let to = ((y * width + x) * 4) as usize;
                let from_x = (x + dx).min(width - 1);
                let from = ((y * width + from_x) * 4) as usize;
                out[to..to + 4].copy_from_slice(&alone[from..from + 4]);
            }
        }
    }
    for fill in fills {
        for y in fill.rect.y..fill.rect.y + fill.rect.height {
            for x in fill.rect.x..fill.rect.x + fill.rect.width {
                let to = ((y * width + x) * 4) as usize;
                out[to..to + 4].copy_from_slice(&fill.color.to_bgra8());
            }
        }
    }
    out
}

fn first_difference(a: &[u8], b: &[u8], width: u32) -> Option<(u32, u32, [u8; 4], [u8; 4])> {
    a.chunks(4)
        .zip(b.chunks(4))
        .enumerate()
        .find(|(_, (a, b))| a != b)
        .map(|(index, (a, b))| {
            let index = u32::try_from(index).unwrap();
            (
                index % width,
                index / width,
                a.try_into().unwrap(),
                b.try_into().unwrap(),
            )
        })
}

/// A split layout: `columns x rows` panes of `extent` cells, `gap` pixels
/// apart, with a split-color fill in every gap.
fn layout(
    extent: GridExtent,
    columns: u32,
    rows: u32,
    gap: u32,
) -> (u32, u32, Vec<PixelRect>, Vec<SolidRect>) {
    let (pane_w, pane_h) = (extent.cols * CELL[0], extent.rows * CELL[1]);
    let width = columns * pane_w + (columns - 1) * gap;
    let height = rows * pane_h + (rows - 1) * gap;
    let split = ClearColor::from_srgba(200.0 / 255.0, 60.0 / 255.0, 60.0 / 255.0, 1.0);
    let mut rects = Vec::new();
    for row in 0..rows {
        for column in 0..columns {
            rects.push(rect_for(
                column * (pane_w + gap),
                row * (pane_h + gap),
                extent,
            ));
        }
    }
    let mut fills = Vec::new();
    for column in 1..columns {
        let x = column * (pane_w + gap) - gap;
        fills.push(SolidRect {
            rect: PixelRect::new(x, 0, gap, height),
            color: split,
        });
    }
    for row in 1..rows {
        let y = row * (pane_h + gap) - gap;
        fills.push(SolidRect {
            rect: PixelRect::new(0, y, width, gap),
            color: split,
        });
    }
    (width, height, rects, fills)
}

fn check_split(columns: u32, rows: u32) {
    let renderer = renderer();
    let glyph = glyph(&renderer);
    let extent = GridExtent::new(4, 10);
    let (width, height, rects, fills) = layout(extent, columns, rows, 4);
    let contents: Vec<Content> = (0..rects.len())
        .map(|seed| content(extent, u32::try_from(seed).unwrap(), &glyph))
        .collect();
    let panes: Vec<PaneScene<'_>> = rects
        .iter()
        .zip(&contents)
        .enumerate()
        .map(|(key, (rect, content))| pane(u64::try_from(key).unwrap(), *rect, content, None))
        .collect();
    let frame = WindowFrame {
        clear: clear(),
        panes: &panes,
        fills: &fills,
    };
    let drawn = renderer
        .snapshot_window(width, height, &frame)
        .expect("a window frame");
    eprintln!(
        "[window] {columns}x{rows} split, {width}x{height} px, {:?} submission",
        renderer.submission_path()
    );
    let placed: Vec<(PixelRect, &Content)> = rects.iter().copied().zip(&contents).collect();
    let expected = composite(&renderer, width, height, &placed, &fills, (0, 0));
    if let Some((x, y, got, want)) = first_difference(&drawn, &expected, width) {
        panic!("{columns}x{rows} split differs at ({x}, {y}): drew {got:?}, single panes {want:?}");
    }
    // Planted negative: one pane copied a pixel to the side must differ.
    let shifted = composite(
        &renderer,
        width,
        height,
        &placed,
        &fills,
        (rects.len() - 1, 1),
    );
    assert!(
        first_difference(&drawn, &shifted, width).is_some(),
        "a pane one pixel off its rectangle went unnoticed"
    );
}

#[test]
fn a_two_pane_split_equals_its_single_pane_renders_composited() {
    check_split(2, 1);
}

#[test]
fn a_four_pane_split_equals_its_single_pane_renders_composited() {
    check_split(2, 2);
}

/// Each pane uploads only its own changed rows: an unchanged window frame
/// uploads nothing once every slot holds it, and a change in one row of
/// one pane uploads that row once per slot and nothing of the other pane.
#[test]
fn a_change_in_one_pane_uploads_only_that_panes_changed_row() {
    let renderer = renderer();
    let glyph = glyph(&renderer);
    let extent = GridExtent::new(4, 10);
    let (width, height, rects, fills) = layout(extent, 2, 1, 4);
    let first = content(extent, 0, &glyph);
    let mut second = content(extent, 1, &glyph);
    let draw = |second: &Content| {
        let panes = [
            pane(1, rects[0], &first, None),
            pane(2, rects[1], second, None),
        ];
        let frame = WindowFrame {
            clear: clear(),
            panes: &panes,
            fills: &fills,
        };
        renderer
            .snapshot_window(width, height, &frame)
            .expect("a window frame");
        renderer.frame_stats().upload_bytes_last
    };
    let slots = crate::FRAME_SLOTS;
    for _ in 0..slots {
        assert!(draw(&second) > 0, "every slot first uploads both panes");
    }
    for _ in 0..slots {
        assert_eq!(
            draw(&second),
            0,
            "an unchanged window frame uploads nothing"
        );
    }
    second.cells.set(2, 3, CellBg::rgb(1, 2, 3));
    let row_bytes = u64::from(extent.cols) * 4;
    for slot in 0..slots {
        assert_eq!(
            draw(&second),
            row_bytes,
            "slot {slot} uploads the one changed CellBg row of the second pane"
        );
    }
    assert_eq!(draw(&second), 0);
}

/// An inactive pane is dimmed like the WebGpu renderer dims it: every
/// pixel is `apply_hsb` of the undimmed one, within 8-bit rounding.
#[test]
fn a_dimmed_pane_is_its_undimmed_pixels_through_apply_hsb() {
    let renderer = renderer();
    let glyph = glyph(&renderer);
    let extent = GridExtent::new(4, 10);
    let rect = rect_for(0, 0, extent);
    let content = content(extent, 2, &glyph);
    let hsb = [1.0, 0.8, 0.7];
    let render = |hsb| {
        let panes = [pane(1, rect, &content, hsb)];
        renderer
            .snapshot_window(
                rect.width,
                rect.height,
                &WindowFrame {
                    clear: clear(),
                    panes: &panes,
                    fills: &[],
                },
            )
            .expect("a window frame")
    };
    let plain = render(None);
    let dimmed = render(Some(hsb));
    assert_ne!(plain, dimmed, "dimming changed nothing");
    for (index, (plain, dimmed)) in plain.chunks(4).zip(dimmed.chunks(4)).enumerate() {
        // Dimming applies to linear light (ft-yccm0.4.7.3).
        let rgba = crate::color::from_srgb_bgra8([plain[0], plain[1], plain[2], plain[3]]);
        let expected = crate::cell_bg::to_bgra8(apply_hsb(rgba, hsb));
        for channel in 0..4 {
            let delta = i16::from(dimmed[channel]) - i16::from(expected[channel]);
            assert!(
                delta.abs() <= 2,
                "pixel {index}: dimmed {dimmed:?}, apply_hsb {expected:?}"
            );
        }
    }
}

/// ft-yccm0.4.7.1: fills are drawn over the panes' text, as the WebGpu
/// renderer draws splits and borders over glyphs, and blend over it. An
/// opaque fill across glyph cells shows only its color; a translucent one is
/// its color over the pane's own pixels (linear light, within 2 codes).
#[test]
fn fills_are_drawn_over_the_panes_text_and_blend_over_it() {
    let renderer = renderer();
    let glyph = glyph(&renderer);
    let extent = GridExtent::new(4, 10);
    let rect = rect_for(0, 0, extent);
    let content = content(extent, 0, &glyph);
    let panes = [pane(1, rect, &content, None)];
    let render = |fills: &[SolidRect]| {
        renderer
            .snapshot_window(
                rect.width,
                rect.height,
                &WindowFrame {
                    clear: clear(),
                    panes: &panes,
                    fills,
                },
            )
            .expect("a window frame")
    };
    let plain = render(&[]);
    // Rows 0 and 1; the content puts a glyph in every fourth cell.
    let band = PixelRect::new(0, 0, rect.width, 2 * CELL[1]);
    let pixels = |image: &[u8]| -> Vec<[u8; 4]> {
        (band.y..band.y + band.height)
            .flat_map(|y| (band.x..band.x + band.width).map(move |x| (x, y)))
            .map(|(x, y)| {
                let at = ((y * rect.width + x) * 4) as usize;
                [image[at], image[at + 1], image[at + 2], image[at + 3]]
            })
            .collect()
    };
    let opaque = ClearColor::from_srgba(200.0 / 255.0, 60.0 / 255.0, 60.0 / 255.0, 1.0);
    // Planted negative: the band holds glyph pixels to cover; without its
    // text the pane draws it differently.
    let under = pixels(&plain);
    let untexted = [PaneScene {
        text: None,
        ..pane(1, rect, &content, None)
    }];
    let backgrounds_only = renderer
        .snapshot_window(
            rect.width,
            rect.height,
            &WindowFrame {
                clear: clear(),
                panes: &untexted,
                fills: &[],
            },
        )
        .expect("a window frame");
    assert_ne!(
        under,
        pixels(&backgrounds_only),
        "the band has no glyphs to cover"
    );
    let covered = render(&[SolidRect {
        rect: band,
        color: opaque,
    }]);
    assert!(
        pixels(&covered)
            .iter()
            .all(|pixel| *pixel == opaque.to_bgra8()),
        "a glyph shows through an opaque fill"
    );
    let rows = (band.height * rect.width * 4) as usize;
    assert_eq!(
        covered[rows..],
        plain[rows..],
        "the fill drew outside its rect"
    );

    let translucent = ClearColor::from_srgba(40.0 / 255.0, 120.0 / 255.0, 220.0 / 255.0, 0.5);
    let blended = render(&[SolidRect {
        rect: band,
        color: translucent,
    }]);
    let top = crate::color::linear_premultiplied(translucent.to_f32());
    for (index, (under, got)) in under.iter().zip(pixels(&blended)).enumerate() {
        let expected = crate::color::to_srgb_bgra8(crate::cell_bg::over(
            top,
            crate::color::from_srgb_bgra8(*under),
        ));
        for channel in 0..4 {
            let delta = i16::from(got[channel]) - i16::from(expected[channel]);
            assert!(
                delta.abs() <= 2,
                "pixel {index}: drew {got:?}, the fill over {under:?} is {expected:?}"
            );
        }
    }
}
