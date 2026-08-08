use crate::resize_increment_calculator::ResizeIncrementCalculator;
use crate::termwindow::render::{LineToEleShapeCacheKey, LineToElementShapeItem};
use crate::utilsprites::RenderMetrics;
use ::window::{Dimensions, ResizeIncrement, Window, WindowOps, WindowState};
use config::{ConfigHandle, DimensionContext};
use frankenterm_font::FontConfiguration;
use lfucache::LfuCache;
use mux::Mux;
use std::cell::RefCell;
use std::rc::Rc;
use wezterm_term::TerminalSize;

/// Synchronous glyph warm-up budget applied on a font scale change (ft-uroqc).
/// Scale changes are rare, user-initiated events (zoom / DPI change), so a
/// one-frame (60fps) bound keeps the change responsive while pre-rasterizing the
/// common ASCII/Latin set at the new `CellMetricKey`. The value is a perf knob,
/// not a correctness parameter — warm-up is idempotent and any unwarmed glyph
/// still rasterizes lazily on first paint exactly as before. Tunable once the
/// renderer_slo bench unblocks (mcp_middleware cfg(test) fix, p4).
const SCALE_CHANGE_GLYPH_WARMUP_BUDGET: std::time::Duration = std::time::Duration::from_millis(16);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CacheGenerationAdvance {
    generation: usize,
    recycled_epoch: bool,
}

/// Select the next live generation-keyed cache authority.
///
/// A saturating counter is not an invalidation authority: once it reaches
/// `usize::MAX`, a later input change would publish the same generation and
/// could hit stale entries.  Recycling to zero is safe only when the caller
/// observes `recycled_epoch` and first retires every cache that can contain an
/// older zero-generation key.
fn next_cache_generation(generation: usize) -> CacheGenerationAdvance {
    match generation.checked_add(1) {
        Some(generation) => CacheGenerationAdvance {
            generation,
            recycled_epoch: false,
        },
        None => CacheGenerationAdvance {
            generation: 0,
            recycled_epoch: true,
        },
    }
}

/// Drop line-shaping results keyed by a superseded shaping-input generation.
///
/// Unlike the lower-level HarfBuzz `shape_cache`, these entries include the
/// pixel-scale-dependent glyph resolution produced by `render_screen_line`.
/// A generation bump makes every prior entry unreachable, and leaving hot
/// unreachable entries in the LFU can crowd out newly shaped lines. Atlas-only
/// generation changes retain the HarfBuzz cache through their separate path,
/// but every font/config/scale input change must clear this derived cache.
fn clear_generation_keyed_line_shape_cache(
    cache: &RefCell<LfuCache<LineToEleShapeCacheKey, LineToElementShapeItem>>,
) {
    cache.borrow_mut().clear();
}

#[derive(Debug, Clone, Copy)]
pub struct RowsAndCols {
    pub rows: usize,
    pub cols: usize,
}

#[derive(Debug)]
pub enum ScaleChange {
    Absolute(f64),
    Relative(f64),
}

impl super::TermWindow {
    /// Advance the shaping-input generation and retire every shaping-specific
    /// cache whose contents depend on those inputs.
    ///
    /// Texture-atlas rebuilds intentionally use a separate invalidation path:
    /// they retain `shape_cache` because HarfBuzz output is atlas-independent,
    /// while still clearing the line caches whose glyph sprites are not.
    pub(super) fn advance_shaping_input_generation(&mut self) {
        let advance = next_cache_generation(self.shape_generation);
        self.shape_cache.borrow_mut().clear();
        clear_generation_keyed_line_shape_cache(&self.line_to_ele_shape_cache);
        self.line_quad_cache.borrow_mut().clear();
        self.shape_generation = advance.generation;
    }

    /// Advance atlas-dependent shape authority while retaining atlas-invariant
    /// HarfBuzz results whenever the generation can advance normally.
    ///
    /// At numeric exhaustion, generation zero may still exist in retained
    /// `CachedShape` entries.  Clear that cache before recycling the epoch so
    /// an old sprite set can never compare equal to the newly rebuilt atlas.
    pub(super) fn advance_texture_atlas_shape_generation(&mut self) {
        let advance = next_cache_generation(self.shape_generation);
        if advance.recycled_epoch {
            self.shape_cache.borrow_mut().clear();
        }
        clear_generation_keyed_line_shape_cache(&self.line_to_ele_shape_cache);
        self.line_quad_cache.borrow_mut().clear();
        self.shape_generation = advance.generation;
    }

    /// Advance whole-quad authority after first retiring every keyed entry.
    /// Clearing before epoch recycling keeps generation zero unique among all
    /// live quad-cache records even after numeric exhaustion.
    pub(super) fn advance_quad_generation(&mut self) {
        let advance = next_cache_generation(self.quad_generation);
        self.line_quad_cache.borrow_mut().clear();
        self.quad_generation = advance.generation;
    }

    pub fn resize(
        &mut self,
        dimensions: Dimensions,
        window_state: WindowState,
        window: &Window,
        live_resizing: bool,
    ) {
        log::trace!(
            "resize event, live={} current cells: {:?}, current dims: {:?}, new dims: {:?} window_state:{:?}",
            live_resizing,
            self.current_cell_dimensions(),
            self.dimensions,
            dimensions,
            window_state,
        );
        if dimensions.pixel_width == 0 || dimensions.pixel_height == 0 {
            // on windows, this can happen when minimizing the window.
            // NOP!
            log::trace!("new dimensions are zero: NOP!");
            return;
        }
        if self.dimensions == dimensions && self.window_state == window_state {
            // It didn't really change
            log::trace!("dimensions didn't change NOP!");
            return;
        }

        // A real non-zero resize is a legitimate recovery signal for a stale,
        // lost, timed-out, or occluded presentation surface. It may reopen
        // only those surface-related recovery states; renderer invariant
        // circuits remain closed until reinitialization.
        self.note_render_surface_recovery_signal();

        // ft-kciew: notify the quad-buffer policy of the gesture
        // boundary so the underlying GPU buffer (continuation
        // bead) won't reallocate during a drag. Idempotent on an
        // active gesture; the level-transition tracking lives on
        // TermWindow.
        if live_resizing {
            self.begin_quad_resize_gesture();
        } else {
            self.end_quad_resize_gesture();
        }
        let last_state = self.window_state;
        self.window_state = window_state;
        if last_state != self.window_state {
            self.load_os_parameters();
            // Queue the maximize/fullscreen bits for this window's workspace
            // so a relaunch can restore them. The persistence coordinator only
            // takes a short in-memory lock here; bounded coalescing, validation,
            // and crash-consistent disk I/O run on its dedicated worker.
            if let Some(mux) = Mux::try_get() {
                if let Some(mux_window) = mux.get_window(self.mux_window_id) {
                    crate::window_state_persist::save_for_workspace(
                        mux_window.get_workspace(),
                        self.window_state,
                    );
                }
            }
        }

        if let Some(webgpu) = self.webgpu.as_mut() {
            webgpu.resize(dimensions);
        }

        // `apply_dimensions` is the single resize invalidation authority. It
        // advances the quad/damage generations and marks the pane bitmaps once
        // after the final dimensions have been selected. Keeping that work out
        // of this outer dispatcher avoids doing the same whole-window dirty
        // walk twice for every high-frequency live-resize event.
        //
        // For simple, user-interactive resizes where the dpi doesn't change,
        // skip our scaling recalculation.
        if live_resizing && self.dimensions.dpi == dimensions.dpi {
            self.apply_dimensions(&dimensions, None, window);
        } else {
            self.scaling_changed(dimensions, self.fonts.get_font_scale(), window);
        }
        // Generated gradients/colors depend on the current pixel geometry.
        // The coordinator coalesces resize storms to one newest pending job,
        // cancels obsolete work, and never performs decode/raster work here.
        self.schedule_background_reload();
        if let Some(modal) = self.get_modal() {
            modal.reconfigure(self);
        }
        self.emit_window_event("window-resized", None);
    }

    pub fn apply_pending_scale_changes(&mut self) {
        while self.resizes_pending == 0 {
            match self.pending_scale_changes.pop_front() {
                Some(ScaleChange::Relative(change)) => {
                    if let Some(window) = self.window.as_ref().map(|w| w.clone()) {
                        self.adjust_font_scale(self.fonts.get_font_scale() * change, &window);
                    }
                }
                Some(ScaleChange::Absolute(change)) => {
                    if let Some(window) = self.window.as_ref().map(|w| w.clone()) {
                        self.adjust_font_scale(change, &window);
                    }
                }
                None => break,
            }
        }
    }

    pub fn apply_scale_change(&mut self, dimensions: &Dimensions, font_scale: f64) {
        let config = &self.config;
        let font_size = config.font_size * font_scale;
        let theoretical_height = font_size * dimensions.dpi as f64 / 72.0;

        if theoretical_height < 2.0 {
            log::warn!(
                "refusing to go to an unreasonably small font scale {:?}
                       font_scale={} would yield font_height {}",
                dimensions,
                font_scale,
                theoretical_height
            );
            return;
        }

        let (prior_font, prior_dpi) = self.fonts.change_scaling(font_scale, dimensions.dpi);
        match RenderMetrics::new(&self.fonts) {
            Ok(metrics) => {
                self.render_metrics = metrics;
                // Invalidate the shape cache: shaped runs cache per-glyph
                // `x_advance` (kerning) + cluster metrics computed against the
                // PRIOR `render_metrics` cell size. Without bumping the
                // generation, the next paint reuses those stale advances and
                // draws old-scale kerning/leading into the new cells — the
                // "absurd kerning/leading on Cmd +/-" bug. Config changes
                // already bump this (mod.rs); scale changes must too. Bump only
                // in the Ok arm: on metric failure we roll back below, so the
                // old cache stays consistent with the old metrics.
                // A scale change alters per-glyph PIXEL advances, so the cached
                // shaping is genuinely stale — not merely its atlas sprites.
                // Clear the shape cache so the next paint re-SHAPES at the new
                // scale. This is also load-bearing for the shape-cache/atlas
                // decoupling (render/mod.rs `recreate_texture_atlas`): every
                // shaping-input change (font/config/scale) must clear, so the
                // only entries that survive a `shape_generation` bump are those
                // that saw an *atlas* rebuild — for which re-resolving sprites
                // (without re-shaping) is correct.
                self.advance_shaping_input_generation();
                // ft-uroqc: warm-rasterize the common ASCII/Latin glyph set at
                // the NEW CellMetricKey so the first paint after a scale change
                // finds them already in the atlas instead of synchronously
                // rasterizing mid-paint. Routes through the same `cached_glyph`
                // path the paint uses (keyed by the same BorrowedGlyphKey,
                // including `metric: CellMetricKey`), so the cached glyphs are
                // byte-identical to lazy rasterization — only WHEN they
                // rasterize changes. Bounded by SCALE_CHANGE_GLYPH_WARMUP_BUDGET;
                // fail-safe (logs + returns stats if the font cannot resolve);
                // no-op when render_state is absent (headless / pre-init).
                if let Some(render_state) = self.render_state.as_ref() {
                    let mut glyph_cache = render_state.glyph_cache.borrow_mut();
                    // ft-b4vw9: drop now-unreachable old-CellMetricKey glyphs/
                    // blocks before warming the new scale, so the CPU-side caches
                    // stay bounded across repeated scale changes. Evicted entries
                    // are keyed by a different CellMetricKey and can't be hit at
                    // the new scale, so rendering is unchanged.
                    let _evicted =
                        glyph_cache.evict_stale_cell_metrics((&self.render_metrics).into());
                    let _warm = glyph_cache.warm_up_default_glyphs(
                        &self.render_metrics,
                        SCALE_CHANGE_GLYPH_WARMUP_BUDGET,
                    );
                }
            }
            Err(err) => {
                log::error!(
                    "{:#} while attempting to scale font to {} with {:?}",
                    err,
                    font_scale,
                    dimensions
                );
                // Restore prior scaling factors
                self.fonts.change_scaling(prior_font, prior_dpi);
            }
        }

        // ft-c9arc: drop the wholesale `recreate_texture_atlas(None)`
        // call (ghostty pattern). The atlas is now versioned —
        // GlyphKey carries CellMetricKey so old-scale glyphs become
        // unreachable from new-scale lookups, and the next allocate
        // bumps the atlas version. Glyphs at the new scale are
        // rasterized lazily on demand. The atlas's version cursor
        // lets the per-frame renderer detect drift; an
        // OutOfTextureSpace return from a future allocate routes
        // through `Atlas::grow` instead of a wholesale rebuild.
        //
        // The integration is observable: `window.atlas.rebuilds.total`
        // should stay flat across pure scale changes once
        // ft-mpc9b.7's RQ-S10 atlas-stability bench enforces it.
        self.invalidate_fancy_tab_bar();
        self.invalidate_modal();
    }

    pub fn apply_dimensions(
        &mut self,
        dimensions: &Dimensions,
        mut scale_changed_cells: Option<RowsAndCols>,
        window: &Window,
    ) {
        log::trace!(
            "apply_dimensions {:?} scale_changed_cells {:?}. window_state {:?}",
            dimensions,
            scale_changed_cells,
            self.window_state
        );
        let saved_dims = self.dimensions;
        self.dimensions = *dimensions;
        self.advance_quad_generation();
        self.mark_all_panes_dirty_with_source(
            frankenterm_core::dirty_line_telemetry::DirtyEventSource::Resize,
        );

        if scale_changed_cells.is_some() && !self.window_state.can_resize() {
            log::warn!(
                "cannot resize window to match {:?} because window_state is {:?}",
                scale_changed_cells,
                self.window_state
            );
            scale_changed_cells.take();
        }

        // Technically speaking, we should compute the rows and cols
        // from the new dimensions and apply those to the tabs, and
        // then for the scaling changed case, try to re-apply the
        // original rows and cols, but if we do that we end up
        // double resizing the tabs, so we speculatively apply the
        // final size, which in that case should result in a NOP
        // change to the tab size.

        let config = &self.config;

        let tab_bar_height = if self.show_tab_bar {
            self.tab_bar_pixel_height().unwrap_or(0.)
        } else {
            0.
        };

        let border = self.get_os_border();

        let (size, dims, ri_calc) = if let Some(cell_dims) = scale_changed_cells {
            // Scaling preserves existing terminal dimensions, yielding a new
            // overall set of window dimensions
            let size = TerminalSize {
                rows: cell_dims.rows,
                cols: cell_dims.cols,
                pixel_height: cell_dims.rows * self.render_metrics.cell_size.height as usize,
                pixel_width: cell_dims.cols * self.render_metrics.cell_size.width as usize,
                dpi: dimensions.dpi as u32,
            };

            let rows = size.rows;
            let cols = size.cols;

            let h_context = DimensionContext {
                dpi: dimensions.dpi as f32,
                pixel_max: size.pixel_width as f32,
                pixel_cell: self.render_metrics.cell_size.width as f32,
            };
            let v_context = DimensionContext {
                dpi: dimensions.dpi as f32,
                pixel_max: size.pixel_height as f32,
                pixel_cell: self.render_metrics.cell_size.height as f32,
            };
            let padding_left = config.window_padding.left.evaluate_as_pixels(h_context) as usize;
            let padding_top = config.window_padding.top.evaluate_as_pixels(v_context) as usize;
            let padding_bottom =
                config.window_padding.bottom.evaluate_as_pixels(v_context) as usize;
            let padding_right = effective_right_padding(&config, h_context);

            let pixel_height = (rows * self.render_metrics.cell_size.height as usize)
                + (padding_top + padding_bottom)
                + (border.top + border.bottom).get() as usize
                + tab_bar_height as usize;

            let pixel_width = (cols * self.render_metrics.cell_size.width as usize)
                + (padding_left + padding_right)
                + (border.left + border.right).get() as usize;

            let dims = Dimensions {
                pixel_width: pixel_width as usize,
                pixel_height: pixel_height as usize,
                dpi: dimensions.dpi,
            };

            let ri_calc = ResizeIncrementCalculator {
                x: self.render_metrics.cell_size.width as u16,
                y: self.render_metrics.cell_size.height as u16,
                padding_left,
                padding_top,
                padding_right,
                padding_bottom,
                border,
                tab_bar_height: tab_bar_height as usize,
            };

            (size, dims, ri_calc)
        } else {
            // Resize of the window dimensions may result in changed terminal dimensions

            let h_context = DimensionContext {
                dpi: dimensions.dpi as f32,
                pixel_max: self.terminal_size.pixel_width as f32,
                pixel_cell: self.render_metrics.cell_size.width as f32,
            };
            let v_context = DimensionContext {
                dpi: dimensions.dpi as f32,
                pixel_max: self.terminal_size.pixel_height as f32,
                pixel_cell: self.render_metrics.cell_size.height as f32,
            };
            let padding_left = config.window_padding.left.evaluate_as_pixels(h_context) as usize;
            let padding_top = config.window_padding.top.evaluate_as_pixels(v_context) as usize;
            let padding_bottom =
                config.window_padding.bottom.evaluate_as_pixels(v_context) as usize;
            let padding_right = effective_right_padding(&config, h_context);

            let avail_width = dimensions.pixel_width.saturating_sub(
                (padding_left + padding_right) as usize
                    + (border.left + border.right).get() as usize,
            );
            let avail_height = dimensions
                .pixel_height
                .saturating_sub(
                    (padding_top + padding_bottom) as usize
                        + (border.top + border.bottom).get() as usize,
                )
                .saturating_sub(tab_bar_height as usize);

            let cell_h = (self.render_metrics.cell_size.height as usize).max(1);
            let cell_w = (self.render_metrics.cell_size.width as usize).max(1);
            let rows = avail_height / cell_h;
            let cols = avail_width / cell_w;

            let size = TerminalSize {
                rows,
                cols,
                // Take care to use the exact pixel dimensions of the cells, rather
                // than the available space, so that apps that are sensitive to
                // the pixels-per-cell have consistent values at a given font size.
                // https://github.com/wezterm/wezterm/issues/535
                pixel_height: rows * self.render_metrics.cell_size.height as usize,
                pixel_width: cols * self.render_metrics.cell_size.width as usize,
                dpi: dimensions.dpi as u32,
            };

            let ri_calc = ResizeIncrementCalculator {
                x: self.render_metrics.cell_size.width as u16,
                y: self.render_metrics.cell_size.height as u16,
                padding_left,
                padding_top,
                padding_right,
                padding_bottom,
                border,
                tab_bar_height: tab_bar_height as usize,
            };

            (size, *dimensions, ri_calc)
        };

        log::trace!("apply_dimensions computed size {:?}, dims {:?}", size, dims);

        // ft-t9l62: coalesce reshape storms. During a live-resize drag macOS
        // fires `resize()` at 60-120Hz, but a cell is ~8x16px, so consecutive
        // events usually map to the SAME rows x cols (`avail / cell_size`,
        // integer division). `Tab::resize` is NOT a no-op on an unchanged size
        // — it tree-walks the width+height constraints and reshapes every pane
        // (mux/tab.rs:3007) — so re-running it for every sub-cell drag pixel is
        // pure waste. Skip the grid reshape (and the overlay reshape, which is
        // also sized to the grid) whenever the freshly recomputed `size` is
        // byte-identical to what the tabs were last sized to: resizing a
        // terminal to its current dimensions is idempotent, so the grid is
        // identical either way. Scale/DPI changes alter `cell_size`, hence
        // `size.pixel_*`, so they never compare equal here and are never
        // skipped. The window pixel dimensions (`self.dimensions`) were already
        // updated above and the panes marked dirty, so letterboxing + re-render
        // at the new pixel size still happen — only the redundant reflow is
        // elided.
        let grid_size_changed = size != self.terminal_size;
        self.terminal_size = size;

        if grid_size_changed {
            if let Some(mux) = Mux::try_get() {
                if let Some(window) = mux.get_window(self.mux_window_id) {
                    for tab in window.iter() {
                        tab.resize(size);
                    }
                }
            }
            self.resize_overlays();
        }
        self.invalidate_fancy_tab_bar();
        self.update_title();

        window.set_resize_increments(if self.config.use_resize_increments {
            ri_calc.into()
        } else {
            ResizeIncrement::disabled()
        });

        // Queue up a speculative resize in order to preserve the number of rows+cols
        if let Some(cell_dims) = scale_changed_cells {
            // If we don't think the dimensions have changed, don't request
            // the window to change.  This seems to help on Wayland where
            // we won't know what size the compositor thinks we should have
            // when we're first opened, until after it sends us a configure event.
            // If we send this too early, it will trump that configure event
            // and we'll end up with weirdness where our window renders in the
            // middle of a larger region that the compositor thinks we live in.
            // Wayland is weird!
            if saved_dims != dims {
                log::trace!(
                    "scale changed so resize from {:?} to {:?} {:?} (event called with {:?})",
                    saved_dims,
                    dims,
                    cell_dims,
                    dimensions
                );
                // Stash this size pre-emptively. Without this, on Windows,
                // when the font scaling is changed we can end up not seeing
                // these dimensions and the scaling_changed logic ends up
                // comparing two dimensions that have the same DPI and recomputing
                // an adjusted terminal size.
                // eg: rather than a simple old-dpi -> new dpi transition, we'd
                // see old-dpi -> new dpi, call set_inner_size, then see a
                // new-dpi -> new-dpi adjustment with a slightly different
                // pixel geometry which is considered to be a user-driven resize.
                // Stashing the dimensions here avoids that misconception.
                self.dimensions = dims;
                self.set_inner_size(window, dims.pixel_width, dims.pixel_height);
            }
        }
    }

    pub fn current_cell_dimensions(&self) -> RowsAndCols {
        RowsAndCols {
            rows: self.terminal_size.rows as usize,
            cols: self.terminal_size.cols as usize,
        }
    }

    #[allow(clippy::float_cmp)]
    pub fn scaling_changed(&mut self, dimensions: Dimensions, font_scale: f64, window: &Window) {
        fn dpi_adjusted(n: usize, dpi: usize) -> f32 {
            n as f32 / dpi as f32
        }

        /// On Windows, scaling changes may adjust the pixel geometry by a few pixels,
        /// so this function checks if we're in a close-enough ballpark.
        fn close_enough(a: f32, b: f32) -> bool {
            let diff = (a - b).abs();
            diff < 10.
        }

        // Distinguish between eg: dpi being detected as double the initial dpi (where
        // the pixel dimensions don't change), and the dpi change being detected, but
        // where the window manager also decides to tile/resize the window.
        // In the latter case, we don't want to preserve the terminal rows/cols.
        let simple_dpi_change = dimensions.dpi != self.dimensions.dpi
            && ((close_enough(
                dpi_adjusted(dimensions.pixel_height, dimensions.dpi),
                dpi_adjusted(self.dimensions.pixel_height, self.dimensions.dpi),
            ) && close_enough(
                dpi_adjusted(dimensions.pixel_width, dimensions.dpi),
                dpi_adjusted(self.dimensions.pixel_width, self.dimensions.dpi),
            )) || (close_enough(
                dimensions.pixel_width as f32,
                self.dimensions.pixel_width as f32,
            ) && close_enough(
                dimensions.pixel_height as f32,
                self.dimensions.pixel_height as f32,
            )));

        if simple_dpi_change && cfg!(target_os = "macos") {
            // Spooky action at a distance: on macOS, NSWindow::isZoomed can falsely
            // return YES in situations such as the current screen changing.
            // That causes window_state to believe that we are MAXIMIZED.
            // We cannot easily detect that in the window layer, but at this
            // layer, if we realize that the dpi was the only thing that changed
            // then remove the MAXIMIZED state so that the can_resize check
            // in adjust_font_scale will not block us from adapting to the new
            // DPI. This is gross and it would be better handled at the macOS
            // layer.
            // <https://github.com/wezterm/wezterm/issues/3503>
            self.window_state -= WindowState::MAXIMIZED;
        }

        let dpi_changed = dimensions.dpi != self.dimensions.dpi;
        let font_scale_changed = font_scale != self.fonts.get_font_scale();
        let scale_changed = dpi_changed || font_scale_changed;

        log::trace!(
            "dpi_changed={}, font_scale_changed={} scale_changed={} simple_dpi_change={}",
            dpi_changed,
            font_scale_changed,
            scale_changed,
            simple_dpi_change
        );

        let cell_dims = self.current_cell_dimensions();

        if scale_changed {
            self.apply_scale_change(&dimensions, font_scale);
        }

        let scale_changed_cells = if font_scale_changed || simple_dpi_change {
            Some(cell_dims)
        } else {
            None
        };

        log::trace!(
            "scaling_changed, follow with applying dimensions. scale_changed_cells={:?}",
            scale_changed_cells
        );
        self.apply_dimensions(&dimensions, scale_changed_cells, window);
    }

    /// Used for applying font size changes only; this takes into account
    /// the `adjust_window_size_when_changing_font_size` configuration and
    /// revises the scaling/resize change accordingly
    pub fn adjust_font_scale(&mut self, font_scale: f64, window: &Window) {
        let adjust_window_size_when_changing_font_size =
            match self.config.adjust_window_size_when_changing_font_size {
                Some(value) => value,
                None => {
                    let is_tiling = self
                        .config
                        .tiling_desktop_environments
                        .iter()
                        .any(|item| item.as_str() == self.connection_name.as_str());
                    !is_tiling
                }
            };

        if self.window_state.can_resize() && adjust_window_size_when_changing_font_size {
            self.scaling_changed(self.dimensions, font_scale, window);
        } else {
            let dimensions = self.dimensions;
            // Compute new font metrics
            self.apply_scale_change(&dimensions, font_scale);
            // Now revise the pty size to fit the window
            self.apply_dimensions(&dimensions, None, window);
        }
        self.schedule_background_reload();
    }

    pub fn decrease_font_size(&mut self) {
        self.pending_scale_changes
            .push_back(ScaleChange::Relative(1.0 / 1.1));
        self.apply_pending_scale_changes();
    }

    pub fn increase_font_size(&mut self) {
        self.pending_scale_changes
            .push_back(ScaleChange::Relative(1.1));
        self.apply_pending_scale_changes();
    }

    pub fn reset_font_size(&mut self) {
        self.pending_scale_changes
            .push_back(ScaleChange::Absolute(1.0));
        self.apply_pending_scale_changes();
    }

    pub fn set_window_size(&mut self, size: TerminalSize, window: &Window) -> anyhow::Result<()> {
        let config = &self.config;
        let fontconfig = Rc::new(FontConfiguration::new(
            Some(config.clone()),
            self.dimensions.dpi,
        )?);
        let render_metrics = RenderMetrics::new(&fontconfig)?;

        let terminal_size = TerminalSize {
            rows: size.rows,
            cols: size.cols,
            pixel_width: (render_metrics.cell_size.width as usize * size.cols),
            pixel_height: (render_metrics.cell_size.height as usize * size.rows),
            dpi: size.dpi,
        };

        let show_tab_bar = config.enable_tab_bar && !config.hide_tab_bar_if_only_one_tab;
        let tab_bar_height = if show_tab_bar {
            self.tab_bar_pixel_height()? as usize
        } else {
            0
        };

        let h_context = DimensionContext {
            dpi: self.dimensions.dpi as f32,
            pixel_max: self.dimensions.pixel_width as f32,
            pixel_cell: render_metrics.cell_size.width as f32,
        };
        let v_context = DimensionContext {
            dpi: self.dimensions.dpi as f32,
            pixel_max: self.dimensions.pixel_height as f32,
            pixel_cell: render_metrics.cell_size.height as f32,
        };
        let padding_left = config.window_padding.left.evaluate_as_pixels(h_context) as usize;
        let padding_top = config.window_padding.top.evaluate_as_pixels(v_context) as usize;
        let padding_bottom = config.window_padding.bottom.evaluate_as_pixels(v_context) as usize;

        let dimensions = Dimensions {
            pixel_width: ((terminal_size.cols as usize * render_metrics.cell_size.width as usize)
                + padding_left
                + effective_right_padding(&config, h_context)),
            pixel_height: ((terminal_size.rows as usize * render_metrics.cell_size.height as usize)
                + padding_top
                + padding_bottom) as usize
                + tab_bar_height,
            dpi: self.dimensions.dpi,
        };

        self.apply_scale_change(&dimensions, 1.0);
        self.apply_dimensions(
            &dimensions,
            Some(RowsAndCols {
                rows: size.rows as usize,
                cols: size.cols as usize,
            }),
            window,
        );
        Ok(())
    }

    pub fn reset_font_and_window_size(&mut self, window: &Window) -> anyhow::Result<()> {
        let size = self.config.initial_size(
            self.dimensions.dpi as u32,
            Some(crate::cell_pixel_dims(
                &self.config,
                self.dimensions.dpi as f64,
            )?),
        );
        self.set_window_size(size, window)
    }

    pub fn effective_right_padding(&self, config: &ConfigHandle) -> usize {
        effective_right_padding(
            config,
            DimensionContext {
                pixel_cell: self.render_metrics.cell_size.width as f32,
                dpi: self.dimensions.dpi as f32,
                pixel_max: self.dimensions.pixel_width as f32,
            },
        )
    }
}

/// Computes the effective padding for the RHS.
/// This is needed because the default is 0, but if the user has
/// enabled the scroll bar then they will expect it to have a reasonable
/// size unless they've specified differently.
pub fn effective_right_padding(config: &ConfigHandle, context: DimensionContext) -> usize {
    if config.enable_scroll_bar && config.window_padding.right.is_zero() {
        context.pixel_cell as usize
    } else {
        config.window_padding.right.evaluate_as_pixels(context) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::{
        clear_generation_keyed_line_shape_cache,
        next_cache_generation,
    };
    use crate::termwindow::render::{LineToEleShapeCacheKey, LineToElementShapeItem};
    use config::ConfigHandle;
    use lfucache::LfuCache;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn line_shape_key(shape_generation: usize, index: usize) -> LineToEleShapeCacheKey {
        LineToEleShapeCacheKey {
            // Every supported Rust target has a pointer width no greater than
            // u128, so this widening cast is lossless even at usize::MAX.
            shape_hash: (index as u128).to_le_bytes(),
            composing: None,
            shape_generation,
        }
    }

    fn empty_line_shape_item() -> LineToElementShapeItem {
        LineToElementShapeItem {
            expires: None,
            shaped: Rc::new(Vec::new()),
            current_highlight: None,
            invalidate_on_hover_change: false,
        }
    }

    #[test]
    fn line_shape_key_widens_the_full_usize_domain_without_truncation() {
        assert_eq!(
            line_shape_key(0, usize::MAX).shape_hash,
            (usize::MAX as u128).to_le_bytes(),
        );
    }

    #[test]
    fn scale_change_advances_shape_generation_for_kerning_cache_invalidation() {
        assert_eq!(
            next_cache_generation(0),
            super::CacheGenerationAdvance {
                generation: 1,
                recycled_epoch: false,
            }
        );
        assert_eq!(
            next_cache_generation(41),
            super::CacheGenerationAdvance {
                generation: 42,
                recycled_epoch: false,
            }
        );
    }

    #[test]
    fn shape_generation_recycles_only_with_an_explicit_epoch_fence() {
        assert_eq!(
            next_cache_generation(usize::MAX - 1),
            super::CacheGenerationAdvance {
                generation: usize::MAX,
                recycled_epoch: false,
            }
        );
        assert_eq!(
            next_cache_generation(usize::MAX),
            super::CacheGenerationAdvance {
                generation: 0,
                recycled_epoch: true,
            },
            "generation zero can be reused only after callers retire the prior epoch"
        );
    }

    #[test]
    fn recycled_shape_generation_cannot_reuse_a_stale_zero_generation_line() {
        let config = ConfigHandle::default_config();
        let cache: RefCell<LfuCache<LineToEleShapeCacheKey, LineToElementShapeItem>> =
            RefCell::new(LfuCache::new(
                "test.line_to_ele_shape_cache.hit",
                "test.line_to_ele_shape_cache.miss",
                |config| config.line_to_ele_shape_cache_size,
                &config,
            ));
        let stale_key = line_shape_key(0, 7);
        cache
            .borrow_mut()
            .put(stale_key.clone(), empty_line_shape_item());

        let advance = next_cache_generation(usize::MAX);
        assert!(advance.recycled_epoch);
        clear_generation_keyed_line_shape_cache(&cache);

        assert!(cache.borrow_mut().get(&stale_key).is_none());
        let current_key = line_shape_key(advance.generation, 7);
        cache
            .borrow_mut()
            .put(current_key.clone(), empty_line_shape_item());
        assert!(cache.borrow_mut().get(&current_key).is_some());
    }

    #[test]
    fn shaping_input_change_clears_hot_stale_line_shape_generation() {
        let config = ConfigHandle::default_config();
        let capacity = config.line_to_ele_shape_cache_size;
        assert!(capacity > 1, "test requires a multi-entry line-shape cache");
        let cache: RefCell<LfuCache<LineToEleShapeCacheKey, LineToElementShapeItem>> =
            RefCell::new(LfuCache::new(
                "test.line_to_ele_shape_cache.hit",
                "test.line_to_ele_shape_cache.miss",
                |config| config.line_to_ele_shape_cache_size,
                &config,
            ));
        let old_generation = 41;

        for index in 0..capacity {
            cache
                .borrow_mut()
                .put(line_shape_key(old_generation, index), empty_line_shape_item());
        }
        // Make every stale entry hot. Without a generation-boundary clear,
        // these unreachable entries would beat fresh frequency-zero entries
        // in LFU eviction and repeatedly evict newly shaped lines.
        for _ in 0..4 {
            for index in 0..capacity {
                assert!(
                    cache
                        .borrow_mut()
                        .get(&line_shape_key(old_generation, index))
                        .is_some()
                );
            }
        }

        clear_generation_keyed_line_shape_cache(&cache);
        assert!(cache.borrow().is_empty());

        let new_generation = next_cache_generation(old_generation).generation;
        for index in 0..capacity {
            cache
                .borrow_mut()
                .put(line_shape_key(new_generation, index), empty_line_shape_item());
        }
        assert_eq!(cache.borrow().len(), capacity);
        for index in 0..capacity {
            assert!(
                cache
                    .borrow_mut()
                    .get(&line_shape_key(new_generation, index))
                    .is_some(),
                "new-generation line shape {index} was crowded out by stale entries"
            );
        }
    }
}
