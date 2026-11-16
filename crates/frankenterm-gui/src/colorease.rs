use crate::uniforms::{UniformBuilder, UniformStruct};
use config::EasingFunction;
use std::time::{Duration, Instant};

const RENDER_DEADLINE_OVERFLOW_FALLBACK: Duration = Duration::from_secs(365 * 24 * 60 * 60);

fn duration_from_secs_f32_lossy(seconds: f32) -> Duration {
    Duration::try_from_secs_f32(seconds).unwrap_or(RENDER_DEADLINE_OVERFLOW_FALLBACK)
}

fn render_deadline_after(start: Instant, duration: Duration) -> Instant {
    start.checked_add(duration).unwrap_or_else(|| {
        start
            .checked_add(RENDER_DEADLINE_OVERFLOW_FALLBACK)
            .unwrap_or(start)
    })
}

/// When an animation's next frame is due: a time since the animation
/// started, or a delay from now.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum NextFrame {
    SinceStart(Duration),
    FromNow(Duration),
}

/// The next frame of an animation that runs at `fps` and started `elapsed`
/// ago (4tenz.8.5.2).
///
/// At 1 fps (also used for constant easing) it renders only at its
/// transitions: the end of the fade in, then the end of the fade out.
/// Faster rates render on a grid of whole-millisecond frames anchored at
/// the start. The interval rounds up, so a rate that does not divide 1000
/// never runs faster than configured, and a render off the grid waits for
/// the next boundary (the interval minus the remainder). Elapsed time also
/// rounds up, so a render less than a millisecond before a boundary waits
/// for the one after it. A render less than an interval after the
/// previous one was asked for by something else, so the animation waits a
/// whole interval from now, which is never earlier than the next boundary.
fn next_animation_frame(
    fps: u64,
    elapsed: Duration,
    since_last_render: Duration,
    fading_in: bool,
    in_duration: Duration,
    out_duration: Duration,
) -> NextFrame {
    if fps <= 1 {
        return NextFrame::SinceStart(if fading_in {
            in_duration
        } else {
            in_duration.saturating_add(out_duration)
        });
    }
    let interval_ms = 1000u64.div_ceil(fps);
    let interval = Duration::from_millis(interval_ms);
    if since_last_render < interval {
        return NextFrame::FromNow(interval);
    }
    let elapsed_ms = u64::try_from(elapsed.as_nanos().div_ceil(1_000_000)).unwrap_or(u64::MAX);
    let next_ms = (elapsed_ms / interval_ms)
        .saturating_add(1)
        .saturating_mul(interval_ms);
    NextFrame::SinceStart(Duration::from_millis(next_ms))
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct ColorEase {
    in_duration: f32,
    in_function: EasingFunction,
    out_duration: f32,
    out_function: EasingFunction,
    start: Option<Instant>,
    last_render: Instant,
}

impl ColorEase {
    pub fn new(
        in_duration_ms: u64,
        in_function: EasingFunction,
        out_duration_ms: u64,
        out_function: EasingFunction,
        start: Option<Instant>,
    ) -> Self {
        Self {
            in_duration: Duration::from_millis(in_duration_ms).as_secs_f32(),
            in_function,
            out_duration: Duration::from_millis(out_duration_ms).as_secs_f32(),
            out_function,
            start,
            last_render: Instant::now(),
        }
    }

    pub fn update_start(&mut self, start: Instant) {
        let start = match self.start.take() {
            Some(prior) if prior >= start => prior,
            _ => start,
        };
        self.start.replace(start);
    }

    pub fn intensity(&mut self, is_one_shot: bool) -> Option<(f32, Instant)> {
        if is_one_shot {
            self.intensity_one_shot()
        } else {
            Some(self.intensity_continuous())
        }
    }

    pub fn intensity_continuous(&mut self) -> (f32, Instant) {
        match self.intensity_one_shot() {
            Some(intensity) => intensity,
            None => {
                // Start a new cycle
                self.start.replace(Instant::now());
                self.intensity_one_shot().expect("just started")
            }
        }
    }

    pub fn intensity_one_shot(&mut self) -> Option<(f32, Instant)> {
        let start = self.start?;
        let elapsed_since_start = start.elapsed();
        let elapsed = elapsed_since_start.as_secs_f32();

        let intensity = if elapsed < self.in_duration {
            let pos = if self.in_duration > 0.0 {
                elapsed / self.in_duration
            } else {
                1.0
            };
            Some(self.in_function.evaluate_at_position(pos))
        } else {
            let completion = if self.out_duration > 0.0 {
                (elapsed - self.in_duration) / self.out_duration
            } else {
                1.0
            };
            if completion >= 1.0 {
                None
            } else {
                Some(1.0 - self.out_function.evaluate_at_position(completion))
            }
        };

        match intensity {
            Some(i) => {
                let now = Instant::now();
                let fps = if self.in_function == EasingFunction::Constant
                    && self.out_function == EasingFunction::Constant
                {
                    1
                } else {
                    (config::configuration().animation_fps as u64).max(1)
                };
                let next = match next_animation_frame(
                    fps,
                    elapsed_since_start,
                    now.saturating_duration_since(self.last_render),
                    elapsed < self.in_duration,
                    duration_from_secs_f32_lossy(self.in_duration),
                    duration_from_secs_f32_lossy(self.out_duration),
                ) {
                    NextFrame::SinceStart(due) => render_deadline_after(start, due),
                    NextFrame::FromNow(delay) => render_deadline_after(now, delay),
                };
                self.last_render = now;
                Some((i, next))
            }
            None => {
                self.start.take();
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_deadline_after_clamps_unrepresentable_deadline() {
        let now = Instant::now();
        let deadline = render_deadline_after(now, Duration::from_secs(u64::MAX));
        assert!(deadline >= now);
    }

    #[test]
    fn duration_from_secs_f32_lossy_clamps_invalid_duration() {
        assert_eq!(
            duration_from_secs_f32_lossy(f32::INFINITY),
            RENDER_DEADLINE_OVERFLOW_FALLBACK
        );
    }

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    /// The next frame of a 500 ms in, 500 ms out animation at `fps` whose
    /// previous render was long enough ago not to count as recent.
    fn next_overdue(fps: u64, elapsed: Duration) -> NextFrame {
        next_animation_frame(fps, elapsed, ms(1000), elapsed < ms(500), ms(500), ms(500))
    }

    #[test]
    fn animation_interval_rounds_up_so_the_rate_never_exceeds_animation_fps() {
        for fps in 2..=255u64 {
            let NextFrame::SinceStart(interval) = next_overdue(fps, Duration::ZERO) else {
                panic!("fps {} did not render on the grid", fps);
            };
            assert!(interval > Duration::ZERO, "fps {}", fps);
            assert!(
                interval.as_millis() * u128::from(fps) >= 1000,
                "fps {} interval {:?}",
                fps,
                interval
            );
        }
        assert_eq!(next_overdue(10, Duration::ZERO), NextFrame::SinceStart(ms(100)));
        assert_eq!(next_overdue(60, Duration::ZERO), NextFrame::SinceStart(ms(17)));
        assert_eq!(next_overdue(7, Duration::ZERO), NextFrame::SinceStart(ms(143)));
        assert_eq!(next_overdue(255, Duration::ZERO), NextFrame::SinceStart(ms(4)));
    }

    #[test]
    fn off_grid_render_waits_for_the_next_boundary_not_the_remainder() {
        // Remainder 30 waits 70 and remainder 70 waits 30: both land on 200.
        assert_eq!(next_overdue(10, ms(130)), NextFrame::SinceStart(ms(200)));
        assert_eq!(next_overdue(10, ms(170)), NextFrame::SinceStart(ms(200)));
        // On the grid, the next frame is a whole interval away.
        assert_eq!(next_overdue(10, ms(200)), NextFrame::SinceStart(ms(300)));
        // Less than a millisecond before a boundary rounds up onto it.
        assert_eq!(
            next_overdue(10, Duration::from_micros(199_500)),
            NextFrame::SinceStart(ms(300))
        );
        assert_eq!(next_overdue(60, ms(40)), NextFrame::SinceStart(ms(51)));
    }

    #[test]
    fn recent_render_waits_a_whole_interval_from_now() {
        for since_last_render in [Duration::ZERO, ms(1), ms(40), ms(99)] {
            for elapsed in [ms(130), ms(170), ms(200)] {
                assert_eq!(
                    next_animation_frame(10, elapsed, since_last_render, true, ms(500), ms(500)),
                    NextFrame::FromNow(ms(100)),
                );
            }
        }
        // Exactly an interval ago is overdue: back on the grid.
        assert_eq!(
            next_animation_frame(10, ms(130), ms(100), true, ms(500), ms(500)),
            NextFrame::SinceStart(ms(200)),
        );
    }

    #[test]
    fn one_fps_renders_only_at_the_transitions() {
        for since_last_render in [Duration::ZERO, ms(5000)] {
            assert_eq!(
                next_animation_frame(1, ms(130), since_last_render, true, ms(500), ms(700)),
                NextFrame::SinceStart(ms(500)),
            );
            assert_eq!(
                next_animation_frame(1, ms(630), since_last_render, false, ms(500), ms(700)),
                NextFrame::SinceStart(ms(1200)),
            );
        }
    }

    #[test]
    fn near_completion_the_last_boundary_is_the_end_of_the_fade() {
        assert_eq!(next_overdue(10, ms(950)), NextFrame::SinceStart(ms(1000)));
        // A render just before the end waits for the frame after it, where a
        // one-shot animation finds itself complete and stops.
        assert_eq!(
            next_overdue(10, Duration::from_micros(999_500)),
            NextFrame::SinceStart(ms(1100))
        );
    }

    /// Wakes driven only by the animation, each landing up to 2 ms late,
    /// never render more often than `animation_fps` over ten seconds.
    #[test]
    fn animation_driven_wakes_stay_within_the_configured_rate() {
        for fps in [2u64, 7, 10, 60, 144, 255] {
            let mut now = Duration::ZERO;
            let mut last_render = Duration::ZERO;
            let mut renders = 0u64;
            let mut lateness = 0u64;
            while now < ms(10_000) {
                renders += 1;
                let next = next_animation_frame(
                    fps,
                    now,
                    now - last_render,
                    true,
                    ms(1_000_000),
                    ms(1_000_000),
                );
                last_render = now;
                lateness = (lateness * 7 + 3) % 2000;
                now = match next {
                    NextFrame::SinceStart(due) => due,
                    NextFrame::FromNow(delay) => now + delay,
                } + Duration::from_micros(lateness);
            }
            assert!(
                renders <= 10 * fps + 1,
                "fps {} rendered {} times in 10 s",
                fps,
                renders
            );
            assert!(renders * 2 >= 10 * fps, "fps {} rendered only {}", fps, renders);
        }
    }
}

pub struct ColorEaseUniform {
    pub in_function: [f32; 4],
    pub out_function: [f32; 4],
    pub in_duration_ms: u32,
    pub out_duration_ms: u32,
}

impl From<ColorEase> for ColorEaseUniform {
    fn from(ease: ColorEase) -> ColorEaseUniform {
        Self {
            in_duration_ms: (ease.in_duration * 1000.).ceil() as u32,
            out_duration_ms: (ease.out_duration * 1000.).ceil() as u32,
            in_function: ease.in_function.as_bezier_array(),
            out_function: ease.out_function.as_bezier_array(),
        }
    }
}

impl<'a> UniformStruct<'a> for ColorEaseUniform {
    fn add_fields(&'a self, struct_name: &str, builder: &mut UniformBuilder<'a>) {
        builder.add_struct_field(struct_name, "in_function", &self.in_function);
        builder.add_struct_field(struct_name, "out_function", &self.out_function);
        builder.add_struct_field(struct_name, "in_duration_ms", &self.in_duration_ms);
        builder.add_struct_field(struct_name, "out_duration_ms", &self.out_duration_ms);
    }
}
