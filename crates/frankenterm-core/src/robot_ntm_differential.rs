//! NTM differential test harness for robot family parity.
//!
//! **Bead:** [BR-RC-ROBOT-CONTRACT.0.1] / `ft-hac7w.1.1`
//! **Companion doc:** [`docs/robot-contracts/ntm-differential-rules.md`].
//!
//! # What this is
//!
//! The bridge plan requires that every robot family with an `ntm`
//! equivalent eventually shows zero observable divergence against a
//! real `ntm` subprocess on a 1000-request fuzz corpus. This module is
//! the methodology shared by every per-family bead
//! (`ft-hac7w.2`…`ft-hac7w.6`): a request stream flows through ft's
//! native handler and an [`NtmInvoker`], the two responses are
//! normalized via the rule table in the companion doc, and the harness
//! asserts byte-for-byte equality on the normalized values. Only
//! [`NtmSubprocessInvoker`] produces live `ntm` parity evidence; mirror
//! invokers are substrate/conformance evidence only.
//!
//! Per-family beads plug into this harness by supplying:
//!
//! 1. The family's [`FamilyContract`] (already shipped in
//!    `robot_family_contract.rs` under ft-hac7w.1).
//! 2. A native handler closure with signature
//!    `Fn(&Value) -> Result<Value, String>`.
//! 3. Optionally, an `ntm` subcommand mapping for the action.
//!    Families that have NO `ntm` equivalent simply skip the
//!    differential half — the harness's normal conformance path
//!    still runs.
//!
//! # What this commit ships
//!
//! `ft-hac7w.1.1` is the methodology bead. This module ships:
//!
//! - The [`DifferentialHarness`] type with the public API every
//!   downstream family will call.
//! - The [`NormalizationRules`] table mirroring the companion
//!   doc's Layer 1 + Layer 2 entries.
//! - [`HarnessMode`] for switching between CI normalization
//!   (Layers 1 + 2) and host-state mode (Layer 1 only).
//! - The [`DivergenceReport`] data type the harness returns when
//!   responses diverge after normalization.
//!
//! What this module does NOT ship (filed as follow-on beads):
//!
//! - The 1000-request fuzz corpus per family — each per-family
//!   bead authors its own corpus.
//! - The CI integration that runs the corpus on every PR — filed
//!   as a follow-on bead because the corpus must exist first.
//! - Family-specific command mappings for every `ntm` surface.
//!
//! # Why a trait-shaped invoker
//!
//! `ntm` is an external CLI; on a developer machine without `ntm`
//! installed, the harness must still compile and run its
//! self-tests. The [`NtmInvoker`] trait abstracts the subprocess:
//! [`NtmSubprocessInvoker`] shells out through explicit per-action
//! command mappings; tests can use [`MockNtmInvoker`] or other
//! mirror invokers when they are proving only native-handler
//! conformance rather than live `ntm` parity.
//!
//! # Cross-references
//!
//! - [`crate::robot_family_contract`] — schema-DSL produced by
//!   ft-hac7w.1.
//! - `tests/robot_family_conformance.rs` — the conformance harness
//!   the differential harness composes with.

use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::robot_ntm_surface::RobotNtmCommand;

/// Replacement token written in place of a normalized field's
/// original value. Constant per-category so harness output stays
/// stable across runs.
pub mod tokens {
    pub const TS: &str = "<NORMALIZED:ts>";
    pub const DURATION: &str = "<NORMALIZED:duration>";
    pub const PID: &str = "<NORMALIZED:pid>";
    pub const UUID: &str = "<NORMALIZED:uuid>";
    pub const HOST: &str = "<NORMALIZED:host>";
    pub const VERSION: &str = "<NORMALIZED:version>";
    pub const CWD: &str = "<NORMALIZED:cwd>";
    pub const HOME: &str = "<NORMALIZED:home>";
    pub const TMP: &str = "<NORMALIZED:tmp>";
    pub const UID: &str = "<NORMALIZED:uid>";
    pub const SOCK: &str = "<NORMALIZED:sock>";
}

/// Harness operating mode controlling which normalization layers
/// fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessMode {
    /// CI mode: applies Layer 1 (trivial drift) + Layer 2
    /// (operational drift). Use when the test runs in CI or in
    /// any context where host-state divergence is acceptable.
    Ci,
    /// Host-state mode: applies Layer 1 only. Use when the test
    /// is a real integration test that intends to assert on the
    /// host's actual paths / uid / hostname.
    HostState,
}

/// One normalization rule. The harness walks the response JSON
/// looking for any field whose name matches `field_name` and
/// replaces its value with [`replacement`]. The match is on field
/// *names*, not full JSON pointers — a deeply nested `timestamp`
/// at any depth gets normalized.
#[derive(Debug, Clone)]
pub struct NormalizationRule {
    /// JSON object key to match on (e.g. `"timestamp"`,
    /// `"pid"`).
    pub field_name: &'static str,
    /// Replacement token. Constant per-category.
    pub replacement: &'static str,
    /// Which layer this rule belongs to. Determines whether it
    /// fires in [`HarnessMode::HostState`].
    pub layer: NormalizationLayer,
}

/// Drift-classification layer for a normalization rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NormalizationLayer {
    /// Layer 1 — trivial drift. Always fires.
    Trivial,
    /// Layer 2 — operational drift. Fires only in CI mode.
    Operational,
}

/// Default normalization rule table mirroring the companion doc.
/// New families add rules here as they discover trivial- or
/// operational-drift fields. Real divergence (Layer 3) is the
/// implicit default — anything not in this table is asserted on
/// byte-for-byte.
#[must_use]
pub fn default_normalization_rules() -> Vec<NormalizationRule> {
    use NormalizationLayer::{Operational, Trivial};
    vec![
        // Layer 1: trivial drift (Always fires).
        NormalizationRule {
            field_name: "timestamp",
            replacement: tokens::TS,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "created_at",
            replacement: tokens::TS,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "updated_at",
            replacement: tokens::TS,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "started_at",
            replacement: tokens::TS,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "completed_at",
            replacement: tokens::TS,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "last_used_at",
            replacement: tokens::TS,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "last_seen_at",
            replacement: tokens::TS,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "first_seen_at",
            replacement: tokens::TS,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "closed_at",
            replacement: tokens::TS,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "expires_at",
            replacement: tokens::TS,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "duration_ms",
            replacement: tokens::DURATION,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "elapsed_ms",
            replacement: tokens::DURATION,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "duration_us",
            replacement: tokens::DURATION,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "elapsed_us",
            replacement: tokens::DURATION,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "took_ms",
            replacement: tokens::DURATION,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "wait_ms",
            replacement: tokens::DURATION,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "pid",
            replacement: tokens::PID,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "process_id",
            replacement: tokens::PID,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "parent_pid",
            replacement: tokens::PID,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "child_pid",
            replacement: tokens::PID,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "runner_pid",
            replacement: tokens::PID,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "uuid",
            replacement: tokens::UUID,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "session_uuid",
            replacement: tokens::UUID,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "correlation_id",
            replacement: tokens::UUID,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "request_id",
            replacement: tokens::UUID,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "execution_id",
            replacement: tokens::UUID,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "run_id",
            replacement: tokens::UUID,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "trace_id",
            replacement: tokens::UUID,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "hostname",
            replacement: tokens::HOST,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "host_name",
            replacement: tokens::HOST,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "host",
            replacement: tokens::HOST,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "version",
            replacement: tokens::VERSION,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "build_sha",
            replacement: tokens::VERSION,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "git_sha",
            replacement: tokens::VERSION,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "commit_sha",
            replacement: tokens::VERSION,
            layer: Trivial,
        },
        // Layer 2: operational drift (Only fires in CI mode).
        NormalizationRule {
            field_name: "cwd",
            replacement: tokens::CWD,
            layer: Operational,
        },
        NormalizationRule {
            field_name: "working_dir",
            replacement: tokens::CWD,
            layer: Operational,
        },
        NormalizationRule {
            field_name: "work_dir",
            replacement: tokens::CWD,
            layer: Operational,
        },
        NormalizationRule {
            field_name: "working_directory",
            replacement: tokens::CWD,
            layer: Operational,
        },
        NormalizationRule {
            field_name: "home_dir",
            replacement: tokens::HOME,
            layer: Operational,
        },
        NormalizationRule {
            field_name: "home",
            replacement: tokens::HOME,
            layer: Operational,
        },
        NormalizationRule {
            field_name: "user_home",
            replacement: tokens::HOME,
            layer: Operational,
        },
        NormalizationRule {
            field_name: "temp_dir",
            replacement: tokens::TMP,
            layer: Operational,
        },
        NormalizationRule {
            field_name: "tmp",
            replacement: tokens::TMP,
            layer: Operational,
        },
        NormalizationRule {
            field_name: "runtime_dir",
            replacement: tokens::TMP,
            layer: Operational,
        },
        NormalizationRule {
            field_name: "uid",
            replacement: tokens::UID,
            layer: Operational,
        },
        NormalizationRule {
            field_name: "gid",
            replacement: tokens::UID,
            layer: Operational,
        },
        NormalizationRule {
            field_name: "euid",
            replacement: tokens::UID,
            layer: Operational,
        },
        NormalizationRule {
            field_name: "egid",
            replacement: tokens::UID,
            layer: Operational,
        },
        NormalizationRule {
            field_name: "socket_path",
            replacement: tokens::SOCK,
            layer: Operational,
        },
        NormalizationRule {
            field_name: "ipc_socket_path",
            replacement: tokens::SOCK,
            layer: Operational,
        },
        NormalizationRule {
            field_name: "sock",
            replacement: tokens::SOCK,
            layer: Operational,
        },
    ]
}

/// Apply the rule table to a response value in place. Walks the
/// JSON tree depth-first and rewrites every matching object key's
/// value to the rule's replacement token.
pub fn normalize(value: &mut Value, rules: &[NormalizationRule], mode: HarnessMode) {
    let active: Vec<&NormalizationRule> = rules
        .iter()
        .filter(|r| match (mode, r.layer) {
            (HarnessMode::Ci, _) => true,
            (HarnessMode::HostState, NormalizationLayer::Trivial) => true,
            (HarnessMode::HostState, NormalizationLayer::Operational) => false,
        })
        .collect();
    let lookup: BTreeMap<&str, &str> = active
        .iter()
        .map(|r| (r.field_name, r.replacement))
        .collect();
    walk_normalize(value, &lookup);
}

fn walk_normalize(value: &mut Value, lookup: &BTreeMap<&str, &str>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if let Some(replacement) = lookup.get(k.as_str()) {
                    *v = Value::String((*replacement).to_string());
                } else {
                    walk_normalize(v, lookup);
                }
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                walk_normalize(item, lookup);
            }
        }
        _ => {}
    }
}

/// Trait abstracting the `ntm` subprocess so the harness compiles
/// and self-tests without a real `ntm` binary. The production
/// implementation shells out to `ntm <action> --json <request>`;
/// tests use [`MockNtmInvoker`].
pub trait NtmInvoker {
    /// Invoke the `ntm` equivalent of `family.action` with the
    /// given request, returning the response JSON or an error
    /// describing why the invocation failed (subprocess crashed,
    /// JSON parse error, action not implemented, etc.).
    fn invoke(&self, family: &str, action: &str, request: &Value) -> Result<Value, String>;
}

/// How a subprocess command receives the robot-family request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NtmRequestEncoding {
    /// Do not pass the request to the subprocess. Use only for commands whose
    /// request is fully represented by static command-line arguments.
    Omit,
    /// Append the serialized request JSON as the final command-line argument.
    JsonArg,
    /// Write the serialized request JSON to stdin.
    JsonStdin,
}

/// Command mapping for one `(family, action)` pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NtmSubprocessCommand {
    args: Vec<String>,
    request_encoding: NtmRequestEncoding,
}

impl NtmSubprocessCommand {
    /// Invoke the command with no serialized request payload.
    #[must_use]
    pub fn no_request<I, S>(args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::new(args, NtmRequestEncoding::Omit)
    }

    /// Invoke the command with the request JSON appended as the final argument.
    #[must_use]
    pub fn json_arg<I, S>(args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::new(args, NtmRequestEncoding::JsonArg)
    }

    /// Invoke the command with the request JSON written to stdin.
    #[must_use]
    pub fn json_stdin<I, S>(args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::new(args, NtmRequestEncoding::JsonStdin)
    }

    /// Build a command from a canonical `ntm ...` equivalence string.
    ///
    /// The leading `ntm` token is intentionally stripped because
    /// [`NtmSubprocessInvoker`] owns the binary path. This keeps live parity
    /// tests from accidentally shelling out to a second binary named in the
    /// metadata.
    pub fn try_from_ntm_command(
        ntm_command: &str,
        request_encoding: NtmRequestEncoding,
    ) -> Result<Self, String> {
        let mut parts = ntm_command.split_whitespace();
        let Some(binary) = parts.next() else {
            return Err("ntm command mapping cannot be empty".to_string());
        };
        if binary != "ntm" {
            return Err(format!(
                "ntm command mapping must start with `ntm`, got `{binary}` in `{ntm_command}`"
            ));
        }
        let args: Vec<String> = parts.map(str::to_string).collect();
        if args.is_empty() {
            return Err(format!(
                "ntm command mapping must include a subcommand after `ntm`: `{ntm_command}`"
            ));
        }
        Ok(Self::new(args, request_encoding))
    }

    /// Build a subprocess command from a robot family's first declared NTM
    /// equivalent.
    pub fn try_from_robot_command(
        command: &RobotNtmCommand,
        request_encoding: NtmRequestEncoding,
    ) -> Result<Self, String> {
        let equivalence = command.ntm_equivalence();
        let Some(ntm_command) = equivalence.ntm_commands.first() else {
            return Err(format!(
                "no ntm equivalent command declared for {}.{}",
                command.family_name(),
                command.action_name()
            ));
        };
        Self::try_from_ntm_command(ntm_command, request_encoding)
    }

    fn new<I, S>(args: I, request_encoding: NtmRequestEncoding) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            args: args.into_iter().map(Into::into).collect(),
            request_encoding,
        }
    }
}

/// Real subprocess-backed invoker for differential evidence.
///
/// The invoker is intentionally explicit about command mappings: different
/// `ntm` families expose different CLIs, and a mirror test must not silently
/// graduate into "real ntm parity" just because a binary happens to exist.
pub struct NtmSubprocessInvoker {
    binary: PathBuf,
    timeout: Duration,
    commands: BTreeMap<(String, String), NtmSubprocessCommand>,
}

impl NtmSubprocessInvoker {
    /// Default timeout for one `ntm` subprocess invocation.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

    /// Construct an invoker for a concrete binary path or PATH-resolved name.
    #[must_use]
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            timeout: Self::DEFAULT_TIMEOUT,
            commands: BTreeMap::new(),
        }
    }

    /// Construct an invoker for the default `ntm` binary.
    #[must_use]
    pub fn ntm() -> Self {
        Self::new("ntm")
    }

    /// Override the per-invocation timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Register a command mapping for one robot family action.
    #[must_use]
    pub fn with_command(
        mut self,
        family: impl Into<String>,
        action: impl Into<String>,
        command: NtmSubprocessCommand,
    ) -> Self {
        self.commands
            .insert((family.into(), action.into()), command);
        self
    }

    /// Register the first canonical NTM equivalence for a robot command.
    pub fn try_with_robot_command(
        mut self,
        command: &RobotNtmCommand,
        request_encoding: NtmRequestEncoding,
    ) -> Result<Self, String> {
        let ntm_command = NtmSubprocessCommand::try_from_robot_command(command, request_encoding)?;
        self.commands.insert(
            (
                command.family_name().to_string(),
                command.action_name().to_string(),
            ),
            ntm_command,
        );
        Ok(self)
    }
}

impl NtmInvoker for NtmSubprocessInvoker {
    fn invoke(&self, family: &str, action: &str, request: &Value) -> Result<Value, String> {
        let command = self
            .commands
            .get(&(family.to_string(), action.to_string()))
            .ok_or_else(|| format!("no ntm subprocess command registered for {family}.{action}"))?;

        run_ntm_subprocess(&self.binary, command, request, self.timeout, family, action)
    }
}

fn run_ntm_subprocess(
    binary: &Path,
    command: &NtmSubprocessCommand,
    request: &Value,
    timeout: Duration,
    family: &str,
    action: &str,
) -> Result<Value, String> {
    let mut process = Command::new(binary);
    process.args(&command.args);
    if command.request_encoding == NtmRequestEncoding::JsonArg {
        process.arg(request.to_string());
    }

    let mut child = process
        .stdin(match command.request_encoding {
            NtmRequestEncoding::JsonStdin => Stdio::piped(),
            NtmRequestEncoding::Omit | NtmRequestEncoding::JsonArg => Stdio::null(),
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| {
            format!(
                "failed to spawn ntm subprocess for {family}.{action} ({}): {err}",
                describe_ntm_command(binary, &command.args)
            )
        })?;

    if command.request_encoding == NtmRequestEncoding::JsonStdin {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| format!("ntm subprocess stdin unavailable for {family}.{action}"))?;
        if let Err(err) = stdin.write_all(request.to_string().as_bytes()) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "failed to write request JSON to ntm subprocess for {family}.{action}: {err}"
            ));
        }
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("ntm subprocess stdout unavailable for {family}.{action}"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| format!("ntm subprocess stderr unavailable for {family}.{action}"))?;
    let stdout_reader = thread::spawn(move || read_stream(stdout));
    let stderr_reader = thread::spawn(move || read_stream(stderr));

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = join_reader(stdout_reader, "stdout")?;
                let stderr = join_reader(stderr_reader, "stderr")?;
                if !status.success() {
                    return Err(format!(
                        "ntm subprocess for {family}.{action} exited with {status}: {}",
                        trim_bytes_for_error(&stderr)
                    ));
                }
                return serde_json::from_slice(&stdout).map_err(|err| {
                    format!(
                        "ntm subprocess for {family}.{action} returned invalid JSON: {err}; stdout={}",
                        trim_bytes_for_error(&stdout)
                    )
                });
            }
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = join_reader(stdout_reader, "stdout");
                let _ = join_reader(stderr_reader, "stderr");
                return Err(format!(
                    "ntm subprocess for {family}.{action} timed out after {} ms",
                    timeout.as_millis()
                ));
            }
            Ok(None) => thread::sleep(Duration::from_millis(5)),
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "failed while waiting for ntm subprocess for {family}.{action}: {err}"
                ));
            }
        }
    }
}

fn read_stream(mut stream: impl Read) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn join_reader(
    handle: thread::JoinHandle<std::io::Result<Vec<u8>>>,
    stream_name: &str,
) -> Result<Vec<u8>, String> {
    handle
        .join()
        .map_err(|_| format!("ntm subprocess {stream_name} reader panicked"))?
        .map_err(|err| format!("failed to read ntm subprocess {stream_name}: {err}"))
}

fn trim_bytes_for_error(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim();
    if trimmed.len() > 512 {
        let mut end = 512;
        while !trimmed.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &trimmed[..end])
    } else {
        trimmed.to_string()
    }
}

fn describe_ntm_command(binary: &Path, args: &[String]) -> String {
    let mut parts = vec![binary.display().to_string()];
    parts.extend(args.iter().cloned());
    parts.join(" ")
}

/// In-memory mock NTM invoker for testing. Indexed by
/// `(family, action)`; the harness asserts on whatever response
/// the test pre-loaded.
pub struct MockNtmInvoker {
    responses: BTreeMap<(String, String), Value>,
}

impl MockNtmInvoker {
    /// Construct an empty mock. Pre-load entries via [`Self::with_response`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            responses: BTreeMap::new(),
        }
    }

    /// Pre-load a `(family, action) -> response` mapping. The
    /// `request` is not part of the key in the mock — tests that
    /// need request-dependent responses subclass via wrapping.
    #[must_use]
    pub fn with_response(mut self, family: &str, action: &str, response: Value) -> Self {
        self.responses
            .insert((family.to_string(), action.to_string()), response);
        self
    }
}

impl Default for MockNtmInvoker {
    fn default() -> Self {
        Self::new()
    }
}

impl NtmInvoker for MockNtmInvoker {
    fn invoke(&self, family: &str, action: &str, _request: &Value) -> Result<Value, String> {
        self.responses
            .get(&(family.to_string(), action.to_string()))
            .cloned()
            .ok_or_else(|| format!("MockNtmInvoker: no response registered for {family}.{action}"))
    }
}

/// Differential-comparison report. `Match` means the normalized
/// responses are byte-equal; `Diverge` carries the two normalized
/// values plus a JSON-pointer-formatted explanation of the first
/// observed divergence.
#[derive(Debug, Clone)]
pub enum DivergenceReport {
    Match,
    Diverge {
        native: Value,
        ntm: Value,
        explanation: String,
    },
}

impl DivergenceReport {
    #[must_use]
    pub fn is_match(&self) -> bool {
        matches!(self, Self::Match)
    }
}

/// The differential harness itself. Each per-family bead constructs
/// one of these (with its handler + invoker + rules) and calls
/// [`Self::compare`] for every request in the corpus.
pub struct DifferentialHarness<'a, F>
where
    F: Fn(&Value) -> Result<Value, String>,
{
    family: &'a str,
    action: &'a str,
    native_handler: F,
    invoker: &'a dyn NtmInvoker,
    rules: Vec<NormalizationRule>,
    mode: HarnessMode,
}

impl<'a, F> DifferentialHarness<'a, F>
where
    F: Fn(&Value) -> Result<Value, String>,
{
    pub fn new(
        family: &'a str,
        action: &'a str,
        native_handler: F,
        invoker: &'a dyn NtmInvoker,
    ) -> Self {
        Self {
            family,
            action,
            native_handler,
            invoker,
            rules: default_normalization_rules(),
            mode: HarnessMode::Ci,
        }
    }

    /// Override the operating mode. Default is [`HarnessMode::Ci`].
    #[must_use]
    pub fn with_mode(mut self, mode: HarnessMode) -> Self {
        self.mode = mode;
        self
    }

    /// Replace the rule table (e.g. to add a family-specific
    /// trivial-drift field). Default is [`default_normalization_rules`].
    #[must_use]
    pub fn with_rules(mut self, rules: Vec<NormalizationRule>) -> Self {
        self.rules = rules;
        self
    }

    /// Compare the native + ntm responses for one request.
    /// Both responses are normalized via the rule table before
    /// equality comparison.
    pub fn compare(&self, request: &Value) -> Result<DivergenceReport, String> {
        let mut native_response = (self.native_handler)(request)?;
        let mut ntm_response = self.invoker.invoke(self.family, self.action, request)?;

        normalize(&mut native_response, &self.rules, self.mode);
        normalize(&mut ntm_response, &self.rules, self.mode);

        if native_response == ntm_response {
            Ok(DivergenceReport::Match)
        } else {
            let explanation = first_divergence(&native_response, &ntm_response, "");
            Ok(DivergenceReport::Diverge {
                native: native_response,
                ntm: ntm_response,
                explanation,
            })
        }
    }
}

/// Walk two values in parallel and return a JSON-pointer-formatted
/// description of the first observed difference. Used to give
/// `DivergenceReport::Diverge` an actionable hint.
fn first_divergence(a: &Value, b: &Value, pointer: &str) -> String {
    match (a, b) {
        (Value::Object(am), Value::Object(bm)) => {
            for (k, av) in am {
                let next_pointer = child_pointer(pointer, k);
                match bm.get(k) {
                    Some(bv) => {
                        let recursed = first_divergence(av, bv, &next_pointer);
                        if !recursed.is_empty() {
                            return recursed;
                        }
                    }
                    None => return format!("missing in ntm: {next_pointer}"),
                }
            }
            for k in bm.keys() {
                if !am.contains_key(k) {
                    return format!("extra in ntm: {pointer}/{k}");
                }
            }
            String::new()
        }
        (Value::Array(av), Value::Array(bv)) => {
            if av.len() != bv.len() {
                return format!(
                    "array length differs at {pointer}: native={} ntm={}",
                    av.len(),
                    bv.len()
                );
            }
            for (i, (ai, bi)) in av.iter().zip(bv.iter()).enumerate() {
                let next_pointer = format!("{pointer}/{i}");
                let recursed = first_divergence(ai, bi, &next_pointer);
                if !recursed.is_empty() {
                    return recursed;
                }
            }
            String::new()
        }
        _ if a == b => String::new(),
        _ => format!("value differs at {pointer}: native={a} ntm={b}"),
    }
}

fn child_pointer(parent: &str, key: &str) -> String {
    format!("{parent}/{}", escape_json_pointer_segment(key))
}

fn escape_json_pointer_segment(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::robot_ntm_surface::{
        ProfileCommand, ProfileShowRequest, ProfileValidateRequest, RobotNtmCommand,
    };
    use serde_json::json;
    #[cfg(unix)]
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn normalize_trivial_drift_rewrites_timestamp_keys() {
        let mut v = json!({
            "result": "ok",
            "timestamp": 1_715_000_000_000_i64,
            "nested": { "started_at": "2026-05-01T12:00:00Z" },
        });
        normalize(&mut v, &default_normalization_rules(), HarnessMode::Ci);
        assert_eq!(v["timestamp"], json!(tokens::TS));
        assert_eq!(v["nested"]["started_at"], json!(tokens::TS));
        assert_eq!(v["result"], json!("ok"));
    }

    #[test]
    fn normalize_host_state_mode_skips_operational_layer() {
        let mut v = json!({
            "timestamp": 100,
            "cwd": "/runners/cwd",
        });
        normalize(
            &mut v,
            &default_normalization_rules(),
            HarnessMode::HostState,
        );
        assert_eq!(v["timestamp"], json!(tokens::TS));
        // cwd is Layer 2 — preserved in HostState mode.
        assert_eq!(v["cwd"], json!("/runners/cwd"));
    }

    #[test]
    fn normalize_ci_mode_rewrites_both_layers() {
        let mut v = json!({"timestamp": 100, "cwd": "/runners/cwd"});
        normalize(&mut v, &default_normalization_rules(), HarnessMode::Ci);
        assert_eq!(v["timestamp"], json!(tokens::TS));
        assert_eq!(v["cwd"], json!(tokens::CWD));
    }

    #[test]
    fn divergence_report_match_for_post_normalize_equal() {
        let invoker = MockNtmInvoker::new().with_response(
            "profile",
            "show",
            json!({"name": "default", "timestamp": 999}),
        );
        let harness = DifferentialHarness::new(
            "profile",
            "show",
            |_req: &Value| Ok(json!({"name": "default", "timestamp": 1})),
            &invoker,
        );
        let report = harness.compare(&json!({"name": "default"})).unwrap();
        assert!(
            report.is_match(),
            "post-normalize equality should match: {report:?}"
        );
    }

    #[test]
    fn divergence_report_diverge_on_real_field() {
        let invoker =
            MockNtmInvoker::new().with_response("profile", "show", json!({"name": "default-A"}));
        let harness = DifferentialHarness::new(
            "profile",
            "show",
            |_req: &Value| Ok(json!({"name": "default-B"})),
            &invoker,
        );
        let report = harness.compare(&json!({})).unwrap();
        assert!(!report.is_match());
        if let DivergenceReport::Diverge { explanation, .. } = report {
            assert!(
                explanation.contains("/name"),
                "explanation should point at /name: {explanation}"
            );
        } else {
            panic!("expected Diverge, got Match");
        }
    }

    #[test]
    fn first_divergence_reports_array_length_mismatch() {
        let a = json!([1, 2, 3]);
        let b = json!([1, 2]);
        let exp = first_divergence(&a, &b, "");
        assert!(exp.contains("array length differs"));
    }

    #[test]
    fn first_divergence_reports_missing_field_path() {
        let a = json!({"a": {"b": 1}});
        let b = json!({"a": {}});
        let exp = first_divergence(&a, &b, "");
        assert!(exp.contains("missing in ntm: /a/b"), "got: {exp}");
    }

    #[test]
    fn first_divergence_escapes_json_pointer_segments() {
        let a = json!({"a/b": {"c~d": 1}});
        let b = json!({"a/b": {"c~d": 2}});
        let exp = first_divergence(&a, &b, "");
        assert!(
            exp.contains("/a~1b/c~0d"),
            "JSON pointer must escape `/` and `~`: {exp}"
        );
    }

    #[test]
    fn mock_invoker_returns_registered_response() {
        let invoker =
            MockNtmInvoker::new().with_response("checkpoint", "save", json!({"ok": true}));
        let resp = invoker.invoke("checkpoint", "save", &json!({})).unwrap();
        assert_eq!(resp, json!({"ok": true}));
    }

    #[test]
    fn mock_invoker_errors_on_unregistered() {
        let invoker = MockNtmInvoker::new();
        let err = invoker
            .invoke("checkpoint", "save", &json!({}))
            .unwrap_err();
        assert!(err.contains("no response registered"));
    }

    #[test]
    fn ntm_command_mapping_strips_owned_binary_token() {
        let command = NtmSubprocessCommand::try_from_ntm_command(
            "ntm profiles show",
            NtmRequestEncoding::JsonStdin,
        )
        .expect("valid ntm command mapping");

        assert_eq!(command.args, ["profiles", "show"]);
        assert_eq!(command.request_encoding, NtmRequestEncoding::JsonStdin);
    }

    #[test]
    fn ntm_command_mapping_rejects_non_ntm_binary() {
        let err = NtmSubprocessCommand::try_from_ntm_command(
            "ft robot profile show",
            NtmRequestEncoding::JsonStdin,
        )
        .expect_err("non-ntm binary should be rejected");

        assert!(err.contains("must start with `ntm`"), "{err}");
    }

    #[test]
    fn robot_command_mapping_rejects_family_without_ntm_equivalent() {
        let command = RobotNtmCommand::Profile(ProfileCommand::Validate(ProfileValidateRequest {
            name: "default".to_string(),
        }));

        let result = NtmSubprocessInvoker::ntm()
            .try_with_robot_command(&command, NtmRequestEncoding::JsonStdin);
        let err = match result {
            Ok(_) => panic!("profile.validate has no ntm equivalent and must not register"),
            Err(err) => err,
        };

        assert!(
            err.contains("no ntm equivalent command declared for profile.validate"),
            "{err}"
        );
    }

    #[cfg(unix)]
    fn fake_ntm(contents: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ntm-fake");
        fs::write(&path, contents).expect("write fake ntm");
        let mut perms = fs::metadata(&path).expect("metadata").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).expect("chmod fake ntm");
        (dir, path)
    }

    #[cfg(unix)]
    #[test]
    fn subprocess_invoker_returns_successful_json_response() {
        let (_dir, binary) = fake_ntm(
            r#"#!/bin/sh
if [ "$1" != "profile" ] || [ "$2" != "show" ] || [ "$3" != "--json" ]; then
  echo "unexpected args: $*" >&2
  exit 64
fi
cat >/dev/null
printf '{"ok":true,"data":{"name":"default","timestamp":123}}'
"#,
        );
        let invoker = NtmSubprocessInvoker::new(binary).with_command(
            "profile",
            "show",
            NtmSubprocessCommand::json_stdin(["profile", "show", "--json"]),
        );

        let response = invoker
            .invoke("profile", "show", &json!({"name": "default"}))
            .expect("subprocess response");

        assert_eq!(response["ok"], json!(true));
        assert_eq!(response["data"]["name"], json!("default"));
    }

    #[cfg(unix)]
    #[test]
    fn subprocess_invoker_registers_robot_family_mapping() {
        let (_dir, binary) = fake_ntm(
            r#"#!/bin/sh
if [ "$1" != "profiles" ] || [ "$2" != "show" ]; then
  echo "unexpected args: $*" >&2
  exit 64
fi
payload=$(cat)
case "$payload" in
  *default*) ;;
  *)
    echo "missing profile request payload: $payload" >&2
    exit 65
    ;;
esac
printf '{"ok":true,"data":{"name":"default","timestamp":123}}'
"#,
        );
        let command = RobotNtmCommand::Profile(ProfileCommand::Show(ProfileShowRequest {
            name: "default".to_string(),
        }));
        let invoker = NtmSubprocessInvoker::new(binary)
            .try_with_robot_command(&command, NtmRequestEncoding::JsonStdin)
            .expect("profile.show should map to its first ntm equivalent");

        let response = invoker
            .invoke("profile", "show", &json!({"name": "default"}))
            .expect("subprocess response");

        assert_eq!(response["ok"], json!(true));
        assert_eq!(response["data"]["name"], json!("default"));
    }

    #[cfg(unix)]
    #[test]
    fn subprocess_invoker_reports_missing_binary() {
        let invoker = NtmSubprocessInvoker::new("/definitely/not/ntm").with_command(
            "profile",
            "list",
            NtmSubprocessCommand::no_request(["profiles", "list", "--json"]),
        );

        let err = invoker
            .invoke("profile", "list", &json!({}))
            .expect_err("missing binary should fail");

        assert!(err.contains("failed to spawn"), "{err}");
        assert!(err.contains("profile.list"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn subprocess_invoker_reports_nonzero_exit_with_stderr() {
        let (_dir, binary) = fake_ntm(
            r#"#!/bin/sh
echo "profile not found" >&2
exit 7
"#,
        );
        let invoker = NtmSubprocessInvoker::new(binary).with_command(
            "profile",
            "show",
            NtmSubprocessCommand::json_arg(["profiles", "show", "--json"]),
        );

        let err = invoker
            .invoke("profile", "show", &json!({"name": "missing"}))
            .expect_err("nonzero exit should fail");

        assert!(err.contains("profile.show"), "{err}");
        assert!(err.contains("profile not found"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn subprocess_invoker_reports_invalid_json() {
        let (_dir, binary) = fake_ntm(
            r#"#!/bin/sh
printf 'not-json'
"#,
        );
        let invoker = NtmSubprocessInvoker::new(binary).with_command(
            "profile",
            "list",
            NtmSubprocessCommand::no_request(["profiles", "list", "--json"]),
        );

        let err = invoker
            .invoke("profile", "list", &json!({}))
            .expect_err("invalid json should fail");

        assert!(err.contains("invalid JSON"), "{err}");
        assert!(err.contains("not-json"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn subprocess_invoker_times_out() {
        let (_dir, binary) = fake_ntm(
            r#"#!/bin/sh
sleep 2
printf '{"ok":true}'
"#,
        );
        let invoker = NtmSubprocessInvoker::new(binary)
            .with_timeout(Duration::from_millis(20))
            .with_command(
                "profile",
                "list",
                NtmSubprocessCommand::no_request(["profiles", "list", "--json"]),
            );

        let err = invoker
            .invoke("profile", "list", &json!({}))
            .expect_err("timeout should fail");

        assert!(err.contains("timed out"), "{err}");
        assert!(err.contains("profile.list"), "{err}");
    }

    #[test]
    fn subprocess_invoker_requires_explicit_action_mapping() {
        let invoker = NtmSubprocessInvoker::ntm();
        let err = invoker
            .invoke("profile", "list", &json!({}))
            .expect_err("missing mapping should fail");

        assert!(
            err.contains("no ntm subprocess command registered"),
            "{err}"
        );
    }
}
