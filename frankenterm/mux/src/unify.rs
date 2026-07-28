//! Pure planner for the "unify windows on a domain" feature (ft-winunify).
//!
//! Computes a [`MergePlan`] that consolidates the GUI windows mirroring the
//! same remote mux domain into a single canonical window, de-duplicating the
//! local tab mirrors that point at the SAME remote session.
//!
//! Model: `Mux -> Window -> Tab -> Pane`. A remote-domain (`ClientDomain`) pane
//! is a LOCAL mirror of a remote pane and carries a `remote_pane_id`. Two local
//! tabs mirror the same remote session iff they share an identity of
//! `(domain_id, sorted set of their panes' remote_pane_ids)` — see
//! [`TabIdentity`].
//!
//! This module is intentionally PURE and side-effect free: it operates on
//! caller-supplied snapshots ([`WindowSnapshot`] / [`TabSnapshot`]) rather than
//! the live `Mux`, so it is fully dry-runnable and unit-testable. The GUI
//! command and robot surface build the snapshots from the live mux (extracting
//! each tab's domain + remote pane ids by downcasting its panes to the client
//! crate's `ClientPane`) and then APPLY the resulting plan via the mux
//! primitives (`move_tab_between_windows` for [`MergePlan::moves`] and
//! `remove_tab_local_only` for [`MergePlan::drops`]). The remote session is
//! never killed: drops remove only the redundant LOCAL mirror.

use crate::domain::DomainId;
use crate::pane::PaneId;
use crate::tab::TabId;
use crate::window::WindowId;
use std::collections::{HashMap, HashSet};

/// Identity of a tab as a mirror of a remote mux session: the domain it belongs
/// to plus the sorted, de-duplicated set of its panes' remote pane ids. Two
/// local mirror tabs are duplicates iff their identities are equal.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TabIdentity {
    pub domain_id: DomainId,
    /// Remote pane ids of every pane in the tab, sorted ascending and
    /// de-duplicated. Always built through [`TabIdentity::new`] so the ordering
    /// invariant holds regardless of the caller's pane iteration order.
    pub remote_pane_ids: Vec<PaneId>,
}

impl TabIdentity {
    pub fn new(domain_id: DomainId, remote_pane_ids: impl IntoIterator<Item = PaneId>) -> Self {
        let mut remote_pane_ids: Vec<PaneId> = remote_pane_ids.into_iter().collect();
        remote_pane_ids.sort_unstable();
        remote_pane_ids.dedup();
        Self {
            domain_id,
            remote_pane_ids,
        }
    }

    /// A tab with no remote panes cannot be PROVEN to mirror the same remote
    /// session as any other tab, so the planner must never treat it as a
    /// duplicate (it is kept and moved, never dropped).
    fn is_dedupable(&self) -> bool {
        !self.remote_pane_ids.is_empty()
    }
}

/// A single tab within a window, as seen by the planner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabSnapshot {
    pub tab_id: TabId,
    /// The remote-session identity of this tab, or `None` if the tab is not a
    /// remote mirror (e.g. a local-domain tab). Tabs whose identity is `None`,
    /// or whose `domain_id` differs from the unify target, are out of scope and
    /// left completely untouched.
    pub identity: Option<TabIdentity>,
}

/// A GUI window and the tabs it currently contains.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowSnapshot {
    pub window_id: WindowId,
    /// Workspace the window belongs to. Unify is SAME-WORKSPACE only: windows in
    /// a different workspace are never merged.
    pub workspace: String,
    pub tabs: Vec<TabSnapshot>,
}

/// A planned move of a tab from its current window into the canonical window.
/// Applied via the `move_tab_between_windows` mux primitive, which preserves the
/// live `Tab`/`Pane`s (a metadata-only move).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabMove {
    pub tab_id: TabId,
    pub from_window: WindowId,
    pub to_window: WindowId,
}

/// A planned local-only drop of a redundant duplicate tab mirror. Applied via
/// the `remove_tab_local_only` mux primitive: the LOCAL mirror is removed
/// WITHOUT sending `Pdu::KillPane` to the remote, so the remote session
/// survives (the canonical window retains an equivalent mirror).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabDrop {
    pub tab_id: TabId,
    pub window: WindowId,
    /// The tab kept as the canonical representative of this identity; this drop
    /// is redundant with it.
    pub duplicate_of: TabId,
}

/// The fully computed, side-effect-free plan to unify a domain's windows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MergePlan {
    /// The window all kept tabs are consolidated into. `None` iff there is
    /// nothing in scope to unify (no in-scope window in the workspace).
    pub canonical_window: Option<WindowId>,
    /// In-scope, non-duplicate tabs that are not already in the canonical
    /// window, to be moved into it. Never contains a dropped tab.
    pub moves: Vec<TabMove>,
    /// Redundant duplicate local mirrors to remove (local-only; remote
    /// preserved). Never contains a moved tab.
    pub drops: Vec<TabDrop>,
    /// In-scope windows (other than the canonical) left with zero tabs after the
    /// moves+drops, to be closed. A window retaining any out-of-scope tab is
    /// NOT closed.
    pub close_windows: Vec<WindowId>,
}

impl MergePlan {
    /// True when the plan would change nothing (no moves and no drops).
    pub fn is_noop(&self) -> bool {
        self.moves.is_empty() && self.drops.is_empty()
    }
}

/// Compute the [`MergePlan`] that unifies every GUI window mirroring `domain_id`
/// within `workspace` into one canonical window.
///
/// Pure and side-effect free; produces a plan only.
///
/// Algorithm:
/// 1. Candidate windows = windows in `workspace` that contain at least one
///    in-scope tab (a tab whose identity is on `domain_id`). With no candidates,
///    the result is an empty plan.
/// 2. Pick the canonical window: `preferred_canonical` if it is a candidate,
///    otherwise the candidate with the most in-scope tabs (fewest moves),
///    tie-broken by the smallest window id.
/// 3. De-duplicate by identity across all in-scope tabs of all candidate
///    windows, processing the canonical window first so a duplicate shared with
///    the canonical keeps the canonical copy. The first tab seen for an identity
///    is KEPT; later tabs with the same identity are DROPPED (local-only). Tabs
///    with an empty remote-pane set are never de-duplicated.
/// 4. Every KEPT in-scope tab not already in the canonical window is MOVED into
///    it. (Non-duplicates are always moved, never dropped.)
/// 5. A non-canonical candidate window whose every tab was moved or dropped is
///    closed; one retaining any out-of-scope tab stays open.
pub fn plan_unify_domain(
    windows: &[WindowSnapshot],
    domain_id: DomainId,
    workspace: &str,
    preferred_canonical: Option<WindowId>,
) -> MergePlan {
    let in_scope = |tab: &TabSnapshot| -> bool {
        matches!(&tab.identity, Some(id) if id.domain_id == domain_id)
    };

    let mut candidates: Vec<&WindowSnapshot> = windows
        .iter()
        .filter(|w| w.workspace == workspace && w.tabs.iter().any(in_scope))
        .collect();

    if candidates.is_empty() {
        return MergePlan::default();
    }
    candidates.sort_by_key(|w| w.window_id);

    let canonical = select_canonical(&candidates, domain_id, preferred_canonical);

    // Process the canonical window first: when a duplicate identity exists both
    // in the canonical window and elsewhere, this keeps the canonical copy and
    // drops the other mirror (avoiding a needless move).
    let mut ordered: Vec<&WindowSnapshot> = Vec::with_capacity(candidates.len());
    if let Some(c) = candidates.iter().find(|w| w.window_id == canonical) {
        ordered.push(*c);
    }
    ordered.extend(
        candidates
            .iter()
            .copied()
            .filter(|w| w.window_id != canonical),
    );

    let mut moves: Vec<TabMove> = Vec::new();
    let mut drops: Vec<TabDrop> = Vec::new();
    // identity -> kept (representative) tab id for that identity.
    let mut kept: HashMap<&TabIdentity, TabId> = HashMap::new();

    for w in &ordered {
        for tab in &w.tabs {
            let Some(identity) = &tab.identity else {
                continue; // not a remote mirror -> out of scope
            };
            if identity.domain_id != domain_id {
                continue; // different domain -> out of scope
            }
            if identity.is_dedupable() {
                if let Some(&dup_of) = kept.get(identity) {
                    drops.push(TabDrop {
                        tab_id: tab.tab_id,
                        window: w.window_id,
                        duplicate_of: dup_of,
                    });
                    continue;
                }
                kept.insert(identity, tab.tab_id);
            }
            // KEPT in-scope tab: consolidate it into the canonical window.
            if w.window_id != canonical {
                moves.push(TabMove {
                    tab_id: tab.tab_id,
                    from_window: w.window_id,
                    to_window: canonical,
                });
            }
        }
    }

    let moved: HashSet<TabId> = moves.iter().map(|m| m.tab_id).collect();
    let dropped: HashSet<TabId> = drops.iter().map(|d| d.tab_id).collect();
    let mut close_windows: Vec<WindowId> = ordered
        .iter()
        .filter(|w| w.window_id != canonical)
        .filter(|w| {
            w.tabs
                .iter()
                .all(|t| moved.contains(&t.tab_id) || dropped.contains(&t.tab_id))
        })
        .map(|w| w.window_id)
        .collect();

    moves.sort_by_key(|m| (m.from_window, m.tab_id));
    drops.sort_by_key(|d| (d.window, d.tab_id));
    close_windows.sort_unstable();

    MergePlan {
        canonical_window: Some(canonical),
        moves,
        drops,
        close_windows,
    }
}

/// Pick the canonical window from the (non-empty) candidate set: the preferred
/// window if it is a candidate, else the candidate with the most in-scope tabs,
/// tie-broken by the smallest window id.
fn select_canonical(
    candidates: &[&WindowSnapshot],
    domain_id: DomainId,
    preferred: Option<WindowId>,
) -> WindowId {
    if let Some(pref) = preferred {
        if candidates.iter().any(|w| w.window_id == pref) {
            return pref;
        }
    }
    candidates
        .iter()
        .map(|w| {
            let in_scope_tabs = w
                .tabs
                .iter()
                .filter(|t| matches!(&t.identity, Some(id) if id.domain_id == domain_id))
                .count();
            (in_scope_tabs, w.window_id)
        })
        // Maximize in-scope tab count; on a tie prefer the SMALLER window id
        // (so flip the id comparison so the smaller id sorts as "greater").
        .max_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)))
        .map(|(_, id)| id)
        .expect("candidates is non-empty")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(tab_id: TabId, domain: DomainId, remotes: &[PaneId]) -> TabSnapshot {
        TabSnapshot {
            tab_id,
            identity: Some(TabIdentity::new(domain, remotes.iter().copied())),
        }
    }

    /// A tab on `domain` that has no remote panes (cannot be de-duplicated).
    fn empty_remote_tab(tab_id: TabId, domain: DomainId) -> TabSnapshot {
        TabSnapshot {
            tab_id,
            identity: Some(TabIdentity::new(domain, std::iter::empty())),
        }
    }

    /// A local (non-mirror) tab, always out of scope.
    fn local_tab(tab_id: TabId) -> TabSnapshot {
        TabSnapshot {
            tab_id,
            identity: None,
        }
    }

    fn win(window_id: WindowId, workspace: &str, tabs: Vec<TabSnapshot>) -> WindowSnapshot {
        WindowSnapshot {
            window_id,
            workspace: workspace.to_string(),
            tabs,
        }
    }

    #[test]
    fn identity_is_order_independent() {
        assert_eq!(
            TabIdentity::new(1, [300, 100, 200]),
            TabIdentity::new(1, [100, 200, 300]),
        );
        // de-dupes repeated remote ids
        assert_eq!(
            TabIdentity::new(1, [100, 100, 200]),
            TabIdentity::new(1, [200, 100]),
        );
        // domain participates in identity
        assert_ne!(TabIdentity::new(1, [100]), TabIdentity::new(2, [100]));
    }

    #[test]
    fn dedup_across_windows_keeps_one_drops_rest() {
        let windows = vec![
            win(1, "default", vec![tab(10, 7, &[100])]),
            win(2, "default", vec![tab(20, 7, &[100])]), // duplicate of tab 10
            win(3, "default", vec![tab(30, 7, &[200])]), // distinct -> moved
        ];
        let plan = plan_unify_domain(&windows, 7, "default", None);

        // All windows hold one in-scope tab -> tie -> smallest id (1) canonical.
        assert_eq!(plan.canonical_window, Some(1));
        assert_eq!(
            plan.drops,
            vec![TabDrop {
                tab_id: 20,
                window: 2,
                duplicate_of: 10,
            }]
        );
        assert_eq!(
            plan.moves,
            vec![TabMove {
                tab_id: 30,
                from_window: 3,
                to_window: 1,
            }]
        );
        // W2 (dropped) and W3 (moved) end up empty -> closed.
        assert_eq!(plan.close_windows, vec![2, 3]);
        assert!(!plan.is_noop());
    }

    #[test]
    fn canonical_prefers_window_with_most_in_scope_tabs() {
        let windows = vec![
            win(1, "ws", vec![tab(10, 7, &[100])]),
            win(2, "ws", vec![tab(20, 7, &[200]), tab(21, 7, &[300])]),
        ];
        let plan = plan_unify_domain(&windows, 7, "ws", None);
        assert_eq!(plan.canonical_window, Some(2));
        assert_eq!(
            plan.moves,
            vec![TabMove {
                tab_id: 10,
                from_window: 1,
                to_window: 2,
            }]
        );
        assert!(plan.drops.is_empty());
        assert_eq!(plan.close_windows, vec![1]);
    }

    #[test]
    fn preferred_canonical_overrides_heuristic() {
        let windows = vec![
            win(1, "ws", vec![tab(10, 7, &[100])]),
            win(2, "ws", vec![tab(20, 7, &[200]), tab(21, 7, &[300])]),
        ];
        let plan = plan_unify_domain(&windows, 7, "ws", Some(1));
        assert_eq!(plan.canonical_window, Some(1));
        assert_eq!(
            plan.moves,
            vec![
                TabMove {
                    tab_id: 20,
                    from_window: 2,
                    to_window: 1,
                },
                TabMove {
                    tab_id: 21,
                    from_window: 2,
                    to_window: 1,
                },
            ]
        );
        assert_eq!(plan.close_windows, vec![2]);
    }

    #[test]
    fn other_workspace_windows_are_ignored() {
        let windows = vec![
            win(1, "a", vec![tab(10, 7, &[100])]),
            win(2, "b", vec![tab(20, 7, &[100])]), // same identity, other workspace
        ];
        let plan = plan_unify_domain(&windows, 7, "a", None);
        assert_eq!(plan.canonical_window, Some(1));
        assert!(plan.moves.is_empty());
        assert!(plan.drops.is_empty(), "must not touch the other workspace");
        assert!(plan.close_windows.is_empty());
        assert!(plan.is_noop());
    }

    #[test]
    fn duplicate_is_dropped_never_moved() {
        // A duplicate must be classified as a local-only drop, and no tab may be
        // both moved and dropped. Pin the canonical to window 1 (otherwise the
        // most-in-scope-tabs heuristic would pick window 2, making tab 10 the
        // dropped duplicate); with window 1 canonical, the non-canonical
        // duplicate (tab 20) must be DROPPED and the distinct tab (21) MOVED.
        let windows = vec![
            win(1, "ws", vec![tab(10, 7, &[100])]),
            win(2, "ws", vec![tab(20, 7, &[100]), tab(21, 7, &[999])]),
        ];
        let plan = plan_unify_domain(&windows, 7, "ws", Some(1));
        let moved: HashSet<TabId> = plan.moves.iter().map(|m| m.tab_id).collect();
        let dropped: HashSet<TabId> = plan.drops.iter().map(|d| d.tab_id).collect();
        assert!(dropped.contains(&20), "duplicate mirror must be dropped");
        assert!(moved.contains(&21), "distinct tab must be moved");
        assert!(
            moved.is_disjoint(&dropped),
            "no tab is both moved and dropped"
        );
    }

    #[test]
    fn out_of_scope_tab_keeps_window_open() {
        let windows = vec![
            win(1, "ws", vec![tab(10, 7, &[100])]),
            win(2, "ws", vec![tab(20, 7, &[100]), local_tab(99)]),
        ];
        let plan = plan_unify_domain(&windows, 7, "ws", None);
        assert_eq!(plan.canonical_window, Some(1));
        assert_eq!(
            plan.drops,
            vec![TabDrop {
                tab_id: 20,
                window: 2,
                duplicate_of: 10,
            }]
        );
        // W2 still holds the out-of-scope local tab 99 -> must NOT be closed.
        assert!(plan.close_windows.is_empty());
    }

    #[test]
    fn empty_remote_pane_set_is_never_dropped() {
        let windows = vec![
            win(1, "ws", vec![empty_remote_tab(10, 7)]),
            win(2, "ws", vec![empty_remote_tab(20, 7)]),
        ];
        let plan = plan_unify_domain(&windows, 7, "ws", None);
        assert_eq!(plan.canonical_window, Some(1));
        assert!(plan.drops.is_empty(), "unprovable duplicates must not drop");
        assert_eq!(
            plan.moves,
            vec![TabMove {
                tab_id: 20,
                from_window: 2,
                to_window: 1,
            }]
        );
        assert_eq!(plan.close_windows, vec![2]);
    }

    #[test]
    fn single_window_internal_duplicate_drops() {
        let windows = vec![win(
            1,
            "ws",
            vec![tab(10, 7, &[100]), tab(11, 7, &[100]), tab(12, 7, &[200])],
        )];
        let plan = plan_unify_domain(&windows, 7, "ws", None);
        assert_eq!(plan.canonical_window, Some(1));
        assert!(plan.moves.is_empty(), "the lone window is canonical");
        assert_eq!(
            plan.drops,
            vec![TabDrop {
                tab_id: 11,
                window: 1,
                duplicate_of: 10,
            }]
        );
        assert!(plan.close_windows.is_empty());
        assert!(!plan.is_noop());
    }

    #[test]
    fn no_candidate_windows_returns_empty_plan() {
        let windows = vec![win(1, "ws", vec![tab(10, 7, &[100])])];
        // domain 999 is not present anywhere.
        let plan = plan_unify_domain(&windows, 999, "ws", None);
        assert_eq!(plan, MergePlan::default());
        assert!(plan.is_noop());
        assert_eq!(plan.canonical_window, None);
    }

    #[test]
    fn duplicate_shared_with_canonical_keeps_canonical_copy() {
        // Identity [100] exists in the chosen canonical (window 2, more tabs)
        // and in window 1; the canonical copy is kept, window 1's is dropped.
        let windows = vec![
            win(1, "ws", vec![tab(10, 7, &[100])]),
            win(2, "ws", vec![tab(20, 7, &[100]), tab(21, 7, &[200])]),
        ];
        let plan = plan_unify_domain(&windows, 7, "ws", None);
        assert_eq!(plan.canonical_window, Some(2));
        assert_eq!(
            plan.drops,
            vec![TabDrop {
                tab_id: 10,
                window: 1,
                duplicate_of: 20,
            }]
        );
        assert!(plan.moves.is_empty());
        assert_eq!(plan.close_windows, vec![1]);
    }

    #[test]
    fn foundation_contract_dedups_selects_canonical_and_scopes_workspace() {
        let windows = vec![
            win(1, "ws", vec![tab(10, 7, &[300, 100]), tab(11, 7, &[400])]),
            win(2, "ws", vec![tab(20, 7, &[100, 300]), tab(21, 7, &[500])]),
            win(3, "other", vec![tab(30, 7, &[100, 300])]),
        ];
        let plan = plan_unify_domain(&windows, 7, "ws", None);

        // Windows 1 and 2 tie on in-scope tab count, so canonical selection
        // falls back to the lower window id. Window 3 is a different workspace
        // and must not participate even though it mirrors the same remote tab.
        assert_eq!(plan.canonical_window, Some(1));
        assert_eq!(
            plan.drops,
            vec![TabDrop {
                tab_id: 20,
                window: 2,
                duplicate_of: 10,
            }]
        );
        assert_eq!(
            plan.moves,
            vec![TabMove {
                tab_id: 21,
                from_window: 2,
                to_window: 1,
            }]
        );
        assert_eq!(plan.close_windows, vec![2]);
        assert!(
            plan.moves.iter().all(|m| m.tab_id != 30) && plan.drops.iter().all(|d| d.tab_id != 30),
            "same-identity tabs in other workspaces stay untouched",
        );
    }
}
