//! Data-driven workflow descriptors and filesystem loading.
//!
//! Defines declarative workflow descriptors that can be authored in YAML/TOML
//! and compiled into executable workflows. Includes filesystem scanning for
//! loading user-defined workflows from a configuration directory.
//!
//! Extracted from `workflows.rs` as part of strangler fig refactoring (ft-c45am).

#[allow(clippy::wildcard_imports)]
use super::*;

// ============================================================================
// Data-Driven Workflow Descriptors (wa-nu4.1.1.6)
// ============================================================================

const DESCRIPTOR_SCHEMA_VERSION: u32 = 1;
pub const DESCRIPTOR_MAX_STEPS: usize = 32;
pub const DESCRIPTOR_MAX_WAIT_TIMEOUT_MS: u64 = 120_000;
const DESCRIPTOR_MAX_SLEEP_MS: u64 = 30_000;
const DESCRIPTOR_MAX_TEXT_LEN: usize = 8_192;
const DESCRIPTOR_MAX_MATCH_LEN: usize = 1_024;

/// Limits for descriptor validation.
#[derive(Debug, Clone)]
pub struct DescriptorLimits {
    pub max_steps: usize,
    pub max_wait_timeout_ms: u64,
    pub max_sleep_ms: u64,
    pub max_text_len: usize,
    pub max_match_len: usize,
}

impl Default for DescriptorLimits {
    fn default() -> Self {
        Self {
            max_steps: DESCRIPTOR_MAX_STEPS,
            max_wait_timeout_ms: DESCRIPTOR_MAX_WAIT_TIMEOUT_MS,
            max_sleep_ms: DESCRIPTOR_MAX_SLEEP_MS,
            max_text_len: DESCRIPTOR_MAX_TEXT_LEN,
            max_match_len: DESCRIPTOR_MAX_MATCH_LEN,
        }
    }
}

/// Trigger definition for descriptor workflows.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DescriptorTrigger {
    /// Event types that trigger this workflow (e.g., "session.compaction").
    #[serde(default)]
    pub event_types: Vec<String>,
    /// Agent types this trigger applies to (e.g., ["codex", "claude_code"]).
    /// Empty means all agents.
    #[serde(default)]
    pub agent_types: Vec<String>,
    /// Rule IDs that trigger this workflow (e.g., "compaction.detected").
    #[serde(default)]
    pub rule_ids: Vec<String>,
}

/// Failure handler for descriptor workflows.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum DescriptorFailureHandler {
    /// Send a notification with a message. Supports `${failed_step}` interpolation.
    Notify { message: String },
    /// Write a log entry with a message. Supports `${failed_step}` interpolation.
    Log { message: String },
    /// Abort with a reason message. Supports `${failed_step}` interpolation.
    Abort { message: String },
}

impl DescriptorFailureHandler {
    /// Interpolate `${failed_step}` in the message template.
    #[must_use]
    pub fn interpolate_message(&self, failed_step: &str) -> String {
        let template = match self {
            Self::Notify { message } | Self::Log { message } | Self::Abort { message } => message,
        };
        template.replace("${failed_step}", failed_step)
    }
}

/// Descriptor for a simple, data-driven workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowDescriptor {
    pub workflow_schema_version: u32,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Triggers that activate this workflow.
    #[serde(default)]
    pub triggers: Vec<DescriptorTrigger>,
    pub steps: Vec<DescriptorStep>,
    /// Handler executed when a step fails.
    #[serde(default)]
    pub on_failure: Option<DescriptorFailureHandler>,
}

impl WorkflowDescriptor {
    pub fn from_yaml_str(input: &str) -> crate::Result<Self> {
        let descriptor: Self = serde_yaml::from_str(input).map_err(|e| {
            crate::Error::Config(crate::error::ConfigError::ParseFailed(e.to_string()))
        })?;
        descriptor.validate(&DescriptorLimits::default())?;
        Ok(descriptor)
    }

    pub fn from_toml_str(input: &str) -> crate::Result<Self> {
        let descriptor: Self = toml::from_str(input).map_err(|e| {
            crate::Error::Config(crate::error::ConfigError::ParseFailed(e.to_string()))
        })?;
        descriptor.validate(&DescriptorLimits::default())?;
        Ok(descriptor)
    }

    pub fn validate(&self, limits: &DescriptorLimits) -> crate::Result<()> {
        if self.workflow_schema_version != DESCRIPTOR_SCHEMA_VERSION {
            return Err(crate::Error::Config(
                crate::error::ConfigError::ValidationError(format!(
                    "Unsupported workflow_schema_version {} (expected {})",
                    self.workflow_schema_version, DESCRIPTOR_SCHEMA_VERSION
                )),
            ));
        }

        if self.steps.is_empty() {
            return Err(crate::Error::Config(
                crate::error::ConfigError::ValidationError(
                    "Descriptor must contain at least one step".to_string(),
                ),
            ));
        }

        if self.steps.len() > limits.max_steps {
            return Err(crate::Error::Config(
                crate::error::ConfigError::ValidationError(format!(
                    "Descriptor has too many steps ({} > max {})",
                    self.steps.len(),
                    limits.max_steps
                )),
            ));
        }

        let mut seen = std::collections::HashSet::new();
        for step in &self.steps {
            let id = step.id();
            if id.trim().is_empty() {
                return Err(crate::Error::Config(
                    crate::error::ConfigError::ValidationError(
                        "Descriptor step id cannot be empty".to_string(),
                    ),
                ));
            }
            if !seen.insert(id.to_string()) {
                return Err(crate::Error::Config(
                    crate::error::ConfigError::ValidationError(format!("Duplicate step id: {id}")),
                ));
            }

            step.validate(limits)?;
        }

        Ok(())
    }
}

/// Matchers in descriptors (substring or regex).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DescriptorMatcher {
    Substring { value: String },
    Regex { pattern: String },
}

impl DescriptorMatcher {
    pub(crate) fn to_text_match(&self) -> TextMatch {
        match self {
            Self::Substring { value } => TextMatch::substring(value.clone()),
            Self::Regex { pattern } => TextMatch::regex(pattern.clone()),
        }
    }

    fn validate(&self, limits: &DescriptorLimits) -> crate::Result<()> {
        match self {
            Self::Substring { value } => {
                if value.len() > limits.max_match_len {
                    return Err(crate::Error::Config(
                        crate::error::ConfigError::ValidationError(format!(
                            "Substring matcher too long ({} > max {})",
                            value.len(),
                            limits.max_match_len
                        )),
                    ));
                }
            }
            Self::Regex { pattern } => {
                if pattern.len() > limits.max_match_len {
                    return Err(crate::Error::Config(
                        crate::error::ConfigError::ValidationError(format!(
                            "Regex matcher too long ({} > max {})",
                            pattern.len(),
                            limits.max_match_len
                        )),
                    ));
                }
                fancy_regex::Regex::new(pattern).map_err(|e| {
                    crate::Error::Config(crate::error::ConfigError::ValidationError(format!(
                        "Invalid regex pattern: {e}"
                    )))
                })?;
            }
        }
        Ok(())
    }
}

/// Supported control key sends in descriptor workflows.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DescriptorControlKey {
    CtrlC,
    CtrlD,
    CtrlZ,
}

/// Descriptor step definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DescriptorStep {
    WaitFor {
        id: String,
        #[serde(default)]
        description: Option<String>,
        matcher: DescriptorMatcher,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    Sleep {
        id: String,
        #[serde(default)]
        description: Option<String>,
        duration_ms: u64,
    },
    SendText {
        id: String,
        #[serde(default)]
        description: Option<String>,
        text: String,
        #[serde(default)]
        wait_for: Option<DescriptorMatcher>,
        #[serde(default)]
        wait_timeout_ms: Option<u64>,
    },
    SendCtrl {
        id: String,
        #[serde(default)]
        description: Option<String>,
        key: DescriptorControlKey,
    },
    /// Send a notification message (logged, not injected into pane).
    Notify {
        id: String,
        #[serde(default)]
        description: Option<String>,
        message: String,
    },
    /// Write an entry to the workflow audit log.
    Log {
        id: String,
        #[serde(default)]
        description: Option<String>,
        message: String,
    },
    /// Abort the workflow immediately with a reason.
    Abort {
        id: String,
        #[serde(default)]
        description: Option<String>,
        reason: String,
    },
    /// Conditional branching: evaluate a matcher and run then/else steps.
    Conditional {
        id: String,
        #[serde(default)]
        description: Option<String>,
        /// Text to test the condition against (supports `${trigger}` interpolation).
        test_text: String,
        matcher: DescriptorMatcher,
        /// Steps to execute if the condition matches.
        then_steps: Vec<DescriptorStep>,
        /// Steps to execute if the condition does not match.
        #[serde(default)]
        else_steps: Vec<DescriptorStep>,
    },
    /// Loop: repeat contained steps a fixed number of times.
    Loop {
        id: String,
        #[serde(default)]
        description: Option<String>,
        /// Number of iterations.
        count: u32,
        /// Steps to repeat.
        body: Vec<DescriptorStep>,
    },
}

impl DescriptorStep {
    fn id(&self) -> &str {
        match self {
            Self::WaitFor { id, .. }
            | Self::Sleep { id, .. }
            | Self::SendText { id, .. }
            | Self::SendCtrl { id, .. }
            | Self::Notify { id, .. }
            | Self::Log { id, .. }
            | Self::Abort { id, .. }
            | Self::Conditional { id, .. }
            | Self::Loop { id, .. } => id,
        }
    }

    fn description(&self) -> String {
        match self {
            Self::WaitFor { description, .. } => description
                .clone()
                .unwrap_or_else(|| "Wait for substring/regex match".to_string()),
            Self::Sleep { description, .. } => {
                description.clone().unwrap_or_else(|| "Sleep".to_string())
            }
            Self::SendText { description, .. } => description
                .clone()
                .unwrap_or_else(|| "Send text".to_string()),
            Self::SendCtrl { description, .. } => description
                .clone()
                .unwrap_or_else(|| "Send control key".to_string()),
            Self::Notify { description, .. } => {
                description.clone().unwrap_or_else(|| "Notify".to_string())
            }
            Self::Log { description, .. } => description
                .clone()
                .unwrap_or_else(|| "Log message".to_string()),
            Self::Abort { description, .. } => description
                .clone()
                .unwrap_or_else(|| "Abort workflow".to_string()),
            Self::Conditional { description, .. } => description
                .clone()
                .unwrap_or_else(|| "Conditional branch".to_string()),
            Self::Loop { description, .. } => {
                description.clone().unwrap_or_else(|| "Loop".to_string())
            }
        }
    }

    fn validate(&self, limits: &DescriptorLimits) -> crate::Result<()> {
        match self {
            Self::WaitFor {
                matcher,
                timeout_ms,
                ..
            } => {
                matcher.validate(limits)?;
                if let Some(timeout_ms) = timeout_ms {
                    if *timeout_ms > limits.max_wait_timeout_ms {
                        return Err(crate::Error::Config(
                            crate::error::ConfigError::ValidationError(format!(
                                "wait_for timeout_ms too large ({} > max {})",
                                timeout_ms, limits.max_wait_timeout_ms
                            )),
                        ));
                    }
                }
            }
            Self::Sleep { duration_ms, .. } => {
                if *duration_ms > limits.max_sleep_ms {
                    return Err(crate::Error::Config(
                        crate::error::ConfigError::ValidationError(format!(
                            "sleep duration_ms too large ({} > max {})",
                            duration_ms, limits.max_sleep_ms
                        )),
                    ));
                }
            }
            Self::SendText {
                text,
                wait_for,
                wait_timeout_ms,
                ..
            } => {
                if text.len() > limits.max_text_len {
                    return Err(crate::Error::Config(
                        crate::error::ConfigError::ValidationError(format!(
                            "send_text too large ({} > max {})",
                            text.len(),
                            limits.max_text_len
                        )),
                    ));
                }
                if let Some(wait_for) = wait_for {
                    wait_for.validate(limits)?;
                }
                if let Some(wait_timeout_ms) = wait_timeout_ms {
                    if *wait_timeout_ms > limits.max_wait_timeout_ms {
                        return Err(crate::Error::Config(
                            crate::error::ConfigError::ValidationError(format!(
                                "send_text wait_timeout_ms too large ({} > max {})",
                                wait_timeout_ms, limits.max_wait_timeout_ms
                            )),
                        ));
                    }
                }
            }
            Self::SendCtrl { .. } => {}
            Self::Notify { message, .. } | Self::Log { message, .. } => {
                if message.len() > limits.max_text_len {
                    return Err(crate::Error::Config(
                        crate::error::ConfigError::ValidationError(format!(
                            "Message too long ({} > max {})",
                            message.len(),
                            limits.max_text_len
                        )),
                    ));
                }
            }
            Self::Abort { reason, .. } => {
                if reason.len() > limits.max_text_len {
                    return Err(crate::Error::Config(
                        crate::error::ConfigError::ValidationError(format!(
                            "Abort reason too long ({} > max {})",
                            reason.len(),
                            limits.max_text_len
                        )),
                    ));
                }
            }
            Self::Conditional {
                matcher,
                then_steps,
                else_steps,
                ..
            } => {
                matcher.validate(limits)?;
                for step in then_steps {
                    step.validate(limits)?;
                }
                for step in else_steps {
                    step.validate(limits)?;
                }
            }
            Self::Loop { count, body, .. } => {
                if *count > 1000 {
                    return Err(crate::Error::Config(
                        crate::error::ConfigError::ValidationError(format!(
                            "Loop count too large ({count} > max 1000)"
                        )),
                    ));
                }
                if body.is_empty() {
                    return Err(crate::Error::Config(
                        crate::error::ConfigError::ValidationError(
                            "Loop body must contain at least one step".to_string(),
                        ),
                    ));
                }
                for step in body {
                    step.validate(limits)?;
                }
            }
        }
        Ok(())
    }
}

const DESCRIPTOR_MAX_RECURSION_DEPTH: usize = 64;

#[derive(Debug, Clone)]
enum ExecutableStep {
    Action(DescriptorStep),
    Jump(usize),
    JumpIfFalse {
        test_text: String,
        matcher: DescriptorMatcher,
        jump_to: usize,
    },
}

/// Workflow implementation backed by a descriptor.
pub struct DescriptorWorkflow {
    pub(crate) name: &'static str,
    description: &'static str,
    descriptor: WorkflowDescriptor,
    executable_steps: Vec<ExecutableStep>,
    step_metadata: Vec<WorkflowStep>,
}

impl DescriptorWorkflow {
    #[must_use]
    pub fn new(descriptor: WorkflowDescriptor) -> Self {
        let name: &'static str = Box::leak(descriptor.name.clone().into_boxed_str());
        let desc_value = descriptor
            .description
            .clone()
            .unwrap_or_else(|| "Descriptor workflow".to_string());
        let description: &'static str = Box::leak(desc_value.into_boxed_str());

        let (executable_steps, step_metadata) = Self::compile(&descriptor.steps);

        Self {
            name,
            description,
            descriptor,
            executable_steps,
            step_metadata,
        }
    }

    fn compile(steps: &[DescriptorStep]) -> (Vec<ExecutableStep>, Vec<WorkflowStep>) {
        let mut exec = Vec::new();
        let mut meta = Vec::new();
        Self::compile_recursive(steps, &mut exec, &mut meta, 0);
        (exec, meta)
    }

    fn compile_recursive(
        steps: &[DescriptorStep],
        exec: &mut Vec<ExecutableStep>,
        meta: &mut Vec<WorkflowStep>,
        depth: usize,
    ) {
        if depth > DESCRIPTOR_MAX_RECURSION_DEPTH {
            return;
        }

        for step in steps {
            match step {
                DescriptorStep::Loop {
                    count,
                    body,
                    id: _,
                    description: _,
                } => {
                    for _ in 0..*count {
                        Self::compile_recursive(body, exec, meta, depth + 1);
                    }
                }
                DescriptorStep::Conditional {
                    test_text,
                    matcher,
                    then_steps,
                    else_steps,
                    id,
                    description,
                } => {
                    let jump_if_false_idx = exec.len();
                    exec.push(ExecutableStep::JumpIfFalse {
                        test_text: test_text.clone(),
                        matcher: matcher.clone(),
                        jump_to: 0,
                    });
                    meta.push(WorkflowStep::new(
                        id.clone(),
                        format!("Check: {}", description.as_deref().unwrap_or("condition")),
                    ));

                    Self::compile_recursive(then_steps, exec, meta, depth + 1);

                    let jump_end_idx = exec.len();
                    exec.push(ExecutableStep::Jump(0));
                    meta.push(WorkflowStep::new(format!("{id}_jump"), "Flow control"));

                    let else_start_idx = exec.len();
                    Self::compile_recursive(else_steps, exec, meta, depth + 1);
                    let end_idx = exec.len();

                    if let ExecutableStep::JumpIfFalse { jump_to, .. } =
                        &mut exec[jump_if_false_idx]
                    {
                        *jump_to = else_start_idx;
                    }
                    if let ExecutableStep::Jump(target) = &mut exec[jump_end_idx] {
                        *target = end_idx;
                    }
                }
                _ => {
                    exec.push(ExecutableStep::Action(step.clone()));
                    meta.push(WorkflowStep::new(step.id(), step.description()));
                }
            }
        }
    }

    pub fn from_yaml_str(input: &str) -> crate::Result<Self> {
        WorkflowDescriptor::from_yaml_str(input).map(Self::new)
    }

    pub fn from_toml_str(input: &str) -> crate::Result<Self> {
        WorkflowDescriptor::from_toml_str(input).map(Self::new)
    }

    #[must_use]
    pub fn descriptor(&self) -> &WorkflowDescriptor {
        &self.descriptor
    }
}

impl Workflow for DescriptorWorkflow {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> &'static str {
        self.description
    }

    fn handles(&self, detection: &crate::patterns::Detection) -> bool {
        if self.descriptor.triggers.is_empty() {
            return false;
        }
        self.descriptor.triggers.iter().any(|trigger| {
            let event_match = trigger.event_types.is_empty()
                || trigger
                    .event_types
                    .iter()
                    .any(|et| et == &detection.event_type);
            let agent_match = trigger.agent_types.is_empty()
                || trigger
                    .agent_types
                    .iter()
                    .any(|at| at == &detection.agent_type.to_string());
            let rule_match = trigger.rule_ids.is_empty()
                || trigger.rule_ids.iter().any(|ri| ri == &detection.rule_id);
            event_match && agent_match && rule_match
        })
    }

    fn steps(&self) -> Vec<WorkflowStep> {
        self.step_metadata.clone()
    }

    fn execute_step(
        &self,
        ctx: &mut WorkflowContext,
        step_idx: usize,
    ) -> BoxFuture<'_, StepResult> {
        let step = self.executable_steps.get(step_idx).cloned();
        let default_wait_timeout_ms = ctx.default_wait_timeout_ms();
        let ctx_clone = ctx.clone();

        Box::pin(async move {
            let Some(step) = step else {
                return StepResult::abort("Unexpected step index");
            };

            match step {
                ExecutableStep::Action(desc_step) => {
                    execute_atomic_descriptor_step(&desc_step, default_wait_timeout_ms, ctx_clone)
                        .await
                }
                ExecutableStep::Jump(target) => StepResult::jump_to(target),
                ExecutableStep::JumpIfFalse {
                    test_text,
                    matcher,
                    jump_to,
                } => {
                    let mut actual_text = test_text;
                    if actual_text.contains("${trigger}") {
                        let trigger_str = ctx_clone.trigger().map_or_else(String::new, |val| {
                            serde_json::to_string(val).unwrap_or_default()
                        });
                        actual_text = actual_text.replace("${trigger}", &trigger_str);
                    }
                    let matches = match matcher {
                        DescriptorMatcher::Substring { value } => {
                            actual_text.contains(value.as_str())
                        }
                        DescriptorMatcher::Regex { pattern } => {
                            thread_local! {
                                static DESC_REGEX_CACHE: std::cell::RefCell<crate::lru_cache::LruCache<String, fancy_regex::Regex>> =
                                    std::cell::RefCell::new(crate::lru_cache::LruCache::new(64));
                            }
                            DESC_REGEX_CACHE.with(|cache| {
                                let mut cache = cache.borrow_mut();
                                if let Some(re) = cache.get(&pattern) {
                                    return re.is_match(&actual_text).unwrap_or(false);
                                }
                                if let Ok(re) = fancy_regex::Regex::new(&pattern) {
                                    let is_match = re.is_match(&actual_text).unwrap_or(false);
                                    cache.put(pattern.clone(), re);
                                    return is_match;
                                }
                                false
                            })
                        }
                    };
                    if matches {
                        StepResult::cont()
                    } else {
                        StepResult::jump_to(jump_to)
                    }
                }
            }
        })
    }
}

async fn execute_atomic_descriptor_step(
    step: &DescriptorStep,
    default_wait_timeout_ms: u64,
    ctx_clone: WorkflowContext,
) -> StepResult {
    match step {
        DescriptorStep::WaitFor {
            matcher,
            timeout_ms,
            ..
        } => {
            let condition = WaitCondition::text_match(matcher.to_text_match());
            if let Some(timeout_ms) = timeout_ms {
                StepResult::wait_for_with_timeout(condition, *timeout_ms)
            } else {
                StepResult::wait_for(condition)
            }
        }
        DescriptorStep::Sleep { duration_ms, .. } => {
            StepResult::wait_for_with_timeout(WaitCondition::sleep(*duration_ms), *duration_ms)
        }
        DescriptorStep::SendText {
            text,
            wait_for,
            wait_timeout_ms,
            ..
        } => {
            if let Some(wait_for) = wait_for {
                let condition = WaitCondition::text_match(wait_for.to_text_match());
                StepResult::send_text_and_wait(
                    text.clone(),
                    condition,
                    wait_timeout_ms.unwrap_or(default_wait_timeout_ms),
                )
            } else {
                StepResult::send_text(text.clone())
            }
        }
        DescriptorStep::SendCtrl { key, .. } => {
            let mut ctx_clone = ctx_clone;
            let result = match key {
                DescriptorControlKey::CtrlC => ctx_clone.send_ctrl_c().await,
                DescriptorControlKey::CtrlD => ctx_clone.send_ctrl_d().await,
                DescriptorControlKey::CtrlZ => ctx_clone.send_ctrl_z().await,
            };
            match result {
                Ok(InjectionResult::Allowed { .. }) => StepResult::cont(),
                Ok(InjectionResult::Denied { decision, .. }) => StepResult::abort(format!(
                    "Policy denied control send: {}",
                    decision.reason().unwrap_or("denied")
                )),
                Ok(InjectionResult::RequiresApproval { decision, .. }) => {
                    StepResult::abort(format!(
                        "Control send requires approval: {}",
                        decision.reason().unwrap_or("requires approval")
                    ))
                }
                Ok(InjectionResult::Error { error, .. }) => {
                    StepResult::abort(format!("Control send failed: {error}"))
                }
                Err(err) => StepResult::abort(err),
            }
        }
        DescriptorStep::Notify { message, id, .. } => {
            tracing::info!(step_id = %id, %message, "descriptor workflow notify");
            StepResult::cont()
        }
        DescriptorStep::Log { message, id, .. } => {
            tracing::info!(step_id = %id, %message, "descriptor workflow log");
            StepResult::cont()
        }
        DescriptorStep::Abort { reason, .. } => StepResult::abort(reason.clone()),
        _ => StepResult::abort("Nested step encountered in atomic execution"),
    }
}

// ============================================================================
// Filesystem Loading (wa-fno.2)
// ============================================================================

/// Load all YAML and TOML workflow descriptors from a directory.
///
/// Scans `dir` for files with `.yaml`, `.yml`, or `.toml` extensions,
/// parses each as a `WorkflowDescriptor`, wraps it in a `DescriptorWorkflow`,
/// and returns the collection. Files that fail to parse are logged and skipped.
///
/// The canonical user directory is `~/.config/wa/workflows/`.
pub fn load_workflows_from_dir(
    dir: &std::path::Path,
) -> Vec<(DescriptorWorkflow, std::path::PathBuf)> {
    let mut workflows = Vec::new();

    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::debug!(path = %dir.display(), error = %e, "cannot read workflow directory");
            return workflows;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "failed to read workflow file");
                continue;
            }
        };

        let result = match ext.as_str() {
            "yaml" | "yml" => DescriptorWorkflow::from_yaml_str(&content),
            "toml" => DescriptorWorkflow::from_toml_str(&content),
            _ => continue,
        };

        match result {
            Ok(workflow) => {
                tracing::info!(
                    name = workflow.name,
                    path = %path.display(),
                    "loaded custom workflow"
                );
                workflows.push((workflow, path));
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "skipping invalid workflow file"
                );
            }
        }
    }

    workflows
}

/// Return the default user workflow directory path (`~/.config/wa/workflows/`).
#[must_use]
pub fn default_workflow_dir() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("ft").join("workflows"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patterns::{AgentType, Detection, Severity};

    fn minimal_yaml() -> &'static str {
        r#"
workflow_schema_version: 1
name: test_workflow
steps:
  - type: send_text
    id: step1
    text: "hello\n"
"#
    }

    fn minimal_toml() -> &'static str {
        r#"
workflow_schema_version = 1
name = "test_workflow"

[[steps]]
type = "send_text"
id = "step1"
text = "hello\n"
"#
    }

    // ========================================================================
    // WorkflowDescriptor parsing
    // ========================================================================

    #[test]
    fn parse_minimal_yaml() {
        let wf = DescriptorWorkflow::from_yaml_str(minimal_yaml()).unwrap();
        assert_eq!(wf.name, "test_workflow");
        assert_eq!(wf.descriptor().steps.len(), 1);
    }

    #[test]
    fn parse_minimal_toml() {
        let wf = DescriptorWorkflow::from_toml_str(minimal_toml()).unwrap();
        assert_eq!(wf.name, "test_workflow");
        assert_eq!(wf.descriptor().steps.len(), 1);
    }

    #[test]
    fn parse_yaml_with_all_step_types() {
        let yaml = r#"
workflow_schema_version: 1
name: all_steps
steps:
  - type: wait_for
    id: wait1
    matcher:
      kind: substring
      value: "prompt$"
  - type: sleep
    id: sleep1
    duration_ms: 1000
  - type: send_text
    id: send1
    text: "ls\n"
    wait_for:
      kind: regex
      pattern: "\\$"
    wait_timeout_ms: 5000
  - type: send_ctrl
    id: ctrl1
    key: ctrl_c
  - type: notify
    id: notify1
    message: "Step done"
  - type: log
    id: log1
    message: "Logging"
  - type: abort
    id: abort1
    reason: "bail out"
"#;
        let wf = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        assert_eq!(wf.descriptor().steps.len(), 7);
    }

    // ========================================================================
    // Validation errors
    // ========================================================================

    #[test]
    fn validate_wrong_schema_version() {
        let yaml = r#"
workflow_schema_version: 99
name: bad
steps:
  - type: send_text
    id: s1
    text: "x"
"#;
        let err = WorkflowDescriptor::from_yaml_str(yaml).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("workflow_schema_version"), "got: {msg}");
    }

    #[test]
    fn validate_empty_steps() {
        let yaml = r"
workflow_schema_version: 1
name: empty
steps: []
";
        let err = WorkflowDescriptor::from_yaml_str(yaml).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("at least one step"), "got: {msg}");
    }

    #[test]
    fn validate_too_many_steps() {
        let mut steps = String::new();
        for i in 0..33 {
            steps.push_str(&format!(
                "  - type: send_text\n    id: s{i}\n    text: \"x\"\n"
            ));
        }
        let yaml = format!("workflow_schema_version: 1\nname: big\nsteps:\n{steps}");
        let err = WorkflowDescriptor::from_yaml_str(&yaml).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("too many steps"), "got: {msg}");
    }

    #[test]
    fn validate_duplicate_step_ids() {
        let yaml = r#"
workflow_schema_version: 1
name: dups
steps:
  - type: send_text
    id: dup
    text: "a"
  - type: send_text
    id: dup
    text: "b"
"#;
        let err = WorkflowDescriptor::from_yaml_str(yaml).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("Duplicate step id"), "got: {msg}");
    }

    #[test]
    fn validate_empty_step_id() {
        let yaml = r#"
workflow_schema_version: 1
name: empty_id
steps:
  - type: send_text
    id: ""
    text: "x"
"#;
        let err = WorkflowDescriptor::from_yaml_str(yaml).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("cannot be empty"), "got: {msg}");
    }

    #[test]
    fn validate_sleep_too_long() {
        let yaml = r"
workflow_schema_version: 1
name: long_sleep
steps:
  - type: sleep
    id: s1
    duration_ms: 999999
";
        let err = WorkflowDescriptor::from_yaml_str(yaml).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("sleep duration_ms too large"), "got: {msg}");
    }

    #[test]
    fn validate_wait_timeout_too_long() {
        let yaml = r#"
workflow_schema_version: 1
name: long_wait
steps:
  - type: wait_for
    id: w1
    matcher:
      kind: substring
      value: "x"
    timeout_ms: 999999
"#;
        let err = WorkflowDescriptor::from_yaml_str(yaml).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("wait_for timeout_ms too large"), "got: {msg}");
    }

    #[test]
    fn validate_send_text_too_long() {
        let long_text = "x".repeat(9000);
        let yaml = format!(
            r#"
workflow_schema_version: 1
name: long_text
steps:
  - type: send_text
    id: s1
    text: "{long_text}"
"#
        );
        let err = WorkflowDescriptor::from_yaml_str(&yaml).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("send_text too large"), "got: {msg}");
    }

    #[test]
    fn validate_invalid_regex() {
        let yaml = r#"
workflow_schema_version: 1
name: bad_regex
steps:
  - type: wait_for
    id: w1
    matcher:
      kind: regex
      pattern: "[invalid"
"#;
        let err = WorkflowDescriptor::from_yaml_str(yaml).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("Invalid regex"), "got: {msg}");
    }

    #[test]
    fn validate_loop_too_many_iterations() {
        let yaml = r#"
workflow_schema_version: 1
name: big_loop
steps:
  - type: loop
    id: loop1
    count: 1001
    body:
      - type: send_text
        id: inner
        text: "x"
"#;
        let err = WorkflowDescriptor::from_yaml_str(yaml).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("Loop count too large"), "got: {msg}");
    }

    #[test]
    fn validate_loop_empty_body() {
        let yaml = r"
workflow_schema_version: 1
name: empty_loop
steps:
  - type: loop
    id: loop1
    count: 5
    body: []
";
        let err = WorkflowDescriptor::from_yaml_str(yaml).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("at least one step"), "got: {msg}");
    }

    // ========================================================================
    // DescriptorFailureHandler
    // ========================================================================

    #[test]
    fn failure_handler_interpolation() {
        let h = DescriptorFailureHandler::Notify {
            message: "Step ${failed_step} failed!".to_string(),
        };
        let result = h.interpolate_message("step_3");
        assert_eq!(result, "Step step_3 failed!");
    }

    #[test]
    fn failure_handler_no_interpolation() {
        let h = DescriptorFailureHandler::Log {
            message: "Something broke".to_string(),
        };
        let result = h.interpolate_message("step_1");
        assert_eq!(result, "Something broke");
    }

    #[test]
    fn failure_handler_abort_variant() {
        let h = DescriptorFailureHandler::Abort {
            message: "Fatal at ${failed_step}".to_string(),
        };
        let result = h.interpolate_message("wait_step");
        assert_eq!(result, "Fatal at wait_step");
    }

    // ========================================================================
    // DescriptorMatcher
    // ========================================================================

    #[test]
    fn descriptor_matcher_to_text_match() {
        let sub = DescriptorMatcher::Substring {
            value: "hello".to_string(),
        };
        let tm = sub.to_text_match();
        assert_eq!(tm, TextMatch::substring("hello"));

        let re = DescriptorMatcher::Regex {
            pattern: r"\d+".to_string(),
        };
        let tm = re.to_text_match();
        assert_eq!(tm, TextMatch::regex(r"\d+"));
    }

    #[test]
    fn descriptor_matcher_validate_substring_too_long() {
        let long = "x".repeat(2000);
        let m = DescriptorMatcher::Substring { value: long };
        let err = m.validate(&DescriptorLimits::default()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("Substring matcher too long"), "got: {msg}");
    }

    #[test]
    fn descriptor_matcher_validate_regex_too_long() {
        let long = "x".repeat(2000);
        let m = DescriptorMatcher::Regex { pattern: long };
        let err = m.validate(&DescriptorLimits::default()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("Regex matcher too long"), "got: {msg}");
    }

    // ========================================================================
    // DescriptorLimits defaults
    // ========================================================================

    #[test]
    fn descriptor_limits_defaults() {
        let limits = DescriptorLimits::default();
        assert_eq!(limits.max_steps, 32);
        assert_eq!(limits.max_wait_timeout_ms, 120_000);
        assert_eq!(limits.max_sleep_ms, 30_000);
        assert_eq!(limits.max_text_len, 8_192);
        assert_eq!(limits.max_match_len, 1_024);
    }

    // ========================================================================
    // DescriptorStep::id and description
    // ========================================================================

    #[test]
    fn descriptor_step_id_all_variants() {
        let steps: Vec<DescriptorStep> = serde_yaml::from_str(
            r#"
- type: send_text
  id: st1
  text: "x"
- type: sleep
  id: sl1
  duration_ms: 100
- type: notify
  id: n1
  message: "hi"
- type: log
  id: l1
  message: "log"
- type: abort
  id: a1
  reason: "done"
"#,
        )
        .unwrap();
        let ids: Vec<&str> = steps.iter().map(|s| s.id()).collect();
        assert_eq!(ids, vec!["st1", "sl1", "n1", "l1", "a1"]);
    }

    // ========================================================================
    // Conditional and Loop compilation
    // ========================================================================

    #[test]
    fn conditional_workflow_parses() {
        let yaml = r#"
workflow_schema_version: 1
name: conditional
steps:
  - type: conditional
    id: cond1
    test_text: "hello world"
    matcher:
      kind: substring
      value: "hello"
    then_steps:
      - type: send_text
        id: then1
        text: "matched\n"
    else_steps:
      - type: send_text
        id: else1
        text: "no match\n"
"#;
        let wf = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        assert_eq!(wf.descriptor().steps.len(), 1);
        // Compiled steps should expand conditional into jump instructions + inner steps
        assert!(wf.step_metadata.len() > 1);
    }

    #[test]
    fn loop_workflow_parses() {
        let yaml = r#"
workflow_schema_version: 1
name: loop_test
steps:
  - type: loop
    id: loop1
    count: 3
    body:
      - type: send_text
        id: inner1
        text: "tick\n"
"#;
        let wf = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        // Loop with count=3 and 1 inner step should compile to 3 steps
        assert_eq!(wf.step_metadata.len(), 3);
    }

    // ========================================================================
    // Workflow triggers
    // ========================================================================

    #[test]
    fn workflow_with_triggers() {
        let yaml = r#"
workflow_schema_version: 1
name: triggered
triggers:
  - event_types: ["session.compaction"]
    agent_types: ["codex"]
    rule_ids: ["compaction.detected"]
steps:
  - type: send_text
    id: s1
    text: "hello\n"
"#;
        let wf = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        assert_eq!(wf.descriptor().triggers.len(), 1);
        let trigger = &wf.descriptor().triggers[0];
        assert_eq!(trigger.event_types, vec!["session.compaction"]);
        assert_eq!(trigger.agent_types, vec!["codex"]);
    }

    // ========================================================================
    // Failure handler serde
    // ========================================================================

    #[test]
    fn failure_handler_serde_roundtrip() {
        let handlers = vec![
            DescriptorFailureHandler::Notify {
                message: "failed at ${failed_step}".into(),
            },
            DescriptorFailureHandler::Log {
                message: "log msg".into(),
            },
            DescriptorFailureHandler::Abort {
                message: "fatal".into(),
            },
        ];
        for h in &handlers {
            let json = serde_json::to_string(h).unwrap();
            let parsed: DescriptorFailureHandler = serde_json::from_str(&json).unwrap();
            let json2 = serde_json::to_string(&parsed).unwrap();
            assert_eq!(json, json2);
        }
    }

    // ========================================================================
    // Filesystem loading
    // ========================================================================

    #[test]
    fn load_workflows_from_dir_reads_yaml_and_toml() {
        let dir = std::env::temp_dir().join("ft_desc_test_load");
        let _ = std::fs::create_dir_all(&dir);

        std::fs::write(dir.join("wf1.yaml"), minimal_yaml()).unwrap();
        std::fs::write(dir.join("wf2.toml"), minimal_toml()).unwrap();
        // Non-workflow file should be ignored
        std::fs::write(dir.join("readme.txt"), "not a workflow").unwrap();

        let loaded = load_workflows_from_dir(&dir);
        assert_eq!(loaded.len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_workflows_from_nonexistent_dir() {
        let loaded = load_workflows_from_dir(std::path::Path::new("/nonexistent_dir_xyz"));
        assert!(loaded.is_empty());
    }

    #[test]
    fn load_workflows_skips_invalid_files() {
        let dir = std::env::temp_dir().join("ft_desc_test_skip");
        let _ = std::fs::create_dir_all(&dir);

        std::fs::write(dir.join("bad.yaml"), "not: valid: yaml: [[").unwrap();
        std::fs::write(dir.join("good.yaml"), minimal_yaml()).unwrap();

        let loaded = load_workflows_from_dir(&dir);
        assert_eq!(loaded.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ========================================================================
    // ControlKey serde
    // ========================================================================

    #[test]
    fn control_key_serde_roundtrip() {
        for key in [
            DescriptorControlKey::CtrlC,
            DescriptorControlKey::CtrlD,
            DescriptorControlKey::CtrlZ,
        ] {
            let json = serde_json::to_string(&key).unwrap();
            let parsed: DescriptorControlKey = serde_json::from_str(&json).unwrap();
            let json2 = serde_json::to_string(&parsed).unwrap();
            assert_eq!(json, json2);
        }
    }

    // ========================================================================
    // WorkflowDescriptor serde roundtrip
    // ========================================================================

    #[test]
    fn workflow_descriptor_serde_roundtrip() {
        let desc = WorkflowDescriptor::from_yaml_str(minimal_yaml()).unwrap();
        let json = serde_json::to_string(&desc).unwrap();
        let parsed: WorkflowDescriptor = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "test_workflow");
        assert_eq!(parsed.steps.len(), 1);
    }

    // ========================================================================
    // DescriptorStep::id() and description()
    // ========================================================================

    #[test]
    fn step_id_returns_correct_id_for_each_variant() {
        let steps = [
            DescriptorStep::WaitFor {
                id: "w1".into(),
                description: None,
                matcher: DescriptorMatcher::Substring {
                    value: "x".into(),
                },
                timeout_ms: None,
            },
            DescriptorStep::Sleep {
                id: "s1".into(),
                description: None,
                duration_ms: 100,
            },
            DescriptorStep::SendText {
                id: "t1".into(),
                description: None,
                text: "hello".into(),
                wait_for: None,
                wait_timeout_ms: None,
            },
            DescriptorStep::SendCtrl {
                id: "c1".into(),
                description: None,
                key: DescriptorControlKey::CtrlC,
            },
            DescriptorStep::Notify {
                id: "n1".into(),
                description: None,
                message: "note".into(),
            },
            DescriptorStep::Log {
                id: "l1".into(),
                description: None,
                message: "log".into(),
            },
            DescriptorStep::Abort {
                id: "a1".into(),
                description: None,
                reason: "bail".into(),
            },
        ];
        let expected_ids = ["w1", "s1", "t1", "c1", "n1", "l1", "a1"];
        for (step, expected) in steps.iter().zip(expected_ids.iter()) {
            assert_eq!(step.id(), *expected);
        }
    }

    #[test]
    fn step_description_defaults_when_none() {
        let step = DescriptorStep::WaitFor {
            id: "w1".into(),
            description: None,
            matcher: DescriptorMatcher::Substring {
                value: "x".into(),
            },
            timeout_ms: None,
        };
        assert_eq!(step.description(), "Wait for substring/regex match");

        let step = DescriptorStep::Sleep {
            id: "s1".into(),
            description: None,
            duration_ms: 100,
        };
        assert_eq!(step.description(), "Sleep");

        let step = DescriptorStep::Abort {
            id: "a1".into(),
            description: None,
            reason: "bye".into(),
        };
        assert_eq!(step.description(), "Abort workflow");
    }

    #[test]
    fn step_description_uses_custom_when_provided() {
        let step = DescriptorStep::SendText {
            id: "t1".into(),
            description: Some("Run ls command".into()),
            text: "ls\n".into(),
            wait_for: None,
            wait_timeout_ms: None,
        };
        assert_eq!(step.description(), "Run ls command");
    }

    #[test]
    fn step_description_all_variants_with_custom() {
        let variants: Vec<(DescriptorStep, &str)> = vec![
            (
                DescriptorStep::WaitFor {
                    id: "a".into(),
                    description: Some("custom wait".into()),
                    matcher: DescriptorMatcher::Substring {
                        value: "x".into(),
                    },
                    timeout_ms: None,
                },
                "custom wait",
            ),
            (
                DescriptorStep::Sleep {
                    id: "b".into(),
                    description: Some("custom sleep".into()),
                    duration_ms: 1,
                },
                "custom sleep",
            ),
            (
                DescriptorStep::SendCtrl {
                    id: "c".into(),
                    description: Some("custom ctrl".into()),
                    key: DescriptorControlKey::CtrlD,
                },
                "custom ctrl",
            ),
            (
                DescriptorStep::Notify {
                    id: "d".into(),
                    description: Some("custom notify".into()),
                    message: "m".into(),
                },
                "custom notify",
            ),
            (
                DescriptorStep::Log {
                    id: "e".into(),
                    description: Some("custom log".into()),
                    message: "m".into(),
                },
                "custom log",
            ),
            (
                DescriptorStep::Conditional {
                    id: "f".into(),
                    description: Some("custom cond".into()),
                    test_text: "x".into(),
                    matcher: DescriptorMatcher::Substring {
                        value: "x".into(),
                    },
                    then_steps: vec![],
                    else_steps: vec![],
                },
                "custom cond",
            ),
            (
                DescriptorStep::Loop {
                    id: "g".into(),
                    description: Some("custom loop".into()),
                    count: 1,
                    body: vec![],
                },
                "custom loop",
            ),
        ];
        for (step, expected) in &variants {
            assert_eq!(step.description(), *expected);
        }
    }

    // ========================================================================
    // DescriptorWorkflow compilation (compile / compile_recursive)
    // ========================================================================

    #[test]
    fn compile_simple_steps_produces_action_and_metadata() {
        let wf = DescriptorWorkflow::from_yaml_str(minimal_yaml()).unwrap();
        let steps = wf.steps();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].name, "step1");
        assert_eq!(steps[0].description, "Send text");
    }

    #[test]
    fn compile_loop_unrolls_body() {
        let yaml = r#"
workflow_schema_version: 1
name: loop_test
steps:
  - type: loop
    id: loop1
    count: 3
    body:
      - type: send_text
        id: inner
        text: "x"
"#;
        let wf = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let steps = wf.steps();
        // Loop with count=3 and 1 inner step should produce 3 steps
        assert_eq!(steps.len(), 3);
        for step in &steps {
            assert_eq!(step.name, "inner");
            assert_eq!(step.description, "Send text");
        }
    }

    #[test]
    fn compile_loop_zero_count_produces_no_steps() {
        let yaml = r#"
workflow_schema_version: 1
name: loop_zero
steps:
  - type: loop
    id: loop1
    count: 0
    body:
      - type: send_text
        id: inner
        text: "x"
  - type: send_text
    id: after
    text: "done"
"#;
        let wf = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let steps = wf.steps();
        // Loop 0 = no unrolled steps, plus 1 trailing step
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].name, "after");
    }

    #[test]
    fn compile_nested_loops_unroll_multiplicatively() {
        let yaml = r#"
workflow_schema_version: 1
name: nested_loop
steps:
  - type: loop
    id: outer
    count: 2
    body:
      - type: loop
        id: inner
        count: 3
        body:
          - type: notify
            id: ping
            message: "ping"
"#;
        let wf = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let steps = wf.steps();
        // 2 * 3 = 6 unrolled steps
        assert_eq!(steps.len(), 6);
        for step in &steps {
            assert_eq!(step.name, "ping");
        }
    }

    #[test]
    fn compile_conditional_produces_jump_and_branch_metadata() {
        let yaml = r#"
workflow_schema_version: 1
name: cond_test
steps:
  - type: conditional
    id: check1
    test_text: "hello"
    matcher:
      kind: substring
      value: "hello"
    then_steps:
      - type: notify
        id: then_step
        message: "matched"
    else_steps:
      - type: notify
        id: else_step
        message: "no match"
"#;
        let wf = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let steps = wf.steps();
        // Conditional produces: JumpIfFalse meta + then_step + Jump meta + else_step = 4
        assert_eq!(steps.len(), 4);
        assert_eq!(steps[0].name, "check1");
        assert!(steps[0].description.contains("Check:"));
        assert_eq!(steps[1].name, "then_step");
        assert_eq!(steps[2].name, "check1_jump");
        assert_eq!(steps[3].name, "else_step");
    }

    #[test]
    fn compile_conditional_empty_else_branch() {
        let yaml = r#"
workflow_schema_version: 1
name: cond_no_else
steps:
  - type: conditional
    id: check
    test_text: "x"
    matcher:
      kind: substring
      value: "x"
    then_steps:
      - type: notify
        id: yes
        message: "yes"
"#;
        let wf = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let steps = wf.steps();
        // JumpIfFalse meta + then_step + Jump meta = 3 (no else steps)
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].name, "check");
        assert_eq!(steps[1].name, "yes");
        assert_eq!(steps[2].name, "check_jump");
    }

    #[test]
    fn compile_mixed_steps_preserves_order() {
        let yaml = r#"
workflow_schema_version: 1
name: mixed
steps:
  - type: send_text
    id: first
    text: "a"
  - type: sleep
    id: second
    duration_ms: 100
  - type: loop
    id: loop1
    count: 2
    body:
      - type: notify
        id: looped
        message: "ping"
  - type: send_text
    id: last
    text: "z"
"#;
        let wf = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let steps = wf.steps();
        // first + second + 2*looped + last = 5
        assert_eq!(steps.len(), 5);
        assert_eq!(steps[0].name, "first");
        assert_eq!(steps[1].name, "second");
        assert_eq!(steps[2].name, "looped");
        assert_eq!(steps[3].name, "looped");
        assert_eq!(steps[4].name, "last");
    }

    // ========================================================================
    // DescriptorWorkflow::handles() — trigger matching
    // ========================================================================

    fn make_detection(rule_id: &str, agent_type: AgentType, event_type: &str) -> Detection {
        Detection {
            rule_id: rule_id.to_string(),
            agent_type,
            event_type: event_type.to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: String::new(),
            span: (0, 0),
        }
    }

    #[test]
    fn handles_returns_false_when_no_triggers() {
        let wf = DescriptorWorkflow::from_yaml_str(minimal_yaml()).unwrap();
        let det = make_detection("wezterm.test", AgentType::Unknown, "test");
        assert!(!wf.handles(&det));
    }

    #[test]
    fn handles_matches_event_type_trigger() {
        let yaml = r#"
workflow_schema_version: 1
name: triggered
triggers:
  - event_types: ["session.compaction"]
steps:
  - type: send_text
    id: s1
    text: "x"
"#;
        let wf = DescriptorWorkflow::from_yaml_str(yaml).unwrap();

        let det = make_detection("wezterm.test", AgentType::Unknown, "session.compaction");
        assert!(wf.handles(&det));

        let det2 = make_detection("wezterm.test", AgentType::Unknown, "other.event");
        assert!(!wf.handles(&det2));
    }

    #[test]
    fn handles_matches_rule_id_trigger() {
        let yaml = r#"
workflow_schema_version: 1
name: rule_triggered
triggers:
  - rule_ids: ["wezterm.stuck_detection"]
steps:
  - type: send_text
    id: s1
    text: "x"
"#;
        let wf = DescriptorWorkflow::from_yaml_str(yaml).unwrap();

        let det = make_detection("wezterm.stuck_detection", AgentType::Unknown, "any");
        assert!(wf.handles(&det));

        let det2 = make_detection("wezterm.other_rule", AgentType::Unknown, "any");
        assert!(!wf.handles(&det2));
    }

    #[test]
    fn handles_matches_agent_type_trigger() {
        let yaml = r#"
workflow_schema_version: 1
name: agent_triggered
triggers:
  - agent_types: ["codex"]
steps:
  - type: send_text
    id: s1
    text: "x"
"#;
        let wf = DescriptorWorkflow::from_yaml_str(yaml).unwrap();

        let det = make_detection("wezterm.test", AgentType::Codex, "any");
        assert!(wf.handles(&det));

        let det2 = make_detection("wezterm.test", AgentType::ClaudeCode, "any");
        assert!(!wf.handles(&det2));
    }

    #[test]
    fn handles_requires_all_trigger_conditions_to_match() {
        let yaml = r#"
workflow_schema_version: 1
name: combined
triggers:
  - event_types: ["stuck"]
    agent_types: ["codex"]
    rule_ids: ["wezterm.stuck"]
steps:
  - type: send_text
    id: s1
    text: "x"
"#;
        let wf = DescriptorWorkflow::from_yaml_str(yaml).unwrap();

        // All three match
        let det = make_detection("wezterm.stuck", AgentType::Codex, "stuck");
        assert!(wf.handles(&det));

        // Event matches but agent doesn't
        let det2 = make_detection("wezterm.stuck", AgentType::ClaudeCode, "stuck");
        assert!(!wf.handles(&det2));

        // Agent matches but rule doesn't
        let det3 = make_detection("wezterm.other", AgentType::Codex, "stuck");
        assert!(!wf.handles(&det3));
    }

    #[test]
    fn handles_empty_trigger_fields_match_anything() {
        let yaml = r#"
workflow_schema_version: 1
name: open_trigger
triggers:
  - event_types: ["stuck"]
steps:
  - type: send_text
    id: s1
    text: "x"
"#;
        let wf = DescriptorWorkflow::from_yaml_str(yaml).unwrap();

        // Empty agent_types and rule_ids means any agent/rule matches
        let det = make_detection("wezterm.anything", AgentType::Gemini, "stuck");
        assert!(wf.handles(&det));
    }

    #[test]
    fn handles_any_of_multiple_triggers() {
        let yaml = r#"
workflow_schema_version: 1
name: multi_trigger
triggers:
  - event_types: ["error"]
  - event_types: ["timeout"]
steps:
  - type: send_text
    id: s1
    text: "x"
"#;
        let wf = DescriptorWorkflow::from_yaml_str(yaml).unwrap();

        let det1 = make_detection("wezterm.test", AgentType::Unknown, "error");
        assert!(wf.handles(&det1));

        let det2 = make_detection("wezterm.test", AgentType::Unknown, "timeout");
        assert!(wf.handles(&det2));

        let det3 = make_detection("wezterm.test", AgentType::Unknown, "success");
        assert!(!wf.handles(&det3));
    }

    // ========================================================================
    // DescriptorMatcher validation edge cases
    // ========================================================================

    #[test]
    fn matcher_validate_valid_substring() {
        let matcher = DescriptorMatcher::Substring {
            value: "hello".into(),
        };
        assert!(matcher.validate(&DescriptorLimits::default()).is_ok());
    }

    #[test]
    fn matcher_validate_valid_regex() {
        let matcher = DescriptorMatcher::Regex {
            pattern: r"\d+".into(),
        };
        assert!(matcher.validate(&DescriptorLimits::default()).is_ok());
    }

    #[test]
    fn matcher_validate_empty_substring_ok() {
        let matcher = DescriptorMatcher::Substring {
            value: String::new(),
        };
        assert!(matcher.validate(&DescriptorLimits::default()).is_ok());
    }

    #[test]
    fn matcher_validate_complex_regex() {
        let matcher = DescriptorMatcher::Regex {
            pattern: r"^(ERROR|WARN|FATAL):\s+.*$".into(),
        };
        assert!(matcher.validate(&DescriptorLimits::default()).is_ok());
    }

    #[test]
    fn matcher_to_text_match_substring_produces_text_match() {
        let matcher = DescriptorMatcher::Substring {
            value: "prompt".into(),
        };
        let tm = matcher.to_text_match();
        // TextMatch is a non-executable descriptor; just verify conversion succeeds
        assert!(format!("{tm:?}").contains("prompt"));
    }

    #[test]
    fn matcher_to_text_match_regex_produces_text_match() {
        let matcher = DescriptorMatcher::Regex {
            pattern: r"ERROR:\s+\d+".into(),
        };
        let tm = matcher.to_text_match();
        assert!(format!("{tm:?}").contains("ERROR"));
    }

    // ========================================================================
    // DescriptorTrigger serde
    // ========================================================================

    #[test]
    fn trigger_serde_roundtrip() {
        let trigger = DescriptorTrigger {
            event_types: vec!["stuck".into(), "timeout".into()],
            agent_types: vec!["codex".into()],
            rule_ids: vec!["wezterm.stuck".into()],
        };
        let json = serde_json::to_string(&trigger).unwrap();
        let parsed: DescriptorTrigger = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.event_types, trigger.event_types);
        assert_eq!(parsed.agent_types, trigger.agent_types);
        assert_eq!(parsed.rule_ids, trigger.rule_ids);
    }

    #[test]
    fn trigger_defaults_empty_fields() {
        let json = r"{}";
        let trigger: DescriptorTrigger = serde_json::from_str(json).unwrap();
        assert!(trigger.event_types.is_empty());
        assert!(trigger.agent_types.is_empty());
        assert!(trigger.rule_ids.is_empty());
    }

    // ========================================================================
    // DescriptorWorkflow Workflow trait impl
    // ========================================================================

    #[test]
    fn workflow_trait_name_and_description() {
        let yaml = r#"
workflow_schema_version: 1
name: my_flow
description: "Does something cool"
steps:
  - type: send_text
    id: s1
    text: "x"
"#;
        let wf = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        assert_eq!(wf.name(), "my_flow");
        assert_eq!(wf.description(), "Does something cool");
    }

    #[test]
    fn workflow_trait_default_description() {
        let wf = DescriptorWorkflow::from_yaml_str(minimal_yaml()).unwrap();
        assert_eq!(wf.description(), "Descriptor workflow");
    }

    #[test]
    fn workflow_trait_steps_returns_metadata() {
        let yaml = r#"
workflow_schema_version: 1
name: multi
steps:
  - type: wait_for
    id: w1
    matcher:
      kind: substring
      value: "$"
  - type: send_text
    id: t1
    text: "ls\n"
  - type: sleep
    id: s1
    duration_ms: 500
"#;
        let wf = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let steps = wf.steps();
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].name, "w1");
        assert_eq!(steps[1].name, "t1");
        assert_eq!(steps[2].name, "s1");
    }

    // ========================================================================
    // Validation: edge cases not yet covered
    // ========================================================================

    #[test]
    fn validate_wait_for_with_custom_timeout() {
        let yaml = r#"
workflow_schema_version: 1
name: wait_custom
steps:
  - type: wait_for
    id: w1
    matcher:
      kind: substring
      value: "$"
    timeout_ms: 5000
"#;
        assert!(DescriptorWorkflow::from_yaml_str(yaml).is_ok());
    }

    #[test]
    fn validate_send_text_with_wait_for() {
        let yaml = r#"
workflow_schema_version: 1
name: send_wait
steps:
  - type: send_text
    id: s1
    text: "ls\n"
    wait_for:
      kind: regex
      pattern: "\\$"
    wait_timeout_ms: 3000
"#;
        assert!(DescriptorWorkflow::from_yaml_str(yaml).is_ok());
    }

    #[test]
    fn validate_max_steps_exactly_at_limit() {
        let mut steps_yaml = String::new();
        for i in 0..DESCRIPTOR_MAX_STEPS {
            steps_yaml.push_str(&format!(
                "  - type: notify\n    id: s{i}\n    message: \"m\"\n"
            ));
        }
        let yaml = format!(
            "workflow_schema_version: 1\nname: max_steps\nsteps:\n{steps_yaml}"
        );
        assert!(DescriptorWorkflow::from_yaml_str(&yaml).is_ok());
    }

    #[test]
    fn validate_one_over_max_steps_fails() {
        let mut steps_yaml = String::new();
        for i in 0..=DESCRIPTOR_MAX_STEPS {
            steps_yaml.push_str(&format!(
                "  - type: notify\n    id: s{i}\n    message: \"m\"\n"
            ));
        }
        let yaml = format!(
            "workflow_schema_version: 1\nname: over_max\nsteps:\n{steps_yaml}"
        );
        assert!(DescriptorWorkflow::from_yaml_str(&yaml).is_err());
    }

    #[test]
    fn validate_loop_max_iterations_boundary() {
        let yaml = r#"
workflow_schema_version: 1
name: loop_max
steps:
  - type: loop
    id: loop1
    count: 1000
    body:
      - type: notify
        id: inner
        message: "x"
"#;
        assert!(DescriptorWorkflow::from_yaml_str(yaml).is_ok());
    }

    #[test]
    fn validate_loop_over_max_iterations_fails() {
        let yaml = r#"
workflow_schema_version: 1
name: loop_over
steps:
  - type: loop
    id: loop1
    count: 1001
    body:
      - type: notify
        id: inner
        message: "x"
"#;
        assert!(DescriptorWorkflow::from_yaml_str(yaml).is_err());
    }

    // ========================================================================
    // DescriptorWorkflow::descriptor() accessor
    // ========================================================================

    #[test]
    fn descriptor_accessor_returns_original() {
        let yaml = r#"
workflow_schema_version: 1
name: accessor_test
description: "Desc"
steps:
  - type: notify
    id: n1
    message: "hello"
"#;
        let wf = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let desc = wf.descriptor();
        assert_eq!(desc.name, "accessor_test");
        assert_eq!(desc.description.as_deref(), Some("Desc"));
        assert_eq!(desc.steps.len(), 1);
        assert!(desc.triggers.is_empty());
        assert!(desc.on_failure.is_none());
    }
}
