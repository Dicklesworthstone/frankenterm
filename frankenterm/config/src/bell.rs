use frankenterm_dynamic::{FromDynamic, ToDynamic};

/// <https://developer.mozilla.org/en-US/docs/Web/CSS/easing-function>
#[derive(Debug, Clone, Copy, FromDynamic, ToDynamic, PartialEq)]
pub enum EasingFunction {
    Linear,
    CubicBezier(f32, f32, f32, f32),
    Ease,
    EaseIn,
    EaseInOut,
    EaseOut,
    Constant,
}

impl EasingFunction {
    pub fn evaluate_at_position(&self, position: f32) -> f32 {
        fn cubic_bezier(p0: f32, p1: f32, p2: f32, p3: f32, x: f32) -> f32 {
            (1.0 - x).powi(3) * p0
                + 3.0 * (1.0 - x).powi(2) * x * p1
                + 3.0 * (1.0 - x) * x.powi(2) * p2
                + x.powi(3) * p3
        }

        let [a, b, c, d] = self.as_bezier_array();
        cubic_bezier(a, b, c, d, position)
    }

    pub fn as_bezier_array(&self) -> [f32; 4] {
        match self {
            Self::Constant => [0., 0., 0., 0.],
            Self::Linear => [0., 0., 1.0, 1.0],
            Self::CubicBezier(a, b, c, d) => [*a, *b, *c, *d],
            Self::Ease => [0.25, 0.1, 0.25, 1.0],
            Self::EaseIn => [0.42, 0.0, 1.0, 1.0],
            Self::EaseInOut => [0.42, 0., 0.58, 1.0],
            Self::EaseOut => [0., 0., 0.58, 1.0],
        }
    }
}

impl Default for EasingFunction {
    fn default() -> Self {
        Self::Ease
    }
}

#[derive(Default, Debug, Clone, FromDynamic, ToDynamic)]
pub struct VisualBell {
    #[dynamic(default)]
    pub fade_in_duration_ms: u64,
    #[dynamic(default)]
    pub fade_in_function: EasingFunction,
    #[dynamic(default)]
    pub fade_out_duration_ms: u64,
    #[dynamic(default)]
    pub fade_out_function: EasingFunction,
    #[dynamic(default)]
    pub target: VisualBellTarget,
}

#[derive(Debug, Clone, PartialEq, Eq, FromDynamic, ToDynamic)]
pub enum VisualBellTarget {
    BackgroundColor,
    CursorColor,
}

impl Default for VisualBellTarget {
    fn default() -> VisualBellTarget {
        Self::BackgroundColor
    }
}

#[derive(Debug, Clone, FromDynamic, ToDynamic)]
pub enum AudibleBell {
    SystemBeep,
    Disabled,
}

impl Default for AudibleBell {
    fn default() -> AudibleBell {
        Self::SystemBeep
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn easing_defaults_and_bezier_arrays() {
        assert_eq!(EasingFunction::default(), EasingFunction::Ease);
        assert_eq!(
            EasingFunction::Constant.as_bezier_array(),
            [0.0, 0.0, 0.0, 0.0]
        );
        assert_eq!(
            EasingFunction::Linear.as_bezier_array(),
            [0.0, 0.0, 1.0, 1.0]
        );
        assert_eq!(
            EasingFunction::CubicBezier(0.1, 0.2, 0.3, 0.4).as_bezier_array(),
            [0.1, 0.2, 0.3, 0.4]
        );
    }

    #[test]
    fn easing_linear_tracks_position() {
        // Linear = [0, 0, 1, 1] Bernstein polynomial: 3x²(1-x) + x³ = 3x² - 2x³
        let x: f32 = 0.37;
        let y = EasingFunction::Linear.evaluate_at_position(x);
        let expected = 3.0 * x * x - 2.0 * x * x * x;
        assert!((y - expected).abs() < 1e-5, "expected {expected}, got {y}");
    }

    #[test]
    fn visual_and_audible_defaults_are_stable() {
        let visual = VisualBell::default();
        assert_eq!(visual.fade_in_duration_ms, 0);
        assert_eq!(visual.fade_out_duration_ms, 0);
        assert_eq!(visual.fade_in_function, EasingFunction::Ease);
        assert_eq!(visual.fade_out_function, EasingFunction::Ease);
        assert_eq!(visual.target, VisualBellTarget::BackgroundColor);

        match AudibleBell::default() {
            AudibleBell::SystemBeep => {}
            AudibleBell::Disabled => panic!("unexpected audible bell default"),
        }
    }

    #[test]
    fn easing_constant_always_zero() {
        let f = EasingFunction::Constant;
        assert_eq!(f.evaluate_at_position(0.0), 0.0);
        assert_eq!(f.evaluate_at_position(0.5), 0.0);
        assert_eq!(f.evaluate_at_position(1.0), 0.0);
    }

    #[test]
    fn easing_linear_boundaries() {
        let f = EasingFunction::Linear;
        assert_eq!(f.evaluate_at_position(0.0), 0.0);
        assert_eq!(f.evaluate_at_position(1.0), 1.0);
    }

    #[test]
    fn easing_ease_in_monotonic() {
        let f = EasingFunction::EaseIn;
        let v1 = f.evaluate_at_position(0.25);
        let v2 = f.evaluate_at_position(0.5);
        let v3 = f.evaluate_at_position(0.75);
        assert!(v1 < v2, "EaseIn not monotonic: {v1} >= {v2}");
        assert!(v2 < v3, "EaseIn not monotonic: {v2} >= {v3}");
    }

    #[test]
    fn easing_ease_out_monotonic() {
        let f = EasingFunction::EaseOut;
        let v1 = f.evaluate_at_position(0.25);
        let v2 = f.evaluate_at_position(0.5);
        let v3 = f.evaluate_at_position(0.75);
        assert!(v1 < v2, "EaseOut not monotonic: {v1} >= {v2}");
        assert!(v2 < v3, "EaseOut not monotonic: {v2} >= {v3}");
    }

    #[test]
    fn easing_ease_in_out_boundaries() {
        // EaseInOut = [0.42, 0., 0.58, 1.0] Bernstein polynomial
        // At x=0: p0 = 0.42; at x=1: p3 = 1.0
        let f = EasingFunction::EaseInOut;
        let start = f.evaluate_at_position(0.0);
        let end = f.evaluate_at_position(1.0);
        assert!((start - 0.42).abs() < 0.01, "start: {start}");
        assert!((end - 1.0).abs() < 0.01, "end: {end}");
    }

    #[test]
    fn easing_named_bezier_values() {
        assert_eq!(
            EasingFunction::Ease.as_bezier_array(),
            [0.25, 0.1, 0.25, 1.0]
        );
        assert_eq!(
            EasingFunction::EaseIn.as_bezier_array(),
            [0.42, 0.0, 1.0, 1.0]
        );
        assert_eq!(
            EasingFunction::EaseInOut.as_bezier_array(),
            [0.42, 0., 0.58, 1.0]
        );
        assert_eq!(
            EasingFunction::EaseOut.as_bezier_array(),
            [0., 0., 0.58, 1.0]
        );
    }

    #[test]
    fn easing_copy_preserves_value() {
        let f = EasingFunction::CubicBezier(0.1, 0.2, 0.3, 0.4);
        let copied = f;
        assert_eq!(f, copied);
    }

    #[test]
    fn visual_bell_target_variants_differ() {
        assert_ne!(
            VisualBellTarget::BackgroundColor,
            VisualBellTarget::CursorColor
        );
    }
}
