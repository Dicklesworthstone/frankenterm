#![cfg(target_os = "linux")]
use super::*;

impl From<&str> for LocalProcessStatus {
    fn from(s: &str) -> Self {
        match s {
            "R" => Self::Run,
            "S" => Self::Sleep,
            "D" => Self::Idle,
            "Z" => Self::Zombie,
            "T" => Self::Stop,
            "t" => Self::Tracing,
            "X" | "x" => Self::Dead,
            "K" => Self::Wakekill,
            "W" => Self::Waking,
            "P" => Self::Parked,
            _ => Self::Unknown,
        }
    }
}

fn normalized_machine_id(raw: &str) -> Option<String> {
    let value = raw.trim();
    if value.len() != 32
        || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
        || value.bytes().all(|byte| byte == b'0')
    {
        return None;
    }
    Some(value.to_ascii_lowercase())
}

impl LocalProcessInfo {
    fn observe_stat_start_time(pid: libc::pid_t) -> ProcessStartTimeObservation {
        let data = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(data) => data,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // ENOENT is ambiguous under procfs `hidepid`: another user's
                // live process can be deliberately invisible. A signal value
                // of zero performs an existence/permission check without
                // delivering a signal; only ESRCH proves absence.
                let result = unsafe { libc::kill(pid, 0) };
                if result == -1
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
                {
                    return ProcessStartTimeObservation::Absent;
                }
                return ProcessStartTimeObservation::Unknown;
            }
            Err(_) => return ProcessStartTimeObservation::Unknown,
        };
        let Some((_pid_space, name)) = data.split_once('(') else {
            return ProcessStartTimeObservation::Unknown;
        };
        let Some((_name, fields)) = name.rsplit_once(')') else {
            return ProcessStartTimeObservation::Unknown;
        };
        match fields
            .split_whitespace()
            .nth(19)
            .and_then(|value| value.parse().ok())
        {
            Some(start_time) => ProcessStartTimeObservation::Running(start_time),
            None => ProcessStartTimeObservation::Unknown,
        }
    }

    /// Read one process-incarnation token without enumerating the process tree.
    pub fn process_start_time(pid: u32) -> Option<u64> {
        match Self::observe_process_start_time(pid) {
            ProcessStartTimeObservation::Running(start_time) => Some(start_time),
            ProcessStartTimeObservation::Absent | ProcessStartTimeObservation::Unknown => None,
        }
    }

    pub fn observe_process_start_time(pid: u32) -> ProcessStartTimeObservation {
        if pid == 0 {
            return ProcessStartTimeObservation::Unknown;
        }
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return ProcessStartTimeObservation::Unknown;
        };
        Self::observe_stat_start_time(pid)
    }

    /// Linux exposes a kernel-generated UUID that is constant for one boot.
    pub fn host_boot_id() -> Option<String> {
        let value = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_string())
    }

    /// Hash the installation-stable Linux machine ID into an application-
    /// scoped fence. The raw `/etc/machine-id` value is confidential and is
    /// never returned or persisted.
    pub fn host_machine_fence() -> Option<String> {
        use std::io::Read;

        let file = std::fs::File::open("/etc/machine-id").ok()?;
        let mut raw = String::new();
        file.take(
            u64::try_from(MAX_RAW_MACHINE_ID_BYTES)
                .ok()?
                .saturating_add(1),
        )
        .read_to_string(&mut raw)
        .ok()?;
        if raw.len() > MAX_RAW_MACHINE_ID_BYTES {
            return None;
        }
        let normalized = normalized_machine_id(&raw)?;
        app_scoped_machine_fence(normalized.as_bytes())
    }

    pub fn current_working_dir(pid: u32) -> Option<PathBuf> {
        if pid == 0 {
            return None;
        }
        std::fs::read_link(format!("/proc/{}/cwd", pid)).ok()
    }

    pub fn executable_path(pid: u32) -> Option<PathBuf> {
        if pid == 0 {
            return None;
        }
        std::fs::read_link(format!("/proc/{}/exe", pid)).ok()
    }

    pub fn with_root_pid(pid: u32) -> Option<Self> {
        use libc::pid_t;

        if pid == 0 {
            return None;
        }
        let pid = pid_t::try_from(pid).ok()?;

        fn all_pids() -> Vec<pid_t> {
            let mut pids = vec![];
            if let Ok(dir) = std::fs::read_dir("/proc") {
                for entry in dir.flatten() {
                    if let Ok(file_type) = entry.file_type() {
                        if file_type.is_dir() {
                            if let Some(name) = entry.file_name().to_str() {
                                if let Ok(pid) = name.parse::<pid_t>() {
                                    pids.push(pid);
                                }
                            }
                        }
                    }
                }
            }
            pids
        }

        struct LinuxStat {
            pid: pid_t,
            name: String,
            status: String,
            ppid: pid_t,
            // Time process started after boot, measured in ticks
            starttime: u64,
        }

        fn info_for_pid(pid: pid_t) -> Option<LinuxStat> {
            let data = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
            let (_pid_space, name) = data.split_once('(')?;
            let (name, fields) = name.rsplit_once(')')?;
            let fields = fields.split_whitespace().collect::<Vec<_>>();

            Some(LinuxStat {
                pid,
                name: name.to_string(),
                status: fields.first()?.to_string(),
                ppid: fields.get(1)?.parse().ok()?,
                starttime: fields.get(19)?.parse().ok()?,
            })
        }

        fn exe_for_pid(pid: pid_t) -> PathBuf {
            std::fs::read_link(format!("/proc/{}/exe", pid)).unwrap_or_else(|_| PathBuf::new())
        }

        fn cwd_for_pid(pid: pid_t) -> PathBuf {
            LocalProcessInfo::current_working_dir(pid as u32).unwrap_or_else(PathBuf::new)
        }

        fn parse_cmdline(pid: pid_t) -> Vec<String> {
            let data = match std::fs::read(format!("/proc/{}/cmdline", pid)) {
                Ok(data) => data,
                Err(_) => return vec![],
            };

            let mut args = vec![];

            let data = data.strip_suffix(&[0]).unwrap_or(&data);

            for arg in data.split(|&c| c == 0) {
                args.push(String::from_utf8_lossy(arg).to_owned().to_string());
            }

            args
        }

        let procs: Vec<_> = all_pids().into_iter().filter_map(info_for_pid).collect();

        fn build_proc(
            info: &LinuxStat,
            procs: &[LinuxStat],
            ancestry: &mut HashSet<pid_t>,
        ) -> LocalProcessInfo {
            let inserted = ancestry.insert(info.pid);
            debug_assert!(inserted);
            let mut children = HashMap::new();

            for kid in procs {
                if kid.ppid == info.pid && !ancestry.contains(&kid.pid) {
                    children.insert(kid.pid as u32, build_proc(kid, procs, ancestry));
                }
            }
            let removed = ancestry.remove(&info.pid);
            debug_assert!(removed);

            let executable = exe_for_pid(info.pid);
            let name = info.name.clone();
            let argv = parse_cmdline(info.pid);

            LocalProcessInfo {
                pid: info.pid as _,
                ppid: info.ppid as _,
                name,
                executable,
                cwd: cwd_for_pid(info.pid),
                argv,
                start_time: info.starttime,
                status: info.status.as_str().into(),
                children,
            }
        }

        if let Some(info) = procs.iter().find(|info| info.pid == pid) {
            Some(build_proc(info, &procs, &mut HashSet::new()))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod machine_identity_tests {
    use super::*;

    #[test]
    fn linux_machine_id_normalization_is_strict() {
        assert_eq!(
            normalized_machine_id("0123456789ABCDEF0123456789ABCDEF\n").as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        for invalid in [
            "",
            "uninitialized",
            "00000000000000000000000000000000",
            "0123456789abcdef",
            "0123456789abcdef0123456789abcdeg",
        ] {
            assert_eq!(normalized_machine_id(invalid), None, "accepted {invalid:?}");
        }
    }
}
