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
    TextUniforms, UiQuad, WindowFrame, apply_hsb,
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
        ui: None,
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
            ui: None,
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
                    ui: None,
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
                    ui: None,
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
                ui: None,
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

/// A chrome layer of `quads` over a 4x4 atlas.
fn chrome_layer<'a>(
    quads: &'a [UiQuad],
    version: u64,
    rgba: &'a [u8],
) -> crate::ui_quads::UiLayer<'a> {
    crate::ui_quads::UiLayer {
        atlas: crate::ui_quads::UiAtlas {
            width: 4,
            height: 4,
            version,
            rgba,
        },
        quads,
        under: 0,
        foreground_text_hsb: [1.0; 3],
    }
}

/// ft-yccm0.4.7.1: window chrome quads are drawn as WebGpu's quad shader
/// shades them (`shade_ui_quad`, its CPU reference), blended with straight
/// alpha over the frame in quad order, within 2 codes. The atlas is
/// uploaded only when its version changes.
#[test]
// Atlas texel indices and pixel coordinates are small and non-negative.
// One scenario: each quad kind, quad order, a planted negative and the
// atlas version rule share the frame they are checked on.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::too_many_lines
)]
fn chrome_quads_draw_as_the_webgpu_quad_shader_shades_them() {
    use crate::color::{byte_to_linear, from_srgb_bgra8, to_srgb_bgra8};
    use crate::ui_quads::{UiLayer, blend_ui, kind, shade_ui_quad};

    let renderer = renderer();
    let glyph = glyph(&renderer);
    let extent = GridExtent::new(4, 10);
    let rect = rect_for(0, 0, extent);
    let content = content(extent, 1, &glyph);
    let panes = [pane(1, rect, &content, None)];
    // A 4x4 atlas of uniform 2x2 blocks, so nearest and linear sampling
    // inside a block's middle both read its color exactly: a color texel, a
    // half coverage, an opaque gray and transparency.
    let atlas = |color: [u8; 4]| -> Vec<u8> {
        (0..4u32)
            .flat_map(|y| (0..4u32).map(move |x| (x / 2, y / 2)))
            .flat_map(|block| match block {
                (0, 0) => color,
                (1, 0) => [255, 255, 255, 128],
                (0, 1) => [90, 90, 90, 255],
                _ => [0, 0, 0, 0],
            })
            .collect()
    };
    let block_uv = |x: f32, y: f32| {
        [
            (x * 2.0 + 0.5) / 4.0,
            (y * 2.0 + 0.5) / 4.0,
            (x * 2.0 + 1.5) / 4.0,
            (y * 2.0 + 1.5) / 4.0,
        ]
    };
    let quad = |rect: [f32; 4], uv, kind| UiQuad {
        rect,
        uv,
        fg: [0.7, 0.3, 0.1, 0.8],
        alt: [0.7, 0.3, 0.1, 0.8],
        mix: 0.0,
        hsv: [1.0; 3],
        kind,
    };
    let quads = [
        quad(
            [0.0, 0.0, 16.0, 16.0],
            block_uv(0.0, 0.0),
            kind::SOLID_COLOR,
        ),
        quad([16.0, 0.0, 32.0, 16.0], block_uv(1.0, 0.0), kind::GLYPH),
        quad(
            [32.0, 0.0, 48.0, 16.0],
            block_uv(0.0, 0.0),
            kind::COLOR_EMOJI,
        ),
        quad(
            [48.0, 0.0, 64.0, 16.0],
            block_uv(1.0, 0.0),
            kind::GRAY_SCALE,
        ),
        quad([0.0, 16.0, 16.0, 32.0], block_uv(0.0, 1.0), kind::BG_IMAGE),
        // Over the first: the later quad covers the earlier one.
        UiQuad {
            fg: [0.1, 0.2, 0.9, 0.5],
            ..quad(
                [8.0, 8.0, 24.0, 24.0],
                block_uv(1.0, 1.0),
                kind::SOLID_COLOR,
            )
        },
    ];
    let render = |ui: Option<UiLayer<'_>>| {
        renderer
            .snapshot_window(
                rect.width,
                rect.height,
                &WindowFrame {
                    clear: clear(),
                    panes: &panes,
                    fills: &[],
                    ui,
                },
            )
            .expect("a window frame")
    };
    let layer = chrome_layer;
    let plain = render(None);
    let first = atlas([200, 120, 40, 255]);
    let drawn = render(Some(layer(&quads, 1, &first)));

    // The reference, quad by quad over each pixel's center.
    let expected = |quads: &[UiQuad], rgba: &[u8]| -> Vec<u8> {
        let texel = |uv: [f32; 4]| {
            let x = ((uv[0] * 4.0) as usize).min(3);
            let y = ((uv[1] * 4.0) as usize).min(3);
            let at = (y * 4 + x) * 4;
            [
                byte_to_linear(rgba[at]),
                byte_to_linear(rgba[at + 1]),
                byte_to_linear(rgba[at + 2]),
                f32::from(rgba[at + 3]) / 255.0,
            ]
        };
        let mut out = plain.clone();
        for (index, pixel) in out.chunks_mut(4).enumerate() {
            let index = u32::try_from(index).unwrap();
            let center = [
                (index % rect.width) as f32 + 0.5,
                (index / rect.width) as f32 + 0.5,
            ];
            let mut color = from_srgb_bgra8([pixel[0], pixel[1], pixel[2], pixel[3]]);
            for quad in quads {
                let [left, top, right, bottom] = quad.rect;
                if (left..right).contains(&center[0]) && (top..bottom).contains(&center[1]) {
                    let sample = texel(quad.uv);
                    color = blend_ui(shade_ui_quad(quad, sample, sample, [1.0; 3]), color);
                }
            }
            pixel.copy_from_slice(&to_srgb_bgra8(color));
        }
        out
    };
    let within = |got: &[u8], want: &[u8]| {
        got.iter()
            .zip(want)
            .all(|(got, want)| (i16::from(*got) - i16::from(*want)).abs() <= 2)
    };
    let reference = expected(&quads, &first);
    if let Some((index, (got, want))) = drawn
        .chunks(4)
        .zip(reference.chunks(4))
        .enumerate()
        .find(|(_, (got, want))| !within(got, want))
    {
        panic!("pixel {index}: drew {got:?}, the WebGpu quad shader {want:?}");
    }
    assert_ne!(drawn, plain, "the chrome drew nothing");
    // Planted negative: shading the glyph quad as a grayscale poly (alpha
    // scaled by coverage instead of replaced) is told apart.
    let mut wrong = quads;
    wrong[1].kind = kind::GRAY_SCALE;
    assert!(
        !within(&drawn, &expected(&wrong, &first)),
        "a glyph shaded as a gray poly went unnoticed"
    );

    // Same version, other bytes: not uploaded again.
    let second = atlas([20, 200, 220, 255]);
    assert_eq!(
        render(Some(layer(&quads, 1, &second))),
        drawn,
        "an unchanged version re-uploaded the atlas"
    );
    // A new version is uploaded and drawn.
    let redrawn = render(Some(layer(&quads, 2, &second)));
    assert!(within(&redrawn, &expected(&quads, &second)));
    assert_ne!(redrawn, drawn);
}

/// ft-yccm0.4.7.1: window background layers (`UiLayer::under`) are drawn
/// under the panes, whose backgrounds blend over them, as the WebGpu
/// renderer draws background images: a pane with a transparent background
/// over a layer reads back as the same pane with that layer's color as its
/// background, within 2 codes.
#[test]
// Pixel sizes are small.
#[allow(clippy::cast_precision_loss)]
fn background_layers_draw_under_the_panes() {
    use crate::color::srgb_to_linear;
    use crate::ui_quads::kind;

    let renderer = renderer();
    let glyph = glyph(&renderer);
    let extent = GridExtent::new(4, 10);
    let rect = rect_for(0, 0, extent);
    let content = content(extent, 3, &glyph);
    let (red, green, blue) = (0.8, 0.1, 0.1);
    let backdrop_color = ClearColor::from_srgba(red, green, blue, 1.0);
    let transparent = ClearColor::from_srgba(0.0, 0.0, 0.0, 0.0);
    let backdrop = [UiQuad {
        rect: [0.0, 0.0, rect.width as f32, rect.height as f32],
        uv: [0.0, 0.0, 1.0, 1.0],
        fg: [
            srgb_to_linear(red),
            srgb_to_linear(green),
            srgb_to_linear(blue),
            1.0,
        ],
        alt: [0.0; 4],
        mix: 0.0,
        hsv: [1.0; 3],
        kind: kind::SOLID_COLOR,
    }];
    let atlas = [0u8; 64];
    let render = |pane_clear: ClearColor, ui| {
        let panes = [PaneScene {
            clear: pane_clear,
            ..pane(1, rect, &content, None)
        }];
        renderer
            .snapshot_window(
                rect.width,
                rect.height,
                &WindowFrame {
                    clear: pane_clear,
                    panes: &panes,
                    fills: &[],
                    ui,
                },
            )
            .expect("a window frame")
    };
    let reference = render(backdrop_color, None);
    let drawn = render(
        transparent,
        Some(crate::ui_quads::UiLayer {
            under: 1,
            ..chrome_layer(&backdrop, 1, &atlas)
        }),
    );
    if let Some((index, (got, want))) =
        drawn
            .chunks(4)
            .zip(reference.chunks(4))
            .enumerate()
            .find(|(_, (got, want))| {
                got.iter()
                    .zip(*want)
                    .any(|(got, want)| (i16::from(*got) - i16::from(*want)).abs() > 2)
            })
    {
        panic!("pixel {index}: drew {got:?} over the layer, {want:?} as the pane's background");
    }
    // Planted negative: the same layer drawn over the panes covers their
    // cells and glyphs.
    let over = render(transparent, Some(chrome_layer(&backdrop, 1, &atlas)));
    assert_ne!(over, reference, "a layer over the panes went unnoticed");
}
