//! Machine-facing runbook compiler for swarm marching orders.
//!
//! This module turns selected Beads, pane inventory, capability
//! passports, AGENTS constraints, dirty-tree state, and verifier budget
//! into deterministic per-pane instructions. It is pure dry-run
//! substrate: it does not claim panes, send input, mutate files, or run
//! commands.

use serde::{Deserialize, Serialize};

use crate::capability_passport::{CapabilityClass, CapabilityPassport};

/// Stable schema for compiled marching-order reports.
pub const RUNBOOK_COMPILER_SCHEMA_VERSION: &str = "ft.runbook.compiler.v1";

/// Bead selected for dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunbookBead {
    pub bead_id: String,
    pub title: String,
    pub priority: u8,
    pub issue_type: String,
    pub ownership: OwnershipScope,
}

/// Paths a bead is allowed to edit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnershipScope {
    pub paths: Vec<String>,
    pub rationale: String,
}

/// Pane plus optional passport known to the compiler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunbookPane {
    pub pane_id: u64,
    pub agent_id: String,
    pub cwd: String,
    pub domain: String,
    #[serde(default)]
    pub passport: Option<CapabilityPassport>,
}

/// Dirty-tree entry visible before dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirtyTreeEntry {
    pub path: String,
    pub status: String,
    #[serde(default)]
    pub owner_agent: Option<String>,
}

/// AGENTS.md and operator constraints that must propagate into every
/// generated order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunbookConstraints {
    pub workspace_root: String,
    pub required_branch: String,
    pub forbidden_paths: Vec<String>,
    pub forbidden_commands: Vec<String>,
    pub safety_rules: Vec<RepoSafetyRule>,
}

/// Non-negotiable repo safety rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoSafetyRule {
    NoFileDeletion,
    NoGitWorktrees,
    PreserveFrankentermCore,
    CargoOnly,
    RuntimeAsyncOnly,
    StageOnlyOwnedPaths,
}

/// Verifier command budget. Commands are templates so the compiler can
/// inject the bead-specific target dir while preserving exact command
/// shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifierBudget {
    pub target_dir_prefix: String,
    pub timeout_seconds: u32,
    pub command_templates: Vec<String>,
}

/// Full compiler input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunbookCompilerInput {
    pub selected_beads: Vec<RunbookBead>,
    pub panes: Vec<RunbookPane>,
    pub constraints: RunbookConstraints,
    pub dirty_tree: Vec<DirtyTreeEntry>,
    pub verifier_budget: VerifierBudget,
    #[serde(default)]
    pub dry_run: bool,
}

/// High-level compile disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunbookCompilationStatus {
    Compiled,
    Refused,
}

/// Compiler output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunbookCompilation {
    pub schema_version: String,
    pub status: RunbookCompilationStatus,
    pub dry_run: bool,
    pub orders: Vec<MarchingOrder>,
    pub conflicts: Vec<ConflictReport>,
    pub diagnostics: Vec<String>,
}

impl RunbookCompilation {
    /// True when the compiler produced executable marching orders.
    #[must_use]
    pub fn is_compiled(&self) -> bool {
        self.status == RunbookCompilationStatus::Compiled
    }
}

/// One per-pane instruction packet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarchingOrder {
    pub order_id: String,
    pub bead_id: String,
    pub pane_id: u64,
    pub agent_id: String,
    pub cwd: String,
    pub ownership_scope: OwnershipScope,
    pub forbidden_paths: Vec<String>,
    pub forbidden_commands: Vec<String>,
    pub safety_rules: Vec<RepoSafetyRule>,
    pub required_branch: String,
    pub cargo_target_dir: String,
    pub exact_commands: Vec<String>,
    pub proof_checklist: Vec<String>,
    pub commit_rule: String,
    pub closeout_rule: String,
}

/// Why compilation refused, or non-fatal issue if future callers decide
/// to emit warnings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictReport {
    pub kind: ConflictKind,
    pub bead_id: Option<String>,
    pub pane_id: Option<u64>,
    pub paths: Vec<String>,
    pub message: String,
}

/// Conflict taxonomy for robot consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    NoSelectedBeads,
    NoVerifierCommands,
    InsufficientEligiblePanes,
    MissingCapabilityPassport,
    PassportNotDispatchable,
    EmptyOwnershipScope,
    ForbiddenPath,
    OverlappingOwnership,
    DirtyTreeConflict,
}

/// Compile deterministic marching orders or refuse with diagnostics.
#[must_use]
pub fn compile_marching_orders(input: &RunbookCompilerInput) -> RunbookCompilation {
    let mut conflicts = Vec::new();
    let mut diagnostics = vec![format!(
        "compiler schema {RUNBOOK_COMPILER_SCHEMA_VERSION}; dry_run={}",
        input.dry_run
    )];

    validate_global_input(input, &mut conflicts);
    let mut ineligible_panes = Vec::new();
    let eligible_panes = eligible_panes(input, &mut ineligible_panes);
    diagnostics.extend(ineligible_panes.iter().map(|conflict| {
        format!(
            "pane {} ineligible: {}",
            conflict
                .pane_id
                .map_or_else(|| "unknown".to_string(), |pane_id| pane_id.to_string()),
            conflict.message
        )
    }));
    validate_beads(input, &mut conflicts);
    validate_ownership_disjoint(&input.selected_beads, &mut conflicts);

    if eligible_panes.len() < input.selected_beads.len() {
        conflicts.push(ConflictReport {
            kind: ConflictKind::InsufficientEligiblePanes,
            bead_id: None,
            pane_id: None,
            paths: Vec::new(),
            message: format!(
                "selected {} bead(s) but only {} eligible pane(s)",
                input.selected_beads.len(),
                eligible_panes.len()
            ),
        });
        conflicts.extend(ineligible_panes);
    }

    if !conflicts.is_empty() {
        diagnostics.push("refused before assignment due to input conflicts".to_string());
        return refused(input.dry_run, conflicts, diagnostics);
    }

    let mut orders = Vec::with_capacity(input.selected_beads.len());
    for (idx, bead) in input.selected_beads.iter().enumerate() {
        let pane = eligible_panes[idx];
        if let Some(conflict) = dirty_tree_conflict(bead, pane, &input.dirty_tree) {
            conflicts.push(conflict);
            continue;
        }
        orders.push(build_order(bead, pane, input));
    }

    if !conflicts.is_empty() {
        diagnostics.push("refused after assignment due to dirty-tree conflicts".to_string());
        return refused(input.dry_run, conflicts, diagnostics);
    }

    diagnostics.push(format!("compiled {} marching order(s)", orders.len()));
    RunbookCompilation {
        schema_version: RUNBOOK_COMPILER_SCHEMA_VERSION.to_string(),
        status: RunbookCompilationStatus::Compiled,
        dry_run: input.dry_run,
        orders,
        conflicts,
        diagnostics,
    }
}

fn validate_global_input(input: &RunbookCompilerInput, conflicts: &mut Vec<ConflictReport>) {
    if input.selected_beads.is_empty() {
        conflicts.push(ConflictReport {
            kind: ConflictKind::NoSelectedBeads,
            bead_id: None,
            pane_id: None,
            paths: Vec::new(),
            message: "no selected beads supplied".to_string(),
        });
    }
    if input.verifier_budget.command_templates.is_empty() {
        conflicts.push(ConflictReport {
            kind: ConflictKind::NoVerifierCommands,
            bead_id: None,
            pane_id: None,
            paths: Vec::new(),
            message: "verifier budget has no exact command templates".to_string(),
        });
    }
}

fn eligible_panes<'a>(
    input: &'a RunbookCompilerInput,
    conflicts: &mut Vec<ConflictReport>,
) -> Vec<&'a RunbookPane> {
    let mut eligible = Vec::new();
    for pane in &input.panes {
        match pane_dispatchable(pane, &input.constraints, &input.verifier_budget) {
            Ok(()) => eligible.push(pane),
            Err(conflict) => conflicts.push(conflict),
        }
    }
    eligible.sort_by_key(|pane| (pane.pane_id, pane.agent_id.as_str()));
    eligible
}

fn pane_dispatchable(
    pane: &RunbookPane,
    constraints: &RunbookConstraints,
    verifier_budget: &VerifierBudget,
) -> Result<(), ConflictReport> {
    if !pane.cwd.starts_with(&constraints.workspace_root) {
        return Err(ConflictReport {
            kind: ConflictKind::PassportNotDispatchable,
            bead_id: None,
            pane_id: Some(pane.pane_id),
            paths: vec![pane.cwd.clone()],
            message: format!(
                "pane {} cwd is outside workspace root {}",
                pane.pane_id, constraints.workspace_root
            ),
        });
    }
    let Some(passport) = pane.passport.as_ref() else {
        return Err(ConflictReport {
            kind: ConflictKind::MissingCapabilityPassport,
            bead_id: None,
            pane_id: Some(pane.pane_id),
            paths: Vec::new(),
            message: format!("pane {} has no capability passport", pane.pane_id),
        });
    };

    let required_workspace = CapabilityClass::FilesystemScope(constraints.workspace_root.clone());
    if !passport.has_verified(&required_workspace)
        || !has_verified_kind(passport, matches_runtime_wrapper)
        || !has_verified_cargo_target_prefix(passport, &verifier_budget.target_dir_prefix)
        || !passport.has_verified(&CapabilityClass::SafetyConstraint(
            "no_destructive_git".to_string(),
        ))
    {
        return Err(ConflictReport {
            kind: ConflictKind::PassportNotDispatchable,
            bead_id: None,
            pane_id: Some(pane.pane_id),
            paths: Vec::new(),
            message: format!(
                "pane {} passport lacks verified workspace/runtime/target/safety capabilities",
                pane.pane_id
            ),
        });
    }
    Ok(())
}

fn has_verified_kind(
    passport: &CapabilityPassport,
    predicate: fn(&CapabilityClass) -> bool,
) -> bool {
    passport
        .capabilities
        .iter()
        .any(|entry| predicate(&entry.class) && entry.verification.is_dispatchable())
}

fn has_verified_cargo_target_prefix(passport: &CapabilityPassport, prefix: &str) -> bool {
    passport.capabilities.iter().any(|entry| {
        entry.verification.is_dispatchable()
            && matches!(
                &entry.class,
                CapabilityClass::CargoTargetDirPolicy(value) if value.starts_with(prefix)
            )
    })
}

fn matches_runtime_wrapper(class: &CapabilityClass) -> bool {
    matches!(class, CapabilityClass::RuntimeWrapper(_))
}

fn validate_beads(input: &RunbookCompilerInput, conflicts: &mut Vec<ConflictReport>) {
    for bead in &input.selected_beads {
        if bead.ownership.paths.is_empty() {
            conflicts.push(ConflictReport {
                kind: ConflictKind::EmptyOwnershipScope,
                bead_id: Some(bead.bead_id.clone()),
                pane_id: None,
                paths: Vec::new(),
                message: format!("bead {} has no ownership paths", bead.bead_id),
            });
        }
        for owned_path in &bead.ownership.paths {
            if input
                .constraints
                .forbidden_paths
                .iter()
                .any(|forbidden| paths_overlap(owned_path, forbidden))
            {
                conflicts.push(ConflictReport {
                    kind: ConflictKind::ForbiddenPath,
                    bead_id: Some(bead.bead_id.clone()),
                    pane_id: None,
                    paths: vec![owned_path.clone()],
                    message: format!("bead {} owns forbidden path {owned_path}", bead.bead_id),
                });
            }
        }
    }
}

fn validate_ownership_disjoint(beads: &[RunbookBead], conflicts: &mut Vec<ConflictReport>) {
    for (left_index, left) in beads.iter().enumerate() {
        for right in beads.iter().skip(left_index + 1) {
            for left_path in &left.ownership.paths {
                for right_path in &right.ownership.paths {
                    if paths_overlap(left_path, right_path) {
                        conflicts.push(ConflictReport {
                            kind: ConflictKind::OverlappingOwnership,
                            bead_id: Some(left.bead_id.clone()),
                            pane_id: None,
                            paths: vec![left_path.clone(), right_path.clone()],
                            message: format!(
                                "beads {} and {} have overlapping ownership",
                                left.bead_id, right.bead_id
                            ),
                        });
                    }
                }
            }
        }
    }
}

fn dirty_tree_conflict(
    bead: &RunbookBead,
    pane: &RunbookPane,
    dirty_tree: &[DirtyTreeEntry],
) -> Option<ConflictReport> {
    let mut conflicting_paths = Vec::new();
    for dirty in dirty_tree {
        let overlaps = bead
            .ownership
            .paths
            .iter()
            .any(|owned_path| paths_overlap(owned_path, &dirty.path));
        let owner_matches = dirty.owner_agent.as_deref() == Some(pane.agent_id.as_str());
        if overlaps && !owner_matches {
            conflicting_paths.push(dirty.path.clone());
        }
    }
    if conflicting_paths.is_empty() {
        None
    } else {
        Some(ConflictReport {
            kind: ConflictKind::DirtyTreeConflict,
            bead_id: Some(bead.bead_id.clone()),
            pane_id: Some(pane.pane_id),
            paths: conflicting_paths,
            message: format!(
                "bead {} overlaps dirty paths not owned by {}",
                bead.bead_id, pane.agent_id
            ),
        })
    }
}

fn build_order(
    bead: &RunbookBead,
    pane: &RunbookPane,
    input: &RunbookCompilerInput,
) -> MarchingOrder {
    let cargo_target_dir = cargo_target_dir(&input.verifier_budget.target_dir_prefix, bead, pane);
    let exact_commands = input
        .verifier_budget
        .command_templates
        .iter()
        .map(|template| render_command_template(template, bead, &cargo_target_dir))
        .collect::<Vec<_>>();
    let proof_checklist = proof_checklist(bead, &exact_commands);
    MarchingOrder {
        order_id: format!("{}:{}:{}", bead.bead_id, pane.pane_id, pane.agent_id),
        bead_id: bead.bead_id.clone(),
        pane_id: pane.pane_id,
        agent_id: pane.agent_id.clone(),
        cwd: pane.cwd.clone(),
        ownership_scope: bead.ownership.clone(),
        forbidden_paths: input.constraints.forbidden_paths.clone(),
        forbidden_commands: input.constraints.forbidden_commands.clone(),
        safety_rules: input.constraints.safety_rules.clone(),
        required_branch: input.constraints.required_branch.clone(),
        cargo_target_dir,
        exact_commands,
        proof_checklist,
        commit_rule: format!(
            "stage only owned paths for {} plus the matching Beads line; commit with bead id",
            bead.bead_id
        ),
        closeout_rule: format!(
            "close {} only after exact verifier commands pass or blockers are recorded",
            bead.bead_id
        ),
    }
}

fn cargo_target_dir(prefix: &str, bead: &RunbookBead, pane: &RunbookPane) -> String {
    format!(
        "{}-{}-pane{}",
        prefix.trim_end_matches('-'),
        sanitize_id(&bead.bead_id),
        pane.pane_id
    )
}

fn render_command_template(template: &str, bead: &RunbookBead, cargo_target_dir: &str) -> String {
    template
        .replace("{target_dir}", cargo_target_dir)
        .replace("{bead_id}", &bead.bead_id)
}

fn proof_checklist(bead: &RunbookBead, exact_commands: &[String]) -> Vec<String> {
    let mut checklist = vec![
        format!("confirm branch is main before editing {}", bead.bead_id),
        format!(
            "inspect dirty tree for owned paths: {}",
            bead.ownership.paths.join(", ")
        ),
        format!("git diff --check -- {}", bead.ownership.paths.join(" ")),
    ];
    checklist.extend(
        exact_commands
            .iter()
            .map(|command| format!("run exact verifier: {command}")),
    );
    checklist.push(format!("record proof in br comment for {}", bead.bead_id));
    checklist
}

fn sanitize_id(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

fn refused(
    dry_run: bool,
    conflicts: Vec<ConflictReport>,
    diagnostics: Vec<String>,
) -> RunbookCompilation {
    RunbookCompilation {
        schema_version: RUNBOOK_COMPILER_SCHEMA_VERSION.to_string(),
        status: RunbookCompilationStatus::Refused,
        dry_run,
        orders: Vec::new(),
        conflicts,
        diagnostics,
    }
}

fn paths_overlap(left: &str, right: &str) -> bool {
    let left = normalize_path(left);
    let right = normalize_path(right);
    left == right
        || left
            .strip_prefix(&right)
            .is_some_and(|suffix| suffix.starts_with('/'))
        || right
            .strip_prefix(&left)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn normalize_path(path: &str) -> String {
    path.trim_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use crate::capability_passport::{CapabilityEntry, CapabilityVerification, RedactedProof};
    use crate::test_fixtures::synthetic_swarm::{SyntheticSwarmScale, synthetic_swarm_scenario};

    use super::*;

    const WORKSPACE: &str = "/Users/jemanuel/projects/frankenterm";

    fn passport(agent_id: &str, pane_id: u64) -> CapabilityPassport {
        CapabilityPassport {
            agent_id: agent_id.to_string(),
            pane_id: Some(pane_id),
            capabilities: vec![
                verified(CapabilityClass::FilesystemScope(WORKSPACE.to_string())),
                verified(CapabilityClass::RuntimeWrapper("codex_cli".to_string())),
                verified(CapabilityClass::CargoTargetDirPolicy(
                    "/tmp/ft-runbook".to_string(),
                )),
                verified(CapabilityClass::SafetyConstraint(
                    "no_destructive_git".to_string(),
                )),
            ],
            generation: 1,
            signed_at_ms: 1_766_000_000_000,
        }
    }

    fn verified(class: CapabilityClass) -> CapabilityEntry {
        CapabilityEntry {
            class,
            verification: CapabilityVerification::Verified,
            last_observed_at_ms: Some(1_766_000_000_000),
            proof: RedactedProof::empty(),
        }
    }

    fn pane(pane_id: u64, agent_id: &str) -> RunbookPane {
        RunbookPane {
            pane_id,
            agent_id: agent_id.to_string(),
            cwd: WORKSPACE.to_string(),
            domain: "local".to_string(),
            passport: Some(passport(agent_id, pane_id)),
        }
    }

    fn bead(bead_id: &str, path: &str) -> RunbookBead {
        RunbookBead {
            bead_id: bead_id.to_string(),
            title: format!("{bead_id} title"),
            priority: 2,
            issue_type: "feature".to_string(),
            ownership: OwnershipScope {
                paths: vec![path.to_string()],
                rationale: "test-owned path".to_string(),
            },
        }
    }

    fn constraints() -> RunbookConstraints {
        RunbookConstraints {
            workspace_root: WORKSPACE.to_string(),
            required_branch: "main".to_string(),
            forbidden_paths: vec![".git".to_string(), "target".to_string()],
            forbidden_commands: vec![
                "git reset --hard".to_string(),
                "git worktree add".to_string(),
                "rm -rf".to_string(),
            ],
            safety_rules: vec![
                RepoSafetyRule::NoFileDeletion,
                RepoSafetyRule::NoGitWorktrees,
                RepoSafetyRule::PreserveFrankentermCore,
                RepoSafetyRule::CargoOnly,
                RepoSafetyRule::RuntimeAsyncOnly,
                RepoSafetyRule::StageOnlyOwnedPaths,
            ],
        }
    }

    fn verifier_budget() -> VerifierBudget {
        VerifierBudget {
            target_dir_prefix: "/tmp/ft-runbook".to_string(),
            timeout_seconds: 300,
            command_templates: vec![
                "rch exec -- env CARGO_TARGET_DIR={target_dir} bash -lc 'cargo test -p frankenterm-core --lib --no-default-features {bead_id}'".to_string(),
                "rustfmt --edition 2024 --check crates/frankenterm-core/src/runbook_compiler.rs".to_string(),
            ],
        }
    }

    fn input(beads: Vec<RunbookBead>, panes: Vec<RunbookPane>) -> RunbookCompilerInput {
        RunbookCompilerInput {
            selected_beads: beads,
            panes,
            constraints: constraints(),
            dirty_tree: Vec::new(),
            verifier_budget: verifier_budget(),
            dry_run: true,
        }
    }

    #[test]
    fn compiles_order_with_forbidden_actions_and_exact_commands() {
        let compilation = compile_marching_orders(&input(
            vec![bead("ft-1650n.8", "docs/runbook-compiler.md")],
            vec![pane(10, "codex")],
        ));

        assert!(compilation.is_compiled());
        let order = &compilation.orders[0];
        assert_eq!(order.required_branch, "main");
        assert!(order.forbidden_commands.contains(&"rm -rf".to_string()));
        assert!(order.safety_rules.contains(&RepoSafetyRule::NoGitWorktrees));
        assert_eq!(order.cargo_target_dir, "/tmp/ft-runbook-ft-1650n-8-pane10");
        assert!(order.exact_commands.iter().any(|command| {
            command.contains("CARGO_TARGET_DIR=/tmp/ft-runbook-ft-1650n-8-pane10")
                && command.contains("cargo test -p frankenterm-core")
        }));
        assert!(
            order
                .proof_checklist
                .iter()
                .any(|item| item.contains("git diff --check -- docs/runbook-compiler.md"))
        );
    }

    #[test]
    fn refuses_dirty_tree_overlap_without_matching_owner() {
        let mut work = input(
            vec![bead("ft-1650n.8", "docs/runbook-compiler.md")],
            vec![pane(10, "codex")],
        );
        work.dirty_tree.push(DirtyTreeEntry {
            path: "docs/runbook-compiler.md".to_string(),
            status: "M".to_string(),
            owner_agent: Some("other-pane".to_string()),
        });

        let compilation = compile_marching_orders(&work);

        assert_eq!(compilation.status, RunbookCompilationStatus::Refused);
        assert!(compilation.conflicts.iter().any(|conflict| {
            conflict.kind == ConflictKind::DirtyTreeConflict
                && conflict.paths == vec!["docs/runbook-compiler.md".to_string()]
        }));
    }

    #[test]
    fn refuses_overlapping_ownership_scopes() {
        let compilation = compile_marching_orders(&input(
            vec![
                bead("ft-a", "docs/runbooks"),
                bead("ft-b", "docs/runbooks/generated.md"),
            ],
            vec![pane(10, "codex-a"), pane(11, "codex-b")],
        ));

        assert_eq!(compilation.status, RunbookCompilationStatus::Refused);
        assert!(
            compilation
                .conflicts
                .iter()
                .any(|conflict| conflict.kind == ConflictKind::OverlappingOwnership)
        );
    }

    #[test]
    fn refuses_pane_without_verified_passport_capabilities() {
        let mut bad_pane = pane(10, "codex");
        bad_pane.passport = Some(CapabilityPassport {
            agent_id: "codex".to_string(),
            pane_id: Some(10),
            capabilities: vec![verified(CapabilityClass::RuntimeWrapper(
                "codex_cli".to_string(),
            ))],
            generation: 1,
            signed_at_ms: 1_766_000_000_000,
        });

        let compilation = compile_marching_orders(&input(
            vec![bead("ft-1650n.8", "docs/runbook-compiler.md")],
            vec![bad_pane],
        ));

        assert_eq!(compilation.status, RunbookCompilationStatus::Refused);
        assert!(
            compilation
                .conflicts
                .iter()
                .any(|conflict| conflict.kind == ConflictKind::PassportNotDispatchable)
        );
    }

    #[test]
    fn synthetic_fleet50_orders_are_disjoint_and_exact() {
        let scenario = synthetic_swarm_scenario(SyntheticSwarmScale::Fleet50);
        let panes = scenario
            .pane_scripts
            .iter()
            .map(|script| pane(script.pane_id, &format!("agent-{}", script.pane_id)))
            .collect::<Vec<_>>();
        let selected_beads = (0..8)
            .map(|index| {
                bead(
                    &format!("ft-1650n.8.{index}"),
                    &format!("docs/generated/runbook-{index}.md"),
                )
            })
            .collect::<Vec<_>>();

        let compilation = compile_marching_orders(&input(selected_beads, panes));

        assert!(compilation.is_compiled());
        assert_eq!(compilation.orders.len(), 8);
        let pane_ids = compilation
            .orders
            .iter()
            .map(|order| order.pane_id)
            .collect::<BTreeSet<_>>();
        assert_eq!(pane_ids.len(), compilation.orders.len());
        let owned_paths = compilation
            .orders
            .iter()
            .flat_map(|order| order.ownership_scope.paths.iter().cloned())
            .collect::<Vec<_>>();
        for (left_index, left) in owned_paths.iter().enumerate() {
            for right in owned_paths.iter().skip(left_index + 1) {
                assert!(!paths_overlap(left, right));
            }
        }
        assert!(compilation.orders.iter().all(|order| {
            order.exact_commands.iter().any(|command| {
                command.contains("rch exec -- env CARGO_TARGET_DIR=/tmp/ft-runbook-")
                    && command.contains("cargo test -p frankenterm-core")
            })
        }));
    }
}
