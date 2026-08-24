//! Runtime ownership for immutable process-family generations.
//!
//! A mux server launched from the managed process-family namespace must hold a
//! shared lock on its generation-local lifetime lease for its entire runtime.
//! Future activation/retirement code can then take the corresponding exclusive
//! lock before deciding that no process still executes that generation.
//!
//! This module deliberately does not create or repair any authority artifact.
//! A path that looks managed but is incomplete or malformed fails closed.

#[cfg(any(target_os = "linux", test))]
use std::path::{Component, Path, PathBuf};

#[cfg(any(target_os = "linux", test))]
const PROCESS_FAMILY_DIRECTORY: &str = "process-family";
#[cfg(any(target_os = "linux", test))]
const GENERATIONS_DIRECTORY: &str = "generations";
#[cfg(any(target_os = "linux", test))]
const MUX_SERVER_FILENAME: &str = "frankenterm-mux-server";
#[cfg(target_os = "linux")]
const LIFETIME_LEASE_FILENAME: &str = ".lifetime-lease.v1";
#[cfg(any(target_os = "linux", test))]
const GENERATION_ID_HEX_LEN: usize = 64;

/// Content-free filesystem identity suitable for a readiness receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GenerationObjectIdentity {
    device: u64,
    inode: u64,
}

impl GenerationObjectIdentity {
    /// Filesystem device number of the pinned object.
    #[must_use]
    pub const fn device(self) -> u64 {
        self.device
    }

    /// Inode number of the pinned object.
    #[must_use]
    pub const fn inode(self) -> u64 {
        self.inode
    }
}

/// Content-free metadata for one held managed-generation lifetime lease.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedGenerationLifetimeMetadata {
    generation_id: String,
    generations_directory: GenerationObjectIdentity,
    generation_directory: GenerationObjectIdentity,
    lifetime_lease: GenerationObjectIdentity,
    executable: GenerationObjectIdentity,
}

impl ManagedGenerationLifetimeMetadata {
    /// Content-derived lowercase SHA-256 generation identifier.
    #[must_use]
    pub fn generation_id(&self) -> &str {
        &self.generation_id
    }

    /// Identity of the pinned `generations` directory.
    #[must_use]
    pub const fn generations_directory(&self) -> GenerationObjectIdentity {
        self.generations_directory
    }

    /// Identity of the pinned immutable generation directory.
    #[must_use]
    pub const fn generation_directory(&self) -> GenerationObjectIdentity {
        self.generation_directory
    }

    /// Identity of the locked generation-local lifetime lease.
    #[must_use]
    pub const fn lifetime_lease(&self) -> GenerationObjectIdentity {
        self.lifetime_lease
    }

    /// Identity of the generation-local mux executable.
    #[must_use]
    pub const fn executable(&self) -> GenerationObjectIdentity {
        self.executable
    }
}

/// Failure to classify or acquire the current mux generation authority.
#[derive(Debug, thiserror::Error)]
pub enum GenerationLifetimeError {
    /// `current_exe` itself could not be resolved.
    #[error("cannot resolve the current mux executable: {source}")]
    CurrentExecutable {
        #[source]
        source: std::io::Error,
    },

    /// The path opted into the managed namespace but did not use its one
    /// canonical grammar.
    #[error("managed mux executable path is malformed: {reason}")]
    MalformedManagedPath { reason: &'static str },

    /// A managed path lacked one of the immutable filesystem authorities.
    #[error("managed generation lifetime authority is invalid: {reason}")]
    InvalidAuthority { reason: &'static str },

    /// A descriptor-confined filesystem operation failed. The operation name
    /// is finite and deliberately omits the operator's path.
    #[cfg(target_os = "linux")]
    #[error("managed generation lifetime I/O failed during {operation}: {source}")]
    Io {
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },
}

#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Debug, Eq, PartialEq)]
struct ManagedExecutablePath {
    generation_id: String,
    generations_directory: PathBuf,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Debug, Eq, PartialEq)]
enum ExecutablePathClass {
    Unmanaged,
    Managed(ManagedExecutablePath),
}

/// RAII owner of a mux server's managed-generation lifetime authority.
///
/// On Linux, `acquire_for_current_process` classifies the exact current
/// executable. A standalone installation is explicitly unmanaged. A managed
/// installation retains pinned directory/executable descriptors and a shared
/// flock until this value is dropped.
pub struct GenerationLifetimeLease {
    #[cfg(target_os = "linux")]
    state: LinuxGenerationLifetimeState,
    #[cfg(not(target_os = "linux"))]
    _unmanaged: (),
}

#[cfg(target_os = "linux")]
enum LinuxGenerationLifetimeState {
    Unmanaged,
    Managed(ManagedGenerationLifetimeLease),
}

#[cfg(target_os = "linux")]
struct ManagedGenerationLifetimeLease {
    metadata: ManagedGenerationLifetimeMetadata,
    // Retaining all four handles pins the exact authority objects for the full
    // mux-server runtime. The lease file's shared flock is released only when
    // `_lifetime_lease` closes.
    _generations_directory: std::fs::File,
    _generation_directory: std::fs::File,
    _executable: std::fs::File,
    _lifetime_lease: std::fs::File,
}

impl GenerationLifetimeLease {
    /// Classify the current executable and acquire its lifetime authority.
    ///
    /// Standalone installations are returned as unmanaged. Managed-looking
    /// paths never fall back to unmanaged when parsing or authority validation
    /// fails.
    pub fn acquire_for_current_process() -> Result<Self, GenerationLifetimeError> {
        #[cfg(target_os = "linux")]
        {
            let executable = std::env::current_exe().map_err(|source| {
                GenerationLifetimeError::CurrentExecutable { source }
            })?;
            let ExecutablePathClass::Managed(managed) = classify_executable_path(&executable)?
            else {
                return Ok(Self {
                    state: LinuxGenerationLifetimeState::Unmanaged,
                });
            };
            let current_identity = current_process_executable_identity()?;
            acquire_linux_managed_generation_lifetime(
                managed,
                rustix::process::geteuid().as_raw(),
                current_identity,
            )
        }

        #[cfg(not(target_os = "linux"))]
        {
            Ok(Self { _unmanaged: () })
        }
    }

    /// Whether this process is running from the managed generation namespace.
    #[must_use]
    pub fn is_managed(&self) -> bool {
        self.metadata().is_some()
    }

    /// Content-free generation metadata for later readiness receipts.
    #[must_use]
    pub fn metadata(&self) -> Option<&ManagedGenerationLifetimeMetadata> {
        #[cfg(target_os = "linux")]
        {
            match &self.state {
                LinuxGenerationLifetimeState::Unmanaged => None,
                LinuxGenerationLifetimeState::Managed(lease) => Some(&lease.metadata),
            }
        }

        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }
}

#[cfg(any(target_os = "linux", test))]
fn classify_executable_path(path: &Path) -> Result<ExecutablePathClass, GenerationLifetimeError> {
    let components = path.components().collect::<Vec<_>>();
    let looks_managed = components.iter().any(|component| {
        matches!(component, Component::Normal(value) if *value == PROCESS_FAMILY_DIRECTORY)
    });

    if !looks_managed {
        return Ok(ExecutablePathClass::Unmanaged);
    }

    if !path.is_absolute() {
        return Err(GenerationLifetimeError::MalformedManagedPath {
            reason: "the path is not absolute",
        });
    }

    let mut normal = Vec::new();
    let mut normalized = PathBuf::from("/");
    for component in &components {
        match component {
            Component::RootDir => {}
            Component::Normal(value) => {
                normal.push(*value);
                normalized.push(value);
            }
            Component::Prefix(_) | Component::CurDir | Component::ParentDir => {
                return Err(GenerationLifetimeError::MalformedManagedPath {
                    reason: "the path is not normalized",
                });
            }
        }
    }

    if normalized.as_os_str() != path.as_os_str() {
        return Err(GenerationLifetimeError::MalformedManagedPath {
            reason: "the path is not in canonical normalized form",
        });
    }

    if normal.len() < 4
        || normal[normal.len() - 4] != PROCESS_FAMILY_DIRECTORY
        || normal[normal.len() - 3] != GENERATIONS_DIRECTORY
        || normal[normal.len() - 1] != MUX_SERVER_FILENAME
    {
        return Err(GenerationLifetimeError::MalformedManagedPath {
            reason: "the path does not end in process-family/generations/<generation-id>/frankenterm-mux-server",
        });
    }

    let generation_id = normal[normal.len() - 2].to_str().ok_or(
        GenerationLifetimeError::MalformedManagedPath {
            reason: "the generation identifier is not UTF-8",
        },
    )?;
    if generation_id.len() != GENERATION_ID_HEX_LEN
        || !generation_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(GenerationLifetimeError::MalformedManagedPath {
            reason: "the generation identifier is not 64 lowercase hexadecimal characters",
        });
    }

    let mut generations_directory = PathBuf::from("/");
    for component in &normal[..normal.len() - 2] {
        generations_directory.push(component);
    }

    Ok(ExecutablePathClass::Managed(ManagedExecutablePath {
        generation_id: generation_id.to_owned(),
        generations_directory,
    }))
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NodeSnapshot {
    identity: GenerationObjectIdentity,
    kind: rustix::fs::FileType,
    owner: u32,
    mode: u32,
    link_count: u64,
    byte_len: u64,
}

#[cfg(target_os = "linux")]
fn io_error(operation: &'static str, source: impl Into<std::io::Error>) -> GenerationLifetimeError {
    GenerationLifetimeError::Io {
        operation,
        source: source.into(),
    }
}

#[cfg(target_os = "linux")]
fn snapshot_from_stat(
    stat: &rustix::fs::Stat,
) -> Result<NodeSnapshot, GenerationLifetimeError> {
    let byte_len = u64::try_from(stat.st_size).map_err(|_| {
        GenerationLifetimeError::InvalidAuthority {
            reason: "a managed filesystem object reports a negative length",
        }
    })?;
    Ok(NodeSnapshot {
        identity: GenerationObjectIdentity {
            device: stat.st_dev,
            inode: stat.st_ino,
        },
        kind: rustix::fs::FileType::from_raw_mode(stat.st_mode),
        owner: stat.st_uid,
        mode: stat.st_mode & 0o7777,
        link_count: stat.st_nlink.into(),
        byte_len,
    })
}

#[cfg(target_os = "linux")]
fn snapshot_file(
    file: &std::fs::File,
    operation: &'static str,
) -> Result<NodeSnapshot, GenerationLifetimeError> {
    let stat = rustix::fs::fstat(file).map_err(|source| io_error(operation, source))?;
    snapshot_from_stat(&stat)
}

#[cfg(target_os = "linux")]
fn snapshot_named(
    directory: &std::fs::File,
    name: &Path,
    operation: &'static str,
) -> Result<NodeSnapshot, GenerationLifetimeError> {
    let stat = rustix::fs::statat(directory, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|source| io_error(operation, source))?;
    snapshot_from_stat(&stat)
}

#[cfg(target_os = "linux")]
fn open_absolute_directory_tree_nofollow(
    path: &Path,
    operation: &'static str,
) -> Result<std::fs::File, GenerationLifetimeError> {
    use std::os::unix::fs::OpenOptionsExt as _;

    if !path.is_absolute() {
        return Err(GenerationLifetimeError::InvalidAuthority {
            reason: "an internal managed directory path is not absolute",
        });
    }

    let mut directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open("/")
        .map_err(|source| io_error(operation, source))?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                let descriptor = rustix::fs::openat(
                    &directory,
                    name,
                    rustix::fs::OFlags::RDONLY
                        | rustix::fs::OFlags::CLOEXEC
                        | rustix::fs::OFlags::DIRECTORY
                        | rustix::fs::OFlags::NOFOLLOW,
                    rustix::fs::Mode::empty(),
                )
                .map_err(|source| io_error(operation, source))?;
                directory = std::fs::File::from(descriptor);
            }
            Component::Prefix(_) | Component::CurDir | Component::ParentDir => {
                return Err(GenerationLifetimeError::InvalidAuthority {
                    reason: "an internal managed directory path is not normalized",
                });
            }
        }
    }
    Ok(directory)
}

#[cfg(target_os = "linux")]
fn open_directory_at_nofollow(
    parent: &std::fs::File,
    name: &Path,
    operation: &'static str,
) -> Result<std::fs::File, GenerationLifetimeError> {
    let descriptor = rustix::fs::openat(
        parent,
        name,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .map_err(|source| io_error(operation, source))?;
    Ok(std::fs::File::from(descriptor))
}

#[cfg(target_os = "linux")]
fn open_regular_file_at_nofollow(
    parent: &std::fs::File,
    name: &Path,
    operation: &'static str,
) -> Result<std::fs::File, GenerationLifetimeError> {
    let descriptor = rustix::fs::openat(
        parent,
        name,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .map_err(|source| io_error(operation, source))?;
    Ok(std::fs::File::from(descriptor))
}

#[cfg(target_os = "linux")]
fn validate_directory_snapshot(
    snapshot: NodeSnapshot,
    expected_owner: u32,
    expected_mode: u32,
    expected_device: Option<u64>,
) -> Result<(), GenerationLifetimeError> {
    if snapshot.kind != rustix::fs::FileType::Directory
        || snapshot.owner != expected_owner
        || snapshot.mode != expected_mode
        || expected_device.is_some_and(|device| snapshot.identity.device != device)
    {
        return Err(GenerationLifetimeError::InvalidAuthority {
            reason: "a managed generation directory has the wrong type, owner, mode, or filesystem",
        });
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_lease_snapshot(
    snapshot: NodeSnapshot,
    expected_owner: u32,
    expected_device: u64,
) -> Result<(), GenerationLifetimeError> {
    if snapshot.kind != rustix::fs::FileType::RegularFile
        || snapshot.owner != expected_owner
        || snapshot.mode != 0o600
        || snapshot.link_count != 1
        || snapshot.byte_len != 0
        || snapshot.identity.device != expected_device
    {
        return Err(GenerationLifetimeError::InvalidAuthority {
            reason: "the generation lifetime lease is not one owner mode-0600 zero-length file on the generation filesystem",
        });
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_executable_snapshot(
    snapshot: NodeSnapshot,
    expected_owner: u32,
    expected_device: u64,
    current_identity: GenerationObjectIdentity,
) -> Result<(), GenerationLifetimeError> {
    if snapshot.kind != rustix::fs::FileType::RegularFile
        || snapshot.owner != expected_owner
        || snapshot.mode != 0o500
        || snapshot.link_count != 1
        || snapshot.byte_len == 0
        || snapshot.identity.device != expected_device
        || snapshot.identity != current_identity
    {
        return Err(GenerationLifetimeError::InvalidAuthority {
            reason: "the generation mux executable does not match the current owner-only single-link executable",
        });
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn require_same_named_object(
    opened: NodeSnapshot,
    named: NodeSnapshot,
    reason: &'static str,
) -> Result<(), GenerationLifetimeError> {
    if opened != named {
        return Err(GenerationLifetimeError::InvalidAuthority { reason });
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn current_process_executable_identity(
) -> Result<GenerationObjectIdentity, GenerationLifetimeError> {
    let descriptor = rustix::fs::openat(
        rustix::fs::CWD,
        Path::new("/proc/self/exe"),
        rustix::fs::OFlags::PATH | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|source| io_error("open current process executable authority", source))?;
    let stat = rustix::fs::fstat(&descriptor)
        .map_err(|source| io_error("inspect current process executable authority", source))?;
    Ok(snapshot_from_stat(&stat)?.identity)
}

#[cfg(target_os = "linux")]
fn revalidate_directory_bindings(
    managed: &ManagedExecutablePath,
    generations: &std::fs::File,
    generations_before: NodeSnapshot,
    generation: &std::fs::File,
    generation_before: NodeSnapshot,
    expected_owner: u32,
) -> Result<(), GenerationLifetimeError> {
    let generations_reopened = open_absolute_directory_tree_nofollow(
        &managed.generations_directory,
        "re-open managed generations directory",
    )?;
    let generations_after = snapshot_file(
        &generations_reopened,
        "re-inspect managed generations directory",
    )?;
    validate_directory_snapshot(generations_after, expected_owner, 0o700, None)?;
    if generations_before.identity != generations_after.identity {
        return Err(GenerationLifetimeError::InvalidAuthority {
            reason: "the named generations directory was rebound after it was pinned",
        });
    }

    let generation_named = snapshot_named(
        generations,
        Path::new(&managed.generation_id),
        "re-inspect named generation directory",
    )?;
    let generation_after = snapshot_file(generation, "re-inspect pinned generation directory")?;
    validate_directory_snapshot(
        generation_after,
        expected_owner,
        0o500,
        Some(generations_before.identity.device),
    )?;
    if generation_before.identity != generation_after.identity
        || generation_before.identity != generation_named.identity
        || generation_after != generation_named
    {
        return Err(GenerationLifetimeError::InvalidAuthority {
            reason: "the named generation directory was rebound after it was pinned",
        });
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn acquire_linux_managed_generation_lifetime(
    managed: ManagedExecutablePath,
    effective_uid: u32,
    current_executable_identity: GenerationObjectIdentity,
) -> Result<GenerationLifetimeLease, GenerationLifetimeError> {
    let generations = open_absolute_directory_tree_nofollow(
        &managed.generations_directory,
        "open managed generations directory",
    )?;
    let generations_before = snapshot_file(&generations, "inspect managed generations directory")?;
    validate_directory_snapshot(generations_before, effective_uid, 0o700, None)?;

    let generation = open_directory_at_nofollow(
        &generations,
        Path::new(&managed.generation_id),
        "open managed generation directory",
    )?;
    let generation_before = snapshot_file(&generation, "inspect managed generation directory")?;
    validate_directory_snapshot(
        generation_before,
        effective_uid,
        0o500,
        Some(generations_before.identity.device),
    )?;

    revalidate_directory_bindings(
        &managed,
        &generations,
        generations_before,
        &generation,
        generation_before,
        effective_uid,
    )?;

    let lifetime_lease = open_regular_file_at_nofollow(
        &generation,
        Path::new(LIFETIME_LEASE_FILENAME),
        "open generation lifetime lease",
    )?;
    let executable = open_regular_file_at_nofollow(
        &generation,
        Path::new(MUX_SERVER_FILENAME),
        "open generation mux executable",
    )?;

    let lease_before = snapshot_file(&lifetime_lease, "inspect generation lifetime lease")?;
    let lease_named_before = snapshot_named(
        &generation,
        Path::new(LIFETIME_LEASE_FILENAME),
        "inspect named generation lifetime lease",
    )?;
    validate_lease_snapshot(
        lease_before,
        effective_uid,
        generation_before.identity.device,
    )?;
    require_same_named_object(
        lease_before,
        lease_named_before,
        "the generation lifetime lease handle does not match its nofollow named entry",
    )?;

    let executable_before = snapshot_file(&executable, "inspect generation mux executable")?;
    let executable_named_before = snapshot_named(
        &generation,
        Path::new(MUX_SERVER_FILENAME),
        "inspect named generation mux executable",
    )?;
    validate_executable_snapshot(
        executable_before,
        effective_uid,
        generation_before.identity.device,
        current_executable_identity,
    )?;
    require_same_named_object(
        executable_before,
        executable_named_before,
        "the generation mux executable handle does not match its nofollow named entry",
    )?;

    // Never wait behind an exclusive retirement owner. A direct stale launch
    // that resolved this executable before retirement must fail immediately,
    // not wake after commit and resurrect the retired generation.
    fs2::FileExt::try_lock_shared(&lifetime_lease).map_err(|source| {
        io_error(
            "acquire nonblocking shared generation lifetime lease",
            source,
        )
    })?;

    // The lock is authority only after every retained handle still matches its
    // exact named object. An activation racing this startup can therefore
    // complete before us or wait behind us, but cannot split our observations.
    revalidate_directory_bindings(
        &managed,
        &generations,
        generations_before,
        &generation,
        generation_before,
        effective_uid,
    )?;

    let lease_after = snapshot_file(&lifetime_lease, "re-inspect locked lifetime lease")?;
    let lease_named_after = snapshot_named(
        &generation,
        Path::new(LIFETIME_LEASE_FILENAME),
        "re-inspect named lifetime lease after lock",
    )?;
    validate_lease_snapshot(
        lease_after,
        effective_uid,
        generation_before.identity.device,
    )?;
    require_same_named_object(
        lease_before,
        lease_after,
        "the generation lifetime lease changed while its shared lock was acquired",
    )?;
    require_same_named_object(
        lease_after,
        lease_named_after,
        "the locked generation lifetime lease no longer matches its named entry",
    )?;

    let executable_after = snapshot_file(&executable, "re-inspect generation mux executable")?;
    let executable_named_after = snapshot_named(
        &generation,
        Path::new(MUX_SERVER_FILENAME),
        "re-inspect named generation mux executable",
    )?;
    validate_executable_snapshot(
        executable_after,
        effective_uid,
        generation_before.identity.device,
        current_executable_identity,
    )?;
    require_same_named_object(
        executable_before,
        executable_after,
        "the generation mux executable changed while the lifetime lock was acquired",
    )?;
    require_same_named_object(
        executable_after,
        executable_named_after,
        "the generation mux executable no longer matches its named entry",
    )?;

    let metadata = ManagedGenerationLifetimeMetadata {
        generation_id: managed.generation_id,
        generations_directory: generations_before.identity,
        generation_directory: generation_before.identity,
        lifetime_lease: lease_after.identity,
        executable: executable_after.identity,
    };
    Ok(GenerationLifetimeLease {
        state: LinuxGenerationLifetimeState::Managed(ManagedGenerationLifetimeLease {
            metadata,
            _generations_directory: generations,
            _generation_directory: generation,
            _executable: executable,
            _lifetime_lease: lifetime_lease,
        }),
    })
}

#[cfg(all(test, target_os = "linux"))]
fn acquire_linux_generation_lifetime_for_test(
    executable_path: &Path,
    effective_uid: u32,
    current_executable_identity: GenerationObjectIdentity,
) -> Result<GenerationLifetimeLease, GenerationLifetimeError> {
    let ExecutablePathClass::Managed(managed) = classify_executable_path(executable_path)? else {
        return Ok(GenerationLifetimeLease {
            state: LinuxGenerationLifetimeState::Unmanaged,
        });
    };
    acquire_linux_managed_generation_lifetime(managed, effective_uid, current_executable_identity)
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::*;
    use std::io::Write as _;
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    const TEST_GENERATION_ID: &str =
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    struct Fixture {
        _root: PathBuf,
        generations: PathBuf,
        generation: PathBuf,
        executable: PathBuf,
        lease: PathBuf,
    }

    enum LeaseFixtureShape {
        Regular,
        Missing,
        Symlink,
        HardLinked,
    }

    fn retained_temp_directory(label: &str) -> PathBuf {
        tempfile::Builder::new()
            .prefix(&format!("ft-generation-lifetime-{label}-"))
            .tempdir()
            .expect("create generation lifetime fixture")
            .keep()
    }

    fn create_file(path: &Path, bytes: &[u8], mode: u32) {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)
            .expect("create fixture file without overwrite");
        file.write_all(bytes).expect("write fixture bytes");
        file.sync_all().expect("sync fixture file");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .expect("set fixture file mode");
    }

    fn fixture(label: &str, lease_shape: LeaseFixtureShape) -> Fixture {
        let root = retained_temp_directory(label);
        let generations = root.join(PROCESS_FAMILY_DIRECTORY).join(GENERATIONS_DIRECTORY);
        let generation = generations.join(TEST_GENERATION_ID);
        std::fs::create_dir_all(&generation).expect("create managed fixture directories");
        let executable = generation.join(MUX_SERVER_FILENAME);
        let lease = generation.join(LIFETIME_LEASE_FILENAME);
        create_file(&executable, b"fixture mux executable", 0o500);

        match lease_shape {
            LeaseFixtureShape::Regular => create_file(&lease, b"", 0o600),
            LeaseFixtureShape::Missing => {}
            LeaseFixtureShape::Symlink => {
                let target = generation.join("lease-target");
                create_file(&target, b"", 0o600);
                std::os::unix::fs::symlink("lease-target", &lease)
                    .expect("create lifetime lease symlink fixture");
            }
            LeaseFixtureShape::HardLinked => {
                create_file(&lease, b"", 0o600);
                std::fs::hard_link(&lease, generation.join("lease-alias"))
                    .expect("create lifetime lease hard-link fixture");
            }
        }

        std::fs::set_permissions(&generation, std::fs::Permissions::from_mode(0o500))
            .expect("seal generation fixture directory");
        std::fs::set_permissions(&generations, std::fs::Permissions::from_mode(0o700))
            .expect("set generations fixture mode");
        Fixture {
            _root: root,
            generations,
            generation,
            executable,
            lease,
        }
    }

    fn fixture_executable_identity(fixture: &Fixture) -> GenerationObjectIdentity {
        let metadata = std::fs::metadata(&fixture.executable).expect("inspect fixture executable");
        GenerationObjectIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }

    fn acquire_fixture(
        fixture: &Fixture,
        expected_uid: u32,
        identity: GenerationObjectIdentity,
    ) -> Result<GenerationLifetimeLease, GenerationLifetimeError> {
        acquire_linux_generation_lifetime_for_test(&fixture.executable, expected_uid, identity)
    }

    #[test]
    fn exact_managed_path_acquires_content_free_metadata() {
        let fixture = fixture("exact", LeaseFixtureShape::Regular);
        let identity = fixture_executable_identity(&fixture);
        let lease = acquire_fixture(&fixture, rustix::process::geteuid().as_raw(), identity)
            .expect("acquire exact managed fixture");
        let metadata = lease.metadata().expect("managed fixture metadata");
        assert_eq!(metadata.generation_id(), TEST_GENERATION_ID);
        assert_eq!(metadata.executable(), identity);
        assert_eq!(metadata.lifetime_lease().device(), identity.device());
        assert!(lease.is_managed());
    }

    #[test]
    fn missing_or_symlinked_lease_never_falls_back_to_unmanaged() {
        for (label, shape) in [
            ("missing", LeaseFixtureShape::Missing),
            ("symlink", LeaseFixtureShape::Symlink),
        ] {
            let fixture = fixture(label, shape);
            let error = acquire_fixture(
                &fixture,
                rustix::process::geteuid().as_raw(),
                fixture_executable_identity(&fixture),
            )
            .err()
            .expect("malformed managed authority must fail");
            assert!(error.to_string().contains("managed generation lifetime"));
        }
    }

    #[test]
    fn owner_mode_link_count_and_executable_identity_are_enforced() {
        let owner_fixture = fixture("owner", LeaseFixtureShape::Regular);
        let actual_uid = rustix::process::geteuid().as_raw();
        let wrong_uid = actual_uid.checked_add(1).unwrap_or_else(|| actual_uid - 1);
        assert!(
            acquire_fixture(
                &owner_fixture,
                wrong_uid,
                fixture_executable_identity(&owner_fixture)
            )
            .is_err()
        );

        let mode_fixture = fixture("mode", LeaseFixtureShape::Regular);
        std::fs::set_permissions(&mode_fixture.lease, std::fs::Permissions::from_mode(0o640))
            .expect("make lease mode invalid");
        assert!(
            acquire_fixture(
                &mode_fixture,
                actual_uid,
                fixture_executable_identity(&mode_fixture)
            )
            .is_err()
        );

        let linked_fixture = fixture("linked", LeaseFixtureShape::HardLinked);
        assert!(
            acquire_fixture(
                &linked_fixture,
                actual_uid,
                fixture_executable_identity(&linked_fixture)
            )
            .is_err()
        );

        let identity_fixture = fixture("identity", LeaseFixtureShape::Regular);
        let mut wrong_identity = fixture_executable_identity(&identity_fixture);
        wrong_identity.inode = wrong_identity.inode.wrapping_add(1);
        assert!(acquire_fixture(&identity_fixture, actual_uid, wrong_identity).is_err());
    }

    #[test]
    fn symlinked_generations_directory_is_rejected() {
        let root = retained_temp_directory("generations-symlink");
        let process_family = root.join(PROCESS_FAMILY_DIRECTORY);
        let real_generations = process_family.join("generations-real");
        let generation = real_generations.join(TEST_GENERATION_ID);
        std::fs::create_dir_all(&generation).expect("create real generation fixture");
        let executable = generation.join(MUX_SERVER_FILENAME);
        create_file(&executable, b"fixture mux executable", 0o500);
        create_file(&generation.join(LIFETIME_LEASE_FILENAME), b"", 0o600);
        std::fs::set_permissions(&generation, std::fs::Permissions::from_mode(0o500))
            .expect("seal real generation directory");
        std::fs::set_permissions(&real_generations, std::fs::Permissions::from_mode(0o700))
            .expect("set real generations mode");
        std::os::unix::fs::symlink("generations-real", process_family.join(GENERATIONS_DIRECTORY))
            .expect("create generations symlink fixture");
        let managed_path = process_family
            .join(GENERATIONS_DIRECTORY)
            .join(TEST_GENERATION_ID)
            .join(MUX_SERVER_FILENAME);
        let metadata = std::fs::metadata(&executable).expect("inspect real fixture executable");
        let identity = GenerationObjectIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        assert!(
            acquire_linux_generation_lifetime_for_test(
                &managed_path,
                rustix::process::geteuid().as_raw(),
                identity
            )
            .is_err()
        );
    }

    #[test]
    fn shared_lifetime_holders_exclude_retirement_until_all_drop() {
        let fixture = fixture("lock", LeaseFixtureShape::Regular);
        let identity = fixture_executable_identity(&fixture);
        let uid = rustix::process::geteuid().as_raw();
        let first = acquire_fixture(&fixture, uid, identity).expect("acquire first shared lease");
        let second = acquire_fixture(&fixture, uid, identity).expect("acquire second shared lease");

        let exclusive = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&fixture.lease)
            .expect("open exclusive retirement probe");
        assert!(fs2::FileExt::try_lock_exclusive(&exclusive).is_err());
        drop(first);
        assert!(fs2::FileExt::try_lock_exclusive(&exclusive).is_err());
        drop(second);
        fs2::FileExt::try_lock_exclusive(&exclusive)
            .expect("exclusive retirement lease succeeds after all holders drop");
    }

    #[test]
    fn exclusive_retirement_owner_rejects_stale_launch_without_waiting() {
        let fixture = fixture("exclusive-first", LeaseFixtureShape::Regular);
        let exclusive = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&fixture.lease)
            .expect("open exclusive retirement authority");
        fs2::FileExt::try_lock_exclusive(&exclusive)
            .expect("acquire exclusive retirement authority");

        let executable = fixture.executable.clone();
        let identity = fixture_executable_identity(&fixture);
        let uid = rustix::process::geteuid().as_raw();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let launch = std::thread::spawn(move || {
            let result = acquire_linux_generation_lifetime_for_test(&executable, uid, identity);
            sender.send(result).expect("report stale-launch result");
        });
        let result = match receiver.recv_timeout(std::time::Duration::from_secs(2)) {
            Ok(result) => result,
            Err(error) => {
                // Release the probe so a mutation back to blocking flock cannot
                // strand the test process after the bounded assertion fails.
                drop(exclusive);
                launch.join().expect("settle blocked stale-launch probe");
                panic!("managed launch waited behind retirement instead of failing: {error}");
            }
        };
        let error = result
            .err()
            .expect("stale managed launch must fail behind exclusive retirement");
        assert!(error
            .to_string()
            .contains("nonblocking shared generation lifetime lease"));
        drop(exclusive);
        launch.join().expect("settle stale-launch probe");
    }

    #[test]
    fn fixture_paths_retain_exact_generation_layout() {
        let fixture = fixture("layout", LeaseFixtureShape::Regular);
        assert_eq!(
            fixture.generations.file_name().and_then(|name| name.to_str()),
            Some(GENERATIONS_DIRECTORY)
        );
        assert_eq!(
            fixture.generation.file_name().and_then(|name| name.to_str()),
            Some(TEST_GENERATION_ID)
        );
    }
}

#[cfg(test)]
mod path_tests {
    use super::*;

    const GENERATION_ID: &str =
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn managed_path(generation: &str) -> PathBuf {
        PathBuf::from("/srv/frankenterm")
            .join(PROCESS_FAMILY_DIRECTORY)
            .join(GENERATIONS_DIRECTORY)
            .join(generation)
            .join(MUX_SERVER_FILENAME)
    }

    #[test]
    fn standalone_path_is_explicitly_unmanaged() {
        assert_eq!(
            classify_executable_path(Path::new("/usr/local/bin/frankenterm-mux-server"))
                .expect("classify standalone path"),
            ExecutablePathClass::Unmanaged
        );
    }

    #[test]
    fn exact_managed_suffix_is_the_only_admitted_grammar() {
        let class = classify_executable_path(&managed_path(GENERATION_ID))
            .expect("classify canonical managed path");
        let ExecutablePathClass::Managed(managed) = class else {
            panic!("canonical managed path was classified as unmanaged");
        };
        assert_eq!(managed.generation_id, GENERATION_ID);
        assert_eq!(
            managed.generations_directory,
            PathBuf::from("/srv/frankenterm/process-family/generations")
        );
    }

    #[test]
    fn managed_hash_is_exact_lowercase_sha256() {
        for invalid in [
            "a",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "gggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggg",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcde/",
        ] {
            assert!(classify_executable_path(&managed_path(invalid)).is_err());
        }
    }

    #[test]
    fn managed_selector_and_noncanonical_paths_fail_closed() {
        let selector = PathBuf::from("/srv/frankenterm")
            .join(PROCESS_FAMILY_DIRECTORY)
            .join("current")
            .join(MUX_SERVER_FILENAME);
        assert!(classify_executable_path(&selector).is_err());
        assert!(
            classify_executable_path(Path::new(
                "/srv/frankenterm/process-family//generations/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef/frankenterm-mux-server"
            ))
            .is_err()
        );
        assert!(
            classify_executable_path(Path::new(
                "srv/process-family/generations/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef/frankenterm-mux-server"
            ))
            .is_err()
        );
    }
}
