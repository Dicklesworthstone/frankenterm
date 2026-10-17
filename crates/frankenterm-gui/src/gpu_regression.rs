//! GPU golden-image regression comparator.
//!
//! Public entry-points used by `tests/gpu_regression.rs` (the harness binary)
//! and the unit-test suite below. Keeping the comparator here — rather than
//! private to the test binary — lets us pin its behavior with a real unit
//! suite (`#[cfg(test)] mod tests`) and lets future tooling (e.g. the e2e
//! script in ft-ombfl.12) call into it directly.
//!
//! Bead: ft-ombfl.11.

use std::cmp;

use image::{Rgba, RgbaImage};
use serde::{Deserialize, Serialize};

/// Comparator thresholds. Defaults match the harness contract documented in
/// `tests/golden/gpu/README.md`.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
pub struct Thresholds {
    /// Floor for the mean windowed SSIM.
    pub min_ssim: f64,
    pub max_l_inf: u8,
    pub max_changed_pixel_fraction: f64,
    /// Floor for the worst single SSIM window. It catches a regression
    /// confined to a few glyphs, which the mean over thousands of windows
    /// barely registers. 0.0 (the default) disables it.
    #[serde(default)]
    pub min_window_ssim: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            min_ssim: 0.99,
            max_l_inf: 8,
            max_changed_pixel_fraction: 0.001,
            min_window_ssim: 0.0,
        }
    }
}

/// Quantitative output of a single comparator run.
#[derive(Debug, Clone, Serialize)]
pub struct CompareMetrics {
    /// Mean SSIM over [`SSIM_WINDOW`]-pixel luma windows (Wang et al. MSSIM),
    /// so a regression confined to a few glyphs still moves the score.
    pub ssim: f64,
    /// SSIM of the worst window, and where it is (top-left pixel).
    pub min_window_ssim: f64,
    pub worst_window: [u32; 2],
    pub l_inf: u8,
    /// Largest absolute delta per channel, `[R, G, B, A]`.
    pub channel_max_delta: [u8; 4],
    pub changed_pixels: u64,
    pub total_pixels: u64,
    pub changed_pixel_fraction: f64,
    pub thresholds: Thresholds,
}

/// Pass/fail verdict plus diff visualizations.
#[derive(Debug)]
pub struct CompareResult {
    pub passed: bool,
    pub metrics: CompareMetrics,
    /// RGBA image highlighting differing pixels in red and showing matching
    /// pixels at their original luminance with reduced alpha.
    pub diff: RgbaImage,
    /// Per-pixel max-channel delta as a heat ramp (black, blue, red, yellow,
    /// white as the delta grows); identical pixels show the expected image's
    /// luminance dimmed, so the scene stays recognizable.
    pub heatmap: RgbaImage,
}

/// Side of the square SSIM window, in pixels.
pub const SSIM_WINDOW: u32 = 8;
/// Step between SSIM windows. Half the window, so every pixel lies in at
/// least one window and interior pixels in four.
pub const SSIM_STRIDE: u32 = 4;

/// Windowed luma SSIM summary; see [`ssim_windowed_luma`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SsimSummary {
    pub mean: f64,
    pub min: f64,
    pub worst_window: [u32; 2],
    pub windows: u64,
}

/// Errors produced by the comparator. Distinct from a *failed* comparison —
/// these surface inputs the comparator refuses to evaluate.
#[derive(Debug, thiserror::Error)]
pub enum CompareError {
    #[error(
        "image dimensions differ: actual={actual_w}x{actual_h}, expected={expected_w}x{expected_h}"
    )]
    DimensionMismatch {
        actual_w: u32,
        actual_h: u32,
        expected_w: u32,
        expected_h: u32,
    },
}

/// Compare two RGBA images against the supplied thresholds.
///
/// Returns:
/// - `Ok(CompareResult)` with the metrics + a diff PNG, regardless of pass/fail.
///   Inspect `result.passed` for the verdict.
/// - `Err(CompareError::DimensionMismatch)` if the inputs disagree on size
///   (the comparator refuses to compare unequal-sized images rather than
///   silently passing or panicking).
pub fn compare_images(
    actual: &RgbaImage,
    expected: &RgbaImage,
    thresholds: Thresholds,
) -> Result<CompareResult, CompareError> {
    let (actual_width, actual_height) = actual.dimensions();
    let (expected_width, expected_height) = expected.dimensions();
    if (actual_width, actual_height) != (expected_width, expected_height) {
        return Err(CompareError::DimensionMismatch {
            actual_w: actual_width,
            actual_h: actual_height,
            expected_w: expected_width,
            expected_h: expected_height,
        });
    }

    let total_pixels = u64::from(actual_width) * u64::from(actual_height);
    let mut changed_pixels = 0u64;
    let mut l_inf = 0u8;
    let mut channel_max_delta = [0u8; 4];
    let mut diff = RgbaImage::new(actual_width, actual_height);
    let mut heatmap = RgbaImage::new(actual_width, actual_height);

    for y in 0..actual_height {
        for x in 0..actual_width {
            let a = actual.get_pixel(x, y).0;
            let e = expected.get_pixel(x, y).0;
            let mut pixel_delta = 0u8;
            for (channel, max) in channel_max_delta.iter_mut().enumerate() {
                let delta = a[channel].abs_diff(e[channel]);
                *max = cmp::max(*max, delta);
                pixel_delta = cmp::max(pixel_delta, delta);
            }
            l_inf = cmp::max(l_inf, pixel_delta);
            if pixel_delta > thresholds.max_l_inf {
                changed_pixels += 1;
                diff.put_pixel(x, y, Rgba([255, 0, 0, 255]));
            } else {
                let shade = ((u16::from(a[0]) + u16::from(a[1]) + u16::from(a[2])) / 3) as u8;
                diff.put_pixel(x, y, Rgba([shade, shade, shade, 96]));
            }
            heatmap.put_pixel(x, y, heat_pixel(pixel_delta, e));
        }
    }

    let changed_pixel_fraction = if total_pixels == 0 {
        0.0
    } else {
        changed_pixels as f64 / total_pixels as f64
    };
    let ssim = ssim_windowed_luma(actual, expected);
    let passed = ssim.mean >= thresholds.min_ssim
        && ssim.min >= thresholds.min_window_ssim
        && l_inf <= thresholds.max_l_inf
        && changed_pixel_fraction <= thresholds.max_changed_pixel_fraction;

    Ok(CompareResult {
        passed,
        metrics: CompareMetrics {
            ssim: ssim.mean,
            min_window_ssim: ssim.min,
            worst_window: ssim.worst_window,
            l_inf,
            channel_max_delta,
            changed_pixels,
            total_pixels,
            changed_pixel_fraction,
            thresholds,
        },
        diff,
        heatmap,
    })
}

/// One heatmap pixel: a perceptual ramp over the max-channel delta, or the
/// expected pixel's dimmed luminance where nothing changed.
fn heat_pixel(delta: u8, expected: [u8; 4]) -> Rgba<u8> {
    if delta == 0 {
        let luma = luma(&Rgba(expected));
        let dim = (luma * 0.3).round().clamp(0.0, 255.0) as u8;
        return Rgba([dim, dim, dim, 255]);
    }
    // sqrt stretches small deltas (antialiasing drift) into visible colors.
    let t = (f64::from(delta) / 255.0).sqrt();
    const STOPS: [[f64; 3]; 5] = [
        [0.0, 0.0, 0.0],
        [0.0, 0.0, 255.0],
        [255.0, 0.0, 0.0],
        [255.0, 255.0, 0.0],
        [255.0, 255.0, 255.0],
    ];
    let scaled = t * (STOPS.len() - 1) as f64;
    let index = (scaled.floor() as usize).min(STOPS.len() - 2);
    let frac = scaled - index as f64;
    let channel = |c: usize| {
        let value = STOPS[index][c] + (STOPS[index + 1][c] - STOPS[index][c]) * frac;
        value.round().clamp(0.0, 255.0) as u8
    };
    Rgba([channel(0), channel(1), channel(2), 255])
}

/// Mean and worst SSIM over [`SSIM_WINDOW`]-square luma windows placed every
/// [`SSIM_STRIDE`] pixels (the last row and column of windows are clamped to
/// the image edge). An image smaller than one window is a single window.
/// Window sums come from summed-area tables, so the cost is linear in pixels.
pub fn ssim_windowed_luma(actual: &RgbaImage, expected: &RgbaImage) -> SsimSummary {
    let (width, height) = actual.dimensions();
    if width == 0 || height == 0 {
        return SsimSummary {
            mean: 1.0,
            min: 1.0,
            worst_window: [0, 0],
            windows: 0,
        };
    }
    let window_w = width.min(SSIM_WINDOW);
    let window_h = height.min(SSIM_WINDOW);
    let tables = SummedAreaTables::new(actual, expected);
    let starts = |extent: u32, window: u32| {
        let last = extent - window;
        let mut starts: Vec<u32> = (0..=last).step_by(SSIM_STRIDE as usize).collect();
        if starts.last() != Some(&last) {
            starts.push(last);
        }
        starts
    };
    let xs = starts(width, window_w);
    let ys = starts(height, window_h);

    let mut sum = 0.0;
    let mut min = f64::INFINITY;
    let mut worst_window = [0, 0];
    for &y in &ys {
        for &x in &xs {
            let s = tables.window_ssim(x, y, window_w, window_h);
            sum += s;
            if s < min {
                min = s;
                worst_window = [x, y];
            }
        }
    }
    let windows = (xs.len() * ys.len()) as u64;
    SsimSummary {
        mean: sum / windows as f64,
        min,
        worst_window,
        windows,
    }
}

/// Summed-area tables of luma `x`, `y`, `x²`, `y²` and `xy` over two images,
/// with a zero row and column in front so window sums need no edge cases.
struct SummedAreaTables {
    stride: usize,
    tables: [Vec<f64>; 5],
}

impl SummedAreaTables {
    fn new(actual: &RgbaImage, expected: &RgbaImage) -> Self {
        let (width, height) = actual.dimensions();
        let stride = width as usize + 1;
        let len = stride * (height as usize + 1);
        let mut tables: [Vec<f64>; 5] = std::array::from_fn(|_| vec![0.0; len]);
        for y in 0..height as usize {
            let mut row = [0.0; 5];
            for x in 0..width as usize {
                let a = luma(actual.get_pixel(x as u32, y as u32));
                let e = luma(expected.get_pixel(x as u32, y as u32));
                let values = [a, e, a * a, e * e, a * e];
                let below = (y + 1) * stride + x + 1;
                let above = y * stride + x + 1;
                for (index, table) in tables.iter_mut().enumerate() {
                    row[index] += values[index];
                    table[below] = table[above] + row[index];
                }
            }
        }
        Self { stride, tables }
    }

    fn window_sum(&self, table: usize, x: u32, y: u32, w: u32, h: u32) -> f64 {
        let (x0, y0) = (x as usize, y as usize);
        let (x1, y1) = (x0 + w as usize, y0 + h as usize);
        let t = &self.tables[table];
        t[y1 * self.stride + x1] - t[y0 * self.stride + x1] - t[y1 * self.stride + x0]
            + t[y0 * self.stride + x0]
    }

    fn window_ssim(&self, x: u32, y: u32, w: u32, h: u32) -> f64 {
        let n = f64::from(w) * f64::from(h);
        let sum = |table| self.window_sum(table, x, y, w, h);
        let (sum_x, sum_y) = (sum(0), sum(1));
        let mean_x = sum_x / n;
        let mean_y = sum_y / n;
        let denom = (n - 1.0).max(1.0);
        // Not clamped: a cancellation residue is far below c2, and computing
        // var and cov identically keeps identical windows at exactly 1.0.
        let var_x = (sum(2) - sum_x * mean_x) / denom;
        let var_y = (sum(3) - sum_y * mean_y) / denom;
        let cov_xy = (sum(4) - sum_x * mean_y) / denom;
        ssim_from_moments(mean_x, mean_y, var_x, var_y, cov_xy)
    }
}

fn ssim_from_moments(mean_x: f64, mean_y: f64, var_x: f64, var_y: f64, cov_xy: f64) -> f64 {
    let c1 = (0.01_f64 * 255.0).powi(2);
    let c2 = (0.03_f64 * 255.0).powi(2);
    ((2.0 * mean_x * mean_y + c1) * (2.0 * cov_xy + c2))
        / ((mean_x.powi(2) + mean_y.powi(2) + c1) * (var_x + var_y + c2))
}

/// Detect the macOS 15 screen-capture permission dialog in a captured frame.
///
/// The WezTerm differential adapter compares screenshots from a black, fixed
/// terminal scene. If macOS overlays the "private window picker" permission
/// prompt, two contaminated frames can compare as equal and produce a false
/// render pass. This detector is deliberately narrow: it requires both a large
/// neutral light dialog body and the characteristic blue button/red recording
/// badge in the central capture region.
pub fn detect_macos_screen_capture_prompt_contamination(image: &RgbaImage) -> bool {
    let width = image.width();
    let height = image.height();
    if width < 200 || height < 200 {
        return false;
    }

    let x_start = width / 4;
    let x_end = width.saturating_mul(3) / 4;
    let y_end = height.saturating_mul(85) / 100;
    let total_pixels = u64::from(width) * u64::from(height);
    let mut light_panel_pixels = 0u64;
    let mut blue_button_pixels = 0u64;
    let mut red_badge_pixels = 0u64;

    for y in 0..y_end {
        for x in x_start..x_end {
            let [red, green, blue, alpha] = image.get_pixel(x, y).0;
            if alpha < 220 {
                continue;
            }

            let max_channel = red.max(green).max(blue);
            let min_channel = red.min(green).min(blue);
            if min_channel >= 215 && max_channel.saturating_sub(min_channel) <= 30 {
                light_panel_pixels += 1;
            }
            if red <= 80 && (80..=180).contains(&green) && blue >= 180 {
                blue_button_pixels += 1;
            }
            if red >= 200 && green <= 90 && blue <= 90 {
                red_badge_pixels += 1;
            }
        }
    }

    light_panel_pixels.saturating_mul(100) >= total_pixels.saturating_mul(5)
        && blue_button_pixels.saturating_mul(1_000) >= total_pixels.saturating_mul(3)
        && red_badge_pixels.saturating_mul(10_000) >= total_pixels.saturating_mul(2)
}

/// Single-window SSIM over the luma channel. Identical inputs produce 1.0;
/// constant images on both sides also produce 1.0 (handled by the `c1`/`c2`
/// stabilization terms in the standard SSIM formula).
pub fn ssim_luma(actual: &RgbaImage, expected: &RgbaImage) -> f64 {
    let n = f64::from(actual.width()) * f64::from(actual.height());
    if n == 0.0 {
        return 1.0;
    }

    let mut sum_x = 0.0;
    let mut sum_y = 0.0;
    for (actual, expected) in actual.pixels().zip(expected.pixels()) {
        sum_x += luma(actual);
        sum_y += luma(expected);
    }
    let mean_x = sum_x / n;
    let mean_y = sum_y / n;

    let mut var_x = 0.0;
    let mut var_y = 0.0;
    let mut cov_xy = 0.0;
    for (actual, expected) in actual.pixels().zip(expected.pixels()) {
        let dx = luma(actual) - mean_x;
        let dy = luma(expected) - mean_y;
        var_x += dx * dx;
        var_y += dy * dy;
        cov_xy += dx * dy;
    }
    let denom = (n - 1.0).max(1.0);
    var_x /= denom;
    var_y /= denom;
    cov_xy /= denom;
    ssim_from_moments(mean_x, mean_y, var_x, var_y, cov_xy)
}

fn luma(pixel: &Rgba<u8>) -> f64 {
    let [r, g, b, _a] = pixel.0;
    0.2126 * f64::from(r) + 0.7152 * f64::from(g) + 0.0722 * f64::from(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};
    use proptest::prelude::*;

    // ── Helpers ──────────────────────────────────────────────────────────────

    /// Solid-color RGBA image.
    fn solid(w: u32, h: u32, rgba: [u8; 4]) -> RgbaImage {
        let mut img = RgbaImage::new(w, h);
        for p in img.pixels_mut() {
            *p = Rgba(rgba);
        }
        img
    }

    /// Checkerboard of 8x8 cells.
    fn checker(w: u32, h: u32, a: [u8; 4], b: [u8; 4]) -> RgbaImage {
        let mut img = RgbaImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let c = if ((x / 8) + (y / 8)) % 2 == 0 { a } else { b };
                img.put_pixel(x, y, Rgba(c));
            }
        }
        img
    }

    /// Return the count of pure-red diff pixels (R=255,G=0,B=0,A=255).
    fn red_pixels(img: &RgbaImage) -> u64 {
        img.pixels().filter(|p| p.0 == [255, 0, 0, 255]).count() as u64
    }

    fn fill_rect(img: &mut RgbaImage, x0: u32, y0: u32, w: u32, h: u32, rgba: [u8; 4]) {
        for y in y0..y0.saturating_add(h).min(img.height()) {
            for x in x0..x0.saturating_add(w).min(img.width()) {
                img.put_pixel(x, y, Rgba(rgba));
            }
        }
    }

    // ── 1. Identical images → PASS ───────────────────────────────────────────

    #[test]
    fn identical_solid_images_pass() {
        let a = solid(32, 32, [42, 99, 200, 255]);
        let b = solid(32, 32, [42, 99, 200, 255]);
        let r = compare_images(&a, &b, Thresholds::default()).unwrap();
        assert!(r.passed, "metrics={:?}", r.metrics);
        assert_eq!(r.metrics.l_inf, 0);
        assert_eq!(r.metrics.changed_pixels, 0);
        assert_eq!(r.metrics.changed_pixel_fraction, 0.0);
        assert!((r.metrics.ssim - 1.0).abs() < 1e-9);
    }

    #[test]
    fn identical_pattern_images_pass() {
        let a = checker(64, 48, [16, 16, 16, 255], [240, 240, 240, 255]);
        let b = checker(64, 48, [16, 16, 16, 255], [240, 240, 240, 255]);
        let r = compare_images(&a, &b, Thresholds::default()).unwrap();
        assert!(r.passed);
        assert_eq!(r.metrics.l_inf, 0);
    }

    // ── 2. One-pixel diff within tolerance → PASS ────────────────────────────

    #[test]
    fn single_pixel_within_l_inf_tolerance_passes() {
        let mut a = solid(32, 32, [100, 100, 100, 255]);
        let b = solid(32, 32, [100, 100, 100, 255]);
        // l_inf = 5, default max_l_inf = 8 → not counted as changed.
        a.put_pixel(0, 0, Rgba([105, 100, 100, 255]));
        let r = compare_images(&a, &b, Thresholds::default()).unwrap();
        assert!(r.passed, "metrics={:?}", r.metrics);
        assert_eq!(r.metrics.l_inf, 5);
        assert_eq!(r.metrics.changed_pixels, 0);
    }

    #[test]
    fn pixel_delta_exactly_at_l_inf_threshold_passes() {
        // Boundary contract: l_inf == max_l_inf is INSIDE the tolerance band.
        // The pass gate uses `l_inf <= max_l_inf`. A future flip of `<=` to
        // `<` (or vice versa) is exactly the kind of off-by-one that would
        // either silently ignore real regressions or flood CI with false
        // positives. This test pins the boundary.
        let mut a = solid(32, 32, [100, 100, 100, 255]);
        let b = solid(32, 32, [100, 100, 100, 255]);
        a.put_pixel(0, 0, Rgba([108, 100, 100, 255])); // delta = max_l_inf = 8
        let r = compare_images(&a, &b, Thresholds::default()).unwrap();
        assert!(
            r.passed,
            "delta == max_l_inf must pass; metrics={:?}",
            r.metrics
        );
        assert_eq!(r.metrics.l_inf, 8);
        assert_eq!(r.metrics.changed_pixels, 0);
    }

    #[test]
    fn pixel_delta_one_past_l_inf_threshold_fails() {
        // Companion to the boundary test: delta = max_l_inf + 1 must fail.
        let mut a = solid(32, 32, [100, 100, 100, 255]);
        let b = solid(32, 32, [100, 100, 100, 255]);
        a.put_pixel(0, 0, Rgba([109, 100, 100, 255])); // delta = 9 = max_l_inf + 1
        let r = compare_images(&a, &b, Thresholds::default()).unwrap();
        assert!(
            !r.passed,
            "delta == max_l_inf+1 must fail; metrics={:?}",
            r.metrics
        );
        assert_eq!(r.metrics.l_inf, 9);
        assert_eq!(r.metrics.changed_pixels, 1);
    }

    // ── 3. One-pixel diff over tolerance → FAIL ──────────────────────────────

    #[test]
    fn single_pixel_over_l_inf_tolerance_fails() {
        let mut a = solid(32, 32, [100, 100, 100, 255]);
        let b = solid(32, 32, [100, 100, 100, 255]);
        // l_inf = 50; over both default max_l_inf=8 AND default fraction=0.001.
        // 1/1024 = 0.000976 < 0.001, so the FAIL is driven by l_inf alone.
        a.put_pixel(0, 0, Rgba([150, 100, 100, 255]));
        let r = compare_images(&a, &b, Thresholds::default()).unwrap();
        assert!(!r.passed, "metrics={:?}", r.metrics);
        assert_eq!(r.metrics.l_inf, 50);
        assert_eq!(r.metrics.changed_pixels, 1);
    }

    // ── 4. Subtle shift within SSIM threshold → PASS ─────────────────────────

    #[test]
    fn small_uniform_shift_keeps_ssim_high() {
        // Shift every pixel by 4 (within max_l_inf=8).
        let a = solid(64, 64, [100, 100, 100, 255]);
        let b = solid(64, 64, [104, 104, 104, 255]);
        let r = compare_images(&a, &b, Thresholds::default()).unwrap();
        assert!(r.passed, "metrics={:?}", r.metrics);
        // SSIM on two solid-color images is exactly 1.0 because variance is 0
        // on both sides (the c1/c2 stabilization saturates).
        assert!(r.metrics.ssim >= 0.99);
        assert_eq!(r.metrics.l_inf, 4);
    }

    // ── 5. Visible local difference → FAIL ───────────────────────────────────

    #[test]
    fn large_localized_block_fails() {
        let mut a = solid(64, 64, [10, 10, 10, 255]);
        let b = solid(64, 64, [10, 10, 10, 255]);
        // Paint a 16x16 white block in the corner (256 pixels of large delta).
        for y in 0..16 {
            for x in 0..16 {
                a.put_pixel(x, y, Rgba([245, 245, 245, 255]));
            }
        }
        let r = compare_images(&a, &b, Thresholds::default()).unwrap();
        assert!(!r.passed, "metrics={:?}", r.metrics);
        assert_eq!(r.metrics.l_inf, 235);
        assert_eq!(r.metrics.changed_pixels, 256);
        // 256/4096 = 0.0625 ≫ 0.001
        assert!(r.metrics.changed_pixel_fraction > 0.001);
    }

    // ── 6. Different sizes → ERROR ───────────────────────────────────────────

    #[test]
    fn different_dimensions_error() {
        let a = solid(32, 32, [0, 0, 0, 255]);
        let b = solid(33, 32, [0, 0, 0, 255]);
        let err = compare_images(&a, &b, Thresholds::default()).unwrap_err();
        match err {
            CompareError::DimensionMismatch {
                actual_w,
                actual_h,
                expected_w,
                expected_h,
            } => {
                assert_eq!((actual_w, actual_h), (32, 32));
                assert_eq!((expected_w, expected_h), (33, 32));
            }
        }
    }

    #[test]
    fn different_heights_error() {
        let a = solid(32, 64, [0, 0, 0, 255]);
        let b = solid(32, 32, [0, 0, 0, 255]);
        assert!(matches!(
            compare_images(&a, &b, Thresholds::default()),
            Err(CompareError::DimensionMismatch { .. })
        ));
    }

    // ── 7. Tolerance override applied ────────────────────────────────────────

    #[test]
    fn meta_threshold_override_loosens_l_inf() {
        let mut a = solid(32, 32, [100, 100, 100, 255]);
        let b = solid(32, 32, [100, 100, 100, 255]);
        a.put_pixel(0, 0, Rgba([150, 100, 100, 255])); // delta=50

        // Default thresholds → fail (max_l_inf=8).
        let strict = compare_images(&a, &b, Thresholds::default()).unwrap();
        assert!(!strict.passed);

        // Override max_l_inf=64 → pass.
        let loose = compare_images(
            &a,
            &b,
            Thresholds {
                max_l_inf: 64,
                ..Thresholds::default()
            },
        )
        .unwrap();
        assert!(loose.passed, "metrics={:?}", loose.metrics);
    }

    #[test]
    fn meta_threshold_override_changed_pixel_fraction_is_recorded() {
        // Structural note: in the current comparator, a pixel only counts as
        // "changed" when delta > max_l_inf. So changed_pixel_fraction > 0
        // implies the l_inf gate has already failed. The fraction gate is a
        // belt-and-suspenders safety net rather than an independent
        // pass/fail axis. This test pins:
        //   1. changed_pixel_fraction is computed correctly (count / total)
        //   2. tightening the fraction does not flip a passing case to fail
        //      when the l_inf gate already passed
        //   3. when the l_inf gate fails, the fraction is reported faithfully
        let mut a = solid(32, 32, [100, 100, 100, 255]);
        let b = solid(32, 32, [100, 100, 100, 255]);
        a.put_pixel(0, 0, Rgba([150, 100, 100, 255]));
        a.put_pixel(1, 1, Rgba([150, 100, 100, 255]));

        // (1)+(3): l_inf gate fails → fraction is reported = 2/1024.
        let r = compare_images(
            &a,
            &b,
            Thresholds {
                min_ssim: 0.0,
                max_l_inf: 49,
                max_changed_pixel_fraction: 0.0,
                min_window_ssim: 0.0,
            },
        )
        .unwrap();
        assert!(!r.passed);
        assert_eq!(r.metrics.changed_pixels, 2);
        assert!((r.metrics.changed_pixel_fraction - 2.0 / 1024.0).abs() < 1e-12);

        // (2): pulling max_l_inf above the actual delta makes the l_inf gate
        // pass; changed_pixels collapses to 0; fraction collapses to 0;
        // tightening max_changed_pixel_fraction to 0.0 is therefore a no-op.
        let pass = compare_images(
            &a,
            &b,
            Thresholds {
                min_ssim: 0.0,
                max_l_inf: 100,
                max_changed_pixel_fraction: 0.0,
                min_window_ssim: 0.0,
            },
        )
        .unwrap();
        assert!(pass.passed, "metrics={:?}", pass.metrics);
        assert_eq!(pass.metrics.changed_pixels, 0);
        assert_eq!(pass.metrics.changed_pixel_fraction, 0.0);
    }

    // ── 8. SSIM threshold override drives the fail path ──────────────────────

    #[test]
    fn ssim_threshold_above_one_always_fails() {
        let a = solid(32, 32, [50, 50, 50, 255]);
        let b = solid(32, 32, [50, 50, 50, 255]);
        // 1.0 is already saturated → require >1 forces fail no matter what.
        let r = compare_images(
            &a,
            &b,
            Thresholds {
                min_ssim: 1.0001,
                ..Thresholds::default()
            },
        )
        .unwrap();
        assert!(!r.passed);
    }

    // ── 9. Diff PNG generation: red where over-tolerance, gray elsewhere ─────

    #[test]
    fn diff_png_marks_only_changed_pixels_red() {
        let mut a = solid(32, 32, [10, 10, 10, 255]);
        let b = solid(32, 32, [10, 10, 10, 255]);
        // Three pixels well over default max_l_inf=8.
        a.put_pixel(5, 5, Rgba([200, 0, 0, 255]));
        a.put_pixel(10, 10, Rgba([0, 200, 0, 255]));
        a.put_pixel(20, 20, Rgba([0, 0, 200, 255]));

        let r = compare_images(&a, &b, Thresholds::default()).unwrap();

        assert_eq!(r.diff.dimensions(), (32, 32));
        assert_eq!(r.diff.get_pixel(5, 5).0, [255, 0, 0, 255]);
        assert_eq!(r.diff.get_pixel(10, 10).0, [255, 0, 0, 255]);
        assert_eq!(r.diff.get_pixel(20, 20).0, [255, 0, 0, 255]);
        // Non-diff pixels: gray-with-low-alpha (alpha = 96).
        let unchanged = r.diff.get_pixel(0, 0).0;
        assert_eq!(unchanged[3], 96);
        assert_eq!(unchanged[0], unchanged[1]);
        assert_eq!(unchanged[1], unchanged[2]);
        assert_eq!(red_pixels(&r.diff), 3);
    }

    // ── 10. Diff PNG handles ALL-DIFFERENT case without overflow ─────────────

    #[test]
    fn diff_png_all_different_no_overflow() {
        let a = solid(32, 32, [255, 255, 255, 255]);
        let b = solid(32, 32, [0, 0, 0, 255]);
        let r = compare_images(&a, &b, Thresholds::default()).unwrap();
        assert!(!r.passed);
        assert_eq!(r.metrics.l_inf, 255);
        assert_eq!(r.metrics.changed_pixels, 32 * 32);
        assert_eq!(r.metrics.total_pixels, 32 * 32);
        assert_eq!(red_pixels(&r.diff), 32 * 32);
    }

    // ── 11. Performance budget ───────────────────────────────────────────────

    #[test]
    fn comparator_meets_perf_budget_on_800x600() {
        // Polish-pass corrected budget: <30ms for 800x600 fixtures (release
        // profile). Debug builds are roughly 3-5x slower; CI hosts add more
        // jitter on top. Pick a generous unit-test budget (1s) that still
        // catches catastrophic regressions without flaking under load.
        let a = checker(800, 600, [16, 16, 16, 255], [240, 240, 240, 255]);
        let b = checker(800, 600, [16, 16, 16, 255], [240, 240, 240, 255]);
        let start = std::time::Instant::now();
        let r = compare_images(&a, &b, Thresholds::default()).unwrap();
        let elapsed = start.elapsed();
        assert!(r.passed);
        assert!(
            elapsed < std::time::Duration::from_millis(1000),
            "comparator took {:?} on 800x600 (debug budget 1000ms; \
             release-profile target is <30ms)",
            elapsed
        );
    }

    // ── 12. Alpha-channel detection ──────────────────────────────────────────

    #[test]
    fn alpha_only_diff_is_detected() {
        // Identical RGB but A differs by 50 → contributes to l_inf.
        let a = solid(32, 32, [100, 100, 100, 200]);
        let b = solid(32, 32, [100, 100, 100, 250]);
        let r = compare_images(&a, &b, Thresholds::default()).unwrap();
        assert_eq!(r.metrics.l_inf, 50);
        assert!(!r.passed);
    }

    // ── 13. Degenerate sizes ─────────────────────────────────────────────────

    #[test]
    fn one_by_one_image_works() {
        let a = solid(1, 1, [10, 20, 30, 255]);
        let b = solid(1, 1, [10, 20, 30, 255]);
        let r = compare_images(&a, &b, Thresholds::default()).unwrap();
        assert!(r.passed);
        assert_eq!(r.metrics.total_pixels, 1);
    }

    #[test]
    fn one_pixel_wide_strip_works() {
        let a = solid(1, 64, [10, 20, 30, 255]);
        let mut b = solid(1, 64, [10, 20, 30, 255]);
        b.put_pixel(0, 32, Rgba([100, 20, 30, 255]));
        let r = compare_images(&a, &b, Thresholds::default()).unwrap();
        assert!(!r.passed);
        assert_eq!(r.metrics.l_inf, 90);
        assert_eq!(r.metrics.changed_pixels, 1);
    }

    #[test]
    fn zero_height_image_passes_vacuously() {
        let a = RgbaImage::new(32, 0);
        let b = RgbaImage::new(32, 0);
        let r = compare_images(&a, &b, Thresholds::default()).unwrap();
        assert!(r.passed);
        assert_eq!(r.metrics.total_pixels, 0);
        assert_eq!(r.metrics.changed_pixel_fraction, 0.0);
        assert!((r.metrics.ssim - 1.0).abs() < 1e-9);
    }

    // ── 14. SSIM properties ──────────────────────────────────────────────────

    #[test]
    fn ssim_identical_images_is_one() {
        let a = checker(32, 32, [10, 20, 30, 255], [200, 210, 220, 255]);
        let s = ssim_luma(&a, &a);
        assert!((s - 1.0).abs() < 1e-9, "ssim(a,a) = {s}");
    }

    #[test]
    fn ssim_constant_images_is_one() {
        // SSIM is well-defined for two constant images via the c1/c2 terms;
        // it must return 1.0 (not NaN) when both variances are zero.
        let a = solid(32, 32, [128, 128, 128, 255]);
        let b = solid(32, 32, [128, 128, 128, 255]);
        let s = ssim_luma(&a, &b);
        assert!(s.is_finite());
        assert!((s - 1.0).abs() < 1e-9);
    }

    #[test]
    fn ssim_drops_when_local_block_changes() {
        let a = solid(64, 64, [10, 10, 10, 255]);
        let mut b = solid(64, 64, [10, 10, 10, 255]);
        // Half the image goes white.
        for y in 0..32 {
            for x in 0..64 {
                b.put_pixel(x, y, Rgba([245, 245, 245, 255]));
            }
        }
        let s = ssim_luma(&a, &b);
        assert!(s < 0.99, "ssim={s} should fall well below 0.99");
    }

    // ── 15. Thresholds defaults are stable contract ──────────────────────────

    #[test]
    fn default_thresholds_contract() {
        let t = Thresholds::default();
        assert_eq!(t.min_ssim, 0.99);
        assert_eq!(t.max_l_inf, 8);
        assert_eq!(t.max_changed_pixel_fraction, 0.001);
    }

    #[test]
    fn thresholds_serde_roundtrip() {
        let t = Thresholds {
            min_ssim: 0.97,
            max_l_inf: 12,
            max_changed_pixel_fraction: 0.005,
            min_window_ssim: 0.9,
        };
        let s = serde_json::to_string(&t).unwrap();
        let back: Thresholds = serde_json::from_str(&s).unwrap();
        assert_eq!(t, back);
    }

    #[test]
    fn legacy_thresholds_without_a_window_floor_disable_it() {
        let back: Thresholds = serde_json::from_str(
            r#"{"min_ssim":0.99,"max_l_inf":8,"max_changed_pixel_fraction":0.001}"#,
        )
        .unwrap();
        assert_eq!(back, Thresholds::default());
        assert_eq!(back.min_window_ssim, 0.0);
    }

    // ── 16. macOS screen-capture prompt contamination guard ─────────────────

    #[test]
    fn macos_screen_capture_prompt_detector_rejects_plain_terminal_frame() {
        let mut terminal = solid(944, 480, [0, 0, 0, 255]);
        fill_rect(&mut terminal, 0, 0, 120, 24, [255, 255, 255, 255]);

        assert!(!detect_macos_screen_capture_prompt_contamination(&terminal));
    }

    #[test]
    fn macos_screen_capture_prompt_detector_finds_permission_dialog_shape() {
        let mut frame = solid(944, 480, [0, 0, 0, 255]);
        fill_rect(&mut frame, 302, 25, 259, 328, [238, 238, 238, 255]);
        fill_rect(&mut frame, 318, 276, 228, 28, [0, 122, 255, 255]);
        fill_rect(&mut frame, 437, 78, 27, 27, [255, 69, 58, 255]);

        assert!(detect_macos_screen_capture_prompt_contamination(&frame));
    }

    // ── 17. Property-based: noise within tolerance always passes ─────────────

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn prop_zero_delta_always_passes(
            r in 0u8..=255,
            g in 0u8..=255,
            b in 0u8..=255,
        ) {
            // Identical solid-color images must always pass — strongest
            // pre-condition for the comparator.
            let a = solid(32, 32, [r, g, b, 255]);
            let b_img = solid(32, 32, [r, g, b, 255]);
            let result = compare_images(&a, &b_img, Thresholds::default()).unwrap();
            prop_assert!(result.passed, "metrics={:?}", result.metrics);
            prop_assert_eq!(result.metrics.l_inf, 0);
            prop_assert_eq!(result.metrics.changed_pixels, 0);
        }

        #[test]
        fn prop_uniform_delta_within_l_inf_keeps_changed_pixels_zero(
            base in 64u8..160,
            delta in 0u8..=8,
        ) {
            // Default max_l_inf is 8: a uniform shift of delta ≤ 8 must keep
            // changed_pixels at 0 and l_inf at exactly `delta`. Note we do
            // not assert PASS — for very low-variance inputs the SSIM gate
            // can fail even at small deltas (intentional behavior of the
            // luma-SSIM metric on near-constant images). The hard contract
            // here is the per-pixel counting metric.
            let shifted = base.saturating_add(delta);
            let a = solid(32, 32, [base, base, base, 255]);
            let b = solid(32, 32, [shifted, shifted, shifted, 255]);
            let r = compare_images(&a, &b, Thresholds::default()).unwrap();
            prop_assert_eq!(r.metrics.l_inf, delta);
            prop_assert_eq!(r.metrics.changed_pixels, 0);
            prop_assert_eq!(red_pixels(&r.diff), 0);
        }

        #[test]
        fn prop_single_overshoot_pixel_always_fails(
            base in 8u8..=128,
            overshoot in 9u8..=120,
        ) {
            // One pixel set to an over-tolerance value → must fail (l_inf gate).
            // Use saturating_add so the property holds across the full base
            // range without u8 overflow.
            let target = base.saturating_add(overshoot);
            let actual_overshoot = target - base; // may be < overshoot if saturated
            // Skip cases where saturation reduces the actual overshoot below
            // the gate (max_l_inf=8) — that would invalidate the precondition.
            prop_assume!(actual_overshoot > 8);

            let a = solid(32, 32, [base, base, base, 255]);
            let mut b = solid(32, 32, [base, base, base, 255]);
            b.put_pixel(7, 7, Rgba([target, base, base, 255]));
            let r = compare_images(&a, &b, Thresholds::default()).unwrap();
            prop_assert!(!r.passed);
            prop_assert_eq!(r.metrics.l_inf, actual_overshoot);
            prop_assert_eq!(r.metrics.changed_pixels, 1);
            prop_assert_eq!(red_pixels(&r.diff), 1);
        }

        #[test]
        fn prop_dimension_mismatch_always_errors(
            w_a in 1u32..=8,
            h_a in 1u32..=8,
            dw in 1u32..=4,
        ) {
            let a = solid(w_a, h_a, [0, 0, 0, 255]);
            let b = solid(w_a + dw, h_a, [0, 0, 0, 255]);
            // proptest's `prop_assert!` runs the expression through
            // `concat!`-style format-string parsing for failure
            // messages; `matches!(..., Err(... { .. }))` confuses that
            // parser on the literal `{`. Materialize the match first
            // and assert the bool, per MEMORY.md varbincode-skip /
            // proptest pitfall lessons.
            let dimension_mismatch = matches!(
                compare_images(&a, &b, Thresholds::default()),
                Err(CompareError::DimensionMismatch { .. })
            );
            prop_assert!(dimension_mismatch);
        }
    }

    // ── 18. CompareMetrics shape stable for downstream JSON consumers ────────

    #[test]
    fn metrics_serializes_to_expected_keys() {
        let a = solid(8, 8, [10, 10, 10, 255]);
        let r = compare_images(&a, &a, Thresholds::default()).unwrap();
        let v = serde_json::to_value(&r.metrics).unwrap();
        for key in [
            "ssim",
            "min_window_ssim",
            "worst_window",
            "l_inf",
            "channel_max_delta",
            "changed_pixels",
            "total_pixels",
            "changed_pixel_fraction",
            "thresholds",
        ] {
            assert!(v.get(key).is_some(), "metrics missing key `{key}`: {v}");
        }
    }

    // ── 17. Windowed SSIM, channel deltas and heatmap (ft-yccm0.1.10) ───────

    /// A text-like frame: dark background with a 7x12 "glyph" in each 8x14
    /// cell, its strokes varying per cell so windows are not all identical.
    fn text_like(cols: u32, rows: u32) -> RgbaImage {
        let mut img = solid(cols * 8, rows * 14, [12, 12, 16, 255]);
        for row in 0..rows {
            for col in 0..cols {
                let seed = row * 31 + col * 17;
                for y in 1..13 {
                    for x in 1..7 {
                        if (x * 3 + y * 5 + seed) % 4 == 0 {
                            img.put_pixel(col * 8 + x, row * 14 + y, Rgba([220, 220, 210, 255]));
                        }
                    }
                }
            }
        }
        img
    }

    #[test]
    fn windowed_ssim_of_identical_images_is_one_and_covers_the_edges() {
        let a = text_like(10, 3);
        let s = ssim_windowed_luma(&a, &a);
        assert!((s.mean - 1.0).abs() < 1e-9, "{s:?}");
        assert!((s.min - 1.0).abs() < 1e-9, "{s:?}");
        // 80x42: x starts 0,4,..,72 (19); y starts 0,4,..,32 plus the clamped 34 (10).
        assert_eq!(s.windows, 19 * 10);
    }

    #[test]
    fn images_smaller_than_a_window_use_one_global_window() {
        let a = solid(5, 3, [40, 40, 40, 255]);
        let mut b = a.clone();
        b.put_pixel(2, 1, Rgba([200, 200, 200, 255]));
        let s = ssim_windowed_luma(&a, &b);
        assert_eq!(s.windows, 1);
        assert!((s.mean - ssim_luma(&a, &b)).abs() < 1e-9, "{s:?}");
    }

    #[test]
    fn windowed_ssim_catches_a_one_glyph_regression_that_global_ssim_misses() {
        let expected = text_like(64, 8);
        let mut actual = expected.clone();
        // Blank one glyph cell, as a missing-glyph regression would.
        let (cell_x, cell_y) = (37 * 8, 5 * 14);
        for y in cell_y..cell_y + 14 {
            for x in cell_x..cell_x + 8 {
                actual.put_pixel(x, y, Rgba([12, 12, 16, 255]));
            }
        }
        let global = ssim_luma(&actual, &expected);
        let windowed = ssim_windowed_luma(&actual, &expected);
        assert!(
            global > 0.99,
            "global SSIM {global} should barely notice one glyph"
        );
        assert!(
            windowed.min < 0.5,
            "worst window should expose the glyph: {windowed:?}"
        );
        let [wx, wy] = windowed.worst_window;
        assert!(
            wx + SSIM_WINDOW > cell_x
                && wx < cell_x + 8
                && wy + SSIM_WINDOW > cell_y
                && wy < cell_y + 14,
            "worst window {:?} should overlap the blanked cell at ({cell_x}, {cell_y})",
            windowed.worst_window
        );
        let gated = compare_images(
            &actual,
            &expected,
            Thresholds {
                min_ssim: 0.0,
                max_l_inf: 255,
                max_changed_pixel_fraction: 1.0,
                min_window_ssim: 0.9,
            },
        )
        .unwrap();
        assert!(!gated.passed, "the window floor must fail the comparison");
    }

    /// The corpus self-test: an intentionally perturbed frame (every glyph
    /// shifted one pixel right, as a cell-origin bug would) drops below the
    /// 0.995 parity threshold.
    #[test]
    fn perturbed_frame_drops_below_the_parity_threshold() {
        let expected = text_like(40, 6);
        let mut actual = solid(expected.width(), expected.height(), [12, 12, 16, 255]);
        for y in 0..expected.height() {
            for x in 1..expected.width() {
                actual.put_pixel(x, y, *expected.get_pixel(x - 1, y));
            }
        }
        let result = compare_images(
            &actual,
            &expected,
            Thresholds {
                min_ssim: 0.995,
                max_l_inf: 255,
                max_changed_pixel_fraction: 1.0,
                min_window_ssim: 0.0,
            },
        )
        .unwrap();
        assert!(result.metrics.ssim < 0.995, "{:?}", result.metrics);
        assert!(!result.passed);
    }

    #[test]
    fn channel_max_delta_is_reported_per_channel() {
        let expected = solid(16, 16, [100, 100, 100, 200]);
        let mut actual = expected.clone();
        actual.put_pixel(3, 3, Rgba([103, 100, 93, 200]));
        actual.put_pixel(9, 12, Rgba([100, 100, 100, 201]));
        let r = compare_images(&actual, &expected, Thresholds::default()).unwrap();
        assert_eq!(r.metrics.channel_max_delta, [3, 0, 7, 1]);
        assert_eq!(r.metrics.l_inf, 7);
    }

    #[test]
    fn heatmap_ramps_with_the_delta_and_dims_unchanged_pixels() {
        let expected = solid(8, 8, [200, 200, 200, 255]);
        let mut actual = expected.clone();
        actual.put_pixel(1, 1, Rgba([0, 0, 0, 255]));
        actual.put_pixel(2, 2, Rgba([196, 200, 200, 255]));
        let r = compare_images(&actual, &expected, Thresholds::default()).unwrap();
        assert_eq!(r.heatmap.dimensions(), (8, 8));
        // Unchanged: the expected luminance dimmed to 30%.
        assert_eq!(r.heatmap.get_pixel(0, 0).0, [60, 60, 60, 255]);
        // Delta 200: between the yellow and white stops.
        let big = r.heatmap.get_pixel(1, 1).0;
        assert!(big[0] == 255 && big[1] > 0 && big[2] < 255, "{big:?}");
        // Delta 4: dark blue.
        let small = r.heatmap.get_pixel(2, 2).0;
        assert!(small[2] > small[0] && small[2] > small[1], "{small:?}");
        assert_eq!(heat_pixel(255, [0, 0, 0, 255]).0, [255, 255, 255, 255]);
    }
}
