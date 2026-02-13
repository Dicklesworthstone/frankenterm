//! Extension manifest (extension.toml) parsing.
//!
//! Defines the [`ExtensionPermissions`] and [`ParsedManifest`] types
//! that govern what a WASM extension is allowed to do.

use anyhow::{Context, Result};
use std::path::Path;

/// Permissions declared by a WASM extension in its manifest.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExtensionPermissions {
    /// Filesystem paths the extension may read from.
    pub filesystem_read: Vec<String>,
    /// Filesystem paths the extension may write to.
    pub filesystem_write: Vec<String>,
    /// Environment variable patterns the extension may access.
    /// Supports trailing glob (`FRANKENTERM_*`).
    pub environment: Vec<String>,
    /// Whether the extension may open network connections.
    pub network: bool,
    /// Whether the extension may read pane content.
    pub pane_access: bool,
}

impl ExtensionPermissions {
    /// Check whether the given env var name is permitted.
    pub fn allows_env_var(&self, name: &str) -> bool {
        self.environment.iter().any(|pattern| {
            if let Some(prefix) = pattern.strip_suffix('*') {
                name.starts_with(prefix)
            } else {
                name == pattern
            }
        })
    }

    /// Check whether the given path is permitted for reading.
    pub fn allows_read(&self, path: &str) -> bool {
        self.filesystem_read
            .iter()
            .any(|allowed| path.starts_with(allowed.as_str()))
    }

    /// Check whether the given path is permitted for writing.
    pub fn allows_write(&self, path: &str) -> bool {
        self.filesystem_write
            .iter()
            .any(|allowed| path.starts_with(allowed.as_str()))
    }
}

/// Parsed extension manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedManifest {
    pub name: String,
    pub version: String,
    pub description: String,
    pub authors: Vec<String>,
    pub license: Option<String>,
    pub min_frankenterm_version: Option<String>,
    pub permissions: ExtensionPermissions,
    pub hooks: Vec<(String, String)>,
}

impl ParsedManifest {
    /// Parse an `extension.toml` file.
    pub fn from_toml_str(toml_str: &str) -> Result<Self> {
        let doc: toml::Value = toml_str.parse().context("failed to parse extension.toml")?;

        let ext = doc.get("extension").context("missing [extension] table")?;

        let name = ext
            .get("name")
            .and_then(|v| v.as_str())
            .context("[extension].name is required")?
            .to_string();

        let version = ext
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0")
            .to_string();

        let description = ext
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let authors = ext
            .get("authors")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let license = ext
            .get("license")
            .and_then(|v| v.as_str())
            .map(String::from);

        let min_frankenterm_version = ext
            .get("min_frankenterm_version")
            .and_then(|v| v.as_str())
            .map(String::from);

        // Parse permissions
        let perms = doc.get("permissions");
        let permissions = parse_permissions(perms);

        // Parse hooks
        let hooks = doc
            .get("hooks")
            .and_then(|v| v.as_table())
            .map(|table| {
                table
                    .iter()
                    .map(|(event, handler)| {
                        (event.clone(), handler.as_str().unwrap_or("").to_string())
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(Self {
            name,
            version,
            description,
            authors,
            license,
            min_frankenterm_version,
            permissions,
            hooks,
        })
    }

    /// Parse a manifest from a file path.
    pub fn from_file(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        Self::from_toml_str(&content)
    }
}

fn parse_permissions(perms: Option<&toml::Value>) -> ExtensionPermissions {
    let Some(perms) = perms else {
        return ExtensionPermissions::default();
    };

    let string_array = |key: &str| -> Vec<String> {
        perms
            .get(key)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };

    let filesystem = string_array("filesystem");
    let (mut filesystem_read, mut filesystem_write) = (vec![], vec![]);
    for entry in &filesystem {
        if let Some(path) = entry.strip_prefix("read:") {
            filesystem_read.push(path.to_string());
        } else if let Some(path) = entry.strip_prefix("write:") {
            filesystem_write.push(path.to_string());
        } else {
            // Bare path = read-only
            filesystem_read.push(entry.clone());
        }
    }

    ExtensionPermissions {
        filesystem_read,
        filesystem_write,
        environment: string_array("environment"),
        network: perms
            .get("network")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        pane_access: perms
            .get("pane_access")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_MANIFEST: &str = r#"
[extension]
name = "my-theme"
version = "1.0.0"
description = "A beautiful theme"
authors = ["Author <author@example.com>"]
license = "MIT"
min_frankenterm_version = "0.2.0"

[permissions]
filesystem = ["read:~/.config/frankenterm/", "write:~/.local/share/frankenterm/"]
environment = ["TERM", "COLORTERM", "FRANKENTERM_*"]
network = false
pane_access = false

[hooks]
on_config_reload = "handle_config_reload"
on_pane_focus = "handle_pane_focus"
"#;

    #[test]
    fn parse_full_manifest() {
        let manifest = ParsedManifest::from_toml_str(FULL_MANIFEST).unwrap();
        assert_eq!(manifest.name, "my-theme");
        assert_eq!(manifest.version, "1.0.0");
        assert_eq!(manifest.description, "A beautiful theme");
        assert_eq!(manifest.authors.len(), 1);
        assert_eq!(manifest.license.as_deref(), Some("MIT"));
        assert_eq!(manifest.min_frankenterm_version.as_deref(), Some("0.2.0"));
        assert!(!manifest.permissions.network);
        assert!(!manifest.permissions.pane_access);
        assert_eq!(manifest.permissions.filesystem_read.len(), 1);
        assert_eq!(manifest.permissions.filesystem_write.len(), 1);
        assert_eq!(manifest.hooks.len(), 2);
    }

    #[test]
    fn minimal_manifest() {
        let toml = r#"
[extension]
name = "minimal"
"#;
        let manifest = ParsedManifest::from_toml_str(toml).unwrap();
        assert_eq!(manifest.name, "minimal");
        assert_eq!(manifest.version, "0.0.0");
        assert!(manifest.permissions.filesystem_read.is_empty());
        assert!(!manifest.permissions.network);
    }

    #[test]
    fn missing_extension_table_errors() {
        let toml = r#"
[other]
key = "value"
"#;
        let result = ParsedManifest::from_toml_str(toml);
        assert!(result.is_err());
    }

    #[test]
    fn env_var_glob_matching() {
        let perms = ExtensionPermissions {
            environment: vec!["TERM".to_string(), "FRANKENTERM_*".to_string()],
            ..Default::default()
        };
        assert!(perms.allows_env_var("TERM"));
        assert!(perms.allows_env_var("FRANKENTERM_CONFIG"));
        assert!(perms.allows_env_var("FRANKENTERM_"));
        assert!(!perms.allows_env_var("HOME"));
        assert!(!perms.allows_env_var("TERMINAL"));
    }

    #[test]
    fn filesystem_permission_checks() {
        let perms = ExtensionPermissions {
            filesystem_read: vec!["~/.config/frankenterm/".to_string()],
            filesystem_write: vec!["~/.local/share/frankenterm/".to_string()],
            ..Default::default()
        };
        assert!(perms.allows_read("~/.config/frankenterm/theme.toml"));
        assert!(!perms.allows_read("~/.ssh/id_rsa"));
        assert!(perms.allows_write("~/.local/share/frankenterm/cache.db"));
        assert!(!perms.allows_write("~/.config/frankenterm/theme.toml"));
    }
}
