//! Persistent-topology metrics for rendered glyph bitmaps.
//!
//! The crate treats a glyph alpha/luma plane as an ink super-level filtration:
//! pixels with `value >= threshold` are foreground, thresholds descend through
//! the nonzero pixel values, and background stays inactive at the zero floor.
//! This is a deliberately small H0/H1 implementation for terminal glyph
//! regression tests, not a replacement for a full cubical-complex package.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::error::Error;
use std::fmt::{Display, Formatter};

const H0_DIMENSION: u8 = 0;
const H1_DIMENSION: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitmapSizeError {
    pub width: usize,
    pub height: usize,
    pub expected_len: Option<usize>,
    pub actual_len: usize,
}

impl Display for BitmapSizeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self.expected_len {
            Some(expected_len) => write!(
                formatter,
                "bitmap {}x{} expected {expected_len} pixel(s), got {}",
                self.width, self.height, self.actual_len
            ),
            None => write!(
                formatter,
                "bitmap {}x{} dimensions overflow usize; got {} pixel(s)",
                self.width, self.height, self.actual_len
            ),
        }
    }
}

impl Error for BitmapSizeError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrayBitmap {
    width: usize,
    height: usize,
    pixels: Vec<u8>,
}

impl GrayBitmap {
    pub fn new(width: usize, height: usize, pixels: Vec<u8>) -> Result<Self, BitmapSizeError> {
        let expected_len = width.checked_mul(height).ok_or(BitmapSizeError {
            width,
            height,
            expected_len: None,
            actual_len: pixels.len(),
        })?;

        if pixels.len() != expected_len {
            return Err(BitmapSizeError {
                width,
                height,
                expected_len: Some(expected_len),
                actual_len: pixels.len(),
            });
        }

        Ok(Self {
            width,
            height,
            pixels,
        })
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BettiSample {
    pub threshold: u8,
    pub beta0: usize,
    pub beta1: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PersistenceFeature {
    pub dimension: u8,
    pub birth: f64,
    pub death: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistenceDiagram {
    pub features: Vec<PersistenceFeature>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TopologyThresholds {
    pub max_h0_bottleneck: f64,
    pub max_h1_bottleneck: f64,
}

impl TopologyThresholds {
    pub const fn new(max_h0_bottleneck: f64, max_h1_bottleneck: f64) -> Self {
        Self {
            max_h0_bottleneck,
            max_h1_bottleneck,
        }
    }
}

impl Default for TopologyThresholds {
    fn default() -> Self {
        Self {
            max_h0_bottleneck: 2.0,
            max_h1_bottleneck: 2.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TopologyDistance {
    pub h0_bottleneck: f64,
    pub h1_bottleneck: f64,
    pub max_bottleneck: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TopologyComparison {
    pub oracle_diagram: PersistenceDiagram,
    pub subject_diagram: PersistenceDiagram,
    pub distance: TopologyDistance,
    pub thresholds: TopologyThresholds,
    pub passed: bool,
}

pub fn betti_curve(bitmap: &GrayBitmap) -> Vec<BettiSample> {
    let thresholds = nonzero_thresholds(bitmap);
    if thresholds.is_empty() {
        return vec![BettiSample {
            threshold: 0,
            beta0: 0,
            beta1: 0,
        }];
    }

    thresholds
        .into_iter()
        .map(|threshold| {
            let active = active_mask(bitmap, threshold);
            let (beta0, beta1) =
                foreground_components_and_holes(bitmap.width, bitmap.height, &active);
            BettiSample {
                threshold,
                beta0,
                beta1,
            }
        })
        .collect()
}

pub fn persistence_diagram(bitmap: &GrayBitmap) -> PersistenceDiagram {
    let mut features = Vec::new();
    let mut open_h0 = Vec::new();
    let mut open_h1 = Vec::new();

    for sample in betti_curve(bitmap) {
        let threshold = f64::from(sample.threshold);
        update_open_intervals(
            sample.beta0,
            threshold,
            H0_DIMENSION,
            &mut open_h0,
            &mut features,
        );
        update_open_intervals(
            sample.beta1,
            threshold,
            H1_DIMENSION,
            &mut open_h1,
            &mut features,
        );
    }

    close_remaining_intervals(H0_DIMENSION, &mut open_h0, &mut features);
    close_remaining_intervals(H1_DIMENSION, &mut open_h1, &mut features);
    features.sort_by(|left, right| {
        left.dimension
            .cmp(&right.dimension)
            .then_with(|| right.birth.total_cmp(&left.birth))
            .then_with(|| right.death.total_cmp(&left.death))
    });

    PersistenceDiagram { features }
}

pub fn compare_bitmaps(
    oracle: &GrayBitmap,
    subject: &GrayBitmap,
    thresholds: TopologyThresholds,
) -> TopologyComparison {
    let oracle_diagram = persistence_diagram(oracle);
    let subject_diagram = persistence_diagram(subject);
    let distance = diagram_distance(&oracle_diagram, &subject_diagram);
    let passed = distance.h0_bottleneck <= thresholds.max_h0_bottleneck
        && distance.h1_bottleneck <= thresholds.max_h1_bottleneck;

    TopologyComparison {
        oracle_diagram,
        subject_diagram,
        distance,
        thresholds,
        passed,
    }
}

pub fn diagram_distance(left: &PersistenceDiagram, right: &PersistenceDiagram) -> TopologyDistance {
    let h0_bottleneck = bottleneck_distance(left, right, H0_DIMENSION);
    let h1_bottleneck = bottleneck_distance(left, right, H1_DIMENSION);
    TopologyDistance {
        h0_bottleneck,
        h1_bottleneck,
        max_bottleneck: h0_bottleneck.max(h1_bottleneck),
    }
}

pub fn bottleneck_distance(
    left: &PersistenceDiagram,
    right: &PersistenceDiagram,
    dimension: u8,
) -> f64 {
    let left_features = features_for_dimension(left, dimension);
    let right_features = features_for_dimension(right, dimension);
    let matrix_size = left_features.len() + right_features.len();

    if matrix_size == 0 {
        return 0.0;
    }

    let cost_matrix = bottleneck_cost_matrix(&left_features, &right_features);
    let mut candidates = finite_costs(&cost_matrix);
    candidates.sort_by(f64::total_cmp);
    candidates.dedup_by(|left_cost, right_cost| (*left_cost - *right_cost).abs() <= f64::EPSILON);

    for candidate in candidates {
        if has_perfect_matching_at_cost(&cost_matrix, candidate) {
            return candidate;
        }
    }

    f64::INFINITY
}

fn nonzero_thresholds(bitmap: &GrayBitmap) -> Vec<u8> {
    let mut seen = [false; 256];
    for pixel in &bitmap.pixels {
        if *pixel > 0 {
            seen[usize::from(*pixel)] = true;
        }
    }

    (1u8..=u8::MAX)
        .rev()
        .filter(|threshold| seen[usize::from(*threshold)])
        .collect()
}

fn active_mask(bitmap: &GrayBitmap, threshold: u8) -> Vec<bool> {
    bitmap
        .pixels
        .iter()
        .map(|pixel| *pixel >= threshold)
        .collect()
}

fn foreground_components_and_holes(width: usize, height: usize, active: &[bool]) -> (usize, usize) {
    let foreground_components = count_components(width, height, active, true).count;
    let background = count_components(width, height, active, false);

    (foreground_components, background.interior_components)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ComponentCount {
    count: usize,
    interior_components: usize,
}

fn count_components(
    width: usize,
    height: usize,
    active: &[bool],
    target_value: bool,
) -> ComponentCount {
    let mut visited = vec![false; active.len()];
    let mut count = 0usize;
    let mut interior_components = 0usize;

    for start_index in 0..active.len() {
        if visited[start_index] || active[start_index] != target_value {
            continue;
        }

        count += 1;
        let touches_border = visit_component(
            start_index,
            width,
            height,
            active,
            target_value,
            &mut visited,
        );
        if !touches_border {
            interior_components += 1;
        }
    }

    ComponentCount {
        count,
        interior_components,
    }
}

fn visit_component(
    start_index: usize,
    width: usize,
    height: usize,
    active: &[bool],
    target_value: bool,
    visited: &mut [bool],
) -> bool {
    let mut touches_border = is_border_index(start_index, width, height);
    let mut queue = VecDeque::from([start_index]);
    visited[start_index] = true;

    while let Some(current_index) = queue.pop_front() {
        for_each_neighbor(current_index, width, height, |neighbor_index| {
            if !visited[neighbor_index] && active[neighbor_index] == target_value {
                visited[neighbor_index] = true;
                touches_border |= is_border_index(neighbor_index, width, height);
                queue.push_back(neighbor_index);
            }
        });
    }

    touches_border
}

fn is_border_index(index: usize, width: usize, height: usize) -> bool {
    if width == 0 || height == 0 {
        return true;
    }

    let x = index % width;
    let y = index / width;
    x == 0 || y == 0 || x + 1 == width || y + 1 == height
}

fn for_each_neighbor(index: usize, width: usize, height: usize, mut visit: impl FnMut(usize)) {
    if width == 0 || height == 0 {
        return;
    }

    let x = index % width;
    let y = index / width;

    if x > 0 {
        visit(index - 1);
    }
    if x + 1 < width {
        visit(index + 1);
    }
    if y > 0 {
        visit(index - width);
    }
    if y + 1 < height {
        visit(index + width);
    }
}

fn update_open_intervals(
    observed_count: usize,
    threshold: f64,
    dimension: u8,
    open_births: &mut Vec<f64>,
    features: &mut Vec<PersistenceFeature>,
) {
    let open_count = open_births.len();
    if observed_count > open_count {
        open_births.extend(std::iter::repeat_n(threshold, observed_count - open_count));
    } else if observed_count < open_count {
        for _ in 0..(open_count - observed_count) {
            if let Some(birth) = open_births.pop() {
                push_feature_if_persistent(features, dimension, birth, threshold);
            }
        }
    }
}

fn close_remaining_intervals(
    dimension: u8,
    open_births: &mut Vec<f64>,
    features: &mut Vec<PersistenceFeature>,
) {
    while let Some(birth) = open_births.pop() {
        push_feature_if_persistent(features, dimension, birth, 0.0);
    }
}

fn push_feature_if_persistent(
    features: &mut Vec<PersistenceFeature>,
    dimension: u8,
    birth: f64,
    death: f64,
) {
    if birth > death {
        features.push(PersistenceFeature {
            dimension,
            birth,
            death,
        });
    }
}

fn features_for_dimension(diagram: &PersistenceDiagram, dimension: u8) -> Vec<PersistenceFeature> {
    diagram
        .features
        .iter()
        .copied()
        .filter(|feature| feature.dimension == dimension)
        .collect()
}

fn bottleneck_cost_matrix(
    left_features: &[PersistenceFeature],
    right_features: &[PersistenceFeature],
) -> Vec<Vec<f64>> {
    let matrix_size = left_features.len() + right_features.len();
    let mut costs = vec![vec![f64::INFINITY; matrix_size]; matrix_size];

    for (left_index, left_feature) in left_features.iter().enumerate() {
        for (right_index, right_feature) in right_features.iter().enumerate() {
            costs[left_index][right_index] = feature_distance(*left_feature, *right_feature);
        }
        costs[left_index][right_features.len() + left_index] = diagonal_distance(*left_feature);
    }

    for (right_index, right_feature) in right_features.iter().enumerate() {
        costs[left_features.len() + right_index][right_index] = diagonal_distance(*right_feature);
        for left_index in 0..left_features.len() {
            costs[left_features.len() + right_index][right_features.len() + left_index] = 0.0;
        }
    }

    costs
}

fn feature_distance(left: PersistenceFeature, right: PersistenceFeature) -> f64 {
    (left.birth - right.birth)
        .abs()
        .max((left.death - right.death).abs())
}

fn diagonal_distance(feature: PersistenceFeature) -> f64 {
    (feature.birth - feature.death).abs() / 2.0
}

fn finite_costs(cost_matrix: &[Vec<f64>]) -> Vec<f64> {
    cost_matrix
        .iter()
        .flat_map(|row| row.iter().copied())
        .filter(|cost| cost.is_finite())
        .collect()
}

fn has_perfect_matching_at_cost(cost_matrix: &[Vec<f64>], max_cost: f64) -> bool {
    let matrix_size = cost_matrix.len();
    let mut matched_by_column = vec![None; matrix_size];

    for row_index in 0..matrix_size {
        let mut seen_columns = vec![false; matrix_size];
        if !augment_matching(
            row_index,
            cost_matrix,
            max_cost,
            &mut seen_columns,
            &mut matched_by_column,
        ) {
            return false;
        }
    }

    true
}

fn augment_matching(
    row_index: usize,
    cost_matrix: &[Vec<f64>],
    max_cost: f64,
    seen_columns: &mut [bool],
    matched_by_column: &mut [Option<usize>],
) -> bool {
    for (column_index, cost) in cost_matrix[row_index].iter().enumerate() {
        if *cost > max_cost || seen_columns[column_index] {
            continue;
        }

        seen_columns[column_index] = true;
        if let Some(matched_row) = matched_by_column[column_index] {
            if !augment_matching(
                matched_row,
                cost_matrix,
                max_cost,
                seen_columns,
                matched_by_column,
            ) {
                continue;
            }
        }

        matched_by_column[column_index] = Some(row_index);
        return true;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bitmap(width: usize, height: usize, pixels: &[u8]) -> GrayBitmap {
        GrayBitmap::new(width, height, pixels.to_vec()).expect("valid bitmap")
    }

    fn hollow_square() -> GrayBitmap {
        bitmap(
            5,
            5,
            &[
                255, 255, 255, 255, 255, 255, 0, 0, 0, 255, 255, 0, 0, 0, 255, 255, 0, 0, 0, 255,
                255, 255, 255, 255, 255,
            ],
        )
    }

    #[test]
    fn rejects_mismatched_pixel_count() {
        let error = GrayBitmap::new(2, 3, vec![0; 5]).expect_err("size mismatch");
        assert_eq!(error.expected_len, Some(6));
        assert_eq!(error.actual_len, 5);
    }

    #[test]
    fn empty_bitmap_has_zero_betti_numbers() {
        let image = bitmap(3, 3, &[0; 9]);
        assert_eq!(
            betti_curve(&image),
            vec![BettiSample {
                threshold: 0,
                beta0: 0,
                beta1: 0
            }]
        );
        assert!(persistence_diagram(&image).features.is_empty());
    }

    #[test]
    fn filled_square_has_one_connected_component_and_no_holes() {
        let image = bitmap(3, 3, &[255; 9]);
        let curve = betti_curve(&image);
        assert_eq!(curve[0].beta0, 1);
        assert_eq!(curve[0].beta1, 0);

        let diagram = persistence_diagram(&image);
        assert_eq!(
            diagram.features,
            vec![PersistenceFeature {
                dimension: H0_DIMENSION,
                birth: 255.0,
                death: 0.0
            }]
        );
    }

    #[test]
    fn hollow_square_exposes_one_h1_loop() {
        let image = hollow_square();
        let curve = betti_curve(&image);
        assert_eq!(curve[0].beta0, 1);
        assert_eq!(curve[0].beta1, 1);

        let diagram = persistence_diagram(&image);
        assert!(
            diagram
                .features
                .iter()
                .any(|feature| feature.dimension == H1_DIMENSION
                    && (feature.birth - 255.0).abs() < f64::EPSILON
                    && (feature.death - 0.0).abs() < f64::EPSILON)
        );
    }

    #[test]
    fn bottleneck_distance_is_zero_for_identical_diagrams() {
        let diagram = persistence_diagram(&hollow_square());
        assert!(bottleneck_distance(&diagram, &diagram, H0_DIMENSION).abs() < f64::EPSILON);
        assert!(bottleneck_distance(&diagram, &diagram, H1_DIMENSION).abs() < f64::EPSILON);
    }

    #[test]
    fn bottleneck_distance_detects_intensity_shift() {
        let full = persistence_diagram(&bitmap(2, 2, &[255; 4]));
        let dimmer = persistence_diagram(&bitmap(2, 2, &[252; 4]));
        let distance = bottleneck_distance(&full, &dimmer, H0_DIMENSION);
        assert!((distance - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn comparison_fails_when_subject_breaks_a_loop() {
        let oracle = hollow_square();
        let subject = bitmap(5, 5, &[255; 25]);
        let comparison = compare_bitmaps(&oracle, &subject, TopologyThresholds::new(2.0, 2.0));

        assert!(!comparison.passed);
        assert!(comparison.distance.h1_bottleneck > 2.0);
    }

    #[test]
    fn comparison_passes_for_antialiasing_with_same_topology() {
        let oracle = hollow_square();
        let subject = bitmap(
            5,
            5,
            &[
                253, 253, 253, 253, 253, 253, 0, 0, 0, 253, 253, 0, 0, 0, 253, 253, 0, 0, 0, 253,
                253, 253, 253, 253, 253,
            ],
        );
        let comparison = compare_bitmaps(&oracle, &subject, TopologyThresholds::new(3.0, 3.0));

        assert!(comparison.passed);
        assert!(comparison.distance.max_bottleneck <= 3.0);
    }
}
