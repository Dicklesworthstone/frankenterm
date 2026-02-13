use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

#[cfg(feature = "lua")]
use frankenterm_dynamic::{FromDynamic, ToDynamic};

mod linux;
mod macos;
mod windows;

#[derive(Debug, Copy, Clone)]
#[cfg_attr(feature = "lua", derive(FromDynamic, ToDynamic))]
pub enum LocalProcessStatus {
    Idle,
    Run,
    Sleep,
    Stop,
    Zombie,
    Tracing,
    Dead,
    Wakekill,
    Waking,
    Parked,
    LockBlocked,
    Unknown,
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "lua", derive(FromDynamic, ToDynamic))]
pub struct LocalProcessInfo {
    /// The process identifier
    pub pid: u32,
    /// The parent process identifier
    pub ppid: u32,
    /// The COMM name of the process. May not bear any relation to
    /// the executable image name. May be changed at runtime by
    /// the process.
    /// Many systems truncate this
    /// field to 15-16 characters.
    pub name: String,
    /// Path to the executable image
    pub executable: PathBuf,
    /// The argument vector.
    /// Some systems allow changing the argv block at runtime
    /// eg: setproctitle().
    pub argv: Vec<String>,
    /// The current working directory for the process, or an empty
    /// path if it was not accessible for some reason.
    pub cwd: PathBuf,
    /// The status of the process. Not all possible values are
    /// portably supported on all systems.
    pub status: LocalProcessStatus,
    /// A clock value in unspecified system dependent units that
    /// indicates the relative age of the process.
    pub start_time: u64,
    /// The console handle associated with the process, if any.
    #[cfg(windows)]
    pub console: u64,
    /// Child processes, keyed by pid
    pub children: HashMap<u32, LocalProcessInfo>,
}
#[cfg(feature = "lua")]
luahelper::impl_lua_conversion_dynamic!(LocalProcessInfo);

impl LocalProcessInfo {
    /// Walk this sub-tree of processes and return a unique set
    /// of executable base names. eg: `foo/bar` and `woot/bar`
    /// produce a set containing just `bar`.
    pub fn flatten_to_exe_names(&self) -> HashSet<String> {
        let mut names = HashSet::new();

        fn flatten(item: &LocalProcessInfo, names: &mut HashSet<String>) {
            if let Some(exe) = item.executable.file_name() {
                names.insert(exe.to_string_lossy().into_owned());
            }
            for proc in item.children.values() {
                flatten(proc, names);
            }
        }

        flatten(self, &mut names);
        names
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    pub fn with_root_pid(_pid: u32) -> Option<Self> {
        None
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    pub fn current_working_dir(_pid: u32) -> Option<PathBuf> {
        None
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    pub fn executable_path(_pid: u32) -> Option<PathBuf> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── LocalProcessStatus ────────────────────────────────────

    #[test]
    fn status_debug() {
        let s = LocalProcessStatus::Run;
        let debug = format!("{s:?}");
        assert_eq!(debug, "Run");
    }

    #[test]
    fn status_clone_copy() {
        let a = LocalProcessStatus::Sleep;
        let b = a;
        let c = a.clone();
        assert!(format!("{b:?}") == format!("{c:?}"));
    }

    #[test]
    fn all_status_variants_are_debug() {
        let variants = [
            LocalProcessStatus::Idle,
            LocalProcessStatus::Run,
            LocalProcessStatus::Sleep,
            LocalProcessStatus::Stop,
            LocalProcessStatus::Zombie,
            LocalProcessStatus::Tracing,
            LocalProcessStatus::Dead,
            LocalProcessStatus::Wakekill,
            LocalProcessStatus::Waking,
            LocalProcessStatus::Parked,
            LocalProcessStatus::LockBlocked,
            LocalProcessStatus::Unknown,
        ];
        for v in &variants {
            let debug = format!("{v:?}");
            assert!(!debug.is_empty());
        }
    }

    // ── LocalProcessInfo construction ─────────────────────────

    fn make_proc(
        name: &str,
        exe: &str,
        children: HashMap<u32, LocalProcessInfo>,
    ) -> LocalProcessInfo {
        LocalProcessInfo {
            pid: 1,
            ppid: 0,
            name: name.to_string(),
            executable: PathBuf::from(exe),
            argv: vec![],
            cwd: PathBuf::new(),
            status: LocalProcessStatus::Run,
            start_time: 0,
            children,
        }
    }

    #[test]
    fn process_info_is_debug() {
        let proc = make_proc("test", "/usr/bin/test", HashMap::new());
        let debug = format!("{proc:?}");
        assert!(debug.contains("test"));
    }

    #[test]
    fn process_info_clone() {
        let proc = make_proc("test", "/usr/bin/test", HashMap::new());
        let cloned = proc.clone();
        assert_eq!(cloned.pid, proc.pid);
        assert_eq!(cloned.name, proc.name);
    }

    // ── flatten_to_exe_names ──────────────────────────────────

    #[test]
    fn flatten_single_process_no_children() {
        let proc = make_proc("bash", "/usr/bin/bash", HashMap::new());
        let names = proc.flatten_to_exe_names();
        assert!(names.contains("bash"));
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn flatten_with_children() {
        let child = LocalProcessInfo {
            pid: 2,
            ppid: 1,
            name: "vim".to_string(),
            executable: PathBuf::from("/usr/bin/vim"),
            argv: vec![],
            cwd: PathBuf::new(),
            status: LocalProcessStatus::Run,
            start_time: 0,
            children: HashMap::new(),
        };
        let mut children = HashMap::new();
        children.insert(2, child);
        let proc = make_proc("bash", "/usr/bin/bash", children);
        let names = proc.flatten_to_exe_names();
        assert!(names.contains("bash"));
        assert!(names.contains("vim"));
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn flatten_deduplicates_same_exe_name() {
        let child = LocalProcessInfo {
            pid: 2,
            ppid: 1,
            name: "bash2".to_string(),
            executable: PathBuf::from("/other/path/bash"),
            argv: vec![],
            cwd: PathBuf::new(),
            status: LocalProcessStatus::Run,
            start_time: 0,
            children: HashMap::new(),
        };
        let mut children = HashMap::new();
        children.insert(2, child);
        let proc = make_proc("bash", "/usr/bin/bash", children);
        let names = proc.flatten_to_exe_names();
        // Both have "bash" as the file_name, so only one entry
        assert!(names.contains("bash"));
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn flatten_empty_executable_skipped() {
        let proc = LocalProcessInfo {
            pid: 1,
            ppid: 0,
            name: "kernel".to_string(),
            executable: PathBuf::new(),
            argv: vec![],
            cwd: PathBuf::new(),
            status: LocalProcessStatus::Idle,
            start_time: 0,
            children: HashMap::new(),
        };
        let names = proc.flatten_to_exe_names();
        // PathBuf::new() has no file_name component
        assert!(names.is_empty());
    }

    // ── Live process queries ──────────────────────────────────

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn with_root_pid_current_process() {
        let pid = std::process::id();
        let info = LocalProcessInfo::with_root_pid(pid);
        assert!(info.is_some());
        let info = info.unwrap();
        assert_eq!(info.pid, pid);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn current_working_dir_current_process() {
        let pid = std::process::id();
        let cwd = LocalProcessInfo::current_working_dir(pid);
        assert!(cwd.is_some());
        let cwd = cwd.unwrap();
        assert!(cwd.is_absolute());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn executable_path_current_process() {
        let pid = std::process::id();
        let exe = LocalProcessInfo::executable_path(pid);
        assert!(exe.is_some());
        let exe = exe.unwrap();
        assert!(exe.is_absolute());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn with_root_pid_nonexistent_returns_none() {
        // PID 0 is the kernel/swapper, unlikely to be returned as a normal process
        // Use a very high PID that's unlikely to exist
        let info = LocalProcessInfo::with_root_pid(u32::MAX);
        assert!(info.is_none());
    }
}
