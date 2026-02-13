//! Extension lifecycle management: install, list, remove, and load.
//!
//! Extensions are stored in a platform-specific directory and loaded
//! from their extracted contents on the filesystem.

use crate::manifest::ParsedManifest;
use crate::package::FtxPackage;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

/// Metadata for an installed extension on disk.
#[derive(Clone, Debug)]
pub struct InstalledExtension {
    /// Extension name (from manifest).
    pub name: String,
    /// Extension version (from manifest).
    pub version: String,
    /// Path to the installed extension directory.
    pub path: PathBuf,
    /// Parsed manifest.
    pub manifest: ParsedManifest,
}

/// Manages extension installation, discovery, and removal.
pub struct ExtensionManager {
    extensions_dir: PathBuf,
}

impl ExtensionManager {
    /// Create a manager for the given extensions directory.
    pub fn new(extensions_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&extensions_dir).with_context(|| {
            format!(
                "failed to create extensions dir {}",
                extensions_dir.display()
            )
        })?;
        Ok(Self { extensions_dir })
    }

    /// Create a manager using the default platform-specific directory.
    pub fn with_default_dir() -> Result<Self> {
        let dir = default_extensions_dir()?;
        Self::new(dir)
    }

    /// Install an extension from a .ftx package file.
    ///
    /// Returns the installed extension metadata. If an extension with the
    /// same name already exists, it is replaced.
    pub fn install(&self, ftx_path: &Path) -> Result<InstalledExtension> {
        let pkg = FtxPackage::open(ftx_path)
            .with_context(|| format!("failed to open package {}", ftx_path.display()))?;

        let name = &pkg.manifest.name;

        // Remove existing installation if present
        let ext_dir = self.extensions_dir.join(name);
        if ext_dir.exists() {
            std::fs::remove_dir_all(&ext_dir).with_context(|| {
                format!("failed to remove existing extension {}", ext_dir.display())
            })?;
        }

        let installed_dir = pkg.extract_to(&self.extensions_dir)?;

        Ok(InstalledExtension {
            name: name.clone(),
            version: pkg.manifest.version.clone(),
            path: installed_dir,
            manifest: pkg.manifest,
        })
    }

    /// List all installed extensions.
    pub fn list(&self) -> Result<Vec<InstalledExtension>> {
        let mut extensions = Vec::new();

        let entries = std::fs::read_dir(&self.extensions_dir).with_context(|| {
            format!(
                "failed to read extensions dir {}",
                self.extensions_dir.display()
            )
        })?;

        for entry in entries {
            let entry = entry?;
            let path = entry.path();

            if !path.is_dir() {
                continue;
            }

            let manifest_path = path.join("extension.toml");
            if !manifest_path.exists() {
                continue;
            }

            match ParsedManifest::from_file(&manifest_path) {
                Ok(manifest) => {
                    extensions.push(InstalledExtension {
                        name: manifest.name.clone(),
                        version: manifest.version.clone(),
                        path,
                        manifest,
                    });
                }
                Err(err) => {
                    // Skip invalid extensions but log the error
                    eprintln!(
                        "warning: skipping invalid extension at {}: {err:#}",
                        path.display()
                    );
                }
            }
        }

        extensions.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(extensions)
    }

    /// Remove an installed extension by name.
    pub fn remove(&self, name: &str) -> Result<()> {
        let ext_dir = self.extensions_dir.join(name);
        if !ext_dir.exists() {
            bail!("extension '{name}' is not installed");
        }

        // Verify it's actually an extension (has extension.toml)
        let manifest_path = ext_dir.join("extension.toml");
        if !manifest_path.exists() {
            bail!(
                "directory {} does not contain an extension.toml",
                ext_dir.display()
            );
        }

        std::fs::remove_dir_all(&ext_dir)
            .with_context(|| format!("failed to remove extension dir {}", ext_dir.display()))?;

        Ok(())
    }

    /// Get metadata for a specific installed extension.
    pub fn get(&self, name: &str) -> Result<Option<InstalledExtension>> {
        let ext_dir = self.extensions_dir.join(name);
        if !ext_dir.exists() {
            return Ok(None);
        }

        let manifest_path = ext_dir.join("extension.toml");
        if !manifest_path.exists() {
            return Ok(None);
        }

        let manifest = ParsedManifest::from_file(&manifest_path)?;
        Ok(Some(InstalledExtension {
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            path: ext_dir,
            manifest,
        }))
    }

    /// The base directory for extensions.
    pub fn extensions_dir(&self) -> &Path {
        &self.extensions_dir
    }
}

/// Returns the default extensions directory for the current platform.
///
/// On Linux/macOS with XDG_DATA_HOME set: `$XDG_DATA_HOME/frankenterm/extensions/`
/// On macOS without XDG: `~/Library/Application Support/FrankenTerm/extensions/`
/// Fallback: `~/.local/share/frankenterm/extensions/`
pub fn default_extensions_dir() -> Result<PathBuf> {
    // Check XDG_DATA_HOME first (works on all platforms)
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        return Ok(PathBuf::from(xdg).join("frankenterm/extensions"));
    }

    // macOS-native fallback
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = home_dir() {
            return Ok(home.join("Library/Application Support/FrankenTerm/extensions"));
        }
    }

    // Linux/generic fallback
    if let Some(home) = home_dir() {
        return Ok(home.join(".local/share/frankenterm/extensions"));
    }

    bail!("could not determine home directory for extensions")
}

fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::FtxBuilder;

    const WASM_MAGIC: &[u8] = &[0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];

    fn test_manifest(name: &str) -> String {
        format!(
            r#"
[extension]
name = "{name}"
version = "1.0.0"

[engine]
type = "wasm"
entry = "main.wasm"
"#
        )
    }

    fn create_test_ftx(dir: &Path, name: &str) -> PathBuf {
        let ftx_path = dir.join(format!("{name}.ftx"));
        FtxBuilder::new()
            .add_manifest(&test_manifest(name))
            .add_file("main.wasm", WASM_MAGIC)
            .write_to(&ftx_path)
            .unwrap();
        ftx_path
    }

    #[test]
    fn install_and_list() {
        let dir = tempfile::tempdir().unwrap();
        let ext_dir = dir.path().join("extensions");
        let manager = ExtensionManager::new(ext_dir).unwrap();

        let ftx_path = create_test_ftx(dir.path(), "test-ext");
        let installed = manager.install(&ftx_path).unwrap();
        assert_eq!(installed.name, "test-ext");
        assert_eq!(installed.version, "1.0.0");

        let list = manager.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "test-ext");
    }

    #[test]
    fn install_replaces_existing() {
        let dir = tempfile::tempdir().unwrap();
        let ext_dir = dir.path().join("extensions");
        let manager = ExtensionManager::new(ext_dir).unwrap();

        let ftx_path = create_test_ftx(dir.path(), "replaceable");
        manager.install(&ftx_path).unwrap();
        manager.install(&ftx_path).unwrap();

        let list = manager.list().unwrap();
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn remove_installed_extension() {
        let dir = tempfile::tempdir().unwrap();
        let ext_dir = dir.path().join("extensions");
        let manager = ExtensionManager::new(ext_dir).unwrap();

        let ftx_path = create_test_ftx(dir.path(), "removable");
        manager.install(&ftx_path).unwrap();

        manager.remove("removable").unwrap();
        let list = manager.list().unwrap();
        assert!(list.is_empty());
    }

    #[test]
    fn remove_nonexistent_fails() {
        let dir = tempfile::tempdir().unwrap();
        let ext_dir = dir.path().join("extensions");
        let manager = ExtensionManager::new(ext_dir).unwrap();

        let result = manager.remove("ghost");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not installed"));
    }

    #[test]
    fn get_installed_extension() {
        let dir = tempfile::tempdir().unwrap();
        let ext_dir = dir.path().join("extensions");
        let manager = ExtensionManager::new(ext_dir).unwrap();

        let ftx_path = create_test_ftx(dir.path(), "findable");
        manager.install(&ftx_path).unwrap();

        let found = manager.get("findable").unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "findable");

        let missing = manager.get("missing").unwrap();
        assert!(missing.is_none());
    }

    #[test]
    fn list_multiple_extensions_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let ext_dir = dir.path().join("extensions");
        let manager = ExtensionManager::new(ext_dir).unwrap();

        let ftx_c = create_test_ftx(dir.path(), "charlie");
        let ftx_a = create_test_ftx(dir.path(), "alpha");
        let ftx_b = create_test_ftx(dir.path(), "bravo");

        manager.install(&ftx_c).unwrap();
        manager.install(&ftx_a).unwrap();
        manager.install(&ftx_b).unwrap();

        let list = manager.list().unwrap();
        let names: Vec<&str> = list.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "bravo", "charlie"]);
    }

    #[test]
    fn default_extensions_dir_returns_path() {
        let result = default_extensions_dir();
        assert!(result.is_ok());
        let path = result.unwrap();
        let path_lower = path.to_string_lossy().to_lowercase();
        assert!(path_lower.contains("frankenterm"));
        assert!(path_lower.contains("extensions"));
    }
}
