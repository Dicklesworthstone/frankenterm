#![no_main]
//! Pattern rule remediation/preview template interpolation fuzzer.
//!
//! Pattern packs can provide `manual_fix` and `preview_command` templates that
//! are interpolated when a rule emits remediation guidance. This target keeps
//! that string boundary covered for arbitrary UTF-8 templates, large numeric
//! pane/event identifiers, and rule IDs that may themselves contain braces.

use std::time::Instant;

use frankenterm_core::patterns::{AgentType, RuleDef, Severity};
use libfuzzer_sys::arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;

const MAX_TEMPLATE_BYTES: usize = 4096;
const MAX_RULE_ID_BYTES: usize = 512;
const MAX_EVAL_MS: u128 = 20;

#[derive(Debug)]
struct FuzzCase {
    preview_template: String,
    manual_template: String,
    rule_id: String,
    pane_id: u64,
    event_id: Option<i64>,
    agent_type: AgentType,
}

impl<'a> Arbitrary<'a> for FuzzCase {
    fn arbitrary(u: &mut Unstructured<'a>) -> libfuzzer_sys::arbitrary::Result<Self> {
        Ok(Self {
            preview_template: bounded_utf8(u, MAX_TEMPLATE_BYTES)?,
            manual_template: bounded_utf8(u, MAX_TEMPLATE_BYTES)?,
            rule_id: bounded_rule_id(u)?,
            pane_id: u.arbitrary()?,
            event_id: if u.arbitrary::<bool>()? {
                Some(u.arbitrary()?)
            } else {
                None
            },
            agent_type: agent_type(u.arbitrary()?),
        })
    }
}

fn bounded_utf8(
    u: &mut Unstructured<'_>,
    max_len: usize,
) -> libfuzzer_sys::arbitrary::Result<String> {
    let len = u.int_in_range(0..=max_len)?;
    Ok(String::from_utf8_lossy(u.bytes(len)?).into_owned())
}

fn bounded_rule_id(u: &mut Unstructured<'_>) -> libfuzzer_sys::arbitrary::Result<String> {
    let raw = bounded_utf8(u, MAX_RULE_ID_BYTES)?;
    if raw.trim().is_empty() {
        Ok("codex.fuzz_template".to_string())
    } else {
        Ok(raw)
    }
}

fn agent_type(tag: u8) -> AgentType {
    match tag % 5 {
        0 => AgentType::Codex,
        1 => AgentType::ClaudeCode,
        2 => AgentType::Gemini,
        3 => AgentType::Wezterm,
        _ => AgentType::Unknown,
    }
}

fn expanded_len_upper_bound(
    template: &str,
    pane_id: u64,
    event_id: Option<i64>,
    agent_type: AgentType,
    rule_id: &str,
) -> usize {
    let pane = pane_id.to_string();
    let event = event_id.map_or_else(|| "unknown".to_string(), |id| id.to_string());
    let agent = agent_type.to_string();

    template.len()
        + template.matches("{pane}").count() * pane.len()
        + template.matches("{event_id}").count() * event.len()
        + template.matches("{agent}").count() * agent.len()
        + template.matches("{rule_id}").count() * rule_id.len()
}

fuzz_target!(|case: FuzzCase| {
    let start = Instant::now();

    let preview = RuleDef::interpolate_template(
        &case.preview_template,
        case.pane_id,
        case.event_id,
        &case.agent_type,
        &case.rule_id,
    );
    let manual = RuleDef::interpolate_template(
        &case.manual_template,
        case.pane_id,
        case.event_id,
        &case.agent_type,
        &case.rule_id,
    );

    let preview_again = RuleDef::interpolate_template(
        &case.preview_template,
        case.pane_id,
        case.event_id,
        &case.agent_type,
        &case.rule_id,
    );
    assert_eq!(preview, preview_again);

    assert!(
        preview.len()
            <= expanded_len_upper_bound(
                &case.preview_template,
                case.pane_id,
                case.event_id,
                case.agent_type,
                &case.rule_id,
            )
    );
    assert!(
        manual.len()
            <= expanded_len_upper_bound(
                &case.manual_template,
                case.pane_id,
                case.event_id,
                case.agent_type,
                &case.rule_id,
            )
    );

    let rule = RuleDef {
        id: case.rule_id,
        agent_type: case.agent_type,
        event_type: "fuzz.template".to_string(),
        severity: Severity::Info,
        anchors: vec!["fuzz".to_string()],
        regex: None,
        description: "fuzz template interpolation".to_string(),
        remediation: None,
        workflow: None,
        manual_fix: Some(case.manual_template),
        preview_command: Some(case.preview_template),
        learn_more_url: None,
    };

    assert_eq!(
        rule.get_preview_command(case.pane_id, case.event_id),
        Some(preview)
    );
    assert_eq!(
        rule.get_manual_fix(case.pane_id, case.event_id),
        Some(manual)
    );

    assert!(
        start.elapsed().as_millis() <= MAX_EVAL_MS,
        "template interpolation exceeded {MAX_EVAL_MS}ms"
    );
});
