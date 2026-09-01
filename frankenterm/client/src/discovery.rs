use anyhow::Context;
#[cfg(unix)]
use std::convert::TryFrom as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
#[cfg(unix)]
use wezterm_uds::UnixStream;

/// Canonical filename prefix for per-process FrankenTerm GUI mux sockets.
pub const GUI_SOCKET_PREFIX: &str = "frankenterm-gui-sock-";

/// Return the canonical runtime path for a FrankenTerm GUI mux socket.
#[must_use]
pub fn gui_socket_path_for_pid(pid: u32) -> PathBuf {
    config::RUNTIME_DIR.join(format!("{GUI_SOCKET_PREFIX}{pid}"))
}

#[cfg(test)]
fn is_gui_socket_name(name: &str) -> bool {
    parse_gui_socket_pid(name).is_some()
}

fn parse_gui_socket_pid(name: &str) -> Option<u32> {
    let pid = name.strip_prefix(GUI_SOCKET_PREFIX)?;
    if pid.is_empty() || !pid.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let parsed = pid.parse::<u32>().ok()?;
    (parsed != 0 && parsed.to_string() == pid).then_some(parsed)
}

#[cfg(unix)]
fn is_socket_entry(entry: &std::fs::DirEntry) -> bool {
    use std::os::unix::fs::FileTypeExt;

    entry
        .file_type()
        .is_ok_and(|file_type| file_type.is_socket())
}

#[cfg(not(unix))]
fn is_socket_entry(_entry: &std::fs::DirEntry) -> bool {
    true
}

#[cfg(unix)]
const STALE_GUI_SOCKET_QUARANTINE: &str = ".stale-gui-sockets";

#[cfg(unix)]
const SOCKET_LEASE_RECORD_MAX_BYTES: u64 = 256;

#[cfg(unix)]
fn process_is_proven_absent(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal zero performs existence/permission validation only. A
    // strictly positive, range-checked pid prevents process-group semantics.
    let result = unsafe { libc::kill(pid, 0) };
    result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

#[cfg(unix)]
fn socket_lock_path(socket_path: &Path) -> PathBuf {
    let mut path = socket_path.as_os_str().to_os_string();
    path.push(".lock");
    PathBuf::from(path)
}

#[cfg(unix)]
fn socket_object_matches(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};

    left.file_type().is_socket()
        && right.file_type().is_socket()
        && left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.uid() == right.uid()
        && left.nlink() == 1
        && right.nlink() == 1
}

#[cfg(unix)]
fn socket_identity_matches(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    socket_object_matches(left, right)
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

#[cfg(unix)]
fn canonical_socket_lease_record(
    socket_metadata: &std::fs::Metadata,
    publisher_pid: u32,
) -> String {
    use std::os::unix::fs::MetadataExt as _;

    format!(
        "FT_SOCKET_LEASE_V1 pid={publisher_pid} dev={} ino={} ctime={} ctime_nsec={}\n",
        socket_metadata.dev(),
        socket_metadata.ino(),
        socket_metadata.ctime(),
        socket_metadata.ctime_nsec(),
    )
}

#[cfg(unix)]
fn lock_identity_matches(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    left.is_file()
        && right.is_file()
        && left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.uid() == right.uid()
        && left.nlink() == 1
        && right.nlink() == 1
        && left.len() == right.len()
        && left.len() <= SOCKET_LEASE_RECORD_MAX_BYTES
        && left.mode() & 0o077 == 0
        && right.mode() & 0o077 == 0
}

#[cfg(unix)]
fn socket_lease_record_matches(
    lock_file: &std::fs::File,
    socket_metadata: &std::fs::Metadata,
    publisher_pid: u32,
) -> bool {
    use std::os::unix::fs::FileExt as _;

    let Ok(lock_metadata) = lock_file.metadata() else {
        return false;
    };
    if lock_metadata.len() == 0 || lock_metadata.len() > SOCKET_LEASE_RECORD_MAX_BYTES {
        return false;
    }
    let Ok(record_len) = usize::try_from(lock_metadata.len()) else {
        return false;
    };
    let mut observed = vec![0_u8; record_len];
    if lock_file.read_exact_at(&mut observed, 0).is_err() {
        return false;
    }
    observed == canonical_socket_lease_record(socket_metadata, publisher_pid).as_bytes()
}

#[cfg(unix)]
fn try_lock_stale_socket_owner(
    path: &Path,
    socket_metadata: &std::fs::Metadata,
    publisher_pid: u32,
    expected_uid: u32,
    expected_device: u64,
) -> Option<(std::fs::File, PathBuf, std::fs::Metadata)> {
    use fs2::FileExt as _;
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};

    let lock_path = socket_lock_path(path);
    let open_existing = || {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&lock_path)
    };
    // A missing lease means this is a legacy, non-cooperating publisher. We
    // cannot manufacture exclusion after observing its socket: an old GUI can
    // remove and rebind the same PID path between the stale probe and rename.
    // Such dead sockets remain undiscoverable but are retained in place.
    let lock_file = open_existing().ok()?;
    let lock_metadata = lock_file.metadata().ok()?;
    let named_lock_metadata = lock_path.symlink_metadata().ok()?;
    if socket_metadata.uid() != expected_uid
        || socket_metadata.dev() != expected_device
        || socket_metadata.nlink() != 1
        || lock_metadata.uid() != expected_uid
        || lock_metadata.dev() != expected_device
        || !lock_identity_matches(&lock_metadata, &named_lock_metadata)
    {
        return None;
    }

    // The nonblocking lock prevents discovery from waiting behind a live
    // listener. The file remains open in the returned tuple, retaining the
    // exclusive lock through both renames.
    lock_file.try_lock_exclusive().ok()?;

    let revalidated_socket = path.symlink_metadata().ok()?;
    let revalidated_lock = lock_path.symlink_metadata().ok()?;
    let open_lock_after = lock_file.metadata().ok()?;
    if !socket_identity_matches(socket_metadata, &revalidated_socket)
        || !lock_identity_matches(&lock_metadata, &revalidated_lock)
        || !lock_identity_matches(&lock_metadata, &open_lock_after)
        || !socket_lease_record_matches(&lock_file, socket_metadata, publisher_pid)
        || !process_is_proven_absent(publisher_pid)
    {
        return None;
    }
    Some((lock_file, lock_path, lock_metadata))
}

#[cfg(unix)]
fn create_quarantine_slot(
    runtime_dir: &Path,
    pid: u32,
    socket_metadata: &std::fs::Metadata,
    expected_uid: u32,
    expected_device: u64,
) -> Option<PathBuf> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let quarantine_root = runtime_dir.join(STALE_GUI_SOCKET_QUARANTINE);
    config::create_user_owned_dirs(&quarantine_root).ok()?;
    for attempt in 0..32_u8 {
        let slot = quarantine_root.join(format!(
            "{GUI_SOCKET_PREFIX}{pid}.dev-{}.ino-{}.attempt-{attempt}",
            socket_metadata.dev(),
            socket_metadata.ino()
        ));
        match std::fs::create_dir(&slot) {
            Ok(()) => {
                std::fs::set_permissions(&slot, std::fs::Permissions::from_mode(0o700)).ok()?;
                let metadata = slot.symlink_metadata().ok()?;
                if !metadata.is_dir()
                    || metadata.uid() != expected_uid
                    || metadata.dev() != expected_device
                    || metadata.mode() & 0o077 != 0
                {
                    return None;
                }
                return Some(slot);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return None,
        }
    }
    None
}

#[cfg(unix)]
fn quarantine_proven_stale_socket(
    runtime_dir: &Path,
    path: &Path,
    pid: u32,
    initial: &std::fs::Metadata,
) -> bool {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};

    let Ok(runtime_metadata) = runtime_dir.symlink_metadata() else {
        return false;
    };
    if !initial.file_type().is_socket()
        || !runtime_metadata.is_dir()
        || initial.uid() != runtime_metadata.uid()
        || initial.dev() != runtime_metadata.dev()
        || initial.nlink() != 1
    {
        return false;
    }
    let Some((_lock_file, lock_path, lock_metadata)) = try_lock_stale_socket_owner(
        path,
        initial,
        pid,
        runtime_metadata.uid(),
        runtime_metadata.dev(),
    ) else {
        return false;
    };
    let Some(quarantine_slot) = create_quarantine_slot(
        runtime_dir,
        pid,
        initial,
        runtime_metadata.uid(),
        runtime_metadata.dev(),
    ) else {
        return false;
    };
    let Ok(revalidated) = path.symlink_metadata() else {
        return false;
    };
    if !socket_identity_matches(initial, &revalidated) || !process_is_proven_absent(pid) {
        return false;
    }
    let quarantine_socket = quarantine_slot.join("socket");
    match std::fs::rename(path, &quarantine_socket) {
        Ok(()) => {
            let moved_matches = quarantine_socket
                .symlink_metadata()
                // A successful rename changes ctime on Unix. The full lease
                // incarnation was revalidated immediately before the move;
                // afterwards the stable object identity proves that the
                // admitted socket is the one retained in quarantine.
                .is_ok_and(|moved| socket_object_matches(initial, &moved));
            if !moved_matches {
                log::error!(
                    "stale GUI socket quarantine identity changed unexpectedly at {}",
                    quarantine_socket.display()
                );
                return false;
            }
            // Keep the lease at its canonical name. Renaming it would let a
            // second publisher create a distinct lock inode while a first
            // publisher was already blocked on this one.
            if !lock_path
                .symlink_metadata()
                .is_ok_and(|retained| lock_identity_matches(&lock_metadata, &retained))
            {
                log::error!(
                    "stale GUI socket lease identity changed unexpectedly at {}",
                    lock_path.display()
                );
            }
            log::info!(
                "quarantined stale GUI socket {} in {}",
                path.display(),
                quarantine_slot.display()
            );
            true
        }
        Err(error) => {
            log::debug!(
                "could not quarantine stale GUI socket {}: {error}",
                path.display()
            );
            false
        }
    }
}

/// There's a lot more code in this windows module than I thought I would need
/// to write.  Ostensibly, we could get away with making a symlink by taking
/// the SessionName environment variable, combining it with the class name
/// and using a symlink to point to the actual path.
/// Symlinks are problematic on Windows, and the SessionName environment
/// variable may not be set.
/// It's a bit of a chore to resolve the name, and then it would be more
/// of a chore to manage the symlink.
/// What this module does is logically equivalent to the above, except
/// that it creates a piece of shared memory in the per-desktop namespace.
/// While there is a lot of code in here, it is simpler overall because
/// the naming is managed by the OS, as well as automatically removing
/// the name from the namespace when there are no more references to it.
#[cfg(windows)]
mod windows {
    use super::*;
    use std::io::Error as IoError;
    use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
    use winapi::um::memoryapi::{
        CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
    };
    use winapi::um::synchapi::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
    use winapi::um::winbase::{INFINITE, WAIT_OBJECT_0};
    use winapi::um::winnt::{HANDLE, PAGE_READWRITE};

    const MAX_NAME: usize = 1024;

    /// Keeps the published name alive for the duration of the process.
    pub struct NameHolder {
        _mapping: FileMapping,
        _view: MappedView,
    }

    /// A Windows file mapping
    struct FileMapping {
        name: String,
        handle: HANDLE,
        size: usize,
    }

    impl Drop for FileMapping {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.handle) };
        }
    }

    impl FileMapping {
        /// Create a new or open an existing mapping with the specified name/size
        pub fn create(name: &str, size: usize) -> anyhow::Result<Self> {
            let wide_name = wide_string(&name);

            let handle = unsafe {
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    std::ptr::null_mut(),
                    PAGE_READWRITE,
                    0,
                    size as _,
                    wide_name.as_ptr(),
                )
            };
            if handle.is_null() {
                return Err(IoError::last_os_error())
                    .with_context(|| format!("creating shared memory with name {}", name));
            }
            Ok(Self {
                name: name.to_string(),
                handle,
                size,
            })
        }

        /// Attempt to open an existing mapping
        pub fn open(name: &str, size: usize) -> anyhow::Result<Self> {
            let wide_name = wide_string(&name);

            let handle = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, wide_name.as_ptr()) };
            if handle.is_null() {
                return Err(IoError::last_os_error())
                    .with_context(|| format!("creating shared memory with name {}", name));
            }
            Ok(Self {
                name: name.to_string(),
                handle,
                size,
            })
        }

        /// Map the mapping into the process address space
        pub fn map(&self) -> anyhow::Result<MappedView> {
            let buf =
                unsafe { MapViewOfFile(self.handle, FILE_MAP_ALL_ACCESS, 0, 0, self.size as _) };
            if buf.is_null() {
                return Err(IoError::last_os_error()).with_context(|| {
                    format!("mapping view of shared memory with name {}", self.name)
                });
            }
            Ok(MappedView {
                buf: buf as _,
                size: self.size,
            })
        }
    }

    /// A mutex that can be used to coordinate between processes
    struct NamedMutex {
        handle: HANDLE,
    }
    impl Drop for NamedMutex {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }

    impl NamedMutex {
        /// Create a mutex with the specified name
        pub fn new(name: &str) -> anyhow::Result<Self> {
            let wide_name = wide_string(name);
            let handle = unsafe { CreateMutexW(std::ptr::null_mut(), 0, wide_name.as_ptr()) };
            if handle.is_null() {
                return Err(IoError::last_os_error())
                    .with_context(|| format!("creating mutex name {}", name));
            }
            Ok(Self { handle })
        }

        /// Acquire the mutex, and perform `func` while the mutex is held.
        /// Once `func` returns, the mutex is released.
        /// Returns the result of `func`.
        pub fn with_lock<F, T>(&self, func: F) -> anyhow::Result<T>
        where
            F: FnOnce() -> anyhow::Result<T>,
        {
            let res = unsafe { WaitForSingleObject(self.handle, INFINITE) };
            if res != WAIT_OBJECT_0 {
                return Err(IoError::last_os_error()).context("acquire mutex");
            }

            let res = func();
            unsafe { ReleaseMutex(self.handle) };
            res
        }
    }

    /// A materialized view of a mapping
    struct MappedView {
        buf: *mut u8,
        size: usize,
    }

    impl Drop for MappedView {
        fn drop(&mut self) {
            unsafe {
                UnmapViewOfFile(self.buf as _);
            }
        }
    }

    impl MappedView {
        fn slice_mut(&mut self) -> &mut [u8] {
            unsafe { std::slice::from_raw_parts_mut(self.buf, self.size) }
        }

        fn slice(&self) -> &[u8] {
            unsafe { std::slice::from_raw_parts(self.buf, self.size) }
        }
    }

    impl NameHolder {
        /// Computes the names of the objects; they use Local scoped
        /// names so that we have one per desktop, rather than one
        /// system wide.
        fn compute_names(class_name: &str) -> (String, String) {
            let mutex_name = format!("Local\\wezterm-sock-mutex-{}", class_name);
            let map_name = format!("Local\\wezterm-sock-{}", class_name);
            (mutex_name, map_name)
        }

        /// Publish path as the path for class_name.
        pub fn new(path: &Path, class_name: &str) -> anyhow::Result<Self> {
            let (mutex_name, map_name) = Self::compute_names(class_name);
            let path = path
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("path has no file_name!?"))?
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("path is not UTF8!"))?
                .to_string();

            let mutex = NamedMutex::new(&mutex_name)?;
            mutex.with_lock(|| {
                let mapping = FileMapping::create(&map_name, MAX_NAME)?;
                let mut view = mapping.map()?;

                let target_slice = view.slice_mut();
                let len = path.len();

                target_slice[0..len].copy_from_slice(path.as_bytes());
                target_slice[len] = 0;

                log::debug!("published gui path as {}", path);

                Ok(Self {
                    _mapping: mapping,
                    _view: view,
                })
            })
        }

        /// Resolve the existing path for class_name
        pub fn resolve(class_name: &str) -> anyhow::Result<PathBuf> {
            let (mutex_name, map_name) = Self::compute_names(class_name);
            let mutex = NamedMutex::new(&mutex_name)?;
            mutex.with_lock(|| {
                let mapping = FileMapping::open(&map_name, MAX_NAME)?;
                let view = mapping.map()?;

                let source_slice = view.slice();
                let len = source_slice
                    .iter()
                    .position(|&c| c == 0)
                    .ok_or_else(|| anyhow::anyhow!("shared memory is not NUL terminated!"))?;

                let path = std::str::from_utf8(&source_slice[0..len])
                    .context("reading path from shared memory")?;

                let path: PathBuf = path.into();

                Ok(path)
            })
        }
    }

    /// Convert a rust string to a windows wide string
    fn wide_string(s: &str) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }
}

#[cfg(unix)]
mod unix {
    use super::*;

    pub struct NameHolder {
        published: PathBuf,
        name: PathBuf,
    }

    impl Drop for NameHolder {
        fn drop(&mut self) {
            // If it still points to us, remove the symlink
            if let Ok(target) = std::fs::read_link(&self.name) {
                if target == self.published {
                    log::trace!("removing {}", self.name.display());
                    std::fs::remove_file(&self.name).ok();
                }
            }
        }
    }

    impl NameHolder {
        fn compute_name(class_name: &str) -> String {
            #[cfg(not(target_os = "macos"))]
            {
                let config = config::configuration();
                if config.enable_wayland {
                    if let Ok(wayland) = std::env::var("WAYLAND_DISPLAY") {
                        return format!("wayland-{}-{}", wayland, class_name);
                    }
                    // We don't assume a default WAYLAND_DISPLAY here because
                    // we don't know if the default should be used or if we
                    // should fall back to X11 without connecting to wayland.
                    // We cannot introduce a dep on a wayland client library
                    // here, but we could potentially try to construct a
                    // unix domain socket client to see if our assumed default
                    // is a working unix socket.
                    // Something to fill in later as/when that question arises!
                }
                let x11 = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());
                format!("x11-{}-{}", x11, class_name)
            }
            #[cfg(target_os = "macos")]
            {
                format!("default-{}", class_name)
            }
        }

        fn compute_path(class_name: &str) -> PathBuf {
            config::RUNTIME_DIR.join(Self::compute_name(class_name))
        }

        pub fn new(path: &Path, class_name: &str) -> anyhow::Result<Self> {
            let name = Self::compute_path(class_name);
            std::fs::remove_file(&name).ok();
            std::os::unix::fs::symlink(path, &name)
                .with_context(|| format!("pointing {} -> {}", name.display(), path.display()))?;
            Ok(Self {
                published: path.to_path_buf(),
                name,
            })
        }

        pub fn resolve(class_name: &str) -> anyhow::Result<PathBuf> {
            let name = Self::compute_path(class_name);
            std::fs::read_link(&name).with_context(|| format!("reading symlink {}", name.display()))
        }
    }
}

#[cfg(windows)]
pub use self::windows::NameHolder;

#[cfg(unix)]
pub use self::unix::NameHolder;

/// Unconditionally update the published path to match the provided path,
/// even if there is a running instance with a legitimate published path.
pub fn publish_gui_sock_path(path: &Path, class_name: &str) -> anyhow::Result<NameHolder> {
    NameHolder::new(path, class_name)
}

/// Resolve the last published path for `class_name`.
/// If successful, there is NO guarantee that the returned path references
/// a running instance; it is just the last published path.
pub fn resolve_gui_sock_path(class_name: &str) -> anyhow::Result<PathBuf> {
    NameHolder::resolve(class_name)
}

/// This function returns a list of the `frankenterm-gui-sock-<pid>` paths in
/// the runtime dir. These represent the locally running FrankenTerm GUI
/// instances.
/// The list is pruned of any entries that are not live
/// and then sorted with the eldest instance first.
pub fn discover_gui_socks() -> Vec<PathBuf> {
    discover_gui_socks_in(config::RUNTIME_DIR.as_path())
}

fn discover_gui_socks_in(runtime_dir: &Path) -> Vec<PathBuf> {
    #[derive(Debug)]
    struct Entry {
        path: PathBuf,
        age: Duration,
    }
    let mut socks: Vec<Entry> = vec![];

    /// Get an idea of the age of the entry.
    /// Some filesystems don't support reporting `created`,
    /// so fall back on `modified`.
    fn meta_age(meta: &std::fs::Metadata) -> Duration {
        let t = if let Ok(created) = meta.created() {
            created
        } else if let Ok(changed) = meta.modified() {
            changed
        } else {
            return Duration::from_millis(300);
        };
        if let Ok(d) = SystemTime::now().duration_since(t) {
            d
        } else {
            Duration::from_millis(300)
        }
    }

    if config::create_user_owned_dirs(runtime_dir).is_err() {
        return Vec::new();
    }
    if let Ok(dir) = std::fs::read_dir(runtime_dir) {
        for entry in dir.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if let Some(pid) = parse_gui_socket_pid(name) {
                    if !is_socket_entry(&entry) {
                        continue;
                    }
                    let path = entry.path();
                    if let Ok(meta) = path.symlink_metadata() {
                        let age = meta_age(&meta);
                        #[cfg(unix)]
                        if is_sock_dead(&path) && age > Duration::from_secs(1) {
                            let _ = quarantine_proven_stale_socket(runtime_dir, &path, pid, &meta);
                            // Discovery truth is independent of maintenance:
                            // a non-listening endpoint is never advertised even
                            // when quarantine correctly refuses unsafe evidence.
                            continue;
                        }

                        socks.push(Entry { path, age });
                    }
                }
            }
        }
    }

    socks.sort_by(|a, b| a.age.cmp(&b.age).reverse());
    log::trace!("{:?}", socks);
    socks.into_iter().map(|e| e.path).collect()
}

#[cfg(unix)]
fn is_sock_dead(sock: &std::path::Path) -> bool {
    match UnixStream::connect(sock) {
        Ok(_) => false,
        Err(error) => is_definitive_dead_socket_error(&error),
    }
}

#[cfg(unix)]
fn is_definitive_dead_socket_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn proven_absent_test_pid() -> u32 {
        (1_000_000..1_001_000)
            .find(|pid| process_is_proven_absent(*pid))
            .expect("the test host must expose one absent PID in the reserved high range")
    }

    #[cfg(unix)]
    fn write_socket_lease_record(lock_file: &mut std::fs::File, socket: &Path, publisher_pid: u32) {
        use std::io::{Seek as _, Write as _};

        let metadata = socket
            .symlink_metadata()
            .expect("inspect socket before writing lease record");
        let record = canonical_socket_lease_record(&metadata, publisher_pid);
        lock_file.set_len(0).expect("truncate socket lease record");
        lock_file.rewind().expect("rewind socket lease record");
        lock_file
            .write_all(record.as_bytes())
            .expect("write socket lease record");
        lock_file.sync_all().expect("sync socket lease record");
    }

    #[test]
    fn canonical_gui_socket_path_uses_frankenterm_prefix() {
        let path = gui_socket_path_for_pid(42);
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("frankenterm-gui-sock-42")
        );
    }

    #[test]
    fn gui_socket_name_requires_canonical_numeric_pid() {
        assert!(is_gui_socket_name("frankenterm-gui-sock-42"));
        assert!(!is_gui_socket_name("gui-sock-42"));
        assert!(!is_gui_socket_name("frankenterm-gui-sock-"));
        assert!(!is_gui_socket_name("frankenterm-gui-sock-not-a-pid"));
        assert!(!is_gui_socket_name("frankenterm-gui-sock-0"));
        assert!(!is_gui_socket_name("frankenterm-gui-sock-00042"));
        assert!(!is_gui_socket_name("frankenterm-gui-sock-4294967296"));
    }

    #[cfg(unix)]
    #[test]
    fn current_process_is_never_classified_as_proven_absent() {
        assert!(!process_is_proven_absent(std::process::id()));
        assert!(!process_is_proven_absent(0));
        assert!(!process_is_proven_absent(u32::MAX));
    }

    #[cfg(unix)]
    #[test]
    fn live_socket_lease_prevents_quarantine() {
        use fs2::FileExt as _;
        use std::os::unix::fs::PermissionsExt as _;

        let runtime = tempfile::tempdir().expect("live socket runtime");
        let socket = runtime.path().join("frankenterm-gui-sock-42");
        let lock = socket_lock_path(&socket);
        let lock_file = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&lock)
            .expect("create socket lease");
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o600))
            .expect("make socket lease private");
        lock_file.lock_exclusive().expect("lock live socket lease");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind live socket");
        let metadata = socket.symlink_metadata().expect("inspect live socket");

        assert!(!quarantine_proven_stale_socket(
            runtime.path(),
            &socket,
            42,
            &metadata,
        ));
        assert!(socket.exists());
        assert!(lock.exists());
    }

    #[cfg(unix)]
    #[test]
    fn transient_or_permission_connect_errors_do_not_prove_socket_death() {
        for kind in [
            std::io::ErrorKind::WouldBlock,
            std::io::ErrorKind::TimedOut,
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::Interrupted,
        ] {
            assert!(!is_definitive_dead_socket_error(&std::io::Error::from(
                kind
            )));
        }
        for kind in [
            std::io::ErrorKind::ConnectionRefused,
            std::io::ErrorKind::NotFound,
        ] {
            assert!(is_definitive_dead_socket_error(&std::io::Error::from(kind)));
        }
    }

    #[cfg(unix)]
    #[test]
    fn unlocked_dead_socket_and_lease_are_quarantined_without_deletion() {
        use std::os::unix::fs::PermissionsExt as _;

        let runtime = tempfile::tempdir().expect("stale socket runtime");
        let pid = proven_absent_test_pid();
        let socket = runtime.path().join(format!("frankenterm-gui-sock-{pid}"));
        let lock = socket_lock_path(&socket);
        let mut lock_file = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&lock)
            .expect("create stale socket lease");
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o600))
            .expect("make stale socket lease private");
        let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind stale socket");
        write_socket_lease_record(&mut lock_file, &socket, pid);
        let metadata = socket.symlink_metadata().expect("inspect stale socket");
        drop(listener);
        drop(lock_file);

        assert!(quarantine_proven_stale_socket(
            runtime.path(),
            &socket,
            pid,
            &metadata,
        ));
        assert!(!socket.exists());
        assert!(lock.exists());
        let quarantine = runtime.path().join(STALE_GUI_SOCKET_QUARANTINE);
        let slots = std::fs::read_dir(quarantine)
            .expect("read quarantine")
            .collect::<Result<Vec<_>, _>>()
            .expect("read quarantine entries");
        assert_eq!(slots.len(), 1);
        assert!(slots[0].path().join("socket").exists());
        assert!(!slots[0].path().join("socket.lock").exists());
        assert!(!quarantine_proven_stale_socket(
            runtime.path(),
            &socket,
            pid,
            &metadata,
        ));
    }

    #[cfg(unix)]
    #[test]
    fn stale_lease_record_cannot_quarantine_a_rebound_socket_incarnation() {
        use std::os::unix::fs::PermissionsExt as _;

        let runtime = tempfile::tempdir().expect("rebound socket runtime");
        std::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o700))
            .expect("make runtime private");
        let pid = proven_absent_test_pid();
        let socket = runtime.path().join(format!("frankenterm-gui-sock-{pid}"));
        let lock = socket_lock_path(&socket);
        let mut lock_file = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&lock)
            .expect("create original socket lease");
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o600))
            .expect("make original socket lease private");
        let original_listener =
            std::os::unix::net::UnixListener::bind(&socket).expect("bind original socket");
        write_socket_lease_record(&mut lock_file, &socket, pid);
        drop(original_listener);
        drop(lock_file);

        let retained_original = runtime.path().join("retained-original-socket");
        std::fs::rename(&socket, &retained_original).expect("retain original socket incarnation");
        drop(std::os::unix::net::UnixListener::bind(&socket).expect("bind replacement socket"));
        let replacement_metadata = socket
            .symlink_metadata()
            .expect("inspect replacement socket incarnation");

        assert!(!quarantine_proven_stale_socket(
            runtime.path(),
            &socket,
            pid,
            &replacement_metadata,
        ));
        assert!(socket.exists());
        assert!(retained_original.exists());
        assert!(lock.exists());
    }

    #[cfg(unix)]
    #[test]
    fn empty_or_malformed_lease_record_refuses_quarantine() {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;

        for record in [b"".as_slice(), b"not-a-socket-lease\n".as_slice()] {
            let runtime = tempfile::tempdir().expect("invalid lease runtime");
            std::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o700))
                .expect("make runtime private");
            let pid = proven_absent_test_pid();
            let socket = runtime.path().join(format!("frankenterm-gui-sock-{pid}"));
            drop(std::os::unix::net::UnixListener::bind(&socket).expect("bind stale socket"));
            let lock = socket_lock_path(&socket);
            let mut lock_file = std::fs::OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(&lock)
                .expect("create invalid socket lease");
            std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o600))
                .expect("make invalid socket lease private");
            lock_file.write_all(record).expect("write invalid lease");
            lock_file.sync_all().expect("sync invalid lease");
            drop(lock_file);
            let metadata = socket.symlink_metadata().expect("inspect stale socket");

            assert!(!quarantine_proven_stale_socket(
                runtime.path(),
                &socket,
                pid,
                &metadata,
            ));
            assert!(socket.exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn dead_legacy_socket_without_cooperative_lease_is_retained() {
        use std::os::unix::fs::PermissionsExt as _;

        let runtime = tempfile::tempdir().expect("legacy socket runtime");
        std::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o700))
            .expect("make runtime private");
        let socket = runtime.path().join("frankenterm-gui-sock-91");
        drop(std::os::unix::net::UnixListener::bind(&socket).expect("bind legacy socket"));
        let metadata = socket.symlink_metadata().expect("inspect legacy socket");

        assert!(!quarantine_proven_stale_socket(
            runtime.path(),
            &socket,
            91,
            &metadata,
        ));
        assert!(socket.exists());
        let lock = socket_lock_path(&socket);
        assert!(!lock.exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_or_hardlinked_lease_refuses_quarantine() {
        use std::os::unix::fs::{symlink, PermissionsExt as _};

        for hardlink in [false, true] {
            let runtime = tempfile::tempdir().expect("unsafe lease runtime");
            std::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o700))
                .expect("make runtime private");
            let socket = runtime.path().join("frankenterm-gui-sock-117");
            drop(std::os::unix::net::UnixListener::bind(&socket).expect("bind stale socket"));
            let lock = socket_lock_path(&socket);
            let target = runtime.path().join("lease-target");
            std::fs::File::create(&target).expect("create unsafe lease target");
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600))
                .expect("make unsafe target private");
            if hardlink {
                std::fs::hard_link(&target, &lock).expect("create hardlinked lease");
            } else {
                symlink(&target, &lock).expect("create symlinked lease");
            }
            let metadata = socket.symlink_metadata().expect("inspect stale socket");

            assert!(!quarantine_proven_stale_socket(
                runtime.path(),
                &socket,
                117,
                &metadata,
            ));
            assert!(socket.exists());
        }
    }
}
