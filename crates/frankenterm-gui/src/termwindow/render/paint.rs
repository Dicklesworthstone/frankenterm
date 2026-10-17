use crate::termwindow::frame_budget::{OpKind, OpPriority};
use crate::termwindow::{DamageGeneration, RenderAttemptFailure};
use ::window::WindowOps;
use ::window::bitmaps::atlas::{AtlasAllocationFailure, OutOfTextureSpace};
use anyhow::Context;
use frankenterm_alloc::resource_ledger::FrameLedger;
use frankenterm_core::frame_budget_a11y_gate::ReduceMotionState;
use frankenterm_font::ClearShapeCache;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowImage {
    Yes,
    Scale(usize),
    No,
}

/// A frame that crossed the renderer's synchronous presentation boundary.
///
/// OpenGL finish/swap and WebGPU submit/present have already returned
/// successfully. This is deliberately not a claim of asynchronous GPU
/// completion or visible scanout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PaintOutcome {
    pub(crate) damage_generation: DamageGeneration,
    submission_mach_ns: Option<u128>,
    post_present: PostPresentWork,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PostPresentWork {
    animation_due: Option<Instant>,
    should_schedule_animation: bool,
    should_force_frame_budget_paint: bool,
    /// When to poll for an image still drawn as a placeholder while its first
    /// frame decodes (`TermWindow::image_poll_due`).
    image_poll_due: Option<Instant>,
}

impl PostPresentWork {
    /// When to wake for the next frame. Animations run only in a focused
    /// window and within the frame budget. An image whose first frame is
    /// still decoding is not motion: it is polled until it shows, focused or
    /// not, because a finished decode wakes nothing and the image would
    /// otherwise stay a transparent placeholder until an unrelated repaint.
    fn animation_wake(self, focused: bool) -> Option<Instant> {
        let animation = self
            .animation_due
            .filter(|_| focused && self.should_schedule_animation);
        match (animation, self.image_poll_due) {
            (Some(animation), Some(image)) => Some(animation.min(image)),
            (animation, image) => animation.or(image),
        }
    }
}

const MAX_PAINT_PASSES: usize = 16;

/// The image poll deadline after noting an image still loading, due to be
/// polled at `due`: the latest pending one. A line cached with a placeholder
/// expires no later than its image's deadline, so only once the latest has
/// passed has every such line expired; keeping an earlier one would let a
/// paint between the two settle on a line still cached with its placeholder.
/// A known deadline that has already passed was for lines that expire and
/// render again, noting their images afresh, so it is replaced.
fn noted_image_poll_due(known: Option<Instant>, due: Instant, now: Instant) -> Instant {
    match known {
        Some(known) if known > now => known.max(due),
        _ => due,
    }
}

/// The image poll deadline that survives into a paint pass starting at
/// `now`. A passed deadline is dropped: every line cached with a placeholder
/// due by then has expired and renders again in this pass.
fn surviving_image_poll_due(known: Option<Instant>, now: Instant) -> Option<Instant> {
    known.filter(|due| *due > now)
}

impl crate::TermWindow {
    /// Records an image drawn in this paint pass by its cache load state and
    /// next due time: `Loading` means the frame holds a transparent
    /// placeholder while the image's first frame decodes on a worker, to be
    /// polled at `next_due` (or the next paint when the cache names none).
    pub(crate) fn note_image_load_state(
        &self,
        load_state: crate::glyphcache::LoadState,
        next_due: Option<Instant>,
    ) {
        if load_state == crate::glyphcache::LoadState::Loading {
            let now = Instant::now();
            let due = next_due.unwrap_or(now);
            self.image_poll_due.set(Some(noted_image_poll_due(
                self.image_poll_due.get(),
                due,
                now,
            )));
        }
    }

    pub(crate) fn paint_impl<P>(&mut self, present: P) -> Result<PaintOutcome, RenderAttemptFailure>
    where
        P: FnOnce(&mut Self) -> Result<(), RenderAttemptFailure>,
    {
        if let Some(ticket) = self.fallback_invalidation_for_paint.take() {
            ticket.begin_font_read();
        }
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

        // This is before paint_pass clears counters or takes self-referential
        // mapped layer borrows. Every accepted replacement is rebuilt below.
        let shrink = self
            .render_state
            .as_mut()
            .map(|state| state.shrink_idle_quads(start, self.quad_buffer_in_resize_gesture));
        match shrink {
            Some(Ok(count)) if count > 0 => {
                self.record_quad_buffer_allocation_snapshot(count);
                self.invalidate_fancy_tab_bar();
                self.invalidate_modal();
            }
            Some(Err(error)) => log::warn!("idle quad shrink refused: {error:#}"),
            _ => {}
        }

        {
            let diff = start.duration_since(self.last_fps_check_time);
            if diff > Duration::from_secs(1) {
                let seconds = diff.as_secs_f32();
                self.fps = self.num_frames as f32 / seconds;
                self.num_frames = 0;
                self.last_fps_check_time = start;
            }
        }

        // Paint passes this frame takes, retries included (ft-yccm0.2.6).
        let mut passes = 0u64;
        let geometry_result = 'pass: {
            for pass in 0..MAX_PAINT_PASSES {
                passes += 1;
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
                            // incorrect because geometry had already outgrown the
                            // buffer. Always grow on demand; idle shrinking remains
                            // separately gated by RenderState at the frame boundary.
                            match render_state.allocate_more_quads() {
                                Ok(change) => {
                                    let snapshot = render_state.quad_allocation_snapshot();
                                    self.quad_buffer_policy.record_live_allocation(
                                        snapshot.used,
                                        snapshot.capacity,
                                        change.reallocation_count,
                                    );
                                    if !change.allocated {
                                        break 'pass Ok(());
                                    }
                                    self.invalidate_fancy_tab_bar();
                                    self.invalidate_modal();
                                }
                                Err(err) => {
                                    break 'pass Err(err.context("allocate_more_quads"));
                                }
                            }
                        }
                        None => {
                            break 'pass Err(anyhow::anyhow!(
                                "paint_pass succeeded without initialized render state"
                            ));
                        }
                    },
                    Err(err) => {
                        if let Some(&OutOfTextureSpace {
                            size: Some(size),
                            current_size,
                            failure: AtlasAllocationFailure::Capacity,
                            ..
                        }) = err.root_cause().downcast_ref::<OutOfTextureSpace>()
                        {
                            let result = if pass == 0 {
                                log::trace!("recreate_texture_atlas");
                                self.recreate_texture_atlas(Some(current_size))
                            } else {
                                log::trace!("grow texture atlas to {}", size);
                                self.recreate_texture_atlas(Some(size))
                            };

                            if let Err(err) = result {
                                self.allow_images = match self.allow_images {
                                    AllowImage::Yes => AllowImage::Scale(2),
                                    AllowImage::Scale(0..=1) => AllowImage::Scale(2),
                                    AllowImage::Scale(2..=3) => AllowImage::Scale(4),
                                    AllowImage::Scale(4..=7) => AllowImage::Scale(8),
                                    AllowImage::Scale(_) => AllowImage::No,
                                    AllowImage::No => {
                                        break 'pass Err(err.context(if pass == 0 {
                                            "clear texture atlas"
                                        } else {
                                            "resize texture atlas"
                                        }));
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
                            self.invalidate_render_caches(
                                crate::termwindow::resize::RenderInvalidationCause::FallbackFont,
                            );
                        } else {
                            break 'pass Err(err.context("paint_pass"));
                        }
                    }
                }
            }

            break 'pass Err(anyhow::anyhow!(
                "paint did not converge within {MAX_PAINT_PASSES} passes"
            ));
        };
        FrameLedger::global().record_paint_passes(passes);

        let present_result = geometry_result
            .map_err(RenderAttemptFailure::paint)
            .and_then(|()| {
                log::debug!("paint_impl before call_draw elapsed={:?}", start.elapsed());
                let damage_generation = self.damage_generation();
                present(self)
                    .inspect_err(|_| FrameLedger::global().record_present_failure())
                    .map(|()| {
                        // Sample immediately after successful backend acceptance,
                        // before bookkeeping or log delivery can delay observation.
                        // The optional diagnostic never substitutes for SCK pixels.
                        let submission_mach_ns = if log::log_enabled!(
                            target: "frankenterm_gui::native_present_profile",
                            log::Level::Debug
                        ) {
                            self.webgpu
                                .as_ref()
                                .and_then(|state| state.native_submission_mach_ns())
                        } else {
                            None
                        };
                        // ft-yccm0.1.4: the GUI's own presented-frame count and
                        // cap, which the throughput harness checks its
                        // screen-capture FPS meter against.
                        let frames = FrameLedger::global();
                        frames.set_max_fps(self.config.max_fps);
                        frames.record_present();
                        (damage_generation, submission_mach_ns)
                    })
            });

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
        // Close out the allocator even when geometry/draw fails so frame-budget
        // accounting cannot leak across retries.
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

        if let Some(state) = self.render_state.as_mut() {
            state.observe_quad_frame(Instant::now(), present_result.is_ok());
        }
        self.report_cache_gauges(Instant::now());
        present_result.map(|(damage_generation, submission_mach_ns)| PaintOutcome {
            damage_generation,
            submission_mach_ns,
            post_present: PostPresentWork {
                animation_due,
                should_schedule_animation,
                should_force_frame_budget_paint,
                image_poll_due: self.image_poll_due.get(),
            },
        })
    }

    /// Runs invalidations that are valid only after the backend has accepted
    /// presentation. Keeping this out of `paint_impl` prevents a failed OpenGL
    /// swap from bypassing the bounded retry lane via an immediate animation or
    /// frame-budget repaint.
    pub(crate) fn complete_presented_paint(&mut self, outcome: PaintOutcome) {
        let now = Instant::now();
        let mut expired_copy = false;
        for (pane_id, state) in self.pane_state.borrow_mut().iter_mut() {
            // The deadline belongs to the transaction, including panes in
            // hidden tabs that are not visited by the visible retry loop.
            expired_copy |= crate::selection::PendingNativeSelection::expire_text_copy(
                &mut state.pending_native_selection,
                now,
            );
            if let Some(frame) = state.selection_frame.presented() {
                // This record is emitted only after backend acceptance and uses
                // the complete frame actually staged for drawing. It is not a
                // GPU completion or scanout timestamp. The Metal host clock was
                // sampled after acceptance; zero means no supported source clock
                // and must not authorize a compositor timing claim.
                log::debug!(
                    target: "frankenterm_gui::native_present_profile",
                    "native_present_complete pane_id={} damage={} source_sequence={} viewport={} geometry={:?} submission_mach_ns={}",
                    pane_id,
                    outcome.damage_generation.value,
                    frame.source_sequence,
                    frame.viewport,
                    frame.geometry,
                    outcome.submission_mach_ns.unwrap_or_default(),
                );
            }
        }
        if expired_copy {
            frankenterm_toast_notification::persistent_toast_notification(
                "Selection was not copied",
                "The selected text did not arrive in time. Copy the selection again.",
            );
        }
        // Released gestures retain their final endpoint through contention,
        // so native anchor retries are not limited to an active drag.
        for pos in self.get_panes_to_render() {
            self.retry_pending_selection_start(&pos.pane);
            self.retry_pending_native_selection(&pos.pane);
            let retry = self
                .pane_state(pos.pane.pane_id())
                .is_some_and(|mut state| {
                    state
                        .pending_native_selection
                        .as_mut()
                        .is_some_and(|pending| pending.take_paint_retry())
                });
            if retry {
                self.schedule_animation_wake(Instant::now() + Duration::from_millis(16));
            }
            let copy_deadline = self.pane_state(pos.pane.pane_id()).and_then(|state| {
                state
                    .pending_native_selection
                    .as_ref()
                    .and_then(|pending| pending.text_copy.as_ref().map(|copy| copy.wake_at()))
            });
            if let Some(deadline) = copy_deadline {
                self.schedule_animation_wake(deadline);
            }
            let retry = self
                .pane_state(pos.pane.pane_id())
                .is_some_and(|mut state| {
                    state
                        .pending_selection_start
                        .as_mut()
                        .is_some_and(|pending| pending.take_paint_retry())
                });
            if retry {
                self.schedule_animation_wake(Instant::now() + Duration::from_millis(16));
            }
        }
        if outcome.post_present.should_force_frame_budget_paint {
            if let Some(window) = self.window.clone() {
                window.invalidate();
            }
        }

        if let Some(next_due) = outcome.post_present.animation_wake(self.focused.is_some()) {
            self.schedule_animation_wake(next_due);
        }
    }

    pub fn paint_modal(&mut self) -> anyhow::Result<()> {
        if let Some(modal) = self.get_modal() {
            for computed in modal.computed_element(self)?.iter() {
                let mut ui_items = computed.ui_items();

                self.render_element(&computed, self.chrome()?, None)?;

                self.ui_items.append(&mut ui_items);
            }
        }

        Ok(())
    }

    pub fn paint_pass(&mut self, frame_reduce_motion: ReduceMotionState) -> anyhow::Result<()> {
        // Every atlas retry rebuilds geometry. Never promote a candidate from
        // a failed or earlier attempt, including a pane omitted by this pass.
        for state in self.pane_state.borrow_mut().values_mut() {
            state.selection_frame.begin_attempt();
        }
        self.image_poll_due.set(surviving_image_poll_due(
            self.image_poll_due.get(),
            Instant::now(),
        ));
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
                        // Published facts: paint never waits for the
                        // terminal (ft-yccm0.2.2.3).
                        None => p.pane.render_facts().dimensions.physical_top,
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
                panes[0].pane.render_facts().palette.background
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

#[cfg(test)]
mod tests {
    use super::*;

    fn work(
        animation_due: Option<Instant>,
        schedule: bool,
        image_poll_due: Option<Instant>,
    ) -> PostPresentWork {
        PostPresentWork {
            animation_due,
            should_schedule_animation: schedule,
            should_force_frame_budget_paint: false,
            image_poll_due,
        }
    }

    #[test]
    fn the_image_poll_deadline_is_the_latest_pending_one() {
        let now = Instant::now();
        let ms = |n| now + Duration::from_millis(n);
        assert_eq!(noted_image_poll_due(None, ms(16), now), ms(16));
        // Two placeholders noted in one paint (a sixel row, then an iTerm2
        // row a few ms later): a wake at the earlier deadline would find the
        // later row still cached with its placeholder.
        assert_eq!(noted_image_poll_due(Some(ms(16)), ms(19), now), ms(19));
        assert_eq!(noted_image_poll_due(Some(ms(19)), ms(16), now), ms(19));
        // A passed deadline belongs to lines that render again; a line that
        // just noted its image afresh must not be hidden behind it.
        assert_eq!(noted_image_poll_due(Some(now), ms(16), now), ms(16));
        assert_eq!(noted_image_poll_due(Some(ms(5)), ms(16), ms(6)), ms(16));
    }

    #[test]
    fn the_image_poll_deadline_survives_cached_paints_until_it_passes() {
        let now = Instant::now();
        let due = now + Duration::from_millis(16);
        // A paint before the deadline may reuse the cached placeholder line
        // without rendering it, so the deadline must survive that paint.
        assert_eq!(surviving_image_poll_due(Some(due), now), Some(due));
        // From the deadline on, that line has expired and renders again.
        assert_eq!(surviving_image_poll_due(Some(due), due), None);
        assert_eq!(surviving_image_poll_due(None, now), None);
    }

    #[test]
    fn animations_wake_only_a_focused_window_within_budget() {
        let due = Instant::now() + Duration::from_millis(16);
        assert_eq!(work(Some(due), true, None).animation_wake(true), Some(due));
        assert_eq!(work(Some(due), true, None).animation_wake(false), None);
        assert_eq!(
            work(Some(due), false, None).animation_wake(true),
            None,
            "reduce motion or frame pressure skips the animation"
        );
        assert_eq!(work(None, true, None).animation_wake(true), None);
    }

    #[test]
    fn a_decoding_image_is_polled_focused_or_not() {
        let now = Instant::now();
        let image = now + Duration::from_millis(16);
        for focused in [false, true] {
            for schedule in [false, true] {
                assert_eq!(
                    work(None, schedule, Some(image)).animation_wake(focused),
                    Some(image),
                    "focused={focused} schedule={schedule}"
                );
            }
        }
        // A running animation and a decoding image: the earlier wins, and an
        // animation the window may not run never delays the image poll.
        let sooner = now + Duration::from_millis(5);
        let later = now + Duration::from_millis(40);
        assert_eq!(
            work(Some(sooner), true, Some(image)).animation_wake(true),
            Some(sooner)
        );
        assert_eq!(
            work(Some(later), true, Some(image)).animation_wake(true),
            Some(image)
        );
        assert_eq!(
            work(Some(sooner), true, Some(image)).animation_wake(false),
            Some(image)
        );
    }
}
