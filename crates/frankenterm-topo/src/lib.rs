//! Persistent-topology metrics for rendered glyph bitmaps.
//!
//! The crate treats a glyph alpha/luma plane as an ink super-level filtration:
//! pixels with `value >= threshold` are foreground, thresholds descend through
//! the nonzero pixel values, and background stays inactive at the zero floor.
//! This is a deliberately small H0/H1 implementation for terminal glyph
//! regression tests, not a replacement for a full cubical-complex package.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::error::Error;
use std::fmt::{Display, Formatter};

const H0_DIMENSION: u8 = 0;
const H1_DIMENSION: u8 = 1;
pub const DEADWIRE_WIRING_STATUS_SCHEMA_VERSION: u32 = 1;
pub const GOVERNED_SUBTRACTION_SCHEMA_VERSION: u32 = 1;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionApiDeclaration {
    pub symbol: String,
    pub defining_path: String,
}

impl DecisionApiDeclaration {
    pub fn new(symbol: impl Into<String>, defining_path: impl Into<String>) -> Self {
        Self {
            symbol: symbol.into(),
            defining_path: defining_path.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionApiSourceFile {
    pub path: String,
    pub text: String,
}

impl DecisionApiSourceFile {
    pub fn new(path: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            text: text.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DormantDecisionApiExemption {
    pub bead_id: String,
    pub expires_on: String,
}

impl DormantDecisionApiExemption {
    pub fn new(bead_id: impl Into<String>, expires_on: impl Into<String>) -> Self {
        Self {
            bead_id: bead_id.into(),
            expires_on: expires_on.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DormantDecisionApiExemptionEntry {
    pub symbol: String,
    pub exemption: DormantDecisionApiExemption,
}

impl DormantDecisionApiExemptionEntry {
    pub fn new(symbol: impl Into<String>, exemption: DormantDecisionApiExemption) -> Self {
        Self {
            symbol: symbol.into(),
            exemption,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionApiCaller {
    pub path: String,
    pub line: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionApiWiringStatus {
    Wired,
    Dormant,
    Deadwire,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeadwireViolationReason {
    MissingProductionCaller,
    MissingDormantBead,
    MissingDormantExpiry,
    InvalidDormantExpiry,
    ExpiredDormantExemption,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeadwireViolation {
    pub symbol: String,
    pub reason: DeadwireViolationReason,
    pub required_action: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionApiWiringRecord {
    pub symbol: String,
    pub defining_path: String,
    pub production_callers: Vec<DecisionApiCaller>,
    pub status: DecisionApiWiringStatus,
    pub dormant_exemption: Option<DormantDecisionApiExemption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionApiWiringReport {
    pub schema_version: u32,
    pub produced_by_bead: String,
    pub generated_at_ms: u64,
    pub records: Vec<DecisionApiWiringRecord>,
    pub violations: Vec<DeadwireViolation>,
    pub passed: bool,
}

/// Reversible disposition for a surface in the governed-subtraction inventory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GovernedSubtractionDisposition {
    Wire,
    Park,
    AttestAsDormant,
}

/// Bead-backed rationale required for any non-wired disposition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GovernedSubtractionProvenance {
    pub bead_id: String,
    pub rationale: String,
}

impl GovernedSubtractionProvenance {
    pub fn new(bead_id: impl Into<String>, rationale: impl Into<String>) -> Self {
        Self {
            bead_id: bead_id.into(),
            rationale: rationale.into(),
        }
    }
}

/// One candidate surface considered for wiring, parking, or dormant attestation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GovernedSubtractionSurface {
    pub surface_id: String,
    pub path: String,
    pub current_disposition: GovernedSubtractionDisposition,
    pub proposed_disposition: GovernedSubtractionDisposition,
    pub provenance: Option<GovernedSubtractionProvenance>,
    pub restore_action: Option<String>,
    pub attestation_ref: Option<String>,
}

/// Report-only decision for a governed-subtraction candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GovernedSubtractionDecision {
    ReportOnlyReviewRequired,
    Blocked,
}

/// Machine-stable reasons emitted by the governed-subtraction planner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GovernedSubtractionReason {
    ReportOnly,
    NoDeletion,
    ExplicitHumanReviewRequired,
    SurfaceBudgetExceeded,
    HardProtectedPath,
    MissingProvenance,
    MissingRestoreAction,
    MissingAttestationReference,
}

/// Per-surface report row. This intentionally has no apply/delete mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GovernedSubtractionPlanItem {
    pub surface: GovernedSubtractionSurface,
    pub decision: GovernedSubtractionDecision,
    pub reasons: Vec<GovernedSubtractionReason>,
    pub mutates_files: bool,
    pub deletes_files: bool,
    pub requires_human_approval: bool,
}

/// Report-only surface-budget plan for reversible governed subtraction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GovernedSubtractionPlan {
    pub schema_version: u32,
    pub produced_by_bead: String,
    pub generated_at_ms: u64,
    pub active_surface_budget: usize,
    pub proposed_wired_surfaces: usize,
    pub report_only: bool,
    pub mutates_files: bool,
    pub deletes_files: bool,
    pub hard_protected_paths: Vec<String>,
    pub items: Vec<GovernedSubtractionPlanItem>,
    pub blocked: usize,
    pub review_required: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct GovernedSubtractionInput<'a> {
    pub surfaces: &'a [GovernedSubtractionSurface],
    pub active_surface_budget: usize,
    pub produced_by_bead: &'a str,
    pub generated_at_ms: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct DecisionApiWiringInput<'a> {
    pub declarations: &'a [DecisionApiDeclaration],
    pub source_files: &'a [DecisionApiSourceFile],
    pub dormant_exemptions: &'a [DormantDecisionApiExemptionEntry],
    pub produced_by_bead: &'a str,
    pub generated_at_ms: u64,
    pub today_utc: &'a str,
}

pub fn analyze_decision_api_wiring(input: DecisionApiWiringInput<'_>) -> DecisionApiWiringReport {
    let dormant_by_symbol: BTreeMap<&str, &DormantDecisionApiExemption> = input
        .dormant_exemptions
        .iter()
        .map(|entry| (entry.symbol.as_str(), &entry.exemption))
        .collect();
    let mut records = Vec::with_capacity(input.declarations.len());
    let mut violations = Vec::new();

    for declaration in input.declarations {
        let production_callers =
            production_callers_for_symbol(&declaration.symbol, input.source_files);
        let dormant_exemption = dormant_by_symbol.get(declaration.symbol.as_str()).copied();
        let mut status = DecisionApiWiringStatus::Wired;

        if production_callers.is_empty() {
            if let Some(exemption) = dormant_exemption {
                let exemption_violations =
                    dormant_exemption_violations(&declaration.symbol, exemption, input.today_utc);
                if exemption_violations.is_empty() {
                    status = DecisionApiWiringStatus::Dormant;
                } else {
                    status = DecisionApiWiringStatus::Deadwire;
                    violations.extend(exemption_violations);
                }
            } else {
                status = DecisionApiWiringStatus::Deadwire;
                violations.push(deadwire_violation(
                    &declaration.symbol,
                    DeadwireViolationReason::MissingProductionCaller,
                    "add a non-test production caller or declare a dormant exemption with bead_id and expires_on",
                ));
            }
        }

        records.push(DecisionApiWiringRecord {
            symbol: declaration.symbol.clone(),
            defining_path: declaration.defining_path.clone(),
            production_callers,
            status,
            dormant_exemption: dormant_exemption.cloned(),
        });
    }

    let passed = violations.is_empty();
    DecisionApiWiringReport {
        schema_version: DEADWIRE_WIRING_STATUS_SCHEMA_VERSION,
        produced_by_bead: input.produced_by_bead.to_string(),
        generated_at_ms: input.generated_at_ms,
        records,
        violations,
        passed,
    }
}

/// Build a no-mutation governed-subtraction plan.
///
/// The planner only classifies surface dispositions. It never deletes files,
/// edits manifests, or treats a parked surface as removed from the repository.
pub fn plan_governed_subtraction(input: GovernedSubtractionInput<'_>) -> GovernedSubtractionPlan {
    let proposed_wired_surfaces = input
        .surfaces
        .iter()
        .filter(|surface| surface.proposed_disposition == GovernedSubtractionDisposition::Wire)
        .count();
    let mut wired_seen = 0;
    let items: Vec<_> = input
        .surfaces
        .iter()
        .cloned()
        .map(|surface| {
            let wired_index =
                if surface.proposed_disposition == GovernedSubtractionDisposition::Wire {
                    wired_seen += 1;
                    Some(wired_seen)
                } else {
                    None
                };
            classify_governed_subtraction_surface(surface, wired_index, input.active_surface_budget)
        })
        .collect();

    let blocked = items
        .iter()
        .filter(|item| item.decision == GovernedSubtractionDecision::Blocked)
        .count();
    let review_required = items.len().saturating_sub(blocked);

    GovernedSubtractionPlan {
        schema_version: GOVERNED_SUBTRACTION_SCHEMA_VERSION,
        produced_by_bead: input.produced_by_bead.to_string(),
        generated_at_ms: input.generated_at_ms,
        active_surface_budget: input.active_surface_budget,
        proposed_wired_surfaces,
        report_only: true,
        mutates_files: false,
        deletes_files: false,
        hard_protected_paths: vec![
            "AGENTS.md".to_string(),
            "crates/frankenterm-core".to_string(),
        ],
        items,
        blocked,
        review_required,
    }
}

fn classify_governed_subtraction_surface(
    surface: GovernedSubtractionSurface,
    wired_index: Option<usize>,
    active_surface_budget: usize,
) -> GovernedSubtractionPlanItem {
    let mut reasons = vec![
        GovernedSubtractionReason::ReportOnly,
        GovernedSubtractionReason::NoDeletion,
        GovernedSubtractionReason::ExplicitHumanReviewRequired,
    ];

    if is_hard_protected_governed_subtraction_path(&surface.path) {
        reasons.push(GovernedSubtractionReason::HardProtectedPath);
    }

    if matches!(
        surface.proposed_disposition,
        GovernedSubtractionDisposition::Park | GovernedSubtractionDisposition::AttestAsDormant
    ) && surface.provenance.as_ref().is_none_or(|provenance| {
        provenance.bead_id.trim().is_empty() || provenance.rationale.trim().is_empty()
    }) {
        reasons.push(GovernedSubtractionReason::MissingProvenance);
    }

    if surface.proposed_disposition == GovernedSubtractionDisposition::Park
        && surface
            .restore_action
            .as_deref()
            .is_none_or(|action| action.trim().is_empty())
    {
        reasons.push(GovernedSubtractionReason::MissingRestoreAction);
    }

    if surface.proposed_disposition == GovernedSubtractionDisposition::AttestAsDormant
        && surface
            .attestation_ref
            .as_deref()
            .is_none_or(|reference| reference.trim().is_empty())
    {
        reasons.push(GovernedSubtractionReason::MissingAttestationReference);
    }

    if wired_index.is_some_and(|index| index > active_surface_budget) {
        reasons.push(GovernedSubtractionReason::SurfaceBudgetExceeded);
    }

    let decision = if reasons.iter().any(|reason| {
        matches!(
            reason,
            GovernedSubtractionReason::SurfaceBudgetExceeded
                | GovernedSubtractionReason::HardProtectedPath
                | GovernedSubtractionReason::MissingProvenance
                | GovernedSubtractionReason::MissingRestoreAction
                | GovernedSubtractionReason::MissingAttestationReference
        )
    }) {
        GovernedSubtractionDecision::Blocked
    } else {
        GovernedSubtractionDecision::ReportOnlyReviewRequired
    };

    GovernedSubtractionPlanItem {
        surface,
        decision,
        reasons,
        mutates_files: false,
        deletes_files: false,
        requires_human_approval: true,
    }
}

fn is_hard_protected_governed_subtraction_path(path: &str) -> bool {
    let trimmed = path.trim();
    let normalized = trimmed
        .strip_prefix("./")
        .unwrap_or(trimmed)
        .trim_end_matches('/');
    normalized == "AGENTS.md"
        || normalized.ends_with("/AGENTS.md")
        || normalized == "crates/frankenterm-core"
        || normalized.starts_with("crates/frankenterm-core/")
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

fn production_callers_for_symbol(
    symbol: &str,
    source_files: &[DecisionApiSourceFile],
) -> Vec<DecisionApiCaller> {
    let mut callers = Vec::new();
    for source_file in source_files {
        if is_test_path(&source_file.path) {
            continue;
        }

        for (line_index, line) in source_file.text.lines().enumerate() {
            if line_mentions_symbol(line, symbol) && !line_declares_symbol(line, symbol) {
                callers.push(DecisionApiCaller {
                    path: source_file.path.clone(),
                    line: line_index + 1,
                });
            }
        }
    }
    callers
}

fn dormant_exemption_violations(
    symbol: &str,
    exemption: &DormantDecisionApiExemption,
    today_utc: &str,
) -> Vec<DeadwireViolation> {
    let mut violations = Vec::new();
    if exemption.bead_id.trim().is_empty() {
        violations.push(deadwire_violation(
            symbol,
            DeadwireViolationReason::MissingDormantBead,
            "set dormant exemption bead_id to the tracking bead that owns wiring the API",
        ));
    }
    if exemption.expires_on.trim().is_empty() {
        violations.push(deadwire_violation(
            symbol,
            DeadwireViolationReason::MissingDormantExpiry,
            "set dormant exemption expires_on to a YYYY-MM-DD date",
        ));
    } else if !is_yyyy_mm_dd(&exemption.expires_on) {
        violations.push(deadwire_violation(
            symbol,
            DeadwireViolationReason::InvalidDormantExpiry,
            "set dormant exemption expires_on to a valid YYYY-MM-DD date",
        ));
    } else if is_yyyy_mm_dd(today_utc) && exemption.expires_on.as_str() < today_utc {
        violations.push(deadwire_violation(
            symbol,
            DeadwireViolationReason::ExpiredDormantExemption,
            "wire the API or renew the dormant exemption with a fresh bead-owned expiry",
        ));
    }
    violations
}

fn deadwire_violation(
    symbol: &str,
    reason: DeadwireViolationReason,
    required_action: &str,
) -> DeadwireViolation {
    DeadwireViolation {
        symbol: symbol.to_string(),
        reason,
        required_action: required_action.to_string(),
    }
}

fn is_test_path(path: &str) -> bool {
    path == "tests.rs"
        || path.starts_with("tests/")
        || path.contains("/tests/")
        || path.ends_with("_test.rs")
        || path.ends_with("_tests.rs")
}

fn line_mentions_symbol(line: &str, symbol: &str) -> bool {
    if symbol.is_empty() {
        return false;
    }

    for (start, _) in line.match_indices(symbol) {
        let bytes = line.as_bytes();
        let before = start
            .checked_sub(1)
            .and_then(|index| bytes.get(index))
            .copied();
        let after = line.as_bytes().get(start + symbol.len()).copied();
        if !before.is_some_and(is_identifier_byte) && !after.is_some_and(is_identifier_byte) {
            return true;
        }
    }
    false
}

fn line_declares_symbol(line: &str, symbol: &str) -> bool {
    let rest = strip_declaration_modifiers(strip_visibility(line.trim_start()));
    let mut tokens = rest.split_whitespace();
    matches!(tokens.next(), Some("fn"))
        && tokens
            .next()
            .is_some_and(|name_token| function_name_matches(name_token, symbol))
}

fn strip_visibility(rest: &str) -> &str {
    for prefix in ["pub ", "pub(crate) ", "pub(super) ", "pub(self) "] {
        if let Some(after) = rest.strip_prefix(prefix) {
            return after;
        }
    }

    if let Some(after_pub) = rest.strip_prefix("pub(") {
        if let Some((_, after_visibility)) = after_pub.split_once(") ") {
            return after_visibility;
        }
    }

    rest
}

fn strip_declaration_modifiers(mut rest: &str) -> &str {
    loop {
        if let Some(after) = rest.strip_prefix("async ") {
            rest = after;
        } else if let Some(after) = rest.strip_prefix("const ") {
            rest = after;
        } else if let Some(after) = rest.strip_prefix("unsafe ") {
            rest = after;
        } else if let Some(after) = rest.strip_prefix("extern ") {
            rest = strip_extern_abi(after);
        } else {
            return rest;
        }
    }
}

fn strip_extern_abi(rest: &str) -> &str {
    let rest = rest.trim_start();
    if let Some(after_quote) = rest.strip_prefix('"') {
        if let Some((_, after_abi)) = after_quote.split_once("\" ") {
            return after_abi;
        }
    }
    rest
}

fn function_name_matches(name_token: &str, symbol: &str) -> bool {
    name_token
        .strip_prefix(symbol)
        .is_some_and(|suffix| suffix.starts_with('(') || suffix.starts_with('<'))
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn is_yyyy_mm_dd(value: &str) -> bool {
    let mut parts = value.split('-');
    let (Some(year), Some(month), Some(day), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };

    let Some(year) = parse_four_digits(year) else {
        return false;
    };
    let Some(month) = parse_two_digits(month) else {
        return false;
    };
    let Some(day) = parse_two_digits(day) else {
        return false;
    };
    let Some(max_day) = days_in_month(year, month) else {
        return false;
    };

    (1..=max_day).contains(&day)
}

fn parse_four_digits(value: &str) -> Option<u16> {
    match value.as_bytes() {
        [
            thousands @ b'0'..=b'9',
            hundreds @ b'0'..=b'9',
            tens @ b'0'..=b'9',
            ones @ b'0'..=b'9',
        ] => Some(
            u16::from(thousands - b'0') * 1000
                + u16::from(hundreds - b'0') * 100
                + u16::from(tens - b'0') * 10
                + u16::from(ones - b'0'),
        ),
        _ => None,
    }
}

fn parse_two_digits(value: &str) -> Option<u8> {
    match value.as_bytes() {
        [tens @ b'0'..=b'9', ones @ b'0'..=b'9'] => Some((tens - b'0') * 10 + (ones - b'0')),
        _ => None,
    }
}

fn days_in_month(year: u16, month: u8) -> Option<u8> {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => Some(31),
        4 | 6 | 9 | 11 => Some(30),
        2 if is_leap_year(year) => Some(29),
        2 => Some(28),
        _ => None,
    }
}

fn is_leap_year(year: u16) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
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

    fn decision_report(
        declarations: &[DecisionApiDeclaration],
        source_files: &[DecisionApiSourceFile],
        dormant_exemptions: &[DormantDecisionApiExemptionEntry],
    ) -> DecisionApiWiringReport {
        analyze_decision_api_wiring(DecisionApiWiringInput {
            declarations,
            source_files,
            dormant_exemptions,
            produced_by_bead: "ft-7h5da.5.5",
            generated_at_ms: 1_704_000_000_000,
            today_utc: "2026-06-14",
        })
    }

    fn governed_surface(
        id: &str,
        path: &str,
        proposed_disposition: GovernedSubtractionDisposition,
    ) -> GovernedSubtractionSurface {
        GovernedSubtractionSurface {
            surface_id: id.to_string(),
            path: path.to_string(),
            current_disposition: GovernedSubtractionDisposition::Wire,
            proposed_disposition,
            provenance: Some(GovernedSubtractionProvenance::new(
                "ft-7h5da.11.14",
                "surface budget review",
            )),
            restore_action: Some("restore by reverting the feature-gate entry".to_string()),
            attestation_ref: Some("docs/attestations/doctrine/wiring-status.json".to_string()),
        }
    }

    fn governed_plan(surfaces: &[GovernedSubtractionSurface]) -> GovernedSubtractionPlan {
        plan_governed_subtraction(GovernedSubtractionInput {
            surfaces,
            active_surface_budget: 8,
            produced_by_bead: "ft-7h5da.11.14",
            generated_at_ms: 1_704_000_000_000,
        })
    }

    #[test]
    fn governed_subtraction_plan_is_report_only_and_deletion_free() {
        let surfaces = [governed_surface(
            "mcp_connector_extract",
            "crates/frankenterm-core-mcp/Cargo.toml",
            GovernedSubtractionDisposition::Park,
        )];

        let plan = governed_plan(&surfaces);

        assert_eq!(plan.schema_version, GOVERNED_SUBTRACTION_SCHEMA_VERSION);
        assert!(plan.report_only);
        assert!(!plan.mutates_files);
        assert!(!plan.deletes_files);
        assert_eq!(plan.review_required, 1);
        assert!(plan.items.iter().any(|item| {
            item.decision == GovernedSubtractionDecision::ReportOnlyReviewRequired
                && !item.mutates_files
                && !item.deletes_files
                && item.requires_human_approval
                && item
                    .reasons
                    .contains(&GovernedSubtractionReason::NoDeletion)
                && item
                    .reasons
                    .contains(&GovernedSubtractionReason::ExplicitHumanReviewRequired)
        }));
    }

    #[test]
    fn governed_subtraction_blocks_protected_paths() {
        for path in [
            "AGENTS.md",
            "docs/AGENTS.md",
            "crates/frankenterm-core",
            "crates/frankenterm-core/src/lib.rs",
            "./crates/frankenterm-core/",
        ] {
            let surfaces = [governed_surface(
                "protected",
                path,
                GovernedSubtractionDisposition::Park,
            )];
            let plan = governed_plan(&surfaces);

            assert!(
                plan.items
                    .iter()
                    .any(|item| item.decision == GovernedSubtractionDecision::Blocked
                        && item
                            .reasons
                            .contains(&GovernedSubtractionReason::HardProtectedPath)
                        && !item.deletes_files),
                "{path}"
            );
        }
    }

    #[test]
    fn governed_subtraction_requires_parking_provenance_and_restore_action() {
        let mut surface = governed_surface(
            "parked_subsystem",
            "crates/frankenterm-core-fleet/Cargo.toml",
            GovernedSubtractionDisposition::Park,
        );
        surface.provenance = None;
        surface.restore_action = None;

        let plan = governed_plan(&[surface]);

        assert!(plan.items.iter().any(|item| {
            item.decision == GovernedSubtractionDecision::Blocked
                && item
                    .reasons
                    .contains(&GovernedSubtractionReason::MissingProvenance)
                && item
                    .reasons
                    .contains(&GovernedSubtractionReason::MissingRestoreAction)
        }));
    }

    #[test]
    fn governed_subtraction_requires_dormant_attestation_reference() {
        let mut surface = governed_surface(
            "dormant_connector",
            "crates/frankenterm-core-connectors/Cargo.toml",
            GovernedSubtractionDisposition::AttestAsDormant,
        );
        surface.attestation_ref = None;

        let plan = governed_plan(&[surface]);

        assert!(plan.items.iter().any(|item| {
            item.decision == GovernedSubtractionDecision::Blocked
                && item
                    .reasons
                    .contains(&GovernedSubtractionReason::MissingAttestationReference)
        }));
    }

    #[test]
    fn governed_subtraction_enforces_active_surface_budget() {
        let surfaces = [
            governed_surface(
                "wire_one",
                "crates/frankenterm-core-resource-types/Cargo.toml",
                GovernedSubtractionDisposition::Wire,
            ),
            governed_surface(
                "wire_two",
                "crates/frankenterm-core-policy-types/Cargo.toml",
                GovernedSubtractionDisposition::Wire,
            ),
        ];
        let plan = plan_governed_subtraction(GovernedSubtractionInput {
            surfaces: &surfaces,
            active_surface_budget: 1,
            produced_by_bead: "ft-7h5da.11.14",
            generated_at_ms: 1_704_000_000_000,
        });

        assert_eq!(plan.items.len(), 2);
        assert!(
            plan.items
                .iter()
                .any(|item| item.surface.surface_id == "wire_one"
                    && item.decision == GovernedSubtractionDecision::ReportOnlyReviewRequired)
        );
        assert!(plan.items.iter().any(|item| {
            item.surface.surface_id == "wire_two"
                && item.decision == GovernedSubtractionDecision::Blocked
                && item
                    .reasons
                    .contains(&GovernedSubtractionReason::SurfaceBudgetExceeded)
        }));
        assert_eq!(plan.proposed_wired_surfaces, 2);
        assert_eq!(plan.blocked, 1);
    }

    #[test]
    fn deadwire_gate_fails_unwired_api_without_dormant_exemption() {
        let declarations = [DecisionApiDeclaration::new(
            "allow_operation",
            "crates/frankenterm-core/src/policy.rs",
        )];
        let source_files = [DecisionApiSourceFile::new(
            "crates/frankenterm-core/src/policy.rs",
            "pub async fn allow_operation() {}\n",
        )];

        let report = decision_report(&declarations, &source_files, &[]);

        assert!(!report.passed);
        assert_eq!(report.records[0].status, DecisionApiWiringStatus::Deadwire);
        assert_eq!(
            report.violations[0].reason,
            DeadwireViolationReason::MissingProductionCaller
        );
    }

    #[test]
    fn deadwire_gate_ignores_test_only_callers() {
        let declarations = [DecisionApiDeclaration::new(
            "evaluate",
            "crates/frankenterm-core/src/governor.rs",
        )];
        let source_files = [
            DecisionApiSourceFile::new(
                "crates/frankenterm-core/src/governor.rs",
                "pub fn evaluate() {}\n",
            ),
            DecisionApiSourceFile::new(
                "crates/frankenterm-core/tests/governor.rs",
                "fn test_uses_api() { policy.evaluate(); }\n",
            ),
        ];

        let report = decision_report(&declarations, &source_files, &[]);

        assert!(!report.passed);
        assert!(report.records[0].production_callers.is_empty());
        assert_eq!(report.records[0].status, DecisionApiWiringStatus::Deadwire);
    }

    #[test]
    fn production_caller_wires_decision_api() {
        let declarations = [DecisionApiDeclaration::new(
            "execute",
            "crates/frankenterm-core/src/decision.rs",
        )];
        let source_files = [
            DecisionApiSourceFile::new(
                "crates/frankenterm-core/src/decision.rs",
                "pub(in crate::decision) async fn execute<T>() {}\n",
            ),
            DecisionApiSourceFile::new(
                "crates/frankenterm-core/src/dispatcher.rs",
                "fn dispatch(decision: &Decision) { decision.execute(); }\n",
            ),
        ];

        let report = decision_report(&declarations, &source_files, &[]);

        assert!(report.passed);
        assert_eq!(report.records[0].status, DecisionApiWiringStatus::Wired);
        assert_eq!(
            report.records[0].production_callers,
            vec![DecisionApiCaller {
                path: "crates/frankenterm-core/src/dispatcher.rs".to_string(),
                line: 1
            }]
        );
    }

    #[test]
    fn valid_dormant_exemption_keeps_unwired_api_from_failing() {
        let declarations = [DecisionApiDeclaration::new(
            "observe",
            "crates/frankenterm-core/src/observer.rs",
        )];
        let source_files = [DecisionApiSourceFile::new(
            "crates/frankenterm-core/src/observer.rs",
            "pub fn observe() {}\n",
        )];
        let exemptions = [DormantDecisionApiExemptionEntry::new(
            "observe",
            DormantDecisionApiExemption::new("ft-7h5da.5.6", "2026-07-01"),
        )];

        let report = decision_report(&declarations, &source_files, &exemptions);

        assert!(report.passed);
        assert_eq!(report.records[0].status, DecisionApiWiringStatus::Dormant);
        assert_eq!(
            report.records[0].dormant_exemption,
            Some(exemptions[0].exemption.clone())
        );
    }

    #[test]
    fn dormant_exemption_requires_bead_and_valid_unexpired_date() {
        let declarations = [
            DecisionApiDeclaration::new("allow_operation", "src/policy.rs"),
            DecisionApiDeclaration::new("evaluate", "src/governor.rs"),
            DecisionApiDeclaration::new("execute", "src/runner.rs"),
            DecisionApiDeclaration::new("observe", "src/observer.rs"),
        ];
        let exemptions = [
            DormantDecisionApiExemptionEntry::new(
                "allow_operation",
                DormantDecisionApiExemption::new("", "2026-07-01"),
            ),
            DormantDecisionApiExemptionEntry::new(
                "evaluate",
                DormantDecisionApiExemption::new("ft-7h5da.5.6", "not-a-date"),
            ),
            DormantDecisionApiExemptionEntry::new(
                "execute",
                DormantDecisionApiExemption::new("ft-7h5da.5.6", "2026-01-01"),
            ),
            DormantDecisionApiExemptionEntry::new(
                "observe",
                DormantDecisionApiExemption::new("ft-7h5da.5.6", "2026-02-31"),
            ),
        ];

        let report = decision_report(&declarations, &[], &exemptions);
        let reasons: Vec<_> = report
            .violations
            .iter()
            .map(|violation| violation.reason)
            .collect();

        assert!(!report.passed);
        assert_eq!(
            reasons,
            vec![
                DeadwireViolationReason::MissingDormantBead,
                DeadwireViolationReason::InvalidDormantExpiry,
                DeadwireViolationReason::ExpiredDormantExemption,
                DeadwireViolationReason::InvalidDormantExpiry
            ]
        );
    }

    #[test]
    fn wiring_report_serializes_machine_facing_schema() -> Result<(), serde_json::Error> {
        let declarations = [DecisionApiDeclaration::new(
            "allow_operation",
            "crates/frankenterm-core/src/policy.rs",
        )];
        let source_files = [DecisionApiSourceFile::new(
            "crates/frankenterm-core/src/workflows/runner.rs",
            "fn run(policy: &PolicyEngine) { policy.allow_operation(); }\n",
        )];

        let report = decision_report(&declarations, &source_files, &[]);
        let json = serde_json::to_string(&report)?;

        assert_eq!(
            json,
            concat!(
                r#"{"schema_version":1,"produced_by_bead":"ft-7h5da.5.5","#,
                r#""generated_at_ms":1704000000000,"records":[{"symbol":"allow_operation","#,
                r#""defining_path":"crates/frankenterm-core/src/policy.rs","#,
                r#""production_callers":[{"path":"crates/frankenterm-core/src/workflows/runner.rs","line":1}],"#,
                r#""status":"wired","dormant_exemption":null}],"violations":[],"passed":true}"#
            )
        );
        Ok(())
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
