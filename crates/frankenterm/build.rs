// Build script: captures git commit, build timestamp, rustc version, target triple,
// and enabled features as compile-time environment variables for `ft --version`.
//
// Reproducibility: honors SOURCE_DATE_EPOCH (https://reproducible-builds.org/specs/source-date-epoch/).
// When set, FT_BUILD_TS is derived from that epoch. Tracked source changes still set
// FT_GIT_DIRTY so reproducibility metadata cannot suppress candidate-integrity evidence.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

use frankenterm_build_identity::{
    AtomicComponentRole, SealedAtomicBuildIdentity, emit_cargo_atomic_component_marker,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Treat SOURCE_DATE_EPOCH="" (exported but empty) the same as unset. The spec requires a
    // non-negative integer; an empty string is not one, and honoring it would produce a
    // meaningless `built: epoch:` line and silently suppress the git-dirty check.
    let source_date_epoch = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .filter(|s| !s.trim().is_empty());

    let root = Path::new(&std::env::var("CARGO_MANIFEST_DIR")?)
        .join("../..")
        .canonicalize()?;
    let dsr = DsrSourceIdentity::from_environment()?;
    let source = resolve_source_identity(&root, dsr.as_ref())?;
    println!("cargo:rustc-env=FT_GIT_HASH={}", source.revision);
    if source.dirty {
        println!("cargo:rustc-env=FT_GIT_DIRTY=+dirty");
    } else {
        println!("cargo:rustc-env=FT_GIT_DIRTY=");
    }

    // Build timestamp (UTC). Under SOURCE_DATE_EPOCH, format that epoch; otherwise read wall clock.
    let build_ts = match source_date_epoch.as_deref() {
        Some(epoch) => format_epoch_utc(epoch),
        None => Command::new("date")
            .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| "unknown".to_string()),
    };
    println!("cargo:rustc-env=FT_BUILD_TS={build_ts}");

    // Rustc version
    let rustc_ver = Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=FT_RUSTC_VERSION={rustc_ver}");

    // Enabled features (collected via cfg)
    let mut features = Vec::new();
    for feat in &[
        "vendored",
        "browser",
        "mcp",
        "web",
        "tui",
        "metrics",
        "distributed",
    ] {
        println!(
            "cargo:rerun-if-env-changed=CARGO_FEATURE_{}",
            feat.to_uppercase()
        );
        if std::env::var(format!("CARGO_FEATURE_{}", feat.to_uppercase())).is_ok() {
            features.push(*feat);
        }
    }
    let feature_list = if features.is_empty() {
        "none".to_string()
    } else {
        features.join(",")
    };
    println!("cargo:rustc-env=FT_FEATURES={feature_list}");

    // Target triple
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=FT_TARGET={target}");

    emit_cargo_atomic_component_marker(AtomicComponentRole::Ft)?;

    // Rerun triggers
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");
    println!("cargo:rerun-if-changed=../../.git/refs");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    for variable in ["DSR_RELEASE_GIT_SHA", "DSR_RELEASE_GIT_REF"] {
        println!("cargo:rerun-if-env-changed={variable}");
    }
    Ok(())
}

/// Build metadata inputs are not source authority by themselves. Gitless
/// release metadata also requires the DSR archive and exact extracted bytes.
#[derive(Clone, Debug)]
pub struct DsrSourceIdentity {
    pub revision: String,
    pub reference: String,
    pub version: String,
    pub target: String,
    pub profile: String,
    pub atomic_identity: String,
}

impl DsrSourceIdentity {
    fn from_environment() -> io::Result<Option<Self>> {
        if std::env::var_os("DSR_RELEASE_GIT_SHA").is_none()
            && std::env::var_os("DSR_RELEASE_GIT_REF").is_none()
        {
            return Ok(None);
        }
        let required = |name| {
            std::env::var(name).map_err(|_| invalid_source("missing DSR build identity field"))
        };
        Ok(Some(Self {
            revision: required("DSR_RELEASE_GIT_SHA")?,
            reference: required("DSR_RELEASE_GIT_REF")?,
            version: required("CARGO_PKG_VERSION")?,
            target: required("TARGET")?,
            profile: required("FT_ATOMIC_BUILD_PROFILE")?,
            atomic_identity: required("FT_ATOMIC_BUILD_IDENTITY")?,
        }))
    }

    pub fn validate(&self) -> io::Result<()> {
        if self.revision.len() != 40
            || !self
                .revision
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || self.revision.bytes().all(|b| b == b'0')
            || self.reference != format!("v{}", self.version)
            || self.profile != "release-interactive"
        {
            return Err(invalid_source("invalid DSR revision, version or profile"));
        }
        SealedAtomicBuildIdentity::from_lower_hex(&self.atomic_identity)
            .map_err(|_| invalid_source("invalid sealed atomic build identity"))?;
        // Exact canonical JSON used by atomic-component-manifest.sh derive-build-id.
        // A BTreeMap pins ordering even if serde_json enables preserve_order.
        let fields = std::collections::BTreeMap::from([
            (
                "feature_contract",
                "application-family-gui-ft-mux-server-pty-guardian-default-features-v1",
            ),
            ("profile", self.profile.as_str()),
            ("schema_version", "ft.atomic_build_identity.v1"),
            ("source_revision", self.revision.as_str()),
            ("target", self.target.as_str()),
            ("version", self.version.as_str()),
        ]);
        let canonical = serde_json::to_vec(&fields).map_err(io::Error::other)?;
        let expected = hex::encode(Sha256::digest(canonical));
        if self.atomic_identity != expected {
            return Err(invalid_source(
                "DSR source and atomic build identity disagree",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct SourceIdentity {
    pub revision: String,
    pub dirty: bool,
}

pub fn resolve_source_identity(
    root: &Path,
    dsr: Option<&DsrSourceIdentity>,
) -> io::Result<SourceIdentity> {
    if let Some(dsr) = dsr {
        dsr.validate()?;
    }
    // A present Git administration entry forbids archive fallback even if Git
    // is broken or unavailable. Never relabel observed tracked dirt as clean.
    let has_git = match root.join(".git").symlink_metadata() {
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    if has_git {
        let revision = source_git(root)
            .args(["rev-parse", "--verify", "HEAD"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .unwrap_or_else(|| "unknown".to_owned());
        if dsr.is_some_and(|identity| identity.revision != revision) {
            return Err(invalid_source("Git revision disagrees with DSR identity"));
        }
        // Cargo does not infer build-script inputs from rustc's source inputs.
        // Watch tracked files explicitly so an unstaged edit cannot retain a
        // cached clean identity; avoid traversing untracked target/cache trees.
        let tracked = tracked_source_paths(root);
        if let Ok(paths) = &tracked {
            for path in paths {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
        let dirty = tracked.is_err()
            || source_git(root)
                .args(["diff-index", "--quiet", "HEAD", "--"])
                .status()
                .map_or(true, |status| !status.success());
        return Ok(SourceIdentity { revision, dirty });
    }
    let Some(dsr) = dsr else {
        return Ok(SourceIdentity {
            revision: "unknown".to_owned(),
            dirty: true,
        });
    };
    let archive = root
        .parent()
        .ok_or_else(|| invalid_source("source has no parent"))?
        .join(".source.tar");
    verify_dsr_archive(root, &archive, &dsr.revision)?;
    println!("cargo:rerun-if-changed={}", archive.display());
    println!("cargo:rerun-if-changed={}", root.display());
    Ok(SourceIdentity {
        revision: dsr.revision.clone(),
        dirty: false,
    })
}

fn invalid_source(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub fn tracked_source_paths(root: &Path) -> io::Result<Vec<PathBuf>> {
    let output = source_git(root)
        .args(["ls-files", "--cached", "-z"])
        .output()?;
    if !output.status.success() {
        return Err(invalid_source("cannot enumerate tracked source inputs"));
    }
    let mut paths = BTreeSet::new();
    for bytes in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|bytes| !bytes.is_empty())
    {
        let name = std::str::from_utf8(bytes)
            .map_err(|_| invalid_source("tracked source path is not UTF-8"))?;
        if name.contains(['\n', '\r']) {
            return Err(invalid_source(
                "tracked source path cannot be a Cargo directive",
            ));
        }
        let path = Path::new(name);
        if !path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        {
            return Err(invalid_source("invalid tracked source path"));
        }
        paths.insert(root.join(path));
    }
    Ok(paths.into_iter().collect())
}

fn source_git(root: &Path) -> Command {
    let mut command = Command::new("git");
    command.current_dir(root);
    // Resolve the checkout being compiled, never an ambient repository/index.
    // This is command-local and does not alter the caller's Git environment.
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    ] {
        command.env_remove(name);
    }
    command
}

fn plain_metadata(path: &Path) -> io::Result<fs::Metadata> {
    let metadata = path.symlink_metadata()?;
    if metadata.file_type().is_symlink() {
        return Err(invalid_source("source authority contains a symlink"));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(invalid_source("source authority contains a reparse point"));
        }
    }
    Ok(metadata)
}

/// Check the immutable sibling archive that DSR retained and verified before
/// starting Cargo. This checks its Git commit comment and every extracted file;
/// the release's DSR pre/post source receipts remain the publication authority.
pub fn verify_dsr_archive(root: &Path, archive_path: &Path, revision: &str) -> io::Result<()> {
    const MAX_ENTRIES: usize = 200_000;
    const MAX_ARCHIVE_BYTES: u64 = 8 * 1024 * 1024 * 1024;
    if !plain_metadata(root)?.is_dir() {
        return Err(invalid_source("source root is not a plain directory"));
    }
    let archive_meta = plain_metadata(archive_path)?;
    if !archive_meta.is_file() || archive_meta.len() > MAX_ARCHIVE_BYTES {
        return Err(invalid_source("invalid or oversized DSR source archive"));
    }
    let mut archive = tar::Archive::new(File::open(archive_path)?);
    let mut expected = BTreeSet::new();
    let mut commit_seen = false;
    let mut buffer = [0u8; 64 * 1024];
    let mut source_buffer = [0u8; 64 * 1024];
    for entry in archive.entries()? {
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        if kind.is_pax_global_extensions() {
            if commit_seen || !expected.is_empty() || entry.size() > 4096 {
                return Err(invalid_source("invalid Git archive commit header"));
            }
            for field in entry
                .pax_extensions()?
                .ok_or_else(|| invalid_source("missing Git archive commit header"))?
            {
                let field = field?;
                if field.key_bytes() == b"comment" && field.value_bytes() == revision.as_bytes() {
                    if commit_seen {
                        return Err(invalid_source("duplicate Git archive commit comment"));
                    }
                    commit_seen = true;
                } else {
                    return Err(invalid_source(
                        "Git archive commit does not match DSR revision",
                    ));
                }
            }
            continue;
        }
        if !commit_seen || (!kind.is_file() && !kind.is_dir()) {
            return Err(invalid_source("unsupported DSR source archive entry"));
        }
        let path = entry.path()?.into_owned();
        if path.as_os_str().is_empty()
            || !path
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
            || !expected.insert(path.clone())
            || expected.len() > MAX_ENTRIES
        {
            return Err(invalid_source("invalid or duplicate archive source path"));
        }
        let source_path = root.join(&path);
        for ancestor in path
            .ancestors()
            .skip(1)
            .filter(|path| !path.as_os_str().is_empty())
        {
            if !plain_metadata(&root.join(ancestor))?.is_dir() {
                return Err(invalid_source("invalid source parent directory"));
            }
        }
        let metadata = plain_metadata(&source_path)?;
        if kind.is_dir() {
            if !metadata.is_dir() {
                return Err(invalid_source("source directory type mismatch"));
            }
            continue;
        }
        if !metadata.is_file() || metadata.len() != entry.size() {
            return Err(invalid_source("source file type or size mismatch"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if (metadata.permissions().mode() & 0o111 != 0) != (entry.header().mode()? & 0o111 != 0)
            {
                return Err(invalid_source("source executable mode mismatch"));
            }
        }
        let mut file = File::open(&source_path)?;
        loop {
            let count = entry.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            file.read_exact(&mut source_buffer[..count])?;
            if buffer[..count] != source_buffer[..count] {
                return Err(invalid_source("source content differs from DSR archive"));
            }
        }
        if file.read(&mut source_buffer[..1])? != 0 {
            return Err(invalid_source("source grew while verifying archive"));
        }
        println!("cargo:rerun-if-changed={}", source_path.display());
    }
    if !commit_seen || expected.is_empty() {
        return Err(invalid_source("DSR source archive has no committed source"));
    }
    let mut actual = BTreeSet::new();
    let mut directories = vec![PathBuf::new()];
    while let Some(directory) = directories.pop() {
        for child in fs::read_dir(root.join(&directory))? {
            let child = child?;
            let relative = directory.join(child.file_name());
            let metadata = plain_metadata(&root.join(&relative))?;
            if !expected.contains(&relative)
                || !actual.insert(relative.clone())
                || actual.len() > MAX_ENTRIES
            {
                return Err(invalid_source("extra or duplicate source inventory entry"));
            }
            if metadata.is_dir() {
                directories.push(relative);
            } else if !metadata.is_file() {
                return Err(invalid_source("special source file"));
            }
        }
    }
    if actual != expected {
        return Err(invalid_source("source inventory differs from DSR archive"));
    }
    Ok(())
}

/// Format a unix epoch (seconds since 1970-01-01 UTC) as `YYYY-MM-DDTHH:MM:SSZ`.
/// Tries GNU `date -d @<epoch>` and BSD `date -r <epoch>` in order; falls back to
/// `epoch:<n>` so the build never fails for a valid SOURCE_DATE_EPOCH value.
fn format_epoch_utc(epoch: &str) -> String {
    let epoch_at = format!("@{epoch}");
    let gnu_args: [&str; 4] = ["-u", "-d", &epoch_at, "+%Y-%m-%dT%H:%M:%SZ"];
    let bsd_args: [&str; 4] = ["-u", "-r", epoch, "+%Y-%m-%dT%H:%M:%SZ"];
    for args in [&gnu_args[..], &bsd_args[..]] {
        let Ok(out) = Command::new("date").args(args).output() else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        let ts = String::from_utf8_lossy(&out.stdout).trim().to_string();
        // Sanity: output should start with a 4-digit year. Rejects BSD's garbage-on-unknown-flag
        // and any locale-mangled output.
        if ts.len() >= 4 && ts.as_bytes()[..4].iter().all(u8::is_ascii_digit) {
            return ts;
        }
    }
    format!("epoch:{epoch}")
}
