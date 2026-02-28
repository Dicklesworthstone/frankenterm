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
pub(crate) const DESCRIPTOR_MAX_STEPS: usize = 32;
pub(crate) const DESCRIPTOR_MAX_WAIT_TIMEOUT_MS: u64 = 120_000;
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
