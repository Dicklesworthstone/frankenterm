use crate::quad::{HeapQuadAllocator, QuadTrait, TripleLayerQuadAllocator};
use crate::selection::SelectionRange;
use crate::termwindow::box_model::*;
use crate::termwindow::render::compositor::{DirtyRect, DrawCmd, Layer, LayerKind};
use crate::termwindow::render::dirty_lines::DirtyLineBitmap;
use crate::termwindow::render::{
    CursorProperties, LineQuadCacheKey, LineQuadCacheValue, LineToEleShapeCacheKey,
    RenderScreenLineParams, same_hyperlink_or_both_none,
};
use crate::termwindow::{ScrollHit, UIItem, UIItemType};
use ::window::DeadKeyStatus;
use ::window::bitmaps::TextureRect;
use anyhow::Context;
use config::VisualBellTarget;
use frankenterm_gui::accessibility_preferences::{
    build_update as build_accessibility_update, probe_platform_preferences,
};
use frankenterm_gui::floating_panes::high_contrast_border_style;
use mux::Mux;
use mux::pane::{PaneId, WithPaneLines};
use mux::renderable::{RenderableDimensions, StableCursorPosition};
use mux::tab::PositionedPane;
use ordered_float::NotNan;
use std::time::Instant;
use wezterm_dynamic::Value;
use wezterm_term::color::{ColorAttribute, ColorPalette};
use wezterm_term::{Line, StableRowIndex};
use window::color::LinearRgba;

/// LayerStack adapter for the current tiled-pane grid.
///
/// This is intentionally geometry-first: `paint.rs` still owns the
/// live GPU allocation path, while this layer establishes the
/// compositor contract and dirty-rect conversion that the paint
/// migration plugs into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TiledGridLayer {
    pane_id: PaneId,
    dirty_rect: Option<DirtyRect>,
    opaque: bool,
    dirty_rows: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub struct TiledGridLayerGeometry {
    pub origin_x_px: i32,
    pub origin_y_px: i32,
    pub cols: usize,
    pub visible_rows: usize,
    pub cell_width_px: u32,
    pub cell_height_px: u32,
}

#[allow(dead_code)]
impl TiledGridLayer {
    #[must_use]
    pub fn from_dirty_lines(
        pane_id: PaneId,
        geometry: TiledGridLayerGeometry,
        dirty_lines: Option<&DirtyLineBitmap>,
        covers_viewport_opaquely: bool,
    ) -> Self {
        let full_rect = tiled_grid_full_rect(
            geometry.origin_x_px,
            geometry.origin_y_px,
            geometry.cols,
            geometry.visible_rows,
            geometry.cell_width_px,
            geometry.cell_height_px,
        );
        let dirty_rect = dirty_lines
            .and_then(|bitmap| {
                tiled_grid_dirty_rect_from_bitmap(
                    geometry.origin_x_px,
                    geometry.origin_y_px,
                    geometry.cols,
                    geometry.cell_width_px,
                    geometry.cell_height_px,
                    bitmap,
                )
            })
            .or_else(|| dirty_lines.is_none().then_some(full_rect))
            .filter(|rect| !rect.is_empty());
        let dirty_rows = dirty_lines.map_or(geometry.visible_rows, DirtyLineBitmap::count) as u32;
        let opaque = covers_viewport_opaquely
            && dirty_rect
                .map(|rect| rect.contains(&full_rect))
                .unwrap_or(false);

        Self {
            pane_id,
            dirty_rect,
            opaque,
            dirty_rows,
        }
    }

    #[must_use]
    pub fn pane_id(&self) -> PaneId {
        self.pane_id
    }

    #[must_use]
    pub fn dirty_rows(&self) -> u32 {
        self.dirty_rows
    }
}

impl Layer for TiledGridLayer {
    fn kind(&self) -> LayerKind {
        LayerKind::TiledGrid
    }

    fn render(
        &mut self,
        _ctx: &crate::termwindow::render::compositor::LayerContext,
    ) -> Vec<DrawCmd> {
        if self.dirty_rect.is_none() {
            return Vec::new();
        }
        vec![DrawCmd::Placeholder {
            layer: LayerKind::TiledGrid,
            count: self.dirty_rows.max(1),
        }]
    }

    fn dirty_rect(&self) -> Option<DirtyRect> {
        self.dirty_rect
    }

    fn opaque(&self) -> bool {
        self.opaque
    }
}

#[must_use]
#[allow(dead_code)]
fn tiled_grid_full_rect(
    pane_origin_x_px: i32,
    pane_origin_y_px: i32,
    cols: usize,
    visible_rows: usize,
    cell_width_px: u32,
    cell_height_px: u32,
) -> DirtyRect {
    DirtyRect::new(
        pane_origin_x_px,
        pane_origin_y_px,
        (cols as u32).saturating_mul(cell_width_px),
        (visible_rows as u32).saturating_mul(cell_height_px),
    )
}

#[must_use]
#[allow(dead_code)]
fn tiled_grid_dirty_rect_from_bitmap(
    pane_origin_x_px: i32,
    pane_origin_y_px: i32,
    cols: usize,
    cell_width_px: u32,
    cell_height_px: u32,
    dirty_lines: &DirtyLineBitmap,
) -> Option<DirtyRect> {
    let mut rows = dirty_lines.iter_dirty();
    let first = rows.next()?;
    let last = rows.last().unwrap_or(first);
    Some(DirtyRect::new(
        pane_origin_x_px,
        pane_origin_y_px.saturating_add((first as i32).saturating_mul(cell_height_px as i32)),
        (cols as u32).saturating_mul(cell_width_px),
        ((last - first + 1) as u32).saturating_mul(cell_height_px),
    ))
}

impl crate::TermWindow {
    fn focused_floating_pane_border_width(&self, pane_id: PaneId) -> Option<f32> {
        let mux = Mux::try_get()?;
        let tab = mux.get_active_tab_for_window(self.mux_window_id)?;
        let focused = tab
            .iter_floating_panes()
            .into_iter()
            .any(|pane| pane.pane_id == pane_id && pane.is_focused && pane.visible);
        if !focused {
            return None;
        }
        let high_contrast = build_accessibility_update(probe_platform_preferences(), vec![])
            .palette
            .high_contrast;
        Some(f32::from(
            high_contrast_border_style(high_contrast, [255, 255, 0, 255]).width_px,
        ))
    }

    fn paint_pane_box_model(&mut self, pos: &PositionedPane) -> anyhow::Result<()> {
        let computed = self.build_pane(pos)?;
        let mut ui_items = computed.ui_items();
        self.ui_items.append(&mut ui_items);
        let gl_state = self
            .render_state
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("render_state not initialized during paint"))?;
        self.render_element(&computed, gl_state, None)
    }

    pub fn paint_pane(
        &mut self,
        pos: &PositionedPane,
        layers: &mut TripleLayerQuadAllocator,
    ) -> anyhow::Result<()> {
        if self.config.use_box_model_render {
            return self.paint_pane_box_model(pos);
        }

        self.check_for_dirty_lines_and_invalidate_selection(&pos.pane);
        /*
        let zone = {
            let dims = pos.pane.get_dimensions();
            let position = self
                .get_viewport(pos.pane.pane_id())
                .unwrap_or(dims.physical_top);

            let zones = self.get_semantic_zones(&pos.pane);
            let idx = match zones.binary_search_by(|zone| zone.start_y.cmp(&position)) {
                Ok(idx) | Err(idx) => idx,
            };
            let idx = ((idx as isize) - 1).max(0) as usize;
            zones.get(idx).cloned()
        };
        */

        let global_cursor_fg = self.palette().cursor_fg;
        let global_cursor_bg = self.palette().cursor_bg;
        let config = self.config.clone();
        let palette = pos.pane.palette();

        let (padding_left, padding_top) = self.padding_left_top();

        let tab_bar_height = if self.show_tab_bar {
            self.tab_bar_pixel_height()
                .context("tab_bar_pixel_height")?
        } else {
            0.
        };
        let (top_bar_height, bottom_bar_height) = if self.config.tab_bar_at_bottom {
            (0.0, tab_bar_height)
        } else {
            (tab_bar_height, 0.0)
        };

        let border = self.get_os_border();
        let top_pixel_y = top_bar_height + padding_top + border.top.get() as f32;

        let cursor = pos.pane.get_cursor_position();
        let pane_id = pos.pane.pane_id();
        let current_viewport = self.get_viewport(pane_id);
        let dims = pos.pane.get_dimensions();
        if pos.is_active {
            if let Some(previous_cursor) = self.prev_cursor.update(&cursor) {
                let viewport = current_viewport.unwrap_or(dims.physical_top);
                let bitmap = self.dirty_lines_for_pane(pane_id, dims.viewport_rows);
                crate::termwindow::mark_cursor_rows_dirty(
                    bitmap,
                    viewport,
                    previous_cursor,
                    cursor,
                );
                self.record_dirty_event(
                    frankenterm_core::dirty_line_telemetry::DirtyEventSource::CursorMove,
                );
            }
        }

        let gl_state = self
            .render_state
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("render_state not initialized during paint"))?;

        let cursor_border_color = palette.cursor_border.to_linear();
        let foreground = palette.foreground.to_linear();
        let white_space = gl_state.util_sprites.white_space.texture_coords();
        let filled_box = gl_state.util_sprites.filled_box.texture_coords();

        let window_is_transparent =
            !self.window_background.is_empty() || config.window_background_opacity != 1.0;

        let default_bg = palette
            .resolve_bg(ColorAttribute::Default)
            .to_linear()
            .mul_alpha(if window_is_transparent {
                0.
            } else {
                config.text_background_opacity
            });

        let cell_width = self.render_metrics.cell_size.width as f32;
        let cell_height = self.render_metrics.cell_size.height as f32;
        let background_rect = {
            // We want to fill out to the edges of the splits
            let (x, width_delta) = if pos.left == 0 {
                (
                    0.,
                    padding_left + border.left.get() as f32 + (cell_width / 2.0),
                )
            } else {
                (
                    padding_left + border.left.get() as f32 - (cell_width / 2.0)
                        + (pos.left as f32 * cell_width),
                    cell_width,
                )
            };

            let (y, height_delta) = if pos.top == 0 {
                (
                    (top_pixel_y - padding_top),
                    padding_top + (cell_height / 2.0),
                )
            } else {
                (
                    top_pixel_y + (pos.top as f32 * cell_height) - (cell_height / 2.0),
                    cell_height,
                )
            };
            euclid::rect(
                x,
                y,
                // Go all the way to the right edge if we're right-most
                if pos.left + pos.width >= self.terminal_size.cols as usize {
                    self.dimensions.pixel_width as f32 - x
                } else {
                    (pos.width as f32 * cell_width) + width_delta
                },
                // Go all the way to the bottom if we're bottom-most
                if pos.top + pos.height >= self.terminal_size.rows as usize {
                    self.dimensions.pixel_height as f32 - y
                } else {
                    (pos.height as f32 * cell_height) + height_delta as f32
                },
            )
        };

        if self.window_background.is_empty() {
            // Per-pane, palette-specified background

            let mut quad = self
                .filled_rectangle(
                    layers,
                    0,
                    background_rect,
                    palette
                        .background
                        .to_linear()
                        .mul_alpha(config.window_background_opacity),
                )
                .context("filled_rectangle")?;
            quad.set_hsv(if pos.is_active {
                None
            } else {
                Some(config.inactive_pane_hsb)
            });
        }

        {
            // If the bell is ringing, we draw another background layer over the
            // top of this in the configured bell color
            if let Some(intensity) = self.get_intensity_if_bell_target_ringing(
                &pos.pane,
                &config,
                VisualBellTarget::BackgroundColor,
            ) {
                // target background color
                let LinearRgba(r, g, b, _) = config
                    .resolved_palette
                    .visual_bell
                    .as_deref()
                    .unwrap_or(&palette.foreground)
                    .to_linear();

                let background = if window_is_transparent {
                    // for transparent windows, we fade in the target color
                    // by adjusting its alpha
                    LinearRgba::with_components(r, g, b, intensity)
                } else {
                    // otherwise We'll interpolate between the background color
                    // and the the target color
                    let (r1, g1, b1, a) = palette
                        .background
                        .to_linear()
                        .mul_alpha(config.window_background_opacity)
                        .tuple();
                    LinearRgba::with_components(
                        r1 + (r - r1) * intensity,
                        g1 + (g - g1) * intensity,
                        b1 + (b - b1) * intensity,
                        a,
                    )
                };
                log::trace!("bell color is {:?}", background);

                let mut quad = self
                    .filled_rectangle(layers, 0, background_rect, background)
                    .context("filled_rectangle")?;

                quad.set_hsv(if pos.is_active {
                    None
                } else {
                    Some(config.inactive_pane_hsb)
                });
            }
        }

        // Agent state border overlay: draw colored border around agent panes.
        if self.config.agent_detection_enabled {
            if let Some(agent_state) = self.agent_pane_states.get(&pane_id) {
                if let Some((r, g, b, a)) = agent_state.border_color_rgba() {
                    let border_w = self.config.agent_border_width.max(1) as f32;
                    let color = LinearRgba::with_components(
                        r as f32 / 255.0,
                        g as f32 / 255.0,
                        b as f32 / 255.0,
                        a as f32 / 255.0,
                    );
                    let (bx, by, bw, bh) = (
                        background_rect.origin.x,
                        background_rect.origin.y,
                        background_rect.size.width,
                        background_rect.size.height,
                    );
                    // Top edge
                    self.filled_rectangle(layers, 2, euclid::rect(bx, by, bw, border_w), color)?;
                    // Bottom edge
                    self.filled_rectangle(
                        layers,
                        2,
                        euclid::rect(bx, by + bh - border_w, bw, border_w),
                        color,
                    )?;
                    // Left edge
                    self.filled_rectangle(layers, 2, euclid::rect(bx, by, border_w, bh), color)?;
                    // Right edge
                    self.filled_rectangle(
                        layers,
                        2,
                        euclid::rect(bx + bw - border_w, by, border_w, bh),
                        color,
                    )?;
                }
            }
        }

        // TODO: we only have a single scrollbar in a single position.
        // We only update it for the active pane, but we should probably
        // do a per-pane scrollbar.  That will require more extensive
        // changes to ScrollHit, mouse positioning, PositionedPane
        // and tab size calculation.
        if pos.is_active && self.show_scroll_bar {
            let thumb_y_offset = top_bar_height as usize + border.top.get();

            let min_height = self.min_scroll_bar_height();

            let info = ScrollHit::thumb(
                &*pos.pane,
                current_viewport,
                self.dimensions.pixel_height.saturating_sub(
                    thumb_y_offset + border.bottom.get() + bottom_bar_height as usize,
                ),
                min_height as usize,
            );
            let abs_thumb_top = thumb_y_offset + info.top;
            let thumb_size = info.height;
            let color = palette.scrollbar_thumb.to_linear();

            // Adjust the scrollbar thumb position
            let config = &self.config;
            let padding = self.effective_right_padding(&config) as f32;

            let thumb_x = self
                .dimensions
                .pixel_width
                .saturating_sub(padding as usize)
                .saturating_sub(border.right.get());

            // Register the scroll bar location
            self.ui_items.push(UIItem {
                x: thumb_x,
                width: padding as usize,
                y: thumb_y_offset,
                height: info.top,
                item_type: UIItemType::AboveScrollThumb,
            });
            self.ui_items.push(UIItem {
                x: thumb_x,
                width: padding as usize,
                y: abs_thumb_top,
                height: thumb_size,
                item_type: UIItemType::ScrollThumb,
            });
            self.ui_items.push(UIItem {
                x: thumb_x,
                width: padding as usize,
                y: abs_thumb_top + thumb_size,
                height: self
                    .dimensions
                    .pixel_height
                    .saturating_sub(abs_thumb_top + thumb_size),
                item_type: UIItemType::BelowScrollThumb,
            });

            self.filled_rectangle(
                layers,
                2,
                euclid::rect(
                    thumb_x as f32,
                    abs_thumb_top as f32,
                    padding,
                    thumb_size as f32,
                ),
                color,
            )
            .context("filled_rectangle")?;
        }

        let (selrange, rectangular) = {
            let sel = self.selection(pos.pane.pane_id());
            (sel.range.clone(), sel.rectangular)
        };

        let start = Instant::now();
        let selection_fg = palette.selection_fg.to_linear();
        let selection_bg = palette.selection_bg.to_linear();
        let cursor_fg = palette.cursor_fg.to_linear();
        let cursor_bg = palette.cursor_bg.to_linear();
        let cursor_is_default_color =
            palette.cursor_fg == global_cursor_fg && palette.cursor_bg == global_cursor_bg;

        {
            let stable_range = match current_viewport {
                Some(top) => top..top + dims.viewport_rows as StableRowIndex,
                None => dims.physical_top..dims.physical_top + dims.viewport_rows as StableRowIndex,
            };

            pos.pane
                .apply_hyperlinks(stable_range.clone(), &self.config.hyperlink_rules);

            struct LineRender<'a, 'b> {
                term_window: &'a mut crate::TermWindow,
                selrange: Option<SelectionRange>,
                rectangular: bool,
                dims: RenderableDimensions,
                top_pixel_y: f32,
                left_pixel_x: f32,
                pos: &'a PositionedPane,
                pane_id: PaneId,
                cursor: &'a StableCursorPosition,
                palette: &'a ColorPalette,
                default_bg: LinearRgba,
                cursor_border_color: LinearRgba,
                selection_fg: LinearRgba,
                selection_bg: LinearRgba,
                cursor_fg: LinearRgba,
                cursor_bg: LinearRgba,
                foreground: LinearRgba,
                cursor_is_default_color: bool,
                white_space: TextureRect,
                filled_box: TextureRect,
                window_is_transparent: bool,
                layers: &'a mut TripleLayerQuadAllocator<'b>,
                error: Option<anyhow::Error>,
            }

            let left_pixel_x = padding_left
                + border.left.get() as f32
                + (pos.left as f32 * self.render_metrics.cell_size.width as f32);

            let mut render = LineRender {
                term_window: self,
                selrange,
                rectangular,
                dims,
                top_pixel_y,
                left_pixel_x,
                pos,
                pane_id,
                cursor: &cursor,
                palette: &palette,
                cursor_border_color,
                selection_fg,
                selection_bg,
                cursor_fg,
                default_bg,
                cursor_bg,
                foreground,
                cursor_is_default_color,
                white_space,
                filled_box,
                window_is_transparent,
                layers,
                error: None,
            };

            impl<'a, 'b> LineRender<'a, 'b> {
                fn render_line(
                    &mut self,
                    stable_top: StableRowIndex,
                    line_idx: usize,
                    line: &&mut Line,
                ) -> anyhow::Result<()> {
                    // Per ft-8pcwy / ft-jvj78 slice 2: ask the
                    // iter-dirty predicate whether this row can be
                    // skipped. The predicate stays inert (returns
                    // false for every row) until per-cell event
                    // sources are wired (ft-camu6) and the gate is
                    // flipped via TermWindow::set_iter_dirty_render_gate.
                    // No behavior change against today's
                    // not-yet-wired sources — the gate is off and
                    // the predicate falls through.
                    let gate_enabled = self.term_window.iter_dirty_render_gate_enabled();
                    if gate_enabled {
                        let should_skip = crate::termwindow::should_skip_clean_line(
                            true,
                            self.term_window.peek_dirty_lines(self.pane_id),
                            line_idx,
                        );
                        if should_skip {
                            self.term_window.record_clean_line_skipped(self.pane_id);
                            return Ok(());
                        }
                    }
                    let stable_row = stable_top + line_idx as StableRowIndex;
                    let selrange = self
                        .selrange
                        .map_or(0..0, |sel| sel.cols_for_row(stable_row, self.rectangular));
                    // Constrain to the pane width!
                    let selrange = selrange.start..selrange.end.min(self.dims.cols);

                    let (cursor, composing, password_input) = if self.cursor.y == stable_row {
                        (
                            Some(CursorProperties {
                                position: StableCursorPosition {
                                    y: 0,
                                    ..*self.cursor
                                },
                                dead_key_or_leader: self.term_window.dead_key_status
                                    != DeadKeyStatus::None
                                    || self.term_window.leader_is_active(),
                                cursor_fg: self.cursor_fg,
                                cursor_bg: self.cursor_bg,
                                cursor_border_color: self.cursor_border_color,
                                cursor_is_default_color: self.cursor_is_default_color,
                            }),
                            match (self.pos.is_active, &self.term_window.dead_key_status) {
                                (true, DeadKeyStatus::Composing(composing)) => {
                                    Some(composing.to_string())
                                }
                                _ => None,
                            },
                            if self.term_window.config.detect_password_input {
                                match self.pos.pane.get_metadata() {
                                    Value::Object(obj) => {
                                        match obj.get(&Value::String("password_input".to_string()))
                                        {
                                            Some(Value::Bool(b)) => *b,
                                            _ => false,
                                        }
                                    }
                                    _ => false,
                                }
                            } else {
                                false
                            },
                        )
                    } else {
                        (None, None, false)
                    };

                    let shape_hash = self.term_window.shape_hash_for_line(line);

                    let quad_key = LineQuadCacheKey {
                        pane_id: self.pane_id,
                        password_input,
                        pane_is_active: self.pos.is_active,
                        config_generation: self.term_window.config.generation(),
                        shape_generation: self.term_window.shape_generation,
                        quad_generation: self.term_window.quad_generation,
                        composing: composing.clone(),
                        selection: selrange.clone(),
                        cursor,
                        shape_hash,
                        top_pixel_y: NotNan::new(self.top_pixel_y).unwrap()
                            + (line_idx + self.pos.top) as f32
                                * self.term_window.render_metrics.cell_size.height as f32,
                        left_pixel_x: NotNan::new(self.left_pixel_x).unwrap(),
                        phys_line_idx: line_idx,
                        reverse_video: self.dims.reverse_video,
                    };

                    if let Some(cached_quad) =
                        self.term_window.line_quad_cache.borrow_mut().get(&quad_key)
                    {
                        let expired = cached_quad
                            .expires
                            .map(|i| Instant::now() >= i)
                            .unwrap_or(false);
                        let hover_changed = if cached_quad.invalidate_on_hover_change {
                            !same_hyperlink_or_both_none(
                                cached_quad.current_highlight.as_ref(),
                                self.term_window.current_highlight.as_ref(),
                            )
                        } else {
                            false
                        };
                        if !expired && !hover_changed {
                            cached_quad
                                .layers
                                .apply_to(self.layers)
                                .context("cached_quad.layers.apply_to")?;
                            self.term_window.update_next_frame_time(cached_quad.expires);
                            return Ok(());
                        }
                    }

                    let mut buf = HeapQuadAllocator::default();
                    let next_due = self.term_window.has_animation.borrow_mut().take();

                    let shape_key = LineToEleShapeCacheKey {
                        shape_hash,
                        shape_generation: quad_key.shape_generation,
                        composing: if self.cursor.y == stable_row && self.pos.is_active {
                            if let DeadKeyStatus::Composing(composing) =
                                &self.term_window.dead_key_status
                            {
                                Some((self.cursor.x, composing.to_string()))
                            } else {
                                None
                            }
                        } else {
                            None
                        },
                    };

                    let render_result = self
                        .term_window
                        .render_screen_line(
                            RenderScreenLineParams {
                                top_pixel_y: *quad_key.top_pixel_y,
                                left_pixel_x: self.left_pixel_x,
                                pixel_width: self.dims.cols as f32
                                    * self.term_window.render_metrics.cell_size.width as f32,
                                stable_line_idx: Some(stable_row),
                                line: &line,
                                selection: selrange.clone(),
                                cursor: &self.cursor,
                                palette: &self.palette,
                                dims: &self.dims,
                                config: &self.term_window.config,
                                cursor_border_color: self.cursor_border_color,
                                foreground: self.foreground,
                                is_active: self.pos.is_active,
                                pane: Some(&self.pos.pane),
                                selection_fg: self.selection_fg,
                                selection_bg: self.selection_bg,
                                cursor_fg: self.cursor_fg,
                                cursor_bg: self.cursor_bg,
                                cursor_is_default_color: self.cursor_is_default_color,
                                white_space: self.white_space,
                                filled_box: self.filled_box,
                                window_is_transparent: self.window_is_transparent,
                                default_bg: self.default_bg,
                                font: None,
                                style: None,
                                use_pixel_positioning: self
                                    .term_window
                                    .config
                                    .experimental_pixel_positioning,
                                render_metrics: self.term_window.render_metrics,
                                shape_key: Some(shape_key),
                                password_input,
                            },
                            &mut TripleLayerQuadAllocator::Heap(&mut buf),
                        )
                        .context("render_screen_line")?;

                    let expires = self.term_window.has_animation.borrow().as_ref().cloned();
                    self.term_window.update_next_frame_time(next_due);

                    buf.apply_to(self.layers)
                        .context("HeapQuadAllocator::apply_to")?;

                    let quad_value = LineQuadCacheValue {
                        layers: buf,
                        expires,
                        invalidate_on_hover_change: render_result.invalidate_on_hover_change,
                        current_highlight: if render_result.invalidate_on_hover_change {
                            self.term_window.current_highlight.clone()
                        } else {
                            None
                        },
                    };

                    self.term_window
                        .line_quad_cache
                        .borrow_mut()
                        .put(quad_key, quad_value);

                    Ok(())
                }
            }

            impl<'a, 'b> WithPaneLines for LineRender<'a, 'b> {
                fn with_lines_mut(&mut self, stable_top: StableRowIndex, lines: &mut [&mut Line]) {
                    for (line_idx, line) in lines.iter().enumerate() {
                        if let Err(err) = self.render_line(stable_top, line_idx, line) {
                            self.error.replace(err);
                            return;
                        }
                    }
                }
            }

            pos.pane.with_lines_mut(stable_range.clone(), &mut render);
            if let Some(error) = render.error.take() {
                return Err(error).context("error while calling with_lines_mut");
            }
        }

        /*
        if let Some(zone) = zone {
            // TODO: render a thingy to jump to prior prompt
        }
        */
        metrics::histogram!("paint_pane.lines").record(start.elapsed());
        log::trace!("lines elapsed {:?}", start.elapsed());

        Ok(())
    }

    pub fn build_pane(&mut self, pos: &PositionedPane) -> anyhow::Result<ComputedElement> {
        // First compute the bounds for the pane background

        let cell_width = self.render_metrics.cell_size.width as f32;
        let cell_height = self.render_metrics.cell_size.height as f32;
        let (padding_left, padding_top) = self.padding_left_top();
        let tab_bar_height = if self.show_tab_bar {
            self.tab_bar_pixel_height()?
        } else {
            0.
        };
        let (top_bar_height, _bottom_bar_height) = if self.config.tab_bar_at_bottom {
            (0.0, tab_bar_height)
        } else {
            (tab_bar_height, 0.0)
        };

        let border = self.get_os_border();
        let top_pixel_y = top_bar_height + padding_top + border.top.get() as f32;

        // We want to fill out to the edges of the splits
        let (x, width_delta) = if pos.left == 0 {
            (
                0.,
                padding_left + border.left.get() as f32 + (cell_width / 2.0),
            )
        } else {
            (
                padding_left + border.left.get() as f32 - (cell_width / 2.0)
                    + (pos.left as f32 * cell_width),
                cell_width,
            )
        };

        let (y, height_delta) = if pos.top == 0 {
            (
                (top_pixel_y - padding_top),
                padding_top + (cell_height / 2.0),
            )
        } else {
            (
                top_pixel_y + (pos.top as f32 * cell_height) - (cell_height / 2.0),
                cell_height,
            )
        };

        let background_rect = euclid::rect(
            x,
            y,
            // Go all the way to the right edge if we're right-most
            if pos.left + pos.width >= self.terminal_size.cols as usize {
                self.dimensions.pixel_width as f32 - x
            } else {
                (pos.width as f32 * cell_width) + width_delta
            },
            // Go all the way to the bottom if we're bottom-most
            if pos.top + pos.height >= self.terminal_size.rows as usize {
                self.dimensions.pixel_height as f32 - y
            } else {
                (pos.height as f32 * cell_height) + height_delta as f32
            },
        );

        // Bounds for the terminal cells
        let content_rect = euclid::rect(
            padding_left + border.left.get() as f32 - (cell_width / 2.0)
                + (pos.left as f32 * cell_width),
            top_pixel_y + (pos.top as f32 * cell_height) - (cell_height / 2.0),
            pos.width as f32 * cell_width,
            pos.height as f32 * cell_height,
        );

        let palette = pos.pane.palette();
        let focus_border_width = self.focused_floating_pane_border_width(pos.pane.pane_id());
        let focus_border = focus_border_width.map(|width| PixelDimension {
            left: width,
            top: width,
            right: width,
            bottom: width,
        });

        // TODO: visual bell background layer
        // TODO: scrollbar

        Ok(ComputedElement {
            item_type: None,
            zindex: 0,
            bounds: background_rect,
            border: focus_border.unwrap_or_default(),
            border_rect: background_rect,
            border_corners: None,
            colors: ElementColors {
                border: focus_border_width
                    .map(|_| BorderColor::new(palette.cursor_border.to_linear()))
                    .unwrap_or_default(),
                bg: if self.window_background.is_empty() {
                    palette
                        .background
                        .to_linear()
                        .mul_alpha(self.config.window_background_opacity)
                        .into()
                } else {
                    InheritableColor::Inherited
                },
                text: InheritableColor::Inherited,
            },
            hover_colors: None,
            padding: background_rect,
            content_rect,
            baseline: 1.0,
            content: ComputedElementContent::Children(vec![]),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::termwindow::render::compositor::{LayerContext, LayerStack};
    use proptest::prelude::*;
    use std::collections::BTreeSet;

    fn geometry() -> TiledGridLayerGeometry {
        TiledGridLayerGeometry {
            origin_x_px: 5,
            origin_y_px: 11,
            cols: 100,
            visible_rows: 24,
            cell_width_px: 8,
            cell_height_px: 16,
        }
    }

    fn arb_geometry() -> impl Strategy<Value = TiledGridLayerGeometry> {
        (
            -2_048_i32..=2_048,
            -2_048_i32..=2_048,
            0_usize..=512,
            0_usize..=128,
            0_u32..=64,
            0_u32..=64,
        )
            .prop_map(
                |(origin_x_px, origin_y_px, cols, visible_rows, cell_width_px, cell_height_px)| {
                    TiledGridLayerGeometry {
                        origin_x_px,
                        origin_y_px,
                        cols,
                        visible_rows,
                        cell_width_px,
                        cell_height_px,
                    }
                },
            )
    }

    fn bitmap_from_rows(capacity: usize, rows: &[usize]) -> DirtyLineBitmap {
        let mut bitmap = DirtyLineBitmap::new(capacity);
        for row in rows {
            bitmap.mark(*row);
        }
        bitmap
    }

    fn expected_dirty_rows(capacity: usize, rows: &[usize]) -> BTreeSet<usize> {
        rows.iter().copied().filter(|row| *row < capacity).collect()
    }

    fn expected_dirty_rect_for_rows(
        geometry: TiledGridLayerGeometry,
        dirty_rows: &BTreeSet<usize>,
    ) -> Option<DirtyRect> {
        let first = dirty_rows.first().copied()?;
        let last = dirty_rows.last().copied().unwrap_or(first);
        let rect = DirtyRect::new(
            geometry.origin_x_px,
            geometry
                .origin_y_px
                .saturating_add((first as i32).saturating_mul(geometry.cell_height_px as i32)),
            (geometry.cols as u32).saturating_mul(geometry.cell_width_px),
            ((last - first + 1) as u32).saturating_mul(geometry.cell_height_px),
        );
        (!rect.is_empty()).then_some(rect)
    }

    fn full_rect_for_geometry(geometry: TiledGridLayerGeometry) -> DirtyRect {
        DirtyRect::new(
            geometry.origin_x_px,
            geometry.origin_y_px,
            (geometry.cols as u32).saturating_mul(geometry.cell_width_px),
            (geometry.visible_rows as u32).saturating_mul(geometry.cell_height_px),
        )
    }

    #[test]
    fn tiled_grid_layer_uses_full_rect_without_bitmap() {
        let layer = TiledGridLayer::from_dirty_lines(
            7,
            TiledGridLayerGeometry {
                origin_x_px: 10,
                origin_y_px: 20,
                cols: 80,
                visible_rows: 24,
                cell_width_px: 9,
                cell_height_px: 18,
            },
            None,
            true,
        );
        assert_eq!(layer.pane_id(), 7);
        assert_eq!(layer.dirty_rows(), 24);
        assert_eq!(layer.dirty_rect(), Some(DirtyRect::new(10, 20, 720, 432)));
        assert!(layer.opaque());
    }

    #[test]
    fn tiled_grid_layer_bounds_dirty_rows_from_bitmap() {
        let mut bitmap = DirtyLineBitmap::new(24);
        bitmap.mark(3);
        bitmap.mark(7);

        let layer = TiledGridLayer::from_dirty_lines(3, geometry(), Some(&bitmap), true);

        assert_eq!(layer.dirty_rows(), 2);
        assert_eq!(layer.dirty_rect(), Some(DirtyRect::new(5, 59, 800, 80)));
        assert!(
            !layer.opaque(),
            "partial dirty rows must not cull layers below"
        );
    }

    #[test]
    fn tiled_grid_layer_reports_clean_when_bitmap_is_empty() {
        let bitmap = DirtyLineBitmap::new(24);
        let layer = TiledGridLayer::from_dirty_lines(3, geometry(), Some(&bitmap), true);
        assert_eq!(layer.dirty_rect(), None);
        assert!(!layer.opaque());
    }

    #[test]
    fn tiled_grid_layer_participates_in_layer_stack_render() {
        let mut bitmap = DirtyLineBitmap::new(24);
        bitmap.mark_range(0..24);
        let layer = TiledGridLayer::from_dirty_lines(
            3,
            TiledGridLayerGeometry {
                origin_x_px: 0,
                origin_y_px: 0,
                cols: 80,
                visible_rows: 24,
                cell_width_px: 9,
                cell_height_px: 18,
            },
            Some(&bitmap),
            true,
        );

        let mut stack = LayerStack::new();
        stack.push(Box::new(layer));
        let report = stack.render(&LayerContext::new(1, DirtyRect::new(0, 0, 720, 432), 0));

        assert_eq!(report.layer_count, 1);
        assert_eq!(report.layers_rendered, 1);
        assert_eq!(report.layers_skipped_clean, 0);
        assert_eq!(report.total_commands, 1);
        assert_eq!(report.damage, DirtyRect::new(0, 0, 720, 432));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        #[test]
        fn proptest_tiled_grid_dirty_bitmap_maps_to_damage_rect_and_commands(
            geometry in arb_geometry(),
            rows in proptest::collection::vec(0_usize..=160, 0..96),
            covers_viewport_opaquely in any::<bool>(),
        ) {
            let bitmap = bitmap_from_rows(geometry.visible_rows, &rows);
            let expected_rows = expected_dirty_rows(geometry.visible_rows, &rows);
            let expected_rect = expected_dirty_rect_for_rows(geometry, &expected_rows);
            let full_rect = full_rect_for_geometry(geometry);
            let expected_opaque = covers_viewport_opaquely
                && expected_rect
                    .map(|rect| rect.contains(&full_rect))
                    .unwrap_or(false);

            let layer = TiledGridLayer::from_dirty_lines(
                9,
                geometry,
                Some(&bitmap),
                covers_viewport_opaquely,
            );

            prop_assert_eq!(layer.pane_id(), 9);
            prop_assert_eq!(layer.dirty_rows(), expected_rows.len() as u32);
            prop_assert_eq!(layer.dirty_rect(), expected_rect);
            prop_assert_eq!(layer.opaque(), expected_opaque);

            let mut render_layer = layer.clone();
            let commands = render_layer.render(&LayerContext::new(1, full_rect, 0));
            let expected_commands = expected_rect.map_or_else(Vec::new, |_| {
                vec![DrawCmd::Placeholder {
                    layer: LayerKind::TiledGrid,
                    count: (expected_rows.len() as u32).max(1),
                }]
            });

            prop_assert_eq!(commands, expected_commands);
        }

        #[test]
        fn proptest_tiled_grid_without_bitmap_uses_full_geometry_damage(
            geometry in arb_geometry(),
            covers_viewport_opaquely in any::<bool>(),
        ) {
            let full_rect = full_rect_for_geometry(geometry);
            let expected_rect = (!full_rect.is_empty()).then_some(full_rect);
            let expected_opaque = covers_viewport_opaquely
                && expected_rect
                    .map(|rect| rect.contains(&full_rect))
                    .unwrap_or(false);

            let layer = TiledGridLayer::from_dirty_lines(
                11,
                geometry,
                None,
                covers_viewport_opaquely,
            );

            prop_assert_eq!(layer.pane_id(), 11);
            prop_assert_eq!(layer.dirty_rows(), geometry.visible_rows as u32);
            prop_assert_eq!(layer.dirty_rect(), expected_rect);
            prop_assert_eq!(layer.opaque(), expected_opaque);
        }
    }
}
