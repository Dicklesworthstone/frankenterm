//! Hardware profile doctor for high-core swarm proof gates.
//!
//! This module intentionally reports unknown platform facts as
//! `unsupported`/`unavailable` instead of guessing. High-scale proof
//! consumers can therefore fail closed when a 64-core / 256 GiB claim
//! lacks live hardware evidence.

use serde::{Deserialize, Serialize};
use std::path::Path;

const SCHEMA_VERSION: u32 = 1;
const HIGH_SCALE_LOGICAL_CPUS: usize = 64;
const HIGH_SCALE_MEMORY_BYTES: u64 = 256 * 1024 * 1024 * 1024;

/// Full hardware profile used by `ft doctor --json` and future proof gates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HardwareProfileReport {
    pub schema_version: u32,
    pub platform: String,
    pub cpu: CpuProfile,
    pub memory: MemoryProfile,
    pub numa: NumaProfile,
    pub page_size_bytes: ProbeValue<u64>,
    pub file_descriptors: FileDescriptorProfile,
    pub storage: StorageProfile,
    pub cgroup: CgroupProfile,
    pub proof_predicates: HighScaleProofPredicates,
    pub recommendations: Vec<String>,
}

/// Probe value with an explicit unsupported/unavailable state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ProbeValue<T> {
    Known { value: T },
    Unavailable { reason: String },
    Unsupported { reason: String },
}

impl<T> ProbeValue<T> {
    #[must_use]
    pub fn known(value: T) -> Self {
        Self::Known { value }
    }

    #[must_use]
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::Unavailable {
            reason: reason.into(),
        }
    }

    #[must_use]
    pub fn unsupported(reason: impl Into<String>) -> Self {
        Self::Unsupported {
            reason: reason.into(),
        }
    }

    #[must_use]
    pub fn as_ref(&self) -> ProbeValue<&T> {
        match self {
            Self::Known { value } => ProbeValue::Known { value },
            Self::Unavailable { reason } => ProbeValue::Unavailable {
                reason: reason.clone(),
            },
            Self::Unsupported { reason } => ProbeValue::Unsupported {
                reason: reason.clone(),
            },
        }
    }
}

impl ProbeValue<u64> {
    #[must_use]
    fn known_value(&self) -> Option<u64> {
        match self {
            Self::Known { value } => Some(*value),
            Self::Unavailable { .. } | Self::Unsupported { .. } => None,
        }
    }
}

impl ProbeValue<usize> {
    #[must_use]
    fn known_value(&self) -> Option<usize> {
        match self {
            Self::Known { value } => Some(*value),
            Self::Unavailable { .. } | Self::Unsupported { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuProfile {
    pub logical_cores: ProbeValue<usize>,
    pub physical_cores: ProbeValue<usize>,
    pub topology_source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryProfile {
    pub total_bytes: ProbeValue<u64>,
    pub available_bytes: ProbeValue<u64>,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NumaProfile {
    pub nodes: ProbeValue<Vec<u32>>,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDescriptorProfile {
    pub nofile_soft: ProbeValue<u64>,
    pub nofile_hard: ProbeValue<u64>,
    pub current_open_fds: ProbeValue<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageProfile {
    pub path: String,
    pub total_bytes: ProbeValue<u64>,
    pub available_bytes: ProbeValue<u64>,
    pub filesystem: ProbeValue<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CgroupProfile {
    pub memory_max_bytes: ProbeValue<u64>,
    pub cpu_quota: ProbeValue<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HighScaleProofPredicates {
    pub required_logical_cores: usize,
    pub required_memory_bytes: u64,
    pub logical_cores_ok: bool,
    pub memory_ok: bool,
    pub proof_status: HardwareProofStatus,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HardwareProofStatus {
    ProvenPredicateMet,
    SkippedNotProven,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HardwareDiagnosticStatus {
    Ok,
    Warn,
}

/// One line suitable for CLI doctor rendering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HardwareDiagnosticLine {
    pub name: String,
    pub status: HardwareDiagnosticStatus,
    pub detail: String,
    pub recommendation: Option<String>,
}

impl HardwareProfileReport {
    /// Convert the report into stable doctor check lines.
    #[must_use]
    pub fn diagnostic_lines(&self) -> Vec<HardwareDiagnosticLine> {
        let mut lines = Vec::new();

        lines.push(match self.cpu.logical_cores.known_value() {
            Some(cores) => HardwareDiagnosticLine {
                name: "hardware logical cores".to_string(),
                status: HardwareDiagnosticStatus::Ok,
                detail: format!("{cores} logical core(s) ({})", self.cpu.topology_source),
                recommendation: None,
            },
            None => HardwareDiagnosticLine {
                name: "hardware logical cores".to_string(),
                status: HardwareDiagnosticStatus::Warn,
                detail: describe_probe(&self.cpu.logical_cores),
                recommendation: Some(
                    "Run ft doctor --json on the target high-core host".to_string(),
                ),
            },
        });

        lines.push(match self.memory.total_bytes.known_value() {
            Some(bytes) => HardwareDiagnosticLine {
                name: "hardware memory".to_string(),
                status: HardwareDiagnosticStatus::Ok,
                detail: format!("{} total ({})", format_bytes(bytes), self.memory.source),
                recommendation: None,
            },
            None => HardwareDiagnosticLine {
                name: "hardware memory".to_string(),
                status: HardwareDiagnosticStatus::Warn,
                detail: describe_probe(&self.memory.total_bytes),
                recommendation: Some(
                    "Run ft doctor --json on the target high-memory host".to_string(),
                ),
            },
        });

        lines.push(match &self.numa.nodes {
            ProbeValue::Known { value } => HardwareDiagnosticLine {
                name: "hardware numa".to_string(),
                status: HardwareDiagnosticStatus::Ok,
                detail: format!("{} node(s) ({})", value.len(), self.numa.source),
                recommendation: None,
            },
            ProbeValue::Unavailable { reason } | ProbeValue::Unsupported { reason } => {
                HardwareDiagnosticLine {
                    name: "hardware numa".to_string(),
                    status: HardwareDiagnosticStatus::Warn,
                    detail: format!("{} ({})", reason, self.numa.source),
                    recommendation: Some(
                        "NUMA-aware placement will use deterministic shard placement".to_string(),
                    ),
                }
            }
        });

        lines.push(match self.proof_predicates.proof_status {
            HardwareProofStatus::ProvenPredicateMet => HardwareDiagnosticLine {
                name: "64-core proof predicate".to_string(),
                status: HardwareDiagnosticStatus::Ok,
                detail: self.proof_predicates.reason.clone(),
                recommendation: None,
            },
            HardwareProofStatus::SkippedNotProven => HardwareDiagnosticLine {
                name: "64-core proof predicate".to_string(),
                status: HardwareDiagnosticStatus::Warn,
                detail: self.proof_predicates.reason.clone(),
                recommendation: Some(
                    "Treat 64-core/256GB performance claims as SKIPPED_NOT_PROVEN here".to_string(),
                ),
            },
        });

        lines
    }
}

/// Collect the current machine profile.
#[must_use]
pub fn collect_hardware_profile(workspace_root: &Path) -> HardwareProfileReport {
    let cpu = collect_cpu_profile();
    let memory = collect_memory_profile();
    let numa = collect_numa_profile();
    let page_size_bytes = collect_page_size();
    let file_descriptors = collect_fd_profile();
    let storage = collect_storage_profile(workspace_root);
    let cgroup = collect_cgroup_profile();
    let proof_predicates = build_high_scale_predicates(&cpu, &memory);
    let recommendations = build_recommendations(&proof_predicates, &numa, &file_descriptors);

    HardwareProfileReport {
        schema_version: SCHEMA_VERSION,
        platform: platform_identifier().to_string(),
        cpu,
        memory,
        numa,
        page_size_bytes,
        file_descriptors,
        storage,
        cgroup,
        proof_predicates,
        recommendations,
    }
}

fn collect_cpu_profile() -> CpuProfile {
    let logical = std::thread::available_parallelism()
        .map(|cores| ProbeValue::known(cores.get()))
        .unwrap_or_else(|_| ProbeValue::unavailable("available_parallelism failed"));

    #[cfg(target_os = "macos")]
    let physical = read_sysctl_usize("hw.physicalcpu")
        .map(ProbeValue::known)
        .unwrap_or_else(|| ProbeValue::unavailable("sysctl hw.physicalcpu unavailable"));

    #[cfg(target_os = "linux")]
    let physical = read_linux_physical_core_count()
        .map(ProbeValue::known)
        .unwrap_or_else(|| ProbeValue::unavailable("/proc/cpuinfo physical core data unavailable"));

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let physical = ProbeValue::unsupported("physical core probe unsupported on this platform");

    CpuProfile {
        logical_cores: logical,
        physical_cores: physical,
        topology_source: topology_source().to_string(),
    }
}

fn collect_memory_profile() -> MemoryProfile {
    #[cfg(target_os = "linux")]
    {
        let (total, available) = read_linux_meminfo()
            .map(|mem| {
                (
                    ProbeValue::known(mem.total_bytes),
                    ProbeValue::known(mem.available_bytes),
                )
            })
            .unwrap_or_else(|| {
                (
                    ProbeValue::unavailable("/proc/meminfo unavailable"),
                    ProbeValue::unavailable("/proc/meminfo unavailable"),
                )
            });
        MemoryProfile {
            total_bytes: total,
            available_bytes: available,
            source: "/proc/meminfo".to_string(),
        }
    }

    #[cfg(target_os = "macos")]
    {
        let total = read_sysctl_u64("hw.memsize")
            .map(ProbeValue::known)
            .unwrap_or_else(|| ProbeValue::unavailable("sysctl hw.memsize unavailable"));
        MemoryProfile {
            total_bytes: total,
            available_bytes: ProbeValue::unsupported(
                "available memory requires vm_stat interpretation outside this proof predicate",
            ),
            source: "sysctl hw.memsize".to_string(),
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        MemoryProfile {
            total_bytes: ProbeValue::unsupported("memory probe unsupported on this platform"),
            available_bytes: ProbeValue::unsupported("memory probe unsupported on this platform"),
            source: "unsupported".to_string(),
        }
    }
}

fn collect_numa_profile() -> NumaProfile {
    #[cfg(target_os = "linux")]
    {
        let nodes = std::fs::read_to_string("/sys/devices/system/node/online")
            .ok()
            .and_then(|text| parse_cpu_range_list(text.trim()).ok())
            .filter(|nodes| !nodes.is_empty())
            .map(ProbeValue::known)
            .unwrap_or_else(|| {
                ProbeValue::unavailable("/sys/devices/system/node/online unavailable")
            });
        NumaProfile {
            nodes,
            source: "linux sysfs".to_string(),
        }
    }

    #[cfg(target_os = "macos")]
    {
        NumaProfile {
            nodes: ProbeValue::unsupported("macOS does not expose Linux-style NUMA nodes"),
            source: "macos".to_string(),
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        NumaProfile {
            nodes: ProbeValue::unsupported("NUMA probe unsupported on this platform"),
            source: "unsupported".to_string(),
        }
    }
}

fn collect_page_size() -> ProbeValue<u64> {
    run_command_stdout("getconf", &["PAGESIZE"])
        .and_then(|text| text.trim().parse::<u64>().ok())
        .map(ProbeValue::known)
        .unwrap_or_else(|| ProbeValue::unavailable("getconf PAGESIZE unavailable"))
}

fn collect_fd_profile() -> FileDescriptorProfile {
    let limits = crate::fd_budget::get_system_limits();
    FileDescriptorProfile {
        nofile_soft: ProbeValue::known(limits.nofile_soft),
        nofile_hard: ProbeValue::known(limits.nofile_hard),
        current_open_fds: ProbeValue::known(limits.current_open_fds),
    }
}

fn collect_storage_profile(path: &Path) -> StorageProfile {
    let path_string = path.display().to_string();
    match run_command_stdout("df", &["-kP", &path_string]).and_then(|text| parse_df_kp(&text)) {
        Some(df) => StorageProfile {
            path: path_string,
            total_bytes: ProbeValue::known(df.total_bytes),
            available_bytes: ProbeValue::known(df.available_bytes),
            filesystem: ProbeValue::known(df.filesystem),
        },
        None => StorageProfile {
            path: path_string,
            total_bytes: ProbeValue::unavailable("df -kP unavailable or unparsable"),
            available_bytes: ProbeValue::unavailable("df -kP unavailable or unparsable"),
            filesystem: ProbeValue::unavailable("df -kP unavailable or unparsable"),
        },
    }
}

fn collect_cgroup_profile() -> CgroupProfile {
    #[cfg(target_os = "linux")]
    {
        CgroupProfile {
            memory_max_bytes: read_linux_cgroup_memory_max()
                .map(ProbeValue::known)
                .unwrap_or_else(|| {
                    ProbeValue::unavailable("cgroup memory.max unavailable or unlimited")
                }),
            cpu_quota: std::fs::read_to_string("/sys/fs/cgroup/cpu.max")
                .ok()
                .map(|text| ProbeValue::known(text.trim().to_string()))
                .unwrap_or_else(|| ProbeValue::unavailable("cgroup cpu.max unavailable")),
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        CgroupProfile {
            memory_max_bytes: ProbeValue::unsupported("cgroup v2 memory probe is Linux-only"),
            cpu_quota: ProbeValue::unsupported("cgroup v2 cpu quota probe is Linux-only"),
        }
    }
}

fn build_high_scale_predicates(
    cpu: &CpuProfile,
    memory: &MemoryProfile,
) -> HighScaleProofPredicates {
    let logical_cores_ok = cpu
        .logical_cores
        .known_value()
        .is_some_and(|cores| cores >= HIGH_SCALE_LOGICAL_CPUS);
    let memory_ok = memory
        .total_bytes
        .known_value()
        .is_some_and(|bytes| bytes >= HIGH_SCALE_MEMORY_BYTES);
    let proof_status = if logical_cores_ok && memory_ok {
        HardwareProofStatus::ProvenPredicateMet
    } else {
        HardwareProofStatus::SkippedNotProven
    };
    let reason = match proof_status {
        HardwareProofStatus::ProvenPredicateMet => format!(
            "hardware predicates met: >= {HIGH_SCALE_LOGICAL_CPUS} logical cores and >= {} memory",
            format_bytes(HIGH_SCALE_MEMORY_BYTES)
        ),
        HardwareProofStatus::SkippedNotProven => format!(
            "hardware predicates not met or unverifiable: need >= {HIGH_SCALE_LOGICAL_CPUS} logical cores and >= {} memory",
            format_bytes(HIGH_SCALE_MEMORY_BYTES)
        ),
    };

    HighScaleProofPredicates {
        required_logical_cores: HIGH_SCALE_LOGICAL_CPUS,
        required_memory_bytes: HIGH_SCALE_MEMORY_BYTES,
        logical_cores_ok,
        memory_ok,
        proof_status,
        reason,
    }
}

fn build_recommendations(
    proof: &HighScaleProofPredicates,
    numa: &NumaProfile,
    fd: &FileDescriptorProfile,
) -> Vec<String> {
    let mut recommendations = Vec::new();
    if proof.proof_status == HardwareProofStatus::SkippedNotProven {
        recommendations.push(
            "Run the high-scale proof gauntlet on a real 64+ CPU / 256GB+ RAM host before marking claims PROVEN".to_string(),
        );
    }
    if !matches!(numa.nodes, ProbeValue::Known { .. }) {
        recommendations.push(
            "NUMA-aware placement should degrade to deterministic shard placement on this host"
                .to_string(),
        );
    }
    if fd
        .nofile_soft
        .known_value()
        .is_some_and(|limit| limit < 65_536)
    {
        recommendations.push(
            "Raise ulimit -n before running large pane swarms; target at least 65536".to_string(),
        );
    }
    recommendations
}

#[cfg(target_os = "macos")]
fn read_sysctl_usize(name: &str) -> Option<usize> {
    run_command_stdout("sysctl", &["-n", name]).and_then(|text| text.trim().parse().ok())
}

#[cfg(target_os = "macos")]
fn read_sysctl_u64(name: &str) -> Option<u64> {
    run_command_stdout("sysctl", &["-n", name]).and_then(|text| text.trim().parse().ok())
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy)]
struct LinuxMemInfo {
    total_bytes: u64,
    available_bytes: u64,
}

#[cfg(target_os = "linux")]
fn read_linux_meminfo() -> Option<LinuxMemInfo> {
    parse_linux_meminfo(&std::fs::read_to_string("/proc/meminfo").ok()?)
}

#[cfg(target_os = "linux")]
fn parse_linux_meminfo(text: &str) -> Option<LinuxMemInfo> {
    let mut total = None;
    let mut available = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total = parse_kib_line(rest).map(|kib| kib.saturating_mul(1024));
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            available = parse_kib_line(rest).map(|kib| kib.saturating_mul(1024));
        }
    }
    Some(LinuxMemInfo {
        total_bytes: total?,
        available_bytes: available?,
    })
}

#[cfg(target_os = "linux")]
fn parse_kib_line(text: &str) -> Option<u64> {
    text.split_whitespace().next()?.parse::<u64>().ok()
}

#[cfg(target_os = "linux")]
fn read_linux_physical_core_count() -> Option<usize> {
    parse_linux_physical_core_count(&std::fs::read_to_string("/proc/cpuinfo").ok()?)
}

#[cfg(target_os = "linux")]
fn parse_linux_physical_core_count(text: &str) -> Option<usize> {
    let mut cores = std::collections::BTreeSet::new();
    let mut current_physical_id: Option<String> = None;
    let mut current_core_id: Option<String> = None;

    for line in text.lines().chain(std::iter::once("")) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if let (Some(socket), Some(core)) = (current_physical_id.take(), current_core_id.take())
            {
                cores.insert((socket, core));
            }
            current_physical_id = None;
            current_core_id = None;
            continue;
        }

        if let Some((key, value)) = trimmed.split_once(':') {
            let key = key.trim();
            let value = value.trim().to_string();
            match key {
                "physical id" => current_physical_id = Some(value),
                "core id" => current_core_id = Some(value),
                _ => {}
            }
        }
    }

    if cores.is_empty() {
        None
    } else {
        Some(cores.len())
    }
}

#[cfg(target_os = "linux")]
fn read_linux_cgroup_memory_max() -> Option<u64> {
    let text = std::fs::read_to_string("/sys/fs/cgroup/memory.max").ok()?;
    parse_linux_cgroup_memory_max(text.trim())
}

#[cfg(target_os = "linux")]
fn parse_linux_cgroup_memory_max(text: &str) -> Option<u64> {
    if text == "max" {
        None
    } else {
        text.parse::<u64>().ok()
    }
}

#[cfg(any(test, target_os = "linux"))]
fn parse_cpu_range_list(text: &str) -> Result<Vec<u32>, String> {
    let mut values = Vec::new();
    if text.trim().is_empty() {
        return Ok(values);
    }

    for part in text.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((start, end)) = part.split_once('-') {
            let start = start
                .trim()
                .parse::<u32>()
                .map_err(|_| format!("invalid range start: {part}"))?;
            let end = end
                .trim()
                .parse::<u32>()
                .map_err(|_| format!("invalid range end: {part}"))?;
            if start > end {
                return Err(format!("descending range: {part}"));
            }
            values.extend(start..=end);
        } else {
            values.push(
                part.parse::<u32>()
                    .map_err(|_| format!("invalid range value: {part}"))?,
            );
        }
    }
    values.sort_unstable();
    values.dedup();
    Ok(values)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DfKp {
    filesystem: String,
    total_bytes: u64,
    available_bytes: u64,
}

fn parse_df_kp(text: &str) -> Option<DfKp> {
    let mut lines = text.lines().filter(|line| !line.trim().is_empty());
    let _header = lines.next()?;
    let row = lines.next()?;
    let cols: Vec<&str> = row.split_whitespace().collect();
    if cols.len() < 6 {
        return None;
    }
    let total_kib = cols.get(1)?.parse::<u64>().ok()?;
    let available_kib = cols.get(3)?.parse::<u64>().ok()?;
    Some(DfKp {
        filesystem: cols.first()?.to_string(),
        total_bytes: total_kib.saturating_mul(1024),
        available_bytes: available_kib.saturating_mul(1024),
    })
}

fn run_command_stdout(command: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new(command)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

fn topology_source() -> &'static str {
    if cfg!(target_os = "linux") {
        "available_parallelism + /proc/cpuinfo"
    } else if cfg!(target_os = "macos") {
        "available_parallelism + sysctl"
    } else {
        "available_parallelism"
    }
}

fn platform_identifier() -> &'static str {
    if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(unix) {
        "unix"
    } else {
        "unknown"
    }
}

fn describe_probe<T>(probe: &ProbeValue<T>) -> String {
    match probe {
        ProbeValue::Known { .. } => "known".to_string(),
        ProbeValue::Unavailable { reason } => format!("unavailable: {reason}"),
        ProbeValue::Unsupported { reason } => format!("unsupported: {reason}"),
    }
}

fn format_bytes(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else {
        format!("{bytes} bytes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cpu_range_list_expands_and_deduplicates() {
        let nodes = parse_cpu_range_list("0-2,2,4").unwrap();
        assert_eq!(nodes, vec![0, 1, 2, 4]);
    }

    #[test]
    fn parse_cpu_range_list_rejects_descending_range() {
        let err = parse_cpu_range_list("4-2").unwrap_err();
        assert!(err.contains("descending range"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_linux_meminfo_extracts_total_and_available() {
        let parsed = parse_linux_meminfo(
            "MemTotal:       1024 kB\nMemFree:         32 kB\nMemAvailable:   768 kB\n",
        )
        .unwrap();
        assert_eq!(parsed.total_bytes, 1024 * 1024);
        assert_eq!(parsed.available_bytes, 768 * 1024);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_linux_physical_core_count_counts_socket_core_pairs() {
        let cpuinfo = "\
processor   : 0
physical id : 0
core id     : 0

processor   : 1
physical id : 0
core id     : 1

processor   : 2
physical id : 1
core id     : 0
";
        assert_eq!(parse_linux_physical_core_count(cpuinfo), Some(3));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_linux_cgroup_memory_max_treats_max_as_unbounded() {
        assert_eq!(parse_linux_cgroup_memory_max("max"), None);
        assert_eq!(parse_linux_cgroup_memory_max("1024"), Some(1024));
    }

    #[test]
    fn parse_df_kp_extracts_capacity() {
        let parsed = parse_df_kp(
            "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/disk1 100 25 75 25% /tmp\n",
        )
        .unwrap();
        assert_eq!(parsed.filesystem, "/dev/disk1");
        assert_eq!(parsed.total_bytes, 100 * 1024);
        assert_eq!(parsed.available_bytes, 75 * 1024);
    }

    #[test]
    fn high_scale_predicates_fail_closed_on_unknowns() {
        let cpu = CpuProfile {
            logical_cores: ProbeValue::unavailable("no cpu"),
            physical_cores: ProbeValue::unavailable("no cpu"),
            topology_source: "test".to_string(),
        };
        let memory = MemoryProfile {
            total_bytes: ProbeValue::unavailable("no memory"),
            available_bytes: ProbeValue::unavailable("no memory"),
            source: "test".to_string(),
        };
        let proof = build_high_scale_predicates(&cpu, &memory);
        assert_eq!(proof.proof_status, HardwareProofStatus::SkippedNotProven);
        assert!(!proof.logical_cores_ok);
        assert!(!proof.memory_ok);
    }

    #[test]
    fn high_scale_predicates_pass_when_requirements_are_met() {
        let cpu = CpuProfile {
            logical_cores: ProbeValue::known(64),
            physical_cores: ProbeValue::known(32),
            topology_source: "test".to_string(),
        };
        let memory = MemoryProfile {
            total_bytes: ProbeValue::known(256 * 1024 * 1024 * 1024),
            available_bytes: ProbeValue::known(128 * 1024 * 1024 * 1024),
            source: "test".to_string(),
        };
        let proof = build_high_scale_predicates(&cpu, &memory);
        assert_eq!(proof.proof_status, HardwareProofStatus::ProvenPredicateMet);
        assert!(proof.logical_cores_ok);
        assert!(proof.memory_ok);
    }
}
