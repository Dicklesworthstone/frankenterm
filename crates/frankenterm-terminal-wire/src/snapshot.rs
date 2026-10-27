//! Frozen terminal snapshots prepared completely before transport publication.
//!
//! The caller must obtain a coherent, authorized mux image. This module only
//! packages that owned image; it performs no mux read or delivery acknowledgment.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{self, Write};

use serde::Serialize;

use crate::{
    BoundedVec, Cursor, Dimensions, GridPatch, Hyperlink, Id, ImageReference, Line,
    Scope, ServerFrame, ServerMessage, ServerSession, SnapshotChunk, SnapshotEnd,
    SnapshotStart, TerminalModes, Text, WireError, WireU64, MAX_CHUNKS,
    MAX_CHUNK_CELLS, MAX_CHUNK_LINES, MAX_FRAME_BYTES, MAX_RESOURCES, MAX_ROWS,
    MAX_SNAPSHOT_BYTES,
};

/// Maximum unique references across a complete snapshot. Each emitted chunk
/// still carries at most MAX_RESOURCES references of either kind.
pub const MAX_SNAPSHOT_RESOURCES: usize = MAX_RESOURCES * MAX_CHUNKS as usize;

/// An owned, already coherent image from the terminal-state owner. Full rows
/// are moved into chunks; neither cell text nor the complete image is cloned.
#[derive(Debug)]
pub struct FrozenSnapshot {
    pub scope: Scope,
    pub snapshot_id: WireU64,
    pub sequence: WireU64,
    pub dimensions: Dimensions,
    pub cursor: Cursor,
    pub modes: TerminalModes,
    pub title: Text<4096>,
    pub cwd: Option<Text<4096>>,
    pub lines: BoundedVec<Line, { MAX_ROWS as usize }>,
    pub hyperlinks: BoundedVec<Hyperlink, MAX_SNAPSHOT_RESOURCES>,
    pub images: BoundedVec<ImageReference, MAX_SNAPSHOT_RESOURCES>,
}

/// A complete, validated publication. A late overflow or invalid row returns
/// an error instead of exposing a start frame or a partially prepared stream.
#[derive(Debug)]
pub struct SnapshotPublication {
    scope: Scope,
    snapshot_id: WireU64,
    sequence: WireU64,
    canonical_bytes: u32,
    wire_bytes: usize,
    frames: Vec<Vec<u8>>,
}

impl SnapshotPublication {
    pub fn scope(&self) -> &Scope { &self.scope }
    pub fn snapshot_id(&self) -> WireU64 { self.snapshot_id }
    pub fn sequence(&self) -> WireU64 { self.sequence }
    pub fn canonical_bytes(&self) -> u32 { self.canonical_bytes }
    pub fn wire_bytes(&self) -> usize { self.wire_bytes }
    pub fn frames(&self) -> &[Vec<u8>] { &self.frames }
    pub fn into_frames(self) -> Vec<Vec<u8>> { self.frames }
}

struct ByteCounter {
    bytes: usize,
    limit_hit: bool,
}

impl Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_FRAME_BYTES.saturating_sub(self.bytes) {
            self.limit_hit = true;
            return Err(io::Error::other("terminal_wire_frame_too_large"));
        }
        self.bytes += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

fn encoded_bytes<T: Serialize>(value: &T) -> Result<usize, WireError> {
    let mut writer = ByteCounter { bytes: 0, limit_hit: false };
    if serde_json::to_writer(&mut writer, value).is_err() {
        return Err(if writer.limit_hit { WireError::FrameTooLarge } else { WireError::Malformed });
    }
    Ok(writer.bytes)
}

/// Canonical compact JSON bytes of one SnapshotChunk payload, excluding its
/// ServerFrame/session envelope. SnapshotStart/End.total_bytes is the exact
/// sum of these counts. This counts without allocating a serialized copy.
///
/// This is a size helper, not semantic validation. Consumers must also validate
/// the frame and bound actual received bytes, including JSON whitespace and
/// Start/End frames, by MAX_SNAPSHOT_BYTES for the complete transfer.
pub fn snapshot_chunk_bytes(chunk: &SnapshotChunk) -> Result<usize, WireError> {
    encoded_bytes(chunk)
}

fn resources_for(
    lines: &[Line],
    hyperlinks: &BTreeMap<u64, Hyperlink>,
    images: &BTreeMap<u64, ImageReference>,
) -> Result<(BoundedVec<Hyperlink, MAX_RESOURCES>, BoundedVec<ImageReference, MAX_RESOURCES>), WireError> {
    let mut links = BTreeSet::new();
    let mut pictures = BTreeSet::new();
    for line in lines {
        for cell in line.cells.as_slice() {
            if let Some(id) = cell.hyperlink {
                if !hyperlinks.contains_key(&id.0) { return Err(WireError::InvalidState); }
                links.insert(id.0);
            }
            if let Some(id) = cell.image {
                if !images.contains_key(&id.0) { return Err(WireError::InvalidState); }
                pictures.insert(id.0);
            }
        }
    }
    if links.len() > MAX_RESOURCES || pictures.len() > MAX_RESOURCES {
        return Err(WireError::LimitExceeded);
    }
    let mut chunk_links = BoundedVec::new();
    let mut chunk_images = BoundedVec::new();
    for id in links {
        chunk_links.try_push(hyperlinks.get(&id).ok_or(WireError::InvalidState)?.clone())?;
    }
    for id in pictures {
        chunk_images.try_push(images.get(&id).ok_or(WireError::InvalidState)?.clone())?;
    }
    Ok((chunk_links, chunk_images))
}

fn split_front(groups: &mut VecDeque<Vec<Line>>, mut rows: Vec<Line>) -> Result<(), WireError> {
    if rows.len() < 2 { return Err(WireError::FrameTooLarge); }
    let right = rows.split_off(rows.len() / 2);
    groups.push_front(right);
    groups.push_front(rows);
    Ok(())
}

fn append_frame(
    frames: &mut Vec<Vec<u8>>,
    wire_bytes: &mut usize,
    bytes: Vec<u8>,
) -> Result<(), WireError> {
    *wire_bytes = wire_bytes.checked_add(bytes.len()).ok_or(WireError::LimitExceeded)?;
    if *wire_bytes > MAX_SNAPSHOT_BYTES as usize { return Err(WireError::LimitExceeded); }
    frames.push(bytes);
    Ok(())
}

pub(crate) fn prepare_snapshot(
    server: &ServerSession,
    session: &Id,
    snapshot: FrozenSnapshot,
) -> Result<SnapshotPublication, WireError> {
    let FrozenSnapshot {
        scope, snapshot_id, sequence, dimensions, cursor, modes, title, cwd,
        lines, hyperlinks, images,
    } = snapshot;
    dimensions.validate()?;
    if lines.len() != usize::from(dimensions.rows) { return Err(WireError::SnapshotIncomplete); }
    let mut rows = lines.into_vec();
    rows.sort_by_key(|line| line.row);
    let mut stable_rows = BTreeSet::new();
    for (row, line) in rows.iter().enumerate() {
        if usize::from(line.row) != row || line.cells.len() != usize::from(dimensions.columns)
            || !stable_rows.insert(line.stable_row)
        {
            return Err(WireError::InvalidBounds);
        }
    }

    let rows_per_chunk = MAX_CHUNK_LINES.min(MAX_CHUNK_CELLS / usize::from(dimensions.columns));
    let initial_chunks = rows.len().div_ceil(rows_per_chunk);
    let mut start = SnapshotStart {
        scope: scope.clone(), snapshot_id, sequence, dimensions: dimensions.clone(),
        cursor: cursor.clone(), modes, title, cwd,
        chunks: u16::try_from(initial_chunks).map_err(|_| WireError::LimitExceeded)?,
        total_cells: dimensions.cell_count(), total_bytes: 1,
    };
    // Reject a missing session, stale scope/sequence, or invalid metadata before
    // doing resource projection. This private preflight is never published.
    server.encode_response(ServerMessage::SnapshotStart(start.clone()))?;

    let mut links = BTreeMap::new();
    for link in hyperlinks.into_vec() {
        if link.id.0 == 0 || link.uri.is_empty() || links.insert(link.id.0, link).is_some() {
            return Err(WireError::InvalidState);
        }
    }
    let mut pictures = BTreeMap::new();
    for image in images.into_vec() {
        if image.id.0 == 0 || image.pixel_width == 0 || image.pixel_height == 0
            || image.pixel_width > 65_536 || image.pixel_height > 65_536
            || image.media_type.is_empty() || pictures.insert(image.id.0, image).is_some()
        {
            return Err(WireError::InvalidState);
        }
    }

    let mut groups = VecDeque::new();
    let mut rows = rows.into_iter();
    loop {
        let group: Vec<_> = rows.by_ref().take(rows_per_chunk).collect();
        if group.is_empty() { break; }
        groups.push_back(group);
    }
    let mut chunks = Vec::new();
    let mut staged_bytes = 0usize;
    while let Some(group) = groups.pop_front() {
        if chunks.len() + groups.len() + 1 > usize::from(MAX_CHUNKS) {
            return Err(WireError::LimitExceeded);
        }
        let resources = resources_for(&group, &links, &pictures);
        let (chunk_links, chunk_images) = match resources {
            Ok(value) => value,
            Err(WireError::LimitExceeded) if group.len() > 1 => {
                split_front(&mut groups, group)?;
                continue;
            }
            Err(error) => return Err(error),
        };
        let chunk = SnapshotChunk {
            scope: scope.clone(), snapshot_id, sequence,
            // Reserve the maximum number of JSON digits before final indexing.
            index: MAX_CHUNKS - 1, chunks: MAX_CHUNKS,
            patch: GridPatch {
                dimensions: dimensions.clone(), cursor: cursor.clone(),
                lines: group.try_into()?, hyperlinks: chunk_links, images: chunk_images,
            },
        };
        chunk.patch.validate()?;
        let frame = ServerFrame::new(Some(session.clone()), ServerMessage::SnapshotChunk(chunk))?;
        if let Err(error) = encoded_bytes(&frame) {
            if error != WireError::FrameTooLarge { return Err(error); }
            let ServerMessage::SnapshotChunk(chunk) = frame.message else { return Err(WireError::InvalidState); };
            split_front(&mut groups, chunk.patch.lines.into_vec())?;
            continue;
        }
        let ServerMessage::SnapshotChunk(chunk) = frame.message else { return Err(WireError::InvalidState); };
        staged_bytes = staged_bytes.checked_add(snapshot_chunk_bytes(&chunk)?).ok_or(WireError::LimitExceeded)?;
        // A provisional 3-digit index/count may use four more bytes per chunk
        // than the final values. Keep this bounded slack until exact indexing.
        let staging_cap = MAX_SNAPSHOT_BYTES as usize + usize::from(MAX_CHUNKS) * 4;
        if staged_bytes > staging_cap { return Err(WireError::LimitExceeded); }
        chunks.push(chunk);
    }

    start.chunks = u16::try_from(chunks.len()).map_err(|_| WireError::LimitExceeded)?;
    let mut total_bytes = 0usize;
    for (index, chunk) in chunks.iter_mut().enumerate() {
        chunk.index = u16::try_from(index).map_err(|_| WireError::LimitExceeded)?;
        chunk.chunks = start.chunks;
        total_bytes = total_bytes.checked_add(snapshot_chunk_bytes(chunk)?).ok_or(WireError::LimitExceeded)?;
        if total_bytes > MAX_SNAPSHOT_BYTES as usize { return Err(WireError::LimitExceeded); }
    }
    start.total_bytes = u32::try_from(total_bytes).map_err(|_| WireError::LimitExceeded)?;
    let end = SnapshotEnd {
        scope: scope.clone(), snapshot_id, sequence, chunks: start.chunks,
        total_cells: start.total_cells, total_bytes: start.total_bytes,
    };
    let canonical_bytes = start.total_bytes;
    let mut frames = Vec::with_capacity(chunks.len() + 2);
    let mut wire_bytes = 0;
    append_frame(&mut frames, &mut wire_bytes, server.encode_response(ServerMessage::SnapshotStart(start))?)?;
    for chunk in chunks {
        append_frame(&mut frames, &mut wire_bytes, server.encode_response(ServerMessage::SnapshotChunk(chunk))?)?;
    }
    append_frame(&mut frames, &mut wire_bytes, server.encode_response(ServerMessage::SnapshotEnd(end))?)?;
    Ok(SnapshotPublication { scope, snapshot_id, sequence, canonical_bytes, wire_bytes, frames })
}
