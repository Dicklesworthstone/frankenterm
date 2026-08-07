//! Browser automation scaffolding for Playwright-based auth flows.
//!
//! Provides lazy Playwright initialization, profile directory management,
//! and safe logging for browser automation tasks.
//!
//! # Architecture
//!
//! ```text
//! BrowserConfig (headless, profiles dir, timeouts)
//!       │
//!       ▼
//! BrowserContext (lazy init, profile isolation)
//!       │
//!       ▼
//! Playwright CLI (subprocess: npx playwright ...)
//! ```
//!
//! # Profiles
//!
//! Browser profiles are stored under the data directory:
//! ```text
//! <data_dir>/browser_profiles/<service>/<account>/
//!   ├── Default/          # Chromium profile data
//!   └── .wa_profile.json  # wa metadata
//! ```
//!
//! Each service+account pair gets an isolated browser profile to prevent
//! cookie/session cross-contamination.
//!
//! # Safety
//!
//! - Device codes, tokens, and secrets are NEVER logged.
//! - Profile paths and persisted browser state are never logged.
//! - All browser operations are behind the `browser` feature flag.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt, OpenOptionsMaybeDirExt};
#[cfg(unix)]
use cap_std::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use serde::{Deserialize, Serialize};

use crate::error::{Result, StorageError};

pub mod anthropic_auth;
pub mod bootstrap;
pub mod google_auth;
pub mod openai_device;

// =============================================================================
// Configuration
// =============================================================================

/// Browser automation configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BrowserConfig {
    /// Run browser in headless mode (default: false for early development).
    pub headless: bool,

    /// Navigation timeout in milliseconds (default: 30s).
    pub navigation_timeout_ms: u64,

    /// Page load timeout in milliseconds (default: 60s).
    pub page_load_timeout_ms: u64,

    /// Timeout for the local Playwright readiness probe (default: 10s).
    pub readiness_probe_timeout_ms: u64,

    /// Browser type to use (default: "chromium").
    pub browser_type: String,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            headless: false,
            navigation_timeout_ms: 30_000,
            page_load_timeout_ms: 60_000,
            readiness_probe_timeout_ms: 10_000,
            browser_type: "chromium".to_string(),
        }
    }
}

// =============================================================================
// Profile Management
// =============================================================================

/// Maximum admitted size of `.wa_profile.json` metadata.
///
/// Profile metadata is a small fixed-schema document. The generous 64 KiB cap
/// bounds allocation and parse work while leaving ample room for future
/// fields. Reads retain one extra byte so growth after the metadata check is
/// still detected rather than silently truncating a valid-looking prefix.
pub const PROFILE_METADATA_MAX_BYTES: u64 = 64 * 1024;

/// Maximum admitted size of Playwright's exported storage-state document.
///
/// Storage state can contain many cookies and local-storage entries, so its
/// limit is intentionally much larger than profile metadata. The finite cap
/// still bounds allocation and I/O when a profile is corrupt or hostile.
pub const STORAGE_STATE_MAX_BYTES: u64 = 16 * 1024 * 1024;

const PROFILE_METADATA_FILE_NAME: &str = ".wa_profile.json";
const STORAGE_STATE_FILE_NAME: &str = "storage_state.json";
const PRIVATE_FILE_CREATE_ATTEMPTS: u64 = 16;
static PRIVATE_FILE_WRITE_NONCE: AtomicU64 = AtomicU64::new(0);

/// Encode structured Node-script input into an ASCII-only literal.
///
/// Browser-flow values can contain newlines, quotes, backslashes, or Unicode
/// line separators. Serializing the complete input object and then base64
/// encoding it avoids constructing JavaScript string literals from those
/// values. The resulting script is delivered over the child's stdin, never
/// argv, the environment, or a temporary file.
pub(super) fn encode_node_script_input(
    input: &serde_json::Value,
) -> std::result::Result<String, BrowserNodeCommandFailure> {
    use base64::Engine as _;

    let json = serde_json::to_vec(input)
        .map_err(|_| BrowserNodeCommandFailure::ScriptOversized)?;
    if json.len() > BROWSER_NODE_INPUT_JSON_MAX_BYTES {
        return Err(BrowserNodeCommandFailure::ScriptOversized);
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(json);
    if encoded.len()
        > BROWSER_NODE_SCRIPT_MAX_BYTES.saturating_sub(BROWSER_NODE_SCRIPT_STATIC_RESERVE_BYTES)
    {
        return Err(BrowserNodeCommandFailure::ScriptOversized);
    }
    Ok(encoded)
}

#[cfg(test)]
fn decode_node_script_input(script: &str) -> serde_json::Value {
    use base64::Engine as _;

    let encoded = script
        .split_once("Buffer.from('")
        .and_then(|(_, tail)| tail.split_once('\''))
        .map(|(encoded, _)| encoded)
        .expect("script must contain one base64 input literal");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .expect("script input must be valid base64");
    serde_json::from_slice(&bytes).expect("script input must be valid JSON")
}

pub(super) const BROWSER_NODE_SCRIPT_MAX_BYTES: usize = 1024 * 1024;
pub(super) const BROWSER_NODE_INPUT_MAX_FIELD_BYTES: usize = 64 * 1024;
pub(super) const BROWSER_NODE_INPUT_MAX_TOTAL_BYTES: usize = 96 * 1024;
pub(super) const BROWSER_NODE_INPUT_MAX_LIST_ENTRIES: usize = 128;
const BROWSER_NODE_INPUT_JSON_MAX_BYTES: usize = 720 * 1024;
const BROWSER_NODE_SCRIPT_STATIC_RESERVE_BYTES: usize = 128 * 1024;
/// A valid storage-state document can become almost twice as large when its
/// JSON text is escaped as a string inside the subprocess result envelope.
/// Two times the admitted document cap plus a finite envelope allowance is
/// therefore the exact retained-output policy.
pub(super) const BROWSER_BOOTSTRAP_MAX_STDOUT_BYTES: usize =
    (STORAGE_STATE_MAX_BYTES as usize) * 2 + 64 * 1024;
const BROWSER_NODE_MAX_STDERR_BYTES: usize = 256 * 1024;
const BROWSER_NODE_MAX_FLOW_TIMEOUT_MS: u64 = 30 * 60 * 1000;
const BROWSER_NODE_MAX_POLL_INTERVAL_MS: u64 = 60 * 1000;
const BROWSER_NODE_PROCESS_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BrowserNodeCommandFailure {
    InvalidTimeout,
    InvalidPollInterval,
    InvalidConfiguration,
    ScriptOversized,
    InputWriteFailed,
    TimedOut,
    Cancelled,
    OutputOversized,
    CaptureIncomplete,
    CleanupIncomplete,
    Unavailable,
}

impl BrowserNodeCommandFailure {
    pub(super) const fn detail(self) -> &'static str {
        match self {
            Self::InvalidTimeout => "Browser automation timeout is outside the supported range",
            Self::InvalidPollInterval => {
                "Browser automation polling interval is outside the supported range"
            }
            Self::InvalidConfiguration => "Browser automation configuration is invalid",
            Self::ScriptOversized => "Browser automation input exceeds its safety limit",
            Self::InputWriteFailed => "Browser automation input could not be delivered safely",
            Self::TimedOut => "Browser automation exceeded its wall-clock deadline",
            Self::Cancelled => "Browser automation was cancelled",
            Self::OutputOversized => "Browser automation output exceeds its safety limit",
            Self::CaptureIncomplete => "Browser automation output capture did not settle",
            Self::CleanupIncomplete => "Browser automation process cleanup did not settle",
            Self::Unavailable => "Browser automation subprocess is unavailable",
        }
    }
}

pub(super) fn admit_browser_timeout(
    timeout_ms: u64,
) -> std::result::Result<(), BrowserNodeCommandFailure> {
    if timeout_ms == 0 || timeout_ms > BROWSER_NODE_MAX_FLOW_TIMEOUT_MS {
        return Err(BrowserNodeCommandFailure::InvalidTimeout);
    }
    Ok(())
}

pub(super) fn admit_browser_poll_interval(
    poll_interval_ms: u64,
    timeout_ms: u64,
) -> std::result::Result<(), BrowserNodeCommandFailure> {
    if poll_interval_ms == 0
        || poll_interval_ms > BROWSER_NODE_MAX_POLL_INTERVAL_MS
        || poll_interval_ms > timeout_ms
    {
        return Err(BrowserNodeCommandFailure::InvalidPollInterval);
    }
    Ok(())
}

pub(super) fn admit_browser_url(
    candidate: &str,
) -> std::result::Result<(), BrowserNodeCommandFailure> {
    let parsed = url::Url::parse(candidate)
        .map_err(|_| BrowserNodeCommandFailure::InvalidConfiguration)?;
    let secure_transport = parsed.scheme() == "https";
    let loopback_http = parsed.scheme() == "http"
        && parsed
            .host_str()
            .is_some_and(|host| matches!(host, "localhost" | "127.0.0.1" | "::1"));
    if (!secure_transport && !loopback_http)
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.host_str().is_none()
    {
        return Err(BrowserNodeCommandFailure::InvalidConfiguration);
    }
    Ok(())
}

pub(super) fn admit_selector_group(
    selectors: &str,
) -> std::result::Result<(), BrowserNodeCommandFailure> {
    let mut count = 0_usize;
    for selector in selectors.split(", ") {
        if selector.trim().is_empty() {
            return Err(BrowserNodeCommandFailure::InvalidConfiguration);
        }
        count = count.saturating_add(1);
        if count > BROWSER_NODE_INPUT_MAX_LIST_ENTRIES {
            return Err(BrowserNodeCommandFailure::InvalidConfiguration);
        }
    }
    Ok(())
}

fn admit_node_script_input_len(total: &mut usize, byte_len: usize) -> bool {
    if byte_len > BROWSER_NODE_INPUT_MAX_FIELD_BYTES {
        return false;
    }
    let Some(next) = total.checked_add(byte_len) else {
        return false;
    };
    if next > BROWSER_NODE_INPUT_MAX_TOTAL_BYTES {
        return false;
    }
    *total = next;
    true
}

pub(super) fn admit_node_script_input_parts(
    text_fields: &[Option<&str>],
    path_fields: &[Option<&Path>],
    list_fields: &[&[String]],
) -> std::result::Result<(), BrowserNodeCommandFailure> {
    let mut total = 0_usize;
    for value in text_fields.iter().flatten() {
        if !admit_node_script_input_len(&mut total, value.len()) {
            return Err(BrowserNodeCommandFailure::ScriptOversized);
        }
    }
    for path in path_fields.iter().flatten() {
        if !admit_node_script_input_len(&mut total, path.as_os_str().as_encoded_bytes().len()) {
            return Err(BrowserNodeCommandFailure::ScriptOversized);
        }
    }
    for values in list_fields {
        if values.len() > BROWSER_NODE_INPUT_MAX_LIST_ENTRIES {
            return Err(BrowserNodeCommandFailure::ScriptOversized);
        }
        for value in *values {
            if !admit_node_script_input_len(&mut total, value.len()) {
                return Err(BrowserNodeCommandFailure::ScriptOversized);
            }
        }
    }
    Ok(())
}

pub(super) fn admit_node_script_source(
    script: String,
) -> std::result::Result<String, BrowserNodeCommandFailure> {
    admit_node_script_source_size(script.len())?;
    Ok(script)
}

fn admit_node_script_source_size(
    byte_len: usize,
) -> std::result::Result<(), BrowserNodeCommandFailure> {
    if byte_len > BROWSER_NODE_SCRIPT_MAX_BYTES {
        return Err(BrowserNodeCommandFailure::ScriptOversized);
    }
    Ok(())
}

pub(super) fn run_node_script_bounded(
    script: String,
    flow_timeout_ms: u64,
    stdout_limit: usize,
) -> std::result::Result<std::process::Output, BrowserNodeCommandFailure> {
    use crate::runtime_async::process::{
        CommandCancelled, CommandInputLimitExceeded, CommandInputWriteFailed,
        CommandOutputCaptureIncomplete, CommandOutputLimitExceeded,
        CommandProcessCleanupIncomplete, CommandTimedOut,
    };

    admit_node_script_source_size(script.len())?;
    admit_browser_timeout(flow_timeout_ms)?;
    let timeout = std::time::Duration::from_millis(flow_timeout_ms)
        .checked_add(BROWSER_NODE_PROCESS_GRACE)
        .ok_or(BrowserNodeCommandFailure::InvalidTimeout)?;
    let mut command = crate::runtime_async::process::Command::new("node");
    command
        .arg("-")
        .stdin_limit(BROWSER_NODE_SCRIPT_MAX_BYTES)
        .stdin_bytes(script.into_bytes())
        .stdout_limit(stdout_limit)
        .stderr_limit(BROWSER_NODE_MAX_STDERR_BYTES);
    command.output_blocking(timeout).map_err(|error| {
        if CommandInputLimitExceeded::from_io_error(&error).is_some() {
            BrowserNodeCommandFailure::ScriptOversized
        } else if CommandInputWriteFailed::from_io_error(&error).is_some() {
            BrowserNodeCommandFailure::InputWriteFailed
        } else if CommandTimedOut::from_io_error(&error).is_some() {
            BrowserNodeCommandFailure::TimedOut
        } else if CommandCancelled::from_io_error(&error).is_some() {
            BrowserNodeCommandFailure::Cancelled
        } else if CommandOutputLimitExceeded::from_io_error(&error).is_some() {
            BrowserNodeCommandFailure::OutputOversized
        } else if CommandOutputCaptureIncomplete::from_io_error(&error).is_some() {
            BrowserNodeCommandFailure::CaptureIncomplete
        } else if CommandProcessCleanupIncomplete::from_io_error(&error).is_some() {
            BrowserNodeCommandFailure::CleanupIncomplete
        } else {
            BrowserNodeCommandFailure::Unavailable
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProfileMetadataReadFailure {
    MetadataUnavailable,
    NotRegularFile,
    Oversized,
    ReadUnavailable,
    ChangedDuringRead,
    InvalidSchema,
    IdentityMismatch,
    InvalidTimestamp,
}

impl ProfileMetadataReadFailure {
    const fn detail(self) -> &'static str {
        match self {
            Self::MetadataUnavailable => {
                "Browser profile metadata attributes could not be inspected"
            }
            Self::NotRegularFile => "Browser profile metadata is not a regular file",
            Self::Oversized => "Browser profile metadata exceeds the 64 KiB safety limit",
            Self::ReadUnavailable => "Browser profile metadata could not be read safely",
            Self::ChangedDuringRead => {
                "Browser profile metadata changed while it was being read"
            }
            Self::InvalidSchema => "Browser profile metadata does not match the required schema",
            Self::IdentityMismatch => {
                "Browser profile metadata identity does not match its profile"
            }
            Self::InvalidTimestamp => {
                "Browser profile metadata contains an invalid timestamp"
            }
        }
    }

    fn into_storage_error(self) -> StorageError {
        StorageError::Database(self.detail().to_string())
    }

    const fn from_private_file(failure: PrivateFileReadFailure) -> Self {
        match failure {
            PrivateFileReadFailure::MetadataUnavailable => Self::MetadataUnavailable,
            PrivateFileReadFailure::NotRegularFile => Self::NotRegularFile,
            PrivateFileReadFailure::Oversized => Self::Oversized,
            PrivateFileReadFailure::ReadUnavailable => Self::ReadUnavailable,
            PrivateFileReadFailure::ChangedDuringRead => Self::ChangedDuringRead,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StorageStateFailure {
    MetadataUnavailable,
    NotRegularFile,
    Oversized,
    ReadUnavailable,
    ChangedDuringRead,
    InvalidJson,
    InvalidShape,
    WriteUnavailable,
}

impl StorageStateFailure {
    const fn detail(self) -> &'static str {
        match self {
            Self::MetadataUnavailable => {
                "Browser storage-state attributes could not be inspected"
            }
            Self::NotRegularFile => "Browser storage state is not a regular file",
            Self::Oversized => "Browser storage state exceeds the 16 MiB safety limit",
            Self::ReadUnavailable => "Browser storage state could not be read safely",
            Self::ChangedDuringRead => "Browser storage state changed while it was being read",
            Self::InvalidJson => "Browser storage state is not valid JSON",
            Self::InvalidShape => "Browser storage state does not match the required shape",
            Self::WriteUnavailable => "Browser storage state could not be written safely",
        }
    }

    fn into_storage_error(self) -> StorageError {
        StorageError::Database(self.detail().to_string())
    }

    const fn from_private_file(failure: PrivateFileReadFailure) -> Self {
        match failure {
            PrivateFileReadFailure::MetadataUnavailable => Self::MetadataUnavailable,
            PrivateFileReadFailure::NotRegularFile => Self::NotRegularFile,
            PrivateFileReadFailure::Oversized => Self::Oversized,
            PrivateFileReadFailure::ReadUnavailable => Self::ReadUnavailable,
            PrivateFileReadFailure::ChangedDuringRead => Self::ChangedDuringRead,
        }
    }
}

/// Observable identity/version snapshot for a browser-profile filesystem entry.
///
/// On Unix, device+inode and nanosecond mtime/ctime make replacement or
/// mutation detection authoritative for the supported macOS/Linux targets.
/// Other targets use the portable length+modified-time surface available in
/// `std`; an adversarial replacement with identical observable metadata is a
/// residual portability limit, while the hard byte cap and typed schema gate
/// remain enforced independently.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProfileMetadataFileSnapshot {
    byte_len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    modified_seconds: i64,
    #[cfg(unix)]
    modified_nanoseconds: i64,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanoseconds: i64,
}

impl ProfileMetadataFileSnapshot {
    fn capture(metadata: &std::fs::Metadata) -> Option<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;

        Some(Self {
            byte_len: metadata.len(),
            modified: Some(metadata.modified().ok()?),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            modified_seconds: metadata.mtime(),
            #[cfg(unix)]
            modified_nanoseconds: metadata.mtime_nsec(),
            #[cfg(unix)]
            changed_seconds: metadata.ctime(),
            #[cfg(unix)]
            changed_nanoseconds: metadata.ctime_nsec(),
        })
    }

    fn capture_cap(metadata: &cap_std::fs::Metadata) -> Option<Self> {
        #[cfg(unix)]
        use cap_fs_ext::OsMetadataExt;

        Some(Self {
            byte_len: metadata.len(),
            modified: Some(metadata.modified().ok()?.into_std()),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            modified_seconds: metadata.mtime(),
            #[cfg(unix)]
            modified_nanoseconds: metadata.mtime_nsec(),
            #[cfg(unix)]
            changed_seconds: metadata.ctime(),
            #[cfg(unix)]
            changed_nanoseconds: metadata.ctime_nsec(),
        })
    }

    #[cfg(test)]
    fn synthetic(byte_len: u64, version: u64) -> Self {
        let seconds = i64::try_from(version).unwrap_or(i64::MAX);
        Self {
            byte_len,
            modified: Some(
                std::time::SystemTime::UNIX_EPOCH
                    .checked_add(std::time::Duration::from_secs(version))
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH),
            ),
            #[cfg(unix)]
            device: 1,
            #[cfg(unix)]
            inode: version,
            #[cfg(unix)]
            modified_seconds: seconds,
            #[cfg(unix)]
            modified_nanoseconds: 0,
            #[cfg(unix)]
            changed_seconds: seconds,
            #[cfg(unix)]
            changed_nanoseconds: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrivateFileReadFailure {
    MetadataUnavailable,
    NotRegularFile,
    Oversized,
    ReadUnavailable,
    ChangedDuringRead,
}

fn private_io_error(message: &'static str) -> std::io::Error {
    std::io::Error::other(message)
}

fn snapshot_std(
    metadata: &std::fs::Metadata,
) -> std::io::Result<ProfileMetadataFileSnapshot> {
    ProfileMetadataFileSnapshot::capture(metadata)
        .ok_or_else(|| private_io_error("required file version metadata is unavailable"))
}

fn snapshot_cap(
    metadata: &cap_std::fs::Metadata,
) -> std::io::Result<ProfileMetadataFileSnapshot> {
    ProfileMetadataFileSnapshot::capture_cap(metadata)
        .ok_or_else(|| private_io_error("required file version metadata is unavailable"))
}

fn open_child_directory_nofollow(
    parent: &cap_std::fs::Dir,
    name: &str,
) -> std::io::Result<cap_std::fs::Dir> {
    open_child_directory_path_nofollow(parent, Path::new(name))
}

fn open_child_directory_path_nofollow(
    parent: &cap_std::fs::Dir,
    name: &Path,
) -> std::io::Result<cap_std::fs::Dir> {
    let mut options = cap_std::fs::OpenOptions::new();
    options
        .read(true)
        .follow(FollowSymlinks::No)
        .maybe_dir(true);
    let file = parent.open_with(name, &options)?;
    if !file.metadata()?.is_dir() {
        return Err(private_io_error("profile component is not a directory"));
    }
    Ok(cap_std::fs::Dir::from_std_file(file.into_std()))
}

fn set_private_directory_permissions(directory: &cap_std::fs::Dir) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        directory
            .try_clone()?
            .into_std_file()
            .set_permissions(std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn verify_private_directory_permissions(directory: &cap_std::fs::Dir) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = directory
            .try_clone()?
            .into_std_file()
            .metadata()?
            .permissions()
            .mode()
            & 0o777;
        if mode & 0o077 != 0 {
            return Err(private_io_error("private directory permissions are too broad"));
        }
    }
    Ok(())
}

fn create_private_child_directory(
    parent: &cap_std::fs::Dir,
    name: &Path,
) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut builder = cap_std::fs::DirBuilder::new();
        builder.mode(0o700);
        parent.create_dir_with(name, &builder)
    }
    #[cfg(not(unix))]
    {
        parent.create_dir(name)
    }
}

fn path_has_parent_component(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
}

fn open_directory_tree_nofollow(path: &Path) -> std::io::Result<cap_std::fs::Dir> {
    if path_has_parent_component(path) {
        return Err(private_io_error("private directory path contains a parent component"));
    }
    let Some(leaf) = path.file_name() else {
        let base = if path.as_os_str().is_empty() {
            Path::new(".")
        } else {
            path
        };
        return cap_std::fs::Dir::open_ambient_dir(base, cap_std::ambient_authority());
    };
    let parent_path = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = open_directory_tree_nofollow(parent_path)?;
    open_child_directory_path_nofollow(&parent, Path::new(leaf))
}

fn ensure_directory_tree_nofollow(path: &Path) -> std::io::Result<cap_std::fs::Dir> {
    if path_has_parent_component(path) {
        return Err(private_io_error("private directory path contains a parent component"));
    }
    let Some(leaf) = path.file_name() else {
        let base = if path.as_os_str().is_empty() {
            Path::new(".")
        } else {
            path
        };
        return cap_std::fs::Dir::open_ambient_dir(base, cap_std::ambient_authority());
    };
    let parent_path = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = ensure_directory_tree_nofollow(parent_path)?;
    let leaf = Path::new(leaf);
    let created = match create_private_child_directory(&parent, leaf) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => return Err(error),
    };
    let directory = open_child_directory_path_nofollow(&parent, leaf)?;
    if created {
        set_private_directory_permissions(&directory)?;
        #[cfg(unix)]
        parent.try_clone()?.into_std_file().sync_all()?;
    }
    Ok(directory)
}

fn open_profiles_root_capability(path: &Path) -> std::io::Result<cap_std::fs::Dir> {
    open_directory_tree_nofollow(path)
}

fn ensure_profiles_root_capability(path: &Path) -> std::io::Result<cap_std::fs::Dir> {
    let directory = ensure_directory_tree_nofollow(path)?;
    set_private_directory_permissions(&directory)?;
    Ok(directory)
}

fn ensure_artifacts_root_capability(path: &Path) -> std::io::Result<cap_std::fs::Dir> {
    let directory = ensure_directory_tree_nofollow(path)?;
    verify_private_directory_permissions(&directory)?;
    Ok(directory)
}

fn ensure_child_directory_nofollow(
    parent: &cap_std::fs::Dir,
    name: &str,
) -> std::io::Result<cap_std::fs::Dir> {
    match create_private_child_directory(parent, Path::new(name)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let directory = open_child_directory_nofollow(parent, name)?;
    set_private_directory_permissions(&directory)?;
    Ok(directory)
}

fn validate_private_directory_component(component: &str) -> std::io::Result<()> {
    if component.is_empty()
        || component.len() > 64
        || !component
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(private_io_error("private directory component is invalid"));
    }
    Ok(())
}

fn validate_private_file_name(file_name: &str) -> std::io::Result<()> {
    if file_name.is_empty()
        || file_name.len() > 64
        || matches!(file_name, "." | "..")
        || !file_name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(private_io_error("private file name is invalid"));
    }
    Ok(())
}

pub(super) fn create_private_invocation_directory(
    root_path: &Path,
    flow_name: &str,
) -> std::io::Result<PathBuf> {
    validate_private_directory_component(flow_name)?;
    // Artifact roots are caller-configurable. Never silently chmod an existing
    // directory that might serve another purpose; a pre-existing root must
    // already be private, while newly created roots are 0700 by construction.
    let root = ensure_artifacts_root_capability(root_path)?;
    let flow_directory = ensure_child_directory_nofollow(&root, flow_name)?;
    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();

    for _ in 0..PRIVATE_FILE_CREATE_ATTEMPTS {
        let entropy: u128 = rand::random();
        let invocation_name = format!("{timestamp}_{entropy:032x}");
        match create_private_child_directory(&flow_directory, Path::new(&invocation_name)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
        let directory = open_child_directory_nofollow(&flow_directory, &invocation_name)?;
        set_private_directory_permissions(&directory)?;
        let handle_snapshot = snapshot_std(&directory.try_clone()?.into_std_file().metadata()?)?;
        let path_metadata = flow_directory.symlink_metadata(&invocation_name)?;
        if !path_metadata.is_dir() {
            return Err(private_io_error(
                "private invocation directory changed after creation",
            ));
        }
        let path_snapshot = snapshot_cap(&path_metadata)?;
        if handle_snapshot != path_snapshot {
            return Err(private_io_error(
                "private invocation directory changed after creation",
            ));
        }
        #[cfg(unix)]
        flow_directory.try_clone()?.into_std_file().sync_all()?;
        return Ok(root_path.join(flow_name).join(invocation_name));
    }

    Err(private_io_error(
        "private invocation directory name could not be allocated",
    ))
}

pub(super) fn write_private_file_create_new(
    directory_path: &Path,
    file_name: &str,
    bytes: &[u8],
    max_bytes: usize,
) -> std::io::Result<PathBuf> {
    validate_private_file_name(file_name)?;
    if bytes.len() > max_bytes {
        return Err(private_io_error("private artifact exceeds its safety limit"));
    }
    let directory = open_profiles_root_capability(directory_path)?;
    verify_private_directory_permissions(&directory)?;
    let mut options = cap_std::fs::OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = directory.open_with(file_name, &options)?.into_std();
    file.write_all(bytes)?;
    file.sync_all()?;
    let handle_snapshot = snapshot_std(&file.metadata()?)?;
    let path_metadata = directory.symlink_metadata(file_name)?;
    if !path_metadata.is_file() {
        return Err(private_io_error("private artifact changed after creation"));
    }
    let path_snapshot = snapshot_cap(&path_metadata)?;
    if handle_snapshot != path_snapshot {
        return Err(private_io_error("private artifact changed after creation"));
    }
    #[cfg(unix)]
    directory.try_clone()?.into_std_file().sync_all()?;
    Ok(directory_path.join(file_name))
}

fn open_private_file_nofollow(
    directory: &cap_std::fs::Dir,
    name: &str,
) -> std::io::Result<std::fs::File> {
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    directory
        .open_with(name, &options)
        .map(cap_std::fs::File::into_std)
}

fn private_file_is_safely_accessible(
    directory: &cap_std::fs::Dir,
    name: &str,
    max_bytes: u64,
) -> bool {
    private_file_is_safely_accessible_with_hook(directory, name, max_bytes, || {})
}

fn private_file_is_safely_accessible_with_hook<F>(
    directory: &cap_std::fs::Dir,
    name: &str,
    max_bytes: u64,
    after_initial_metadata: F,
) -> bool
where
    F: FnOnce(),
{
    let Ok(path_metadata_before) = directory.symlink_metadata(name) else {
        return false;
    };
    if !path_metadata_before.is_file() || path_metadata_before.len() > max_bytes {
        return false;
    }
    let Ok(path_snapshot_before) = snapshot_cap(&path_metadata_before) else {
        return false;
    };
    after_initial_metadata();

    let Ok(file) = open_private_file_nofollow(directory, name) else {
        return false;
    };
    let Ok(handle_metadata) = file.metadata() else {
        return false;
    };
    if !handle_metadata.is_file() || handle_metadata.len() > max_bytes {
        return false;
    }
    let Ok(handle_snapshot) = snapshot_std(&handle_metadata) else {
        return false;
    };
    let Ok(path_metadata_after) = directory.symlink_metadata(name) else {
        return false;
    };
    if !path_metadata_after.is_file() || path_metadata_after.len() > max_bytes {
        return false;
    }
    let Ok(path_snapshot_after) = snapshot_cap(&path_metadata_after) else {
        return false;
    };

    path_snapshot_before == handle_snapshot && handle_snapshot == path_snapshot_after
}

fn read_private_file_bounded(
    directory: &cap_std::fs::Dir,
    name: &str,
    max_bytes: u64,
) -> std::result::Result<Option<Vec<u8>>, PrivateFileReadFailure> {
    let path_metadata_before = match directory.symlink_metadata(name) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(PrivateFileReadFailure::MetadataUnavailable),
    };
    if !path_metadata_before.is_file() {
        return Err(PrivateFileReadFailure::NotRegularFile);
    }
    if path_metadata_before.len() > max_bytes {
        return Err(PrivateFileReadFailure::Oversized);
    }
    let path_snapshot_before = snapshot_cap(&path_metadata_before)
        .map_err(|_| PrivateFileReadFailure::MetadataUnavailable)?;

    let mut file = open_private_file_nofollow(directory, name)
        .map_err(|_| PrivateFileReadFailure::ReadUnavailable)?;
    let opened_metadata = file
        .metadata()
        .map_err(|_| PrivateFileReadFailure::MetadataUnavailable)?;
    if !opened_metadata.is_file() {
        return Err(PrivateFileReadFailure::NotRegularFile);
    }
    if opened_metadata.len() > max_bytes {
        return Err(PrivateFileReadFailure::Oversized);
    }
    let opened_snapshot = snapshot_std(&opened_metadata)
        .map_err(|_| PrivateFileReadFailure::MetadataUnavailable)?;
    if opened_snapshot != path_snapshot_before {
        return Err(PrivateFileReadFailure::ChangedDuringRead);
    }

    let bytes = read_bytes_to_finite_cap(&mut file, max_bytes)?;

    let handle_metadata_after = file
        .metadata()
        .map_err(|_| PrivateFileReadFailure::MetadataUnavailable)?;
    if !handle_metadata_after.is_file() || handle_metadata_after.len() > max_bytes {
        return Err(PrivateFileReadFailure::ChangedDuringRead);
    }
    let handle_snapshot_after = snapshot_std(&handle_metadata_after)
        .map_err(|_| PrivateFileReadFailure::MetadataUnavailable)?;
    let path_metadata_after = directory
        .symlink_metadata(name)
        .map_err(|_| PrivateFileReadFailure::ChangedDuringRead)?;
    if !path_metadata_after.is_file() || path_metadata_after.len() > max_bytes {
        return Err(PrivateFileReadFailure::ChangedDuringRead);
    }
    let path_snapshot_after = snapshot_cap(&path_metadata_after)
        .map_err(|_| PrivateFileReadFailure::MetadataUnavailable)?;
    if handle_snapshot_after != opened_snapshot || path_snapshot_after != handle_snapshot_after {
        return Err(PrivateFileReadFailure::ChangedDuringRead);
    }

    Ok(Some(bytes))
}

fn read_bytes_to_finite_cap(
    reader: impl Read,
    max_bytes: u64,
) -> std::result::Result<Vec<u8>, PrivateFileReadFailure> {
    let mut bytes = Vec::new();
    reader
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| PrivateFileReadFailure::ReadUnavailable)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_bytes {
        return Err(PrivateFileReadFailure::Oversized);
    }
    Ok(bytes)
}

fn admit_storage_state_size(
    byte_len: u64,
) -> std::result::Result<(), StorageStateFailure> {
    if byte_len > STORAGE_STATE_MAX_BYTES {
        return Err(StorageStateFailure::Oversized);
    }
    Ok(())
}

fn admit_storage_state_document(
    bytes: &[u8],
) -> std::result::Result<(), StorageStateFailure> {
    admit_storage_state_size(u64::try_from(bytes.len()).unwrap_or(u64::MAX))?;
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| StorageStateFailure::InvalidJson)?;
    let object = value
        .as_object()
        .ok_or(StorageStateFailure::InvalidShape)?;
    for field in ["cookies", "origins"] {
        let entries = object
            .get(field)
            .and_then(serde_json::Value::as_array)
            .ok_or(StorageStateFailure::InvalidShape)?;
        if entries.iter().any(|entry| !entry.is_object()) {
            return Err(StorageStateFailure::InvalidShape);
        }
    }
    Ok(())
}

fn write_private_file_atomically(
    directory: &cap_std::fs::Dir,
    target_name: &str,
    bytes: &[u8],
) -> std::io::Result<()> {
    let mut last_collision = None;
    for _ in 0..PRIVATE_FILE_CREATE_ATTEMPTS {
        let nonce = PRIVATE_FILE_WRITE_NONCE.fetch_add(1, Ordering::Relaxed);
        let created_nanos = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let temp_name = format!(
            ".{target_name}.tmp-{}-{created_nanos}-{nonce}",
            std::process::id()
        );
        let mut options = cap_std::fs::OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        #[cfg(unix)]
        options.mode(0o600);
        let cap_file = match directory.open_with(&temp_name, &options) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                last_collision = Some(error);
                continue;
            }
            Err(error) => return Err(error),
        };
        let mut file = cap_file.into_std();
        let write_result = (|| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
            file.write_all(bytes)?;
            file.sync_all()?;
            directory.rename(&temp_name, directory, target_name)?;
            #[cfg(unix)]
            directory.try_clone()?.into_std_file().sync_all()?;

            let handle_snapshot = snapshot_std(&file.metadata()?)?;
            let path_metadata = directory.symlink_metadata(target_name)?;
            let expected_len = u64::try_from(bytes.len())
                .map_err(|_| private_io_error("private file length is not representable"))?;
            if !path_metadata.is_file() || path_metadata.len() != expected_len {
                return Err(private_io_error("private file replacement did not remain stable"));
            }
            let path_snapshot = snapshot_cap(&path_metadata)?;
            if path_snapshot != handle_snapshot {
                return Err(private_io_error("private file replacement changed after rename"));
            }
            Ok(())
        })();
        // Never remove the temporary path by name after a failure: another
        // actor could replace that directory entry between the failed
        // operation and cleanup. A rare 0600 orphan inside the 0700 profile
        // directory is safer than deleting an object whose identity is no
        // longer authoritative.
        return write_result;
    }

    Err(last_collision.unwrap_or_else(|| private_io_error("private temporary file unavailable")))
}

#[cfg(test)]
fn admit_profile_metadata_shape_and_size(
    is_file: bool,
    byte_len: u64,
) -> std::result::Result<(), ProfileMetadataReadFailure> {
    if !is_file {
        return Err(ProfileMetadataReadFailure::NotRegularFile);
    }
    if byte_len > PROFILE_METADATA_MAX_BYTES {
        return Err(ProfileMetadataReadFailure::Oversized);
    }
    Ok(())
}

#[cfg(test)]
fn read_profile_metadata_bounded(
    reader: impl Read,
) -> std::result::Result<Vec<u8>, ProfileMetadataReadFailure> {
    let mut bytes = Vec::new();
    reader
        .take(PROFILE_METADATA_MAX_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| ProfileMetadataReadFailure::ReadUnavailable)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > PROFILE_METADATA_MAX_BYTES {
        return Err(ProfileMetadataReadFailure::Oversized);
    }
    Ok(bytes)
}

/// Resolved browser profile directory for a service+account pair.
#[derive(Debug, Clone)]
pub struct BrowserProfile {
    /// Root profiles directory (e.g. `~/.local/share/wa/browser_profiles`)
    profiles_root: PathBuf,
    /// Service identifier (e.g. "openai", "anthropic", "google")
    service: String,
    /// Account identifier (e.g. account name or hash)
    account: String,
}

impl BrowserProfile {
    /// Create a new profile reference.
    ///
    /// Does NOT create the directory on disk — call `ensure_dir()` for that.
    #[must_use]
    pub fn new(profiles_root: impl Into<PathBuf>, service: &str, account: &str) -> Self {
        Self {
            profiles_root: profiles_root.into(),
            service: encode_profile_path_component(service),
            account: encode_profile_path_component(account),
        }
    }

    fn from_storage_components(
        profiles_root: impl Into<PathBuf>,
        service: &str,
        account: &str,
    ) -> std::io::Result<Self> {
        validate_profile_storage_component(service)?;
        validate_profile_storage_component(account)?;
        Ok(Self {
            profiles_root: profiles_root.into(),
            service: service.to_string(),
            account: account.to_string(),
        })
    }

    /// Canonical on-disk service identity.
    #[must_use]
    pub fn service(&self) -> &str {
        &self.service
    }

    /// Canonical on-disk account identity.
    #[must_use]
    pub fn account(&self) -> &str {
        &self.account
    }

    /// Full path to this profile's directory.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.profiles_root.join(&self.service).join(&self.account)
    }

    fn validate_components(&self) -> std::io::Result<()> {
        validate_profile_storage_component(&self.service)?;
        validate_profile_storage_component(&self.account)
    }

    fn open_profile_dir_capability(&self) -> std::io::Result<cap_std::fs::Dir> {
        self.validate_components()?;
        let root = open_profiles_root_capability(&self.profiles_root)?;
        let service = open_child_directory_nofollow(&root, &self.service)?;
        open_child_directory_nofollow(&service, &self.account)
    }

    fn ensure_profile_dir_capability(&self) -> std::io::Result<cap_std::fs::Dir> {
        self.validate_components()?;
        let root = ensure_profiles_root_capability(&self.profiles_root)?;
        let service = ensure_child_directory_nofollow(&root, &self.service)?;
        ensure_child_directory_nofollow(&service, &self.account)
    }

    /// Ensure the profile directory exists on disk.
    pub fn ensure_dir(&self) -> Result<PathBuf> {
        let dir = self.path();
        self.ensure_profile_dir_capability().map_err(|_| {
            StorageError::Database(
                "Browser profile directory could not be created safely".to_string(),
            )
        })?;

        tracing::debug!("Browser profile directory ensured");

        Ok(dir)
    }

    /// Check whether the profile directory is safely admitted and accessible.
    ///
    /// Symlinked, non-directory, raced, or inaccessible components return
    /// `false` even if some raw directory entry exists on disk.
    #[must_use]
    pub fn exists(&self) -> bool {
        self.open_profile_dir_capability().is_ok()
    }

    /// Path to the profile metadata file.
    #[must_use]
    pub fn metadata_path(&self) -> PathBuf {
        self.path().join(PROFILE_METADATA_FILE_NAME)
    }

    /// Path to the exported Playwright storage state file.
    ///
    /// This contains cookies and localStorage, enabling session restoration
    /// without re-authenticating.
    #[must_use]
    pub fn storage_state_path(&self) -> PathBuf {
        self.path().join(STORAGE_STATE_FILE_NAME)
    }

    /// Check whether safely admitted storage state exists for this profile.
    ///
    /// This returns `false` for symlinks, non-files, inaccessible state, and
    /// files larger than [`STORAGE_STATE_MAX_BYTES`].
    #[must_use]
    pub fn has_storage_state(&self) -> bool {
        self.open_profile_dir_capability()
            .is_ok_and(|directory| {
                private_file_is_safely_accessible(
                    &directory,
                    STORAGE_STATE_FILE_NAME,
                    STORAGE_STATE_MAX_BYTES,
                )
            })
    }

    /// Write profile metadata to disk.
    ///
    /// The metadata file tracks when the profile was bootstrapped,
    /// the method used, and when it was last used.
    pub fn write_metadata(&self, metadata: &ProfileMetadata) -> Result<()> {
        let json = serialize_profile_metadata_bounded(metadata, &self.service, &self.account)
            .map_err(ProfileMetadataReadFailure::into_storage_error)?;
        let directory = self.open_profile_dir_capability().map_err(|_| {
            StorageError::Database("Browser profile metadata could not be written".to_string())
        })?;
        write_private_file_atomically(&directory, PROFILE_METADATA_FILE_NAME, json.as_bytes())
            .map_err(|_| {
                StorageError::Database(
                    "Browser profile metadata could not be written safely".to_string(),
                )
            })?;

        tracing::debug!(bytes = json.len(), "Profile metadata written");
        Ok(())
    }

    /// Read profile metadata from disk.
    ///
    /// Returns `None` if the metadata file does not exist. Existing metadata
    /// must be a regular file within [`PROFILE_METADATA_MAX_BYTES`], remain
    /// within that cap across the open/read race, match the fixed
    /// [`ProfileMetadata`] schema, and identify this exact profile. Failure
    /// details are content-free and never echo paths, OS errors, or file data.
    pub fn read_metadata(&self) -> Result<Option<ProfileMetadata>> {
        let directory = match self.open_profile_dir_capability() {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => {
                return Err(ProfileMetadataReadFailure::ReadUnavailable
                    .into_storage_error()
                    .into());
            }
        };
        let data = read_private_file_bounded(
            &directory,
            PROFILE_METADATA_FILE_NAME,
            PROFILE_METADATA_MAX_BYTES,
        )
        .map_err(ProfileMetadataReadFailure::from_private_file)
        .map_err(ProfileMetadataReadFailure::into_storage_error)?;
        let Some(data) = data else {
            return Ok(None);
        };
        let meta = parse_profile_metadata_bytes(&data, &self.service, &self.account)
            .map_err(ProfileMetadataReadFailure::into_storage_error)?;
        Ok(Some(meta))
    }

    /// Save Playwright storage state (cookies + localStorage) to the profile.
    ///
    /// The content should be the JSON output from Playwright's
    /// `context.storageState()` call.
    pub fn save_storage_state(&self, state_json: &[u8]) -> Result<()> {
        admit_storage_state_document(state_json)
            .map_err(StorageStateFailure::into_storage_error)?;
        let directory = self.open_profile_dir_capability().map_err(|_| {
            StorageStateFailure::WriteUnavailable.into_storage_error()
        })?;
        write_private_file_atomically(&directory, STORAGE_STATE_FILE_NAME, state_json)
            .map_err(|_| StorageStateFailure::WriteUnavailable.into_storage_error())?;

        tracing::debug!(bytes = state_json.len(), "Storage state saved");
        Ok(())
    }

    /// Load Playwright storage state from the profile.
    ///
    /// Returns `None` if no storage state has been saved.
    pub fn load_storage_state(&self) -> Result<Option<Vec<u8>>> {
        let directory = match self.open_profile_dir_capability() {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => {
                return Err(StorageStateFailure::ReadUnavailable
                    .into_storage_error()
                    .into());
            }
        };
        let data = read_private_file_bounded(
            &directory,
            STORAGE_STATE_FILE_NAME,
            STORAGE_STATE_MAX_BYTES,
        )
        .map_err(StorageStateFailure::from_private_file)
        .map_err(StorageStateFailure::into_storage_error)?;
        if let Some(bytes) = data.as_deref() {
            admit_storage_state_document(bytes)
                .map_err(StorageStateFailure::into_storage_error)?;
        }
        Ok(data)
    }

    /// Validate the persisted storage-state document without exposing it.
    ///
    /// `Ok(Missing)` and `Ok(Valid)` are authoritative outcomes. Any unsafe,
    /// malformed, raced, oversized, or inaccessible state returns a
    /// content-free error instead of being mistaken for an authenticated
    /// profile.
    pub fn validate_storage_state(&self) -> Result<StorageStateValidation> {
        match self.load_storage_state()? {
            Some(_) => Ok(StorageStateValidation::Valid),
            None => Ok(StorageStateValidation::Missing),
        }
    }

    /// Persist a successful browser authentication result and its metadata.
    pub fn record_authenticated_state(
        &self,
        state_json: &[u8],
        method: BootstrapMethod,
    ) -> Result<()> {
        // Refuse to overwrite state when existing metadata is malformed or its
        // identity disagrees with this profile.
        let mut metadata = self
            .read_metadata()?
            .unwrap_or_else(|| ProfileMetadata::new(&self.service, &self.account));
        if metadata.bootstrapped_at.is_some() {
            metadata.record_use();
        } else {
            metadata.record_bootstrap(method);
        }
        // State is the authority required for reuse. Commit it first; if the
        // metadata replacement subsequently fails, callers receive an error
        // and never report the operation as fully successful.
        self.save_storage_state(state_json)?;
        self.write_metadata(&metadata)
    }
}

/// Validation outcome for a browser profile's persisted storage state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageStateValidation {
    /// No storage-state file exists.
    Missing,
    /// A bounded, stable, schema-valid storage-state document exists.
    Valid,
}

/// Maximum directory entries examined by one profile-discovery operation.
pub const BROWSER_PROFILE_DISCOVERY_MAX_ENTRIES: usize = 4096;

/// Capability-safe bounded browser-profile discovery result.
#[derive(Debug, Clone)]
pub struct BrowserProfileDiscovery {
    /// Safely opened service/account profile identities, sorted deterministically.
    pub profiles: Vec<BrowserProfile>,
    /// Service and account directory entries examined under the fixed budget.
    pub entries_examined: usize,
    /// Entries that could not be classified or safely opened.
    pub unclassified_entries: usize,
    /// True when additional entries may exist beyond the fixed budget.
    pub truncated: bool,
}

impl BrowserProfileDiscovery {
    /// Whether the complete observed tree was classified within the budget.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        !self.truncated && self.unclassified_entries == 0
    }
}

/// Discover browser profiles without following service or account symlinks.
///
/// A missing root is a complete empty result. Other root-level failures return
/// a content-free error. Per-entry failures remain visible through
/// `unclassified_entries`, and budget exhaustion is explicit via `truncated`.
pub fn discover_browser_profiles_bounded(profiles_root: &Path) -> Result<BrowserProfileDiscovery> {
    discover_browser_profiles_with_budget(profiles_root, BROWSER_PROFILE_DISCOVERY_MAX_ENTRIES)
}

fn discover_browser_profiles_with_budget(
    profiles_root: &Path,
    max_entries: usize,
) -> Result<BrowserProfileDiscovery> {
    let root = match open_profiles_root_capability(profiles_root) {
        Ok(root) => root,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BrowserProfileDiscovery {
                profiles: Vec::new(),
                entries_examined: 0,
                unclassified_entries: 0,
                truncated: false,
            });
        }
        Err(_) => {
            return Err(StorageError::Database(
                "Browser profile directory could not be inspected safely".to_string(),
            )
            .into());
        }
    };
    let service_entries = root.entries().map_err(|_| {
        StorageError::Database(
            "Browser profile directory could not be inspected safely".to_string(),
        )
    })?;
    let mut discovery = BrowserProfileDiscovery {
        profiles: Vec::new(),
        entries_examined: 0,
        unclassified_entries: 0,
        truncated: false,
    };

    'services: for service_entry in service_entries {
        if discovery.entries_examined >= max_entries {
            discovery.truncated = true;
            break;
        }
        discovery.entries_examined = discovery.entries_examined.saturating_add(1);
        let service_entry = match service_entry {
            Ok(entry) => entry,
            Err(_) => {
                discovery.unclassified_entries =
                    discovery.unclassified_entries.saturating_add(1);
                continue;
            }
        };
        let service_type = match service_entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => {
                discovery.unclassified_entries =
                    discovery.unclassified_entries.saturating_add(1);
                continue;
            }
        };
        if !service_type.is_dir() {
            if service_type.is_symlink() {
                discovery.unclassified_entries =
                    discovery.unclassified_entries.saturating_add(1);
            }
            continue;
        }
        let service_name = service_entry.file_name();
        let Some(service) = service_name.to_str() else {
            discovery.unclassified_entries = discovery.unclassified_entries.saturating_add(1);
            continue;
        };
        if validate_profile_storage_component(service).is_err() {
            discovery.unclassified_entries = discovery.unclassified_entries.saturating_add(1);
            continue;
        }
        let service_directory = match open_child_directory_nofollow(&root, service) {
            Ok(directory) => directory,
            Err(_) => {
                discovery.unclassified_entries =
                    discovery.unclassified_entries.saturating_add(1);
                continue;
            }
        };
        let account_entries = match service_directory.entries() {
            Ok(entries) => entries,
            Err(_) => {
                discovery.unclassified_entries =
                    discovery.unclassified_entries.saturating_add(1);
                continue;
            }
        };
        for account_entry in account_entries {
            if discovery.entries_examined >= max_entries {
                discovery.truncated = true;
                break 'services;
            }
            discovery.entries_examined = discovery.entries_examined.saturating_add(1);
            let account_entry = match account_entry {
                Ok(entry) => entry,
                Err(_) => {
                    discovery.unclassified_entries =
                        discovery.unclassified_entries.saturating_add(1);
                    continue;
                }
            };
            let account_type = match account_entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => {
                    discovery.unclassified_entries =
                        discovery.unclassified_entries.saturating_add(1);
                    continue;
                }
            };
            if !account_type.is_dir() {
                if account_type.is_symlink() {
                    discovery.unclassified_entries =
                        discovery.unclassified_entries.saturating_add(1);
                }
                continue;
            }
            let account_name = account_entry.file_name();
            let Some(account) = account_name.to_str() else {
                discovery.unclassified_entries =
                    discovery.unclassified_entries.saturating_add(1);
                continue;
            };
            let profile = match BrowserProfile::from_storage_components(
                profiles_root,
                service,
                account,
            ) {
                Ok(profile) => profile,
                Err(_) => {
                    discovery.unclassified_entries =
                        discovery.unclassified_entries.saturating_add(1);
                    continue;
                }
            };
            if profile.open_profile_dir_capability().is_err() {
                discovery.unclassified_entries =
                    discovery.unclassified_entries.saturating_add(1);
                continue;
            }
            discovery.profiles.push(profile);
        }
    }
    discovery.profiles.sort_by(|left, right| {
        left.service
            .cmp(&right.service)
            .then_with(|| left.account.cmp(&right.account))
    });
    Ok(discovery)
}

// =============================================================================
// Profile Metadata
// =============================================================================

/// Metadata about a browser profile's bootstrap and usage history.
///
/// Stored as `.wa_profile.json` inside the profile directory.
/// The schema is not intended to contain secrets, but persisted bytes are
/// treated as untrusted until bounded parsing and identity validation succeed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileMetadata {
    /// Service this profile is for (e.g., "openai", "anthropic").
    pub service: String,
    /// Account identifier.
    pub account: String,
    /// ISO 8601 timestamp of when this profile was first bootstrapped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bootstrapped_at: Option<String>,
    /// Method used for the last bootstrap ("interactive" or "automated").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bootstrap_method: Option<BootstrapMethod>,
    /// ISO 8601 timestamp of the last successful use.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<String>,
    /// Number of successful automated uses since last bootstrap.
    #[serde(default)]
    pub automated_use_count: u64,
}

fn parse_profile_metadata_bytes(
    bytes: &[u8],
    expected_service: &str,
    expected_account: &str,
) -> std::result::Result<ProfileMetadata, ProfileMetadataReadFailure> {
    let metadata: ProfileMetadata = serde_json::from_slice(bytes)
        .map_err(|_| ProfileMetadataReadFailure::InvalidSchema)?;
    validate_profile_metadata(&metadata, expected_service, expected_account)?;
    Ok(metadata)
}

fn validate_profile_metadata(
    metadata: &ProfileMetadata,
    expected_service: &str,
    expected_account: &str,
) -> std::result::Result<(), ProfileMetadataReadFailure> {
    if metadata.service != expected_service || metadata.account != expected_account {
        return Err(ProfileMetadataReadFailure::IdentityMismatch);
    }
    for timestamp in [
        metadata.bootstrapped_at.as_deref(),
        metadata.last_used_at.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        chrono::DateTime::parse_from_rfc3339(timestamp)
            .map_err(|_| ProfileMetadataReadFailure::InvalidTimestamp)?;
    }
    Ok(())
}

fn serialize_profile_metadata_bounded(
    metadata: &ProfileMetadata,
    expected_service: &str,
    expected_account: &str,
) -> std::result::Result<String, ProfileMetadataReadFailure> {
    validate_profile_metadata(metadata, expected_service, expected_account)?;
    let json = serde_json::to_string_pretty(metadata)
        .map_err(|_| ProfileMetadataReadFailure::InvalidSchema)?;
    if u64::try_from(json.len()).unwrap_or(u64::MAX) > PROFILE_METADATA_MAX_BYTES {
        return Err(ProfileMetadataReadFailure::Oversized);
    }
    Ok(json)
}

impl ProfileMetadata {
    /// Create new metadata for a fresh profile.
    #[must_use]
    pub fn new(service: &str, account: &str) -> Self {
        Self {
            service: service.to_string(),
            account: account.to_string(),
            bootstrapped_at: None,
            bootstrap_method: None,
            last_used_at: None,
            automated_use_count: 0,
        }
    }

    /// Record a successful bootstrap.
    pub fn record_bootstrap(&mut self, method: BootstrapMethod) {
        let now = chrono::Utc::now().to_rfc3339();
        self.bootstrapped_at = Some(now.clone());
        self.bootstrap_method = Some(method);
        self.last_used_at = Some(now);
    }

    /// Record a successful automated use.
    pub fn record_use(&mut self) {
        self.last_used_at = Some(chrono::Utc::now().to_rfc3339());
        self.automated_use_count = self.automated_use_count.saturating_add(1);
    }
}

/// How a browser profile was bootstrapped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BootstrapMethod {
    /// User completed login interactively in a visible browser window.
    #[serde(rename = "interactive")]
    Interactive,
    /// Login was completed automatically (e.g., already authenticated).
    #[serde(rename = "automated")]
    Automated,
}

/// Resolve the profiles root directory from the data directory.
///
/// Returns `<data_dir>/browser_profiles`.
#[must_use]
pub fn profiles_root_from_data_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("browser_profiles")
}

const HASHED_PROFILE_COMPONENT_PREFIX: &str = "h-";
const HASHED_PROFILE_COMPONENT_HEX_BYTES: usize = 62;

fn profile_component_can_be_stored_verbatim(component: &str) -> bool {
    !component.is_empty()
        && component.len() <= 64
        && !matches!(component, "." | "..")
        && !component.starts_with(HASHED_PROFILE_COMPONENT_PREFIX)
        && component
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn validate_profile_storage_component(component: &str) -> std::io::Result<()> {
    if component.is_empty()
        || component.len() > 64
        || matches!(component, "." | "..")
        || !component
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(private_io_error("browser profile identity is not canonical"));
    }
    Ok(())
}

/// Encode an arbitrary logical identity as one safe, bounded path component.
///
/// Already-safe short identifiers remain readable. Every other input receives
/// a reserved `h-` prefix and a 248-bit SHA-256-derived identity. Reserving the
/// prefix prevents a literal safe identifier from aliasing a hashed one, while
/// hashing rather than lossy character replacement prevents accounts such as
/// `a/b` and `a?b` from sharing a browser profile.
#[must_use]
fn encode_profile_path_component(identity: &str) -> String {
    use sha2::Digest as _;

    if profile_component_can_be_stored_verbatim(identity) {
        return identity.to_string();
    }
    let digest = sha2::Sha256::digest(identity.as_bytes());
    let encoded = hex::encode(digest);
    format!(
        "{HASHED_PROFILE_COMPONENT_PREFIX}{}",
        &encoded[..HASHED_PROFILE_COMPONENT_HEX_BYTES]
    )
}

// =============================================================================
// Browser Context (lazy initialization)
// =============================================================================

/// Status of the browser automation runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserStatus {
    /// Not yet initialized.
    NotInitialized,
    /// Ready to use.
    Ready,
    /// Failed to initialize.
    Failed(String),
}

/// Browser automation context with lazy initialization.
///
/// The actual Playwright process is not started until `ensure_ready()` is called.
/// This avoids unnecessary overhead when browser features are not used.
pub struct BrowserContext {
    config: BrowserConfig,
    profiles_root: PathBuf,
    pub(crate) status: BrowserStatus,
}

impl BrowserContext {
    /// Create a new browser context (does NOT start Playwright).
    #[must_use]
    pub fn new(config: BrowserConfig, data_dir: &Path) -> Self {
        Self {
            config,
            profiles_root: profiles_root_from_data_dir(data_dir),
            status: BrowserStatus::NotInitialized,
        }
    }

    /// Current browser status.
    #[must_use]
    pub fn status(&self) -> &BrowserStatus {
        &self.status
    }

    /// Current configuration.
    #[must_use]
    pub fn config(&self) -> &BrowserConfig {
        &self.config
    }

    /// Profiles root directory.
    #[must_use]
    pub fn profiles_root(&self) -> &Path {
        &self.profiles_root
    }

    /// Get a profile reference for a service+account.
    #[must_use]
    pub fn profile(&self, service: &str, account: &str) -> BrowserProfile {
        BrowserProfile::new(&self.profiles_root, service, account)
    }

    /// Lazily initialize the browser automation runtime.
    ///
    /// Checks that the Playwright CLI is available and the profiles root
    /// directory can be created. Does NOT launch a browser — that happens
    /// on first use.
    pub fn ensure_ready(&mut self) -> Result<()> {
        if self.status == BrowserStatus::Ready {
            return Ok(());
        }

        if self.config.browser_type != "chromium"
            || admit_browser_timeout(self.config.navigation_timeout_ms).is_err()
            || admit_browser_timeout(self.config.page_load_timeout_ms).is_err()
            || admit_browser_timeout(self.config.readiness_probe_timeout_ms).is_err()
        {
            let msg = "Browser automation configuration is invalid".to_string();
            self.status = BrowserStatus::Failed(msg.clone());
            return Err(StorageError::Database(msg).into());
        }

        tracing::info!(headless = self.config.headless, "Initializing browser automation context");

        // Ensure profiles root exists
        ensure_profiles_root_capability(&self.profiles_root).map_err(|_| {
            let msg = "Browser profiles directory could not be created safely".to_string();
            self.status = BrowserStatus::Failed(msg.clone());
            StorageError::Database(msg)
        })?;

        // Check Playwright CLI availability
        match check_playwright_available(std::time::Duration::from_millis(
            self.config.readiness_probe_timeout_ms,
        )) {
            Ok(version) => {
                tracing::info!(version_bytes = version.len(), "Playwright CLI available");
            }
            Err(_) => {
                let msg = "Playwright CLI is unavailable".to_string();
                tracing::warn!("Playwright CLI is unavailable");
                self.status = BrowserStatus::Failed(msg.clone());
                return Err(StorageError::Database(msg).into());
            }
        }

        self.status = BrowserStatus::Ready;
        Ok(())
    }
}

/// Check if the Playwright CLI is available and return its version.
fn check_playwright_available(timeout: std::time::Duration) -> std::result::Result<String, String> {
    const VERSION_OUTPUT_LIMIT_BYTES: usize = 4 * 1024;
    let mut command = crate::runtime_async::process::Command::new("npx");
    command
        .args(["--no-install", "--offline", "playwright", "--version"])
        .stdout_limit(VERSION_OUTPUT_LIMIT_BYTES)
        .stderr_limit(VERSION_OUTPUT_LIMIT_BYTES);
    let output = command
        .output_blocking(timeout)
        .map_err(|_| "Playwright version probe could not be started".to_string())?;

    if !output.status.success() {
        return Err("Playwright version probe was unsuccessful".to_string());
    }
    let version = String::from_utf8(output.stdout)
        .map_err(|_| "Playwright version probe returned invalid text".to_string())?;
    let version = version.trim().to_string();
    Ok(version)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical_system_temp_dir() -> PathBuf {
        std::fs::canonicalize(std::env::temp_dir())
            .expect("system temporary directory must be resolvable")
    }

    // =========================================================================
    // BrowserConfig tests
    // =========================================================================

    #[test]
    fn config_defaults() {
        let cfg = BrowserConfig::default();
        assert!(!cfg.headless);
        assert_eq!(cfg.navigation_timeout_ms, 30_000);
        assert_eq!(cfg.page_load_timeout_ms, 60_000);
        assert_eq!(cfg.readiness_probe_timeout_ms, 10_000);
        assert_eq!(cfg.browser_type, "chromium");
    }

    #[test]
    fn config_serde_round_trip() {
        let cfg = BrowserConfig {
            headless: true,
            navigation_timeout_ms: 15_000,
            page_load_timeout_ms: 45_000,
            readiness_probe_timeout_ms: 5_000,
            browser_type: "firefox".to_string(),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let deserialized: BrowserConfig = serde_json::from_str(&json).unwrap();
        assert!(deserialized.headless);
        assert_eq!(deserialized.navigation_timeout_ms, 15_000);
        assert_eq!(deserialized.readiness_probe_timeout_ms, 5_000);
        assert_eq!(deserialized.browser_type, "firefox");
    }

    #[test]
    fn config_serde_defaults_on_missing() {
        let json = "{}";
        let cfg: BrowserConfig = serde_json::from_str(json).unwrap();
        assert!(!cfg.headless);
        assert_eq!(cfg.navigation_timeout_ms, 30_000);
    }

    #[test]
    fn browser_configuration_admission_is_finite_and_precedes_readiness_probe() {
        assert_eq!(admit_browser_timeout(1), Ok(()));
        assert_eq!(
            admit_browser_timeout(0),
            Err(BrowserNodeCommandFailure::InvalidTimeout)
        );
        assert_eq!(
            admit_browser_timeout(BROWSER_NODE_MAX_FLOW_TIMEOUT_MS.saturating_add(1)),
            Err(BrowserNodeCommandFailure::InvalidTimeout)
        );
        assert_eq!(admit_browser_poll_interval(1, 1), Ok(()));
        assert_eq!(
            admit_browser_poll_interval(0, 1),
            Err(BrowserNodeCommandFailure::InvalidPollInterval)
        );
        assert_eq!(admit_browser_url("https://example.com/login"), Ok(()));
        assert_eq!(admit_browser_url("http://localhost/login"), Ok(()));
        assert_eq!(
            admit_browser_url("file:///tmp/login"),
            Err(BrowserNodeCommandFailure::InvalidConfiguration)
        );
        assert_eq!(
            admit_browser_url("https://user:secret@example.com/login"),
            Err(BrowserNodeCommandFailure::InvalidConfiguration)
        );

        let selectors = vec!["body"; BROWSER_NODE_INPUT_MAX_LIST_ENTRIES.saturating_add(1)]
            .join(", ");
        assert_eq!(
            admit_selector_group(&selectors),
            Err(BrowserNodeCommandFailure::InvalidConfiguration)
        );

        let temp = tempfile::tempdir().expect("isolated invalid browser config root");
        let mut context = BrowserContext::new(
            BrowserConfig {
                browser_type: "firefox".to_string(),
                ..BrowserConfig::default()
            },
            temp.path(),
        );
        assert!(context.ensure_ready().is_err());
        assert!(matches!(context.status(), BrowserStatus::Failed(_)));
        assert!(!context.profiles_root().exists());
    }

    #[test]
    fn browser_node_failure_contract_is_finite_and_content_free() {
        for failure in [
            BrowserNodeCommandFailure::InvalidTimeout,
            BrowserNodeCommandFailure::InvalidPollInterval,
            BrowserNodeCommandFailure::InvalidConfiguration,
            BrowserNodeCommandFailure::ScriptOversized,
            BrowserNodeCommandFailure::InputWriteFailed,
            BrowserNodeCommandFailure::TimedOut,
            BrowserNodeCommandFailure::Cancelled,
            BrowserNodeCommandFailure::OutputOversized,
            BrowserNodeCommandFailure::CaptureIncomplete,
            BrowserNodeCommandFailure::CleanupIncomplete,
            BrowserNodeCommandFailure::Unavailable,
        ] {
            let detail = failure.detail();
            assert!(detail.len() <= 96);
            assert!(!detail.contains("AKIAIOSFODNN7EXAMPLE"));
            assert!(!detail.contains('/'));
            assert!(!detail.contains('\\'));
            assert!(!detail.contains('\n'));
            assert!(!detail.contains('\u{202e}'));
        }
        assert!(BROWSER_NODE_SCRIPT_MAX_BYTES <= 1024 * 1024);
        assert!(BROWSER_BOOTSTRAP_MAX_STDOUT_BYTES >= STORAGE_STATE_MAX_BYTES as usize * 2);
    }

    #[test]
    fn browser_node_input_admission_enforces_exact_and_one_over_boundaries() {
        let exact_field = "x".repeat(BROWSER_NODE_INPUT_MAX_FIELD_BYTES);
        assert_eq!(
            admit_node_script_input_parts(&[Some(&exact_field)], &[], &[]),
            Ok(())
        );
        let oversized_field = "x".repeat(BROWSER_NODE_INPUT_MAX_FIELD_BYTES + 1);
        assert_eq!(
            admit_node_script_input_parts(&[Some(&oversized_field)], &[], &[]),
            Err(BrowserNodeCommandFailure::ScriptOversized)
        );
        let exact_path = PathBuf::from("p".repeat(BROWSER_NODE_INPUT_MAX_FIELD_BYTES));
        assert_eq!(
            admit_node_script_input_parts(&[], &[Some(exact_path.as_path())], &[]),
            Ok(())
        );
        let oversized_path =
            PathBuf::from("p".repeat(BROWSER_NODE_INPUT_MAX_FIELD_BYTES.saturating_add(1)));
        assert_eq!(
            admit_node_script_input_parts(&[], &[Some(oversized_path.as_path())], &[]),
            Err(BrowserNodeCommandFailure::ScriptOversized)
        );

        let remaining = "y".repeat(
            BROWSER_NODE_INPUT_MAX_TOTAL_BYTES - BROWSER_NODE_INPUT_MAX_FIELD_BYTES,
        );
        assert_eq!(
            admit_node_script_input_parts(
                &[Some(&exact_field), Some(&remaining)],
                &[],
                &[],
            ),
            Ok(())
        );
        let total_one_over = format!("{remaining}z");
        assert_eq!(
            admit_node_script_input_parts(
                &[Some(&exact_field), Some(&total_one_over)],
                &[],
                &[],
            ),
            Err(BrowserNodeCommandFailure::ScriptOversized)
        );

        let exact_list = vec![String::new(); BROWSER_NODE_INPUT_MAX_LIST_ENTRIES];
        assert_eq!(
            admit_node_script_input_parts(&[], &[], &[exact_list.as_slice()]),
            Ok(())
        );
        let oversized_list =
            vec![String::new(); BROWSER_NODE_INPUT_MAX_LIST_ENTRIES.saturating_add(1)];
        assert_eq!(
            admit_node_script_input_parts(&[], &[], &[oversized_list.as_slice()]),
            Err(BrowserNodeCommandFailure::ScriptOversized)
        );

        assert_eq!(
            admit_node_script_source_size(BROWSER_NODE_SCRIPT_MAX_BYTES),
            Ok(())
        );
        assert_eq!(
            admit_node_script_source_size(BROWSER_NODE_SCRIPT_MAX_BYTES.saturating_add(1)),
            Err(BrowserNodeCommandFailure::ScriptOversized)
        );
        assert_eq!(
            BROWSER_BOOTSTRAP_MAX_STDOUT_BYTES,
            (STORAGE_STATE_MAX_BYTES as usize) * 2 + 64 * 1024
        );
    }

    #[test]
    fn browser_process_sources_use_bounded_supervision_without_inline_script_argv() {
        let source = include_str!("mod.rs");
        let runner_start = source
            .find("pub(super) fn run_node_script_bounded(")
            .expect("node runner source");
        let runner_tail = &source[runner_start..];
        let runner_end = runner_tail
            .find("\n#[derive(Debug, Clone, Copy, PartialEq, Eq)]\nenum ProfileMetadataReadFailure")
            .expect("node runner source boundary");
        let runner = &runner_tail[..runner_end];
        assert!(runner.contains("Command::new(\"node\")"));
        assert!(runner.contains(".arg(\"-\")"));
        assert!(runner.contains(".stdin_limit("));
        assert!(runner.contains(".stdin_bytes("));
        assert!(runner.contains(".stdout_limit("));
        assert!(runner.contains(".stderr_limit("));
        assert!(runner.contains(".output_blocking("));
        assert!(!runner.contains(".arg(\"-e\")"));
        assert!(!runner.contains(".env("));
        assert!(!runner.contains("tempfile"));

        let probe_start = source
            .find("fn check_playwright_available(")
            .expect("Playwright probe source");
        let probe_tail = &source[probe_start..];
        let probe_end = probe_tail
            .find("\n}\n\n// =============================================================================\n// Tests")
            .expect("Playwright probe source boundary");
        let probe = &probe_tail[..probe_end];
        assert!(probe.contains("runtime_async::process::Command"));
        assert!(probe.contains("output_blocking"));
        assert!(!probe.contains("std::process::Command::new(\"npx\")"));
    }

    // =========================================================================
    // Profile path resolution tests
    // =========================================================================

    #[test]
    fn profile_path_resolution() {
        let root = PathBuf::from("/home/user/.local/share/wa");
        let profiles_root = profiles_root_from_data_dir(&root);
        let profile = BrowserProfile::new(&profiles_root, "openai", "my-account");

        let expected =
            PathBuf::from("/home/user/.local/share/wa/browser_profiles/openai/my-account");
        assert_eq!(profile.path(), expected);
    }

    #[test]
    fn profile_path_different_services() {
        let profiles_root = PathBuf::from("/data/browser_profiles");

        let p1 = BrowserProfile::new(&profiles_root, "openai", "account-1");
        let p2 = BrowserProfile::new(&profiles_root, "anthropic", "account-1");
        let p3 = BrowserProfile::new(&profiles_root, "google", "work-acct");

        assert_ne!(p1.path(), p2.path());
        assert_ne!(p2.path(), p3.path());
        assert!(p1.path().to_string_lossy().contains("openai"));
        assert!(p2.path().to_string_lossy().contains("anthropic"));
        assert!(p3.path().to_string_lossy().contains("google"));
    }

    #[test]
    fn profile_path_sanitization() {
        let profiles_root = PathBuf::from("/data/profiles");

        // Unsafe logical identities receive collision-resistant bounded names.
        let profile = BrowserProfile::new(&profiles_root, "my/service", "acct@email.com");
        let path = profile.path();
        let path_str = path.to_string_lossy();

        assert!(!path_str.contains("my/service"));
        assert!(profile.service().starts_with(HASHED_PROFILE_COMPONENT_PREFIX));
        assert!(profile.account().starts_with(HASHED_PROFILE_COMPONENT_PREFIX));
        assert_eq!(profile.service().len(), 64);
        assert_eq!(profile.account().len(), 64);
    }

    #[test]
    fn profile_path_traversal_prevention() {
        let profiles_root = PathBuf::from("/data/profiles");

        // Path traversal attempts should be sanitized
        let profile = BrowserProfile::new(&profiles_root, "../etc", "passwd");
        let path = profile.path();

        // Must still be under profiles_root
        assert!(path.starts_with("/data/profiles"));
        // .. should be sanitized to __
        assert!(!path.to_string_lossy().contains("../"));
    }

    #[test]
    fn encode_profile_path_component_alphanumeric() {
        assert_eq!(
            encode_profile_path_component("hello-world_123"),
            "hello-world_123"
        );
    }

    #[test]
    fn encoded_profile_components_do_not_alias_after_character_replacement() {
        let slash = encode_profile_path_component("a/b");
        let question = encode_profile_path_component("a?b");
        let literal_underscore = encode_profile_path_component("a_b");
        assert_ne!(slash, question);
        assert_ne!(slash, literal_underscore);
        assert_ne!(question, literal_underscore);
        assert!(slash.starts_with(HASHED_PROFILE_COMPONENT_PREFIX));
        assert_eq!(literal_underscore, "a_b");
    }

    #[test]
    fn encode_profile_path_component_dots_preserved() {
        assert_eq!(encode_profile_path_component("file.name"), "file.name");
        assert_eq!(encode_profile_path_component("v1.2.3"), "v1.2.3");
    }

    #[test]
    fn encode_profile_path_component_empty_is_bounded_and_nonempty() {
        let encoded = encode_profile_path_component("");
        assert!(encoded.starts_with(HASHED_PROFILE_COMPONENT_PREFIX));
        assert_eq!(encoded.len(), 64);
    }

    /// ft-klznn regression guard: previously '..' passed the
    /// allowlist (both '.' chars are in the allowlist), and the
    /// returned '..' then traversed out via Path::join.
    #[test]
    fn encode_profile_path_component_rejects_bare_double_dot() {
        let encoded = encode_profile_path_component("..");
        assert_ne!(encoded, "..");
        assert_eq!(encoded.len(), 64);
    }

    #[test]
    fn encode_profile_path_component_rejects_bare_single_dot() {
        let encoded = encode_profile_path_component(".");
        assert_ne!(encoded, ".");
        assert_eq!(encoded.len(), 64);
    }

    #[test]
    fn encode_profile_path_component_preserves_dot_files_and_versions() {
        assert_eq!(encode_profile_path_component("file.name"), "file.name");
        assert_eq!(encode_profile_path_component("v1.2.3"), "v1.2.3");
        assert_eq!(encode_profile_path_component(".hidden"), ".hidden");
        assert_eq!(encode_profile_path_component("..."), "...");
    }

    #[test]
    fn profile_path_does_not_escape_root_with_bare_double_dot() {
        // Direct end-to-end coverage of ft-klznn: a profile
        // constructed with bare '..' as service must NOT escape
        // profiles_root.
        let profiles_root = PathBuf::from("/data/profiles");
        let profile = BrowserProfile::new(&profiles_root, "..", "secrets");
        let path = profile.path();
        assert!(
            path.starts_with("/data/profiles"),
            "bare '..' service must not escape profiles_root, got {}",
            path.display()
        );
    }

    #[test]
    fn publicly_mutated_profile_components_fail_closed() {
        let profiles_root = PathBuf::from("/data/profiles");
        let mut profile = BrowserProfile::new(&profiles_root, "openai", "safe");
        profile.service = "../outside".to_string();
        assert!(profile.validate_components().is_err());

        profile.service = "openai".to_string();
        profile.account = ".".to_string();
        assert!(profile.validate_components().is_err());
    }

    // =========================================================================
    // Profile directory tests
    // =========================================================================

    #[test]
    fn profile_ensure_dir_creates_directory() {
        let temp =
            canonical_system_temp_dir().join(format!("wa_browser_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);

        let profile = BrowserProfile::new(&temp, "openai", "test-account");
        assert!(!profile.exists());

        let dir = profile.ensure_dir().unwrap();
        assert!(dir.is_dir());
        assert!(profile.exists());

        // Cleanup
        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn profile_ensure_dir_idempotent() {
        let temp = canonical_system_temp_dir().join(format!(
            "wa_browser_test_idempotent_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&temp);

        let profile = BrowserProfile::new(&temp, "openai", "test");
        profile.ensure_dir().unwrap();
        profile.ensure_dir().unwrap(); // Should not fail

        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn bounded_profile_discovery_is_sorted_and_explicitly_truncated() {
        let temp = tempfile::tempdir().expect("isolated discovery root");
        let profiles_root = temp.path().join("profiles");
        BrowserProfile::new(&profiles_root, "google", "work")
            .ensure_dir()
            .expect("google profile");
        BrowserProfile::new(&profiles_root, "anthropic", "default")
            .ensure_dir()
            .expect("Anthropic profile");

        let complete = discover_browser_profiles_with_budget(&profiles_root, 8)
            .expect("complete profile discovery");
        assert!(complete.is_complete());
        assert_eq!(complete.profiles.len(), 2);
        assert_eq!(complete.profiles[0].service(), "anthropic");
        assert_eq!(complete.profiles[1].service(), "google");

        let truncated = discover_browser_profiles_with_budget(&profiles_root, 1)
            .expect("bounded profile discovery");
        assert!(truncated.truncated);
        assert!(!truncated.is_complete());
        assert_eq!(truncated.entries_examined, 1);
    }

    #[test]
    fn bounded_profile_discovery_treats_a_missing_root_as_complete_and_empty() {
        let temp = tempfile::tempdir().expect("isolated missing discovery root");
        let missing = temp.path().join("missing");
        let discovery = discover_browser_profiles_bounded(&missing)
            .expect("missing profile root is an empty inventory");
        assert!(discovery.is_complete());
        assert!(discovery.profiles.is_empty());
        assert_eq!(discovery.entries_examined, 0);
    }

    #[cfg(unix)]
    #[test]
    fn bounded_profile_discovery_never_follows_service_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("isolated discovery symlink root");
        let profiles_root = temp.path().join("profiles");
        let outside = temp.path().join("outside");
        std::fs::create_dir(&profiles_root).expect("profiles root fixture");
        std::fs::create_dir(&outside).expect("outside fixture");
        std::fs::create_dir(outside.join("account")).expect("outside account fixture");
        symlink(&outside, profiles_root.join("openai")).expect("service symlink fixture");

        let discovery = discover_browser_profiles_bounded(&profiles_root)
            .expect("symlink-safe profile discovery");
        assert!(discovery.profiles.is_empty());
        assert_eq!(discovery.unclassified_entries, 1);
        assert!(!discovery.is_complete());
    }

    #[cfg(unix)]
    #[test]
    fn profile_dir_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temp = canonical_system_temp_dir().join(format!(
            "wa_browser_test_perms_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&temp);

        let profile = BrowserProfile::new(&temp, "openai", "secure");
        let dir = profile.ensure_dir().unwrap();

        let perms = std::fs::metadata(&dir).unwrap().permissions();
        assert_eq!(perms.mode() & 0o777, 0o700);

        let _ = std::fs::remove_dir_all(&temp);
    }

    // =========================================================================
    // BrowserContext tests
    // =========================================================================

    #[test]
    fn context_new_is_not_initialized() {
        let temp = canonical_system_temp_dir().join("wa_browser_ctx_test");
        let ctx = BrowserContext::new(BrowserConfig::default(), &temp);
        assert_eq!(*ctx.status(), BrowserStatus::NotInitialized);
    }

    #[test]
    fn context_profile_resolution() {
        let data_dir = PathBuf::from("/home/user/.local/share/wa");
        let ctx = BrowserContext::new(BrowserConfig::default(), &data_dir);

        let profile = ctx.profile("openai", "acct-1");
        assert_eq!(
            profile.path(),
            PathBuf::from("/home/user/.local/share/wa/browser_profiles/openai/acct-1")
        );
    }

    #[test]
    fn context_profiles_root() {
        let data_dir = PathBuf::from("/data/wa");
        let ctx = BrowserContext::new(BrowserConfig::default(), &data_dir);
        assert_eq!(ctx.profiles_root(), Path::new("/data/wa/browser_profiles"));
    }

    #[test]
    fn context_config_accessible() {
        let cfg = BrowserConfig {
            headless: true,
            ..Default::default()
        };
        let ctx = BrowserContext::new(cfg, Path::new("/tmp"));
        assert!(ctx.config().headless);
    }

    // =========================================================================
    // profiles_root_from_data_dir tests
    // =========================================================================

    #[test]
    fn profiles_root_linux() {
        let root = profiles_root_from_data_dir(Path::new("/home/user/.local/share/wa"));
        assert_eq!(
            root,
            PathBuf::from("/home/user/.local/share/wa/browser_profiles")
        );
    }

    #[test]
    fn profiles_root_custom() {
        let root = profiles_root_from_data_dir(Path::new("/opt/wa-data"));
        assert_eq!(root, PathBuf::from("/opt/wa-data/browser_profiles"));
    }

    // =========================================================================
    // ProfileMetadata tests
    // =========================================================================

    #[test]
    fn metadata_new() {
        let meta = ProfileMetadata::new("openai", "my-account");
        assert_eq!(meta.service, "openai");
        assert_eq!(meta.account, "my-account");
        assert!(meta.bootstrapped_at.is_none());
        assert!(meta.bootstrap_method.is_none());
        assert!(meta.last_used_at.is_none());
        assert_eq!(meta.automated_use_count, 0);
    }

    #[test]
    fn metadata_record_bootstrap() {
        let mut meta = ProfileMetadata::new("openai", "test");
        meta.record_bootstrap(BootstrapMethod::Interactive);
        assert!(meta.bootstrapped_at.is_some());
        assert_eq!(meta.bootstrap_method, Some(BootstrapMethod::Interactive));
        assert!(meta.last_used_at.is_some());
    }

    #[test]
    fn metadata_record_use() {
        let mut meta = ProfileMetadata::new("openai", "test");
        assert_eq!(meta.automated_use_count, 0);
        meta.record_use();
        assert_eq!(meta.automated_use_count, 1);
        assert!(meta.last_used_at.is_some());
        meta.record_use();
        assert_eq!(meta.automated_use_count, 2);

        meta.automated_use_count = u64::MAX;
        meta.record_use();
        assert_eq!(meta.automated_use_count, u64::MAX);
    }

    #[test]
    fn metadata_serde_round_trip() {
        let mut meta = ProfileMetadata::new("anthropic", "work");
        meta.record_bootstrap(BootstrapMethod::Automated);
        meta.record_use();

        let json = serde_json::to_string(&meta).unwrap();
        let deserialized: ProfileMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.service, "anthropic");
        assert_eq!(deserialized.account, "work");
        assert_eq!(
            deserialized.bootstrap_method,
            Some(BootstrapMethod::Automated)
        );
        assert_eq!(deserialized.automated_use_count, 1);
    }

    #[test]
    fn metadata_serde_skip_none_fields() {
        let meta = ProfileMetadata::new("openai", "test");
        let json = serde_json::to_string(&meta).unwrap();
        assert!(!json.contains("bootstrapped_at"));
        assert!(!json.contains("bootstrap_method"));
        assert!(!json.contains("last_used_at"));
    }

    #[test]
    fn profile_metadata_reader_enforces_exact_finite_boundary() {
        assert_eq!(
            admit_profile_metadata_shape_and_size(true, PROFILE_METADATA_MAX_BYTES),
            Ok(())
        );
        assert_eq!(
            admit_profile_metadata_shape_and_size(
                true,
                PROFILE_METADATA_MAX_BYTES.saturating_add(1)
            ),
            Err(ProfileMetadataReadFailure::Oversized)
        );
        assert_eq!(
            admit_profile_metadata_shape_and_size(false, 0),
            Err(ProfileMetadataReadFailure::NotRegularFile)
        );

        let max_bytes = usize::try_from(PROFILE_METADATA_MAX_BYTES)
            .expect("profile metadata cap must fit usize");
        let exact = vec![b'x'; max_bytes];
        assert_eq!(
            read_profile_metadata_bounded(std::io::Cursor::new(exact))
                .expect("exact-cap metadata must be retained")
                .len(),
            max_bytes
        );
        let oversized = vec![b'x'; max_bytes.saturating_add(1)];
        assert_eq!(
            read_profile_metadata_bounded(std::io::Cursor::new(oversized)),
            Err(ProfileMetadataReadFailure::Oversized)
        );

        let before = ProfileMetadataFileSnapshot::synthetic(128, 1);
        assert_eq!(before, ProfileMetadataFileSnapshot::synthetic(128, 1));
        assert_ne!(before, ProfileMetadataFileSnapshot::synthetic(129, 1));
        assert_ne!(before, ProfileMetadataFileSnapshot::synthetic(128, 2));
    }

    #[test]
    fn profile_metadata_parser_is_schema_identity_and_timestamp_specific() {
        let valid = br#"{
            "service":"openai",
            "account":"test",
            "bootstrapped_at":"2026-08-06T12:34:56Z",
            "bootstrap_method":"interactive",
            "last_used_at":"2026-08-06T12:35:00+00:00",
            "automated_use_count":3
        }"#;
        let parsed = parse_profile_metadata_bytes(valid, "openai", "test")
            .expect("valid typed metadata must parse");
        assert_eq!(parsed.service, "openai");
        assert_eq!(parsed.account, "test");
        assert!(parsed.bootstrapped_at.is_some());

        assert_eq!(
            parse_profile_metadata_bytes(valid, "anthropic", "test").unwrap_err(),
            ProfileMetadataReadFailure::IdentityMismatch
        );
        assert_eq!(
            parse_profile_metadata_bytes(
                br#"{"service":"openai","account":"test","bootstrapped_at":"bad"}"#,
                "openai",
                "test"
            )
            .unwrap_err(),
            ProfileMetadataReadFailure::InvalidTimestamp
        );
        assert_eq!(
            parse_profile_metadata_bytes(
                br#"{"service":"openai","account":"test","automated_use_count":"many"}"#,
                "openai",
                "test"
            )
            .unwrap_err(),
            ProfileMetadataReadFailure::InvalidSchema
        );
        assert_eq!(
            parse_profile_metadata_bytes(
                br#"{"service":"openai","account":"test","unexpected":true}"#,
                "openai",
                "test"
            )
            .unwrap_err(),
            ProfileMetadataReadFailure::InvalidSchema
        );
    }

    #[test]
    fn profile_metadata_writer_uses_the_same_identity_timestamp_and_size_contract() {
        let mut valid = ProfileMetadata::new("openai", "test");
        valid.record_bootstrap(BootstrapMethod::Interactive);
        let encoded = serialize_profile_metadata_bounded(&valid, "openai", "test")
            .expect("valid metadata must remain writable and readable");
        let reparsed = parse_profile_metadata_bytes(encoded.as_bytes(), "openai", "test")
            .expect("writer output must satisfy reader contract");
        assert_eq!(reparsed.service, "openai");
        assert_eq!(reparsed.account, "test");

        assert_eq!(
            serialize_profile_metadata_bounded(&valid, "anthropic", "test").unwrap_err(),
            ProfileMetadataReadFailure::IdentityMismatch
        );

        let mut invalid_timestamp = ProfileMetadata::new("openai", "test");
        invalid_timestamp.bootstrapped_at = Some("not-a-timestamp".to_string());
        assert_eq!(
            serialize_profile_metadata_bounded(&invalid_timestamp, "openai", "test")
                .unwrap_err(),
            ProfileMetadataReadFailure::InvalidTimestamp
        );

        let oversized_account = "a".repeat(
            usize::try_from(PROFILE_METADATA_MAX_BYTES)
                .expect("metadata cap must fit usize"),
        );
        let oversized = ProfileMetadata::new("openai", &oversized_account);
        assert_eq!(
            serialize_profile_metadata_bounded(&oversized, "openai", &oversized_account)
                .unwrap_err(),
            ProfileMetadataReadFailure::Oversized
        );
    }

    #[test]
    fn profile_metadata_failures_are_content_free() {
        for failure in [
            ProfileMetadataReadFailure::MetadataUnavailable,
            ProfileMetadataReadFailure::NotRegularFile,
            ProfileMetadataReadFailure::Oversized,
            ProfileMetadataReadFailure::ReadUnavailable,
            ProfileMetadataReadFailure::ChangedDuringRead,
            ProfileMetadataReadFailure::InvalidSchema,
            ProfileMetadataReadFailure::IdentityMismatch,
            ProfileMetadataReadFailure::InvalidTimestamp,
        ] {
            let detail = failure.detail();
            let rendered = failure.into_storage_error().to_string();
            assert!(!detail.contains("AKIAIOSFODNN7EXAMPLE"));
            assert!(!detail.contains('/'));
            assert!(!detail.contains('\\'));
            assert!(!detail.contains('\n'));
            assert!(!detail.contains('\u{202e}'));
            assert!(detail.len() <= 96);
            assert!(!rendered.contains("AKIAIOSFODNN7EXAMPLE"));
            assert!(!rendered.contains('/'));
            assert!(!rendered.contains('\\'));
            assert!(!rendered.contains('\n'));
            assert!(!rendered.contains('\u{202e}'));
            assert!(rendered.len() <= 128);
        }
    }

    #[test]
    fn storage_state_reader_enforces_exact_finite_boundary() {
        assert_eq!(admit_storage_state_size(STORAGE_STATE_MAX_BYTES), Ok(()));
        assert_eq!(
            admit_storage_state_size(STORAGE_STATE_MAX_BYTES.saturating_add(1)),
            Err(StorageStateFailure::Oversized)
        );

        const TEST_CAP: u64 = 8;
        let exact = vec![b'x'; 8];
        assert_eq!(
            read_bytes_to_finite_cap(std::io::Cursor::new(exact), TEST_CAP)
                .expect("exact-cap storage state must be retained")
                .len(),
            8
        );
        let oversized = vec![b'x'; 9];
        assert_eq!(
            read_bytes_to_finite_cap(std::io::Cursor::new(oversized), TEST_CAP),
            Err(PrivateFileReadFailure::Oversized)
        );

        assert_eq!(
            admit_storage_state_document(br#"{"cookies":[],"origins":[]}"#),
            Ok(())
        );
        assert_eq!(
            admit_storage_state_document(br#"{"cookies":[]}"#),
            Err(StorageStateFailure::InvalidShape)
        );
        assert_eq!(
            admit_storage_state_document(br#"{"cookies":[false],"origins":[]}"#),
            Err(StorageStateFailure::InvalidShape)
        );
        assert_eq!(
            admit_storage_state_document(b"not-json"),
            Err(StorageStateFailure::InvalidJson)
        );
    }

    #[test]
    fn storage_state_failures_are_content_free() {
        for failure in [
            StorageStateFailure::MetadataUnavailable,
            StorageStateFailure::NotRegularFile,
            StorageStateFailure::Oversized,
            StorageStateFailure::ReadUnavailable,
            StorageStateFailure::ChangedDuringRead,
            StorageStateFailure::InvalidJson,
            StorageStateFailure::InvalidShape,
            StorageStateFailure::WriteUnavailable,
        ] {
            let detail = failure.detail();
            let rendered = failure.into_storage_error().to_string();
            for value in [detail, rendered.as_str()] {
                assert!(!value.contains("AKIAIOSFODNN7EXAMPLE"));
                assert!(!value.contains('/'));
                assert!(!value.contains('\\'));
                assert!(!value.contains('\n'));
                assert!(!value.contains('\u{202e}'));
                assert!(value.len() <= 128);
            }
        }
    }

    #[test]
    fn bootstrap_method_serde() {
        let interactive = BootstrapMethod::Interactive;
        let json = serde_json::to_string(&interactive).unwrap();
        assert_eq!(json, "\"interactive\"");

        let automated = BootstrapMethod::Automated;
        let json = serde_json::to_string(&automated).unwrap();
        assert_eq!(json, "\"automated\"");

        let deserialized: BootstrapMethod = serde_json::from_str("\"interactive\"").unwrap();
        assert_eq!(deserialized, BootstrapMethod::Interactive);
    }

    // =========================================================================
    // Profile metadata persistence tests
    // =========================================================================

    #[test]
    fn profile_metadata_write_and_read() {
        let temp =
            canonical_system_temp_dir().join(format!("wa_meta_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);

        let profile = BrowserProfile::new(&temp, "openai", "test-account");
        profile.ensure_dir().unwrap();

        let mut meta = ProfileMetadata::new("openai", "test-account");
        meta.record_bootstrap(BootstrapMethod::Interactive);

        profile.write_metadata(&meta).unwrap();
        assert!(profile.metadata_path().is_file());

        let loaded = profile.read_metadata().unwrap().unwrap();
        assert_eq!(loaded.service, "openai");
        assert_eq!(loaded.bootstrap_method, Some(BootstrapMethod::Interactive));

        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn profile_metadata_read_missing() {
        let temp = canonical_system_temp_dir().join(format!(
            "wa_meta_missing_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&temp);

        let profile = BrowserProfile::new(&temp, "openai", "nonexistent");
        let result = profile.read_metadata().unwrap();
        assert!(result.is_none());

        let _ = std::fs::remove_dir_all(&temp);
    }

    #[cfg(unix)]
    #[test]
    fn profile_metadata_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temp = canonical_system_temp_dir().join(format!(
            "wa_meta_perms_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&temp);

        let profile = BrowserProfile::new(&temp, "openai", "secure");
        profile.ensure_dir().unwrap();

        let meta = ProfileMetadata::new("openai", "secure");
        profile.write_metadata(&meta).unwrap();

        let perms = std::fs::metadata(profile.metadata_path())
            .unwrap()
            .permissions();
        assert_eq!(perms.mode() & 0o777, 0o600);

        let _ = std::fs::remove_dir_all(&temp);
    }

    #[cfg(unix)]
    #[test]
    fn profile_private_files_never_follow_leaf_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("isolated profile test root");
        let temp_path = std::fs::canonicalize(temp.path()).expect("canonical profile test root");
        let profiles_root = temp_path.join("profiles");
        let profile = BrowserProfile::new(&profiles_root, "openai", "safe-account");
        profile.ensure_dir().expect("profile directory");

        let outside_metadata = temp_path.join("outside-metadata.json");
        let outside_metadata_bytes =
            br#"{"service":"openai","account":"safe-account","automated_use_count":0}"#;
        std::fs::write(&outside_metadata, outside_metadata_bytes).expect("outside metadata fixture");
        symlink(&outside_metadata, profile.metadata_path()).expect("metadata symlink fixture");
        assert!(profile.read_metadata().is_err());

        let metadata = ProfileMetadata::new("openai", "safe-account");
        profile
            .write_metadata(&metadata)
            .expect("atomic replacement must replace, not follow, the metadata symlink");
        assert_eq!(
            std::fs::read(&outside_metadata).expect("outside metadata remains readable"),
            outside_metadata_bytes
        );
        assert!(
            std::fs::symlink_metadata(profile.metadata_path())
                .expect("replacement metadata")
                .file_type()
                .is_file()
        );

        let outside_state = temp_path.join("outside-state.json");
        let outside_state_bytes = br#"{"secret":"must-not-change"}"#;
        std::fs::write(&outside_state, outside_state_bytes).expect("outside state fixture");
        symlink(&outside_state, profile.storage_state_path()).expect("storage symlink fixture");
        assert!(profile.load_storage_state().is_err());
        assert!(!profile.has_storage_state());

        let state = br#"{"cookies":[],"origins":[]}"#;
        profile
            .save_storage_state(state)
            .expect("atomic replacement must replace, not follow, the storage-state symlink");
        assert_eq!(
            std::fs::read(&outside_state).expect("outside state remains readable"),
            outside_state_bytes
        );
        assert_eq!(
            profile
                .load_storage_state()
                .expect("safe storage state read")
                .expect("storage state exists"),
            state
        );
    }

    #[cfg(unix)]
    #[test]
    fn profile_directory_components_never_follow_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temp = tempfile::tempdir().expect("isolated profile test root");
        let temp_path = std::fs::canonicalize(temp.path()).expect("canonical profile test root");
        let profiles_root = temp_path.join("profiles");
        let outside = temp_path.join("outside-service");
        std::fs::create_dir(&profiles_root).expect("profiles root fixture");
        std::fs::create_dir(&outside).expect("outside directory fixture");
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o755))
            .expect("outside permissions fixture");
        symlink(&outside, profiles_root.join("openai")).expect("service symlink fixture");

        let profile = BrowserProfile::new(&profiles_root, "openai", "safe-account");
        assert!(profile.ensure_dir().is_err());
        assert!(!profile.exists());
        let outside_mode = std::fs::metadata(&outside)
            .expect("outside directory remains available")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(outside_mode, 0o755);
        assert!(!outside.join("safe-account").exists());
    }

    #[cfg(unix)]
    #[test]
    fn storage_state_accessibility_fails_closed_on_permission_and_replacement() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("isolated profile test root");
        let temp_path = std::fs::canonicalize(temp.path()).expect("canonical profile test root");
        let profile = BrowserProfile::new(temp_path.join("profiles"), "openai", "test");
        profile.ensure_dir().expect("profile directory");
        let initial_state = br#"{"cookies":[],"origins":[]}"#;
        profile
            .save_storage_state(initial_state)
            .expect("initial state");

        let state_path = profile.storage_state_path();
        std::fs::set_permissions(&state_path, std::fs::Permissions::from_mode(0o000))
            .expect("remove fixture read permission");
        if std::fs::File::open(&state_path)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied)
        {
            assert!(!profile.has_storage_state());
        }
        std::fs::set_permissions(&state_path, std::fs::Permissions::from_mode(0o600))
            .expect("restore fixture permission");

        let directory = profile
            .open_profile_dir_capability()
            .expect("profile capability");
        let replacement_path = profile.path().join("replacement-state.json");
        std::fs::write(&replacement_path, vec![b'x'; initial_state.len()])
            .expect("replacement fixture");
        assert!(!private_file_is_safely_accessible_with_hook(
            &directory,
            STORAGE_STATE_FILE_NAME,
            STORAGE_STATE_MAX_BYTES,
            || {
                std::fs::rename(&replacement_path, &state_path)
                    .expect("replace state between admission observations");
            },
        ));
    }

    #[test]
    fn private_atomic_writer_never_deletes_an_unverified_temp_name() {
        let source = include_str!("mod.rs");
        let start = source
            .find("fn write_private_file_atomically(")
            .expect("atomic writer source");
        let tail = &source[start..];
        let end = tail
            .find("\n}\n\n#[cfg(test)]\nfn admit_profile_metadata_shape_and_size")
            .expect("atomic writer source boundary");
        let body = &tail[..end];
        assert!(!body.contains("remove_file"));
        assert!(body.contains("directory.rename"));
        #[cfg(unix)]
        assert!(body.contains("into_std_file().sync_all"));
    }

    // =========================================================================
    // Storage state persistence tests
    // =========================================================================

    #[test]
    fn profile_storage_state_paths() {
        let profiles_root = PathBuf::from("/data/profiles");
        let profile = BrowserProfile::new(&profiles_root, "openai", "test");
        assert_eq!(
            profile.storage_state_path(),
            PathBuf::from("/data/profiles/openai/test/storage_state.json")
        );
    }

    #[test]
    fn profile_no_storage_state_initially() {
        let temp = canonical_system_temp_dir().join(format!(
            "wa_state_test_none_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&temp);

        let profile = BrowserProfile::new(&temp, "openai", "fresh");
        assert!(!profile.has_storage_state());

        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn profile_save_and_load_storage_state() {
        let temp =
            canonical_system_temp_dir().join(format!("wa_state_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);

        let profile = BrowserProfile::new(&temp, "openai", "test-account");
        profile.ensure_dir().unwrap();

        let state = br#"{"cookies":[],"origins":[]}"#;
        profile.save_storage_state(state).unwrap();

        assert!(profile.has_storage_state());
        assert_eq!(
            profile
                .validate_storage_state()
                .expect("valid state validation"),
            StorageStateValidation::Valid
        );

        let loaded = profile.load_storage_state().unwrap().unwrap();
        assert_eq!(loaded, state);

        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn authenticated_state_commit_updates_metadata_and_refuses_corrupt_authority() {
        let temp = tempfile::tempdir().expect("isolated authenticated-state root");
        let profile = BrowserProfile::new(temp.path().join("profiles"), "openai", "test");
        profile.ensure_dir().expect("profile directory");
        let state = br#"{"cookies":[],"origins":[]}"#;
        profile
            .record_authenticated_state(state, BootstrapMethod::Automated)
            .expect("authenticated state commit");
        assert_eq!(
            profile.validate_storage_state().expect("state validation"),
            StorageStateValidation::Valid
        );
        let metadata = profile
            .read_metadata()
            .expect("metadata read")
            .expect("metadata exists");
        assert_eq!(metadata.bootstrap_method, Some(BootstrapMethod::Automated));

        std::fs::write(profile.metadata_path(), b"not-json").expect("corrupt metadata fixture");
        let replacement_state = br#"{"cookies":[{}],"origins":[]}"#;
        assert!(
            profile
                .record_authenticated_state(replacement_state, BootstrapMethod::Automated)
                .is_err()
        );
        assert_eq!(
            profile
                .load_storage_state()
                .expect("original state remains readable")
                .expect("original state exists"),
            state
        );
    }

    #[test]
    fn profile_storage_state_rejects_invalid_json_and_shape_on_both_boundaries() {
        let temp = tempfile::tempdir().expect("isolated storage-state root");
        let temp_path =
            std::fs::canonicalize(temp.path()).expect("canonical storage-state root");
        let profile = BrowserProfile::new(temp_path.join("profiles"), "openai", "test");
        profile.ensure_dir().expect("profile directory");

        assert!(profile.save_storage_state(b"not-json").is_err());
        assert!(profile.save_storage_state(br#"{"cookies":[]}"#).is_err());
        assert!(!profile.storage_state_path().exists());
        assert_eq!(
            profile
                .validate_storage_state()
                .expect("missing state validation"),
            StorageStateValidation::Missing
        );

        std::fs::write(profile.storage_state_path(), br#"{"cookies":[],"origins":false}"#)
            .expect("invalid persisted fixture");
        assert!(profile.load_storage_state().is_err());
        assert!(profile.validate_storage_state().is_err());
    }

    #[test]
    fn profile_load_storage_state_missing() {
        let temp = canonical_system_temp_dir().join(format!(
            "wa_state_missing_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&temp);

        let profile = BrowserProfile::new(&temp, "openai", "no-state");
        let result = profile.load_storage_state().unwrap();
        assert!(result.is_none());

        let _ = std::fs::remove_dir_all(&temp);
    }

    #[cfg(unix)]
    #[test]
    fn profile_storage_state_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temp = canonical_system_temp_dir().join(format!(
            "wa_state_perms_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&temp);

        let profile = BrowserProfile::new(&temp, "openai", "secure");
        profile.ensure_dir().unwrap();

        let state = br#"{"cookies":[],"origins":[]}"#;
        profile.save_storage_state(state).unwrap();

        let perms = std::fs::metadata(profile.storage_state_path())
            .unwrap()
            .permissions();
        assert_eq!(perms.mode() & 0o777, 0o600);

        let _ = std::fs::remove_dir_all(&temp);
    }

    // =========================================================================
    // Metadata path resolution tests
    // =========================================================================

    #[test]
    fn metadata_path_resolution() {
        let profiles_root = PathBuf::from("/data/profiles");
        let profile = BrowserProfile::new(&profiles_root, "openai", "test");
        assert_eq!(
            profile.metadata_path(),
            PathBuf::from("/data/profiles/openai/test/.wa_profile.json")
        );
    }
}
