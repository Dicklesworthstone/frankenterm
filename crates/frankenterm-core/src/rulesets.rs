//! Ruleset profile management for pattern packs.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::{PackOverride, PatternsConfig};

const RULESET_MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RulesetManifest {
    pub version: u32,
    pub rulesets: Vec<RulesetManifestEntry>,
}

impl Default for RulesetManifest {
    fn default() -> Self {
        Self {
            version: RULESET_MANIFEST_VERSION,
            rulesets: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RulesetManifestEntry {
    pub name: String,
    pub path: String,
    pub description: Option<String>,
    pub created_at: Option<u64>,
    pub updated_at: Option<u64>,
    pub last_applied_at: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RulesetProfileFile {
    pub name: String,
    pub description: Option<String>,
    pub inherits: Option<String>,
    pub patterns: PatternsConfigPatch,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PatternsConfigPatch {
    pub packs: Option<Vec<String>>,
    pub pack_overrides: Option<HashMap<String, PackOverride>>,
    pub quick_reject_enabled: Option<bool>,
}

impl PatternsConfigPatch {
    #[must_use]
    pub fn apply_to(&self, base: &PatternsConfig) -> PatternsConfig {
        let mut merged = base.clone();

        if let Some(packs) = &self.packs {
            merged.packs.clone_from(packs);
        }

        if let Some(overrides) = &self.pack_overrides {
            merged.pack_overrides = merge_pack_overrides(&merged.pack_overrides, overrides);
        }

        if let Some(enabled) = self.quick_reject_enabled {
            merged.quick_reject_enabled = enabled;
        }

        merged
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RulesetProfileSummary {
    pub name: String,
    pub description: Option<String>,
    pub path: Option<String>,
    pub last_applied_at: Option<u64>,
    pub implicit: bool,
}

pub fn resolve_rulesets_dir(config_path: Option<&Path>) -> PathBuf {
    if let Some(path) = config_path {
        return path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("rulesets");
    }

    if let Some(path) = crate::config::resolve_config_path(None) {
        if let Some(parent) = path.parent() {
            return parent.join("rulesets");
        }
    }

    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("ft")
        .join("rulesets")
}

pub fn list_profiles(rulesets_dir: &Path) -> crate::Result<Vec<RulesetProfileSummary>> {
    let manifest = match load_manifest(rulesets_dir) {
        Ok(Some(manifest)) => manifest,
        Ok(None) => scan_rulesets(rulesets_dir)?,
        Err(err) => {
            tracing::warn!(error = %err, "Failed to read ruleset manifest; scanning directory");
            scan_rulesets(rulesets_dir)?
        }
    };

    let mut profiles = Vec::with_capacity(manifest.rulesets.len() + 1);
    profiles.push(RulesetProfileSummary {
        name: "default".to_string(),
        description: Some("Base ft.toml patterns".to_string()),
        path: None,
        last_applied_at: None,
        implicit: true,
    });

    for entry in manifest.rulesets {
        profiles.push(RulesetProfileSummary {
            name: entry.name,
            description: entry.description,
            path: Some(entry.path),
            last_applied_at: entry.last_applied_at,
            implicit: false,
        });
    }

    Ok(profiles)
}

pub fn resolve_patterns_for_profile(
    base: &PatternsConfig,
    rulesets_dir: &Path,
    manifest: Option<&RulesetManifest>,
    profile_name: &str,
) -> crate::Result<PatternsConfig> {
    let name = canonicalize_profile_name(profile_name)?;
    if name == "default" {
        return Ok(base.clone());
    }

    let mut visited = HashSet::new();
    let mut resolved = base.clone();
    resolve_profile_chain(&name, rulesets_dir, manifest, &mut visited, &mut resolved)?;
    Ok(resolved)
}

pub fn load_manifest(rulesets_dir: &Path) -> crate::Result<Option<RulesetManifest>> {
    let path = rulesets_dir.join("manifest.json");
    if !path.exists() {
        return Ok(None);
    }

    let content = std::fs::read_to_string(&path).map_err(|e| {
        crate::error::ConfigError::ReadFailed(path.display().to_string(), e.to_string())
    })?;
    let manifest: RulesetManifest = serde_json::from_str(&content)
        .map_err(|e| crate::error::ConfigError::ParseFailed(e.to_string()))?;

    Ok(Some(manifest))
}

pub fn write_manifest(rulesets_dir: &Path, manifest: &RulesetManifest) -> crate::Result<()> {
    let path = rulesets_dir.join("manifest.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            crate::error::ConfigError::ReadFailed(parent.display().to_string(), e.to_string())
        })?;
    }

    let content = serde_json::to_string_pretty(manifest)
        .map_err(|e| crate::error::ConfigError::SerializeFailed(e.to_string()))?;

    let tmp_path = path.with_extension("json.tmp");
    std::fs::write(&tmp_path, content).map_err(|e| {
        crate::error::ConfigError::ReadFailed(tmp_path.display().to_string(), e.to_string())
    })?;
    std::fs::rename(&tmp_path, &path).map_err(|e| {
        crate::error::ConfigError::ReadFailed(path.display().to_string(), e.to_string())
    })?;

    Ok(())
}

pub fn touch_last_applied(
    manifest: &mut RulesetManifest,
    profile_name: &str,
    profile_path: &str,
    applied_at: u64,
) {
    if let Some(entry) = manifest
        .rulesets
        .iter_mut()
        .find(|entry| entry.name == profile_name)
    {
        entry.last_applied_at = Some(applied_at);
        entry.updated_at = Some(applied_at);
        return;
    }

    manifest.rulesets.push(RulesetManifestEntry {
        name: profile_name.to_string(),
        path: profile_path.to_string(),
        description: None,
        created_at: Some(applied_at),
        updated_at: Some(applied_at),
        last_applied_at: Some(applied_at),
    });
}

pub fn scan_rulesets(rulesets_dir: &Path) -> crate::Result<RulesetManifest> {
    let mut manifest = RulesetManifest::default();

    if !rulesets_dir.exists() {
        return Ok(manifest);
    }

    let entries = std::fs::read_dir(rulesets_dir).map_err(|e| {
        crate::error::ConfigError::ReadFailed(rulesets_dir.display().to_string(), e.to_string())
    })?;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("toml") {
            continue;
        }

        match load_profile_from_path(&path) {
            Ok(profile) => {
                let name = if profile.name.trim().is_empty() {
                    path.file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_string()
                } else {
                    profile.name.clone()
                };

                let name = match canonicalize_profile_name(&name) {
                    Ok(name) if name != "default" => name,
                    _ => {
                        tracing::warn!(
                            path = %path.display(),
                            "Skipping ruleset profile with invalid or reserved name"
                        );
                        continue;
                    }
                };

                let (created_at, updated_at) = timestamps_for(&path);
                let file_name = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();

                manifest.rulesets.push(RulesetManifestEntry {
                    name,
                    path: file_name,
                    description: profile.description,
                    created_at,
                    updated_at,
                    last_applied_at: None,
                });
            }
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "Skipping invalid ruleset profile");
            }
        }
    }

    manifest.rulesets.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(manifest)
}

pub fn load_profile_by_name(
    rulesets_dir: &Path,
    manifest: Option<&RulesetManifest>,
    profile_name: &str,
) -> crate::Result<RulesetProfileFile> {
    let canonical = canonicalize_profile_name(profile_name)?;
    if canonical == "default" {
        return Err(crate::error::ConfigError::ValidationError(
            "default is implicit and has no profile file".to_string(),
        )
        .into());
    }

    let path = manifest
        .and_then(|manifest| {
            manifest
                .rulesets
                .iter()
                .find(|entry| entry.name == canonical)
                .map(|entry| rulesets_dir.join(&entry.path))
        })
        .unwrap_or_else(|| rulesets_dir.join(format!("{canonical}.toml")));

    let profile = load_profile_from_path(&path)?;
    if !profile.name.trim().is_empty() {
        let file_name = canonicalize_profile_name(&profile.name)?;
        if file_name != canonical {
            return Err(crate::error::ConfigError::ValidationError(format!(
                "ruleset profile name '{}' does not match requested '{}'",
                profile.name, canonical
            ))
            .into());
        }
    }

    Ok(profile)
}

fn resolve_profile_chain(
    name: &str,
    rulesets_dir: &Path,
    manifest: Option<&RulesetManifest>,
    visited: &mut HashSet<String>,
    current: &mut PatternsConfig,
) -> crate::Result<()> {
    if name == "default" {
        return Ok(());
    }

    if !visited.insert(name.to_string()) {
        return Err(crate::error::ConfigError::ValidationError(format!(
            "ruleset profile inheritance cycle detected at '{name}'"
        ))
        .into());
    }

    let profile = load_profile_by_name(rulesets_dir, manifest, name)?;
    let inherits = profile.inherits.as_deref().unwrap_or("default");
    let inherits = canonicalize_profile_name(inherits)?;

    if inherits != "default" {
        resolve_profile_chain(&inherits, rulesets_dir, manifest, visited, current)?;
    }

    *current = profile.patterns.apply_to(current);
    Ok(())
}

fn load_profile_from_path(path: &Path) -> crate::Result<RulesetProfileFile> {
    let content = std::fs::read_to_string(path).map_err(|e| {
        crate::error::ConfigError::ReadFailed(path.display().to_string(), e.to_string())
    })?;

    toml::from_str(&content)
        .map_err(|e| crate::error::ConfigError::ParseFailed(e.to_string()).into())
}

fn canonicalize_profile_name(raw: &str) -> crate::Result<String> {
    let name = raw.trim().to_lowercase();
    if !is_valid_profile_name(&name) {
        return Err(crate::error::ConfigError::ValidationError(format!(
            "invalid ruleset profile name '{raw}' (expected [a-z0-9_-]{{1,32}})"
        ))
        .into());
    }

    Ok(name)
}

fn is_valid_profile_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 32 {
        return false;
    }

    name.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

fn merge_pack_overrides(
    base: &HashMap<String, PackOverride>,
    overlay: &HashMap<String, PackOverride>,
) -> HashMap<String, PackOverride> {
    let mut merged = base.clone();

    for (key, override_cfg) in overlay {
        match merged.get(key) {
            Some(existing) => {
                merged.insert(key.clone(), merge_pack_override(existing, override_cfg));
            }
            None => {
                merged.insert(key.clone(), override_cfg.clone());
            }
        }
    }

    merged
}

fn merge_pack_override(base: &PackOverride, overlay: &PackOverride) -> PackOverride {
    let mut merged = base.clone();

    for rule in &overlay.disabled_rules {
        if !merged.disabled_rules.contains(rule) {
            merged.disabled_rules.push(rule.clone());
        }
    }

    for (rule_id, severity) in &overlay.severity_overrides {
        merged
            .severity_overrides
            .insert(rule_id.clone(), severity.clone());
    }

    for (key, value) in &overlay.extra {
        merged.extra.insert(key.clone(), value.clone());
    }

    merged
}

fn timestamps_for(path: &Path) -> (Option<u64>, Option<u64>) {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => return (None, None),
    };

    let created_at = metadata.created().ok().and_then(system_time_to_epoch_ms);
    let updated_at = metadata.modified().ok().and_then(system_time_to_epoch_ms);

    (created_at, updated_at)
}

fn system_time_to_epoch_ms(ts: SystemTime) -> Option<u64> {
    ts.duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_last_applied_updates_existing_ruleset_timestamps() {
        let mut manifest = RulesetManifest {
            version: RULESET_MANIFEST_VERSION,
            rulesets: vec![RulesetManifestEntry {
                name: "ops".to_string(),
                path: "ops.toml".to_string(),
                description: Some("Ops profile".to_string()),
                created_at: Some(100),
                updated_at: Some(200),
                last_applied_at: Some(200),
            }],
        };

        touch_last_applied(&mut manifest, "ops", "ops.toml", 700);

        assert_eq!(manifest.rulesets[0].last_applied_at, Some(700));
        assert_eq!(manifest.rulesets[0].updated_at, Some(700));
        assert_eq!(manifest.rulesets[0].created_at, Some(100));
    }

    #[test]
    fn touch_last_applied_creates_entry_when_missing() {
        let mut manifest = RulesetManifest::default();

        touch_last_applied(&mut manifest, "newruleset", "newruleset.toml", 1000);

        assert_eq!(manifest.rulesets.len(), 1);
        assert_eq!(manifest.rulesets[0].name, "newruleset");
        assert_eq!(manifest.rulesets[0].path, "newruleset.toml");
        assert_eq!(manifest.rulesets[0].created_at, Some(1000));
    }

    // =========================================================================
    // Profile Name Validation (rulesets)
    // =========================================================================

    #[test]
    fn valid_ruleset_profile_names() {
        for name in ["ops", "dev-ci", "staging_1", "a", "abc-123_def"] {
            assert!(
                canonicalize_profile_name(name).is_ok(),
                "'{name}' should be valid"
            );
        }
    }

    #[test]
    fn empty_ruleset_name_rejected() {
        assert!(canonicalize_profile_name("").is_err());
        assert!(canonicalize_profile_name("  ").is_err());
    }

    #[test]
    fn too_long_ruleset_name_rejected() {
        let long = "x".repeat(33);
        assert!(canonicalize_profile_name(&long).is_err());
        let exact = "x".repeat(32);
        assert!(canonicalize_profile_name(&exact).is_ok());
    }

    #[test]
    fn special_chars_in_ruleset_name_rejected() {
        for name in ["has space", "dot.name", "slash/name", "excl!", "пример"] {
            assert!(
                canonicalize_profile_name(name).is_err(),
                "'{name}' should be rejected"
            );
        }
    }

    #[test]
    fn ruleset_name_canonicalization() {
        assert_eq!(canonicalize_profile_name("OPS").unwrap(), "ops");
        assert_eq!(canonicalize_profile_name("  Dev  ").unwrap(), "dev");
    }

    // =========================================================================
    // PatternsConfigPatch Tests
    // =========================================================================

    #[test]
    fn patch_apply_replaces_packs() {
        let base = PatternsConfig {
            packs: vec!["agent-codex".to_string()],
            ..Default::default()
        };
        let patch = PatternsConfigPatch {
            packs: Some(vec!["agent-claude".to_string(), "agent-gemini".to_string()]),
            ..Default::default()
        };
        let result = patch.apply_to(&base);
        assert_eq!(result.packs, vec!["agent-claude", "agent-gemini"]);
    }

    #[test]
    fn patch_apply_preserves_base_when_none() {
        let base = PatternsConfig {
            packs: vec!["agent-codex".to_string()],
            quick_reject_enabled: true,
            ..Default::default()
        };
        let patch = PatternsConfigPatch::default();
        let result = patch.apply_to(&base);
        assert_eq!(result.packs, vec!["agent-codex"]);
        assert!(result.quick_reject_enabled);
    }

    #[test]
    fn patch_apply_overrides_quick_reject() {
        let base = PatternsConfig {
            quick_reject_enabled: true,
            ..Default::default()
        };
        let patch = PatternsConfigPatch {
            quick_reject_enabled: Some(false),
            ..Default::default()
        };
        let result = patch.apply_to(&base);
        assert!(!result.quick_reject_enabled);
    }

    #[test]
    fn patch_merges_pack_overrides() {
        let mut base_overrides = HashMap::new();
        base_overrides.insert(
            "agent-codex".to_string(),
            PackOverride {
                disabled_rules: vec!["rule1".to_string()],
                ..Default::default()
            },
        );
        let base = PatternsConfig {
            pack_overrides: base_overrides,
            ..Default::default()
        };

        let mut overlay_overrides = HashMap::new();
        overlay_overrides.insert(
            "agent-codex".to_string(),
            PackOverride {
                disabled_rules: vec!["rule2".to_string()],
                ..Default::default()
            },
        );
        let patch = PatternsConfigPatch {
            pack_overrides: Some(overlay_overrides),
            ..Default::default()
        };

        let result = patch.apply_to(&base);
        let codex_override = result.pack_overrides.get("agent-codex").unwrap();
        assert!(codex_override.disabled_rules.contains(&"rule1".to_string()));
        assert!(codex_override.disabled_rules.contains(&"rule2".to_string()));
    }

    // =========================================================================
    // Profile Name Boundary Tests
    // =========================================================================

    #[test]
    fn is_valid_profile_name_accepts_all_allowed_chars() {
        assert!(is_valid_profile_name("abcdefghijklmnopqrstuvwxyz"));
        assert!(is_valid_profile_name("0123456789"));
        assert!(is_valid_profile_name("_"));
        assert!(is_valid_profile_name("-"));
        assert!(is_valid_profile_name("a-b_c-0"));
    }

    #[test]
    fn is_valid_profile_name_rejects_uppercase() {
        assert!(!is_valid_profile_name("A"));
        assert!(!is_valid_profile_name("Dev"));
    }

    #[test]
    fn default_profile_resolves_to_base() {
        let base = PatternsConfig {
            packs: vec!["default-pack".to_string()],
            ..Default::default()
        };
        let dir = std::env::temp_dir().join("wa_test_rulesets_default");
        let _ = std::fs::create_dir_all(&dir);
        let result = resolve_patterns_for_profile(&base, &dir, None, "default").unwrap();
        assert_eq!(result.packs, base.packs);
    }

    #[test]
    fn load_default_profile_by_name_is_rejected() {
        let dir = std::env::temp_dir().join("wa_test_rulesets_load_default");
        let _ = std::fs::create_dir_all(&dir);
        let result = load_profile_by_name(&dir, None, "default");
        assert!(result.is_err());
    }

    #[test]
    fn missing_profile_file_produces_error() {
        let dir = std::env::temp_dir().join("wa_test_rulesets_missing");
        let _ = std::fs::create_dir_all(&dir);
        let result = load_profile_by_name(&dir, None, "nonexistent");
        assert!(result.is_err());
    }

    // =========================================================================
    // Serde roundtrip tests
    // =========================================================================

    #[test]
    fn ruleset_manifest_serde_roundtrip() {
        let manifest = RulesetManifest {
            version: RULESET_MANIFEST_VERSION,
            rulesets: vec![RulesetManifestEntry {
                name: "ci-ops".to_string(),
                path: "ci-ops.toml".to_string(),
                description: Some("CI operations profile".to_string()),
                created_at: Some(1000),
                updated_at: Some(2000),
                last_applied_at: Some(2000),
            }],
        };
        let json = serde_json::to_string(&manifest).unwrap();
        let back: RulesetManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.version, RULESET_MANIFEST_VERSION);
        assert_eq!(back.rulesets.len(), 1);
        assert_eq!(back.rulesets[0].name, "ci-ops");
        assert_eq!(
            back.rulesets[0].description.as_deref(),
            Some("CI operations profile")
        );
    }

    #[test]
    fn ruleset_manifest_serde_default_fills_missing() {
        let back: RulesetManifest = serde_json::from_str("{}").unwrap();
        assert_eq!(back.version, RULESET_MANIFEST_VERSION);
        assert!(back.rulesets.is_empty());
    }

    #[test]
    fn ruleset_manifest_entry_serde_default() {
        let back: RulesetManifestEntry = serde_json::from_str("{}").unwrap();
        assert!(back.name.is_empty());
        assert!(back.path.is_empty());
        assert!(back.description.is_none());
        assert!(back.created_at.is_none());
        assert!(back.updated_at.is_none());
        assert!(back.last_applied_at.is_none());
    }

    #[test]
    fn ruleset_profile_file_serde_roundtrip() {
        let profile = RulesetProfileFile {
            name: "staging".to_string(),
            description: Some("Staging rules".to_string()),
            inherits: Some("default".to_string()),
            patterns: PatternsConfigPatch::default(),
        };
        let json = serde_json::to_string(&profile).unwrap();
        let back: RulesetProfileFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "staging");
        assert_eq!(back.inherits.as_deref(), Some("default"));
    }

    #[test]
    fn patterns_config_patch_serde_roundtrip() {
        let patch = PatternsConfigPatch {
            packs: Some(vec!["agent-codex".to_string()]),
            pack_overrides: Some(HashMap::new()),
            quick_reject_enabled: Some(true),
        };
        let json = serde_json::to_string(&patch).unwrap();
        let back: PatternsConfigPatch = serde_json::from_str(&json).unwrap();
        assert_eq!(back.packs.as_ref().unwrap().len(), 1);
        assert_eq!(back.quick_reject_enabled, Some(true));
    }

    #[test]
    fn ruleset_profile_summary_serializes() {
        let summary = RulesetProfileSummary {
            name: "ops".to_string(),
            description: Some("Ops rules".to_string()),
            path: Some("ops.toml".to_string()),
            last_applied_at: Some(5000),
            implicit: false,
        };
        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.contains("ops"));
        assert!(json.contains("5000"));
    }

    // =========================================================================
    // Default value tests
    // =========================================================================

    #[test]
    fn ruleset_manifest_default() {
        let m = RulesetManifest::default();
        assert_eq!(m.version, RULESET_MANIFEST_VERSION);
        assert!(m.rulesets.is_empty());
    }

    #[test]
    fn ruleset_manifest_entry_default() {
        let e = RulesetManifestEntry::default();
        assert!(e.name.is_empty());
        assert!(e.path.is_empty());
        assert!(e.description.is_none());
    }

    #[test]
    fn patterns_config_patch_default() {
        let p = PatternsConfigPatch::default();
        assert!(p.packs.is_none());
        assert!(p.pack_overrides.is_none());
        assert!(p.quick_reject_enabled.is_none());
    }

    #[test]
    fn ruleset_profile_file_default() {
        let f = RulesetProfileFile::default();
        assert!(f.name.is_empty());
        assert!(f.description.is_none());
        assert!(f.inherits.is_none());
    }

    // =========================================================================
    // touch_last_applied edge cases
    // =========================================================================

    #[test]
    fn touch_last_applied_multiple_entries_only_updates_target() {
        let mut manifest = RulesetManifest {
            version: RULESET_MANIFEST_VERSION,
            rulesets: vec![
                RulesetManifestEntry {
                    name: "alpha".to_string(),
                    path: "alpha.toml".to_string(),
                    description: None,
                    created_at: Some(100),
                    updated_at: Some(100),
                    last_applied_at: Some(100),
                },
                RulesetManifestEntry {
                    name: "beta".to_string(),
                    path: "beta.toml".to_string(),
                    description: None,
                    created_at: Some(200),
                    updated_at: Some(200),
                    last_applied_at: Some(200),
                },
            ],
        };

        touch_last_applied(&mut manifest, "alpha", "alpha.toml", 500);

        assert_eq!(manifest.rulesets[0].last_applied_at, Some(500));
        assert_eq!(manifest.rulesets[0].updated_at, Some(500));
        // Beta should be unchanged
        assert_eq!(manifest.rulesets[1].last_applied_at, Some(200));
        assert_eq!(manifest.rulesets[1].updated_at, Some(200));
    }

    #[test]
    fn touch_last_applied_preserves_description() {
        let mut manifest = RulesetManifest {
            version: RULESET_MANIFEST_VERSION,
            rulesets: vec![RulesetManifestEntry {
                name: "ops".to_string(),
                path: "ops.toml".to_string(),
                description: Some("Operations profile".to_string()),
                created_at: Some(100),
                updated_at: Some(100),
                last_applied_at: None,
            }],
        };

        touch_last_applied(&mut manifest, "ops", "ops.toml", 300);

        assert_eq!(
            manifest.rulesets[0].description.as_deref(),
            Some("Operations profile")
        );
    }

    // =========================================================================
    // PatternsConfigPatch::apply_to edge cases
    // =========================================================================

    #[test]
    fn patch_apply_empty_packs_clears_base() {
        let base = PatternsConfig {
            packs: vec!["agent-codex".to_string()],
            ..Default::default()
        };
        let patch = PatternsConfigPatch {
            packs: Some(vec![]),
            ..Default::default()
        };
        let result = patch.apply_to(&base);
        assert!(result.packs.is_empty());
    }

    #[test]
    fn patch_apply_new_pack_override_key() {
        let base = PatternsConfig::default();
        let mut overlay = HashMap::new();
        overlay.insert(
            "new-pack".to_string(),
            PackOverride {
                disabled_rules: vec!["new-rule".to_string()],
                ..Default::default()
            },
        );
        let patch = PatternsConfigPatch {
            pack_overrides: Some(overlay),
            ..Default::default()
        };
        let result = patch.apply_to(&base);
        assert!(result.pack_overrides.contains_key("new-pack"));
    }

    #[test]
    fn patch_apply_severity_override_merged() {
        let mut base_overrides = HashMap::new();
        base_overrides.insert(
            "pack-a".to_string(),
            PackOverride {
                severity_overrides: {
                    let mut m = HashMap::new();
                    m.insert("rule-1".to_string(), "warning".to_string());
                    m
                },
                ..Default::default()
            },
        );
        let base = PatternsConfig {
            pack_overrides: base_overrides,
            ..Default::default()
        };

        let mut overlay_overrides = HashMap::new();
        overlay_overrides.insert(
            "pack-a".to_string(),
            PackOverride {
                severity_overrides: {
                    let mut m = HashMap::new();
                    m.insert("rule-1".to_string(), "critical".to_string());
                    m.insert("rule-2".to_string(), "info".to_string());
                    m
                },
                ..Default::default()
            },
        );
        let patch = PatternsConfigPatch {
            pack_overrides: Some(overlay_overrides),
            ..Default::default()
        };

        let result = patch.apply_to(&base);
        let pack_a = result.pack_overrides.get("pack-a").unwrap();
        // Overlay replaces existing severity
        assert_eq!(pack_a.severity_overrides.get("rule-1").unwrap(), "critical");
        // Overlay adds new severity
        assert_eq!(pack_a.severity_overrides.get("rule-2").unwrap(), "info");
    }

    #[test]
    fn patch_apply_disabled_rules_no_duplicates() {
        let mut base_overrides = HashMap::new();
        base_overrides.insert(
            "pack-a".to_string(),
            PackOverride {
                disabled_rules: vec!["rule-1".to_string(), "rule-2".to_string()],
                ..Default::default()
            },
        );
        let base = PatternsConfig {
            pack_overrides: base_overrides,
            ..Default::default()
        };

        let mut overlay = HashMap::new();
        overlay.insert(
            "pack-a".to_string(),
            PackOverride {
                disabled_rules: vec!["rule-2".to_string(), "rule-3".to_string()],
                ..Default::default()
            },
        );
        let patch = PatternsConfigPatch {
            pack_overrides: Some(overlay),
            ..Default::default()
        };

        let result = patch.apply_to(&base);
        let pack_a = result.pack_overrides.get("pack-a").unwrap();
        // rule-2 should not be duplicated
        let rule2_count = pack_a
            .disabled_rules
            .iter()
            .filter(|r| *r == "rule-2")
            .count();
        assert_eq!(rule2_count, 1);
        assert!(pack_a.disabled_rules.contains(&"rule-1".to_string()));
        assert!(pack_a.disabled_rules.contains(&"rule-3".to_string()));
    }

    // =========================================================================
    // is_valid_profile_name edge cases
    // =========================================================================

    #[test]
    fn is_valid_profile_name_exactly_32_chars() {
        let name = "a".repeat(32);
        assert!(is_valid_profile_name(&name));
    }

    #[test]
    fn is_valid_profile_name_33_chars_rejected() {
        let name = "a".repeat(33);
        assert!(!is_valid_profile_name(&name));
    }

    #[test]
    fn is_valid_profile_name_single_char() {
        assert!(is_valid_profile_name("a"));
        assert!(is_valid_profile_name("0"));
        assert!(is_valid_profile_name("_"));
        assert!(is_valid_profile_name("-"));
    }

    #[test]
    fn is_valid_profile_name_empty() {
        assert!(!is_valid_profile_name(""));
    }

    // =========================================================================
    // canonicalize_profile_name edge cases
    // =========================================================================

    #[test]
    fn canonicalize_trims_whitespace() {
        assert_eq!(canonicalize_profile_name("  dev  ").unwrap(), "dev");
    }

    #[test]
    fn canonicalize_lowercases_mixed_case() {
        assert_eq!(canonicalize_profile_name("Dev-CI").unwrap(), "dev-ci");
    }

    #[test]
    fn canonicalize_preserves_digits() {
        assert_eq!(canonicalize_profile_name("v2-beta-3").unwrap(), "v2-beta-3");
    }

    #[test]
    fn canonicalize_rejects_dots() {
        assert!(canonicalize_profile_name("my.profile").is_err());
    }

    #[test]
    fn canonicalize_rejects_whitespace_in_middle() {
        assert!(canonicalize_profile_name("my profile").is_err());
    }

    // =========================================================================
    // Debug / Clone trait tests
    // =========================================================================

    #[test]
    fn ruleset_manifest_debug() {
        let m = RulesetManifest::default();
        let dbg = format!("{m:?}");
        assert!(dbg.contains("RulesetManifest"));
    }

    #[test]
    fn ruleset_manifest_entry_clone() {
        let e = RulesetManifestEntry {
            name: "ops".to_string(),
            path: "ops.toml".to_string(),
            description: Some("desc".to_string()),
            created_at: Some(1),
            updated_at: Some(2),
            last_applied_at: Some(3),
        };
        let e2 = e.clone();
        assert_eq!(e2.name, "ops");
        assert_eq!(e2.last_applied_at, Some(3));
    }

    #[test]
    fn ruleset_profile_summary_debug_clone() {
        let s = RulesetProfileSummary {
            name: "test".to_string(),
            description: None,
            path: None,
            last_applied_at: None,
            implicit: true,
        };
        let s2 = s.clone();
        assert_eq!(s2.name, "test");
        assert!(s2.implicit);
        let dbg = format!("{s:?}");
        assert!(dbg.contains("RulesetProfileSummary"));
    }

    // =========================================================================
    // system_time_to_epoch_ms (private, tested indirectly)
    // =========================================================================

    #[test]
    fn resolve_rulesets_dir_with_config_path() {
        let config_path = std::path::Path::new("/home/user/.config/ft/ft.toml");
        let dir = resolve_rulesets_dir(Some(config_path));
        assert_eq!(
            dir,
            std::path::PathBuf::from("/home/user/.config/ft/rulesets")
        );
    }

    #[test]
    fn resolve_rulesets_dir_with_root_config_path() {
        // Edge case: config in root directory
        let config_path = std::path::Path::new("/ft.toml");
        let dir = resolve_rulesets_dir(Some(config_path));
        assert_eq!(dir, std::path::PathBuf::from("/rulesets"));
    }
}
