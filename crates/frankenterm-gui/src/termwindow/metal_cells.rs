//! The live Metal frame (ft-yccm0.4.4).
//!
//! Each Metal paint captures every visible pane's changed rows into its
//! render mirror under one short hold of its terminal lock
//! (`LocalPane::capture_render_rows`; the generic pane path for mux clients
//! and cold scrollback). It then rebuilds the frame's cell backgrounds and
//! glyph instances outside the lock, only for the rows that need it
//! ([`frankenterm_gui::metal_scene`]). Colors and the cursor are resolved the
//! way the WebGpu renderer resolves them. The palette comes from the pane's
//! published render facts, so it is read without the lock. The image-parity
//! snapshot (ft-yccm0.1.10) draws the same scene offscreen.
//!
//! What a frame needs from its window is captured on the main thread as
//! [`MetalFrameInputs`], one per visible pane (`metal_window`, ft-yccm0.4.6),
//! so the frame itself can be built on the window's render thread
//! (ft-yccm0.4.1.2) with that thread's own fonts.

use crate::colorease::ColorEase;
use crate::selection::SelectionRange;
use crate::termwindow::metal_glyphs::{FallbackReady, FontGlyphs, MAX_SHAPE_PASSES};
use crate::utilsprites::RenderMetrics;
use config::ConfigHandle;
use frankenterm_font::FontConfiguration;
use frankenterm_gui::metal_scene::{
    BlinkLevels, Compose, CursorSprite, MetalScene, SceneStyle, cursor_attr_colors, mix_linear,
    reverse_video_cursor_applies,
};
use frankenterm_renderer_metal::{
    BackgroundUniforms, CursorShape as MetalCursorShape, CursorUniform, MetalRenderer, TextUniforms,
};
use mux::localpane::LocalPane;
use mux::pane::{Pane, PaneId};
use mux::render_mirror::{CaptureRequest, RenderMirror, capture_pane_rows};
use mux::renderable::StableCursorPosition;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;
use termwiz::hyperlink::Hyperlink;
use termwiz::surface::{CursorShape, CursorVisibility};
use wezterm_term::color::{ColorPalette, SrgbaTuple};
use wezterm_term::{StableRowIndex, TerminalConfiguration};

/// Straight-alpha sRGB to the premultiplied RGBA the shader composites.
fn premultiplied(color: SrgbaTuple) -> [f32; 4] {
    let SrgbaTuple(red, green, blue, alpha) = color;
    [red * alpha, green * alpha, blue * alpha, alpha]
}

/// The metal cursor shape for a terminal cursor shape: an unfocused window
/// (or inactive pane) draws a hollow block, as the WebGpu renderer does.
fn cursor_shape(shape: CursorShape, focused_and_active: bool) -> MetalCursorShape {
    if !focused_and_active {
        return MetalCursorShape::HollowBlock;
    }
    match shape {
        CursorShape::BlinkingUnderline | CursorShape::SteadyUnderline => {
            MetalCursorShape::Underline
        }
        CursorShape::BlinkingBar | CursorShape::SteadyBar => MetalCursorShape::Bar,
        CursorShape::Default | CursorShape::BlinkingBlock | CursorShape::SteadyBlock => {
            MetalCursorShape::Block
        }
    }
}

/// The sprite the scene draws for a cursor (ft-yccm0.4.7.3): the WebGpu
/// renderer's own cursor sprites, anti-aliased, for every shape but a
/// focused block, which the background pass fills.
fn cursor_sprite(shape: MetalCursorShape) -> Option<CursorSprite> {
    match shape {
        MetalCursorShape::HollowBlock => Some(CursorSprite::HollowBlock),
        MetalCursorShape::Bar => Some(CursorSprite::Bar),
        MetalCursorShape::Underline => Some(CursorSprite::Underline),
        MetalCursorShape::Block | MetalCursorShape::Hidden => None,
    }
}

/// A color the palette leaves fully transparent keeps the cell's own.
fn visible(color: SrgbaTuple) -> Option<SrgbaTuple> {
    (color.3 > 0.0).then_some(color)
}

/// One pane's live Metal frame state.
pub(crate) struct MetalFrame {
    pane_id: PaneId,
    mirror: RenderMirror,
    scene: MetalScene,
    glyphs: FontGlyphs,
    /// The cursor and the pane's focus as last drawn, and when either last
    /// changed, which restarts the cursor's blink (WebGpu's `prev_cursor`).
    cursor_seen: Option<(StableCursorPosition, bool)>,
    cursor_moved: Instant,
}

impl MetalFrame {
    pub(crate) fn scene(&self) -> &MetalScene {
        &self.scene
    }
}

/// The uniforms of one Metal frame.
pub(crate) struct MetalFrameUniforms {
    pub(crate) background: BackgroundUniforms,
    pub(crate) text: TextUniforms,
    /// How many of the scene's rows this update rebuilt (ft-yccm0.4.6).
    pub(crate) rows_rebuilt: usize,
    /// Whether the scene has slow and rapid blinking text (ft-yccm0.4.7.3).
    pub(crate) blinking: (bool, bool),
    /// When the cursor's blink next needs a frame.
    pub(crate) cursor_due: Option<Instant>,
    /// When the scene's image cells next need a frame: an animation's next
    /// frame, or a poll of an image still decoding (ft-yccm0.4.7.2).
    pub(crate) image_due: Option<Instant>,
    /// An image cell is still decoding, drawing nothing yet.
    pub(crate) images_loading: bool,
}

/// A window's blink clocks for its Metal frames (ft-yccm0.4.7.3): the WebGpu
/// renderer's `blink_state`, `rapid_blink_state` and `cursor_blink_state`,
/// built from the configuration as `TermWindow` builds them (and again when
/// it changes), one set per window so its panes blink in phase. A Metal
/// window's frames are drawn on its render thread, which keeps them there.
pub(crate) struct MetalBlinkClocks {
    generation: usize,
    slow: ColorEase,
    rapid: ColorEase,
    cursor: ColorEase,
}

/// The text blink levels of one draw, and when each next moves.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct MetalBlinkPhase {
    pub(crate) levels: BlinkLevels,
    slow_due: Option<Instant>,
    rapid_due: Option<Instant>,
}

impl MetalBlinkClocks {
    pub(crate) fn new(config: &ConfigHandle) -> Self {
        Self {
            generation: config.generation(),
            slow: ColorEase::new(
                config.text_blink_rate,
                config.text_blink_ease_in,
                config.text_blink_rate,
                config.text_blink_ease_out,
                None,
            ),
            rapid: ColorEase::new(
                config.text_blink_rate_rapid,
                config.text_blink_rapid_ease_in,
                config.text_blink_rate_rapid,
                config.text_blink_rapid_ease_out,
                None,
            ),
            cursor: ColorEase::new(
                config.cursor_blink_rate,
                config.cursor_blink_ease_in,
                config.cursor_blink_rate,
                config.cursor_blink_ease_out,
                None,
            ),
        }
    }

    /// The text blink levels now (`intensity_continuous`, as WebGpu reads
    /// them while shaping blinking text), from clocks rebuilt first if
    /// `config` changed, or `pin`, a render snapshot's pinned level, which
    /// does not move. A blink rate of 0 does not blink.
    pub(crate) fn text_phase(
        &mut self,
        config: &ConfigHandle,
        pin: Option<f32>,
    ) -> MetalBlinkPhase {
        if self.generation != config.generation() {
            *self = Self::new(config);
        }
        let level = |ease: &mut ColorEase| match pin {
            Some(level) => (level, Instant::now() + std::time::Duration::from_secs(3600)),
            None => ease.intensity_continuous(),
        };
        let slow = (config.text_blink_rate != 0).then(|| level(&mut self.slow));
        let rapid = (config.text_blink_rate_rapid != 0).then(|| level(&mut self.rapid));
        MetalBlinkPhase {
            levels: BlinkLevels {
                slow: slow.map(|(level, _)| level),
                rapid: rapid.map(|(level, _)| level),
            },
            slow_due: slow.map(|(_, due)| due),
            rapid_due: rapid.map(|(_, due)| due),
        }
    }

    /// The blink level of a cursor that last moved at `moved`, and when it
    /// next changes, as WebGpu's `compute_cell_fg_bg` reads it, or `pin`.
    fn cursor_level(&mut self, moved: Instant, pin: Option<f32>) -> (f32, Instant) {
        self.cursor.update_start(moved);
        match pin {
            Some(level) => (level, Instant::now() + std::time::Duration::from_secs(3600)),
            None => self.cursor.intensity_continuous(),
        }
    }
}

impl MetalBlinkPhase {
    /// When blinking text next needs a frame, for scenes with the slow and
    /// rapid blinks in `blinking`.
    pub(crate) fn due(&self, (slow, rapid): (bool, bool)) -> Option<Instant> {
        let slow = self.slow_due.filter(|_| slow);
        let rapid = self.rapid_due.filter(|_| rapid);
        slow.into_iter().chain(rapid).min()
    }
}

/// A compose cursor's state in the active pane, from the window
/// (ft-yccm0.4.7.3): WebGpu's `dead_key_or_leader`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MetalCompose {
    /// The IME composition (`DeadKeyStatus::Composing`); `None` while a dead
    /// key is held or the leader key is active.
    pub(crate) text: Option<String>,
}

/// What a Metal frame needs from its window, captured on the main thread
/// (ft-yccm0.4.1.2). It is all `Send`, so a render thread can build the frame
/// from it.
#[derive(Clone)]
pub(crate) struct MetalFrameInputs {
    /// The pane, or its overlay; `None` draws only the clear color.
    pub(crate) pane: Option<Arc<dyn Pane>>,
    pub(crate) viewport_top: Option<StableRowIndex>,
    pub(crate) config: ConfigHandle,
    /// The window is focused and the pane is its active pane.
    pub(crate) focused: bool,
    /// The pane's selection, normalized, and whether it is rectangular.
    pub(crate) selection: Option<(SelectionRange, bool)>,
    /// The hyperlink under the mouse.
    pub(crate) hover: Option<Arc<Hyperlink>>,
    /// Where the grid's first cell starts, in pixels: padding, tab bar and
    /// window border.
    pub(crate) grid_origin: [f32; 2],
    /// A dead key, IME composition or the leader key in this pane, which
    /// must be the window's active pane.
    pub(crate) compose: Option<MetalCompose>,
    /// The visual bell's level while it rings on this pane's cursor
    /// (`VisualBellTarget::CursorColor`).
    pub(crate) bell_cursor: Option<f32>,
    /// The window's repaint when a fallback font resolves.
    pub(crate) fallback_ready: Option<FallbackReady>,
    /// The level a render snapshot pins every blink at.
    pub(crate) blink_pin: Option<f32>,
    /// The pane reports password input (`detect_password_input`): its
    /// cursor is drawn as the lock glyph.
    pub(crate) password_input: bool,
}

impl MetalFrame {
    /// Captures `inputs.pane`'s changed rows into the render mirror of the
    /// frame in `slot` and brings its scene up to date; `slot` then holds the
    /// frame. `None` without a pane. Runs on whichever thread draws the
    /// window: the main thread, or the window's render thread with that
    /// thread's own `fonts` and `metrics` (ft-yccm0.4.1.2). Blinking text is
    /// drawn at `phase`, and a blinking cursor reads the window's `clocks`.
    // Pixel sizes and cell coordinates are small and non-negative.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        clippy::too_many_lines
    )]
    pub(crate) fn update(
        slot: &mut Option<MetalFrame>,
        inputs: &MetalFrameInputs,
        fonts: &Rc<FontConfiguration>,
        metrics: &RenderMetrics,
        metal: &Rc<MetalRenderer>,
        clocks: &mut MetalBlinkClocks,
        phase: &MetalBlinkPhase,
    ) -> Option<MetalFrameUniforms> {
        let pane = inputs.pane.as_ref()?;
        let pane_id = pane.pane_id();
        let config = &inputs.config;
        let mut frame = match slot.take() {
            Some(frame)
                if frame.pane_id == pane_id
                    && frame
                        .glyphs
                        .serves(fonts, metal, metrics, config.generation()) =>
            {
                frame
            }
            _ => MetalFrame {
                pane_id,
                mirror: RenderMirror::new(),
                scene: MetalScene::new(),
                glyphs: FontGlyphs::new(
                    Rc::clone(fonts),
                    config.clone(),
                    Rc::clone(metal),
                    metrics,
                ),
                cursor_seen: None,
                cursor_moved: Instant::now(),
            },
        };
        frame
            .glyphs
            .set_fallback_ready(inputs.fallback_ready.clone());

        let request = CaptureRequest {
            viewport_top: inputs.viewport_top,
            rules: &config.hyperlink_rules,
            rules_generation: config.generation(),
        };
        let captured = pane
            .downcast_ref::<LocalPane>()
            .and_then(|local| local.capture_render_rows(&mut frame.mirror, &request))
            .unwrap_or_else(|| capture_pane_rows(&**pane, &mut frame.mirror, &request));
        metrics::histogram!("gui.metal.capture.rows_copied").record(captured.rows_captured as f64);

        if frame.glyphs.begin_frame() {
            frame.scene.invalidate();
        }
        let facts = pane.render_facts();
        let palette: &ColorPalette = &facts.palette;
        let focused_and_active = inputs.focused;
        let cursor = frame.mirror.cursor();
        // As WebGpu's `prev_cursor`: a cursor that moved, or a focus change,
        // restarts the cursor's blink.
        let seen = (cursor, focused_and_active);
        if frame.cursor_seen != Some(seen) {
            frame.cursor_seen = Some(seen);
            frame.cursor_moved = Instant::now();
        }
        let shape = config.default_cursor_style.effective_shape(cursor.shape);
        let block_cursor = focused_and_active
            && matches!(
                shape,
                CursorShape::Default | CursorShape::BlinkingBlock | CursorShape::SteadyBlock
            );
        let metal_shape = cursor_shape(shape, focused_and_active);
        let bold_brightens = config.bold_brightens_ansi_colors != config::BoldBrightening::No;
        // WebGpu's reverse-video cursor (force_reverse_video_cursor), for a
        // pane that keeps the window's cursor colors.
        let reverse_video_cursor = config
            .force_reverse_video_cursor
            .then(|| {
                let window = config::TermConfig::new().color_palette();
                palette.cursor_fg == window.cursor_fg && palette.cursor_bg == window.cursor_bg
            })
            .unwrap_or(false)
            .then_some(config.reverse_video_cursor_min_contrast);
        // The cell under the cursor by its attribute colors, as WebGpu's
        // cursor quad resolves them, and whether the reverse-video cursor
        // applies to it.
        let cursor_cell = usize::try_from(cursor.y - frame.mirror.first())
            .ok()
            .and_then(|row| frame.mirror.rows().get(row));
        let (attr_fg, attr_bg) = cursor_attr_colors(cursor_cell, cursor.x, palette, bold_brightens);
        let reversed = reverse_video_cursor.is_some_and(|min_contrast| {
            reverse_video_cursor_applies(attr_fg, attr_bg, min_contrast)
        });
        // The visual bell ringing on the cursor, or else a dead key, an IME
        // composition or the leader key, draws a solid cursor: the first two
        // branches of WebGpu's `compute_cell_fg_bg`.
        let solid_base = if reversed { attr_fg } else { palette.cursor_bg };
        let solid_color = match (inputs.bell_cursor, &inputs.compose) {
            (Some(level), _) => {
                let fg = if reversed { attr_bg } else { palette.cursor_fg };
                let fg = config
                    .text_min_contrast_ratio
                    .and_then(|ratio| fg.ensure_contrast_ratio(&solid_base, ratio))
                    .unwrap_or(fg);
                let bell = config
                    .resolved_palette
                    .visual_bell
                    .as_deref()
                    .copied()
                    .unwrap_or(fg);
                Some(mix_linear(solid_base, bell, level))
            }
            (None, Some(_)) => Some(
                config
                    .resolved_palette
                    .compose_cursor
                    .as_deref()
                    .copied()
                    .unwrap_or(solid_base),
            ),
            (None, None) => None,
        };
        let solid = solid_color.map(|color| Compose {
            text: inputs
                .compose
                .as_ref()
                .and_then(|compose| compose.text.as_deref()),
            color,
            fg: palette.cursor_fg,
            under: palette.cursor_bg,
            lock: inputs.password_input,
        });
        // Any other focused cursor blinks as WebGpu's `blinking` does.
        let cursor_blink = (solid.is_none()
            && focused_and_active
            && shape.is_blinking()
            && config.cursor_blink_rate != 0
            && cursor.visibility == CursorVisibility::Visible)
            .then(|| clocks.cursor_level(frame.cursor_moved, inputs.blink_pin));
        // A focused cursor is in the cursor color, or in its cell's
        // foreground where the reverse-video cursor applies to the cell; a
        // blinking one moves toward its cell's background by its level.
        let mut cursor_color = if !focused_and_active {
            palette.cursor_border
        } else if reversed {
            attr_fg
        } else {
            palette.cursor_bg
        };
        if let Some((level, _)) = cursor_blink {
            cursor_color = mix_linear(cursor_color, attr_bg, level);
        }
        // At a password prompt WebGpu draws its lock glyph in place of the
        // cursor's sprite, in its color and layer, a focused block's fill
        // included; the text under a block keeps the block's colors.
        let sprite = if inputs.password_input {
            Some(CursorSprite::Lock {
                over: cursor_sprite(metal_shape) == Some(CursorSprite::Bar),
            })
        } else {
            cursor_sprite(metal_shape)
        };
        let selection = &inputs.selection;
        let selected = |stable| {
            selection.as_ref().map_or(0..0, |(range, rectangular)| {
                range.cols_for_row(stable, *rectangular)
            })
        };
        let style = SceneStyle {
            palette,
            generation: ((config.generation() as u64) << 32)
                | (facts.palette_generation & 0xffff_ffff),
            bold_brightens,
            selection_fg: visible(palette.selection_fg),
            selection_bg: palette.selection_bg,
            cursor_fg: if block_cursor {
                visible(palette.cursor_fg)
            } else {
                None
            },
            cursor_bg: block_cursor.then_some(palette.cursor_bg),
            cursor_sprite: sprite.map(|sprite| (sprite, cursor_color)),
            hover: inputs.hover.as_ref(),
            blink: phase.levels,
            compose: solid,
            min_contrast: config.text_min_contrast_ratio,
            reverse_video_cursor,
            cursor_blink: cursor_blink.map(|(level, _)| level),
        };
        let mut update = frame
            .scene
            .update(&frame.mirror, &style, &selected, &mut frame.glyphs);
        // ft-yccm0.4.7.3: a fallback font installed while shaping leaves the
        // clusters shaped before it stale. Rebuild the scene against the grown
        // chain, as the WebGpu renderer re-runs its paint pass.
        let mut passes = 1;
        while frame.glyphs.take_chain_changed() && passes < MAX_SHAPE_PASSES {
            frame.scene.invalidate();
            update = frame
                .scene
                .update(&frame.mirror, &style, &selected, &mut frame.glyphs);
            passes += 1;
        }
        metrics::histogram!("gui.metal.scene.rows_rebuilt").record(update.rows_rebuilt as f64);
        // Rows with images are rebuilt every frame (the mirror recaptures
        // them), so this covers every image cell in the scene.
        let (image_due, images_loading) = frame.glyphs.image_poll();

        let rows = frame.mirror.rows();
        let cursor_row = cursor.y - frame.mirror.first();
        let cursor = match usize::try_from(cursor_row) {
            Ok(row) if cursor.visibility == CursorVisibility::Visible && row < rows.len() => {
                let width_cells = rows[row]
                    .cells()
                    .iter()
                    .find(|cell| (cell.col()..cell.col() + cell.width()).contains(&cursor.x))
                    .map_or(1, |cell| cell.width().clamp(1, 2) as u32);
                CursorUniform {
                    // A cursor the scene draws as a sprite is not drawn here.
                    shape: if solid.is_some() || sprite.is_some() {
                        MetalCursorShape::Hidden
                    } else {
                        metal_shape
                    },
                    col: cursor.x as u32,
                    row: row as u32,
                    width_cells,
                    thickness: metrics.underline_height.max(1) as f32,
                    color: premultiplied(cursor_color),
                }
            }
            _ => CursorUniform::default(),
        };
        let uniforms = MetalFrameUniforms {
            background: BackgroundUniforms {
                cell_size: [
                    metrics.cell_size.width as f32,
                    metrics.cell_size.height as f32,
                ],
                grid_origin: inputs.grid_origin,
                row_offset: frame.scene.cells().row_offset(),
                cursor,
                selection_tint: premultiplied(palette.selection_bg),
                search_tint: [0.0; 4],
                current_match_tint: [0.0; 4],
            },
            text: TextUniforms {
                underline_position: metrics.descender_row as f32,
                line_thickness: metrics.underline_height.max(1) as f32,
                strikethrough_position: metrics.strike_row as f32,
            },
            rows_rebuilt: update.rows_rebuilt,
            blinking: frame.scene.blinking(),
            cursor_due: cursor_blink.map(|(_, due)| due),
            image_due,
            images_loading,
        };
        *slot = Some(frame);
        Some(uniforms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unfocused_windows_draw_a_hollow_cursor_whatever_the_shape() {
        for shape in [
            CursorShape::Default,
            CursorShape::SteadyBar,
            CursorShape::BlinkingUnderline,
        ] {
            assert_eq!(cursor_shape(shape, false), MetalCursorShape::HollowBlock);
        }
    }

    #[test]
    fn focused_cursor_shapes_map_one_to_one() {
        assert_eq!(
            cursor_shape(CursorShape::Default, true),
            MetalCursorShape::Block
        );
        assert_eq!(
            cursor_shape(CursorShape::SteadyBlock, true),
            MetalCursorShape::Block
        );
        assert_eq!(
            cursor_shape(CursorShape::BlinkingUnderline, true),
            MetalCursorShape::Underline
        );
        assert_eq!(
            cursor_shape(CursorShape::SteadyBar, true),
            MetalCursorShape::Bar
        );
    }

    /// ft-yccm0.4.7.3: only a focused block is left to the background pass.
    #[test]
    fn every_cursor_but_a_focused_block_is_a_sprite() {
        assert_eq!(cursor_sprite(MetalCursorShape::Block), None);
        assert_eq!(cursor_sprite(MetalCursorShape::Hidden), None);
        for (shape, focused, sprite) in [
            (CursorShape::SteadyBlock, false, CursorSprite::HollowBlock),
            (CursorShape::SteadyBar, false, CursorSprite::HollowBlock),
            (CursorShape::SteadyBar, true, CursorSprite::Bar),
            (
                CursorShape::BlinkingUnderline,
                true,
                CursorSprite::Underline,
            ),
        ] {
            assert_eq!(
                cursor_sprite(cursor_shape(shape, focused)),
                Some(sprite),
                "{shape:?} focused {focused}"
            );
        }
    }

    #[test]
    fn transparent_palette_colors_keep_the_cell_color_and_tints_premultiply() {
        assert_eq!(visible(SrgbaTuple(0.0, 0.0, 0.0, 0.0)), None);
        assert_eq!(
            visible(SrgbaTuple(1.0, 0.0, 0.0, 1.0)),
            Some(SrgbaTuple(1.0, 0.0, 0.0, 1.0))
        );
        assert_eq!(
            premultiplied(SrgbaTuple(1.0, 0.5, 0.25, 0.5)),
            [0.5, 0.25, 0.125, 0.5]
        );
    }

    /// ft-yccm0.4.7.3: frames are scheduled for a blink only while some
    /// scene has text with it, at the sooner of the two.
    #[test]
    fn blinking_text_schedules_frames_only_for_the_blinks_drawn() {
        let now = Instant::now();
        let (slow, rapid) = (
            now + std::time::Duration::from_millis(40),
            now + std::time::Duration::from_millis(10),
        );
        let phase = MetalBlinkPhase {
            levels: BlinkLevels::default(),
            slow_due: Some(slow),
            rapid_due: Some(rapid),
        };
        assert_eq!(phase.due((false, false)), None);
        assert_eq!(phase.due((true, false)), Some(slow));
        assert_eq!(phase.due((false, true)), Some(rapid));
        assert_eq!(phase.due((true, true)), Some(rapid));
        assert_eq!(MetalBlinkPhase::default().due((true, true)), None);
    }

    /// The default configuration blinks text: both levels are in 0..=1 and
    /// due again later; the cursor's level restarts when it moves.
    #[test]
    fn the_blink_clocks_follow_the_configuration() {
        let config = ConfigHandle::default_config();
        let mut clocks = MetalBlinkClocks::new(&config);
        let phase = clocks.text_phase(&config, None);
        let start = Instant::now();
        for level in [phase.levels.slow, phase.levels.rapid] {
            let level = level.expect("the default rates blink");
            assert!((0.0..=1.0).contains(&level), "{level}");
        }
        assert!(phase.due((true, true)).is_some_and(|due| due >= start));
        let (level, due) = clocks.cursor_level(Instant::now(), None);
        assert!((0.0..=1.0).contains(&level), "{level}");
        assert!(due >= start);

        // A render snapshot's pin is every level, and does not move soon.
        let pinned = clocks.text_phase(&config, Some(0.25));
        assert_eq!(
            (pinned.levels.slow, pinned.levels.rapid),
            (Some(0.25), Some(0.25))
        );
        assert!(
            pinned
                .due((true, true))
                .is_some_and(|due| due > start + std::time::Duration::from_secs(60))
        );
        assert_eq!(clocks.cursor_level(Instant::now(), Some(0.75)).0, 0.75);
    }
}
