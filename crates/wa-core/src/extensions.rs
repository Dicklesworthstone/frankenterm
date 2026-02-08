//! Extension management for wa pattern packs.
//!
//! Provides listing, installation, removal, and validation of pattern pack
//! extensions. Extensions are pattern packs installed as files alongside the
//! built-in packs.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::config::PatternsConfig;
use crate::patterns::{PatternPack, RuleDef};

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
                            || (rel_stem.is_some() && e.path.as_deref() == rel_stem.as_deref())
                    });
                    if already_listed {
                        continue;
                    }

                    let active = config.packs.contains(&file_id)
                        || rel_id.as_ref().is_some_and(|id| config.packs.contains(id));

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

    let pack = load_pack_safe(&pack_id, config_root)
        .map_err(|_| crate::Error::Runtime(format!("extension '{name}' not found")))?;

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
        result
            .errors
            .push(format!("file not found: {}", path.display()));
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
pub fn install_extension(source_path: &Path, config_path: Option<&Path>) -> Result<String> {
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

    crate::config::resolve_config_path(None).and_then(|p| p.parent().map(|d| d.join("extensions")))
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
// =============================================================================
// Extension Sandboxing (wa-fno.4)
// =============================================================================
//
// When extensions evolve from declarative pattern packs to executable code
// (WASM modules), they need a capability-based sandbox to prevent arbitrary
// system access. This module defines:
//
// - Capability levels (read-only → full access)
// - Fine-grained permission flags
// - Extension manifest with capability declarations
// - Policy enforcement (check/deny pattern)
//
// The actual WASM runtime (wasmtime) is a future addition; these types
// provide the security framework that any runtime must integrate with.

/// Fine-grained capabilities that an extension can request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxCapabilities {
    /// Read captured pane output (pattern matching).
    pub read_pane_output: bool,
    /// Send desktop/webhook notifications.
    pub send_notifications: bool,
    /// Make outbound HTTP requests (to declared hosts only).
    pub http_requests: bool,
    /// File system access scope.
    pub file_access: FileAccessScope,
    /// Invoke wa workflows.
    pub invoke_workflows: bool,
    /// Send text to panes (requires explicit approval).
    pub send_text: bool,
}

impl Default for SandboxCapabilities {
    fn default() -> Self {
        Self {
            read_pane_output: true,
            send_notifications: false,
            http_requests: false,
            file_access: FileAccessScope::None,
            invoke_workflows: false,
            send_text: false,
        }
    }
}

/// File system access scope for sandboxed extensions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileAccessScope {
    /// No file system access.
    None,
    /// Read-only access to extension's own data directory.
    OwnDataReadOnly,
    /// Read-write access to extension's own data directory.
    OwnDataReadWrite,
    /// Read-only access to wa config directory.
    ConfigReadOnly,
}

/// Predefined capability levels for common extension use cases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityLevel {
    /// Level 1: Read-only (pattern matching only).
    ReadOnly,
    /// Level 2: Read + notify (most detection workflows).
    ReadNotify,
    /// Level 3: Read + notify + HTTP (external integrations).
    Integration,
    /// Level 4: Full access (admin/privileged extensions).
    Full,
}

impl CapabilityLevel {
    /// Convert a capability level to its concrete capabilities.
    #[must_use]
    pub fn to_capabilities(self) -> SandboxCapabilities {
        match self {
            Self::ReadOnly => SandboxCapabilities {
                read_pane_output: true,
                ..SandboxCapabilities::default()
            },
            Self::ReadNotify => SandboxCapabilities {
                read_pane_output: true,
                send_notifications: true,
                file_access: FileAccessScope::OwnDataReadWrite,
                ..SandboxCapabilities::default()
            },
            Self::Integration => SandboxCapabilities {
                read_pane_output: true,
                send_notifications: true,
                http_requests: true,
                file_access: FileAccessScope::OwnDataReadWrite,
                invoke_workflows: true,
                send_text: false,
            },
            Self::Full => SandboxCapabilities {
                read_pane_output: true,
                send_notifications: true,
                http_requests: true,
                file_access: FileAccessScope::ConfigReadOnly,
                invoke_workflows: true,
                send_text: true,
            },
        }
    }
}

/// Manifest for a sandboxed (WASM) extension.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionManifest {
    /// Extension name (unique identifier).
    pub name: String,
    /// Semantic version.
    pub version: String,
    /// Requested capabilities.
    pub capabilities: SandboxCapabilities,
    /// Optional: predefined level (overrides individual capabilities if set).
    pub capability_level: Option<CapabilityLevel>,
    /// Allowed HTTP hosts (only relevant if `http_requests` is true).
    pub allowed_hosts: Vec<String>,
    /// Maximum memory in bytes the extension can use.
    pub max_memory_bytes: u64,
    /// Maximum execution time per invocation.
    pub max_execution_ms: u64,
}

impl Default for ExtensionManifest {
    fn default() -> Self {
        Self {
            name: String::new(),
            version: "0.0.0".to_string(),
            capabilities: SandboxCapabilities::default(),
            capability_level: None,
            allowed_hosts: Vec::new(),
            max_memory_bytes: 16 * 1024 * 1024, // 16 MiB
            max_execution_ms: 5000,             // 5 seconds
        }
    }
}

impl ExtensionManifest {
    /// Resolve effective capabilities, preferring `capability_level` if set.
    #[must_use]
    pub fn effective_capabilities(&self) -> SandboxCapabilities {
        if let Some(level) = self.capability_level {
            level.to_capabilities()
        } else {
            self.capabilities.clone()
        }
    }
}

/// A violation detected when an extension exceeds its declared capabilities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxViolation {
    /// Extension that triggered the violation.
    pub extension_name: String,
    /// The capability that was requested but not granted.
    pub capability: String,
    /// Human-readable description.
    pub message: String,
}

impl std::fmt::Display for SandboxViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "sandbox violation [{}]: {} — {}",
            self.extension_name, self.capability, self.message
        )
    }
}

impl std::error::Error for SandboxViolation {}

/// Policy enforcer for extension sandbox capabilities.
///
/// Given a manifest, checks whether a specific operation is allowed.
pub struct SandboxPolicy {
    caps: SandboxCapabilities,
    allowed_hosts: Vec<String>,
    extension_name: String,
}

impl SandboxPolicy {
    /// Create a policy enforcer from a manifest.
    #[must_use]
    pub fn from_manifest(manifest: &ExtensionManifest) -> Self {
        Self {
            caps: manifest.effective_capabilities(),
            allowed_hosts: manifest.allowed_hosts.clone(),
            extension_name: manifest.name.clone(),
        }
    }

    /// Check if reading pane output is allowed.
    pub fn check_read_pane_output(&self) -> std::result::Result<(), SandboxViolation> {
        if self.caps.read_pane_output {
            Ok(())
        } else {
            Err(self.violation("read_pane_output", "pane output read not permitted"))
        }
    }

    /// Check if sending notifications is allowed.
    pub fn check_send_notification(&self) -> std::result::Result<(), SandboxViolation> {
        if self.caps.send_notifications {
            Ok(())
        } else {
            Err(self.violation("send_notifications", "notification sending not permitted"))
        }
    }

    /// Check if an HTTP request to a specific host is allowed.
    pub fn check_http_request(&self, host: &str) -> std::result::Result<(), SandboxViolation> {
        if !self.caps.http_requests {
            return Err(self.violation("http_requests", "HTTP requests not permitted"));
        }
        if self.allowed_hosts.is_empty() {
            return Ok(()); // No host restriction
        }
        if self.allowed_hosts.iter().any(|h| h == host) {
            Ok(())
        } else {
            Err(self.violation(
                "http_requests",
                &format!("host {host} not in allowed_hosts"),
            ))
        }
    }

    /// Check if file access at the given scope is allowed.
    pub fn check_file_access(
        &self,
        requested: &FileAccessScope,
    ) -> std::result::Result<(), SandboxViolation> {
        let allowed = match (&self.caps.file_access, requested) {
            (_, FileAccessScope::None) => true,
            (FileAccessScope::None, _) => false,
            (FileAccessScope::OwnDataReadOnly, FileAccessScope::OwnDataReadOnly) => true,
            (FileAccessScope::OwnDataReadWrite, FileAccessScope::OwnDataReadOnly) => true,
            (FileAccessScope::OwnDataReadWrite, FileAccessScope::OwnDataReadWrite) => true,
            (FileAccessScope::ConfigReadOnly, _) => true,
            _ => false,
        };
        if allowed {
            Ok(())
        } else {
            Err(self.violation(
                "file_access",
                &format!(
                    "requested {requested:?} exceeds granted {:?}",
                    self.caps.file_access
                ),
            ))
        }
    }

    /// Check if invoking workflows is allowed.
    pub fn check_invoke_workflow(&self) -> std::result::Result<(), SandboxViolation> {
        if self.caps.invoke_workflows {
            Ok(())
        } else {
            Err(self.violation("invoke_workflows", "workflow invocation not permitted"))
        }
    }

    /// Check if sending text to panes is allowed.
    pub fn check_send_text(&self) -> std::result::Result<(), SandboxViolation> {
        if self.caps.send_text {
            Ok(())
        } else {
            Err(self.violation("send_text", "sending text to panes not permitted"))
        }
    }

    fn violation(&self, capability: &str, message: &str) -> SandboxViolation {
        SandboxViolation {
            extension_name: self.extension_name.clone(),
            capability: capability.to_string(),
            message: message.to_string(),
        }
    }
}

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

        let removed = remove_extension("my-ext", &config, Some(&config_path)).unwrap();
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

    // -----------------------------------------------------------------------
    // Sandbox tests (wa-fno.4)
    // -----------------------------------------------------------------------

    #[test]
    fn sandbox_capabilities_default_is_read_only() {
        let caps = SandboxCapabilities::default();
        assert!(caps.read_pane_output);
        assert!(!caps.send_notifications);
        assert!(!caps.http_requests);
        assert_eq!(caps.file_access, FileAccessScope::None);
        assert!(!caps.invoke_workflows);
        assert!(!caps.send_text);
    }

    #[test]
    fn capability_level_read_only() {
        let caps = CapabilityLevel::ReadOnly.to_capabilities();
        assert!(caps.read_pane_output);
        assert!(!caps.send_notifications);
        assert!(!caps.http_requests);
        assert_eq!(caps.file_access, FileAccessScope::None);
    }

    #[test]
    fn capability_level_read_notify() {
        let caps = CapabilityLevel::ReadNotify.to_capabilities();
        assert!(caps.read_pane_output);
        assert!(caps.send_notifications);
        assert!(!caps.http_requests);
        assert_eq!(caps.file_access, FileAccessScope::OwnDataReadWrite);
    }

    #[test]
    fn capability_level_integration() {
        let caps = CapabilityLevel::Integration.to_capabilities();
        assert!(caps.read_pane_output);
        assert!(caps.send_notifications);
        assert!(caps.http_requests);
        assert!(caps.invoke_workflows);
        assert!(!caps.send_text);
    }

    #[test]
    fn capability_level_full() {
        let caps = CapabilityLevel::Full.to_capabilities();
        assert!(caps.read_pane_output);
        assert!(caps.send_notifications);
        assert!(caps.http_requests);
        assert!(caps.invoke_workflows);
        assert!(caps.send_text);
        assert_eq!(caps.file_access, FileAccessScope::ConfigReadOnly);
    }

    #[test]
    fn manifest_effective_capabilities_uses_level_when_set() {
        let manifest = ExtensionManifest {
            name: "test-ext".to_string(),
            capability_level: Some(CapabilityLevel::Full),
            capabilities: SandboxCapabilities::default(), // Should be ignored
            ..ExtensionManifest::default()
        };
        let caps = manifest.effective_capabilities();
        assert!(
            caps.send_text,
            "level should override individual capabilities"
        );
    }

    #[test]
    fn manifest_effective_capabilities_uses_individual_when_no_level() {
        let manifest = ExtensionManifest {
            name: "test-ext".to_string(),
            capability_level: None,
            capabilities: SandboxCapabilities {
                send_notifications: true,
                ..SandboxCapabilities::default()
            },
            ..ExtensionManifest::default()
        };
        let caps = manifest.effective_capabilities();
        assert!(caps.send_notifications);
        assert!(!caps.http_requests);
    }

    #[test]
    fn policy_read_only_allows_pane_read() {
        let manifest = ExtensionManifest {
            name: "reader".to_string(),
            capability_level: Some(CapabilityLevel::ReadOnly),
            ..ExtensionManifest::default()
        };
        let policy = SandboxPolicy::from_manifest(&manifest);
        assert!(policy.check_read_pane_output().is_ok());
    }

    #[test]
    fn policy_read_only_denies_notifications() {
        let manifest = ExtensionManifest {
            name: "reader".to_string(),
            capability_level: Some(CapabilityLevel::ReadOnly),
            ..ExtensionManifest::default()
        };
        let policy = SandboxPolicy::from_manifest(&manifest);
        let err = policy.check_send_notification().unwrap_err();
        assert_eq!(err.extension_name, "reader");
        assert_eq!(err.capability, "send_notifications");
    }

    #[test]
    fn policy_read_only_denies_http() {
        let manifest = ExtensionManifest {
            name: "reader".to_string(),
            capability_level: Some(CapabilityLevel::ReadOnly),
            ..ExtensionManifest::default()
        };
        let policy = SandboxPolicy::from_manifest(&manifest);
        assert!(policy.check_http_request("example.com").is_err());
    }

    #[test]
    fn policy_read_only_denies_send_text() {
        let manifest = ExtensionManifest {
            name: "reader".to_string(),
            capability_level: Some(CapabilityLevel::ReadOnly),
            ..ExtensionManifest::default()
        };
        let policy = SandboxPolicy::from_manifest(&manifest);
        assert!(policy.check_send_text().is_err());
    }

    #[test]
    fn policy_read_only_denies_workflow() {
        let manifest = ExtensionManifest {
            name: "reader".to_string(),
            capability_level: Some(CapabilityLevel::ReadOnly),
            ..ExtensionManifest::default()
        };
        let policy = SandboxPolicy::from_manifest(&manifest);
        assert!(policy.check_invoke_workflow().is_err());
    }

    #[test]
    fn policy_integration_allows_http_to_declared_host() {
        let manifest = ExtensionManifest {
            name: "slack-int".to_string(),
            capability_level: Some(CapabilityLevel::Integration),
            allowed_hosts: vec!["hooks.slack.com".to_string()],
            ..ExtensionManifest::default()
        };
        let policy = SandboxPolicy::from_manifest(&manifest);
        assert!(policy.check_http_request("hooks.slack.com").is_ok());
    }

    #[test]
    fn policy_integration_denies_http_to_undeclared_host() {
        let manifest = ExtensionManifest {
            name: "slack-int".to_string(),
            capability_level: Some(CapabilityLevel::Integration),
            allowed_hosts: vec!["hooks.slack.com".to_string()],
            ..ExtensionManifest::default()
        };
        let policy = SandboxPolicy::from_manifest(&manifest);
        let err = policy.check_http_request("evil.com").unwrap_err();
        assert!(err.message.contains("evil.com"));
        assert!(err.message.contains("allowed_hosts"));
    }

    #[test]
    fn policy_no_host_restrictions_allows_any() {
        let manifest = ExtensionManifest {
            name: "open-ext".to_string(),
            capability_level: Some(CapabilityLevel::Integration),
            allowed_hosts: vec![], // No restrictions
            ..ExtensionManifest::default()
        };
        let policy = SandboxPolicy::from_manifest(&manifest);
        assert!(policy.check_http_request("any-host.example.com").is_ok());
    }

    #[test]
    fn policy_full_allows_send_text() {
        let manifest = ExtensionManifest {
            name: "admin".to_string(),
            capability_level: Some(CapabilityLevel::Full),
            ..ExtensionManifest::default()
        };
        let policy = SandboxPolicy::from_manifest(&manifest);
        assert!(policy.check_send_text().is_ok());
    }

    #[test]
    fn policy_file_access_none_denied_for_read() {
        let manifest = ExtensionManifest {
            name: "no-fs".to_string(),
            capability_level: Some(CapabilityLevel::ReadOnly),
            ..ExtensionManifest::default()
        };
        let policy = SandboxPolicy::from_manifest(&manifest);
        let err = policy
            .check_file_access(&FileAccessScope::OwnDataReadOnly)
            .unwrap_err();
        assert_eq!(err.capability, "file_access");
    }

    #[test]
    fn policy_file_access_read_write_allows_read_only() {
        let manifest = ExtensionManifest {
            name: "data-ext".to_string(),
            capability_level: Some(CapabilityLevel::ReadNotify),
            ..ExtensionManifest::default()
        };
        let policy = SandboxPolicy::from_manifest(&manifest);
        assert!(
            policy
                .check_file_access(&FileAccessScope::OwnDataReadOnly)
                .is_ok()
        );
    }

    #[test]
    fn policy_file_access_read_only_denies_write() {
        let manifest = ExtensionManifest {
            name: "ro-ext".to_string(),
            capabilities: SandboxCapabilities {
                file_access: FileAccessScope::OwnDataReadOnly,
                ..SandboxCapabilities::default()
            },
            ..ExtensionManifest::default()
        };
        let policy = SandboxPolicy::from_manifest(&manifest);
        assert!(
            policy
                .check_file_access(&FileAccessScope::OwnDataReadWrite)
                .is_err()
        );
    }

    #[test]
    fn policy_file_access_none_always_allowed() {
        let manifest = ExtensionManifest {
            name: "minimal".to_string(),
            capability_level: Some(CapabilityLevel::ReadOnly),
            ..ExtensionManifest::default()
        };
        let policy = SandboxPolicy::from_manifest(&manifest);
        assert!(policy.check_file_access(&FileAccessScope::None).is_ok());
    }

    #[test]
    fn sandbox_violation_display() {
        let v = SandboxViolation {
            extension_name: "evil-ext".to_string(),
            capability: "send_text".to_string(),
            message: "not permitted".to_string(),
        };
        let s = v.to_string();
        assert!(s.contains("evil-ext"));
        assert!(s.contains("send_text"));
        assert!(s.contains("not permitted"));
    }

    #[test]
    fn sandbox_capabilities_serde_roundtrip() {
        let caps = CapabilityLevel::Integration.to_capabilities();
        let json = serde_json::to_string(&caps).unwrap();
        let deserialized: SandboxCapabilities = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, caps);
    }

    #[test]
    fn capability_level_serde_roundtrip() {
        for level in [
            CapabilityLevel::ReadOnly,
            CapabilityLevel::ReadNotify,
            CapabilityLevel::Integration,
            CapabilityLevel::Full,
        ] {
            let json = serde_json::to_string(&level).unwrap();
            let deserialized: CapabilityLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(deserialized, level);
        }
    }

    #[test]
    fn file_access_scope_serde_roundtrip() {
        for scope in [
            FileAccessScope::None,
            FileAccessScope::OwnDataReadOnly,
            FileAccessScope::OwnDataReadWrite,
            FileAccessScope::ConfigReadOnly,
        ] {
            let json = serde_json::to_string(&scope).unwrap();
            let deserialized: FileAccessScope = serde_json::from_str(&json).unwrap();
            assert_eq!(deserialized, scope);
        }
    }

    #[test]
    fn extension_manifest_serde_roundtrip() {
        let manifest = ExtensionManifest {
            name: "my-ext".to_string(),
            version: "1.2.3".to_string(),
            capability_level: Some(CapabilityLevel::Integration),
            allowed_hosts: vec!["api.example.com".to_string()],
            max_memory_bytes: 32 * 1024 * 1024,
            max_execution_ms: 10000,
            ..ExtensionManifest::default()
        };
        let json = serde_json::to_string(&manifest).unwrap();
        let deserialized: ExtensionManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.name, "my-ext");
        assert_eq!(deserialized.version, "1.2.3");
        assert_eq!(
            deserialized.capability_level,
            Some(CapabilityLevel::Integration)
        );
        assert_eq!(deserialized.allowed_hosts, vec!["api.example.com"]);
        assert_eq!(deserialized.max_memory_bytes, 32 * 1024 * 1024);
    }

    #[test]
    fn manifest_default_resource_limits() {
        let manifest = ExtensionManifest::default();
        assert_eq!(manifest.max_memory_bytes, 16 * 1024 * 1024);
        assert_eq!(manifest.max_execution_ms, 5000);
    }

    #[test]
    fn sandbox_violation_serde_roundtrip() {
        let v = SandboxViolation {
            extension_name: "test".to_string(),
            capability: "http_requests".to_string(),
            message: "host not allowed".to_string(),
        };
        let json = serde_json::to_string(&v).unwrap();
        let deserialized: SandboxViolation = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, v);
    }

    #[test]
    fn policy_config_read_only_allows_all_lower_scopes() {
        let manifest = ExtensionManifest {
            name: "admin".to_string(),
            capability_level: Some(CapabilityLevel::Full),
            ..ExtensionManifest::default()
        };
        let policy = SandboxPolicy::from_manifest(&manifest);
        assert!(policy.check_file_access(&FileAccessScope::None).is_ok());
        assert!(
            policy
                .check_file_access(&FileAccessScope::OwnDataReadOnly)
                .is_ok()
        );
        assert!(
            policy
                .check_file_access(&FileAccessScope::OwnDataReadWrite)
                .is_ok()
        );
        assert!(
            policy
                .check_file_access(&FileAccessScope::ConfigReadOnly)
                .is_ok()
        );
    }
}
