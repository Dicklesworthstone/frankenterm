#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use frankenterm_core::rch_admission::{
    RchAdmissionAgentMailDiagnostic, RchAdmissionCargoCommandAnalysis, RchAdmissionCollectorInput,
    RchAdmissionCollectorObservation, RchAdmissionCommandDiagnostic,
    RchAdmissionLocalDiskDiagnostic, RchAdmissionProbeDiagnostic, RchAdmissionProofStatus,
    RchAdmissionQueueDiagnostic, RchAdmissionReasonCode, RchAdmissionSeverity,
    RchAdmissionWorkerRejection, analyze_rch_admission_cargo_command, build_rch_admission_report,
};
use libfuzzer_sys::fuzz_target;

const MAX_DATA_BYTES: usize = 16 * 1024;
const MAX_TEXT_CHARS: usize = 512;
const MAX_ENV_PAIRS: usize = 8;
const MAX_ITEMS: usize = 8;
const GENERATED_AT_MS: u64 = 1_779_105_000_000;

#[derive(Arbitrary, Debug)]
struct FuzzInput<'a> {
    command: CommandInput<'a>,
    env: Vec<EnvPair>,
    installed_selector_slots: SlotEstimate,
    observation_error: ErrorCategoryInput<'a>,
    reason: ReasonInput,
    queue: QueueInput,
    local_disk: LocalDiskInput,
    agent_mail: AgentMailInput,
}

#[derive(Arbitrary, Debug)]
enum CommandInput<'a> {
    Raw(&'a [u8]),
    Cargo {
        subcommand: CargoSubcommand,
        jobs: JobsInput,
        target_dir: OptionalText<'a>,
        packages: Vec<TextInput<'a>>,
        excludes: Vec<TextInput<'a>>,
        test_filters: Vec<TextInput<'a>>,
        libtest_args: Vec<TextInput<'a>>,
        malformed_quote: bool,
    },
}

#[derive(Arbitrary, Debug)]
enum CargoSubcommand {
    Test,
    Check,
    Clippy,
    Build,
    Bench,
    Fuzz,
    Other,
    Missing,
}

#[derive(Arbitrary, Debug)]
enum JobsInput {
    None,
    ShortAttached(u16),
    ShortSeparated(u16),
    LongAttached(u16),
    LongSeparated(u16),
}

#[derive(Arbitrary, Debug)]
enum SlotEstimate {
    None,
    Some(u16),
}

#[derive(Arbitrary, Debug)]
enum OptionalText<'a> {
    None,
    Some(TextInput<'a>),
}

#[derive(Arbitrary, Debug)]
enum TextInput<'a> {
    Bytes(&'a [u8]),
    Text(String),
}

#[derive(Arbitrary, Debug)]
struct EnvPair {
    key: EnvKey,
    value: String,
}

#[derive(Arbitrary, Debug)]
enum EnvKey {
    CargoBuildJobs,
    CargoTargetDir,
    Other(String),
}

#[derive(Arbitrary, Debug)]
enum ErrorCategoryInput<'a> {
    None,
    Known(ReasonInput),
    Raw(&'a [u8]),
}

#[derive(Arbitrary, Debug, Clone, Copy)]
enum ReasonInput {
    LocalEnoSpace,
    NoAdmissibleWorkers,
    CriticalPressure,
    TelemetryGap,
    InsufficientSlots,
    ActiveProjectExclusion,
    SpeedscoreResponseShape,
    DryRunInconsistentWorker,
    Unknown,
}

#[derive(Arbitrary, Debug)]
struct QueueInput {
    active_project_exclusion: bool,
    active_builds: u8,
    queued_builds: u8,
    workers_healthy: OptionalU8,
    workers_total: OptionalU8,
}

#[derive(Arbitrary, Debug)]
struct LocalDiskInput {
    repo_enospc: bool,
    rch_cache_enospc: bool,
}

#[derive(Arbitrary, Debug)]
struct AgentMailInput {
    db_open: OptionalBool,
    api_reachable: OptionalBool,
}

#[derive(Arbitrary, Debug)]
enum OptionalU8 {
    None,
    Some(u8),
}

#[derive(Arbitrary, Debug)]
enum OptionalBool {
    None,
    Some(bool),
}

impl<'a> FuzzInput<'a> {
    fn run(self) {
        let raw_command = self.command.into_command();
        let env = self.env.into_iter().take(MAX_ENV_PAIRS).map(EnvPair::pair);
        let analysis = analyze_rch_admission_cargo_command(
            &raw_command,
            env,
            self.installed_selector_slots.value(),
        );

        assert_analysis_invariants(&analysis);

        let mut input = RchAdmissionCollectorInput::new(
            GENERATED_AT_MS,
            "fuzz.rch_admission_cargo",
            RchAdmissionCommandDiagnostic::new(raw_command),
        )
        .with_cargo_command_analysis(&analysis)
        .with_local_disk(self.local_disk.diagnostic())
        .with_agent_mail(self.agent_mail.diagnostic())
        .with_rch_queue(self.queue.diagnostic())
        .with_collector_observation(
            self.observation_error
                .observation()
                .unwrap_or_else(|| analysis.collector_observation()),
        );

        input = input.with_worker_rejection(RchAdmissionWorkerRejection::new(
            Some("fuzz-worker"),
            self.reason.reason_code(),
            self.reason.error_category(),
            severity_for(self.reason.reason_code()),
        ));

        let report = build_rch_admission_report(&input);
        assert_report_invariants(&report, &analysis);
        assert_reason_contract(self.reason.reason_code(), report.proof_status);
    }
}

impl<'a> CommandInput<'a> {
    fn into_command(self) -> String {
        match self {
            Self::Raw(bytes) => limited_lossy(bytes),
            Self::Cargo {
                subcommand,
                jobs,
                target_dir,
                packages,
                excludes,
                test_filters,
                libtest_args,
                malformed_quote,
            } => {
                let mut parts = vec!["cargo".to_string()];
                if let Some(subcommand) = subcommand.as_str() {
                    parts.push(subcommand.to_string());
                }
                jobs.push_to(&mut parts);
                if let Some(target_dir) = target_dir.value("target-rch") {
                    parts.push("--target-dir".to_string());
                    parts.push(target_dir);
                }
                for package in packages.into_iter().take(MAX_ITEMS) {
                    parts.push("-p".to_string());
                    parts.push(package.value("frankenterm-core"));
                }
                for (idx, exclude) in excludes.into_iter().take(MAX_ITEMS).enumerate() {
                    let value = exclude.value("frankenterm-gui");
                    if idx % 2 == 0 {
                        parts.push("--exclude".to_string());
                        parts.push(value);
                    } else {
                        parts.push(format!("--exclude={value}"));
                    }
                }
                if subcommand.matches_test() {
                    for test_filter in test_filters.into_iter().take(MAX_ITEMS) {
                        parts.push(test_filter.value("rch_admission"));
                    }
                }
                if !libtest_args.is_empty() {
                    parts.push("--".to_string());
                    for arg in libtest_args.into_iter().take(MAX_ITEMS) {
                        parts.push(arg.value("--nocapture"));
                    }
                }
                let mut command = parts.join(" ");
                if malformed_quote {
                    command.push_str(" \"unterminated");
                }
                command
            }
        }
    }
}

impl CargoSubcommand {
    fn as_str(&self) -> Option<&'static str> {
        match self {
            Self::Test => Some("test"),
            Self::Check => Some("check"),
            Self::Clippy => Some("clippy"),
            Self::Build => Some("build"),
            Self::Bench => Some("bench"),
            Self::Fuzz => Some("fuzz"),
            Self::Other => Some("metadata"),
            Self::Missing => None,
        }
    }

    fn matches_test(&self) -> bool {
        matches!(self, Self::Test)
    }
}

impl JobsInput {
    fn push_to(self, parts: &mut Vec<String>) {
        match self {
            Self::None => {}
            Self::ShortAttached(value) => parts.push(format!("-j{}", positive_slot(value))),
            Self::ShortSeparated(value) => {
                parts.push("-j".to_string());
                parts.push(positive_slot(value).to_string());
            }
            Self::LongAttached(value) => parts.push(format!("--jobs={}", positive_slot(value))),
            Self::LongSeparated(value) => {
                parts.push("--jobs".to_string());
                parts.push(positive_slot(value).to_string());
            }
        }
    }
}

impl SlotEstimate {
    fn value(self) -> Option<u32> {
        match self {
            Self::None => None,
            Self::Some(value) => Some(positive_slot(value)),
        }
    }
}

impl<'a> OptionalText<'a> {
    fn value(self, fallback: &str) -> Option<String> {
        match self {
            Self::None => None,
            Self::Some(value) => Some(value.value(fallback)),
        }
    }
}

impl TextInput<'_> {
    fn value(self, fallback: &str) -> String {
        let value = match self {
            Self::Bytes(bytes) => limited_lossy(bytes),
            Self::Text(text) => limited_text(text),
        };
        if value.trim().is_empty() {
            fallback.to_string()
        } else {
            shell_word(value)
        }
    }
}

impl EnvPair {
    fn pair(self) -> (String, String) {
        let key = match self.key {
            EnvKey::CargoBuildJobs => "CARGO_BUILD_JOBS".to_string(),
            EnvKey::CargoTargetDir => "CARGO_TARGET_DIR".to_string(),
            EnvKey::Other(key) => limited_text(key),
        };
        let value = limited_text(self.value);
        (
            if key.trim().is_empty() {
                "IGNORED_ENV".to_string()
            } else {
                key
            },
            if value.trim().is_empty() {
                "1".to_string()
            } else {
                value
            },
        )
    }
}

impl ErrorCategoryInput<'_> {
    fn observation(self) -> Option<RchAdmissionCollectorObservation> {
        let error_category = match self {
            Self::None => return None,
            Self::Known(reason) => reason.error_category(),
            Self::Raw(bytes) => limited_lossy(bytes),
        };
        Some(
            RchAdmissionCollectorObservation::new(
                "fuzz.collector",
                "fuzz generated collector category",
                "collector category normalization fuzz input",
            )
            .error_category(if error_category.trim().is_empty() {
                "unknown".to_string()
            } else {
                error_category
            }),
        )
    }
}

impl ReasonInput {
    fn reason_code(self) -> RchAdmissionReasonCode {
        match self {
            Self::LocalEnoSpace => RchAdmissionReasonCode::LocalEnoSpace,
            Self::NoAdmissibleWorkers => RchAdmissionReasonCode::NoAdmissibleWorkers,
            Self::CriticalPressure => RchAdmissionReasonCode::CriticalPressure,
            Self::TelemetryGap => RchAdmissionReasonCode::TelemetryGap,
            Self::InsufficientSlots => RchAdmissionReasonCode::InsufficientSlots,
            Self::ActiveProjectExclusion => RchAdmissionReasonCode::ActiveProjectExclusion,
            Self::SpeedscoreResponseShape => RchAdmissionReasonCode::SpeedscoreResponseShape,
            Self::DryRunInconsistentWorker => RchAdmissionReasonCode::DryRunInconsistentWorker,
            Self::Unknown => RchAdmissionReasonCode::Unknown,
        }
    }

    fn error_category(self) -> String {
        match self {
            Self::LocalEnoSpace => "ENOSPC: no space left on device",
            Self::NoAdmissibleWorkers => "no_admissible_workers worker=null",
            Self::CriticalPressure => "critical_pressure=5",
            Self::TelemetryGap => "telemetry_gap stale_telemetry",
            Self::InsufficientSlots => "insufficient_slots",
            Self::ActiveProjectExclusion => "active_project_exclusion",
            Self::SpeedscoreResponseShape => "speedscore response_shape",
            Self::DryRunInconsistentWorker => "dry_run inconsistent worker",
            Self::Unknown => "unclassified",
        }
        .to_string()
    }
}

impl QueueInput {
    fn diagnostic(self) -> RchAdmissionQueueDiagnostic {
        let workers_healthy = self.workers_healthy.value();
        let workers_total = self.workers_total.value();
        RchAdmissionQueueDiagnostic {
            posture: Some("fuzz".to_string()),
            active_project_exclusion: self.active_project_exclusion,
            active_builds: u32::from(self.active_builds),
            queued_builds: u32::from(self.queued_builds),
            workers_healthy,
            workers_total: workers_total.map(|total| total.max(workers_healthy.unwrap_or(0))),
        }
    }
}

impl LocalDiskInput {
    fn diagnostic(self) -> RchAdmissionLocalDiskDiagnostic {
        RchAdmissionLocalDiskDiagnostic {
            system_data_free_bytes: Some(1024 * 1024 * 1024),
            private_tmp_free_bytes: Some(1024 * 1024 * 1024),
            repo_write_probe: if self.repo_enospc {
                RchAdmissionProbeDiagnostic::failed("ENOSPC", "No space left on device")
            } else {
                RchAdmissionProbeDiagnostic::ok("repo write probe ok")
            },
            rch_cache_write_probe: if self.rch_cache_enospc {
                RchAdmissionProbeDiagnostic::failed(
                    "config.cache.write_failed",
                    "RCH cache write failed",
                )
            } else {
                RchAdmissionProbeDiagnostic::ok("RCH cache write probe ok")
            },
        }
    }
}

impl AgentMailInput {
    fn diagnostic(self) -> RchAdmissionAgentMailDiagnostic {
        RchAdmissionAgentMailDiagnostic {
            db_open: self.db_open.value(),
            api_reachable: self.api_reachable.value(),
            reservation_conflicts: Vec::new(),
        }
    }
}

impl OptionalU8 {
    fn value(self) -> Option<u32> {
        match self {
            Self::None => None,
            Self::Some(value) => Some(u32::from(value)),
        }
    }
}

impl OptionalBool {
    fn value(self) -> Option<bool> {
        match self {
            Self::None => None,
            Self::Some(value) => Some(value),
        }
    }
}

fn assert_analysis_invariants(analysis: &RchAdmissionCargoCommandAnalysis) {
    assert!(!analysis.raw.is_empty());
    assert!(!analysis.normalized.is_empty());
    assert!(!analysis.classification.is_empty());
    assert!(analysis.effective_jobs >= 1);
    assert!(analysis.estimated_slots >= 1);
    assert!(!analysis.explanation.is_empty());
    let diagnostic = analysis.command_diagnostic();
    assert!(!diagnostic.raw.is_empty());
    assert!(
        diagnostic
            .classification
            .as_deref()
            .is_some_and(|classification| !classification.is_empty())
    );
}

fn assert_report_invariants(
    report: &frankenterm_core::rch_admission::RchAdmissionReport,
    analysis: &RchAdmissionCargoCommandAnalysis,
) {
    assert_eq!(report.schema_version, 1);
    assert!(!report.contract_id.is_empty());
    assert!(!report.command.raw.is_empty());
    assert!(!report.reason_codes.is_empty());
    assert!(!report.recommendations.is_empty());
    assert!(!report.forbidden_actions.is_empty());
    assert!(report.estimated_slots.unwrap_or(1) >= 1);
    if let Some(cargo_jobs) = report.cargo_jobs {
        assert!(cargo_jobs >= 1);
    }
    if analysis.would_intercept {
        assert_eq!(report.command.would_intercept, Some(true));
    }
}

fn assert_reason_contract(
    reason_code: RchAdmissionReasonCode,
    proof_status: RchAdmissionProofStatus,
) {
    assert_reason_only_contract(reason_code);

    if reason_code.blocks_proof() {
        assert_eq!(proof_status, RchAdmissionProofStatus::Blocked);
    }
    if matches!(
        reason_code,
        RchAdmissionReasonCode::TelemetryGap
            | RchAdmissionReasonCode::SpeedscoreResponseShape
            | RchAdmissionReasonCode::Unknown
    ) {
        assert_ne!(proof_status, RchAdmissionProofStatus::Runnable);
    }
}

fn assert_reason_only_contract(reason_code: RchAdmissionReasonCode) {
    let report = build_rch_admission_report(
        &RchAdmissionCollectorInput::new(
            GENERATED_AT_MS,
            "fuzz.rch_admission_cargo.reason_only",
            RchAdmissionCommandDiagnostic::new("cargo test").classification("cargo_test"),
        )
        .with_worker_rejection(RchAdmissionWorkerRejection::new(
            Some("reason-worker"),
            reason_code,
            format!("reason={reason_code:?}"),
            severity_for(reason_code),
        )),
    );

    if reason_code.blocks_proof() {
        assert_eq!(report.proof_status, RchAdmissionProofStatus::Blocked);
    } else if matches!(
        reason_code,
        RchAdmissionReasonCode::TelemetryGap
            | RchAdmissionReasonCode::SpeedscoreResponseShape
            | RchAdmissionReasonCode::Unknown
    ) {
        assert_eq!(report.proof_status, RchAdmissionProofStatus::Unknown);
    }
}

fn assert_seeded_regressions() {
    let analysis = analyze_rch_admission_cargo_command(
        "cargo test --workspace --exclude frankenterm-core --exclude=frankenterm-gui rch_admission --all-targets",
        std::iter::empty::<(&str, &str)>(),
        Some(2),
    );
    assert_eq!(analysis.classification, "cargo_test");
    assert!(analysis.package_scope.is_empty());
    assert_eq!(analysis.test_scope, vec![String::from("rch_admission")]);
}

fn severity_for(reason_code: RchAdmissionReasonCode) -> RchAdmissionSeverity {
    if reason_code.blocks_proof() {
        RchAdmissionSeverity::Blocked
    } else {
        RchAdmissionSeverity::Warning
    }
}

fn positive_slot(value: u16) -> u32 {
    u32::from(value).max(1)
}

fn limited_lossy(bytes: &[u8]) -> String {
    let capped = &bytes[..bytes.len().min(MAX_TEXT_CHARS)];
    let value = String::from_utf8_lossy(capped).into_owned();
    if value.trim().is_empty() {
        "cargo test".to_string()
    } else {
        value
    }
}

fn limited_text(value: String) -> String {
    value.chars().take(MAX_TEXT_CHARS).collect()
}

fn shell_word(value: String) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '/' | '.' | ':'))
    {
        value
    } else {
        format!("'{}'", value.replace('\'', ""))
    }
}

fn run_raw_command(data: &[u8]) {
    let raw = limited_lossy(data);
    let analysis =
        analyze_rch_admission_cargo_command(&raw, std::iter::empty::<(&str, &str)>(), None);
    assert_analysis_invariants(&analysis);
    let report = build_rch_admission_report(
        &RchAdmissionCollectorInput::new(
            GENERATED_AT_MS,
            "fuzz.rch_admission_cargo.raw",
            RchAdmissionCommandDiagnostic::new(raw),
        )
        .with_cargo_command_analysis(&analysis),
    );
    assert_report_invariants(&report, &analysis);
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_DATA_BYTES {
        return;
    }

    run_raw_command(data);
    assert_seeded_regressions();

    let mut unstructured = Unstructured::new(data);
    if let Ok(input) = FuzzInput::arbitrary(&mut unstructured) {
        input.run();
    }
});
