//! Bounded, transactional browser terminal state. A decoded frame is not an
//! applied screen: only a complete snapshot or a contiguous delta commits state.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};

use frankenterm_terminal_wire::{
    AppliedAck, ClientMessage, Cursor, Delta, Dimensions, Hyperlink, Id,
    ImageReference, Line, PaneEventKind, ReadRequest, Resync, ResyncReason,
    Scope, ServerMessage, SnapshotChunk, SnapshotEnd, SnapshotStart, TerminalModes,
    Text, WireError, WireU64, MAX_CHUNKS, MAX_FRAME_BYTES, MAX_RESOURCES,
    MAX_SCREEN_CELLS, MAX_SNAPSHOT_BYTES, snapshot_chunk_bytes,
};
use serde::Serialize;

pub const MAX_MODEL_PANES: usize = 16;
pub const MAX_MODEL_CELLS: usize = MAX_SCREEN_CELLS as usize * 4;
pub const MAX_MODEL_BYTES: usize = MAX_SNAPSHOT_BYTES as usize * 2;
pub const MAX_PENDING_SNAPSHOTS: usize = 4;
pub const MAX_RETIRED_SCOPES: usize = 128;
pub const SNAPSHOT_TIMEOUT_MS: u64 = 15_000;
pub const RECOVERY_INTERVAL_MS: u64 = 1_000;
pub const MAX_RECOVERY_ATTEMPTS: u8 = 3;
const MAX_SCREEN_RESOURCES: usize = MAX_RESOURCES * MAX_CHUNKS as usize;

/// Outcome describes model application, never browser painting or pane effects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApplyOutcome {
    Staged { scope: Scope, snapshot_id: WireU64, received: usize, expected: u16 },
    Applied { scope: Scope, sequence: WireU64 },
    Duplicate { scope: Scope, sequence: WireU64 },
    RecoveryRequired { scope: Scope, reason: ResyncReason },
    Event { message: ServerMessage },
}

/// The last completely installed screen. Rows retain full graphemes, widths,
/// styles, stable coordinates and resource references; no flat UI conversion.
#[derive(Debug)]
pub struct TerminalScreen {
    scope: Scope,
    sequence: WireU64,
    dimensions: Dimensions,
    cursor: Cursor,
    modes: TerminalModes,
    title: Text<4096>,
    cwd: Option<Text<4096>>,
    lines: Vec<Line>,
    hyperlinks: BTreeMap<u64, Hyperlink>,
    images: BTreeMap<u64, ImageReference>,
    charged_bytes: usize,
}

impl TerminalScreen {
    pub fn scope(&self) -> &Scope { &self.scope }
    pub fn sequence(&self) -> WireU64 { self.sequence }
    pub fn dimensions(&self) -> &Dimensions { &self.dimensions }
    pub fn cursor(&self) -> &Cursor { &self.cursor }
    pub fn modes(&self) -> &TerminalModes { &self.modes }
    pub fn lines(&self) -> &[Line] { &self.lines }
    pub fn hyperlinks(&self) -> &BTreeMap<u64, Hyperlink> { &self.hyperlinks }
    pub fn images(&self) -> &BTreeMap<u64, ImageReference> { &self.images }
}

#[derive(Debug)]
struct PendingSnapshot {
    start: SnapshotStart,
    deadline_at_ms: u64,
    raw_bytes: usize,
    reserved_bytes: usize,
    canonical_bytes: usize,
    cells: usize,
    chunks: Vec<SnapshotChunk>,
    indices: BTreeSet<u16>,
    rows: BTreeSet<u16>,
    stable_rows: BTreeSet<i64>,
    hyperlinks: BTreeMap<u64, (usize, usize)>,
    images: BTreeMap<u64, (usize, usize)>,
}

#[derive(Debug)]
struct PaneReplica {
    scope: Scope,
    dimensions: Dimensions,
    /// Minimum advertised/delivered cursor; this is NOT an applied baseline.
    watermark: WireU64,
    previous_watermark: WireU64,
    screen: Option<TerminalScreen>,
    pending: Option<PendingSnapshot>,
    last_snapshot: Option<SnapshotStart>,
    committed_snapshot: Option<SnapshotEnd>,
    ack_pending: Option<WireU64>,
    recovery: Option<ResyncReason>,
    last_recovery_ms: Option<u64>,
    recovery_attempts: u8,
    snapshot_attempts: u8,
}

#[derive(Debug, Default)]
pub struct TerminalModel {
    panes: Vec<PaneReplica>,
    retired: Vec<Scope>,
    retirement_exhausted: bool,
    now_ms: u64,
}

impl TerminalModel {
    pub fn new() -> Self { Self::default() }

    fn index(&self, scope: &Scope) -> Result<usize, WireError> {
        self.panes.iter().position(|pane| pane.scope == *scope)
            .ok_or(WireError::StaleScope)
    }

    pub fn bind_scope(&mut self, scope: Scope, dimensions: Dimensions, minimum: WireU64) -> Result<(), WireError> {
        dimensions.validate()?;
        if self.panes.iter().any(|pane| pane.scope.identity.pane == scope.identity.pane)
            || self.retired.iter().any(|old| old.identity == scope.identity && old.epoch >= scope.epoch)
        {
            return Err(WireError::StaleScope);
        }
        if self.retirement_exhausted || self.panes.len() >= MAX_MODEL_PANES {
            return Err(WireError::LimitExceeded);
        }
        self.panes.push(PaneReplica {
            scope, dimensions, watermark: minimum, previous_watermark: minimum,
            screen: None, pending: None, last_snapshot: None, committed_snapshot: None,
            ack_pending: None, recovery: Some(ResyncReason::BaselineMissing),
            last_recovery_ms: None, recovery_attempts: 0, snapshot_attempts: 0,
        });
        Ok(())
    }

    /// Retirement always discards the scope, even at the tombstone bound. Once
    /// history is full, new binding requires a fresh session instead of refunding
    /// sequence/lease fences through unbind/rebind of an old exact scope.
    pub fn unbind_scope(&mut self, scope: &Scope) -> Result<(), WireError> {
        let index = self.index(scope)?;
        self.panes.remove(index);
        if self.retired.len() < MAX_RETIRED_SCOPES {
            self.retired.push(scope.clone());
        } else {
            self.retirement_exhausted = true;
        }
        Ok(())
    }

    pub fn watermark(&self, scope: &Scope) -> Result<WireU64, WireError> {
        Ok(self.panes[self.index(scope)?].watermark)
    }

    pub fn screen(&self, scope: &Scope) -> Result<Option<&TerminalScreen>, WireError> {
        Ok(self.panes[self.index(scope)?].screen.as_ref())
    }

    pub fn applied_sequence(&self, scope: &Scope) -> Result<Option<WireU64>, WireError> {
        Ok(self.screen(scope)?.map(TerminalScreen::sequence))
    }

    pub fn recovery_reason(&self, scope: &Scope) -> Result<Option<ResyncReason>, WireError> {
        Ok(self.panes[self.index(scope)?].recovery)
    }

    pub fn verify_delivery(&self, scope: &Scope, previous: WireU64, sequence: WireU64) -> Result<(), WireError> {
        let pane = &self.panes[self.index(scope)?];
        if pane.screen.as_ref().is_none_or(|screen| screen.sequence != sequence)
            || pane.previous_watermark != previous || pane.watermark != sequence
        {
            return Err(WireError::StaleSequence);
        }
        Ok(())
    }

    pub fn validate_ack(&self, ack: &AppliedAck) -> Result<(), WireError> {
        let pane = &self.panes[self.index(&ack.scope)?];
        if pane.recovery.is_some()
            || pane.screen.as_ref().is_none_or(|screen| screen.sequence != ack.sequence)
        {
            return Err(WireError::StaleSequence);
        }
        Ok(())
    }

    pub fn pending_ack(&self, scope: &Scope) -> Result<Option<AppliedAck>, WireError> {
        let pane = &self.panes[self.index(scope)?];
        if pane.recovery.is_some() { return Ok(None); }
        Ok(pane.ack_pending.map(|sequence| AppliedAck { scope: scope.clone(), sequence }))
    }

    pub fn acknowledge_encoded(&mut self, ack: &AppliedAck) -> Result<(), WireError> {
        self.validate_ack(ack)?;
        let index = self.index(&ack.scope)?;
        let pane = &mut self.panes[index];
        if pane.ack_pending == Some(ack.sequence) { pane.ack_pending = None; }
        Ok(())
    }

    /// Host time is monotonic by contract. A backwards reading never extends a
    /// deadline: the stored high-water mark remains authoritative on all paths.
    pub fn expire_snapshots(&mut self, now_ms: u64) -> usize {
        self.now_ms = self.now_ms.max(now_ms);
        let mut expired = 0;
        for pane in &mut self.panes {
            if pane.pending.as_ref().is_some_and(|pending| self.now_ms >= pending.deadline_at_ms) {
                invalidate(pane, ResyncReason::SnapshotRejected);
                expired += 1;
            }
        }
        expired
    }

    pub fn abort_snapshot(&mut self, scope: &Scope) -> Result<(), WireError> {
        let index = self.index(scope)?;
        invalidate(&mut self.panes[index], ResyncReason::SnapshotRejected);
        Ok(())
    }

    pub fn require_recovery(&mut self, scope: &Scope, reason: ResyncReason) -> Result<(), WireError> {
        let index = self.index(scope)?;
        invalidate(&mut self.panes[index], reason);
        Ok(())
    }

    /// Geometry is an explicit host-authoritative update. Old-size cells cannot
    /// become a new-size baseline; the stream cursor survives until resnapshot.
    pub fn update_dimensions(&mut self, scope: &Scope, dimensions: Dimensions) -> Result<(), WireError> {
        dimensions.validate()?;
        let index = self.index(scope)?;
        let pane = &mut self.panes[index];
        if pane.dimensions != dimensions {
            pane.dimensions = dimensions;
            pane.screen = None;
            pane.committed_snapshot = None;
            invalidate(pane, ResyncReason::BaselineMissing);
        }
        Ok(())
    }

    fn recovery_ready(&self, scope: &Scope) -> Result<bool, WireError> {
        let pane = &self.panes[self.index(scope)?];
        Ok(pane.recovery.is_some() && pane.pending.is_none()
            && self.now_ms.checked_add(RECOVERY_INTERVAL_MS).is_some()
            && pane.recovery_attempts < MAX_RECOVERY_ATTEMPTS
            && pane.snapshot_attempts < MAX_RECOVERY_ATTEMPTS
            && pane.last_recovery_ms.is_none_or(|at|
                at.checked_add(RECOVERY_INTERVAL_MS).is_some_and(|ready| self.now_ms >= ready)))
    }

    /// A missing baseline uses SnapshotRequest rather than inventing ACK zero.
    pub fn recovery_request(&self, scope: &Scope, request_id: Id) -> Result<Option<ClientMessage>, WireError> {
        if !self.recovery_ready(scope)? { return Ok(None); }
        let pane = &self.panes[self.index(scope)?];
        Ok(Some(match &pane.screen {
            Some(screen) => ClientMessage::Resync(Resync {
                scope: scope.clone(), last_applied: screen.sequence,
                reason: pane.recovery.ok_or(WireError::InvalidState)?,
            }),
            None => ClientMessage::SnapshotRequest(ReadRequest { scope: scope.clone(), request_id }),
        }))
    }

    pub fn validate_recovery_request(&self, message: &ClientMessage) -> Result<(), WireError> {
        let scope = match message {
            ClientMessage::SnapshotRequest(value) => &value.scope,
            ClientMessage::Resync(value) => {
                if self.applied_sequence(&value.scope)? != Some(value.last_applied) {
                    return Err(WireError::StaleSequence);
                }
                &value.scope
            }
            _ => return Ok(()),
        };
        if !self.recovery_ready(scope)? { return Err(WireError::LimitExceeded); }
        Ok(())
    }

    pub fn recovery_encoded(&mut self, scope: &Scope) -> Result<(), WireError> {
        if !self.recovery_ready(scope)? { return Err(WireError::LimitExceeded); }
        let index = self.index(scope)?;
        let pane = &mut self.panes[index];
        pane.recovery_attempts += 1;
        pane.last_recovery_ms = Some(self.now_ms);
        Ok(())
    }

    fn budget(&self, replace_screen: Option<usize>, replace_pending: Option<usize>, bytes: usize, cells: usize) -> Result<(), WireError> {
        let mut total_bytes = bytes;
        let mut total_cells = cells;
        for (index, pane) in self.panes.iter().enumerate() {
            if Some(index) != replace_screen {
                if let Some(screen) = &pane.screen {
                    total_bytes = total_bytes.checked_add(screen.charged_bytes).ok_or(WireError::LimitExceeded)?;
                    total_cells = total_cells.checked_add(screen.dimensions.cell_count() as usize).ok_or(WireError::LimitExceeded)?;
                }
            }
            if Some(index) != replace_pending {
                if let Some(pending) = &pane.pending {
                    total_bytes = total_bytes.checked_add(pending.reserved_bytes).ok_or(WireError::LimitExceeded)?;
                    total_cells = total_cells.checked_add(pending.start.total_cells as usize).ok_or(WireError::LimitExceeded)?;
                }
            }
        }
        if total_bytes > MAX_MODEL_BYTES || total_cells > MAX_MODEL_CELLS { return Err(WireError::LimitExceeded); }
        Ok(())
    }

    /// Only a failure belonging to the current transfer may discard it. An old
    /// delayed ID, foreign scope or unrelated new Start cannot destroy staging.
    fn affected_transfer(&self, message: &ServerMessage) -> Option<(Scope, ResyncReason)> {
        let scope = message.scope()?;
        let pane = &self.panes[self.index(scope).ok()?];
        match message {
            ServerMessage::Delta(value) => {
                if pane.screen.as_ref().is_some_and(|screen| value.sequence <= screen.sequence) { return None; }
                Some((scope.clone(), ResyncReason::Gap))
            }
            ServerMessage::SnapshotStart(value) => {
                if value.sequence < pane.watermark { return None; }
                if let Some(pending) = &pane.pending {
                    if value.snapshot_id != pending.start.snapshot_id || value.sequence != pending.start.sequence { return None; }
                } else if pane.last_snapshot.as_ref().is_some_and(|old| value.snapshot_id <= old.snapshot_id) {
                    return None;
                }
                Some((scope.clone(), ResyncReason::SnapshotRejected))
            }
            ServerMessage::SnapshotChunk(value) => self.affected_snapshot(scope, value.snapshot_id, value.sequence),
            ServerMessage::SnapshotEnd(value) => self.affected_snapshot(scope, value.snapshot_id, value.sequence),
            _ => None,
        }
    }

    fn affected_snapshot(&self, scope: &Scope, snapshot_id: WireU64, sequence: WireU64) -> Option<(Scope, ResyncReason)> {
        let pane = &self.panes[self.index(scope).ok()?];
        let current = if let Some(pending) = &pane.pending {
            snapshot_id == pending.start.snapshot_id && sequence == pending.start.sequence
        } else {
            sequence > pane.watermark || pane.screen.is_none()
        };
        current.then(|| (scope.clone(), ResyncReason::SnapshotRejected))
    }

    /// Caller supplies the actual, already bounded UTF-8 frame length. Every
    /// failure of a current state transfer discards staging, never the old screen.
    pub fn ingest(&mut self, message: ServerMessage, frame_bytes: usize, now_ms: u64) -> Result<ApplyOutcome, WireError> {
        self.expire_snapshots(now_ms);
        if frame_bytes == 0 || frame_bytes > MAX_FRAME_BYTES { return Err(WireError::FrameTooLarge); }
        let affected = self.affected_transfer(&message);
        let result = message.validate().and_then(|()| self.ingest_valid(message, frame_bytes));
        if result.is_err() {
            if let Some((scope, reason)) = affected { self.require_recovery(&scope, reason)?; }
        }
        result
    }

    fn ingest_valid(&mut self, message: ServerMessage, frame_bytes: usize) -> Result<ApplyOutcome, WireError> {
        match message {
            ServerMessage::SnapshotStart(value) => self.start(value, frame_bytes),
            ServerMessage::SnapshotChunk(value) => self.chunk(value, frame_bytes),
            ServerMessage::SnapshotEnd(value) => self.end(value, frame_bytes),
            ServerMessage::Delta(value) => self.delta(value),
            ServerMessage::ResyncRequired(value) => {
                self.require_recovery(&value.scope, value.reason)?;
                Ok(ApplyOutcome::RecoveryRequired { scope: value.scope, reason: value.reason })
            }
            ServerMessage::Unsubscribed(value) => {
                self.unbind_scope(&value.scope)?;
                Ok(ApplyOutcome::Event { message: ServerMessage::Unsubscribed(value) })
            }
            ServerMessage::PaneEvent(value) => {
                match value.event {
                    PaneEventKind::Closed => self.unbind_scope(&value.scope)?,
                    PaneEventKind::Resized => self.update_dimensions(&value.scope,
                        value.dimensions.clone().ok_or(WireError::InvalidState)?)?,
                    PaneEventKind::Gap => self.require_recovery(&value.scope, ResyncReason::Gap)?,
                    _ => { self.index(&value.scope)?; }
                }
                Ok(ApplyOutcome::Event { message: ServerMessage::PaneEvent(value) })
            }
            other => Ok(ApplyOutcome::Event { message: other }),
        }
    }

    fn start(&mut self, start: SnapshotStart, frame_bytes: usize) -> Result<ApplyOutcome, WireError> {
        let index = self.index(&start.scope)?;
        let pane = &self.panes[index];
        if start.sequence < pane.watermark || pane.pending.as_ref().is_some_and(|pending|
            start.snapshot_id < pending.start.snapshot_id || start.sequence < pending.start.sequence)
        {
            return Ok(ApplyOutcome::Duplicate { scope: start.scope, sequence: start.sequence });
        }
        if start.dimensions != pane.dimensions { return Err(WireError::StaleScope); }
        if let Some(pending) = &pane.pending {
            if pending.start != start { return Err(WireError::InvalidState); }
            let raw = transfer_bytes(pending.raw_bytes, frame_bytes)?;
            let received = pending.chunks.len();
            let expected = start.chunks;
            self.panes[index].pending.as_mut().ok_or(WireError::InvalidState)?.raw_bytes = raw;
            return Ok(ApplyOutcome::Staged { scope: start.scope, snapshot_id: start.snapshot_id, received, expected });
        }
        if let Some(previous) = &pane.last_snapshot {
            if start.snapshot_id <= previous.snapshot_id {
                if start == *previous && pane.committed_snapshot.as_ref().is_some_and(|end| end_matches(&start, end)) {
                    return Ok(ApplyOutcome::Duplicate { scope: start.scope, sequence: start.sequence });
                }
                return Err(WireError::StaleSequence);
            }
        }
        if pane.snapshot_attempts >= MAX_RECOVERY_ATTEMPTS
            || self.panes.iter().filter(|pane| pane.pending.is_some()).count() >= MAX_PENDING_SNAPSHOTS
        {
            return Err(WireError::LimitExceeded);
        }
        let deadline_at_ms = self.now_ms.checked_add(SNAPSHOT_TIMEOUT_MS).ok_or(WireError::InvalidBounds)?;
        let reserved_bytes = (start.total_bytes as usize).checked_add(frame_bytes).ok_or(WireError::LimitExceeded)?;
        self.budget(None, None, reserved_bytes, start.total_cells as usize)?;
        let outcome = ApplyOutcome::Staged { scope: start.scope.clone(), snapshot_id: start.snapshot_id, received: 0, expected: start.chunks };
        let pane = &mut self.panes[index];
        pane.snapshot_attempts += 1;
        pane.last_snapshot = Some(start.clone());
        pane.pending = Some(PendingSnapshot {
            start, deadline_at_ms, raw_bytes: frame_bytes, reserved_bytes,
            canonical_bytes: 0, cells: 0, chunks: Vec::new(), indices: BTreeSet::new(),
            rows: BTreeSet::new(), stable_rows: BTreeSet::new(),
            hyperlinks: BTreeMap::new(), images: BTreeMap::new(),
        });
        Ok(outcome)
    }

    fn chunk(&mut self, chunk: SnapshotChunk, frame_bytes: usize) -> Result<ApplyOutcome, WireError> {
        let index = self.index(&chunk.scope)?;
        let pane = &self.panes[index];
        if chunk.sequence < pane.watermark || pane.pending.as_ref().is_some_and(|pending|
            chunk.snapshot_id < pending.start.snapshot_id || chunk.sequence < pending.start.sequence)
            || (pane.pending.is_none() && pane.committed_snapshot.as_ref().is_some_and(|end|
                chunk.snapshot_id == end.snapshot_id && chunk.sequence == end.sequence))
        {
            return Ok(ApplyOutcome::Duplicate { scope: chunk.scope, sequence: chunk.sequence });
        }
        let pending = pane.pending.as_ref().ok_or(WireError::SnapshotIncomplete)?;
        if chunk.snapshot_id != pending.start.snapshot_id || chunk.sequence != pending.start.sequence
            || chunk.chunks != pending.start.chunks || chunk.patch.dimensions != pending.start.dimensions
            || chunk.patch.cursor != pending.start.cursor
        {
            return Err(WireError::InvalidState);
        }
        let raw_bytes = transfer_bytes(pending.raw_bytes, frame_bytes)?;
        if let Some(old) = pending.chunks.iter().find(|old| old.index == chunk.index) {
            if old != &chunk { return Err(WireError::InvalidState); }
            self.panes[index].pending.as_mut().ok_or(WireError::InvalidState)?.raw_bytes = raw_bytes;
            return Ok(ApplyOutcome::Duplicate { scope: chunk.scope, sequence: chunk.sequence });
        }
        let canonical_bytes = pending.canonical_bytes.checked_add(snapshot_chunk_bytes(&chunk)?).ok_or(WireError::LimitExceeded)?;
        let cells = pending.cells.checked_add(chunk.patch.cell_count()).ok_or(WireError::LimitExceeded)?;
        if canonical_bytes > pending.start.total_bytes as usize || cells > pending.start.total_cells as usize { return Err(WireError::InvalidBounds); }
        for line in chunk.patch.lines.as_slice() {
            if pending.rows.contains(&line.row) || pending.stable_rows.contains(&line.stable_row.0) { return Err(WireError::InvalidState); }
        }
        for link in chunk.patch.hyperlinks.as_slice() {
            if let Some(&(chunk_index, resource_index)) = pending.hyperlinks.get(&link.id.0) {
                if &pending.chunks[chunk_index].patch.hyperlinks.as_slice()[resource_index] != link { return Err(WireError::InvalidState); }
            }
        }
        for image in chunk.patch.images.as_slice() {
            if let Some(&(chunk_index, resource_index)) = pending.images.get(&image.id.0) {
                if &pending.chunks[chunk_index].patch.images.as_slice()[resource_index] != image { return Err(WireError::InvalidState); }
            }
        }
        let pending = self.panes[index].pending.as_mut().ok_or(WireError::InvalidState)?;
        let position = pending.chunks.len();
        for line in chunk.patch.lines.as_slice() {
            pending.rows.insert(line.row);
            pending.stable_rows.insert(line.stable_row.0);
        }
        for (resource_index, link) in chunk.patch.hyperlinks.as_slice().iter().enumerate() {
            pending.hyperlinks.entry(link.id.0).or_insert((position, resource_index));
        }
        for (resource_index, image) in chunk.patch.images.as_slice().iter().enumerate() {
            pending.images.entry(image.id.0).or_insert((position, resource_index));
        }
        pending.raw_bytes = raw_bytes;
        pending.canonical_bytes = canonical_bytes;
        pending.cells = cells;
        pending.indices.insert(chunk.index);
        let outcome = ApplyOutcome::Staged {
            scope: chunk.scope.clone(), snapshot_id: chunk.snapshot_id,
            received: position + 1, expected: pending.start.chunks,
        };
        pending.chunks.push(chunk);
        Ok(outcome)
    }

    fn end(&mut self, end: SnapshotEnd, frame_bytes: usize) -> Result<ApplyOutcome, WireError> {
        let index = self.index(&end.scope)?;
        let pane = &self.panes[index];
        if end.sequence < pane.watermark || pane.pending.as_ref().is_some_and(|pending|
            end.snapshot_id < pending.start.snapshot_id || end.sequence < pending.start.sequence)
            || (pane.pending.is_none() && pane.committed_snapshot.as_ref() == Some(&end))
        {
            return Ok(ApplyOutcome::Duplicate { scope: end.scope, sequence: end.sequence });
        }
        let pending = pane.pending.as_ref().ok_or(WireError::SnapshotIncomplete)?;
        transfer_bytes(pending.raw_bytes, frame_bytes)?;
        if !end_matches(&pending.start, &end)
            || pending.chunks.len() != usize::from(end.chunks)
            || pending.indices.len() != usize::from(end.chunks)
            || pending.rows.len() != usize::from(pending.start.dimensions.rows)
            || pending.canonical_bytes != end.total_bytes as usize || pending.cells != end.total_cells as usize
        {
            return Err(WireError::SnapshotIncomplete);
        }
        let mut rows: Vec<&Line> = pending.chunks.iter().flat_map(|chunk| chunk.patch.lines.as_slice()).collect();
        rows.sort_by_key(|line| line.row);
        if rows.iter().enumerate().any(|(row, line)| usize::from(line.row) != row) { return Err(WireError::SnapshotIncomplete); }
        let links: BTreeMap<u64, &Hyperlink> = pending.hyperlinks.iter().map(|(&id, &(chunk, item))|
            (id, &pending.chunks[chunk].patch.hyperlinks.as_slice()[item])).collect();
        let images: BTreeMap<u64, &ImageReference> = pending.images.iter().map(|(&id, &(chunk, item))|
            (id, &pending.chunks[chunk].patch.images.as_slice()[item])).collect();
        let (links, images) = references(&rows, &links, &images)?;
        let used_links: BTreeSet<u64> = links.iter().map(|link| link.id.0).collect();
        let used_images: BTreeSet<u64> = images.iter().map(|image| image.id.0).collect();
        let charged_bytes = measure(&ScreenMeasure {
            scope: &pending.start.scope, sequence: pending.start.sequence,
            dimensions: &pending.start.dimensions, cursor: &pending.start.cursor,
            modes: &pending.start.modes, title: &pending.start.title, cwd: &pending.start.cwd,
            lines: &rows, hyperlinks: &links, images: &images,
        })?;
        self.budget(Some(index), Some(index), charged_bytes, pending.cells)?;
        let pending = self.panes[index].pending.take().ok_or(WireError::InvalidState)?;
        let mut lines = Vec::new();
        let mut hyperlinks = BTreeMap::new();
        let mut images = BTreeMap::new();
        for chunk in pending.chunks {
            lines.extend(chunk.patch.lines.into_vec());
            for link in chunk.patch.hyperlinks.into_vec() {
                if used_links.contains(&link.id.0) { hyperlinks.entry(link.id.0).or_insert(link); }
            }
            for image in chunk.patch.images.into_vec() {
                if used_images.contains(&image.id.0) { images.entry(image.id.0).or_insert(image); }
            }
        }
        lines.sort_by_key(|line| line.row);
        let start = pending.start;
        let screen = TerminalScreen {
            scope: start.scope, sequence: start.sequence, dimensions: start.dimensions,
            cursor: start.cursor, modes: start.modes, title: start.title, cwd: start.cwd,
            lines, hyperlinks, images, charged_bytes,
        };
        let pane = &mut self.panes[index];
        pane.previous_watermark = pane.watermark;
        pane.watermark = screen.sequence;
        pane.ack_pending = Some(screen.sequence);
        pane.screen = Some(screen);
        pane.committed_snapshot = Some(end.clone());
        pane.recovery = None;
        pane.recovery_attempts = 0;
        pane.snapshot_attempts = 0;
        pane.last_recovery_ms = None;
        Ok(ApplyOutcome::Applied { scope: end.scope, sequence: end.sequence })
    }

    fn delta(&mut self, delta: Delta) -> Result<ApplyOutcome, WireError> {
        let index = self.index(&delta.scope)?;
        let pane = &self.panes[index];
        if pane.screen.as_ref().is_some_and(|screen| delta.sequence <= screen.sequence) {
            return Ok(ApplyOutcome::Duplicate { scope: delta.scope, sequence: delta.sequence });
        }
        let screen = pane.screen.as_ref().ok_or(WireError::SnapshotIncomplete)?;
        if pane.recovery.is_some() || pane.pending.is_some() || delta.base_sequence != screen.sequence { return Err(WireError::StaleSequence); }
        if delta.patch.dimensions != pane.dimensions
            || delta.modes.alternate_screen != screen.modes.alternate_screen
        {
            return Err(WireError::StaleScope);
        }
        let replacements: BTreeMap<u16, &Line> = delta.patch.lines.as_slice().iter().map(|line| (line.row, line)).collect();
        let rows: Vec<&Line> = screen.lines.iter().map(|line| replacements.get(&line.row).copied().unwrap_or(line)).collect();
        let mut stable_rows = BTreeSet::new();
        if rows.iter().any(|line| !stable_rows.insert(line.stable_row.0)) { return Err(WireError::InvalidState); }
        let mut links: BTreeMap<u64, &Hyperlink> = screen.hyperlinks.iter().map(|(&id, link)| (id, link)).collect();
        let mut images: BTreeMap<u64, &ImageReference> = screen.images.iter().map(|(&id, image)| (id, image)).collect();
        for link in delta.patch.hyperlinks.as_slice() {
            if links.get(&link.id.0).is_some_and(|old| *old != link) { return Err(WireError::InvalidState); }
            links.insert(link.id.0, link);
        }
        for image in delta.patch.images.as_slice() {
            if images.get(&image.id.0).is_some_and(|old| *old != image) { return Err(WireError::InvalidState); }
            images.insert(image.id.0, image);
        }
        let (links, images) = references(&rows, &links, &images)?;
        let used_links: BTreeSet<u64> = links.iter().map(|link| link.id.0).collect();
        let used_images: BTreeSet<u64> = images.iter().map(|image| image.id.0).collect();
        let charged_bytes = measure(&ScreenMeasure {
            scope: &screen.scope, sequence: delta.sequence, dimensions: &screen.dimensions,
            cursor: &delta.patch.cursor, modes: &delta.modes, title: &screen.title, cwd: &screen.cwd,
            lines: &rows, hyperlinks: &links, images: &images,
        })?;
        self.budget(Some(index), None, charged_bytes, screen.dimensions.cell_count() as usize)?;
        let pane = &mut self.panes[index];
        let screen = pane.screen.as_mut().ok_or(WireError::InvalidState)?;
        for line in delta.patch.lines.into_vec() {
            let row = usize::from(line.row);
            screen.lines[row] = line;
        }
        for link in delta.patch.hyperlinks.into_vec() { screen.hyperlinks.entry(link.id.0).or_insert(link); }
        for image in delta.patch.images.into_vec() { screen.images.entry(image.id.0).or_insert(image); }
        screen.hyperlinks.retain(|id, _| used_links.contains(id));
        screen.images.retain(|id, _| used_images.contains(id));
        screen.sequence = delta.sequence;
        screen.cursor = delta.patch.cursor;
        screen.modes = delta.modes;
        screen.charged_bytes = charged_bytes;
        pane.previous_watermark = pane.watermark;
        pane.watermark = delta.sequence;
        pane.ack_pending = Some(delta.sequence);
        Ok(ApplyOutcome::Applied { scope: delta.scope, sequence: delta.sequence })
    }

    /// Metadata is small; rows and resources are exported by bounded row pages.
    pub fn metadata_json(&self, scope: &Scope) -> Result<String, WireError> {
        let pane = &self.panes[self.index(scope)?];
        #[derive(Serialize)]
        struct Metadata<'a> {
            scope: &'a Scope, dimensions: &'a Dimensions, applied_sequence: Option<WireU64>,
            recovery: Option<ResyncReason>, pending_chunks: usize, expected_chunks: Option<u16>,
            recovery_attempts: u8, snapshot_attempts: u8, cursor: Option<&'a Cursor>,
            modes: Option<&'a TerminalModes>, title: Option<&'a Text<4096>>, cwd: Option<&'a Text<4096>>,
        }
        bounded_json(&Metadata {
            scope, dimensions: &pane.dimensions, applied_sequence: pane.screen.as_ref().map(|s| s.sequence),
            recovery: pane.recovery, pending_chunks: pane.pending.as_ref().map_or(0, |p| p.chunks.len()),
            expected_chunks: pane.pending.as_ref().map(|p| p.start.chunks), recovery_attempts: pane.recovery_attempts,
            snapshot_attempts: pane.snapshot_attempts, cursor: pane.screen.as_ref().map(|s| &s.cursor),
            modes: pane.screen.as_ref().map(|s| &s.modes), title: pane.screen.as_ref().map(|s| &s.title),
            cwd: pane.screen.as_ref().and_then(|s| s.cwd.as_ref()),
        })
    }

    pub fn row_json(&self, scope: &Scope, row: u16) -> Result<String, WireError> {
        let screen = self.screen(scope)?.ok_or(WireError::SnapshotIncomplete)?;
        let line = screen.lines.get(usize::from(row)).ok_or(WireError::InvalidBounds)?;
        let links: BTreeMap<_, _> = screen.hyperlinks.iter().map(|(&id, resource)| (id, resource)).collect();
        let images: BTreeMap<_, _> = screen.images.iter().map(|(&id, resource)| (id, resource)).collect();
        let (hyperlinks, images) = references(&[line], &links, &images)?;
        #[derive(Serialize)]
        struct RowPage<'a> {
            scope: &'a Scope, sequence: WireU64, line: &'a Line,
            hyperlinks: Vec<&'a Hyperlink>, images: Vec<&'a ImageReference>,
        }
        bounded_json(&RowPage { scope, sequence: screen.sequence, line, hyperlinks, images })
    }

    /// Accessibility/export page; never substitutes a truncated glyph for the
    /// complete grapheme. At most 64 rows and one frame of UTF-8 are returned.
    pub fn text_page(&self, scope: &Scope, first_row: u16, row_count: u16) -> Result<String, WireError> {
        let screen = self.screen(scope)?.ok_or(WireError::SnapshotIncomplete)?;
        if row_count == 0 || row_count > 64 { return Err(WireError::InvalidBounds); }
        let end = first_row.checked_add(row_count).ok_or(WireError::InvalidBounds)?;
        if end > screen.dimensions.rows { return Err(WireError::InvalidBounds); }
        let mut output = String::new();
        for row in first_row..end {
            if row != first_row { append_text(&mut output, "\n")?; }
            for cell in screen.lines[usize::from(row)].cells.as_slice() {
                if cell.width != 0 { append_text(&mut output, cell.text.as_str())?; }
            }
        }
        Ok(output)
    }
}

fn invalidate(pane: &mut PaneReplica, reason: ResyncReason) {
    pane.pending = None;
    pane.ack_pending = None;
    pane.recovery = Some(reason);
}

fn end_matches(start: &SnapshotStart, end: &SnapshotEnd) -> bool {
    start.scope == end.scope && start.snapshot_id == end.snapshot_id && start.sequence == end.sequence
        && start.chunks == end.chunks && start.total_cells == end.total_cells && start.total_bytes == end.total_bytes
}

fn transfer_bytes(previous: usize, next: usize) -> Result<usize, WireError> {
    previous.checked_add(next).filter(|total| *total <= MAX_SNAPSHOT_BYTES as usize).ok_or(WireError::LimitExceeded)
}

fn references<'a>(
    rows: &[&Line], hyperlinks: &BTreeMap<u64, &'a Hyperlink>, images: &BTreeMap<u64, &'a ImageReference>,
) -> Result<(Vec<&'a Hyperlink>, Vec<&'a ImageReference>), WireError> {
    let mut used_links = BTreeSet::new();
    let mut used_images = BTreeSet::new();
    for line in rows {
        for cell in line.cells.as_slice() {
            if let Some(id) = cell.hyperlink { used_links.insert(id.0); }
            if let Some(id) = cell.image { used_images.insert(id.0); }
        }
    }
    if used_links.len() > MAX_SCREEN_RESOURCES || used_images.len() > MAX_SCREEN_RESOURCES { return Err(WireError::LimitExceeded); }
    let hyperlinks = used_links.into_iter().map(|id| hyperlinks.get(&id).copied().ok_or(WireError::InvalidState)).collect::<Result<Vec<_>, _>>()?;
    let images = used_images.into_iter().map(|id| images.get(&id).copied().ok_or(WireError::InvalidState)).collect::<Result<Vec<_>, _>>()?;
    Ok((hyperlinks, images))
}

#[derive(Serialize)]
struct ScreenMeasure<'a> {
    scope: &'a Scope, sequence: WireU64, dimensions: &'a Dimensions, cursor: &'a Cursor,
    modes: &'a TerminalModes, title: &'a Text<4096>, cwd: &'a Option<Text<4096>>,
    lines: &'a [&'a Line], hyperlinks: &'a [&'a Hyperlink], images: &'a [&'a ImageReference],
}

struct CountWriter { bytes: usize, limit: usize }
impl Write for CountWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let total = self.bytes.checked_add(bytes.len()).filter(|total| *total <= self.limit)
            .ok_or_else(|| io::Error::other("terminal_model_limit"))?;
        self.bytes = total;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

fn measure(value: &impl Serialize) -> Result<usize, WireError> {
    let mut writer = CountWriter { bytes: 0, limit: MAX_SNAPSHOT_BYTES as usize };
    serde_json::to_writer(&mut writer, value).map_err(|_| WireError::LimitExceeded)?;
    Ok(writer.bytes)
}

struct JsonWriter(Vec<u8>);
impl Write for JsonWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.len().checked_add(bytes.len()).filter(|total| *total <= MAX_FRAME_BYTES)
            .ok_or_else(|| io::Error::other("terminal_model_page_limit"))?;
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

pub(crate) fn bounded_json(value: &impl Serialize) -> Result<String, WireError> {
    let mut writer = JsonWriter(Vec::new());
    serde_json::to_writer(&mut writer, value).map_err(|_| WireError::FrameTooLarge)?;
    String::from_utf8(writer.0).map_err(|_| WireError::Malformed)
}

fn append_text(output: &mut String, text: &str) -> Result<(), WireError> {
    output.len().checked_add(text.len()).filter(|total| *total <= MAX_FRAME_BYTES).ok_or(WireError::FrameTooLarge)?;
    output.push_str(text);
    Ok(())
}
