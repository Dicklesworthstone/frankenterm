#![no_main]

use arbitrary::Arbitrary;
use frankenterm_core::session_topology::{PaneNode, TabSnapshot, TopologySnapshot, WindowSnapshot};
use libfuzzer_sys::fuzz_target;
use serde_json::{Map, Value};

const MAX_TEXT_LEN: usize = 64;
const MAX_WINDOWS: usize = 4;
const MAX_TABS: usize = 4;
const MAX_LEAVES: usize = 6;
const MAX_EXTRA_FIELDS: usize = 6;
const MAX_JSON_BYTES: usize = 256 * 1024;

#[derive(Arbitrary, Debug, Clone)]
struct RawSnapshotInput {
    decode_mode: DecodeMode,
    wire_mode: WireMode,
    schema_version: u32,
    captured_at: u64,
    workspace_id: Option<String>,
    windows: Vec<RawWindow>,
    extra_fields: Vec<RawObjectEntry>,
}

#[derive(Arbitrary, Debug, Clone)]
enum DecodeMode {
    Valid,
    Missing(MissingField),
    Mistyped(WrongField),
}

#[derive(Arbitrary, Debug, Clone)]
enum MissingField {
    SchemaVersion,
    CapturedAt,
    Windows,
    PaneTree,
}

#[derive(Arbitrary, Debug, Clone)]
enum WrongField {
    SchemaVersion,
    CapturedAt,
    Windows,
    ActiveTabIndex,
    PaneTree,
}

#[derive(Arbitrary, Debug, Clone)]
enum WireMode {
    Exact,
    Truncate(u8),
    AppendGarbage(RawScalar),
}

#[derive(Arbitrary, Debug, Clone)]
struct RawWindow {
    window_id: u64,
    title: Option<String>,
    include_position: bool,
    pos_x: i16,
    pos_y: i16,
    include_size: bool,
    width: u16,
    height: u16,
    tabs: Vec<RawTab>,
    active_tab_seed: Option<u8>,
}

#[derive(Arbitrary, Debug, Clone)]
struct RawTab {
    tab_id: u64,
    title: Option<String>,
    leaves: Vec<RawLeaf>,
    shape: PaneTreeShape,
    active_leaf_seed: Option<u8>,
}

#[derive(Arbitrary, Debug, Clone)]
enum PaneTreeShape {
    Single,
    HSplit,
    VSplit,
    NestedHSplitVSplit,
    NestedVSplitHSplit,
}

#[derive(Arbitrary, Debug, Clone)]
struct RawLeaf {
    rows: u16,
    cols: u16,
    cwd: Option<String>,
    title: Option<String>,
    is_active: bool,
}

#[derive(Arbitrary, Debug, Clone)]
struct RawObjectEntry {
    key: String,
    value: RawScalar,
}

#[derive(Arbitrary, Debug, Clone)]
enum RawScalar {
    Null,
    Bool(bool),
    Int(i64),
    Text(String),
}

impl RawSnapshotInput {
    fn into_bytes_and_expected(self) -> Option<(Vec<u8>, Option<TopologySnapshot>, bool)> {
        let valid = matches!(self.decode_mode, DecodeMode::Valid);
        let exact = matches!(self.wire_mode, WireMode::Exact);

        let snapshot = self.build_snapshot();
        let mut json = serde_json::to_value(&snapshot).ok()?;
        inject_extra_fields(&mut json, &self.extra_fields);
        apply_decode_mode(&mut json, &self.decode_mode);

        let mut bytes = serde_json::to_vec(&json).ok()?;
        apply_wire_mode(&mut bytes, &self.wire_mode);

        if bytes.len() > MAX_JSON_BYTES {
            return None;
        }

        let expected = if valid && exact { Some(snapshot) } else { None };
        Some((bytes, expected, valid && exact))
    }

    fn build_snapshot(&self) -> TopologySnapshot {
        let windows = self
            .windows
            .iter()
            .take(MAX_WINDOWS)
            .enumerate()
            .map(|(window_idx, window)| window.build_window(window_idx))
            .collect();

        TopologySnapshot {
            schema_version: self.schema_version,
            captured_at: self.captured_at,
            workspace_id: self
                .workspace_id
                .clone()
                .map(|text| limited_text(text, "workspace")),
            windows,
        }
    }
}

impl RawWindow {
    fn build_window(&self, window_idx: usize) -> WindowSnapshot {
        let tabs = self
            .tabs
            .iter()
            .take(MAX_TABS)
            .enumerate()
            .map(|(tab_idx, tab)| tab.build_tab(window_idx, tab_idx))
            .collect::<Vec<_>>();

        let active_tab_index = if tabs.is_empty() {
            None
        } else {
            self.active_tab_seed
                .map(|seed| usize::from(seed) % tabs.len())
        };

        WindowSnapshot {
            window_id: self.window_id,
            title: self.title.clone().map(|text| limited_text(text, "window")),
            position: self
                .include_position
                .then_some((i32::from(self.pos_x), i32::from(self.pos_y))),
            size: self
                .include_size
                .then_some((u32::from(self.width), u32::from(self.height))),
            tabs,
            active_tab_index,
        }
    }
}

impl RawTab {
    fn build_tab(&self, window_idx: usize, tab_idx: usize) -> TabSnapshot {
        let leaves = self.normalized_leaves(window_idx, tab_idx);
        let active_pane_id = if leaves.is_empty() {
            None
        } else {
            self.active_leaf_seed
                .map(|seed| pane_id_of(&leaves[usize::from(seed) % leaves.len()]))
        };

        TabSnapshot {
            tab_id: self.tab_id,
            title: self.title.clone().map(|text| limited_text(text, "tab")),
            pane_tree: build_tree(&self.shape, &leaves),
            active_pane_id,
        }
    }

    fn normalized_leaves(&self, window_idx: usize, tab_idx: usize) -> Vec<PaneNode> {
        let mut leaves = self
            .leaves
            .iter()
            .take(MAX_LEAVES)
            .enumerate()
            .map(|(leaf_idx, leaf)| {
                let pane_id =
                    ((window_idx as u64) << 40) | ((tab_idx as u64) << 24) | (leaf_idx as u64);
                PaneNode::Leaf {
                    pane_id,
                    rows: leaf.rows.max(1),
                    cols: leaf.cols.max(1),
                    cwd: leaf.cwd.clone().map(|text| limited_text(text, "/tmp")),
                    title: leaf.title.clone().map(|text| limited_text(text, "pane")),
                    is_active: leaf.is_active,
                }
            })
            .collect::<Vec<_>>();

        if leaves.is_empty() {
            leaves.push(PaneNode::Leaf {
                pane_id: ((window_idx as u64) << 40) | ((tab_idx as u64) << 24),
                rows: 24,
                cols: 80,
                cwd: Some("/tmp".to_string()),
                title: Some("pane".to_string()),
                is_active: true,
            });
        }

        leaves
    }
}

impl RawScalar {
    fn into_json(self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Bool(value) => Value::Bool(value),
            Self::Int(value) => Value::from(value),
            Self::Text(value) => Value::String(limited_text(value, "extra")),
        }
    }
}

fn build_tree(shape: &PaneTreeShape, leaves: &[PaneNode]) -> PaneNode {
    if leaves.len() == 1 {
        return leaves[0].clone();
    }

    match shape {
        PaneTreeShape::Single => leaves[0].clone(),
        PaneTreeShape::HSplit => split_node(true, leaves.to_vec()),
        PaneTreeShape::VSplit => split_node(false, leaves.to_vec()),
        PaneTreeShape::NestedHSplitVSplit => nested_split(true, leaves),
        PaneTreeShape::NestedVSplitHSplit => nested_split(false, leaves),
    }
}

fn pane_id_of(node: &PaneNode) -> u64 {
    match node {
        PaneNode::Leaf { pane_id, .. } => *pane_id,
        PaneNode::HSplit { children } | PaneNode::VSplit { children } => children
            .first()
            .map(|(_, child)| pane_id_of(child))
            .unwrap_or(0),
    }
}

fn split_node(horizontal: bool, leaves: Vec<PaneNode>) -> PaneNode {
    let weight = 1.0 / leaves.len() as f64;
    let children = leaves
        .into_iter()
        .map(|leaf| (weight, leaf))
        .collect::<Vec<_>>();
    if horizontal {
        PaneNode::HSplit { children }
    } else {
        PaneNode::VSplit { children }
    }
}

fn nested_split(outer_horizontal: bool, leaves: &[PaneNode]) -> PaneNode {
    if leaves.len() < 3 {
        return split_node(outer_horizontal, leaves.to_vec());
    }

    let split_at = (leaves.len() / 2).clamp(1, leaves.len() - 1);
    let left = split_node(!outer_horizontal, leaves[..split_at].to_vec());
    let right = split_node(!outer_horizontal, leaves[split_at..].to_vec());
    let children = vec![(0.5, left), (0.5, right)];

    if outer_horizontal {
        PaneNode::HSplit { children }
    } else {
        PaneNode::VSplit { children }
    }
}

fn inject_extra_fields(json: &mut Value, extra_fields: &[RawObjectEntry]) {
    let Some(object) = json.as_object_mut() else {
        return;
    };

    for extra in extra_fields.iter().take(MAX_EXTRA_FIELDS) {
        object.insert(
            limited_text(extra.key.clone(), "extra_key"),
            extra.value.clone().into_json(),
        );
    }
}

fn apply_decode_mode(json: &mut Value, decode_mode: &DecodeMode) {
    match decode_mode {
        DecodeMode::Valid => {}
        DecodeMode::Missing(field) => apply_missing_field(json, field),
        DecodeMode::Mistyped(field) => apply_mistyped_field(json, field),
    }
}

fn apply_missing_field(json: &mut Value, field: &MissingField) {
    let Some(root) = json.as_object_mut() else {
        return;
    };

    match field {
        MissingField::SchemaVersion => {
            root.remove("schema_version");
        }
        MissingField::CapturedAt => {
            root.remove("captured_at");
        }
        MissingField::Windows => {
            root.remove("windows");
        }
        MissingField::PaneTree => {
            if let Some(tab) = first_tab_object_mut(root) {
                tab.remove("pane_tree");
            }
        }
    }
}

fn apply_mistyped_field(json: &mut Value, field: &WrongField) {
    let Some(root) = json.as_object_mut() else {
        return;
    };

    match field {
        WrongField::SchemaVersion => {
            root.insert(
                "schema_version".to_string(),
                Value::String("wrong".to_string()),
            );
        }
        WrongField::CapturedAt => {
            root.insert("captured_at".to_string(), Value::Array(Vec::new()));
        }
        WrongField::Windows => {
            root.insert("windows".to_string(), Value::Object(Map::new()));
        }
        WrongField::ActiveTabIndex => {
            if let Some(window) = first_window_object_mut(root) {
                window.insert(
                    "active_tab_index".to_string(),
                    Value::String("wrong".to_string()),
                );
            }
        }
        WrongField::PaneTree => {
            if let Some(tab) = first_tab_object_mut(root) {
                tab.insert("pane_tree".to_string(), Value::String("wrong".to_string()));
            }
        }
    }
}

fn first_window_object_mut(root: &mut Map<String, Value>) -> Option<&mut Map<String, Value>> {
    root.get_mut("windows")?
        .as_array_mut()?
        .first_mut()?
        .as_object_mut()
}

fn first_tab_object_mut(root: &mut Map<String, Value>) -> Option<&mut Map<String, Value>> {
    first_window_object_mut(root)?
        .get_mut("tabs")?
        .as_array_mut()?
        .first_mut()?
        .as_object_mut()
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
            let Ok(mut trailer) = serde_json::to_vec(&scalar.clone().into_json()) else {
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

fuzz_target!(|raw: RawSnapshotInput| {
    let Some((bytes, expected, exact_valid)) = raw.into_bytes_and_expected() else {
        return;
    };

    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(_) => return,
    };

    let decoded = TopologySnapshot::from_json(text);

    if let Ok(snapshot) = decoded {
        assert_eq!(snapshot.pane_count(), snapshot.pane_ids().len());

        let Ok(reencoded) = snapshot.to_json() else {
            return;
        };
        if reencoded.len() > MAX_JSON_BYTES {
            return;
        }

        let reparsed = TopologySnapshot::from_json(&reencoded).ok();
        assert_eq!(reparsed.as_ref(), Some(&snapshot));

        if exact_valid {
            assert_eq!(expected.as_ref(), Some(&snapshot));
        }
    }
});
