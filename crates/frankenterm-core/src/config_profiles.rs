//! Config profile management (ft.toml overlays).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const CONFIG_PROFILE_MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ConfigProfileManifest {
    pub version: u32,
    pub profiles: Vec<ConfigProfileManifestEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_applied_profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_applied_at: Option<u64>,
}

impl Default for ConfigProfileManifest {
    fn default() -> Self {
        Self {
            version: CONFIG_PROFILE_MANIFEST_VERSION,
            profiles: Vec::new(),
            last_applied_profile: None,
            last_applied_at: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ConfigProfileManifestEntry {
    pub name: String,
    pub path: String,
    pub description: Option<String>,
    pub created_at: Option<u64>,
    pub updated_at: Option<u64>,
    pub last_applied_at: Option<u64>,
}

impl Default for ConfigProfileManifestEntry {
    fn default() -> Self {
        Self {
            name: String::new(),
            path: String::new(),
            description: None,
            created_at: None,
            updated_at: None,
            last_applied_at: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigProfileSummary {
    pub name: String,
    pub description: Option<String>,
    pub path: Option<String>,
    pub last_applied_at: Option<u64>,
    pub implicit: bool,
}

pub fn resolve_profiles_dir(config_path: Option<&Path>) -> PathBuf {
    if let Some(path) = config_path {
        return path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("profiles");
    }

    if let Some(path) = crate::config::resolve_config_path(None) {
        if let Some(parent) = path.parent() {
            return parent.join("profiles");
        }
    }

    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("ft")
        .join("profiles")
}

pub fn list_profiles(profiles_dir: &Path) -> crate::Result<Vec<ConfigProfileSummary>> {
    let manifest = match load_manifest(profiles_dir) {
        Ok(Some(manifest)) => manifest,
        Ok(None) => scan_profiles(profiles_dir)?,
        Err(err) => {
            tracing::warn!(error = %err, "Failed to read config profile manifest; scanning directory");
            scan_profiles(profiles_dir)?
        }
    };

    let mut profiles = Vec::with_capacity(manifest.profiles.len() + 1);
    profiles.push(ConfigProfileSummary {
        name: "default".to_string(),
        description: Some("Base ft.toml config".to_string()),
        path: None,
        last_applied_at: None,
        implicit: true,
    });

    for entry in manifest.profiles {
        profiles.push(ConfigProfileSummary {
            name: entry.name,
            description: entry.description,
            path: Some(entry.path),
            last_applied_at: entry.last_applied_at,
            implicit: false,
        });
    }

    Ok(profiles)
}

pub fn load_manifest(profiles_dir: &Path) -> crate::Result<Option<ConfigProfileManifest>> {
    let path = profiles_dir.join("manifest.json");
    if !path.exists() {
        return Ok(None);
    }

    let content = std::fs::read_to_string(&path).map_err(|e| {
        crate::error::ConfigError::ReadFailed(path.display().to_string(), e.to_string())
    })?;
    let manifest: ConfigProfileManifest = serde_json::from_str(&content)
        .map_err(|e| crate::error::ConfigError::ParseFailed(e.to_string()))?;

    Ok(Some(manifest))
}

pub fn write_manifest(profiles_dir: &Path, manifest: &ConfigProfileManifest) -> crate::Result<()> {
    let path = profiles_dir.join("manifest.json");
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
    manifest: &mut ConfigProfileManifest,
    profile_name: &str,
    profile_path: &str,
    applied_at: u64,
) {
    manifest.last_applied_profile = Some(profile_name.to_string());
    manifest.last_applied_at = Some(applied_at);

    if let Some(entry) = manifest
        .profiles
        .iter_mut()
        .find(|entry| entry.name == profile_name)
    {
        entry.last_applied_at = Some(applied_at);
        entry.updated_at = Some(applied_at);
        return;
    }

    manifest.profiles.push(ConfigProfileManifestEntry {
        name: profile_name.to_string(),
        path: profile_path.to_string(),
        description: None,
        created_at: Some(applied_at),
        updated_at: Some(applied_at),
        last_applied_at: Some(applied_at),
    });
}

pub fn scan_profiles(profiles_dir: &Path) -> crate::Result<ConfigProfileManifest> {
    let mut manifest = ConfigProfileManifest::default();

    if !profiles_dir.exists() {
        return Ok(manifest);
    }

    let entries = std::fs::read_dir(profiles_dir).map_err(|e| {
        crate::error::ConfigError::ReadFailed(profiles_dir.display().to_string(), e.to_string())
    })?;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("toml") {
            continue;
        }

        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let name = match canonicalize_profile_name(&name) {
            Ok(name) if name != "default" => name,
            _ => {
                tracing::warn!(
                    path = %path.display(),
                    "Skipping config profile with invalid or reserved name"
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

        manifest.profiles.push(ConfigProfileManifestEntry {
            name,
            path: file_name,
            description: None,
            created_at,
            updated_at,
            last_applied_at: None,
        });
    }

    manifest.profiles.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(manifest)
}

pub fn resolve_profile_path(
    profiles_dir: &Path,
    manifest: Option<&ConfigProfileManifest>,
    profile_name: &str,
) -> crate::Result<(String, PathBuf, String)> {
    let canonical = canonicalize_profile_name(profile_name)?;
    if canonical == "default" {
        return Err(crate::error::ConfigError::ValidationError(
            "default is implicit and has no profile file".to_string(),
        )
        .into());
    }

    let (path, rel_path) = manifest
        .and_then(|manifest| {
            manifest
                .profiles
                .iter()
                .find(|entry| entry.name == canonical)
                .map(|entry| (profiles_dir.join(&entry.path), entry.path.clone()))
        })
        .unwrap_or_else(|| {
            let file_name = format!("{canonical}.toml");
            (profiles_dir.join(&file_name), file_name)
        });

    Ok((canonical, path, rel_path))
}

pub fn canonicalize_profile_name(raw: &str) -> crate::Result<String> {
    let name = raw.trim().to_lowercase();
    if !is_valid_profile_name(&name) {
        return Err(crate::error::ConfigError::ValidationError(format!(
            "invalid profile name '{raw}' (expected [a-z0-9_-]{{1,32}})"
        ))
        .into());
    }
    Ok(name)
}

fn is_valid_profile_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    let len = bytes.len();
    if len == 0 || len > 32 {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-')
}

fn timestamps_for(path: &Path) -> (Option<u64>, Option<u64>) {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => return (None, None),
    };

    let created_at = metadata.created().ok().and_then(system_time_to_ms);
    let updated_at = metadata.modified().ok().and_then(system_time_to_ms);
    (created_at, updated_at)
}

fn system_time_to_ms(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_last_applied_updates_existing_entry_timestamps() {
        let mut manifest = ConfigProfileManifest {
            version: CONFIG_PROFILE_MANIFEST_VERSION,
            profiles: vec![ConfigProfileManifestEntry {
                name: "dev".to_string(),
                path: "dev.toml".to_string(),
                description: Some("Dev profile".to_string()),
                created_at: Some(100),
                updated_at: Some(200),
                last_applied_at: Some(200),
            }],
            last_applied_profile: None,
            last_applied_at: None,
        };

        touch_last_applied(&mut manifest, "dev", "dev.toml", 500);

        assert_eq!(manifest.last_applied_profile.as_deref(), Some("dev"));
        assert_eq!(manifest.last_applied_at, Some(500));
        assert_eq!(manifest.profiles[0].last_applied_at, Some(500));
        assert_eq!(manifest.profiles[0].updated_at, Some(500));
        assert_eq!(manifest.profiles[0].created_at, Some(100));
    }

    #[test]
    fn touch_last_applied_creates_new_entry_when_missing() {
        let mut manifest = ConfigProfileManifest {
            version: CONFIG_PROFILE_MANIFEST_VERSION,
            profiles: vec![],
            last_applied_profile: None,
            last_applied_at: None,
        };

        touch_last_applied(&mut manifest, "staging", "staging.toml", 900);

        assert_eq!(manifest.last_applied_profile.as_deref(), Some("staging"));
        assert_eq!(manifest.last_applied_at, Some(900));
        assert_eq!(manifest.profiles.len(), 1);
        assert_eq!(manifest.profiles[0].name, "staging");
        assert_eq!(manifest.profiles[0].path, "staging.toml");
        assert_eq!(manifest.profiles[0].created_at, Some(900));
    }

    // =========================================================================
    // Profile Name Validation Tests
    // =========================================================================

    #[test]
    fn valid_profile_names() {
        for name in [
            "dev",
            "production",
            "my-profile",
            "test_env",
            "abc123",
            "a",
            "a-b_c",
        ] {
            assert!(
                canonicalize_profile_name(name).is_ok(),
                "'{name}' should be valid"
            );
        }
    }

    #[test]
    fn profile_name_trims_and_lowercases() {
        assert_eq!(canonicalize_profile_name("  Dev  ").unwrap(), "dev");
        assert_eq!(
            canonicalize_profile_name("PRODUCTION").unwrap(),
            "production"
        );
        assert_eq!(
            canonicalize_profile_name(" My-Profile ").unwrap(),
            "my-profile"
        );
    }

    #[test]
    fn empty_profile_name_rejected() {
        assert!(canonicalize_profile_name("").is_err());
        assert!(canonicalize_profile_name("   ").is_err());
    }

    #[test]
    fn profile_name_too_long_rejected() {
        let long_name = "a".repeat(33);
        assert!(canonicalize_profile_name(&long_name).is_err());
        // Exactly 32 should be fine
        let exact = "a".repeat(32);
        assert!(canonicalize_profile_name(&exact).is_ok());
    }

    #[test]
    fn profile_name_special_chars_rejected() {
        for name in [
            "my profile",
            "test!",
            "foo@bar",
            "a/b",
            "a.b",
            "café",
            "日本語",
        ] {
            assert!(
                canonicalize_profile_name(name).is_err(),
                "'{name}' should be rejected"
            );
        }
    }

    #[test]
    fn is_valid_profile_name_boundary() {
        assert!(is_valid_profile_name("a"));
        assert!(is_valid_profile_name("0"));
        assert!(is_valid_profile_name("-"));
        assert!(is_valid_profile_name("_"));
        assert!(!is_valid_profile_name(""));
        assert!(!is_valid_profile_name("A")); // uppercase
        assert!(!is_valid_profile_name(" ")); // space
    }

    // ── ConfigProfileManifest tests ──

    #[test]
    fn manifest_default_has_version_1() {
        let manifest = ConfigProfileManifest::default();
        assert_eq!(manifest.version, 1);
        assert!(manifest.profiles.is_empty());
        assert!(manifest.last_applied_profile.is_none());
        assert!(manifest.last_applied_at.is_none());
    }

    #[test]
    fn manifest_serde_roundtrip() {
        let manifest = ConfigProfileManifest {
            version: 1,
            profiles: vec![ConfigProfileManifestEntry {
                name: "dev".to_string(),
                path: "dev.toml".to_string(),
                description: Some("Development".to_string()),
                created_at: Some(1000),
                updated_at: Some(2000),
                last_applied_at: Some(3000),
            }],
            last_applied_profile: Some("dev".to_string()),
            last_applied_at: Some(3000),
        };

        let json = serde_json::to_string(&manifest).unwrap();
        let back: ConfigProfileManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.version, 1);
        assert_eq!(back.profiles.len(), 1);
        assert_eq!(back.profiles[0].name, "dev");
        assert_eq!(back.last_applied_profile.as_deref(), Some("dev"));
    }

    #[test]
    fn manifest_serde_defaults_from_empty_json() {
        let manifest: ConfigProfileManifest = serde_json::from_str("{}").unwrap();
        assert_eq!(manifest.version, 1);
        assert!(manifest.profiles.is_empty());
    }

    #[test]
    fn manifest_skip_serializing_none_fields() {
        let manifest = ConfigProfileManifest::default();
        let json = serde_json::to_string(&manifest).unwrap();
        assert!(!json.contains("last_applied_profile"));
        assert!(!json.contains("last_applied_at"));
    }

    // ── ConfigProfileManifestEntry tests ──

    #[test]
    fn manifest_entry_default() {
        let entry = ConfigProfileManifestEntry::default();
        assert!(entry.name.is_empty());
        assert!(entry.path.is_empty());
        assert!(entry.description.is_none());
        assert!(entry.created_at.is_none());
        assert!(entry.updated_at.is_none());
        assert!(entry.last_applied_at.is_none());
    }

    #[test]
    fn manifest_entry_serde_roundtrip() {
        let entry = ConfigProfileManifestEntry {
            name: "prod".to_string(),
            path: "prod.toml".to_string(),
            description: Some("Production profile".to_string()),
            created_at: Some(100),
            updated_at: Some(200),
            last_applied_at: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: ConfigProfileManifestEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "prod");
        assert_eq!(back.description.as_deref(), Some("Production profile"));
        assert!(back.last_applied_at.is_none());
    }

    #[test]
    fn manifest_entry_serde_defaults_from_empty_json() {
        let entry: ConfigProfileManifestEntry = serde_json::from_str("{}").unwrap();
        assert!(entry.name.is_empty());
        assert!(entry.path.is_empty());
    }

    // ── ConfigProfileSummary tests ──

    #[test]
    fn profile_summary_serialize() {
        let summary = ConfigProfileSummary {
            name: "dev".to_string(),
            description: Some("Dev profile".to_string()),
            path: Some("dev.toml".to_string()),
            last_applied_at: Some(500),
            implicit: false,
        };
        let json = serde_json::to_string(&summary).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["name"], "dev");
        assert_eq!(parsed["implicit"], false);
        assert_eq!(parsed["last_applied_at"], 500);
    }

    #[test]
    fn profile_summary_implicit() {
        let summary = ConfigProfileSummary {
            name: "default".to_string(),
            description: Some("Base config".to_string()),
            path: None,
            last_applied_at: None,
            implicit: true,
        };
        let json = serde_json::to_string(&summary).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["implicit"], true);
        assert!(parsed["path"].is_null());
    }

    // ── Helper function tests ──

    #[test]
    fn system_time_to_ms_works() {
        let time = UNIX_EPOCH + std::time::Duration::from_millis(1_700_000_000_000);
        assert_eq!(system_time_to_ms(time), Some(1_700_000_000_000));
    }

    #[test]
    fn timestamps_for_nonexistent_returns_none() {
        let (created, updated) = timestamps_for(Path::new("/nonexistent/path/foo.toml"));
        assert!(created.is_none());
        assert!(updated.is_none());
    }

    #[test]
    fn timestamps_for_real_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("test.toml");
        std::fs::write(&path, "# test").unwrap();
        let (created, updated) = timestamps_for(&path);
        // Both should be Some on macOS/Linux
        assert!(created.is_some() || updated.is_some());
    }

    // ── File-based tests ──

    #[test]
    fn write_and_load_manifest_roundtrip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let profiles_dir = tmp.path();

        let manifest = ConfigProfileManifest {
            version: 1,
            profiles: vec![ConfigProfileManifestEntry {
                name: "test".to_string(),
                path: "test.toml".to_string(),
                description: Some("Test".to_string()),
                created_at: Some(100),
                updated_at: Some(200),
                last_applied_at: None,
            }],
            last_applied_profile: None,
            last_applied_at: None,
        };

        write_manifest(profiles_dir, &manifest).unwrap();
        let loaded = load_manifest(profiles_dir).unwrap().unwrap();
        assert_eq!(loaded.profiles.len(), 1);
        assert_eq!(loaded.profiles[0].name, "test");
    }

    #[test]
    fn load_manifest_returns_none_when_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let result = load_manifest(tmp.path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn scan_profiles_empty_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let manifest = scan_profiles(tmp.path()).unwrap();
        assert!(manifest.profiles.is_empty());
    }

    #[test]
    fn scan_profiles_nonexistent_dir() {
        let manifest = scan_profiles(Path::new("/nonexistent/profiles")).unwrap();
        assert!(manifest.profiles.is_empty());
    }

    #[test]
    fn scan_profiles_finds_toml_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("dev.toml"), "# dev").unwrap();
        std::fs::write(tmp.path().join("staging.toml"), "# staging").unwrap();
        std::fs::write(tmp.path().join("readme.md"), "# not a profile").unwrap();

        let manifest = scan_profiles(tmp.path()).unwrap();
        assert_eq!(manifest.profiles.len(), 2);
        let names: Vec<_> = manifest.profiles.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"dev"));
        assert!(names.contains(&"staging"));
    }

    #[test]
    fn scan_profiles_skips_default_name() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("default.toml"), "# default").unwrap();
        std::fs::write(tmp.path().join("dev.toml"), "# dev").unwrap();

        let manifest = scan_profiles(tmp.path()).unwrap();
        assert_eq!(manifest.profiles.len(), 1);
        assert_eq!(manifest.profiles[0].name, "dev");
    }

    #[test]
    fn scan_profiles_sorts_alphabetically() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("zebra.toml"), "").unwrap();
        std::fs::write(tmp.path().join("alpha.toml"), "").unwrap();
        std::fs::write(tmp.path().join("mid.toml"), "").unwrap();

        let manifest = scan_profiles(tmp.path()).unwrap();
        let names: Vec<_> = manifest.profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "mid", "zebra"]);
    }

    // ── resolve_profile_path tests ──

    #[test]
    fn resolve_profile_path_rejects_default() {
        let tmp = tempfile::TempDir::new().unwrap();
        let result = resolve_profile_path(tmp.path(), None, "default");
        assert!(result.is_err());
    }

    #[test]
    fn resolve_profile_path_without_manifest() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (canonical, path, rel) = resolve_profile_path(tmp.path(), None, "dev").unwrap();
        assert_eq!(canonical, "dev");
        assert_eq!(path, tmp.path().join("dev.toml"));
        assert_eq!(rel, "dev.toml");
    }

    #[test]
    fn resolve_profile_path_with_manifest_entry() {
        let tmp = tempfile::TempDir::new().unwrap();
        let manifest = ConfigProfileManifest {
            version: 1,
            profiles: vec![ConfigProfileManifestEntry {
                name: "prod".to_string(),
                path: "production.toml".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };

        let (canonical, path, rel) =
            resolve_profile_path(tmp.path(), Some(&manifest), "prod").unwrap();
        assert_eq!(canonical, "prod");
        assert_eq!(path, tmp.path().join("production.toml"));
        assert_eq!(rel, "production.toml");
    }

    #[test]
    fn resolve_profile_path_normalizes_name() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (canonical, _path, _rel) = resolve_profile_path(tmp.path(), None, "  DEV  ").unwrap();
        assert_eq!(canonical, "dev");
    }

    // ── list_profiles tests ──

    #[test]
    fn list_profiles_always_includes_default() {
        let tmp = tempfile::TempDir::new().unwrap();
        let profiles = list_profiles(tmp.path()).unwrap();
        assert!(!profiles.is_empty());
        assert_eq!(profiles[0].name, "default");
        assert!(profiles[0].implicit);
    }

    #[test]
    fn list_profiles_includes_scanned_profiles() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("dev.toml"), "# dev").unwrap();

        let profiles = list_profiles(tmp.path()).unwrap();
        assert!(profiles.len() >= 2);
        assert_eq!(profiles[0].name, "default");
        assert!(profiles.iter().any(|p| p.name == "dev"));
    }

    // ── resolve_profiles_dir tests ──

    #[test]
    fn resolve_profiles_dir_with_config_path() {
        let dir = resolve_profiles_dir(Some(Path::new("/etc/ft/ft.toml")));
        assert_eq!(dir, PathBuf::from("/etc/ft/profiles"));
    }

    #[test]
    fn resolve_profiles_dir_with_bare_config_path() {
        let dir = resolve_profiles_dir(Some(Path::new("ft.toml")));
        // "ft.toml" parent is "" which is empty, so unwrap_or uses "."
        // Path::new(".").join("profiles") = "profiles" (no leading ./)
        assert!(dir.ends_with("profiles"));
    }

    // ── touch_last_applied edge cases ──

    #[test]
    fn touch_last_applied_multiple_entries_updates_correct_one() {
        let mut manifest = ConfigProfileManifest {
            version: 1,
            profiles: vec![
                ConfigProfileManifestEntry {
                    name: "dev".to_string(),
                    path: "dev.toml".to_string(),
                    created_at: Some(100),
                    updated_at: Some(100),
                    last_applied_at: Some(100),
                    ..Default::default()
                },
                ConfigProfileManifestEntry {
                    name: "prod".to_string(),
                    path: "prod.toml".to_string(),
                    created_at: Some(200),
                    updated_at: Some(200),
                    last_applied_at: Some(200),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        touch_last_applied(&mut manifest, "prod", "prod.toml", 999);

        assert_eq!(manifest.profiles[0].last_applied_at, Some(100)); // dev unchanged
        assert_eq!(manifest.profiles[1].last_applied_at, Some(999)); // prod updated
        assert_eq!(manifest.last_applied_profile.as_deref(), Some("prod"));
    }
}
