//! Extension management for wa pattern packs.
//!
//! Provides listing, installation, removal, and validation of pattern pack
//! extensions. Extensions are pattern packs installed as files alongside the
//! built-in packs.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::PatternsConfig;
use crate::patterns::{PatternPack, RuleDef};
use crate::Result;

// ---------------------------------------------------------------------------
// Extension info
// ---------------------------------------------------------------------------

/// Source of an extension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionSource {
    Builtin,
    File,
}

/// Summary information about an installed extension.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionInfo {
    pub name: String,
    pub version: String,
    pub source: ExtensionSource,
    pub rule_count: usize,
    pub path: Option<String>,
    pub active: bool,
}

/// Detailed information about an extension (including rule list).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionDetail {
    pub name: String,
    pub version: String,
    pub source: ExtensionSource,
    pub path: Option<String>,
    pub rules: Vec<ExtensionRuleInfo>,
}

/// Summary of a single rule within an extension.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionRuleInfo {
    pub id: String,
    pub agent_type: String,
    pub event_type: String,
    pub severity: String,
    pub description: String,
}

impl From<&RuleDef> for ExtensionRuleInfo {
    fn from(rule: &RuleDef) -> Self {
        Self {
            id: rule.id.clone(),
            agent_type: format!("{}", rule.agent_type),
            event_type: rule.event_type.clone(),
            severity: format!("{:?}", rule.severity).to_lowercase(),
            description: rule.description.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Validation result
// ---------------------------------------------------------------------------

/// Result of validating an extension file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationResult {
    pub valid: bool,
    pub pack_name: Option<String>,
    pub version: Option<String>,
    pub rule_count: usize,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// Extensions directory
// ---------------------------------------------------------------------------

/// Resolve the extensions directory (alongside config dir).
pub fn resolve_extensions_dir(config_path: Option<&Path>) -> PathBuf {
    if let Some(path) = config_path {
        return path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("extensions");
    }

    if let Some(path) = crate::config::resolve_config_path(None) {
        if let Some(parent) = path.parent() {
            return parent.join("extensions");
        }
    }

    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("wa")
        .join("extensions")
}

// ---------------------------------------------------------------------------
// List extensions
// ---------------------------------------------------------------------------

/// List all extensions (built-in + file-based from config).
pub fn list_extensions(
    config: &PatternsConfig,
    config_root: Option<&Path>,
) -> Result<Vec<ExtensionInfo>> {
    let mut extensions = Vec::new();

    // Built-in packs.
    let builtin_names = ["core", "codex", "claude_code", "gemini", "wezterm"];
    for name in &builtin_names {
        let pack_id = format!("builtin:{name}");
        let active = config.packs.contains(&pack_id);

        // Load to get version and rule count.
        if let Ok(pack) = load_pack_safe(&pack_id, config_root) {
            extensions.push(ExtensionInfo {
                name: pack.name.clone(),
                version: pack.version.clone(),
                source: ExtensionSource::Builtin,
                rule_count: pack.rules.len(),
                path: None,
                active,
            });
        }
    }

    // File-based packs from config.
    for pack_id in &config.packs {
        if pack_id.starts_with("file:") {
            if let Ok(pack) = load_pack_safe(pack_id, config_root) {
                let path_str = pack_id.strip_prefix("file:").unwrap_or(pack_id);
                extensions.push(ExtensionInfo {
                    name: pack.name.clone(),
                    version: pack.version.clone(),
                    source: ExtensionSource::File,
                    rule_count: pack.rules.len(),
                    path: Some(path_str.to_string()),
                    active: true,
                });
            }
        }
    }

    // Scan extensions directory for installed but possibly inactive extensions.
    if let Some(ext_dir) = find_extensions_dir(config_root) {
        if ext_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&ext_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let ext = path
                        .extension()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_lowercase();

                    if !matches!(ext.as_str(), "toml" | "yaml" | "yml" | "json") {
                        continue;
                    }

                    let file_id = format!("file:{}", path.display());
                    let rel_id = path
                        .strip_prefix(
                            config_root
                                .and_then(|p| p.parent())
                                .unwrap_or_else(|| Path::new(".")),
                        )
                        .ok()
                        .map(|p| format!("file:{}", p.display()));

                    // Skip if already listed.
                    let path_str = path.display().to_string();
                    let rel_stem = rel_id
                        .as_ref()
                        .map(|s| s.strip_prefix("file:").unwrap_or(s).to_string());
                    let already_listed = extensions.iter().any(|e| {
                        e.path.as_deref() == Some(path_str.as_str())
                            || (rel_stem.is_some()
                                && e.path.as_deref() == rel_stem.as_deref())
                    });
                    if already_listed {
                        continue;
                    }

                    let active = config.packs.contains(&file_id)
                        || rel_id
                            .as_ref()
                            .is_some_and(|id| config.packs.contains(id));

                    if let Ok(pack) = load_pack_safe(&file_id, None) {
                        extensions.push(ExtensionInfo {
                            name: pack.name.clone(),
                            version: pack.version.clone(),
                            source: ExtensionSource::File,
                            rule_count: pack.rules.len(),
                            path: Some(path.display().to_string()),
                            active,
                        });
                    }
                }
            }
        }
    }

    Ok(extensions)
}

// ---------------------------------------------------------------------------
// Extension info (detail)
// ---------------------------------------------------------------------------

/// Get detailed information about a specific extension.
pub fn extension_info(
    name: &str,
    config: &PatternsConfig,
    config_root: Option<&Path>,
) -> Result<ExtensionDetail> {
    // Try as builtin.
    let pack_id = if name.contains(':') {
        name.to_string()
    } else if let Some(pack) = try_resolve_name(name, config, config_root) {
        pack
    } else {
        // Default: try builtin, then file.
        format!("builtin:{name}")
    };

    let pack = load_pack_safe(&pack_id, config_root).map_err(|_| {
        crate::Error::Runtime(format!("extension '{name}' not found"))
    })?;

    let source = if pack_id.starts_with("builtin:") {
        ExtensionSource::Builtin
    } else {
        ExtensionSource::File
    };

    let path = if pack_id.starts_with("file:") {
        Some(
            pack_id
                .strip_prefix("file:")
                .unwrap_or(&pack_id)
                .to_string(),
        )
    } else {
        None
    };

    Ok(ExtensionDetail {
        name: pack.name.clone(),
        version: pack.version.clone(),
        source,
        path,
        rules: pack.rules.iter().map(ExtensionRuleInfo::from).collect(),
    })
}

// ---------------------------------------------------------------------------
// Validate
// ---------------------------------------------------------------------------

/// Validate an extension file without installing it.
pub fn validate_extension(path: &Path) -> ValidationResult {
    let mut result = ValidationResult {
        valid: false,
        pack_name: None,
        version: None,
        rule_count: 0,
        errors: Vec::new(),
        warnings: Vec::new(),
    };

    // Check file exists.
    if !path.exists() {
        result.errors.push(format!("file not found: {}", path.display()));
        return result;
    }

    // Check extension.
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();

    if !matches!(ext.as_str(), "toml" | "yaml" | "yml" | "json") {
        result.errors.push(format!(
            "unsupported file extension '.{ext}' (expected .toml, .yaml, .yml, .json)"
        ));
        return result;
    }

    // Try to load as a pack.
    let pack_id = format!("file:{}", path.display());
    match load_pack_safe(&pack_id, None) {
        Ok(pack) => {
            // Strip file: prefix from name if present (load_pack_safe rewrites it).
            let display_name = pack
                .name
                .strip_prefix("file:")
                .and_then(|p| {
                    Path::new(p)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .map(String::from)
                })
                .unwrap_or_else(|| pack.name.clone());
            result.pack_name = Some(display_name);
            result.version = Some(pack.version.clone());
            result.rule_count = pack.rules.len();

            if pack.name.trim().is_empty() {
                result.errors.push("pack name is empty".to_string());
            }
            if pack.version.trim().is_empty() {
                result.warnings.push("pack version is empty".to_string());
            }
            if pack.rules.is_empty() {
                result.warnings.push("pack contains no rules".to_string());
            }

            // Check for duplicate rule IDs.
            let mut seen_ids = std::collections::HashSet::new();
            for rule in &pack.rules {
                if !seen_ids.insert(&rule.id) {
                    result
                        .warnings
                        .push(format!("duplicate rule ID: {}", rule.id));
                }
            }

            result.valid = result.errors.is_empty();
        }
        Err(e) => {
            result.errors.push(format!("failed to parse: {e}"));
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

/// Install an extension from a local file path into the extensions directory.
///
/// Returns the pack ID that should be added to `config.patterns.packs`.
pub fn install_extension(
    source_path: &Path,
    config_path: Option<&Path>,
) -> Result<String> {
    // Validate first.
    let validation = validate_extension(source_path);
    if !validation.valid {
        return Err(crate::Error::Runtime(format!(
            "extension validation failed: {}",
            validation.errors.join("; ")
        )));
    }

    let ext_dir = resolve_extensions_dir(config_path);
    std::fs::create_dir_all(&ext_dir)?;

    let file_name = source_path
        .file_name()
        .ok_or_else(|| crate::Error::Runtime("source path has no filename".into()))?;
    let dest = ext_dir.join(file_name);

    // Don't overwrite without warning.
    if dest.exists() && dest.canonicalize().ok() != source_path.canonicalize().ok() {
        return Err(crate::Error::Runtime(format!(
            "extension already exists at {}; remove it first",
            dest.display()
        )));
    }

    // Copy file.
    if dest.canonicalize().ok() != source_path.canonicalize().ok() {
        std::fs::copy(source_path, &dest)?;
    }

    Ok(format!("file:{}", dest.display()))
}

// ---------------------------------------------------------------------------
// Remove
// ---------------------------------------------------------------------------

/// Remove an installed extension file.
///
/// Returns the pack ID that should be removed from config.
pub fn remove_extension(
    name: &str,
    config: &PatternsConfig,
    config_path: Option<&Path>,
) -> Result<Option<String>> {
    // Find the pack ID and file path.
    let ext_dir = resolve_extensions_dir(config_path);

    // Check if name matches a file-based pack in config.
    for pack_id in &config.packs {
        if !pack_id.starts_with("file:") {
            continue;
        }
        let file_path = pack_id.strip_prefix("file:").unwrap_or(pack_id);
        let path = PathBuf::from(file_path);
        let matches = path
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|stem| stem == name)
            || file_path == name
            || pack_id == name;

        if matches {
            // Only delete if it's in the extensions directory.
            let full_path = if path.is_absolute() {
                path.clone()
            } else {
                config_path
                    .and_then(|p| p.parent())
                    .unwrap_or_else(|| Path::new("."))
                    .join(&path)
            };

            if full_path.starts_with(&ext_dir) && full_path.exists() {
                std::fs::remove_file(&full_path)?;
            }

            return Ok(Some(pack_id.clone()));
        }
    }

    // Check extensions directory directly.
    if ext_dir.exists() {
        for ext in ["toml", "yaml", "yml", "json"] {
            let candidate = ext_dir.join(format!("{name}.{ext}"));
            if candidate.exists() {
                let pack_id = format!("file:{}", candidate.display());
                std::fs::remove_file(&candidate)?;
                return Ok(Some(pack_id));
            }
        }
    }

    Ok(None)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn load_pack_safe(pack_id: &str, root: Option<&Path>) -> Result<PatternPack> {
    use crate::patterns::PatternEngine;

    let config = PatternsConfig {
        packs: vec![pack_id.to_string()],
        ..Default::default()
    };
    let engine = PatternEngine::from_config_with_root(&config, root)?;
    let packs = engine.packs();
    packs
        .first()
        .cloned()
        .ok_or_else(|| crate::Error::Runtime(format!("pack '{pack_id}' not loadable")))
}

fn find_extensions_dir(config_root: Option<&Path>) -> Option<PathBuf> {
    if let Some(root) = config_root {
        return Some(
            root.parent()
                .unwrap_or_else(|| Path::new("."))
                .join("extensions"),
        );
    }

    crate::config::resolve_config_path(None)
        .and_then(|p| p.parent().map(|d| d.join("extensions")))
}

fn try_resolve_name(
    name: &str,
    config: &PatternsConfig,
    config_root: Option<&Path>,
) -> Option<String> {
    // Check builtins first.
    let builtin_id = format!("builtin:{name}");
    if load_pack_safe(&builtin_id, config_root).is_ok() {
        return Some(builtin_id);
    }

    // Check file-based packs in config.
    for pack_id in &config.packs {
        if !pack_id.starts_with("file:") {
            continue;
        }
        let file_path = pack_id.strip_prefix("file:").unwrap_or(pack_id);
        let path = PathBuf::from(file_path);
        if path.file_stem().and_then(|s| s.to_str()) == Some(name) {
            return Some(pack_id.clone());
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_extensions_includes_builtins() {
        let config = PatternsConfig::default();
        let exts = list_extensions(&config, None).unwrap();

        let builtin_names: Vec<_> = exts
            .iter()
            .filter(|e| e.source == ExtensionSource::Builtin)
            .map(|e| e.name.as_str())
            .collect();

        assert!(builtin_names.contains(&"builtin:core"));
        assert!(builtin_names.contains(&"builtin:codex"));
        assert!(builtin_names.contains(&"builtin:claude_code"));

        // All default builtins should be active.
        for ext in &exts {
            if ext.source == ExtensionSource::Builtin {
                assert!(ext.active, "builtin {} should be active", ext.name);
            }
        }
    }

    #[test]
    fn extension_info_builtin() {
        let config = PatternsConfig::default();
        let detail = extension_info("codex", &config, None).unwrap();

        assert_eq!(detail.source, ExtensionSource::Builtin);
        assert!(!detail.rules.is_empty());
    }

    #[test]
    fn extension_info_not_found() {
        let config = PatternsConfig::default();
        let result = extension_info("nonexistent_extension_xyz", &config, None);
        assert!(result.is_err());
    }

    #[test]
    fn validate_nonexistent_file() {
        let result = validate_extension(Path::new("/tmp/does_not_exist_xyz.toml"));
        assert!(!result.valid);
        assert!(!result.errors.is_empty());
    }

    #[test]
    fn validate_unsupported_extension() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "not a pack").unwrap();

        let result = validate_extension(&path);
        assert!(!result.valid);
        assert!(result.errors[0].contains("unsupported file extension"));
    }

    #[test]
    fn validate_valid_pack() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test-pack.toml");
        std::fs::write(
            &path,
            r#"
name = "test-pack"
version = "1.0.0"
[[rules]]
id = "codex.test_rule1"
agent_type = "codex"
event_type = "test.event"
severity = "info"
description = "A test rule"
anchors = ["test anchor"]
"#,
        )
        .unwrap();

        let result = validate_extension(&path);
        assert!(result.valid, "errors: {:?}", result.errors);
        assert_eq!(result.pack_name.as_deref(), Some("test-pack"));
        assert_eq!(result.rule_count, 1);
    }

    #[test]
    fn validate_invalid_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "this is not valid { toml").unwrap();

        let result = validate_extension(&path);
        assert!(!result.valid);
        assert!(!result.errors.is_empty());
    }

    #[test]
    fn install_and_remove_extension() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("my-ext.toml");
        std::fs::write(
            &source,
            r#"
name = "my-ext"
version = "0.1.0"
[[rules]]
id = "codex.my_custom_rule"
agent_type = "codex"
event_type = "my.event"
severity = "warning"
description = "My custom rule"
anchors = ["custom anchor"]
"#,
        )
        .unwrap();

        // Create a fake config path so extensions dir is within tempdir.
        let config_path = dir.path().join("wa.toml");
        std::fs::write(&config_path, "").unwrap();

        let pack_id = install_extension(&source, Some(&config_path)).unwrap();
        assert!(pack_id.starts_with("file:"));
        assert!(pack_id.contains("my-ext.toml"));

        // Verify the file was copied.
        let ext_dir = dir.path().join("extensions");
        assert!(ext_dir.join("my-ext.toml").exists());

        // Remove it.
        let mut config = PatternsConfig::default();
        config.packs.push(pack_id.clone());

        let removed =
            remove_extension("my-ext", &config, Some(&config_path)).unwrap();
        assert_eq!(removed, Some(pack_id));
        assert!(!ext_dir.join("my-ext.toml").exists());
    }

    #[test]
    fn extensions_dir_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("wa.toml");
        let ext_dir = resolve_extensions_dir(Some(&config_path));
        assert_eq!(ext_dir, dir.path().join("extensions"));
    }
}
