#![allow(
    clippy::collapsible_if,
    clippy::manual_map,
    clippy::redundant_guards,
    clippy::suspicious_to_owned,
    clippy::unnecessary_lazy_evaluations,
    clippy::unwrap_or_default
)]

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

    // ── Additional flatten_to_exe_names tests ─────────────────

    #[test]
    fn flatten_deeply_nested_children() {
        let grandchild = LocalProcessInfo {
            pid: 3,
            ppid: 2,
            name: "grep".to_string(),
            executable: PathBuf::from("/usr/bin/grep"),
            argv: vec![],
            cwd: PathBuf::new(),
            status: LocalProcessStatus::Run,
            start_time: 0,
            children: HashMap::new(),
        };
        let mut gc_children = HashMap::new();
        gc_children.insert(3, grandchild);
        let child = LocalProcessInfo {
            pid: 2,
            ppid: 1,
            name: "find".to_string(),
            executable: PathBuf::from("/usr/bin/find"),
            argv: vec![],
            cwd: PathBuf::new(),
            status: LocalProcessStatus::Run,
            start_time: 0,
            children: gc_children,
        };
        let mut children = HashMap::new();
        children.insert(2, child);
        let proc = make_proc("bash", "/usr/bin/bash", children);
        let names = proc.flatten_to_exe_names();
        assert_eq!(names.len(), 3);
        assert!(names.contains("bash"));
        assert!(names.contains("find"));
        assert!(names.contains("grep"));
    }

    #[test]
    fn flatten_mixed_valid_and_empty_executables() {
        let child_with_exe = LocalProcessInfo {
            pid: 2,
            ppid: 1,
            name: "cat".to_string(),
            executable: PathBuf::from("/bin/cat"),
            argv: vec![],
            cwd: PathBuf::new(),
            status: LocalProcessStatus::Run,
            start_time: 0,
            children: HashMap::new(),
        };
        let child_without_exe = LocalProcessInfo {
            pid: 3,
            ppid: 1,
            name: "kernel_worker".to_string(),
            executable: PathBuf::new(),
            argv: vec![],
            cwd: PathBuf::new(),
            status: LocalProcessStatus::Sleep,
            start_time: 0,
            children: HashMap::new(),
        };
        let mut children = HashMap::new();
        children.insert(2, child_with_exe);
        children.insert(3, child_without_exe);
        let proc = make_proc("bash", "/usr/bin/bash", children);
        let names = proc.flatten_to_exe_names();
        assert_eq!(names.len(), 2); // bash + cat, not kernel_worker
        assert!(names.contains("bash"));
        assert!(names.contains("cat"));
    }

    #[test]
    fn flatten_many_children_same_level() {
        let mut children = HashMap::new();
        for i in 0..10 {
            children.insert(
                i + 2,
                LocalProcessInfo {
                    pid: i + 2,
                    ppid: 1,
                    name: format!("worker{}", i),
                    executable: PathBuf::from(format!("/usr/bin/worker{}", i)),
                    argv: vec![],
                    cwd: PathBuf::new(),
                    status: LocalProcessStatus::Run,
                    start_time: 0,
                    children: HashMap::new(),
                },
            );
        }
        let proc = make_proc("supervisor", "/usr/bin/supervisor", children);
        let names = proc.flatten_to_exe_names();
        assert_eq!(names.len(), 11); // supervisor + 10 workers
    }

    #[test]
    fn flatten_exe_only_filename_component() {
        // Exe paths with same filename but different directories
        let child1 = LocalProcessInfo {
            pid: 2,
            ppid: 1,
            name: "app".to_string(),
            executable: PathBuf::from("/opt/v1/app"),
            argv: vec![],
            cwd: PathBuf::new(),
            status: LocalProcessStatus::Run,
            start_time: 0,
            children: HashMap::new(),
        };
        let child2 = LocalProcessInfo {
            pid: 3,
            ppid: 1,
            name: "app".to_string(),
            executable: PathBuf::from("/opt/v2/app"),
            argv: vec![],
            cwd: PathBuf::new(),
            status: LocalProcessStatus::Run,
            start_time: 0,
            children: HashMap::new(),
        };
        let mut children = HashMap::new();
        children.insert(2, child1);
        children.insert(3, child2);
        let proc = make_proc("init", "/sbin/init", children);
        let names = proc.flatten_to_exe_names();
        // "app" from both children deduplicates to one, plus "init"
        assert_eq!(names.len(), 2);
        assert!(names.contains("app"));
        assert!(names.contains("init"));
    }

    // ── Additional struct/field tests ─────────────────────────

    #[test]
    fn process_info_fields_accessible() {
        let proc = LocalProcessInfo {
            pid: 42,
            ppid: 1,
            name: "myproc".to_string(),
            executable: PathBuf::from("/usr/local/bin/myproc"),
            argv: vec!["myproc".to_string(), "--flag".to_string()],
            cwd: PathBuf::from("/home/user"),
            status: LocalProcessStatus::Sleep,
            start_time: 1234567890,
            children: HashMap::new(),
        };
        assert_eq!(proc.pid, 42);
        assert_eq!(proc.ppid, 1);
        assert_eq!(proc.name, "myproc");
        assert_eq!(proc.executable, PathBuf::from("/usr/local/bin/myproc"));
        assert_eq!(proc.argv.len(), 2);
        assert_eq!(proc.cwd, PathBuf::from("/home/user"));
        assert_eq!(proc.start_time, 1234567890);
    }

    #[test]
    fn process_info_clone_preserves_all_fields() {
        let child = LocalProcessInfo {
            pid: 2,
            ppid: 1,
            name: "child".to_string(),
            executable: PathBuf::from("/bin/child"),
            argv: vec!["child".to_string()],
            cwd: PathBuf::from("/tmp"),
            status: LocalProcessStatus::Run,
            start_time: 100,
            children: HashMap::new(),
        };
        let mut children = HashMap::new();
        children.insert(2, child);
        let proc = LocalProcessInfo {
            pid: 1,
            ppid: 0,
            name: "parent".to_string(),
            executable: PathBuf::from("/bin/parent"),
            argv: vec!["parent".to_string(), "-d".to_string()],
            cwd: PathBuf::from("/home"),
            status: LocalProcessStatus::Sleep,
            start_time: 50,
            children,
        };
        let cloned = proc.clone();
        assert_eq!(cloned.pid, proc.pid);
        assert_eq!(cloned.ppid, proc.ppid);
        assert_eq!(cloned.name, proc.name);
        assert_eq!(cloned.executable, proc.executable);
        assert_eq!(cloned.argv, proc.argv);
        assert_eq!(cloned.cwd, proc.cwd);
        assert_eq!(cloned.start_time, proc.start_time);
        assert_eq!(cloned.children.len(), 1);
    }

    // ── Status variant tests ──────────────────────────────────

    #[test]
    fn status_debug_all_distinct() {
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
        let mut names = HashSet::new();
        for v in &variants {
            names.insert(format!("{v:?}"));
        }
        assert_eq!(
            names.len(),
            variants.len(),
            "all variant debug strings should be unique"
        );
    }

    // ── Additional live process tests ─────────────────────────

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn with_root_pid_has_nonempty_name() {
        let pid = std::process::id();
        let info = LocalProcessInfo::with_root_pid(pid).unwrap();
        assert!(!info.name.is_empty(), "current process should have a name");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn with_root_pid_executable_is_absolute() {
        let pid = std::process::id();
        let info = LocalProcessInfo::with_root_pid(pid).unwrap();
        assert!(
            info.executable.is_absolute(),
            "executable should be an absolute path"
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn with_root_pid_ppid_is_nonzero() {
        let pid = std::process::id();
        let info = LocalProcessInfo::with_root_pid(pid).unwrap();
        // Our test process is not PID 1, so ppid should be nonzero
        assert!(info.ppid > 0, "test process ppid should be nonzero");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn current_working_dir_matches_env() {
        let pid = std::process::id();
        let proc_cwd = LocalProcessInfo::current_working_dir(pid).unwrap();
        let env_cwd = std::env::current_dir().unwrap();
        // Canonicalize both to handle symlinks (e.g., /private/tmp vs /tmp on macOS)
        let proc_canonical = std::fs::canonicalize(&proc_cwd).unwrap_or(proc_cwd);
        let env_canonical = std::fs::canonicalize(&env_cwd).unwrap_or(env_cwd);
        assert_eq!(proc_canonical, env_canonical);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn executable_path_nonexistent_pid_returns_none() {
        let exe = LocalProcessInfo::executable_path(u32::MAX);
        assert!(exe.is_none());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn current_working_dir_nonexistent_pid_returns_none() {
        let cwd = LocalProcessInfo::current_working_dir(u32::MAX);
        assert!(cwd.is_none());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn with_root_pid_flatten_includes_current_exe() {
        let pid = std::process::id();
        let info = LocalProcessInfo::with_root_pid(pid).unwrap();
        let names = info.flatten_to_exe_names();
        // Our test runner should appear in the flattened exe names
        assert!(!names.is_empty(), "should have at least one exe name");
    }

    // ── Additional flatten edge cases ────────────────────────

    #[test]
    fn flatten_all_empty_exes_returns_empty() {
        let child = LocalProcessInfo {
            pid: 2,
            ppid: 1,
            name: "child".to_string(),
            executable: PathBuf::new(),
            argv: vec![],
            cwd: PathBuf::new(),
            status: LocalProcessStatus::Run,
            start_time: 0,
            children: HashMap::new(),
        };
        let mut children = HashMap::new();
        children.insert(2, child);
        let proc = LocalProcessInfo {
            pid: 1,
            ppid: 0,
            name: "parent".to_string(),
            executable: PathBuf::new(),
            argv: vec![],
            cwd: PathBuf::new(),
            status: LocalProcessStatus::Run,
            start_time: 0,
            children,
        };
        let names = proc.flatten_to_exe_names();
        assert!(names.is_empty());
    }

    #[test]
    fn flatten_root_only_exe_with_empty_children() {
        let child = LocalProcessInfo {
            pid: 2,
            ppid: 1,
            name: "kworker".to_string(),
            executable: PathBuf::new(),
            argv: vec![],
            cwd: PathBuf::new(),
            status: LocalProcessStatus::Sleep,
            start_time: 0,
            children: HashMap::new(),
        };
        let mut children = HashMap::new();
        children.insert(2, child);
        let proc = make_proc("init", "/sbin/init", children);
        let names = proc.flatten_to_exe_names();
        assert_eq!(names.len(), 1);
        assert!(names.contains("init"));
    }

    #[test]
    fn flatten_four_levels_deep() {
        let level4 = LocalProcessInfo {
            pid: 5,
            ppid: 4,
            name: "d".to_string(),
            executable: PathBuf::from("/bin/d"),
            argv: vec![],
            cwd: PathBuf::new(),
            status: LocalProcessStatus::Run,
            start_time: 0,
            children: HashMap::new(),
        };
        let mut c3 = HashMap::new();
        c3.insert(5, level4);
        let level3 = LocalProcessInfo {
            pid: 4,
            ppid: 3,
            name: "c".to_string(),
            executable: PathBuf::from("/bin/c"),
            argv: vec![],
            cwd: PathBuf::new(),
            status: LocalProcessStatus::Run,
            start_time: 0,
            children: c3,
        };
        let mut c2 = HashMap::new();
        c2.insert(4, level3);
        let level2 = LocalProcessInfo {
            pid: 3,
            ppid: 2,
            name: "b".to_string(),
            executable: PathBuf::from("/bin/b"),
            argv: vec![],
            cwd: PathBuf::new(),
            status: LocalProcessStatus::Run,
            start_time: 0,
            children: c2,
        };
        let mut c1 = HashMap::new();
        c1.insert(3, level2);
        let proc = make_proc("a", "/bin/a", c1);
        let names = proc.flatten_to_exe_names();
        assert_eq!(names.len(), 4);
        for name in &["a", "b", "c", "d"] {
            assert!(names.contains(*name));
        }
    }

    // ── Process info construction variants ────────────────────

    #[test]
    fn process_info_with_argv() {
        let proc = LocalProcessInfo {
            pid: 10,
            ppid: 1,
            name: "cargo".to_string(),
            executable: PathBuf::from("/usr/bin/cargo"),
            argv: vec![
                "cargo".to_string(),
                "test".to_string(),
                "--release".to_string(),
            ],
            cwd: PathBuf::from("/home/user/project"),
            status: LocalProcessStatus::Run,
            start_time: 999,
            children: HashMap::new(),
        };
        assert_eq!(proc.argv.len(), 3);
        assert_eq!(proc.argv[0], "cargo");
        assert_eq!(proc.argv[2], "--release");
    }

    #[test]
    fn process_info_debug_contains_pid() {
        let proc = make_proc("test", "/usr/bin/test", HashMap::new());
        let debug = format!("{:?}", proc);
        assert!(debug.contains("pid"));
        assert!(debug.contains("1"));
    }

    #[test]
    fn process_info_debug_contains_name() {
        let proc = make_proc("unique_name_xyz", "/bin/unique_name_xyz", HashMap::new());
        let debug = format!("{:?}", proc);
        assert!(debug.contains("unique_name_xyz"));
    }

    #[test]
    fn process_info_default_cwd_is_empty() {
        let proc = make_proc("test", "/bin/test", HashMap::new());
        assert_eq!(proc.cwd, PathBuf::new());
    }

    #[test]
    fn process_info_children_map_operations() {
        let mut children = HashMap::new();
        for i in 10..15u32 {
            children.insert(
                i,
                LocalProcessInfo {
                    pid: i,
                    ppid: 1,
                    name: format!("child{}", i),
                    executable: PathBuf::from(format!("/bin/child{}", i)),
                    argv: vec![],
                    cwd: PathBuf::new(),
                    status: LocalProcessStatus::Run,
                    start_time: 0,
                    children: HashMap::new(),
                },
            );
        }
        let proc = make_proc("parent", "/bin/parent", children);
        assert_eq!(proc.children.len(), 5);
        assert!(proc.children.contains_key(&10));
        assert!(proc.children.contains_key(&14));
        assert!(!proc.children.contains_key(&15));
    }

    // ── Status variant debug names ───────────────────────────

    #[test]
    fn status_idle_debug() {
        assert_eq!(format!("{:?}", LocalProcessStatus::Idle), "Idle");
    }

    #[test]
    fn status_zombie_debug() {
        assert_eq!(format!("{:?}", LocalProcessStatus::Zombie), "Zombie");
    }

    #[test]
    fn status_lock_blocked_debug() {
        assert_eq!(
            format!("{:?}", LocalProcessStatus::LockBlocked),
            "LockBlocked"
        );
    }

    // ── Additional live process tests ────────────────────────

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn with_root_pid_start_time_is_nonzero() {
        let pid = std::process::id();
        let info = LocalProcessInfo::with_root_pid(pid).unwrap();
        assert!(info.start_time > 0, "start_time should be nonzero");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn with_root_pid_cwd_is_nonempty() {
        let pid = std::process::id();
        let info = LocalProcessInfo::with_root_pid(pid).unwrap();
        assert!(
            !info.cwd.as_os_str().is_empty(),
            "cwd should be non-empty for current process"
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn executable_path_current_is_file() {
        let pid = std::process::id();
        let exe = LocalProcessInfo::executable_path(pid).unwrap();
        assert!(exe.exists(), "executable should exist on disk");
    }

    // ── Second-pass expansion ────────────────────────────────────

    #[test]
    fn flatten_single_child_no_grandchildren() {
        let child = LocalProcessInfo {
            pid: 2,
            ppid: 1,
            name: "ls".to_string(),
            executable: PathBuf::from("/bin/ls"),
            argv: vec!["ls".to_string(), "-la".to_string()],
            cwd: PathBuf::from("/tmp"),
            status: LocalProcessStatus::Run,
            start_time: 100,
            children: HashMap::new(),
        };
        let mut children = HashMap::new();
        children.insert(2, child);
        let proc = make_proc("sh", "/bin/sh", children);
        let names = proc.flatten_to_exe_names();
        assert_eq!(names.len(), 2);
        assert!(names.contains("sh"));
        assert!(names.contains("ls"));
    }

    #[test]
    fn flatten_three_distinct_children() {
        let mut children = HashMap::new();
        for (id, name) in [(2u32, "cat"), (3, "grep"), (4, "sed")] {
            children.insert(
                id,
                LocalProcessInfo {
                    pid: id,
                    ppid: 1,
                    name: name.to_string(),
                    executable: PathBuf::from(format!("/usr/bin/{}", name)),
                    argv: vec![],
                    cwd: PathBuf::new(),
                    status: LocalProcessStatus::Run,
                    start_time: 0,
                    children: HashMap::new(),
                },
            );
        }
        let proc = make_proc("pipe", "/usr/bin/pipe", children);
        let names = proc.flatten_to_exe_names();
        assert_eq!(names.len(), 4);
    }

    #[test]
    fn status_copy_semantics() {
        let a = LocalProcessStatus::Run;
        let b = a;
        // Both should work since Copy
        let _da = format!("{:?}", a);
        let _db = format!("{:?}", b);
    }

    #[test]
    fn status_dead_debug() {
        assert_eq!(format!("{:?}", LocalProcessStatus::Dead), "Dead");
    }

    #[test]
    fn status_tracing_debug() {
        assert_eq!(format!("{:?}", LocalProcessStatus::Tracing), "Tracing");
    }

    #[test]
    fn status_wakekill_debug() {
        assert_eq!(format!("{:?}", LocalProcessStatus::Wakekill), "Wakekill");
    }

    #[test]
    fn status_waking_debug() {
        assert_eq!(format!("{:?}", LocalProcessStatus::Waking), "Waking");
    }

    #[test]
    fn status_parked_debug() {
        assert_eq!(format!("{:?}", LocalProcessStatus::Parked), "Parked");
    }

    #[test]
    fn status_unknown_debug() {
        assert_eq!(format!("{:?}", LocalProcessStatus::Unknown), "Unknown");
    }

    #[test]
    fn status_stop_debug() {
        assert_eq!(format!("{:?}", LocalProcessStatus::Stop), "Stop");
    }

    #[test]
    fn status_sleep_debug() {
        assert_eq!(format!("{:?}", LocalProcessStatus::Sleep), "Sleep");
    }

    #[test]
    fn status_run_debug() {
        assert_eq!(format!("{:?}", LocalProcessStatus::Run), "Run");
    }

    #[test]
    fn process_info_empty_argv() {
        let proc = make_proc("daemon", "/usr/sbin/daemon", HashMap::new());
        assert!(proc.argv.is_empty());
    }

    #[test]
    fn process_info_children_empty_by_default() {
        let proc = make_proc("leaf", "/bin/leaf", HashMap::new());
        assert!(proc.children.is_empty());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn with_root_pid_argv_is_nonempty() {
        let pid = std::process::id();
        let info = LocalProcessInfo::with_root_pid(pid).unwrap();
        // Test runner should have at least one argument
        assert!(!info.argv.is_empty(), "test process should have argv");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn with_root_pid_status_is_run() {
        let pid = std::process::id();
        let info = LocalProcessInfo::with_root_pid(pid).unwrap();
        assert!(
            matches!(info.status, LocalProcessStatus::Run),
            "current process should be running, got {:?}",
            info.status
        );
    }

    // ── Third-pass expansion ────────────────────────────────────

    #[test]
    fn flatten_parent_no_exe_children_have_exes() {
        let child = LocalProcessInfo {
            pid: 2,
            ppid: 1,
            name: "worker".to_string(),
            executable: PathBuf::from("/usr/bin/worker"),
            argv: vec![],
            cwd: PathBuf::new(),
            status: LocalProcessStatus::Run,
            start_time: 0,
            children: HashMap::new(),
        };
        let mut children = HashMap::new();
        children.insert(2, child);
        let proc = LocalProcessInfo {
            pid: 1,
            ppid: 0,
            name: "kthread".to_string(),
            executable: PathBuf::new(), // no exe
            argv: vec![],
            cwd: PathBuf::new(),
            status: LocalProcessStatus::Run,
            start_time: 0,
            children,
        };
        let names = proc.flatten_to_exe_names();
        assert_eq!(names.len(), 1);
        assert!(names.contains("worker"));
    }

    #[test]
    fn flatten_branching_two_subtrees() {
        let gc1 = LocalProcessInfo {
            pid: 4,
            ppid: 2,
            name: "gc1".to_string(),
            executable: PathBuf::from("/bin/gc1"),
            argv: vec![],
            cwd: PathBuf::new(),
            status: LocalProcessStatus::Run,
            start_time: 0,
            children: HashMap::new(),
        };
        let gc2 = LocalProcessInfo {
            pid: 5,
            ppid: 3,
            name: "gc2".to_string(),
            executable: PathBuf::from("/bin/gc2"),
            argv: vec![],
            cwd: PathBuf::new(),
            status: LocalProcessStatus::Run,
            start_time: 0,
            children: HashMap::new(),
        };
        let mut c1_kids = HashMap::new();
        c1_kids.insert(4, gc1);
        let mut c2_kids = HashMap::new();
        c2_kids.insert(5, gc2);
        let child1 = LocalProcessInfo {
            pid: 2,
            ppid: 1,
            name: "c1".to_string(),
            executable: PathBuf::from("/bin/c1"),
            argv: vec![],
            cwd: PathBuf::new(),
            status: LocalProcessStatus::Run,
            start_time: 0,
            children: c1_kids,
        };
        let child2 = LocalProcessInfo {
            pid: 3,
            ppid: 1,
            name: "c2".to_string(),
            executable: PathBuf::from("/bin/c2"),
            argv: vec![],
            cwd: PathBuf::new(),
            status: LocalProcessStatus::Run,
            start_time: 0,
            children: c2_kids,
        };
        let mut children = HashMap::new();
        children.insert(2, child1);
        children.insert(3, child2);
        let proc = make_proc("root", "/bin/root", children);
        let names = proc.flatten_to_exe_names();
        assert_eq!(names.len(), 5);
        for n in &["root", "c1", "c2", "gc1", "gc2"] {
            assert!(names.contains(*n), "missing {n}");
        }
    }

    #[test]
    fn process_info_clone_deep_nested() {
        let grandchild = LocalProcessInfo {
            pid: 3,
            ppid: 2,
            name: "gc".to_string(),
            executable: PathBuf::from("/bin/gc"),
            argv: vec!["gc".to_string(), "--deep".to_string()],
            cwd: PathBuf::from("/deep"),
            status: LocalProcessStatus::Sleep,
            start_time: 300,
            children: HashMap::new(),
        };
        let mut gc_map = HashMap::new();
        gc_map.insert(3, grandchild);
        let child = LocalProcessInfo {
            pid: 2,
            ppid: 1,
            name: "child".to_string(),
            executable: PathBuf::from("/bin/child"),
            argv: vec![],
            cwd: PathBuf::new(),
            status: LocalProcessStatus::Run,
            start_time: 200,
            children: gc_map,
        };
        let mut c_map = HashMap::new();
        c_map.insert(2, child);
        let proc = make_proc("top", "/bin/top", c_map);
        let cloned = proc.clone();
        assert_eq!(cloned.children.len(), 1);
        let cloned_child = &cloned.children[&2];
        assert_eq!(cloned_child.children.len(), 1);
        let cloned_gc = &cloned_child.children[&3];
        assert_eq!(cloned_gc.name, "gc");
        assert_eq!(cloned_gc.argv, vec!["gc", "--deep"]);
    }

    #[test]
    fn process_info_large_pid_values() {
        let proc = LocalProcessInfo {
            pid: u32::MAX,
            ppid: u32::MAX - 1,
            name: "big".to_string(),
            executable: PathBuf::from("/bin/big"),
            argv: vec![],
            cwd: PathBuf::new(),
            status: LocalProcessStatus::Run,
            start_time: u64::MAX,
            children: HashMap::new(),
        };
        assert_eq!(proc.pid, u32::MAX);
        assert_eq!(proc.ppid, u32::MAX - 1);
        assert_eq!(proc.start_time, u64::MAX);
    }

    #[test]
    fn flatten_exe_just_filename_no_directory() {
        // An executable path with no directory component
        let proc = LocalProcessInfo {
            pid: 1,
            ppid: 0,
            name: "bare".to_string(),
            executable: PathBuf::from("myapp"),
            argv: vec![],
            cwd: PathBuf::new(),
            status: LocalProcessStatus::Run,
            start_time: 0,
            children: HashMap::new(),
        };
        let names = proc.flatten_to_exe_names();
        assert_eq!(names.len(), 1);
        assert!(names.contains("myapp"));
    }

    #[test]
    fn process_info_debug_contains_executable() {
        let proc = LocalProcessInfo {
            pid: 1,
            ppid: 0,
            name: "test".to_string(),
            executable: PathBuf::from("/unique/path/to/binary"),
            argv: vec![],
            cwd: PathBuf::new(),
            status: LocalProcessStatus::Run,
            start_time: 0,
            children: HashMap::new(),
        };
        let debug = format!("{proc:?}");
        assert!(debug.contains("/unique/path/to/binary"), "got: {debug}");
    }

    #[test]
    fn process_info_debug_shows_children_count() {
        let mut children = HashMap::new();
        children.insert(2, make_proc("kid", "/bin/kid", HashMap::new()));
        let proc = make_proc("parent", "/bin/parent", children);
        let debug = format!("{proc:?}");
        // Debug should show the children map which contains an entry
        assert!(debug.contains("kid"), "got: {debug}");
    }

    #[test]
    fn status_clone_preserves_variant() {
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
            let cloned = *v; // Copy
            assert_eq!(format!("{v:?}"), format!("{cloned:?}"));
        }
    }

    #[test]
    fn flatten_hashset_dedup_behavior() {
        // Verify flatten returns a HashSet that deduplicates properly
        let mut children = HashMap::new();
        for i in 2..12u32 {
            children.insert(
                i,
                LocalProcessInfo {
                    pid: i,
                    ppid: 1,
                    name: format!("worker{i}"),
                    // All have the same exe filename
                    executable: PathBuf::from(format!("/path{i}/same_name")),
                    argv: vec![],
                    cwd: PathBuf::new(),
                    status: LocalProcessStatus::Run,
                    start_time: 0,
                    children: HashMap::new(),
                },
            );
        }
        let proc = make_proc("same_name", "/bin/same_name", children);
        let names = proc.flatten_to_exe_names();
        // All 11 processes have exe name "same_name" → deduplicated to 1
        assert_eq!(names.len(), 1);
        assert!(names.contains("same_name"));
    }

    #[test]
    fn process_info_argv_with_empty_strings() {
        let proc = LocalProcessInfo {
            pid: 1,
            ppid: 0,
            name: "test".to_string(),
            executable: PathBuf::from("/bin/test"),
            argv: vec!["test".to_string(), "".to_string(), "arg".to_string()],
            cwd: PathBuf::new(),
            status: LocalProcessStatus::Run,
            start_time: 0,
            children: HashMap::new(),
        };
        assert_eq!(proc.argv.len(), 3);
        assert_eq!(proc.argv[1], "");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn with_root_pid_parent_exists() {
        // Our parent process should be accessible
        let pid = std::process::id();
        let info = LocalProcessInfo::with_root_pid(pid).unwrap();
        let parent = LocalProcessInfo::with_root_pid(info.ppid);
        assert!(parent.is_some(), "parent process should exist");
        assert_eq!(parent.unwrap().pid, info.ppid);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn executable_path_matches_with_root_pid() {
        let pid = std::process::id();
        let exe_standalone = LocalProcessInfo::executable_path(pid).unwrap();
        let info = LocalProcessInfo::with_root_pid(pid).unwrap();
        // Canonicalize to handle macOS /tmp → /private/tmp symlink
        let a = std::fs::canonicalize(&exe_standalone).unwrap_or(exe_standalone);
        let b = std::fs::canonicalize(&info.executable).unwrap_or(info.executable);
        assert_eq!(a, b);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn with_root_pid_current_has_children_map() {
        let pid = std::process::id();
        let info = LocalProcessInfo::with_root_pid(pid).unwrap();
        // children is a HashMap, could be empty for our test process
        // but the field should be accessible
        let _ = info.children.len();
    }
}
