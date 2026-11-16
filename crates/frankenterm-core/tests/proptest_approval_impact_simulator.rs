//! Property tests for [`approval_impact_simulator`] (ft-1650n.14).
//!
//! Pins the simulator's gate invariants over arbitrary
//! `ProposedAction`s. Complements the substrate's unit tests
//! (13 cases including the linter-extended preview/audit
//! comparison surface).
//!
//! Properties pinned here:
//!
//! 1. **simulate_impact is pure** — same input → same output.
//! 2. **Secret credentials always force manual** —
//!    `CredentialClass::Secret` ⇒ `ImpactReport::ManualApprovalRequired`,
//!    regardless of unknowns / rollback / blast radius.
//! 3. **Non-empty unknowns force manual** — any non-empty
//!    `unknowns` list ⇒ `ManualApprovalRequired` with at least
//!    one reason.
//! 4. **Trivial actions don't require rollback** — when an
//!    action has no commands, no touched files, and `None`
//!    credential class, missing `rollback_plan` is fine.
//! 5. **Non-trivial without rollback forces manual** — if any
//!    of (commands, files, non-None credentials) is non-empty
//!    AND `rollback_plan` is None ⇒ ManualApprovalRequired.
//! 6. **Blast radius matches input shape** — when the simulator
//!    returns `AutomationCapable`, the preview's
//!    `blast_radius.{pane_count, command_count, file_count}`
//!    equal the corresponding input list lengths.
//! 7. **Action ID + summary preserved** — automation-capable
//!    previews preserve `action_id` and `summary` from the
//!    proposed action verbatim (substrate trusts caller-side
//!    redaction).
//! 8. **Reasons list is non-empty when ManualApprovalRequired** —
//!    operators reading the report always receive at least one
//!    actionable reason.

use std::sync::Once;

use frankenterm_core::approval_impact_simulator::{
    CredentialClass, CredentialClassBlastBand, ImpactReport, ProposedAction, RollbackPlan,
    UnknownReason, simulate_impact,
};
use proptest::prelude::*;

fn init_test_tracing_json() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .json()
            .with_target(true)
            .with_test_writer()
            .try_init();
    });
}

fn arb_credentials() -> impl Strategy<Value = CredentialClass> {
    prop_oneof![
        Just(CredentialClass::None),
        Just(CredentialClass::LocalReadOnly),
        Just(CredentialClass::LocalReadWrite),
        Just(CredentialClass::NetworkReadOnly),
        Just(CredentialClass::NetworkReadWrite),
        Just(CredentialClass::Secret),
    ]
}

fn arb_rollback_plan() -> impl Strategy<Value = RollbackPlan> {
    (any::<bool>(), 0usize..=4).prop_map(|(verified, n)| RollbackPlan {
        description: "rollback".to_string(),
        commands: (0..n).map(|i| format!("cmd_{i}")).collect(),
        verified,
    })
}

fn arb_unknown() -> impl Strategy<Value = UnknownReason> {
    prop_oneof![
        (0u64..=999).prop_map(|p| UnknownReason::UnknownPaneCapability { pane_id: p }),
        Just(UnknownReason::UnpredictableCommand {
            command: "x".to_string()
        }),
        Just(UnknownReason::UnboundedFileScope {
            hint: "**/*".to_string()
        }),
        Just(UnknownReason::UnknownCredentialScope {
            credential_label: "lbl".to_string()
        }),
        Just(UnknownReason::Other {
            message: "m".to_string()
        }),
    ]
}

fn arb_action() -> impl Strategy<Value = ProposedAction> {
    (
        prop::collection::vec(0u64..=999, 0..=5), // target_panes
        prop::collection::vec("cmd_[a-z]{1,4}", 0..=4), // commands
        prop::collection::vec("/tmp/[a-z]{1,4}", 0..=4), // touched_files
        arb_credentials(),
        prop::option::of(arb_rollback_plan()),
        prop::collection::vec(arb_unknown(), 0..=3),
    )
        .prop_map(
            |(panes, cmds, files, creds, rollback, unknowns)| ProposedAction {
                action_id: "act-1".to_string(),
                summary: "summary".to_string(),
                target_panes: panes,
                commands: cmds,
                touched_files: files,
                credentials: creds,
                rollback_plan: rollback,
                unknowns,
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// **simulate_impact is pure**: same input → same output.
    #[test]
    fn simulate_impact_is_pure(action in arb_action()) {
        init_test_tracing_json();
        let r1 = simulate_impact(&action);
        let r2 = simulate_impact(&action);
        prop_assert_eq!(r1, r2);
    }

    /// **Secret credentials always manual**: any action with
    /// `CredentialClass::Secret` returns `ManualApprovalRequired`,
    /// regardless of other signals.
    #[test]
    fn secret_credentials_force_manual_approval(action in arb_action()) {
        init_test_tracing_json();
        let mut a = action;
        a.credentials = CredentialClass::Secret;
        let report = simulate_impact(&a);
        let is_manual = matches!(report, ImpactReport::ManualApprovalRequired { .. });
        prop_assert!(is_manual, "Secret credentials must force manual approval");
    }

    /// **Non-empty unknowns force manual**: any action with
    /// `unknowns.len() > 0` returns `ManualApprovalRequired`.
    #[test]
    fn unknowns_force_manual_approval(action in arb_action()) {
        init_test_tracing_json();
        prop_assume!(!action.unknowns.is_empty());
        let report = simulate_impact(&action);
        let is_manual = matches!(report, ImpactReport::ManualApprovalRequired { .. });
        prop_assert!(is_manual, "non-empty unknowns must force manual approval");
        if let ImpactReport::ManualApprovalRequired { reasons } = report {
            prop_assert!(!reasons.is_empty(), "reasons must be non-empty");
            prop_assert!(reasons.len() >= action.unknowns.len(),
                "reasons should cover every unknown");
        }
    }

    /// **Trivial actions don't require rollback**: an action
    /// with no commands, no files, no credentials, no unknowns,
    /// and NO rollback_plan is automation-capable.
    #[test]
    fn trivial_actions_skip_rollback_requirement(
        panes in prop::collection::vec(0u64..=999, 0..=5),
    ) {
        init_test_tracing_json();
        let action = ProposedAction {
            action_id: "act".to_string(),
            summary: "summary".to_string(),
            target_panes: panes,
            commands: vec![],
            touched_files: vec![],
            credentials: CredentialClass::None,
            rollback_plan: None,
            unknowns: vec![],
        };
        let is_auto = matches!(
            simulate_impact(&action),
            ImpactReport::AutomationCapable { .. }
        );
        prop_assert!(is_auto, "trivial actions must be automation-capable");
    }

    /// **Non-trivial without rollback forces manual**: any
    /// action with at least one of (commands, files, non-None
    /// credentials) AND `rollback_plan = None` returns
    /// ManualApprovalRequired.
    #[test]
    fn non_trivial_without_rollback_forces_manual(action in arb_action()) {
        init_test_tracing_json();
        let mut a = action;
        a.unknowns = vec![];
        a.rollback_plan = None;
        let trivial = a.commands.is_empty()
            && a.touched_files.is_empty()
            && matches!(a.credentials, CredentialClass::None);
        let secret = matches!(a.credentials, CredentialClass::Secret);
        prop_assume!(!trivial);
        // Secret credentials short-circuit to manual regardless of
        // rollback; the property below holds for non-Secret too.
        let report = simulate_impact(&a);
        let is_manual = matches!(report, ImpactReport::ManualApprovalRequired { .. });
        prop_assert!(
            is_manual,
            "non-trivial action without rollback should require manual approval (secret={secret})"
        );
    }

    /// **Blast radius matches input shape**: when the simulator
    /// returns AutomationCapable, the preview's blast_radius
    /// counts equal the input list lengths.
    #[test]
    fn blast_radius_matches_input_shape(action in arb_action()) {
        init_test_tracing_json();
        if let ImpactReport::AutomationCapable { preview } = simulate_impact(&action) {
            prop_assert_eq!(preview.blast_radius.pane_count, action.target_panes.len());
            prop_assert_eq!(preview.blast_radius.command_count, action.commands.len());
            prop_assert_eq!(preview.blast_radius.file_count, action.touched_files.len());
            // Credential band mirrors CredentialClass.
            let expected_band: CredentialClassBlastBand = action.credentials.into();
            prop_assert_eq!(preview.blast_radius.credential_class, expected_band);
        }
    }

    /// **Action ID + summary preserved verbatim**: substrate
    /// trusts the caller's redaction contract; the preview
    /// passes these strings through unchanged.
    #[test]
    fn action_id_and_summary_preserved_in_preview(action in arb_action()) {
        init_test_tracing_json();
        if let ImpactReport::AutomationCapable { preview } = simulate_impact(&action) {
            prop_assert_eq!(preview.action_id, action.action_id);
            prop_assert_eq!(preview.summary, action.summary);
            prop_assert_eq!(preview.target_panes, action.target_panes);
            prop_assert_eq!(preview.commands, action.commands);
            prop_assert_eq!(preview.touched_files, action.touched_files);
            prop_assert_eq!(preview.credentials, action.credentials);
        }
    }

    /// **ManualApprovalRequired carries non-empty reasons**:
    /// operators always receive at least one actionable reason.
    #[test]
    fn manual_approval_carries_non_empty_reasons(action in arb_action()) {
        init_test_tracing_json();
        if let ImpactReport::ManualApprovalRequired { reasons } = simulate_impact(&action) {
            prop_assert!(!reasons.is_empty(), "reasons must be non-empty");
            for r in &reasons {
                prop_assert!(!r.is_empty(), "each reason must be non-empty string");
            }
        }
    }

    /// **CredentialClassBlastBand mapping is deterministic**:
    /// `CredentialClass::Secret` always maps to
    /// `CredentialClassBlastBand::Secret`, etc.
    #[test]
    fn credential_band_mapping_is_total_and_stable(creds in arb_credentials()) {
        init_test_tracing_json();
        let band: CredentialClassBlastBand = creds.into();
        let expected = match creds {
            CredentialClass::None => CredentialClassBlastBand::None,
            CredentialClass::LocalReadOnly | CredentialClass::NetworkReadOnly => {
                CredentialClassBlastBand::ReadOnly
            }
            CredentialClass::LocalReadWrite => CredentialClassBlastBand::LocalWrite,
            CredentialClass::NetworkReadWrite => CredentialClassBlastBand::NetworkWrite,
            CredentialClass::Secret => CredentialClassBlastBand::Secret,
        };
        prop_assert_eq!(band, expected);
    }

    /// **Serde roundtrip on the report**: any output from
    /// `simulate_impact` round-trips through JSON.
    #[test]
    fn impact_report_serde_roundtrip(action in arb_action()) {
        init_test_tracing_json();
        let report = simulate_impact(&action);
        let json = serde_json::to_string(&report).expect("serialize");
        let back: ImpactReport = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(report, back);
    }
}
