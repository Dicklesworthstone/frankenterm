//! Config profile management (wa.toml overlays).

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
        .join("wa")
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
        description: Some("Base wa.toml config".to_string()),
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
}
