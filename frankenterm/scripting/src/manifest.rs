//! Extension manifest (extension.toml) parsing.
//!
//! Defines the [`ExtensionPermissions`] and [`ParsedManifest`] types
//! that govern what a WASM extension is allowed to do.

use anyhow::{Context, Result, bail};
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

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
        path_allowed_by_roots(path, &self.filesystem_read)
    }

    /// Check whether the given path is permitted for writing.
    ///
    /// Non-existent write targets are allowed when their normalized path stays
    /// under an allowed root. Existing absolute path prefixes are resolved to
    /// catch symlink escapes before the containment check.
    pub fn allows_write(&self, path: &str) -> bool {
        path_allowed_by_roots(path, &self.filesystem_write)
    }
}

/// Engine type for an extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineType {
    Wasm,
    Lua,
    Both,
}

impl EngineType {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "wasm" => Some(Self::Wasm),
            "lua" => Some(Self::Lua),
            "both" => Some(Self::Both),
            _ => None,
        }
    }

    /// Whether this engine type requires a WASM module.
    pub fn needs_wasm(self) -> bool {
        matches!(self, Self::Wasm | Self::Both)
    }

    /// Whether this engine type requires a Lua script.
    pub fn needs_lua(self) -> bool {
        matches!(self, Self::Lua | Self::Both)
    }
}

/// Engine configuration from the `[engine]` section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineConfig {
    pub engine_type: EngineType,
    pub entry: String,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            engine_type: EngineType::Wasm,
            entry: "main.wasm".to_string(),
        }
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
    pub homepage: Option<String>,
    pub min_frankenterm_version: Option<String>,
    pub engine: EngineConfig,
    pub permissions: ExtensionPermissions,
    pub hooks: Vec<(String, String)>,
    pub asset_themes: Vec<String>,
}

impl ParsedManifest {
    /// Parse an `extension.toml` file.
    pub fn from_toml_str(toml_str: &str) -> Result<Self> {
        let doc: toml::Table = toml_str.parse().context("failed to parse extension.toml")?;

        let ext = doc.get("extension").context("missing [extension] table")?;

        let name = ext
            .get("name")
            .and_then(|v| v.as_str())
            .context("[extension].name is required")?;
        validate_extension_name(name)?;
        let name = name.to_string();

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

        let homepage = ext
            .get("homepage")
            .and_then(|v| v.as_str())
            .map(String::from);

        let min_frankenterm_version = ext
            .get("min_frankenterm_version")
            .and_then(|v| v.as_str())
            .map(String::from);

        // Parse engine configuration
        let engine = parse_engine_config(doc.get("engine"));

        // Parse permissions
        let perms = doc.get("permissions");
        let permissions = parse_permissions(perms)?;

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

        // Parse assets
        let asset_themes = doc
            .get("assets")
            .and_then(|v| v.get("themes"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        Ok(Self {
            name,
            version,
            description,
            authors,
            license,
            homepage,
            min_frankenterm_version,
            engine,
            permissions,
            hooks,
            asset_themes,
        })
    }

    /// Parse a manifest from a file path.
    pub fn from_file(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        Self::from_toml_str(&content)
    }

    /// Return the manifest name after re-validating it as a safe directory name.
    ///
    /// `ParsedManifest::from_toml_str` validates parsed manifests, but this
    /// method protects call sites from manually constructed or mutated values.
    pub fn validated_name(&self) -> Result<&str> {
        validate_extension_name(&self.name)?;
        Ok(&self.name)
    }
}

/// Validate an extension name before it is used as a filesystem path component.
pub fn validate_extension_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("invalid extension name: name must not be empty");
    }

    if name != name.trim() {
        bail!("invalid extension name '{name}': surrounding whitespace is not allowed");
    }

    if name == "." || name == ".." {
        bail!("invalid extension name '{name}': dot path components are not allowed");
    }

    if name.starts_with('/') || name.starts_with('\\') || name.contains('/') || name.contains('\\')
    {
        bail!("invalid extension name '{name}': path separators are not allowed");
    }

    if matches!(name.as_bytes(), [first, b':', ..] if first.is_ascii_alphabetic()) {
        bail!("invalid extension name '{name}': Windows drive prefixes are not allowed");
    }

    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        bail!(
            "invalid extension name '{name}': only ASCII letters, digits, '.', '-', and '_' are allowed"
        );
    }

    let mut components = Path::new(name).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(component)), None) if component == OsStr::new(name) => Ok(()),
        _ => bail!("invalid extension name '{name}': name must be one path component"),
    }
}

fn parse_engine_config(engine: Option<&toml::Value>) -> EngineConfig {
    let Some(engine) = engine else {
        return EngineConfig::default();
    };

    let engine_type = engine
        .get("type")
        .and_then(|v| v.as_str())
        .and_then(EngineType::from_str)
        .unwrap_or(EngineType::Wasm);

    let default_entry = match engine_type {
        EngineType::Wasm | EngineType::Both => "main.wasm",
        EngineType::Lua => "main.lua",
    };

    let entry = engine
        .get("entry")
        .and_then(|v| v.as_str())
        .unwrap_or(default_entry)
        .to_string();

    EngineConfig { engine_type, entry }
}

fn parse_permissions(perms: Option<&toml::Value>) -> Result<ExtensionPermissions> {
    let Some(perms) = perms else {
        return Ok(ExtensionPermissions::default());
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
            filesystem_read.push(validated_filesystem_permission_path(path)?);
        } else if let Some(path) = entry.strip_prefix("write:") {
            filesystem_write.push(validated_filesystem_permission_path(path)?);
        } else {
            // Bare path = read-only
            filesystem_read.push(validated_filesystem_permission_path(entry)?);
        }
    }

    Ok(ExtensionPermissions {
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
    })
}

fn path_allowed_by_roots(path: &str, allowed_roots: &[String]) -> bool {
    let Some(requested) = normalize_filesystem_path(path) else {
        return false;
    };

    allowed_roots.iter().any(|allowed| {
        let Some(allowed_root) = normalize_filesystem_path(allowed) else {
            return false;
        };

        requested.starts_with(&allowed_root)
            && canonical_containment_matches(&requested, &allowed_root)
    })
}

fn validated_filesystem_permission_path(path: &str) -> Result<String> {
    let Some(normalized) = normalize_filesystem_path(path) else {
        bail!("invalid filesystem permission path '{path}'");
    };
    Ok(normalized.to_string_lossy().into_owned())
}

fn normalize_filesystem_path(path: &str) -> Option<PathBuf> {
    if path.is_empty() || path.contains('\0') || path.contains('\\') {
        return None;
    }

    let mut normalized = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Prefix(_) => return None,
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }

    (!normalized.as_os_str().is_empty()).then_some(normalized)
}

fn canonical_containment_matches(requested: &Path, allowed_root: &Path) -> bool {
    if !requested.is_absolute() || !allowed_root.is_absolute() {
        return true;
    }

    let Some(requested) = canonicalize_existing_prefix(requested) else {
        return false;
    };
    let Some(allowed_root) = canonicalize_existing_prefix(allowed_root) else {
        return false;
    };

    requested.starts_with(allowed_root)
}

fn canonicalize_existing_prefix(path: &Path) -> Option<PathBuf> {
    let mut probe = path.to_path_buf();
    let mut missing_components: Vec<OsString> = Vec::new();

    loop {
        if let Ok(mut canonical) = std::fs::canonicalize(&probe) {
            for component in missing_components.iter().rev() {
                canonical.push(component);
            }
            return Some(canonical);
        }

        let component = probe.file_name()?.to_os_string();
        missing_components.push(component);

        if !probe.pop() {
            return None;
        }
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
homepage = "https://github.com/author/my-theme"
min_frankenterm_version = "0.2.0"

[engine]
type = "wasm"
entry = "main.wasm"

[permissions]
filesystem = ["read:~/.config/frankenterm/", "write:~/.local/share/frankenterm/"]
environment = ["TERM", "COLORTERM", "FRANKENTERM_*"]
network = false
pane_access = false

[hooks]
on_config_reload = "handle_config_reload"
on_pane_focus = "handle_pane_focus"

[assets]
themes = ["assets/theme.toml"]
"#;

    #[test]
    fn parse_full_manifest() {
        let manifest = ParsedManifest::from_toml_str(FULL_MANIFEST).unwrap();
        assert_eq!(manifest.name, "my-theme");
        assert_eq!(manifest.version, "1.0.0");
        assert_eq!(manifest.description, "A beautiful theme");
        assert_eq!(manifest.authors.len(), 1);
        assert_eq!(manifest.license.as_deref(), Some("MIT"));
        assert_eq!(
            manifest.homepage.as_deref(),
            Some("https://github.com/author/my-theme")
        );
        assert_eq!(manifest.min_frankenterm_version.as_deref(), Some("0.2.0"));
        assert_eq!(manifest.engine.engine_type, EngineType::Wasm);
        assert_eq!(manifest.engine.entry, "main.wasm");
        assert!(!manifest.permissions.network);
        assert!(!manifest.permissions.pane_access);
        assert_eq!(manifest.permissions.filesystem_read.len(), 1);
        assert_eq!(manifest.permissions.filesystem_write.len(), 1);
        assert_eq!(manifest.hooks.len(), 2);
        assert_eq!(manifest.asset_themes, vec!["assets/theme.toml"]);
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
        assert_eq!(manifest.engine.engine_type, EngineType::Wasm);
        assert_eq!(manifest.engine.entry, "main.wasm");
        assert!(manifest.permissions.filesystem_read.is_empty());
        assert!(!manifest.permissions.network);
    }

    #[test]
    fn lua_engine_type() {
        let toml = r#"
[extension]
name = "lua-ext"

[engine]
type = "lua"
"#;
        let manifest = ParsedManifest::from_toml_str(toml).unwrap();
        assert_eq!(manifest.engine.engine_type, EngineType::Lua);
        assert_eq!(manifest.engine.entry, "main.lua");
        assert!(manifest.engine.engine_type.needs_lua());
        assert!(!manifest.engine.engine_type.needs_wasm());
    }

    #[test]
    fn both_engine_type() {
        let toml = r#"
[extension]
name = "dual-ext"

[engine]
type = "both"
entry = "main.wasm"
"#;
        let manifest = ParsedManifest::from_toml_str(toml).unwrap();
        assert_eq!(manifest.engine.engine_type, EngineType::Both);
        assert!(manifest.engine.engine_type.needs_wasm());
        assert!(manifest.engine.engine_type.needs_lua());
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
    fn invalid_extension_names_are_rejected() {
        for name in [
            "",
            ".",
            "..",
            "../outside",
            "/absolute",
            "nested/name",
            r"nested\name",
            "C:outside",
            "two words",
            " leading",
            "trailing ",
            "semi;colon",
        ] {
            let escaped_name = name.replace('\\', "\\\\").replace('"', "\\\"");
            let toml = format!("[extension]\nname = \"{escaped_name}\"\n");

            let result = ParsedManifest::from_toml_str(&toml);

            assert!(result.is_err(), "name should be rejected: {name:?}");
            let err = result.unwrap_err();
            assert!(
                format!("{err:#}").contains("invalid extension name"),
                "unexpected error for {name:?}: {err:#}"
            );
        }
    }

    #[test]
    fn extension_name_validator_accepts_single_safe_components() {
        for name in ["minimal", "my-theme", "lua_ext", "com.example.theme"] {
            validate_extension_name(name).unwrap();
        }
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
        assert!(perms.allows_read("~/.config/frankenterm"));
        assert!(perms.allows_read("~/.config/frankenterm/../frankenterm/theme.toml"));
        assert!(!perms.allows_read("~/.config/frankenterm-secret/theme.toml"));
        assert!(!perms.allows_read("~/.config/frankenterm/../../.ssh/id_rsa"));
        assert!(!perms.allows_read("~/.ssh/id_rsa"));
        assert!(perms.allows_write("~/.local/share/frankenterm/cache.db"));
        assert!(!perms.allows_write("~/.local/share/frankenterm-backup/cache.db"));
        assert!(!perms.allows_write("~/.config/frankenterm/theme.toml"));
    }

    #[test]
    fn filesystem_permission_checks_are_component_aware() {
        let perms = ExtensionPermissions {
            filesystem_read: vec!["/tmp/ft-allowed".to_string()],
            filesystem_write: vec!["/tmp/ft-out/".to_string()],
            ..Default::default()
        };

        assert!(perms.allows_read("/tmp/ft-allowed"));
        assert!(perms.allows_read("/tmp/ft-allowed/file.txt"));
        assert!(perms.allows_read("/tmp/ft-allowed/./nested/file.txt"));
        assert!(perms.allows_read("/tmp/ft-allowed/nested/../file.txt"));
        assert!(!perms.allows_read("/tmp/ft-allowed-secret/file.txt"));
        assert!(!perms.allows_read("/tmp/ft-allowed/../ft-allowed-secret/file.txt"));
        assert!(!perms.allows_read("../tmp/ft-allowed/file.txt"));

        assert!(perms.allows_write("/tmp/ft-out/new-file.txt"));
        assert!(!perms.allows_write("/tmp/ft-outputside/new-file.txt"));
        assert!(!perms.allows_write("/tmp/ft-out/../ft-outputside/new-file.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn filesystem_permission_checks_reject_symlink_escape() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = dir.path().join("allowed");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, allowed.join("outside-link")).unwrap();

        let perms = ExtensionPermissions {
            filesystem_read: vec![allowed.to_string_lossy().into_owned()],
            ..Default::default()
        };

        assert!(perms.allows_read(&allowed.join("file.txt").to_string_lossy()));
        assert!(!perms.allows_read(&allowed.join("outside-link/secret.txt").to_string_lossy()));
    }

    #[test]
    fn manifest_rejects_invalid_filesystem_permission_paths() {
        for permission in [
            "read:",
            "write:",
            "read:../escape",
            "write:/tmp/frankenterm/../../..",
            r"read:C:\Users\Public",
        ] {
            let escaped_permission = permission.replace('\\', "\\\\").replace('"', "\\\"");
            let toml = format!(
                r#"
[extension]
name = "bad-permission"

[permissions]
filesystem = ["{escaped_permission}"]
"#
            );

            let result = ParsedManifest::from_toml_str(&toml);

            assert!(
                result.is_err(),
                "permission should be rejected: {permission:?}"
            );
            let err = result.unwrap_err();
            assert!(
                format!("{err:#}").contains("invalid filesystem permission path"),
                "unexpected error for {permission:?}: {err:#}"
            );
        }
    }

    // ===================================================================
    // Property-based tests
    // ===================================================================

    use proptest::prelude::*;

    fn toml_escaped(value: &str) -> String {
        value.replace('\\', "\\\\").replace('"', "\\\"")
    }

    fn fuzz_component() -> impl Strategy<Value = String> {
        proptest::string::string_regex("[a-z][a-z0-9_.-]{0,12}")
            .expect("static fuzz component regex is valid")
    }

    fn fuzz_windows_drive() -> impl Strategy<Value = String> {
        proptest::string::string_regex("[A-Za-z]").expect("static drive regex is valid")
    }

    fn malicious_extension_name() -> impl Strategy<Value = String> {
        prop_oneof![
            Just(String::new()),
            Just(".".to_string()),
            Just("..".to_string()),
            fuzz_component().prop_map(|part| format!("../{part}")),
            fuzz_component().prop_map(|part| format!("/{part}")),
            (fuzz_component(), fuzz_component())
                .prop_map(|(left, right)| format!("{left}/{right}")),
            (fuzz_component(), fuzz_component())
                .prop_map(|(left, right)| format!(r"{left}\{right}")),
            (fuzz_windows_drive(), fuzz_component())
                .prop_map(|(drive, tail)| format!("{drive}:{tail}")),
            fuzz_component().prop_map(|part| format!(" {part}")),
            fuzz_component().prop_map(|part| format!("{part} ")),
            fuzz_component().prop_map(|part| format!("{part};evil")),
        ]
    }

    fn invalid_filesystem_permission_path() -> impl Strategy<Value = String> {
        prop_oneof![
            Just(String::new()),
            fuzz_component().prop_map(|part| format!("../{part}")),
            fuzz_component().prop_map(|part| format!("{part}/../../escape")),
            fuzz_component().prop_map(|part| format!("/tmp/{part}/../../../escape")),
            (fuzz_component(), fuzz_component())
                .prop_map(|(left, right)| format!(r"{left}\{right}")),
            (fuzz_windows_drive(), fuzz_component())
                .prop_map(|(drive, tail)| format!(r"{drive}:\{tail}")),
        ]
    }

    fn filesystem_bypass_path() -> impl Strategy<Value = (String, String)> {
        prop_oneof![
            (fuzz_component(), fuzz_component()).prop_map(|(root, suffix)| {
                let allowed_root = format!("/tmp/{root}");
                let bypass = format!("/tmp/{root}-{suffix}/payload.txt");
                (allowed_root, bypass)
            }),
            (fuzz_component(), fuzz_component()).prop_map(|(root, suffix)| {
                let allowed_root = format!("/tmp/{root}");
                let bypass = format!("/tmp/{root}/../{root}-{suffix}/payload.txt");
                (allowed_root, bypass)
            }),
            fuzz_component().prop_map(|root| {
                let allowed_root = format!("/tmp/{root}");
                let bypass = format!(r"/tmp/{root}\payload.txt");
                (allowed_root, bypass)
            }),
            fuzz_component().prop_map(|root| {
                let allowed_root = format!("/tmp/{root}");
                let bypass = format!("../tmp/{root}/payload.txt");
                (allowed_root, bypass)
            }),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(50))]

        /// EngineType::needs_wasm and needs_lua are complementary.
        #[test]
        fn prop_engine_type_needs(variant in 0_u8..3) {
            let et = match variant {
                0 => EngineType::Wasm,
                1 => EngineType::Lua,
                _ => EngineType::Both,
            };
            match et {
                EngineType::Wasm => {
                    prop_assert!(et.needs_wasm());
                    prop_assert!(!et.needs_lua());
                }
                EngineType::Lua => {
                    prop_assert!(!et.needs_wasm());
                    prop_assert!(et.needs_lua());
                }
                EngineType::Both => {
                    prop_assert!(et.needs_wasm());
                    prop_assert!(et.needs_lua());
                }
            }
        }

        /// Exact env var match always succeeds.
        #[test]
        fn prop_env_exact_match(name in "[A-Z]{3,10}") {
            let perms = ExtensionPermissions {
                environment: vec![name.clone()],
                ..Default::default()
            };
            prop_assert!(perms.allows_env_var(&name));
        }

        /// Glob prefix match works for trailing-star patterns.
        #[test]
        fn prop_env_glob_prefix_match(
            prefix in "[A-Z]{3,8}_",
            suffix in "[A-Z]{1,8}",
        ) {
            let pattern = format!("{prefix}*");
            let name = format!("{prefix}{suffix}");
            let perms = ExtensionPermissions {
                environment: vec![pattern],
                ..Default::default()
            };
            prop_assert!(perms.allows_env_var(&name));
        }

        /// Non-matching env var is denied.
        #[test]
        fn prop_env_no_match(
            allowed in "[A-Z]{3,6}",
            other in "[a-z]{3,6}",
        ) {
            let perms = ExtensionPermissions {
                environment: vec![allowed],
                ..Default::default()
            };
            // lowercase vs uppercase means no match
            prop_assert!(!perms.allows_env_var(&other));
        }

        /// Read prefix match works.
        #[test]
        fn prop_read_prefix_match(
            prefix in "/[a-z]{3,10}/",
            file in "[a-z.]{3,15}",
        ) {
            let perms = ExtensionPermissions {
                filesystem_read: vec![prefix.clone()],
                ..Default::default()
            };
            let path = format!("{prefix}{file}");
            prop_assert!(perms.allows_read(&path));
        }

        /// Read with different prefix is denied.
        #[test]
        fn prop_read_different_prefix_denied(
            allowed in "/allowed_[a-z]{3,8}/",
            denied in "/denied_[a-z]{3,8}/",
            file in "[a-z]{3,10}",
        ) {
            let perms = ExtensionPermissions {
                filesystem_read: vec![allowed],
                ..Default::default()
            };
            let path = format!("{denied}{file}");
            prop_assert!(!perms.allows_read(&path));
        }

        /// Write prefix match works.
        #[test]
        fn prop_write_prefix_match(
            prefix in "/[a-z]{3,10}/",
            file in "[a-z.]{3,15}",
        ) {
            let perms = ExtensionPermissions {
                filesystem_write: vec![prefix.clone()],
                ..Default::default()
            };
            let path = format!("{prefix}{file}");
            prop_assert!(perms.allows_write(&path));
        }

        /// Default permissions deny everything.
        #[test]
        fn prop_default_denies_all(
            path in "/[a-z]{3,10}/[a-z]{3,10}",
            env in "[A-Z]{3,10}",
        ) {
            let perms = ExtensionPermissions::default();
            prop_assert!(!perms.allows_read(&path));
            prop_assert!(!perms.allows_write(&path));
            prop_assert!(!perms.allows_env_var(&env));
            prop_assert!(!perms.network);
            prop_assert!(!perms.pane_access);
        }

        /// Minimal manifest parses with generated names.
        #[test]
        fn prop_minimal_manifest(name in "[a-z][a-z0-9_-]{2,20}") {
            let toml_str = format!(
                "[extension]\nname = \"{name}\"\n"
            );
            let manifest = ParsedManifest::from_toml_str(&toml_str).unwrap();
            prop_assert_eq!(&manifest.name, &name);
            prop_assert_eq!(&manifest.version, "0.0.0");
        }

        /// Manifest preserves version strings.
        #[test]
        fn prop_manifest_version(
            major in 0_u32..100,
            minor in 0_u32..100,
            patch in 0_u32..100,
        ) {
            let version = format!("{major}.{minor}.{patch}");
            let toml_str = format!(
                "[extension]\nname = \"test\"\nversion = \"{version}\"\n"
            );
            let manifest = ParsedManifest::from_toml_str(&toml_str).unwrap();
            prop_assert_eq!(&manifest.version, &version);
        }

        /// Missing [extension] table always errors.
        #[test]
        fn prop_missing_extension_errors(key in "[a-z]{3,10}") {
            let toml_str = format!("[{key}]\nval = \"x\"\n");
            if key != "extension" {
                prop_assert!(ParsedManifest::from_toml_str(&toml_str).is_err());
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        /// Fuzz malicious manifest names: every generated path-like name must fail closed.
        #[test]
        fn prop_fuzz_malicious_manifest_names_fail_closed(name in malicious_extension_name()) {
            prop_assert!(
                validate_extension_name(&name).is_err(),
                "validator accepted malicious extension name: {name:?}"
            );

            let toml_str = format!("[extension]\nname = \"{}\"\n", toml_escaped(&name));
            let result = ParsedManifest::from_toml_str(&toml_str);

            prop_assert!(
                result.is_err(),
                "manifest parser accepted malicious extension name: {name:?}"
            );
            let err = result.unwrap_err();
            prop_assert!(
                format!("{err:#}").contains("invalid extension name"),
                "unexpected manifest error for {name:?}: {err:#}"
            );
        }

        /// Fuzz fail-open permission roots: malformed declared roots must reject at parse time.
        #[test]
        fn prop_fuzz_invalid_filesystem_permission_roots_fail_closed(path in invalid_filesystem_permission_path()) {
            let escaped_path = toml_escaped(&path);
            let toml_str = format!(
                "[extension]\nname = \"fuzz-permissions\"\n\n[permissions]\nfilesystem = [\"read:{escaped_path}\"]\n"
            );
            let result = ParsedManifest::from_toml_str(&toml_str);

            prop_assert!(
                result.is_err(),
                "manifest parser accepted invalid filesystem permission root: {path:?}"
            );
            let err = result.unwrap_err();
            prop_assert!(
                format!("{err:#}").contains("invalid filesystem permission path"),
                "unexpected permission error for {path:?}: {err:#}"
            );
        }

        /// Fuzz request paths that previously bypassed raw-prefix checks.
        #[test]
        fn prop_fuzz_filesystem_bypass_paths_are_denied((allowed_root, bypass_path) in filesystem_bypass_path()) {
            let perms = ExtensionPermissions {
                filesystem_read: vec![allowed_root.clone()],
                filesystem_write: vec![allowed_root],
                ..Default::default()
            };

            prop_assert!(
                !perms.allows_read(&bypass_path),
                "read bypass should be denied: {bypass_path:?}"
            );
            prop_assert!(
                !perms.allows_write(&bypass_path),
                "write bypass should be denied: {bypass_path:?}"
            );
        }
    }
}
