//! Shared query helpers for optional UI surfaces (TUI and web).
//!
//! These helpers keep read-only UI data access logic in one place so
//! frontends don't duplicate storage/profile resolution behavior.

use std::path::Path;

use serde::Serialize;

use crate::rulesets::RulesetProfileSummary;
use crate::storage::{PaneBookmarkRecord, SavedSearchRecord, StorageHandle};

/// Bookmark data prepared for UI rendering.
#[derive(Debug, Clone, Serialize)]
pub struct PaneBookmarkView {
    pub pane_id: u64,
    pub alias: String,
    pub tags: Vec<String>,
    pub description: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl From<PaneBookmarkRecord> for PaneBookmarkView {
    fn from(record: PaneBookmarkRecord) -> Self {
        Self {
            pane_id: record.pane_id,
            alias: record.alias,
            tags: record.tags.unwrap_or_default(),
            description: record.description,
            created_at: record.created_at,
            updated_at: record.updated_at,
        }
    }
}

/// Saved search data prepared for UI rendering.
#[derive(Debug, Clone, Serialize)]
pub struct SavedSearchView {
    pub id: String,
    pub name: String,
    pub query: String,
    pub pane_id: Option<u64>,
    pub limit: i64,
    pub since_mode: String,
    pub since_ms: Option<i64>,
    pub schedule_interval_ms: Option<i64>,
    pub enabled: bool,
    pub last_run_at: Option<i64>,
    pub last_result_count: Option<i64>,
    pub last_error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl From<SavedSearchRecord> for SavedSearchView {
    fn from(record: SavedSearchRecord) -> Self {
        Self {
            id: record.id,
            name: record.name,
            query: record.query,
            pane_id: record.pane_id,
            limit: record.limit,
            since_mode: record.since_mode,
            since_ms: record.since_ms,
            schedule_interval_ms: record.schedule_interval_ms,
            enabled: record.enabled,
            last_run_at: record.last_run_at,
            last_result_count: record.last_result_count,
            last_error: record.last_error,
            created_at: record.created_at,
            updated_at: record.updated_at,
        }
    }
}

/// Ruleset profile state prepared for UI rendering.
#[derive(Debug, Clone, Serialize)]
pub struct RulesetProfileState {
    pub active_profile: String,
    pub active_last_applied_at: Option<u64>,
    pub profiles: Vec<RulesetProfileSummary>,
}

impl Default for RulesetProfileState {
    fn default() -> Self {
        Self {
            active_profile: "default".to_string(),
            active_last_applied_at: None,
            profiles: vec![RulesetProfileSummary {
                name: "default".to_string(),
                description: Some("Base wa.toml patterns".to_string()),
                path: None,
                last_applied_at: None,
                implicit: true,
            }],
        }
    }
}

/// List all pane bookmarks for UI surfaces.
pub async fn list_pane_bookmarks(storage: &StorageHandle) -> crate::Result<Vec<PaneBookmarkView>> {
    let records = storage.list_pane_bookmarks().await?;
    Ok(records.into_iter().map(PaneBookmarkView::from).collect())
}

/// List saved searches for UI surfaces.
pub async fn list_saved_searches(storage: &StorageHandle) -> crate::Result<Vec<SavedSearchView>> {
    let records = storage.list_saved_searches().await?;
    Ok(records.into_iter().map(SavedSearchView::from).collect())
}

/// Resolve ruleset profile status, including the currently active profile.
///
/// Active profile semantics:
/// - `default` when no profile has been applied yet
/// - otherwise, profile with the greatest `last_applied_at` timestamp
/// - ties resolve lexicographically by profile name for determinism
pub fn resolve_ruleset_profile_state(
    config_path: Option<&Path>,
) -> crate::Result<RulesetProfileState> {
    let rulesets_dir = crate::rulesets::resolve_rulesets_dir(config_path);
    let profiles = crate::rulesets::list_profiles(&rulesets_dir)?;

    let mut active_profile = "default".to_string();
    let mut active_last_applied_at = None;

    for profile in &profiles {
        let Some(ts) = profile.last_applied_at else {
            continue;
        };
        match active_last_applied_at {
            None => {
                active_last_applied_at = Some(ts);
                active_profile.clone_from(&profile.name);
            }
            Some(current) if ts > current => {
                active_last_applied_at = Some(ts);
                active_profile.clone_from(&profile.name);
            }
            Some(current) if ts == current && profile.name < active_profile => {
                active_last_applied_at = Some(ts);
                active_profile.clone_from(&profile.name);
            }
            Some(_) => {}
        }
    }

    Ok(RulesetProfileState {
        active_profile,
        active_last_applied_at,
        profiles,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_temp_dir(label: &str) -> std::path::PathBuf {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|d| u128::try_from(d.as_nanos()).ok())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("wa_ui_query_{label}_{now}"))
    }

    #[test]
    fn profile_state_defaults_to_default_profile() {
        let root = unique_temp_dir("default");
        std::fs::create_dir_all(&root).expect("create temp root");
        let config_path = root.join("wa.toml");
        std::fs::write(&config_path, "").expect("write temp config");

        let state = resolve_ruleset_profile_state(Some(&config_path)).expect("resolve state");
        assert_eq!(state.active_profile, "default");
        assert!(
            state
                .profiles
                .iter()
                .any(|profile| profile.name == "default"),
            "default profile should always exist"
        );
    }

    #[test]
    fn profile_state_uses_most_recent_last_applied() {
        let root = unique_temp_dir("active");
        let rulesets_dir = root.join("rulesets");
        std::fs::create_dir_all(&rulesets_dir).expect("create rulesets dir");
        let config_path = root.join("wa.toml");
        std::fs::write(&config_path, "").expect("write temp config");

        let manifest = crate::rulesets::RulesetManifest {
            version: 1,
            rulesets: vec![
                crate::rulesets::RulesetManifestEntry {
                    name: "dev".to_string(),
                    path: "dev.toml".to_string(),
                    description: Some("Dev profile".to_string()),
                    created_at: None,
                    updated_at: None,
                    last_applied_at: Some(100),
                },
                crate::rulesets::RulesetManifestEntry {
                    name: "incident".to_string(),
                    path: "incident.toml".to_string(),
                    description: Some("Incident response".to_string()),
                    created_at: None,
                    updated_at: None,
                    last_applied_at: Some(250),
                },
            ],
        };
        let manifest_json = serde_json::to_string(&manifest).expect("serialize manifest");
        std::fs::write(rulesets_dir.join("manifest.json"), manifest_json).expect("write manifest");

        let state = resolve_ruleset_profile_state(Some(&config_path)).expect("resolve state");
        assert_eq!(state.active_profile, "incident");
        assert_eq!(state.active_last_applied_at, Some(250));
    }

    #[test]
    fn saved_search_view_preserves_last_run_status() {
        let mut record = SavedSearchRecord::new(
            "errors".to_string(),
            "error".to_string(),
            Some(7),
            25,
            crate::storage::SAVED_SEARCH_SINCE_MODE_LAST_RUN.to_string(),
            None,
        );
        record.schedule_interval_ms = Some(60_000);
        record.enabled = true;
        record.last_run_at = Some(111);
        record.last_result_count = Some(3);
        record.last_error = Some("none".to_string());

        let view = SavedSearchView::from(record);
        assert_eq!(view.name, "errors");
        assert_eq!(view.pane_id, Some(7));
        assert_eq!(view.limit, 25);
        assert!(view.enabled);
        assert_eq!(view.last_run_at, Some(111));
        assert_eq!(view.last_result_count, Some(3));
        assert_eq!(view.last_error.as_deref(), Some("none"));
    }
}
