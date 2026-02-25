//! GitOps Cross-Swarm Federation & Webhook Dispatcher for ARS.
//!
//! Syncs Active reflexes to a central Git repository and dispatches
//! webhook notifications (Slack/Discord/generic) on critical state changes.
//!
//! # Federation Model
//!
//! ```text
//! Dev Swarm  ──┐
//!              ├──→ Central Git Repo ──→ All Swarms Import
//! CI/CD Swarm ─┘        │
//!                        └──→ Webhook Dispatcher
//!                               ├─→ Slack
//!                               ├─→ Discord
//!                               └─→ Generic HTTP
//! ```
//!
//! # Export Format
//!
//! Each reflex exports as a YAML manifest + evidence JSON sidecar,
//! committed to a well-known branch structure:
//!
//! ```text
//! reflexes/
//!   <cluster_id>/
//!     <reflex_id>.yaml     # ReflexExport
//!     <reflex_id>.evidence  # Evidence JSON
//! ```

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::ars_blast_radius::MaturityTier;
use crate::ars_evolve::VersionStatus;
use crate::ars_fst::ReflexId;
use crate::ars_serialize::{EvidenceSummary, ReflexRecord, ReflexStore};

// =============================================================================
// Export types
// =============================================================================

/// Exportable reflex definition for GitOps federation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReflexExport {
    /// Export schema version.
    pub schema_version: u32,
    /// Source swarm identifier.
    pub swarm_id: String,
    /// Reflex identity.
    pub reflex_id: ReflexId,
    /// Cluster grouping.
    pub cluster_id: String,
    /// Version number.
    pub version: u32,
    /// Trigger pattern (hex-encoded for YAML readability).
    pub trigger_key_hex: String,
    /// Action commands.
    pub commands: Vec<String>,
    /// Current status.
    pub status: VersionStatus,
    /// Maturity tier.
    pub tier: MaturityTier,
    /// Execution stats.
    pub successes: u64,
    pub failures: u64,
    /// E-value at export time.
    pub e_value: f64,
    /// Evidence summary.
    pub evidence: EvidenceSummary,
    /// Export timestamp (ms).
    pub exported_at_ms: u64,
}

impl ReflexExport {
    /// Create an export from a ReflexRecord.
    pub fn from_record(record: &ReflexRecord, swarm_id: &str, now_ms: u64) -> Self {
        Self {
            schema_version: 1,
            swarm_id: swarm_id.to_string(),
            reflex_id: record.reflex_id,
            cluster_id: record.cluster_id.clone(),
            version: record.version,
            trigger_key_hex: hex_encode(&record.trigger_key),
            commands: record.commands.clone(),
            status: record.status,
            tier: record.tier,
            successes: record.successes,
            failures: record.failures,
            e_value: record.drift_state.e_value,
            evidence: record.evidence_summary.clone(),
            exported_at_ms: now_ms,
        }
    }

    /// Path in the federation repo.
    pub fn repo_path(&self) -> String {
        format!("reflexes/{}/{}.yaml", self.cluster_id, self.reflex_id)
    }

    /// Evidence sidecar path.
    pub fn evidence_path(&self) -> String {
        format!("reflexes/{}/{}.evidence", self.cluster_id, self.reflex_id)
    }
}

/// Hex-encode bytes (lowercase).
fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Hex-decode a string to bytes.
fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

// =============================================================================
// Federation engine
// =============================================================================

/// Configuration for cross-swarm federation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FederationConfig {
    /// This swarm's identifier.
    pub swarm_id: String,
    /// Only export reflexes with these statuses.
    pub export_statuses: Vec<VersionStatus>,
    /// Only export reflexes at or above this tier.
    pub min_export_tier: MaturityTier,
    /// Minimum e-value for export eligibility.
    pub min_export_e_value: f64,
    /// Whether to include evidence in exports.
    pub include_evidence: bool,
    /// Webhook endpoints.
    pub webhooks: Vec<WebhookConfig>,
}

impl Default for FederationConfig {
    fn default() -> Self {
        Self {
            swarm_id: "default".to_string(),
            export_statuses: vec![VersionStatus::Active],
            min_export_tier: MaturityTier::Graduated,
            min_export_e_value: 1.0,
            include_evidence: true,
            webhooks: Vec::new(),
        }
    }
}

/// Webhook endpoint configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookConfig {
    /// Endpoint URL.
    pub url: String,
    /// Webhook type.
    pub kind: WebhookKind,
    /// Only fire on these event types.
    pub event_filter: Vec<FederationEventKind>,
    /// Whether this webhook is enabled.
    pub enabled: bool,
}

/// Supported webhook platforms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WebhookKind {
    Slack,
    Discord,
    Generic,
}

/// Types of federation events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FederationEventKind {
    /// New reflex exported.
    ReflexExported,
    /// Reflex evolved (v1 → v2).
    ReflexEvolved,
    /// Reflex pruned/blacklisted.
    ReflexPruned,
    /// Drift detected.
    DriftDetected,
    /// Reflex promoted (tier change).
    TierPromotion,
    /// Import from another swarm.
    ReflexImported,
}

/// Federation event for webhook dispatch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FederationEvent {
    /// Event kind.
    pub kind: FederationEventKind,
    /// Source swarm.
    pub swarm_id: String,
    /// Affected reflex.
    pub reflex_id: ReflexId,
    /// Cluster.
    pub cluster_id: String,
    /// Human-readable summary.
    pub summary: String,
    /// Timestamp (ms).
    pub timestamp_ms: u64,
    /// Additional metadata.
    pub metadata: HashMap<String, String>,
}

impl FederationEvent {
    /// Format as Slack message payload.
    pub fn to_slack_payload(&self) -> String {
        serde_json::json!({
            "text": self.summary,
            "blocks": [{
                "type": "section",
                "text": {
                    "type": "mrkdwn",
                    "text": format!(
                        "*{}* | Reflex `{}` (cluster: `{}`)\n{}",
                        self.kind_label(),
                        self.reflex_id,
                        self.cluster_id,
                        self.summary
                    )
                }
            }]
        })
        .to_string()
    }

    /// Format as Discord message payload.
    pub fn to_discord_payload(&self) -> String {
        serde_json::json!({
            "content": self.summary,
            "embeds": [{
                "title": self.kind_label(),
                "description": format!(
                    "Reflex `{}` (cluster: `{}`): {}",
                    self.reflex_id, self.cluster_id, self.summary
                ),
                "color": self.kind_color(),
            }]
        })
        .to_string()
    }

    /// Format as generic webhook payload.
    pub fn to_generic_payload(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Format payload for a specific webhook kind.
    pub fn format_for(&self, kind: &WebhookKind) -> String {
        match kind {
            WebhookKind::Slack => self.to_slack_payload(),
            WebhookKind::Discord => self.to_discord_payload(),
            WebhookKind::Generic => self.to_generic_payload(),
        }
    }

    fn kind_label(&self) -> &'static str {
        match self.kind {
            FederationEventKind::ReflexExported => "Reflex Exported",
            FederationEventKind::ReflexEvolved => "Reflex Evolved",
            FederationEventKind::ReflexPruned => "Reflex Pruned",
            FederationEventKind::DriftDetected => "Drift Detected",
            FederationEventKind::TierPromotion => "Tier Promotion",
            FederationEventKind::ReflexImported => "Reflex Imported",
        }
    }

    fn kind_color(&self) -> u32 {
        match self.kind {
            FederationEventKind::ReflexExported => 0x00CC00, // green
            FederationEventKind::ReflexEvolved => 0x0066FF,  // blue
            FederationEventKind::ReflexPruned => 0xFF3300,   // red
            FederationEventKind::DriftDetected => 0xFFAA00,  // orange
            FederationEventKind::TierPromotion => 0x9900FF,  // purple
            FederationEventKind::ReflexImported => 0x00CCCC, // teal
        }
    }
}

/// Webhook delivery record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookDelivery {
    /// Target URL.
    pub url: String,
    /// Webhook kind.
    pub kind: WebhookKind,
    /// Event that triggered delivery.
    pub event_kind: FederationEventKind,
    /// Formatted payload body.
    pub payload: String,
    /// Delivery status.
    pub status: DeliveryStatus,
    /// Timestamp (ms).
    pub timestamp_ms: u64,
}

/// Status of a webhook delivery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryStatus {
    /// Queued for delivery.
    Pending,
    /// Successfully delivered.
    Delivered,
    /// Delivery failed.
    Failed { reason: String },
}

// =============================================================================
// Federation engine
// =============================================================================

/// Engine that orchestrates export, import, and webhook dispatch.
pub struct FederationEngine {
    config: FederationConfig,
    /// Pending webhook deliveries.
    delivery_queue: Vec<WebhookDelivery>,
    /// Export history.
    export_log: Vec<ExportLogEntry>,
    /// Import history.
    import_log: Vec<ImportLogEntry>,
    /// Stats.
    total_exports: u64,
    total_imports: u64,
    total_webhooks: u64,
}

/// Log entry for exports.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportLogEntry {
    pub reflex_id: ReflexId,
    pub cluster_id: String,
    pub version: u32,
    pub repo_path: String,
    pub timestamp_ms: u64,
}

/// Log entry for imports.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportLogEntry {
    pub reflex_id: ReflexId,
    pub source_swarm: String,
    pub cluster_id: String,
    pub version: u32,
    pub timestamp_ms: u64,
}

impl FederationEngine {
    /// Create a new federation engine.
    pub fn new(config: FederationConfig) -> Self {
        Self {
            config,
            delivery_queue: Vec::new(),
            export_log: Vec::new(),
            import_log: Vec::new(),
            total_exports: 0,
            total_imports: 0,
            total_webhooks: 0,
        }
    }

    /// Create with default config.
    pub fn with_defaults() -> Self {
        Self::new(FederationConfig::default())
    }

    /// Export eligible reflexes from a store.
    pub fn export(&mut self, store: &ReflexStore, now_ms: u64) -> Vec<ReflexExport> {
        let mut exports = Vec::new();
        let tier_rank = tier_to_rank(self.config.min_export_tier);

        for entry in store.manifest().entries.values() {
            // Check status filter.
            if !self.config.export_statuses.contains(&entry.status) {
                continue;
            }

            // Check tier.
            if tier_to_rank(entry.tier) < tier_rank {
                continue;
            }

            // Check e-value.
            if entry.e_value < self.config.min_export_e_value {
                continue;
            }

            // Get full record for export.
            if let Some(record) = store.get(entry.reflex_id) {
                let export = ReflexExport::from_record(record, &self.config.swarm_id, now_ms);
                self.export_log.push(ExportLogEntry {
                    reflex_id: export.reflex_id,
                    cluster_id: export.cluster_id.clone(),
                    version: export.version,
                    repo_path: export.repo_path(),
                    timestamp_ms: now_ms,
                });
                self.total_exports += 1;

                // Emit webhook event.
                self.emit_event(FederationEvent {
                    kind: FederationEventKind::ReflexExported,
                    swarm_id: self.config.swarm_id.clone(),
                    reflex_id: export.reflex_id,
                    cluster_id: export.cluster_id.clone(),
                    summary: format!(
                        "Exported reflex {} v{} ({})",
                        export.reflex_id,
                        export.version,
                        export.tier.name()
                    ),
                    timestamp_ms: now_ms,
                    metadata: HashMap::new(),
                });

                exports.push(export);
            }
        }

        exports
    }

    /// Import a reflex export from another swarm (validation only, no I/O).
    pub fn import(&mut self, export: &ReflexExport, now_ms: u64) -> ImportResult {
        // Reject self-imports.
        if export.swarm_id == self.config.swarm_id {
            return ImportResult::SelfImport;
        }

        // Validate schema.
        if export.schema_version != 1 {
            return ImportResult::UnsupportedSchema {
                version: export.schema_version,
            };
        }

        // Validate trigger key hex.
        if hex_decode(&export.trigger_key_hex).is_none() {
            return ImportResult::InvalidTriggerKey;
        }

        // Validate commands non-empty.
        if export.commands.is_empty() {
            return ImportResult::EmptyCommands;
        }

        self.import_log.push(ImportLogEntry {
            reflex_id: export.reflex_id,
            source_swarm: export.swarm_id.clone(),
            cluster_id: export.cluster_id.clone(),
            version: export.version,
            timestamp_ms: now_ms,
        });
        self.total_imports += 1;

        // Emit import event.
        self.emit_event(FederationEvent {
            kind: FederationEventKind::ReflexImported,
            swarm_id: export.swarm_id.clone(),
            reflex_id: export.reflex_id,
            cluster_id: export.cluster_id.clone(),
            summary: format!(
                "Imported reflex {} v{} from swarm '{}'",
                export.reflex_id, export.version, export.swarm_id
            ),
            timestamp_ms: now_ms,
            metadata: HashMap::new(),
        });

        ImportResult::Imported {
            reflex_id: export.reflex_id,
            source_swarm: export.swarm_id.clone(),
        }
    }

    /// Emit a federation event and queue webhook deliveries.
    pub fn emit_event(&mut self, event: FederationEvent) {
        for webhook in &self.config.webhooks {
            if !webhook.enabled {
                continue;
            }
            if !webhook.event_filter.contains(&event.kind) {
                continue;
            }
            let payload = event.format_for(&webhook.kind);
            self.delivery_queue.push(WebhookDelivery {
                url: webhook.url.clone(),
                kind: webhook.kind.clone(),
                event_kind: event.kind.clone(),
                payload,
                status: DeliveryStatus::Pending,
                timestamp_ms: event.timestamp_ms,
            });
            self.total_webhooks += 1;
        }
    }

    /// Drain the delivery queue.
    pub fn drain_deliveries(&mut self) -> Vec<WebhookDelivery> {
        std::mem::take(&mut self.delivery_queue)
    }

    /// Get pending delivery count.
    pub fn pending_deliveries(&self) -> usize {
        self.delivery_queue.len()
    }

    /// Get export log.
    pub fn export_log(&self) -> &[ExportLogEntry] {
        &self.export_log
    }

    /// Get import log.
    pub fn import_log(&self) -> &[ImportLogEntry] {
        &self.import_log
    }

    /// Get statistics.
    pub fn stats(&self) -> FederationStats {
        FederationStats {
            total_exports: self.total_exports,
            total_imports: self.total_imports,
            total_webhooks: self.total_webhooks,
            pending_deliveries: self.delivery_queue.len() as u64,
        }
    }

    /// Get config.
    pub fn config(&self) -> &FederationConfig {
        &self.config
    }
}

/// Result of an import attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImportResult {
    /// Successfully validated for import.
    Imported {
        reflex_id: ReflexId,
        source_swarm: String,
    },
    /// Rejected: same swarm.
    SelfImport,
    /// Rejected: unsupported schema version.
    UnsupportedSchema { version: u32 },
    /// Rejected: invalid trigger key hex.
    InvalidTriggerKey,
    /// Rejected: no commands.
    EmptyCommands,
}

/// Federation statistics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FederationStats {
    pub total_exports: u64,
    pub total_imports: u64,
    pub total_webhooks: u64,
    pub pending_deliveries: u64,
}

/// Convert tier to numeric rank for comparison.
fn tier_to_rank(tier: MaturityTier) -> u8 {
    match tier {
        MaturityTier::Incubating => 0,
        MaturityTier::Graduated => 1,
        MaturityTier::Veteran => 2,
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ars_drift::EValueConfig;
    use crate::ars_evidence::EvidenceVerdict;
    use crate::ars_serialize::{DriftSnapshot, ReflexRecord};

    fn make_record(id: ReflexId, cluster: &str, tier: MaturityTier) -> ReflexRecord {
        ReflexRecord {
            reflex_id: id,
            cluster_id: cluster.to_string(),
            version: 1,
            trigger_key: vec![0xDE, 0xAD],
            commands: vec!["restart".to_string()],
            status: VersionStatus::Active,
            tier,
            successes: 50,
            failures: 2,
            consecutive_failures: 0,
            drift_state: DriftSnapshot {
                e_value: 5.0,
                null_rate: 0.9,
                total_observations: 100,
                post_cal_successes: 90,
                post_cal_observations: 100,
                drift_count: 0,
                calibrated: true,
                config: EValueConfig::default(),
            },
            evidence_summary: EvidenceSummary {
                entry_count: 3,
                is_complete: true,
                overall_verdict: EvidenceVerdict::Support,
                root_hash: "abc123".to_string(),
                categories: vec!["ChangeDetection".to_string()],
            },
            parent_reflex_id: None,
            parent_version: None,
            created_at_ms: 1000,
            updated_at_ms: 2000,
        }
    }

    fn make_store_with_records() -> ReflexStore {
        let mut store = ReflexStore::new();
        store.upsert(make_record(1, "network", MaturityTier::Graduated));
        store.upsert(make_record(2, "network", MaturityTier::Veteran));
        store.upsert(make_record(3, "disk", MaturityTier::Incubating));
        store
    }

    fn make_webhook_config() -> WebhookConfig {
        WebhookConfig {
            url: "https://hooks.example.com/ars".to_string(),
            kind: WebhookKind::Slack,
            event_filter: vec![
                FederationEventKind::ReflexExported,
                FederationEventKind::ReflexImported,
            ],
            enabled: true,
        }
    }

    // ---- Hex encoding ----

    #[test]
    fn hex_encode_bytes() {
        assert_eq!(hex_encode(&[0xDE, 0xAD, 0xBE, 0xEF]), "deadbeef");
    }

    #[test]
    fn hex_encode_empty() {
        assert_eq!(hex_encode(&[]), "");
    }

    #[test]
    fn hex_decode_valid() {
        assert_eq!(hex_decode("deadbeef"), Some(vec![0xDE, 0xAD, 0xBE, 0xEF]));
    }

    #[test]
    fn hex_decode_empty() {
        assert_eq!(hex_decode(""), Some(vec![]));
    }

    #[test]
    fn hex_decode_invalid_odd_length() {
        assert_eq!(hex_decode("abc"), None);
    }

    #[test]
    fn hex_decode_invalid_chars() {
        assert_eq!(hex_decode("zzzz"), None);
    }

    // ---- ReflexExport ----

    #[test]
    fn export_from_record() {
        let record = make_record(42, "net", MaturityTier::Graduated);
        let export = ReflexExport::from_record(&record, "dev-swarm", 5000);
        assert_eq!(export.reflex_id, 42);
        assert_eq!(export.swarm_id, "dev-swarm");
        assert_eq!(export.trigger_key_hex, "dead");
        assert_eq!(export.commands, vec!["restart"]);
    }

    #[test]
    fn export_repo_path() {
        let record = make_record(42, "network", MaturityTier::Graduated);
        let export = ReflexExport::from_record(&record, "s1", 1000);
        assert_eq!(export.repo_path(), "reflexes/network/42.yaml");
        assert_eq!(export.evidence_path(), "reflexes/network/42.evidence");
    }

    #[test]
    fn export_serde_roundtrip() {
        let record = make_record(7, "disk", MaturityTier::Veteran);
        let export = ReflexExport::from_record(&record, "s1", 3000);
        let json = serde_json::to_string(&export).unwrap();
        let decoded: ReflexExport = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, export);
    }

    // ---- FederationEngine export ----

    #[test]
    fn export_filters_by_tier() {
        let store = make_store_with_records();
        let config = FederationConfig {
            min_export_tier: MaturityTier::Graduated,
            ..Default::default()
        };
        let mut engine = FederationEngine::new(config);
        let exports = engine.export(&store, 5000);
        // Should export Graduated (id=1) and Veteran (id=2), but not Incubating (id=3).
        assert_eq!(exports.len(), 2);
        let ids: Vec<_> = exports.iter().map(|e| e.reflex_id).collect();
        assert!(ids.contains(&1));
        assert!(ids.contains(&2));
        assert!(!ids.contains(&3));
    }

    #[test]
    fn export_filters_by_status() {
        let mut store = ReflexStore::new();
        let mut r1 = make_record(1, "net", MaturityTier::Graduated);
        r1.status = VersionStatus::Active;
        let mut r2 = make_record(2, "net", MaturityTier::Graduated);
        r2.status = VersionStatus::Deprecated;
        store.upsert(r1);
        store.upsert(r2);

        let config = FederationConfig {
            min_export_tier: MaturityTier::Graduated,
            ..Default::default()
        };
        let mut engine = FederationEngine::new(config);
        let exports = engine.export(&store, 5000);
        assert_eq!(exports.len(), 1);
        assert_eq!(exports[0].reflex_id, 1);
    }

    #[test]
    fn export_filters_by_e_value() {
        let mut store = ReflexStore::new();
        let mut r = make_record(1, "net", MaturityTier::Graduated);
        r.drift_state.e_value = 0.5; // Below default threshold of 1.0.
        store.upsert(r);

        let mut engine = FederationEngine::with_defaults();
        let exports = engine.export(&store, 5000);
        assert!(exports.is_empty());
    }

    #[test]
    fn export_increments_stats() {
        let store = make_store_with_records();
        let mut engine = FederationEngine::with_defaults();
        engine.export(&store, 5000);
        let stats = engine.stats();
        assert_eq!(stats.total_exports, 2);
    }

    #[test]
    fn export_creates_log_entries() {
        let store = make_store_with_records();
        let mut engine = FederationEngine::with_defaults();
        engine.export(&store, 5000);
        assert_eq!(engine.export_log().len(), 2);
    }

    // ---- Import ----

    #[test]
    fn import_valid_export() {
        let record = make_record(1, "net", MaturityTier::Graduated);
        let export = ReflexExport::from_record(&record, "other-swarm", 3000);
        let mut engine = FederationEngine::with_defaults();
        let result = engine.import(&export, 5000);
        let is_imported = matches!(result, ImportResult::Imported { .. });
        assert!(is_imported);
    }

    #[test]
    fn import_rejects_self() {
        let record = make_record(1, "net", MaturityTier::Graduated);
        let export = ReflexExport::from_record(&record, "default", 3000);
        let mut engine = FederationEngine::with_defaults();
        let result = engine.import(&export, 5000);
        assert_eq!(result, ImportResult::SelfImport);
    }

    #[test]
    fn import_rejects_bad_schema() {
        let record = make_record(1, "net", MaturityTier::Graduated);
        let mut export = ReflexExport::from_record(&record, "other", 3000);
        export.schema_version = 99;
        let mut engine = FederationEngine::with_defaults();
        let result = engine.import(&export, 5000);
        let is_unsupported = matches!(result, ImportResult::UnsupportedSchema { version: 99 });
        assert!(is_unsupported);
    }

    #[test]
    fn import_rejects_invalid_hex() {
        let record = make_record(1, "net", MaturityTier::Graduated);
        let mut export = ReflexExport::from_record(&record, "other", 3000);
        export.trigger_key_hex = "xyz".to_string(); // Odd length, invalid hex.
        let mut engine = FederationEngine::with_defaults();
        let result = engine.import(&export, 5000);
        assert_eq!(result, ImportResult::InvalidTriggerKey);
    }

    #[test]
    fn import_rejects_empty_commands() {
        let record = make_record(1, "net", MaturityTier::Graduated);
        let mut export = ReflexExport::from_record(&record, "other", 3000);
        export.commands.clear();
        let mut engine = FederationEngine::with_defaults();
        let result = engine.import(&export, 5000);
        assert_eq!(result, ImportResult::EmptyCommands);
    }

    #[test]
    fn import_increments_stats() {
        let record = make_record(1, "net", MaturityTier::Graduated);
        let export = ReflexExport::from_record(&record, "other", 3000);
        let mut engine = FederationEngine::with_defaults();
        engine.import(&export, 5000);
        assert_eq!(engine.stats().total_imports, 1);
        assert_eq!(engine.import_log().len(), 1);
    }

    // ---- Webhooks ----

    #[test]
    fn webhook_queued_on_export() {
        let store = make_store_with_records();
        let config = FederationConfig {
            webhooks: vec![make_webhook_config()],
            ..Default::default()
        };
        let mut engine = FederationEngine::new(config);
        engine.export(&store, 5000);
        assert!(engine.pending_deliveries() > 0);
    }

    #[test]
    fn webhook_filtered_by_event_kind() {
        let config = FederationConfig {
            webhooks: vec![WebhookConfig {
                url: "https://example.com".to_string(),
                kind: WebhookKind::Generic,
                event_filter: vec![FederationEventKind::DriftDetected], // Only drift.
                enabled: true,
            }],
            ..Default::default()
        };
        let mut engine = FederationEngine::new(config);
        let store = make_store_with_records();
        engine.export(&store, 5000); // Emits ReflexExported, not DriftDetected.
        assert_eq!(engine.pending_deliveries(), 0);
    }

    #[test]
    fn webhook_disabled_not_queued() {
        let config = FederationConfig {
            webhooks: vec![WebhookConfig {
                url: "https://example.com".to_string(),
                kind: WebhookKind::Slack,
                event_filter: vec![FederationEventKind::ReflexExported],
                enabled: false,
            }],
            ..Default::default()
        };
        let mut engine = FederationEngine::new(config);
        let store = make_store_with_records();
        engine.export(&store, 5000);
        assert_eq!(engine.pending_deliveries(), 0);
    }

    #[test]
    fn drain_deliveries_empties_queue() {
        let store = make_store_with_records();
        let config = FederationConfig {
            webhooks: vec![make_webhook_config()],
            ..Default::default()
        };
        let mut engine = FederationEngine::new(config);
        engine.export(&store, 5000);
        let deliveries = engine.drain_deliveries();
        assert!(!deliveries.is_empty());
        assert_eq!(engine.pending_deliveries(), 0);
    }

    // ---- Event formatting ----

    #[test]
    fn event_slack_payload_contains_summary() {
        let event = FederationEvent {
            kind: FederationEventKind::ReflexExported,
            swarm_id: "s1".to_string(),
            reflex_id: 42,
            cluster_id: "net".to_string(),
            summary: "Test export".to_string(),
            timestamp_ms: 1000,
            metadata: HashMap::new(),
        };
        let payload = event.to_slack_payload();
        assert!(payload.contains("Test export"));
        assert!(payload.contains("42"));
    }

    #[test]
    fn event_discord_payload_contains_summary() {
        let event = FederationEvent {
            kind: FederationEventKind::DriftDetected,
            swarm_id: "s1".to_string(),
            reflex_id: 7,
            cluster_id: "disk".to_string(),
            summary: "Drift detected".to_string(),
            timestamp_ms: 1000,
            metadata: HashMap::new(),
        };
        let payload = event.to_discord_payload();
        assert!(payload.contains("Drift detected"));
        assert!(payload.contains("7"));
    }

    #[test]
    fn event_generic_payload_is_json() {
        let event = FederationEvent {
            kind: FederationEventKind::TierPromotion,
            swarm_id: "s1".to_string(),
            reflex_id: 1,
            cluster_id: "c".to_string(),
            summary: "Promoted".to_string(),
            timestamp_ms: 1000,
            metadata: HashMap::new(),
        };
        let payload = event.to_generic_payload();
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(parsed["kind"], "TierPromotion");
    }

    #[test]
    fn event_format_for_selects_correctly() {
        let event = FederationEvent {
            kind: FederationEventKind::ReflexExported,
            swarm_id: "s1".to_string(),
            reflex_id: 1,
            cluster_id: "c".to_string(),
            summary: "test".to_string(),
            timestamp_ms: 1000,
            metadata: HashMap::new(),
        };
        let slack = event.format_for(&WebhookKind::Slack);
        let discord = event.format_for(&WebhookKind::Discord);
        let generic = event.format_for(&WebhookKind::Generic);
        // Each format should be valid JSON.
        serde_json::from_str::<serde_json::Value>(&slack).unwrap();
        serde_json::from_str::<serde_json::Value>(&discord).unwrap();
        serde_json::from_str::<serde_json::Value>(&generic).unwrap();
    }

    // ---- Serde roundtrips ----

    #[test]
    fn federation_config_serde_roundtrip() {
        let config = FederationConfig {
            webhooks: vec![make_webhook_config()],
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let decoded: FederationConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.swarm_id, config.swarm_id);
        assert_eq!(decoded.webhooks.len(), 1);
    }

    #[test]
    fn import_result_serde_roundtrip() {
        let results = [
            ImportResult::Imported {
                reflex_id: 1,
                source_swarm: "s".to_string(),
            },
            ImportResult::SelfImport,
            ImportResult::UnsupportedSchema { version: 99 },
            ImportResult::InvalidTriggerKey,
            ImportResult::EmptyCommands,
        ];
        for r in &results {
            let json = serde_json::to_string(r).unwrap();
            let decoded: ImportResult = serde_json::from_str(&json).unwrap();
            assert_eq!(&decoded, r);
        }
    }

    #[test]
    fn federation_stats_serde_roundtrip() {
        let stats = FederationStats {
            total_exports: 10,
            total_imports: 5,
            total_webhooks: 20,
            pending_deliveries: 3,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let decoded: FederationStats = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, stats);
    }

    #[test]
    fn delivery_status_serde_roundtrip() {
        let statuses = [
            DeliveryStatus::Pending,
            DeliveryStatus::Delivered,
            DeliveryStatus::Failed {
                reason: "timeout".to_string(),
            },
        ];
        for s in &statuses {
            let json = serde_json::to_string(s).unwrap();
            let decoded: DeliveryStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(&decoded, s);
        }
    }

    #[test]
    fn tier_rank_ordering() {
        assert!(tier_to_rank(MaturityTier::Incubating) < tier_to_rank(MaturityTier::Graduated));
        assert!(tier_to_rank(MaturityTier::Graduated) < tier_to_rank(MaturityTier::Veteran));
    }
}
