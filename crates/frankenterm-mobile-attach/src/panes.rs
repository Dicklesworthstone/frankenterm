//! The mux's pane topology as the phone app sees it: one compact summary per
//! pane, published as a private DCS (`ESC P ft-mobile;panes;<json> ESC \`)
//! that terminals ignore and the app parses.

use codec::ListPanesResponse;
use frankenterm_term::TerminalSize;
use mux::tab::{PaneEntry, PaneNode};
use serde::Serialize;

/// One pane, in the order the mux lists them (window, tab, split position).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PaneSummary {
    pub id: u64,
    pub title: String,
    pub tab: u64,
    pub window: u64,
    pub workspace: String,
    /// The pane is the active pane of its tab.
    pub active: bool,
    pub cols: usize,
    pub rows: usize,
    /// The full size (pixels and dpi too), restored when the phone detaches.
    #[serde(skip)]
    pub size: TerminalSize,
}

impl PaneSummary {
    fn of(entry: &PaneEntry) -> Self {
        Self {
            id: entry.pane_id as u64,
            title: entry.title.clone(),
            tab: entry.tab_id as u64,
            window: entry.window_id as u64,
            workspace: entry.workspace.clone(),
            active: entry.is_active_pane,
            cols: entry.size.cols,
            rows: entry.size.rows,
            size: entry.size,
        }
    }
}

fn collect_leaves(node: &PaneNode, out: &mut Vec<PaneSummary>) {
    match node {
        PaneNode::Empty => {}
        PaneNode::Split { left, right, .. } => {
            collect_leaves(left, out);
            collect_leaves(right, out);
        }
        PaneNode::Leaf(entry) => out.push(PaneSummary::of(entry)),
    }
}

/// Every pane of a `ListPanes` reply: tiled panes in tree order, then any
/// floating panes.  A pane id appears at most once.
#[must_use]
pub fn summarize(list: &ListPanesResponse) -> Vec<PaneSummary> {
    let mut panes = Vec::new();
    for tab in &list.tabs {
        collect_leaves(tab, &mut panes);
    }
    for floating in &list.floating_panes {
        panes.push(PaneSummary::of(&floating.pane));
    }
    let mut seen = std::collections::HashSet::new();
    panes.retain(|pane| seen.insert(pane.id));
    panes
}

#[derive(Serialize)]
struct PaneListPayload<'a> {
    attached: u64,
    panes: &'a [PaneSummary],
}

/// Re-encode JSON text as pure printable ASCII: every non-ASCII character
/// (and DEL) becomes `\uXXXX` (UTF-16, surrogate pairs above the BMP).
/// serde_json already escapes C0 controls, so the result never contains a
/// byte that could end the DCS early.  Such characters only occur inside
/// JSON strings, where the escape is equivalent.
#[must_use]
pub fn ascii_json(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    for ch in json.chars() {
        if ch.is_ascii() && ch != '\u{7f}' {
            out.push(ch);
        } else {
            let mut units = [0u16; 2];
            for unit in ch.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

/// The JSON payload of the pane-list DCS.
#[must_use]
pub fn pane_list_json(attached: u64, panes: &[PaneSummary]) -> String {
    let json = serde_json::to_string(&PaneListPayload { attached, panes })
        .unwrap_or_else(|_| format!("{{\"attached\":{attached},\"panes\":[]}}"));
    ascii_json(&json)
}

/// The complete DCS announcing `panes` with `attached` as the current pane.
#[must_use]
pub fn pane_list_dcs(attached: u64, panes: &[PaneSummary]) -> Vec<u8> {
    let mut out = b"\x1bPft-mobile;panes;".to_vec();
    out.extend_from_slice(pane_list_json(attached, panes).as_bytes());
    out.extend_from_slice(b"\x1b\\");
    out
}

/// Choose the pane to attach to when the phone did not name one.
///
/// `recent_focus` lists the panes other mux clients (the desktop GUI) have
/// focused, most recent input first.  Without one, take the active pane of
/// the newest window, else the first pane.  `exclude` skips a pane that is
/// known to be gone.
#[must_use]
pub fn choose_pane(
    panes: &[PaneSummary],
    recent_focus: &[u64],
    exclude: Option<u64>,
) -> Option<u64> {
    let usable = |id: u64| Some(id) != exclude && panes.iter().any(|pane| pane.id == id);
    if let Some(id) = recent_focus.iter().copied().find(|&id| usable(id)) {
        return Some(id);
    }
    let mut best: Option<&PaneSummary> = None;
    for pane in panes
        .iter()
        .filter(|pane| pane.active && Some(pane.id) != exclude)
    {
        if best.is_none_or(|best| pane.window > best.window) {
            best = Some(pane);
        }
    }
    best.map(|pane| pane.id).or_else(|| {
        panes
            .iter()
            .map(|pane| pane.id)
            .find(|&id| Some(id) != exclude)
    })
}

/// A title safe to place in an OSC: control characters removed, bounded.
#[must_use]
pub fn sanitize_title(title: &str) -> String {
    title
        .chars()
        .filter(|ch| !ch.is_control())
        .take(256)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(id: u64, window: u64, active: bool) -> PaneSummary {
        PaneSummary {
            id,
            title: format!("pane {id}"),
            tab: id,
            window,
            workspace: "default".to_string(),
            active,
            cols: 80,
            rows: 24,
            size: TerminalSize::default(),
        }
    }

    #[test]
    fn the_dcs_payload_is_compact_printable_ascii_json() {
        let mut first = pane(3, 1, true);
        first.title = "vim \u{e9}t\u{e9} \u{1f600} \u{1b}]2;x\u{7}\u{7f}".to_string();
        first.workspace = "w\u{2603}".to_string();
        let dcs = pane_list_dcs(3, &[first.clone(), pane(4, 2, false)]);
        assert!(dcs.starts_with(b"\x1bPft-mobile;panes;{"));
        assert!(dcs.ends_with(b"}\x1b\\"));
        let json = &dcs[b"\x1bPft-mobile;panes;".len()..dcs.len() - 2];
        assert!(
            json.iter().all(|b| (0x20..0x7f).contains(b)),
            "payload must be printable ASCII: {}",
            String::from_utf8_lossy(json)
        );
        let text = std::str::from_utf8(json).unwrap();
        assert!(text.starts_with(
            "{\"attached\":3,\"panes\":[{\"id\":3,\"title\":\"vim \\u00e9t\\u00e9 \\ud83d\\ude00"
        ));
        assert!(
            !text.contains("\": ") && !text.contains('\n'),
            "compact JSON"
        );
        let value: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(value["attached"], 3);
        assert_eq!(value["panes"][0]["title"], first.title.as_str());
        assert_eq!(value["panes"][0]["workspace"], "w\u{2603}");
        assert_eq!(value["panes"][1]["id"], 4);
        assert_eq!(value["panes"][1]["window"], 2);
        assert_eq!(value["panes"][1]["active"], false);
        assert_eq!(value["panes"][1]["cols"], 80);
        assert_eq!(value["panes"][1]["rows"], 24);
        assert!(value["panes"][1].get("size").is_none());
    }

    #[test]
    fn a_focused_pane_wins_then_the_newest_windows_active_pane() {
        let panes = [pane(1, 0, true), pane(2, 5, false), pane(3, 5, true)];
        assert_eq!(choose_pane(&panes, &[2, 1], None), Some(2));
        assert_eq!(
            choose_pane(&panes, &[9, 1], None),
            Some(1),
            "unknown focus skipped"
        );
        assert_eq!(choose_pane(&panes, &[], None), Some(3));
        assert_eq!(choose_pane(&panes, &[3], Some(3)), Some(1));
        assert_eq!(
            choose_pane(&[pane(7, 0, false), pane(8, 0, false)], &[], Some(7)),
            Some(8)
        );
        assert_eq!(choose_pane(&[pane(7, 0, true)], &[], Some(7)), None);
        assert_eq!(choose_pane(&[], &[1], None), None);
    }

    #[test]
    fn titles_lose_control_characters_and_are_bounded() {
        assert_eq!(sanitize_title("a\u{1b}]0;b\u{7}c\u{9c}d"), "a]0;bcd");
        assert_eq!(sanitize_title(&"x".repeat(1000)).len(), 256);
    }
}
