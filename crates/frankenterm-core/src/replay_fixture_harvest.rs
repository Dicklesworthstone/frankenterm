//! Replay fixture harvesting pipeline for deterministic regression artifacts.
//!
//! This module supports bead `ft-og6q6.7.1` by turning real incident/swarm
//! captures into curated `.ftreplay` artifacts with:
//! - deterministic redaction and sensitivity tagging,
//! - quality filters,
//! - duplicate overlap detection,
//! - registry persistence for query by incident/risk tags.

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use flate2::{Compression as GzipCompression, read::GzDecoder, write::GzEncoder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::event_id::{RecorderMergeKey, StreamKind};
use crate::policy::Redactor;
use crate::recorder_invariants::{InvariantChecker, ViolationKind, ViolationSeverity};
use crate::recording::{
    RecorderControlMarkerType, RecorderEvent, RecorderEventPayload, RecorderRedactionLevel,
    parse_recorder_event_json,
};

pub const REPLAY_FIXTURE_HARVEST_SCHEMA_VERSION: &str = "ft.replay.fixture.harvest.v1";
pub const REPLAY_FIXTURE_REGISTRY_SCHEMA_VERSION: &str = "ft.replay.fixture.registry.v1";
pub const FTREPLAY_FILE_SCHEMA_VERSION: &str = "ftreplay.v1";
pub const FTREPLAY_CHUNK_MANIFEST_SCHEMA_VERSION: &str = "ftreplay.chunk_manifest.v1";
const REGISTRY_FILE_NAME: &str = "fixture_registry.json";
const RECORDER_EVENT_FILE_EXTENSIONS: [&str; 4] = ["jsonl", "ndjson", "json", "ftreplay"];
const FTREPLAY_HEADER_SECTION: &str = "--- ftreplay-header ---";
const FTREPLAY_ENTITIES_SECTION: &str = "--- ftreplay-entities ---";
const FTREPLAY_TIMELINE_SECTION: &str = "--- ftreplay-timeline ---";
const FTREPLAY_DECISIONS_SECTION: &str = "--- ftreplay-decisions ---";
const FTREPLAY_FOOTER_SECTION: &str = "--- ftreplay-footer ---";
const DUPLICATE_OVERLAP_THRESHOLD: f64 = 0.90;
const DEFAULT_MAX_EVENTS_PER_CHUNK: usize = 100_000;

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

    #[must_use]
    pub fn as_ftreplay_label(self) -> &'static str {
        match self {
            Self::T1 => "T1",
            Self::T2 => "T2",
            Self::T3 => "T3",
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

        let risk_tags = derive_risk_tags(sensitivity_tier, event_count, decision_count, false);

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
                redact_json_value(details, &self.redactor, self.redaction_mode);
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactCompression {
    None,
    Gzip,
    Zstd,
}

impl ArtifactCompression {
    #[must_use]
    pub fn from_path(path: &Path) -> Self {
        if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("zst"))
        {
            return Self::Zstd;
        }
        if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("gz"))
        {
            return Self::Gzip;
        }
        Self::None
    }

    #[must_use]
    pub const fn extension_suffix(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Gzip => ".gz",
            Self::Zstd => ".zst",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ArtifactWriterConfig {
    pub max_events_per_chunk: usize,
}

impl Default for ArtifactWriterConfig {
    fn default() -> Self {
        Self {
            max_events_per_chunk: DEFAULT_MAX_EVENTS_PER_CHUNK,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactChunkEntry {
    pub index: usize,
    pub path: String,
    pub event_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_event_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_event_id: Option<String>,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactChunkManifest {
    pub schema_version: String,
    pub artifact_id: String,
    pub total_event_count: usize,
    pub chunk_size_events: usize,
    pub chunk_count: usize,
    pub compression: ArtifactCompression,
    #[serde(default)]
    pub chunks: Vec<ArtifactChunkEntry>,
}

#[derive(Debug, Clone)]
pub struct ArtifactWriteResult {
    pub primary_path: PathBuf,
    pub chunk_paths: Vec<PathBuf>,
    pub manifest_path: Option<PathBuf>,
    pub compression: ArtifactCompression,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FtreplayValidationReport {
    pub event_count: usize,
    pub decision_count: usize,
    pub entity_count: usize,
    pub merge_order_verified: bool,
    pub sequence_monotonicity_verified: bool,
    pub causality_integrity_verified: bool,
}

#[derive(Debug, Clone)]
pub struct FtreplayWriter {
    pub config: ArtifactWriterConfig,
    staged_entities: Vec<serde_json::Value>,
    staged_events: Vec<RecorderEvent>,
    staged_decisions: Vec<serde_json::Value>,
}

impl Default for FtreplayWriter {
    fn default() -> Self {
        Self {
            config: ArtifactWriterConfig::default(),
            staged_entities: Vec::new(),
            staged_events: Vec::new(),
            staged_decisions: Vec::new(),
        }
    }
}

impl FtreplayWriter {
    #[must_use]
    pub fn with_config(config: ArtifactWriterConfig) -> Self {
        Self {
            config,
            ..Self::default()
        }
    }

    pub fn add_entity(&mut self, entity: serde_json::Value) {
        self.staged_entities.push(entity);
    }

    pub fn add_event(&mut self, event: RecorderEvent) {
        self.staged_events.push(event);
    }

    pub fn add_decision(&mut self, decision: serde_json::Value) {
        self.staged_decisions.push(decision);
    }

    pub fn write(
        &self,
        artifact: &ReplayArtifact,
        artifact_path: &Path,
    ) -> crate::Result<ArtifactWriteResult> {
        let mut cloned = self.clone();
        cloned.finalize(artifact, artifact_path)
    }

    pub fn finalize(
        &mut self,
        artifact: &ReplayArtifact,
        artifact_path: &Path,
    ) -> crate::Result<ArtifactWriteResult> {
        let mut effective = artifact.clone();

        if !self.staged_events.is_empty() {
            self.staged_events.sort_by_key(RecorderMergeKey::from_event);
            effective.events = std::mem::take(&mut self.staged_events);
            effective.metadata.event_count = effective.events.len();
            effective.metadata.pane_count = unique_pane_count(&effective.events);
            effective.metadata.decision_count = count_decisions(&effective.events);
            effective.metadata.sensitivity_tier = max_sensitivity_tier(&effective.events);
            effective.metadata.checksum_sha256 = checksum_events(&effective.events)?;
            effective.metadata.risk_tags = derive_risk_tags(
                effective.metadata.sensitivity_tier,
                effective.metadata.event_count,
                effective.metadata.decision_count,
                effective.metadata.tombstoned,
            );
        }

        effective.validate()?;

        let compression = ArtifactCompression::from_path(artifact_path);
        let entity_override = if self.staged_entities.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.staged_entities))
        };
        let decision_override = if self.staged_decisions.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.staged_decisions))
        };

        if effective.events.len() > self.config.max_events_per_chunk {
            return write_chunked_ftreplay(
                &effective,
                artifact_path,
                self.config.max_events_per_chunk,
                compression,
            );
        }

        write_single_ftreplay_file(
            &effective,
            artifact_path,
            compression,
            entity_override.as_deref(),
            decision_override.as_deref(),
        )?;
        Ok(ArtifactWriteResult {
            primary_path: artifact_path.to_path_buf(),
            chunk_paths: Vec::new(),
            manifest_path: None,
            compression,
        })
    }
}

pub struct FtreplayValidator;

impl FtreplayValidator {
    pub fn validate_file(path: &Path) -> crate::Result<FtreplayValidationReport> {
        let payload = read_ftreplay_payload(path)?;
        validate_ftreplay_payload(&payload)
    }
}

#[derive(Debug, Clone)]
pub struct FtreplayArtifact {
    pub schema_version: String,
    pub source_path: String,
    pub compression: ArtifactCompression,
    pub entities: Vec<serde_json::Value>,
    pub decisions: Vec<serde_json::Value>,
    pub events: Vec<RecorderEvent>,
    pub compatibility: ArtifactCompatibility,
    pub validation_report: FtreplayValidationReport,
    pub migration_report: Option<MigrationReport>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactCompatibility {
    Exact,
    MigrationAvailable { chain: Vec<String> },
    Incompatible { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    pub from_version: String,
    pub target_version: String,
    pub events_migrated: usize,
    pub fields_added: usize,
    pub fields_removed: usize,
    pub warnings: Vec<String>,
}

pub trait ArtifactMigration: Send + Sync {
    fn source_version(&self) -> &'static str;
    fn to_version(&self) -> &'static str;
    fn migrate_event(&self, event: serde_json::Value) -> crate::Result<serde_json::Value>;
}

#[derive(Debug)]
struct V0ToV1Migration;

impl ArtifactMigration for V0ToV1Migration {
    fn source_version(&self) -> &'static str {
        "ftreplay.v0"
    }

    fn to_version(&self) -> &'static str {
        FTREPLAY_FILE_SCHEMA_VERSION
    }

    fn migrate_event(&self, mut event: serde_json::Value) -> crate::Result<serde_json::Value> {
        if let Some(object) = event.as_object_mut() {
            object
                .entry("schema_version".to_string())
                .or_insert_with(|| serde_json::Value::String("ft.recorder.event.v1".to_string()));
        }
        Ok(event)
    }
}

#[derive(Debug)]
struct V1ToV2Migration;

impl ArtifactMigration for V1ToV2Migration {
    fn source_version(&self) -> &'static str {
        FTREPLAY_FILE_SCHEMA_VERSION
    }

    fn to_version(&self) -> &'static str {
        "ftreplay.v2"
    }

    fn migrate_event(&self, mut event: serde_json::Value) -> crate::Result<serde_json::Value> {
        if let Some(object) = event.as_object_mut() {
            object
                .entry("record_origin".to_string())
                .or_insert_with(|| serde_json::Value::String("capture".to_string()));
        }
        Ok(event)
    }
}

#[derive(Debug)]
struct V2ToV3Migration;

impl ArtifactMigration for V2ToV3Migration {
    fn source_version(&self) -> &'static str {
        "ftreplay.v2"
    }

    fn to_version(&self) -> &'static str {
        "ftreplay.v3"
    }

    fn migrate_event(&self, mut event: serde_json::Value) -> crate::Result<serde_json::Value> {
        if let Some(object) = event.as_object_mut() {
            if let Some(value) = object.remove("record_origin") {
                object.insert("capture_origin".to_string(), value);
            } else {
                object
                    .entry("capture_origin".to_string())
                    .or_insert_with(|| serde_json::Value::String("capture".to_string()));
            }
        }
        Ok(event)
    }
}

#[derive(Clone)]
pub struct SchemaMigrator {
    migrations: Vec<Arc<dyn ArtifactMigration>>,
}

impl Default for SchemaMigrator {
    fn default() -> Self {
        Self {
            migrations: vec![
                Arc::new(V0ToV1Migration),
                Arc::new(V1ToV2Migration),
                Arc::new(V2ToV3Migration),
            ],
        }
    }
}

impl SchemaMigrator {
    #[must_use]
    pub fn new(migrations: Vec<Arc<dyn ArtifactMigration>>) -> Self {
        Self { migrations }
    }

    #[must_use]
    pub fn supported_versions(&self) -> Vec<String> {
        let mut versions = BTreeSet::new();
        for migration in &self.migrations {
            versions.insert(migration.source_version().to_string());
            versions.insert(migration.to_version().to_string());
        }
        versions.into_iter().collect()
    }

    fn resolve_chain_indices(&self, from_version: &str, to_version: &str) -> Option<Vec<usize>> {
        if from_version == to_version {
            return Some(Vec::new());
        }

        let mut queue = VecDeque::new();
        let mut visited = HashSet::new();
        queue.push_back((from_version.to_string(), Vec::<usize>::new()));
        visited.insert(from_version.to_string());

        while let Some((current_version, path)) = queue.pop_front() {
            for (idx, migration) in self.migrations.iter().enumerate() {
                if migration.source_version() != current_version {
                    continue;
                }

                let next_version = migration.to_version().to_string();
                let mut next_path = path.clone();
                next_path.push(idx);

                if next_version == to_version {
                    return Some(next_path);
                }

                if visited.insert(next_version.clone()) {
                    queue.push_back((next_version, next_path));
                }
            }
        }

        None
    }

    #[must_use]
    pub fn resolve_chain(&self, from_version: &str, to_version: &str) -> Option<Vec<String>> {
        let indices = self.resolve_chain_indices(from_version, to_version)?;
        let chain = indices
            .iter()
            .map(|idx| {
                let migration = &self.migrations[*idx];
                format!("{}->{}", migration.source_version(), migration.to_version())
            })
            .collect();
        Some(chain)
    }

    pub fn migrate_timeline_values(
        &self,
        from_version: &str,
        target_version: &str,
        timeline_values: &[serde_json::Value],
    ) -> crate::Result<(Vec<serde_json::Value>, MigrationReport)> {
        let chain_indices = self
            .resolve_chain_indices(from_version, target_version)
            .ok_or_else(|| {
                crate::Error::Runtime(format!(
                    "no migration path from '{}' to '{}'",
                    from_version, target_version
                ))
            })?;

        let mut values = timeline_values.to_vec();
        let mut events_migrated = 0usize;
        let mut fields_added = 0usize;
        let mut fields_removed = 0usize;
        let mut warnings = Vec::new();

        for index in chain_indices {
            let migration = &self.migrations[index];
            let mut next_values = Vec::with_capacity(values.len());
            for value in values {
                let before_fields = value.as_object().map_or(0, serde_json::Map::len);
                let migrated = migration.migrate_event(value)?;
                let after_fields = migrated.as_object().map_or(0, serde_json::Map::len);
                fields_added += after_fields.saturating_sub(before_fields);
                fields_removed += before_fields.saturating_sub(after_fields);
                if !migrated.is_object() {
                    warnings.push(format!(
                        "migration {}->{} produced non-object timeline event",
                        migration.source_version(),
                        migration.to_version()
                    ));
                }
                next_values.push(migrated);
            }
            events_migrated += next_values.len();
            values = next_values;
        }

        let report = MigrationReport {
            from_version: from_version.to_string(),
            target_version: target_version.to_string(),
            events_migrated,
            fields_added,
            fields_removed,
            warnings,
        };
        Ok((values, report))
    }
}

#[derive(Clone)]
pub struct CompatibilityChecker {
    engine_version: String,
    migrator: SchemaMigrator,
}

impl CompatibilityChecker {
    #[must_use]
    pub fn new(engine_version: impl Into<String>, migrator: SchemaMigrator) -> Self {
        Self {
            engine_version: engine_version.into(),
            migrator,
        }
    }

    #[must_use]
    pub fn check(&self, artifact_version: &str) -> ArtifactCompatibility {
        if artifact_version == self.engine_version {
            return ArtifactCompatibility::Exact;
        }

        if let Some(chain) = self
            .migrator
            .resolve_chain(artifact_version, &self.engine_version)
        {
            return ArtifactCompatibility::MigrationAvailable { chain };
        }

        let supported = self.migrator.supported_versions().join(", ");
        ArtifactCompatibility::Incompatible {
            reason: format!(
                "artifact schema '{}' is incompatible with engine '{}'; no migration chain exists. Supported versions: [{}]. Remediation: upgrade ft or regenerate artifact with a supported schema.",
                artifact_version, self.engine_version, supported
            ),
        }
    }
}

#[derive(Clone)]
pub struct ArtifactReader {
    engine_schema_version: String,
    migrator: SchemaMigrator,
    compatibility_checker: CompatibilityChecker,
}

impl Default for ArtifactReader {
    fn default() -> Self {
        let engine_schema_version = FTREPLAY_FILE_SCHEMA_VERSION.to_string();
        let migrator = SchemaMigrator::default();
        let compatibility_checker =
            CompatibilityChecker::new(engine_schema_version.clone(), migrator.clone());
        Self {
            engine_schema_version,
            migrator,
            compatibility_checker,
        }
    }
}

impl ArtifactReader {
    #[must_use]
    pub fn new(engine_schema_version: impl Into<String>, migrator: SchemaMigrator) -> Self {
        let engine_schema_version = engine_schema_version.into();
        let compatibility_checker =
            CompatibilityChecker::new(engine_schema_version.clone(), migrator.clone());
        Self {
            engine_schema_version,
            migrator,
            compatibility_checker,
        }
    }

    pub fn open(&self, path: &Path) -> crate::Result<FtreplayArtifact> {
        let payload = read_ftreplay_payload(path)?;
        let text = std::str::from_utf8(&payload).map_err(|err| {
            crate::Error::Runtime(format!(
                "failed to decode artifact {} as UTF-8: {err}",
                path.display()
            ))
        })?;
        let sections = parse_ftreplay_sections(text)?;
        let header: serde_json::Value = serde_json::from_str(&sections.header).map_err(|err| {
            crate::Error::Runtime(format!(
                "failed to parse ftreplay header JSON in {}: {err}",
                path.display()
            ))
        })?;
        let artifact_version = header
            .get("schema_version")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                crate::Error::Runtime(format!(
                    "ftreplay header in {} is missing schema_version",
                    path.display()
                ))
            })?;

        let compatibility = self.compatibility_checker.check(artifact_version);
        if let ArtifactCompatibility::Incompatible { reason } = &compatibility {
            return Err(crate::Error::Runtime(reason.clone()));
        }

        let validation_report = validate_ftreplay_payload_for_schema(&payload, artifact_version)?;
        let entities = parse_json_value_lines(path, "entities", &sections.entities)?;
        let decisions = parse_json_value_lines(path, "decisions", &sections.decisions)?;
        let mut timeline_values = parse_json_value_lines(path, "timeline", &sections.timeline)?;

        let migration_report = match &compatibility {
            ArtifactCompatibility::MigrationAvailable { .. } => {
                let (migrated, report) = self.migrator.migrate_timeline_values(
                    artifact_version,
                    &self.engine_schema_version,
                    &timeline_values,
                )?;
                timeline_values = migrated;
                Some(report)
            }
            _ => None,
        };

        let events = parse_timeline_values_to_events(path, &timeline_values)?;
        let schema_version = if migration_report.is_some() {
            self.engine_schema_version.clone()
        } else {
            artifact_version.to_string()
        };

        Ok(FtreplayArtifact {
            schema_version,
            source_path: path.display().to_string(),
            compression: ArtifactCompression::from_path(path),
            entities,
            decisions,
            events,
            compatibility,
            validation_report,
            migration_report,
        })
    }

    pub fn stream_events(&self, path: &Path) -> crate::Result<ArtifactEventStream> {
        let payload = read_ftreplay_payload(path)?;
        let text = std::str::from_utf8(&payload).map_err(|err| {
            crate::Error::Runtime(format!(
                "failed to decode artifact {} as UTF-8: {err}",
                path.display()
            ))
        })?;
        let sections = parse_ftreplay_sections(text)?;
        let header: serde_json::Value = serde_json::from_str(&sections.header)?;
        let artifact_version = header
            .get("schema_version")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                crate::Error::Runtime(format!(
                    "ftreplay header in {} is missing schema_version",
                    path.display()
                ))
            })?;

        match self.compatibility_checker.check(artifact_version) {
            ArtifactCompatibility::Exact => {}
            ArtifactCompatibility::MigrationAvailable { chain } => {
                return Err(crate::Error::Runtime(format!(
                    "streaming mode requires exact schema compatibility; migration chain [{}] available. Use ArtifactReader::open for in-memory migration.",
                    chain.join(", ")
                )));
            }
            ArtifactCompatibility::Incompatible { reason } => {
                return Err(crate::Error::Runtime(reason));
            }
        }

        let _ = validate_ftreplay_payload_for_schema(&payload, artifact_version)?;
        ArtifactEventStream::open(path.to_path_buf())
    }
}

pub struct ArtifactEventStream {
    path: PathBuf,
    lines: std::io::Lines<Box<dyn BufRead>>,
    line_number: usize,
    in_timeline: bool,
    finished: bool,
}

impl ArtifactEventStream {
    fn open(path: PathBuf) -> crate::Result<Self> {
        let reader = open_ftreplay_bufread(&path)?;
        Ok(Self {
            path,
            lines: reader.lines(),
            line_number: 0,
            in_timeline: false,
            finished: false,
        })
    }
}

impl Iterator for ArtifactEventStream {
    type Item = crate::Result<RecorderEvent>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }

        for line_result in self.lines.by_ref() {
            self.line_number += 1;
            let line = match line_result {
                Ok(line) => line,
                Err(err) => return Some(Err(err.into())),
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            if trimmed == FTREPLAY_TIMELINE_SECTION {
                self.in_timeline = true;
                continue;
            }

            if trimmed.starts_with("--- ftreplay-") && trimmed.ends_with("---") {
                if self.in_timeline {
                    self.finished = true;
                    return None;
                }
                continue;
            }

            if !self.in_timeline {
                continue;
            }

            return Some(parse_timeline_line_to_event(
                &self.path,
                self.line_number,
                trimmed,
            ));
        }

        self.finished = true;
        None
    }
}

fn parse_json_value_lines(
    path: &Path,
    section_name: &str,
    lines: &[String],
) -> crate::Result<Vec<serde_json::Value>> {
    lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            serde_json::from_str::<serde_json::Value>(line).map_err(|err| {
                crate::Error::Runtime(format!(
                    "failed to parse {} JSON line {} in {}: {err}",
                    section_name,
                    index + 1,
                    path.display()
                ))
            })
        })
        .collect()
}

fn parse_timeline_values_to_events(
    path: &Path,
    timeline_values: &[serde_json::Value],
) -> crate::Result<Vec<RecorderEvent>> {
    timeline_values
        .iter()
        .enumerate()
        .map(|(line_idx, value)| {
            let line = serde_json::to_string(value)?;
            parse_timeline_line_to_event(path, line_idx + 1, &line)
        })
        .collect()
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
            let write_result = write_artifact(&artifact, &artifact_path)?;

            let entry = ArtifactRegistryEntry {
                artifact_id: artifact.artifact_id.clone(),
                artifact_path: write_result.primary_path.display().to_string(),
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
                artifact_path: Some(write_result.primary_path.display().to_string()),
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
            if is_supported_source_file(&path) {
                out.push(path);
            }
        }
    }

    out.sort();
    Ok(out)
}

fn parse_events_from_source(path: &Path) -> crate::Result<Vec<RecorderEvent>> {
    if is_ftreplay_path(path) {
        return parse_events_from_ftreplay(path);
    }

    let ext = path
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();

    match ext.as_str() {
        "json" => parse_events_from_json(path),
        _ => parse_events_from_jsonl(path),
    }
}

fn is_supported_source_file(path: &Path) -> bool {
    if is_ftreplay_path(path) {
        return true;
    }
    let Some(ext) = path.extension().and_then(std::ffi::OsStr::to_str) else {
        return false;
    };
    RECORDER_EVENT_FILE_EXTENSIONS
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(ext))
}

fn is_ftreplay_path(path: &Path) -> bool {
    let lowered = path.to_string_lossy().to_ascii_lowercase();
    lowered.ends_with(".ftreplay")
        || lowered.ends_with(".ftreplay.gz")
        || lowered.ends_with(".ftreplay.zst")
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
    parse_events_from_json_value(value, path)
}

fn parse_events_from_json_value(
    value: serde_json::Value,
    path: &Path,
) -> crate::Result<Vec<RecorderEvent>> {
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

fn parse_events_from_ftreplay(path: &Path) -> crate::Result<Vec<RecorderEvent>> {
    let payload = read_ftreplay_payload(path)?;
    let text = String::from_utf8(payload).map_err(|err| {
        crate::Error::Runtime(format!(
            "failed to decode ftreplay payload as UTF-8 ({}): {err}",
            path.display()
        ))
    })?;

    let sections = parse_ftreplay_sections(&text);
    if let Ok(parsed) = sections {
        let mut events = Vec::with_capacity(parsed.timeline.len());
        for (line_idx, line) in parsed.timeline.iter().enumerate() {
            let parsed_event = parse_timeline_line_to_event(path, line_idx + 1, line)?;
            events.push(parsed_event);
        }
        return Ok(events);
    }

    let value: serde_json::Value = serde_json::from_str(&text).map_err(|err| {
        crate::Error::Runtime(format!(
            "failed to parse ftreplay source {}: {err}",
            path.display()
        ))
    })?;
    parse_events_from_json_value(value, path)
}

#[derive(Debug, Default)]
struct ParsedFtreplaySections {
    header: String,
    entities: Vec<String>,
    timeline: Vec<String>,
    decisions: Vec<String>,
    footer: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParsedSection {
    None,
    Header,
    Entities,
    Timeline,
    Decisions,
    Footer,
    AfterFooter,
}

fn parse_ftreplay_sections(payload: &str) -> crate::Result<ParsedFtreplaySections> {
    let mut parsed = ParsedFtreplaySections::default();
    let mut current = ParsedSection::None;
    let mut last_section_rank: i8 = -1;

    for (line_idx, raw_line) in payload.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        if line.starts_with("--- ftreplay-") && line.ends_with("---") {
            let (next, rank) = match line {
                FTREPLAY_HEADER_SECTION => (ParsedSection::Header, 0),
                FTREPLAY_ENTITIES_SECTION => (ParsedSection::Entities, 1),
                FTREPLAY_TIMELINE_SECTION => (ParsedSection::Timeline, 2),
                FTREPLAY_DECISIONS_SECTION => (ParsedSection::Decisions, 3),
                FTREPLAY_FOOTER_SECTION => (ParsedSection::Footer, 4),
                _ => {
                    if matches!(current, ParsedSection::Footer | ParsedSection::AfterFooter) {
                        current = ParsedSection::AfterFooter;
                        continue;
                    }
                    return Err(crate::Error::Runtime(format!(
                        "unknown ftreplay section marker '{}' at line {}",
                        line,
                        line_idx + 1
                    )));
                }
            };

            if i8::try_from(rank).unwrap_or_default() <= last_section_rank {
                return Err(crate::Error::Runtime(format!(
                    "duplicate or out-of-order ftreplay section marker '{}' at line {}",
                    line,
                    line_idx + 1
                )));
            }
            if i8::try_from(rank).unwrap_or_default() > last_section_rank + 1 {
                return Err(crate::Error::Runtime(format!(
                    "missing ftreplay section before '{}' at line {}",
                    line,
                    line_idx + 1
                )));
            }

            current = next;
            last_section_rank = i8::try_from(rank).unwrap_or_default();
            continue;
        }

        match current {
            ParsedSection::Header => {
                if !parsed.header.is_empty() {
                    return Err(crate::Error::Runtime(
                        "ftreplay header must be a single JSON line".to_string(),
                    ));
                }
                parsed.header = line.to_string();
            }
            ParsedSection::Entities => parsed.entities.push(line.to_string()),
            ParsedSection::Timeline => parsed.timeline.push(line.to_string()),
            ParsedSection::Decisions => parsed.decisions.push(line.to_string()),
            ParsedSection::Footer => {
                if !parsed.footer.is_empty() {
                    return Err(crate::Error::Runtime(
                        "ftreplay footer must be a single JSON line".to_string(),
                    ));
                }
                parsed.footer = line.to_string();
                current = ParsedSection::AfterFooter;
            }
            ParsedSection::AfterFooter => {}
            ParsedSection::None => {
                return Err(crate::Error::Runtime(format!(
                    "unexpected content before ftreplay header at line {}",
                    line_idx + 1
                )));
            }
        }
    }

    if last_section_rank != 4 || parsed.header.is_empty() || parsed.footer.is_empty() {
        return Err(crate::Error::Runtime(
            "ftreplay payload missing required sections".to_string(),
        ));
    }

    Ok(parsed)
}

fn parse_timeline_line_to_event(
    path: &Path,
    line_idx: usize,
    line: &str,
) -> crate::Result<RecorderEvent> {
    parse_recorder_event_json(line).map_err(|err| {
        crate::Error::Runtime(format!(
            "failed to parse ftreplay timeline event at {}:{}: {err}",
            path.display(),
            line_idx
        ))
    })
}

fn write_artifact(
    artifact: &ReplayArtifact,
    artifact_path: &Path,
) -> crate::Result<ArtifactWriteResult> {
    FtreplayWriter::default().write(artifact, artifact_path)
}

fn write_single_ftreplay_file(
    artifact: &ReplayArtifact,
    artifact_path: &Path,
    compression: ArtifactCompression,
    entities_override: Option<&[serde_json::Value]>,
    decisions_override: Option<&[serde_json::Value]>,
) -> crate::Result<()> {
    let payload = render_ftreplay_payload(artifact, entities_override, decisions_override)?;
    write_ftreplay_payload(artifact_path, payload.as_bytes(), compression)
}

fn write_chunked_ftreplay(
    artifact: &ReplayArtifact,
    artifact_path: &Path,
    max_events_per_chunk: usize,
    compression: ArtifactCompression,
) -> crate::Result<ArtifactWriteResult> {
    if max_events_per_chunk == 0 {
        return Err(crate::Error::Runtime(
            "max_events_per_chunk must be greater than zero".to_string(),
        ));
    }

    let output_dir = artifact_path.parent().ok_or_else(|| {
        crate::Error::Runtime(format!(
            "artifact path has no parent directory: {}",
            artifact_path.display()
        ))
    })?;
    let stem = artifact_chunk_stem(artifact_path);
    let mut chunk_paths = Vec::new();
    let mut manifest_chunks = Vec::new();

    for (offset, chunk_events) in artifact.events.chunks(max_events_per_chunk).enumerate() {
        let chunk_index = offset + 1;
        let mut chunk_artifact = artifact.clone();
        chunk_artifact.artifact_id = format!("{}-chunk-{chunk_index:04}", artifact.artifact_id);
        chunk_artifact.events = chunk_events.to_vec();
        chunk_artifact.metadata.event_count = chunk_artifact.events.len();
        chunk_artifact.metadata.pane_count = unique_pane_count(&chunk_artifact.events);
        chunk_artifact.metadata.decision_count = count_decisions(&chunk_artifact.events);
        chunk_artifact.metadata.checksum_sha256 = checksum_events(&chunk_artifact.events)?;
        chunk_artifact.metadata.sensitivity_tier = max_sensitivity_tier(&chunk_artifact.events);
        chunk_artifact.metadata.risk_tags = derive_risk_tags(
            chunk_artifact.metadata.sensitivity_tier,
            chunk_artifact.metadata.event_count,
            chunk_artifact.metadata.decision_count,
            chunk_artifact.metadata.tombstoned,
        );
        chunk_artifact.validate()?;

        let chunk_file_name = format!(
            "{stem}.chunk-{chunk_index:04}.ftreplay{}",
            compression.extension_suffix()
        );
        let chunk_path = output_dir.join(&chunk_file_name);
        write_single_ftreplay_file(&chunk_artifact, &chunk_path, compression, None, None)?;
        let chunk_sha = checksum_bytes(&fs::read(&chunk_path)?);

        manifest_chunks.push(ArtifactChunkEntry {
            index: chunk_index,
            path: chunk_file_name,
            event_count: chunk_artifact.metadata.event_count,
            start_event_id: chunk_artifact
                .events
                .first()
                .map(|event| event.event_id.clone()),
            end_event_id: chunk_artifact
                .events
                .last()
                .map(|event| event.event_id.clone()),
            sha256: chunk_sha,
        });
        chunk_paths.push(chunk_path);
    }

    let manifest = ArtifactChunkManifest {
        schema_version: FTREPLAY_CHUNK_MANIFEST_SCHEMA_VERSION.to_string(),
        artifact_id: artifact.artifact_id.clone(),
        total_event_count: artifact.events.len(),
        chunk_size_events: max_events_per_chunk,
        chunk_count: manifest_chunks.len(),
        compression,
        chunks: manifest_chunks,
    };
    let manifest_path = output_dir.join(format!("{stem}.manifest.json"));
    let manifest_json = serde_json::to_string_pretty(&manifest)?;
    fs::write(&manifest_path, manifest_json)?;

    Ok(ArtifactWriteResult {
        primary_path: manifest_path.clone(),
        chunk_paths,
        manifest_path: Some(manifest_path),
        compression,
    })
}

fn artifact_chunk_stem(path: &Path) -> String {
    let file_name = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or("artifact")
        .to_string();
    let mut stem = file_name;
    for suffix in [".gz", ".zst"] {
        if stem.ends_with(suffix) {
            stem.truncate(stem.len() - suffix.len());
        }
    }
    if stem.ends_with(".ftreplay") {
        stem.truncate(stem.len() - ".ftreplay".len());
    }
    stem
}

fn render_ftreplay_payload(
    artifact: &ReplayArtifact,
    entities_override: Option<&[serde_json::Value]>,
    decisions_override: Option<&[serde_json::Value]>,
) -> crate::Result<String> {
    let mut ordered_events = artifact.events.clone();
    ordered_events.sort_by_key(RecorderMergeKey::from_event);

    let entity_values = entities_override
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| build_ftreplay_entities(&ordered_events));
    let entity_lines = serialize_jsonl_lines(&entity_values)?;
    let timeline_lines: Vec<String> = ordered_events
        .iter()
        .enumerate()
        .map(|(position, event)| event_to_timeline_entry(event, position))
        .collect::<crate::Result<Vec<_>>>()?;
    let decision_values = decisions_override
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| build_ftreplay_decision_entries(&ordered_events));
    let decision_lines = serialize_jsonl_lines(&decision_values)?;

    let entities_sha = checksum_lines(&entity_lines);
    let timeline_sha = checksum_lines(&timeline_lines);
    let decisions_sha = checksum_lines(&decision_lines);

    let stream_domains = ordered_events
        .iter()
        .map(|event| (event.pane_id, StreamKind::from_payload(&event.payload)))
        .collect::<BTreeSet<_>>()
        .len();
    let gap_count = ordered_events
        .iter()
        .filter(|event| {
            matches!(
                event.payload,
                RecorderEventPayload::EgressOutput { is_gap: true, .. }
            )
        })
        .count();
    let checker_report = InvariantChecker::new().check(&ordered_events);
    let warning_count = checker_report
        .violations
        .iter()
        .filter(|violation| violation.severity == ViolationSeverity::Warning)
        .count();
    let violation_count = checker_report
        .violations
        .len()
        .saturating_sub(warning_count);
    let merge_order_verified = checker_report
        .violations
        .iter()
        .all(|violation| !matches!(violation.kind, ViolationKind::MergeOrderViolation));
    let sequence_monotonicity_verified = checker_report.violations.iter().all(|violation| {
        !matches!(
            violation.kind,
            ViolationKind::SequenceRegression
                | ViolationKind::SequenceGap
                | ViolationKind::DuplicateSequence
        )
    });
    let causality_integrity_verified = checker_report.violations.iter().all(|violation| {
        !matches!(
            violation.kind,
            ViolationKind::DanglingParentRef
                | ViolationKind::DanglingTriggerRef
                | ViolationKind::DanglingRootRef
        )
    });

    let started_at_ms = ordered_events
        .iter()
        .map(|event| event.occurred_at_ms)
        .min()
        .unwrap_or_default();
    let ended_at_ms = ordered_events
        .iter()
        .map(|event| event.occurred_at_ms)
        .max()
        .unwrap_or_default();
    let duration_ms = ended_at_ms.saturating_sub(started_at_ms);

    let created_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let header = serde_json::json!({
        "schema_version": FTREPLAY_FILE_SCHEMA_VERSION,
        "format_version": 1,
        "created_at": created_at,
        "created_by": format!("ft replay harvest v{}", env!("CARGO_PKG_VERSION")),
        "ft_version": env!("CARGO_PKG_VERSION"),
        "ft_commit": option_env!("GIT_COMMIT_HASH").unwrap_or("unknown"),
        "capture": {
            "session_id": ordered_events
                .iter()
                .find_map(|event| event.session_id.clone())
                .unwrap_or_else(|| "unknown".to_string()),
            "hostname": std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown-host".to_string()),
            "os": format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
            "started_at_ms": started_at_ms,
            "ended_at_ms": ended_at_ms,
            "duration_ms": duration_ms,
            "capture_mode": if artifact.metadata.tombstoned { "filtered" } else { "full" },
        },
        "content": {
            "event_count": timeline_lines.len(),
            "decision_count": decision_lines.len(),
            "entity_count": entity_lines.len(),
            "pane_count": unique_pane_count(&ordered_events),
            "stream_domains": stream_domains,
            "gap_count": gap_count,
            "clock_anomaly_count": 0,
        },
        "sensitivity": {
            "tier": artifact.metadata.sensitivity_tier.as_ftreplay_label(),
            "redaction_applied": true,
            "redaction_version": "1.0",
            "redaction_patterns_checked": 0,
            "redactions_made": count_redacted_events(&ordered_events),
        },
        "integrity": {
            "timeline_sha256": timeline_sha,
            "decisions_sha256": decisions_sha,
            "entities_sha256": entities_sha,
        }
    });

    let footer = serde_json::json!({
        "schema_version": FTREPLAY_FILE_SCHEMA_VERSION,
        "event_count_verified": timeline_lines.len(),
        "decision_count_verified": decision_lines.len(),
        "entity_count_verified": entity_lines.len(),
        "merge_order_verified": merge_order_verified,
        "sequence_monotonicity_verified": sequence_monotonicity_verified,
        "causality_integrity_verified": causality_integrity_verified,
        "integrity_check": {
            "timeline_sha256_match": true,
            "decisions_sha256_match": true,
            "entities_sha256_match": true,
        },
        "invariant_report": {
            "violations": violation_count,
            "warnings": warning_count,
            "events_checked": checker_report.events_checked,
            "panes_observed": checker_report.panes_observed,
            "domains_observed": checker_report.domains_observed,
        },
        "finalized_at": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
    });

    let mut out = String::new();
    out.push_str(FTREPLAY_HEADER_SECTION);
    out.push('\n');
    out.push_str(&serde_json::to_string(&header)?);
    out.push('\n');
    out.push_str(FTREPLAY_ENTITIES_SECTION);
    out.push('\n');
    for line in &entity_lines {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(FTREPLAY_TIMELINE_SECTION);
    out.push('\n');
    for line in &timeline_lines {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(FTREPLAY_DECISIONS_SECTION);
    out.push('\n');
    for line in &decision_lines {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(FTREPLAY_FOOTER_SECTION);
    out.push('\n');
    out.push_str(&serde_json::to_string(&footer)?);
    out.push('\n');

    Ok(out)
}

fn build_ftreplay_entities(events: &[RecorderEvent]) -> Vec<serde_json::Value> {
    let mut entities = Vec::new();
    let pane_ids: BTreeSet<u64> = events.iter().map(|event| event.pane_id).collect();
    for pane_id in pane_ids {
        entities.push(serde_json::json!({
            "entity_type": "pane",
            "pane_id": pane_id,
            "metadata": {},
        }));
    }

    let mut sessions: BTreeMap<String, BTreeSet<u64>> = BTreeMap::new();
    for event in events {
        if let Some(session_id) = &event.session_id {
            sessions
                .entry(session_id.clone())
                .or_default()
                .insert(event.pane_id);
        }
    }
    for (session_id, panes) in sessions {
        entities.push(serde_json::json!({
            "entity_type": "session",
            "session_id": session_id,
            "pane_ids": panes.into_iter().collect::<Vec<_>>(),
            "metadata": {},
        }));
    }

    entities
}

fn build_ftreplay_decision_entries(events: &[RecorderEvent]) -> Vec<serde_json::Value> {
    let mut decisions = Vec::new();

    for (position, event) in events.iter().enumerate() {
        match &event.payload {
            RecorderEventPayload::ControlMarker {
                control_marker_type: RecorderControlMarkerType::PolicyDecision,
                details,
            } => {
                let policy_details = details.clone();
                let definition_hash = format!(
                    "sha256:{}",
                    checksum_bytes(policy_details.to_string().as_bytes())
                );
                decisions.push(serde_json::json!({
                    "decision_type": "policy",
                    "timeline_position": position,
                    "event_id": event.event_id,
                    "pane_id": event.pane_id,
                    "sequence": event.sequence,
                    "occurred_at_ms": event.occurred_at_ms,
                    "policy": {
                        "action_kind": policy_details.get("action_kind").and_then(serde_json::Value::as_str).unwrap_or("unknown"),
                        "decision": policy_details.get("decision").and_then(serde_json::Value::as_str).unwrap_or("unknown"),
                        "rule_id": policy_details.get("rule_id").and_then(serde_json::Value::as_str).unwrap_or("unknown"),
                        "policy_definition_hash": definition_hash,
                        "context": policy_details,
                    }
                }));
            }
            RecorderEventPayload::ControlMarker {
                control_marker_type: RecorderControlMarkerType::ApprovalCheckpoint,
                details,
            } => {
                let workflow_details = details.clone();
                let definition_hash = format!(
                    "sha256:{}",
                    checksum_bytes(workflow_details.to_string().as_bytes())
                );
                decisions.push(serde_json::json!({
                    "decision_type": "workflow_step",
                    "timeline_position": position,
                    "event_id": event.event_id,
                    "pane_id": event.pane_id,
                    "sequence": event.sequence,
                    "occurred_at_ms": event.occurred_at_ms,
                    "workflow_step": {
                        "workflow_id": event.workflow_id,
                        "workflow_definition_hash": definition_hash,
                        "step_name": workflow_details.get("step_name").and_then(serde_json::Value::as_str).unwrap_or("approval_checkpoint"),
                        "step_index": workflow_details.get("step_index").and_then(serde_json::Value::as_u64).unwrap_or_default(),
                        "result": workflow_details.get("result").and_then(serde_json::Value::as_str).unwrap_or("continue"),
                        "result_data": workflow_details.get("result_data").cloned().unwrap_or(serde_json::Value::Null),
                        "retry_delay_ms": workflow_details.get("retry_delay_ms").cloned().unwrap_or(serde_json::Value::Null),
                        "abort_reason": workflow_details.get("abort_reason").cloned().unwrap_or(serde_json::Value::Null),
                        "trigger_event_id": event.causality.trigger_event_id,
                    }
                }));
            }
            _ => {}
        }
    }

    decisions
}

fn event_to_timeline_entry(event: &RecorderEvent, merge_position: usize) -> crate::Result<String> {
    let mut value = serde_json::to_value(event)?;
    if let serde_json::Value::Object(map) = &mut value {
        map.insert(
            "stream_kind".to_string(),
            serde_json::to_value(payload_stream_kind(&event.payload))?,
        );
        map.insert(
            "merge_position".to_string(),
            serde_json::json!(merge_position),
        );
    }
    Ok(serde_json::to_string(&value)?)
}

fn payload_stream_kind(payload: &RecorderEventPayload) -> &'static str {
    match payload {
        RecorderEventPayload::LifecycleMarker { .. } => "lifecycle",
        RecorderEventPayload::ControlMarker { .. } => "control",
        RecorderEventPayload::IngressText { .. } => "ingress",
        RecorderEventPayload::EgressOutput { .. } => "egress",
    }
}

fn serialize_jsonl_lines(values: &[serde_json::Value]) -> crate::Result<Vec<String>> {
    values
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn checksum_lines(lines: &[String]) -> String {
    let mut hasher = Sha256::new();
    for line in lines {
        hasher.update(line.as_bytes());
        hasher.update(b"\n");
    }
    to_hex(&hasher.finalize())
}

fn checksum_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    to_hex(&hasher.finalize())
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn validate_ftreplay_payload(raw: &[u8]) -> crate::Result<FtreplayValidationReport> {
    validate_ftreplay_payload_for_schema(raw, FTREPLAY_FILE_SCHEMA_VERSION)
}

fn validate_ftreplay_payload_for_schema(
    raw: &[u8],
    expected_schema_version: &str,
) -> crate::Result<FtreplayValidationReport> {
    let text = std::str::from_utf8(raw).map_err(|err| {
        crate::Error::Runtime(format!("ftreplay payload is not valid UTF-8: {err}"))
    })?;
    let sections = parse_ftreplay_sections(text)?;

    let header: serde_json::Value = serde_json::from_str(&sections.header)?;
    let schema_version = header
        .get("schema_version")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if schema_version != expected_schema_version {
        return Err(crate::Error::Runtime(format!(
            "unsupported ftreplay schema version '{schema_version}' (expected '{expected_schema_version}')"
        )));
    }

    let event_count = sections.timeline.len();
    let decision_count = sections.decisions.len();
    let entity_count = sections.entities.len();

    let expected_timeline_sha = header
        .pointer("/integrity/timeline_sha256")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            crate::Error::Runtime("ftreplay header missing integrity.timeline_sha256".to_string())
        })?;
    let expected_decisions_sha = header
        .pointer("/integrity/decisions_sha256")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            crate::Error::Runtime("ftreplay header missing integrity.decisions_sha256".to_string())
        })?;
    let expected_entities_sha = header
        .pointer("/integrity/entities_sha256")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            crate::Error::Runtime("ftreplay header missing integrity.entities_sha256".to_string())
        })?;

    let actual_timeline_sha = checksum_lines(&sections.timeline);
    let actual_decisions_sha = checksum_lines(&sections.decisions);
    let actual_entities_sha = checksum_lines(&sections.entities);
    if expected_timeline_sha != actual_timeline_sha
        || expected_decisions_sha != actual_decisions_sha
        || expected_entities_sha != actual_entities_sha
    {
        return Err(crate::Error::Runtime(
            "ftreplay integrity mismatch detected".to_string(),
        ));
    }

    let header_event_count = header
        .pointer("/content/event_count")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or_default();
    let header_decision_count = header
        .pointer("/content/decision_count")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or_default();
    let header_entity_count = header
        .pointer("/content/entity_count")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or_default();
    if header_event_count != event_count
        || header_decision_count != decision_count
        || header_entity_count != entity_count
    {
        return Err(crate::Error::Runtime(format!(
            "ftreplay content count mismatch: header=({header_event_count},{header_decision_count},{header_entity_count}) actual=({event_count},{decision_count},{entity_count})"
        )));
    }

    let footer: serde_json::Value = serde_json::from_str(&sections.footer)?;
    let footer_schema_version = footer
        .get("schema_version")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if footer_schema_version != expected_schema_version {
        return Err(crate::Error::Runtime(format!(
            "ftreplay footer schema version mismatch: footer='{footer_schema_version}', expected='{expected_schema_version}'"
        )));
    }

    let footer_event_count = footer
        .get("event_count_verified")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or_default();
    let footer_decision_count = footer
        .get("decision_count_verified")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or_default();
    let footer_entity_count = footer
        .get("entity_count_verified")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or_default();
    if footer_event_count != event_count
        || footer_decision_count != decision_count
        || footer_entity_count != entity_count
    {
        return Err(crate::Error::Runtime(format!(
            "ftreplay footer count mismatch: footer=({footer_event_count},{footer_decision_count},{footer_entity_count}) actual=({event_count},{decision_count},{entity_count})"
        )));
    }

    let timeline_events: Vec<RecorderEvent> = sections
        .timeline
        .iter()
        .enumerate()
        .map(|(line_idx, line)| {
            parse_timeline_line_to_event(Path::new("<ftreplay>"), line_idx + 1, line)
        })
        .collect::<crate::Result<Vec<_>>>()?;
    let merge_order_verified = timeline_events.windows(2).all(|window| {
        let left = RecorderMergeKey::from_event(&window[0]);
        let right = RecorderMergeKey::from_event(&window[1]);
        left <= right
    });

    let invariant_report = InvariantChecker::new().check(&timeline_events);
    let sequence_monotonicity_verified = invariant_report.violations.iter().all(|violation| {
        !matches!(
            violation.kind,
            ViolationKind::SequenceRegression
                | ViolationKind::SequenceGap
                | ViolationKind::DuplicateSequence
        )
    });
    let causality_integrity_verified = invariant_report.violations.iter().all(|violation| {
        !matches!(
            violation.kind,
            ViolationKind::DanglingParentRef
                | ViolationKind::DanglingTriggerRef
                | ViolationKind::DanglingRootRef
        )
    });

    Ok(FtreplayValidationReport {
        event_count,
        decision_count,
        entity_count,
        merge_order_verified,
        sequence_monotonicity_verified,
        causality_integrity_verified,
    })
}

fn count_redacted_events(events: &[RecorderEvent]) -> usize {
    events
        .iter()
        .filter(|event| {
            matches!(
                event.payload,
                RecorderEventPayload::IngressText {
                    redaction: RecorderRedactionLevel::Partial | RecorderRedactionLevel::Full,
                    ..
                } | RecorderEventPayload::EgressOutput {
                    redaction: RecorderRedactionLevel::Partial | RecorderRedactionLevel::Full,
                    ..
                }
            )
        })
        .count()
}

fn open_ftreplay_bufread(path: &Path) -> crate::Result<Box<dyn BufRead>> {
    let mut file = fs::File::open(path)?;
    let mut magic = [0_u8; 4];
    let read = file.read(&mut magic)?;
    file.seek(SeekFrom::Start(0))?;

    if read >= 2 && magic[..2] == [0x1F, 0x8B] {
        let decoder = GzDecoder::new(file);
        return Ok(Box::new(BufReader::new(decoder)));
    }
    if read == 4 && magic == [0x28, 0xB5, 0x2F, 0xFD] {
        let decoder = zstd::stream::read::Decoder::new(file)?;
        return Ok(Box::new(BufReader::new(decoder)));
    }

    Ok(Box::new(BufReader::new(file)))
}

fn read_ftreplay_payload(path: &Path) -> crate::Result<Vec<u8>> {
    let payload = fs::read(path)?;
    if payload.starts_with(&[0x1F, 0x8B]) {
        let mut decoder = GzDecoder::new(&payload[..]);
        let mut out = Vec::new();
        decoder.read_to_end(&mut out)?;
        return Ok(out);
    }
    if payload.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
        return zstd::stream::decode_all(std::io::Cursor::new(payload)).map_err(Into::into);
    }
    Ok(payload)
}

fn write_ftreplay_payload(
    path: &Path,
    payload: &[u8],
    compression: ArtifactCompression,
) -> crate::Result<()> {
    let encoded = match compression {
        ArtifactCompression::None => payload.to_vec(),
        ArtifactCompression::Gzip => {
            let mut encoder = GzEncoder::new(Vec::new(), GzipCompression::default());
            encoder.write_all(payload)?;
            encoder.finish()?
        }
        ArtifactCompression::Zstd => zstd::stream::encode_all(std::io::Cursor::new(payload), 0)?,
    };
    fs::write(path, encoded)?;
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

fn derive_risk_tags(
    sensitivity_tier: SensitivityTier,
    event_count: usize,
    decision_count: usize,
    tombstoned: bool,
) -> Vec<String> {
    let mut risk_tags = vec![sensitivity_tier.as_risk_tag().to_string()];
    if decision_count > 0 {
        risk_tags.push("decisionful".to_string());
    }
    if event_count > 10_000 {
        risk_tags.push("large-artifact".to_string());
    }
    if tombstoned {
        risk_tags.push("retention:tombstoned".to_string());
    }
    risk_tags
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
                RedactionMode::Drop => String::new(),
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
            sha256_hex(serialized)
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
    Ok(sha256_hex(hasher.finalize()))
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

    fn rewrite_ftreplay_schema_version(path: &Path, schema_version: &str) {
        let raw = fs::read_to_string(path).expect("read artifact");
        let sections = parse_ftreplay_sections(&raw).expect("parse sections");
        let mut header: serde_json::Value =
            serde_json::from_str(&sections.header).expect("parse header");
        let mut footer: serde_json::Value =
            serde_json::from_str(&sections.footer).expect("parse footer");
        header["schema_version"] = serde_json::Value::String(schema_version.to_string());
        footer["schema_version"] = serde_json::Value::String(schema_version.to_string());

        let mut rebuilt = String::new();
        rebuilt.push_str(FTREPLAY_HEADER_SECTION);
        rebuilt.push('\n');
        rebuilt.push_str(&serde_json::to_string(&header).expect("header serialize"));
        rebuilt.push('\n');
        rebuilt.push_str(FTREPLAY_ENTITIES_SECTION);
        rebuilt.push('\n');
        for line in &sections.entities {
            rebuilt.push_str(line);
            rebuilt.push('\n');
        }
        rebuilt.push_str(FTREPLAY_TIMELINE_SECTION);
        rebuilt.push('\n');
        for line in &sections.timeline {
            rebuilt.push_str(line);
            rebuilt.push('\n');
        }
        rebuilt.push_str(FTREPLAY_DECISIONS_SECTION);
        rebuilt.push('\n');
        for line in &sections.decisions {
            rebuilt.push_str(line);
            rebuilt.push('\n');
        }
        rebuilt.push_str(FTREPLAY_FOOTER_SECTION);
        rebuilt.push('\n');
        rebuilt.push_str(&serde_json::to_string(&footer).expect("footer serialize"));
        rebuilt.push('\n');

        fs::write(path, rebuilt).expect("rewrite artifact");
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

    #[test]
    fn write_artifact_emits_ftreplay_sections_and_validates() {
        let dir = create_temp_dir("ftreplay_write");
        let path = dir.join("artifact.ftreplay");
        let artifact = build_artifact(vec![
            make_egress_event("ev-1", 1, 0, "safe line"),
            make_decision_event("ev-2", 1, 1),
        ]);

        let write_result = write_artifact(&artifact, &path).expect("write artifact");
        assert_eq!(write_result.primary_path, path);

        let payload = fs::read_to_string(&write_result.primary_path).expect("read payload");
        assert!(payload.contains(FTREPLAY_HEADER_SECTION));
        assert!(payload.contains(FTREPLAY_ENTITIES_SECTION));
        assert!(payload.contains(FTREPLAY_TIMELINE_SECTION));
        assert!(payload.contains(FTREPLAY_DECISIONS_SECTION));
        assert!(payload.contains(FTREPLAY_FOOTER_SECTION));

        let report =
            FtreplayValidator::validate_file(&write_result.primary_path).expect("validate");
        assert_eq!(report.event_count, 2);
        assert!(report.merge_order_verified);
    }

    #[test]
    fn parse_events_from_ftreplay_roundtrips_events() {
        let dir = create_temp_dir("ftreplay_roundtrip");
        let path = dir.join("artifact.ftreplay");
        let artifact = build_artifact(vec![
            make_ingress_event("ev-1", 1, 0, "hello"),
            make_egress_event("ev-2", 1, 1, "world"),
        ]);

        write_artifact(&artifact, &path).expect("write");
        let parsed = parse_events_from_source(&path).expect("parse ftreplay");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].event_id, "ev-1");
        assert_eq!(parsed[1].event_id, "ev-2");
    }

    #[test]
    fn parse_events_from_legacy_json_ftreplay_still_works() {
        let dir = create_temp_dir("legacy_ftreplay_json");
        let path = dir.join("artifact.ftreplay");
        let events = vec![
            make_ingress_event("ev-1", 1, 0, "hello"),
            make_egress_event("ev-2", 1, 1, "world"),
        ];
        fs::write(&path, serde_json::json!({"events": events}).to_string())
            .expect("write legacy json");

        let parsed = parse_events_from_source(&path).expect("parse legacy");
        assert_eq!(parsed.len(), 2);
    }

    #[test]
    fn parse_events_from_gzip_ftreplay_roundtrips() {
        let dir = create_temp_dir("ftreplay_gzip");
        let path = dir.join("artifact.ftreplay.gz");
        let artifact = build_artifact(vec![
            make_egress_event("ev-1", 1, 0, "safe line"),
            make_decision_event("ev-2", 1, 1),
        ]);

        write_artifact(&artifact, &path).expect("write");
        let bytes = fs::read(&path).expect("read bytes");
        assert!(bytes.starts_with(&[0x1F, 0x8B]));
        let parsed = parse_events_from_source(&path).expect("parse");
        assert_eq!(parsed.len(), 2);
        let report = FtreplayValidator::validate_file(&path).expect("validate");
        assert_eq!(report.event_count, 2);
    }

    #[test]
    fn parse_events_from_zstd_ftreplay_roundtrips() {
        let dir = create_temp_dir("ftreplay_zstd");
        let path = dir.join("artifact.ftreplay.zst");
        let artifact = build_artifact(vec![
            make_ingress_event("ev-1", 1, 0, "alpha"),
            make_egress_event("ev-2", 2, 1, "beta"),
            make_decision_event("ev-3", 2, 2),
        ]);

        write_artifact(&artifact, &path).expect("write");
        let bytes = fs::read(&path).expect("read bytes");
        assert!(bytes.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]));
        let parsed = parse_events_from_source(&path).expect("parse");
        assert_eq!(parsed.len(), 3);
        let report = FtreplayValidator::validate_file(&path).expect("validate");
        assert_eq!(report.event_count, 3);
    }

    #[test]
    fn chunking_writes_manifest_and_valid_chunks() {
        let dir = create_temp_dir("ftreplay_chunk");
        let path = dir.join("artifact.ftreplay");
        let mut events = Vec::new();
        for i in 0..7 {
            events.push(make_egress_event(
                &format!("ev-{i}"),
                (i % 2) as u64 + 1,
                i as u64,
                "chunk-line",
            ));
        }
        events.push(make_decision_event("ev-decision", 1, 7));
        let artifact = build_artifact(events);

        let mut writer = FtreplayWriter::with_config(ArtifactWriterConfig {
            max_events_per_chunk: 3,
        });
        let result = writer.finalize(&artifact, &path).expect("chunked write");
        assert!(result.manifest_path.is_some());
        assert_eq!(result.chunk_paths.len(), 3);

        let manifest_path = result.manifest_path.expect("manifest");
        let manifest: ArtifactChunkManifest =
            serde_json::from_str(&fs::read_to_string(&manifest_path).expect("read manifest"))
                .expect("parse manifest");
        assert_eq!(
            manifest.schema_version,
            FTREPLAY_CHUNK_MANIFEST_SCHEMA_VERSION
        );
        assert_eq!(manifest.chunk_count, 3);
        let total_events: usize = manifest.chunks.iter().map(|chunk| chunk.event_count).sum();
        assert_eq!(total_events, artifact.events.len());

        for chunk_path in &result.chunk_paths {
            let report = FtreplayValidator::validate_file(chunk_path).expect("validate chunk");
            assert!(report.event_count > 0);
        }
    }

    #[test]
    fn validator_detects_timeline_tampering() {
        let dir = create_temp_dir("ftreplay_tamper");
        let path = dir.join("artifact.ftreplay");
        let artifact = build_artifact(vec![
            make_egress_event("ev-1", 1, 0, "safe line"),
            make_decision_event("ev-2", 1, 1),
        ]);

        write_artifact(&artifact, &path).expect("write");
        let original = fs::read_to_string(&path).expect("read");
        let tampered = original.replacen("safe line", "unsafe line", 1);
        fs::write(&path, tampered).expect("tamper write");

        let err = FtreplayValidator::validate_file(&path).expect_err("validation should fail");
        assert!(format!("{err}").contains("integrity mismatch"));
    }

    #[test]
    fn streaming_writer_finalize_uses_staged_events() {
        let dir = create_temp_dir("ftreplay_streaming");
        let path = dir.join("artifact.ftreplay");
        let artifact = build_artifact(vec![make_ingress_event("template-ev", 1, 0, "template")]);

        let mut writer = FtreplayWriter::default();
        writer.add_event(make_ingress_event("staged-1", 1, 0, "hello"));
        writer.add_event(make_egress_event("staged-2", 1, 1, "world"));
        writer.finalize(&artifact, &path).expect("finalize");

        let parsed = parse_events_from_source(&path).expect("parse staged events");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].event_id, "staged-1");
        assert_eq!(parsed[1].event_id, "staged-2");
    }

    #[test]
    fn discover_source_files_includes_compressed_ftreplay() {
        let dir = create_temp_dir("discover_ftreplay_compressed");
        fs::write(dir.join("a.ftreplay.gz"), "").expect("create gzip placeholder");
        fs::write(dir.join("b.ftreplay.zst"), "").expect("create zstd placeholder");
        fs::write(dir.join("c.jsonl"), "").expect("create jsonl");

        let files = discover_source_files(&dir).expect("discover");
        assert_eq!(files.len(), 3);
    }

    #[test]
    fn compatibility_checker_reports_exact() {
        let checker =
            CompatibilityChecker::new(FTREPLAY_FILE_SCHEMA_VERSION, SchemaMigrator::default());
        assert_eq!(
            checker.check(FTREPLAY_FILE_SCHEMA_VERSION),
            ArtifactCompatibility::Exact
        );
    }

    #[test]
    fn compatibility_checker_reports_migration_chain() {
        let checker =
            CompatibilityChecker::new(FTREPLAY_FILE_SCHEMA_VERSION, SchemaMigrator::default());
        let compatibility = checker.check("ftreplay.v0");
        match compatibility {
            ArtifactCompatibility::MigrationAvailable { chain } => {
                assert_eq!(chain, vec!["ftreplay.v0->ftreplay.v1".to_string()]);
            }
            _ => panic!("expected migration chain"),
        }
    }

    #[test]
    fn compatibility_checker_reports_incompatible_with_guidance() {
        let checker =
            CompatibilityChecker::new(FTREPLAY_FILE_SCHEMA_VERSION, SchemaMigrator::default());
        let compatibility = checker.check("ftreplay.v99");
        match compatibility {
            ArtifactCompatibility::Incompatible { reason } => {
                assert!(reason.contains("no migration chain exists"));
                assert!(reason.contains("Remediation"));
            }
            _ => panic!("expected incompatible"),
        }
    }

    #[test]
    fn schema_migrator_chain_v1_to_v3_composes() {
        let migrator = SchemaMigrator::default();
        let event = serde_json::json!({
            "schema_version": RECORDER_EVENT_SCHEMA_VERSION_V1,
            "event_id": "ev-1",
            "pane_id": 1,
            "session_id": "sess",
            "workflow_id": null,
            "correlation_id": null,
            "source": "robot_mode",
            "occurred_at_ms": 1,
            "recorded_at_ms": 1,
            "sequence": 1,
            "causality": {"parent_event_id": null, "trigger_event_id": null, "root_event_id": null},
            "event_type": "ingress_text",
            "text": "hello",
            "encoding": "utf8",
            "redaction": "none",
            "ingress_kind": "send_text"
        });

        let (migrated, report) = migrator
            .migrate_timeline_values(FTREPLAY_FILE_SCHEMA_VERSION, "ftreplay.v3", &[event])
            .expect("migrate");
        assert_eq!(report.target_version, "ftreplay.v3");
        assert_eq!(report.events_migrated, 2);
        assert_eq!(migrated.len(), 1);
        assert!(migrated[0].get("capture_origin").is_some());
    }

    #[test]
    fn schema_migrator_idempotent_when_versions_match() {
        let migrator = SchemaMigrator::default();
        let event = serde_json::json!({
            "schema_version": RECORDER_EVENT_SCHEMA_VERSION_V1,
            "event_id": "ev-1"
        });
        let (migrated, report) = migrator
            .migrate_timeline_values(
                FTREPLAY_FILE_SCHEMA_VERSION,
                FTREPLAY_FILE_SCHEMA_VERSION,
                &[event.clone()],
            )
            .expect("no-op migrate");
        assert_eq!(migrated[0], event);
        assert_eq!(report.events_migrated, 0);
        assert_eq!(report.fields_added, 0);
        assert_eq!(report.fields_removed, 0);
    }

    #[test]
    fn artifact_reader_open_reads_v1_artifact() {
        let dir = create_temp_dir("artifact_reader_open_v1");
        let path = dir.join("artifact.ftreplay");
        let artifact = build_artifact(vec![
            make_ingress_event("ev-1", 1, 0, "hello"),
            make_egress_event("ev-2", 1, 1, "world"),
            make_decision_event("ev-3", 1, 2),
        ]);
        write_artifact(&artifact, &path).expect("write");

        let loaded = ArtifactReader::default().open(&path).expect("open");
        assert_eq!(loaded.schema_version, FTREPLAY_FILE_SCHEMA_VERSION);
        assert_eq!(loaded.events.len(), 3);
        assert_eq!(loaded.entities.len(), 2);
        assert!(loaded.migration_report.is_none());
        assert_eq!(loaded.compatibility, ArtifactCompatibility::Exact);
    }

    #[test]
    fn artifact_reader_open_applies_v0_to_v1_migration() {
        let dir = create_temp_dir("artifact_reader_open_v0");
        let path = dir.join("artifact.ftreplay");
        let artifact = build_artifact(vec![
            make_ingress_event("ev-1", 1, 0, "hello"),
            make_egress_event("ev-2", 1, 1, "world"),
        ]);
        write_artifact(&artifact, &path).expect("write");
        rewrite_ftreplay_schema_version(&path, "ftreplay.v0");

        let loaded = ArtifactReader::default()
            .open(&path)
            .expect("open migrated");
        assert_eq!(loaded.schema_version, FTREPLAY_FILE_SCHEMA_VERSION);
        assert_eq!(loaded.events.len(), 2);
        assert!(loaded.migration_report.is_some());
        assert!(matches!(
            loaded.compatibility,
            ArtifactCompatibility::MigrationAvailable { .. }
        ));
    }

    #[test]
    fn artifact_reader_open_rejects_unknown_future_schema() {
        let dir = create_temp_dir("artifact_reader_open_future");
        let path = dir.join("artifact.ftreplay");
        let artifact = build_artifact(vec![make_ingress_event("ev-1", 1, 0, "hello")]);
        write_artifact(&artifact, &path).expect("write");
        rewrite_ftreplay_schema_version(&path, "ftreplay.v99");

        let err = ArtifactReader::default()
            .open(&path)
            .expect_err("should reject unknown schema");
        let message = format!("{err}");
        assert!(message.contains("incompatible"));
        assert!(message.contains("Remediation"));
    }

    #[test]
    fn artifact_reader_stream_events_reads_large_trace() {
        let dir = create_temp_dir("artifact_reader_stream");
        let path = dir.join("artifact.ftreplay");
        let mut events = Vec::new();
        for i in 0..10_000 {
            events.push(make_egress_event(
                &format!("ev-{i}"),
                (i % 3) as u64 + 1,
                i as u64,
                "bulk",
            ));
        }
        let artifact = build_artifact(events);
        write_artifact(&artifact, &path).expect("write");

        let reader = ArtifactReader::default();
        let mut count = 0usize;
        for event in reader.stream_events(&path).expect("stream open") {
            let _ = event.expect("event parse");
            count += 1;
        }
        assert_eq!(count, 10_000);
    }

    #[test]
    fn artifact_reader_stream_events_rejects_migration_required_schema() {
        let dir = create_temp_dir("artifact_reader_stream_migration_required");
        let path = dir.join("artifact.ftreplay");
        let artifact = build_artifact(vec![make_egress_event("ev-1", 1, 0, "x")]);
        write_artifact(&artifact, &path).expect("write");
        rewrite_ftreplay_schema_version(&path, "ftreplay.v0");

        let err = match ArtifactReader::default().stream_events(&path) {
            Ok(_) => panic!("stream should require exact schema"),
            Err(err) => err,
        };
        assert!(format!("{err}").contains("streaming mode requires exact schema compatibility"));
    }

    #[test]
    fn artifact_reader_reports_integrity_mismatch() {
        let dir = create_temp_dir("artifact_reader_integrity_mismatch");
        let path = dir.join("artifact.ftreplay");
        let artifact = build_artifact(vec![
            make_egress_event("ev-1", 1, 0, "safe line"),
            make_decision_event("ev-2", 1, 1),
        ]);
        write_artifact(&artifact, &path).expect("write");

        let original = fs::read_to_string(&path).expect("read");
        let tampered = original.replacen("safe line", "unsafe line", 1);
        fs::write(&path, tampered).expect("write tampered");

        let err = ArtifactReader::default()
            .open(&path)
            .expect_err("open should fail on integrity mismatch");
        assert!(format!("{err}").contains("integrity mismatch"));
    }
}
