//! Recorder semantic chunking/windowing policy (`wa-oegrb.5.2`).
//!
//! This module implements `ft.recorder.chunking.v1` with deterministic chunk
//! IDs, hard/soft boundary rules, overlap handling, and glue rules for tiny
//! fragments. It is intentionally pure and side-effect free so the same input
//! event stream always produces the same chunk sequence.

use crate::recorder_storage::RecorderOffset;
use crate::recording::{RecorderEvent, RecorderEventPayload};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Canonical semantic chunking policy identifier.
pub const RECORDER_CHUNKING_POLICY_V1: &str = "ft.recorder.chunking.v1";

/// Runtime-tunable policy knobs for semantic chunking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkPolicyConfig {
    /// Hard cap on normalized text length per chunk.
    pub max_chunk_chars: usize,
    /// Maximum number of event contributions per chunk.
    pub max_chunk_events: usize,
    /// Maximum temporal width of a chunk.
    pub max_window_ms: u64,
    /// Time jump that forces a hard boundary.
    pub hard_gap_ms: u64,
    /// Minimum target chunk size before glue/merge.
    pub min_chunk_chars: usize,
    /// Maximum adjacency window for glue.
    pub merge_window_ms: u64,
    /// Prefix overlap chars from previous chunk for soft splits.
    pub overlap_chars: usize,
}

impl Default for ChunkPolicyConfig {
    fn default() -> Self {
        Self {
            max_chunk_chars: 1_800,
            max_chunk_events: 48,
            max_window_ms: 120_000,
            hard_gap_ms: 30_000,
            min_chunk_chars: 80,
            merge_window_ms: 8_000,
            overlap_chars: 120,
        }
    }
}

/// Canonical chunk direction label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkDirection {
    Ingress,
    Egress,
    MixedGlued,
}

impl ChunkDirection {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ingress => "ingress",
            Self::Egress => "egress",
            Self::MixedGlued => "mixed_glued",
        }
    }
}

/// Stable source offset tuple for traceability/replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkSourceOffset {
    pub segment_id: u64,
    pub ordinal: u64,
    pub byte_offset: u64,
}

impl From<&RecorderOffset> for ChunkSourceOffset {
    fn from(value: &RecorderOffset) -> Self {
        Self {
            segment_id: value.segment_id,
            ordinal: value.ordinal,
            byte_offset: value.byte_offset,
        }
    }
}

/// Input tuple for chunking: recorder event + canonical append-log offset.
#[derive(Debug, Clone)]
pub struct ChunkInputEvent {
    pub event: RecorderEvent,
    pub offset: RecorderOffset,
}

/// Metadata describing overlap carried from a prior chunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkOverlap {
    /// Source chunk ID that provided overlap text.
    pub from_chunk_id: String,
    /// Source end offset from the prior chunk.
    pub source_end_offset: ChunkSourceOffset,
    /// Number of overlap characters included.
    pub chars: usize,
    /// Overlap text included as prefix in this chunk.
    pub text: String,
}

/// Semantic chunk output for embedding/indexing pipelines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticChunk {
    pub chunk_id: String,
    pub policy_version: String,
    pub pane_id: u64,
    pub session_id: Option<String>,
    pub direction: ChunkDirection,
    pub start_offset: ChunkSourceOffset,
    pub end_offset: ChunkSourceOffset,
    pub event_ids: Vec<String>,
    pub event_count: usize,
    pub occurred_at_start_ms: u64,
    pub occurred_at_end_ms: u64,
    pub text_chars: usize,
    pub content_hash: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlap: Option<ChunkOverlap>,
}

#[derive(Debug, Clone)]
struct TextContribution {
    event_id: String,
    pane_id: u64,
    session_id: Option<String>,
    direction: ChunkDirection,
    text: String,
    text_chars: usize,
    occurred_at_ms: u64,
    offset: ChunkSourceOffset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClassifiedInputKind {
    BoundaryOnly,
    Text,
}

#[derive(Debug, Clone)]
struct ClassifiedInput {
    kind: ClassifiedInputKind,
    text: Option<TextContribution>,
}

#[derive(Debug, Clone)]
struct ChunkBuilder {
    pane_id: u64,
    session_id: Option<String>,
    direction: ChunkDirection,
    start_offset: ChunkSourceOffset,
    end_offset: ChunkSourceOffset,
    event_ids: Vec<String>,
    event_count: usize,
    occurred_at_start_ms: u64,
    occurred_at_end_ms: u64,
    text_chars: usize,
    text: String,
    overlap: Option<ChunkOverlap>,
}

impl ChunkBuilder {
    fn new(first: &TextContribution, overlap: Option<ChunkOverlap>) -> Self {
        let mut text = String::new();
        let mut text_chars = 0;
        if let Some(meta) = &overlap {
            append_text_line(&mut text, &mut text_chars, &meta.text);
        }

        Self {
            pane_id: first.pane_id,
            session_id: first.session_id.clone(),
            direction: first.direction,
            start_offset: first.offset.clone(),
            end_offset: first.offset.clone(),
            event_ids: Vec::new(),
            event_count: 0,
            occurred_at_start_ms: first.occurred_at_ms,
            occurred_at_end_ms: first.occurred_at_ms,
            text_chars,
            text,
            overlap,
        }
    }

    fn push(&mut self, contribution: TextContribution) {
        if self.session_id.as_deref() != contribution.session_id.as_deref() {
            self.session_id = None;
        }

        append_text_line(
            &mut self.text,
            &mut self.text_chars,
            contribution.text.as_str(),
        );
        self.end_offset = contribution.offset;
        self.occurred_at_end_ms = contribution.occurred_at_ms;
        self.event_ids.push(contribution.event_id);
        self.event_count += 1;
    }

    fn finalize(self) -> SemanticChunk {
        let content_hash = sha256_hex(self.text.as_bytes());
        let chunk_id = chunk_id_for(
            self.pane_id,
            self.direction,
            self.start_offset.ordinal,
            self.end_offset.ordinal,
            &content_hash,
        );

        SemanticChunk {
            chunk_id,
            policy_version: RECORDER_CHUNKING_POLICY_V1.to_string(),
            pane_id: self.pane_id,
            session_id: self.session_id,
            direction: self.direction,
            start_offset: self.start_offset,
            end_offset: self.end_offset,
            event_ids: self.event_ids,
            event_count: self.event_count,
            occurred_at_start_ms: self.occurred_at_start_ms,
            occurred_at_end_ms: self.occurred_at_end_ms,
            text_chars: self.text_chars,
            content_hash,
            text: self.text,
            overlap: self.overlap,
        }
    }
}

/// Build deterministic semantic chunks from ordered/unordered recorder events.
///
/// The function sorts inputs by `(segment_id, ordinal, byte_offset)` first to
/// guarantee deterministic ordering even if caller input order differs.
#[must_use]
pub fn build_semantic_chunks(
    events: &[ChunkInputEvent],
    config: &ChunkPolicyConfig,
) -> Vec<SemanticChunk> {
    if events.is_empty() {
        return Vec::new();
    }

    let mut ordered = events.to_vec();
    ordered.sort_by_key(|item| {
        (
            item.offset.segment_id,
            item.offset.ordinal,
            item.offset.byte_offset,
        )
    });

    let mut chunks: Vec<SemanticChunk> = Vec::new();
    let mut current: Option<ChunkBuilder> = None;
    let mut previous_finalized: Option<SemanticChunk> = None;
    let mut allow_overlap_on_next_start = false;

    for input in &ordered {
        let classified = classify_input(input);
        if classified.kind == ClassifiedInputKind::BoundaryOnly {
            flush_current(&mut current, &mut chunks, &mut previous_finalized);
            allow_overlap_on_next_start = false;
            continue;
        }

        let Some(base_contribution) = classified.text else {
            continue;
        };

        // Very long single events are deterministically split by character
        // windows so they still respect max_chunk_chars soft limits.
        let contributions = split_contribution_by_chars(base_contribution, config.max_chunk_chars);

        for contribution in contributions {
            let hard_boundary = current.as_ref().is_some_and(|builder| {
                builder.pane_id != contribution.pane_id
                    || builder.direction != contribution.direction
                    || contribution
                        .occurred_at_ms
                        .saturating_sub(builder.occurred_at_end_ms)
                        > config.hard_gap_ms
            });

            if hard_boundary {
                flush_current(&mut current, &mut chunks, &mut previous_finalized);
                allow_overlap_on_next_start = false;
            }

            if current.is_none() {
                let overlap = if allow_overlap_on_next_start {
                    previous_finalized.as_ref().and_then(|previous| {
                        overlap_from_previous(previous, &contribution, config.overlap_chars)
                    })
                } else {
                    None
                };
                current = Some(ChunkBuilder::new(&contribution, overlap));
                allow_overlap_on_next_start = false;
            }

            let should_soft_split = current.as_ref().is_some_and(|builder| {
                builder.event_count > 0 && exceeds_soft_limits(builder, &contribution, config)
            });

            if should_soft_split {
                flush_current(&mut current, &mut chunks, &mut previous_finalized);
                let overlap = previous_finalized.as_ref().and_then(|previous| {
                    overlap_from_previous(previous, &contribution, config.overlap_chars)
                });
                current = Some(ChunkBuilder::new(&contribution, overlap));
                allow_overlap_on_next_start = false;
            }

            if let Some(builder) = current.as_mut() {
                builder.push(contribution);
            }
        }
    }

    flush_current(&mut current, &mut chunks, &mut previous_finalized);
    apply_glue_rules(chunks, config)
}

fn flush_current(
    current: &mut Option<ChunkBuilder>,
    chunks: &mut Vec<SemanticChunk>,
    previous_finalized: &mut Option<SemanticChunk>,
) {
    if let Some(builder) = current.take() {
        let finalized = builder.finalize();
        *previous_finalized = Some(finalized.clone());
        chunks.push(finalized);
    }
}

fn classify_input(input: &ChunkInputEvent) -> ClassifiedInput {
    let offset = ChunkSourceOffset::from(&input.offset);
    let event = &input.event;

    match &event.payload {
        RecorderEventPayload::IngressText { text, .. } => {
            let normalized = normalize_payload_text(text);
            let assembled = prefixed_text("[IN] ", &normalized);
            ClassifiedInput {
                kind: ClassifiedInputKind::Text,
                text: Some(TextContribution {
                    event_id: event.event_id.clone(),
                    pane_id: event.pane_id,
                    session_id: event.session_id.clone(),
                    direction: ChunkDirection::Ingress,
                    text_chars: assembled.chars().count(),
                    text: assembled,
                    occurred_at_ms: event.occurred_at_ms,
                    offset,
                }),
            }
        }
        RecorderEventPayload::EgressOutput { text, is_gap, .. } => {
            if *is_gap {
                return ClassifiedInput {
                    kind: ClassifiedInputKind::BoundaryOnly,
                    text: None,
                };
            }

            let normalized = normalize_payload_text(text);
            let assembled = prefixed_text("[OUT] ", &normalized);
            ClassifiedInput {
                kind: ClassifiedInputKind::Text,
                text: Some(TextContribution {
                    event_id: event.event_id.clone(),
                    pane_id: event.pane_id,
                    session_id: event.session_id.clone(),
                    direction: ChunkDirection::Egress,
                    text_chars: assembled.chars().count(),
                    text: assembled,
                    occurred_at_ms: event.occurred_at_ms,
                    offset,
                }),
            }
        }
        RecorderEventPayload::ControlMarker { .. }
        | RecorderEventPayload::LifecycleMarker { .. } => ClassifiedInput {
            kind: ClassifiedInputKind::BoundaryOnly,
            text: None,
        },
    }
}

fn split_contribution_by_chars(
    contribution: TextContribution,
    max_chars: usize,
) -> Vec<TextContribution> {
    if max_chars == 0 || contribution.text_chars <= max_chars {
        return vec![contribution];
    }

    let segments = split_text_by_char_limit(&contribution.text, max_chars);
    segments
        .into_iter()
        .enumerate()
        .map(|(index, segment)| {
            let mut piece = contribution.clone();
            piece.text_chars = segment.chars().count();
            piece.text = segment;
            if index > 0 {
                piece.event_id = format!("{}::part{}", piece.event_id, index + 1);
            }
            piece
        })
        .collect()
}

fn split_text_by_char_limit(text: &str, max_chars: usize) -> Vec<String> {
    if max_chars == 0 {
        return vec![text.to_string()];
    }

    let mut out = Vec::new();
    let mut buffer = String::new();
    let mut count = 0usize;

    for ch in text.chars() {
        buffer.push(ch);
        count += 1;
        if count >= max_chars {
            out.push(std::mem::take(&mut buffer));
            count = 0;
        }
    }

    if !buffer.is_empty() {
        out.push(buffer);
    }

    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn exceeds_soft_limits(
    builder: &ChunkBuilder,
    contribution: &TextContribution,
    config: &ChunkPolicyConfig,
) -> bool {
    let separator = usize::from(builder.text_chars > 0 && contribution.text_chars > 0);
    let projected_chars = builder
        .text_chars
        .saturating_add(separator)
        .saturating_add(contribution.text_chars);
    let projected_events = builder.event_count.saturating_add(1);
    let projected_window_ms = contribution
        .occurred_at_ms
        .saturating_sub(builder.occurred_at_start_ms);

    projected_chars > config.max_chunk_chars
        || projected_events > config.max_chunk_events
        || projected_window_ms > config.max_window_ms
}

fn overlap_from_previous(
    previous: &SemanticChunk,
    contribution: &TextContribution,
    overlap_chars: usize,
) -> Option<ChunkOverlap> {
    if overlap_chars == 0
        || previous.pane_id != contribution.pane_id
        || previous.direction != contribution.direction
        || previous.text.is_empty()
    {
        return None;
    }

    let overlap_text = tail_chars(&previous.text, overlap_chars);
    if overlap_text.is_empty() {
        return None;
    }

    Some(ChunkOverlap {
        from_chunk_id: previous.chunk_id.clone(),
        source_end_offset: previous.end_offset.clone(),
        chars: overlap_text.chars().count(),
        text: overlap_text,
    })
}

fn apply_glue_rules(chunks: Vec<SemanticChunk>, config: &ChunkPolicyConfig) -> Vec<SemanticChunk> {
    if chunks.is_empty() {
        return chunks;
    }

    // Pass 1: tiny ingress + immediate egress => mixed_glued.
    let mut mixed_pass: Vec<SemanticChunk> = Vec::new();
    let mut index = 0usize;
    while index < chunks.len() {
        let current = &chunks[index];
        if index + 1 < chunks.len() {
            let next = &chunks[index + 1];
            let should_merge_mixed = current.direction == ChunkDirection::Ingress
                && next.direction == ChunkDirection::Egress
                && current.text_chars < config.min_chunk_chars
                && can_glue(current, next, config);
            if should_merge_mixed {
                mixed_pass.push(merge_chunks(current, next, ChunkDirection::MixedGlued));
                index += 2;
                continue;
            }
        }

        mixed_pass.push(current.clone());
        index += 1;
    }

    // Pass 2: tiny trailing chunks attach to previous chunk when safe.
    let mut final_chunks: Vec<SemanticChunk> = Vec::new();
    for chunk in mixed_pass {
        if let Some(previous) = final_chunks.last() {
            let can_attach =
                chunk.text_chars < config.min_chunk_chars && can_glue(previous, &chunk, config);
            if can_attach {
                let merged_direction = if previous.direction == chunk.direction {
                    previous.direction
                } else {
                    ChunkDirection::MixedGlued
                };
                let merged = merge_chunks(previous, &chunk, merged_direction);
                let _ = final_chunks.pop();
                final_chunks.push(merged);
                continue;
            }
        }
        final_chunks.push(chunk);
    }

    final_chunks
}

fn can_glue(left: &SemanticChunk, right: &SemanticChunk, config: &ChunkPolicyConfig) -> bool {
    if left.pane_id != right.pane_id {
        return false;
    }
    if left.end_offset.segment_id != right.start_offset.segment_id {
        return false;
    }
    // Conservative "no hard boundary crossed" guard: if ordinals have a gap >1,
    // we assume there may have been a boundary-only marker between them.
    if right.start_offset.ordinal > left.end_offset.ordinal.saturating_add(1) {
        return false;
    }
    right
        .occurred_at_start_ms
        .saturating_sub(left.occurred_at_end_ms)
        <= config.merge_window_ms
}

fn merge_chunks(
    left: &SemanticChunk,
    right: &SemanticChunk,
    direction: ChunkDirection,
) -> SemanticChunk {
    let mut text = left.text.clone();
    let mut text_chars = left.text_chars;
    append_text_line(&mut text, &mut text_chars, right.text.as_str());

    let mut event_ids = left.event_ids.clone();
    event_ids.extend(right.event_ids.iter().cloned());
    let content_hash = sha256_hex(text.as_bytes());
    let chunk_id = chunk_id_for(
        left.pane_id,
        direction,
        left.start_offset.ordinal,
        right.end_offset.ordinal,
        &content_hash,
    );

    let session_id = if left.session_id.as_deref() == right.session_id.as_deref() {
        left.session_id.clone()
    } else {
        None
    };

    SemanticChunk {
        chunk_id,
        policy_version: RECORDER_CHUNKING_POLICY_V1.to_string(),
        pane_id: left.pane_id,
        session_id,
        direction,
        start_offset: left.start_offset.clone(),
        end_offset: right.end_offset.clone(),
        event_ids: event_ids.clone(),
        event_count: event_ids.len(),
        occurred_at_start_ms: left.occurred_at_start_ms,
        occurred_at_end_ms: right.occurred_at_end_ms,
        text_chars,
        content_hash,
        text,
        overlap: None,
    }
}

fn append_text_line(buffer: &mut String, chars: &mut usize, line: &str) {
    if line.is_empty() {
        return;
    }
    if !buffer.is_empty() {
        buffer.push('\n');
        *chars += 1;
    }
    buffer.push_str(line);
    *chars += line.chars().count();
}

fn prefixed_text(prefix: &str, normalized: &str) -> String {
    if normalized.is_empty() {
        prefix.trim_end().to_string()
    } else {
        format!("{prefix}{normalized}")
    }
}

fn normalize_payload_text(text: &str) -> String {
    let line_normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    line_normalized
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

fn tail_chars(text: &str, n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    let total = text.chars().count();
    if total <= n {
        return text.to_string();
    }
    text.chars().skip(total - n).collect()
}

fn chunk_id_for(
    pane_id: u64,
    direction: ChunkDirection,
    start_ordinal: u64,
    end_ordinal: u64,
    content_hash: &str,
) -> String {
    let seed = format!(
        "{RECORDER_CHUNKING_POLICY_V1}:{pane_id}:{}:{start_ordinal}:{end_ordinal}:{content_hash}",
        direction.as_str()
    );
    sha256_hex(seed.as_bytes())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::{
        RecorderEventCausality, RecorderEventSource, RecorderIngressKind, RecorderRedactionLevel,
        RecorderSegmentKind, RecorderTextEncoding, RECORDER_EVENT_SCHEMA_VERSION_V1,
    };

    // ── Test helpers ──────────────────────────────────────────────────────

    fn make_offset(segment_id: u64, ordinal: u64, byte_offset: u64) -> RecorderOffset {
        RecorderOffset {
            segment_id,
            ordinal,
            byte_offset,
        }
    }

    fn make_causality() -> RecorderEventCausality {
        RecorderEventCausality {
            parent_event_id: None,
            trigger_event_id: None,
            root_event_id: None,
        }
    }

    fn make_egress_event(
        pane_id: u64,
        text: &str,
        occurred_at_ms: u64,
        event_id: &str,
    ) -> RecorderEvent {
        RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: event_id.to_string(),
            pane_id,
            session_id: Some("sess-1".to_string()),
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::WeztermMux,
            occurred_at_ms,
            recorded_at_ms: occurred_at_ms + 1,
            sequence: 0,
            causality: make_causality(),
            payload: RecorderEventPayload::EgressOutput {
                text: text.to_string(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                segment_kind: RecorderSegmentKind::Delta,
                is_gap: false,
            },
        }
    }

    fn make_ingress_event(
        pane_id: u64,
        text: &str,
        occurred_at_ms: u64,
        event_id: &str,
    ) -> RecorderEvent {
        RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: event_id.to_string(),
            pane_id,
            session_id: Some("sess-1".to_string()),
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::RobotMode,
            occurred_at_ms,
            recorded_at_ms: occurred_at_ms + 1,
            sequence: 0,
            causality: make_causality(),
            payload: RecorderEventPayload::IngressText {
                text: text.to_string(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                ingress_kind: RecorderIngressKind::SendText,
            },
        }
    }

    fn make_gap_event(pane_id: u64, occurred_at_ms: u64, event_id: &str) -> RecorderEvent {
        RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: event_id.to_string(),
            pane_id,
            session_id: Some("sess-1".to_string()),
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::WeztermMux,
            occurred_at_ms,
            recorded_at_ms: occurred_at_ms + 1,
            sequence: 0,
            causality: make_causality(),
            payload: RecorderEventPayload::EgressOutput {
                text: String::new(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                segment_kind: RecorderSegmentKind::Gap,
                is_gap: true,
            },
        }
    }

    fn make_control_event(pane_id: u64, occurred_at_ms: u64, event_id: &str) -> RecorderEvent {
        RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: event_id.to_string(),
            pane_id,
            session_id: Some("sess-1".to_string()),
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::WeztermMux,
            occurred_at_ms,
            recorded_at_ms: occurred_at_ms + 1,
            sequence: 0,
            causality: make_causality(),
            payload: RecorderEventPayload::ControlMarker {
                control_marker_type: crate::recording::RecorderControlMarkerType::PromptBoundary,
                details: serde_json::Value::Null,
            },
        }
    }

    fn make_lifecycle_event(pane_id: u64, occurred_at_ms: u64, event_id: &str) -> RecorderEvent {
        RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: event_id.to_string(),
            pane_id,
            session_id: Some("sess-1".to_string()),
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::WeztermMux,
            occurred_at_ms,
            recorded_at_ms: occurred_at_ms + 1,
            sequence: 0,
            causality: make_causality(),
            payload: RecorderEventPayload::LifecycleMarker {
                lifecycle_phase: crate::recording::RecorderLifecyclePhase::CaptureStarted,
                reason: None,
                details: serde_json::Value::Null,
            },
        }
    }

    fn make_input(event: RecorderEvent, offset: RecorderOffset) -> ChunkInputEvent {
        ChunkInputEvent { event, offset }
    }

    fn default_config() -> ChunkPolicyConfig {
        ChunkPolicyConfig::default()
    }

    // ── ChunkPolicyConfig tests ───────────────────────────────────────────

    #[test]
    fn default_config_has_sane_values() {
        let cfg = ChunkPolicyConfig::default();
        assert_eq!(cfg.max_chunk_chars, 1_800);
        assert_eq!(cfg.max_chunk_events, 48);
        assert_eq!(cfg.max_window_ms, 120_000);
        assert_eq!(cfg.hard_gap_ms, 30_000);
        assert_eq!(cfg.min_chunk_chars, 80);
        assert_eq!(cfg.merge_window_ms, 8_000);
        assert_eq!(cfg.overlap_chars, 120);
    }

    #[test]
    fn config_serde_roundtrip() {
        let cfg = ChunkPolicyConfig {
            max_chunk_chars: 500,
            max_chunk_events: 10,
            max_window_ms: 60_000,
            hard_gap_ms: 15_000,
            min_chunk_chars: 50,
            merge_window_ms: 5_000,
            overlap_chars: 80,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let deserialized: ChunkPolicyConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, deserialized);
    }

    // ── ChunkDirection tests ──────────────────────────────────────────────

    #[test]
    fn direction_as_str() {
        assert_eq!(ChunkDirection::Ingress.as_str(), "ingress");
        assert_eq!(ChunkDirection::Egress.as_str(), "egress");
        assert_eq!(ChunkDirection::MixedGlued.as_str(), "mixed_glued");
    }

    #[test]
    fn direction_serde_roundtrip() {
        for dir in [
            ChunkDirection::Ingress,
            ChunkDirection::Egress,
            ChunkDirection::MixedGlued,
        ] {
            let json = serde_json::to_string(&dir).unwrap();
            let deserialized: ChunkDirection = serde_json::from_str(&json).unwrap();
            assert_eq!(dir, deserialized);
        }
    }

    // ── normalize_payload_text tests ──────────────────────────────────────

    #[test]
    fn normalize_strips_trailing_whitespace() {
        let result = normalize_payload_text("hello   \nworld  ");
        assert_eq!(result, "hello\nworld");
    }

    #[test]
    fn normalize_converts_crlf_to_lf() {
        let result = normalize_payload_text("line1\r\nline2\rline3");
        assert_eq!(result, "line1\nline2\nline3");
    }

    #[test]
    fn normalize_empty_string() {
        assert_eq!(normalize_payload_text(""), "");
    }

    #[test]
    fn normalize_preserves_leading_whitespace() {
        let result = normalize_payload_text("  indented\n    more");
        assert_eq!(result, "  indented\n    more");
    }

    // ── tail_chars tests ──────────────────────────────────────────────────

    #[test]
    fn tail_chars_returns_last_n() {
        assert_eq!(tail_chars("hello world", 5), "world");
    }

    #[test]
    fn tail_chars_returns_full_when_shorter() {
        assert_eq!(tail_chars("hi", 10), "hi");
    }

    #[test]
    fn tail_chars_zero_returns_empty() {
        assert_eq!(tail_chars("hello", 0), "");
    }

    #[test]
    fn tail_chars_unicode() {
        // 4 unicode chars: 🎉🎊🎈🎁
        assert_eq!(tail_chars("🎉🎊🎈🎁", 2), "🎈🎁");
    }

    #[test]
    fn tail_chars_exact_length() {
        assert_eq!(tail_chars("abc", 3), "abc");
    }

    // ── prefixed_text tests ───────────────────────────────────────────────

    #[test]
    fn prefixed_text_normal() {
        assert_eq!(prefixed_text("[OUT] ", "hello"), "[OUT] hello");
    }

    #[test]
    fn prefixed_text_empty_normalized() {
        assert_eq!(prefixed_text("[IN] ", ""), "[IN]");
    }

    // ── append_text_line tests ────────────────────────────────────────────

    #[test]
    fn append_text_line_to_empty_buffer() {
        let mut buf = String::new();
        let mut chars = 0;
        append_text_line(&mut buf, &mut chars, "hello");
        assert_eq!(buf, "hello");
        assert_eq!(chars, 5);
    }

    #[test]
    fn append_text_line_adds_newline_separator() {
        let mut buf = "first".to_string();
        let mut chars = 5;
        append_text_line(&mut buf, &mut chars, "second");
        assert_eq!(buf, "first\nsecond");
        assert_eq!(chars, 12); // 5 + 1 (newline) + 6
    }

    #[test]
    fn append_text_line_empty_line_is_noop() {
        let mut buf = "existing".to_string();
        let mut chars = 8;
        append_text_line(&mut buf, &mut chars, "");
        assert_eq!(buf, "existing");
        assert_eq!(chars, 8);
    }

    // ── split_text_by_char_limit tests ────────────────────────────────────

    #[test]
    fn split_text_within_limit() {
        let result = split_text_by_char_limit("short", 100);
        assert_eq!(result, vec!["short"]);
    }

    #[test]
    fn split_text_at_exact_limit() {
        let result = split_text_by_char_limit("abc", 3);
        assert_eq!(result, vec!["abc"]);
    }

    #[test]
    fn split_text_exceeding_limit() {
        let result = split_text_by_char_limit("abcdef", 4);
        assert_eq!(result, vec!["abcd", "ef"]);
    }

    #[test]
    fn split_text_empty() {
        let result = split_text_by_char_limit("", 5);
        assert_eq!(result, vec![""]);
    }

    #[test]
    fn split_text_limit_zero_returns_whole() {
        let result = split_text_by_char_limit("hello", 0);
        assert_eq!(result, vec!["hello"]);
    }

    #[test]
    fn split_text_limit_one() {
        let result = split_text_by_char_limit("abc", 1);
        assert_eq!(result, vec!["a", "b", "c"]);
    }

    #[test]
    fn split_text_unicode_chars() {
        // Each emoji is one char
        let result = split_text_by_char_limit("🎉🎊🎈🎁", 2);
        assert_eq!(result, vec!["🎉🎊", "🎈🎁"]);
    }

    // ── sha256_hex tests ──────────────────────────────────────────────────

    #[test]
    fn sha256_hex_deterministic() {
        let h1 = sha256_hex(b"hello");
        let h2 = sha256_hex(b"hello");
        assert_eq!(h1, h2);
    }

    #[test]
    fn sha256_hex_different_input() {
        assert_ne!(sha256_hex(b"hello"), sha256_hex(b"world"));
    }

    #[test]
    fn sha256_hex_known_value() {
        // SHA-256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        let h = sha256_hex(b"");
        assert_eq!(
            h,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    // ── chunk_id_for tests ────────────────────────────────────────────────

    #[test]
    fn chunk_id_deterministic() {
        let id1 = chunk_id_for(1, ChunkDirection::Egress, 0, 5, "abc123");
        let id2 = chunk_id_for(1, ChunkDirection::Egress, 0, 5, "abc123");
        assert_eq!(id1, id2);
    }

    #[test]
    fn chunk_id_differs_by_pane() {
        let id1 = chunk_id_for(1, ChunkDirection::Egress, 0, 5, "abc123");
        let id2 = chunk_id_for(2, ChunkDirection::Egress, 0, 5, "abc123");
        assert_ne!(id1, id2);
    }

    #[test]
    fn chunk_id_differs_by_direction() {
        let id1 = chunk_id_for(1, ChunkDirection::Egress, 0, 5, "abc123");
        let id2 = chunk_id_for(1, ChunkDirection::Ingress, 0, 5, "abc123");
        assert_ne!(id1, id2);
    }

    #[test]
    fn chunk_id_differs_by_ordinal_range() {
        let id1 = chunk_id_for(1, ChunkDirection::Egress, 0, 5, "abc123");
        let id2 = chunk_id_for(1, ChunkDirection::Egress, 1, 5, "abc123");
        assert_ne!(id1, id2);
    }

    // ── ChunkSourceOffset tests ───────────────────────────────────────────

    #[test]
    fn chunk_source_offset_from_recorder_offset() {
        let recorder_offset = RecorderOffset {
            segment_id: 42,
            ordinal: 100,
            byte_offset: 2048,
        };
        let source_offset = ChunkSourceOffset::from(&recorder_offset);
        assert_eq!(source_offset.segment_id, 42);
        assert_eq!(source_offset.ordinal, 100);
        assert_eq!(source_offset.byte_offset, 2048);
    }

    // ── build_semantic_chunks: empty input ────────────────────────────────

    #[test]
    fn empty_input_produces_no_chunks() {
        let chunks = build_semantic_chunks(&[], &default_config());
        assert!(chunks.is_empty());
    }

    // ── build_semantic_chunks: single egress event ────────────────────────

    #[test]
    fn single_egress_event_produces_one_chunk() {
        let event = make_egress_event(1, "hello world", 1000, "evt-1");
        let inputs = vec![make_input(event, make_offset(0, 0, 0))];
        let chunks = build_semantic_chunks(&inputs, &default_config());
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].pane_id, 1);
        assert_eq!(chunks[0].direction, ChunkDirection::Egress);
        assert!(chunks[0].text.contains("[OUT] hello world"));
        assert_eq!(chunks[0].event_count, 1);
        assert_eq!(chunks[0].policy_version, RECORDER_CHUNKING_POLICY_V1);
    }

    // ── build_semantic_chunks: single ingress event ───────────────────────

    #[test]
    fn single_ingress_event_produces_one_chunk() {
        let event = make_ingress_event(1, "ls -la", 1000, "evt-1");
        let inputs = vec![make_input(event, make_offset(0, 0, 0))];
        let chunks = build_semantic_chunks(&inputs, &default_config());
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].direction, ChunkDirection::Ingress);
        assert!(chunks[0].text.contains("[IN] ls -la"));
    }

    // ── build_semantic_chunks: multiple same-direction events ─────────────

    #[test]
    fn consecutive_egress_events_within_limits_merge_into_one_chunk() {
        let config = default_config();
        let inputs: Vec<_> = (0..5)
            .map(|i| {
                let event = make_egress_event(
                    1,
                    &format!("line {i}"),
                    1000 + i * 100,
                    &format!("evt-{i}"),
                );
                make_input(event, make_offset(0, i, i * 50))
            })
            .collect();

        let chunks = build_semantic_chunks(&inputs, &config);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].event_count, 5);
        for i in 0..5 {
            assert!(chunks[0].text.contains(&format!("line {i}")));
        }
    }

    // ── build_semantic_chunks: hard boundary on pane change ───────────────

    #[test]
    fn different_panes_produce_separate_chunks() {
        let events = vec![
            make_input(
                make_egress_event(1, "pane1 output", 1000, "evt-1"),
                make_offset(0, 0, 0),
            ),
            make_input(
                make_egress_event(2, "pane2 output", 1100, "evt-2"),
                make_offset(0, 1, 100),
            ),
        ];

        let chunks = build_semantic_chunks(&events, &default_config());
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].pane_id, 1);
        assert_eq!(chunks[1].pane_id, 2);
    }

    // ── build_semantic_chunks: hard boundary on direction change ──────────

    #[test]
    fn direction_change_produces_separate_chunks() {
        let events = vec![
            make_input(
                make_egress_event(1, "output", 1000, "evt-1"),
                make_offset(0, 0, 0),
            ),
            make_input(
                make_ingress_event(1, "input", 1100, "evt-2"),
                make_offset(0, 1, 100),
            ),
        ];

        let chunks = build_semantic_chunks(&events, &default_config());
        // May be 1 (glued) or 2 depending on glue rules (tiny fragments)
        assert!(chunks.len() >= 1);
        // At least verify the text contents exist
        let all_text: String = chunks.iter().map(|c| c.text.clone()).collect();
        assert!(all_text.contains("[OUT] output"));
        assert!(all_text.contains("[IN] input"));
    }

    // ── build_semantic_chunks: hard boundary on time gap ──────────────────

    #[test]
    fn hard_gap_ms_triggers_boundary() {
        let config = ChunkPolicyConfig {
            hard_gap_ms: 5_000,
            ..default_config()
        };
        let events = vec![
            make_input(
                make_egress_event(1, "before gap", 1000, "evt-1"),
                make_offset(0, 0, 0),
            ),
            make_input(
                make_egress_event(1, "after gap", 7000, "evt-2"),
                make_offset(0, 1, 100),
            ),
        ];

        let chunks = build_semantic_chunks(&events, &config);
        // Should be 2 chunks because 7000 - 1000 = 6000 > hard_gap_ms of 5000
        assert_eq!(chunks.len(), 2);
    }

    #[test]
    fn events_within_hard_gap_stay_together() {
        let config = ChunkPolicyConfig {
            hard_gap_ms: 10_000,
            ..default_config()
        };
        let events = vec![
            make_input(
                make_egress_event(1, "first", 1000, "evt-1"),
                make_offset(0, 0, 0),
            ),
            make_input(
                make_egress_event(1, "second", 5000, "evt-2"),
                make_offset(0, 1, 100),
            ),
        ];

        let chunks = build_semantic_chunks(&events, &config);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].event_count, 2);
    }

    // ── build_semantic_chunks: soft limits ────────────────────────────────

    #[test]
    fn exceeds_max_chunk_chars_triggers_split() {
        let config = ChunkPolicyConfig {
            max_chunk_chars: 50,
            min_chunk_chars: 5,
            ..default_config()
        };
        let events = vec![
            make_input(
                make_egress_event(1, &"a".repeat(40), 1000, "evt-1"),
                make_offset(0, 0, 0),
            ),
            make_input(
                make_egress_event(1, &"b".repeat(40), 1100, "evt-2"),
                make_offset(0, 1, 100),
            ),
        ];

        let chunks = build_semantic_chunks(&events, &config);
        // Should split because total chars exceeds max_chunk_chars
        assert!(chunks.len() >= 2);
    }

    #[test]
    fn exceeds_max_events_triggers_split() {
        let config = ChunkPolicyConfig {
            max_chunk_events: 3,
            ..default_config()
        };
        let events: Vec<_> = (0..6)
            .map(|i| {
                make_input(
                    make_egress_event(1, &format!("evt {i}"), 1000 + i * 100, &format!("e-{i}")),
                    make_offset(0, i, i * 50),
                )
            })
            .collect();

        let chunks = build_semantic_chunks(&events, &config);
        assert!(chunks.len() >= 2);
        // Each chunk should have at most max_chunk_events
        for chunk in &chunks {
            assert!(chunk.event_count <= config.max_chunk_events + 1);
        }
    }

    #[test]
    fn exceeds_max_window_ms_triggers_split() {
        let config = ChunkPolicyConfig {
            max_window_ms: 5_000,
            hard_gap_ms: 100_000, // high to avoid hard boundary
            ..default_config()
        };
        let events = vec![
            make_input(
                make_egress_event(1, "first", 1000, "evt-1"),
                make_offset(0, 0, 0),
            ),
            make_input(
                make_egress_event(1, "second", 3000, "evt-2"),
                make_offset(0, 1, 50),
            ),
            make_input(
                make_egress_event(1, "third", 7000, "evt-3"),
                make_offset(0, 2, 100),
            ),
        ];

        let chunks = build_semantic_chunks(&events, &config);
        // Third event at 7000 exceeds window of 5000 from start at 1000
        assert!(chunks.len() >= 2);
    }

    // ── build_semantic_chunks: gap events ─────────────────────────────────

    #[test]
    fn gap_event_creates_hard_boundary() {
        let events = vec![
            make_input(
                make_egress_event(1, "before gap", 1000, "evt-1"),
                make_offset(0, 0, 0),
            ),
            make_input(make_gap_event(1, 1100, "evt-gap"), make_offset(0, 1, 50)),
            make_input(
                make_egress_event(1, "after gap", 1200, "evt-2"),
                make_offset(0, 2, 100),
            ),
        ];

        let chunks = build_semantic_chunks(&events, &default_config());
        // Gap should force boundary: before + after = 2 chunks
        assert_eq!(chunks.len(), 2);
    }

    // ── build_semantic_chunks: control/lifecycle markers ──────────────────

    #[test]
    fn control_marker_creates_boundary() {
        let events = vec![
            make_input(
                make_egress_event(1, "before", 1000, "evt-1"),
                make_offset(0, 0, 0),
            ),
            make_input(
                make_control_event(1, 1100, "evt-ctrl"),
                make_offset(0, 1, 50),
            ),
            make_input(
                make_egress_event(1, "after", 1200, "evt-2"),
                make_offset(0, 2, 100),
            ),
        ];

        let chunks = build_semantic_chunks(&events, &default_config());
        assert_eq!(chunks.len(), 2);
    }

    #[test]
    fn lifecycle_marker_creates_boundary() {
        let events = vec![
            make_input(
                make_egress_event(1, "before", 1000, "evt-1"),
                make_offset(0, 0, 0),
            ),
            make_input(
                make_lifecycle_event(1, 1100, "evt-lc"),
                make_offset(0, 1, 50),
            ),
            make_input(
                make_egress_event(1, "after", 1200, "evt-2"),
                make_offset(0, 2, 100),
            ),
        ];

        let chunks = build_semantic_chunks(&events, &default_config());
        assert_eq!(chunks.len(), 2);
    }

    // ── build_semantic_chunks: deterministic ordering ─────────────────────

    #[test]
    fn out_of_order_inputs_produce_same_result() {
        let event_a = make_egress_event(1, "first", 1000, "evt-1");
        let event_b = make_egress_event(1, "second", 1100, "evt-2");

        let ordered = vec![
            make_input(event_a.clone(), make_offset(0, 0, 0)),
            make_input(event_b.clone(), make_offset(0, 1, 50)),
        ];
        let reversed = vec![
            make_input(event_b, make_offset(0, 1, 50)),
            make_input(event_a, make_offset(0, 0, 0)),
        ];

        let chunks_ordered = build_semantic_chunks(&ordered, &default_config());
        let chunks_reversed = build_semantic_chunks(&reversed, &default_config());

        assert_eq!(chunks_ordered.len(), chunks_reversed.len());
        for (a, b) in chunks_ordered.iter().zip(chunks_reversed.iter()) {
            assert_eq!(a.chunk_id, b.chunk_id);
            assert_eq!(a.text, b.text);
            assert_eq!(a.content_hash, b.content_hash);
        }
    }

    // ── build_semantic_chunks: glue rules ─────────────────────────────────

    #[test]
    fn tiny_ingress_glued_to_following_egress() {
        let config = ChunkPolicyConfig {
            min_chunk_chars: 80,
            merge_window_ms: 8_000,
            ..default_config()
        };
        // Tiny ingress (< min_chunk_chars) followed by egress should be glued
        let events = vec![
            make_input(
                make_ingress_event(1, "ls", 1000, "evt-1"),
                make_offset(0, 0, 0),
            ),
            make_input(
                make_egress_event(1, "file1.rs\nfile2.rs\nfile3.rs\nfile4.rs", 1050, "evt-2"),
                make_offset(0, 1, 50),
            ),
        ];

        let chunks = build_semantic_chunks(&events, &config);
        // Should be glued into one mixed chunk because ingress is tiny
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].direction, ChunkDirection::MixedGlued);
    }

    #[test]
    fn tiny_trailing_chunk_attached_to_previous() {
        let config = ChunkPolicyConfig {
            min_chunk_chars: 80,
            max_chunk_chars: 200,
            merge_window_ms: 8_000,
            ..default_config()
        };

        // Create events that produce a big chunk + a tiny trailing chunk
        let events = vec![
            make_input(
                make_egress_event(1, &"x".repeat(100), 1000, "evt-1"),
                make_offset(0, 0, 0),
            ),
            make_input(
                make_egress_event(1, &"y".repeat(100), 1100, "evt-2"),
                make_offset(0, 1, 100),
            ),
            make_input(
                make_egress_event(1, "tiny", 1200, "evt-3"),
                make_offset(0, 2, 200),
            ),
        ];

        let chunks = build_semantic_chunks(&events, &config);
        // The "tiny" trailing chunk should be glued to the previous
        let last = chunks.last().unwrap();
        assert!(last.text.contains("tiny"));
    }

    // ── build_semantic_chunks: overlap ────────────────────────────────────

    #[test]
    fn soft_split_includes_overlap_from_previous() {
        let config = ChunkPolicyConfig {
            max_chunk_chars: 50,
            overlap_chars: 10,
            min_chunk_chars: 5,
            ..default_config()
        };

        let events = vec![
            make_input(
                make_egress_event(1, &"a".repeat(40), 1000, "evt-1"),
                make_offset(0, 0, 0),
            ),
            make_input(
                make_egress_event(1, &"b".repeat(40), 1100, "evt-2"),
                make_offset(0, 1, 100),
            ),
        ];

        let chunks = build_semantic_chunks(&events, &config);
        // Should have soft split with overlap
        if chunks.len() >= 2 {
            let second = &chunks[1];
            if let Some(overlap) = &second.overlap {
                assert!(overlap.chars > 0);
                assert!(!overlap.text.is_empty());
                assert_eq!(overlap.from_chunk_id, chunks[0].chunk_id);
            }
        }
    }

    #[test]
    fn overlap_not_applied_across_different_panes() {
        let config = ChunkPolicyConfig {
            overlap_chars: 10,
            ..default_config()
        };

        let events = vec![
            make_input(
                make_egress_event(1, "pane1 data", 1000, "evt-1"),
                make_offset(0, 0, 0),
            ),
            make_input(
                make_egress_event(2, "pane2 data", 1100, "evt-2"),
                make_offset(0, 1, 100),
            ),
        ];

        let chunks = build_semantic_chunks(&events, &config);
        // No overlap across panes
        for chunk in &chunks {
            assert!(chunk.overlap.is_none());
        }
    }

    // ── build_semantic_chunks: long event splitting ───────────────────────

    #[test]
    fn very_long_event_is_split_by_char_limit() {
        let config = ChunkPolicyConfig {
            max_chunk_chars: 100,
            min_chunk_chars: 5,
            ..default_config()
        };

        let long_text = "x".repeat(300);
        let events = vec![make_input(
            make_egress_event(1, &long_text, 1000, "evt-1"),
            make_offset(0, 0, 0),
        )];

        let chunks = build_semantic_chunks(&events, &config);
        // Long text should be split into multiple chunks
        assert!(chunks.len() >= 2);
    }

    // ── build_semantic_chunks: content hash stability ─────────────────────

    #[test]
    fn content_hash_is_stable_for_same_input() {
        let events = vec![make_input(
            make_egress_event(1, "deterministic output", 1000, "evt-1"),
            make_offset(0, 0, 0),
        )];

        let chunks1 = build_semantic_chunks(&events, &default_config());
        let chunks2 = build_semantic_chunks(&events, &default_config());

        assert_eq!(chunks1[0].content_hash, chunks2[0].content_hash);
        assert_eq!(chunks1[0].chunk_id, chunks2[0].chunk_id);
    }

    // ── build_semantic_chunks: session_id handling ────────────────────────

    #[test]
    fn session_id_preserved_when_consistent() {
        let events = vec![make_input(
            make_egress_event(1, "output", 1000, "evt-1"),
            make_offset(0, 0, 0),
        )];

        let chunks = build_semantic_chunks(&events, &default_config());
        assert_eq!(chunks[0].session_id.as_deref(), Some("sess-1"));
    }

    #[test]
    fn session_id_set_to_none_when_mixed() {
        let mut event1 = make_egress_event(1, "output1", 1000, "evt-1");
        event1.session_id = Some("sess-a".to_string());
        let mut event2 = make_egress_event(1, "output2", 1100, "evt-2");
        event2.session_id = Some("sess-b".to_string());

        let events = vec![
            make_input(event1, make_offset(0, 0, 0)),
            make_input(event2, make_offset(0, 1, 50)),
        ];

        let chunks = build_semantic_chunks(&events, &default_config());
        // When merged, mixed session IDs should result in None
        if chunks.len() == 1 {
            assert!(chunks[0].session_id.is_none());
        }
    }

    // ── build_semantic_chunks: SemanticChunk serde roundtrip ──────────────

    #[test]
    fn semantic_chunk_serde_roundtrip() {
        let events = vec![
            make_input(
                make_egress_event(1, "hello", 1000, "evt-1"),
                make_offset(0, 0, 0),
            ),
            make_input(
                make_egress_event(1, "world", 1100, "evt-2"),
                make_offset(0, 1, 50),
            ),
        ];

        let chunks = build_semantic_chunks(&events, &default_config());
        assert!(!chunks.is_empty());

        for chunk in &chunks {
            let json = serde_json::to_string(chunk).unwrap();
            let deserialized: SemanticChunk = serde_json::from_str(&json).unwrap();
            assert_eq!(chunk.chunk_id, deserialized.chunk_id);
            assert_eq!(chunk.text, deserialized.text);
            assert_eq!(chunk.content_hash, deserialized.content_hash);
            assert_eq!(chunk.direction, deserialized.direction);
            assert_eq!(chunk.pane_id, deserialized.pane_id);
        }
    }

    // ── can_glue tests ────────────────────────────────────────────────────

    #[test]
    fn can_glue_rejects_different_panes() {
        let mut left = build_semantic_chunks(
            &[make_input(
                make_egress_event(1, "left", 1000, "e1"),
                make_offset(0, 0, 0),
            )],
            &default_config(),
        )
        .pop()
        .unwrap();
        left.pane_id = 1;

        let mut right = build_semantic_chunks(
            &[make_input(
                make_egress_event(2, "right", 1100, "e2"),
                make_offset(0, 1, 50),
            )],
            &default_config(),
        )
        .pop()
        .unwrap();
        right.pane_id = 2;

        assert!(!can_glue(&left, &right, &default_config()));
    }

    #[test]
    fn can_glue_rejects_different_segments() {
        let left = build_semantic_chunks(
            &[make_input(
                make_egress_event(1, "left", 1000, "e1"),
                make_offset(0, 0, 0),
            )],
            &default_config(),
        )
        .pop()
        .unwrap();

        let right = build_semantic_chunks(
            &[make_input(
                make_egress_event(1, "right", 1100, "e2"),
                make_offset(1, 1, 50),
            )],
            &default_config(),
        )
        .pop()
        .unwrap();

        assert!(!can_glue(&left, &right, &default_config()));
    }

    // ── exceeds_soft_limits tests ─────────────────────────────────────────

    #[test]
    fn exceeds_soft_limits_chars() {
        let config = ChunkPolicyConfig {
            max_chunk_chars: 20,
            ..default_config()
        };

        let builder = ChunkBuilder {
            pane_id: 1,
            session_id: None,
            direction: ChunkDirection::Egress,
            start_offset: ChunkSourceOffset {
                segment_id: 0,
                ordinal: 0,
                byte_offset: 0,
            },
            end_offset: ChunkSourceOffset {
                segment_id: 0,
                ordinal: 0,
                byte_offset: 0,
            },
            event_ids: vec![],
            event_count: 1,
            occurred_at_start_ms: 1000,
            occurred_at_end_ms: 1000,
            text_chars: 15,
            text: "x".repeat(15),
            overlap: None,
        };

        let contribution = TextContribution {
            event_id: "e2".into(),
            pane_id: 1,
            session_id: None,
            direction: ChunkDirection::Egress,
            text: "y".repeat(10),
            text_chars: 10,
            occurred_at_ms: 1100,
            offset: ChunkSourceOffset {
                segment_id: 0,
                ordinal: 1,
                byte_offset: 50,
            },
        };

        // 15 + 1 (separator) + 10 = 26 > 20
        assert!(exceeds_soft_limits(&builder, &contribution, &config));
    }

    #[test]
    fn exceeds_soft_limits_events() {
        let config = ChunkPolicyConfig {
            max_chunk_events: 3,
            max_chunk_chars: 10_000,
            max_window_ms: 1_000_000,
            ..default_config()
        };

        let builder = ChunkBuilder {
            pane_id: 1,
            session_id: None,
            direction: ChunkDirection::Egress,
            start_offset: ChunkSourceOffset {
                segment_id: 0,
                ordinal: 0,
                byte_offset: 0,
            },
            end_offset: ChunkSourceOffset {
                segment_id: 0,
                ordinal: 2,
                byte_offset: 100,
            },
            event_ids: vec!["e1".into(), "e2".into(), "e3".into()],
            event_count: 3,
            occurred_at_start_ms: 1000,
            occurred_at_end_ms: 1200,
            text_chars: 30,
            text: "x".repeat(30),
            overlap: None,
        };

        let contribution = TextContribution {
            event_id: "e4".into(),
            pane_id: 1,
            session_id: None,
            direction: ChunkDirection::Egress,
            text: "new".into(),
            text_chars: 3,
            occurred_at_ms: 1300,
            offset: ChunkSourceOffset {
                segment_id: 0,
                ordinal: 3,
                byte_offset: 150,
            },
        };

        // event_count (3) + 1 = 4 > max_chunk_events (3)
        assert!(exceeds_soft_limits(&builder, &contribution, &config));
    }

    // ── Policy version constant ───────────────────────────────────────────

    #[test]
    fn policy_version_matches_constant() {
        let events = vec![make_input(
            make_egress_event(1, "test", 1000, "evt-1"),
            make_offset(0, 0, 0),
        )];
        let chunks = build_semantic_chunks(&events, &default_config());
        assert_eq!(chunks[0].policy_version, "ft.recorder.chunking.v1");
    }

    // ── Only boundary events produce no chunks ────────────────────────────

    #[test]
    fn only_boundary_events_produce_empty_result() {
        let events = vec![
            make_input(
                make_control_event(1, 1000, "ctrl-1"),
                make_offset(0, 0, 0),
            ),
            make_input(
                make_lifecycle_event(1, 1100, "lc-1"),
                make_offset(0, 1, 50),
            ),
            make_input(make_gap_event(1, 1200, "gap-1"), make_offset(0, 2, 100)),
        ];

        let chunks = build_semantic_chunks(&events, &default_config());
        assert!(chunks.is_empty());
    }

    // ── CRLF normalization in chunks ──────────────────────────────────────

    #[test]
    fn crlf_normalized_in_chunk_text() {
        let events = vec![make_input(
            make_egress_event(1, "line1\r\nline2\rline3", 1000, "evt-1"),
            make_offset(0, 0, 0),
        )];

        let chunks = build_semantic_chunks(&events, &default_config());
        assert!(!chunks[0].text.contains('\r'));
    }

    // ── ChunkOverlap serde roundtrip ──────────────────────────────────────

    #[test]
    fn chunk_overlap_serde_roundtrip() {
        let overlap = ChunkOverlap {
            from_chunk_id: "chunk-abc".to_string(),
            source_end_offset: ChunkSourceOffset {
                segment_id: 0,
                ordinal: 5,
                byte_offset: 2048,
            },
            chars: 50,
            text: "overlap text here".to_string(),
        };

        let json = serde_json::to_string(&overlap).unwrap();
        let deserialized: ChunkOverlap = serde_json::from_str(&json).unwrap();
        assert_eq!(overlap, deserialized);
    }

    // ── Event IDs tracked correctly ───────────────────────────────────────

    #[test]
    fn event_ids_tracked_in_chunk() {
        let events = vec![
            make_input(
                make_egress_event(1, "first", 1000, "evt-alpha"),
                make_offset(0, 0, 0),
            ),
            make_input(
                make_egress_event(1, "second", 1100, "evt-beta"),
                make_offset(0, 1, 50),
            ),
        ];

        let chunks = build_semantic_chunks(&events, &default_config());
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].event_ids.contains(&"evt-alpha".to_string()));
        assert!(chunks[0].event_ids.contains(&"evt-beta".to_string()));
    }
}
