//! Replay fixture harvesting pipeline for deterministic regression artifacts.
//!
//! This module supports bead `ft-og6q6.7.1` by turning real incident/swarm
//! captures into curated `.ftreplay` artifacts with:
//! - deterministic redaction and sensitivity tagging,
//! - quality filters,
//! - duplicate overlap detection,
//! - registry persistence for query by incident/risk tags.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::policy::Redactor;
use crate::recording::{
    RecorderControlMarkerType, RecorderEvent, RecorderEventPayload, RecorderRedactionLevel,
    parse_recorder_event_json,
};

pub const REPLAY_FIXTURE_HARVEST_SCHEMA_VERSION: &str = "ft.replay.fixture.harvest.v1";
pub const REPLAY_FIXTURE_REGISTRY_SCHEMA_VERSION: &str = "ft.replay.fixture.registry.v1";
const REGISTRY_FILE_NAME: &str = "fixture_registry.json";
const RECORDER_EVENT_FILE_EXTENSIONS: [&str; 4] = ["jsonl", "ndjson", "json", "ftreplay"];
const DUPLICATE_OVERLAP_THRESHOLD: f64 = 0.90;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarvestSourceType {
    IncidentRecording,
    SwarmCapture,
    ManualExport,
}

impl HarvestSourceType {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::IncidentRecording => "incident_recording",
            Self::SwarmCapture => "swarm_capture",
            Self::ManualExport => "manual_export",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HarvestSource {
    IncidentRecording(PathBuf),
    SwarmCapture(PathBuf),
    ManualExport(PathBuf),
}

impl HarvestSource {
    #[must_use]
    pub fn from_path(path: PathBuf) -> Self {
        let lowered = path.to_string_lossy().to_ascii_lowercase();
        if lowered.contains("incident") {
            return Self::IncidentRecording(path);
        }
        if lowered.contains("swarm") || lowered.contains("run") {
            return Self::SwarmCapture(path);
        }
        Self::ManualExport(path)
    }

    #[must_use]
    pub fn source_type(&self) -> HarvestSourceType {
        match self {
            Self::IncidentRecording(_) => HarvestSourceType::IncidentRecording,
            Self::SwarmCapture(_) => HarvestSourceType::SwarmCapture,
            Self::ManualExport(_) => HarvestSourceType::ManualExport,
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::IncidentRecording(path) | Self::SwarmCapture(path) | Self::ManualExport(path) => {
                path.as_path()
            }
        }
    }

    #[must_use]
    pub fn incident_id(&self) -> Option<String> {
        let stem = self
            .path()
            .file_stem()
            .and_then(std::ffi::OsStr::to_str)
            .map(ToString::to_string);
        if matches!(self, Self::IncidentRecording(_)) {
            return stem;
        }
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SensitivityTier {
    T1,
    T2,
    T3,
}

impl SensitivityTier {
    #[must_use]
    pub fn as_risk_tag(self) -> &'static str {
        match self {
            Self::T1 => "tier:t1",
            Self::T2 => "tier:t2",
            Self::T3 => "tier:t3",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedactionMode {
    Mask,
    Hash,
    Drop,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarvestMetadata {
    pub harvest_date: String,
    pub source_type: HarvestSourceType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incident_id: Option<String>,
    pub event_count: usize,
    pub pane_count: usize,
    pub decision_count: usize,
    pub sensitivity_tier: SensitivityTier,
    pub checksum_sha256: String,
    #[serde(default)]
    pub risk_tags: Vec<String>,
    #[serde(default)]
    pub tombstoned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_tier_days: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayArtifact {
    pub schema_version: String,
    pub artifact_id: String,
    pub source_path: String,
    pub metadata: HarvestMetadata,
    pub events: Vec<RecorderEvent>,
}

impl ReplayArtifact {
    pub fn validate(&self) -> crate::Result<()> {
        if self.schema_version != REPLAY_FIXTURE_HARVEST_SCHEMA_VERSION {
            return Err(crate::Error::Runtime(format!(
                "unsupported replay fixture schema '{}'",
                self.schema_version
            )));
        }

        if self.metadata.event_count != self.events.len() {
            return Err(crate::Error::Runtime(format!(
                "event count mismatch: metadata={}, actual={}",
                self.metadata.event_count,
                self.events.len()
            )));
        }

        let pane_count = unique_pane_count(&self.events);
        if pane_count != self.metadata.pane_count {
            return Err(crate::Error::Runtime(format!(
                "pane count mismatch: metadata={}, actual={pane_count}",
                self.metadata.pane_count
            )));
        }

        let decision_count = count_decisions(&self.events);
        if decision_count != self.metadata.decision_count {
            return Err(crate::Error::Runtime(format!(
                "decision count mismatch: metadata={}, actual={decision_count}",
                self.metadata.decision_count
            )));
        }

        let checksum = checksum_events(&self.events)?;
        if checksum != self.metadata.checksum_sha256 {
            return Err(crate::Error::Runtime(format!(
                "checksum mismatch: metadata={}, actual={checksum}",
                self.metadata.checksum_sha256
            )));
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRegistryEntry {
    pub artifact_id: String,
    pub artifact_path: String,
    pub source_path: String,
    pub source_type: HarvestSourceType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incident_id: Option<String>,
    pub event_count: usize,
    pub pane_count: usize,
    pub decision_count: usize,
    pub sensitivity_tier: SensitivityTier,
    pub checksum_sha256: String,
    #[serde(default)]
    pub risk_tags: Vec<String>,
    #[serde(default)]
    pub event_fingerprints: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRegistry {
    pub schema_version: String,
    #[serde(default)]
    pub artifacts: Vec<ArtifactRegistryEntry>,
}

impl Default for ArtifactRegistry {
    fn default() -> Self {
        Self {
            schema_version: REPLAY_FIXTURE_REGISTRY_SCHEMA_VERSION.to_string(),
            artifacts: Vec::new(),
        }
    }
}

impl ArtifactRegistry {
    pub fn load_or_default(path: &Path) -> crate::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(path)?;
        let parsed: Self = serde_json::from_str(&raw)?;
        Ok(parsed)
    }

    pub fn persist(&self, path: &Path) -> crate::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let payload = serde_json::to_string_pretty(self)?;
        fs::write(path, payload)?;
        Ok(())
    }

    pub fn register(&mut self, entry: ArtifactRegistryEntry) {
        self.artifacts.retain(|existing| {
            !(existing.artifact_id == entry.artifact_id
                || existing.checksum_sha256 == entry.checksum_sha256)
        });
        self.artifacts.push(entry);
    }

    #[must_use]
    pub fn query_by_incident(&self, incident_id: &str) -> Vec<&ArtifactRegistryEntry> {
        self.artifacts
            .iter()
            .filter(|entry| entry.incident_id.as_deref() == Some(incident_id))
            .collect()
    }

    #[must_use]
    pub fn query_by_risk_tag(&self, risk_tag: &str) -> Vec<&ArtifactRegistryEntry> {
        self.artifacts
            .iter()
            .filter(|entry| entry.risk_tags.iter().any(|tag| tag == risk_tag))
            .collect()
    }

    #[must_use]
    pub fn find_duplicate(
        &self,
        event_fingerprints: &HashSet<String>,
        threshold: f64,
    ) -> Option<DuplicateMatch> {
        if event_fingerprints.is_empty() {
            return None;
        }

        let mut best: Option<DuplicateMatch> = None;
        for entry in &self.artifacts {
            if entry.event_fingerprints.is_empty() {
                continue;
            }
            let existing: HashSet<&String> = entry.event_fingerprints.iter().collect();
            let intersection = event_fingerprints
                .iter()
                .filter(|fingerprint| existing.contains(*fingerprint))
                .count();
            let overlap = intersection as f64 / event_fingerprints.len() as f64;
            if overlap >= threshold {
                let candidate = DuplicateMatch {
                    artifact_id: entry.artifact_id.clone(),
                    overlap_ratio: overlap,
                };
                match &best {
                    Some(current) if current.overlap_ratio >= overlap => {}
                    _ => best = Some(candidate),
                }
            }
        }

        best
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DuplicateMatch {
    pub artifact_id: String,
    pub overlap_ratio: f64,
}

#[derive(Debug, Clone)]
pub struct QualityFilters {
    pub min_event_count: usize,
    pub min_pane_count: usize,
    pub min_decision_count: usize,
    pub duplicate_overlap_threshold: f64,
}

impl Default for QualityFilters {
    fn default() -> Self {
        Self {
            min_event_count: 100,
            min_pane_count: 2,
            min_decision_count: 1,
            duplicate_overlap_threshold: DUPLICATE_OVERLAP_THRESHOLD,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RetentionPolicy {
    pub t1_days: i64,
    pub t2_days: i64,
    pub t3_days: i64,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            t1_days: 90,
            t2_days: 30,
            t3_days: 7,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RetentionEnforcer {
    pub policy: RetentionPolicy,
}

impl Default for RetentionEnforcer {
    fn default() -> Self {
        Self {
            policy: RetentionPolicy::default(),
        }
    }
}

impl RetentionEnforcer {
    #[must_use]
    pub fn retention_days_for_tier(&self, tier: SensitivityTier) -> i64 {
        match tier {
            SensitivityTier::T1 => self.policy.t1_days,
            SensitivityTier::T2 => self.policy.t2_days,
            SensitivityTier::T3 => self.policy.t3_days,
        }
    }

    #[must_use]
    pub fn should_tombstone(
        &self,
        harvest_date: &str,
        tier: SensitivityTier,
        now: DateTime<Utc>,
    ) -> bool {
        let Ok(ts) = DateTime::parse_from_rfc3339(harvest_date) else {
            return false;
        };
        let retention_days = self.retention_days_for_tier(tier);
        let cutoff = now - Duration::days(retention_days);
        ts.with_timezone(&Utc) < cutoff
    }

    pub fn apply(&self, artifact: &mut ReplayArtifact, now: DateTime<Utc>) {
        let retention_days = self.retention_days_for_tier(artifact.metadata.sensitivity_tier);
        artifact.metadata.retention_tier_days = Some(retention_days);

        if self.should_tombstone(
            &artifact.metadata.harvest_date,
            artifact.metadata.sensitivity_tier,
            now,
        ) {
            artifact.events.clear();
            artifact.metadata.tombstoned = true;
            artifact.metadata.checksum_sha256 =
                checksum_events(&artifact.events).unwrap_or_else(|_| String::new());
            artifact
                .metadata
                .risk_tags
                .push("retention:tombstoned".to_string());
            artifact.metadata.event_count = 0;
            artifact.metadata.decision_count = 0;
            artifact.metadata.pane_count = 0;
        }
    }
}

#[derive(Debug, Clone)]
pub struct FixtureHarvester {
    redactor: Redactor,
    pub redaction_mode: RedactionMode,
    pub quality_filters: QualityFilters,
    pub retention_enforcer: RetentionEnforcer,
}

impl Default for FixtureHarvester {
    fn default() -> Self {
        Self {
            redactor: Redactor::new(),
            redaction_mode: RedactionMode::Mask,
            quality_filters: QualityFilters::default(),
            retention_enforcer: RetentionEnforcer::default(),
        }
    }
}

impl FixtureHarvester {
    pub fn harvest(&self, source: HarvestSource) -> crate::Result<ReplayArtifact> {
        let source_path = source.path().to_path_buf();
        if !source_path.exists() {
            return Err(crate::Error::Runtime(format!(
                "harvest source path not found: {}",
                source_path.display()
            )));
        }

        let mut events = parse_events_from_source(&source_path)?;
        if events.is_empty() {
            return Err(crate::Error::Runtime(format!(
                "harvest source had no recorder events: {}",
                source_path.display()
            )));
        }

        events.sort_by(|a, b| {
            (a.recorded_at_ms, a.pane_id, a.sequence, &a.event_id).cmp(&(
                b.recorded_at_ms,
                b.pane_id,
                b.sequence,
                &b.event_id,
            ))
        });

        for event in &mut events {
            self.apply_redaction(event);
        }

        let event_count = events.len();
        let pane_count = unique_pane_count(&events);
        let decision_count = count_decisions(&events);
        let sensitivity_tier = max_sensitivity_tier(&events);
        let checksum_sha256 = checksum_events(&events)?;
        let harvest_date = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);

        let mut risk_tags = vec![sensitivity_tier.as_risk_tag().to_string()];
        if decision_count > 0 {
            risk_tags.push("decisionful".to_string());
        }
        if event_count > 10_000 {
            risk_tags.push("large-artifact".to_string());
        }

        let artifact_id = format!(
            "{}-{}",
            source.source_type().as_str(),
            &checksum_sha256[..16.min(checksum_sha256.len())]
        );

        let mut artifact = ReplayArtifact {
            schema_version: REPLAY_FIXTURE_HARVEST_SCHEMA_VERSION.to_string(),
            artifact_id,
            source_path: source_path.display().to_string(),
            metadata: HarvestMetadata {
                harvest_date,
                source_type: source.source_type(),
                incident_id: source.incident_id(),
                event_count,
                pane_count,
                decision_count,
                sensitivity_tier,
                checksum_sha256,
                risk_tags,
                tombstoned: false,
                retention_tier_days: None,
            },
            events,
        };

        self.retention_enforcer.apply(&mut artifact, Utc::now());
        artifact.validate()?;
        Ok(artifact)
    }

    pub fn quality_failure_reason(&self, artifact: &ReplayArtifact) -> Option<String> {
        if artifact.metadata.event_count < self.quality_filters.min_event_count {
            return Some(format!(
                "event_count_below_min(actual={},min={})",
                artifact.metadata.event_count, self.quality_filters.min_event_count
            ));
        }
        if artifact.metadata.pane_count < self.quality_filters.min_pane_count {
            return Some(format!(
                "pane_count_below_min(actual={},min={})",
                artifact.metadata.pane_count, self.quality_filters.min_pane_count
            ));
        }
        if artifact.metadata.decision_count < self.quality_filters.min_decision_count {
            return Some(format!(
                "decision_count_below_min(actual={},min={})",
                artifact.metadata.decision_count, self.quality_filters.min_decision_count
            ));
        }

        None
    }

    fn apply_redaction(&self, event: &mut RecorderEvent) {
        match &mut event.payload {
            RecorderEventPayload::IngressText {
                text, redaction, ..
            }
            | RecorderEventPayload::EgressOutput {
                text, redaction, ..
            } => {
                redact_text(text, redaction, &self.redactor, self.redaction_mode);
            }
            RecorderEventPayload::ControlMarker { details, .. }
            | RecorderEventPayload::LifecycleMarker { details, .. } => {
                redact_json_value(details, &self.redactor, self.redaction_mode)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarvestSourceFilter {
    All,
    IncidentOnly,
}

impl HarvestSourceFilter {
    pub fn from_cli_flag(flag: &str) -> crate::Result<Self> {
        match flag {
            "all" => Ok(Self::All),
            "incident-only" => Ok(Self::IncidentOnly),
            other => Err(crate::Error::Runtime(format!(
                "invalid harvest filter '{other}', expected 'all' or 'incident-only'"
            ))),
        }
    }

    #[must_use]
    pub fn includes(self, source: &HarvestSource) -> bool {
        match self {
            Self::All => true,
            Self::IncidentOnly => matches!(source, HarvestSource::IncidentRecording(_)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarvestEntryStatus {
    Harvested,
    Skipped,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarvestEntry {
    pub source_path: String,
    pub status: HarvestEntryStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlap_ratio: Option<f64>,
    #[serde(default)]
    pub event_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HarvestReport {
    pub harvested: usize,
    pub skipped: usize,
    pub errors: usize,
    pub total_events: usize,
    #[serde(default)]
    pub entries: Vec<HarvestEntry>,
}

pub struct HarvestPipeline {
    harvester: FixtureHarvester,
}

impl Default for HarvestPipeline {
    fn default() -> Self {
        Self {
            harvester: FixtureHarvester::default(),
        }
    }
}

impl HarvestPipeline {
    #[must_use]
    pub fn new(harvester: FixtureHarvester) -> Self {
        Self { harvester }
    }

    pub fn harvest_directory(
        &self,
        source_dir: &Path,
        output_dir: &Path,
        filter: HarvestSourceFilter,
    ) -> crate::Result<HarvestReport> {
        if !source_dir.is_dir() {
            return Err(crate::Error::Runtime(format!(
                "source directory does not exist or is not a directory: {}",
                source_dir.display()
            )));
        }

        fs::create_dir_all(output_dir)?;
        let registry_path = output_dir.join(REGISTRY_FILE_NAME);
        let mut registry = ArtifactRegistry::load_or_default(&registry_path)?;
        let source_paths = discover_source_files(source_dir)?;

        let mut report = HarvestReport::default();

        for source_path in source_paths {
            let source = HarvestSource::from_path(source_path.clone());
            if !filter.includes(&source) {
                report.skipped += 1;
                report.entries.push(HarvestEntry {
                    source_path: source_path.display().to_string(),
                    status: HarvestEntryStatus::Skipped,
                    artifact_path: None,
                    reason: Some("filtered_out".to_string()),
                    overlap_ratio: None,
                    event_count: 0,
                });
                continue;
            }

            let artifact = match self.harvester.harvest(source.clone()) {
                Ok(artifact) => artifact,
                Err(err) => {
                    report.errors += 1;
                    report.entries.push(HarvestEntry {
                        source_path: source.path().display().to_string(),
                        status: HarvestEntryStatus::Error,
                        artifact_path: None,
                        reason: Some(format!("{err}")),
                        overlap_ratio: None,
                        event_count: 0,
                    });
                    continue;
                }
            };

            if let Some(reason) = self.harvester.quality_failure_reason(&artifact) {
                report.skipped += 1;
                report.entries.push(HarvestEntry {
                    source_path: artifact.source_path.clone(),
                    status: HarvestEntryStatus::Skipped,
                    artifact_path: None,
                    reason: Some(reason),
                    overlap_ratio: None,
                    event_count: artifact.metadata.event_count,
                });
                continue;
            }

            let fingerprints = event_fingerprints(&artifact.events);
            if let Some(duplicate) = registry.find_duplicate(
                &fingerprints,
                self.harvester.quality_filters.duplicate_overlap_threshold,
            ) {
                report.skipped += 1;
                report.entries.push(HarvestEntry {
                    source_path: artifact.source_path.clone(),
                    status: HarvestEntryStatus::Skipped,
                    artifact_path: None,
                    reason: Some(format!("duplicate_of:{}", duplicate.artifact_id)),
                    overlap_ratio: Some(duplicate.overlap_ratio),
                    event_count: artifact.metadata.event_count,
                });
                continue;
            }

            let artifact_path = output_dir.join(format!("{}.ftreplay", artifact.artifact_id));
            write_artifact(&artifact, &artifact_path)?;

            let entry = ArtifactRegistryEntry {
                artifact_id: artifact.artifact_id.clone(),
                artifact_path: artifact_path.display().to_string(),
                source_path: artifact.source_path.clone(),
                source_type: artifact.metadata.source_type,
                incident_id: artifact.metadata.incident_id.clone(),
                event_count: artifact.metadata.event_count,
                pane_count: artifact.metadata.pane_count,
                decision_count: artifact.metadata.decision_count,
                sensitivity_tier: artifact.metadata.sensitivity_tier,
                checksum_sha256: artifact.metadata.checksum_sha256.clone(),
                risk_tags: artifact.metadata.risk_tags.clone(),
                event_fingerprints: fingerprints.into_iter().collect(),
            };
            registry.register(entry);

            report.harvested += 1;
            report.total_events += artifact.metadata.event_count;
            report.entries.push(HarvestEntry {
                source_path: artifact.source_path,
                status: HarvestEntryStatus::Harvested,
                artifact_path: Some(artifact_path.display().to_string()),
                reason: None,
                overlap_ratio: None,
                event_count: artifact.metadata.event_count,
            });
        }

        registry.persist(&registry_path)?;
        Ok(report)
    }
}

fn discover_source_files(root: &Path) -> crate::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }

            let Some(ext) = path.extension().and_then(std::ffi::OsStr::to_str) else {
                continue;
            };
            if RECORDER_EVENT_FILE_EXTENSIONS
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(ext))
            {
                out.push(path);
            }
        }
    }

    out.sort();
    Ok(out)
}

fn parse_events_from_source(path: &Path) -> crate::Result<Vec<RecorderEvent>> {
    let ext = path
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();

    match ext.as_str() {
        "json" | "ftreplay" => parse_events_from_json(path),
        _ => parse_events_from_jsonl(path),
    }
}

fn parse_events_from_jsonl(path: &Path) -> crate::Result<Vec<RecorderEvent>> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut events = Vec::new();

    for (line_idx, line) in reader.lines().enumerate() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let parsed = parse_recorder_event_json(trimmed).map_err(|err| {
            crate::Error::Runtime(format!(
                "failed to parse recorder event at {}:{}: {err}",
                path.display(),
                line_idx + 1
            ))
        })?;
        events.push(parsed);
    }

    Ok(events)
}

fn parse_events_from_json(path: &Path) -> crate::Result<Vec<RecorderEvent>> {
    let raw = fs::read_to_string(path)?;
    let value: serde_json::Value = serde_json::from_str(&raw).map_err(|err| {
        crate::Error::Runtime(format!(
            "failed to parse JSON source {}: {err}",
            path.display()
        ))
    })?;

    if let Some(events_value) = value.get("events") {
        let events: Vec<RecorderEvent> = serde_json::from_value(events_value.clone())?;
        return Ok(events);
    }

    if value.is_array() {
        let events: Vec<RecorderEvent> = serde_json::from_value(value)?;
        return Ok(events);
    }

    Err(crate::Error::Runtime(format!(
        "JSON source {} must be an event array or {{\"events\": [...]}}",
        path.display()
    )))
}

fn write_artifact(artifact: &ReplayArtifact, artifact_path: &Path) -> crate::Result<()> {
    artifact.validate()?;
    let payload = serde_json::to_string_pretty(artifact)?;
    fs::write(artifact_path, payload)?;
    Ok(())
}

fn unique_pane_count(events: &[RecorderEvent]) -> usize {
    events
        .iter()
        .map(|event| event.pane_id)
        .collect::<BTreeSet<_>>()
        .len()
}

fn count_decisions(events: &[RecorderEvent]) -> usize {
    events
        .iter()
        .filter(|event| {
            matches!(
                &event.payload,
                RecorderEventPayload::ControlMarker {
                    control_marker_type: RecorderControlMarkerType::PolicyDecision,
                    ..
                } | RecorderEventPayload::ControlMarker {
                    control_marker_type: RecorderControlMarkerType::ApprovalCheckpoint,
                    ..
                }
            )
        })
        .count()
}

fn max_sensitivity_tier(events: &[RecorderEvent]) -> SensitivityTier {
    events
        .iter()
        .map(classify_event_sensitivity)
        .max()
        .unwrap_or(SensitivityTier::T1)
}

fn classify_event_sensitivity(event: &RecorderEvent) -> SensitivityTier {
    match &event.payload {
        RecorderEventPayload::IngressText { text, .. }
        | RecorderEventPayload::EgressOutput { text, .. } => classify_text_sensitivity(text),
        RecorderEventPayload::ControlMarker {
            control_marker_type,
            details,
        } => {
            let base = match control_marker_type {
                RecorderControlMarkerType::PolicyDecision
                | RecorderControlMarkerType::ApprovalCheckpoint => SensitivityTier::T2,
                _ => SensitivityTier::T1,
            };
            std::cmp::max(base, classify_text_sensitivity(&details.to_string()))
        }
        RecorderEventPayload::LifecycleMarker { details, .. } => {
            classify_text_sensitivity(&details.to_string())
        }
    }
}

fn classify_text_sensitivity(text: &str) -> SensitivityTier {
    if contains_bearer_token(text) || contains_jwt_like_token(text) {
        return SensitivityTier::T3;
    }

    if contains_api_like_secret(text) || contains_connection_string(text) {
        return SensitivityTier::T2;
    }

    SensitivityTier::T1
}

fn contains_bearer_token(text: &str) -> bool {
    text.to_ascii_lowercase().contains("bearer ")
}

fn contains_api_like_secret(text: &str) -> bool {
    let prefixes: [(&str, usize); 4] = [("sk-", 20), ("AKIA", 16), ("ghp_", 20), ("xoxb-", 18)];

    text.split_whitespace().any(|token| {
        let normalized = normalize_token(token);
        prefixes
            .iter()
            .any(|(prefix, min_len)| normalized.starts_with(prefix) && normalized.len() >= *min_len)
    })
}

fn contains_connection_string(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();
    [
        "postgres://",
        "postgresql://",
        "mysql://",
        "mongodb://",
        "redis://",
        "password=",
    ]
    .iter()
    .any(|needle| lowered.contains(needle))
}

fn contains_jwt_like_token(text: &str) -> bool {
    text.split_whitespace().any(|token| {
        let normalized = normalize_token(token);
        let parts: Vec<&str> = normalized.split('.').collect();
        if parts.len() != 3 {
            return false;
        }
        parts.iter().all(|part| {
            part.len() >= 8
                && part
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
        })
    })
}

fn normalize_token(token: &str) -> String {
    token
        .trim_matches(|ch: char| {
            !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.' || ch == ':')
        })
        .to_string()
}

fn redact_text(
    text: &mut String,
    redaction: &mut RecorderRedactionLevel,
    redactor: &Redactor,
    mode: RedactionMode,
) {
    let source = text.clone();
    let redacted = redactor.redact(&source);

    if redacted == source {
        return;
    }

    match mode {
        RedactionMode::Mask => {
            *text = redacted;
            *redaction = RecorderRedactionLevel::Partial;
        }
        RedactionMode::Hash => {
            *text = format!("sha256:{}", sha256_hex(redacted.as_bytes()));
            *redaction = RecorderRedactionLevel::Full;
        }
        RedactionMode::Drop => {
            text.clear();
            *redaction = RecorderRedactionLevel::Full;
        }
    }
}

fn redact_json_value(value: &mut serde_json::Value, redactor: &Redactor, mode: RedactionMode) {
    match value {
        serde_json::Value::String(text) => {
            let source = text.clone();
            let redacted = redactor.redact(&source);
            if redacted == source {
                return;
            }
            *text = match mode {
                RedactionMode::Mask => redacted,
                RedactionMode::Hash => format!("sha256:{}", sha256_hex(redacted.as_bytes())),
                RedactionMode::Drop => "".to_string(),
            };
        }
        serde_json::Value::Array(values) => {
            for inner in values {
                redact_json_value(inner, redactor, mode);
            }
        }
        serde_json::Value::Object(map) => {
            for value in map.values_mut() {
                redact_json_value(value, redactor, mode);
            }
        }
        _ => {}
    }
}

fn event_fingerprints(events: &[RecorderEvent]) -> HashSet<String> {
    events
        .iter()
        .map(|event| {
            if !event.event_id.is_empty() {
                return event.event_id.clone();
            }
            let serialized = serde_json::to_vec(event).unwrap_or_default();
            sha256_hex(&serialized)
        })
        .collect()
}

fn checksum_events(events: &[RecorderEvent]) -> crate::Result<String> {
    let mut hasher = Sha256::new();
    for event in events {
        let encoded = serde_json::to_vec(event)?;
        hasher.update(encoded);
        hasher.update(b"\n");
    }
    Ok(sha256_hex(&hasher.finalize()))
}

fn sha256_hex(bytes: impl AsRef<[u8]>) -> String {
    let digest = Sha256::digest(bytes.as_ref());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::recording::{
        RECORDER_EVENT_SCHEMA_VERSION_V1, RecorderControlMarkerType, RecorderEvent,
        RecorderEventCausality, RecorderEventPayload, RecorderEventSource, RecorderIngressKind,
        RecorderRedactionLevel, RecorderSegmentKind, RecorderTextEncoding,
    };

    fn make_ingress_event(
        event_id: &str,
        pane_id: u64,
        sequence: u64,
        text: &str,
    ) -> RecorderEvent {
        RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: event_id.to_string(),
            pane_id,
            session_id: Some("sess-test".to_string()),
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::RobotMode,
            occurred_at_ms: 1000 + sequence,
            recorded_at_ms: 1000 + sequence,
            sequence,
            causality: RecorderEventCausality::default(),
            payload: RecorderEventPayload::IngressText {
                text: text.to_string(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                ingress_kind: RecorderIngressKind::SendText,
            },
        }
    }

    fn make_decision_event(event_id: &str, pane_id: u64, sequence: u64) -> RecorderEvent {
        RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: event_id.to_string(),
            pane_id,
            session_id: Some("sess-test".to_string()),
            workflow_id: Some("wf-1".to_string()),
            correlation_id: Some("corr-1".to_string()),
            source: RecorderEventSource::WorkflowEngine,
            occurred_at_ms: 2000 + sequence,
            recorded_at_ms: 2000 + sequence,
            sequence,
            causality: RecorderEventCausality::default(),
            payload: RecorderEventPayload::ControlMarker {
                control_marker_type: RecorderControlMarkerType::PolicyDecision,
                details: serde_json::json!({"decision":"allow","reason":"test"}),
            },
        }
    }

    fn make_egress_event(event_id: &str, pane_id: u64, sequence: u64, text: &str) -> RecorderEvent {
        RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: event_id.to_string(),
            pane_id,
            session_id: Some("sess-test".to_string()),
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::WeztermMux,
            occurred_at_ms: 3000 + sequence,
            recorded_at_ms: 3000 + sequence,
            sequence,
            causality: RecorderEventCausality::default(),
            payload: RecorderEventPayload::EgressOutput {
                text: text.to_string(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                segment_kind: RecorderSegmentKind::Delta,
                is_gap: false,
            },
        }
    }

    fn create_temp_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ft_replay_fixture_harvest_{}_{}",
            label,
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::create_dir_all(&path).expect("temp dir create");
        path
    }

    fn write_jsonl(path: &Path, events: &[RecorderEvent]) {
        let mut out = String::new();
        for event in events {
            out.push_str(&serde_json::to_string(event).expect("serialize event"));
            out.push('\n');
        }
        fs::write(path, out).expect("write jsonl");
    }

    fn build_artifact(events: Vec<RecorderEvent>) -> ReplayArtifact {
        let checksum = checksum_events(&events).expect("checksum");
        ReplayArtifact {
            schema_version: REPLAY_FIXTURE_HARVEST_SCHEMA_VERSION.to_string(),
            artifact_id: "artifact-test".to_string(),
            source_path: "source.jsonl".to_string(),
            metadata: HarvestMetadata {
                harvest_date: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
                source_type: HarvestSourceType::ManualExport,
                incident_id: None,
                event_count: events.len(),
                pane_count: unique_pane_count(&events),
                decision_count: count_decisions(&events),
                sensitivity_tier: max_sensitivity_tier(&events),
                checksum_sha256: checksum,
                risk_tags: vec![],
                tombstoned: false,
                retention_tier_days: None,
            },
            events,
        }
    }

    #[test]
    fn harvest_source_from_path_detects_incident_recording() {
        let source = HarvestSource::from_path(PathBuf::from("/tmp/incident_abc.jsonl"));
        assert!(matches!(source, HarvestSource::IncidentRecording(_)));
        assert_eq!(source.source_type(), HarvestSourceType::IncidentRecording);
        assert_eq!(source.incident_id().as_deref(), Some("incident_abc"));
    }

    #[test]
    fn harvest_source_from_path_detects_swarm_capture() {
        let source = HarvestSource::from_path(PathBuf::from("/tmp/swarm_run_42.ndjson"));
        assert!(matches!(source, HarvestSource::SwarmCapture(_)));
        assert_eq!(source.source_type(), HarvestSourceType::SwarmCapture);
    }

    #[test]
    fn harvest_source_manual_export_has_no_incident_id() {
        let source = HarvestSource::from_path(PathBuf::from("/tmp/manual_export.jsonl"));
        assert!(matches!(source, HarvestSource::ManualExport(_)));
        assert_eq!(source.incident_id(), None);
    }

    #[test]
    fn harvest_source_filter_rejects_invalid_flag() {
        let err = HarvestSourceFilter::from_cli_flag("invalid").expect_err("must fail");
        assert!(format!("{err}").contains("invalid harvest filter"));
    }

    #[test]
    fn sensitivity_detects_api_key_as_t2() {
        assert_eq!(
            classify_text_sensitivity("token sk-abcdefghijklmnopqrstuvwxyz123456"),
            SensitivityTier::T2
        );
    }

    #[test]
    fn sensitivity_detects_bearer_as_t3() {
        assert_eq!(
            classify_text_sensitivity("Authorization: Bearer abc.defghijkl.mnopqrst"),
            SensitivityTier::T3
        );
    }

    #[test]
    fn policy_control_marker_is_at_least_t2() {
        let event = make_decision_event("ev-1", 1, 0);
        assert_eq!(classify_event_sensitivity(&event), SensitivityTier::T2);
    }

    #[test]
    fn count_decisions_only_counts_policy_and_approval_markers() {
        let mut events = Vec::new();
        events.push(make_decision_event("ev-1", 1, 0));
        events.push(make_ingress_event("ev-2", 1, 1, "hello"));
        events.push(RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: "ev-3".to_string(),
            pane_id: 1,
            session_id: None,
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::RobotMode,
            occurred_at_ms: 1,
            recorded_at_ms: 1,
            sequence: 2,
            causality: RecorderEventCausality::default(),
            payload: RecorderEventPayload::ControlMarker {
                control_marker_type: RecorderControlMarkerType::PromptBoundary,
                details: serde_json::json!({}),
            },
        });

        assert_eq!(count_decisions(&events), 1);
    }

    #[test]
    fn redaction_mask_mode_scrubs_secrets() {
        let harvester = FixtureHarvester::default();
        let mut event = make_ingress_event(
            "ev-1",
            1,
            0,
            "export OPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwxyz123456",
        );

        harvester.apply_redaction(&mut event);

        let RecorderEventPayload::IngressText {
            text, redaction, ..
        } = &event.payload
        else {
            panic!("expected ingress payload");
        };

        assert!(!text.contains("sk-abcdefghijklmnopqrstuvwxyz123456"));
        assert_eq!(*redaction, RecorderRedactionLevel::Partial);
    }

    #[test]
    fn redaction_hash_mode_is_deterministic() {
        let mut harvester = FixtureHarvester::default();
        harvester.redaction_mode = RedactionMode::Hash;
        let mut event_a =
            make_ingress_event("ev-1", 1, 0, "token sk-abcdefghijklmnopqrstuvwxyz123456");
        let mut event_b =
            make_ingress_event("ev-2", 1, 1, "token sk-abcdefghijklmnopqrstuvwxyz123456");

        harvester.apply_redaction(&mut event_a);
        harvester.apply_redaction(&mut event_b);

        let RecorderEventPayload::IngressText { text: a, .. } = &event_a.payload else {
            panic!("expected ingress payload")
        };
        let RecorderEventPayload::IngressText { text: b, .. } = &event_b.payload else {
            panic!("expected ingress payload")
        };
        assert_eq!(a, b);
        assert!(a.starts_with("sha256:"));
    }

    #[test]
    fn redaction_drop_mode_clears_content() {
        let mut harvester = FixtureHarvester::default();
        harvester.redaction_mode = RedactionMode::Drop;
        let mut event =
            make_ingress_event("ev-1", 1, 0, "token sk-abcdefghijklmnopqrstuvwxyz123456");

        harvester.apply_redaction(&mut event);

        let RecorderEventPayload::IngressText {
            text, redaction, ..
        } = &event.payload
        else {
            panic!("expected ingress payload");
        };
        assert!(text.is_empty());
        assert_eq!(*redaction, RecorderRedactionLevel::Full);
    }

    #[test]
    fn parse_events_from_jsonl_works() {
        let dir = create_temp_dir("parse_jsonl");
        let path = dir.join("sample.jsonl");
        let events = vec![
            make_ingress_event("ev-1", 1, 0, "hello"),
            make_decision_event("ev-2", 2, 1),
        ];
        write_jsonl(&path, &events);

        let parsed = parse_events_from_source(&path).expect("parse events");
        assert_eq!(parsed.len(), 2);
    }

    #[test]
    fn parse_events_from_json_array_works() {
        let dir = create_temp_dir("parse_json_array");
        let path = dir.join("sample.json");
        let events = vec![make_ingress_event("ev-1", 1, 0, "hello")];
        fs::write(
            &path,
            serde_json::to_string(&events).expect("serialize events array"),
        )
        .expect("write json");

        let parsed = parse_events_from_source(&path).expect("parse events");
        assert_eq!(parsed.len(), 1);
    }

    #[test]
    fn parse_events_from_json_object_events_works() {
        let dir = create_temp_dir("parse_json_object");
        let path = dir.join("sample.json");
        let events = vec![make_ingress_event("ev-1", 1, 0, "hello")];
        fs::write(&path, serde_json::json!({"events": events}).to_string()).expect("write json");

        let parsed = parse_events_from_source(&path).expect("parse events");
        assert_eq!(parsed.len(), 1);
    }

    #[test]
    fn quality_filter_rejects_small_artifact() {
        let mut artifact = ReplayArtifact {
            schema_version: REPLAY_FIXTURE_HARVEST_SCHEMA_VERSION.to_string(),
            artifact_id: "artifact-1".to_string(),
            source_path: "source.jsonl".to_string(),
            metadata: HarvestMetadata {
                harvest_date: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
                source_type: HarvestSourceType::ManualExport,
                incident_id: None,
                event_count: 10,
                pane_count: 2,
                decision_count: 1,
                sensitivity_tier: SensitivityTier::T1,
                checksum_sha256: String::new(),
                risk_tags: vec![],
                tombstoned: false,
                retention_tier_days: None,
            },
            events: vec![],
        };
        artifact.metadata.checksum_sha256 = checksum_events(&artifact.events).expect("checksum");
        let harvester = FixtureHarvester::default();
        let reason = harvester
            .quality_failure_reason(&artifact)
            .expect("expected quality failure");
        assert!(reason.contains("event_count_below_min"));
    }

    #[test]
    fn quality_filter_rejects_single_pane_artifact() {
        let mut events = Vec::new();
        for i in 0..120 {
            events.push(make_egress_event(
                &format!("ev-{i}"),
                1,
                i as u64,
                "safe line",
            ));
        }
        events.push(make_decision_event("ev-decision", 1, 999));
        let artifact = build_artifact(events);

        let reason = FixtureHarvester::default()
            .quality_failure_reason(&artifact)
            .expect("expected pane count quality failure");
        assert!(reason.contains("pane_count_below_min"));
    }

    #[test]
    fn quality_filter_rejects_zero_decision_artifact() {
        let mut events = Vec::new();
        for i in 0..120 {
            events.push(make_egress_event(
                &format!("ev-{i}"),
                (i % 2) as u64 + 1,
                i as u64,
                "safe line",
            ));
        }
        let artifact = build_artifact(events);

        let reason = FixtureHarvester::default()
            .quality_failure_reason(&artifact)
            .expect("expected decision count quality failure");
        assert!(reason.contains("decision_count_below_min"));
    }

    #[test]
    fn retention_enforcer_tombstones_expired_t3_artifact() {
        let mut artifact = ReplayArtifact {
            schema_version: REPLAY_FIXTURE_HARVEST_SCHEMA_VERSION.to_string(),
            artifact_id: "artifact-1".to_string(),
            source_path: "source.jsonl".to_string(),
            metadata: HarvestMetadata {
                harvest_date: (Utc::now() - Duration::days(60))
                    .to_rfc3339_opts(SecondsFormat::Secs, true),
                source_type: HarvestSourceType::ManualExport,
                incident_id: None,
                event_count: 2,
                pane_count: 1,
                decision_count: 1,
                sensitivity_tier: SensitivityTier::T3,
                checksum_sha256: String::new(),
                risk_tags: vec![],
                tombstoned: false,
                retention_tier_days: None,
            },
            events: vec![
                make_ingress_event("ev-1", 1, 0, "hello"),
                make_decision_event("ev-2", 1, 1),
            ],
        };
        artifact.metadata.checksum_sha256 = checksum_events(&artifact.events).expect("checksum");

        RetentionEnforcer::default().apply(&mut artifact, Utc::now());

        assert!(artifact.metadata.tombstoned);
        assert_eq!(artifact.events.len(), 0);
        assert!(
            artifact
                .metadata
                .risk_tags
                .iter()
                .any(|tag| tag == "retention:tombstoned")
        );
    }

    #[test]
    fn registry_duplicate_detection_works() {
        let mut registry = ArtifactRegistry::default();
        registry.register(ArtifactRegistryEntry {
            artifact_id: "artifact-a".to_string(),
            artifact_path: "/tmp/a.ftreplay".to_string(),
            source_path: "/tmp/a.jsonl".to_string(),
            source_type: HarvestSourceType::IncidentRecording,
            incident_id: Some("inc-1".to_string()),
            event_count: 100,
            pane_count: 2,
            decision_count: 2,
            sensitivity_tier: SensitivityTier::T1,
            checksum_sha256: "abc".to_string(),
            risk_tags: vec!["tier:t1".to_string()],
            event_fingerprints: vec!["e1".to_string(), "e2".to_string(), "e3".to_string()],
        });

        let candidate: HashSet<String> = ["e1", "e2", "e3", "e4"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let duplicate = registry
            .find_duplicate(&candidate, 0.70)
            .expect("should detect duplicate");
        assert_eq!(duplicate.artifact_id, "artifact-a");
        assert!(duplicate.overlap_ratio >= 0.75);
    }

    #[test]
    fn registry_query_by_incident_and_risk_tag() {
        let mut registry = ArtifactRegistry::default();
        registry.register(ArtifactRegistryEntry {
            artifact_id: "artifact-a".to_string(),
            artifact_path: "/tmp/a.ftreplay".to_string(),
            source_path: "/tmp/a.jsonl".to_string(),
            source_type: HarvestSourceType::IncidentRecording,
            incident_id: Some("inc-1".to_string()),
            event_count: 100,
            pane_count: 2,
            decision_count: 2,
            sensitivity_tier: SensitivityTier::T2,
            checksum_sha256: "abc".to_string(),
            risk_tags: vec!["tier:t2".to_string(), "decisionful".to_string()],
            event_fingerprints: vec![],
        });

        assert_eq!(registry.query_by_incident("inc-1").len(), 1);
        assert_eq!(registry.query_by_risk_tag("decisionful").len(), 1);
    }

    #[test]
    fn artifact_validate_rejects_checksum_mismatch() {
        let artifact = ReplayArtifact {
            schema_version: REPLAY_FIXTURE_HARVEST_SCHEMA_VERSION.to_string(),
            artifact_id: "artifact-1".to_string(),
            source_path: "source.jsonl".to_string(),
            metadata: HarvestMetadata {
                harvest_date: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
                source_type: HarvestSourceType::ManualExport,
                incident_id: None,
                event_count: 1,
                pane_count: 1,
                decision_count: 0,
                sensitivity_tier: SensitivityTier::T1,
                checksum_sha256: "bad".to_string(),
                risk_tags: vec![],
                tombstoned: false,
                retention_tier_days: None,
            },
            events: vec![make_ingress_event("ev-1", 1, 0, "hello")],
        };

        let err = artifact.validate().expect_err("must fail");
        assert!(format!("{err}").contains("checksum mismatch"));
    }

    #[test]
    fn artifact_validate_rejects_event_count_mismatch() {
        let mut artifact = build_artifact(vec![make_ingress_event("ev-1", 1, 0, "hello")]);
        artifact.metadata.event_count = 2;
        let err = artifact.validate().expect_err("must fail");
        assert!(format!("{err}").contains("event count mismatch"));
    }

    #[test]
    fn artifact_validate_rejects_unknown_schema_version() {
        let mut artifact = build_artifact(vec![make_ingress_event("ev-1", 1, 0, "hello")]);
        artifact.schema_version = "bad.schema.v0".to_string();
        let err = artifact.validate().expect_err("must fail");
        assert!(format!("{err}").contains("unsupported replay fixture schema"));
    }

    #[test]
    fn retention_enforcer_keeps_recent_artifact() {
        let mut artifact = build_artifact(vec![
            make_egress_event("ev-1", 1, 0, "safe"),
            make_decision_event("ev-2", 2, 1),
        ]);
        artifact.metadata.sensitivity_tier = SensitivityTier::T1;
        artifact.metadata.harvest_date = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        let before_events = artifact.events.len();

        RetentionEnforcer::default().apply(&mut artifact, Utc::now());

        assert!(!artifact.metadata.tombstoned);
        assert_eq!(artifact.events.len(), before_events);
        assert_eq!(
            artifact.metadata.retention_tier_days,
            Some(RetentionPolicy::default().t1_days)
        );
    }

    #[test]
    fn parse_events_from_invalid_json_object_errors() {
        let dir = create_temp_dir("parse_json_invalid_object");
        let path = dir.join("sample.json");
        fs::write(&path, serde_json::json!({"not_events": true}).to_string()).expect("write json");

        let err = parse_events_from_source(&path).expect_err("must fail");
        assert!(format!("{err}").contains("must be an event array"));
    }

    #[test]
    fn redaction_hash_mode_scrubs_control_marker_details() {
        let mut harvester = FixtureHarvester::default();
        harvester.redaction_mode = RedactionMode::Hash;
        let mut event = make_decision_event("ev-1", 1, 0);
        if let RecorderEventPayload::ControlMarker { details, .. } = &mut event.payload {
            *details = serde_json::json!({
                "secret": "sk-abcdefghijklmnopqrstuvwxyz123456"
            });
        }

        harvester.apply_redaction(&mut event);

        let RecorderEventPayload::ControlMarker { details, .. } = &event.payload else {
            panic!("expected control marker payload");
        };
        let secret = details
            .get("secret")
            .and_then(serde_json::Value::as_str)
            .expect("string secret");
        assert!(secret.starts_with("sha256:"));
    }

    #[test]
    fn harvest_produces_metadata_and_artifact_id() {
        let dir = create_temp_dir("harvest_basic");
        let path = dir.join("incident_run.jsonl");

        let mut events = Vec::new();
        for i in 0..120 {
            events.push(make_egress_event(
                &format!("ev-{i}"),
                (i % 2) as u64 + 1,
                i as u64,
                "ok",
            ));
        }
        events.push(make_decision_event("ev-decision", 1, 200));
        write_jsonl(&path, &events);

        let artifact = FixtureHarvester::default()
            .harvest(HarvestSource::IncidentRecording(path.clone()))
            .expect("harvest should pass");

        assert_eq!(
            artifact.metadata.source_type,
            HarvestSourceType::IncidentRecording
        );
        assert!(artifact.metadata.event_count >= 100);
        assert!(artifact.metadata.pane_count >= 2);
        assert!(artifact.metadata.decision_count >= 1);
        assert!(artifact.artifact_id.starts_with("incident_recording-"));
        assert!(artifact.validate().is_ok());
    }

    #[test]
    fn discover_source_files_finds_supported_extensions_recursively() {
        let dir = create_temp_dir("discover");
        let nested = dir.join("nested");
        fs::create_dir_all(&nested).expect("create nested dir");
        fs::write(dir.join("a.jsonl"), "").expect("create a.jsonl");
        fs::write(nested.join("b.ndjson"), "").expect("create b.ndjson");
        fs::write(nested.join("c.json"), "{}").expect("create c.json");
        fs::write(nested.join("ignored.txt"), "").expect("create ignored.txt");

        let files = discover_source_files(&dir).expect("discover files");
        assert_eq!(files.len(), 3);
    }

    #[test]
    fn pipeline_harvests_and_registers_artifacts() {
        let source_dir = create_temp_dir("pipeline_source");
        let output_dir = create_temp_dir("pipeline_output");

        let source_path = source_dir.join("incident_case_a.jsonl");
        let mut events = Vec::new();
        for i in 0..120 {
            events.push(make_egress_event(
                &format!("ev-{i}"),
                (i % 2) as u64 + 1,
                i as u64,
                "safe line",
            ));
        }
        events.push(make_decision_event("ev-decision", 1, 500));
        write_jsonl(&source_path, &events);

        let report = HarvestPipeline::default()
            .harvest_directory(&source_dir, &output_dir, HarvestSourceFilter::All)
            .expect("pipeline success");

        assert_eq!(report.harvested, 1);
        assert_eq!(report.skipped, 0);
        assert_eq!(report.errors, 0);
        assert!(report.total_events >= 100);

        let registry = ArtifactRegistry::load_or_default(&output_dir.join(REGISTRY_FILE_NAME))
            .expect("registry load");
        assert_eq!(registry.artifacts.len(), 1);
    }

    #[test]
    fn pipeline_filters_non_incident_sources_when_requested() {
        let source_dir = create_temp_dir("pipeline_filter_source");
        let output_dir = create_temp_dir("pipeline_filter_output");

        let source_path = source_dir.join("manual_capture.jsonl");
        let mut events = Vec::new();
        for i in 0..120 {
            events.push(make_egress_event(
                &format!("ev-{i}"),
                (i % 2) as u64 + 1,
                i as u64,
                "safe line",
            ));
        }
        events.push(make_decision_event("ev-decision", 1, 500));
        write_jsonl(&source_path, &events);

        let report = HarvestPipeline::default()
            .harvest_directory(&source_dir, &output_dir, HarvestSourceFilter::IncidentOnly)
            .expect("pipeline success");

        assert_eq!(report.harvested, 0);
        assert_eq!(report.skipped, 1);
        assert_eq!(report.errors, 0);
    }

    #[test]
    fn pipeline_skips_duplicate_overlap() {
        let source_dir = create_temp_dir("pipeline_duplicate_source");
        let output_dir = create_temp_dir("pipeline_duplicate_output");

        let source_path_a = source_dir.join("incident_case_a.jsonl");
        let source_path_b = source_dir.join("incident_case_b.jsonl");

        let mut events = Vec::new();
        for i in 0..120 {
            events.push(make_egress_event(
                &format!("ev-{i}"),
                (i % 2) as u64 + 1,
                i as u64,
                "safe line",
            ));
        }
        events.push(make_decision_event("ev-decision", 1, 500));
        write_jsonl(&source_path_a, &events);
        write_jsonl(&source_path_b, &events);

        let pipeline = HarvestPipeline::default();
        let first_report = pipeline
            .harvest_directory(&source_dir, &output_dir, HarvestSourceFilter::All)
            .expect("first run success");
        assert!(first_report.harvested >= 1);

        let second_report = pipeline
            .harvest_directory(&source_dir, &output_dir, HarvestSourceFilter::All)
            .expect("second run success");
        assert!(second_report.skipped >= 1);
        assert!(second_report.entries.iter().any(|entry| {
            entry
                .reason
                .as_deref()
                .unwrap_or_default()
                .contains("duplicate_of")
        }));
    }
}
