#![no_main]

use arbitrary::Arbitrary;
use frankenterm_core::workflows::{DescriptorWorkflow, WorkflowDescriptor};
use libfuzzer_sys::fuzz_target;
use serde::Serialize;

const MAX_TEXT_LEN: usize = 128;
const MAX_LIST_ITEMS: usize = 4;
const MAX_STEPS: usize = 8;

#[derive(Arbitrary, Debug)]
struct RawWorkflow {
    name: String,
    description: Option<String>,
    triggers: Vec<RawTrigger>,
    steps: Vec<RawStep>,
    on_failure: Option<RawFailureHandler>,
}

#[derive(Arbitrary, Debug)]
struct RawTrigger {
    event_types: Vec<String>,
    agent_types: Vec<String>,
    rule_ids: Vec<String>,
}

#[derive(Arbitrary, Debug)]
enum RawFailureHandler {
    Notify { message: String },
    Log { message: String },
    Abort { message: String },
}

#[derive(Arbitrary, Debug)]
enum RawMatcherKind {
    Substring,
    Regex,
}

#[derive(Arbitrary, Debug)]
struct RawMatcher {
    kind: RawMatcherKind,
    value: String,
}

#[derive(Arbitrary, Debug)]
enum RawControlKey {
    CtrlC,
    CtrlD,
    CtrlZ,
}

#[derive(Arbitrary, Debug)]
enum RawStep {
    WaitFor {
        description: Option<String>,
        matcher: RawMatcher,
        timeout_ms: Option<u16>,
    },
    Sleep {
        description: Option<String>,
        duration_ms: u16,
    },
    SendText {
        description: Option<String>,
        text: String,
        wait_for: Option<RawMatcher>,
        wait_timeout_ms: Option<u16>,
    },
    SendCtrl {
        description: Option<String>,
        key: RawControlKey,
    },
    Notify {
        description: Option<String>,
        message: String,
    },
    Log {
        description: Option<String>,
        message: String,
    },
    Abort {
        description: Option<String>,
        reason: String,
    },
}

#[derive(Serialize)]
struct WorkflowYaml {
    workflow_schema_version: u32,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    triggers: Vec<TriggerYaml>,
    steps: Vec<StepYaml>,
    #[serde(skip_serializing_if = "Option::is_none")]
    on_failure: Option<FailureHandlerYaml>,
}

#[derive(Serialize)]
struct TriggerYaml {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    event_types: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    agent_types: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    rule_ids: Vec<String>,
}

#[derive(Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum FailureHandlerYaml {
    Notify { message: String },
    Log { message: String },
    Abort { message: String },
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum MatcherYaml {
    Substring { value: String },
    Regex { pattern: String },
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum ControlKeyYaml {
    CtrlC,
    CtrlD,
    CtrlZ,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StepYaml {
    WaitFor {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        matcher: MatcherYaml,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
    Sleep {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        duration_ms: u64,
    },
    SendText {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        wait_for: Option<MatcherYaml>,
        #[serde(skip_serializing_if = "Option::is_none")]
        wait_timeout_ms: Option<u64>,
    },
    SendCtrl {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        key: ControlKeyYaml,
    },
    Notify {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        message: String,
    },
    Log {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        message: String,
    },
    Abort {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        reason: String,
    },
}

impl RawWorkflow {
    fn into_yaml(self) -> WorkflowYaml {
        let mut steps = self
            .steps
            .into_iter()
            .take(MAX_STEPS)
            .enumerate()
            .map(|(idx, step)| step.into_yaml(idx))
            .collect::<Vec<_>>();

        if steps.is_empty() {
            steps.push(StepYaml::Sleep {
                id: "step_0".to_string(),
                description: Some("generated fallback sleep".to_string()),
                duration_ms: 0,
            });
        }

        WorkflowYaml {
            workflow_schema_version: 1,
            name: limited_text(self.name, MAX_TEXT_LEN, "workflow"),
            description: self
                .description
                .map(|text| limited_text(text, MAX_TEXT_LEN, "desc")),
            triggers: self
                .triggers
                .into_iter()
                .take(MAX_LIST_ITEMS)
                .map(RawTrigger::into_yaml)
                .collect(),
            steps,
            on_failure: self.on_failure.map(RawFailureHandler::into_yaml),
        }
    }
}

impl RawTrigger {
    fn into_yaml(self) -> TriggerYaml {
        TriggerYaml {
            event_types: limited_list(self.event_types),
            agent_types: limited_list(self.agent_types),
            rule_ids: limited_list(self.rule_ids),
        }
    }
}

impl RawFailureHandler {
    fn into_yaml(self) -> FailureHandlerYaml {
        match self {
            Self::Notify { message } => FailureHandlerYaml::Notify {
                message: limited_text(message, MAX_TEXT_LEN, "notify"),
            },
            Self::Log { message } => FailureHandlerYaml::Log {
                message: limited_text(message, MAX_TEXT_LEN, "log"),
            },
            Self::Abort { message } => FailureHandlerYaml::Abort {
                message: limited_text(message, MAX_TEXT_LEN, "abort"),
            },
        }
    }
}

impl RawMatcher {
    fn into_yaml(self) -> MatcherYaml {
        match self.kind {
            RawMatcherKind::Substring => MatcherYaml::Substring {
                value: limited_text(self.value, MAX_TEXT_LEN, "match"),
            },
            RawMatcherKind::Regex => {
                let value = limited_text(self.value, MAX_TEXT_LEN / 2, "regex");
                MatcherYaml::Regex {
                    pattern: format!("^{}$", regex_escape_literal(&value)),
                }
            }
        }
    }
}

impl RawControlKey {
    const fn into_yaml(self) -> ControlKeyYaml {
        match self {
            Self::CtrlC => ControlKeyYaml::CtrlC,
            Self::CtrlD => ControlKeyYaml::CtrlD,
            Self::CtrlZ => ControlKeyYaml::CtrlZ,
        }
    }
}

impl RawStep {
    fn into_yaml(self, idx: usize) -> StepYaml {
        let id = format!("step_{idx}");

        match self {
            Self::WaitFor {
                description,
                matcher,
                timeout_ms,
            } => StepYaml::WaitFor {
                id,
                description: description.map(|text| limited_text(text, MAX_TEXT_LEN, "wait")),
                matcher: matcher.into_yaml(),
                timeout_ms: timeout_ms.map(u64::from),
            },
            Self::Sleep {
                description,
                duration_ms,
            } => StepYaml::Sleep {
                id,
                description: description.map(|text| limited_text(text, MAX_TEXT_LEN, "sleep")),
                duration_ms: u64::from(duration_ms),
            },
            Self::SendText {
                description,
                text,
                wait_for,
                wait_timeout_ms,
            } => StepYaml::SendText {
                id,
                description: description.map(|text| limited_text(text, MAX_TEXT_LEN, "send")),
                text: limited_text(text, MAX_TEXT_LEN, "text"),
                wait_for: wait_for.map(RawMatcher::into_yaml),
                wait_timeout_ms: wait_timeout_ms.map(u64::from),
            },
            Self::SendCtrl { description, key } => StepYaml::SendCtrl {
                id,
                description: description.map(|text| limited_text(text, MAX_TEXT_LEN, "ctrl")),
                key: key.into_yaml(),
            },
            Self::Notify {
                description,
                message,
            } => StepYaml::Notify {
                id,
                description: description.map(|text| limited_text(text, MAX_TEXT_LEN, "notify")),
                message: limited_text(message, MAX_TEXT_LEN, "message"),
            },
            Self::Log {
                description,
                message,
            } => StepYaml::Log {
                id,
                description: description.map(|text| limited_text(text, MAX_TEXT_LEN, "log")),
                message: limited_text(message, MAX_TEXT_LEN, "message"),
            },
            Self::Abort {
                description,
                reason,
            } => StepYaml::Abort {
                id,
                description: description.map(|text| limited_text(text, MAX_TEXT_LEN, "abort")),
                reason: limited_text(reason, MAX_TEXT_LEN, "reason"),
            },
        }
    }
}

fn limited_list(values: Vec<String>) -> Vec<String> {
    values
        .into_iter()
        .take(MAX_LIST_ITEMS)
        .enumerate()
        .map(|(idx, value)| limited_text(value, MAX_TEXT_LEN / 2, &format!("item_{idx}")))
        .collect()
}

fn limited_text(value: String, max_len: usize, fallback: &str) -> String {
    if value.is_empty() {
        return fallback.to_string();
    }

    value.chars().take(max_len).collect()
}

fn regex_escape_literal(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|' => {
                escaped.push('\\');
                escaped.push(ch);
            }
            _ => escaped.push(ch),
        }
    }
    escaped
}

fuzz_target!(|raw: RawWorkflow| {
    let yaml = match serde_yaml::to_string(&raw.into_yaml()) {
        Ok(yaml) => yaml,
        Err(_) => return,
    };

    let descriptor = WorkflowDescriptor::from_yaml_str(&yaml);
    let workflow = DescriptorWorkflow::from_yaml_str(&yaml);
    assert_eq!(descriptor.is_ok(), workflow.is_ok());

    if let (Ok(descriptor), Ok(workflow)) = (descriptor, workflow) {
        let compiled = workflow.descriptor();
        assert_eq!(compiled.name, descriptor.name);
        assert_eq!(compiled.triggers.len(), descriptor.triggers.len());
        assert_eq!(compiled.steps.len(), descriptor.steps.len());
    }
});
