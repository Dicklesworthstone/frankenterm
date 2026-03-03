#![cfg(feature = "subprocess-bridge")]

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use frankenterm_core::fleet_launcher::{
    FleetLaunchStatus, LaunchOutcome, LaunchPlan, StartupStrategy,
};
use frankenterm_core::mission_agent_mail::{
    AckRequirementReport, CoordinationEventKind, CoordinationEventRequest,
    CoordinationInboxMessage, MissionAgentMailKernel, MissionCoordinationContext,
    MissionMailTransport,
};

#[derive(Default)]
struct LoopbackTransport {
    next_id: RefCell<u64>,
    sent: RefCell<Vec<(String, String, String, String)>>,
    inbox: RefCell<HashMap<String, Vec<CoordinationInboxMessage>>>,
    fail_once: RefCell<HashSet<String>>,
}

impl LoopbackTransport {
    fn fail_next_send_to(&self, recipient: &str) {
        self.fail_once.borrow_mut().insert(recipient.to_string());
    }
}

impl MissionMailTransport for LoopbackTransport {
    fn send_message(&self, to: &str, subject: &str, body: &str) -> Result<String, String> {
        if self.fail_once.borrow_mut().remove(to) {
            return Err("injected transport failure".to_string());
        }

        let mut next = self.next_id.borrow_mut();
        *next += 1;
        let id = format!("loopback-{next}");
        self.sent.borrow_mut().push((
            id.clone(),
            to.to_string(),
            subject.to_string(),
            body.to_string(),
        ));

        self.inbox
            .borrow_mut()
            .entry(to.to_string())
            .or_default()
            .push(CoordinationInboxMessage {
                id: Some(id.clone()),
                from: Some("mission-kernel".to_string()),
                subject: subject.to_string(),
                body: body.to_string(),
                read: false,
                thread_id: None,
                timestamp: None,
                ack_required: None,
            });

        Ok(id)
    }

    fn fetch_inbox(&self, agent_name: &str) -> Vec<CoordinationInboxMessage> {
        self.inbox
            .borrow()
            .get(agent_name)
            .cloned()
            .unwrap_or_default()
    }
}

fn sample_context() -> MissionCoordinationContext {
    MissionCoordinationContext {
        mission_id: "ft-3681t.3.4".to_string(),
        fleet_id: Some("fleet-alpha".to_string()),
        bead_id: Some("ft-3681t.3.4".to_string()),
        assignment_id: Some("assignment-x".to_string()),
        thread_id: None,
        correlation_id: "corr-it-001".to_string(),
        scenario_id: Some("integration".to_string()),
    }
}

#[test]
fn conflict_path_degrades_then_recovers() {
    let transport = LoopbackTransport::default();
    transport.fail_next_send_to("AgentB");
    let kernel = MissionAgentMailKernel::new(transport);

    let first = kernel.emit_handoff_notice_at(
        1_000,
        vec!["AgentB".to_string()],
        sample_context(),
        "handoff to AgentB",
        "taking over scheduler shard",
    );
    assert_eq!(first.attempted, 1);
    assert!(first.delivered.is_empty());
    assert_eq!(first.failed.len(), 1);

    let second = kernel.emit_handoff_notice_at(
        2_000,
        vec!["AgentB".to_string()],
        sample_context(),
        "handoff retry",
        "retrying after transient transport fault",
    );
    assert_eq!(second.attempted, 1);
    assert_eq!(second.delivered.len(), 1);
    assert!(second.failed.is_empty());
}

#[test]
fn ack_required_roundtrip_is_enforceable() {
    let transport = LoopbackTransport::default();
    let kernel = MissionAgentMailKernel::new(transport);

    let dispatch = kernel.emit_start_notice_at(
        10_000,
        vec!["AgentX".to_string()],
        sample_context(),
        "start mission slice",
        "beginning deterministic validation",
    );
    assert_eq!(dispatch.delivered.len(), 1);
    let original_id = dispatch.delivered[0].message_id.clone();

    let pending: AckRequirementReport =
        kernel.collect_pending_acknowledgements_at("AgentX", 10_010);
    assert_eq!(pending.pending.len(), 1);
    assert_eq!(pending.pending[0].message_id, original_id);

    let ack_report =
        kernel.emit_acknowledgements_at(10_050, "AgentX", &pending.pending, "acknowledged");
    assert_eq!(ack_report.delivered.len(), 1);
    assert!(ack_report.failed.is_empty());

    let coordinator_inbox = kernel.consume_inbox("mission-kernel");
    assert!(coordinator_inbox.parsed.iter().any(|message| {
        message.envelope.event_kind == CoordinationEventKind::Acknowledgement
            && message
                .envelope
                .metadata
                .get("ack_for_message_id")
                .is_some_and(|value| value == &original_id)
    }));
}

#[test]
fn explicit_event_request_roundtrips_through_loopback_inbox() {
    let kernel = MissionAgentMailKernel::new(LoopbackTransport::default());

    let mut metadata = HashMap::new();
    metadata.insert("step".to_string(), "2".to_string());
    metadata.insert("decision".to_string(), "escalate".to_string());

    let dispatch = kernel.emit_event_at(
        20_000,
        CoordinationEventRequest {
            kind: CoordinationEventKind::ProgressUpdate,
            summary: "dispatch progress".to_string(),
            body: "completed 2/5 assignments".to_string(),
            recipients: vec!["Observer".to_string()],
            ack_required: false,
            context: sample_context(),
            reason_code: "mission.progress_update".to_string(),
            error_code: None,
            metadata,
        },
    );
    assert_eq!(dispatch.delivered.len(), 1);

    let report = kernel.consume_inbox("Observer");
    assert_eq!(report.parsed.len(), 1);
    assert_eq!(
        report.parsed[0].envelope.event_kind,
        CoordinationEventKind::ProgressUpdate
    );
    assert_eq!(
        report.parsed[0]
            .envelope
            .metadata
            .get("step")
            .map(String::as_str),
        Some("2")
    );
}

#[test]
fn fleet_launch_emitters_roundtrip_with_fleet_context() {
    let kernel = MissionAgentMailKernel::new(LoopbackTransport::default());
    let plan = LaunchPlan {
        name: "fleet-zeta".to_string(),
        slots: Vec::new(),
        layout_template: None,
        strategy: StartupStrategy::Phased,
        phases: Vec::new(),
        generation: 9,
        workspace_id: "ws-prod".to_string(),
        domain: "local".to_string(),
        planned_at: 100,
        warnings: Vec::new(),
    };

    let start = kernel.emit_fleet_launch_start_notice_at(
        30_000,
        vec!["Ops".to_string()],
        &plan,
        "corr-fleet-001",
        Some("scenario-fleet".to_string()),
    );
    assert_eq!(start.delivered.len(), 1);

    let progress = kernel.emit_fleet_launch_progress_update_at(
        30_100,
        vec!["Ops".to_string()],
        &plan,
        1,
        2,
        "corr-fleet-001",
        Some("scenario-fleet".to_string()),
    );
    assert_eq!(progress.delivered.len(), 1);

    let outcome = LaunchOutcome {
        name: "fleet-zeta".to_string(),
        slot_outcomes: Vec::new(),
        status: FleetLaunchStatus::Partial,
        registry_snapshot: Vec::new(),
        completed_at: 30_200,
        total_slots: 4,
        successful_slots: 3,
        failed_slots: 1,
        pre_launch_checkpoint: Some(77),
        bootstrap_dispatches: vec![(0, 2)],
    };
    let finish = kernel.emit_fleet_launch_outcome_notice_at(
        30_200,
        vec!["Ops".to_string()],
        &outcome,
        "ws-prod",
        "local",
        9,
        "corr-fleet-001",
        Some("scenario-fleet".to_string()),
    );
    assert_eq!(finish.delivered.len(), 1);

    let inbox = kernel.consume_inbox("Ops");
    assert_eq!(inbox.parsed.len(), 3);

    let mut by_reason = HashMap::new();
    for message in inbox.parsed {
        by_reason.insert(message.envelope.reason_code.clone(), message.envelope);
    }

    let start_env = by_reason
        .get("mission.fleet_launch_start")
        .expect("start notice envelope");
    assert!(start_env.ack_required);
    assert_eq!(start_env.context.fleet_id.as_deref(), Some("fleet-zeta"));
    assert_eq!(
        start_env.context.canonical_thread_id(),
        "fleet-launch-fleet-zeta-g9"
    );

    let progress_env = by_reason
        .get("mission.fleet_launch_progress")
        .expect("progress envelope");
    assert!(!progress_env.ack_required);
    assert_eq!(
        progress_env
            .metadata
            .get("started_slots")
            .map(String::as_str),
        Some("2")
    );

    let outcome_env = by_reason
        .get("mission.fleet_launch_outcome")
        .expect("outcome envelope");
    assert_eq!(
        outcome_env.metadata.get("status").map(String::as_str),
        Some("partial")
    );
    assert_eq!(
        outcome_env
            .metadata
            .get("successful_slots")
            .map(String::as_str),
        Some("3")
    );
}
