//! The browser fleet viewer package, served under `/ui/`.
//!
//! `scripts/build-browser-ui.sh` writes a package directory whose
//! `manifest.json` names every file with its SHA-256. The server loads the
//! package once at startup: each listed file is read, size-checked and
//! digest-checked, and kept in memory. Requests are answered only from that
//! set, so no request path ever reaches the filesystem (no traversal or
//! symlink escape) and one server never mixes files from two builds.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::web_framework::{Response, ResponseBody, StatusCode};

/// The only package schema this server reads.
pub const UI_PACKAGE_SCHEMA: &str = "ft-browser-ui-package-v1";
const MANIFEST_FILE: &str = "manifest.json";
const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_FILES: usize = 256;
const MAX_PATH_BYTES: usize = 200;

/// Content security policy for the viewer page: same-origin everything,
/// WebAssembly compilation allowed, no inline script, no framing.
pub const UI_CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; \
style-src 'self'; font-src 'self'; connect-src 'self'; img-src 'self' data:; \
base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// Why a package was refused. Startup fails with this rather than serving a
/// partial or unverified viewer.
#[derive(Debug, thiserror::Error)]
pub enum UiAssetError {
    #[error("browser UI package manifest: {0}")]
    Manifest(String),
    #[error("browser UI package path {path:?} is not allowed: {reason}")]
    Path { path: String, reason: &'static str },
    #[error("browser UI package file {path:?}: {reason}")]
    File { path: String, reason: String },
    #[error("browser UI package file {path:?} does not match its manifest digest")]
    Digest { path: String },
    #[error("browser UI package is too large: {0}")]
    Limit(String),
}

/// One served file.
#[derive(Debug)]
pub struct UiAsset {
    content_type: &'static str,
    bytes: Arc<Vec<u8>>,
}

impl UiAsset {
    #[must_use]
    pub fn content_type(&self) -> &'static str {
        self.content_type
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

/// A verified package, keyed by its manifest-relative path.
#[derive(Debug)]
pub struct UiAssetSet {
    files: BTreeMap<String, UiAsset>,
    manifest_sha256: String,
}

#[derive(serde::Deserialize)]
struct Manifest {
    schema: String,
    files: BTreeMap<String, String>,
}

fn path_error(path: &str, reason: &'static str) -> UiAssetError {
    UiAssetError::Path {
        path: path.to_string(),
        reason,
    }
}

/// A manifest path is relative, ASCII, and made of plain components.
fn validate_relative_path(path: &str) -> Result<(), UiAssetError> {
    if path.is_empty() || path.len() > MAX_PATH_BYTES {
        return Err(path_error(path, "empty or too long"));
    }
    if !path
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'/'))
    {
        return Err(path_error(
            path,
            "only ASCII letters, digits, '.', '-', '_' and '/'",
        ));
    }
    for component in path.split('/') {
        if component.is_empty() {
            return Err(path_error(path, "empty component or leading/trailing '/'"));
        }
        if component.starts_with('.') {
            return Err(path_error(path, "dot components and hidden files"));
        }
    }
    if path == MANIFEST_FILE {
        return Err(path_error(path, "the manifest lists files, not itself"));
    }
    Ok(())
}

fn content_type_for(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("woff2") => "font/woff2",
        Some("json") => "application/json",
        _ => "text/plain; charset=utf-8",
    }
}

/// Read a regular file (never through a symlink) of at most `limit` bytes.
fn read_bounded(path: &Path, display: &str, limit: u64) -> Result<Vec<u8>, UiAssetError> {
    let file_error = |reason: String| UiAssetError::File {
        path: display.to_string(),
        reason,
    };
    let meta = std::fs::symlink_metadata(path).map_err(|e| file_error(e.to_string()))?;
    if !meta.file_type().is_file() {
        return Err(file_error("not a regular file".to_string()));
    }
    if meta.len() > limit {
        return Err(file_error(format!("{} bytes exceeds {limit}", meta.len())));
    }
    let file = std::fs::File::open(path).map_err(|e| file_error(e.to_string()))?;
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| file_error(e.to_string()))?;
    if bytes.len() as u64 > limit {
        return Err(file_error(format!("grew past {limit} bytes while reading")));
    }
    Ok(bytes)
}

impl UiAssetSet {
    /// Load and verify the package rooted at `dir`.
    pub fn load(dir: &Path) -> Result<Self, UiAssetError> {
        let root: PathBuf = std::fs::canonicalize(dir).map_err(|e| UiAssetError::File {
            path: dir.display().to_string(),
            reason: e.to_string(),
        })?;
        let manifest_bytes =
            read_bounded(&root.join(MANIFEST_FILE), MANIFEST_FILE, MAX_MANIFEST_BYTES)?;
        let manifest: Manifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|e| UiAssetError::Manifest(e.to_string()))?;
        if manifest.schema != UI_PACKAGE_SCHEMA {
            return Err(UiAssetError::Manifest(format!(
                "schema {:?}, expected {UI_PACKAGE_SCHEMA:?}",
                manifest.schema
            )));
        }
        if !manifest.files.contains_key("index.html") {
            return Err(UiAssetError::Manifest("no index.html".to_string()));
        }
        if manifest.files.len() > MAX_FILES {
            return Err(UiAssetError::Limit(format!(
                "{} files exceeds {MAX_FILES}",
                manifest.files.len()
            )));
        }

        let mut files = BTreeMap::new();
        let mut total: u64 = 0;
        for (rel, expected) in &manifest.files {
            validate_relative_path(rel)?;
            let expected = expected.to_ascii_lowercase();
            if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(UiAssetError::Manifest(format!(
                    "{rel:?}: digest is not SHA-256 hex"
                )));
            }
            let path = rel
                .split('/')
                .fold(root.clone(), |acc, part| acc.join(part));
            let bytes = read_bounded(&path, rel, MAX_FILE_BYTES)?;
            total += bytes.len() as u64;
            if total > MAX_TOTAL_BYTES {
                return Err(UiAssetError::Limit(format!(
                    "over {MAX_TOTAL_BYTES} bytes in total"
                )));
            }
            if hex::encode(Sha256::digest(&bytes)) != expected {
                return Err(UiAssetError::Digest { path: rel.clone() });
            }
            files.insert(
                rel.clone(),
                UiAsset {
                    content_type: content_type_for(rel),
                    bytes: Arc::new(bytes),
                },
            );
        }
        Ok(Self {
            files,
            manifest_sha256: hex::encode(Sha256::digest(&manifest_bytes)),
        })
    }

    /// Manifest-relative paths of every served file.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }

    #[must_use]
    pub fn get(&self, path: &str) -> Option<&UiAsset> {
        self.files.get(path)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// SHA-256 of the manifest: the identity of this package build.
    #[must_use]
    pub fn manifest_sha256(&self) -> &str {
        &self.manifest_sha256
    }

    /// The response for one manifest path. Unknown paths are 404.
    #[must_use]
    pub fn response(&self, path: &str) -> Response {
        let Some(asset) = self.files.get(path) else {
            return Response::with_status(StatusCode::NOT_FOUND)
                .header("content-type", b"text/plain; charset=utf-8".to_vec())
                .header("x-content-type-options", b"nosniff".to_vec())
                .body(ResponseBody::Bytes(b"not found".to_vec()));
        };
        let mut response = Response::with_status(StatusCode::OK)
            .header("content-type", asset.content_type.as_bytes().to_vec())
            .header("x-content-type-options", b"nosniff".to_vec())
            // Revalidate: file names carry no content hash, so a cached copy
            // could pair old JavaScript glue with new WebAssembly.
            .header("cache-control", b"no-cache".to_vec())
            .header("referrer-policy", b"no-referrer".to_vec());
        if asset.content_type.starts_with("text/html") {
            response = response
                .header(
                    "content-security-policy",
                    UI_CONTENT_SECURITY_POLICY.as_bytes().to_vec(),
                )
                .header("x-frame-options", b"DENY".to_vec());
        }
        response.body(ResponseBody::Bytes(asset.bytes.as_ref().clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web_framework::FrameworkResponseBody;

    fn write_package(dir: &Path, files: &[(&str, &[u8])], tamper: Option<&str>) {
        let mut listed = BTreeMap::new();
        for (rel, bytes) in files {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, bytes).unwrap();
            let digest = if Some(*rel) == tamper {
                "0".repeat(64)
            } else {
                hex::encode(Sha256::digest(bytes))
            };
            listed.insert((*rel).to_string(), digest);
        }
        let manifest = serde_json::json!({ "schema": UI_PACKAGE_SCHEMA, "files": listed });
        std::fs::write(
            dir.join(MANIFEST_FILE),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
    }

    fn body(response: &Response) -> Vec<u8> {
        match response.body_ref() {
            FrameworkResponseBody::Bytes(bytes) => bytes.clone(),
            _ => panic!("expected a bytes body"),
        }
    }

    fn header<'a>(response: &'a Response, name: &str) -> Option<&'a [u8]> {
        response
            .headers()
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_slice())
    }

    const FILES: &[(&str, &[u8])] = &[
        ("index.html", b"<!doctype html><title>viewer</title>"),
        ("app.mjs", b"export {};"),
        ("pkg/viewer_bg.wasm", b"\0asm\x01\0\0\0"),
    ];

    #[test]
    fn verified_package_serves_listed_files_with_types_and_headers() {
        let dir = tempfile::tempdir().unwrap();
        write_package(dir.path(), FILES, None);
        let set = UiAssetSet::load(dir.path()).expect("package loads");
        assert_eq!(set.len(), 3);
        assert_eq!(set.manifest_sha256().len(), 64);

        let index = set.response("index.html");
        assert_eq!(index.status(), StatusCode::OK);
        assert_eq!(body(&index), FILES[0].1);
        assert_eq!(
            header(&index, "content-type"),
            Some(&b"text/html; charset=utf-8"[..])
        );
        assert!(header(&index, "content-security-policy").is_some());
        assert_eq!(header(&index, "x-frame-options"), Some(&b"DENY"[..]));

        let wasm = set.response("pkg/viewer_bg.wasm");
        assert_eq!(
            header(&wasm, "content-type"),
            Some(&b"application/wasm"[..])
        );
        assert_eq!(
            header(&wasm, "x-content-type-options"),
            Some(&b"nosniff"[..])
        );
        assert!(header(&wasm, "content-security-policy").is_none());
    }

    #[test]
    fn unlisted_and_traversal_paths_are_not_found() {
        let dir = tempfile::tempdir().unwrap();
        write_package(dir.path(), FILES, None);
        std::fs::write(dir.path().join("secret.txt"), b"on disk but unlisted").unwrap();
        let set = UiAssetSet::load(dir.path()).unwrap();
        for path in [
            "secret.txt",
            "../index.html",
            "manifest.json",
            "pkg/../index.html",
            "",
        ] {
            assert_eq!(
                set.response(path).status(),
                StatusCode::NOT_FOUND,
                "{path:?}"
            );
        }
    }

    #[test]
    fn tampered_file_refuses_the_whole_package() {
        let dir = tempfile::tempdir().unwrap();
        write_package(dir.path(), FILES, Some("app.mjs"));
        let err = UiAssetSet::load(dir.path()).unwrap_err();
        assert!(
            matches!(err, UiAssetError::Digest { ref path } if path == "app.mjs"),
            "{err}"
        );
    }

    #[test]
    fn manifest_paths_must_be_plain_relative_names() {
        for bad in [
            "../etc/passwd",
            "/abs.js",
            "pkg//x.js",
            ".hidden",
            "pkg/.x",
            "a\\b.js",
            "manifest.json",
            "sp ace.js",
        ] {
            assert!(validate_relative_path(bad).is_err(), "{bad:?} accepted");
        }
        for good in [
            "index.html",
            "pkg/FrankenTerm_bg.wasm",
            "fonts/pragmasevka-nf-subset.woff2",
        ] {
            validate_relative_path(good).unwrap_or_else(|e| panic!("{good:?}: {e}"));
        }
    }

    #[test]
    fn wrong_schema_missing_index_and_missing_file_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(MANIFEST_FILE),
            br#"{"schema":"other","files":{"index.html":"00"}}"#,
        )
        .unwrap();
        assert!(matches!(
            UiAssetSet::load(dir.path()),
            Err(UiAssetError::Manifest(_))
        ));

        let dir = tempfile::tempdir().unwrap();
        write_package(dir.path(), &FILES[1..], None);
        assert!(matches!(
            UiAssetSet::load(dir.path()),
            Err(UiAssetError::Manifest(_))
        ));

        let dir = tempfile::tempdir().unwrap();
        write_package(dir.path(), FILES, None);
        std::fs::remove_file(dir.path().join("app.mjs")).unwrap();
        assert!(matches!(
            UiAssetSet::load(dir.path()),
            Err(UiAssetError::File { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_files_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        write_package(dir.path(), FILES, None);
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("x.mjs"), b"export {};").unwrap();
        std::fs::remove_file(dir.path().join("app.mjs")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("x.mjs"), dir.path().join("app.mjs"))
            .unwrap();
        assert!(matches!(
            UiAssetSet::load(dir.path()),
            Err(UiAssetError::File { .. })
        ));
    }
}
