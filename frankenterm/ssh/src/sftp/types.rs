use bitflags::bitflags;

bitflags! {
    struct FileTypeFlags: u32 {
        const DIR = 0o040000;
        const FILE = 0o100000;
        const SYMLINK = 0o120000;
    }
}

bitflags! {
    struct FilePermissionFlags: u32 {
        const OWNER_READ = 0o400;
        const OWNER_WRITE = 0o200;
        const OWNER_EXEC = 0o100;
        const GROUP_READ = 0o40;
        const GROUP_WRITE = 0o20;
        const GROUP_EXEC = 0o10;
        const OTHER_READ = 0o4;
        const OTHER_WRITE = 0o2;
        const OTHER_EXEC = 0o1;
    }
}

/// Represents the type associated with a remote file
#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
pub enum FileType {
    Dir,
    File,
    Symlink,
    Other,
}

impl FileType {
    /// Returns true if file is a type of directory
    pub fn is_dir(self) -> bool {
        matches!(self, Self::Dir)
    }

    /// Returns true if file is a type of regular file
    pub fn is_file(self) -> bool {
        matches!(self, Self::File)
    }

    /// Returns true if file is a type of symlink
    pub fn is_symlink(self) -> bool {
        matches!(self, Self::Symlink)
    }

    /// Create from a unix mode bitset
    pub fn from_unix_mode(mode: u32) -> Self {
        let flags = FileTypeFlags::from_bits_truncate(mode);
        if flags.contains(FileTypeFlags::DIR) {
            Self::Dir
        } else if flags.contains(FileTypeFlags::FILE) {
            Self::File
        } else if flags.contains(FileTypeFlags::SYMLINK) {
            Self::Symlink
        } else {
            Self::Other
        }
    }

    /// Convert to a unix mode bitset
    pub fn to_unix_mode(self) -> u32 {
        let flags = match self {
            FileType::Dir => FileTypeFlags::DIR,
            FileType::File => FileTypeFlags::FILE,
            FileType::Symlink => FileTypeFlags::SYMLINK,
            FileType::Other => FileTypeFlags::empty(),
        };

        flags.bits
    }
}

/// Represents permissions associated with a remote file
#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
pub struct FilePermissions {
    pub owner_read: bool,
    pub owner_write: bool,
    pub owner_exec: bool,

    pub group_read: bool,
    pub group_write: bool,
    pub group_exec: bool,

    pub other_read: bool,
    pub other_write: bool,
    pub other_exec: bool,
}

impl FilePermissions {
    /// Returns true if all write permissions (owner, group, other) are false.
    pub fn is_readonly(self) -> bool {
        !(self.owner_write || self.group_write || self.other_write)
    }

    /// Create from a unix mode bitset
    pub fn from_unix_mode(mode: u32) -> Self {
        let flags = FilePermissionFlags::from_bits_truncate(mode);
        Self {
            owner_read: flags.contains(FilePermissionFlags::OWNER_READ),
            owner_write: flags.contains(FilePermissionFlags::OWNER_WRITE),
            owner_exec: flags.contains(FilePermissionFlags::OWNER_EXEC),
            group_read: flags.contains(FilePermissionFlags::GROUP_READ),
            group_write: flags.contains(FilePermissionFlags::GROUP_WRITE),
            group_exec: flags.contains(FilePermissionFlags::GROUP_EXEC),
            other_read: flags.contains(FilePermissionFlags::OTHER_READ),
            other_write: flags.contains(FilePermissionFlags::OTHER_WRITE),
            other_exec: flags.contains(FilePermissionFlags::OTHER_EXEC),
        }
    }

    /// Convert to a unix mode bitset
    pub fn to_unix_mode(self) -> u32 {
        let mut flags = FilePermissionFlags::empty();

        if self.owner_read {
            flags.insert(FilePermissionFlags::OWNER_READ);
        }
        if self.owner_write {
            flags.insert(FilePermissionFlags::OWNER_WRITE);
        }
        if self.owner_exec {
            flags.insert(FilePermissionFlags::OWNER_EXEC);
        }

        if self.group_read {
            flags.insert(FilePermissionFlags::GROUP_READ);
        }
        if self.group_write {
            flags.insert(FilePermissionFlags::GROUP_WRITE);
        }
        if self.group_exec {
            flags.insert(FilePermissionFlags::GROUP_EXEC);
        }

        if self.other_read {
            flags.insert(FilePermissionFlags::OTHER_READ);
        }
        if self.other_write {
            flags.insert(FilePermissionFlags::OTHER_WRITE);
        }
        if self.other_exec {
            flags.insert(FilePermissionFlags::OTHER_EXEC);
        }

        flags.bits
    }
}

/// Represents metadata about a remote file
#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
pub struct Metadata {
    /// Type of the remote file
    pub ty: FileType,

    /// Permissions associated with the file
    pub permissions: Option<FilePermissions>,

    /// File size, in bytes of the file
    pub size: Option<u64>,

    /// Owner ID of the file
    pub uid: Option<u32>,

    /// Owning group of the file
    pub gid: Option<u32>,

    /// Last access time of the file
    pub accessed: Option<u64>,

    /// Last modification time of the file
    pub modified: Option<u64>,
}

impl Metadata {
    /// Returns true if metadata is for a directory
    pub fn is_dir(self) -> bool {
        self.ty.is_dir()
    }

    /// Returns true if metadata is for a regular file
    pub fn is_file(self) -> bool {
        self.ty.is_file()
    }

    /// Returns true if metadata is for a symlink
    pub fn is_symlink(self) -> bool {
        self.ty.is_symlink()
    }
}

/// Represents options to provide when opening a file or directory
#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
pub struct OpenOptions {
    /// If true, opens a file (or directory) for reading
    pub read: bool,

    /// If provided, opens a file for writing or appending
    pub write: Option<WriteMode>,

    /// Unix mode that is used when creating a new file
    pub mode: i32,

    /// Whether opening a file or directory
    pub ty: OpenFileType,
}

/// Represents whether opening a file or directory
#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
pub enum OpenFileType {
    Dir,
    File,
}

/// Represents different writing modes for opening a file
#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
pub enum WriteMode {
    /// Append data to end of file instead of overwriting it
    Append,

    /// Overwrite an existing file when opening to write it
    Write,
}

/// Represents options to provide when renaming a file or directory
#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
pub struct RenameOptions {
    /// Overwrite the destination if it exists, otherwise fail
    pub overwrite: bool,

    /// Request atomic rename operation
    pub atomic: bool,

    /// Request native system calls
    pub native: bool,
}

impl Default for RenameOptions {
    /// Default is to enable all options
    fn default() -> Self {
        Self {
            overwrite: true,
            atomic: true,
            native: true,
        }
    }
}

/// Contains libssh2-specific implementations
#[cfg(feature = "ssh2")]
mod ssh2_impl {
    use super::*;
    use ::ssh2::{
        FileStat as Ssh2FileStat, FileType as Ssh2FileType, OpenFlags as Ssh2OpenFlags,
        OpenType as Ssh2OpenType, RenameFlags as Ssh2RenameFlags,
    };

    impl From<OpenFileType> for Ssh2OpenType {
        fn from(ty: OpenFileType) -> Self {
            match ty {
                OpenFileType::Dir => Self::Dir,
                OpenFileType::File => Self::File,
            }
        }
    }

    impl From<RenameOptions> for Ssh2RenameFlags {
        fn from(opts: RenameOptions) -> Self {
            let mut flags = Self::empty();

            if opts.overwrite {
                flags |= Self::OVERWRITE;
            }

            if opts.atomic {
                flags |= Self::ATOMIC;
            }

            if opts.native {
                flags |= Self::NATIVE;
            }

            flags
        }
    }

    impl From<OpenOptions> for Ssh2OpenFlags {
        fn from(opts: OpenOptions) -> Self {
            let mut flags = Self::empty();

            if opts.read {
                flags |= Self::READ;
            }

            match opts.write {
                Some(WriteMode::Write) => flags |= Self::WRITE | Self::TRUNCATE,
                Some(WriteMode::Append) => flags |= Self::WRITE | Self::APPEND | Self::CREATE,
                None => {}
            }

            flags
        }
    }

    impl From<Ssh2FileType> for FileType {
        fn from(ft: Ssh2FileType) -> Self {
            if ft.is_dir() {
                Self::Dir
            } else if ft.is_file() {
                Self::File
            } else if ft.is_symlink() {
                Self::Symlink
            } else {
                Self::Other
            }
        }
    }

    impl From<Ssh2FileStat> for Metadata {
        fn from(stat: Ssh2FileStat) -> Self {
            Self {
                ty: FileType::from(stat.file_type()),
                permissions: stat.perm.map(FilePermissions::from_unix_mode),
                size: stat.size,
                uid: stat.uid,
                gid: stat.gid,
                accessed: stat.atime,
                modified: stat.mtime,
            }
        }
    }

    impl From<Metadata> for Ssh2FileStat {
        fn from(metadata: Metadata) -> Self {
            let ft = metadata.ty;

            Self {
                perm: metadata
                    .permissions
                    .map(|p| p.to_unix_mode() | ft.to_unix_mode()),
                size: metadata.size,
                uid: metadata.uid,
                gid: metadata.gid,
                atime: metadata.accessed,
                mtime: metadata.modified,
            }
        }
    }
}

#[cfg(feature = "libssh-rs")]
mod libssh_impl {
    use super::*;
    use std::time::SystemTime;

    impl From<libssh_rs::FileType> for FileType {
        fn from(ft: libssh_rs::FileType) -> Self {
            match ft {
                libssh_rs::FileType::Directory => Self::Dir,
                libssh_rs::FileType::Regular => Self::File,
                libssh_rs::FileType::Symlink => Self::Symlink,
                _ => Self::Other,
            }
        }
    }

    fn sys_time_to_unix(t: SystemTime) -> u64 {
        t.duration_since(SystemTime::UNIX_EPOCH)
            .expect("UNIX_EPOCH < SystemTime")
            .as_secs()
    }

    fn unix_to_sys(u: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(u)
    }

    impl From<libssh_rs::Metadata> for Metadata {
        fn from(stat: libssh_rs::Metadata) -> Self {
            Self {
                ty: stat
                    .file_type()
                    .map(FileType::from)
                    .unwrap_or(FileType::Other),
                permissions: stat.permissions().map(FilePermissions::from_unix_mode),
                size: stat.len(),
                uid: stat.uid(),
                gid: stat.gid(),
                accessed: stat.accessed().map(sys_time_to_unix),
                modified: stat.modified().map(sys_time_to_unix),
            }
        }
    }

    impl Into<libssh_rs::SetAttributes> for Metadata {
        fn into(self) -> libssh_rs::SetAttributes {
            let size = self.size;
            let uid_gid = match (self.uid, self.gid) {
                (Some(uid), Some(gid)) => Some((uid, gid)),
                _ => None,
            };
            let permissions = self.permissions.map(FilePermissions::to_unix_mode);
            let atime_mtime = match (self.accessed, self.modified) {
                (Some(a), Some(m)) => {
                    let a = unix_to_sys(a);
                    let m = unix_to_sys(m);
                    Some((a, m))
                }
                _ => None,
            };
            libssh_rs::SetAttributes {
                size,
                uid_gid,
                permissions,
                atime_mtime,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_type_is_dir() {
        assert!(FileType::Dir.is_dir());
        assert!(!FileType::Dir.is_file());
        assert!(!FileType::Dir.is_symlink());
    }

    #[test]
    fn file_type_is_file() {
        assert!(!FileType::File.is_dir());
        assert!(FileType::File.is_file());
        assert!(!FileType::File.is_symlink());
    }

    #[test]
    fn file_type_is_symlink() {
        assert!(!FileType::Symlink.is_dir());
        assert!(!FileType::Symlink.is_file());
        assert!(FileType::Symlink.is_symlink());
    }

    #[test]
    fn file_type_other() {
        assert!(!FileType::Other.is_dir());
        assert!(!FileType::Other.is_file());
        assert!(!FileType::Other.is_symlink());
    }

    #[test]
    fn file_type_from_unix_mode_dir() {
        assert_eq!(FileType::from_unix_mode(0o040755), FileType::Dir);
    }

    #[test]
    fn file_type_from_unix_mode_file() {
        assert_eq!(FileType::from_unix_mode(0o100644), FileType::File);
    }

    #[test]
    fn file_type_from_unix_mode_symlink() {
        assert_eq!(FileType::from_unix_mode(0o120777), FileType::Symlink);
    }

    #[test]
    fn file_type_from_unix_mode_other() {
        assert_eq!(FileType::from_unix_mode(0o010644), FileType::Other);
    }

    #[test]
    fn file_type_to_unix_mode_roundtrip() {
        for ft in [FileType::Dir, FileType::File, FileType::Symlink] {
            let mode = ft.to_unix_mode();
            assert_eq!(FileType::from_unix_mode(mode), ft);
        }
    }

    #[test]
    fn file_type_other_to_unix_mode() {
        assert_eq!(FileType::Other.to_unix_mode(), 0);
    }

    #[test]
    fn file_permissions_from_unix_mode_755() {
        let perms = FilePermissions::from_unix_mode(0o755);
        assert!(perms.owner_read);
        assert!(perms.owner_write);
        assert!(perms.owner_exec);
        assert!(perms.group_read);
        assert!(!perms.group_write);
        assert!(perms.group_exec);
        assert!(perms.other_read);
        assert!(!perms.other_write);
        assert!(perms.other_exec);
    }

    #[test]
    fn file_permissions_from_unix_mode_644() {
        let perms = FilePermissions::from_unix_mode(0o644);
        assert!(perms.owner_read);
        assert!(perms.owner_write);
        assert!(!perms.owner_exec);
        assert!(perms.group_read);
        assert!(!perms.group_write);
        assert!(!perms.group_exec);
        assert!(perms.other_read);
        assert!(!perms.other_write);
        assert!(!perms.other_exec);
    }

    #[test]
    fn file_permissions_from_unix_mode_000() {
        let perms = FilePermissions::from_unix_mode(0o000);
        assert!(!perms.owner_read);
        assert!(!perms.owner_write);
        assert!(!perms.owner_exec);
        assert!(!perms.group_read);
        assert!(!perms.group_write);
        assert!(!perms.group_exec);
        assert!(!perms.other_read);
        assert!(!perms.other_write);
        assert!(!perms.other_exec);
    }

    #[test]
    fn file_permissions_from_unix_mode_777() {
        let perms = FilePermissions::from_unix_mode(0o777);
        assert!(perms.owner_read);
        assert!(perms.owner_write);
        assert!(perms.owner_exec);
        assert!(perms.group_read);
        assert!(perms.group_write);
        assert!(perms.group_exec);
        assert!(perms.other_read);
        assert!(perms.other_write);
        assert!(perms.other_exec);
    }

    #[test]
    fn file_permissions_to_unix_mode_roundtrip() {
        for mode in [0o000, 0o400, 0o644, 0o755, 0o777] {
            let perms = FilePermissions::from_unix_mode(mode);
            assert_eq!(perms.to_unix_mode(), mode);
        }
    }

    #[test]
    fn file_permissions_is_readonly() {
        let readonly = FilePermissions::from_unix_mode(0o444);
        assert!(readonly.is_readonly());

        let writable = FilePermissions::from_unix_mode(0o644);
        assert!(!writable.is_readonly());
    }

    #[test]
    fn file_permissions_is_readonly_group_write() {
        let perms = FilePermissions::from_unix_mode(0o464);
        assert!(!perms.is_readonly());
    }

    #[test]
    fn file_permissions_is_readonly_other_write() {
        let perms = FilePermissions::from_unix_mode(0o446);
        assert!(!perms.is_readonly());
    }

    #[test]
    fn metadata_is_dir() {
        let meta = Metadata {
            ty: FileType::Dir,
            permissions: None,
            size: None,
            uid: None,
            gid: None,
            accessed: None,
            modified: None,
        };
        assert!(meta.is_dir());
        assert!(!meta.is_file());
        assert!(!meta.is_symlink());
    }

    #[test]
    fn metadata_is_file() {
        let meta = Metadata {
            ty: FileType::File,
            permissions: Some(FilePermissions::from_unix_mode(0o644)),
            size: Some(1024),
            uid: Some(1000),
            gid: Some(1000),
            accessed: Some(1000000),
            modified: Some(2000000),
        };
        assert!(!meta.is_dir());
        assert!(meta.is_file());
        assert!(!meta.is_symlink());
        assert_eq!(meta.size, Some(1024));
    }

    #[test]
    fn metadata_equality() {
        let a = Metadata {
            ty: FileType::File,
            permissions: Some(FilePermissions::from_unix_mode(0o644)),
            size: Some(100),
            uid: None,
            gid: None,
            accessed: None,
            modified: None,
        };
        let b = a;
        assert_eq!(a, b);
    }

    #[test]
    fn open_options_equality() {
        let a = OpenOptions {
            read: true,
            write: Some(WriteMode::Write),
            mode: 0o644,
            ty: OpenFileType::File,
        };
        let b = OpenOptions {
            read: true,
            write: Some(WriteMode::Write),
            mode: 0o644,
            ty: OpenFileType::File,
        };
        assert_eq!(a, b);
    }

    #[test]
    fn open_options_inequality() {
        let a = OpenOptions {
            read: true,
            write: None,
            mode: 0o644,
            ty: OpenFileType::File,
        };
        let b = OpenOptions {
            read: true,
            write: Some(WriteMode::Append),
            mode: 0o644,
            ty: OpenFileType::File,
        };
        assert_ne!(a, b);
    }

    #[test]
    fn rename_options_default() {
        let opts = RenameOptions::default();
        assert!(opts.overwrite);
        assert!(opts.atomic);
        assert!(opts.native);
    }

    #[test]
    fn write_mode_variants() {
        assert_ne!(WriteMode::Append, WriteMode::Write);
        assert_eq!(WriteMode::Append, WriteMode::Append);
    }

    #[test]
    fn open_file_type_variants() {
        assert_ne!(OpenFileType::Dir, OpenFileType::File);
        assert_eq!(OpenFileType::Dir, OpenFileType::Dir);
    }

    #[test]
    fn file_type_clone_and_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(FileType::Dir);
        set.insert(FileType::File);
        set.insert(FileType::Symlink);
        set.insert(FileType::Other);
        assert_eq!(set.len(), 4);
    }

    #[test]
    fn metadata_clone() {
        let meta = Metadata {
            ty: FileType::File,
            permissions: Some(FilePermissions::from_unix_mode(0o755)),
            size: Some(42),
            uid: Some(0),
            gid: Some(0),
            accessed: Some(100),
            modified: Some(200),
        };
        let cloned = meta;
        assert_eq!(meta, cloned);
    }

    #[test]
    fn file_type_debug_format() {
        assert_eq!(format!("{:?}", FileType::Dir), "Dir");
        assert_eq!(format!("{:?}", FileType::File), "File");
        assert_eq!(format!("{:?}", FileType::Symlink), "Symlink");
        assert_eq!(format!("{:?}", FileType::Other), "Other");
    }

    #[test]
    fn file_permissions_debug_format() {
        let perms = FilePermissions::from_unix_mode(0o644);
        let dbg = format!("{:?}", perms);
        assert!(dbg.contains("FilePermissions"));
        assert!(dbg.contains("owner_read: true"));
        assert!(dbg.contains("owner_write: true"));
        assert!(dbg.contains("owner_exec: false"));
    }

    #[test]
    fn metadata_is_symlink() {
        let meta = Metadata {
            ty: FileType::Symlink,
            permissions: None,
            size: None,
            uid: None,
            gid: None,
            accessed: None,
            modified: None,
        };
        assert!(meta.is_symlink());
        assert!(!meta.is_file());
        assert!(!meta.is_dir());
    }

    #[test]
    fn metadata_all_none_fields() {
        let meta = Metadata {
            ty: FileType::Other,
            permissions: None,
            size: None,
            uid: None,
            gid: None,
            accessed: None,
            modified: None,
        };
        assert!(!meta.is_dir());
        assert!(!meta.is_file());
        assert!(!meta.is_symlink());
        assert_eq!(meta.size, None);
        assert_eq!(meta.uid, None);
        assert_eq!(meta.gid, None);
    }

    #[test]
    fn open_options_read_only() {
        let opts = OpenOptions {
            read: true,
            write: None,
            mode: 0o644,
            ty: OpenFileType::File,
        };
        assert!(opts.read);
        assert!(opts.write.is_none());
    }

    #[test]
    fn open_options_write_append() {
        let opts = OpenOptions {
            read: false,
            write: Some(WriteMode::Append),
            mode: 0o644,
            ty: OpenFileType::File,
        };
        assert!(!opts.read);
        assert_eq!(opts.write, Some(WriteMode::Append));
    }

    #[test]
    fn rename_options_custom_no_atomic() {
        let opts = RenameOptions {
            overwrite: true,
            atomic: false,
            native: false,
        };
        assert!(opts.overwrite);
        assert!(!opts.atomic);
        assert!(!opts.native);
    }

    #[test]
    fn file_permissions_owner_exec_only() {
        let perms = FilePermissions::from_unix_mode(0o100);
        assert!(!perms.owner_read);
        assert!(!perms.owner_write);
        assert!(perms.owner_exec);
        assert!(!perms.group_read);
        assert!(!perms.group_write);
        assert!(!perms.group_exec);
        assert!(!perms.other_read);
        assert!(!perms.other_write);
        assert!(!perms.other_exec);
        assert!(perms.is_readonly());
    }
}
