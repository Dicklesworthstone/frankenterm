//! FrankenTerm Extension Package (.ftx) handling.
//!
//! A `.ftx` file is a ZIP archive containing an `extension.toml` manifest
//! plus optional WASM/Lua entry points and asset files.

use crate::manifest::ParsedManifest;
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Represents an opened .ftx package with its parsed manifest and file listing.
#[derive(Clone, Debug)]
pub struct FtxPackage {
    /// Parsed manifest from extension.toml inside the archive.
    pub manifest: ParsedManifest,
    /// SHA-256 hash of the entire .ftx file.
    pub content_hash: [u8; 32],
    /// List of file paths inside the archive.
    pub entries: Vec<String>,
    /// Source path of the .ftx file.
    source_path: PathBuf,
}

impl FtxPackage {
    /// Open and validate a .ftx package from a file path.
    pub fn open(path: &Path) -> Result<Self> {
        let ftx_bytes = std::fs::read(path)
            .with_context(|| format!("failed to read .ftx file {}", path.display()))?;

        Self::from_bytes(&ftx_bytes, path.to_path_buf())
    }

    /// Open and validate a .ftx package from in-memory bytes.
    pub fn from_bytes(ftx_bytes: &[u8], source_path: PathBuf) -> Result<Self> {
        let content_hash = sha256(ftx_bytes);

        let cursor = std::io::Cursor::new(ftx_bytes);
        let mut archive =
            zip::ZipArchive::new(cursor).context("failed to open .ftx as ZIP archive")?;

        // Collect entry names
        let entries: Vec<String> = (0..archive.len())
            .filter_map(|i| archive.by_index(i).ok().map(|f| f.name().to_string()))
            .collect();

        // Read and parse extension.toml
        let manifest_str = {
            let mut manifest_file = archive
                .by_name("extension.toml")
                .context("missing extension.toml in .ftx package")?;
            let mut buf = String::new();
            manifest_file
                .read_to_string(&mut buf)
                .context("failed to read extension.toml from .ftx")?;
            buf
        };

        let manifest =
            ParsedManifest::from_toml_str(&manifest_str).context("invalid extension.toml")?;

        let pkg = Self {
            manifest,
            content_hash,
            entries,
            source_path,
        };

        pkg.validate()?;
        Ok(pkg)
    }

    /// Validate that the package contents match the manifest requirements.
    fn validate(&self) -> Result<()> {
        let entry_set: HashSet<&str> = self.entries.iter().map(|s| s.as_str()).collect();

        // Verify the engine entry file exists
        let entry = &self.manifest.engine.entry;
        if !entry_set.contains(entry.as_str()) {
            bail!(
                "manifest declares entry '{}' but it is missing from the package",
                entry
            );
        }

        // If engine needs WASM, verify the entry is a .wasm file
        if self.manifest.engine.engine_type.needs_wasm() && !entry.ends_with(".wasm") {
            let has_wasm = self.entries.iter().any(|e| e.ends_with(".wasm"));
            if !has_wasm {
                bail!("WASM engine type declared but no .wasm file found in package");
            }
        }

        // If engine needs Lua, verify a .lua file exists
        if self.manifest.engine.engine_type.needs_lua() {
            let has_lua = self.entries.iter().any(|e| e.ends_with(".lua"));
            if !has_lua && !entry.ends_with(".lua") {
                bail!("Lua engine type declared but no .lua file found in package");
            }
        }

        // Verify declared asset themes exist
        for theme in &self.manifest.asset_themes {
            if !entry_set.contains(theme.as_str()) {
                bail!(
                    "manifest declares asset theme '{}' but it is missing from the package",
                    theme
                );
            }
        }

        Ok(())
    }

    /// Extract the package contents to a directory.
    ///
    /// Creates a subdirectory named after the extension. Returns the path
    /// to the extracted extension directory.
    pub fn extract_to(&self, base_dir: &Path) -> Result<PathBuf> {
        let ext_dir = base_dir.join(&self.manifest.name);
        std::fs::create_dir_all(&ext_dir)
            .with_context(|| format!("failed to create extension dir {}", ext_dir.display()))?;

        let ftx_bytes = std::fs::read(&self.source_path).with_context(|| {
            format!("failed to re-read .ftx file {}", self.source_path.display())
        })?;

        let cursor = std::io::Cursor::new(ftx_bytes);
        let mut archive =
            zip::ZipArchive::new(cursor).context("failed to open .ftx for extraction")?;

        for i in 0..archive.len() {
            let mut file = archive
                .by_index(i)
                .with_context(|| format!("failed to read archive entry {i}"))?;

            let name = file.name().to_string();

            // Security: reject paths with directory traversal
            if name.contains("..") {
                bail!("rejecting path with directory traversal: {name}");
            }

            let out_path = ext_dir.join(&name);

            if file.is_dir() {
                std::fs::create_dir_all(&out_path).with_context(|| {
                    format!("failed to create directory {}", out_path.display())
                })?;
            } else {
                // Ensure parent directory exists
                if let Some(parent) = out_path.parent() {
                    std::fs::create_dir_all(parent).with_context(|| {
                        format!("failed to create parent dir {}", parent.display())
                    })?;
                }

                let mut buf = Vec::new();
                file.read_to_end(&mut buf)
                    .with_context(|| format!("failed to read {name} from archive"))?;

                std::fs::write(&out_path, &buf)
                    .with_context(|| format!("failed to write {}", out_path.display()))?;
            }
        }

        Ok(ext_dir)
    }

    /// The source path of the .ftx file.
    pub fn source_path(&self) -> &Path {
        &self.source_path
    }

    /// Hex-encoded SHA-256 content hash.
    pub fn content_hash_hex(&self) -> String {
        hex_encode(&self.content_hash)
    }

    /// Check if a specific file exists in the package.
    pub fn has_file(&self, name: &str) -> bool {
        self.entries.iter().any(|e| e == name)
    }

    /// Read a specific file from the package as bytes.
    pub fn read_file(&self, name: &str) -> Result<Vec<u8>> {
        let ftx_bytes = std::fs::read(&self.source_path).with_context(|| {
            format!("failed to re-read .ftx file {}", self.source_path.display())
        })?;

        let cursor = std::io::Cursor::new(ftx_bytes);
        let mut archive =
            zip::ZipArchive::new(cursor).context("failed to open .ftx for file read")?;

        let mut file = archive
            .by_name(name)
            .with_context(|| format!("file '{name}' not found in .ftx package"))?;

        let mut buf = Vec::new();
        file.read_to_end(&mut buf)
            .with_context(|| format!("failed to read {name} from .ftx"))?;

        Ok(buf)
    }
}

/// Build a minimal .ftx package in memory (for testing and programmatic creation).
#[derive(Default)]
pub struct FtxBuilder {
    files: Vec<(String, Vec<u8>)>,
}

impl FtxBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a file to the package.
    pub fn add_file(mut self, name: &str, content: &[u8]) -> Self {
        self.files.push((name.to_string(), content.to_vec()));
        self
    }

    /// Add extension.toml from a manifest string.
    pub fn add_manifest(self, toml_str: &str) -> Self {
        self.add_file("extension.toml", toml_str.as_bytes())
    }

    /// Build the .ftx ZIP archive and return raw bytes.
    pub fn build(self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        {
            let cursor = std::io::Cursor::new(&mut buf);
            let mut writer = zip::ZipWriter::new(cursor);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);

            for (name, content) in &self.files {
                writer
                    .start_file(name, options)
                    .with_context(|| format!("failed to add {name} to ZIP"))?;
                std::io::Write::write_all(&mut writer, content)
                    .with_context(|| format!("failed to write {name} content"))?;
            }

            writer.finish().context("failed to finalize ZIP archive")?;
        }
        Ok(buf)
    }

    /// Build and write to a file path.
    pub fn write_to(self, path: &Path) -> Result<()> {
        let bytes = self.build()?;
        std::fs::write(path, &bytes)
            .with_context(|| format!("failed to write .ftx to {}", path.display()))?;
        Ok(())
    }
}

fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::EngineType;

    const MINIMAL_WASM_MANIFEST: &str = r#"
[extension]
name = "test-ext"
version = "1.0.0"

[engine]
type = "wasm"
entry = "main.wasm"
"#;

    const LUA_MANIFEST: &str = r#"
[extension]
name = "lua-ext"
version = "0.1.0"

[engine]
type = "lua"
entry = "main.lua"
"#;

    // Minimal valid WASM module (magic + version header only — enough for packaging tests)
    const WASM_MAGIC: &[u8] = &[0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];

    fn build_test_ftx(manifest: &str, entry_name: &str, entry_content: &[u8]) -> Vec<u8> {
        FtxBuilder::new()
            .add_manifest(manifest)
            .add_file(entry_name, entry_content)
            .build()
            .unwrap()
    }

    #[test]
    fn open_valid_wasm_ftx() {
        let dir = tempfile::tempdir().unwrap();
        let ftx_path = dir.path().join("test.ftx");
        let bytes = build_test_ftx(MINIMAL_WASM_MANIFEST, "main.wasm", WASM_MAGIC);
        std::fs::write(&ftx_path, &bytes).unwrap();

        let pkg = FtxPackage::open(&ftx_path).unwrap();
        assert_eq!(pkg.manifest.name, "test-ext");
        assert_eq!(pkg.manifest.version, "1.0.0");
        assert!(pkg.has_file("extension.toml"));
        assert!(pkg.has_file("main.wasm"));
        assert!(!pkg.content_hash_hex().is_empty());
    }

    #[test]
    fn open_valid_lua_ftx() {
        let dir = tempfile::tempdir().unwrap();
        let ftx_path = dir.path().join("lua.ftx");
        let bytes = build_test_ftx(LUA_MANIFEST, "main.lua", b"-- lua script\n");
        std::fs::write(&ftx_path, &bytes).unwrap();

        let pkg = FtxPackage::open(&ftx_path).unwrap();
        assert_eq!(pkg.manifest.name, "lua-ext");
        assert_eq!(pkg.manifest.engine.engine_type, EngineType::Lua);
    }

    #[test]
    fn missing_entry_file_fails() {
        let dir = tempfile::tempdir().unwrap();
        let ftx_path = dir.path().join("bad.ftx");
        // Manifest says main.wasm but we only include extension.toml
        let bytes = FtxBuilder::new()
            .add_manifest(MINIMAL_WASM_MANIFEST)
            .build()
            .unwrap();
        std::fs::write(&ftx_path, &bytes).unwrap();

        let result = FtxPackage::open(&ftx_path);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("main.wasm"), "error: {err}");
    }

    #[test]
    fn missing_manifest_fails() {
        let dir = tempfile::tempdir().unwrap();
        let ftx_path = dir.path().join("no-manifest.ftx");
        let bytes = FtxBuilder::new()
            .add_file("main.wasm", WASM_MAGIC)
            .build()
            .unwrap();
        std::fs::write(&ftx_path, &bytes).unwrap();

        let result = FtxPackage::open(&ftx_path);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("extension.toml"));
    }

    #[test]
    fn extract_creates_extension_directory() {
        let dir = tempfile::tempdir().unwrap();
        let ftx_path = dir.path().join("test.ftx");
        let bytes = build_test_ftx(MINIMAL_WASM_MANIFEST, "main.wasm", WASM_MAGIC);
        std::fs::write(&ftx_path, &bytes).unwrap();

        let pkg = FtxPackage::open(&ftx_path).unwrap();
        let ext_dir = pkg.extract_to(dir.path()).unwrap();

        assert_eq!(ext_dir.file_name().unwrap(), "test-ext");
        assert!(ext_dir.join("extension.toml").exists());
        assert!(ext_dir.join("main.wasm").exists());
    }

    #[test]
    fn extract_with_subdirectories() {
        let dir = tempfile::tempdir().unwrap();
        let ftx_path = dir.path().join("assets.ftx");

        let manifest = r#"
[extension]
name = "themed"
version = "1.0.0"

[engine]
type = "wasm"
entry = "main.wasm"

[assets]
themes = ["assets/theme.toml"]
"#;

        let bytes = FtxBuilder::new()
            .add_manifest(manifest)
            .add_file("main.wasm", WASM_MAGIC)
            .add_file("assets/theme.toml", b"[colors]\nforeground = \"#fff\"\n")
            .build()
            .unwrap();
        std::fs::write(&ftx_path, &bytes).unwrap();

        let pkg = FtxPackage::open(&ftx_path).unwrap();
        let ext_dir = pkg.extract_to(dir.path()).unwrap();

        assert!(ext_dir.join("assets/theme.toml").exists());
    }

    #[test]
    fn read_file_from_package() {
        let dir = tempfile::tempdir().unwrap();
        let ftx_path = dir.path().join("read.ftx");
        let bytes = build_test_ftx(MINIMAL_WASM_MANIFEST, "main.wasm", WASM_MAGIC);
        std::fs::write(&ftx_path, &bytes).unwrap();

        let pkg = FtxPackage::open(&ftx_path).unwrap();
        let wasm_bytes = pkg.read_file("main.wasm").unwrap();
        assert_eq!(wasm_bytes, WASM_MAGIC);
    }

    #[test]
    fn content_hash_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let ftx_path = dir.path().join("hash.ftx");
        let bytes = build_test_ftx(MINIMAL_WASM_MANIFEST, "main.wasm", WASM_MAGIC);
        std::fs::write(&ftx_path, &bytes).unwrap();

        let pkg1 = FtxPackage::open(&ftx_path).unwrap();
        let pkg2 = FtxPackage::open(&ftx_path).unwrap();
        assert_eq!(pkg1.content_hash, pkg2.content_hash);
    }

    #[test]
    fn builder_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let ftx_path = dir.path().join("built.ftx");

        FtxBuilder::new()
            .add_manifest(MINIMAL_WASM_MANIFEST)
            .add_file("main.wasm", WASM_MAGIC)
            .add_file("README.md", b"# Test Extension\n")
            .write_to(&ftx_path)
            .unwrap();

        let pkg = FtxPackage::open(&ftx_path).unwrap();
        assert_eq!(pkg.manifest.name, "test-ext");
        assert_eq!(pkg.entries.len(), 3);
        assert!(pkg.has_file("README.md"));
    }

    #[test]
    fn invalid_zip_fails() {
        let dir = tempfile::tempdir().unwrap();
        let ftx_path = dir.path().join("garbage.ftx");
        std::fs::write(&ftx_path, b"not a zip file").unwrap();

        let result = FtxPackage::open(&ftx_path);
        assert!(result.is_err());
    }

    #[test]
    fn missing_asset_theme_fails() {
        let dir = tempfile::tempdir().unwrap();
        let ftx_path = dir.path().join("bad-assets.ftx");

        let manifest = r#"
[extension]
name = "bad-theme"
version = "1.0.0"

[engine]
type = "wasm"
entry = "main.wasm"

[assets]
themes = ["assets/missing.toml"]
"#;

        let bytes = FtxBuilder::new()
            .add_manifest(manifest)
            .add_file("main.wasm", WASM_MAGIC)
            .build()
            .unwrap();
        std::fs::write(&ftx_path, &bytes).unwrap();

        let result = FtxPackage::open(&ftx_path);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing.toml"));
    }
}
