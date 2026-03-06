//! Dynamic YAML Compilation & Injection for ARS reflexes.
//!
//! Compiles a generalized command template, timeout decision, and evidence
//! ledger into a [`WorkflowDescriptor`] ready for execution by the workflow
//! engine.
//!
//! # Architecture
//!
//! ```text
//! GeneralizedCommand ─┐
//! TimeoutDecision ────>──→ ArsCompiler ──→ WorkflowDescriptor
//! LedgerDigest ───────┘        │
//!                              ├─ Log (snapshot context)
//!                              ├─ SendText (instantiated command)
//!                              ├─ WaitFor (success verification)
//!                              └─ Log (evidence digest)
//! ```
//!
//! # Operator Override Gate
//!
//! If an operator has locked a manual YAML for a given reflex cluster,
//! compilation aborts with `CompileError::OperatorLocked`.
//!
//! # Safety
//!
//! - Template variables are validated against safety regexes before injection.
//! - Generated descriptors always include pre-action context snapshot and
//!   post-action verification steps.
//! - Timeout is clamped to the evidence-backed range.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::ars_evidence::{EvidenceVerdict, LedgerDigest};
use crate::ars_generalize::GeneralizedCommand;
use crate::ars_timeout::TimeoutDecision;
use crate::workflows::{
    DescriptorFailureHandler, DescriptorMatcher, DescriptorStep, DescriptorTrigger,
    WorkflowDescriptor,
};

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for ARS reflex compilation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CompileConfig {
    /// Workflow schema version to emit.
    pub schema_version: u32,
    /// Whether to inject a pre-action context snapshot log step.
    pub inject_snapshot: bool,
    /// Whether to inject a post-action WaitFor verification step.
    pub inject_wait_for: bool,
    /// Whether to inject evidence digest as a trailing log step.
    pub inject_evidence_log: bool,
    /// Default success pattern for WaitFor steps (regex).
    pub default_success_pattern: String,
    /// Whether WaitFor uses regex (true) or substring (false) matching.
    pub wait_for_regex: bool,
    /// Maximum number of steps in a compiled workflow.
    pub max_steps: usize,
    /// Maximum total text length across all SendText steps.
    pub max_send_text_len: usize,
    /// Minimum timeout (ms) for WaitFor steps.
    pub min_wait_timeout_ms: u64,
    /// Maximum timeout (ms) for WaitFor steps.
    pub max_wait_timeout_ms: u64,
    /// Operator-locked reflex cluster IDs (compilation aborts for these).
    pub operator_locked: Vec<String>,
    /// Whether to append newline to SendText commands.
    pub append_newline: bool,
}

impl Default for CompileConfig {
    fn default() -> Self {
        Self {
            schema_version: 1,
            inject_snapshot: true,
            inject_wait_for: true,
            inject_evidence_log: true,
            default_success_pattern: r"\$\s*$".to_string(),
            wait_for_regex: true,
            max_steps: 20,
            max_send_text_len: 4096,
            min_wait_timeout_ms: 500,
            max_wait_timeout_ms: 120_000,
            operator_locked: Vec::new(),
            append_newline: true,
        }
    }
}

// =============================================================================
// Error types
// =============================================================================

/// Compilation error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompileError {
    /// An operator has locked a manual YAML for this cluster.
    OperatorLocked { cluster_id: String },
    /// The evidence ledger rejects this reflex.
    EvidenceRejected { reason: String },
    /// A template variable failed safety validation.
    UnsafeParameter {
        variable: String,
        value: String,
        safety_regex: String,
    },
    /// Too many steps would be generated.
    TooManySteps { count: usize, max: usize },
    /// Total SendText payload exceeds limit.
    TextTooLong { total: usize, max: usize },
    /// Missing required bindings for template variables.
    MissingBindings { missing: Vec<String> },
    /// No commands to compile.
    EmptyCommands,
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OperatorLocked { cluster_id } => {
                write!(f, "operator locked cluster: {cluster_id}")
            }
            Self::EvidenceRejected { reason } => {
                write!(f, "evidence rejected: {reason}")
            }
            Self::UnsafeParameter {
                variable,
                value,
                safety_regex,
            } => {
                write!(f, "unsafe param {variable}={value} (regex: {safety_regex})")
            }
            Self::TooManySteps { count, max } => {
                write!(f, "too many steps: {count} > {max}")
            }
            Self::TextTooLong { total, max } => {
                write!(f, "text too long: {total} > {max}")
            }
            Self::MissingBindings { missing } => {
                write!(f, "missing bindings: {}", missing.join(", "))
            }
            Self::EmptyCommands => write!(f, "no commands to compile"),
        }
    }
}

// =============================================================================
// Compilation input
// =============================================================================

/// Input for compiling an ARS reflex into a WorkflowDescriptor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompileInput {
    /// The reflex cluster identifier.
    pub cluster_id: String,
    /// Human-readable name for the reflex.
    pub reflex_name: String,
    /// Optional description.
    pub description: Option<String>,
    /// Generalized commands from the template engine.
    pub commands: Vec<GeneralizedCommand>,
    /// Timeout decision from the expected-loss calculator.
    pub timeout: TimeoutDecision,
    /// Evidence ledger digest.
    pub evidence_digest: LedgerDigest,
    /// Optional trigger configuration.
    pub trigger: Option<DescriptorTrigger>,
    /// Concrete bindings: variable_name → value.
    /// Used to instantiate the template at compile time (if known).
    /// If empty, templates are left as-is for runtime substitution.
    pub bindings: HashMap<String, String>,
    /// Optional custom success pattern (overrides config default).
    pub success_pattern: Option<String>,
}

// =============================================================================
// Compilation output
// =============================================================================

/// Result of a successful compilation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompileOutput {
    /// The compiled workflow descriptor.
    pub descriptor: WorkflowDescriptor,
    /// Number of steps in the compiled workflow.
    pub step_count: usize,
    /// Whether template variables were instantiated or left as templates.
    pub is_instantiated: bool,
    /// Evidence verdict that was considered.
    pub evidence_verdict: EvidenceVerdict,
    /// The effective timeout applied (ms).
    pub effective_timeout_ms: u64,
    /// Warnings generated during compilation.
    pub warnings: Vec<String>,
}

// =============================================================================
// Compiler
// =============================================================================

/// Compiles ARS reflex definitions into executable WorkflowDescriptors.
#[derive(Debug, Clone)]
pub struct ArsCompiler {
    config: CompileConfig,
}

impl ArsCompiler {
    /// Create a compiler with the given configuration.
    pub fn new(config: CompileConfig) -> Self {
        Self { config }
    }

    /// Create a compiler with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(CompileConfig::default())
    }

    /// Get the configuration.
    pub fn config(&self) -> &CompileConfig {
        &self.config
    }

    /// Compile a reflex into a WorkflowDescriptor.
    pub fn compile(&self, input: &CompileInput) -> Result<CompileOutput, CompileError> {
        debug!(cluster_id = %input.cluster_id, "compiling ARS reflex");

        // Gate: operator lock.
        if self
            .config
            .operator_locked
            .iter()
            .any(|id| id == &input.cluster_id)
        {
            warn!(cluster_id = %input.cluster_id, "operator locked");
            return Err(CompileError::OperatorLocked {
                cluster_id: input.cluster_id.clone(),
            });
        }

        // Gate: evidence rejection.
        if input.evidence_digest.overall_verdict == EvidenceVerdict::Reject {
            return Err(CompileError::EvidenceRejected {
                reason: format!(
                    "ledger verdict is Reject ({} entries, complete={})",
                    input.evidence_digest.entry_count, input.evidence_digest.is_complete,
                ),
            });
        }

        // Gate: empty commands.
        if input.commands.is_empty() {
            return Err(CompileError::EmptyCommands);
        }

        // Validate bindings against safety regexes.
        let mut warnings = Vec::new();
        if !input.bindings.is_empty() {
            self.validate_bindings(&input.commands, &input.bindings)?;
        }

        // Build steps.
        let mut steps = Vec::new();
        let mut step_idx = 0u32;

        // Step: pre-action context snapshot (log).
        if self.config.inject_snapshot {
            steps.push(DescriptorStep::Log {
                id: format!("ars_snapshot_{step_idx}"),
                description: Some("ARS pre-action context snapshot".to_string()),
                message: format!(
                    "ARS reflex '{}' activating (cluster={}, evidence_entries={}, verdict={:?})",
                    input.reflex_name,
                    input.cluster_id,
                    input.evidence_digest.entry_count,
                    input.evidence_digest.overall_verdict,
                ),
            });
            step_idx += 1;
        }

        // Steps: SendText for each command.
        let effective_timeout = self.clamp_timeout(input.timeout.timeout_ms);
        let is_instantiated = !input.bindings.is_empty();

        for cmd in &input.commands {
            let text = if is_instantiated {
                let mut instantiated = cmd.template.clone();
                for var in &cmd.variables {
                    if let Some(val) = input.bindings.get(&var.name) {
                        instantiated = instantiated.replace(&var.placeholder, val);
                    }
                }
                instantiated
            } else {
                cmd.template.clone()
            };

            let send_text = if self.config.append_newline && !text.ends_with('\n') {
                format!("{text}\n")
            } else {
                text
            };

            steps.push(DescriptorStep::SendText {
                id: format!("ars_cmd_{step_idx}"),
                description: Some(format!("ARS command (block {})", cmd.block_index)),
                text: send_text,
                wait_for: None,
                wait_timeout_ms: None,
            });
            step_idx += 1;
        }

        // Step: post-action WaitFor verification.
        if self.config.inject_wait_for {
            let pattern = input
                .success_pattern
                .as_deref()
                .unwrap_or(&self.config.default_success_pattern);

            let matcher = if self.config.wait_for_regex {
                DescriptorMatcher::Regex {
                    pattern: pattern.to_string(),
                }
            } else {
                DescriptorMatcher::Substring {
                    value: pattern.to_string(),
                }
            };

            steps.push(DescriptorStep::WaitFor {
                id: format!("ars_verify_{step_idx}"),
                description: Some("ARS post-action verification".to_string()),
                matcher,
                timeout_ms: Some(effective_timeout),
            });
            step_idx += 1;
        }

        // Step: evidence digest log.
        if self.config.inject_evidence_log {
            let digest_summary = format!(
                "Evidence: {} entries, verdict={:?}, complete={}, hash={}",
                input.evidence_digest.entry_count,
                input.evidence_digest.overall_verdict,
                input.evidence_digest.is_complete,
                &input.evidence_digest.root_hash[..16],
            );
            steps.push(DescriptorStep::Log {
                id: format!("ars_evidence_{step_idx}"),
                description: Some("ARS evidence digest".to_string()),
                message: digest_summary,
            });
        }

        // Validate step count.
        if steps.len() > self.config.max_steps {
            return Err(CompileError::TooManySteps {
                count: steps.len(),
                max: self.config.max_steps,
            });
        }

        // Validate total text length.
        let total_text: usize = steps
            .iter()
            .map(|s| match s {
                DescriptorStep::SendText { text, .. } => text.len(),
                _ => 0,
            })
            .sum();
        if total_text > self.config.max_send_text_len {
            return Err(CompileError::TextTooLong {
                total: total_text,
                max: self.config.max_send_text_len,
            });
        }

        // Check for unresolved template variables.
        if is_instantiated {
            for cmd in &input.commands {
                for var in &cmd.variables {
                    if !input.bindings.contains_key(&var.name) {
                        warnings.push(format!(
                            "unresolved template variable: {} in block {}",
                            var.name, cmd.block_index,
                        ));
                    }
                }
            }
        }

        // Build failure handler.
        let on_failure = Some(DescriptorFailureHandler::Log {
            message: format!(
                "ARS reflex '{}' failed at step ${{failed_step}} (cluster={})",
                input.reflex_name, input.cluster_id,
            ),
        });

        // Assemble descriptor.
        let descriptor = WorkflowDescriptor {
            workflow_schema_version: self.config.schema_version,
            name: format!("ars-{}", input.cluster_id),
            description: input.description.clone(),
            triggers: input
                .trigger
                .as_ref()
                .map_or_else(Vec::new, |t| vec![t.clone()]),
            steps,
            on_failure,
        };

        let step_count = descriptor.steps.len();

        Ok(CompileOutput {
            descriptor,
            step_count,
            is_instantiated,
            evidence_verdict: input.evidence_digest.overall_verdict,
            effective_timeout_ms: effective_timeout,
            warnings,
        })
    }

    /// Compile and serialize to YAML string.
    pub fn compile_to_yaml(&self, input: &CompileInput) -> Result<String, CompileError> {
        let output = self.compile(input)?;
        serde_yaml::to_string(&output.descriptor).map_err(|e| CompileError::EvidenceRejected {
            reason: format!("YAML serialization failed: {e}"),
        })
    }

    /// Validate bindings against safety regexes.
    #[allow(clippy::unused_self)]
    fn validate_bindings(
        &self,
        commands: &[GeneralizedCommand],
        bindings: &HashMap<String, String>,
    ) -> Result<(), CompileError> {
        for cmd in commands {
            for var in &cmd.variables {
                if let Some(val) = bindings.get(&var.name) {
                    if !matches_safety_regex(val, &var.safety_regex) {
                        return Err(CompileError::UnsafeParameter {
                            variable: var.name.clone(),
                            value: val.clone(),
                            safety_regex: var.safety_regex.clone(),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// Clamp timeout to configured bounds.
    fn clamp_timeout(&self, timeout_ms: u64) -> u64 {
        timeout_ms
            .max(self.config.min_wait_timeout_ms)
            .min(self.config.max_wait_timeout_ms)
    }
}

// =============================================================================
// Safety regex matching (shared with ars_generalize)
// =============================================================================

/// Check if a value matches a safety regex pattern.
fn matches_safety_regex(value: &str, pattern: &str) -> bool {
    if value.is_empty() {
        return false;
    }

    // Compile the regex. If the agent provided an invalid regex,
    // we fail closed (deny the binding) rather than failing open.
    match regex::Regex::new(pattern) {
        Ok(re) => re.is_match(value),
        Err(e) => {
            tracing::warn!("Invalid safety regex provided: '{}' ({})", pattern, e);
            false
        }
    }
}

// =============================================================================
// Helpers for building CompileInput
// =============================================================================

/// Builder for constructing CompileInput conveniently.
#[derive(Debug, Clone)]
pub struct CompileInputBuilder {
    cluster_id: String,
    reflex_name: String,
    description: Option<String>,
    commands: Vec<GeneralizedCommand>,
    timeout: Option<TimeoutDecision>,
    evidence_digest: Option<LedgerDigest>,
    trigger: Option<DescriptorTrigger>,
    bindings: HashMap<String, String>,
    success_pattern: Option<String>,
}

impl CompileInputBuilder {
    /// Create a new builder.
    pub fn new(cluster_id: impl Into<String>, reflex_name: impl Into<String>) -> Self {
        Self {
            cluster_id: cluster_id.into(),
            reflex_name: reflex_name.into(),
            description: None,
            commands: Vec::new(),
            timeout: None,
            evidence_digest: None,
            trigger: None,
            bindings: HashMap::new(),
            success_pattern: None,
        }
    }

    /// Set the description.
    #[must_use]
    pub fn description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    /// Add a generalized command.
    #[must_use]
    pub fn command(mut self, cmd: GeneralizedCommand) -> Self {
        self.commands.push(cmd);
        self
    }

    /// Set commands from a vec.
    #[must_use]
    pub fn commands(mut self, cmds: Vec<GeneralizedCommand>) -> Self {
        self.commands = cmds;
        self
    }

    /// Set the timeout decision.
    #[must_use]
    pub fn timeout(mut self, td: TimeoutDecision) -> Self {
        self.timeout = Some(td);
        self
    }

    /// Set the evidence digest.
    #[must_use]
    pub fn evidence(mut self, digest: LedgerDigest) -> Self {
        self.evidence_digest = Some(digest);
        self
    }

    /// Set the trigger.
    #[must_use]
    pub fn trigger(mut self, trigger: DescriptorTrigger) -> Self {
        self.trigger = Some(trigger);
        self
    }

    /// Add a binding.
    #[must_use]
    pub fn bind(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.bindings.insert(name.into(), value.into());
        self
    }

    /// Set bindings from a map.
    #[must_use]
    pub fn bindings(mut self, map: HashMap<String, String>) -> Self {
        self.bindings = map;
        self
    }

    /// Set custom success pattern.
    #[must_use]
    pub fn success_pattern(mut self, pattern: impl Into<String>) -> Self {
        self.success_pattern = Some(pattern.into());
        self
    }

    /// Build the CompileInput. Uses defaults for timeout and evidence if not set.
    pub fn build(self) -> CompileInput {
        let timeout = self.timeout.unwrap_or_else(default_timeout);
        let evidence_digest = self.evidence_digest.unwrap_or_else(default_digest);
        CompileInput {
            cluster_id: self.cluster_id,
            reflex_name: self.reflex_name,
            description: self.description,
            commands: self.commands,
            timeout,
            evidence_digest,
            trigger: self.trigger,
            bindings: self.bindings,
            success_pattern: self.success_pattern,
        }
    }
}

/// Create a default timeout decision for testing/fallback.
fn default_timeout() -> TimeoutDecision {
    use crate::ars_timeout::TimeoutMethod;
    TimeoutDecision {
        timeout_ms: 5000,
        raw_optimal_ms: 5000.0,
        expected_loss: 0.0,
        method: TimeoutMethod::Default,
        stats: None,
        is_data_driven: false,
    }
}

/// Create a default evidence digest for testing/fallback.
fn default_digest() -> LedgerDigest {
    LedgerDigest {
        entry_count: 0,
        overall_verdict: EvidenceVerdict::Neutral,
        categories_present: Vec::new(),
        is_complete: false,
        timestamp_range: (0, 0),
        root_hash: "0".repeat(64),
    }
}

// =============================================================================
// Convenience: quick compile for simple reflexes
// =============================================================================

/// Compile a single-command reflex with minimal configuration.
pub fn quick_compile(
    cluster_id: &str,
    command_text: &str,
    timeout_ms: u64,
    evidence_digest: &LedgerDigest,
) -> Result<CompileOutput, CompileError> {
    let cmd = GeneralizedCommand {
        original: command_text.to_string(),
        template: command_text.to_string(),
        variables: Vec::new(),
        block_index: 0,
    };

    let timeout = TimeoutDecision {
        timeout_ms,
        raw_optimal_ms: timeout_ms as f64,
        expected_loss: 0.0,
        method: crate::ars_timeout::TimeoutMethod::Default,
        stats: None,
        is_data_driven: false,
    };

    let input = CompileInput {
        cluster_id: cluster_id.to_string(),
        reflex_name: format!("reflex-{cluster_id}"),
        description: None,
        commands: vec![cmd],
        timeout,
        evidence_digest: evidence_digest.clone(),
        trigger: None,
        bindings: HashMap::new(),
        success_pattern: None,
    };

    ArsCompiler::with_defaults().compile(&input)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ars_evidence::{EvidenceCategory, EvidenceVerdict};
    use crate::ars_generalize::{ParamKind, TemplateVar};
    use crate::ars_timeout::TimeoutMethod;

    fn test_digest(verdict: EvidenceVerdict) -> LedgerDigest {
        LedgerDigest {
            entry_count: 3,
            overall_verdict: verdict,
            categories_present: vec![
                EvidenceCategory::ChangeDetection,
                EvidenceCategory::SafetyProof,
            ],
            is_complete: true,
            timestamp_range: (1000, 3000),
            root_hash: "a".repeat(64),
        }
    }

    fn test_timeout(ms: u64) -> TimeoutDecision {
        TimeoutDecision {
            timeout_ms: ms,
            raw_optimal_ms: ms as f64,
            expected_loss: 0.5,
            method: TimeoutMethod::ExpectedLoss,
            stats: None,
            is_data_driven: true,
        }
    }

    fn test_command(text: &str, idx: u32) -> GeneralizedCommand {
        GeneralizedCommand {
            original: text.to_string(),
            template: text.to_string(),
            variables: Vec::new(),
            block_index: idx,
        }
    }

    fn test_templated_command() -> GeneralizedCommand {
        GeneralizedCommand {
            original: "cargo build --manifest-path src/main.rs".to_string(),
            template: "cargo build --manifest-path {{cap.file_0}}".to_string(),
            variables: vec![TemplateVar {
                name: "file_0".to_string(),
                placeholder: "{{cap.file_0}}".to_string(),
                original: "src/main.rs".to_string(),
                kind: ParamKind::FilePath,
                safety_regex: "^[a-zA-Z0-9_./-]+$".to_string(),
            }],
            block_index: 0,
        }
    }

    fn test_input(cmds: Vec<GeneralizedCommand>) -> CompileInput {
        CompileInput {
            cluster_id: "test-001".to_string(),
            reflex_name: "test reflex".to_string(),
            description: Some("A test reflex".to_string()),
            commands: cmds,
            timeout: test_timeout(5000),
            evidence_digest: test_digest(EvidenceVerdict::Support),
            trigger: None,
            bindings: HashMap::new(),
            success_pattern: None,
        }
    }

    // ---- Basic compilation ----

    #[test]
    fn compile_single_command() {
        let compiler = ArsCompiler::with_defaults();
        let input = test_input(vec![test_command("cargo test", 0)]);
        let output = compiler.compile(&input).unwrap();

        assert!(output.step_count >= 3); // snapshot + command + verify
        assert!(!output.is_instantiated);
        assert_eq!(output.evidence_verdict, EvidenceVerdict::Support);
    }

    #[test]
    fn compile_multiple_commands() {
        let compiler = ArsCompiler::with_defaults();
        let cmds = vec![
            test_command("git stash", 0),
            test_command("git pull --rebase", 1),
            test_command("git stash pop", 2),
        ];
        let input = test_input(cmds);
        let output = compiler.compile(&input).unwrap();

        // snapshot(1) + commands(3) + verify(1) + evidence(1) = 6
        assert_eq!(output.step_count, 6);
    }

    #[test]
    fn compile_with_bindings() {
        let compiler = ArsCompiler::with_defaults();
        let cmd = test_templated_command();
        let mut input = test_input(vec![cmd]);
        input
            .bindings
            .insert("file_0".to_string(), "lib/utils.rs".to_string());

        let output = compiler.compile(&input).unwrap();
        assert!(output.is_instantiated);

        // Check the SendText step has instantiated command.
        let send_steps: Vec<_> = output
            .descriptor
            .steps
            .iter()
            .filter(|s| matches!(s, DescriptorStep::SendText { .. }))
            .collect();
        assert_eq!(send_steps.len(), 1);
        if let DescriptorStep::SendText { text, .. } = send_steps[0] {
            assert!(text.contains("lib/utils.rs"));
            assert!(!text.contains("{{cap.file_0}}"));
        }
    }

    #[test]
    fn compile_without_bindings_preserves_template() {
        let compiler = ArsCompiler::with_defaults();
        let cmd = test_templated_command();
        let input = test_input(vec![cmd]);

        let output = compiler.compile(&input).unwrap();
        assert!(!output.is_instantiated);

        let send_steps: Vec<_> = output
            .descriptor
            .steps
            .iter()
            .filter(|s| matches!(s, DescriptorStep::SendText { .. }))
            .collect();
        if let DescriptorStep::SendText { text, .. } = send_steps[0] {
            assert!(text.contains("{{cap.file_0}}"));
        }
    }

    // ---- Descriptor structure ----

    #[test]
    fn descriptor_has_correct_schema_version() {
        let compiler = ArsCompiler::with_defaults();
        let input = test_input(vec![test_command("echo ok", 0)]);
        let output = compiler.compile(&input).unwrap();
        assert_eq!(output.descriptor.workflow_schema_version, 1);
    }

    #[test]
    fn descriptor_name_includes_cluster_id() {
        let compiler = ArsCompiler::with_defaults();
        let input = test_input(vec![test_command("echo ok", 0)]);
        let output = compiler.compile(&input).unwrap();
        assert!(output.descriptor.name.contains("test-001"));
    }

    #[test]
    fn descriptor_has_failure_handler() {
        let compiler = ArsCompiler::with_defaults();
        let input = test_input(vec![test_command("echo ok", 0)]);
        let output = compiler.compile(&input).unwrap();
        assert!(output.descriptor.on_failure.is_some());
    }

    #[test]
    fn descriptor_first_step_is_snapshot_log() {
        let compiler = ArsCompiler::with_defaults();
        let input = test_input(vec![test_command("echo ok", 0)]);
        let output = compiler.compile(&input).unwrap();
        let is_log = matches!(&output.descriptor.steps[0], DescriptorStep::Log { id, .. } if id.starts_with("ars_snapshot"));
        assert!(is_log);
    }

    #[test]
    fn descriptor_last_step_is_evidence_log() {
        let compiler = ArsCompiler::with_defaults();
        let input = test_input(vec![test_command("echo ok", 0)]);
        let output = compiler.compile(&input).unwrap();
        let last = output.descriptor.steps.last().unwrap();
        let is_evidence =
            matches!(last, DescriptorStep::Log { id, .. } if id.starts_with("ars_evidence"));
        assert!(is_evidence);
    }

    #[test]
    fn descriptor_has_wait_for_step() {
        let compiler = ArsCompiler::with_defaults();
        let input = test_input(vec![test_command("echo ok", 0)]);
        let output = compiler.compile(&input).unwrap();
        let wait_count = output
            .descriptor
            .steps
            .iter()
            .filter(|s| matches!(s, DescriptorStep::WaitFor { .. }))
            .count();
        assert_eq!(wait_count, 1);
    }

    #[test]
    fn wait_for_has_correct_timeout() {
        let compiler = ArsCompiler::with_defaults();
        let input = test_input(vec![test_command("echo ok", 0)]);
        let output = compiler.compile(&input).unwrap();
        for step in &output.descriptor.steps {
            if let DescriptorStep::WaitFor { timeout_ms, .. } = step {
                assert_eq!(*timeout_ms, Some(5000));
            }
        }
    }

    // ---- Error gates ----

    #[test]
    fn rejects_operator_locked_cluster() {
        let config = CompileConfig {
            operator_locked: vec!["locked-001".to_string()],
            ..Default::default()
        };
        let compiler = ArsCompiler::new(config);
        let mut input = test_input(vec![test_command("echo ok", 0)]);
        input.cluster_id = "locked-001".to_string();

        let err = compiler.compile(&input).unwrap_err();
        assert_eq!(
            err,
            CompileError::OperatorLocked {
                cluster_id: "locked-001".to_string()
            }
        );
    }

    #[test]
    fn rejects_evidence_rejected() {
        let compiler = ArsCompiler::with_defaults();
        let mut input = test_input(vec![test_command("echo ok", 0)]);
        input.evidence_digest = test_digest(EvidenceVerdict::Reject);

        let err = compiler.compile(&input).unwrap_err();
        let is_rejected = matches!(err, CompileError::EvidenceRejected { .. });
        assert!(is_rejected);
    }

    #[test]
    fn rejects_empty_commands() {
        let compiler = ArsCompiler::with_defaults();
        let input = test_input(Vec::new());
        let err = compiler.compile(&input).unwrap_err();
        assert_eq!(err, CompileError::EmptyCommands);
    }

    #[test]
    fn rejects_unsafe_binding() {
        let compiler = ArsCompiler::with_defaults();
        let cmd = test_templated_command();
        let mut input = test_input(vec![cmd]);
        // Inject unsafe value (semicolon = shell injection).
        input
            .bindings
            .insert("file_0".to_string(), "foo;rm -rf /".to_string());

        let err = compiler.compile(&input).unwrap_err();
        let is_unsafe = matches!(err, CompileError::UnsafeParameter { .. });
        assert!(is_unsafe);
    }

    #[test]
    fn rejects_too_many_steps() {
        let config = CompileConfig {
            max_steps: 3,
            ..Default::default()
        };
        let compiler = ArsCompiler::new(config);
        let cmds = vec![
            test_command("cmd1", 0),
            test_command("cmd2", 1),
            test_command("cmd3", 2),
        ];
        // snapshot(1) + 3 commands + verify(1) + evidence(1) = 6 > 3
        let input = test_input(cmds);
        let err = compiler.compile(&input).unwrap_err();
        let is_too_many = matches!(err, CompileError::TooManySteps { .. });
        assert!(is_too_many);
    }

    #[test]
    fn rejects_text_too_long() {
        let config = CompileConfig {
            max_send_text_len: 10,
            ..Default::default()
        };
        let compiler = ArsCompiler::new(config);
        let input = test_input(vec![test_command("a very long command text here", 0)]);
        let err = compiler.compile(&input).unwrap_err();
        let is_too_long = matches!(err, CompileError::TextTooLong { .. });
        assert!(is_too_long);
    }

    // ---- Timeout clamping ----

    #[test]
    fn timeout_clamped_to_min() {
        let compiler = ArsCompiler::with_defaults();
        let mut input = test_input(vec![test_command("echo ok", 0)]);
        input.timeout.timeout_ms = 100; // Below min 500.
        let output = compiler.compile(&input).unwrap();
        assert!(output.effective_timeout_ms >= 500);
    }

    #[test]
    fn timeout_clamped_to_max() {
        let compiler = ArsCompiler::with_defaults();
        let mut input = test_input(vec![test_command("echo ok", 0)]);
        input.timeout.timeout_ms = 999_999; // Above max 120_000.
        let output = compiler.compile(&input).unwrap();
        assert!(output.effective_timeout_ms <= 120_000);
    }

    #[test]
    fn timeout_in_range_unchanged() {
        let compiler = ArsCompiler::with_defaults();
        let input = test_input(vec![test_command("echo ok", 0)]);
        let output = compiler.compile(&input).unwrap();
        assert_eq!(output.effective_timeout_ms, 5000);
    }

    // ---- Config variations ----

    #[test]
    fn no_snapshot_when_disabled() {
        let config = CompileConfig {
            inject_snapshot: false,
            ..Default::default()
        };
        let compiler = ArsCompiler::new(config);
        let input = test_input(vec![test_command("echo ok", 0)]);
        let output = compiler.compile(&input).unwrap();

        let has_snapshot =
            output.descriptor.steps.iter().any(
                |s| matches!(s, DescriptorStep::Log { id, .. } if id.starts_with("ars_snapshot")),
            );
        assert!(!has_snapshot);
    }

    #[test]
    fn no_wait_for_when_disabled() {
        let config = CompileConfig {
            inject_wait_for: false,
            ..Default::default()
        };
        let compiler = ArsCompiler::new(config);
        let input = test_input(vec![test_command("echo ok", 0)]);
        let output = compiler.compile(&input).unwrap();

        let wait_count = output
            .descriptor
            .steps
            .iter()
            .filter(|s| matches!(s, DescriptorStep::WaitFor { .. }))
            .count();
        assert_eq!(wait_count, 0);
    }

    #[test]
    fn no_evidence_log_when_disabled() {
        let config = CompileConfig {
            inject_evidence_log: false,
            ..Default::default()
        };
        let compiler = ArsCompiler::new(config);
        let input = test_input(vec![test_command("echo ok", 0)]);
        let output = compiler.compile(&input).unwrap();

        let has_evidence =
            output.descriptor.steps.iter().any(
                |s| matches!(s, DescriptorStep::Log { id, .. } if id.starts_with("ars_evidence")),
            );
        assert!(!has_evidence);
    }

    #[test]
    fn substrate_match_uses_substring_when_configured() {
        let config = CompileConfig {
            wait_for_regex: false,
            ..Default::default()
        };
        let compiler = ArsCompiler::new(config);
        let input = test_input(vec![test_command("echo ok", 0)]);
        let output = compiler.compile(&input).unwrap();

        let has_substring = output.descriptor.steps.iter().any(|s| {
            matches!(
                s,
                DescriptorStep::WaitFor {
                    matcher: DescriptorMatcher::Substring { .. },
                    ..
                }
            )
        });
        assert!(has_substring);
    }

    #[test]
    fn custom_success_pattern_used() {
        let compiler = ArsCompiler::with_defaults();
        let mut input = test_input(vec![test_command("echo ok", 0)]);
        input.success_pattern = Some("SUCCESS".to_string());
        let output = compiler.compile(&input).unwrap();

        for step in &output.descriptor.steps {
            if let DescriptorStep::WaitFor { matcher, .. } = step {
                match matcher {
                    DescriptorMatcher::Regex { pattern } => {
                        assert_eq!(pattern, "SUCCESS");
                    }
                    DescriptorMatcher::Substring { value } => {
                        assert_eq!(value, "SUCCESS");
                    }
                }
            }
        }
    }

    // ---- Trigger ----

    #[test]
    fn trigger_included_when_set() {
        let compiler = ArsCompiler::with_defaults();
        let trigger = DescriptorTrigger {
            event_types: vec!["error.detected".to_string()],
            agent_types: vec![],
            rule_ids: vec!["compile_error".to_string()],
        };
        let mut input = test_input(vec![test_command("echo ok", 0)]);
        input.trigger = Some(trigger);
        let output = compiler.compile(&input).unwrap();

        assert_eq!(output.descriptor.triggers.len(), 1);
        assert_eq!(
            output.descriptor.triggers[0].event_types[0],
            "error.detected"
        );
    }

    #[test]
    fn no_trigger_by_default() {
        let compiler = ArsCompiler::with_defaults();
        let input = test_input(vec![test_command("echo ok", 0)]);
        let output = compiler.compile(&input).unwrap();
        assert!(output.descriptor.triggers.is_empty());
    }

    // ---- YAML serialization ----

    #[test]
    fn compile_to_yaml_succeeds() {
        let compiler = ArsCompiler::with_defaults();
        let input = test_input(vec![test_command("cargo test", 0)]);
        let yaml = compiler.compile_to_yaml(&input).unwrap();
        assert!(yaml.contains("ars-test-001"));
        assert!(yaml.contains("cargo test"));
    }

    #[test]
    fn yaml_is_valid_yaml() {
        let compiler = ArsCompiler::with_defaults();
        let input = test_input(vec![test_command("echo ok", 0)]);
        let yaml = compiler.compile_to_yaml(&input).unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        assert!(parsed.is_mapping());
    }

    // ---- Builder ----

    #[test]
    fn builder_creates_valid_input() {
        let input = CompileInputBuilder::new("cluster-42", "fix compile error")
            .description("Auto-fix compile errors")
            .command(test_command("cargo build", 0))
            .timeout(test_timeout(8000))
            .evidence(test_digest(EvidenceVerdict::Support))
            .success_pattern("Compiling")
            .build();

        assert_eq!(input.cluster_id, "cluster-42");
        assert_eq!(input.commands.len(), 1);
        assert_eq!(input.timeout.timeout_ms, 8000);
    }

    #[test]
    fn builder_uses_defaults_when_unset() {
        let input = CompileInputBuilder::new("c1", "test")
            .command(test_command("echo", 0))
            .build();

        assert_eq!(input.timeout.timeout_ms, 5000); // default timeout
        assert!(!input.evidence_digest.is_complete); // default digest
    }

    #[test]
    fn builder_with_bindings() {
        let input = CompileInputBuilder::new("c1", "test")
            .command(test_templated_command())
            .bind("file_0", "src/lib.rs")
            .build();

        assert_eq!(input.bindings["file_0"], "src/lib.rs");
    }

    // ---- Safety regex ----

    #[test]
    fn safety_accepts_valid_file_path() {
        assert!(matches_safety_regex("src/main.rs", "^[a-zA-Z0-9_./-]+$"));
    }

    #[test]
    fn safety_rejects_semicolon() {
        assert!(!matches_safety_regex("foo;rm -rf /", "^[a-zA-Z0-9_./-]+$"));
    }

    #[test]
    fn safety_rejects_backtick() {
        assert!(!matches_safety_regex("foo`cmd`", "^[a-zA-Z0-9_./-]+$"));
    }

    #[test]
    fn safety_rejects_empty() {
        assert!(!matches_safety_regex("", "^[a-zA-Z0-9_./-]+$"));
    }

    #[test]
    fn safety_accepts_line_number() {
        assert!(matches_safety_regex("42", "^[0-9]+$"));
    }

    #[test]
    fn safety_accepts_identifier() {
        assert!(matches_safety_regex(
            "MyStruct_v2",
            "^[a-zA-Z_][a-zA-Z0-9_]*$"
        ));
    }

    #[test]
    fn safety_complex_pattern_rejects_non_matching() {
        // Complex patterns now use a real regex engine, so they correctly reject bad values.
        assert!(!matches_safety_regex("anything", "^(?:foo|bar)$"));
        assert!(matches_safety_regex("foo", "^(?:foo|bar)$"));
    }

    // ---- Newline appending ----

    #[test]
    fn append_newline_to_command() {
        let compiler = ArsCompiler::with_defaults();
        let input = test_input(vec![test_command("echo ok", 0)]);
        let output = compiler.compile(&input).unwrap();

        for step in &output.descriptor.steps {
            if let DescriptorStep::SendText { text, .. } = step {
                assert!(text.ends_with('\n'));
            }
        }
    }

    #[test]
    fn no_double_newline() {
        let compiler = ArsCompiler::with_defaults();
        let input = test_input(vec![test_command("echo ok\n", 0)]);
        let output = compiler.compile(&input).unwrap();

        for step in &output.descriptor.steps {
            if let DescriptorStep::SendText { text, .. } = step {
                assert!(!text.ends_with("\n\n"));
            }
        }
    }

    #[test]
    fn no_newline_when_disabled() {
        let config = CompileConfig {
            append_newline: false,
            ..Default::default()
        };
        let compiler = ArsCompiler::new(config);
        let input = test_input(vec![test_command("echo ok", 0)]);
        let output = compiler.compile(&input).unwrap();

        for step in &output.descriptor.steps {
            if let DescriptorStep::SendText { text, .. } = step {
                assert!(!text.ends_with('\n'));
            }
        }
    }

    // ---- CompileError display ----

    #[test]
    fn error_display_operator_locked() {
        let e = CompileError::OperatorLocked {
            cluster_id: "foo".to_string(),
        };
        assert!(e.to_string().contains("operator locked"));
    }

    #[test]
    fn error_display_evidence_rejected() {
        let e = CompileError::EvidenceRejected {
            reason: "bad".to_string(),
        };
        assert!(e.to_string().contains("evidence rejected"));
    }

    #[test]
    fn error_display_unsafe() {
        let e = CompileError::UnsafeParameter {
            variable: "v".to_string(),
            value: "x".to_string(),
            safety_regex: "r".to_string(),
        };
        assert!(e.to_string().contains("unsafe param"));
    }

    #[test]
    fn error_display_too_many() {
        let e = CompileError::TooManySteps { count: 5, max: 3 };
        assert!(e.to_string().contains("too many steps"));
    }

    #[test]
    fn error_display_text_long() {
        let e = CompileError::TextTooLong {
            total: 5000,
            max: 4096,
        };
        assert!(e.to_string().contains("text too long"));
    }

    #[test]
    fn error_display_missing() {
        let e = CompileError::MissingBindings {
            missing: vec!["x".to_string()],
        };
        assert!(e.to_string().contains("missing bindings"));
    }

    #[test]
    fn error_display_empty() {
        let e = CompileError::EmptyCommands;
        assert!(e.to_string().contains("no commands"));
    }

    // ---- quick_compile ----

    #[test]
    fn quick_compile_succeeds() {
        let digest = test_digest(EvidenceVerdict::Support);
        let output = quick_compile("qc-1", "cargo build", 3000, &digest).unwrap();
        assert!(output.step_count >= 3);
    }

    #[test]
    fn quick_compile_rejects_evidence() {
        let digest = test_digest(EvidenceVerdict::Reject);
        let err = quick_compile("qc-2", "echo", 3000, &digest).unwrap_err();
        let is_rejected = matches!(err, CompileError::EvidenceRejected { .. });
        assert!(is_rejected);
    }

    // ---- Neutral verdict passes ----

    #[test]
    fn neutral_verdict_compiles_ok() {
        let compiler = ArsCompiler::with_defaults();
        let mut input = test_input(vec![test_command("echo ok", 0)]);
        input.evidence_digest = test_digest(EvidenceVerdict::Neutral);
        let output = compiler.compile(&input).unwrap();
        assert_eq!(output.evidence_verdict, EvidenceVerdict::Neutral);
    }

    // ---- Serde roundtrip ----

    #[test]
    fn compile_config_serde_roundtrip() {
        let config = CompileConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let decoded: CompileConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.schema_version, config.schema_version);
        assert_eq!(decoded.max_steps, config.max_steps);
        assert_eq!(decoded.inject_snapshot, config.inject_snapshot);
    }

    #[test]
    fn compile_error_serde_roundtrip() {
        let err = CompileError::OperatorLocked {
            cluster_id: "x".to_string(),
        };
        let json = serde_json::to_string(&err).unwrap();
        let decoded: CompileError = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, err);
    }

    #[test]
    fn compile_input_serde_roundtrip() {
        let input = test_input(vec![test_command("echo", 0)]);
        let json = serde_json::to_string(&input).unwrap();
        let decoded: CompileInput = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.cluster_id, input.cluster_id);
        assert_eq!(decoded.commands.len(), input.commands.len());
    }

    #[test]
    fn compile_output_serde_roundtrip() {
        let compiler = ArsCompiler::with_defaults();
        let input = test_input(vec![test_command("echo ok", 0)]);
        let output = compiler.compile(&input).unwrap();
        let json = serde_json::to_string(&output).unwrap();
        let decoded: CompileOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.step_count, output.step_count);
        assert_eq!(decoded.effective_timeout_ms, output.effective_timeout_ms);
    }

    // ---- Step ID uniqueness ----

    #[test]
    fn all_step_ids_are_unique() {
        let compiler = ArsCompiler::with_defaults();
        let cmds = vec![
            test_command("cmd1", 0),
            test_command("cmd2", 1),
            test_command("cmd3", 2),
        ];
        let input = test_input(cmds);
        let output = compiler.compile(&input).unwrap();

        let ids: Vec<&str> = output
            .descriptor
            .steps
            .iter()
            .map(|s| match s {
                DescriptorStep::Log { id, .. }
                | DescriptorStep::SendText { id, .. }
                | DescriptorStep::WaitFor { id, .. }
                | DescriptorStep::Sleep { id, .. }
                | DescriptorStep::SendCtrl { id, .. }
                | DescriptorStep::Notify { id, .. }
                | DescriptorStep::Abort { id, .. }
                | DescriptorStep::Conditional { id, .. }
                | DescriptorStep::Loop { id, .. } => id.as_str(),
            })
            .collect();

        let mut deduped = ids.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(ids.len(), deduped.len(), "step IDs must be unique");
    }

    // ---- Description passthrough ----

    #[test]
    fn description_set_in_descriptor() {
        let compiler = ArsCompiler::with_defaults();
        let input = test_input(vec![test_command("echo ok", 0)]);
        let output = compiler.compile(&input).unwrap();
        assert_eq!(
            output.descriptor.description,
            Some("A test reflex".to_string())
        );
    }

    #[test]
    fn none_description_ok() {
        let compiler = ArsCompiler::with_defaults();
        let mut input = test_input(vec![test_command("echo ok", 0)]);
        input.description = None;
        let output = compiler.compile(&input).unwrap();
        assert!(output.descriptor.description.is_none());
    }
}
