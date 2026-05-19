use crate::termwindow::frame_budget::{OpKind, OpPriority};
use crate::termwindow::{RenderFrame, TermWindowNotif};
use ::window::WindowOps;
use ::window::bitmaps::atlas::OutOfTextureSpace;
use anyhow::Context;
use frankenterm_core::frame_budget_a11y_gate::ReduceMotionState;
use frankenterm_font::ClearShapeCache;
use promise::spawn::sleep;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowImage {
    Yes,
    Scale(usize),
    No,
}

impl crate::TermWindow {
    pub fn paint_impl(&mut self, frame: &mut RenderFrame) {
        self.num_frames += 1;
        // Per ft-d6nrd / ft-96uy6: tick the per-frame budget allocator
        // at the top of paint, then reconcile any carry-over cosmetic
        // work that drains before fresh render operations are gated.
        let _frame_start = self.frame_budget_begin_frame();
        let _drained_carryover = self.frame_budget_drain_deferred_cosmetic();
        let frame_reduce_motion = self.frame_budget_reduce_motion_state();
        // If nothing on screen needs animating, then we can avoid
        // invalidating as frequently
        *self.has_animation.borrow_mut() = None;
        // Start with the assumption that we should allow images to render
        self.allow_images = AllowImage::Yes;

        let start = Instant::now();

        {
            let diff = start.duration_since(self.last_fps_check_time);
            if diff > Duration::from_secs(1) {
                let seconds = diff.as_secs_f32();
                self.fps = self.num_frames as f32 / seconds;
                self.num_frames = 0;
                self.last_fps_check_time = start;
            }
        }

        'pass: for pass in 0.. {
            let _dirty_quad_budget = self.frame_budget_should_run_render_op_with_reduce_motion(
                OpKind::DirtyQuadRebuild,
                OpPriority::Required,
                frame_reduce_motion,
            );
            match self.paint_pass(frame_reduce_motion) {
                Ok(_) => match self.render_state.as_mut() {
                    Some(render_state) => {
                        // NOTE: the previous revision deferred quad-buffer
                        // *growth* while a resize gesture was active. That was
                        // a perf optimization to avoid per-resize-event GPU
                        // buffer reallocations, but it was incorrect: the
                        // geometry pass had already written `next_quad`
                        // quads to a buffer whose capacity is now too small,
                        // so the subsequent draw call sliced out-of-range
                        // (panic before the unwrap→let-else fix at
                        // draw.rs:264, transparent window after it). Always
                        // grow on demand. Elastic-buffer *shrinking* during a
                        // gesture is still deferred via
                        // ElasticBuffer::try_shrink_if_idle — that's a
                        // separate concern handled in elastic_buffer.rs.

                        match render_state.allocate_more_quads() {
                            Ok(change) => {
                                let snapshot = render_state.quad_allocation_snapshot();
                                self.quad_buffer_policy.record_live_allocation(
                                    snapshot.used,
                                    snapshot.capacity,
                                    change.reallocation_count,
                                );
                                if !change.allocated {
                                    break 'pass;
                                }
                                self.invalidate_fancy_tab_bar();
                                self.invalidate_modal();
                            }
                            Err(err) => {
                                log::error!("{:#}", err);
                                break 'pass;
                            }
                        }
                    }
                    None => {
                        log::error!("paint_pass succeeded without initialized render state");
                        break 'pass;
                    }
                },
                Err(err) => {
                    if let Some(&OutOfTextureSpace {
                        size: Some(size),
                        current_size,
                    }) = err.root_cause().downcast_ref::<OutOfTextureSpace>()
                    {
                        let result = if pass == 0 {
                            // Let's try clearing out the atlas and trying again
                            // self.clear_texture_atlas()
                            log::trace!("recreate_texture_atlas");
                            self.recreate_texture_atlas(Some(current_size))
                        } else {
                            log::trace!("grow texture atlas to {}", size);
                            self.recreate_texture_atlas(Some(size))
                        };
                        self.invalidate_fancy_tab_bar();
                        self.invalidate_modal();

                        if let Err(err) = result {
                            self.allow_images = match self.allow_images {
                                AllowImage::Yes => AllowImage::Scale(2),
                                AllowImage::Scale(2) => AllowImage::Scale(4),
                                AllowImage::Scale(4) => AllowImage::Scale(8),
                                AllowImage::Scale(8) => AllowImage::No,
                                AllowImage::No | _ => {
                                    log::error!(
                                        "Failed to {} texture: {}",
                                        if pass == 0 { "clear" } else { "resize" },
                                        err
                                    );
                                    break 'pass;
                                }
                            };

                            log::info!(
                                "Not enough texture space ({:#}); \
                                     will retry render with {:?}",
                                err,
                                self.allow_images,
                            );
                        }
                    } else if err.root_cause().downcast_ref::<ClearShapeCache>().is_some() {
                        self.invalidate_fancy_tab_bar();
                        self.invalidate_modal();
                        self.shape_generation += 1;
                        self.shape_cache.borrow_mut().clear();
                        self.line_to_ele_shape_cache.borrow_mut().clear();
                    } else {
                        log::error!("paint_pass failed: {:#}", err);
                        break 'pass;
                    }
                }
            }
        }
        log::debug!("paint_impl before call_draw elapsed={:?}", start.elapsed());

        self.call_draw(frame).ok();
        // Per ft-jvj78 (cont of ft-5ykn9): consult the substrate's
        // `should_clear_at_frame_end` predicate and reset every
        // per-pane dirty bitmap when allowed. Coarse whole-screen
        // invalidations (font/theme/resize/focus) leave the marks
        // across the boundary so the next paint still observes them.
        self.clear_dirty_lines_after_frame();

        // Scheduling the next animation frame is cosmetic: reduce-motion
        // skips it entirely, and frame pressure defers it into the
        // outstanding-work path that forces a follow-up paint.
        let animation_due = *self.has_animation.borrow();
        let should_schedule_animation = animation_due.is_some()
            && self.frame_budget_should_run_render_op_with_reduce_motion(
                OpKind::Animations,
                OpPriority::Cosmetic,
                frame_reduce_motion,
            );
        let _bulk_drained = self.frame_budget_try_bulk_drain_cosmetic();
        // Per ft-d6nrd / ft-96uy6: close out the allocator after
        // draining cosmetic carry-over so `frame_budget_telemetry()`
        // reflects real render decisions from this frame.
        let _frame_end = self.frame_budget_end_frame();
        let should_force_frame_budget_paint = self.frame_budget_should_force_paint();
        self.last_frame_duration = start.elapsed();
        log::debug!(
            "paint_impl elapsed={:?}, fps={}",
            self.last_frame_duration,
            self.fps
        );
        metrics::histogram!("gui.paint.impl").record(self.last_frame_duration);
        metrics::histogram!("gui.paint.impl.rate").record(1.);

        // If self.has_animation is some, then the last render detected
        // image attachments with multiple frames, so we also need to
        // invalidate the viewport when the next frame is due
        if should_force_frame_budget_paint {
            if let Some(window) = self.window.clone() {
                window.invalidate();
            }
        }

        if self.focused.is_some() && should_schedule_animation {
            if let Some(next_due) = animation_due {
                let prior = self.scheduled_animation.borrow_mut().take();
                match prior {
                    Some(prior) if prior <= next_due => {
                        // Already due before that time
                    }
                    _ => {
                        self.scheduled_animation.borrow_mut().replace(next_due);
                        if let Some(window) = self.window.clone() {
                            promise::spawn::spawn(async move {
                                sleep(next_due.saturating_duration_since(Instant::now())).await;
                                let win = window.clone();
                                window.notify(TermWindowNotif::Apply(Box::new(move |tw| {
                                    tw.scheduled_animation.borrow_mut().take();
                                    win.invalidate();
                                })));
                            })
                            .detach();
                        } else {
                            log::debug!(
                                "Skipping scheduled animation invalidation for detached window"
                            );
                            self.scheduled_animation.borrow_mut().take();
                        }
                    }
                }
            }
        }
    }

    pub fn paint_modal(&mut self) -> anyhow::Result<()> {
        if let Some(modal) = self.get_modal() {
            for computed in modal.computed_element(self)?.iter() {
                let mut ui_items = computed.ui_items();

                let gl_state = self
                    .render_state
                    .as_ref()
                    .context("render state is not initialized")?;
                self.render_element(&computed, gl_state, None)?;

                self.ui_items.append(&mut ui_items);
            }
        }

        Ok(())
    }

    pub fn paint_pass(&mut self, frame_reduce_motion: ReduceMotionState) -> anyhow::Result<()> {
        {
            let gl_state = self
                .render_state
                .as_ref()
                .context("render state is not initialized")?;
            for layer in gl_state.layers.borrow().iter() {
                layer.clear_quad_allocation();
            }
            // ft-mpc9b.1.1: snapshot the atlas version cursor at the
            // start of every paint pass so subsequent allocates inside
            // the pass (newly-rasterized glyphs) bump the atlas above
            // the cursor and per-frame state can detect drift via
            // `glyph_cache.sprite_needs_resync(version)`. A pure
            // window-resize that does NOT allocate keeps the version
            // unchanged — the renderer can short-circuit the atlas-
            // sync work entirely (the headline correctness rule).
            gl_state.glyph_cache.borrow_mut().snapshot_atlas_version();
        }

        // Clear out UI item positions; we'll rebuild these as we render
        self.ui_items.clear();

        let panes = self.get_panes_to_render();
        let focused = self.focused.is_some();
        let window_is_transparent =
            !self.window_background.is_empty() || self.config.window_background_opacity != 1.0;

        let start = Instant::now();
        let gl_state = self
            .render_state
            .as_ref()
            .context("render state is not initialized")?;
        let layer = gl_state
            .layer_for_zindex(0)
            .context("layer_for_zindex(0)")?;
        let mut layers = layer.quad_allocator();
        log::trace!("quad map elapsed {:?}", start.elapsed());
        metrics::histogram!("quad.map").record(start.elapsed());

        let mut paint_terminal_background = false;

        // Render the full window background
        match (self.window_background.is_empty(), self.allow_images) {
            (false, AllowImage::Yes | AllowImage::Scale(_)) => {
                let bg_color = self.palette().background.to_linear();

                let top = panes
                    .iter()
                    .find(|p| p.is_active)
                    .map(|p| match self.get_viewport(p.pane.pane_id()) {
                        Some(top) => top,
                        None => p.pane.get_dimensions().physical_top,
                    })
                    .unwrap_or(0);

                let loaded_any = self
                    .render_backgrounds(bg_color, top)
                    .context("render_backgrounds")?;

                if !loaded_any {
                    // Either there was a problem loading the background(s)
                    // or they haven't finished loading yet.
                    // Use the regular terminal background until that changes.
                    paint_terminal_background = true;
                }
            }
            _ if window_is_transparent => {
                // Avoid doubling up the background color: the panes
                // will render out through the padding so there
                // should be no gaps that need filling in
            }
            _ => {
                paint_terminal_background = true;
            }
        }

        if paint_terminal_background {
            // Regular window background color
            let background = if panes.len() == 1 {
                // If we're the only pane, use the pane's palette
                // to draw the padding background
                panes[0].pane.palette().background
            } else {
                self.palette().background
            }
            .to_linear()
            .mul_alpha(self.config.window_background_opacity);

            self.filled_rectangle(
                &mut layers,
                0,
                euclid::rect(
                    0.,
                    0.,
                    self.dimensions.pixel_width as f32,
                    self.dimensions.pixel_height as f32,
                ),
                background,
            )
            .context("filled_rectangle for window background")?;
        }

        for pos in panes {
            if pos.is_active {
                let _cursor_budget = self.frame_budget_should_run_render_op_with_reduce_motion(
                    OpKind::Cursor,
                    OpPriority::Required,
                    frame_reduce_motion,
                );
                self.update_text_cursor(&pos);
                if focused {
                    pos.pane.advise_focus();
                    if let Some(mux) = mux::Mux::try_get() {
                        mux.record_focus_for_current_identity(pos.pane.pane_id());
                    }
                }
            }
            self.paint_pane(&pos, &mut layers).context("paint_pane")?;
        }

        let paint_decorations = self.frame_budget_should_run_render_op_with_reduce_motion(
            OpKind::Decorations,
            OpPriority::Cosmetic,
            frame_reduce_motion,
        );

        // Splits, tab bar, and window borders are cosmetic frame
        // decorations; deferrals enqueue follow-up paint through the
        // frame-budget outstanding-work path.
        if paint_decorations {
            if let Some(pane) = self.get_active_pane_or_overlay() {
                let splits = self.get_splits();
                for split in &splits {
                    self.paint_split(&mut layers, split, &pane)
                        .context("paint_split")?;
                }
            }
        }

        if paint_decorations && self.show_tab_bar {
            self.paint_tab_bar(&mut layers).context("paint_tab_bar")?;
        }

        if paint_decorations {
            self.paint_window_borders(&mut layers)
                .context("paint_window_borders")?;
        }
        drop(layers);
        self.paint_modal().context("paint_modal")?;

        Ok(())
    }
}
