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
                allow_overlap_on_next_start = true;
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
