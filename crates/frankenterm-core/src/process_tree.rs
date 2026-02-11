//! Per-pane process tree capture for session persistence.
//!
//! Captures the full process tree rooted at a pane's shell process,
//! revealing not just the foreground process but all subprocesses
//! (agent CLIs, compilers, language servers, etc.).
//!
//! - **Linux**: reads `/proc/<pid>/{stat,cmdline,status}` via `std::fs`
//! - **macOS**: uses `ps` command output (safe, no FFI)
//! - **Other**: returns empty trees
//!
//! The tree is used for:
//! - Activity inference (is the agent compiling? idle? running tests?)
//! - Richer restore decisions (re-launch agents with correct subprocesses)
//! - Resource accounting (RSS per pane subtree)

use serde::{Deserialize, Serialize};

// =============================================================================
// Configuration
// =============================================================================

/// Process tree capture configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProcessTreeConfig {
    /// Enable process tree capture.
    pub enabled: bool,
    /// How often to capture trees (seconds). Separate from pane polling.
    pub capture_interval_secs: u64,
    /// Maximum tree depth to walk.
    pub max_depth: u32,
    /// Include thread info (more expensive).
    pub include_threads: bool,
}

impl Default for ProcessTreeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            capture_interval_secs: 30,
            max_depth: 5,
            include_threads: false,
        }
    }
}

// =============================================================================
// Core types
// =============================================================================

/// A complete process tree rooted at a pane's shell process.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessTree {
    /// Root process node (usually the shell).
    pub root: ProcessNode,
    /// Total process count in the tree.
    pub total_processes: usize,
    /// Total RSS in KB across all processes.
    pub total_rss_kb: u64,
}

/// A single process node in the tree.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessNode {
    /// Process ID.
    pub pid: u32,
    /// Parent process ID.
    pub ppid: u32,
    /// Process name (executable basename).
    pub name: String,
    /// Command-line arguments (truncated to first 8).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub argv: Vec<String>,
    /// Process state.
    pub state: ProcessState,
    /// Resident set size in KB.
    pub rss_kb: u64,
    /// Child processes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<ProcessNode>,
}

/// Process execution state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessState {
    Running,
    Sleeping,
    DiskSleep,
    Stopped,
    Zombie,
    Unknown,
}

impl std::fmt::Display for ProcessState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Running => write!(f, "running"),
            Self::Sleeping => write!(f, "sleeping"),
            Self::DiskSleep => write!(f, "disk_sleep"),
            Self::Stopped => write!(f, "stopped"),
            Self::Zombie => write!(f, "zombie"),
            Self::Unknown => write!(f, "unknown"),
        }
    }
}

// =============================================================================
// Activity inference
// =============================================================================

/// Inferred pane activity based on process tree contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneActivity {
    /// No child processes — likely waiting for input.
    Idle,
    /// Compiling (cargo, rustc, gcc, make, etc.)
    Compiling,
    /// Running tests (cargo test, pytest, jest, etc.)
    Testing,
    /// Version control operation (git, hg)
    VersionControl,
    /// Running an AI agent CLI
    AgentRunning,
    /// Running a language server or editor
    Editing,
    /// General subprocess activity
    Active,
}

impl std::fmt::Display for PaneActivity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Idle => write!(f, "idle"),
            Self::Compiling => write!(f, "compiling"),
            Self::Testing => write!(f, "testing"),
            Self::VersionControl => write!(f, "vcs"),
            Self::AgentRunning => write!(f, "agent"),
            Self::Editing => write!(f, "editing"),
            Self::Active => write!(f, "active"),
        }
    }
}

/// Infer pane activity from a process tree.
pub fn infer_activity(tree: &ProcessTree) -> PaneActivity {
    if tree.root.children.is_empty() {
        return PaneActivity::Idle;
    }

    // Collect all executable names in the tree (flattened).
    let mut names = Vec::new();
    collect_names(&tree.root, &mut names);

    // Check patterns in priority order.
    for name in &names {
        let lower = name.to_lowercase();

        // Agent CLIs
        if matches!(
            lower.as_str(),
            "claude" | "claude-code" | "codex" | "gemini" | "aider" | "copilot"
        ) {
            return PaneActivity::AgentRunning;
        }

        // Compilation
        if matches!(
            lower.as_str(),
            "cargo"
                | "rustc"
                | "gcc"
                | "g++"
                | "clang"
                | "clang++"
                | "cc"
                | "make"
                | "cmake"
                | "ninja"
                | "meson"
                | "javac"
                | "tsc"
                | "swc"
                | "esbuild"
                | "webpack"
                | "vite"
        ) {
            return PaneActivity::Compiling;
        }

        // Testing
        if lower.contains("test")
            || lower.contains("pytest")
            || lower == "jest"
            || lower == "mocha"
            || lower == "vitest"
            || lower.starts_with("cargo-nextest")
        {
            return PaneActivity::Testing;
        }

        // Version control
        if matches!(lower.as_str(), "git" | "hg" | "svn" | "gh") {
            return PaneActivity::VersionControl;
        }

        // Editing
        if matches!(
            lower.as_str(),
            "vim"
                | "nvim"
                | "emacs"
                | "nano"
                | "code"
                | "helix"
                | "hx"
                | "rust-analyzer"
                | "gopls"
                | "pyright"
                | "typescript-language-server"
        ) {
            return PaneActivity::Editing;
        }
    }

    PaneActivity::Active
}

fn collect_names(node: &ProcessNode, names: &mut Vec<String>) {
    names.push(node.name.clone());
    for child in &node.children {
        collect_names(child, names);
    }
}

// =============================================================================
// Capture implementation
// =============================================================================

/// Capture the process tree rooted at the given PID.
///
/// Returns `None` if the process doesn't exist or tree capture fails.
pub fn capture_tree(root_pid: u32, config: &ProcessTreeConfig) -> Option<ProcessTree> {
    let root = capture_node(root_pid, 0, config.max_depth)?;
    let mut total_processes = 0;
    let mut total_rss_kb = 0;
    count_tree(&root, &mut total_processes, &mut total_rss_kb);

    Some(ProcessTree {
        root,
        total_processes,
        total_rss_kb,
    })
}

fn count_tree(node: &ProcessNode, count: &mut usize, rss: &mut u64) {
    *count += 1;
    *rss += node.rss_kb;
    for child in &node.children {
        count_tree(child, count, rss);
    }
}

fn capture_node(pid: u32, depth: u32, max_depth: u32) -> Option<ProcessNode> {
    let info = read_process_info(pid)?;

    let children = if depth < max_depth {
        find_children(pid)
            .into_iter()
            .filter_map(|child_pid| capture_node(child_pid, depth + 1, max_depth))
            .collect()
    } else {
        Vec::new()
    };

    Some(ProcessNode {
        pid: info.pid,
        ppid: info.ppid,
        name: info.name,
        argv: info.argv,
        state: info.state,
        rss_kb: info.rss_kb,
        children,
    })
}

/// Raw process info from OS, before tree assembly.
struct RawProcessInfo {
    pid: u32,
    ppid: u32,
    name: String,
    argv: Vec<String>,
    state: ProcessState,
    rss_kb: u64,
}

// =============================================================================
// Linux: /proc filesystem
// =============================================================================

#[cfg(target_os = "linux")]
fn read_process_info(pid: u32) -> Option<RawProcessInfo> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let cmdline = std::fs::read_to_string(format!("/proc/{pid}/cmdline")).unwrap_or_default();

    // Parse /proc/<pid>/stat: "pid (name) state ppid ..."
    // Name can contain spaces and parens, so find the last ')'.
    let name_start = stat.find('(')?;
    let name_end = stat.rfind(')')?;
    let name = stat[name_start + 1..name_end].to_string();

    let rest = &stat[name_end + 2..]; // skip ") "
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // fields[0] = state, fields[1] = ppid
    let state = fields
        .first()
        .map(|s| parse_linux_state(s))
        .unwrap_or(ProcessState::Unknown);
    let ppid = fields
        .get(1)
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);

    // Parse RSS from /proc/<pid>/status (VmRSS line, in kB).
    let rss_kb = status
        .lines()
        .find(|l| l.starts_with("VmRSS:"))
        .and_then(|l| {
            l.split_whitespace()
                .nth(1)
                .and_then(|v| v.parse::<u64>().ok())
        })
        .unwrap_or(0);

    // Parse cmdline (null-separated).
    let argv: Vec<String> = cmdline
        .split('\0')
        .filter(|s| !s.is_empty())
        .take(8)
        .map(String::from)
        .collect();

    Some(RawProcessInfo {
        pid,
        ppid,
        name,
        argv,
        state,
        rss_kb,
    })
}

#[cfg(target_os = "linux")]
fn parse_linux_state(s: &str) -> ProcessState {
    match s {
        "R" => ProcessState::Running,
        "S" => ProcessState::Sleeping,
        "D" => ProcessState::DiskSleep,
        "T" | "t" => ProcessState::Stopped,
        "Z" => ProcessState::Zombie,
        _ => ProcessState::Unknown,
    }
}

#[cfg(target_os = "linux")]
fn find_children(pid: u32) -> Vec<u32> {
    // Read /proc/<pid>/task/<pid>/children if available (kernel 3.5+).
    let children_path = format!("/proc/{pid}/task/{pid}/children");
    if let Ok(contents) = std::fs::read_to_string(&children_path) {
        return contents
            .split_whitespace()
            .filter_map(|s| s.parse::<u32>().ok())
            .collect();
    }

    // Fallback: scan /proc for processes with this ppid.
    scan_children_from_proc(pid)
}

#[cfg(target_os = "linux")]
fn scan_children_from_proc(ppid: u32) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };

    let mut children = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid_str) = name.to_str() else {
            continue;
        };
        let Ok(child_pid) = pid_str.parse::<u32>() else {
            continue;
        };

        let stat_path = format!("/proc/{child_pid}/stat");
        if let Ok(stat) = std::fs::read_to_string(&stat_path) {
            if let Some(end) = stat.rfind(')') {
                let rest = &stat[end + 2..];
                let fields: Vec<&str> = rest.split_whitespace().collect();
                if let Some(parent) = fields.get(1).and_then(|s| s.parse::<u32>().ok()) {
                    if parent == ppid {
                        children.push(child_pid);
                    }
                }
            }
        }
    }
    children
}

// =============================================================================
// macOS: ps command (safe, no FFI)
// =============================================================================

#[cfg(target_os = "macos")]
fn read_process_info(pid: u32) -> Option<RawProcessInfo> {
    // Use `ps` to get process info in a single call.
    // Format: pid ppid state rss comm (args...)
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "pid=,ppid=,state=,rss=,comm="])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let line = String::from_utf8(output.stdout).ok()?;
    let line = line.trim();
    if line.is_empty() {
        return None;
    }

    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 5 {
        return None;
    }

    let pid_val = fields[0].parse::<u32>().ok()?;
    let ppid = fields[1].parse::<u32>().ok()?;
    let state = parse_macos_state(fields[2]);
    let rss_kb = fields[3].parse::<u64>().unwrap_or(0);
    // comm= gives the full path; extract basename.
    let comm = fields[4];
    let name = comm.rsplit('/').next().unwrap_or(comm).to_string();

    // Get argv separately (ps -o args= gives the full command line).
    let argv = read_macos_argv(pid_val);

    Some(RawProcessInfo {
        pid: pid_val,
        ppid,
        name,
        argv,
        state,
        rss_kb,
    })
}

#[cfg(target_os = "macos")]
fn parse_macos_state(s: &str) -> ProcessState {
    // macOS ps state codes: R=running, S=sleeping, U=uninterruptible,
    // T=stopped, Z=zombie. First char is primary state.
    match s.chars().next() {
        Some('R') => ProcessState::Running,
        Some('S') => ProcessState::Sleeping,
        Some('U') => ProcessState::DiskSleep,
        Some('T') => ProcessState::Stopped,
        Some('Z') => ProcessState::Zombie,
        _ => ProcessState::Unknown,
    }
}

#[cfg(target_os = "macos")]
fn read_macos_argv(pid: u32) -> Vec<String> {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "args="])
        .output()
        .ok();

    output
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| {
            s.trim()
                .split_whitespace()
                .take(8)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(target_os = "macos")]
fn find_children(pid: u32) -> Vec<u32> {
    // Use pgrep to find child processes.
    let output = std::process::Command::new("pgrep")
        .args(["-P", &pid.to_string()])
        .output()
        .ok();

    output
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout).ok()
            } else {
                None
            }
        })
        .map(|s| {
            s.lines()
                .filter_map(|line| line.trim().parse::<u32>().ok())
                .collect()
        })
        .unwrap_or_default()
}

// =============================================================================
// Other platforms: stub
// =============================================================================

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn read_process_info(_pid: u32) -> Option<RawProcessInfo> {
    None
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn find_children(_pid: u32) -> Vec<u32> {
    Vec::new()
}

// =============================================================================
// ProcessTree aggregate methods
// =============================================================================

impl ProcessTree {
    /// Collect all executable names in the tree (flattened, deduplicated).
    pub fn exe_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        collect_names(&self.root, &mut names);
        names.sort();
        names.dedup();
        names
    }

    /// Check if a process with the given name exists anywhere in the tree.
    pub fn contains_process(&self, name: &str) -> bool {
        contains_process_recursive(&self.root, name)
    }
}

fn contains_process_recursive(node: &ProcessNode, name: &str) -> bool {
    if node.name == name {
        return true;
    }
    node.children
        .iter()
        .any(|child| contains_process_recursive(child, name))
}

impl ProcessNode {
    /// Total RSS of this node and all descendants.
    pub fn subtree_rss_kb(&self) -> u64 {
        let mut total = self.rss_kb;
        for child in &self.children {
            total += child.subtree_rss_kb();
        }
        total
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tree() -> ProcessTree {
        ProcessTree {
            root: ProcessNode {
                pid: 100,
                ppid: 1,
                name: "bash".into(),
                argv: vec!["bash".into(), "--login".into()],
                state: ProcessState::Sleeping,
                rss_kb: 5000,
                children: vec![ProcessNode {
                    pid: 101,
                    ppid: 100,
                    name: "claude".into(),
                    argv: vec!["claude".into()],
                    state: ProcessState::Running,
                    rss_kb: 50000,
                    children: vec![
                        ProcessNode {
                            pid: 102,
                            ppid: 101,
                            name: "cargo".into(),
                            argv: vec!["cargo".into(), "check".into()],
                            state: ProcessState::Running,
                            rss_kb: 200000,
                            children: vec![ProcessNode {
                                pid: 103,
                                ppid: 102,
                                name: "rustc".into(),
                                argv: vec!["rustc".into()],
                                state: ProcessState::Running,
                                rss_kb: 150000,
                                children: vec![],
                            }],
                        },
                        ProcessNode {
                            pid: 104,
                            ppid: 101,
                            name: "git".into(),
                            argv: vec!["git".into(), "status".into()],
                            state: ProcessState::Sleeping,
                            rss_kb: 3000,
                            children: vec![],
                        },
                    ],
                }],
            },
            total_processes: 5,
            total_rss_kb: 408000,
        }
    }

    #[test]
    fn config_defaults() {
        let cfg = ProcessTreeConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.capture_interval_secs, 30);
        assert_eq!(cfg.max_depth, 5);
        assert!(!cfg.include_threads);
    }

    #[test]
    fn config_serde_roundtrip() {
        let cfg = ProcessTreeConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: ProcessTreeConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.max_depth, cfg.max_depth);
    }

    #[test]
    fn tree_total_processes() {
        let tree = sample_tree();
        assert_eq!(tree.total_processes, 5);
    }

    #[test]
    fn tree_total_rss() {
        let tree = sample_tree();
        assert_eq!(tree.total_rss_kb, 408000);
    }

    #[test]
    fn tree_exe_names() {
        let tree = sample_tree();
        let names = tree.exe_names();
        assert!(names.contains(&"bash".to_string()));
        assert!(names.contains(&"claude".to_string()));
        assert!(names.contains(&"cargo".to_string()));
        assert!(names.contains(&"rustc".to_string()));
        assert!(names.contains(&"git".to_string()));
        assert_eq!(names.len(), 5);
    }

    #[test]
    fn tree_contains_process() {
        let tree = sample_tree();
        assert!(tree.contains_process("cargo"));
        assert!(tree.contains_process("git"));
        assert!(!tree.contains_process("python"));
    }

    #[test]
    fn subtree_rss() {
        let tree = sample_tree();
        // Claude subtree: 50000 + 200000 + 150000 + 3000 = 403000
        let claude_node = &tree.root.children[0];
        assert_eq!(claude_node.subtree_rss_kb(), 403000);
    }

    #[test]
    fn infer_idle() {
        let tree = ProcessTree {
            root: ProcessNode {
                pid: 1,
                ppid: 0,
                name: "bash".into(),
                argv: vec![],
                state: ProcessState::Sleeping,
                rss_kb: 5000,
                children: vec![],
            },
            total_processes: 1,
            total_rss_kb: 5000,
        };
        assert_eq!(infer_activity(&tree), PaneActivity::Idle);
    }

    #[test]
    fn infer_agent_running() {
        let tree = sample_tree();
        // Tree has claude, cargo, git — agent takes priority
        assert_eq!(infer_activity(&tree), PaneActivity::AgentRunning);
    }

    #[test]
    fn infer_compiling_no_agent() {
        let tree = ProcessTree {
            root: ProcessNode {
                pid: 1,
                ppid: 0,
                name: "bash".into(),
                argv: vec![],
                state: ProcessState::Sleeping,
                rss_kb: 5000,
                children: vec![ProcessNode {
                    pid: 2,
                    ppid: 1,
                    name: "cargo".into(),
                    argv: vec!["cargo".into(), "build".into()],
                    state: ProcessState::Running,
                    rss_kb: 100000,
                    children: vec![],
                }],
            },
            total_processes: 2,
            total_rss_kb: 105000,
        };
        assert_eq!(infer_activity(&tree), PaneActivity::Compiling);
    }

    #[test]
    fn infer_testing() {
        let tree = ProcessTree {
            root: ProcessNode {
                pid: 1,
                ppid: 0,
                name: "bash".into(),
                argv: vec![],
                state: ProcessState::Sleeping,
                rss_kb: 5000,
                children: vec![ProcessNode {
                    pid: 2,
                    ppid: 1,
                    name: "pytest".into(),
                    argv: vec!["pytest".into(), "-v".into()],
                    state: ProcessState::Running,
                    rss_kb: 50000,
                    children: vec![],
                }],
            },
            total_processes: 2,
            total_rss_kb: 55000,
        };
        assert_eq!(infer_activity(&tree), PaneActivity::Testing);
    }

    #[test]
    fn infer_version_control() {
        let tree = ProcessTree {
            root: ProcessNode {
                pid: 1,
                ppid: 0,
                name: "bash".into(),
                argv: vec![],
                state: ProcessState::Sleeping,
                rss_kb: 5000,
                children: vec![ProcessNode {
                    pid: 2,
                    ppid: 1,
                    name: "git".into(),
                    argv: vec!["git".into(), "push".into()],
                    state: ProcessState::Running,
                    rss_kb: 10000,
                    children: vec![],
                }],
            },
            total_processes: 2,
            total_rss_kb: 15000,
        };
        assert_eq!(infer_activity(&tree), PaneActivity::VersionControl);
    }

    #[test]
    fn infer_editing() {
        let tree = ProcessTree {
            root: ProcessNode {
                pid: 1,
                ppid: 0,
                name: "bash".into(),
                argv: vec![],
                state: ProcessState::Sleeping,
                rss_kb: 5000,
                children: vec![ProcessNode {
                    pid: 2,
                    ppid: 1,
                    name: "nvim".into(),
                    argv: vec!["nvim".into(), "main.rs".into()],
                    state: ProcessState::Running,
                    rss_kb: 30000,
                    children: vec![],
                }],
            },
            total_processes: 2,
            total_rss_kb: 35000,
        };
        assert_eq!(infer_activity(&tree), PaneActivity::Editing);
    }

    #[test]
    fn infer_active_unknown_process() {
        let tree = ProcessTree {
            root: ProcessNode {
                pid: 1,
                ppid: 0,
                name: "bash".into(),
                argv: vec![],
                state: ProcessState::Sleeping,
                rss_kb: 5000,
                children: vec![ProcessNode {
                    pid: 2,
                    ppid: 1,
                    name: "my-custom-tool".into(),
                    argv: vec![],
                    state: ProcessState::Running,
                    rss_kb: 10000,
                    children: vec![],
                }],
            },
            total_processes: 2,
            total_rss_kb: 15000,
        };
        assert_eq!(infer_activity(&tree), PaneActivity::Active);
    }

    #[test]
    fn process_state_display() {
        assert_eq!(format!("{}", ProcessState::Running), "running");
        assert_eq!(format!("{}", ProcessState::Zombie), "zombie");
    }

    #[test]
    fn pane_activity_display() {
        assert_eq!(format!("{}", PaneActivity::Compiling), "compiling");
        assert_eq!(format!("{}", PaneActivity::Idle), "idle");
    }

    #[test]
    fn tree_serde_roundtrip() {
        let tree = sample_tree();
        let json = serde_json::to_string(&tree).unwrap();
        let parsed: ProcessTree = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, tree);
    }

    #[test]
    fn count_tree_correct() {
        let tree = sample_tree();
        let mut count = 0;
        let mut rss = 0;
        count_tree(&tree.root, &mut count, &mut rss);
        assert_eq!(count, 5);
        assert_eq!(rss, 408000);
    }

    #[test]
    fn capture_current_process() {
        // Capture tree for our own process — should succeed on Linux/macOS.
        let pid = std::process::id();
        let config = ProcessTreeConfig {
            max_depth: 2,
            ..Default::default()
        };
        let tree = capture_tree(pid, &config);
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            let tree = tree.expect("should capture tree for current process");
            assert_eq!(tree.root.pid, pid);
            assert!(tree.total_processes >= 1);
            assert!(tree.total_rss_kb > 0);
        }
    }

    #[test]
    fn capture_nonexistent_pid() {
        let config = ProcessTreeConfig::default();
        let result = capture_tree(u32::MAX - 1, &config);
        assert!(result.is_none());
    }

    #[test]
    fn max_depth_limits_tree() {
        let config = ProcessTreeConfig {
            max_depth: 0,
            ..Default::default()
        };
        let pid = std::process::id();
        if let Some(tree) = capture_tree(pid, &config) {
            // At max_depth=0, root should have no children (even if real process does)
            assert!(tree.root.children.is_empty());
        }
    }

    #[test]
    fn process_state_serde_roundtrip() {
        for state in [
            ProcessState::Running,
            ProcessState::Sleeping,
            ProcessState::DiskSleep,
            ProcessState::Stopped,
            ProcessState::Zombie,
            ProcessState::Unknown,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            let parsed: ProcessState = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, state);
        }
    }

    #[test]
    fn pane_activity_serde_roundtrip() {
        for activity in [
            PaneActivity::Idle,
            PaneActivity::Compiling,
            PaneActivity::Testing,
            PaneActivity::VersionControl,
            PaneActivity::AgentRunning,
            PaneActivity::Editing,
            PaneActivity::Active,
        ] {
            let json = serde_json::to_string(&activity).unwrap();
            let parsed: PaneActivity = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, activity);
        }
    }
}
