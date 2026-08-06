//! Stable witnesses for snapshot checkpoints and restore receipts.
//!
//! The persisted witness is a corruption/consistency check, not an
//! authentication mechanism: a writer that can rewrite the database can also
//! recompute SHA-256. Stable, explicitly framed inputs still let readers
//! distinguish complete, internally consistent v2 records from legacy rows and
//! from accidental partial mutation across toolchain releases.

use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

pub(crate) const SNAPSHOT_DEDUP_PREFIX: &str = "snpd2:";
pub(crate) const SNAPSHOT_WITNESS_PREFIX: &str = "snp2:";
pub(crate) const RESTORE_INTENT_WITNESS_PREFIX: &str = "rsi2:";
pub(crate) const RESTORE_RECEIPT_WITNESS_PREFIX: &str = "rst2:";
pub(crate) const CHECKPOINT_ROLE_SNAPSHOT: &str = "snapshot";
pub(crate) const CHECKPOINT_ROLE_RESTORE_INTENT: &str = "restore_intent";
pub(crate) const CHECKPOINT_ROLE_RESTORE_RECEIPT: &str = "restore_receipt";

#[derive(Debug, thiserror::Error)]
pub(crate) enum CheckpointWitnessError {
    #[error("checkpoint witness JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("checkpoint witness field is too large: {0}")]
    LengthOverflow(&'static str),
    #[error("checkpoint witness contains duplicate pane id {0}")]
    DuplicatePaneId(i64),
    #[error("unsupported checkpoint role: {0}")]
    UnsupportedRole(String),
}

/// Exact columns persisted for one `mux_pane_state` row.
///
/// JSON strings are canonicalized before a new row is inserted. Readers hash
/// the bytes that are actually stored, so whitespace/key-order rewrites of a
/// v2 row do not silently preserve its witness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PersistedPaneState {
    pub(crate) pane_id: i64,
    pub(crate) cwd: Option<String>,
    pub(crate) command: Option<String>,
    pub(crate) env_json: Option<String>,
    pub(crate) terminal_state_json: String,
    pub(crate) agent_metadata_json: Option<String>,
    pub(crate) scrollback_checkpoint_seq: Option<i64>,
    pub(crate) last_output_at: Option<i64>,
}

/// Recursively sort object keys without changing array order or scalar value.
fn canonicalize_json_value(value: Value) -> Value {
    match value {
        Value::Object(fields) => {
            let mut entries: Vec<(String, Value)> = fields.into_iter().collect();
            entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
            let mut canonical = Map::new();
            for (key, value) in entries {
                canonical.insert(key, canonicalize_json_value(value));
            }
            Value::Object(canonical)
        }
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(canonicalize_json_value)
                .collect(),
        ),
        scalar => scalar,
    }
}

pub(crate) fn canonical_json_string<T: Serialize>(
    value: &T,
) -> Result<String, CheckpointWitnessError> {
    let value = serde_json::to_value(value)?;
    Ok(serde_json::to_string(&canonicalize_json_value(value))?)
}

struct FramedSha256 {
    hasher: Sha256,
}

impl FramedSha256 {
    fn new(domain: &'static [u8]) -> Result<Self, CheckpointWitnessError> {
        let mut framed = Self {
            hasher: Sha256::new(),
        };
        framed.required_bytes("domain", domain)?;
        Ok(framed)
    }

    fn label(&mut self, label: &'static str) -> Result<(), CheckpointWitnessError> {
        let length = u32::try_from(label.len())
            .map_err(|_| CheckpointWitnessError::LengthOverflow("field label"))?;
        self.hasher.update(length.to_be_bytes());
        self.hasher.update(label.as_bytes());
        Ok(())
    }

    fn required_bytes(
        &mut self,
        label: &'static str,
        value: &[u8],
    ) -> Result<(), CheckpointWitnessError> {
        self.label(label)?;
        self.hasher.update([1]);
        let length = u64::try_from(value.len())
            .map_err(|_| CheckpointWitnessError::LengthOverflow(label))?;
        self.hasher.update(length.to_be_bytes());
        self.hasher.update(value);
        Ok(())
    }

    fn optional_bytes(
        &mut self,
        label: &'static str,
        value: Option<&[u8]>,
    ) -> Result<(), CheckpointWitnessError> {
        self.label(label)?;
        match value {
            Some(value) => {
                self.hasher.update([1]);
                let length = u64::try_from(value.len())
                    .map_err(|_| CheckpointWitnessError::LengthOverflow(label))?;
                self.hasher.update(length.to_be_bytes());
                self.hasher.update(value);
            }
            None => self.hasher.update([0]),
        }
        Ok(())
    }

    fn required_i64(
        &mut self,
        label: &'static str,
        value: i64,
    ) -> Result<(), CheckpointWitnessError> {
        self.required_bytes(label, &value.to_be_bytes())
    }

    fn required_u64(
        &mut self,
        label: &'static str,
        value: u64,
    ) -> Result<(), CheckpointWitnessError> {
        self.required_bytes(label, &value.to_be_bytes())
    }

    fn optional_i64(
        &mut self,
        label: &'static str,
        value: Option<i64>,
    ) -> Result<(), CheckpointWitnessError> {
        match value {
            Some(value) => self.optional_bytes(label, Some(&value.to_be_bytes())),
            None => self.optional_bytes(label, None),
        }
    }

    fn finish(self, prefix: &'static str) -> String {
        format!("{prefix}{}", hex::encode(self.hasher.finalize()))
    }
}

fn sorted_panes(
    panes: &[PersistedPaneState],
) -> Result<Vec<&PersistedPaneState>, CheckpointWitnessError> {
    let mut panes: Vec<&PersistedPaneState> = panes.iter().collect();
    panes.sort_unstable_by_key(|pane| pane.pane_id);
    for adjacent in panes.windows(2) {
        if adjacent[0].pane_id == adjacent[1].pane_id {
            return Err(CheckpointWitnessError::DuplicatePaneId(
                adjacent[0].pane_id,
            ));
        }
    }
    Ok(panes)
}

fn frame_panes(
    framed: &mut FramedSha256,
    panes: &[PersistedPaneState],
) -> Result<(), CheckpointWitnessError> {
    framed.required_u64(
        "persisted_pane_row_count",
        u64::try_from(panes.len())
            .map_err(|_| CheckpointWitnessError::LengthOverflow("pane rows"))?,
    )?;

    let mut already_sorted = true;
    for adjacent in panes.windows(2) {
        if adjacent[0].pane_id == adjacent[1].pane_id {
            return Err(CheckpointWitnessError::DuplicatePaneId(
                adjacent[0].pane_id,
            ));
        }
        if adjacent[0].pane_id > adjacent[1].pane_id {
            already_sorted = false;
        }
    }
    if already_sorted {
        for pane in panes {
            frame_pane(framed, pane)?;
        }
        return Ok(());
    }

    for pane in sorted_panes(panes)? {
        frame_pane(framed, pane)?;
    }
    Ok(())
}

fn frame_pane(
    framed: &mut FramedSha256,
    pane: &PersistedPaneState,
) -> Result<(), CheckpointWitnessError> {
    framed.required_i64("pane_id", pane.pane_id)?;
    framed.optional_bytes("cwd", pane.cwd.as_deref().map(str::as_bytes))?;
    framed.optional_bytes("command", pane.command.as_deref().map(str::as_bytes))?;
    framed.optional_bytes(
        "env_json",
        pane.env_json.as_deref().map(str::as_bytes),
    )?;
    framed.required_bytes(
        "terminal_state_json",
        pane.terminal_state_json.as_bytes(),
    )?;
    framed.optional_bytes(
        "agent_metadata_json",
        pane.agent_metadata_json.as_deref().map(str::as_bytes),
    )?;
    framed.optional_i64(
        "scrollback_checkpoint_seq",
        pane.scrollback_checkpoint_seq,
    )?;
    framed.optional_i64("last_output_at", pane.last_output_at)?;
    Ok(())
}

/// Stable semantic digest used only for in-memory periodic deduplication.
///
/// The topology capture clock is excluded. Every other input is an exact
/// column that the checkpoint transaction persists; process PID/argv, shell,
/// and scrollback total-segment counters therefore do not participate.
pub(crate) fn snapshot_dedup_witness(
    topology_json: &str,
    panes: &[PersistedPaneState],
) -> Result<String, CheckpointWitnessError> {
    let mut topology: Value = serde_json::from_str(topology_json)?;
    if let Value::Object(fields) = &mut topology {
        fields.remove("captured_at");
    }
    let topology = serde_json::to_string(&canonicalize_json_value(topology))?;

    let mut framed = FramedSha256::new(b"frankenterm:snapshot-dedup:v2")?;
    framed.required_bytes("topology_json", topology.as_bytes())?;
    frame_panes(&mut framed, panes)?;
    Ok(framed.finish(SNAPSHOT_DEDUP_PREFIX))
}

/// Stable persisted checkpoint witness for a snapshot, restore intent, or
/// restore outcome receipt.
#[allow(clippy::too_many_arguments)]
pub(crate) fn checkpoint_witness(
    role: &str,
    session_id: &str,
    checkpoint_id: i64,
    checkpoint_at: i64,
    checkpoint_type: &str,
    pane_count: i64,
    total_bytes: i64,
    metadata_json: Option<&str>,
    topology_json: Option<&str>,
    panes: &[PersistedPaneState],
) -> Result<String, CheckpointWitnessError> {
    let (domain, prefix) = match role {
        CHECKPOINT_ROLE_SNAPSHOT => (
            &b"frankenterm:snapshot-checkpoint:v2"[..],
            SNAPSHOT_WITNESS_PREFIX,
        ),
        CHECKPOINT_ROLE_RESTORE_INTENT => (
            &b"frankenterm:restore-intent:v2"[..],
            RESTORE_INTENT_WITNESS_PREFIX,
        ),
        CHECKPOINT_ROLE_RESTORE_RECEIPT => (
            &b"frankenterm:restore-receipt:v2"[..],
            RESTORE_RECEIPT_WITNESS_PREFIX,
        ),
        other => return Err(CheckpointWitnessError::UnsupportedRole(other.to_string())),
    };

    let mut framed = FramedSha256::new(domain)?;
    framed.required_bytes("checkpoint_role", role.as_bytes())?;
    framed.required_bytes("session_id", session_id.as_bytes())?;
    framed.required_i64("checkpoint_id", checkpoint_id)?;
    framed.required_i64("checkpoint_at", checkpoint_at)?;
    framed.required_bytes("checkpoint_type", checkpoint_type.as_bytes())?;
    framed.required_i64("pane_count", pane_count)?;
    framed.required_i64("total_bytes", total_bytes)?;
    framed.optional_bytes("metadata_json", metadata_json.map(str::as_bytes))?;
    framed.optional_bytes("topology_json", topology_json.map(str::as_bytes))?;
    frame_panes(&mut framed, panes)?;
    Ok(framed.finish(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_topology(captured_at: u64) -> String {
        format!(
            "{{\"captured_at\":{captured_at},\"schema_version\":1,\"windows\":[],\"workspace_id\":null}}"
        )
    }

    #[test]
    fn canonical_json_sorts_nested_objects_without_reordering_arrays() {
        let value = serde_json::json!({
            "z": {"b": 2, "a": 1},
            "a": [{"d": 4, "c": 3}, 2, 1],
        });
        assert_eq!(
            canonical_json_string(&value).unwrap(),
            r#"{"a":[{"c":3,"d":4},2,1],"z":{"a":1,"b":2}}"#
        );
    }

    #[test]
    fn snapshot_dedup_excludes_only_topology_capture_clock() {
        let first = snapshot_dedup_witness(&empty_topology(1), &[]).unwrap();
        let second = snapshot_dedup_witness(&empty_topology(99), &[]).unwrap();
        assert_eq!(first, second);
        assert!(first.starts_with(SNAPSHOT_DEDUP_PREFIX));
    }

    #[test]
    fn witness_role_domains_are_distinct() {
        let snapshot = checkpoint_witness(
            CHECKPOINT_ROLE_SNAPSHOT,
            "sess",
            7,
            11,
            "startup",
            0,
            0,
            None,
            Some(&empty_topology(11)),
            &[],
        )
        .unwrap();
        let receipt = checkpoint_witness(
            CHECKPOINT_ROLE_RESTORE_RECEIPT,
            "sess",
            7,
            11,
            "startup",
            0,
            0,
            None,
            None,
            &[],
        )
        .unwrap();
        let intent = checkpoint_witness(
            CHECKPOINT_ROLE_RESTORE_INTENT,
            "sess",
            7,
            11,
            "startup",
            0,
            0,
            None,
            None,
            &[],
        )
        .unwrap();
        assert!(snapshot.starts_with(SNAPSHOT_WITNESS_PREFIX));
        assert!(intent.starts_with(RESTORE_INTENT_WITNESS_PREFIX));
        assert!(receipt.starts_with(RESTORE_RECEIPT_WITNESS_PREFIX));
        assert_ne!(snapshot, receipt);
        assert_ne!(snapshot, intent);
        assert_ne!(intent, receipt);
    }

    #[test]
    fn fixed_empty_vectors_pin_cross_release_framing() {
        let topology = empty_topology(11);
        assert_eq!(
            snapshot_dedup_witness(&topology, &[]).unwrap(),
            "snpd2:e6467ddc380a22d7198585a511ec35ae988c68d092714e0fc4b3895534feedac"
        );
        assert_eq!(
            checkpoint_witness(
                CHECKPOINT_ROLE_SNAPSHOT,
                "sess",
                7,
                11,
                "startup",
                0,
                0,
                None,
                Some(&topology),
                &[],
            )
            .unwrap(),
            "snp2:1143f1e341b53fa623bddf79e3e963d7d98649dd3e145b35d5231a9414d460f1"
        );
        assert_eq!(
            checkpoint_witness(
                CHECKPOINT_ROLE_RESTORE_RECEIPT,
                "sess",
                7,
                11,
                "startup",
                0,
                0,
                None,
                None,
                &[],
            )
            .unwrap(),
            "rst2:efe2b0ede649a2f4f00d2b8873b09cc589cd23b0867c1711529215882142d07f"
        );
    }

    #[test]
    fn fixed_populated_vector_pins_optional_utf8_and_i64_framing() {
        let topology = empty_topology(1234);
        let pane = PersistedPaneState {
            pane_id: 42,
            cwd: Some("/tmp/☃".to_string()),
            command: Some("zsh".to_string()),
            env_json: Some(r#"{"vars":{"LANG":"C"}}"#.to_string()),
            terminal_state_json: r#"{"cols":80,"rows":24}"#.to_string(),
            agent_metadata_json: None,
            scrollback_checkpoint_seq: Some(9),
            last_output_at: Some(1234),
        };
        assert_eq!(
            checkpoint_witness(
                CHECKPOINT_ROLE_SNAPSHOT,
                "sess-unicode",
                99,
                1234,
                "event",
                1,
                123,
                Some(r#"{"reason":"golden"}"#),
                Some(&topology),
                &[pane],
            )
            .unwrap(),
            "snp2:a32d1f49065f8e7c64aa64e3fe2f6b508bec43ea406c02b690d2417dcfbaa1a1"
        );
    }

    #[test]
    fn pane_order_is_canonical_but_duplicates_and_optional_shape_are_distinct() {
        let topology = empty_topology(1);
        let pane = |pane_id, cwd| PersistedPaneState {
            pane_id,
            cwd,
            command: None,
            env_json: None,
            terminal_state_json: "{}".to_string(),
            agent_metadata_json: None,
            scrollback_checkpoint_seq: None,
            last_output_at: None,
        };
        let first = pane(1, None);
        let second = pane(2, Some(String::new()));
        let ordered = snapshot_dedup_witness(&topology, &[first.clone(), second.clone()]).unwrap();
        let reversed = snapshot_dedup_witness(&topology, &[second.clone(), first.clone()]).unwrap();
        assert_eq!(ordered, reversed);

        let empty_instead_of_none =
            snapshot_dedup_witness(&topology, &[pane(1, Some(String::new())), second.clone()])
                .unwrap();
        assert_ne!(ordered, empty_instead_of_none);

        assert!(matches!(
            snapshot_dedup_witness(&topology, &[first.clone(), first]),
            Err(CheckpointWitnessError::DuplicatePaneId(1))
        ));
    }
}
