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
    pub min_ssim: f64,
    pub max_l_inf: u8,
    pub max_changed_pixel_fraction: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            min_ssim: 0.99,
            max_l_inf: 8,
            max_changed_pixel_fraction: 0.001,
        }
    }
}

/// Quantitative output of a single comparator run.
#[derive(Debug, Clone, Serialize)]
pub struct CompareMetrics {
    pub ssim: f64,
    pub l_inf: u8,
    pub changed_pixels: u64,
    pub total_pixels: u64,
    pub changed_pixel_fraction: f64,
    pub thresholds: Thresholds,
}

/// Pass/fail verdict plus a diff visualization PNG.
#[derive(Debug)]
pub struct CompareResult {
    pub passed: bool,
    pub metrics: CompareMetrics,
    /// RGBA image highlighting differing pixels in red and showing matching
    /// pixels at their original luminance with reduced alpha.
    pub diff: RgbaImage,
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
    let mut diff = RgbaImage::new(actual_width, actual_height);

    for y in 0..actual_height {
        for x in 0..actual_width {
            let a = actual.get_pixel(x, y).0;
            let e = expected.get_pixel(x, y).0;
            let pixel_delta = a
                .iter()
                .zip(e.iter())
                .map(|(left, right)| left.abs_diff(*right))
                .max()
                .unwrap_or(0);
            l_inf = cmp::max(l_inf, pixel_delta);
            if pixel_delta > thresholds.max_l_inf {
                changed_pixels += 1;
                diff.put_pixel(x, y, Rgba([255, 0, 0, 255]));
            } else {
                let shade = ((u16::from(a[0]) + u16::from(a[1]) + u16::from(a[2])) / 3) as u8;
                diff.put_pixel(x, y, Rgba([shade, shade, shade, 96]));
            }
        }
    }

    let changed_pixel_fraction = if total_pixels == 0 {
        0.0
    } else {
        changed_pixels as f64 / total_pixels as f64
    };
    let ssim = ssim_luma(actual, expected);
    let passed = ssim >= thresholds.min_ssim
        && l_inf <= thresholds.max_l_inf
        && changed_pixel_fraction <= thresholds.max_changed_pixel_fraction;

    Ok(CompareResult {
        passed,
        metrics: CompareMetrics {
            ssim,
            l_inf,
            changed_pixels,
            total_pixels,
            changed_pixel_fraction,
            thresholds,
        },
        diff,
    })
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

    let c1 = (0.01_f64 * 255.0).powi(2);
    let c2 = (0.03_f64 * 255.0).powi(2);
    ((2.0 * mean_x * mean_y + c1) * (2.0 * cov_xy + c2))
        / ((mean_x.powi(2) + mean_y.powi(2) + c1) * (var_x + var_y + c2))
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
        assert!(r.passed, "delta == max_l_inf must pass; metrics={:?}", r.metrics);
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
        assert!(!r.passed, "delta == max_l_inf+1 must fail; metrics={:?}", r.metrics);
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
        };
        let s = serde_json::to_string(&t).unwrap();
        let back: Thresholds = serde_json::from_str(&s).unwrap();
        assert_eq!(t, back);
    }

    // ── 16. Property-based: noise within tolerance always passes ─────────────

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

    // ── 17. CompareMetrics shape stable for downstream JSON consumers ────────

    #[test]
    fn metrics_serializes_to_expected_keys() {
        let a = solid(8, 8, [10, 10, 10, 255]);
        let r = compare_images(&a, &a, Thresholds::default()).unwrap();
        let v = serde_json::to_value(&r.metrics).unwrap();
        for key in [
            "ssim",
            "l_inf",
            "changed_pixels",
            "total_pixels",
            "changed_pixel_fraction",
            "thresholds",
        ] {
            assert!(v.get(key).is_some(), "metrics missing key `{key}`: {v}");
        }
    }
}
