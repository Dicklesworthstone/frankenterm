#![no_main]

use std::collections::HashMap;
use std::fs;

use arbitrary::Arbitrary;
use frankenterm_core::config::PatternsConfig;
use frankenterm_core::patterns::{AgentType, PatternEngine, RuleDef, Severity};
use libfuzzer_sys::fuzz_target;
use serde::Serialize;
use tempfile::tempdir;
use toml::Value;

const MAX_TEXT_LEN: usize = 64;
const MAX_RULES: usize = 6;
const MAX_ANCHORS: usize = 4;
const MAX_TOML_BYTES: usize = 64 * 1024;

#[derive(Arbitrary, Debug, Clone)]
struct RawPackInput {
    load_mode: LoadMode,
    decode_mode: DecodeMode,
    wire_mode: WireMode,
    pack_name: String,
    version: String,
    rules: Vec<RawRule>,
}

#[derive(Arbitrary, Debug, Clone)]
enum LoadMode {
    ExplicitFile,
    WorkspaceDiscovery,
    WorkspaceRulesDir,
    UserDiscovery,
}

#[derive(Arbitrary, Debug, Clone)]
enum DecodeMode {
    Valid,
    Missing(MissingField),
    Mistyped(WrongField),
}

#[derive(Arbitrary, Debug, Clone)]
enum MissingField {
    Name,
    Version,
    Rules,
    Anchors,
    AgentType,
}

#[derive(Arbitrary, Debug, Clone)]
enum WrongField {
    Rules,
    Anchors,
    AgentType,
    Regex,
}

#[derive(Arbitrary, Debug, Clone)]
enum WireMode {
    Exact,
    Truncate(u8),
    AppendGarbage(RawScalar),
}

#[derive(Arbitrary, Debug, Clone)]
struct RawRule {
    agent_type: RawAgentType,
    severity: RawSeverity,
    event_type: String,
    description: String,
    anchors: Vec<String>,
    regex: Option<String>,
    remediation: Option<String>,
    workflow: Option<String>,
    manual_fix: Option<String>,
    preview_command: Option<String>,
    learn_more_url: Option<String>,
}

#[derive(Arbitrary, Debug, Clone, Copy)]
enum RawAgentType {
    Codex,
    ClaudeCode,
    Gemini,
    Wezterm,
}

#[derive(Arbitrary, Debug, Clone, Copy)]
enum RawSeverity {
    Info,
    Warning,
    Critical,
}

#[derive(Arbitrary, Debug, Clone)]
enum RawScalar {
    Null,
    Bool(bool),
    Int(i64),
    Text(String),
}

#[derive(Serialize)]
struct PackToml {
    name: String,
    version: String,
    rules: Vec<RuleDef>,
}

impl RawPackInput {
    fn into_toml_bytes(self) -> Option<(Vec<u8>, bool)> {
        let exact_valid = matches!(self.decode_mode, DecodeMode::Valid)
            && matches!(self.wire_mode, WireMode::Exact);
        let pack = self.build_pack();
        let mut value = Value::try_from(pack).ok()?;
        apply_decode_mode(&mut value, &self.decode_mode);
        let mut bytes = toml::to_string(&value).ok()?.into_bytes();
        apply_wire_mode(&mut bytes, &self.wire_mode);

        if bytes.len() > MAX_TOML_BYTES {
            return None;
        }

        Some((bytes, exact_valid))
    }

    fn build_pack(&self) -> PackToml {
        let mut rules = self
            .rules
            .iter()
            .take(MAX_RULES)
            .enumerate()
            .map(|(idx, rule)| rule.build_rule(idx))
            .collect::<Vec<_>>();

        if rules.is_empty() {
            rules.push(RawRule::fallback().build_rule(0));
        }

        PackToml {
            name: limited_text(self.pack_name.clone(), "generated-pack"),
            version: limited_text(self.version.clone(), "0.1.0"),
            rules,
        }
    }
}

impl RawRule {
    fn build_rule(&self, index: usize) -> RuleDef {
        let agent_type = self.agent_type.to_agent_type();
        let prefix = self.agent_type.rule_prefix();
        let mut anchors = self
            .anchors
            .iter()
            .take(MAX_ANCHORS)
            .map(|anchor| limited_text(anchor.clone(), "anchor"))
            .filter(|anchor| !anchor.trim().is_empty())
            .collect::<Vec<_>>();

        if anchors.is_empty() {
            anchors.push("anchor".to_string());
        }

        RuleDef {
            id: format!("{prefix}.generated_{index}"),
            agent_type,
            event_type: limited_text(self.event_type.clone(), "usage.warning"),
            severity: self.severity.to_severity(),
            anchors,
            regex: self
                .regex
                .clone()
                .map(|regex| limited_text(regex, "anchor.*")),
            description: limited_text(self.description.clone(), "generated rule"),
            remediation: self
                .remediation
                .clone()
                .map(|text| limited_text(text, "retry")),
            workflow: self
                .workflow
                .clone()
                .map(|text| limited_text(text, "recover")),
            manual_fix: self
                .manual_fix
                .clone()
                .map(|text| limited_text(text, "inspect")),
            preview_command: self
                .preview_command
                .clone()
                .map(|text| limited_text(text, "echo preview")),
            learn_more_url: self
                .learn_more_url
                .clone()
                .map(|text| limited_text(text, "https://example.invalid/rule")),
        }
    }

    fn fallback() -> Self {
        Self {
            agent_type: RawAgentType::Codex,
            severity: RawSeverity::Info,
            event_type: "usage.warning".to_string(),
            description: "fallback rule".to_string(),
            anchors: vec!["anchor".to_string()],
            regex: Some("anchor.*".to_string()),
            remediation: None,
            workflow: None,
            manual_fix: None,
            preview_command: None,
            learn_more_url: None,
        }
    }
}

impl RawAgentType {
    fn to_agent_type(self) -> AgentType {
        match self {
            Self::Codex => AgentType::Codex,
            Self::ClaudeCode => AgentType::ClaudeCode,
            Self::Gemini => AgentType::Gemini,
            Self::Wezterm => AgentType::Wezterm,
        }
    }

    fn rule_prefix(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude_code",
            Self::Gemini => "gemini",
            Self::Wezterm => "wezterm",
        }
    }
}

impl RawSeverity {
    fn to_severity(self) -> Severity {
        match self {
            Self::Info => Severity::Info,
            Self::Warning => Severity::Warning,
            Self::Critical => Severity::Critical,
        }
    }
}

impl RawScalar {
    fn into_toml(self) -> Value {
        match self {
            Self::Null => Value::String("null".to_string()),
            Self::Bool(value) => Value::Boolean(value),
            Self::Int(value) => Value::Integer(value),
            Self::Text(value) => Value::String(limited_text(value, "garbage")),
        }
    }
}

fn apply_decode_mode(value: &mut Value, decode_mode: &DecodeMode) {
    match decode_mode {
        DecodeMode::Valid => {}
        DecodeMode::Missing(field) => apply_missing_field(value, field),
        DecodeMode::Mistyped(field) => apply_mistyped_field(value, field),
    }
}

fn apply_missing_field(value: &mut Value, field: &MissingField) {
    let Some(root) = value.as_table_mut() else {
        return;
    };

    match field {
        MissingField::Name => {
            root.remove("name");
        }
        MissingField::Version => {
            root.remove("version");
        }
        MissingField::Rules => {
            root.remove("rules");
        }
        MissingField::Anchors => {
            if let Some(rule) = first_rule_table_mut(root) {
                rule.remove("anchors");
            }
        }
        MissingField::AgentType => {
            if let Some(rule) = first_rule_table_mut(root) {
                rule.remove("agent_type");
            }
        }
    }
}

fn apply_mistyped_field(value: &mut Value, field: &WrongField) {
    let Some(root) = value.as_table_mut() else {
        return;
    };

    match field {
        WrongField::Rules => {
            root.insert("rules".to_string(), Value::String("wrong".to_string()));
        }
        WrongField::Anchors => {
            if let Some(rule) = first_rule_table_mut(root) {
                rule.insert("anchors".to_string(), Value::String("wrong".to_string()));
            }
        }
        WrongField::AgentType => {
            if let Some(rule) = first_rule_table_mut(root) {
                rule.insert("agent_type".to_string(), Value::Integer(7));
            }
        }
        WrongField::Regex => {
            if let Some(rule) = first_rule_table_mut(root) {
                rule.insert("regex".to_string(), Value::String("[".to_string()));
            }
        }
    }
}

fn first_rule_table_mut(
    root: &mut toml::map::Map<String, Value>,
) -> Option<&mut toml::map::Map<String, Value>> {
    root.get_mut("rules")?
        .as_array_mut()?
        .first_mut()?
        .as_table_mut()
}

fn apply_wire_mode(bytes: &mut Vec<u8>, wire_mode: &WireMode) {
    match wire_mode {
        WireMode::Exact => {}
        WireMode::Truncate(seed) => {
            if bytes.is_empty() {
                return;
            }
            let keep = bytes
                .len()
                .saturating_sub(usize::from(*seed) % (bytes.len() + 1));
            bytes.truncate(keep);
        }
        WireMode::AppendGarbage(scalar) => {
            let Ok(mut trailer) =
                toml::to_string(&scalar.clone().into_toml()).map(String::into_bytes)
            else {
                return;
            };
            bytes.append(&mut trailer);
        }
    }
}

fn limited_text(value: String, fallback: &str) -> String {
    if value.is_empty() {
        return fallback.to_string();
    }

    value.chars().take(MAX_TEXT_LEN).collect()
}

fn write_pack_for_mode(
    dir: &std::path::Path,
    load_mode: &LoadMode,
    bytes: &[u8],
) -> Option<PatternsConfig> {
    let mut config = PatternsConfig {
        packs: Vec::new(),
        pack_overrides: HashMap::new(),
        quick_reject_enabled: true,
        user_packs_enabled: false,
        user_packs_dir: None,
    };

    match load_mode {
        LoadMode::ExplicitFile => {
            let path = dir.join("attacker.toml");
            fs::write(&path, bytes).ok()?;
            config.packs.push("file:attacker.toml".to_string());
        }
        LoadMode::WorkspaceDiscovery => {
            let patterns_dir = dir.join(".ft").join("patterns");
            fs::create_dir_all(&patterns_dir).ok()?;
            fs::write(patterns_dir.join("attacker.toml"), bytes).ok()?;
        }
        LoadMode::WorkspaceRulesDir => {
            let pack_dir = dir.join(".ft").join("patterns").join("attacker");
            fs::create_dir_all(&pack_dir).ok()?;
            fs::write(pack_dir.join("rules.toml"), bytes).ok()?;
        }
        LoadMode::UserDiscovery => {
            let user_dir = dir.join("user-patterns");
            fs::create_dir_all(&user_dir).ok()?;
            fs::write(user_dir.join("attacker.toml"), bytes).ok()?;
            config.user_packs_enabled = true;
            config.user_packs_dir = Some(user_dir.to_string_lossy().into_owned());
        }
    }

    Some(config)
}

fuzz_target!(|raw: RawPackInput| {
    let Some((bytes, exact_valid)) = raw.clone().into_toml_bytes() else {
        return;
    };

    let Ok(root) = tempdir() else {
        return;
    };

    let Some(config) = write_pack_for_mode(root.path(), &raw.load_mode, &bytes) else {
        return;
    };

    let engine = PatternEngine::from_config_with_root(&config, Some(root.path()));

    if let Ok(engine) = engine {
        assert!(!engine.packs().is_empty());
        assert!(engine.rules().iter().all(|rule| !rule.anchors.is_empty()));

        if exact_valid {
            assert!(!engine.rules().is_empty());
        }
    }
});
