#![allow(clippy::range_plus_one)]
use super::*;
use crate::config::{BidiMode, ScrollbackSnapshotGeneration, ScrollbackSpillError};
#[cfg(feature = "use_serde")]
use crate::config::{
    ScrollbackActivationError, ScrollbackPrefix, ScrollbackSnapshotFidelity,
    ScrollbackSnapshotLimits,
};
#[cfg(feature = "use_serde")]
use frankenterm_surface::line::LineWrapGeometry;
use frankenterm_surface::line::{
    LineWrapLayout, LineWrapScorecard as MonospaceLineWrapScorecard, LineWrapWidthPrefixScratch,
    MonospaceKpCostModel, MonospaceWrapMode,
};
use frankenterm_surface::SequenceNo;
use log::{debug, warn};
#[cfg(feature = "use_serde")]
use std::collections::BTreeMap;
use std::collections::{hash_map::DefaultHasher, HashMap, VecDeque};
use std::convert::TryFrom;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, LazyLock, Weak};
use std::time::{Duration, Instant};
use termwiz::input::KeyboardEncoding;

#[cfg(test)]
std::thread_local! {
    static REFLOW_SOURCE_SIGNATURE_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static REFLOW_FULL_LAYOUT_SIGNATURE_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static REFLOW_LAYOUT_LINE_HASHES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static REFLOW_CURSOR_PREFIX_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static REFLOW_ROW_PREFIX_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static REFLOW_CACHED_RESERVED_CAPACITY: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static REFLOW_RETAINED_SOURCE_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static REFLOW_RETAINED_REPLAN_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static COLD_GEOMETRY_COUNT_REUSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn logical_len_exceeds_limit(current: usize, additional: usize, limit: usize) -> bool {
    match current.checked_add(additional) {
        Some(total) => total > limit,
        None => true,
    }
}

fn reuse_unlinked_scan_state_for_reflow() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var_os("FT_DISABLE_REFLOW_SCAN_STATE_REUSE").as_deref()
            != Some(std::ffi::OsStr::new("1"))
    })
}

struct ReflowComputePool {
    pool: rayon::ThreadPool,
    // One admitted reflow at a time bounds the queued batch count. Contending
    // callers retain the synchronous scalar path instead of waiting here.
    admission: std::sync::Mutex<()>,
}

fn reflow_compute_pool() -> Option<&'static ReflowComputePool> {
    static POOL: LazyLock<Option<ReflowComputePool>> = LazyLock::new(|| {
        let available = std::thread::available_parallelism()
            .map(|n| n.get().saturating_sub(1))
            .unwrap_or(0);
        // Use the bounded compute capacity by default, leaving one reported
        // CPU for interactive work. The process-start override supports
        // same-binary scalar and worker-count comparisons.
        let requested = std::env::var("FT_REFLOW_COMPUTE_WORKERS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(available);
        let workers = requested.min(available).min(8);
        if workers < 2 {
            return None;
        }
        match rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .thread_name(|idx| format!("ft-reflow-{idx}"))
            .build()
        {
            Ok(pool) => Some(ReflowComputePool {
                pool,
                admission: std::sync::Mutex::new(()),
            }),
            Err(err) => {
                warn!("reflow compute pool unavailable; using scalar reflow: {err}");
                None
            }
        }
    });
    POOL.as_ref()
}

/// Allocation identity for one screen coordinate generation. A cloned Screen
/// is a distinct model, never another owner of the original authority.
#[derive(Debug, Default)]
struct ScreenCoordinateIdentity(Arc<()>);

impl Clone for ScreenCoordinateIdentity {
    fn clone(&self) -> Self {
        Self::default()
    }
}

/// Screen-local coordinate authority only. This does not certify a retained
/// store interval, pane registration, active-screen selection, or permission
/// to publish a cold-read result. Those require independent validation.
/// Configuration invalidation covers installation through Screen; interior
/// mutation of a configuration capability requires its own revision fence.
#[derive(Debug, Clone)]
pub struct ScreenCoordinateWitness {
    identity: Arc<()>,
    rows: usize,
    cols: usize,
    dpi: u32,
}

/// A cell coordinate, or the boundary immediately before column zero.
/// These are terminal coordinates, never pixels or retained selected text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionAnchorCoordinate {
    pub column: Option<usize>,
    pub row: StableRowIndex,
}

/// Opaque ownership of one finite selection in a live Screen. A preparation
/// clone cannot resolve or update this token, even if its contents are equal.
#[derive(Debug, Clone)]
pub struct ScreenSelectionAnchor(Arc<()>);

impl PartialEq for ScreenSelectionAnchor {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for ScreenSelectionAnchor {}

#[derive(Debug)]
struct SelectionAnchorEntry {
    owner: Weak<()>,
    source_sequence: SequenceNo,
    witness: ScreenCoordinateWitness,
    points: [Option<SelectionAnchorCoordinate>; 3],
}

/// Access is serialized by the existing terminal lock. Entries are weak and
/// bounded, so abandoned GUI selections neither retain text nor grow a queue.
#[derive(Debug, Default)]
struct SelectionAnchorRegistry(Vec<SelectionAnchorEntry>);

impl Clone for SelectionAnchorRegistry {
    fn clone(&self) -> Self {
        Self::default()
    }
}

/// Owned read work, captured without storage IO. Only the sink and COW row
/// snapshots cross the worker boundary; no Screen or pane lock escapes.
#[cfg(feature = "use_serde")]
pub struct LineReadCaptureBudget {
    bytes_left: usize,
    work_left: usize,
}

#[cfg(feature = "use_serde")]
impl Default for LineReadCaptureBudget {
    fn default() -> Self {
        Self {
            bytes_left: ScreenLineRead::MAX_PAYLOAD_BYTES,
            work_left: 65_536,
        }
    }
}

#[cfg(feature = "use_serde")]
#[derive(Clone)]
pub struct LineReadFailureWitness {
    screen: ScreenCoordinateWitness,
    layout_seqno: SequenceNo,
    cold: Option<(
        Arc<dyn crate::config::ScrollbackSpillSink>,
        crate::config::ScrollbackInterval,
    )>,
    index_budget_exhausted: Arc<std::sync::atomic::AtomicBool>,
    attempted_index: bool,
    fragments: Option<Arc<ColdRowFragments>>,
}

#[cfg(feature = "use_serde")]
impl LineReadFailureWitness {
    pub fn retry_without_index(&self) -> bool {
        self.attempted_index
            && self
                .index_budget_exhausted
                .load(std::sync::atomic::Ordering::Acquire)
    }
    pub fn matches(&self, screen: &Screen) -> bool {
        if self.retry_without_index() {
            return false;
        }
        if !screen.matches_coordinate_witness(&self.screen)
            || screen.cold_visual_seqno != self.layout_seqno
            || !screen.same_cold_fragments(&self.fragments)
        {
            return false;
        }
        let Some((before_sink, before)) = &self.cold else {
            return false;
        };
        let Some(now_sink) = screen.config.scrollback_spill_sink() else {
            return false;
        };
        if !Arc::ptr_eq(before_sink, &now_sink) {
            return false;
        }
        match (before.rows(), now_sink.try_capture_scrollback_interval()) {
            (Some(rows), crate::config::ScrollbackIntervalCapture::Ready(now)) => {
                now.rows().as_ref() == Some(&rows) && now.retains(before, rows)
            }
            _ => false,
        }
    }
}

#[cfg(feature = "use_serde")]
#[derive(Debug)]
pub struct ColdReadGeometryUnavailable;

#[cfg(feature = "use_serde")]
impl std::fmt::Display for ColdReadGeometryUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cold history requires a coherent layout index at this width")
    }
}

#[cfg(feature = "use_serde")]
impl std::error::Error for ColdReadGeometryUnavailable {}

/// A transient metadata lock conflict, rather than missing or invalid source
/// authority. Only an off-thread owner may wait and retry this refusal.
#[cfg(feature = "use_serde")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColdReadMetadataBusy;

#[cfg(feature = "use_serde")]
impl std::fmt::Display for ColdReadMetadataBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cold history metadata is busy")
    }
}

#[cfg(feature = "use_serde")]
impl std::error::Error for ColdReadMetadataBusy {}

/// The captured source interval was invalidated before historical IO began.
/// This does not classify an IO, authentication, or decoding failure as transient.
#[cfg(feature = "use_serde")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColdReadSourceChanged;

#[cfg(feature = "use_serde")]
impl std::fmt::Display for ColdReadSourceChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cold history source changed before reading")
    }
}

#[cfg(feature = "use_serde")]
impl std::error::Error for ColdReadSourceChanged {}

#[cfg(feature = "use_serde")]
fn validate_cold_source_before_read(
    sink: &dyn crate::config::ScrollbackSpillSink,
    captured: &crate::config::ScrollbackInterval,
    rows: Range<StableRowIndex>,
) -> anyhow::Result<()> {
    use crate::config::ScrollbackIntervalCapture;
    let captured_rows = captured
        .rows()
        .ok_or_else(|| anyhow::anyhow!("cold read captured source unavailable"))?;
    anyhow::ensure!(
        rows.start < rows.end && captured_rows.start <= rows.start && rows.end <= captured_rows.end,
        "cold read range exceeds captured source"
    );
    match sink.try_capture_scrollback_interval() {
        ScrollbackIntervalCapture::Ready(current) => {
            anyhow::ensure!(current.retains(captured, rows), ColdReadSourceChanged);
            Ok(())
        }
        ScrollbackIntervalCapture::Busy => Err(ColdReadMetadataBusy.into()),
        ScrollbackIntervalCapture::Unavailable => anyhow::bail!("cold read source unavailable"),
    }
}

#[cfg(feature = "use_serde")]
pub struct ScreenLineRead {
    witness: ScreenCoordinateWitness,
    layout_seqno: SequenceNo,
    first: StableRowIndex,
    end: StableRowIndex,
    resident_first: StableRowIndex,
    resident: Vec<Line>,
    cold: Option<(
        Arc<dyn crate::config::ScrollbackSpillSink>,
        crate::config::ScrollbackInterval,
    )>,
    hydrated: Vec<Line>,
    payload_bytes: usize,
    complete: bool,
    cold_context: Option<Range<StableRowIndex>>,
    rendered: Option<Vec<Line>>,
    hot_top: StableRowIndex,
    known_resident_history_start: Option<StableRowIndex>,
    wrap_policy: ResizeWrapPolicy,
    layout: Option<Arc<ColdVisualLayout>>,
    logical_view: Option<(StableRowIndex, Vec<Line>)>,
    index_budget_exhausted: Arc<std::sync::atomic::AtomicBool>,
    attempted_index: bool,
    fragments: Option<Arc<ColdRowFragments>>,
    geometry: Option<ColdGeometrySnapshot>,
}

/// Worker-owned coordinate preparation, never a receipt for readable text.
/// Admitted geometry can establish the complete cold mapping without loading
/// the arbitrary row used to capture its source. Displaced metadata remains
/// here until the worker releases the terminal and registration guards.
#[cfg(feature = "use_serde")]
pub struct PreparedColdLayout {
    source: ScreenLineRead,
    retired_layout: Option<Arc<ColdVisualLayout>>,
    installed: bool,
}

/// Width information belongs to immutable admitted source rows, independently
/// of the current viewport coordinates. Payload remains in the spill sink.
#[cfg(feature = "use_serde")]
#[derive(Debug, Clone)]
struct ColdGeometryRow {
    geometry: Arc<LineWrapGeometry>,
    wrapped: bool,
}

#[cfg(feature = "use_serde")]
#[derive(Debug, Clone)]
struct ColdGeometryIndex {
    sink: Arc<dyn crate::config::ScrollbackSpillSink>,
    interval: crate::config::ScrollbackInterval,
    source: Range<StableRowIndex>,
    rows: VecDeque<ColdGeometryRow>,
    geometry_bytes: usize,
}

#[cfg(feature = "use_serde")]
#[derive(Debug)]
struct ColdGeometrySnapshot {
    source: Range<StableRowIndex>,
    rows: Vec<ColdGeometryRow>,
}

#[cfg(feature = "use_serde")]
impl ColdGeometryIndex {
    fn append(&mut self, row: StableRowIndex, line: &Line) -> Option<()> {
        let retained = self.interval.rows()?;
        let end = row.checked_add(1)?;
        if row != self.source.end || retained.end != end || retained.start > row {
            return None;
        }
        while self.source.start < retained.start && !self.rows.is_empty() {
            let removed = self.rows.pop_front()?;
            self.geometry_bytes = self.geometry_bytes.checked_sub(
                removed.geometry.retained_bytes() + 2 * std::mem::size_of::<usize>(),
            )?;
            self.source.start += 1;
        }
        if self.rows.len() >= ScreenLineRead::MAX_INDEX_SOURCE_ROWS {
            return None;
        }
        let capacity = if self.rows.len() == self.rows.capacity() {
            self.rows
                .capacity()
                .max(32)
                .checked_mul(2)?
                .min(ScreenLineRead::MAX_INDEX_SOURCE_ROWS)
        } else {
            self.rows.capacity()
        };
        let peak_capacity = if capacity > self.rows.capacity() {
            capacity.checked_add(self.rows.capacity())?
        } else {
            capacity
        };
        let overhead = peak_capacity
            .checked_mul(std::mem::size_of::<ColdGeometryRow>())?
            .checked_add(std::mem::size_of::<Self>())?
            .checked_add(2 * std::mem::size_of::<usize>())?;
        let available = ScreenLineRead::MAX_INDEX_METADATA_BYTES
            .checked_sub(overhead)?
            .checked_sub(self.geometry_bytes)?;
        let geometry = LineWrapGeometry::capture(line, available)?;
        // Pending rows can still use clustered storage; durable rows have
        // passed through cell serialization. Admission must certify the same
        // widths in either representation, even for a one-row logical group.
        if !geometry.supports_join() {
            return None;
        }
        let bytes = geometry
            .retained_bytes()
            .checked_add(2 * std::mem::size_of::<usize>())?;
        if bytes > available {
            return None;
        }
        if capacity > self.rows.capacity() {
            self.rows
                .try_reserve_exact(capacity - self.rows.len())
                .ok()?;
            if self.rows.capacity() > capacity {
                return None;
            }
        }
        self.geometry_bytes = self.geometry_bytes.checked_add(bytes)?;
        self.rows.push_back(ColdGeometryRow {
            geometry: Arc::new(geometry),
            wrapped: line.last_cell_was_wrapped(),
        });
        self.source.end = end;
        Some(())
    }
}

/// Visual coordinates never replace authenticated backing-store row keys.
/// Canonical entries describe complete reflowed logical groups. StoredPhysical
/// entries preserve admitted physical rows, including a clipped first group or
/// a last group that continues across the resident frontier.
/// Only compact metadata is retained by Screen; decoded cells remain worker
/// owned and are retired by the read consumer.
#[cfg(feature = "use_serde")]
#[derive(Debug)]
struct ColdVisualLayout {
    kind: ColdVisualLayoutKind,
    source: Range<StableRowIndex>,
    visual: Range<StableRowIndex>,
    resident_frontier: StableRowIndex,
    groups: Vec<(Range<StableRowIndex>, Range<StableRowIndex>)>,
    witness: ScreenCoordinateWitness,
    interval: crate::config::ScrollbackInterval,
}

#[cfg(feature = "use_serde")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColdVisualLayoutKind {
    Canonical,
    StoredPhysical { open_tail: bool },
}

#[cfg(feature = "use_serde")]
impl ColdVisualLayout {
    fn stored_physical(&self) -> bool {
        matches!(self.kind, ColdVisualLayoutKind::StoredPhysical { .. })
    }

    fn extends(&self, before: &Self) -> bool {
        self.source.start == before.source.start
            && self.visual.start == before.visual.start
            && self.source.end >= before.source.end
            && self.stored_physical() == before.stored_physical()
            && (self.stored_physical() || self.groups.starts_with(&before.groups))
    }
}

/// Append metadata captured before Screen relinquishes the exact physical row.
/// No decoded cells or per-spill copy of the complete index is retained here.
#[cfg(feature = "use_serde")]
#[derive(Debug, Clone)]
struct StoredPhysicalLayout {
    sink: Arc<dyn crate::config::ScrollbackSpillSink>,
    interval: crate::config::ScrollbackInterval,
    witness: ScreenCoordinateWitness,
    source: Range<StableRowIndex>,
    groups: VecDeque<Range<StableRowIndex>>,
    open_tail: bool,
}

#[cfg(feature = "use_serde")]
impl StoredPhysicalLayout {
    // Bound both the builder allocation and its frozen range-pair snapshot.
    const MAX_GROUPS: usize = (ScreenLineRead::MAX_INDEX_METADATA_BYTES
        - std::mem::size_of::<Self>()
        - std::mem::size_of::<ColdVisualLayout>()
        - 2 * std::mem::size_of::<usize>())
        / (3 * std::mem::size_of::<Range<StableRowIndex>>());

    fn append(&mut self, row: StableRowIndex, line: &Line) -> bool {
        let Some(end) = row.checked_add(1) else {
            return false;
        };
        let Some(retained) = self.interval.rows() else {
            return false;
        };
        while self
            .groups
            .front()
            .is_some_and(|group| group.end <= retained.start)
        {
            self.groups.pop_front();
        }
        if let Some(first) = self.groups.front_mut() {
            first.start = first.start.max(retained.start);
        }
        if self.open_tail && !self.groups.is_empty() {
            self.groups.back_mut().unwrap().end = end;
        } else {
            if self.groups.len() >= Self::MAX_GROUPS {
                return false;
            }
            if self.groups.len() == self.groups.capacity() {
                let capacity = self
                    .groups
                    .capacity()
                    .max(32)
                    .saturating_mul(2)
                    .min(Self::MAX_GROUPS);
                if self
                    .groups
                    .try_reserve_exact(capacity - self.groups.len())
                    .is_err()
                    || self.groups.capacity() > Self::MAX_GROUPS
                {
                    return false;
                }
            }
            self.groups.push_back(row..end);
        }
        self.source.start = self.source.start.max(retained.start);
        self.source.end = end;
        self.open_tail = line.last_cell_was_wrapped();
        true
    }
}

#[cfg(feature = "use_serde")]
#[derive(Debug, Clone)]
pub(crate) struct ColdRowFragments {
    pub(crate) sink: Arc<dyn crate::config::ScrollbackSpillSink>,
    pub(crate) interval: crate::config::ScrollbackInterval,
    pub(crate) rows: Arc<BTreeMap<StableRowIndex, Line>>,
    pub(crate) aligned_frontier: StableRowIndex,
    pub(crate) aligned_source_start: StableRowIndex,
    pub(crate) cols: usize,
    pub(crate) dpi: u32,
    pub(crate) policy: ResizeWrapPolicy,
}

#[cfg(feature = "use_serde")]
impl ColdRowFragments {
    fn alignment_retained(&self, interval: &crate::config::ScrollbackInterval) -> bool {
        interval.rows().is_some_and(|rows| {
            rows.start <= self.aligned_source_start && rows.end >= self.aligned_frontier
        })
    }
    fn aligned_at(
        &self,
        frontier: StableRowIndex,
        cols: usize,
        dpi: u32,
        policy: ResizeWrapPolicy,
    ) -> bool {
        self.aligned_frontier == frontier
            && self.cols == cols
            && self.dpi == dpi
            && self.policy == policy
    }
}

/// Off-lock preparation for a complete logical group crossing the cold/hot
/// frontier. Stored keys and the resident row count never change. Displaced
/// allocations remain in this owner until its worker retires it.
#[cfg(feature = "use_serde")]
#[derive(Debug)]
pub struct ColdSeamReflow {
    witness: ScreenCoordinateWitness,
    sink: Arc<dyn crate::config::ScrollbackSpillSink>,
    interval: crate::config::ScrollbackInterval,
    frontier: StableRowIndex,
    resident: Vec<Line>,
    previous: Option<Arc<ColdRowFragments>>,
    policy: ResizeWrapPolicy,
    replacement: Option<(Arc<ColdRowFragments>, Vec<Line>)>,
    source: Option<Range<StableRowIndex>>,
    retired_layout: Option<Arc<ColdVisualLayout>>,
}

#[cfg(feature = "use_serde")]
#[derive(Debug)]
pub struct ColdReadPayloadLimit;

#[cfg(feature = "use_serde")]
impl std::fmt::Display for ColdReadPayloadLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cold read payload limit")
    }
}

#[cfg(feature = "use_serde")]
impl std::error::Error for ColdReadPayloadLimit {}

#[cfg(feature = "use_serde")]
struct ColdReadCharge {
    used: usize,
    limit: usize,
    index_failure: Option<Arc<std::sync::atomic::AtomicBool>>,
}

#[cfg(feature = "use_serde")]
impl ColdReadCharge {
    fn line_with_fragment(
        &mut self,
        fragments: Option<&ColdRowFragments>,
        row: StableRowIndex,
        original: Line,
    ) -> anyhow::Result<Line> {
        self.serialize_line(&original)?;
        match fragments.and_then(|fragments| fragments.rows.get(&row)) {
            Some(replacement) => {
                self.serialize_line(replacement)?;
                Ok(replacement.clone())
            }
            None => Ok(original),
        }
    }

    fn serialize_line(&mut self, line: &Line) -> anyhow::Result<()> {
        serde_json::to_writer(self, line).map_err(|error| {
            // This writer performs no IO: its only IO error is its byte cap.
            // Preserve a typed context explicitly; serde_json and io::Error
            // may forward source() past the custom writer error itself.
            if error.is_io() {
                anyhow::Error::new(error).context(ColdReadPayloadLimit)
            } else {
                error.into()
            }
        })
    }
}

#[cfg(feature = "use_serde")]
impl std::io::Write for ColdReadCharge {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.used = self
            .used
            .checked_add(bytes.len())
            .filter(|n| *n <= self.limit)
            .ok_or_else(|| {
                if let Some(failed) = &self.index_failure {
                    failed.store(true, std::sync::atomic::Ordering::Release);
                }
                std::io::Error::other(ColdReadPayloadLimit)
            })?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(feature = "use_serde")]
fn cold_row_with_fragment(
    fragments: Option<&ColdRowFragments>,
    row: StableRowIndex,
    original: Line,
) -> Line {
    fragments
        .and_then(|fragments| fragments.rows.get(&row))
        .cloned()
        .unwrap_or(original)
}

#[cfg(feature = "use_serde")]
impl ColdSeamReflow {
    pub fn is_ready(&self) -> bool {
        self.replacement.is_some()
    }

    pub fn hydrate(mut self, cancelled: impl Fn() -> bool) -> anyhow::Result<Self> {
        anyhow::ensure!(self.replacement.is_none(), "cold seam already prepared");
        let profile_start =
            log::log_enabled!(target: "frankenterm_term::screen::reflow_profile", log::Level::Debug)
                .then(Instant::now);
        let retained = self
            .interval
            .rows()
            .ok_or_else(|| anyhow::anyhow!("cold seam unavailable"))?;
        let mut charge = ColdReadCharge {
            used: 0,
            limit: ScreenLineRead::MAX_PAYLOAD_BYTES,
            index_failure: None,
        };
        // A completed seam already owns every replacement row and its exact
        // logical-group boundary. If that complete source is still retained at
        // the same frontier, rereading the immutable spill rows only to replace
        // them again performs unnecessary storage/decode work on every resize.
        // A changed frontier, lineage, or incomplete fragment set uses the
        // ordinary storage path. Publication still revalidates the live source
        // interval, coordinate witness, resident rows and fragment identity.
        let fragment_start = self.previous.as_ref().and_then(|previous| {
            let start = previous.aligned_source_start;
            let count = self.frontier.checked_sub(start)?;
            let count = usize::try_from(count).ok()?;
            if count == 0
                || count >= ScreenLineRead::MAX_ROWS
                || previous.aligned_frontier != self.frontier
                || !Arc::ptr_eq(&previous.sink, &self.sink)
                || !self
                    .interval
                    .retains(&previous.interval, start..self.frontier)
            {
                return None;
            }
            let complete = previous.rows.range(start..self.frontier).try_fold(
                0usize,
                |offset, (&key, line)| {
                    (key == start + offset as StableRowIndex && line.last_cell_was_wrapped())
                        .then_some(offset + 1)
                },
            );
            (complete == Some(count)).then_some(start)
        });
        let mut rows = VecDeque::new();
        let mut first = self.frontier;
        while first > fragment_start.unwrap_or(retained.start) {
            anyhow::ensure!(!cancelled(), "cold seam cancelled");
            anyhow::ensure!(
                rows.len() + self.resident.len() < ScreenLineRead::MAX_ROWS,
                "cold seam row limit"
            );
            let key = first - 1;
            let line = if fragment_start.is_some() {
                let fragment = self
                    .previous
                    .as_ref()
                    .and_then(|previous| previous.rows.get(&key))
                    .ok_or_else(|| anyhow::anyhow!("cold seam fragment unavailable"))?;
                charge.serialize_line(fragment)?;
                fragment.clone()
            } else {
                let mut batch = self.sink.load_scrollback_lines(key..first);
                anyhow::ensure!(batch.len() == 1, "cold seam source unavailable");
                let original = batch
                    .pop()
                    .ok_or_else(|| anyhow::anyhow!("cold seam source unavailable"))?;
                charge.line_with_fragment(self.previous.as_deref(), key, original)?
            };
            if !line.last_cell_was_wrapped() {
                if rows.is_empty() {
                    return Ok(self);
                }
                break;
            }
            rows.push_front(line);
            first = key;
        }
        if rows.is_empty() {
            return Ok(self);
        }
        let load_elapsed = profile_start.map(|start| start.elapsed());
        let mut logical: Option<Line> = None;
        let mut cells = 0usize;
        for source in rows.iter().chain(&self.resident) {
            anyhow::ensure!(!cancelled(), "cold seam cancelled");
            anyhow::ensure!(
                !source.has_image_attachments(),
                "cold seam mutable image source"
            );
            cells = cells
                .checked_add(source.len())
                .ok_or_else(|| anyhow::anyhow!("cold seam cell overflow"))?;
            anyhow::ensure!(
                cells <= ScreenLineRead::MAX_PAYLOAD_BYTES / std::mem::size_of::<Cell>(),
                "cold seam cell limit"
            );
            serde_json::to_writer(&mut charge, source)?;
            let mut line = source.clone();
            let seqno = line.current_seqno();
            line.set_last_cell_was_wrapped(false, seqno);
            if let Some(logical) = &mut logical {
                logical.append_line(line, logical.current_seqno().max(seqno));
            } else {
                logical = Some(line);
            }
        }
        let logical = logical.ok_or_else(|| anyhow::anyhow!("cold seam empty context"))?;
        let join_elapsed = profile_start.map(|start| start.elapsed());
        let seqno = logical.current_seqno();
        let (mut wrapped, _) = Screen::wrap_single_logical_line_for_resize(
            logical,
            self.witness.cols,
            seqno,
            self.policy,
            &mut LineWrapWidthPrefixScratch::default(),
        );
        anyhow::ensure!(
            wrapped.len() >= self.resident.len() && wrapped.len() <= ScreenLineRead::MAX_ROWS,
            "cold seam resident geometry unavailable"
        );
        for line in &mut wrapped {
            let _ = line.cells_mut_for_attr_changes_only();
            serde_json::to_writer(&mut charge, line)?;
        }
        let replacement_resident = wrapped.split_off(wrapped.len() - self.resident.len());
        anyhow::ensure!(
            !wrapped.is_empty(),
            "cold seam requires zero-cell continuation support"
        );
        let mut prefix = wrapped.first().cloned().unwrap_or_else(|| Line::new(seqno));
        prefix.set_last_cell_was_wrapped(false, seqno);
        for line in wrapped.iter().skip(1) {
            let mut line = line.clone();
            line.set_last_cell_was_wrapped(false, seqno);
            prefix.append_line(line, seqno);
        }
        // A cold-only consumer must reproduce exactly these prefix rows. A
        // paragraph-global policy can choose different breaks without the
        // resident suffix; refuse that transaction rather than publish two
        // geometries. Empty prefixes deliberately occupy zero visual rows.
        if !wrapped.is_empty() {
            let independent = Screen::wrap_cold_logical_line(
                prefix.clone(),
                self.witness.cols,
                seqno,
                self.policy,
                true,
                ScreenLineRead::MAX_ROWS,
            )
            .ok_or_else(|| anyhow::anyhow!("cold seam prefix row limit"))?;
            anyhow::ensure!(
                independent == wrapped,
                "cold seam prefix requires full paragraph geometry"
            );
        }
        let wrap_elapsed = profile_start.map(|start| start.elapsed());
        let mut replacements = BTreeMap::new();
        if let Some(previous) = &self.previous {
            for (key, line) in previous.rows.range(retained.clone()) {
                anyhow::ensure!(!cancelled(), "cold seam cancelled");
                serde_json::to_writer(&mut charge, line)?;
                replacements.insert(*key, line.clone());
            }
        }
        for (index, original) in rows.iter().enumerate() {
            let take = if index + 1 == rows.len() {
                prefix.len()
            } else {
                original.len().min(prefix.len())
            };
            // Empty Lines cannot retain WRAPPED without fabricating a space.
            anyhow::ensure!(
                take > 0,
                "cold seam requires zero-cell continuation support"
            );
            let remainder = prefix.split_off(take, seqno);
            prefix.set_last_cell_was_wrapped(true, seqno);
            let _ = prefix.cells_mut_for_attr_changes_only();
            serde_json::to_writer(&mut charge, &prefix)?;
            replacements.insert(first + index as StableRowIndex, prefix);
            prefix = remainder;
        }
        anyhow::ensure!(
            replacements.len() <= ScreenLineRead::MAX_ROWS,
            "cold seam fragment row limit"
        );
        anyhow::ensure!(!cancelled(), "cold seam cancelled");
        self.source = Some(first..self.frontier);
        self.replacement = Some((
            Arc::new(ColdRowFragments {
                sink: Arc::clone(&self.sink),
                interval: self.interval.clone(),
                rows: Arc::new(replacements),
                aligned_frontier: self.frontier,
                aligned_source_start: first,
                cols: self.witness.cols,
                dpi: self.witness.dpi,
                policy: self.policy,
            }),
            replacement_resident,
        ));
        if let (Some(start), Some(load), Some(join), Some(wrap)) =
            (profile_start, load_elapsed, join_elapsed, wrap_elapsed)
        {
            let total = start.elapsed();
            log::debug!(
                target: "frankenterm_term::screen::reflow_profile",
                "cold_seam_stages frontier={} cols={} source_rows={} resident_rows={} reused_fragments={} load_us={} join_us={} wrap_us={} fragments_us={} total_us={}",
                self.frontier,
                self.witness.cols,
                rows.len(),
                self.resident.len(),
                fragment_start.is_some(),
                load.as_micros(),
                join.saturating_sub(load).as_micros(),
                wrap.saturating_sub(join).as_micros(),
                total.saturating_sub(wrap).as_micros(),
                total.as_micros(),
            );
        }
        Ok(self)
    }
}

/// One exact previous paragraph, scoped to one width/policy and one captured
/// read. Equal compact widths avoid rehashing every width as a usize and
/// locking/cloning the process-wide wrap-plan cache for repeated geometry.
/// Text and attributes are deliberately absent; all join certificates, trim
/// boundaries and physical lengths participate in LineWrapGeometry equality.
#[cfg(feature = "use_serde")]
struct ColdGeometryRowCounts {
    previous: Option<(LineWrapGeometry, usize)>,
    cols: usize,
    cost_model: MonospaceKpCostModel,
    plans: usize,
    reuses: usize,
}

#[cfg(feature = "use_serde")]
impl ColdGeometryRowCounts {
    fn new(cols: usize, cost_model: MonospaceKpCostModel) -> Self {
        Self {
            previous: None,
            cols,
            cost_model,
            plans: 0,
            reuses: 0,
        }
    }

    fn retained_bytes(&self) -> usize {
        self.previous
            .as_ref()
            .map_or(0, |(g, _)| g.retained_bytes())
    }

    /// Charge the optional previous paragraph beside the next allocation's
    /// peak. Evict before allocation when necessary; reuse must never turn an
    /// otherwise admitted baseline operation into a budget refusal.
    fn reserve(&mut self, budget: usize, peak: Option<usize>) -> usize {
        if peak
            .and_then(|bytes| bytes.checked_add(self.retained_bytes()))
            .is_none_or(|bytes| bytes > budget)
        {
            self.previous = None;
        }
        budget.saturating_sub(self.retained_bytes())
    }

    fn count(
        &mut self,
        logical: LineWrapGeometry,
        scratch: &mut LineWrapWidthPrefixScratch,
        budget: usize,
    ) -> Option<usize> {
        let planning_budget = budget.checked_sub(logical.retained_bytes())?;
        // Keep the baseline planner admission even on a reuse. The previous
        // paragraph is retired before any planning allocation, and the newly
        // completed paragraph moves into its place without another clone.
        if logical.planning_bytes_upper_bound(self.cols, self.cost_model, scratch)?
            > planning_budget
        {
            return None;
        }
        let reused = self
            .previous
            .as_ref()
            .filter(|(before, _)| *before == logical)
            .map(|(_, count)| *count);
        self.previous = None;
        let count = if let Some(count) = reused {
            self.reuses += 1;
            #[cfg(test)]
            COLD_GEOMETRY_COUNT_REUSES.with(|n| n.set(n.get() + 1));
            count
        } else {
            self.plans += 1;
            logical.row_count(self.cols, self.cost_model, scratch)
        };
        self.previous = Some((logical, count));
        Some(count)
    }
}

#[cfg(feature = "use_serde")]
impl ScreenLineRead {
    pub const MAX_ROWS: usize = 16_384;
    pub const MAX_PAYLOAD_BYTES: usize = 32 * 1024 * 1024;
    const MAX_INDEX_SOURCE_ROWS: usize = 1_000_000;
    const MAX_INDEX_METADATA_BYTES: usize = 16 * 1024 * 1024;
    const MAX_INDEX_SOURCE_BYTES: usize = 512 * 1024 * 1024;
    const MAX_INDEX_CELL_VISITS: usize = 64 * 1024 * 1024;

    /// Plan coordinates from admitted widths without opening historical text.
    /// An uncertified join or exhausted metadata budget keeps the payload
    /// planner available; it never publishes a partial geometry index.
    fn geometry_cold_layout(
        &self,
        limit: usize,
        cancelled: &impl Fn() -> bool,
    ) -> anyhow::Result<Option<(Arc<ColdVisualLayout>, usize)>> {
        let profile_start =
            log::log_enabled!(target: "frankenterm_term::screen::reflow_profile", log::Level::Debug)
                .then(Instant::now);
        let Some(snapshot) = &self.geometry else {
            return Ok(None);
        };
        let Some((_, interval)) = &self.cold else {
            return Ok(None);
        };
        let header_bytes =
            std::mem::size_of::<ColdVisualLayout>() + 2 * std::mem::size_of::<usize>();
        let entry_bytes = std::mem::size_of::<(Range<StableRowIndex>, Range<StableRowIndex>)>();
        let Some(maximum_bytes) = snapshot
            .rows
            .len()
            .checked_mul(entry_bytes)
            .and_then(|bytes| bytes.checked_add(header_bytes))
        else {
            return Ok(None);
        };
        if maximum_bytes >= limit.min(Self::MAX_INDEX_METADATA_BYTES) {
            return Ok(None);
        }
        let mut groups = Vec::new();
        if groups.try_reserve_exact(snapshot.rows.len()).is_err() {
            return Ok(None);
        }
        let metadata_bytes = header_bytes + groups.capacity() * entry_bytes;
        if metadata_bytes >= limit.min(Self::MAX_INDEX_METADATA_BYTES) {
            return Ok(None);
        }
        let Some(group_budget) = limit.checked_sub(metadata_bytes) else {
            return Ok(None);
        };
        let aligned_seam = self.fragments.as_ref().is_some_and(|fragments| {
            fragments.alignment_retained(interval)
                && fragments.aligned_at(
                    self.hot_top,
                    self.witness.cols,
                    self.witness.dpi,
                    self.wrap_policy,
                )
        });
        let mut group: Option<LineWrapGeometry> = None;
        let mut group_start = snapshot.source.start;
        let mut source_end = self.hot_top;
        let mut visual_rows: StableRowIndex = 0;
        let mut cell_visits = 0usize;
        let mut scratch = LineWrapWidthPrefixScratch::default();
        let mut counts =
            ColdGeometryRowCounts::new(self.witness.cols, self.wrap_policy.kp_cost_model);
        for (offset, captured) in snapshot.rows.iter().enumerate() {
            anyhow::ensure!(!cancelled(), "cold geometry index cancelled");
            let scratch_bytes = scratch
                .capacity()
                .checked_mul(std::mem::size_of::<u128>())
                .ok_or_else(|| anyhow::anyhow!("cold geometry scratch overflow"))?;
            let Some(geometry_budget) = group_budget.checked_sub(scratch_bytes) else {
                return Ok(None);
            };
            let row = snapshot.source.start + StableRowIndex::try_from(offset)?;
            let replacement = self
                .fragments
                .as_ref()
                .and_then(|fragments| fragments.rows.get(&row));
            // Append may retain both the old and replacement width buffers.
            // This conservative peak is only for optional-cache eviction; the
            // original capture/append admission below remains authoritative.
            let peak = if replacement.is_some() {
                None // replacement capture has its own temporary text budget
            } else if let Some(logical) = &group {
                logical
                    .retained_bytes()
                    .checked_mul(2)
                    .and_then(|bytes| bytes.checked_add(captured.geometry.retained_bytes()))
            } else {
                Some(captured.geometry.retained_bytes())
            };
            let geometry_budget = counts.reserve(geometry_budget, peak);
            let replacement_geometry;
            let (geometry, wrapped) = if let Some(line) = replacement {
                let available = geometry_budget
                    .saturating_sub(group.as_ref().map_or(0, LineWrapGeometry::retained_bytes));
                let Some(rebuilt) = LineWrapGeometry::capture(line, available) else {
                    return Ok(None);
                };
                if !rebuilt.supports_join() {
                    return Ok(None);
                }
                replacement_geometry = rebuilt;
                (&replacement_geometry, line.last_cell_was_wrapped())
            } else {
                (captured.geometry.as_ref(), captured.wrapped)
            };
            cell_visits = match cell_visits.checked_add(geometry.physical_len().max(1)) {
                Some(visits) if visits <= Self::MAX_INDEX_CELL_VISITS => visits,
                _ => return Ok(None),
            };
            if let Some(logical) = &mut group {
                let available = geometry_budget.saturating_sub(if replacement.is_some() {
                    geometry.retained_bytes()
                } else {
                    0
                });
                if !logical.append(geometry, available) {
                    return Ok(None);
                }
            } else {
                let required = geometry
                    .retained_bytes()
                    .checked_mul(if replacement.is_some() { 2 } else { 1 });
                if required.is_none_or(|bytes| bytes > geometry_budget) {
                    return Ok(None);
                }
                group = Some(geometry.clone());
            }
            let end = row
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("cold geometry source overflow"))?;
            if end.saturating_sub(group_start) as usize > Self::MAX_ROWS {
                return Ok(None);
            }
            if wrapped && !(end == self.hot_top && aligned_seam) {
                continue;
            }
            let mut logical = group.take().expect("current row created its logical group");
            if wrapped && end == self.hot_top && aligned_seam {
                // This is only a prefix of a paragraph continuing in resident
                // rows. Its last spaces are separators, not terminal padding.
                logical.preserve_trailing_spaces();
            }
            let count =
                if wrapped && end == self.hot_top && aligned_seam && logical.physical_len() == 0 {
                    0
                } else {
                    let transient_bytes = if replacement.is_some() {
                        geometry.retained_bytes()
                    } else {
                        0
                    };
                    let Some(planning_budget) = group_budget.checked_sub(transient_bytes) else {
                        return Ok(None);
                    };
                    let Some(count) = counts.count(logical, &mut scratch, planning_budget) else {
                        return Ok(None);
                    };
                    count
                };
            if count > Self::MAX_ROWS {
                return Ok(None);
            }
            let next_visual = visual_rows
                .checked_add(StableRowIndex::try_from(count)?)
                .ok_or_else(|| anyhow::anyhow!("cold geometry visual overflow"))?;
            groups.push((group_start..end, visual_rows..next_visual));
            visual_rows = next_visual;
            group_start = end;
        }
        if group.is_some() {
            source_end = group_start;
            anyhow::ensure!(self.end <= source_end, ColdReadGeometryUnavailable);
        }
        let visual_first = source_end
            .checked_sub(visual_rows)
            .ok_or_else(|| anyhow::anyhow!("cold geometry origin overflow"))?;
        for (_, visual) in &mut groups {
            anyhow::ensure!(!cancelled(), "cold geometry index cancelled");
            visual.start = visual
                .start
                .checked_add(visual_first)
                .ok_or_else(|| anyhow::anyhow!("cold geometry coordinate overflow"))?;
            visual.end = visual
                .end
                .checked_add(visual_first)
                .ok_or_else(|| anyhow::anyhow!("cold geometry coordinate overflow"))?;
        }
        anyhow::ensure!(!cancelled(), "cold geometry index cancelled");
        debug!(
            "cold_geometry_counts plans={} reuses={}",
            counts.plans, counts.reuses
        );
        if let Some(start) = profile_start {
            log::debug!(
                target: "frankenterm_term::screen::reflow_profile",
                "cold_geometry_stages cols={} source_rows={} groups={} plans={} reuses={} total_us={}",
                self.witness.cols,
                snapshot.rows.len(),
                groups.len(),
                counts.plans,
                counts.reuses,
                start.elapsed().as_micros(),
            );
        }
        Ok(Some((
            Arc::new(ColdVisualLayout {
                kind: ColdVisualLayoutKind::Canonical,
                source: snapshot.source.start..source_end,
                visual: visual_first..source_end,
                resident_frontier: self.hot_top,
                groups,
                witness: self.witness.clone(),
                interval: interval.clone(),
            }),
            metadata_bytes,
        )))
    }

    /// Scan a large prefix without retaining its decoded rows. A completed
    /// logical group is wrapped, reduced to coordinate metadata, and retired
    /// before the next group. Viewport cells are hydrated separately after
    /// the final visual origin is known.
    fn streamed_cold_layout(
        &self,
        limit: usize,
        cancelled: &impl Fn() -> bool,
    ) -> anyhow::Result<(Arc<ColdVisualLayout>, usize)> {
        let (sink, interval) = self
            .cold
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("cold index source unavailable"))?;
        let retained = interval
            .rows()
            .ok_or_else(|| anyhow::anyhow!("cold index source unavailable"))?;
        let exhausted = || {
            self.index_budget_exhausted
                .store(true, std::sync::atomic::Ordering::Release)
        };
        if self.hot_top.saturating_sub(retained.start) as usize > Self::MAX_INDEX_SOURCE_ROWS {
            exhausted();
            anyhow::bail!("cold index source row work limit");
        }
        let aligned_seam = self.fragments.as_ref().is_some_and(|fragments| {
            fragments.alignment_retained(interval)
                && fragments.aligned_at(
                    self.hot_top,
                    self.witness.cols,
                    self.witness.dpi,
                    self.wrap_policy,
                )
        });
        let metadata_limit = limit.min(Self::MAX_INDEX_METADATA_BYTES);
        let header_bytes =
            std::mem::size_of::<ColdVisualLayout>() + 2 * std::mem::size_of::<usize>();
        let entry_bytes = std::mem::size_of::<(Range<StableRowIndex>, Range<StableRowIndex>)>();
        let maximum_entries = metadata_limit.saturating_sub(header_bytes) / entry_bytes;
        let mut groups = Vec::new();
        let mut row = retained.start;
        let mut group_start = row;
        let mut logical: Option<Line> = None;
        let mut group_rows = 0usize;
        let mut group_cells = 0usize;
        let mut source_bytes = 0usize;
        let mut cell_visits = 0usize;
        let mut visual_rows: StableRowIndex = 0;
        let mut source_end = self.hot_top;
        let mut charge = ColdReadCharge {
            used: 0,
            limit,
            index_failure: Some(Arc::clone(&self.index_budget_exhausted)),
        };
        while row < self.hot_top {
            anyhow::ensure!(!cancelled(), "cold index cancelled");
            let end = self.hot_top.min(row.saturating_add(32));
            validate_cold_source_before_read(sink.as_ref(), interval, row..end)?;
            let batch = sink.load_scrollback_lines(row..end);
            anyhow::ensure!(
                !batch.is_empty() && batch.len() <= (end - row) as usize,
                "cold index missing rows"
            );
            for original in batch {
                anyhow::ensure!(!cancelled(), "cold index cancelled");
                let before_bytes = charge.used;
                let mut line =
                    charge.line_with_fragment(self.fragments.as_deref(), row, original)?;
                source_bytes = source_bytes
                    .checked_add(charge.used - before_bytes)
                    .ok_or_else(|| anyhow::anyhow!("cold index source byte overflow"))?;
                group_cells = group_cells
                    .checked_add(line.len())
                    .ok_or_else(|| anyhow::anyhow!("cold index group cell overflow"))?;
                cell_visits = cell_visits
                    .checked_add(line.len().max(1))
                    .ok_or_else(|| anyhow::anyhow!("cold index cell work overflow"))?;
                group_rows += 1;
                if source_bytes > Self::MAX_INDEX_SOURCE_BYTES
                    || cell_visits > Self::MAX_INDEX_CELL_VISITS
                    || group_rows > Self::MAX_ROWS
                    || group_cells > charge.limit / std::mem::size_of::<Cell>()
                {
                    exhausted();
                    anyhow::bail!("cold index group or job work limit");
                }
                let wrapped = line.last_cell_was_wrapped();
                let seqno = line.current_seqno();
                line.set_last_cell_was_wrapped(false, seqno);
                if let Some(logical) = &mut logical {
                    logical.append_line(line, logical.current_seqno().max(seqno));
                } else {
                    logical = Some(line);
                }
                row += 1;
                if wrapped && !(row == self.hot_top && aligned_seam) {
                    continue;
                }
                let logical = logical
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("cold index empty group"))?;
                let seqno = logical.current_seqno();
                let wrapped = Screen::wrap_cold_logical_line(
                    logical,
                    self.witness.cols,
                    seqno,
                    self.wrap_policy,
                    wrapped && row == self.hot_top && aligned_seam,
                    Self::MAX_ROWS,
                )
                .ok_or_else(|| {
                    exhausted();
                    anyhow::anyhow!("cold index group output row limit")
                })?;
                let next_visual = visual_rows
                    .checked_add(StableRowIndex::try_from(wrapped.len())?)
                    .ok_or_else(|| anyhow::anyhow!("cold index visual coordinate overflow"))?;
                drop(wrapped);
                if groups.len() == groups.capacity() {
                    let next_capacity = groups
                        .capacity()
                        .saturating_mul(2)
                        .max(64)
                        .min(maximum_entries);
                    if next_capacity <= groups.len() {
                        exhausted();
                        anyhow::bail!("cold index metadata limit");
                    }
                    groups.try_reserve_exact(next_capacity - groups.len())?;
                }
                groups.push((group_start..row, visual_rows..next_visual));
                visual_rows = next_visual;
                group_start = row;
                group_rows = 0;
                group_cells = 0;
                charge.used = 0;
                charge.limit = limit
                    .checked_sub(header_bytes + groups.capacity() * entry_bytes)
                    .ok_or_else(|| anyhow::anyhow!("cold index metadata limit"))?;
            }
        }
        if logical.is_some() {
            source_end = group_start;
            anyhow::ensure!(self.end <= source_end, ColdReadGeometryUnavailable);
        }
        let visual_first = source_end
            .checked_sub(visual_rows)
            .ok_or_else(|| anyhow::anyhow!("cold index visual origin overflow"))?;
        for (_, visual) in &mut groups {
            anyhow::ensure!(!cancelled(), "cold index cancelled");
            visual.start = visual
                .start
                .checked_add(visual_first)
                .ok_or_else(|| anyhow::anyhow!("cold index visual coordinate overflow"))?;
            visual.end = visual
                .end
                .checked_add(visual_first)
                .ok_or_else(|| anyhow::anyhow!("cold index visual coordinate overflow"))?;
        }
        let metadata_bytes = groups
            .capacity()
            .checked_mul(entry_bytes)
            .and_then(|bytes| bytes.checked_add(header_bytes))
            .ok_or_else(|| anyhow::anyhow!("cold index metadata overflow"))?;
        if metadata_bytes > metadata_limit {
            exhausted();
            anyhow::bail!("cold index metadata limit");
        }
        anyhow::ensure!(!cancelled(), "cold index cancelled");
        Ok((
            Arc::new(ColdVisualLayout {
                kind: ColdVisualLayoutKind::Canonical,
                source: retained.start..source_end,
                visual: visual_first..source_end,
                resident_frontier: self.hot_top,
                groups,
                witness: self.witness.clone(),
                interval: interval.clone(),
            }),
            metadata_bytes,
        ))
    }

    pub fn first_row(&self) -> StableRowIndex {
        self.first
    }
    /// The captured metadata proved that no readable row precedes this one.
    /// Unknown spill metadata and rebased cold geometry cannot grant this proof.
    pub fn starts_at_known_history_start(&self) -> bool {
        self.known_resident_history_start == Some(self.first)
    }
    pub fn failure_witness(&self) -> LineReadFailureWitness {
        LineReadFailureWitness {
            screen: self.witness.clone(),
            layout_seqno: self.layout_seqno,
            cold: self.cold.clone(),
            index_budget_exhausted: Arc::clone(&self.index_budget_exhausted),
            attempted_index: self.attempted_index,
            fragments: self.fragments.clone(),
        }
    }
    pub fn row_count(&self) -> usize {
        self.rendered
            .as_ref()
            .map_or(self.hydrated.len() + self.resident.len(), Vec::len)
    }
    pub fn requested_row_count(&self) -> usize {
        self.end.saturating_sub(self.first) as usize
    }
    pub fn payload_bytes(&self) -> usize {
        self.payload_bytes
    }

    /// A cache may serve a subset of the complete logical context hydrated by
    /// its worker. Borrowed cells stay owned by this read's retirement guard.
    pub fn cached_lines(
        &self,
        requested: Range<StableRowIndex>,
    ) -> Option<(StableRowIndex, &[Line])> {
        let (first, lines) = self.logical_view.as_ref()?;
        let end = first.checked_add(StableRowIndex::try_from(lines.len()).ok()?)?;
        if requested.start < *first || requested.end > end || requested.end < requested.start {
            return None;
        }
        Some((
            requested.start,
            &lines[(requested.start - first) as usize..(requested.end - first) as usize],
        ))
    }

    /// Clone a cached viewport only after every row is structurally admitted.
    /// The caller already holds the publication/source fence. No temporary
    /// reference vector or partially cloned prefix is allocated on refusal.
    pub fn try_clone_viewport_for_snapshot(
        &self,
        requested: Range<StableRowIndex>,
        bytes_left: &mut usize,
        work_left: &mut usize,
    ) -> Option<(StableRowIndex, Vec<Line>)> {
        let (first, head, tail) = if let Some((first, lines)) = self.cached_lines(requested) {
            (first, lines, &[][..])
        } else if let Some(lines) = self.rendered.as_ref() {
            (self.first, lines.as_slice(), &[][..])
        } else {
            (
                self.first,
                self.hydrated.as_slice(),
                self.resident.as_slice(),
            )
        };
        Line::try_clone_batch_for_snapshot(head, tail, bytes_left, work_left)
            .map(|rows| (first, rows))
    }

    /// Run only off the UI thread and outside terminal/registration locks.
    /// The bounded writer counts full serialized attributes and image payloads,
    /// not just row pointers. Shared source allocations are not new allocations
    /// and remain subject to the source store's own admission limits.
    pub fn hydrate(self, cancelled: impl Fn() -> bool) -> anyhow::Result<Self> {
        self.hydrate_with_payload_limit(Self::MAX_PAYLOAD_BYTES, cancelled)
    }

    /// Prepare only layout authority when admitted widths are sufficient.
    /// The existing bounded payload path remains the fallback for unavailable
    /// geometry. This does not make an unhydrated ScreenLineRead publishable.
    pub fn prepare_cold_layout(
        mut self,
        cancelled: impl Fn() -> bool,
    ) -> anyhow::Result<PreparedColdLayout> {
        anyhow::ensure!(!self.complete, "line read already hydrated");
        anyhow::ensure!(!cancelled(), "cold layout preparation cancelled");
        if self.layout.is_none()
            && self.attempted_index
            && self.first < self.resident_first
            && self.geometry.is_some()
        {
            if let Some((layout, metadata_bytes)) =
                self.geometry_cold_layout(Self::MAX_PAYLOAD_BYTES, &cancelled)?
            {
                debug!(
                    "cold_geometry_layout outcome=ready groups={} metadata_bytes={} purpose=layout_only",
                    layout.groups.len(),
                    metadata_bytes
                );
                // Coordinates depend on the complete indexed source, even
                // though the capture requested only one otherwise unused row.
                self.cold_context = Some(layout.source.clone());
                self.layout = Some(layout);
                self.geometry = None;
                self.payload_bytes = metadata_bytes;
                return Ok(PreparedColdLayout {
                    source: self,
                    retired_layout: None,
                    installed: false,
                });
            }
        }
        Ok(PreparedColdLayout {
            source: self.hydrate(cancelled)?,
            retired_layout: None,
            installed: false,
        })
    }

    pub fn hydrate_with_payload_limit(
        mut self,
        limit: usize,
        cancelled: impl Fn() -> bool,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(!self.complete, "line read already hydrated");
        anyhow::ensure!(limit <= Self::MAX_PAYLOAD_BYTES, "line read payload limit");
        if self.layout.is_none()
            && self.attempted_index
            && self.first < self.resident_first
            && (self.geometry.is_some()
                || self
                    .cold
                    .as_ref()
                    .and_then(|(_, interval)| interval.rows())
                    .is_some_and(|rows| {
                        self.hot_top.saturating_sub(rows.start) as usize > Self::MAX_ROWS
                    }))
        {
            let (layout, metadata_bytes) = match self.geometry_cold_layout(limit, &cancelled)? {
                Some(layout) => {
                    debug!(
                        "cold_geometry_layout outcome=ready groups={} metadata_bytes={}",
                        layout.0.groups.len(),
                        layout.1
                    );
                    layout
                }
                None => {
                    debug!("cold_geometry_layout outcome=unavailable");
                    self.streamed_cold_layout(limit, &cancelled)?
                }
            };
            let requested_rows = self.end.saturating_sub(self.first);
            self.first = self.first.max(layout.visual.start);
            if self.resident.is_empty() {
                // The first request can predate knowledge of the new visual
                // origin. Preserve its row count inside indexed cold history;
                // no terminal access or uncaptured resident rows are needed.
                self.end = self
                    .first
                    .checked_add(requested_rows)
                    .ok_or_else(|| anyhow::anyhow!("cold read range overflow"))?
                    .min(layout.visual.end);
                self.resident_first = self.end;
                anyhow::ensure!(self.first <= self.end, ColdReadGeometryUnavailable);
            } else {
                // A mixed capture already owns an exact resident endpoint.
                // Extending it would invent rows outside that snapshot.
                self.end = self.end.max(self.first);
            }
            self.geometry = None;
            self.layout = Some(layout);
            let mut ready = self.hydrate_with_payload_limit(limit - metadata_bytes, cancelled)?;
            ready.payload_bytes += metadata_bytes;
            return Ok(ready);
        }
        let mut charge = ColdReadCharge {
            used: 0,
            limit,
            index_failure: None,
        };
        let fragment_alignment_retained = self.fragments.as_ref().is_some_and(|fragments| {
            self.cold
                .as_ref()
                .is_some_and(|(_, interval)| fragments.alignment_retained(interval))
        });
        let aligned_seam = fragment_alignment_retained
            && self.fragments.as_ref().is_some_and(|fragments| {
                fragments.aligned_at(
                    self.hot_top,
                    self.witness.cols,
                    self.witness.dpi,
                    self.wrap_policy,
                )
            });
        fn materialize_physical_row(
            line: &mut Line,
            charge: &mut ColdReadCharge,
            cancelled: &impl Fn() -> bool,
        ) -> anyhow::Result<()> {
            anyhow::ensure!(!cancelled(), "cold read cancelled");
            let before = charge.used;
            let cells = line
                .len()
                .checked_mul(std::mem::size_of::<Cell>())
                .and_then(|bytes| bytes.checked_add(std::mem::size_of::<Line>()))
                .and_then(|bytes| before.checked_add(bytes))
                .filter(|bytes| *bytes <= charge.limit)
                .ok_or(ColdReadPayloadLimit)?;
            // Materialize on the worker, after bounding cell-array growth.
            // This is the final raw-row representation: its complete fields
            // are charged once, including all attributes and image payloads.
            let _ = line.cells_mut_for_attr_changes_only();
            charge.serialize_line(line)?;
            charge.used = charge.used.max(cells);
            anyhow::ensure!(!cancelled(), "cold read cancelled");
            Ok(())
        }
        fn context_batch(
            sink: &dyn crate::config::ScrollbackSpillSink,
            interval: &crate::config::ScrollbackInterval,
            mut range: Range<StableRowIndex>,
            charge: &mut ColdReadCharge,
            cancelled: &impl Fn() -> bool,
            fragments: Option<&ColdRowFragments>,
            stored_physical: bool,
        ) -> anyhow::Result<Vec<Line>> {
            let mut result = Vec::new();
            while range.start < range.end {
                anyhow::ensure!(!cancelled(), "cold read cancelled");
                validate_cold_source_before_read(sink, interval, range.clone())?;
                let batch = sink.load_scrollback_lines(range.clone());
                anyhow::ensure!(
                    !batch.is_empty() && batch.len() <= (range.end - range.start) as usize,
                    "cold logical context unavailable"
                );
                for mut line in batch {
                    anyhow::ensure!(!cancelled(), "cold read cancelled");
                    if stored_physical {
                        anyhow::ensure!(fragments.is_none(), ColdReadGeometryUnavailable);
                        materialize_physical_row(&mut line, charge, cancelled)?;
                        result.push(line);
                        range.start += 1;
                        continue;
                    }
                    let line = charge.line_with_fragment(fragments, range.start, line)?;
                    result.push(line);
                    range.start += 1;
                }
            }
            Ok(result)
        }
        if let Some(layout) = self.layout.as_ref().map(Arc::clone) {
            anyhow::ensure!(
                self.resident_first <= layout.visual.end,
                ColdReadGeometryUnavailable
            );
            let (sink, interval) = self
                .cold
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("cold read unavailable"))?;
            let mut visible = Vec::new();
            let mut context_first = None;
            let mut expanded_cells = 0usize;
            let first_group = layout
                .groups
                .partition_point(|(_, visual)| visual.end <= self.first);
            let end_group = layout
                .groups
                .partition_point(|(_, visual)| visual.start < self.resident_first);
            let groups = &layout.groups[first_group..end_group.max(first_group)];
            let source_end = groups.last().map_or(0, |(source, _)| source.end);
            let mut next_source_row = groups.first().map_or(0, |(source, _)| source.start);
            let context_source_start = next_source_row;
            if layout.stored_physical() {
                anyhow::ensure!(self.fragments.is_none(), ColdReadGeometryUnavailable);
                let metadata_bytes = std::mem::size_of::<ColdVisualLayout>()
                    + 2 * std::mem::size_of::<usize>()
                    + layout.groups.capacity()
                        * std::mem::size_of::<(Range<StableRowIndex>, Range<StableRowIndex>)>();
                anyhow::ensure!(metadata_bytes <= limit, "cold physical index payload limit");
                charge.used = metadata_bytes;
                anyhow::ensure!(
                    source_end.saturating_sub(next_source_row) as usize
                        <= Self::MAX_ROWS.saturating_sub(self.resident.len()),
                    "cold logical context row limit"
                );
            }
            let mut prefetched = Vec::new().into_iter();
            for (source, visual) in groups {
                anyhow::ensure!(!cancelled(), "cold read cancelled");
                anyhow::ensure!(
                    source.start == next_source_row && source.start < source.end,
                    "cold visual index source discontinuity"
                );
                let mut source_row = source.start;
                let mut logical: Option<Line> = None;
                let mut tail_wrapped = false;
                while source_row < source.end {
                    anyhow::ensure!(!cancelled(), "cold read cancelled");
                    if prefetched.len() == 0 {
                        // Neighboring logical groups share one bounded storage
                        // read. Charge every prefetched row, including fragment
                        // replacements, before retaining it across a group.
                        // Never read beyond the selected logical context.
                        prefetched = context_batch(
                            sink.as_ref(),
                            interval,
                            source_row..source_end.min(source_row.saturating_add(32)),
                            &mut charge,
                            &cancelled,
                            self.fragments.as_deref(),
                            layout.stored_physical(),
                        )?
                        .into_iter();
                    }
                    let mut line = prefetched
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("cold logical context unavailable"))?;
                    if layout.stored_physical() {
                        // The admission receipt binds these exact physical
                        // rows to the unchanged coordinate generation. Joining
                        // and wrapping again could change even same-width rows.
                        visible.push(line);
                        source_row += 1;
                        continue;
                    }
                    tail_wrapped = line.last_cell_was_wrapped();
                    let seqno = line.current_seqno();
                    line.set_last_cell_was_wrapped(false, seqno);
                    if let Some(logical) = &mut logical {
                        let seqno = logical.current_seqno().max(seqno);
                        logical.append_line(line, seqno);
                    } else {
                        logical = Some(line);
                    }
                    source_row += 1;
                }
                next_source_row = source_row;
                if layout.stored_physical() {
                    anyhow::ensure!(source == visual, "cold physical index coordinates changed");
                    context_first.get_or_insert(visual.start);
                    continue;
                }
                let logical =
                    logical.ok_or_else(|| anyhow::anyhow!("cold logical group unavailable"))?;
                expanded_cells = expanded_cells
                    .checked_add(logical.len())
                    .ok_or_else(|| anyhow::anyhow!("cold reflow expanded cell overflow"))?;
                anyhow::ensure!(
                    expanded_cells <= limit / std::mem::size_of::<Cell>(),
                    "cold reflow expanded cell limit"
                );
                let seqno = logical.current_seqno();
                let wrapped = Screen::wrap_cold_logical_line(
                    logical,
                    self.witness.cols,
                    seqno,
                    self.wrap_policy,
                    tail_wrapped
                        && fragment_alignment_retained
                        && self.fragments.as_ref().is_some_and(|fragments| {
                            fragments.aligned_at(
                                source.end,
                                self.witness.cols,
                                self.witness.dpi,
                                self.wrap_policy,
                            )
                        }),
                    (visual.end - visual.start) as usize,
                )
                .ok_or_else(|| anyhow::anyhow!("cold visual index source changed"))?;
                anyhow::ensure!(
                    wrapped.len() == (visual.end - visual.start) as usize,
                    "cold visual index source changed"
                );
                context_first.get_or_insert(visual.start);
                visible.extend(wrapped);
            }
            if layout.stored_physical() {
                // Prefetched physical rows already own their final charged
                // cells. Charging them again rejects an ordinary 2,000-row
                // reply even when its complete payload fits the hard limit.
                for source in &self.resident {
                    let mut line = source.clone();
                    materialize_physical_row(&mut line, &mut charge, &cancelled)?;
                    visible.push(line);
                }
            } else {
                visible.extend(self.resident.iter().cloned());
                for line in &mut visible {
                    anyhow::ensure!(!cancelled(), "cold read cancelled");
                    let _ = line.cells_mut_for_attr_changes_only();
                    charge.serialize_line(line)?;
                }
            }
            let context_first = context_first.unwrap_or(self.resident_first);
            let selected: Vec<_> = visible
                .iter()
                .skip((self.first - context_first) as usize)
                .take(self.requested_row_count())
                .cloned()
                .collect();
            anyhow::ensure!(
                selected.len() == self.requested_row_count(),
                "cold visual index incomplete viewport"
            );
            self.cold_context = Some(if layout.stored_physical() {
                context_source_start..source_end
            } else {
                layout.source.clone()
            });
            self.logical_view = Some((context_first, visible));
            self.rendered = Some(selected);
            self.payload_bytes = charge.used;
            self.complete = true;
            return Ok(self);
        }
        // Index the entire admitted cold prefix so a row-count change in an
        // earlier logical group cannot silently shift only this viewport.
        // All reads and reflow happen on the worker under one payload budget.
        let cold_source = if self.first < self.resident_first && self.attempted_index {
            let retained = self
                .cold
                .as_ref()
                .and_then(|(_, interval)| interval.rows())
                .ok_or_else(|| anyhow::anyhow!("cold read unavailable"))?;
            let source = retained.start..self.hot_top;
            (source.end.saturating_sub(source.start) as usize
                <= Self::MAX_ROWS.saturating_sub(self.resident.len()))
            .then_some(source)
        } else {
            None
        };
        if cold_source.is_some() {
            charge.index_failure = Some(Arc::clone(&self.index_budget_exhausted));
        }
        let mut row = cold_source
            .as_ref()
            .map_or(self.first, |source| source.start);
        let cold_end = cold_source
            .as_ref()
            .map_or(self.resident_first, |source| source.end);
        while row < cold_end {
            anyhow::ensure!(!cancelled(), "cold read cancelled");
            let (sink, interval) = self
                .cold
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("cold read unavailable"))?;
            let end = cold_end.min(row.saturating_add(32));
            validate_cold_source_before_read(sink.as_ref(), interval, row..end)?;
            let batch = sink.load_scrollback_lines(row..end);
            anyhow::ensure!(
                !batch.is_empty() && batch.len() <= (end - row) as usize,
                "cold read missing rows"
            );
            for line in batch {
                anyhow::ensure!(!cancelled(), "cold read cancelled");
                let line = charge.line_with_fragment(self.fragments.as_deref(), row, line)?;
                self.hydrated.push(line);
                row += 1;
            }
        }
        for line in &self.resident {
            anyhow::ensure!(!cancelled(), "cold read cancelled");
            charge.serialize_line(line)?;
        }
        if !self.hydrated.is_empty() {
            let (sink, interval) = self
                .cold
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("cold context unavailable"))?;
            let retained = interval
                .rows()
                .ok_or_else(|| anyhow::anyhow!("cold context unavailable"))?;
            let mut context: VecDeque<Line> = self.hydrated.iter().cloned().collect();
            let mut context_first = cold_source
                .as_ref()
                .map_or(self.first, |source| source.start);
            let mut checked_first = context_first;
            let mut predecessor_batch_rows = 1;
            // The predecessor is part of the witness even when it terminates
            // a different logical line: its wrap bit justified this boundary.
            while context_first > retained.start {
                anyhow::ensure!(!cancelled(), "cold read cancelled");
                anyhow::ensure!(
                    context.len() + self.resident.len() < Self::MAX_ROWS,
                    "cold logical context row limit"
                );
                let count = (Self::MAX_ROWS - context.len() - self.resident.len())
                    .min(predecessor_batch_rows) as StableRowIndex;
                let from = retained.start.max(context_first.saturating_sub(count));
                let batch = context_batch(
                    sink.as_ref(),
                    interval,
                    from..context_first,
                    &mut charge,
                    &cancelled,
                    self.fragments.as_deref(),
                    false,
                )?;
                let mut found_start = false;
                for line in batch.into_iter().rev() {
                    let previous = context_first - 1;
                    checked_first = previous;
                    if !line.last_cell_was_wrapped() {
                        found_start = true;
                        break;
                    }
                    context.push_front(line);
                    context_first = previous;
                }
                if found_start {
                    break;
                }
                predecessor_batch_rows = (predecessor_batch_rows * 2).min(32);
            }
            let mut context_end = cold_source
                .as_ref()
                .map_or(self.resident_first, |source| source.end);
            let mut successor_batch_rows = 1;
            while context.back().is_some_and(Line::last_cell_was_wrapped)
                && context_end < self.hot_top
            {
                anyhow::ensure!(!cancelled(), "cold read cancelled");
                anyhow::ensure!(
                    context.len() + self.resident.len() < Self::MAX_ROWS,
                    "cold logical context row limit"
                );
                anyhow::ensure!(
                    context_end < retained.end,
                    "cold logical successor unavailable"
                );
                let count = (Self::MAX_ROWS - context.len() - self.resident.len())
                    .min(successor_batch_rows) as StableRowIndex;
                let to = self
                    .hot_top
                    .min(retained.end)
                    .min(context_end.saturating_add(count));
                let batch = context_batch(
                    sink.as_ref(),
                    interval,
                    context_end..to,
                    &mut charge,
                    &cancelled,
                    self.fragments.as_deref(),
                    false,
                )?;
                for line in batch {
                    let wrapped = line.last_cell_was_wrapped();
                    context.push_back(line);
                    context_end += 1;
                    if !wrapped {
                        break;
                    }
                }
                successor_batch_rows = (successor_batch_rows * 2).min(32);
            }
            self.cold_context = Some(checked_first..context_end);
            let cold_len = context.len();
            context.extend(self.resident.iter().cloned());
            let source: Vec<Line> = context.into_iter().collect();
            let mut output = Vec::with_capacity(source.len());
            let mut groups = Vec::new();
            let mut expanded_cells = 0usize;
            let mut index_source_end = self.hot_top;
            let mut start = 0;
            while start < source.len() {
                anyhow::ensure!(!cancelled(), "cold read cancelled");
                if start >= cold_len {
                    output.extend(source[start..].iter().cloned());
                    break;
                }
                let mut end = start + 1;
                while source[end - 1].last_cell_was_wrapped()
                    && end < source.len()
                    && !(aligned_seam && end == cold_len)
                {
                    end += 1;
                }
                if cold_source.is_some()
                    && ((source[end - 1].last_cell_was_wrapped()
                        && !(aligned_seam && end == cold_len))
                        || end > cold_len)
                {
                    // An unfinished trailing logical group must not make all
                    // earlier cold history unreadable. Keep its source/visual
                    // coordinates reserved until the seam transaction can
                    // move its resident cells and anchors atomically.
                    index_source_end = context_first + start as StableRowIndex;
                    anyhow::ensure!(self.end <= index_source_end, ColdReadGeometryUnavailable);
                    break;
                }
                anyhow::ensure!(
                    !source[end - 1].last_cell_was_wrapped() || (aligned_seam && end == cold_len),
                    ColdReadGeometryUnavailable
                );
                anyhow::ensure!(end <= cold_len, ColdReadGeometryUnavailable);
                let mut logical = source[start].clone();
                logical.set_last_cell_was_wrapped(false, logical.current_seqno());
                for line in &source[start + 1..end] {
                    let seqno = logical.current_seqno().max(line.current_seqno());
                    let mut line = line.clone();
                    line.set_last_cell_was_wrapped(false, seqno);
                    logical.append_line(line, seqno);
                }
                let seqno = logical.current_seqno();
                logical.set_last_cell_was_wrapped(false, seqno);
                expanded_cells = expanded_cells
                    .checked_add(logical.len())
                    .ok_or_else(|| anyhow::anyhow!("cold reflow expanded cell overflow"))?;
                if cold_source.is_some() && expanded_cells > limit / std::mem::size_of::<Cell>() {
                    self.index_budget_exhausted
                        .store(true, std::sync::atomic::Ordering::Release);
                }
                anyhow::ensure!(
                    expanded_cells <= limit / std::mem::size_of::<Cell>(),
                    "cold reflow expanded cell limit"
                );
                let wrapped = Screen::wrap_cold_logical_line(
                    logical,
                    self.witness.cols,
                    seqno,
                    self.wrap_policy,
                    source[end - 1].last_cell_was_wrapped() && aligned_seam && end == cold_len,
                    if cold_source.is_some() {
                        Self::MAX_ROWS.saturating_sub(output.len())
                    } else {
                        end - start
                    },
                )
                .ok_or_else(|| {
                    if cold_source.is_some() {
                        self.index_budget_exhausted
                            .store(true, std::sync::atomic::Ordering::Release);
                        anyhow::anyhow!("cold visual index output row limit")
                    } else {
                        anyhow::anyhow!(ColdReadGeometryUnavailable)
                    }
                })?;
                if cold_source.is_none() {
                    anyhow::ensure!(wrapped.len() == end - start, ColdReadGeometryUnavailable);
                }
                if cold_source.is_some()
                    && output
                        .len()
                        .checked_add(wrapped.len())
                        .is_none_or(|n| n > Self::MAX_ROWS)
                {
                    self.index_budget_exhausted
                        .store(true, std::sync::atomic::Ordering::Release);
                }
                anyhow::ensure!(
                    output
                        .len()
                        .checked_add(wrapped.len())
                        .is_some_and(|n| n <= Self::MAX_ROWS),
                    "cold visual index output row limit"
                );
                groups.push((
                    (context_first + start as StableRowIndex)
                        ..(context_first + end as StableRowIndex),
                    output.len()..output.len() + wrapped.len(),
                ));
                output.extend(wrapped);
                start = end;
            }
            let visual_count = output.len() - self.resident.len();
            let visual_first = if cold_source.is_some() {
                index_source_end
                    .checked_sub(StableRowIndex::try_from(visual_count)?)
                    .ok_or_else(|| anyhow::anyhow!("cold visual coordinate overflow"))?
            } else {
                context_first
            };
            self.layout = cold_source.as_ref().map(|_| {
                Arc::new(ColdVisualLayout {
                    kind: ColdVisualLayoutKind::Canonical,
                    source: context_first..index_source_end,
                    visual: visual_first..index_source_end,
                    resident_frontier: self.hot_top,
                    groups: groups
                        .into_iter()
                        .map(|(source, visual)| {
                            (
                                source,
                                (visual_first + visual.start as StableRowIndex)
                                    ..(visual_first + visual.end as StableRowIndex),
                            )
                        })
                        .collect(),
                    witness: self.witness.clone(),
                    interval: interval.clone(),
                })
            });
            let requested_rows = self.end.saturating_sub(self.first);
            self.first = self.first.max(visual_first);
            if cold_source.is_some() && self.resident.is_empty() {
                // Match streamed/geometry indexing: an oldest-row request
                // predates the new visual origin. Preserve its count within
                // the indexed cold prefix instead of clamping it to empty.
                self.end = self
                    .first
                    .checked_add(requested_rows)
                    .ok_or_else(|| anyhow::anyhow!("cold read range overflow"))?
                    .min(index_source_end);
                self.resident_first = self.end;
                anyhow::ensure!(self.first <= self.end, ColdReadGeometryUnavailable);
            } else {
                // Mixed captures must keep their exact resident endpoint.
                self.end = self.end.max(self.first);
            }
            let skip = (self.first - visual_first) as usize;
            for line in &mut output {
                anyhow::ensure!(!cancelled(), "cold read cancelled");
                let _ = line.cells_mut_for_attr_changes_only();
                charge.serialize_line(line)?;
            }
            let visible: Vec<Line> = output
                .iter()
                .skip(skip)
                .take(self.requested_row_count())
                .cloned()
                .collect();
            anyhow::ensure!(
                visible.len() == self.requested_row_count(),
                "cold reflow incomplete viewport"
            );
            self.logical_view = Some((visual_first, output));
            self.rendered = Some(visible);
        }
        anyhow::ensure!(!cancelled(), "cold read cancelled");
        self.payload_bytes = charge.used;
        self.complete = true;
        Ok(self)
    }

    pub fn lines(&self) -> impl Iterator<Item = &Line> {
        let (first, rest) = match &self.rendered {
            Some(lines) => (lines.as_slice(), &[][..]),
            None => (self.hydrated.as_slice(), self.resident.as_slice()),
        };
        first.iter().chain(rest)
    }
}

/// Holds the model of a screen. This can either be the primary screen,
/// including scrollback text, or the alternate screen without scrollback.
#[derive(Debug, Clone)]
pub struct Screen {
    coordinate_identity: ScreenCoordinateIdentity,
    selection_anchors: SelectionAnchorRegistry,
    #[cfg(feature = "use_serde")]
    cold_visual_layout: Option<Arc<ColdVisualLayout>>,
    #[cfg(feature = "use_serde")]
    stored_physical_layout: Option<StoredPhysicalLayout>,
    #[cfg(feature = "use_serde")]
    cold_geometry_index: Option<ColdGeometryIndex>,
    #[cfg(feature = "use_serde")]
    pub(crate) cold_row_fragments: Option<Arc<ColdRowFragments>>,
    #[cfg(feature = "use_serde")]
    cold_visual_seqno: SequenceNo,
    #[cfg(feature = "use_serde")]
    cold_source_observation: Option<(
        Arc<dyn crate::config::ScrollbackSpillSink>,
        crate::config::ScrollbackInterval,
    )>,
    #[cfg(feature = "use_serde")]
    cold_index_budget_exhausted: Arc<std::sync::atomic::AtomicBool>,
    /// Holds the line data that comprises the screen contents.
    /// This is allocated with capacity for the entire scrollback.
    /// The last N lines are the visible lines, with those prior being
    /// the lines that have scrolled off the top of the screen.
    /// Index 0 is the topmost line of the screen/scrollback (depending
    /// on the current window size) and will be the first line to be
    /// popped off the front of the screen when a new line is added that
    /// would otherwise have exceeded the line capacity
    lines: VecDeque<Line>,

    /// Whenever we scroll a line off the top of the scrollback, we
    /// increment this.  We use this offset to translate between
    /// PhysRowIndex and StableRowIndex.
    stable_row_index_offset: usize,

    /// config so we can access Maximum number of lines of scrollback
    config: Arc<dyn TerminalConfiguration>,
    scrollback_tiering: ScrollbackTieringState,
    /// Authenticated cold-store identity and original half-open prefix boundary
    /// retained while a checkpoint is restored and raw output is replayed off
    /// topology. Its presence also prevents replay from evicting any resident
    /// recovery row before checked activation can repartition the history.
    recovery_scrollback: Option<RecoveryScrollbackBoundary>,

    /// Whether scrollback is allowed; this is another way of saying
    /// that we're the primary rather than the alternate screen.
    allow_scrollback: bool,

    pub(crate) keyboard_stack: Vec<KeyboardEncoding>,

    /// Physical, visible height of the screen (not including scrollback)
    pub physical_rows: usize,
    /// Physical, visible width of the screen
    pub physical_cols: usize,
    pub dpi: u32,

    pub(crate) saved_cursor: Option<SavedCursor>,
    rewrap_cache: Option<Arc<LogicalLineWrapCache>>,
    rewrap_line_cache: HashMap<WrapLineCacheKey, CachedWrappedLine>,
    rewrap_line_cache_order: VecDeque<WrapLineCacheKey>,
    rewrap_scratch_slots: Vec<Option<RewrapScratch>>,
    rewrap_row_prefix_scratch: Vec<usize>,
    rewrap_width_prefix_scratch: LineWrapWidthPrefixScratch,
    cold_scrollback_worker: ColdScrollbackReflowWorker,
    last_viewport_first_reflow_us: u64,
    resize_wrap_policy: ResizeWrapPolicy,
    last_resize_wrap_scorecard: Option<ResizeWrapScorecard>,
    last_resize_wrap_gate_payload: Option<String>,
    cursor_consistency_telemetry: CursorConsistencyTelemetry,
    last_good_frame: Option<LastGoodFrame>,
    last_good_frame_lifecycle: LastGoodFrameLifecycle,
    #[cfg(test)]
    rewrap_line_cache_hits: usize,
    #[cfg(test)]
    forced_rollback_cause: Option<LastGoodFrameRollbackCause>,
}

#[derive(Clone, Copy, Debug)]
struct RecoveryScrollbackBoundary {
    #[cfg_attr(not(feature = "use_serde"), allow(dead_code))]
    expected_generation: Option<ScrollbackSnapshotGeneration>,
    original_cold_prefix_newest_exclusive: StableRowIndex,
}

/// Semantic screen fields retained by the guardian checkpoint codec.
///
/// Caches, workers, telemetry, and configuration capabilities are rebuilt by
/// `Screen::new` during restore and are intentionally absent here.
#[cfg(feature = "use_serde")]
pub(crate) struct ScreenCheckpointParts {
    pub lines: Vec<Line>,
    pub stable_row_index_offset: usize,
    pub cold_snapshot_generation: Option<ScrollbackSnapshotGeneration>,
    pub cold_prefix_line_count: usize,
    pub allow_scrollback: bool,
    pub keyboard_stack: Vec<KeyboardEncoding>,
    pub physical_rows: usize,
    pub physical_cols: usize,
    pub dpi: u32,
    pub saved_cursor: Option<SavedCursor>,
}

#[cfg(feature = "use_serde")]
#[derive(Clone, Debug)]
pub(crate) struct StagedColdSource {
    pub(crate) sink: Arc<dyn crate::config::ScrollbackSpillSink>,
    pub(crate) before_interval: crate::config::ScrollbackInterval,
    pub(crate) expected_newest_exclusive: StableRowIndex,
    pub(crate) max_cold_bytes: u64,
    fragments: Option<Arc<ColdRowFragments>>,
}

#[cfg(feature = "use_serde")]
#[derive(Clone, Debug)]
pub(crate) struct StagedScreenCheckpoint {
    pub(crate) resident_lines: Vec<Line>,
    pub(crate) resident_oldest: usize,
    pub(crate) cold_snapshot_generation: Option<ScrollbackSnapshotGeneration>,
    pub(crate) cold_prefix_line_count: usize,
    pub(crate) allow_scrollback: bool,
    pub(crate) keyboard_stack: Vec<KeyboardEncoding>,
    pub(crate) physical_rows: usize,
    pub(crate) physical_cols: usize,
    pub(crate) dpi: u32,
    pub(crate) saved_cursor: Option<SavedCursor>,
    pub(crate) cold_source: Option<StagedColdSource>,
}

#[cfg(feature = "use_serde")]
#[derive(Clone, Copy, Debug)]
pub(crate) struct ScreenCheckpointLimits {
    pub max_total_lines: usize,
    pub max_total_cell_records: usize,
    pub max_total_cells: usize,
    pub max_total_cell_text_bytes: usize,
    pub max_total_hyperlink_bytes: usize,
    pub max_total_hyperlink_params: usize,
    pub max_string_bytes: usize,
    pub max_hyperlink_params_per_link: usize,
    pub max_cold_scrollback_bytes: usize,
    pub max_keyboard_stack_depth: usize,
    pub max_rows: usize,
    pub max_cols: usize,
    pub max_visible_grid_cells: usize,
    pub max_retained_capture_bytes: usize,
    pub estimated_bytes_per_line: usize,
    pub estimated_bytes_per_cell: usize,
}

#[cfg(feature = "use_serde")]
#[derive(Default, Debug)]
pub(crate) struct ScreenCheckpointUsage {
    pub lines: usize,
    pub cell_records: usize,
    pub cells: usize,
    pub cell_text_bytes: usize,
    pub hyperlink_bytes: usize,
    pub hyperlink_params: usize,
    pub retained_capture_bytes: usize,
}

#[cfg(feature = "use_serde")]
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ScreenCheckpointCaptureError {
    ResourceLimit {
        resource: &'static str,
        observed: usize,
        maximum: usize,
    },
    ArithmeticOverflow(&'static str),
    ResourceAllocation(&'static str),
    InvalidLineGeometry,
    UnsupportedGraphicsState,
    ColdScrollbackMetadataInconsistent,
    ColdScrollbackSnapshot(ScrollbackSpillError),
    ColdScrollbackNotRecoveryGrade,
}

#[cfg_attr(feature = "use_serde", derive(Deserialize, Serialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TieredScrollbackStatus {
    pub tiering_enabled: bool,
    pub configured_scrollback_rows: usize,
    pub configured_hot_lines: usize,
    pub configured_warm_max_bytes: usize,
    pub visible_rows: usize,
    pub in_memory_scrollback_rows: usize,
    pub warm_resident_lines: usize,
    pub warm_resident_bytes: usize,
    pub warm_spill_lines_total: u64,
    pub warm_spill_bytes_total: u64,
    pub cold_spill_lines_total: u64,
    pub cold_spill_bytes_total: u64,
    pub cold_sink_retained_lines: usize,
    pub cold_sink_retained_bytes: usize,
    pub cold_worker_peak_backlog_depth: usize,
    pub cold_worker_completion_throughput_lines_per_sec: u64,
    pub cold_worker_completed_lines_total: u64,
    pub cold_worker_completed_batches_total: u64,
    pub cold_worker_cancellation_count: u64,
}

const MAX_WRAP_CACHE_ENTRIES: usize = 6;
const MAX_WRAP_LINE_CACHE_ENTRIES: usize = 16_384;
const FT_OSYAF_PERSISTENT_REFLOW_CHUNKS: bool = true;
const MAX_REFLOW_BATCH_LOGICAL_LINES: usize = 64;
const REFLOW_OVERSCAN_ROW_MULTIPLIER: usize = 1;
const REFLOW_OVERSCAN_ROW_CAP: usize = 256;
const COLD_SCROLLBACK_BACKLOG_DEPTH_CAP: usize = 1_048_576;
const SCROLLBACK_WARM_MAX_BYTES_CAP: usize = 1024 * 1024 * 1024;
const LAST_GOOD_FRAME_MAX_BYTES_MULTIPLIER: usize = 4;
const MOONSHOT_RECOMMENDED_ENV: &str = "FT_MOONSHOT_RECOMMENDED";
const ASCII_CLUSTER_RUN_APPEND_ENV: &str = "FT_MOONSHOT_TERM_ASCII_CLUSTER_RUN_APPEND";

#[cfg(test)]
static ASCII_CLUSTER_RUN_APPEND_TEST_OVERRIDE: std::sync::atomic::AtomicU8 =
    std::sync::atomic::AtomicU8::new(0);

#[cfg(test)]
static ASCII_CLUSTER_RUN_APPEND_HITS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
pub(crate) fn set_ascii_cluster_run_append_test_override(force: Option<bool>) {
    ASCII_CLUSTER_RUN_APPEND_TEST_OVERRIDE.store(
        match force {
            Some(false) => 1,
            Some(true) => 2,
            None => 0,
        },
        std::sync::atomic::Ordering::Relaxed,
    );
}

#[cfg(test)]
pub(crate) fn reset_ascii_cluster_run_append_hits() {
    ASCII_CLUSTER_RUN_APPEND_HITS.store(0, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) fn ascii_cluster_run_append_hits() -> usize {
    ASCII_CLUSTER_RUN_APPEND_HITS.load(std::sync::atomic::Ordering::Relaxed)
}

fn moonshot_env_falsey(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            let value = value.trim();
            value.is_empty()
                || value == "0"
                || value.eq_ignore_ascii_case("false")
                || value.eq_ignore_ascii_case("off")
                || value.eq_ignore_ascii_case("no")
        })
        .unwrap_or(false)
}

fn moonshot_recommended_enabled() -> bool {
    !moonshot_env_falsey(MOONSHOT_RECOMMENDED_ENV)
}

fn ascii_cluster_run_append_enabled() -> bool {
    #[cfg(test)]
    match ASCII_CLUSTER_RUN_APPEND_TEST_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed) {
        1 => return false,
        2 => return true,
        _ => {}
    }

    static ENABLED: LazyLock<bool> = LazyLock::new(|| {
        // Round-7 promotion: this flag is part of the recommended dense-ASCII
        // term-render stack and is default-on, with both set-wide and per-flag
        // falsey escape hatches.
        moonshot_recommended_enabled() && !moonshot_env_falsey(ASCII_CLUSTER_RUN_APPEND_ENV)
    });
    *ENABLED
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LastGoodFrameTransition {
    ResizeBegin,
    ResizeCommit,
    ContentMutation,
    ScrollbackErase,
}

impl LastGoodFrameTransition {
    fn as_str(self) -> &'static str {
        match self {
            Self::ResizeBegin => "resize_begin",
            Self::ResizeCommit => "resize_commit",
            Self::ContentMutation => "content_mutation",
            Self::ScrollbackErase => "scrollback_erase",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LastGoodFrameRollbackCause {
    ResizeCommitValidation,
    #[cfg(test)]
    ForcedFailureInjection,
}

impl LastGoodFrameRollbackCause {
    fn as_str(self) -> &'static str {
        match self {
            Self::ResizeCommitValidation => "resize_commit_validation",
            #[cfg(test)]
            Self::ForcedFailureInjection => "forced_failure_injection",
        }
    }
}

#[derive(Debug, Clone)]
struct LastGoodFrame {
    visible_lines: Vec<Line>,
    cols: usize,
    rows: usize,
    dpi: u32,
    layout_signature: u64,
    captured_seqno: SequenceNo,
    estimated_bytes: usize,
    lineage_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct LastGoodFrameLifecycle {
    capture_count: u64,
    invalidation_count: u64,
    drop_over_budget_count: u64,
    rollback_count: u64,
    rollback_missing_snapshot_count: u64,
    current_retained_bytes: usize,
    peak_retained_bytes: usize,
    last_budget_bytes: usize,
    last_lineage_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResizeReadabilityGatePolicy {
    pub enabled: bool,
    pub max_line_badness_delta: i64,
    pub max_total_badness_delta: i64,
    pub max_fallback_ratio_percent: u8,
}

impl Default for ResizeReadabilityGatePolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            max_line_badness_delta: 500,
            max_total_badness_delta: 2000,
            max_fallback_ratio_percent: 20,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResizeWrapGateFailureReason {
    LineBadnessDeltaExceeded,
    TotalBadnessDeltaExceeded,
    FallbackRatioExceeded,
}

impl ResizeWrapGateFailureReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::LineBadnessDeltaExceeded => "line_badness_delta_exceeded",
            Self::TotalBadnessDeltaExceeded => "total_badness_delta_exceeded",
            Self::FallbackRatioExceeded => "fallback_ratio_exceeded",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResizeWrapGateStatus {
    Disabled,
    Pass,
    Fail(ResizeWrapGateFailureReason),
}

impl ResizeWrapGateStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Pass => "pass",
            Self::Fail(_) => "fail",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResizeWrapPolicy {
    pub kp_cost_model: MonospaceKpCostModel,
    pub scorecard_enabled: bool,
    pub readability_gate: ResizeReadabilityGatePolicy,
}

impl Default for ResizeWrapPolicy {
    fn default() -> Self {
        Self {
            kp_cost_model: MonospaceKpCostModel::terminal_default(),
            scorecard_enabled: true,
            readability_gate: ResizeReadabilityGatePolicy::default(),
        }
    }
}

impl ResizeWrapPolicy {
    pub(crate) fn from_terminal_configuration(config: &dyn TerminalConfiguration) -> Self {
        Self {
            kp_cost_model: config.resize_wrap_kp_cost_model(),
            scorecard_enabled: config.resize_wrap_scorecard_enabled(),
            readability_gate: ResizeReadabilityGatePolicy {
                enabled: config.resize_wrap_readability_gate_enabled(),
                max_line_badness_delta: config.resize_wrap_readability_max_line_badness_delta(),
                max_total_badness_delta: config.resize_wrap_readability_max_total_badness_delta(),
                max_fallback_ratio_percent: config
                    .resize_wrap_readability_max_fallback_ratio_percent()
                    .min(100),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ResizeWrapScorecard {
    pub scored_lines: usize,
    pub dp_lines: usize,
    pub fallback_lines: usize,
    pub greedy_total_cost: u64,
    pub selected_total_cost: u64,
    pub max_badness_delta: i64,
    pub total_badness_delta: i64,
}

impl ResizeWrapScorecard {
    fn record_line(&mut self, scorecard: MonospaceLineWrapScorecard) {
        self.scored_lines = self.scored_lines.saturating_add(1);
        match scorecard.mode {
            MonospaceWrapMode::Dp => {
                self.dp_lines = self.dp_lines.saturating_add(1);
            }
            MonospaceWrapMode::Fallback => {
                self.fallback_lines = self.fallback_lines.saturating_add(1);
            }
        }
        self.greedy_total_cost = self
            .greedy_total_cost
            .saturating_add(scorecard.greedy_total_cost);
        self.selected_total_cost = self
            .selected_total_cost
            .saturating_add(scorecard.selected_total_cost);
        self.max_badness_delta = self.max_badness_delta.max(scorecard.badness_delta);
        self.total_badness_delta = self
            .total_badness_delta
            .saturating_add(scorecard.badness_delta);
    }

    fn fallback_ratio_percent(&self) -> usize {
        if self.scored_lines == 0 {
            return 0;
        }
        self.fallback_lines.saturating_mul(100) / self.scored_lines
    }

    fn gate_status(&self, policy: ResizeReadabilityGatePolicy) -> ResizeWrapGateStatus {
        if !policy.enabled {
            return ResizeWrapGateStatus::Disabled;
        }
        if self.max_badness_delta > policy.max_line_badness_delta {
            return ResizeWrapGateStatus::Fail(
                ResizeWrapGateFailureReason::LineBadnessDeltaExceeded,
            );
        }
        if self.total_badness_delta > policy.max_total_badness_delta {
            return ResizeWrapGateStatus::Fail(
                ResizeWrapGateFailureReason::TotalBadnessDeltaExceeded,
            );
        }
        if self.fallback_ratio_percent() > usize::from(policy.max_fallback_ratio_percent) {
            return ResizeWrapGateStatus::Fail(ResizeWrapGateFailureReason::FallbackRatioExceeded);
        }
        ResizeWrapGateStatus::Pass
    }

    fn to_machine_payload(&self, policy: ResizeReadabilityGatePolicy) -> String {
        let status = self.gate_status(policy);
        let reason = match status {
            ResizeWrapGateStatus::Fail(reason) => {
                format!("\"{}\"", reason.as_str())
            }
            _ => "null".to_string(),
        };

        format!(
            "{{\"gate\":\"resize_wrap_readability\",\"status\":\"{}\",\"reason\":{},\"scored_lines\":{},\"dp_lines\":{},\"fallback_lines\":{},\"fallback_ratio_percent\":{},\"max_badness_delta\":{},\"total_badness_delta\":{},\"greedy_total_cost\":{},\"selected_total_cost\":{},\"policy\":{{\"enabled\":{},\"max_line_badness_delta\":{},\"max_total_badness_delta\":{},\"max_fallback_ratio_percent\":{}}}}}",
            status.as_str(),
            reason,
            self.scored_lines,
            self.dp_lines,
            self.fallback_lines,
            self.fallback_ratio_percent(),
            self.max_badness_delta,
            self.total_badness_delta,
            self.greedy_total_cost,
            self.selected_total_cost,
            if policy.enabled { "true" } else { "false" },
            policy.max_line_badness_delta,
            policy.max_total_badness_delta,
            policy.max_fallback_ratio_percent
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReflowBatchPriority {
    Viewport,
    NearViewport,
    ColdScrollback,
}

impl ReflowBatchPriority {
    fn as_str(self) -> &'static str {
        match self {
            Self::Viewport => "viewport",
            Self::NearViewport => "near_viewport",
            Self::ColdScrollback => "cold_scrollback",
        }
    }

    fn rationale(self) -> &'static str {
        match self {
            Self::Viewport => "intersects visible viewport",
            Self::NearViewport => "inside overscan window",
            Self::ColdScrollback => "outside overscan window",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReflowBatchPlan {
    logical_range: Range<usize>,
    priority: ReflowBatchPriority,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct ViewportReflowPlan {
    batches: Vec<ReflowBatchPlan>,
}

impl ViewportReflowPlan {
    fn full_scan(logical_count: usize) -> Self {
        let mut batches = Vec::new();
        let mut start = 0usize;
        while start < logical_count {
            let end = (start + MAX_REFLOW_BATCH_LOGICAL_LINES).min(logical_count);
            batches.push(ReflowBatchPlan {
                logical_range: start..end,
                priority: ReflowBatchPriority::ColdScrollback,
            });
            start = end;
        }
        Self { batches }
    }

    fn covers_each_logical_line_once(&self, logical_count: usize) -> bool {
        if logical_count == 0 {
            return self.batches.is_empty();
        }
        let mut coverage = vec![0u8; logical_count];
        for batch in &self.batches {
            if batch.logical_range.end > logical_count
                || batch.logical_range.start >= batch.logical_range.end
            {
                return false;
            }
            for idx in batch.logical_range.clone() {
                coverage[idx] = coverage[idx].saturating_add(1);
            }
        }
        coverage.into_iter().all(|count| count == 1)
    }
}

fn ranges_intersect(lhs: &Range<usize>, rhs: &Range<usize>) -> bool {
    lhs.start < rhs.end && rhs.start < lhs.end
}

fn duration_micros_u64(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct WrapCacheKey {
    physical_cols: usize,
    dpi: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct WrapLineCacheKey {
    physical_cols: usize,
    dpi: u32,
    line_len: usize,
    line_shape_hash: [u8; 16],
    badness_scale: u64,
    forced_break_penalty: u64,
    lookahead_limit: usize,
    max_dp_states: usize,
    scorecard_enabled: bool,
}

impl WrapLineCacheKey {
    fn new(line: &Line, physical_cols: usize, dpi: u32, policy: ResizeWrapPolicy) -> Self {
        Self {
            physical_cols,
            dpi,
            line_len: line.len(),
            line_shape_hash: line.compute_shape_hash(),
            badness_scale: policy.kp_cost_model.badness_scale,
            forced_break_penalty: policy.kp_cost_model.forced_break_penalty,
            lookahead_limit: policy.kp_cost_model.lookahead_limit,
            max_dp_states: policy.kp_cost_model.max_dp_states,
            scorecard_enabled: policy.scorecard_enabled,
        }
    }
}

#[derive(Debug, Clone)]
struct CachedWrappedLine {
    lines: Arc<[Line]>,
    scorecard: Option<MonospaceLineWrapScorecard>,
}

#[derive(Debug, Clone)]
struct CachedResizeLines {
    lines: Arc<[Arc<[Line]>]>,
    row_prefix: Arc<std::sync::OnceLock<Arc<[usize]>>>,
    has_images: bool,
    scorecard: Option<ResizeWrapScorecard>,
    gate_payload: Option<String>,
}

impl CachedResizeLines {
    fn row_prefix(&self) -> &Arc<[usize]> {
        self.row_prefix.get_or_init(|| {
            // Counts belong to these immutable chunks, independent of cell
            // seqnos, hyperlink scan state and mutable image payloads.
            #[cfg(test)]
            REFLOW_ROW_PREFIX_BUILDS.with(|count| count.set(count.get() + 1));
            let mut prefix = Vec::with_capacity(self.lines.len().saturating_add(1));
            prefix.push(0);
            let mut total_rows = 0usize;
            for lines in self.lines.iter() {
                total_rows = total_rows.saturating_add(lines.len());
                prefix.push(total_rows);
            }
            Arc::from(prefix)
        })
    }
}

#[derive(Debug, Clone)]
struct LogicalLineWrapCache {
    // Cold preparation relies on exact source-row validation at publication.
    // Until then, an unsigned cache cannot authorize a generic cache lookup.
    // Images and direct source reconstruction retain the fresh-hash path.
    source_signature: Option<u64>,
    // At most one published target, sharing immutable chunks from the most
    // recent layout. Reuse requires exact ordered content equality, not a hash
    // or the publication seqno. Pruning must discard this witness entirely.
    published_target: Option<Arc<[Arc<[Line]>]>>,
    logical_lines: Arc<[CachedLogicalLine]>,
    wrapped_by_key: HashMap<WrapCacheKey, CachedResizeLines>,
    wrap_key_order: VecDeque<WrapCacheKey>,
}

#[derive(Debug, Clone)]
struct CachedLogicalLine {
    line: Line,
    // Shared across preparation snapshots. Content extraction happens once on
    // a compute worker; width changes reuse the immutable token allocation.
    wrap_source: Option<Arc<std::sync::OnceLock<LineWrapLayout>>>,
}

// Bound retained token, width-prefix and break-array slots while physical-row storage still
// coexists with logical content. This is not a bound on the entire terminal or
// externally shared image/attribute payloads. Prefer recent history.
const MAX_RETAINED_WRAP_TOKEN_BYTES: usize = 64 * 1024 * 1024;

fn retained_wrap_token_budget() -> usize {
    static BUDGET: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *BUDGET.get_or_init(|| {
        if std::env::var_os("FT_DISABLE_RETAINED_WRAP_TOKENS").is_some_and(|value| value == "1") {
            0
        } else {
            MAX_RETAINED_WRAP_TOKEN_BYTES
        }
    })
}

/// Owned wrap work. This carries no authority to replace terminal state.
/// Capture is performed under the terminal lock; `prepare` runs without it.
pub struct ScreenReflowPreparation {
    snapshot: Screen,
    source_lines: VecDeque<Line>,
    source_cursor: CursorPosition,
    source_logical_cursor: Option<(usize, usize)>,
    source_dpi: u32,
    target: TerminalSize,
    ready: bool,
    applied: bool,
}

impl ScreenReflowPreparation {
    pub fn was_applied(&self) -> bool {
        self.applied
    }

    fn logical_cursor_for(&self, cursor: CursorPosition) -> Option<Option<(usize, usize)>> {
        // The row controls trailing-blank pruning. Same-row cursor movement
        // changes only the offset, while visibility/style/sequence do not
        // change wrap geometry. Exact line validation remains mandatory.
        if cursor.y != self.source_cursor.y || cursor.seqno < self.source_cursor.seqno {
            return None;
        }
        // An absent physical-to-logical mapping is valid in the existing
        // height/ConPTY resize path. Preserve it only for unchanged coordinates;
        // the outer None instead means this preparation cannot be reused.
        let Some((group, column)) = self.source_logical_cursor else {
            return (cursor.x == self.source_cursor.x).then_some(None);
        };
        let prefix = column.checked_sub(self.source_cursor.x)?;
        Some(Some((group, prefix.checked_add(cursor.x)?)))
    }

    pub fn prepare(&mut self, is_cancelled: impl Fn() -> bool) -> bool {
        self.ready = false;
        self.applied = false;
        // Image payloads are internally mutable and cannot be frozen by Line's
        // copy-on-write cells. Keep those screens on the synchronous path.
        if is_cancelled() || self.snapshot.lines.iter().any(Line::has_image_attachments) {
            return false;
        }
        self.source_lines = self.snapshot.lines.clone();
        // Pruning only removes rows after the cursor. Compute its logical
        // prefix on the immutable worker source before that pruning, then
        // reuse it only under the same exact-source authority as the wraps.
        self.source_logical_cursor = self.snapshot.logical_cursor_from_physical(
            self.source_cursor.x,
            self.snapshot.phys_row(self.source_cursor.y),
        );
        self.snapshot
            .prune_resize_trailing_blanks(self.source_cursor);
        if self.target.dpi != self.snapshot.dpi {
            if let Some(cache) = self.snapshot.rewrap_cache.as_mut() {
                Arc::make_mut(cache).clear_wraps();
            }
            self.snapshot.dpi = self.target.dpi;
        }
        self.ready = self
            .snapshot
            .logical_wraps_for_resize(
                self.target.cols.max(1),
                self.source_cursor.seqno,
                false,
                &is_cancelled,
            )
            .is_some();
        if !self.ready || is_cancelled() {
            self.ready = false;
            return false;
        }
        let cache = Arc::make_mut(self.snapshot.rewrap_cache.as_mut().unwrap());
        let key = WrapCacheKey {
            physical_cols: self.target.cols.max(1),
            dpi: self.target.dpi,
        };
        if let Some(wrapped) = cache.wrapped_by_key.get_mut(&key) {
            // Populate once on this worker. A direct uncached resize keeps
            // using its existing scratch prefix, without paying a second pass.
            wrapped.row_prefix();
        }
        self.ready = !is_cancelled();
        self.ready
    }
}

enum WrappedResizeLines {
    Cached(CachedResizeLines),
    Scratch { logical_count: usize },
}

/// A target selected only after exact live-source validation in the resize
/// transaction. It does not grant authority across another terminal mutation.
struct VerifiedPreparedResize {
    wrapped: CachedResizeLines,
    logical_count: usize,
    cache_entries: usize,
    logical_cursor: Option<(usize, usize)>,
}

#[derive(Debug, Clone)]
enum LogicalLineForResize {
    PhysicalLine(usize),
    PhysicalRange {
        range: Range<usize>,
        seqno: SequenceNo,
        logical: std::sync::OnceLock<Line>,
    },
    Owned(Line),
}

#[derive(Debug, Clone)]
struct LogicalLineRebuild {
    logical_lines: Vec<LogicalLineForResize>,
    physical_ranges: Vec<Range<usize>>,
}

#[derive(Debug, Clone)]
enum RewrapScratch {
    PhysicalLine(usize),
    Lines(Vec<Line>),
    SharedLines(Arc<[Line]>),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ScrollbackSpillOutcome {
    cold_lines_evicted: usize,
    cold_bytes_evicted: usize,
}

#[derive(Debug, Clone, Default)]
struct ScrollbackTieringState {
    warm_line_bytes: VecDeque<usize>,
    warm_bytes: usize,
    warm_spill_lines_total: u64,
    warm_spill_bytes_total: u64,
    cold_spill_lines_total: u64,
    cold_spill_bytes_total: u64,
}

#[derive(Debug, Clone, Default)]
struct ColdScrollbackReflowWorker {
    active_intent: Option<SequenceNo>,
    backlog_depth: usize,
    peak_backlog_depth: usize,
    completion_throughput_lines_per_sec: u64,
    completed_lines_total: u64,
    completed_batches_total: u64,
    cancellation_count: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct CursorConsistencyTelemetry {
    checks_passed: u64,
    checks_failed: u64,
}

impl CursorConsistencyTelemetry {
    fn record(&mut self, passed: bool) {
        if passed {
            self.checks_passed = self.checks_passed.saturating_add(1);
        } else {
            self.checks_failed = self.checks_failed.saturating_add(1);
        }
    }

    fn total_checks(&self) -> u64 {
        self.checks_passed.saturating_add(self.checks_failed)
    }
}

impl ColdScrollbackReflowWorker {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn begin_intent(&mut self, seqno: SequenceNo, backlog_depth: usize) {
        if let Some(active_intent) = self.active_intent {
            if active_intent != seqno && self.backlog_depth > 0 {
                self.cancellation_count = self.cancellation_count.saturating_add(1);
            }
        }

        self.active_intent = Some(seqno);
        self.backlog_depth = backlog_depth.min(COLD_SCROLLBACK_BACKLOG_DEPTH_CAP);
        self.peak_backlog_depth = self.peak_backlog_depth.max(self.backlog_depth);
    }

    fn complete_cold_batch(&mut self, seqno: SequenceNo, batch_lines: usize) {
        if self.active_intent != Some(seqno) {
            return;
        }
        self.backlog_depth = self.backlog_depth.saturating_sub(batch_lines);
        self.completed_lines_total = self
            .completed_lines_total
            .saturating_add(batch_lines as u64);
        self.completed_batches_total = self.completed_batches_total.saturating_add(1);
    }

    fn finish_intent(
        &mut self,
        seqno: SequenceNo,
        elapsed: std::time::Duration,
        completed_lines_for_intent: usize,
    ) {
        if self.active_intent != Some(seqno) {
            return;
        }

        self.completion_throughput_lines_per_sec = if completed_lines_for_intent == 0 {
            0
        } else {
            let elapsed_nanos = elapsed.as_nanos().max(1);
            let rate =
                (completed_lines_for_intent as u128).saturating_mul(1_000_000_000) / elapsed_nanos;
            rate.min(u64::MAX as u128) as u64
        };
        self.active_intent = None;
        self.backlog_depth = 0;
    }

    #[cfg(test)]
    fn backlog_depth(&self) -> usize {
        self.backlog_depth
    }

    fn peak_backlog_depth(&self) -> usize {
        self.peak_backlog_depth
    }

    fn completion_throughput_lines_per_sec(&self) -> u64 {
        self.completion_throughput_lines_per_sec
    }

    #[cfg(test)]
    fn completed_lines_total(&self) -> u64 {
        self.completed_lines_total
    }

    #[cfg(test)]
    fn completed_batches_total(&self) -> u64 {
        self.completed_batches_total
    }

    fn cancellation_count(&self) -> u64 {
        self.cancellation_count
    }

    #[cfg(test)]
    fn active_intent(&self) -> Option<SequenceNo> {
        self.active_intent
    }
}

impl LogicalLineWrapCache {
    fn new(source_signature: Option<u64>, logical_lines: Vec<Line>) -> Self {
        Self::with_token_budget(
            source_signature,
            logical_lines,
            retained_wrap_token_budget(),
        )
    }

    fn with_token_budget(
        source_signature: Option<u64>,
        logical_lines: Vec<Line>,
        mut remaining: usize,
    ) -> Self {
        let mut logical_lines: Vec<_> = logical_lines
            .into_iter()
            .rev()
            .map(|line| {
                let bytes = line
                    .len()
                    .checked_mul(
                        std::mem::size_of::<Cell>()
                            + 2 * std::mem::size_of::<u128>()
                            + std::mem::size_of::<bool>(),
                    )
                    // Retained plans own source cells, cumulative widths,
                    // word widths, space flags and a growable break vector.
                    .and_then(|bytes| {
                        line.len()
                            .checked_mul(2)?
                            .max(8)
                            .checked_mul(std::mem::size_of::<usize>())?
                            .checked_add(bytes)
                    })
                    .and_then(|bytes| {
                        bytes.checked_add(
                            std::mem::size_of::<std::sync::OnceLock<LineWrapLayout>>()
                                + std::mem::size_of::<LineWrapWidthPrefixScratch>()
                                + std::mem::size_of::<u128>()
                                // Arc headers for source cells, prefix and
                                // the lazily initialized retained layout.
                                + 6 * std::mem::size_of::<usize>(),
                        )
                    });
                let wrap_source = bytes.filter(|bytes| *bytes <= remaining).map(|bytes| {
                    remaining -= bytes;
                    Arc::new(std::sync::OnceLock::new())
                });
                CachedLogicalLine { line, wrap_source }
            })
            .collect();
        logical_lines.reverse();
        Self {
            source_signature,
            published_target: None,
            logical_lines: Arc::from(logical_lines),
            wrapped_by_key: HashMap::new(),
            wrap_key_order: VecDeque::new(),
        }
    }

    fn matches_published_target(&self, current: &VecDeque<Line>) -> bool {
        self.published_target.as_ref().is_some_and(|chunks| {
            let mut published = chunks.iter().flat_map(|chunk| chunk.iter());
            current.iter().all(|line| {
                published.next().is_some_and(|before| {
                    (reuse_unlinked_scan_state_for_reflow()
                        || line.implicit_hyperlinks_are_scanned()
                            == before.implicit_hyperlinks_are_scanned())
                        && line.is_same_reflow_content(before)
                })
            }) && published.next().is_none()
        })
    }

    fn touch_key(&mut self, key: WrapCacheKey) {
        if let Some(idx) = self.wrap_key_order.iter().position(|k| *k == key) {
            self.wrap_key_order.remove(idx);
        }
        self.wrap_key_order.push_back(key);
    }

    fn get_wrapped(&mut self, key: WrapCacheKey) -> Option<CachedResizeLines> {
        let wrapped = self.wrapped_by_key.get(&key).cloned();
        if wrapped.is_some() {
            self.touch_key(key);
        }
        wrapped
    }

    fn insert_wrapped(
        &mut self,
        key: WrapCacheKey,
        wrapped: Vec<Arc<[Line]>>,
        scorecard: Option<ResizeWrapScorecard>,
        gate_payload: Option<String>,
    ) {
        if !self.wrapped_by_key.contains_key(&key)
            && self.wrapped_by_key.len() >= MAX_WRAP_CACHE_ENTRIES
        {
            if let Some(evicted) = self.wrap_key_order.pop_front() {
                self.wrapped_by_key.remove(&evicted);
            }
        }
        // ImageData may change behind an Arc without mutating a cached Line.
        // Its content-derived shape hash must continue to be computed fresh.
        let has_images = wrapped
            .iter()
            .flat_map(|chunk| chunk.iter())
            .any(Line::has_image_attachments);
        self.wrapped_by_key.insert(
            key,
            CachedResizeLines {
                lines: Arc::from(wrapped),
                row_prefix: Arc::new(std::sync::OnceLock::new()),
                has_images,
                scorecard,
                gate_payload,
            },
        );
        self.touch_key(key);
    }

    fn clear_wraps(&mut self) {
        self.wrapped_by_key.clear();
        self.wrap_key_order.clear();
    }
}

trait ReflowLogicalLine {
    fn line<'a>(&'a self, physical_lines: &'a VecDeque<Line>) -> &'a Line;

    fn clone_line(&self, physical_lines: &VecDeque<Line>) -> Line {
        self.line(physical_lines).clone()
    }

    fn scratch_for_unwrapped(&self, physical_lines: &VecDeque<Line>) -> RewrapScratch;

    fn retained_wrap_source(&self) -> Option<&std::sync::OnceLock<LineWrapLayout>> {
        None
    }
}

impl ReflowLogicalLine for CachedLogicalLine {
    fn line<'a>(&'a self, _physical_lines: &'a VecDeque<Line>) -> &'a Line {
        &self.line
    }

    fn scratch_for_unwrapped(&self, _physical_lines: &VecDeque<Line>) -> RewrapScratch {
        RewrapScratch::Lines(vec![self.line.clone()])
    }

    fn retained_wrap_source(&self) -> Option<&std::sync::OnceLock<LineWrapLayout>> {
        self.wrap_source.as_deref()
    }
}

impl ReflowLogicalLine for LogicalLineForResize {
    fn line<'a>(&'a self, physical_lines: &'a VecDeque<Line>) -> &'a Line {
        match self {
            Self::PhysicalLine(idx) => &physical_lines[*idx],
            Self::PhysicalRange {
                range,
                seqno,
                logical,
            } => logical.get_or_init(|| {
                let rows = physical_lines.range(range.clone());
                Line::try_compact_logical_rows(rows.clone(), *seqno).unwrap_or_else(|| {
                    // Images and the diagnostic eager control retain the
                    // original reconstruction semantics on the same worker.
                    let mut joined: Option<Line> = None;
                    for row in rows {
                        let mut row = row.clone();
                        row.update_last_change_seqno(*seqno);
                        if row.last_cell_was_wrapped() {
                            row.set_last_cell_was_wrapped(false, *seqno);
                        }
                        match &mut joined {
                            Some(line) => line.append_line(row, *seqno),
                            None => joined = Some(row),
                        }
                    }
                    joined.expect("a logical physical range is nonempty")
                })
            }),
            Self::Owned(line) => line,
        }
    }

    fn scratch_for_unwrapped(&self, physical_lines: &VecDeque<Line>) -> RewrapScratch {
        match self {
            Self::PhysicalLine(idx) => RewrapScratch::PhysicalLine(*idx),
            Self::PhysicalRange { .. } => {
                RewrapScratch::Lines(vec![self.line(physical_lines).clone()])
            }
            Self::Owned(line) => RewrapScratch::Lines(vec![line.clone()]),
        }
    }
}

impl ReflowLogicalLine for Line {
    fn line<'a>(&'a self, _physical_lines: &'a VecDeque<Line>) -> &'a Line {
        self
    }

    fn scratch_for_unwrapped(&self, _physical_lines: &VecDeque<Line>) -> RewrapScratch {
        RewrapScratch::Lines(vec![self.clone()])
    }
}

impl RewrapScratch {
    fn row_count(&self) -> usize {
        match self {
            Self::PhysicalLine(_) => 1,
            Self::Lines(lines) => lines.len(),
            Self::SharedLines(lines) => lines.len(),
        }
    }

    fn shared_chunk(&self, physical_lines: &VecDeque<Line>) -> Arc<[Line]> {
        match self {
            Self::PhysicalLine(idx) => Arc::from(vec![physical_lines[*idx].clone()]),
            Self::Lines(lines) => Arc::from(lines.clone()),
            Self::SharedLines(lines) => Arc::clone(lines),
        }
    }
}

impl ScrollbackTieringState {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn record_spill(&mut self, line_bytes: usize, warm_max_bytes: usize) -> ScrollbackSpillOutcome {
        let mut outcome = ScrollbackSpillOutcome::default();
        let bounded_line_bytes = line_bytes.max(std::mem::size_of::<Line>());
        self.warm_spill_lines_total = self.warm_spill_lines_total.saturating_add(1);
        self.warm_spill_bytes_total = self
            .warm_spill_bytes_total
            .saturating_add(bounded_line_bytes as u64);

        if warm_max_bytes == 0 {
            self.cold_spill_lines_total = self.cold_spill_lines_total.saturating_add(1);
            self.cold_spill_bytes_total = self
                .cold_spill_bytes_total
                .saturating_add(bounded_line_bytes as u64);
            outcome.cold_lines_evicted = 1;
            outcome.cold_bytes_evicted = bounded_line_bytes;
            return outcome;
        }

        self.warm_line_bytes.push_back(bounded_line_bytes);
        self.warm_bytes = self.warm_bytes.saturating_add(bounded_line_bytes);

        while self.warm_bytes > warm_max_bytes {
            if let Some(evicted) = self.warm_line_bytes.pop_front() {
                self.warm_bytes = self.warm_bytes.saturating_sub(evicted);
                self.cold_spill_lines_total = self.cold_spill_lines_total.saturating_add(1);
                self.cold_spill_bytes_total =
                    self.cold_spill_bytes_total.saturating_add(evicted as u64);
                outcome.cold_lines_evicted = outcome.cold_lines_evicted.saturating_add(1);
                outcome.cold_bytes_evicted = outcome.cold_bytes_evicted.saturating_add(evicted);
            } else {
                self.warm_bytes = 0;
                break;
            }
        }

        outcome
    }

    fn evict_all_warm(&mut self) -> ScrollbackSpillOutcome {
        let mut outcome = ScrollbackSpillOutcome::default();

        while let Some(evicted) = self.warm_line_bytes.pop_front() {
            self.warm_bytes = self.warm_bytes.saturating_sub(evicted);
            self.cold_spill_lines_total = self.cold_spill_lines_total.saturating_add(1);
            self.cold_spill_bytes_total =
                self.cold_spill_bytes_total.saturating_add(evicted as u64);
            outcome.cold_lines_evicted = outcome.cold_lines_evicted.saturating_add(1);
            outcome.cold_bytes_evicted = outcome.cold_bytes_evicted.saturating_add(evicted);
        }

        self.warm_bytes = 0;
        outcome
    }

    fn warm_resident_lines(&self) -> usize {
        self.warm_line_bytes.len()
    }
}

fn scrollback_hot_size(config: &Arc<dyn TerminalConfiguration>, allow_scrollback: bool) -> usize {
    if !allow_scrollback {
        return 0;
    }
    let total = config.scrollback_size();
    let tier = config.scrollback_tier_config();
    if !tier.enabled {
        return total;
    }
    if total == 0 {
        return 0;
    }
    tier.hot_lines.max(1).min(total)
}

#[cfg(feature = "use_serde")]
fn accumulate_checkpoint_usage(
    current: &mut usize,
    additional: usize,
    maximum: usize,
    resource: &'static str,
) -> Result<(), ScreenCheckpointCaptureError> {
    let observed = current
        .checked_add(additional)
        .ok_or(ScreenCheckpointCaptureError::ArithmeticOverflow(resource))?;
    if observed > maximum {
        return Err(ScreenCheckpointCaptureError::ResourceLimit {
            resource,
            observed,
            maximum,
        });
    }
    *current = observed;
    Ok(())
}

#[cfg(feature = "use_serde")]
fn inspect_checkpoint_line(
    line: &Line,
    limits: &ScreenCheckpointLimits,
    usage: &mut ScreenCheckpointUsage,
) -> Result<(), ScreenCheckpointCaptureError> {
    if line.has_image_attachments() {
        return Err(ScreenCheckpointCaptureError::UnsupportedGraphicsState);
    }
    accumulate_checkpoint_usage(&mut usage.lines, 1, limits.max_total_lines, "screen_lines")?;
    accumulate_checkpoint_usage(
        &mut usage.retained_capture_bytes,
        limits.estimated_bytes_per_line,
        limits.max_retained_capture_bytes,
        "retained_capture_bytes",
    )?;
    accumulate_checkpoint_usage(
        &mut usage.cells,
        line.len(),
        limits.max_total_cells,
        "screen_cells",
    )?;
    let cell_structural_bytes = line
        .len()
        .checked_mul(limits.estimated_bytes_per_cell)
        .ok_or(ScreenCheckpointCaptureError::ArithmeticOverflow(
            "retained_capture_bytes",
        ))?;
    accumulate_checkpoint_usage(
        &mut usage.retained_capture_bytes,
        cell_structural_bytes,
        limits.max_retained_capture_bytes,
        "retained_capture_bytes",
    )?;

    let mut semantic_cells = 0usize;
    for cell in line.visible_cells() {
        accumulate_checkpoint_usage(
            &mut usage.cell_records,
            1,
            limits.max_total_cell_records,
            "screen_cell_records",
        )?;
        let width = cell.width();
        if !(1..=2).contains(&width) {
            return Err(ScreenCheckpointCaptureError::InvalidLineGeometry);
        }
        semantic_cells = semantic_cells.checked_add(width).ok_or(
            ScreenCheckpointCaptureError::ArithmeticOverflow("semantic_line_cells"),
        )?;

        let text_bytes = cell.str().len();
        if text_bytes > limits.max_string_bytes {
            return Err(ScreenCheckpointCaptureError::ResourceLimit {
                resource: "cell_text_bytes",
                observed: text_bytes,
                maximum: limits.max_string_bytes,
            });
        }
        accumulate_checkpoint_usage(
            &mut usage.cell_text_bytes,
            text_bytes,
            limits.max_total_cell_text_bytes,
            "cell_text_bytes",
        )?;
        accumulate_checkpoint_usage(
            &mut usage.retained_capture_bytes,
            text_bytes,
            limits.max_retained_capture_bytes,
            "retained_capture_bytes",
        )?;

        if let Some(link) = cell.attrs().hyperlink() {
            if link.params().len() > limits.max_hyperlink_params_per_link {
                return Err(ScreenCheckpointCaptureError::ResourceLimit {
                    resource: "hyperlink_params_per_link",
                    observed: link.params().len(),
                    maximum: limits.max_hyperlink_params_per_link,
                });
            }
            accumulate_checkpoint_usage(
                &mut usage.hyperlink_params,
                link.params().len(),
                limits.max_total_hyperlink_params,
                "hyperlink_params",
            )?;

            let mut link_bytes = link.uri().len();
            if link.uri().len() > limits.max_string_bytes {
                return Err(ScreenCheckpointCaptureError::ResourceLimit {
                    resource: "hyperlink_uri_bytes",
                    observed: link.uri().len(),
                    maximum: limits.max_string_bytes,
                });
            }
            for (key, value) in link.params() {
                if key.len() > limits.max_string_bytes {
                    return Err(ScreenCheckpointCaptureError::ResourceLimit {
                        resource: "hyperlink_param_key_bytes",
                        observed: key.len(),
                        maximum: limits.max_string_bytes,
                    });
                }
                if value.len() > limits.max_string_bytes {
                    return Err(ScreenCheckpointCaptureError::ResourceLimit {
                        resource: "hyperlink_param_value_bytes",
                        observed: value.len(),
                        maximum: limits.max_string_bytes,
                    });
                }
                link_bytes = link_bytes
                    .checked_add(key.len())
                    .and_then(|bytes| bytes.checked_add(value.len()))
                    .ok_or(ScreenCheckpointCaptureError::ArithmeticOverflow(
                        "hyperlink_bytes",
                    ))?;
            }
            accumulate_checkpoint_usage(
                &mut usage.hyperlink_bytes,
                link_bytes,
                limits.max_total_hyperlink_bytes,
                "hyperlink_bytes",
            )?;
            accumulate_checkpoint_usage(
                &mut usage.retained_capture_bytes,
                link_bytes,
                limits.max_retained_capture_bytes,
                "retained_capture_bytes",
            )?;
        }
    }

    if semantic_cells != line.len() {
        return Err(ScreenCheckpointCaptureError::InvalidLineGeometry);
    }
    Ok(())
}

impl Screen {
    #[cfg(feature = "use_serde")]
    fn fragments_match_interval(
        fragments: &ColdRowFragments,
        sink: &Arc<dyn crate::config::ScrollbackSpillSink>,
        now: &crate::config::ScrollbackInterval,
    ) -> bool {
        if !Arc::ptr_eq(&fragments.sink, sink) {
            return false;
        }
        let Some(rows) = now.rows() else {
            return true;
        };
        let mut retained = fragments.rows.range(rows);
        let Some((&first, _)) = retained.next() else {
            return true;
        };
        let last = retained.next_back().map_or(first, |(&key, _)| key);
        last.checked_add(1)
            .is_some_and(|end| now.retains(&fragments.interval, first..end))
    }

    #[cfg(feature = "use_serde")]
    fn same_cold_fragments(&self, previous: &Option<Arc<ColdRowFragments>>) -> bool {
        match (&self.cold_row_fragments, previous) {
            (None, None) => true,
            (Some(now), Some(before)) => Arc::ptr_eq(now, before),
            _ => false,
        }
    }

    /// Capture only a complete resident-head logical group wholly above the
    /// live viewport. This transaction cannot move an active/saved cursor.
    /// Visible-head seams require the separate cursor-anchor transaction.
    #[cfg(feature = "use_serde")]
    pub fn capture_cold_seam_reflow(&self) -> anyhow::Result<Option<ColdSeamReflow>> {
        use crate::config::ScrollbackIntervalCapture;
        if !self.allow_scrollback || self.recovery_scrollback.is_some() {
            return Ok(None);
        }
        let Some(sink) = self.config.scrollback_spill_sink() else {
            return Ok(None);
        };
        let interval = match sink.try_capture_scrollback_interval() {
            ScrollbackIntervalCapture::Ready(interval) => interval,
            ScrollbackIntervalCapture::Busy => anyhow::bail!(ColdReadMetadataBusy),
            ScrollbackIntervalCapture::Unavailable => {
                anyhow::bail!("cold seam metadata unavailable");
            }
        };
        let frontier = self.phys_to_stable_row_index(0);
        let Some(rows) = interval.rows() else {
            return Ok(None);
        };
        if rows.start >= frontier {
            return Ok(None);
        }
        anyhow::ensure!(rows.end >= frontier, "cold seam discontinuity");
        if let Some(fragments) = &self.cold_row_fragments {
            anyhow::ensure!(
                Self::fragments_match_interval(fragments, &sink, &interval),
                "cold seam source replaced"
            );
            if fragments.aligned_at(
                frontier,
                self.physical_cols,
                self.dpi,
                self.resize_wrap_policy,
            ) && fragments.alignment_retained(&interval)
            {
                return Ok(None);
            }
        }
        let maximum = self
            .lines
            .len()
            .saturating_sub(self.physical_rows)
            .min(ScreenLineRead::MAX_ROWS - 1);
        let mut budget = LineReadCaptureBudget::default();
        let mut count = None;
        for (index, line) in self.lines.iter().take(maximum).enumerate() {
            // Vector tail lookup visits visible cells; clustered tail lookup
            // is constant-time. Conservatively charge columns for both before
            // inspecting either, sharing the eventual snapshot work budget.
            budget.work_left = budget
                .work_left
                .checked_sub(line.len().max(1))
                .ok_or_else(|| anyhow::anyhow!("cold seam boundary work limit"))?;
            if !line.last_cell_was_wrapped() {
                count = Some(index + 1);
                break;
            }
        }
        let Some(count) = count else {
            return Ok(None);
        };
        let (first, second) = self.lines.as_slices();
        let resident = Line::try_clone_batch_for_snapshot(
            &first[..count.min(first.len())],
            &second[..count.saturating_sub(first.len())],
            &mut budget.bytes_left,
            &mut budget.work_left,
        )
        .ok_or_else(|| anyhow::anyhow!("cold seam resident snapshot limit"))?;
        Ok(Some(ColdSeamReflow {
            witness: self.capture_coordinate_witness(),
            sink,
            interval,
            frontier,
            resident,
            previous: self.cold_row_fragments.clone(),
            policy: self.resize_wrap_policy,
            replacement: None,
            source: None,
            retired_layout: None,
        }))
    }

    /// Commit under the caller's exact pane/resize authority and terminal
    /// lock. Swaps retain all displaced payloads in the worker-owned plan.
    #[cfg(feature = "use_serde")]
    pub fn install_cold_seam_reflow(
        &mut self,
        prepared: &mut ColdSeamReflow,
        seqno: SequenceNo,
    ) -> anyhow::Result<bool> {
        use crate::config::ScrollbackIntervalCapture;
        if seqno == SequenceNo::MAX
            || prepared.replacement.is_none()
            || !self.matches_coordinate_witness(&prepared.witness)
            || !self.same_cold_fragments(&prepared.previous)
            || self.phys_to_stable_row_index(0) != prepared.frontier
            || prepared.resident.len() > self.lines.len().saturating_sub(self.physical_rows)
            || !self
                .lines
                .iter()
                .zip(&prepared.resident)
                .all(|(now, before)| now.is_same_reflow_source(before))
        {
            return Ok(false);
        }
        let Some(sink) = self.config.scrollback_spill_sink() else {
            return Ok(false);
        };
        if !Arc::ptr_eq(&sink, &prepared.sink) {
            return Ok(false);
        }
        let now = match sink.try_capture_scrollback_interval() {
            ScrollbackIntervalCapture::Ready(now) => now,
            ScrollbackIntervalCapture::Busy => anyhow::bail!(ColdReadMetadataBusy),
            ScrollbackIntervalCapture::Unavailable => return Ok(false),
        };
        let Some(source) = prepared.source.as_ref() else {
            return Ok(false);
        };
        if !now.retains(&prepared.interval, source.clone()) {
            return Ok(false);
        }
        let Some((replacement, rows)) = prepared.replacement.as_mut() else {
            return Ok(false);
        };
        if !Self::fragments_match_interval(replacement, &sink, &now) {
            return Ok(false);
        }
        for (live, replacement) in self.lines.iter_mut().zip(rows) {
            replacement.update_last_change_seqno(seqno);
            std::mem::swap(live, replacement);
        }
        let previous = self.cold_row_fragments.replace(Arc::clone(replacement));
        prepared.previous = previous;
        prepared.retired_layout = self.cold_visual_layout.take();
        self.invalidate_coordinate_witnesses();
        self.cold_visual_seqno = seqno;
        Ok(true)
    }
    /// Capture a bounded request without invoking any blocking sink method.
    #[cfg(feature = "use_serde")]
    pub fn capture_line_read(
        &self,
        requested: Range<StableRowIndex>,
    ) -> anyhow::Result<ScreenLineRead> {
        self.capture_line_read_with_budget(requested, &mut LineReadCaptureBudget::default())
    }

    #[cfg(feature = "use_serde")]
    pub fn capture_line_read_with_budget(
        &self,
        requested: Range<StableRowIndex>,
        budget: &mut LineReadCaptureBudget,
    ) -> anyhow::Result<ScreenLineRead> {
        use crate::config::ScrollbackIntervalCapture;
        let count = requested.end.saturating_sub(requested.start).max(0) as usize;
        anyhow::ensure!(count <= ScreenLineRead::MAX_ROWS, "line read row limit");
        let hot_top = self.phys_to_stable_row_index(0);
        let newest = self.phys_to_stable_row_index(self.lines.len());
        let mut oldest = hot_top;
        let mut oldest_is_known = true;
        let mut cold = None;
        if self.allow_scrollback {
            if let Some(sink) = self.config.scrollback_spill_sink() {
                match sink.try_capture_scrollback_interval() {
                    ScrollbackIntervalCapture::Ready(interval) => {
                        if requested.start < hot_top {
                            if let Some(fragments) = &self.cold_row_fragments {
                                anyhow::ensure!(
                                    Self::fragments_match_interval(fragments, &sink, &interval),
                                    "cold fragment source replaced"
                                );
                            }
                        }
                        if let Some(rows) = interval.rows() {
                            // A missing cold/hot seam is not permission to rebase
                            // a requested cold row onto resident row zero.
                            anyhow::ensure!(rows.end >= hot_top, "cold read discontinuity");
                            oldest = rows.start.min(hot_top);
                            if let Some(layout) = self.cold_visual_layout_for_interval(&interval) {
                                oldest = layout.visual.start;
                            }
                        }
                        cold = Some((sink, interval));
                    }
                    _ if requested.start >= hot_top && requested.end <= newest => {
                        oldest_is_known = false;
                    }
                    ScrollbackIntervalCapture::Busy => anyhow::bail!(ColdReadMetadataBusy),
                    ScrollbackIntervalCapture::Unavailable => {
                        anyhow::bail!("cold read metadata unavailable");
                    }
                }
            }
        }
        let len = count.min(newest.saturating_sub(oldest).max(0) as usize);
        let first = if count == 0 {
            requested.start
        } else if requested.end > newest {
            newest.saturating_sub(len as StableRowIndex).max(oldest)
        } else {
            requested.start.max(oldest)
        };
        let end = first
            .checked_add(len as StableRowIndex)
            .ok_or_else(|| anyhow::anyhow!("line read range overflow"))?;
        let resident_first = first.max(hot_top).min(end);
        let resident = if resident_first < end {
            let start = self
                .stable_row_to_phys(resident_first)
                .ok_or_else(|| anyhow::anyhow!("resident read unavailable"))?;
            let stop = start + (end - resident_first) as usize;
            anyhow::ensure!(stop <= self.lines.len(), "resident read unavailable");
            let (first, second) = self.lines.as_slices();
            Line::try_clone_batch_for_snapshot(
                &first[start.min(first.len())..stop.min(first.len())],
                &second[start.saturating_sub(first.len())..stop.saturating_sub(first.len())],
                &mut budget.bytes_left,
                &mut budget.work_left,
            )
            .ok_or_else(|| anyhow::anyhow!("resident snapshot allocation/work limit"))?
        } else {
            Vec::new()
        };
        let layout = if first < resident_first {
            let stored = cold.as_ref().and_then(|(sink, interval)| {
                self.capture_stored_physical_layout(sink, interval, hot_top, budget)
            });
            stored.or_else(|| {
                cold.as_ref()
                    .and_then(|(_, interval)| self.cold_visual_layout_for_interval(interval))
                    .filter(|layout| resident_first <= layout.visual.end)
                    .and_then(|_| self.cold_visual_layout.as_ref().map(Arc::clone))
            })
        } else {
            None
        };
        let geometry = if first < resident_first && layout.is_none() {
            cold.as_ref().and_then(|(sink, interval)| {
                self.capture_cold_geometry(sink, interval, hot_top, budget)
            })
        } else {
            None
        };
        Ok(ScreenLineRead {
            witness: self.capture_coordinate_witness(),
            layout_seqno: self.cold_visual_seqno,
            first,
            end,
            resident_first,
            resident,
            cold,
            hydrated: Vec::new(),
            payload_bytes: 0,
            complete: false,
            cold_context: None,
            rendered: None,
            hot_top,
            known_resident_history_start: (oldest_is_known && oldest == hot_top).then_some(hot_top),
            wrap_policy: self.resize_wrap_policy,
            layout,
            logical_view: None,
            index_budget_exhausted: Arc::clone(&self.cold_index_budget_exhausted),
            attempted_index: !self
                .cold_index_budget_exhausted
                .load(std::sync::atomic::Ordering::Acquire),
            fragments: self.cold_row_fragments.clone(),
            geometry,
        })
    }

    #[cfg(feature = "use_serde")]
    fn capture_cold_geometry(
        &self,
        sink: &Arc<dyn crate::config::ScrollbackSpillSink>,
        interval: &crate::config::ScrollbackInterval,
        frontier: StableRowIndex,
        budget: &mut LineReadCaptureBudget,
    ) -> Option<ColdGeometrySnapshot> {
        let index = self.cold_geometry_index.as_ref()?;
        let retained = interval.rows()?;
        if !Arc::ptr_eq(sink, &index.sink)
            || index.source.start > retained.start
            || index.source.end != frontier
            || !interval.retains(&index.interval, retained.start..frontier)
        {
            return None;
        }
        let skip = usize::try_from(retained.start.checked_sub(index.source.start)?).ok()?;
        let count = index.rows.len().checked_sub(skip)?;
        if count == 0 || count > budget.work_left {
            return None;
        }
        let bytes = count
            .checked_mul(std::mem::size_of::<ColdGeometryRow>())?
            .checked_add(std::mem::size_of::<ColdGeometrySnapshot>())?;
        if bytes > budget.bytes_left {
            return None;
        }
        let mut rows = Vec::new();
        rows.try_reserve_exact(count).ok()?;
        let actual_bytes = rows
            .capacity()
            .checked_mul(std::mem::size_of::<ColdGeometryRow>())?
            .checked_add(std::mem::size_of::<ColdGeometrySnapshot>())?;
        if actual_bytes > budget.bytes_left {
            return None;
        }
        rows.extend(index.rows.iter().skip(skip).cloned());
        budget.work_left -= count;
        budget.bytes_left -= actual_bytes;
        Some(ColdGeometrySnapshot {
            source: retained.start..frontier,
            rows,
        })
    }

    #[cfg(feature = "use_serde")]
    fn admitted_stored_physical_layout(
        &self,
        sink: &Arc<dyn crate::config::ScrollbackSpillSink>,
        interval: &crate::config::ScrollbackInterval,
        frontier: StableRowIndex,
    ) -> Option<&StoredPhysicalLayout> {
        let stored = self.stored_physical_layout.as_ref()?;
        let retained = interval.rows()?;
        if self.cold_row_fragments.is_some()
            || self
                .cold_visual_layout_for_interval(interval)
                .is_some_and(|layout| !layout.stored_physical())
            || !Arc::ptr_eq(sink, &stored.sink)
            || !self.matches_coordinate_witness(&stored.witness)
            || stored.source.start > retained.start
            || stored.source.end != frontier
            || !interval.retains(&stored.interval, retained.start..frontier)
        {
            return None;
        }
        Some(stored)
    }

    #[cfg(feature = "use_serde")]
    fn capture_stored_physical_layout(
        &self,
        sink: &Arc<dyn crate::config::ScrollbackSpillSink>,
        interval: &crate::config::ScrollbackInterval,
        frontier: StableRowIndex,
        budget: &mut LineReadCaptureBudget,
    ) -> Option<Arc<ColdVisualLayout>> {
        let stored = self.admitted_stored_physical_layout(sink, interval, frontier)?;
        let retained = interval.rows()?;
        if let Some(layout) = self.cold_visual_layout.as_ref().filter(|layout| {
            layout.stored_physical()
                && layout.source == (retained.start..frontier)
                && self.matches_coordinate_witness(&layout.witness)
                && interval.retains(&layout.interval, layout.source.clone())
        }) {
            return Some(Arc::clone(layout));
        }
        let count = stored.groups.len();
        let bytes = count
            .checked_mul(std::mem::size_of::<(
                Range<StableRowIndex>,
                Range<StableRowIndex>,
            )>())?
            .checked_add(
                std::mem::size_of::<ColdVisualLayout>() + 2 * std::mem::size_of::<usize>(),
            )?;
        if count > budget.work_left || bytes > budget.bytes_left {
            return None;
        }
        let mut groups = Vec::with_capacity(count);
        for group in &stored.groups {
            let source = group.start.max(retained.start)..group.end.min(frontier);
            if source.start < source.end {
                groups.push((source.clone(), source));
            }
        }
        budget.work_left -= count;
        budget.bytes_left -= bytes;
        Some(Arc::new(ColdVisualLayout {
            kind: ColdVisualLayoutKind::StoredPhysical {
                open_tail: stored.open_tail,
            },
            source: retained.start..frontier,
            visual: retained.start..frontier,
            resident_frontier: frontier,
            groups,
            witness: stored.witness.clone(),
            interval: interval.clone(),
        }))
    }

    /// Call under the terminal lock, in the same critical section as publication.
    #[cfg(feature = "use_serde")]
    pub fn validates_line_read(&self, read: &ScreenLineRead) -> bool {
        self.try_validate_line_read(read).unwrap_or(false)
    }

    /// Publication callers that can retry must distinguish transient metadata
    /// contention from a read whose source is no longer valid. Paint callers
    /// may use `validates_line_read` to defer either refusal.
    #[cfg(feature = "use_serde")]
    pub fn try_validate_line_read(
        &self,
        read: &ScreenLineRead,
    ) -> Result<bool, ColdReadMetadataBusy> {
        if !read.complete || read.row_count() != read.end.saturating_sub(read.first) as usize {
            return Ok(false);
        }
        self.validates_line_read_source(read)
    }

    #[cfg(feature = "use_serde")]
    fn validates_line_read_source(
        &self,
        read: &ScreenLineRead,
    ) -> Result<bool, ColdReadMetadataBusy> {
        use crate::config::ScrollbackIntervalCapture;
        if !self.matches_coordinate_witness(&read.witness)
            || !self.same_cold_fragments(&read.fragments)
        {
            return Ok(false);
        }
        if read.first < read.resident_first || read.layout.is_some() {
            if read.layout.as_ref().is_some_and(|layout| {
                !layout.stored_physical()
                    && layout.resident_frontier != self.phys_to_stable_row_index(0)
            }) {
                // Canonical visual rows are anchored to the complete cold
                // frontier. New source rows can have a different wrapped row
                // count and shift even a closed prefix's visual origin.
                return Ok(false);
            }
            if read.layout.as_ref().is_some_and(|layout| {
                matches!(
                    layout.kind,
                    ColdVisualLayoutKind::StoredPhysical { open_tail: true }
                ) && layout.resident_frontier < self.phys_to_stable_row_index(0)
                    && read
                        .cold_context
                        .as_ref()
                        .is_some_and(|context| context.end == layout.source.end)
            }) {
                // The old bytes are retained, but this cached logical context
                // ended at an open seam. Newly spilled rows may continue or
                // close that same logical group; never certify its old end.
                return Ok(false);
            }
            if read.layout_seqno != self.cold_visual_seqno {
                // Another worker may have published while this read was in
                // flight. Identical or prefix-extending mappings are safe;
                // shifted coordinates must never roll the live map backwards.
                // An old physical layout still identifies closed prefix rows
                // even when its open tail is no longer current. The check
                // above rejects that tail, and source retention is checked
                // below before the cached bytes can be reused.
                let current = self.current_cold_visual_layout().or_else(|| {
                    self.cold_visual_layout.as_deref().filter(|layout| {
                        layout.stored_physical()
                            && self.matches_coordinate_witness(&layout.witness)
                            && layout.resident_frontier <= self.phys_to_stable_row_index(0)
                    })
                });
                let compatible = read
                    .layout
                    .as_ref()
                    .zip(current)
                    .is_some_and(|(before, now)| {
                        std::ptr::eq(before.as_ref(), now)
                            || before.extends(now)
                            || now.extends(before)
                    });
                if !compatible {
                    return Ok(false);
                }
            }
            let Some((source, before)) = &read.cold else {
                return Ok(false);
            };
            let Some(current) = self.config.scrollback_spill_sink() else {
                return Ok(false);
            };
            if !Arc::ptr_eq(source, &current) {
                return Ok(false);
            }
            match current.try_capture_scrollback_interval() {
                ScrollbackIntervalCapture::Ready(now)
                    if read.layout.as_ref().is_none_or(|layout| {
                        layout.resident_frontier <= self.phys_to_stable_row_index(0)
                            && now
                                .rows()
                                .is_some_and(|rows| rows.start == layout.source.start)
                    }) && now.retains(
                        before,
                        read.cold_context
                            .clone()
                            .unwrap_or(read.first..read.resident_first),
                    ) => {}
                ScrollbackIntervalCapture::Busy => return Err(ColdReadMetadataBusy),
                _ => return Ok(false),
            }
        }
        if !read.resident.is_empty() {
            let Some(start) = self.stable_row_to_phys(read.resident_first) else {
                return Ok(false);
            };
            if start
                .checked_add(read.resident.len())
                .is_none_or(|end| end > self.lines.len())
            {
                return Ok(false);
            }
            if !self
                .lines
                .iter()
                .skip(start)
                .zip(&read.resident)
                .all(|(now, before)| now == before)
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Install only after all reads in a publication transaction validate.
    /// The stored prefix and resident coordinates are unchanged; consumers
    /// now share this generation's visual-to-source mapping.
    #[cfg(feature = "use_serde")]
    pub fn line_read_changes_layout(&self, read: &ScreenLineRead) -> bool {
        read.layout.as_ref().is_some_and(|next| {
            // This compares coordinate mappings, not permission to publish
            // bytes. Source validation has its own retained-interval fence.
            // Re-probing storage here can observe Busy just after validation
            // and turn an unchanged mapping into a spurious sequence advance.
            self.cold_visual_layout_at_current_coordinates()
                .is_none_or(|current| {
                    if std::ptr::eq(next.as_ref(), current) {
                        return false;
                    }
                    // Extending an unchanged prefix with already-current-width
                    // rows does not invalidate older visual coordinates. An
                    // older validated read of that prefix is unchanged too;
                    // installation keeps the more complete current layout.
                    !next.extends(current) && !current.extends(next)
                })
        })
    }

    #[cfg(feature = "use_serde")]
    pub fn validates_prepared_cold_layout(
        &self,
        prepared: &PreparedColdLayout,
    ) -> Result<bool, ColdReadMetadataBusy> {
        if prepared.installed {
            return Ok(false);
        }
        self.validates_line_read_source(&prepared.source)
    }

    #[cfg(feature = "use_serde")]
    pub fn prepared_cold_layout_changes_layout(&self, prepared: &PreparedColdLayout) -> bool {
        self.line_read_changes_layout(&prepared.source)
    }

    /// Validate and install under the caller's pane/resize authority. A single
    /// receipt cannot publish twice or retire its displaced layout under lock.
    #[cfg(feature = "use_serde")]
    pub fn install_prepared_cold_layout(
        &mut self,
        prepared: &mut PreparedColdLayout,
        seqno: SequenceNo,
    ) -> Result<bool, ColdReadMetadataBusy> {
        if seqno == SequenceNo::MAX || !self.validates_prepared_cold_layout(prepared)? {
            return Ok(false);
        }
        prepared.retired_layout = self.replace_line_read_layout(&prepared.source, seqno);
        prepared.installed = true;
        Ok(true)
    }

    /// Reconcile asynchronous retention/replacement with the terminal's wire
    /// sequence. The caller increments its sequence before publishing a pair.
    /// Busy is not an unchanged authority and must defer the observation.
    #[cfg(feature = "use_serde")]
    pub fn refresh_cold_source_observation(
        &mut self,
    ) -> Result<Option<bool>, ColdReadMetadataBusy> {
        let Some(sink) = self
            .config
            .scrollback_spill_sink()
            .filter(|_| self.allow_scrollback)
        else {
            return Ok(Some(self.cold_source_observation.take().is_some()));
        };
        let now = match sink.try_capture_scrollback_interval() {
            crate::config::ScrollbackIntervalCapture::Ready(now) => now,
            crate::config::ScrollbackIntervalCapture::Busy => return Err(ColdReadMetadataBusy),
            crate::config::ScrollbackIntervalCapture::Unavailable => return Ok(None),
        };
        let changed = self
            .cold_source_observation
            .as_ref()
            .is_none_or(|(before_sink, before)| {
                !Arc::ptr_eq(before_sink, &sink)
                    || match (before.rows(), now.rows()) {
                        (Some(before_rows), Some(now_rows)) => {
                            before_rows.start != now_rows.start
                                || now_rows.end < before_rows.end
                                || !now.retains(before, before_rows)
                        }
                        (None, None) => false,
                        _ => true,
                    }
            });
        if changed {
            self.cold_index_budget_exhausted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        }
        self.cold_source_observation = Some((sink, now));
        Ok(Some(changed))
    }

    /// Authoritative scrollback geometry derived from the active cold observation,
    /// valid only after a successful refresh under terminal lock.
    /// Row count is computed against live hot rows.
    #[cfg(feature = "use_serde")]
    pub fn observed_scrollback_geometry(&self) -> Option<(StableRowIndex, usize)> {
        let hot_top = self.phys_to_stable_row_index(0);
        let newest = self.phys_to_stable_row_index(self.lines.len());
        let Some(sink) = self
            .config
            .scrollback_spill_sink()
            .filter(|_| self.allow_scrollback)
        else {
            let rows = usize::try_from(newest.saturating_sub(hot_top)).unwrap_or(0);
            return Some((hot_top, rows));
        };
        let (observed_sink, interval) = self.cold_source_observation.as_ref()?;
        if !Arc::ptr_eq(observed_sink, &sink) {
            return None;
        }
        let oldest = if let Some(layout) = self.cold_visual_layout_for_interval(interval) {
            layout.visual.start
        } else {
            interval.rows().map(|rows| rows.start).unwrap_or(hot_top)
        }
        .min(hot_top);
        let rows = usize::try_from(newest.saturating_sub(oldest)).unwrap_or(0);
        Some((oldest, rows))
    }

    #[cfg(feature = "use_serde")]
    pub fn install_line_read_layout(&mut self, read: &ScreenLineRead, seqno: SequenceNo) {
        drop(self.replace_line_read_layout(read, seqno));
    }

    #[cfg(feature = "use_serde")]
    fn replace_line_read_layout(
        &mut self,
        read: &ScreenLineRead,
        seqno: SequenceNo,
    ) -> Option<Arc<ColdVisualLayout>> {
        if let Some(layout) = &read.layout {
            if self.current_cold_visual_layout().is_some_and(|current| {
                std::ptr::eq(layout.as_ref(), current) || current.extends(layout)
            }) {
                return None;
            }
            if self.line_read_changes_layout(read) {
                self.cold_visual_seqno = seqno;
            }
            return self.cold_visual_layout.replace(Arc::clone(layout));
        }
        None
    }

    #[cfg(feature = "use_serde")]
    pub fn cold_visual_layout_seqno(&self) -> SequenceNo {
        self.cold_visual_seqno
    }

    /// Expand presentation work to complete indexed logical groups before
    /// hyperlink scanning. Binary search touches metadata only, never storage.
    #[cfg(feature = "use_serde")]
    pub fn expand_cold_logical_range(
        &self,
        requested: Range<StableRowIndex>,
    ) -> Range<StableRowIndex> {
        if requested.start >= requested.end {
            return requested;
        }
        let frontier = self.phys_to_stable_row_index(0);
        let admitted = self.config.scrollback_spill_sink().and_then(|sink| {
            let crate::config::ScrollbackIntervalCapture::Ready(interval) =
                sink.try_capture_scrollback_interval()
            else {
                return None;
            };
            let retained = interval.rows()?;
            self.admitted_stored_physical_layout(&sink, &interval, frontier)
                .map(|stored| (stored, retained))
        });
        let (start, mut stop, open_tail) = if let Some((stored, retained)) = admitted {
            // The published layout can lag ordinary append. Consult the
            // atomically admitted groups without copying or decoding them so
            // a formerly resident continuation is included after it spills.
            let first = stored
                .groups
                .partition_point(|source| source.end <= requested.start.max(retained.start));
            let end = stored
                .groups
                .partition_point(|source| source.start < requested.end);
            if first >= end {
                return requested;
            }
            (
                stored.groups[first]
                    .start
                    .max(retained.start)
                    .min(requested.start),
                stored.groups[end - 1].end.max(requested.end),
                stored.open_tail && end == stored.groups.len(),
            )
        } else {
            let Some(layout) = self.current_cold_visual_layout() else {
                return requested;
            };
            let first = layout
                .groups
                .partition_point(|(_, visual)| visual.end <= requested.start);
            let end = layout
                .groups
                .partition_point(|(_, visual)| visual.start < requested.end);
            if first >= end {
                return requested;
            }
            (
                layout.groups[first].1.start.min(requested.start),
                layout.groups[end - 1].1.end.max(requested.end),
                matches!(
                    layout.kind,
                    ColdVisualLayoutKind::StoredPhysical { open_tail: true }
                ) && end == layout.groups.len()
                    && layout.resident_frontier == frontier,
            )
        };
        if open_tail {
            // The spill frontier may split a logical group. Its resident tail
            // is already decoded; inspect only bounded wrap bits, never reflow
            // or cold IO under the terminal lock.
            for (offset, line) in self.lines.iter().enumerate().take(ScreenLineRead::MAX_ROWS) {
                let row_end = frontier.saturating_add(offset as StableRowIndex + 1);
                if row_end.saturating_sub(start) as usize > ScreenLineRead::MAX_ROWS {
                    break;
                }
                stop = stop.max(row_end);
                if !line.last_cell_was_wrapped() {
                    break;
                }
            }
        }
        start..stop
    }

    #[cfg(feature = "use_serde")]
    fn current_cold_visual_layout(&self) -> Option<&ColdVisualLayout> {
        // Avoid invoking a sink at all when the local coordinates are stale.
        self.cold_visual_layout_at_current_coordinates()?;
        let sink = self.config.scrollback_spill_sink()?;
        match sink.try_capture_scrollback_interval() {
            crate::config::ScrollbackIntervalCapture::Ready(interval) => {
                self.cold_visual_layout_for_interval(&interval)
            }
            crate::config::ScrollbackIntervalCapture::Busy => {
                let (before_sink, interval) = self.cold_source_observation.as_ref()?;
                if Arc::ptr_eq(before_sink, &sink) {
                    self.cold_visual_layout_for_interval(interval)
                } else {
                    None
                }
            }
            crate::config::ScrollbackIntervalCapture::Unavailable => None,
        }
    }

    /// A capture already owns one coherent interval. Re-probing its sink here
    /// could turn temporary contention into a false cache miss and payload
    /// reindex. Publication still revalidates against the live source.
    #[cfg(feature = "use_serde")]
    fn cold_visual_layout_for_interval(
        &self,
        interval: &crate::config::ScrollbackInterval,
    ) -> Option<&ColdVisualLayout> {
        self.cold_visual_layout_at_current_coordinates()
            .filter(|layout| {
                interval
                    .rows()
                    .is_some_and(|rows| rows.start == layout.source.start)
                    && interval.retains(&layout.interval, layout.source.clone())
            })
    }

    #[cfg(feature = "use_serde")]
    fn cold_visual_layout_at_current_coordinates(&self) -> Option<&ColdVisualLayout> {
        let layout = self.cold_visual_layout.as_deref()?;
        let frontier = self.phys_to_stable_row_index(0);
        if !self.matches_coordinate_witness(&layout.witness)
            || layout.resident_frontier > frontier
            || (!layout.stored_physical() && layout.resident_frontier != frontier)
            || (matches!(
                layout.kind,
                ColdVisualLayoutKind::StoredPhysical { open_tail: true }
            ) && layout.resident_frontier < frontier)
        {
            return None;
        }
        Some(layout)
    }

    /// Capture without consulting configuration/sink callbacks or scanning
    /// lines. Cloning the token only increments its reference count.
    pub fn capture_coordinate_witness(&self) -> ScreenCoordinateWitness {
        ScreenCoordinateWitness {
            identity: Arc::clone(&self.coordinate_identity.0),
            rows: self.physical_rows,
            cols: self.physical_cols,
            dpi: self.dpi,
        }
    }

    /// Constant-time, allocation-free local check. Ordinary append and hot
    /// eviction preserve stable coordinates, but may change interval retention;
    /// callers must separately validate the exact requested storage interval.
    pub fn matches_coordinate_witness(&self, witness: &ScreenCoordinateWitness) -> bool {
        Arc::ptr_eq(&self.coordinate_identity.0, &witness.identity)
            && self.physical_rows == witness.rows
            && self.physical_cols == witness.cols
            && self.dpi == witness.dpi
    }

    /// Capture a resident selection against a caller's atomic terminal source
    /// observation. The caller must hold terminal ownership and supply its
    /// current sequence. Cold rows and ambiguous gaps in wrapped rows require
    /// a separate source-bound history projection and are not certified here.
    /// Points are [origin, range start, range end] in the original drag order;
    /// both range endpoints must be present or both absent. Endpoint roles
    /// determine the meaning of BeforeZero when a physical row is unwrapped.
    pub fn capture_selection_anchor(
        &mut self,
        source_sequence: SequenceNo,
        points: [Option<SelectionAnchorCoordinate>; 3],
    ) -> Option<ScreenSelectionAnchor> {
        if !self.allow_scrollback
            || source_sequence == SequenceNo::MAX
            || points.iter().all(Option::is_none)
            || points[1].is_some() != points[2].is_some()
            || points[0].is_some_and(|point| point.column.is_none())
            || (points[1] == points[2] && points[1].is_some_and(|point| point.column.is_none()))
            || !self.selection_anchor_points_are_resident(&points)
        {
            return None;
        }
        self.selection_anchors
            .0
            .retain(|entry| entry.owner.strong_count() != 0);
        if self.selection_anchors.0.len() >= 16 {
            return None;
        }
        let token = ScreenSelectionAnchor(Arc::new(()));
        let entry = SelectionAnchorEntry {
            owner: Arc::downgrade(&token.0),
            source_sequence,
            witness: self.capture_coordinate_witness(),
            points,
        };
        self.selection_anchors.0.push(entry);
        Some(token)
    }

    pub fn resolve_selection_anchor(
        &self,
        token: &ScreenSelectionAnchor,
        source_sequence: SequenceNo,
    ) -> Option<[Option<SelectionAnchorCoordinate>; 3]> {
        let entry = self.selection_anchors.0.iter().find(|entry| {
            entry.owner.as_ptr() == Arc::as_ptr(&token.0)
                && entry.source_sequence <= source_sequence
                && source_sequence != SequenceNo::MAX
                && self.matches_coordinate_witness(&entry.witness)
                && self.selection_anchor_rows_unchanged_since(&entry.points, entry.source_sequence)
        })?;
        self.selection_anchor_points_are_resident(&entry.points)
            .then_some(entry.points)
    }

    fn selection_anchor_points_are_resident(
        &self,
        points: &[Option<SelectionAnchorCoordinate>; 3],
    ) -> bool {
        points.iter().flatten().all(|point| {
            let Some(row) = self.stable_row_to_phys(point.row) else {
                return false;
            };
            let line = &self.lines[row];
            point.column != Some(usize::MAX)
                && (!line.last_cell_was_wrapped()
                    || point.column.is_none_or(|column| column < line.len()))
        })
    }

    /// The terminal owner supplies the current source sequence. Advancing
    /// that sequence elsewhere does not invalidate these resident coordinates
    /// when every row in the selected span is unchanged. Never load cold rows
    /// or accept pruned endpoints as evidence of an unchanged selection.
    pub fn selection_anchor_rows_unchanged_since(
        &self,
        points: &[Option<SelectionAnchorCoordinate>; 3],
        sequence: SequenceNo,
    ) -> bool {
        let mut rows = points.iter().flatten().map(|point| point.row);
        let Some(first) = rows.next() else {
            return false;
        };
        let (start, end) = rows.fold((first, first), |(start, end), row| {
            (start.min(row), end.max(row))
        });
        let Some(start) = self.stable_row_to_phys(start) else {
            return false;
        };
        let Some(end) = self
            .stable_row_to_phys(end)
            .and_then(|end| end.checked_add(1))
        else {
            return false;
        };
        // Validate directly across both deque slices. Materializing a pointer
        // vector here allocates for the entire selection on every observation,
        // including each clipboard chunk and resize preparation.
        self.lines
            .range(start..end)
            .all(|line| !line.changed_since(sequence))
    }

    /// LocalPane finalizes its layout floor after the terminal resize. Only
    /// anchors already mapped to this exact Screen can follow that publication
    /// sequence increment; an invalidated/replaced Screen cannot be blessed.
    pub fn publish_selection_anchor_sequence(
        &mut self,
        resize_sequence: SequenceNo,
        published_sequence: SequenceNo,
    ) {
        if published_sequence == SequenceNo::MAX
            || (published_sequence != resize_sequence
                && resize_sequence.checked_add(1) != Some(published_sequence))
        {
            return;
        }
        let witness = self.capture_coordinate_witness();
        for entry in &mut self.selection_anchors.0 {
            if entry.source_sequence == resize_sequence
                && Arc::ptr_eq(&entry.witness.identity, &witness.identity)
                && entry.witness.rows == witness.rows
                && entry.witness.cols == witness.cols
                && entry.witness.dpi == witness.dpi
            {
                entry.source_sequence = published_sequence;
            }
        }
    }

    fn take_selection_anchors_for_resize(&mut self, seqno: SequenceNo) -> SelectionAnchorRegistry {
        let mut anchors = std::mem::take(&mut self.selection_anchors);
        anchors.0.retain(|entry| {
            entry.owner.strong_count() != 0
                && seqno != SequenceNo::MAX
                && seqno
                    .checked_sub(1)
                    .is_some_and(|before| entry.source_sequence <= before)
                && self.matches_coordinate_witness(&entry.witness)
                && self.selection_anchor_points_are_resident(&entry.points)
                && self.selection_anchor_rows_unchanged_since(&entry.points, entry.source_sequence)
        });
        anchors
    }

    fn finish_selection_anchor_resize(
        &mut self,
        mut anchors: SelectionAnchorRegistry,
        seqno: SequenceNo,
    ) {
        anchors
            .0
            .retain(|entry| self.selection_anchor_points_are_resident(&entry.points));
        for entry in &mut anchors.0 {
            entry.source_sequence = seqno;
            entry.witness = self.capture_coordinate_witness();
        }
        self.selection_anchors = anchors;
    }

    pub(crate) fn invalidate_coordinate_witnesses(&mut self) {
        self.coordinate_identity = ScreenCoordinateIdentity::default();
        #[cfg(feature = "use_serde")]
        {
            self.cold_index_budget_exhausted = Arc::new(std::sync::atomic::AtomicBool::new(false));
            self.stored_physical_layout = None;
        }
    }

    /// Validate and account only the resident screen model.
    ///
    /// This is intentionally non-cloning and never crosses the cold-storage
    /// capability boundary. Inert replay uses it after each raw journal record
    /// to fail before an attacker can accumulate semantic state beyond the
    /// checkpoint admission envelope.
    #[cfg(feature = "use_serde")]
    pub(crate) fn preflight_resident_checkpoint_usage(
        &self,
        limits: &ScreenCheckpointLimits,
        usage: &mut ScreenCheckpointUsage,
    ) -> Result<(), ScreenCheckpointCaptureError> {
        if self.physical_rows == 0 || self.physical_rows > limits.max_rows {
            return Err(ScreenCheckpointCaptureError::ResourceLimit {
                resource: "physical_rows",
                observed: self.physical_rows,
                maximum: limits.max_rows,
            });
        }
        if self.physical_cols == 0 || self.physical_cols > limits.max_cols {
            return Err(ScreenCheckpointCaptureError::ResourceLimit {
                resource: "physical_cols",
                observed: self.physical_cols,
                maximum: limits.max_cols,
            });
        }
        let visible_grid_cells = self.physical_rows.checked_mul(self.physical_cols).ok_or(
            ScreenCheckpointCaptureError::ArithmeticOverflow("visible_grid_cells"),
        )?;
        if visible_grid_cells > limits.max_visible_grid_cells {
            return Err(ScreenCheckpointCaptureError::ResourceLimit {
                resource: "visible_grid_cells",
                observed: visible_grid_cells,
                maximum: limits.max_visible_grid_cells,
            });
        }
        if self.keyboard_stack.len() > limits.max_keyboard_stack_depth {
            return Err(ScreenCheckpointCaptureError::ResourceLimit {
                resource: "keyboard_stack_depth",
                observed: self.keyboard_stack.len(),
                maximum: limits.max_keyboard_stack_depth,
            });
        }
        let keyboard_stack_bytes = self
            .keyboard_stack
            .len()
            .checked_mul(std::mem::size_of::<KeyboardEncoding>())
            .ok_or(ScreenCheckpointCaptureError::ArithmeticOverflow(
                "retained_capture_bytes",
            ))?;
        accumulate_checkpoint_usage(
            &mut usage.retained_capture_bytes,
            keyboard_stack_bytes,
            limits.max_retained_capture_bytes,
            "retained_capture_bytes",
        )?;
        for line in &self.lines {
            inspect_checkpoint_line(line, limits, usage)?;
        }
        Ok(())
    }

    #[cfg(feature = "use_serde")]
    pub(crate) fn checkpoint_parts_staged(
        &self,
        limits: &ScreenCheckpointLimits,
        usage: &mut ScreenCheckpointUsage,
    ) -> Result<StagedScreenCheckpoint, ScreenCheckpointCaptureError> {
        let resident_oldest = self.stable_row_index_offset;

        // Inspect every resident line before asking the sink to allocate or
        // decode a cold snapshot. This also establishes the remaining global
        // checkpoint budget supplied to the sink.
        self.preflight_resident_checkpoint_usage(limits, usage)?;

        if let Some(recovery) = self.recovery_scrollback {
            let oldest_stable = StableRowIndex::try_from(resident_oldest)
                .map_err(|_| ScreenCheckpointCaptureError::ColdScrollbackMetadataInconsistent)?;
            let resident_count = StableRowIndex::try_from(self.lines.len())
                .map_err(|_| ScreenCheckpointCaptureError::ColdScrollbackMetadataInconsistent)?;
            let resident_newest = oldest_stable
                .checked_add(resident_count)
                .ok_or(ScreenCheckpointCaptureError::ColdScrollbackMetadataInconsistent)?;
            let boundary = recovery.original_cold_prefix_newest_exclusive;
            if boundary < oldest_stable || boundary > resident_newest {
                return Err(ScreenCheckpointCaptureError::ColdScrollbackMetadataInconsistent);
            }
            let cold_prefix_line_count = usize::try_from(boundary - oldest_stable)
                .map_err(|_| ScreenCheckpointCaptureError::ColdScrollbackMetadataInconsistent)?;
            if !self.allow_scrollback
                && (cold_prefix_line_count != 0 || recovery.expected_generation.is_some())
            {
                return Err(ScreenCheckpointCaptureError::ColdScrollbackMetadataInconsistent);
            }
            let mut keyboard_stack = Vec::new();
            keyboard_stack
                .try_reserve_exact(self.keyboard_stack.len())
                .map_err(|_| ScreenCheckpointCaptureError::ResourceAllocation("keyboard_stack"))?;
            keyboard_stack.extend(self.keyboard_stack.iter().cloned());

            return Ok(StagedScreenCheckpoint {
                resident_lines: self
                    .lines
                    .iter()
                    .map(Line::semantic_checkpoint_clone)
                    .collect(),
                resident_oldest,
                cold_snapshot_generation: recovery.expected_generation,
                cold_prefix_line_count,
                allow_scrollback: self.allow_scrollback,
                keyboard_stack,
                physical_rows: self.physical_rows,
                physical_cols: self.physical_cols,
                dpi: self.dpi,
                saved_cursor: self.saved_cursor.clone(),
                cold_source: None,
            });
        }

        let cold_source = if self.allow_scrollback {
            if let Some(sink) = self.config.scrollback_spill_sink() {
                let expected_newest_exclusive =
                    StableRowIndex::try_from(resident_oldest).map_err(|_| {
                        ScreenCheckpointCaptureError::ColdScrollbackMetadataInconsistent
                    })?;
                let max_cold_bytes =
                    u64::try_from(limits.max_cold_scrollback_bytes).map_err(|_| {
                        ScreenCheckpointCaptureError::ArithmeticOverflow("cold_scrollback_bytes")
                    })?;
                let (before_interval, fragments) = if let Some(fragments) = &self.cold_row_fragments
                {
                    let crate::config::ScrollbackIntervalCapture::Ready(before) =
                        sink.try_capture_scrollback_interval()
                    else {
                        return Err(
                            ScreenCheckpointCaptureError::ColdScrollbackMetadataInconsistent,
                        );
                    };
                    if !Self::fragments_match_interval(fragments, &sink, &before) {
                        return Err(
                            ScreenCheckpointCaptureError::ColdScrollbackMetadataInconsistent,
                        );
                    }
                    (before, Some(Arc::clone(fragments)))
                } else {
                    let crate::config::ScrollbackIntervalCapture::Ready(before) =
                        sink.try_capture_scrollback_interval()
                    else {
                        return Err(
                            ScreenCheckpointCaptureError::ColdScrollbackMetadataInconsistent,
                        );
                    };
                    (before, None)
                };
                Some(StagedColdSource {
                    sink,
                    before_interval,
                    expected_newest_exclusive,
                    max_cold_bytes,
                    fragments,
                })
            } else {
                None
            }
        } else {
            None
        };

        let mut keyboard_stack = Vec::new();
        keyboard_stack
            .try_reserve_exact(self.keyboard_stack.len())
            .map_err(|_| ScreenCheckpointCaptureError::ResourceAllocation("keyboard_stack"))?;
        keyboard_stack.extend(self.keyboard_stack.iter().cloned());

        Ok(StagedScreenCheckpoint {
            resident_lines: self
                .lines
                .iter()
                .map(Line::semantic_checkpoint_clone)
                .collect(),
            resident_oldest,
            cold_snapshot_generation: None,
            cold_prefix_line_count: 0,
            allow_scrollback: self.allow_scrollback,
            keyboard_stack,
            physical_rows: self.physical_rows,
            physical_cols: self.physical_cols,
            dpi: self.dpi,
            saved_cursor: self.saved_cursor.clone(),
            cold_source,
        })
    }

    #[cfg(feature = "use_serde")]
    pub(crate) fn checkpoint_parts(
        &self,
        limits: &ScreenCheckpointLimits,
        usage: &mut ScreenCheckpointUsage,
    ) -> Result<ScreenCheckpointParts, ScreenCheckpointCaptureError> {
        self.checkpoint_parts_staged(limits, usage)?
            .materialize(limits, usage)
    }
}

#[cfg(feature = "use_serde")]
impl StagedScreenCheckpoint {
    pub(crate) fn materialize(
        self,
        limits: &ScreenCheckpointLimits,
        usage: &mut ScreenCheckpointUsage,
    ) -> Result<ScreenCheckpointParts, ScreenCheckpointCaptureError> {
        let (oldest, cold_snapshot_generation, cold_prefix_line_count, mut cold_lines) =
            if let Some(cold) = self.cold_source {
                let snapshot = cold
                    .sink
                    .snapshot_scrollback(
                        cold.expected_newest_exclusive,
                        ScrollbackSnapshotLimits {
                            max_rows: limits.max_total_lines.saturating_sub(usage.lines),
                            max_stored_bytes: cold.max_cold_bytes,
                            max_decoded_bytes: limits
                                .max_retained_capture_bytes
                                .saturating_sub(usage.retained_capture_bytes),
                            max_physical_bytes: cold.max_cold_bytes,
                        },
                    )
                    .map_err(ScreenCheckpointCaptureError::ColdScrollbackSnapshot)?;
                if !snapshot.rows().is_empty()
                    && snapshot.fidelity() != ScrollbackSnapshotFidelity::ExactSemantic
                {
                    return Err(ScreenCheckpointCaptureError::ColdScrollbackNotRecoveryGrade);
                }
                if snapshot.newest_stable_row_exclusive() != cold.expected_newest_exclusive
                    || snapshot.stored_bytes() > cold.max_cold_bytes
                    || snapshot.decoded_bytes()
                        > limits
                            .max_retained_capture_bytes
                            .saturating_sub(usage.retained_capture_bytes)
                {
                    return Err(ScreenCheckpointCaptureError::ColdScrollbackMetadataInconsistent);
                }
                let oldest = match (snapshot.oldest_stable_row(), snapshot.rows().is_empty()) {
                    (None, true) => self.resident_oldest,
                    (Some(oldest), false) => usize::try_from(oldest).map_err(|_| {
                        ScreenCheckpointCaptureError::ColdScrollbackMetadataInconsistent
                    })?,
                    _ => {
                        return Err(
                            ScreenCheckpointCaptureError::ColdScrollbackMetadataInconsistent,
                        );
                    }
                };
                if oldest > self.resident_oldest
                    || self.resident_oldest.checked_sub(oldest) != Some(snapshot.rows().len())
                {
                    return Err(ScreenCheckpointCaptureError::ColdScrollbackMetadataInconsistent);
                }
                let generation = snapshot.generation();
                let cold_prefix_line_count = snapshot.rows().len();

                let crate::config::ScrollbackIntervalCapture::Ready(after) =
                    cold.sink.try_capture_scrollback_interval()
                else {
                    return Err(ScreenCheckpointCaptureError::ColdScrollbackMetadataInconsistent);
                };
                if !after.same_lineage(&cold.before_interval) {
                    return Err(ScreenCheckpointCaptureError::ColdScrollbackMetadataInconsistent);
                }
                let oldest_stable = StableRowIndex::try_from(oldest).map_err(|_| {
                    ScreenCheckpointCaptureError::ColdScrollbackMetadataInconsistent
                })?;
                if cold_prefix_line_count > 0
                    && (!after.retains(
                        &cold.before_interval,
                        oldest_stable..cold.expected_newest_exclusive,
                    ) || cold
                        .before_interval
                        .rows()
                        .is_none_or(|rows| rows.start != oldest_stable)
                        || after.rows().is_none_or(|rows| rows.start != oldest_stable))
                {
                    return Err(ScreenCheckpointCaptureError::ColdScrollbackMetadataInconsistent);
                }

                let mut cold_lines = snapshot.into_rows();
                for (index, line) in cold_lines.iter_mut().enumerate() {
                    let stable_idx =
                        match oldest_stable.checked_add(index as StableRowIndex) {
                            Some(idx) => idx,
                            None => return Err(
                                ScreenCheckpointCaptureError::ColdScrollbackMetadataInconsistent,
                            ),
                        };
                    if let Some(replacement) = cold
                        .fragments
                        .as_ref()
                        .and_then(|fragments| fragments.rows.get(&stable_idx))
                    {
                        inspect_checkpoint_line(replacement, limits, usage)?;
                        *line = replacement.semantic_checkpoint_clone();
                    } else {
                        inspect_checkpoint_line(line, limits, usage)?;
                        *line = line.semantic_checkpoint_clone();
                    }
                }
                (oldest, Some(generation), cold_prefix_line_count, cold_lines)
            } else {
                (
                    self.resident_oldest,
                    self.cold_snapshot_generation,
                    self.cold_prefix_line_count,
                    Vec::new(),
                )
            };

        let total_lines = cold_lines
            .len()
            .checked_add(self.resident_lines.len())
            .ok_or(ScreenCheckpointCaptureError::ArithmeticOverflow(
                "screen_lines",
            ))?;
        if usage.lines > limits.max_total_lines {
            return Err(ScreenCheckpointCaptureError::ResourceLimit {
                resource: "screen_lines",
                observed: usage.lines,
                maximum: limits.max_total_lines,
            });
        }

        let mut lines = Vec::new();
        lines
            .try_reserve_exact(total_lines)
            .map_err(|_| ScreenCheckpointCaptureError::ResourceAllocation("screen_lines"))?;
        lines.append(&mut cold_lines);
        lines.extend(self.resident_lines);

        Ok(ScreenCheckpointParts {
            lines,
            stable_row_index_offset: oldest,
            cold_snapshot_generation,
            cold_prefix_line_count,
            allow_scrollback: self.allow_scrollback,
            keyboard_stack: self.keyboard_stack,
            physical_rows: self.physical_rows,
            physical_cols: self.physical_cols,
            dpi: self.dpi,
            saved_cursor: self.saved_cursor,
        })
    }
}

impl Screen {
    /// Rebuild a screen from parts that have already passed the terminal
    /// checkpoint validator.  Only semantic state is installed; all caches,
    /// workers, scorecards, telemetry, and configuration-derived policies come
    /// from a fresh `Screen::new` instance.
    #[cfg(feature = "use_serde")]
    pub(crate) fn from_validated_checkpoint_parts(
        parts: ScreenCheckpointParts,
        config: &Arc<dyn TerminalConfiguration>,
        seqno: SequenceNo,
        bidi_mode: BidiMode,
    ) -> Result<Self, ScreenCheckpointCaptureError> {
        let oldest = StableRowIndex::try_from(parts.stable_row_index_offset)
            .map_err(|_| ScreenCheckpointCaptureError::ColdScrollbackMetadataInconsistent)?;
        let cold_prefix_line_count = StableRowIndex::try_from(parts.cold_prefix_line_count)
            .map_err(|_| ScreenCheckpointCaptureError::ColdScrollbackMetadataInconsistent)?;
        let original_cold_prefix_newest_exclusive = oldest
            .checked_add(cold_prefix_line_count)
            .ok_or(ScreenCheckpointCaptureError::ColdScrollbackMetadataInconsistent)?;
        if parts.cold_prefix_line_count > parts.lines.len().saturating_sub(parts.physical_rows)
            || (!parts.allow_scrollback
                && (parts.cold_prefix_line_count != 0 || parts.cold_snapshot_generation.is_some()))
        {
            return Err(ScreenCheckpointCaptureError::ColdScrollbackMetadataInconsistent);
        }
        let size = TerminalSize {
            rows: parts.physical_rows,
            cols: parts.physical_cols,
            pixel_width: 0,
            pixel_height: 0,
            dpi: parts.dpi,
        };
        let mut screen =
            Self::try_new(size, config, parts.allow_scrollback, seqno, bidi_mode, true)
                .map_err(|_| ScreenCheckpointCaptureError::ResourceAllocation("screen"))?;
        screen.lines = parts.lines.into();
        screen.stable_row_index_offset = parts.stable_row_index_offset;
        screen.recovery_scrollback = Some(RecoveryScrollbackBoundary {
            expected_generation: parts.cold_snapshot_generation,
            original_cold_prefix_newest_exclusive,
        });
        screen.keyboard_stack = parts.keyboard_stack;
        screen.saved_cursor = parts.saved_cursor;
        Ok(screen)
    }

    /// Create a new Screen with the specified dimensions.
    /// The Cells in the viewable portion of the screen are set to the
    /// default cell attributes.
    pub fn new(
        size: TerminalSize,
        config: &Arc<dyn TerminalConfiguration>,
        allow_scrollback: bool,
        seqno: SequenceNo,
        bidi_mode: BidiMode,
    ) -> Screen {
        Self::try_new(size, config, allow_scrollback, seqno, bidi_mode, false)
            // ubs:ignore[rust.ownership.panic-macro] — This infallible constructor cannot return a partially allocated screen; recovery uses the fallible constructor.
            .unwrap_or_else(|()| panic!("unable to allocate terminal screen"))
    }

    fn try_new(
        size: TerminalSize,
        config: &Arc<dyn TerminalConfiguration>,
        allow_scrollback: bool,
        seqno: SequenceNo,
        bidi_mode: BidiMode,
        checkpoint_restore: bool,
    ) -> Result<Screen, ()> {
        let physical_rows = size.rows.max(1);
        let physical_cols = size.cols.max(1);

        let capacity = if checkpoint_restore {
            0
        } else {
            physical_rows
                .checked_add(scrollback_hot_size(config, allow_scrollback))
                .ok_or(())?
        };
        let mut lines = VecDeque::new();
        lines.try_reserve_exact(capacity).map_err(|_| ())?;
        if !checkpoint_restore {
            for _ in 0..physical_rows {
                let mut line = Line::new(seqno);
                bidi_mode.apply_to_line(&mut line, seqno);
                lines.push_back(line);
            }
        }

        Ok(Screen {
            lines,
            config: Arc::clone(config),
            scrollback_tiering: ScrollbackTieringState::default(),
            recovery_scrollback: None,
            allow_scrollback,
            physical_rows,
            physical_cols,
            stable_row_index_offset: 0,
            coordinate_identity: ScreenCoordinateIdentity::default(),
            selection_anchors: SelectionAnchorRegistry::default(),
            #[cfg(feature = "use_serde")]
            cold_visual_layout: None,
            #[cfg(feature = "use_serde")]
            stored_physical_layout: None,
            #[cfg(feature = "use_serde")]
            cold_geometry_index: None,
            #[cfg(feature = "use_serde")]
            cold_row_fragments: None,
            #[cfg(feature = "use_serde")]
            cold_visual_seqno: 0,
            #[cfg(feature = "use_serde")]
            cold_source_observation: None,
            #[cfg(feature = "use_serde")]
            cold_index_budget_exhausted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            dpi: size.dpi,
            keyboard_stack: vec![],
            saved_cursor: None,
            rewrap_cache: None,
            rewrap_line_cache: HashMap::new(),
            rewrap_line_cache_order: VecDeque::new(),
            rewrap_scratch_slots: Vec::new(),
            rewrap_row_prefix_scratch: Vec::new(),
            rewrap_width_prefix_scratch: LineWrapWidthPrefixScratch::default(),
            cold_scrollback_worker: ColdScrollbackReflowWorker::default(),
            last_viewport_first_reflow_us: 0,
            resize_wrap_policy: ResizeWrapPolicy::from_terminal_configuration(config.as_ref()),
            last_resize_wrap_scorecard: None,
            last_resize_wrap_gate_payload: None,
            cursor_consistency_telemetry: CursorConsistencyTelemetry::default(),
            last_good_frame: None,
            last_good_frame_lifecycle: LastGoodFrameLifecycle::default(),
            #[cfg(test)]
            rewrap_line_cache_hits: 0,
            #[cfg(test)]
            forced_rollback_cause: None,
        })
    }

    pub fn full_reset(&mut self) {
        self.keyboard_stack.clear();
    }

    pub(crate) fn set_config(&mut self, config: &Arc<dyn TerminalConfiguration>) {
        let resize_wrap_policy = ResizeWrapPolicy::from_terminal_configuration(config.as_ref());
        self.install_prepared_config(config, resize_wrap_policy);
    }

    pub(crate) fn install_prepared_config(
        &mut self,
        config: &Arc<dyn TerminalConfiguration>,
        resize_wrap_policy: ResizeWrapPolicy,
    ) {
        self.invalidate_coordinate_witnesses();
        if self.resize_wrap_policy != resize_wrap_policy {
            // Width/DPI cache keys describe results under the previous policy.
            // Keep the logical content, but recompute wraps and quality evidence.
            if let Some(cache) = self.rewrap_cache.as_mut() {
                Arc::make_mut(cache).clear_wraps();
            }
            self.last_resize_wrap_scorecard = None;
            self.last_resize_wrap_gate_payload = None;
        }
        self.config = Arc::clone(config);
        self.resize_wrap_policy = resize_wrap_policy;
        self.clear_rewrap_line_cache();
    }

    #[cfg(feature = "use_serde")]
    fn validated_recovery_scrollback_boundary(
        &self,
    ) -> Result<RecoveryScrollbackBoundary, ScrollbackActivationError> {
        let recovery = self
            .recovery_scrollback
            .ok_or(ScrollbackActivationError::MissingRecoveryBoundary)?;
        let oldest = StableRowIndex::try_from(self.stable_row_index_offset)
            .map_err(|_| ScrollbackActivationError::InvalidRecoveryBoundary)?;
        let resident_count = StableRowIndex::try_from(self.lines.len())
            .map_err(|_| ScrollbackActivationError::InvalidRecoveryBoundary)?;
        let newest = oldest
            .checked_add(resident_count)
            .ok_or(ScrollbackActivationError::InvalidRecoveryBoundary)?;
        if recovery.original_cold_prefix_newest_exclusive < oldest
            || recovery.original_cold_prefix_newest_exclusive > newest
            || (!self.allow_scrollback
                && (recovery.original_cold_prefix_newest_exclusive != oldest
                    || recovery.expected_generation.is_some()))
        {
            return Err(ScrollbackActivationError::InvalidRecoveryBoundary);
        }
        Ok(recovery)
    }

    #[cfg(feature = "use_serde")]
    pub(crate) fn preflight_recovery_without_scrollback(
        &self,
    ) -> Result<(), ScrollbackActivationError> {
        self.validated_recovery_scrollback_boundary()?;
        if self.allow_scrollback {
            return Err(ScrollbackActivationError::InvalidRecoveryBoundary);
        }
        Ok(())
    }

    #[cfg(feature = "use_serde")]
    pub(crate) fn preflight_recovery_checkpoint_boundary(
        &self,
    ) -> Result<(), ScreenCheckpointCaptureError> {
        self.validated_recovery_scrollback_boundary()
            .map(|_| ())
            .map_err(|_| ScreenCheckpointCaptureError::ColdScrollbackMetadataInconsistent)
    }

    #[cfg(feature = "use_serde")]
    pub(crate) fn finish_recovery_without_scrollback(&mut self) {
        debug_assert!(!self.allow_scrollback);
        self.recovery_scrollback = None;
    }

    /// Atomically publish the exact desired cold prefix before removing any
    /// recovered resident row. Every fallible calculation and sink operation
    /// precedes the in-memory split, so an error leaves the complete model
    /// available for retry.
    #[cfg(feature = "use_serde")]
    pub(crate) fn activate_recovered_scrollback(
        &mut self,
        live_config: &Arc<dyn TerminalConfiguration>,
    ) -> Result<(), ScrollbackActivationError> {
        let recovery = self.validated_recovery_scrollback_boundary()?;
        if !self.allow_scrollback {
            return Err(ScrollbackActivationError::InvalidRecoveryBoundary);
        }

        let resident_scrollback = self.lines.len().saturating_sub(self.physical_rows);
        let configured_scrollback = live_config.scrollback_size();
        if resident_scrollback > configured_scrollback {
            return Err(ScrollbackActivationError::ConfiguredRetentionInsufficient);
        }

        let tier = live_config.scrollback_tier_config();
        let desired_hot_rows = if tier.enabled {
            scrollback_hot_size(live_config, true)
        } else {
            resident_scrollback
        };
        let cold_prefix_line_count = resident_scrollback.saturating_sub(desired_hot_rows);
        let max_retained_rows = if tier.enabled {
            configured_scrollback.saturating_sub(desired_hot_rows)
        } else {
            0
        };
        if cold_prefix_line_count > max_retained_rows {
            return Err(ScrollbackActivationError::ConfiguredRetentionInsufficient);
        }

        let new_resident_oldest = self
            .stable_row_index_offset
            .checked_add(cold_prefix_line_count)
            .ok_or(ScrollbackActivationError::InvalidRecoveryBoundary)?;
        let newest_stable_row_exclusive = StableRowIndex::try_from(new_resident_oldest)
            .map_err(|_| ScrollbackActivationError::InvalidRecoveryBoundary)?;
        let oldest_stable_row = if cold_prefix_line_count == 0 {
            None
        } else {
            Some(
                StableRowIndex::try_from(self.stable_row_index_offset)
                    .map_err(|_| ScrollbackActivationError::InvalidRecoveryBoundary)?,
            )
        };

        let sink = live_config.scrollback_spill_sink();
        if sink.is_none()
            && (recovery.expected_generation.is_some()
                || cold_prefix_line_count != 0
                || (tier.enabled && max_retained_rows != 0))
        {
            return Err(ScrollbackActivationError::MissingStorageCapability);
        }

        let next_coordinate_identity = ScreenCoordinateIdentity::default();
        if let Some(sink) = sink {
            let (first, second) = self.lines.as_slices();
            let first_count = first.len().min(cold_prefix_line_count);
            let second_count = cold_prefix_line_count.saturating_sub(first_count);
            let prefix = ScrollbackPrefix::from_slices(
                oldest_stable_row,
                newest_stable_row_exclusive,
                &first[..first_count],
                &second[..second_count],
            )
            .map_err(ScrollbackActivationError::Spill)?;
            let commit = sink
                .replace_scrollback_prefix(recovery.expected_generation, prefix, max_retained_rows)
                .map_err(ScrollbackActivationError::Spill)?;
            let generation_is_valid = match recovery.expected_generation {
                Some(expected) => {
                    commit.generation().content_epoch() == expected.content_epoch()
                        && expected
                            .revision()
                            .checked_add(1)
                            .is_some_and(|revision| commit.generation().revision() == revision)
                }
                None => commit.generation().revision() == 1,
            };
            if !generation_is_valid
                || commit.oldest_stable_row() != oldest_stable_row
                || commit.newest_stable_row_exclusive() != newest_stable_row_exclusive
            {
                return Err(ScrollbackActivationError::CommitOutcomeIndeterminate);
            }
        }

        // The sink receipt is now verified. These operations are allocation
        // free and infallible: publish-before-remove is the activation commit
        // order, and no later step may strand the only copy of a row.
        self.coordinate_identity = next_coordinate_identity;
        self.lines.drain(..cold_prefix_line_count);
        self.stable_row_index_offset = new_resident_oldest;
        self.scrollback_tiering.reset();
        self.recovery_scrollback = None;
        Ok(())
    }

    #[cfg(any(test, feature = "use_serde"))]
    pub(crate) fn resize_wrap_policy(&self) -> ResizeWrapPolicy {
        self.resize_wrap_policy
    }

    #[cfg(test)]
    pub(crate) fn set_resize_wrap_policy(&mut self, policy: ResizeWrapPolicy) {
        let config = Arc::clone(&self.config);
        self.install_prepared_config(&config, policy);
    }

    #[cfg(test)]
    pub(crate) fn last_resize_wrap_gate_payload(&self) -> Option<&str> {
        self.last_resize_wrap_gate_payload.as_deref()
    }

    #[cfg(test)]
    fn force_resize_commit_rollback(&mut self, cause: LastGoodFrameRollbackCause) {
        self.forced_rollback_cause = Some(cause);
    }

    fn hot_scrollback_size(&self) -> usize {
        scrollback_hot_size(&self.config, self.allow_scrollback)
    }

    fn tiered_scrollback_warm_max_bytes(&self) -> usize {
        if !self.allow_scrollback {
            return 0;
        }
        let tier = self.config.scrollback_tier_config();
        if !tier.enabled {
            return 0;
        }
        tier.warm_max_bytes.min(SCROLLBACK_WARM_MAX_BYTES_CAP)
    }

    fn cold_sink_retention_rows(&self) -> usize {
        if !self.allow_scrollback {
            return 0;
        }
        self.config
            .scrollback_size()
            .saturating_sub(self.hot_scrollback_size())
    }

    fn cold_sink_retained_rows(&self) -> usize {
        self.config
            .scrollback_spill_sink()
            .map(|sink| sink.retained_scrollback_rows())
            .unwrap_or(0)
    }

    fn cold_sink_retained_bytes(&self) -> usize {
        self.config
            .scrollback_spill_sink()
            .map(|sink| sink.retained_scrollback_bytes())
            .unwrap_or(0)
    }

    fn oldest_reachable_stable_row(&self) -> StableRowIndex {
        #[cfg(feature = "use_serde")]
        if let Some(layout) = self.current_cold_visual_layout() {
            return layout.visual.start;
        }
        let hot_top = self.phys_to_stable_row_index(0);
        let Some(sink) = self.config.scrollback_spill_sink() else {
            return hot_top;
        };
        let oldest = match sink.try_capture_scrollback_interval() {
            crate::config::ScrollbackIntervalCapture::Ready(interval) => {
                interval.rows().map(|rows| rows.start)
            }
            crate::config::ScrollbackIntervalCapture::Busy => {
                #[cfg(feature = "use_serde")]
                {
                    if let Some((before_sink, interval)) = &self.cold_source_observation {
                        if Arc::ptr_eq(before_sink, &sink) {
                            interval.rows().map(|rows| rows.start)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                #[cfg(not(feature = "use_serde"))]
                None
            }
            crate::config::ScrollbackIntervalCapture::Unavailable => None,
        };
        oldest.unwrap_or(hot_top).min(hot_top)
    }

    fn estimate_line_bytes(line: &Line) -> usize {
        line.len()
            .saturating_mul(std::mem::size_of::<Cell>())
            .max(std::mem::size_of::<Line>())
    }

    fn apply_cold_spill_outcome(
        &mut self,
        seqno: SequenceNo,
        spill_outcome: ScrollbackSpillOutcome,
        reason: &'static str,
    ) {
        if spill_outcome.cold_lines_evicted == 0 {
            return;
        }
        if self
            .config
            .scrollback_spill_sink()
            .is_some_and(|sink| sink.requires_scrollback_flush())
        {
            // Queue admission is only a memory transfer. The deferred adapter
            // reports actual durable acknowledgements after its backing write;
            // never manufacture worker completions here before that happens.
            return;
        }

        self.cold_scrollback_worker
            .begin_intent(seqno, spill_outcome.cold_lines_evicted);
        self.cold_scrollback_worker
            .complete_cold_batch(seqno, spill_outcome.cold_lines_evicted);
        self.cold_scrollback_worker.finish_intent(
            seqno,
            Duration::from_millis(1),
            spill_outcome.cold_lines_evicted,
        );
        debug!(
            "tiered_scrollback_cold_spill reason={} seqno={} cold_lines_evicted={} cold_bytes_evicted={} warm_resident_lines={} warm_resident_bytes={}",
            reason,
            seqno,
            spill_outcome.cold_lines_evicted,
            spill_outcome.cold_bytes_evicted,
            self.scrollback_tiering.warm_resident_lines(),
            self.scrollback_tiering.warm_bytes
        );
    }

    fn record_scrollback_spill(
        &mut self,
        stable_row: StableRowIndex,
        line: &Line,
        seqno: SequenceNo,
    ) -> bool {
        if !self.allow_scrollback {
            return true;
        }
        if self.hot_scrollback_size() == 0 {
            return true;
        }
        let tier = self.config.scrollback_tier_config();
        if !tier.enabled {
            return true;
        }
        let max_retained_rows = self.cold_sink_retention_rows();
        if max_retained_rows > 0 {
            let Some(sink) = self.config.scrollback_spill_sink() else {
                return false;
            };
            #[cfg(not(feature = "use_serde"))]
            if !sink.store_scrollback_line(stable_row, line, max_retained_rows) {
                return false;
            }
            #[cfg(feature = "use_serde")]
            match sink.store_scrollback_line_with_receipt(stable_row, line, max_retained_rows) {
                crate::config::ScrollbackLineAdmission::Refused => return false,
                crate::config::ScrollbackLineAdmission::Admitted {
                    interval: Some(interval),
                } => {
                    self.advance_cold_seam_alignment(&sink, &interval, stable_row);
                    self.record_cold_geometry_row(
                        Arc::clone(&sink),
                        interval.clone(),
                        stable_row,
                        line,
                    );
                    self.record_stored_physical_row(sink, interval, stable_row, line);
                }
                crate::config::ScrollbackLineAdmission::Admitted { interval: None } => {
                    self.stored_physical_layout = None;
                    self.cold_geometry_index = None;
                }
            }
        }
        let line_bytes = Self::estimate_line_bytes(line);
        let spill_outcome = self
            .scrollback_tiering
            .record_spill(line_bytes, self.tiered_scrollback_warm_max_bytes());
        self.apply_cold_spill_outcome(seqno, spill_outcome, "budget_overflow");
        true
    }

    #[cfg(feature = "use_serde")]
    fn advance_cold_seam_alignment(
        &mut self,
        sink: &Arc<dyn crate::config::ScrollbackSpillSink>,
        interval: &crate::config::ScrollbackInterval,
        row: StableRowIndex,
    ) {
        let Some(end) = row.checked_add(1) else {
            return;
        };
        let Some(fragments) = self.cold_row_fragments.as_mut() else {
            return;
        };
        if !Arc::ptr_eq(&fragments.sink, sink)
            || !fragments.aligned_at(row, self.physical_cols, self.dpi, self.resize_wrap_policy)
            || !interval.retains(&fragments.interval, fragments.aligned_source_start..row)
            || !interval.rows().is_some_and(|rows| rows.end == end)
        {
            return;
        }
        // The admitted row was already laid out on the resident side of this
        // exact seam. Moving it to the sink does not require reflowing its
        // continuation. Keep prior readers' certificates immutable: changing
        // their frontier would authorize cells they never captured.
        // The immutable row map is shared, so this copies only bounded metadata.
        let fragments = Arc::make_mut(fragments);
        fragments.aligned_frontier = end;
        fragments.interval = interval.clone();
    }

    #[cfg(feature = "use_serde")]
    fn record_cold_geometry_row(
        &mut self,
        sink: Arc<dyn crate::config::ScrollbackSpillSink>,
        interval: crate::config::ScrollbackInterval,
        row: StableRowIndex,
        line: &Line,
    ) {
        let extends = self.cold_geometry_index.as_ref().is_some_and(|index| {
            Arc::ptr_eq(&sink, &index.sink)
                && interval.same_lineage(&index.interval)
                && index.source.end == row
        });
        if !extends {
            self.cold_geometry_index = Some(ColdGeometryIndex {
                sink,
                interval: interval.clone(),
                source: row..row,
                rows: VecDeque::new(),
                geometry_bytes: 0,
            });
        }
        let index = self.cold_geometry_index.as_mut().unwrap();
        index.interval = interval;
        if index.append(row, line).is_none() {
            self.cold_geometry_index = None;
        }
    }

    #[cfg(feature = "use_serde")]
    fn record_stored_physical_row(
        &mut self,
        sink: Arc<dyn crate::config::ScrollbackSpillSink>,
        interval: crate::config::ScrollbackInterval,
        row: StableRowIndex,
        line: &Line,
    ) {
        let Some(end) = row.checked_add(1) else {
            self.stored_physical_layout = None;
            return;
        };
        if self.cold_row_fragments.is_some()
            || !interval
                .rows()
                .is_some_and(|rows| rows.start <= row && rows.end == end)
        {
            self.stored_physical_layout = None;
            return;
        }
        let extends = self.stored_physical_layout.as_ref().is_some_and(|stored| {
            Arc::ptr_eq(&sink, &stored.sink)
                && self.matches_coordinate_witness(&stored.witness)
                && interval.same_lineage(&stored.interval)
                && stored.source.end == row
        });
        if !extends {
            self.stored_physical_layout = Some(StoredPhysicalLayout {
                sink,
                interval: interval.clone(),
                witness: self.capture_coordinate_witness(),
                source: row..row,
                groups: VecDeque::new(),
                open_tail: false,
            });
        }
        let stored = self.stored_physical_layout.as_mut().unwrap();
        stored.interval = interval;
        if !stored.append(row, line) {
            self.stored_physical_layout = None;
        }
    }

    /// Retry hot-tier overflow after a deferred sink has drained. Returns None
    /// when settled, or whether any row moved, so the parser drains again
    /// before accepting more input. Only scrollback is transferred; visible
    /// rows, recovery-owned rows and the model's dirty sequence are unchanged.
    pub(crate) fn trim_deferred_scrollback(&mut self, seqno: SequenceNo) -> Option<bool> {
        if !self.allow_scrollback
            || self.recovery_scrollback.is_some()
            || !self.config.scrollback_tier_config().enabled
            || !self
                .config
                .scrollback_spill_sink()
                .is_some_and(|sink| sink.requires_scrollback_flush())
        {
            return None;
        }
        let limit = self
            .physical_rows
            .saturating_add(self.hot_scrollback_size());
        let had_overflow = self.lines.len() > limit;
        let mut moved = false;
        while self.lines.len() > limit {
            let Some(line) = self.lines.pop_front() else {
                break;
            };
            let stable_row = self.stable_row_index_for_removed_top(0);
            if !self.record_scrollback_spill(stable_row, &line, seqno) {
                self.lines.push_front(line);
                break;
            }
            self.advance_stable_row_index_offset(1);
            moved = true;
        }
        had_overflow.then_some(moved)
    }

    /// Force all warm-tier residency into cold-tier accounting.
    ///
    /// This is the low-level primitive needed by fleet-level memory actions
    /// such as `EvictWarmScrollback` and emergency cleanup paths.
    pub fn evict_warm_scrollback(&mut self, seqno: SequenceNo) -> usize {
        if !self.allow_scrollback {
            return 0;
        }
        let tier = self.config.scrollback_tier_config();
        if !tier.enabled {
            return 0;
        }

        let spill_outcome = self.scrollback_tiering.evict_all_warm();
        let evicted = spill_outcome.cold_lines_evicted;
        self.apply_cold_spill_outcome(seqno, spill_outcome, "manual_evict");
        evicted
    }

    fn visible_frame_snapshot(&self) -> Vec<Line> {
        let start = self.lines.len().saturating_sub(self.physical_rows);
        self.lines
            .iter()
            .skip(start)
            .take(self.physical_rows)
            .cloned()
            .collect()
    }

    fn estimate_frame_bytes(lines: &[Line]) -> usize {
        lines
            .iter()
            .map(|line| {
                line.len()
                    .saturating_mul(std::mem::size_of::<Cell>())
                    .max(std::mem::size_of::<Line>())
            })
            .sum()
    }

    fn retained_frame_byte_budget(&self) -> usize {
        self.physical_rows
            .saturating_mul(self.physical_cols.max(1))
            .saturating_mul(std::mem::size_of::<Cell>())
            .saturating_mul(LAST_GOOD_FRAME_MAX_BYTES_MULTIPLIER.max(1))
    }

    fn invalidate_last_good_frame(
        &mut self,
        transition: LastGoodFrameTransition,
        seqno: Option<SequenceNo>,
    ) {
        if let Some(prior) = self.last_good_frame.take() {
            self.last_good_frame_lifecycle.invalidation_count = self
                .last_good_frame_lifecycle
                .invalidation_count
                .saturating_add(1);
            self.last_good_frame_lifecycle.current_retained_bytes = 0;
            let lineage_id = self
                .last_good_frame_lifecycle
                .last_lineage_id
                .saturating_add(1);
            self.last_good_frame_lifecycle.last_lineage_id = lineage_id;
            debug!(
                "last_good_frame_lineage id={} transition={} action=invalidate seqno={:?} prior_lineage_id={} prior_seqno={} prior_dims={}x{} prior_signature={} prior_bytes={} prior_lines={}",
                lineage_id,
                transition.as_str(),
                seqno,
                prior.lineage_id,
                prior.captured_seqno,
                prior.cols,
                prior.rows,
                prior.layout_signature,
                prior.estimated_bytes,
                prior.visible_lines.len()
            );
        }
    }

    fn retain_last_good_frame(&mut self, seqno: SequenceNo, transition: LastGoodFrameTransition) {
        let visible_lines = self.visible_frame_snapshot();
        let estimated_bytes = Self::estimate_frame_bytes(&visible_lines);
        let budget_bytes = self.retained_frame_byte_budget();
        self.last_good_frame_lifecycle.last_budget_bytes = budget_bytes;

        if estimated_bytes > budget_bytes {
            self.last_good_frame_lifecycle.drop_over_budget_count = self
                .last_good_frame_lifecycle
                .drop_over_budget_count
                .saturating_add(1);
            self.invalidate_last_good_frame(transition, Some(seqno));
            debug!(
                "last_good_frame_lineage id={} transition={} action=drop_over_budget seqno={} estimated_bytes={} budget_bytes={}",
                self.last_good_frame_lifecycle.last_lineage_id,
                transition.as_str(),
                seqno,
                estimated_bytes,
                budget_bytes
            );
            return;
        }

        let layout_signature = Self::compute_layout_signature_for_lines(visible_lines.iter());
        let prior_dims = self
            .last_good_frame
            .as_ref()
            .map(|frame| format!("{}x{}", frame.cols, frame.rows))
            .unwrap_or_else(|| "none".to_string());
        let prior_signature = self
            .last_good_frame
            .as_ref()
            .map(|frame| frame.layout_signature.to_string())
            .unwrap_or_else(|| "none".to_string());
        let action = if self.last_good_frame.is_some() {
            "replace"
        } else {
            "retain"
        };

        self.last_good_frame_lifecycle.capture_count = self
            .last_good_frame_lifecycle
            .capture_count
            .saturating_add(1);
        self.last_good_frame_lifecycle.current_retained_bytes = estimated_bytes;
        self.last_good_frame_lifecycle.peak_retained_bytes = self
            .last_good_frame_lifecycle
            .peak_retained_bytes
            .max(estimated_bytes);
        let lineage_id = self
            .last_good_frame_lifecycle
            .last_lineage_id
            .saturating_add(1);
        self.last_good_frame_lifecycle.last_lineage_id = lineage_id;
        self.last_good_frame = Some(LastGoodFrame {
            visible_lines,
            cols: self.physical_cols,
            rows: self.physical_rows,
            dpi: self.dpi,
            layout_signature,
            captured_seqno: seqno,
            estimated_bytes,
            lineage_id,
        });

        debug!(
            "last_good_frame_lineage id={} transition={} action={} seqno={} dims={}x{} signature={} bytes={} budget_bytes={} prior_dims={} prior_signature={}",
            lineage_id,
            transition.as_str(),
            action,
            seqno,
            self.physical_cols,
            self.physical_rows,
            layout_signature,
            estimated_bytes,
            budget_bytes,
            prior_dims,
            prior_signature
        );
    }

    fn rollback_to_last_good_frame(
        &mut self,
        seqno: SequenceNo,
        cause: LastGoodFrameRollbackCause,
    ) -> bool {
        let Some(frame) = self.last_good_frame.clone() else {
            self.last_good_frame_lifecycle
                .rollback_missing_snapshot_count = self
                .last_good_frame_lifecycle
                .rollback_missing_snapshot_count
                .saturating_add(1);
            warn!(
                "last_good_frame_rollback cause={} action=missing_snapshot seqno={}",
                cause.as_str(),
                seqno
            );
            return false;
        };

        let prior_rows = self.physical_rows;
        let prior_cols = self.physical_cols;
        let prior_dpi = self.dpi;
        let old_visible_start = self.lines.len().saturating_sub(prior_rows);
        self.lines.truncate(old_visible_start);
        self.lines.extend(frame.visible_lines.iter().cloned());
        self.physical_rows = frame.rows;
        self.physical_cols = frame.cols;
        self.dpi = frame.dpi;
        self.mark_visible_lines_dirty(seqno);
        self.last_good_frame_lifecycle.rollback_count = self
            .last_good_frame_lifecycle
            .rollback_count
            .saturating_add(1);
        debug!(
            "last_good_frame_rollback cause={} action=applied seqno={} prior_dims={}x{} prior_dpi={} restored_dims={}x{} restored_dpi={} lineage_id={} signature={} bytes={} visible_lines={}",
            cause.as_str(),
            seqno,
            prior_cols,
            prior_rows,
            prior_dpi,
            frame.cols,
            frame.rows,
            frame.dpi,
            frame.lineage_id,
            frame.layout_signature,
            frame.estimated_bytes,
            frame.visible_lines.len()
        );
        true
    }

    fn mark_visible_lines_dirty(&mut self, seqno: SequenceNo) {
        let start = self.lines.len().saturating_sub(self.physical_rows);
        for idx in start..self.lines.len() {
            self.lines[idx].update_last_change_seqno(seqno);
        }
    }

    fn blank_line_borrowing_bidi(&self, seqno: SequenceNo) -> Line {
        let mut line = Line::new(seqno);
        if let Some((enabled, hint)) = self.lines.back().map(Line::bidi_info) {
            line.set_bidi_info(enabled, hint, seqno);
        }
        line
    }

    fn wrap_single_logical_line_for_resize(
        line: Line,
        physical_cols: usize,
        seqno: SequenceNo,
        policy: ResizeWrapPolicy,
        width_prefix_scratch: &mut LineWrapWidthPrefixScratch,
    ) -> (Vec<Line>, Option<MonospaceLineWrapScorecard>) {
        if line.len() <= physical_cols {
            return (vec![line], None);
        }

        let layout = line.plan_wrap_with_width_prefix_scratch(
            physical_cols,
            policy.kp_cost_model,
            width_prefix_scratch,
        );
        let scorecard = policy.scorecard_enabled.then(|| layout.scorecard());
        (
            layout.deferred_rows(0..layout.row_count(), seqno),
            scorecard,
        )
    }

    #[cfg(feature = "use_serde")]
    fn wrap_cold_logical_line(
        logical: Line,
        cols: usize,
        seqno: SequenceNo,
        policy: ResizeWrapPolicy,
        incomplete_prefix: bool,
        max_rows: usize,
    ) -> Option<Vec<Line>> {
        if incomplete_prefix && logical.len() == 0 {
            return Some(Vec::new());
        }
        let mut rows = if logical.len() <= cols {
            if max_rows == 0 {
                return None;
            }
            vec![logical]
        } else {
            let mut scratch = LineWrapWidthPrefixScratch::default();
            let layout = if incomplete_prefix {
                logical.plan_wrap_preserving_trailing_spaces(
                    cols,
                    policy.kp_cost_model,
                    &mut scratch,
                )
            } else {
                logical.plan_wrap_with_width_prefix_scratch(
                    cols,
                    policy.kp_cost_model,
                    &mut scratch,
                )
            };
            // Natural word boundaries can leave spare columns. Only the actual
            // plan bounds output rows; ceil(cell_count / cols) cannot do so.
            // Refuse before allocating the deferred physical-row collection.
            if layout.row_count() > max_rows {
                return None;
            }
            layout.deferred_rows(0..layout.row_count(), seqno)
        };
        if incomplete_prefix {
            if let Some(last) = rows.last_mut() {
                last.set_last_cell_was_wrapped(true, seqno);
            }
        }
        Some(rows)
    }

    fn clear_rewrap_line_cache(&mut self) {
        self.rewrap_line_cache.clear();
        self.rewrap_line_cache_order.clear();
    }

    fn cached_rewrap_line(
        &self,
        key: WrapLineCacheKey,
    ) -> Option<(RewrapScratch, Option<MonospaceLineWrapScorecard>)> {
        if !FT_OSYAF_PERSISTENT_REFLOW_CHUNKS {
            return None;
        }

        self.rewrap_line_cache.get(&key).map(|cached| {
            (
                RewrapScratch::SharedLines(Arc::clone(&cached.lines)),
                cached.scorecard,
            )
        })
    }

    fn insert_rewrap_line_cache(&mut self, key: WrapLineCacheKey, cached: CachedWrappedLine) {
        if let Some(existing) = self.rewrap_line_cache.get_mut(&key) {
            *existing = cached;
            return;
        }

        while self.rewrap_line_cache.len() >= MAX_WRAP_LINE_CACHE_ENTRIES {
            let Some(evicted) = self.rewrap_line_cache_order.pop_front() else {
                break;
            };
            self.rewrap_line_cache.remove(&evicted);
        }

        self.rewrap_line_cache.insert(key, cached);
        self.rewrap_line_cache_order.push_back(key);
    }

    fn wrap_logical_line_source_for_resize<T>(
        logical_line: &T,
        physical_lines: &VecDeque<Line>,
        physical_cols: usize,
        seqno: SequenceNo,
        policy: ResizeWrapPolicy,
        width_prefix_scratch: &mut LineWrapWidthPrefixScratch,
    ) -> (RewrapScratch, Option<MonospaceLineWrapScorecard>)
    where
        T: ReflowLogicalLine,
    {
        let line = logical_line.line(physical_lines);
        if line.len() <= physical_cols {
            return (logical_line.scratch_for_unwrapped(physical_lines), None);
        }

        if let Some(retained) = logical_line.retained_wrap_source() {
            let mut initialized_for_request = false;
            let source = retained.get_or_init(|| {
                #[cfg(test)]
                REFLOW_RETAINED_SOURCE_BUILDS.with(|count| count.set(count.get() + 1));
                let layout = line
                    .clone()
                    .plan_wrap_with_width_prefix_scratch(
                        physical_cols,
                        policy.kp_cost_model,
                        width_prefix_scratch,
                    )
                    .retain_width_prefix();
                initialized_for_request = true;
                layout
            });
            // Only the caller whose initializer ran knows that this layout
            // already matches its width and cost policy. A concurrent caller
            // that waited for the same source must still plan its own request.
            let replanned;
            let layout = if initialized_for_request {
                source
            } else {
                #[cfg(test)]
                REFLOW_RETAINED_REPLAN_CALLS.with(|count| count.set(count.get() + 1));
                replanned =
                    source.replan(physical_cols, policy.kp_cost_model, width_prefix_scratch);
                &replanned
            };
            let scorecard = policy.scorecard_enabled.then(|| layout.scorecard());
            return (
                RewrapScratch::Lines(layout.deferred_rows(0..layout.row_count(), seqno)),
                scorecard,
            );
        }

        let line = logical_line.clone_line(physical_lines);
        let (lines, scorecard) = Self::wrap_single_logical_line_for_resize(
            line,
            physical_cols,
            seqno,
            policy,
            width_prefix_scratch,
        );
        (RewrapScratch::Lines(lines), scorecard)
    }

    fn wrap_logical_line_for_resize<T>(
        &mut self,
        logical_line: &T,
        physical_cols: usize,
        seqno: SequenceNo,
    ) -> (RewrapScratch, Option<MonospaceLineWrapScorecard>)
    where
        T: ReflowLogicalLine,
    {
        Self::wrap_logical_line_source_for_resize(
            logical_line,
            &self.lines,
            physical_cols,
            seqno,
            self.resize_wrap_policy,
            &mut self.rewrap_width_prefix_scratch,
        )
    }

    fn wrap_logical_line_for_resize_cached<T>(
        &mut self,
        logical_line: &T,
        physical_cols: usize,
        seqno: SequenceNo,
    ) -> (RewrapScratch, Option<MonospaceLineWrapScorecard>)
    where
        T: ReflowLogicalLine,
    {
        let line = logical_line.line(&self.lines);
        if line.len() <= physical_cols {
            return self.wrap_logical_line_for_resize(logical_line, physical_cols, seqno);
        }

        let key = WrapLineCacheKey::new(line, physical_cols, self.dpi, self.resize_wrap_policy);
        if let Some(cached) = self.cached_rewrap_line(key) {
            #[cfg(test)]
            {
                self.rewrap_line_cache_hits = self.rewrap_line_cache_hits.saturating_add(1);
            }
            return cached;
        }

        let (wrapped, scorecard) =
            self.wrap_logical_line_for_resize(logical_line, physical_cols, seqno);
        if let RewrapScratch::Lines(lines) = &wrapped {
            self.insert_rewrap_line_cache(
                key,
                CachedWrappedLine {
                    lines: Arc::from(lines.clone()),
                    scorecard,
                },
            );
        }
        (wrapped, scorecard)
    }

    fn compute_layout_signature_for_lines<'a, I>(lines: I) -> u64
    where
        I: IntoIterator<Item = &'a Line>,
    {
        let mut hasher = DefaultHasher::new();
        let mut line_count = 0usize;
        for (idx, line) in lines.into_iter().enumerate() {
            line_count = idx + 1;
            Self::hash_layout_line(&mut hasher, line);
        }
        line_count.hash(&mut hasher);
        // ubs:ignore[rust.security.non-crypto-random] — This process-local layout cache fingerprint is neither a credential nor a cryptographic identity.
        hasher.finish()
    }

    fn hash_layout_line(hasher: &mut DefaultHasher, line: &Line) {
        #[cfg(test)]
        REFLOW_LAYOUT_LINE_HASHES.with(|count| count.set(count.get() + 1));
        line.len().hash(hasher);
        line.last_cell_was_wrapped().hash(hasher);
        if reuse_unlinked_scan_state_for_reflow()
            && !line.has_hyperlink()
            && line.implicit_hyperlinks_are_scanned()
        {
            // A no-match scan changes no cell or layout attribute. Normalize
            // only that derived bit. Linked lines keep their exact hash, and
            // cached materialization below clears the no-match scan state so
            // a changed rule epoch can never inherit an old "already scanned".
            let mut unscanned = line.clone();
            unscanned.invalidate_implicit_hyperlinks(line.current_seqno());
            unscanned.compute_shape_hash().hash(hasher);
        } else {
            line.compute_shape_hash().hash(hasher);
        }
    }

    fn compute_layout_signature(&self) -> u64 {
        #[cfg(test)]
        REFLOW_FULL_LAYOUT_SIGNATURE_SCANS.with(|count| count.set(count.get() + 1));
        Self::compute_layout_signature_for_lines(self.lines.iter())
    }

    fn logical_line_physical_ranges(lines: &VecDeque<Line>) -> Vec<Range<usize>> {
        if lines.is_empty() {
            return Vec::new();
        }

        let mut ranges = Vec::new();
        let mut start = 0usize;
        for (idx, line) in lines.iter().enumerate() {
            if !line.last_cell_was_wrapped() {
                ranges.push(start..idx + 1);
                start = idx + 1;
            }
        }

        if start < lines.len() {
            ranges.push(start..lines.len());
        }

        ranges
    }

    fn append_batches_for_indices(
        indices: &[usize],
        priority: ReflowBatchPriority,
        batches: &mut Vec<ReflowBatchPlan>,
    ) {
        if indices.is_empty() {
            return;
        }

        let mut run_start = indices[0];
        let mut run_end = run_start + 1;
        for &idx in indices.iter().skip(1) {
            if idx == run_end {
                run_end += 1;
                continue;
            }

            let mut chunk_start = run_start;
            while chunk_start < run_end {
                let chunk_end = (chunk_start + MAX_REFLOW_BATCH_LOGICAL_LINES).min(run_end);
                batches.push(ReflowBatchPlan {
                    logical_range: chunk_start..chunk_end,
                    priority,
                });
                chunk_start = chunk_end;
            }

            run_start = idx;
            run_end = idx + 1;
        }

        let mut chunk_start = run_start;
        while chunk_start < run_end {
            let chunk_end = (chunk_start + MAX_REFLOW_BATCH_LOGICAL_LINES).min(run_end);
            batches.push(ReflowBatchPlan {
                logical_range: chunk_start..chunk_end,
                priority,
            });
            chunk_start = chunk_end;
        }
    }

    fn build_viewport_reflow_plan_from_ranges(
        logical_ranges: &[Range<usize>],
        visible_phys_range: Range<usize>,
        total_phys_rows: usize,
    ) -> ViewportReflowPlan {
        if logical_ranges.is_empty() {
            return ViewportReflowPlan::default();
        }

        let overscan_rows = visible_phys_range
            .len()
            .saturating_mul(REFLOW_OVERSCAN_ROW_MULTIPLIER)
            .min(REFLOW_OVERSCAN_ROW_CAP)
            .max(1);
        let overscan_start = visible_phys_range.start.saturating_sub(overscan_rows);
        let overscan_end = (visible_phys_range.end + overscan_rows).min(total_phys_rows);
        let overscan_range = overscan_start..overscan_end;

        let mut viewport = Vec::new();
        let mut near = Vec::new();
        let mut cold = Vec::new();

        for (logical_idx, phys_range) in logical_ranges.iter().enumerate() {
            if ranges_intersect(phys_range, &visible_phys_range) {
                viewport.push(logical_idx);
            } else if ranges_intersect(phys_range, &overscan_range) {
                near.push(logical_idx);
            } else {
                cold.push(logical_idx);
            }
        }

        let mut batches = Vec::new();
        Self::append_batches_for_indices(&viewport, ReflowBatchPriority::Viewport, &mut batches);
        Self::append_batches_for_indices(&near, ReflowBatchPriority::NearViewport, &mut batches);
        Self::append_batches_for_indices(&cold, ReflowBatchPriority::ColdScrollback, &mut batches);
        ViewportReflowPlan { batches }
    }

    fn build_viewport_reflow_plan_for_current_snapshot(
        &self,
        logical_count: usize,
    ) -> ViewportReflowPlan {
        if logical_count == 0 {
            return ViewportReflowPlan::default();
        }

        let logical_ranges = Self::logical_line_physical_ranges(&self.lines);
        self.build_viewport_reflow_plan_for_logical_ranges(&logical_ranges, logical_count)
    }

    fn build_viewport_reflow_plan_for_logical_ranges(
        &self,
        logical_ranges: &[Range<usize>],
        logical_count: usize,
    ) -> ViewportReflowPlan {
        if logical_count == 0 {
            return ViewportReflowPlan::default();
        }

        if logical_ranges.len() != logical_count {
            return ViewportReflowPlan::full_scan(logical_count);
        }

        let visible_start = self.lines.len().saturating_sub(self.physical_rows);
        let visible_range = visible_start..self.lines.len();
        Self::build_viewport_reflow_plan_from_ranges(
            &logical_ranges,
            visible_range,
            self.lines.len(),
        )
    }

    fn logical_cursor_from_physical(
        &self,
        cursor_x: usize,
        cursor_y: PhysRowIndex,
    ) -> Option<(usize, usize)> {
        #[cfg(test)]
        REFLOW_CURSOR_PREFIX_SCANS.with(|count| count.set(count.get() + 1));
        let mut logical_idx = 0usize;
        let mut prefix_len = 0usize;

        for (phys_idx, line) in self.lines.iter().enumerate() {
            if phys_idx == cursor_y {
                return Some((logical_idx, cursor_x.checked_add(prefix_len)?));
            }

            if line.last_cell_was_wrapped() {
                prefix_len = prefix_len.checked_add(line.len())?;
            } else {
                logical_idx = logical_idx.checked_add(1)?;
                prefix_len = 0;
            }
        }

        None
    }

    fn record_cursor_consistency_telemetry(
        &mut self,
        seqno: SequenceNo,
        cursor_x: usize,
        cursor_y: PhysRowIndex,
    ) {
        let (passed, reason) = if cursor_y >= self.lines.len() {
            (false, "cursor_phys_out_of_bounds")
        } else {
            // Every in-range physical row belongs to exactly one logical
            // range, including the final unterminated soft-wrapped range.
            // Computing the entire prefix merely to test existence cannot
            // detect another failure beyond the bounds check above.
            let stable_row = self.phys_to_stable_row_index(cursor_y);
            if self.stable_row_to_phys(stable_row) != Some(cursor_y) {
                (false, "stable_row_roundtrip_mismatch")
            } else {
                (true, "ok")
            }
        };

        self.cursor_consistency_telemetry.record(passed);
        debug!(
            "cursor_consistency seqno={:?} status={} reason={} cursor_x={} cursor_y={} checks_total={} checks_passed={} checks_failed={}",
            seqno,
            if passed { "pass" } else { "fail" },
            reason,
            cursor_x,
            cursor_y,
            self.cursor_consistency_telemetry.total_checks(),
            self.cursor_consistency_telemetry.checks_passed,
            self.cursor_consistency_telemetry.checks_failed
        );
    }

    #[cfg(test)]
    fn rebuild_logical_lines_from_physical(&self, seqno: SequenceNo) -> Vec<LogicalLineForResize> {
        self.rebuild_logical_lines_from_physical_inner(seqno, None)
            .logical_lines
    }

    fn rebuild_logical_lines_from_physical_with_ranges(
        &self,
        seqno: SequenceNo,
    ) -> (Vec<LogicalLineForResize>, Vec<Range<usize>>) {
        let rebuild = self.rebuild_logical_lines_from_physical_inner(seqno, None);
        (rebuild.logical_lines, rebuild.physical_ranges)
    }

    fn rebuild_logical_lines_from_physical_with_signature_and_ranges(
        &self,
        seqno: SequenceNo,
    ) -> (Vec<LogicalLineForResize>, u64, Vec<Range<usize>>) {
        #[cfg(test)]
        REFLOW_SOURCE_SIGNATURE_SCANS.with(|count| count.set(count.get() + 1));
        let mut hasher = DefaultHasher::new();
        let rebuild = self.rebuild_logical_lines_from_physical_inner(seqno, Some(&mut hasher));
        self.lines.len().hash(&mut hasher);
        // ubs:ignore[rust.security.non-crypto-random] — This tuple carries a process-local layout cache fingerprint, not a security token.
        (
            rebuild.logical_lines,
            hasher.finish(),
            rebuild.physical_ranges,
        )
    }

    fn rebuild_logical_lines_from_physical_inner(
        &self,
        seqno: SequenceNo,
        mut signature_hasher: Option<&mut DefaultHasher>,
    ) -> LogicalLineRebuild {
        let mut logical_lines: Vec<LogicalLineForResize> = Vec::with_capacity(self.lines.len());
        let mut physical_ranges: Vec<Range<usize>> = Vec::with_capacity(self.lines.len());
        let mut joined_until = 0usize;

        for (idx, physical_line) in self.lines.iter().enumerate() {
            if let Some(hasher) = signature_hasher.as_deref_mut() {
                Self::hash_layout_line(hasher, physical_line);
            }

            if idx < joined_until {
                continue;
            }

            if physical_line.last_cell_was_wrapped() {
                let end = self
                    .lines
                    .range(idx..)
                    .position(|line| !line.last_cell_was_wrapped())
                    .map_or(self.lines.len(), |offset| idx + offset + 1);
                let logical =
                    Line::try_join_deferred_logical_rows(self.lines.range(idx..end), seqno)
                        .map(LogicalLineForResize::Owned)
                        .unwrap_or_else(|| LogicalLineForResize::PhysicalRange {
                            range: idx..end,
                            seqno,
                            logical: std::sync::OnceLock::new(),
                        });
                // Only discover ranges here. Expensive first-use token
                // construction runs when the existing viewport-prioritized
                // wrap workers first access a range, under their admission
                // and cancellation batch bounds. Cache publication reuses it.
                logical_lines.push(logical);
                physical_ranges.push(idx..end);
                joined_until = end;
                continue;
            }

            logical_lines.push(LogicalLineForResize::PhysicalLine(idx));
            physical_ranges.push(idx..idx + 1);
        }

        LogicalLineRebuild {
            logical_lines,
            physical_ranges,
        }
    }

    fn prepare_rewrap_scratch_slots(&mut self, logical_count: usize) {
        if self.rewrap_scratch_slots.len() < logical_count {
            self.rewrap_scratch_slots
                .resize_with(logical_count, || None);
        }
        for slot in self.rewrap_scratch_slots.iter_mut().take(logical_count) {
            *slot = None;
        }
    }

    fn clone_wrapped_from_scratch(&self, logical_count: usize) -> Vec<Arc<[Line]>> {
        let mut wrapped = Vec::with_capacity(logical_count);
        for slot in self.rewrap_scratch_slots.iter().take(logical_count) {
            wrapped.push(
                slot.as_ref()
                    .expect("missing wrapped line result after planner")
                    .shared_chunk(&self.lines),
            );
        }
        wrapped
    }

    fn rebuild_rewrap_row_prefix_scratch_from_slots(&mut self, logical_count: usize) {
        #[cfg(test)]
        REFLOW_ROW_PREFIX_BUILDS.with(|count| count.set(count.get() + 1));
        self.rewrap_row_prefix_scratch.clear();
        self.rewrap_row_prefix_scratch
            .reserve(logical_count.saturating_add(1));
        self.rewrap_row_prefix_scratch.push(0);

        let mut total_rows = 0usize;
        for slot in self.rewrap_scratch_slots.iter().take(logical_count) {
            total_rows = total_rows.saturating_add(
                slot.as_ref()
                    .expect("missing wrapped line result after planner")
                    .row_count(),
            );
            self.rewrap_row_prefix_scratch.push(total_rows);
        }
    }

    // Preparation can omit its first source hash because exact source rows
    // authorize publication. Subsequent text-only lookup uses an exact COW
    // target witness; images and unpublished direct sources retain hashing.
    fn logical_wraps_for_resize(
        &mut self,
        physical_cols: usize,
        seqno: SequenceNo,
        retain_source_signature: bool,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Option<(WrappedResizeLines, usize, bool, bool, usize)> {
        if is_cancelled() {
            return None;
        }
        let wrap_key = WrapCacheKey {
            physical_cols,
            dpi: self.dpi,
        };
        let mut logical_cache_hit = false;
        let mut wrap_cache_hit = false;
        let mut cache = self.rewrap_cache.take();
        let published_target_matches = cache
            .as_ref()
            .is_some_and(|entry| entry.matches_published_target(&self.lines));
        let source_signature = if !published_target_matches
            && cache
                .as_ref()
                .is_some_and(|entry| entry.source_signature.is_some())
        {
            #[cfg(test)]
            REFLOW_SOURCE_SIGNATURE_SCANS.with(|count| count.set(count.get() + 1));
            Some(self.compute_layout_signature())
        } else {
            None
        };

        if let Some(entry) = cache.as_mut() {
            if published_target_matches
                || source_signature
                    // ubs:ignore[rust.security.constant-time-compare] — These are public layout cache fingerprints, not secret authentication material.
                    .is_some_and(|signature| entry.source_signature == Some(signature))
            {
                let entry = Arc::make_mut(entry);
                logical_cache_hit = true;
                let logical_count = entry.logical_lines.len();
                if let Some(mut wrapped) = entry.get_wrapped(wrap_key) {
                    wrap_cache_hit = true;
                    // Quality evidence belongs to this target layout, just as
                    // its cells do. A different cached width has different totals.
                    self.last_resize_wrap_scorecard = wrapped.scorecard.take();
                    self.last_resize_wrap_gate_payload = wrapped.gate_payload.take();
                    let cache_entries = entry.wrapped_by_key.len();
                    self.rewrap_cache = cache;
                    return Some((
                        WrappedResizeLines::Cached(wrapped),
                        logical_count,
                        logical_cache_hit,
                        wrap_cache_hit,
                        cache_entries,
                    ));
                }

                if !self.wrap_logical_lines_for_resize(
                    &entry.logical_lines,
                    physical_cols,
                    seqno,
                    Some(&self.build_viewport_reflow_plan_for_current_snapshot(logical_count)),
                    is_cancelled,
                ) {
                    return None;
                }
                entry.insert_wrapped(
                    wrap_key,
                    self.clone_wrapped_from_scratch(logical_count),
                    self.last_resize_wrap_scorecard.clone(),
                    self.last_resize_wrap_gate_payload.clone(),
                );
                let cache_entries = entry.wrapped_by_key.len();
                self.rewrap_cache = cache;
                return Some((
                    WrappedResizeLines::Scratch { logical_count },
                    logical_count,
                    logical_cache_hit,
                    wrap_cache_hit,
                    cache_entries,
                ));
            }
        }

        let (logical_lines, source_signature, logical_ranges) =
            if source_signature.is_some() || !retain_source_signature {
                let (logical_lines, logical_ranges) =
                    self.rebuild_logical_lines_from_physical_with_ranges(seqno);
                (logical_lines, source_signature, logical_ranges)
            } else {
                let (logical_lines, source_signature, logical_ranges) =
                    self.rebuild_logical_lines_from_physical_with_signature_and_ranges(seqno);
                (logical_lines, Some(source_signature), logical_ranges)
            };
        let logical_count = logical_lines.len();
        let reflow_plan =
            self.build_viewport_reflow_plan_for_logical_ranges(&logical_ranges, logical_count);
        if !self.wrap_logical_lines_for_resize(
            &logical_lines,
            physical_cols,
            seqno,
            Some(&reflow_plan),
            is_cancelled,
        ) {
            return None;
        }
        let cache_logical_lines = logical_lines
            .iter()
            .map(|line| line.clone_line(&self.lines))
            .collect();
        let mut new_cache = LogicalLineWrapCache::new(source_signature, cache_logical_lines);
        new_cache.insert_wrapped(
            wrap_key,
            self.clone_wrapped_from_scratch(logical_count),
            self.last_resize_wrap_scorecard.clone(),
            self.last_resize_wrap_gate_payload.clone(),
        );
        let cache_entries = new_cache.wrapped_by_key.len();
        self.rewrap_cache = Some(Arc::new(new_cache));
        Some((
            WrappedResizeLines::Scratch { logical_count },
            logical_count,
            logical_cache_hit,
            wrap_cache_hit,
            cache_entries,
        ))
    }

    fn needs_rewrap_for_width_change(&self, physical_cols: usize) -> bool {
        if physical_cols == self.physical_cols {
            return false;
        }

        if physical_cols > self.physical_cols {
            // Growing wider only needs reflow when we have soft-wrapped
            // logical lines to merge.
            self.lines.iter().any(Line::last_cell_was_wrapped)
        } else {
            // Shrinking may require adding wraps to long lines, and we
            // still need to preserve existing wrapped logical lines.
            self.lines
                .iter()
                .any(|line| line.last_cell_was_wrapped() || line.len() > physical_cols)
        }
    }

    fn wrap_logical_lines_for_resize<T>(
        &mut self,
        logical_lines: &[T],
        physical_cols: usize,
        seqno: SequenceNo,
        reflow_plan: Option<&ViewportReflowPlan>,
        is_cancelled: &dyn Fn() -> bool,
    ) -> bool
    where
        T: ReflowLogicalLine + Sync,
    {
        let compute_pool = (logical_lines.len() >= 256)
            .then(reflow_compute_pool)
            .flatten();
        self.wrap_logical_lines_with_compute_pool(
            logical_lines,
            physical_cols,
            seqno,
            reflow_plan,
            is_cancelled,
            compute_pool,
        )
    }

    fn wrap_logical_lines_with_compute_pool<T>(
        &mut self,
        logical_lines: &[T],
        physical_cols: usize,
        seqno: SequenceNo,
        reflow_plan: Option<&ViewportReflowPlan>,
        is_cancelled: &dyn Fn() -> bool,
        compute_pool: Option<&ReflowComputePool>,
    ) -> bool
    where
        T: ReflowLogicalLine + Sync,
    {
        let logical_count = logical_lines.len();
        if logical_count == 0 {
            return true;
        }

        let wrap_policy = self.resize_wrap_policy;
        let mut wrap_scorecard = wrap_policy
            .scorecard_enabled
            .then(ResizeWrapScorecard::default);
        self.prepare_rewrap_scratch_slots(logical_count);
        let fallback_plan;
        let plan = match reflow_plan {
            Some(plan) if plan.covers_each_logical_line_once(logical_count) => plan,
            _ => {
                fallback_plan = ViewportReflowPlan::full_scan(logical_count);
                &fallback_plan
            }
        };
        let cold_backlog_depth = plan
            .batches
            .iter()
            .filter(|batch| batch.priority == ReflowBatchPriority::ColdScrollback)
            .map(|batch| {
                batch
                    .logical_range
                    .end
                    .saturating_sub(batch.logical_range.start)
            })
            .sum::<usize>();
        self.cold_scrollback_worker
            .begin_intent(seqno, cold_backlog_depth);
        let cold_started = Instant::now();
        let viewport_reflow_started = Instant::now();
        let mut viewport_ready_recorded = false;
        let mut cold_lines_completed = 0usize;
        let mut cold_batches_completed = 0usize;

        // Do not initialize the pool for small screens. A retained admission
        // guard prevents concurrent panes from flooding its queue; each batch
        // below joins before the next cancellation checkpoint.
        let admission = compute_pool.and_then(|pool| pool.admission.try_lock().ok());
        let compute_pool = compute_pool.filter(|_| admission.is_some());
        let worker_count = compute_pool.map_or(1, |pool| pool.pool.current_num_threads());

        for (batch_idx, batch) in plan.batches.iter().enumerate() {
            if is_cancelled() {
                return false;
            }
            let batch_len = batch
                .logical_range
                .end
                .saturating_sub(batch.logical_range.start);
            if batch_len == 0 {
                continue;
            }

            if batch.priority == ReflowBatchPriority::ColdScrollback && !viewport_ready_recorded {
                self.last_viewport_first_reflow_us =
                    duration_micros_u64(viewport_reflow_started.elapsed());
                viewport_ready_recorded = true;
            }

            debug!(
                "reflow planner batch={} logical={}..{} priority={} rationale={}",
                batch_idx,
                batch.logical_range.start,
                batch.logical_range.end,
                batch.priority.as_str(),
                batch.priority.rationale()
            );

            let batch_workers = worker_count.min(batch_len / 8).max(1);
            if batch_workers <= 1 {
                for idx in batch.logical_range.clone() {
                    let logical_line = &logical_lines[idx];
                    let (wrapped, line_scorecard) = self.wrap_logical_line_for_resize_cached(
                        logical_line,
                        physical_cols,
                        seqno,
                    );
                    if let (Some(scorecard), Some(line_scorecard)) =
                        (wrap_scorecard.as_mut(), line_scorecard)
                    {
                        scorecard.record_line(line_scorecard);
                    }
                    self.rewrap_scratch_slots[idx] = Some(wrapped);
                }
                if batch.priority == ReflowBatchPriority::ColdScrollback {
                    self.cold_scrollback_worker
                        .complete_cold_batch(seqno, batch_len);
                    cold_lines_completed = cold_lines_completed.saturating_add(batch_len);
                    cold_batches_completed = cold_batches_completed.saturating_add(1);
                }
                continue;
            }

            let chunk_size = batch_len.div_ceil(batch_workers).max(1);
            let mut pending_line_cache_inserts = Vec::new();
            #[cfg(test)]
            let mut pending_line_cache_hits = 0usize;
            let mut results: Vec<_> = (0..batch_workers).map(|_| None).collect();
            compute_pool
                .expect("parallel reflow requires admission")
                .pool
                .in_place_scope(|scope| {
                    for (worker_idx, result) in results.iter_mut().enumerate() {
                        let start = batch.logical_range.start + worker_idx * chunk_size;
                        if start >= batch.logical_range.end {
                            break;
                        }
                        let end = (start + chunk_size).min(batch.logical_range.end);
                        let logical_slice = &logical_lines[start..end];
                        let physical_lines = &self.lines;
                        let line_cache = &self.rewrap_line_cache;
                        let dpi = self.dpi;
                        scope.spawn(move |_| {
                            let mut wrapped = Vec::with_capacity(end - start);
                            let mut width_prefix_scratch = LineWrapWidthPrefixScratch::default();
                            for (offset, line) in logical_slice.iter().enumerate() {
                                let idx = start + offset;
                                let cache_key = {
                                    let source_line = line.line(physical_lines);
                                    (source_line.len() > physical_cols).then(|| {
                                        WrapLineCacheKey::new(
                                            source_line,
                                            physical_cols,
                                            dpi,
                                            wrap_policy,
                                        )
                                    })
                                };
                                if let Some(key) = cache_key {
                                    if let Some(cached) = line_cache.get(&key) {
                                        wrapped.push((
                                            idx,
                                            RewrapScratch::SharedLines(Arc::clone(&cached.lines)),
                                            cached.scorecard,
                                            None,
                                            true,
                                        ));
                                        continue;
                                    }
                                }

                                let (wrapped_lines, line_scorecard) =
                                    Self::wrap_logical_line_source_for_resize(
                                        line,
                                        physical_lines,
                                        physical_cols,
                                        seqno,
                                        wrap_policy,
                                        &mut width_prefix_scratch,
                                    );
                                let cache_insert = match (cache_key, &wrapped_lines) {
                                    (Some(key), RewrapScratch::Lines(lines)) => Some((
                                        key,
                                        CachedWrappedLine {
                                            lines: Arc::from(lines.clone()),
                                            scorecard: line_scorecard,
                                        },
                                    )),
                                    _ => None,
                                };
                                wrapped.push((
                                    idx,
                                    wrapped_lines,
                                    line_scorecard,
                                    cache_insert,
                                    false,
                                ));
                            }
                            *result = Some(wrapped);
                        });
                    }
                });
            for result in results {
                for (idx, lines, line_scorecard, cache_insert, cache_hit) in
                    result.expect("joined reflow chunk must have a result")
                {
                    pending_line_cache_inserts.extend(cache_insert);
                    #[cfg(not(test))]
                    let _ = cache_hit;
                    #[cfg(test)]
                    if cache_hit {
                        pending_line_cache_hits = pending_line_cache_hits.saturating_add(1);
                    }
                    if let (Some(scorecard), Some(line_scorecard)) =
                        (wrap_scorecard.as_mut(), line_scorecard)
                    {
                        scorecard.record_line(line_scorecard);
                    }
                    self.rewrap_scratch_slots[idx] = Some(lines);
                }
            }
            for (key, cached) in pending_line_cache_inserts {
                self.insert_rewrap_line_cache(key, cached);
            }
            #[cfg(test)]
            {
                self.rewrap_line_cache_hits = self
                    .rewrap_line_cache_hits
                    .saturating_add(pending_line_cache_hits);
            }

            if batch.priority == ReflowBatchPriority::ColdScrollback {
                self.cold_scrollback_worker
                    .complete_cold_batch(seqno, batch_len);
                cold_lines_completed = cold_lines_completed.saturating_add(batch_len);
                cold_batches_completed = cold_batches_completed.saturating_add(1);
            }
        }

        if !viewport_ready_recorded {
            self.last_viewport_first_reflow_us =
                duration_micros_u64(viewport_reflow_started.elapsed());
        }

        self.cold_scrollback_worker.finish_intent(
            seqno,
            cold_started.elapsed(),
            cold_lines_completed,
        );
        debug!(
            "cold_scrollback_worker intent={:?} backlog_depth={} peak_backlog_depth={} completed_batches={} completed_lines={} throughput_lines_per_sec={} cancellation_count={}",
            seqno,
            cold_backlog_depth,
            self.cold_scrollback_worker.peak_backlog_depth(),
            cold_batches_completed,
            cold_lines_completed,
            self.cold_scrollback_worker
                .completion_throughput_lines_per_sec(),
            self.cold_scrollback_worker.cancellation_count()
        );

        for idx in 0..logical_count {
            if self.rewrap_scratch_slots[idx].is_some() {
                continue;
            }
            let (wrapped, line_scorecard) =
                self.wrap_logical_line_for_resize_cached(&logical_lines[idx], physical_cols, seqno);
            if let (Some(scorecard), Some(line_scorecard)) =
                (wrap_scorecard.as_mut(), line_scorecard)
            {
                scorecard.record_line(line_scorecard);
            }
            self.rewrap_scratch_slots[idx] = Some(wrapped);
        }

        if let Some(scorecard) = wrap_scorecard {
            let gate_status = scorecard.gate_status(wrap_policy.readability_gate);
            let payload = scorecard.to_machine_payload(wrap_policy.readability_gate);
            match gate_status {
                ResizeWrapGateStatus::Fail(_) => {
                    warn!("resize_wrap_scorecard_gate {}", payload);
                }
                _ => {
                    debug!("resize_wrap_scorecard_gate {}", payload);
                }
            }
            self.last_resize_wrap_scorecard = Some(scorecard);
            self.last_resize_wrap_gate_payload = Some(payload);
        } else {
            self.last_resize_wrap_scorecard = None;
            self.last_resize_wrap_gate_payload = None;
        }
        true
    }

    fn rewrap_lines(
        &mut self,
        physical_cols: usize,
        physical_rows: usize,
        cursor: (usize, PhysRowIndex),
        seqno: SequenceNo,
        verified: Option<VerifiedPreparedResize>,
        selection_anchors: &mut SelectionAnchorRegistry,
    ) -> (usize, PhysRowIndex) {
        let (cursor_x, cursor_y) = cursor;
        self.invalidate_coordinate_witnesses();
        let started = Instant::now();
        let old_cols = self.physical_cols;
        let original_len = self.lines.len();
        // Profiling-only stage attribution. Enable with
        // RUST_LOG=frankenterm_term::screen::reflow_profile=debug.
        // No extra clock reads are performed when this target is disabled.
        let profile_start =
            log::log_enabled!(target: "frankenterm_term::screen::reflow_profile", log::Level::Debug)
                .then(Instant::now);
        let logical_cursor = verified.as_ref().map_or_else(
            || self.logical_cursor_from_physical(cursor_x, cursor_y),
            |prepared| prepared.logical_cursor,
        );
        // These are the live points at commit, never the worker snapshot's
        // points. Mapping reads only row lengths/wrap flags, not cell payloads.
        let logical_anchors: Vec<_> = selection_anchors
            .0
            .iter()
            .map(|entry| {
                let normalized_start = entry.points[1].zip(entry.points[2]).map(|(start, end)| {
                    if (start.row, start.column) <= (end.row, end.column) {
                        1
                    } else {
                        2
                    }
                });
                std::array::from_fn::<_, 3, _>(|index| {
                    let point = entry.points[index];
                    point.and_then(|point| {
                        let row = self.stable_row_to_phys(point.row)?;
                        let (group, column) =
                            self.logical_cursor_from_physical(point.column.unwrap_or(0), row)?;
                        // BeforeZero starts include column zero; BeforeZero
                        // ends exclude it. Preserve that role through reversal.
                        let before = point.column.is_none() && normalized_start != Some(index);
                        Some((group, column, before))
                    })
                })
            })
            .collect();
        let cursor_elapsed = profile_start.map(|start| start.elapsed());
        let (wrapped, logical_count, logical_cache_hit, wrap_cache_hit, cache_entries) =
            if let Some(mut verified) = verified {
                // Exact source, cursor and policy validation already happened
                // under this same terminal lock, before trailing-blank pruning.
                // Reusing that target must retain its quality/scan semantics,
                // but does not need another full source-shape hash.
                self.last_resize_wrap_scorecard = verified.wrapped.scorecard.take();
                self.last_resize_wrap_gate_payload = verified.wrapped.gate_payload.take();
                (
                    WrappedResizeLines::Cached(verified.wrapped),
                    verified.logical_count,
                    true,
                    true,
                    verified.cache_entries,
                )
            } else {
                self.logical_wraps_for_resize(physical_cols, seqno, true, &|| false)
                    .expect("synchronous reflow cannot be cancelled")
            };
        let wraps_elapsed = profile_start.map(|start| start.elapsed());
        let cached_row_prefix = match &wrapped {
            WrappedResizeLines::Cached(wrapped) => Some(Arc::clone(wrapped.row_prefix())),
            WrappedResizeLines::Scratch { logical_count } => {
                self.rebuild_rewrap_row_prefix_scratch_from_slots(*logical_count);
                None
            }
        };
        let row_prefix = cached_row_prefix
            .as_deref()
            .unwrap_or(&self.rewrap_row_prefix_scratch);
        let mut adjusted_cursor = (cursor_x, cursor_y);
        let wrapped_count = row_prefix.last().copied().unwrap_or(0);
        let prefix_elapsed = profile_start.map(|start| start.elapsed());

        let required_capacity = wrapped_count.max(physical_rows);
        let mut pruned_rows = 0usize;
        self.lines = match wrapped {
            WrappedResizeLines::Cached(wrapped) => {
                let mut rewrapped = std::mem::take(&mut self.lines);
                rewrapped.clear();
                // reserve is relative to len (now zero), not existing capacity.
                rewrapped.reserve(required_capacity);
                #[cfg(test)]
                REFLOW_CACHED_RESERVED_CAPACITY.with(|capacity| capacity.set(rewrapped.capacity()));
                for chunk in wrapped.lines.iter() {
                    for line in chunk.iter() {
                        let mut line = line.clone();
                        line.update_last_change_seqno(seqno);
                        rewrapped.push_back(line);
                    }
                }
                rewrapped
            }
            WrappedResizeLines::Scratch { logical_count } => {
                let source_lines = std::mem::take(&mut self.lines);
                let source_capacity = source_lines.capacity();
                let mut source_iter = source_lines.into_iter().enumerate();
                let mut next_source = source_iter.next();
                let mut rewrapped = VecDeque::with_capacity(required_capacity.max(source_capacity));

                for slot in self.rewrap_scratch_slots.iter_mut().take(logical_count) {
                    let scratch = slot
                        .take()
                        .expect("missing wrapped line result after planner");
                    match scratch {
                        RewrapScratch::PhysicalLine(target_idx) => {
                            let mut moved_line = None;
                            while let Some((phys_idx, mut line)) = next_source.take() {
                                next_source = source_iter.next();
                                if phys_idx == target_idx {
                                    line.update_last_change_seqno(seqno);
                                    moved_line = Some(line);
                                    break;
                                }
                            }
                            rewrapped.push_back(
                                moved_line.expect("missing borrowed physical line during rewrap"),
                            );
                        }
                        RewrapScratch::Lines(lines) => {
                            for mut line in lines {
                                line.update_last_change_seqno(seqno);
                                rewrapped.push_back(line);
                            }
                        }
                        RewrapScratch::SharedLines(chunk) => {
                            for line in chunk.iter() {
                                let mut line = line.clone();
                                line.update_last_change_seqno(seqno);
                                rewrapped.push_back(line);
                            }
                        }
                    }
                }
                rewrapped
            }
        };
        let materialize_elapsed = profile_start.map(|start| start.elapsed());

        if logical_cache_hit && reuse_unlinked_scan_state_for_reflow() {
            for line in &mut self.lines {
                if !line.has_hyperlink() {
                    line.invalidate_implicit_hyperlinks(seqno);
                }
            }
        }

        // Map through the actual chosen rows, not logical_x / window_width.
        // Wide graphemes and bounded wrap plans can leave a row underfull;
        // division then shifts the cursor onto another grapheme or a spacer.
        // A trailing virtual column remains on the last row of its record.
        if let Some((logical_idx, mut remaining)) = logical_cursor {
            if let (Some(&start), Some(&end)) =
                (row_prefix.get(logical_idx), row_prefix.get(logical_idx + 1))
            {
                for row in start..end {
                    let row_len = self.lines[row].len();
                    if remaining < row_len || row + 1 == end {
                        adjusted_cursor = (remaining, row);
                        break;
                    }
                    remaining -= row_len;
                }
            }
        }

        let mut anchor_index = 0;
        selection_anchors.0.retain_mut(|entry| {
            let logical = &logical_anchors[anchor_index];
            anchor_index += 1;
            for (point, logical) in entry.points.iter_mut().zip(logical) {
                if point.is_none() {
                    continue;
                }
                let Some((group, mut remaining, before)) = *logical else {
                    return false;
                };
                let (Some(&start), Some(&end)) = (row_prefix.get(group), row_prefix.get(group + 1))
                else {
                    return false;
                };
                let mut mapped = None;
                for row in start..end {
                    let len = self.lines[row].len();
                    if remaining < len || row + 1 == end {
                        mapped = Some(SelectionAnchorCoordinate {
                            row: self.phys_to_stable_row_index(row),
                            column: if before {
                                remaining.checked_sub(1)
                            } else {
                                Some(remaining)
                            },
                        });
                        break;
                    }
                    remaining -= len;
                }
                let Some(mapped) = mapped else {
                    return false;
                };
                *point = Some(mapped);
            }
            true
        });

        // Prune unused trailing blanks before allowing scrollback to grow,
        // but retain the cursor's row even when it is an empty hard newline.
        // Removing that row would leave the cursor outside the retained lines.
        let capacity = physical_rows + self.hot_scrollback_size();
        while self.lines.len() > capacity
            && self.lines.len().saturating_sub(1) > adjusted_cursor.1
            && self.lines.back().map(Line::is_whitespace).unwrap_or(false)
        {
            self.lines.pop_back();
            pruned_rows += 1;
        }

        let cursor_map_elapsed = profile_start.map(|start| start.elapsed());
        if pruned_rows > 0 {
            self.rewrap_cache = None;
        } else {
            let key = WrapCacheKey {
                physical_cols,
                dpi: self.dpi,
            };
            let published_target = self
                .rewrap_cache
                .as_ref()
                .and_then(|cache| cache.wrapped_by_key.get(&key))
                .filter(|wrapped| !wrapped.has_images)
                .map(|wrapped| Arc::clone(&wrapped.lines));
            // Text rows retain the exact immutable chunks used for this
            // publication. The next lookup compares every ordered row against
            // them, ignoring only seqno and the existing no-match scan bit.
            // Mutable image payloads require a fresh complete content hash.
            let source_signature = published_target
                .is_none()
                .then(|| self.compute_layout_signature());
            if let Some(cache) = self.rewrap_cache.as_mut() {
                let cache = Arc::make_mut(cache);
                cache.source_signature = source_signature;
                cache.published_target = published_target;
            }
        }

        let signature_elapsed = profile_start.map(|start| start.elapsed());
        let final_cache_entries = self
            .rewrap_cache
            .as_ref()
            .map(|cache| cache.wrapped_by_key.len())
            .unwrap_or(0);

        debug!(
            "rewrap_lines cols={}→{} physical_lines={} logical_lines={} rewrapped_lines={} cache.logical={} cache.wrap={} cache.entries={} scratch.slot_capacity={} scratch.prefix_capacity={} pruned_rows={} elapsed_ms={}",
            old_cols,
            physical_cols,
            original_len,
            logical_count,
            wrapped_count,
            logical_cache_hit,
            wrap_cache_hit,
            final_cache_entries.max(cache_entries),
            self.rewrap_scratch_slots.capacity(),
            self.rewrap_row_prefix_scratch.capacity(),
            pruned_rows,
            started.elapsed().as_millis()
        );
        self.record_cursor_consistency_telemetry(seqno, adjusted_cursor.0, adjusted_cursor.1);
        if let (
            Some(start),
            Some(cursor),
            Some(wraps),
            Some(prefix),
            Some(materialize),
            Some(cursor_map),
            Some(signature),
        ) = (
            profile_start,
            cursor_elapsed,
            wraps_elapsed,
            prefix_elapsed,
            materialize_elapsed,
            cursor_map_elapsed,
            signature_elapsed,
        ) {
            // Emit once, after all measured work. The final bucket includes
            // cache bookkeeping and the existing summary log as well as the
            // cursor audit; it is deliberately labeled as a remainder.
            let total = start.elapsed();
            log::debug!(
                target: "frankenterm_term::screen::reflow_profile",
                "reflow_stages seq={} old_cols={} cols={} source_rows={} target_rows={} logical_lines={} cursor_us={} wraps_us={} prefix_us={} materialize_us={} cursor_map_prune_us={} signature_us={} remainder_us={} total_us={} wrap_cache_hit={}",
                seqno,
                old_cols,
                physical_cols,
                original_len,
                self.lines.len(),
                logical_count,
                cursor.as_micros(),
                wraps.saturating_sub(cursor).as_micros(),
                prefix.saturating_sub(wraps).as_micros(),
                materialize.saturating_sub(prefix).as_micros(),
                cursor_map.saturating_sub(materialize).as_micros(),
                signature.saturating_sub(cursor_map).as_micros(),
                total.saturating_sub(signature).as_micros(),
                total.as_micros(),
                wrap_cache_hit,
            );
        }

        adjusted_cursor
    }

    pub(crate) fn capture_reflow_preparation(
        &self,
        size: TerminalSize,
        cursor: CursorPosition,
    ) -> Option<ScreenReflowPreparation> {
        if !self.allow_scrollback || !self.needs_rewrap_for_width_change(size.cols.max(1)) {
            return None;
        }
        let profile_start =
            log::log_enabled!(target: "frankenterm_term::screen::reflow_profile", log::Level::Debug)
                .then(Instant::now);
        // The empty constructor avoids allocating another full scrollback.
        // Copy only the inputs to wrapping, not recovery state, retained frames,
        // keyboard state, per-line caches or scratch buffers. The logical cache
        // is shared here and detaches on the worker, outside the terminal lock.
        let mut snapshot = Self::try_new(
            size,
            &self.config,
            true,
            cursor.seqno,
            BidiMode {
                enabled: false,
                hint: frankenterm_bidi::ParagraphDirectionHint::LeftToRight,
            },
            true,
        )
        .ok()?;
        snapshot.lines = self.lines.clone();
        snapshot.physical_cols = self.physical_cols;
        snapshot.physical_rows = self.physical_rows;
        snapshot.dpi = self.dpi;
        snapshot.stable_row_index_offset = self.stable_row_index_offset;
        snapshot.resize_wrap_policy = self.resize_wrap_policy;
        snapshot.rewrap_cache = self.rewrap_cache.clone();
        if let Some(start) = profile_start {
            log::debug!(
                target: "frankenterm_term::screen::reflow_profile",
                "reflow_capture seq={} source_rows={} cols={} capture_us={}",
                cursor.seqno,
                self.lines.len(),
                size.cols,
                start.elapsed().as_micros(),
            );
        }
        Some(ScreenReflowPreparation {
            snapshot,
            source_lines: VecDeque::new(),
            source_cursor: cursor,
            source_logical_cursor: None,
            source_dpi: self.dpi,
            target: size,
            ready: false,
            applied: false,
        })
    }

    fn matches_reflow_preparation(
        &self,
        prepared: &ScreenReflowPreparation,
        size: TerminalSize,
        cursor: CursorPosition,
    ) -> bool {
        let profile_start =
            log::log_enabled!(target: "frankenterm_term::screen::reflow_profile", log::Level::Debug)
                .then(Instant::now);
        let matches = prepared.ready
            && self.allow_scrollback
            && prepared.target == size
            && prepared.logical_cursor_for(cursor).is_some()
            && prepared.source_dpi == self.dpi
            && prepared.snapshot.physical_cols == self.physical_cols
            && prepared.snapshot.physical_rows == self.physical_rows
            && prepared.snapshot.stable_row_index_offset == self.stable_row_index_offset
            && prepared.snapshot.resize_wrap_policy == self.resize_wrap_policy
            && prepared.source_lines.len() == self.lines.len()
            && prepared
                .source_lines
                .iter()
                .zip(&self.lines)
                .all(|(source, live)| source.is_same_reflow_source(live));
        if let Some(start) = profile_start {
            log::debug!(
                target: "frankenterm_term::screen::reflow_profile",
                "reflow_validate seq={} source_rows={} cols={} matches={} validate_us={}",
                cursor.seqno,
                self.lines.len(),
                size.cols,
                matches,
                start.elapsed().as_micros(),
            );
        }
        matches
    }

    fn prune_resize_trailing_blanks(&mut self, cursor: CursorPosition) {
        let cursor_phys = self.phys_row(cursor.y);
        for _ in cursor_phys + 1..self.lines.len() {
            if self.lines.back().map(Line::is_whitespace).unwrap_or(false) {
                self.lines.pop_back();
            }
        }
    }

    /// Resize the physical, viewable portion of the screen.
    pub fn resize(
        &mut self,
        size: TerminalSize,
        cursor: CursorPosition,
        seqno: SequenceNo,
        is_conpty: bool,
    ) -> CursorPosition {
        self.resize_with_prepared_reflow(size, cursor, seqno, is_conpty, None)
    }

    pub(crate) fn resize_with_prepared_reflow(
        &mut self,
        size: TerminalSize,
        cursor: CursorPosition,
        seqno: SequenceNo,
        is_conpty: bool,
        mut prepared: Option<&mut ScreenReflowPreparation>,
    ) -> CursorPosition {
        // A resize may return early or reuse cached wraps without recording a
        // fresh viewport batch. Never attribute an earlier resize's work to it.
        self.last_viewport_first_reflow_us = 0;
        let physical_rows = size.rows.max(1);
        let physical_cols = size.cols.max(1);
        let mut selection_anchors = self.take_selection_anchors_for_resize(seqno);

        if physical_rows == self.physical_rows
            && physical_cols == self.physical_cols
            && size.dpi == self.dpi
        {
            self.finish_selection_anchor_resize(selection_anchors, seqno);
            return cursor;
        }
        log::debug!(
            "resize screen to {physical_cols}x{physical_rows} dpi={}",
            size.dpi
        );
        self.invalidate_coordinate_witnesses();
        self.retain_last_good_frame(seqno, LastGoodFrameTransition::ResizeBegin);
        let prepared_matches = prepared
            .as_ref()
            .is_some_and(|prepared| self.matches_reflow_preparation(prepared, size, cursor));
        let dpi_changed = self.dpi != size.dpi;
        self.dpi = size.dpi;
        if dpi_changed {
            if !prepared_matches {
                if let Some(cache) = self.rewrap_cache.as_mut() {
                    Arc::make_mut(cache).clear_wraps();
                }
            }
            self.clear_rewrap_line_cache();
        }

        if let Some(candidate) = prepared.as_deref_mut() {
            if candidate.ready
                && candidate.target == size
                && candidate.snapshot.resize_wrap_policy == self.resize_wrap_policy
                && !candidate.snapshot.rewrap_line_cache.is_empty()
            {
                // Output may invalidate the complete prepared layout while
                // leaving most logical lines unchanged. Retain its bounded,
                // content-keyed wraps: each live line still passes the normal
                // width/DPI/policy/shape lookup before reuse. Never install a
                // stale layout or cursor. Swap so displaced buffers retire on
                // the preparation's worker after terminal locks are released.
                std::mem::swap(
                    &mut self.rewrap_line_cache,
                    &mut candidate.snapshot.rewrap_line_cache,
                );
                std::mem::swap(
                    &mut self.rewrap_line_cache_order,
                    &mut candidate.snapshot.rewrap_line_cache_order,
                );
            }
        }
        let prepared = prepared.filter(|_| prepared_matches);

        // pre-prune blank lines that range from the cursor position to the end of the display;
        // this avoids growing the scrollback size when rapidly switching between normal and
        // maximized states.
        let cursor_phys = self.phys_row(cursor.y);
        self.prune_resize_trailing_blanks(cursor);
        let mut verified_wraps = None;
        if let Some(prepared) = prepared {
            // Only memoized wraps move into the live screen. Resize below still
            // owns cursor mapping, alternate-screen handling and state updates.
            // Retain the displaced cache in the preparation so its destruction
            // happens after the caller releases the terminal and queue locks.
            std::mem::swap(&mut self.rewrap_cache, &mut prepared.snapshot.rewrap_cache);
            if let Some(cache) = self.rewrap_cache.as_mut() {
                let cache = Arc::make_mut(cache);
                verified_wraps = cache
                    .get_wrapped(WrapCacheKey {
                        physical_cols,
                        dpi: size.dpi,
                    })
                    .map(|wrapped| VerifiedPreparedResize {
                        wrapped,
                        logical_count: cache.logical_lines.len(),
                        cache_entries: cache.wrapped_by_key.len(),
                        logical_cursor: prepared.logical_cursor_for(cursor).flatten(),
                    });
            }
            // The preparation owns only new counters, never a stale copy of
            // live telemetry. Add completed work without overwriting activity
            // that happened while the terminal lock was released.
            let work = &prepared.snapshot.cold_scrollback_worker;
            self.cold_scrollback_worker.peak_backlog_depth = self
                .cold_scrollback_worker
                .peak_backlog_depth
                .max(work.peak_backlog_depth);
            self.cold_scrollback_worker.completed_lines_total = self
                .cold_scrollback_worker
                .completed_lines_total
                .saturating_add(work.completed_lines_total);
            self.cold_scrollback_worker.completed_batches_total = self
                .cold_scrollback_worker
                .completed_batches_total
                .saturating_add(work.completed_batches_total);
            if work.completed_batches_total > 0 {
                self.cold_scrollback_worker
                    .completion_throughput_lines_per_sec = work.completion_throughput_lines_per_sec;
            }
            self.last_viewport_first_reflow_us = prepared.snapshot.last_viewport_first_reflow_us;
            prepared.ready = false;
            prepared.applied = verified_wraps.is_some();
        }

        let (cursor_x, cursor_y) = if physical_cols != self.physical_cols {
            // Check to see if we need to rewrap lines that were
            // wrapped due to reaching the right hand side of the terminal.
            // For each one that we find, we need to join it with its
            // successor and then re-split it.
            // We only do this for the primary, and not for the alternate
            // screen (hence the check for allow_scrollback), to avoid
            // conflicting screen updates with full screen apps.
            if self.allow_scrollback {
                if self.needs_rewrap_for_width_change(physical_cols) {
                    self.rewrap_lines(
                        physical_cols,
                        physical_rows,
                        (cursor.x, cursor_phys),
                        seqno,
                        verified_wraps,
                        &mut selection_anchors,
                    )
                } else {
                    // Keep resize responsive for large scrollback histories
                    // when there is no logical wrapping work to perform.
                    self.mark_visible_lines_dirty(seqno);
                    (cursor.x, cursor_phys)
                }
            } else {
                for line in &mut self.lines {
                    if physical_cols < self.physical_cols {
                        // Do a simple prune of the lines instead
                        line.resize(physical_cols, seqno);
                    } else {
                        // otherwise: invalidate them
                        line.update_last_change_seqno(seqno);
                    }
                }
                (cursor.x, cursor_phys)
            }
        } else {
            (cursor.x, cursor_phys)
        };

        let capacity = physical_rows + self.hot_scrollback_size();
        let current_capacity = self.lines.capacity();
        if capacity > current_capacity {
            self.lines.reserve(capacity - self.lines.len());
        }

        // If we resized wider and the rewrap resulted in fewer
        // lines than the viewport size, or we resized taller,
        // pad us back out to the viewport size
        while self.lines.len() < physical_rows {
            self.lines.push_back(self.blank_line_borrowing_bidi(seqno));
        }

        let new_cursor_y;

        // true if a resize operation should consider rows that have
        // made it to scrollback as being immutable.
        // When immutable, the resize operation will pad out the screen height
        // with additional blank rows and due to implementation details means
        // that the user will need to scroll back the scrollbar post-resize
        // than they would otherwise.
        //
        // When mutable, resizing the window taller won't add extra rows;
        // instead the resize will tend to have "bottom gravity" meaning that
        // making the window taller will reveal more history than in the other
        // mode.
        //
        // mutable is generally speaking a nicer experience.
        //
        // On Windows, the PTY layer doesn't play well with a mutable scrollback,
        // frequently moving the cursor up to high and erasing portions of the
        // screen.
        //
        // This behavior only happens with the windows pty layer; it doesn't
        // manifest when using eg: ssh directly to a remote unix system.
        let resize_preserves_scrollback = is_conpty;

        if resize_preserves_scrollback {
            new_cursor_y = cursor
                .y
                .saturating_add(cursor_y as i64)
                .saturating_sub(cursor_phys as i64)
                .max(0);

            // We need to ensure that the bottom of the screen has sufficient lines;
            // we use simple subtraction of physical_rows from the bottom of the lines
            // array to define the visible region.  Our resize operation may have
            // temporarily violated that, which can result in the cursor unintentionally
            // moving up into the scrollback and damaging the output
            let required_num_rows_after_cursor =
                physical_rows.saturating_sub(new_cursor_y as usize);
            let actual_num_rows_after_cursor = self.lines.len().saturating_sub(cursor_y);
            for _ in actual_num_rows_after_cursor..required_num_rows_after_cursor {
                self.lines.push_back(self.blank_line_borrowing_bidi(seqno));
            }
        } else {
            // Compute the new cursor location; this is logically the inverse
            // of the phys_row() function, but given the revised cursor_y
            // (the rewrap adjusted physical row of the cursor).  This
            // computes its new VisibleRowIndex given the new viewport size.
            new_cursor_y = cursor_y as VisibleRowIndex
                - (self.lines.len() as VisibleRowIndex - physical_rows as VisibleRowIndex);
        }

        if self.lines.len() < physical_rows
            && self.rollback_to_last_good_frame(
                seqno,
                LastGoodFrameRollbackCause::ResizeCommitValidation,
            )
        {
            return cursor;
        }

        #[cfg(test)]
        if let Some(cause) = self.forced_rollback_cause.take() {
            if self.rollback_to_last_good_frame(seqno, cause) {
                return cursor;
            }
        }

        self.physical_rows = physical_rows;
        self.physical_cols = physical_cols;
        self.finish_selection_anchor_resize(selection_anchors, seqno);
        self.retain_last_good_frame(seqno, LastGoodFrameTransition::ResizeCommit);
        CursorPosition {
            x: cursor_x,
            y: new_cursor_y,
            shape: cursor.shape,
            visibility: cursor.visibility,
            seqno,
        }
    }

    /// Get mutable reference to a line, relative to start of scrollback.
    #[inline]
    pub fn line_mut(&mut self, idx: PhysRowIndex) -> &mut Line {
        &mut self.lines[idx]
    }

    /// Returns the number of rows currently retained in memory, including the
    /// visible viewport and any in-memory scrollback rows.
    pub fn scrollback_rows(&self) -> usize {
        self.lines.len()
    }

    /// Returns the oldest stable row reachable through either the hot in-memory
    /// buffer or the configured cold-spill sink.
    pub fn scrollback_top_stable_row(&self) -> StableRowIndex {
        self.oldest_reachable_stable_row()
    }

    /// Returns the number of rows reachable through hot memory plus cold spill
    /// hydration, including the visible viewport.
    pub fn reachable_scrollback_rows(&self) -> usize {
        let oldest = self.oldest_reachable_stable_row();
        let newest_exclusive = self.phys_to_stable_row_index(self.lines.len());
        usize::try_from(newest_exclusive.saturating_sub(oldest)).unwrap_or(0)
    }

    /// Derive both values from one metadata observation, not two potentially
    /// different asynchronous retention states.
    pub fn scrollback_geometry(&self) -> (StableRowIndex, usize) {
        let oldest = self.oldest_reachable_stable_row();
        let newest = self.phys_to_stable_row_index(self.lines.len());
        (
            oldest,
            usize::try_from(newest.saturating_sub(oldest)).unwrap_or(0),
        )
    }

    /// Returns the number of in-memory scrollback rows retained above the
    /// visible viewport.
    pub fn in_memory_scrollback_rows(&self) -> usize {
        self.lines.len().saturating_sub(self.physical_rows)
    }

    /// Microseconds spent reflowing viewport and near-viewport batches during
    /// the most recent resize before cold scrollback convergence began.
    /// Zero when that resize skipped fresh viewport batch reflow.
    pub fn last_viewport_first_reflow_us(&self) -> u64 {
        self.last_viewport_first_reflow_us
    }

    /// Returns a nonblocking snapshot of tiered scrollback state.
    ///
    /// If an external cold sink is present, its coherent usage is probed
    /// without blocking. Lock contention returns `Err(ColdReadMetadataBusy)`.
    /// Unsupported or untrustworthy usage returns `Ok(None)` instead of false
    /// zero counts; this telemetry is not cold-read or recovery authority.
    pub fn try_tiered_scrollback_status(
        &self,
    ) -> Result<Option<TieredScrollbackStatus>, ColdReadMetadataBusy> {
        let (cold_sink_retained_lines, cold_sink_retained_bytes) = if let Some(sink) =
            self.config.scrollback_spill_sink()
        {
            match sink.try_capture_scrollback_usage() {
                crate::config::ScrollbackUsageCapture::Ready(usage) => (usage.rows, usage.bytes),
                crate::config::ScrollbackUsageCapture::Busy => {
                    return Err(ColdReadMetadataBusy);
                }
                crate::config::ScrollbackUsageCapture::Unsupported
                | crate::config::ScrollbackUsageCapture::Unavailable => return Ok(None),
            }
        } else {
            (0, 0)
        };

        Ok(Some(self.tiered_scrollback_status_with_usage(
            cold_sink_retained_lines,
            cold_sink_retained_bytes,
        )))
    }

    /// Returns a stable snapshot of the tiered scrollback state for telemetry,
    /// diagnostics, and future GUI surfaces.
    pub fn tiered_scrollback_status(&self) -> TieredScrollbackStatus {
        self.tiered_scrollback_status_with_usage(
            self.cold_sink_retained_rows(),
            self.cold_sink_retained_bytes(),
        )
    }

    fn tiered_scrollback_status_with_usage(
        &self,
        cold_sink_retained_lines: usize,
        cold_sink_retained_bytes: usize,
    ) -> TieredScrollbackStatus {
        let tier = self.config.scrollback_tier_config();
        let tiering_enabled = self.allow_scrollback && tier.enabled;
        let configured_scrollback_rows = if self.allow_scrollback {
            self.config.scrollback_size()
        } else {
            0
        };

        TieredScrollbackStatus {
            tiering_enabled,
            configured_scrollback_rows,
            configured_hot_lines: if tiering_enabled {
                self.hot_scrollback_size()
            } else {
                0
            },
            configured_warm_max_bytes: if tiering_enabled {
                self.tiered_scrollback_warm_max_bytes()
            } else {
                0
            },
            visible_rows: self.physical_rows,
            in_memory_scrollback_rows: self.in_memory_scrollback_rows(),
            warm_resident_lines: self.scrollback_tiering.warm_resident_lines(),
            warm_resident_bytes: self.scrollback_tiering.warm_bytes,
            warm_spill_lines_total: self.scrollback_tiering.warm_spill_lines_total,
            warm_spill_bytes_total: self.scrollback_tiering.warm_spill_bytes_total,
            cold_spill_lines_total: self.scrollback_tiering.cold_spill_lines_total,
            cold_spill_bytes_total: self.scrollback_tiering.cold_spill_bytes_total,
            cold_sink_retained_lines,
            cold_sink_retained_bytes,
            cold_worker_peak_backlog_depth: self.cold_scrollback_worker.peak_backlog_depth(),
            cold_worker_completion_throughput_lines_per_sec: self
                .cold_scrollback_worker
                .completion_throughput_lines_per_sec(),
            cold_worker_completed_lines_total: self.cold_scrollback_worker.completed_lines_total,
            cold_worker_completed_batches_total: self
                .cold_scrollback_worker
                .completed_batches_total,
            cold_worker_cancellation_count: self.cold_scrollback_worker.cancellation_count(),
        }
    }

    /// Sets a line dirty.  The line is relative to the visible origin.
    #[inline]
    pub fn dirty_line(&mut self, idx: VisibleRowIndex, seqno: SequenceNo) {
        let line_idx = self.phys_row(idx);
        if line_idx < self.lines.len() {
            self.lines[line_idx].update_last_change_seqno(seqno);
        }
    }

    /// Returns a copy of the visible lines in the screen (no scrollback)
    #[cfg(test)]
    pub fn visible_lines(&self) -> Vec<Line> {
        let line_idx = self.lines.len() - self.physical_rows;
        let mut lines = Vec::new();
        for line in self.lines.iter().skip(line_idx) {
            if lines.len() >= self.physical_rows {
                break;
            }
            lines.push(line.clone());
        }
        lines
    }

    /// Returns a copy of the lines in the screen (including scrollback)
    #[cfg(test)]
    pub fn all_lines(&self) -> Vec<Line> {
        self.lines.iter().cloned().collect()
    }

    pub fn insert_cell(
        &mut self,
        x: usize,
        y: VisibleRowIndex,
        right_margin: usize,
        seqno: SequenceNo,
    ) {
        self.invalidate_last_good_frame(LastGoodFrameTransition::ContentMutation, Some(seqno));
        let phys_cols = self.physical_cols;

        let line_idx = self.phys_row(y);
        let line = self.line_mut(line_idx);
        line.update_last_change_seqno(seqno);
        line.insert_cell(x, Cell::default(), right_margin, seqno);
        if line.len() > phys_cols {
            // Don't allow the line width to grow beyond
            // the physical width
            line.resize(phys_cols, seqno);
        }
    }

    pub fn erase_cell(
        &mut self,
        x: usize,
        y: VisibleRowIndex,
        right_margin: usize,
        seqno: SequenceNo,
        blank_attr: CellAttributes,
    ) {
        self.invalidate_last_good_frame(LastGoodFrameTransition::ContentMutation, Some(seqno));
        let line_idx = self.phys_row(y);
        let line = self.line_mut(line_idx);
        line.erase_cell_with_margin(x, right_margin, seqno, blank_attr);
    }

    /// Set a cell.  the x and y coordinates are relative to the visible screeen
    /// origin.  0,0 is the top left.
    pub fn set_cell(&mut self, x: usize, y: VisibleRowIndex, cell: &Cell, seqno: SequenceNo) {
        self.invalidate_last_good_frame(LastGoodFrameTransition::ContentMutation, Some(seqno));
        let line_idx = self.phys_row(y);
        //debug!("set_cell x={} y={} phys={} {:?}", x, y, line_idx, cell);

        let line = self.line_mut(line_idx);
        line.set_cell(x, cell.clone(), seqno);
    }

    pub fn set_cell_grapheme(
        &mut self,
        x: usize,
        y: VisibleRowIndex,
        text: &str,
        width: usize,
        attr: CellAttributes,
        seqno: SequenceNo,
    ) {
        self.invalidate_last_good_frame(LastGoodFrameTransition::ContentMutation, Some(seqno));
        let line_idx = self.phys_row(y);
        let line = self.line_mut(line_idx);
        line.set_cell_grapheme(x, text, width, attr, seqno);
    }

    pub fn set_ascii_cell_run(
        &mut self,
        x: usize,
        y: VisibleRowIndex,
        text: &str,
        attr: CellAttributes,
        seqno: SequenceNo,
    ) {
        debug_assert!(text.is_ascii());
        if text.is_empty() || x.checked_add(text.len()).is_none() {
            return;
        }

        self.invalidate_last_good_frame(LastGoodFrameTransition::ContentMutation, Some(seqno));
        let line_idx = self.phys_row(y);
        let line = self.line_mut(line_idx);
        if ascii_cluster_run_append_enabled()
            && line.append_ascii_cell_run(x, text, attr.clone(), seqno)
        {
            #[cfg(test)]
            ASCII_CLUSTER_RUN_APPEND_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return;
        }
        for (offset, byte) in text.bytes().enumerate() {
            line.set_cell(x + offset, Cell::new(char::from(byte), attr.clone()), seqno);
        }
    }

    pub fn cell_mut(&mut self, x: usize, y: VisibleRowIndex) -> Option<&mut Cell> {
        let line_idx = self.phys_row(y);
        let line = self.lines.get_mut(line_idx)?;
        line.cells_mut().get_mut(x)
    }

    pub fn get_cell(&mut self, x: usize, y: VisibleRowIndex) -> Option<&Cell> {
        let line_idx = self.phys_row(y);
        let line = self.lines.get_mut(line_idx)?;
        line.cells_mut().get(x)
    }

    pub fn clear_line(
        &mut self,
        y: VisibleRowIndex,
        cols: Range<usize>,
        attr: &CellAttributes,
        seqno: SequenceNo,
        bidi_mode: BidiMode,
    ) {
        self.invalidate_last_good_frame(LastGoodFrameTransition::ContentMutation, Some(seqno));
        let line_idx = self.phys_row(y);
        if y == 0 && cols.start == 0 && cols.end >= self.physical_cols {
            // Erasing the entire first viewport row destroys the continuation
            // of any retained soft-wrapped history. Keep that history's text,
            // but do not let a later repaint join it during resize/reflow.
            // Tiered scrollback retains at least one hot history row whenever
            // history is enabled, so this boundary never needs cold sink I/O.
            if let Some(previous) = line_idx.checked_sub(1) {
                self.line_mut(previous)
                    .set_last_cell_was_wrapped(false, seqno);
            }
        }
        let line = self.line_mut(line_idx);
        if cols.start == 0 {
            bidi_mode.apply_to_line(line, seqno);
        }
        line.fill_range(cols, &Cell::blank_with_attrs(attr.clone()), seqno);
    }

    /// Ensure that row is within the range of the physical portion of
    /// the screen; 0 .. physical_rows by clamping it to the nearest
    /// boundary.
    #[inline]
    fn clamp_visible_row(&self, row: VisibleRowIndex) -> VisibleRowIndex {
        (row.max(0) as usize).min(self.physical_rows) as VisibleRowIndex
    }

    /// Translate a VisibleRowIndex into a PhysRowIndex.  The resultant index
    /// will be invalidated by inserting or removing rows!
    #[inline]
    pub fn phys_row(&self, row: VisibleRowIndex) -> PhysRowIndex {
        let row = self.clamp_visible_row(row);
        self.lines
            .len()
            .saturating_sub(self.physical_rows)
            .saturating_add(row as PhysRowIndex)
    }

    /// Given a possibly negative row number, return the corresponding physical
    /// row.  This is similar to phys_row() but allows indexing backwards into
    /// the scrollback.
    #[inline]
    pub fn scrollback_or_visible_row(&self, row: ScrollbackOrVisibleRowIndex) -> PhysRowIndex {
        (self.lines.len().saturating_sub(self.physical_rows) as ScrollbackOrVisibleRowIndex + row)
            .max(0) as usize
    }

    #[inline]
    pub fn scrollback_or_visible_range(
        &self,
        range: &Range<ScrollbackOrVisibleRowIndex>,
    ) -> Range<PhysRowIndex> {
        self.scrollback_or_visible_row(range.start)..self.scrollback_or_visible_row(range.end)
    }

    /// Converts a StableRowIndex range to the current effective
    /// physical row index range.  If the StableRowIndex goes off the top
    /// of the scrollback, we'll return the top n rows, but if it goes off
    /// the bottom we'll return the bottom n rows.
    pub fn stable_range(&self, range: &Range<StableRowIndex>) -> Range<PhysRowIndex> {
        if range.start >= range.end {
            return 0..0;
        }

        let range_len =
            usize::try_from(range.end.saturating_sub(range.start)).unwrap_or(usize::MAX);
        let oldest = self.phys_to_stable_row_index(0);
        let newest_exclusive = self.phys_to_stable_row_index(self.lines.len());
        // Saturated stable coordinates do not make unrepresentable rows
        // addressable. Match the half-open interval used by get_lines.
        let addressable_len = usize::try_from(newest_exclusive.saturating_sub(oldest))
            .unwrap_or(0)
            .min(self.lines.len());
        let resolved_len = range_len.min(addressable_len);
        let first = if range.end > newest_exclusive {
            // Use the exclusive endpoint: subtracting from the last row
            // instead would return one more row than the caller requested.
            addressable_len.saturating_sub(resolved_len)
        } else {
            self.stable_row_to_phys(range.start).unwrap_or(0)
        };

        first..first.saturating_add(resolved_len).min(addressable_len)
    }

    /// Translate a range of VisibleRowIndex to a range of PhysRowIndex.
    /// The resultant range will be invalidated by inserting or removing rows!
    #[inline]
    pub fn phys_range(&self, range: &Range<VisibleRowIndex>) -> Range<PhysRowIndex> {
        self.phys_row(range.start)..self.phys_row(range.end)
    }

    #[inline]
    pub fn phys_to_stable_row_index(&self, phys: PhysRowIndex) -> StableRowIndex {
        phys.checked_add(self.stable_row_index_offset)
            .and_then(|idx| StableRowIndex::try_from(idx).ok())
            .unwrap_or(StableRowIndex::MAX)
    }

    fn stable_row_index_for_removed_top(&self, removed_from_top: usize) -> StableRowIndex {
        self.stable_row_index_offset
            .checked_add(removed_from_top)
            .and_then(|idx| StableRowIndex::try_from(idx).ok())
            .unwrap_or(StableRowIndex::MAX)
    }

    fn advance_stable_row_index_offset(&mut self, delta: usize) {
        self.stable_row_index_offset = self.stable_row_index_offset.saturating_add(delta);
    }

    #[inline]
    pub fn stable_row_to_phys(&self, stable: StableRowIndex) -> Option<PhysRowIndex> {
        let offset = StableRowIndex::try_from(self.stable_row_index_offset).ok()?;
        let idx = stable.checked_sub(offset)?;
        let idx = usize::try_from(idx).ok()?;
        if idx >= self.lines.len() {
            // Index is no longer valid
            None
        } else {
            Some(idx)
        }
    }

    #[inline]
    pub fn visible_row_to_stable_row(&self, vis: VisibleRowIndex) -> StableRowIndex {
        self.phys_to_stable_row_index(self.phys_row(vis))
    }

    /// Scroll the scroll_region up by num_rows, respecting left and right margins.
    /// Text outside the left and right margins is left untouched.
    /// Any rows that would be scrolled beyond the top get removed from the screen.
    /// Blank rows are added at the bottom.
    /// If left and right margins are set smaller than the screen width, scrolled rows
    /// will not be placed into scrollback, because they are not complete rows.
    pub fn scroll_up_within_margins(
        &mut self,
        scroll_region: &Range<VisibleRowIndex>,
        left_and_right_margins: &Range<usize>,
        num_rows: usize,
        seqno: SequenceNo,
        blank_attr: CellAttributes,
        bidi_mode: BidiMode,
    ) {
        self.invalidate_last_good_frame(LastGoodFrameTransition::ContentMutation, Some(seqno));
        log::debug!(
            target: "frankenterm_term::screen::scroll",
            "scroll_up_within_margins region:{:?} margins:{:?} rows={}",
            scroll_region,
            left_and_right_margins,
            num_rows
        );

        if left_and_right_margins.start == 0 && left_and_right_margins.end == self.physical_cols {
            return self.scroll_up(scroll_region, num_rows, seqno, blank_attr, bidi_mode);
        }

        // Need to do the slower, more complex left and right bounded scroll
        let phys_scroll = self.phys_range(scroll_region);

        // The scroll is really a copy + a clear operation
        let region_height = phys_scroll.end - phys_scroll.start;
        let num_rows = num_rows.min(region_height);
        let rows_to_copy = region_height - num_rows;

        if rows_to_copy > 0 {
            for dest_row in phys_scroll.start..phys_scroll.start + rows_to_copy {
                let src_row = dest_row + num_rows;

                // Copy the source cells first
                let cells = {
                    self.lines[src_row]
                        .cells_mut()
                        .iter()
                        .skip(left_and_right_margins.start)
                        .take(left_and_right_margins.end - left_and_right_margins.start)
                        .cloned()
                        .collect::<Vec<_>>()
                };

                // and place them into the dest
                let dest_row = self.line_mut(dest_row);
                dest_row.update_last_change_seqno(seqno);
                let dest_range =
                    left_and_right_margins.start..left_and_right_margins.start + cells.len();
                if dest_row.len() < dest_range.end {
                    dest_row.resize(dest_range.end, seqno);
                }

                let tail_range = dest_range.end..left_and_right_margins.end;

                for (src_cell, dest_cell) in
                    cells.into_iter().zip(&mut dest_row.cells_mut()[dest_range])
                {
                    *dest_cell = src_cell.clone();
                }

                dest_row.fill_range(
                    tail_range,
                    &Cell::blank_with_attrs(blank_attr.clone()),
                    seqno,
                );
            }
        }

        // and blank out rows at the bottom
        for n in phys_scroll.start + rows_to_copy..phys_scroll.end {
            let dest_row = self.line_mut(n);
            dest_row.update_last_change_seqno(seqno);
            for cell in dest_row
                .cells_mut()
                .iter_mut()
                .skip(left_and_right_margins.start)
                .take(left_and_right_margins.end - left_and_right_margins.start)
            {
                *cell = Cell::blank_with_attrs(blank_attr.clone());
            }
        }
    }

    /// ```text
    /// ---------
    /// |
    /// |--- top
    /// |
    /// |--- bottom
    /// ```
    ///
    /// scroll the region up by num_rows.  Any rows that would be scrolled
    /// beyond the top get removed from the screen.
    /// In other words, we remove (top..top+num_rows) and then insert num_rows
    /// at bottom.
    /// If the top of the region is the top of the visible display, rather than
    /// removing the lines we let them go into the scrollback.
    pub fn scroll_up(
        &mut self,
        scroll_region: &Range<VisibleRowIndex>,
        num_rows: usize,
        seqno: SequenceNo,
        blank_attr: CellAttributes,
        bidi_mode: BidiMode,
    ) {
        self.invalidate_last_good_frame(LastGoodFrameTransition::ContentMutation, Some(seqno));
        let phys_scroll = self.phys_range(scroll_region);
        let num_rows = num_rows.min(phys_scroll.end - phys_scroll.start);
        let scrollback_ok = scroll_region.start == 0 && self.allow_scrollback;
        let insert_at_end = scroll_region.end as usize == self.physical_rows;
        if num_rows != 0 && (!scrollback_ok || !insert_at_end) {
            self.invalidate_coordinate_witnesses();
        }

        debug!(
            target: "frankenterm_term::screen::scroll",
            "scroll_up {:?} num_rows={} phys_scroll={:?}",
            scroll_region, num_rows, phys_scroll
        );
        // Invalidate the lines that will move before they move so that
        // the indices of the lines are stable (we may remove lines below)
        // We only need invalidate if the StableRowIndex of the row would be
        // changed by the scroll operation.  For normal newline at the bottom
        // of the screen based scrolling, the StableRowIndex does not change,
        // so we use the scroll region bounds to gate the invalidation.
        if !scrollback_ok {
            for y in phys_scroll.clone() {
                self.line_mut(y).update_last_change_seqno(seqno);
            }
        }

        // if we're going to remove lines due to lack of scrollback capacity,
        // remember how many so that we can adjust our insertion point later.
        let lines_removed = if !scrollback_ok {
            // No scrollback available for these;
            // Remove the scrolled lines
            num_rows
        } else if self.recovery_scrollback.is_some() {
            // Authenticated recovery rows remain resident until checked
            // activation has durably replaced the exact cold prefix. The
            // post-record recovery audit enforces the global line envelope;
            // evicting here would destroy the only replay copy before that
            // audit can fail closed.
            0
        } else {
            let max_allowed = self.physical_rows + self.hot_scrollback_size();
            if self.lines.len() + num_rows >= max_allowed {
                (self.lines.len() + num_rows) - max_allowed
            } else {
                0
            }
        };

        if scroll_region.start == 0 {
            for y in self.phys_range(&(0..num_rows as VisibleRowIndex)) {
                self.line_mut(y).compress_for_scrollback();
            }
        }

        let remove_idx = if scroll_region.start == 0 {
            0
        } else {
            phys_scroll.start
        };

        let default_blank = CellAttributes::blank();
        // To avoid thrashing the heap, prefer to move lines that were
        // scrolled off the top and re-use them at the bottom.
        let to_move = lines_removed.min(num_rows);
        let mut removed_from_top = 0usize;
        let mut spill_blocked = false;
        let mut lines_moved = 0usize;
        let (to_remove, to_add) = {
            for _ in 0..to_move {
                let mut line = match self.lines.remove(remove_idx) {
                    Some(line) => line,
                    None => break,
                };
                if remove_idx == 0 && scrollback_ok {
                    let stable_row = self.stable_row_index_for_removed_top(removed_from_top);
                    if !self.record_scrollback_spill(stable_row, &line, seqno) {
                        self.lines.insert(remove_idx, line);
                        spill_blocked = true;
                        if !self
                            .config
                            .scrollback_spill_sink()
                            .is_some_and(|sink| sink.requires_scrollback_flush())
                        {
                            warn!(
                                "cold scrollback persistence failed at stable row {}; retaining the row in memory",
                                stable_row
                            );
                        }
                        break;
                    }
                    removed_from_top = removed_from_top.saturating_add(1);
                }
                let line = if default_blank == blank_attr {
                    Line::new(seqno)
                } else {
                    // Make the line like a new one of the appropriate width
                    line.resize_and_clear(self.physical_cols, seqno, blank_attr.clone());
                    line.update_last_change_seqno(seqno);
                    line
                };
                if insert_at_end {
                    self.lines.push_back(line);
                } else {
                    self.lines.insert(phys_scroll.end.saturating_sub(1), line);
                }
                lines_moved = lines_moved.saturating_add(1);
            }
            // We may still have some lines to add at the bottom, so
            // return revised counts for remove/add
            (
                if spill_blocked {
                    0
                } else {
                    lines_removed.saturating_sub(lines_moved)
                },
                num_rows.saturating_sub(lines_moved),
            )
        };

        // Perform the removal
        for _ in 0..to_remove {
            if let Some(removed) = self.lines.remove(remove_idx) {
                if remove_idx == 0 && scrollback_ok {
                    let stable_row = self.stable_row_index_for_removed_top(removed_from_top);
                    if !self.record_scrollback_spill(stable_row, &removed, seqno) {
                        self.lines.insert(remove_idx, removed);
                        if !self
                            .config
                            .scrollback_spill_sink()
                            .is_some_and(|sink| sink.requires_scrollback_flush())
                        {
                            warn!(
                                "cold scrollback persistence failed at stable row {}; retaining the row in memory",
                                stable_row
                            );
                        }
                        break;
                    }
                    removed_from_top = removed_from_top.saturating_add(1);
                }
            }
        }

        if remove_idx == 0 && scrollback_ok {
            self.advance_stable_row_index_offset(removed_from_top);
        }

        for _ in 0..to_add {
            let mut line = if default_blank == blank_attr {
                Line::new(seqno)
            } else {
                Line::with_width_and_cell(
                    self.physical_cols,
                    Cell::blank_with_attrs(blank_attr.clone()),
                    seqno,
                )
            };
            bidi_mode.apply_to_line(&mut line, seqno);
            if insert_at_end {
                self.lines.push_back(line);
            } else {
                self.lines.insert(phys_scroll.end, line);
            }
        }

        // If we have invalidated the StableRowIndex, mark all subsequent lines as dirty
        if to_remove > 0 || (to_add > 0 && !insert_at_end) {
            for y in self.phys_range(&(scroll_region.end..self.physical_rows as VisibleRowIndex)) {
                self.line_mut(y).update_last_change_seqno(seqno);
            }
        }

        // Reclaim excess VecDeque capacity after bulk removal.
        // Only shrink when capacity exceeds 2x the actual length to avoid
        // thrashing on every scroll-off. This prevents the ring buffer from
        // retaining peak capacity indefinitely in long-lived sessions.
        if lines_removed > 0 {
            let cap = self.lines.capacity();
            let len = self.lines.len();
            if cap > 1024 && cap > len * 2 {
                self.lines.shrink_to_fit();
            }
        }
    }

    pub fn erase_scrollback(&mut self) -> Result<(), ScrollbackSpillError> {
        let len = self.lines.len();
        let to_clear = len.saturating_sub(self.physical_rows);
        let recovery_boundary_after_clear = if self.recovery_scrollback.is_some() {
            let new_oldest = self
                .stable_row_index_offset
                .checked_add(to_clear)
                .ok_or(ScrollbackSpillError::ArithmeticOverflow("stable_row_range"))?;
            Some(
                StableRowIndex::try_from(new_oldest)
                    .map_err(|_| ScrollbackSpillError::ArithmeticOverflow("stable_row_range"))?,
            )
        } else {
            None
        };
        // The durable logical clear is the commit point. Keep every hot row and
        // all accounting untouched when it fails so callers can retry without
        // having created a split-brain history between memory and the sink.
        let next_coordinate_identity = ScreenCoordinateIdentity::default();
        if let Some(sink) = self.config.scrollback_spill_sink() {
            let _commit = sink.clear_scrollback()?;
        }
        self.coordinate_identity = next_coordinate_identity;
        #[cfg(feature = "use_serde")]
        {
            self.cold_row_fragments = None;
            self.cold_source_observation = None;
        }
        self.invalidate_last_good_frame(LastGoodFrameTransition::ScrollbackErase, None);
        for _ in 0..to_clear {
            self.lines.pop_front();
            if self.allow_scrollback {
                self.advance_stable_row_index_offset(1);
            }
        }
        if let (Some(recovery), Some(boundary)) = (
            self.recovery_scrollback.as_mut(),
            recovery_boundary_after_clear,
        ) {
            recovery.original_cold_prefix_newest_exclusive = boundary;
        }
        self.scrollback_tiering.reset();
        self.cold_scrollback_worker.reset();
        // Reclaim memory from the VecDeque after bulk removal.
        // Without this, the ring buffer retains capacity for the
        // evicted scrollback lines indefinitely.
        self.lines.shrink_to_fit();
        Ok(())
    }

    /// ```text
    /// ---------
    /// |
    /// |--- top
    /// |
    /// |--- bottom
    /// ```
    ///
    /// scroll the region down by num_rows.  Any rows that would be scrolled
    /// beyond the bottom get removed from the screen.
    /// In other words, we remove (bottom-num_rows..bottom) and then insert
    /// num_rows at scroll_top.
    pub fn scroll_down(
        &mut self,
        scroll_region: &Range<VisibleRowIndex>,
        num_rows: usize,
        seqno: SequenceNo,
        blank_attr: CellAttributes,
        bidi_mode: BidiMode,
    ) {
        self.invalidate_last_good_frame(LastGoodFrameTransition::ContentMutation, Some(seqno));
        debug!("scroll_down {:?} {}", scroll_region, num_rows);
        let phys_scroll = self.phys_range(scroll_region);
        let num_rows = num_rows.min(phys_scroll.end.saturating_sub(phys_scroll.start));
        if num_rows != 0 {
            self.invalidate_coordinate_witnesses();
        }

        let middle = phys_scroll.end.saturating_sub(num_rows);

        // dirty the rows in the region
        for y in phys_scroll.start..middle {
            self.line_mut(y).update_last_change_seqno(seqno);
        }

        for _ in 0..num_rows {
            self.lines.remove(middle);
        }

        let default_blank = CellAttributes::blank();

        for _ in 0..num_rows {
            let mut line = if blank_attr == default_blank {
                Line::new(seqno)
            } else {
                Line::with_width_and_cell(
                    self.physical_cols,
                    Cell::blank_with_attrs(blank_attr.clone()),
                    seqno,
                )
            };
            bidi_mode.apply_to_line(&mut line, seqno);
            self.lines.insert(phys_scroll.start, line);
        }
    }

    pub fn scroll_down_within_margins(
        &mut self,
        scroll_region: &Range<VisibleRowIndex>,
        left_and_right_margins: &Range<usize>,
        num_rows: usize,
        seqno: SequenceNo,
        blank_attr: CellAttributes,
        bidi_mode: BidiMode,
    ) {
        self.invalidate_last_good_frame(LastGoodFrameTransition::ContentMutation, Some(seqno));
        if left_and_right_margins.start == 0 && left_and_right_margins.end == self.physical_cols {
            return self.scroll_down(scroll_region, num_rows, seqno, blank_attr, bidi_mode);
        }

        // Need to do the slower, more complex left and right bounded scroll
        let phys_scroll = self.phys_range(scroll_region);

        // The scroll is really a copy + a clear operation
        let region_height = phys_scroll.end - phys_scroll.start;
        let num_rows = num_rows.min(region_height);
        let rows_to_copy = region_height - num_rows;

        if rows_to_copy > 0 {
            for src_row in (phys_scroll.start..phys_scroll.start + rows_to_copy).rev() {
                let dest_row = src_row + num_rows;

                // Copy the source cells first
                let cells = {
                    self.lines[src_row]
                        .cells_mut()
                        .iter()
                        .skip(left_and_right_margins.start)
                        .take(left_and_right_margins.end - left_and_right_margins.start)
                        .cloned()
                        .collect::<Vec<_>>()
                };

                // and place them into the dest
                let dest_row = self.line_mut(dest_row);
                dest_row.update_last_change_seqno(seqno);
                let dest_range =
                    left_and_right_margins.start..left_and_right_margins.start + cells.len();
                if dest_row.len() < dest_range.end {
                    dest_row.resize(dest_range.end, seqno);
                }
                let tail_range = dest_range.end..left_and_right_margins.end;

                for (src_cell, dest_cell) in
                    cells.into_iter().zip(&mut dest_row.cells_mut()[dest_range])
                {
                    *dest_cell = src_cell.clone();
                }

                dest_row.fill_range(
                    tail_range,
                    &Cell::blank_with_attrs(blank_attr.clone()),
                    seqno,
                );
            }
        }

        // and blank out rows at the top
        for n in phys_scroll.start..phys_scroll.start + num_rows {
            let dest_row = self.line_mut(n);
            dest_row.update_last_change_seqno(seqno);
            for cell in dest_row
                .cells_mut()
                .iter_mut()
                .skip(left_and_right_margins.start)
                .take(left_and_right_margins.end - left_and_right_margins.start)
            {
                *cell = Cell::blank_with_attrs(blank_attr.clone());
            }
        }
    }

    pub fn lines_in_phys_range(&self, phys_range: Range<PhysRowIndex>) -> Vec<Line> {
        self.lines
            .iter()
            .skip(phys_range.start)
            .take(phys_range.end - phys_range.start)
            .cloned()
            .collect()
    }

    pub fn lines_in_stable_range(
        &mut self,
        stable_range: Range<StableRowIndex>,
    ) -> (StableRowIndex, Vec<Line>) {
        if stable_range.start >= stable_range.end {
            return (stable_range.start, Vec::new());
        }

        #[cfg(feature = "use_serde")]
        if stable_range.start < self.phys_to_stable_row_index(0)
            && self.cold_visual_layout.as_ref().is_some_and(|layout| {
                self.matches_coordinate_witness(&layout.witness)
                    && layout.resident_frontier < self.phys_to_stable_row_index(0)
                    && (!layout.stored_physical()
                        || matches!(
                            layout.kind,
                            ColdVisualLayoutKind::StoredPhysical { open_tail: true }
                        ))
            })
        {
            // A spill can shift the canonical origin and extend its last
            // logical group. Rejecting the old map then falling back to raw
            // source rows would mix coordinate systems. Resolve the complete
            // request through the same bounded reader used off-lock instead.
            // Do not publish here: the caller owns layout sequence authority.
            let read = self
                .capture_line_read(stable_range.clone())
                .and_then(|read| read.hydrate(|| false));
            return match read {
                Ok(read) if self.validates_line_read(&read) => {
                    (read.first_row(), read.lines().cloned().collect())
                }
                _ => (stable_range.start, Vec::new()),
            };
        }

        let requested_len = stable_range.end.saturating_sub(stable_range.start) as usize;
        let sink = self.config.scrollback_spill_sink();
        // This is the synchronous semantic API, not the paint/dimensions
        // probe. Metadata contention must not silently rebase persisted text
        // onto the resident frontier.
        let source_oldest = sink
            .as_ref()
            .and_then(|sink| sink.oldest_scrollback_row())
            .unwrap_or_else(|| self.phys_to_stable_row_index(0))
            .min(self.phys_to_stable_row_index(0));
        #[cfg(feature = "use_serde")]
        let interval = if self.cold_visual_layout.is_some() || self.cold_row_fragments.is_some() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                match sink
                    .as_ref()
                    .map(|sink| sink.try_capture_scrollback_interval())
                {
                    Some(crate::config::ScrollbackIntervalCapture::Ready(now)) => break Some(now),
                    Some(crate::config::ScrollbackIntervalCapture::Busy)
                        if std::time::Instant::now() < deadline =>
                    {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    _ => return (stable_range.start, Vec::new()),
                }
            }
        } else {
            None
        };
        #[cfg(feature = "use_serde")]
        let layout = self
            .cold_visual_layout
            .as_ref()
            .filter(|layout| {
                self.matches_coordinate_witness(&layout.witness)
                    && layout.resident_frontier <= self.phys_to_stable_row_index(0)
                    && interval.as_ref().is_some_and(|now| {
                        now.rows()
                            .is_some_and(|rows| rows.start == layout.source.start)
                            && now.retains(&layout.interval, layout.source.clone())
                    })
            })
            .cloned();
        #[cfg(feature = "use_serde")]
        let oldest = layout
            .as_ref()
            .map_or(source_oldest, |layout| layout.visual.start);
        #[cfg(not(feature = "use_serde"))]
        let oldest = source_oldest;
        let newest_exclusive = self.phys_to_stable_row_index(self.lines.len());
        if oldest >= newest_exclusive {
            return (newest_exclusive, Vec::new());
        }

        let available_len = usize::try_from(newest_exclusive.saturating_sub(oldest)).unwrap_or(0);
        let resolved_len = requested_len.min(available_len);
        let mut first = stable_range.start.max(oldest);
        if stable_range.end > newest_exclusive {
            first = newest_exclusive.saturating_sub(resolved_len as StableRowIndex);
        }
        first = first.max(oldest);

        #[cfg(feature = "use_serde")]
        if let (Some(fragments), Some(sink)) = (&self.cold_row_fragments, &sink) {
            if !interval
                .as_ref()
                .is_some_and(|now| Self::fragments_match_interval(fragments, sink, now))
            {
                return (first, Vec::new());
            }
        }
        let mut lines = Vec::with_capacity(resolved_len);
        let end = first + resolved_len as StableRowIndex;
        let mut stable_row = first;
        while stable_row < end {
            if let Some(phys) = self.stable_row_to_phys(stable_row) {
                if let Some(line) = self.lines.get(phys) {
                    lines.push(line.clone());
                    stable_row += 1;
                } else {
                    break;
                }
                continue;
            }

            let Some(sink) = sink.as_ref() else {
                break;
            };
            #[cfg(feature = "use_serde")]
            if let Some(layout) = layout
                .as_ref()
                .filter(|layout| !layout.stored_physical() && layout.visual.contains(&stable_row))
            {
                let Some((source, visual)) = layout
                    .groups
                    .iter()
                    .find(|(_, visual)| visual.contains(&stable_row))
                else {
                    break;
                };
                let mut source_row = source.start;
                let mut logical: Option<Line> = None;
                let mut tail_wrapped = false;
                while source_row < source.end {
                    let batch = sink.load_scrollback_lines(
                        source_row..source.end.min(source_row.saturating_add(32)),
                    );
                    if batch.is_empty() || batch.len() > (source.end - source_row) as usize {
                        return (first, lines);
                    }
                    for line in batch {
                        let mut line = cold_row_with_fragment(
                            self.cold_row_fragments.as_deref(),
                            source_row,
                            line,
                        );
                        tail_wrapped = line.last_cell_was_wrapped();
                        let seqno = line.current_seqno();
                        line.set_last_cell_was_wrapped(false, seqno);
                        if let Some(logical) = &mut logical {
                            let seqno = logical.current_seqno().max(seqno);
                            logical.append_line(line, seqno);
                        } else {
                            logical = Some(line);
                        }
                        source_row += 1;
                    }
                }
                let Some(logical) = logical else {
                    break;
                };
                let seqno = logical.current_seqno();
                let Some(wrapped) = Self::wrap_cold_logical_line(
                    logical,
                    self.physical_cols,
                    seqno,
                    self.resize_wrap_policy,
                    tail_wrapped
                        && self.cold_row_fragments.as_ref().is_some_and(|fragments| {
                            fragments.aligned_at(
                                source.end,
                                self.physical_cols,
                                self.dpi,
                                self.resize_wrap_policy,
                            )
                        }),
                    (visual.end - visual.start) as usize,
                ) else {
                    break;
                };
                if wrapped.len() != (visual.end - visual.start) as usize {
                    break;
                }
                let count = (end.min(visual.end) - stable_row) as usize;
                lines.extend(
                    wrapped
                        .into_iter()
                        .skip((stable_row - visual.start) as usize)
                        .take(count),
                );
                stable_row += count as StableRowIndex;
                continue;
            }
            let cold_end = end.min(self.phys_to_stable_row_index(0));
            let batch_end = cold_end.min(stable_row.saturating_add(32));
            let batch = sink.load_scrollback_lines(stable_row..batch_end);
            if batch.is_empty() || batch.len() > (batch_end - stable_row) as usize {
                break;
            }
            for line in batch {
                #[cfg(feature = "use_serde")]
                let line =
                    cold_row_with_fragment(self.cold_row_fragments.as_deref(), stable_row, line);
                lines.push(line);
                stable_row += 1;
            }
        }

        (first, lines)
    }

    pub fn get_changed_stable_rows(
        &self,
        stable_lines: Range<StableRowIndex>,
        seqno: SequenceNo,
    ) -> Vec<StableRowIndex> {
        // Unlike a text fetch, a dirty query must never substitute nearby rows
        // for an unavailable interval. Intersect before stable_range applies
        // its deliberate top/bottom clamping semantics.
        let resident = stable_lines.start.max(self.phys_to_stable_row_index(0))
            ..stable_lines
                .end
                .min(self.phys_to_stable_row_index(self.lines.len()));
        let phys = self.stable_range(&resident);
        let mut set = vec![];
        #[cfg(feature = "use_serde")]
        if self.cold_visual_seqno > seqno {
            if let Some(layout) = self.current_cold_visual_layout() {
                set.extend(
                    stable_lines.start.max(layout.visual.start)
                        ..stable_lines.end.min(layout.visual.end),
                );
            }
        }
        for (idx, line) in self
            .lines
            .iter()
            .enumerate()
            .skip(phys.start)
            .take(phys.end - phys.start)
        {
            if line.changed_since(seqno) {
                set.push(self.phys_to_stable_row_index(idx))
            }
        }
        set
    }

    pub fn with_phys_lines<F>(&self, phys_range: Range<PhysRowIndex>, mut func: F)
    where
        F: FnMut(&[&Line]),
    {
        let (first, second) = self.lines.as_slices();
        let first_range = 0..first.len();
        let second_range = first.len()..first.len() + second.len();
        let first_range = phys_intersection(&first_range, &phys_range);
        let second_range = phys_intersection(&second_range, &phys_range);

        let mut lines: Vec<&Line> = Vec::with_capacity(phys_range.end - phys_range.start);
        for line in &first[first_range] {
            lines.push(line);
        }
        for line in &second[second_range.start.saturating_sub(first.len())
            ..second_range.end.saturating_sub(first.len())]
        {
            lines.push(line);
        }
        func(&lines)
    }

    pub fn with_phys_lines_mut<F>(&mut self, phys_range: Range<PhysRowIndex>, mut func: F)
    where
        F: FnMut(&mut [&mut Line]),
    {
        let (first, second) = self.lines.as_mut_slices();
        let first_len = first.len();
        let first_range = 0..first.len();
        let second_range = first.len()..first.len() + second.len();
        let first_range = phys_intersection(&first_range, &phys_range);
        let second_range = phys_intersection(&second_range, &phys_range);

        let mut lines: Vec<&mut Line> = Vec::with_capacity(phys_range.end - phys_range.start);
        for line in &mut first[first_range] {
            lines.push(line);
        }
        for line in &mut second[second_range.start.saturating_sub(first_len)
            ..second_range.end.saturating_sub(first_len)]
        {
            lines.push(line);
        }
        func(&mut lines)
    }

    pub fn for_each_phys_line<F>(&self, mut f: F)
    where
        F: FnMut(usize, &Line),
    {
        for (idx, line) in self.lines.iter().enumerate() {
            f(idx, line);
        }
    }

    pub fn for_each_phys_line_mut<F>(&mut self, mut f: F)
    where
        F: FnMut(usize, &mut Line),
    {
        for (idx, line) in self.lines.iter_mut().enumerate() {
            f(idx, line);
        }
    }

    pub fn for_each_logical_line_in_stable_range_mut<F>(
        &mut self,
        stable_range: Range<StableRowIndex>,
        mut f: F,
    ) where
        F: FnMut(Range<StableRowIndex>, &mut [&mut Line]) -> bool,
    {
        let mut phys_range = self.stable_range(&stable_range);

        // Avoid pathological cases where we have eg: a really long logical line
        // (such as 1.5MB of json) that we previously wrapped.  We don't want to
        // un-wrap, scan, and re-wrap that thing.
        // This is an imperfect length constraint to partially manage the cost.
        const MAX_LOGICAL_LINE_LEN: usize = 1024;

        // Look backwards to find the start of the first logical line
        let mut back_len = 0;
        while phys_range.start > 0 {
            let prior = &mut self.lines[phys_range.start - 1];
            if !prior.last_cell_was_wrapped() {
                break;
            }
            if logical_len_exceeds_limit(back_len, prior.len(), MAX_LOGICAL_LINE_LEN) {
                break;
            }
            back_len = back_len.saturating_add(prior.len());
            phys_range.start -= 1
        }

        let mut phys_row = phys_range.start;
        while phys_row < phys_range.end {
            // Look forwards until we find the end of this logical line
            let mut total_len = 0;
            let mut end_inclusive = phys_row;

            // First pass to measure number of lines
            for idx in phys_row.. {
                if let Some(line) = self.lines.get(idx) {
                    if total_len > 0
                        && logical_len_exceeds_limit(total_len, line.len(), MAX_LOGICAL_LINE_LEN)
                    {
                        break;
                    }
                    end_inclusive = idx;
                    total_len = total_len.saturating_add(line.len());
                    if !line.last_cell_was_wrapped() {
                        break;
                    }
                } else if idx == phys_row {
                    // No more rows exist
                    return;
                } else {
                    break;
                }
            }

            let phys_range = phys_row..end_inclusive + 1;

            let logical_stable_range = self.phys_to_stable_row_index(phys_row)
                ..self.phys_to_stable_row_index(end_inclusive + 1);

            phys_row = end_inclusive + 1;

            if logical_stable_range.end < stable_range.start {
                continue;
            }
            if logical_stable_range.start > stable_range.end {
                break;
            }

            let mut continue_iteration = false;
            self.with_phys_lines_mut(phys_range, |lines| {
                continue_iteration = f(logical_stable_range.clone(), lines);
            });

            if !continue_iteration {
                break;
            }
        }
    }

    pub fn for_each_logical_line_in_stable_range<F>(
        &self,
        stable_range: Range<StableRowIndex>,
        mut f: F,
    ) where
        F: FnMut(Range<StableRowIndex>, &[&Line]) -> bool,
    {
        let mut phys_range = self.stable_range(&stable_range);

        // Avoid pathological cases where we have eg: a really long logical line
        // (such as 1.5MB of json) that we previously wrapped.  We don't want to
        // un-wrap, scan, and re-wrap that thing.
        // This is an imperfect length constraint to partially manage the cost.
        const MAX_LOGICAL_LINE_LEN: usize = 1024;

        // Look backwards to find the start of the first logical line
        let mut back_len = 0;
        while phys_range.start > 0 {
            let prior = &self.lines[phys_range.start - 1];
            if !prior.last_cell_was_wrapped() {
                break;
            }
            if logical_len_exceeds_limit(back_len, prior.len(), MAX_LOGICAL_LINE_LEN) {
                break;
            }
            back_len = back_len.saturating_add(prior.len());
            phys_range.start -= 1
        }

        let mut phys_row = phys_range.start;
        let mut line_vec: Vec<&Line> = vec![];
        while phys_row < phys_range.end {
            // Look forwards until we find the end of this logical line
            let mut total_len = 0;
            let mut end_inclusive = phys_row;
            line_vec.clear();

            for idx in phys_row.. {
                if let Some(line) = self.lines.get(idx) {
                    if total_len > 0
                        && logical_len_exceeds_limit(total_len, line.len(), MAX_LOGICAL_LINE_LEN)
                    {
                        break;
                    }
                    end_inclusive = idx;
                    total_len = total_len.saturating_add(line.len());
                    line_vec.push(line);
                    if !line.last_cell_was_wrapped() {
                        break;
                    }
                } else if idx == phys_row {
                    // No more rows exist
                    return;
                } else {
                    break;
                }
            }

            let logical_stable_range = self.phys_to_stable_row_index(phys_row)
                ..self.phys_to_stable_row_index(end_inclusive + 1);

            phys_row = end_inclusive + 1;

            if logical_stable_range.end < stable_range.start {
                continue;
            }
            if logical_stable_range.start > stable_range.end {
                break;
            }

            let continue_iteration = f(logical_stable_range, &line_vec);

            if !continue_iteration {
                break;
            }
        }
    }
}

fn phys_intersection(r1: &Range<PhysRowIndex>, r2: &Range<PhysRowIndex>) -> Range<PhysRowIndex> {
    let start = r1.start.max(r2.start);
    let end = r1.end.min(r2.end);
    if end > start {
        start..end
    } else {
        0..0
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::color::ColorPalette;
    use crate::config::ScrollbackSpillSink;
    use frankenterm_bidi::ParagraphDirectionHint;
    use frankenterm_cell::{Cell, CellAttributes};
    use frankenterm_surface::{CursorShape, CursorVisibility};

    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    #[test]
    fn logical_len_limit_check_treats_overflow_as_exceeded() {
        assert!(!logical_len_exceeds_limit(10, 5, 20));
        assert!(logical_len_exceeds_limit(10, 11, 20));
        assert!(logical_len_exceeds_limit(usize::MAX, 1, usize::MAX));
    }

    #[derive(Debug, Clone)]
    struct TestTermConfig {
        scrollback: usize,
        scrollback_tier: crate::config::ScrollbackTierConfig,
        cold_sink: Option<Arc<dyn crate::config::ScrollbackSpillSink>>,
        kp_cost_model: MonospaceKpCostModel,
        scorecard_enabled: bool,
        readability_gate: ResizeReadabilityGatePolicy,
    }

    impl Default for TestTermConfig {
        fn default() -> Self {
            Self {
                scrollback: 32,
                scrollback_tier: crate::config::ScrollbackTierConfig {
                    enabled: false,
                    hot_lines: 32,
                    warm_max_bytes: 0,
                },
                cold_sink: None,
                kp_cost_model: MonospaceKpCostModel::terminal_default(),
                scorecard_enabled: false,
                readability_gate: ResizeReadabilityGatePolicy {
                    enabled: false,
                    max_line_badness_delta: 0,
                    max_total_badness_delta: 0,
                    max_fallback_ratio_percent: 100,
                },
            }
        }
    }

    impl TerminalConfiguration for TestTermConfig {
        fn scrollback_size(&self) -> usize {
            self.scrollback
        }

        fn scrollback_tier_config(&self) -> crate::config::ScrollbackTierConfig {
            self.scrollback_tier
        }

        fn scrollback_spill_sink(&self) -> Option<Arc<dyn crate::config::ScrollbackSpillSink>> {
            self.cold_sink.clone()
        }

        fn color_palette(&self) -> ColorPalette {
            ColorPalette::default()
        }

        fn resize_wrap_kp_cost_model(&self) -> MonospaceKpCostModel {
            self.kp_cost_model
        }

        fn resize_wrap_scorecard_enabled(&self) -> bool {
            self.scorecard_enabled
        }

        fn resize_wrap_readability_gate_enabled(&self) -> bool {
            self.readability_gate.enabled
        }

        fn resize_wrap_readability_max_line_badness_delta(&self) -> i64 {
            self.readability_gate.max_line_badness_delta
        }

        fn resize_wrap_readability_max_total_badness_delta(&self) -> i64 {
            self.readability_gate.max_total_badness_delta
        }

        fn resize_wrap_readability_max_fallback_ratio_percent(&self) -> u8 {
            self.readability_gate.max_fallback_ratio_percent
        }
    }

    fn test_size(rows: usize, cols: usize, dpi: u32) -> TerminalSize {
        TerminalSize {
            rows,
            cols,
            pixel_width: cols * 8,
            pixel_height: rows * 16,
            dpi,
        }
    }

    fn test_cursor(x: usize, y: VisibleRowIndex, seqno: SequenceNo) -> CursorPosition {
        CursorPosition {
            x,
            y,
            shape: CursorShape::Default,
            visibility: CursorVisibility::Visible,
            seqno,
        }
    }

    fn bidi_mode() -> BidiMode {
        BidiMode {
            enabled: false,
            hint: ParagraphDirectionHint::LeftToRight,
        }
    }

    fn rtl_bidi_mode() -> BidiMode {
        BidiMode {
            enabled: true,
            hint: ParagraphDirectionHint::RightToLeft,
        }
    }

    fn test_screen_with_config(
        rows: usize,
        cols: usize,
        dpi: u32,
        config: TestTermConfig,
    ) -> Screen {
        let config: Arc<dyn TerminalConfiguration> = Arc::new(config);
        Screen::new(test_size(rows, cols, dpi), &config, true, 0, bidi_mode())
    }

    fn test_screen(rows: usize, cols: usize, dpi: u32) -> Screen {
        test_screen_with_config(rows, cols, dpi, TestTermConfig::default())
    }

    fn anchor_point(column: usize, row: StableRowIndex) -> Option<SelectionAnchorCoordinate> {
        Some(SelectionAnchorCoordinate {
            column: Some(column),
            row,
        })
    }

    #[test]
    fn selection_anchor_survives_unrelated_output_but_not_selected_row_mutation() {
        for change_selected_row in [false, true] {
            let config: Arc<dyn TerminalConfiguration> = Arc::new(TestTermConfig::default());
            let mut term = crate::Terminal::new(
                test_size(4, 12, 96),
                config,
                "anchor-source",
                "test",
                Box::new(Vec::<u8>::new()),
            );
            term.advance_bytes("A界BCDEF\r\nother".as_bytes());
            let sequence = term.current_seqno();
            let points = [anchor_point(3, 0); 3];
            let token = term
                .screen_mut()
                .capture_selection_anchor(sequence, points)
                .unwrap();
            term.advance_bytes(if change_selected_row {
                b"\x1b[1;1HZ"
            } else {
                b"\x1b[2;1HZ"
            });
            let after_output = term.current_seqno();
            assert!(after_output > sequence);
            assert_eq!(
                term.screen().resolve_selection_anchor(&token, after_output),
                (!change_selected_row).then_some(points)
            );
            term.resize(test_size(4, 2, 96));
            let mapped = term
                .screen()
                .resolve_selection_anchor(&token, term.current_seqno());
            if change_selected_row {
                assert!(
                    mapped.is_none(),
                    "selected-row mutation must reject anchor transport"
                );
            } else {
                let point =
                    mapped.expect("unrelated output must preserve held selection")[0].unwrap();
                let row = term.screen().stable_row_to_phys(point.row).unwrap();
                assert_eq!(
                    term.screen().lines[row]
                        .get_cell(point.column.unwrap())
                        .unwrap()
                        .str(),
                    "B"
                );
            }
        }
    }

    #[test]
    fn selection_anchor_rejects_mutation_inside_selected_span() {
        let mut screen = test_screen(3, 12, 96);
        for row in 0..3 {
            screen.lines[row] = Line::from_text("selected", &CellAttributes::blank(), 1, None);
        }
        let points = [anchor_point(0, 0), anchor_point(0, 0), anchor_point(3, 2)];
        let token = screen.capture_selection_anchor(1, points).unwrap();
        assert!(screen.resolve_selection_anchor(&token, 0).is_none());
        screen.lines[1].set_cell(0, Cell::new('Z', CellAttributes::blank()), 2);
        assert!(screen.resolve_selection_anchor(&token, 2).is_none());
        screen.resize(test_size(3, 6, 96), test_cursor(3, 2, 2), 3, false);
        assert!(screen.resolve_selection_anchor(&token, 3).is_none());
    }

    #[test]
    fn selection_anchor_projects_actual_wide_rows_and_repeated_widths() {
        let mut screen = test_screen(1, 12, 96);
        screen.lines[0] = Line::from_text("A界BCDEF", &CellAttributes::blank(), 1, None);
        let points = [anchor_point(0, 0), anchor_point(3, 0), anchor_point(7, 0)];
        let token = screen.capture_selection_anchor(1, points).unwrap();
        let after_last = screen
            .capture_selection_anchor(1, [anchor_point(8, 0); 3])
            .unwrap();
        let mut cursor = test_cursor(8, 0, 1);
        for (seqno, cols) in [(2, 2), (3, 5), (4, 12)] {
            cursor = screen.resize(test_size(1, cols, 96), cursor, seqno, false);
            let mapped = screen.resolve_selection_anchor(&token, seqno).unwrap();
            assert_eq!(
                screen.resolve_selection_anchor(&after_last, seqno),
                Some([anchor_point(cursor.x, screen.visible_row_to_stable_row(cursor.y)); 3])
            );
            for (point, text) in [
                (mapped[0].unwrap(), "A"),
                (mapped[1].unwrap(), "B"),
                (mapped[2].unwrap(), "F"),
            ] {
                let row = screen.stable_row_to_phys(point.row).unwrap();
                assert_eq!(
                    screen.lines[row]
                        .get_cell(point.column.unwrap())
                        .unwrap()
                        .str(),
                    text
                );
            }
            if cols == 2 {
                assert_eq!(
                    mapped[1],
                    anchor_point(0, 2),
                    "division by width would select the wide glyph instead of B"
                );
            }
        }
        screen.lines[0].set_cell(0, Cell::new('Z', CellAttributes::blank()), 5);
        assert!(
            screen.resolve_selection_anchor(&token, 5).is_none(),
            "later selected-row content is not the committed source"
        );
    }

    #[test]
    fn selection_anchor_preserves_before_zero_boundary_across_soft_wrap() {
        let mut screen = test_screen(1, 12, 96);
        screen.lines[0] = Line::from_text("A界BCDEF", &CellAttributes::blank(), 1, None);
        let cursor = screen.resize(test_size(1, 2, 96), test_cursor(8, 0, 1), 2, false);
        let before_b = [
            anchor_point(0, 0),
            anchor_point(0, 0),
            Some(SelectionAnchorCoordinate {
                row: 2,
                column: None,
            }),
        ];
        let token = screen.capture_selection_anchor(2, before_b).unwrap();
        screen.resize(test_size(1, 12, 96), cursor, 3, false);
        assert_eq!(
            screen.resolve_selection_anchor(&token, 3),
            Some([anchor_point(0, 0), anchor_point(0, 0), anchor_point(2, 0)])
        );
    }

    #[test]
    fn selection_anchor_preparation_does_not_publish_or_overwrite_live_points() {
        let mut screen = test_screen(1, 12, 96);
        screen.lines[0] = Line::from_text("A界BCDEF", &CellAttributes::blank(), 1, None);
        let first = screen
            .capture_selection_anchor(1, [anchor_point(0, 0); 3])
            .unwrap();
        let cursor = test_cursor(8, 0, 1);
        let size = test_size(1, 2, 96);
        let mut prepared = screen.capture_reflow_preparation(size, cursor).unwrap();
        assert!(prepared.snapshot.selection_anchors.0.is_empty());
        assert!(prepared.prepare(|| false));
        assert_eq!(
            screen.resolve_selection_anchor(&first, 1),
            Some([anchor_point(0, 0); 3])
        );
        let latest = screen
            .capture_selection_anchor(1, [anchor_point(3, 0); 3])
            .unwrap();
        screen.resize_with_prepared_reflow(size, cursor, 2, false, Some(&mut prepared));
        assert!(prepared.was_applied());
        assert_eq!(
            screen.resolve_selection_anchor(&latest, 2),
            Some([anchor_point(0, 2); 3])
        );
        assert_eq!(
            screen.resolve_selection_anchor(&first, 2),
            Some([anchor_point(0, 0); 3])
        );
    }

    #[test]
    fn selection_anchor_rejects_changed_source_pruned_rows_and_replaced_screen() {
        let mut screen = test_screen(3, 12, 96);
        screen.lines[0] = Line::from_text("A界BCDEF", &CellAttributes::blank(), 1, None);
        let points = [anchor_point(0, 0); 3];
        let old = screen.capture_selection_anchor(1, points).unwrap();
        assert!(screen.clone().resolve_selection_anchor(&old, 1).is_none());
        assert!(screen
            .capture_selection_anchor(SequenceNo::MAX, points)
            .is_none());
        assert!(screen
            .capture_selection_anchor(1, [anchor_point(0, -1); 3])
            .is_none());
        screen.lines[0].set_cell(0, Cell::new('Z', CellAttributes::blank()), 2);
        screen.resize(test_size(3, 5, 96), test_cursor(8, 0, 2), 3, false);
        assert!(
            screen.resolve_selection_anchor(&old, 3).is_none(),
            "a source change between capture and resize must reject transport"
        );

        let pruned = screen
            .capture_selection_anchor(3, [anchor_point(0, 2); 3])
            .unwrap();
        screen.resize(test_size(1, 12, 96), test_cursor(3, 1, 3), 4, false);
        assert!(screen.resolve_selection_anchor(&pruned, 4).is_none());
        let current = screen.capture_selection_anchor(4, points).unwrap();
        screen.invalidate_coordinate_witnesses();
        assert!(screen.resolve_selection_anchor(&current, 4).is_none());
        screen.publish_selection_anchor_sequence(4, 5);
        assert!(
            screen.resolve_selection_anchor(&current, 5).is_none(),
            "publication cannot bless a replaced coordinate domain"
        );
    }

    #[test]
    fn selection_anchor_registry_is_bounded_and_reclaims_retired_tokens() {
        let mut screen = test_screen(1, 12, 96);
        let points = [anchor_point(0, 0); 3];
        for malformed in [
            [points[0], points[1], None],
            [points[0], None, points[2]],
            [
                None,
                Some(SelectionAnchorCoordinate {
                    row: 0,
                    column: None,
                }),
                None,
            ],
        ] {
            assert!(screen.capture_selection_anchor(1, malformed).is_none());
        }
        assert!(
            screen.selection_anchors.0.is_empty(),
            "malformed tuples must not consume registry capacity"
        );
        let tokens: Vec<_> = (0..16)
            .map(|_| screen.capture_selection_anchor(1, points).unwrap())
            .collect();
        assert!(screen.capture_selection_anchor(1, points).is_none());
        assert_eq!(screen.selection_anchors.0.len(), 16);
        drop(tokens);
        assert!(screen.capture_selection_anchor(1, points).is_some());
        assert_eq!(screen.selection_anchors.0.len(), 1);
    }

    #[test]
    fn compute_pool_matches_scalar_wraps_and_bounded_cancellation() {
        let pool = ReflowComputePool {
            pool: rayon::ThreadPoolBuilder::new()
                .num_threads(4)
                .build()
                .unwrap(),
            admission: std::sync::Mutex::new(()),
        };
        let logical: Vec<_> = (0..257)
            .map(|idx| {
                Line::from_text(
                    &format!("{idx}:{}", "界👩‍💻e\u{301}אב abcdefghijklmnop".repeat(8)),
                    &CellAttributes::blank(),
                    1,
                    None,
                )
            })
            .collect();
        let mut scalar = test_screen_with_scorecard(4, 120);
        let mut parallel = scalar.clone();
        for cols in [61, 200, 79, 61] {
            assert!(scalar.wrap_logical_lines_with_compute_pool(
                &logical,
                cols,
                2,
                None,
                &|| false,
                None,
            ));
            assert!(parallel.wrap_logical_lines_with_compute_pool(
                &logical,
                cols,
                2,
                None,
                &|| false,
                Some(&pool),
            ));
            let expected = scalar.clone_wrapped_from_scratch(logical.len());
            let actual = parallel.clone_wrapped_from_scratch(logical.len());
            assert_eq!(actual, expected);
            assert_eq!(
                parallel.rewrap_line_cache_hits,
                scalar.rewrap_line_cache_hits
            );
            assert_eq!(
                parallel.last_resize_wrap_scorecard,
                scalar.last_resize_wrap_scorecard
            );
            assert_eq!(
                Screen::compute_layout_signature_for_lines(
                    actual.iter().flat_map(|rows| rows.iter())
                ),
                Screen::compute_layout_signature_for_lines(
                    expected.iter().flat_map(|rows| rows.iter())
                ),
            );
        }
        // A busy pool must not wait (including recursively on the same thread).
        let guard = pool.admission.lock().unwrap();
        assert!(parallel.wrap_logical_lines_with_compute_pool(
            &logical,
            79,
            3,
            None,
            &|| false,
            Some(&pool),
        ));
        drop(guard);
        for selected_pool in [None, Some(&pool)] {
            let polls = std::cell::Cell::new(0);
            let mut screen = test_screen(4, 120, 96);
            assert!(!screen.wrap_logical_lines_with_compute_pool(
                &logical,
                61,
                4,
                None,
                &|| {
                    polls.set(polls.get() + 1);
                    polls.get() == 2
                },
                selected_pool,
            ));
            assert_eq!(polls.get(), 2);
            assert_eq!(
                screen
                    .rewrap_scratch_slots
                    .iter()
                    .filter(|slot| slot.is_some())
                    .count(),
                MAX_REFLOW_BATCH_LOGICAL_LINES
            );
        }
    }

    #[cfg(feature = "use_serde")]
    pub(crate) fn cold_seam_test_terminal() -> (crate::Terminal, Arc<TestColdScrollbackSink>) {
        let sink = Arc::new(TestColdScrollbackSink::default());
        let config: Arc<dyn TerminalConfiguration> = Arc::new(TestTermConfig {
            scrollback: 32,
            scrollback_tier: crate::config::ScrollbackTierConfig {
                enabled: true,
                hot_lines: 1,
                warm_max_bytes: 0,
            },
            cold_sink: Some(sink.clone()),
            ..TestTermConfig::default()
        });
        let mut terminal = crate::Terminal::new(
            test_size(2, 4, 96),
            config,
            "FrankenTerm",
            "cold-seam-test",
            Box::new(std::io::sink()),
        );
        terminal.advance_bytes(b"abcdefgh\r\none\r\n");
        terminal.resize(test_size(2, 3, 96));
        (terminal, sink)
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn cold_seam_alignment_survives_spilling_a_wrapped_resident_row() {
        let (mut terminal, _) = cold_seam_test_terminal();
        let mut seam = terminal
            .screen()
            .capture_cold_seam_reflow()
            .unwrap()
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        let screen = terminal.screen_mut();
        assert!(screen.install_cold_seam_reflow(&mut seam, 20).unwrap());
        let end = screen.phys_to_stable_row_index(screen.lines.len());
        let before = screen
            .capture_line_read(0..end)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        let expected: String = before
            .lines()
            .map(|line| line.as_str().into_owned())
            .collect();
        let frontier = screen.phys_to_stable_row_index(0);
        let head = screen.lines.front().unwrap().clone();
        assert!(
            head.last_cell_was_wrapped(),
            "fixture must spill an open logical group"
        );
        assert!(screen.record_scrollback_spill(frontier, &head, 21));
        assert_eq!(
            before.fragments.as_ref().unwrap().aligned_frontier,
            frontier
        );
        assert_eq!(
            screen.cold_row_fragments.as_ref().unwrap().aligned_frontier,
            frontier + 1
        );
        assert!(Arc::ptr_eq(
            &before.fragments.as_ref().unwrap().rows,
            &screen.cold_row_fragments.as_ref().unwrap().rows,
        ));
        screen.lines.pop_front().unwrap();
        screen.advance_stable_row_index_offset(1);
        screen.lines.push_back(Line::new(21));
        let end = screen.phys_to_stable_row_index(screen.lines.len());
        let after = screen
            .capture_line_read(0..end)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        assert!(screen.validates_line_read(&after));
        let actual: String = after
            .lines()
            .map(|line| line.as_str().into_owned())
            .collect();
        assert_eq!(
            actual, expected,
            "moving a wrapped row to cold storage must preserve every cell"
        );
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn cold_seam_spilling_hard_line_end_preserves_newline() {
        for blank_tail in [false, true] {
            let (mut terminal, _) = cold_seam_test_terminal();
            let mut seam = terminal
                .screen()
                .capture_cold_seam_reflow()
                .unwrap()
                .unwrap()
                .hydrate(|| false)
                .unwrap();
            let screen = terminal.screen_mut();
            assert!(screen.install_cold_seam_reflow(&mut seam, 20).unwrap());
            let mut spilled_hard_end = false;
            for _ in 0..screen.lines.len() + 2 {
                let frontier = screen.phys_to_stable_row_index(0);
                let head = screen.lines.front().unwrap().clone();
                let wrapped = head.last_cell_was_wrapped();
                assert!(screen.record_scrollback_spill(frontier, &head, 21));
                screen.lines.pop_front().unwrap();
                screen.advance_stable_row_index_offset(1);
                screen.lines.push_back(Line::new(21));
                if !wrapped && (!blank_tail || head.len() == 0) {
                    spilled_hard_end = true;
                    break;
                }
            }
            assert!(
                spilled_hard_end,
                "fixture must spill the requested hard end"
            );
            let frontier = screen.phys_to_stable_row_index(0);
            let captured = screen.capture_line_read(frontier - 1..frontier).unwrap();
            let geometry = captured
                .geometry_cold_layout(ScreenLineRead::MAX_PAYLOAD_BYTES, &|| false)
                .unwrap()
                .unwrap()
                .0;
            let streamed = captured
                .streamed_cold_layout(ScreenLineRead::MAX_PAYLOAD_BYTES, &|| false)
                .unwrap()
                .0;
            assert_eq!(geometry.source, streamed.source);
            assert_eq!(geometry.visual, streamed.visual);
            assert_eq!(geometry.groups, streamed.groups);
            let read = captured.hydrate(|| false).unwrap();
            assert!(screen.validates_line_read(&read));
            assert_eq!(read.row_count(), 1);
            let row = read.lines().next().unwrap();
            assert!(
                !row.last_cell_was_wrapped(),
                "hard newline became a soft wrap"
            );
            if blank_tail {
                assert_eq!(row.len(), 0, "empty hard line must remain present");
            }
            screen.install_line_read_layout(&read, 22);
            let (first, rows) = screen.lines_in_stable_range(frontier - 1..frontier);
            assert_eq!(first, frontier - 1);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].as_str(), row.as_str());
            assert!(!rows[0].last_cell_was_wrapped());
        }
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn cold_line_layout_parser_spill_preserves_exact_corpus() {
        let sink = Arc::new(TestColdScrollbackSink::default());
        let config: Arc<dyn TerminalConfiguration> = Arc::new(TestTermConfig {
            // This test must not confuse intentional retention eviction with
            // corruption when the narrow Unicode record and probe spill.
            scrollback: 1024,
            scrollback_tier: crate::config::ScrollbackTierConfig {
                enabled: true,
                hot_lines: 1,
                warm_max_bytes: 0,
            },
            cold_sink: Some(sink),
            ..TestTermConfig::default()
        });
        let mut terminal = crate::Terminal::new(
            test_size(2, 4, 96),
            config,
            "FrankenTerm",
            "cold-line-layout-corpus",
            Box::new(std::io::sink()),
        );
        terminal.advance_bytes(b"abcdefgh\r\none\r\n");
        terminal.resize(test_size(2, 3, 96));
        if let Some(seam) = terminal.screen().capture_cold_seam_reflow().unwrap() {
            let mut seam = seam.hydrate(|| false).unwrap();
            assert!(terminal
                .screen_mut()
                .install_cold_seam_reflow(&mut seam, 20)
                .unwrap());
        }
        let record = format!(
            "FT_RECORD_00000 A  B\u{00a0}C\u{2003}D e\u{0301} 界面 🚀 {}FT_END_00000\r\n",
            "0123456789 abcdefghijklmnopqrstuvwxyz ".repeat(3),
        );
        terminal.advance_bytes(record.as_bytes());
        terminal.resize(test_size(2, 3, 96));
        if let Some(seam) = terminal.screen().capture_cold_seam_reflow().unwrap() {
            let mut seam = seam.hydrate(|| false).unwrap();
            assert!(terminal
                .screen_mut()
                .install_cold_seam_reflow(&mut seam, 20)
                .unwrap());
        }
        let end = terminal
            .screen()
            .phys_to_stable_row_index(terminal.screen().lines.len());
        let before = terminal
            .screen()
            .capture_line_read(0..end)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        terminal.screen_mut().install_line_read_layout(&before, 20);
        // Hydration establishes the visual origin, which can move below zero
        // when narrower wrapping adds rows above the resident frontier.
        let first = terminal.screen().scrollback_top_stable_row();
        let before = terminal
            .screen()
            .capture_line_read(first..end)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        let expected: String = before
            .lines()
            .map(|line| line.as_str().into_owned())
            .collect();
        assert_eq!(
            expected,
            format!("abcdefghone{}", record.trim_end_matches("\r\n"))
        );

        terminal.advance_bytes(b"FT_PROBE warmup 2 3\r\n");

        let new_end = terminal
            .screen()
            .phys_to_stable_row_index(terminal.screen().lines.len());
        let after = terminal
            .screen()
            .capture_line_read(0..new_end)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        assert!(terminal.screen().validates_line_read(&after));
        // The synchronous reader must not use an older canonical map after
        // the probe advances the resident frontier. Compare before publishing
        // the new off-lock layout so this exercises the stale-cache boundary.
        let (sync_first, sync_rows) = terminal.screen_mut().lines_in_stable_range(0..new_end);
        assert_eq!(sync_first, after.first_row());
        assert_eq!(
            sync_rows
                .iter()
                .map(|line| (line.as_str().into_owned(), line.last_cell_was_wrapped()))
                .collect::<Vec<_>>(),
            after
                .lines()
                .map(|line| (line.as_str().into_owned(), line.last_cell_was_wrapped()))
                .collect::<Vec<_>>(),
            "synchronous read after spill must agree with fresh canonical geometry"
        );
        terminal.screen_mut().install_line_read_layout(&after, 21);
        let first = terminal.screen().scrollback_top_stable_row();
        let after = terminal
            .screen()
            .capture_line_read(first..new_end)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        assert!(terminal.screen().validates_line_read(&after));
        let actual: String = after
            .lines()
            .map(|line| line.as_str().into_owned())
            .collect();
        assert!(
            actual.starts_with(&expected),
            "parser output after installed cold line layout must preserve exact corpus: \
             expected={:?} actual={:?}",
            expected,
            actual,
        );
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn cold_seam_alignment_does_not_advance_without_current_geometry_and_receipt() {
        for change in 0..5 {
            let (mut terminal, sink) = cold_seam_test_terminal();
            let mut seam = terminal
                .screen()
                .capture_cold_seam_reflow()
                .unwrap()
                .unwrap()
                .hydrate(|| false)
                .unwrap();
            let screen = terminal.screen_mut();
            assert!(screen.install_cold_seam_reflow(&mut seam, 20).unwrap());
            let frontier = screen.phys_to_stable_row_index(0);
            let head = screen.lines.front().unwrap().clone();
            match change {
                0 => screen.physical_cols += 1,
                1 => screen.dpi += 1,
                2 => sink.omit_admission_receipt.store(true, Ordering::Relaxed),
                3 => sink.refuse_admission.store(true, Ordering::Relaxed),
                4 => *sink.interval_identity.lock().unwrap() = Default::default(),
                _ => unreachable!(),
            }
            assert_eq!(
                screen.record_scrollback_spill(frontier, &head, 21),
                change != 3
            );
            assert_eq!(
                screen.cold_row_fragments.as_ref().unwrap().aligned_frontier,
                frontier,
                "unqualified spill must not advance the certificate: case {}",
                change
            );
        }
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn cold_seam_metadata_busy_retains_exact_prepared_transaction() {
        let (mut terminal, sink) = cold_seam_test_terminal();
        let mut plan = terminal
            .screen()
            .capture_cold_seam_reflow()
            .unwrap()
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        assert!(plan.is_ready());
        let before = terminal.screen().lines.clone();
        sink.force_busy_probe.store(true, Ordering::Release);
        let capture = terminal.screen().capture_cold_seam_reflow();
        assert!(capture.err().unwrap().is::<ColdReadMetadataBusy>());
        let installation = terminal
            .screen_mut()
            .install_cold_seam_reflow(&mut plan, 20);
        assert!(installation.unwrap_err().is::<ColdReadMetadataBusy>());
        assert_eq!(terminal.screen().lines, before);
        assert!(
            plan.is_ready(),
            "busy metadata does not consume the prepared cells"
        );
        sink.force_busy_probe.store(false, Ordering::Release);
        assert!(terminal
            .screen_mut()
            .install_cold_seam_reflow(&mut plan, 20)
            .unwrap());
        assert!(
            !terminal
                .screen_mut()
                .install_cold_seam_reflow(&mut plan, 21)
                .unwrap(),
            "a consumed or stale transaction remains a definitive rejection"
        );
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn cold_seam_transaction_preserves_keys_and_moves_real_cells_atomically() {
        let sink = Arc::new(TestColdScrollbackSink::default());
        let mut cold = Line::from_text("abcd", &CellAttributes::blank(), 1, None);
        cold.set_last_cell_was_wrapped(true, 1);
        assert!(sink.store_scrollback_line(0, &cold, 32));
        let mut screen = test_screen_with_config(
            2,
            3,
            96,
            TestTermConfig {
                cold_sink: Some(sink.clone()),
                ..TestTermConfig::default()
            },
        );
        screen.stable_row_index_offset = 1;
        let mut head = Line::from_text("efg", &CellAttributes::blank(), 1, None);
        head.set_last_cell_was_wrapped(true, 1);
        screen.lines = [
            head,
            Line::from_text("h", &CellAttributes::blank(), 1, None),
            Line::new(1),
            Line::new(1),
        ]
        .into();
        let before = screen.lines.clone();
        let plan = screen.capture_cold_seam_reflow().unwrap().unwrap();
        assert!(plan.hydrate(|| true).is_err());
        assert_eq!(screen.lines, before);
        let mut stale = screen
            .capture_cold_seam_reflow()
            .unwrap()
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        screen.lines[0].update_last_change_seqno(2);
        assert!(!screen.install_cold_seam_reflow(&mut stale, 3).unwrap());
        screen.lines[0].update_last_change_seqno(1);
        let mut plan = screen
            .capture_cold_seam_reflow()
            .unwrap()
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        assert!(plan.is_ready());
        assert!(screen.install_cold_seam_reflow(&mut plan, 2).unwrap());
        assert_eq!(screen.lines[0].as_str(), "def");
        assert_eq!(screen.lines[1].as_str(), "gh");
        assert_eq!(screen.stable_row_index_offset, 1);
        assert_eq!(
            sink.load_scrollback_line(0).unwrap(),
            cold,
            "immutable source unchanged"
        );
        assert!(
            screen.capture_cold_seam_reflow().unwrap().is_none(),
            "same-width transaction is idempotent"
        );
        let read = screen
            .capture_line_read(0..3)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        assert!(screen.validates_line_read(&read));
        assert_eq!(
            read.lines()
                .map(|line| line.as_str().into_owned())
                .collect::<Vec<_>>(),
            ["abc", "def", "gh"]
        );
        screen.install_line_read_layout(&read, 3);
        assert_eq!(
            screen
                .lines_in_stable_range(0..3)
                .1
                .iter()
                .map(|line| line.as_str().into_owned())
                .collect::<Vec<_>>(),
            ["abc", "def", "gh"]
        );
        assert!(
            !screen.install_cold_seam_reflow(&mut plan, 4).unwrap(),
            "old authority cannot publish twice"
        );
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn cold_seam_reuses_retained_fragments_across_width_changes_without_payload_reads() {
        for (cold_text, head_text, tail_text) in [
            ("abcd", "efg", "h"),
            ("界ab", "cde", "f"),
            ("e\u{301}abc", "def", "g"),
            ("🚀ab", "cde", "f"),
        ] {
            let sink = Arc::new(TestColdScrollbackSink::default());
            let mut cold = Line::from_text(cold_text, &CellAttributes::blank(), 1, None);
            cold.set_last_cell_was_wrapped(true, 1);
            assert!(sink.store_scrollback_line(0, &cold, 32));
            let mut screen = test_screen_with_config(
                2,
                3,
                96,
                TestTermConfig {
                    cold_sink: Some(sink.clone()),
                    ..TestTermConfig::default()
                },
            );
            screen.stable_row_index_offset = 1;
            let mut head = Line::from_text(head_text, &CellAttributes::blank(), 1, None);
            head.set_last_cell_was_wrapped(true, 1);
            screen.lines = [
                head,
                Line::from_text(tail_text, &CellAttributes::blank(), 1, None),
                Line::new(1),
                Line::new(1),
            ]
            .into();
            let mut initial = screen
                .capture_cold_seam_reflow()
                .unwrap()
                .unwrap()
                .hydrate(|| false)
                .unwrap();
            assert!(screen.install_cold_seam_reflow(&mut initial, 2).unwrap());
            let mut cursor = test_cursor(0, 1, 2);
            for (index, cols) in [2, 3, 2, 3].iter().copied().enumerate() {
                let seqno = 3 + index * 2;
                cursor = screen.resize(test_size(2, cols, 96), cursor, seqno, false);
                let capture = screen.capture_cold_seam_reflow().unwrap().unwrap();
                let before = screen.lines.clone();
                sink.batch_reads.store(0, Ordering::Relaxed);
                sink.single_reads.store(0, Ordering::Relaxed);
                let mut ready = capture.hydrate(|| false).unwrap();
                assert!(ready.is_ready());
                assert_eq!(sink.batch_reads.load(Ordering::Relaxed), 0, "cols={cols}");
                assert_eq!(sink.single_reads.load(Ordering::Relaxed), 0);
                assert_eq!(screen.lines, before, "preparation cannot publish");

                // Force the established storage path without changing the
                // retained replacement rows: only invalidate its optional
                // boundary certificate in this independent preparation.
                let mut reference = screen.capture_cold_seam_reflow().unwrap().unwrap();
                let mut fragments = reference.previous.as_deref().unwrap().clone();
                fragments.aligned_frontier += 1;
                reference.previous = Some(Arc::new(fragments));
                let reference = reference.hydrate(|| false).unwrap();
                assert!(sink.batch_reads.load(Ordering::Relaxed) > 0);
                let (actual_fragments, actual_rows) = ready.replacement.as_ref().unwrap();
                let (expected_fragments, expected_rows) = reference.replacement.as_ref().unwrap();
                assert_eq!(actual_fragments.rows, expected_fragments.rows);
                assert_eq!(actual_rows, expected_rows);
                assert_eq!(ready.source, reference.source);
                assert!(screen
                    .install_cold_seam_reflow(&mut ready, seqno + 1)
                    .unwrap());
                assert_eq!(screen.phys_to_stable_row_index(0), 1);
                let fragments = screen.cold_row_fragments.as_ref().unwrap();
                let actual = fragments.rows[&0].as_str().into_owned()
                    + &screen
                        .lines
                        .iter()
                        .map(|line| line.as_str().into_owned())
                        .collect::<String>();
                assert_eq!(
                    actual.trim_end(),
                    format!("{cold_text}{head_text}{tail_text}")
                );
            }
        }
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn cold_seam_fragment_reuse_rejects_cancelled_and_replaced_sources() {
        for replace_before_capture in [false, true] {
            let (mut terminal, sink) = cold_seam_test_terminal();
            let mut initial = terminal
                .screen()
                .capture_cold_seam_reflow()
                .unwrap()
                .unwrap()
                .hydrate(|| false)
                .unwrap();
            assert!(terminal
                .screen_mut()
                .install_cold_seam_reflow(&mut initial, 20)
                .unwrap());
            terminal.resize(test_size(2, 2, 96));
            let before = terminal.screen().lines.clone();
            let cancelled = terminal
                .screen()
                .capture_cold_seam_reflow()
                .unwrap()
                .unwrap();
            sink.batch_reads.store(0, Ordering::Relaxed);
            assert!(cancelled.hydrate(|| true).is_err());
            assert_eq!(sink.batch_reads.load(Ordering::Relaxed), 0);
            let mut prepared = if replace_before_capture {
                None
            } else {
                Some(
                    terminal
                        .screen()
                        .capture_cold_seam_reflow()
                        .unwrap()
                        .unwrap()
                        .hydrate(|| false)
                        .unwrap(),
                )
            };
            let key = *sink.rows.lock().unwrap().first_key_value().unwrap().0;
            assert!(sink.store_scrollback_line(
                key,
                &Line::from_text("new", &CellAttributes::blank(), 21, None),
                32,
            ));
            if let Some(prepared) = prepared.as_mut() {
                assert!(!terminal
                    .screen_mut()
                    .install_cold_seam_reflow(prepared, 22)
                    .unwrap());
            } else {
                assert!(terminal.screen().capture_cold_seam_reflow().is_err());
            }
            assert_eq!(terminal.screen().lines, before);
        }
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn cold_seam_refuses_empty_continuation_and_visible_head_without_mutation() {
        let sink = Arc::new(TestColdScrollbackSink::default());
        let mut cold = Line::from_text("a", &CellAttributes::blank(), 1, None);
        cold.set_last_cell_was_wrapped(true, 1);
        assert!(sink.store_scrollback_line(0, &cold, 32));
        let mut screen = test_screen_with_config(
            2,
            3,
            96,
            TestTermConfig {
                cold_sink: Some(sink.clone()),
                ..TestTermConfig::default()
            },
        );
        screen.stable_row_index_offset = 1;
        let mut head = Line::from_text("bcd", &CellAttributes::blank(), 1, None);
        head.set_last_cell_was_wrapped(true, 1);
        screen.lines = [
            head,
            Line::from_text("ef", &CellAttributes::blank(), 1, None),
            Line::new(1),
            Line::new(1),
        ]
        .into();
        let before = screen.lines.clone();
        let error = screen
            .capture_cold_seam_reflow()
            .unwrap()
            .unwrap()
            .hydrate(|| false)
            .err()
            .unwrap();
        assert!(error.to_string().contains("zero-cell continuation"));
        assert_eq!(screen.lines, before);
        assert_eq!(sink.load_scrollback_line(0).unwrap(), cold);
        assert!(screen.cold_row_fragments.is_none());
        screen.physical_rows = 3;
        assert!(
            screen.capture_cold_seam_reflow().unwrap().is_none(),
            "never modify a logical head crossing into the live viewport"
        );
        screen.physical_rows = 2;
        screen.lines[0] = Line::from_text(&"x".repeat(65_537), &CellAttributes::blank(), 1, None);
        screen.lines[0].compress_for_scrollback();
        assert!(screen
            .capture_cold_seam_reflow()
            .err()
            .unwrap()
            .to_string()
            .contains("boundary work limit"));
        assert_eq!(
            screen.lines[0].len(),
            65_537,
            "bounded refusal preserves source"
        );
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn cold_seam_partial_retention_invalidates_alignment_without_reusing_keys() {
        let sink = Arc::new(TestColdScrollbackSink::default());
        for (key, text) in [(0, "abcd"), (1, "efg")] {
            let mut line = Line::from_text(text, &CellAttributes::blank(), 1, None);
            line.set_last_cell_was_wrapped(true, 1);
            assert!(sink.store_scrollback_line(key, &line, 32));
        }
        let mut screen = test_screen_with_config(
            2,
            3,
            96,
            TestTermConfig {
                cold_sink: Some(sink.clone()),
                ..TestTermConfig::default()
            },
        );
        screen.stable_row_index_offset = 2;
        screen.lines = [
            Line::from_text("h", &CellAttributes::blank(), 1, None),
            Line::new(1),
            Line::new(1),
        ]
        .into();
        let mut plan = screen
            .capture_cold_seam_reflow()
            .unwrap()
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        assert!(screen.install_cold_seam_reflow(&mut plan, 2).unwrap());
        assert_eq!(screen.lines[0].as_str(), "gh");
        sink.rows.lock().unwrap().remove(&0);
        sink.batch_reads.store(0, Ordering::Relaxed);
        let mut after_trim = screen
            .capture_cold_seam_reflow()
            .unwrap()
            .expect("partial trim must not reuse old alignment")
            .hydrate(|| false)
            .unwrap();
        assert!(
            sink.batch_reads.load(Ordering::Relaxed) > 0,
            "partial retention must use the storage path, not the old seam boundary"
        );
        assert!(screen.install_cold_seam_reflow(&mut after_trim, 3).unwrap());
        assert_eq!(screen.lines[0].as_str(), "h");
        assert_eq!(screen.stable_row_index_offset, 2);
        assert_eq!(sink.load_scrollback_line(1).unwrap().as_str(), "efg");
        let read = screen
            .capture_line_read(1..3)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        assert!(screen.validates_line_read(&read));
        assert_eq!(
            read.lines()
                .map(|line| line.as_str().into_owned())
                .collect::<Vec<_>>(),
            ["efg", "h"]
        );
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn synchronous_cold_read_does_not_rebase_when_metadata_probe_is_busy() {
        let sink = Arc::new(TestColdScrollbackSink::default());
        let stored = Line::from_text("disk", &CellAttributes::blank(), 1, None);
        assert!(sink.store_scrollback_line(0, &stored, 10));
        let mut screen = test_screen_with_config(
            3,
            4,
            96,
            TestTermConfig {
                cold_sink: Some(sink.clone()),
                ..TestTermConfig::default()
            },
        );
        screen.stable_row_index_offset = 1;
        sink.force_busy_probe.store(true, Ordering::Relaxed);
        let (first, lines) = screen.lines_in_stable_range(0..4);
        assert_eq!(first, 0);
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].as_str(), "disk");
    }

    #[cfg(feature = "use_serde")]
    fn streamed_cold_fragment_fixture(
        originals: &[Line],
        replacements: BTreeMap<StableRowIndex, Line>,
        cols: usize,
    ) -> (Screen, Arc<TestColdScrollbackSink>) {
        let sink = Arc::new(TestColdScrollbackSink::default());
        for (row, line) in originals.iter().enumerate() {
            assert!(sink.store_scrollback_line(row as StableRowIndex, line, originals.len()));
        }
        let mut screen = test_screen_with_config(
            3,
            cols,
            96,
            TestTermConfig {
                cold_sink: Some(sink.clone()),
                ..TestTermConfig::default()
            },
        );
        screen.stable_row_index_offset = originals.len();
        let crate::config::ScrollbackIntervalCapture::Ready(interval) =
            sink.try_capture_scrollback_interval()
        else {
            panic!("fixture interval must be ready");
        };
        screen.cold_row_fragments = Some(Arc::new(ColdRowFragments {
            sink: sink.clone(),
            interval,
            rows: Arc::new(replacements),
            aligned_frontier: originals.len() as StableRowIndex,
            aligned_source_start: 0,
            cols,
            dpi: 96,
            policy: screen.resize_wrap_policy,
        }));
        (screen, sink)
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn streamed_cold_index_unchanged_rows_have_one_exact_charge_with_fragments() {
        use std::cell::Cell as Counter;

        let originals = [
            Line::from_text(&"x".repeat(128), &CellAttributes::blank(), 1, None),
            Line::from_text("abc", &CellAttributes::blank(), 1, None),
        ];
        let replacement = Line::from_text("xy", &CellAttributes::blank(), 1, None);
        let (screen, sink) =
            streamed_cold_fragment_fixture(&originals, [(1, replacement.clone())].into(), 256);
        // The first closed group sets the exact peak source budget. The next
        // group must still pay for its real replacement and retained metadata.
        let exact = serde_json::to_vec(&originals[0]).unwrap().len();
        let plan = screen.capture_line_read(0..1).unwrap();
        let (layout, metadata_bytes) = plan.streamed_cold_layout(exact, &|| false).unwrap();
        assert_eq!(layout.groups, vec![(0..1, 0..1), (1..2, 1..2)]);
        assert!(
            serde_json::to_vec(&originals[1]).unwrap().len()
                + serde_json::to_vec(&replacement).unwrap().len()
                + metadata_bytes
                < exact
        );
        assert!(!plan.index_budget_exhausted.load(Ordering::Acquire));

        // Cancel between the unchanged group and the actual fragment, after
        // real decoding/accounting work. No index is published or poisoned.
        sink.batch_reads.store(0, Ordering::Relaxed);
        let checks = Counter::new(0);
        let cancelled = plan
            .streamed_cold_layout(exact, &|| {
                checks.set(checks.get() + 1);
                checks.get() >= 3
            })
            .unwrap_err();
        assert!(cancelled.to_string().contains("cold index cancelled"));
        assert_eq!(sink.batch_reads.load(Ordering::Relaxed), 1);
        assert!(screen.cold_visual_layout.is_none());
        assert!(!plan.index_budget_exhausted.load(Ordering::Acquire));

        let refused = plan.streamed_cold_layout(exact - 1, &|| false).unwrap_err();
        assert!(refused.downcast_ref::<ColdReadPayloadLimit>().is_some());
        assert_eq!(sink.load_scrollback_line(0).unwrap(), originals[0]);
        assert_eq!(sink.load_scrollback_line(1).unwrap(), originals[1]);
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn streamed_cold_index_actual_fragment_charges_original_and_replacement() {
        let mut original =
            Line::from_text(&"original ".repeat(8), &CellAttributes::blank(), 1, None);
        original.set_last_cell_was_wrapped(true, 1);
        let originals = [
            original,
            Line::from_text("end", &CellAttributes::blank(), 1, None),
        ];
        let mut attrs = CellAttributes::blank();
        attrs.set_hyperlink(Some(Arc::new(termwiz::hyperlink::Hyperlink::new(format!(
            "https://example.invalid/{}",
            "a".repeat(8_192)
        )))));
        let mut replacement = Line::from_text("xy", &attrs, 1, None);
        replacement.set_last_cell_was_wrapped(true, 1);
        let (screen, sink) =
            streamed_cold_fragment_fixture(&originals, [(0, replacement.clone())].into(), 8);
        let original_bytes = serde_json::to_vec(&originals[0]).unwrap().len();
        let tail_bytes = serde_json::to_vec(&originals[1]).unwrap().len();
        let replacement_bytes = serde_json::to_vec(&replacement).unwrap().len();
        assert!(replacement_bytes > 8_192, "attributes consume the budget");
        let exact = original_bytes + replacement_bytes + tail_bytes;
        let plan = screen.capture_line_read(0..1).unwrap();
        let (layout, metadata_bytes) = plan.streamed_cold_layout(exact, &|| false).unwrap();
        assert!(metadata_bytes < exact);
        // The replacement's five-cell joined group fits one row; wrapping the
        // original source instead would produce multiple rows at this width.
        assert_eq!(layout.groups, vec![(0..2, 1..2)]);
        for limit in [
            exact - 1,
            replacement_bytes + tail_bytes,
            original_bytes + tail_bytes,
        ] {
            let refused = plan.streamed_cold_layout(limit, &|| false).unwrap_err();
            assert!(refused.downcast_ref::<ColdReadPayloadLimit>().is_some());
        }
        assert!(screen.cold_visual_layout.is_none());
        assert_eq!(sink.load_scrollback_line(0).unwrap(), originals[0]);
        assert_eq!(sink.load_scrollback_line(1).unwrap(), originals[1]);
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn canonical_context_charges_only_actual_fragment_replacements() {
        let originals = [
            Line::from_text(&"x".repeat(128), &CellAttributes::blank(), 1, None),
            Line::from_text("original", &CellAttributes::blank(), 1, None),
        ];
        let mut attrs = CellAttributes::blank();
        attrs.set_hyperlink(Some(Arc::new(termwiz::hyperlink::Hyperlink::new(format!(
            "https://example.invalid/{}",
            "a".repeat(8_192)
        )))));
        let replacement = Line::from_text("xy", &attrs, 1, None);
        let (screen, _) =
            streamed_cold_fragment_fixture(&originals, [(1, replacement.clone())].into(), 256);
        let plan = screen.capture_line_read(0..1).unwrap();
        let (layout, _) = plan
            .streamed_cold_layout(ScreenLineRead::MAX_PAYLOAD_BYTES, &|| false)
            .unwrap();
        for row in 0..2 {
            let source = if row == 1 {
                &replacement
            } else {
                &originals[row]
            };
            let mut output = source.clone();
            // Alignment certifies coordinates, not a continuation. These
            // source rows end in hard newlines, including the replacement.
            let _ = output.cells_mut_for_attr_changes_only();
            let exact = serde_json::to_vec(&originals[row]).unwrap().len()
                + if row == 1 {
                    serde_json::to_vec(&replacement).unwrap().len()
                } else {
                    0
                }
                + serde_json::to_vec(&output).unwrap().len();
            let request = row as StableRowIndex..row as StableRowIndex + 1;
            let mut captured = screen.capture_line_read(request.clone()).unwrap();
            captured.layout = Some(Arc::clone(&layout));
            let ready = captured
                .hydrate_with_payload_limit(exact, || false)
                .unwrap_or_else(|error| panic!("row={} exact={}: {:#}", row, exact, error));
            assert_eq!(ready.payload_bytes(), exact);
            assert_eq!(ready.lines().cloned().collect::<Vec<_>>(), vec![output]);
            assert!(screen.validates_line_read(&ready));
            let mut captured = screen.capture_line_read(request).unwrap();
            captured.layout = Some(Arc::clone(&layout));
            let refused = captured
                .hydrate_with_payload_limit(exact - 1, || false)
                .err()
                .unwrap();
            assert!(refused.downcast_ref::<ColdReadPayloadLimit>().is_some());
        }
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn streamed_cold_index_maps_100001_rows_without_retaining_decoded_history() {
        use std::cell::Cell as Counter;
        const SOURCE_ROWS: usize = 100_001;
        let sink = Arc::new(TestColdScrollbackSink::default());
        let source = Line::from_text("abcd", &CellAttributes::blank(), 1, None);
        *sink.rows.lock().unwrap() = (0..SOURCE_ROWS)
            .map(|row| (row as StableRowIndex, source.clone()))
            .collect();
        let mut screen = test_screen_with_config(
            3,
            3,
            96,
            TestTermConfig {
                cold_sink: Some(sink.clone()),
                ..TestTermConfig::default()
            },
        );
        screen.stable_row_index_offset = SOURCE_ROWS;
        let checks = Counter::new(0usize);
        let cancelled = screen.capture_line_read(0..1).unwrap().hydrate(|| {
            checks.set(checks.get() + 1);
            checks.get() > 1000
        });
        assert!(cancelled.is_err());
        assert!(sink.batch_reads.load(Ordering::Relaxed) < SOURCE_ROWS as u64 / 2);
        assert!(screen.cold_visual_layout.is_none());
        let read = screen
            .capture_line_read(0..1)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        let layout = read
            .layout
            .as_ref()
            .expect("large prefix must build real visual metadata");
        assert_eq!(layout.groups.len(), SOURCE_ROWS);
        assert_eq!(layout.source, 0..SOURCE_ROWS as StableRowIndex);
        assert_eq!(
            layout.visual,
            -(SOURCE_ROWS as StableRowIndex)..SOURCE_ROWS as StableRowIndex
        );
        assert!(
            read.hydrated.is_empty(),
            "indexing must retire all decoded source rows"
        );
        assert_eq!(
            read.logical_view.as_ref().unwrap().1.len(),
            2,
            "only requested logical group is retained"
        );
        assert_eq!(read.lines().next().unwrap().as_str(), "d");
        assert!(read.payload_bytes() <= ScreenLineRead::MAX_PAYLOAD_BYTES);
        assert!(screen.validates_line_read(&read));
        screen.install_line_read_layout(&read, 2);
        assert!(screen.validates_line_read(&read));
        assert!(!screen.line_read_changes_layout(&read));
        assert_eq!(
            screen.scrollback_geometry(),
            (-(SOURCE_ROWS as StableRowIndex), SOURCE_ROWS * 2 + 3)
        );
        let first = -(SOURCE_ROWS as StableRowIndex);
        let head = screen
            .capture_line_read(first..first + 2)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        assert_eq!(
            head.lines()
                .map(|line| line.as_str().into_owned())
                .collect::<Vec<_>>(),
            ["abc", "d"]
        );
        assert_eq!(
            screen
                .lines_in_stable_range(first..first + 2)
                .1
                .iter()
                .map(|line| line.as_str().into_owned())
                .collect::<Vec<_>>(),
            ["abc", "d"]
        );
        assert!(sink.store_scrollback_line(0, &source, SOURCE_ROWS));
        assert!(
            !screen.validates_line_read(&read),
            "same-key source replacement rejects a complete old index"
        );
        assert!(!screen.validates_line_read(&head));
    }

    #[cfg(feature = "use_serde")]
    #[derive(Debug)]
    struct ColdPrefetchTestSink {
        inner: TestColdScrollbackSink,
        max_batch_rows: usize,
        requests: Mutex<Vec<Range<StableRowIndex>>>,
        metadata_probes: AtomicU64,
        metadata_probe_budget: AtomicU64,
        cancel_on_read: AtomicBool,
        cancelled: AtomicBool,
        overfill: AtomicBool,
    }

    #[cfg(feature = "use_serde")]
    impl ScrollbackSpillSink for ColdPrefetchTestSink {
        fn try_capture_scrollback_interval(&self) -> crate::config::ScrollbackIntervalCapture {
            self.metadata_probes.fetch_add(1, Ordering::Relaxed);
            if self
                .metadata_probe_budget
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |left| match left {
                    u64::MAX => Some(left),
                    0 => None,
                    _ => Some(left - 1),
                })
                .is_err()
            {
                return crate::config::ScrollbackIntervalCapture::Busy;
            }
            self.inner.try_capture_scrollback_interval()
        }
        fn try_capture_scrollback_usage(&self) -> crate::config::ScrollbackUsageCapture {
            self.inner.try_capture_scrollback_usage()
        }
        fn store_scrollback_line(&self, row: StableRowIndex, line: &Line, limit: usize) -> bool {
            self.inner.store_scrollback_line(row, line, limit)
        }
        fn load_scrollback_line(&self, row: StableRowIndex) -> Option<Line> {
            self.inner.load_scrollback_line(row)
        }
        fn load_scrollback_lines(&self, requested: Range<StableRowIndex>) -> Vec<Line> {
            self.requests.lock().unwrap().push(requested.clone());
            assert!(requested.end - requested.start <= 32);
            let end = if self.overfill.load(Ordering::Relaxed) {
                requested.end + 1
            } else {
                requested.end
            };
            let rows = self.inner.rows.lock().unwrap();
            let result = (requested.start..end)
                .take(self.max_batch_rows)
                .map_while(|row| rows.get(&row).cloned())
                .collect();
            if self.cancel_on_read.load(Ordering::Relaxed) {
                self.cancelled.store(true, Ordering::Relaxed);
            }
            result
        }
        fn oldest_scrollback_row(&self) -> Option<StableRowIndex> {
            self.inner.oldest_scrollback_row()
        }
        fn retained_scrollback_rows(&self) -> usize {
            self.inner.retained_scrollback_rows()
        }
        fn retained_scrollback_bytes(&self) -> usize {
            self.inner.retained_scrollback_bytes()
        }
        fn snapshot_scrollback(
            &self,
            _: StableRowIndex,
            _: crate::config::ScrollbackSnapshotLimits,
        ) -> Result<crate::config::ScrollbackSnapshot, crate::config::ScrollbackSpillError>
        {
            panic!("cold prefetch must not request a full storage snapshot")
        }
        fn replace_scrollback_prefix(
            &self,
            _: Option<crate::config::ScrollbackSnapshotGeneration>,
            _: crate::config::ScrollbackPrefix<'_>,
            _: usize,
        ) -> Result<crate::config::ScrollbackReplaceCommit, crate::config::ScrollbackSpillError>
        {
            panic!("cold prefetch must not replace storage")
        }
        fn clear_scrollback(
            &self,
        ) -> Result<crate::config::ScrollbackClearCommit, crate::config::ScrollbackSpillError>
        {
            panic!("cold prefetch must not clear storage")
        }
    }

    #[cfg(feature = "use_serde")]
    fn cold_prefetch_fixture(max_batch_rows: usize) -> (Screen, Arc<ColdPrefetchTestSink>) {
        let sink = Arc::new(ColdPrefetchTestSink {
            inner: TestColdScrollbackSink::default(),
            max_batch_rows,
            requests: Mutex::new(Vec::new()),
            metadata_probes: AtomicU64::new(0),
            metadata_probe_budget: AtomicU64::new(u64::MAX),
            cancel_on_read: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            overfill: AtomicBool::new(false),
        });
        let mut attrs = CellAttributes::blank();
        attrs.set_italic(true);
        for row in 0..65 {
            let line = Line::from_text(&format!("{row:02}界e\u{301}"), &attrs, 1, None);
            assert!(sink.store_scrollback_line(row, &line, 65));
        }
        let mut screen = test_screen_with_config(
            3,
            8,
            96,
            TestTermConfig {
                cold_sink: Some(sink.clone()),
                ..TestTermConfig::default()
            },
        );
        screen.stable_row_index_offset = 65;
        let indexed = screen
            .capture_line_read(0..1)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        assert!(screen.validates_line_read(&indexed));
        screen.install_line_read_layout(&indexed, 2);
        assert_eq!(
            screen.current_cold_visual_layout().unwrap().groups.len(),
            65
        );
        sink.requests.lock().unwrap().clear();
        (screen, sink)
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn cold_prefetch_coalesces_groups_with_exact_rows_and_charges() {
        for batch_rows in [32, 2] {
            let (screen, sink) = cold_prefetch_fixture(batch_rows);
            let mut expected = Vec::new();
            let mut expected_bytes = 0;
            // Separate logical-group reads are the non-coalesced reference.
            for row in 0..65 {
                let read = screen
                    .capture_line_read(row..row + 1)
                    .unwrap()
                    .hydrate(|| false)
                    .unwrap();
                expected.extend(read.lines().cloned());
                expected_bytes += read.payload_bytes();
            }
            assert_eq!(sink.requests.lock().unwrap().len(), 65);
            sink.requests.lock().unwrap().clear();
            let read = screen
                .capture_line_read(0..65)
                .unwrap()
                .hydrate(|| false)
                .unwrap();
            assert_eq!(read.lines().cloned().collect::<Vec<_>>(), expected);
            assert_eq!(read.payload_bytes(), expected_bytes);
            assert!(screen.validates_line_read(&read));
            let requests = sink.requests.lock().unwrap();
            assert_eq!(requests.len(), 65usize.div_ceil(batch_rows));
            assert_eq!(requests.first(), Some(&(0..32)));
            assert_eq!(requests.last(), Some(&(64..65)));
            drop(requests);
            let replacement = Line::from_text("changed", &CellAttributes::blank(), 1, None);
            assert!(sink.store_scrollback_line(20, &replacement, 65));
            assert!(
                !screen.validates_line_read(&read),
                "same-key replacement must reject publication"
            );
        }
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn cold_prefetch_continues_wrapped_groups_across_batch_boundaries() {
        for batch_rows in [32, 2] {
            let (mut screen, sink) = cold_prefetch_fixture(batch_rows);
            for row in 30..33 {
                let mut line = Line::from_text("abcdefgh", &CellAttributes::blank(), 1, None);
                line.set_last_cell_was_wrapped(row < 32, 1);
                assert!(sink.store_scrollback_line(row, &line, 65));
            }
            assert!(screen.current_cold_visual_layout().is_none());
            let indexed = screen
                .capture_line_read(0..1)
                .unwrap()
                .hydrate(|| false)
                .unwrap();
            assert!(screen.validates_line_read(&indexed));
            screen.install_line_read_layout(&indexed, 3);
            let groups = &screen.current_cold_visual_layout().unwrap().groups;
            assert_eq!(groups[30], (30..33, 30..33));
            let mut expected = Vec::new();
            let mut expected_bytes = 0;
            for (_, visual) in groups {
                let read = screen
                    .capture_line_read(visual.clone())
                    .unwrap()
                    .hydrate(|| false)
                    .unwrap();
                expected.extend(read.lines().cloned());
                expected_bytes += read.payload_bytes();
            }
            sink.requests.lock().unwrap().clear();
            let read = screen
                .capture_line_read(0..65)
                .unwrap()
                .hydrate(|| false)
                .unwrap();
            assert_eq!(read.lines().cloned().collect::<Vec<_>>(), expected);
            assert_eq!(read.payload_bytes(), expected_bytes);
            assert!(screen.validates_line_read(&read));
            assert_eq!(
                sink.requests.lock().unwrap().len(),
                65usize.div_ceil(batch_rows)
            );
        }
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn cold_prefetch_stops_at_selected_context_and_refuses_missing_or_excess_rows() {
        let (screen, sink) = cold_prefetch_fixture(33);
        sink.inner.rows.lock().unwrap().remove(&20);
        let read = screen
            .capture_line_read(18..20)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        assert_eq!(read.row_count(), 2);
        assert_eq!(
            sink.requests.lock().unwrap().as_slice(),
            std::slice::from_ref(&(18..20))
        );
        sink.requests.lock().unwrap().clear();
        let error = screen
            .capture_line_read(18..21)
            .unwrap()
            .hydrate(|| false)
            .err()
            .expect("a missing row must refuse the entire read");
        assert!(error
            .to_string()
            .contains("cold logical context unavailable"));
        assert_eq!(*sink.requests.lock().unwrap(), [18..21, 20..21]);
        sink.requests.lock().unwrap().clear();
        sink.overfill.store(true, Ordering::Relaxed);
        let error = screen
            .capture_line_read(0..2)
            .unwrap()
            .hydrate(|| false)
            .err()
            .expect("an oversized sink response must be refused");
        assert!(error
            .to_string()
            .contains("cold logical context unavailable"));
        assert_eq!(
            sink.requests.lock().unwrap().as_slice(),
            std::slice::from_ref(&(0..2))
        );
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn cold_prefetch_cancels_before_read_and_after_a_short_batch() {
        let (screen, sink) = cold_prefetch_fixture(2);
        assert!(screen
            .capture_line_read(0..65)
            .unwrap()
            .hydrate(|| true)
            .is_err());
        assert!(sink.requests.lock().unwrap().is_empty());
        sink.cancel_on_read.store(true, Ordering::Relaxed);
        let error = screen
            .capture_line_read(0..65)
            .unwrap()
            .hydrate(|| sink.cancelled.load(Ordering::Relaxed))
            .err()
            .expect("cancellation during the first batch must stop continuation");
        assert!(error.to_string().contains("cold read cancelled"));
        assert_eq!(
            sink.requests.lock().unwrap().as_slice(),
            std::slice::from_ref(&(0..32))
        );
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn cold_prefetch_preserves_fragment_charges_and_payload_boundary() {
        let (mut screen, sink) = cold_prefetch_fixture(32);
        let crate::config::ScrollbackIntervalCapture::Ready(interval) =
            sink.try_capture_scrollback_interval()
        else {
            panic!("fixture interval must be available");
        };
        screen.cold_row_fragments = Some(Arc::new(ColdRowFragments {
            sink: sink.clone(),
            interval,
            rows: Arc::new(
                [(
                    31,
                    Line::from_text("换界", &CellAttributes::blank(), 1, None),
                )]
                .into(),
            ),
            aligned_frontier: 65,
            aligned_source_start: 0,
            cols: 8,
            dpi: 96,
            policy: screen.resize_wrap_policy,
        }));
        let mut expected = Vec::new();
        let mut expected_bytes = 0;
        for row in 0..64 {
            let read = screen
                .capture_line_read(row..row + 1)
                .unwrap()
                .hydrate(|| false)
                .unwrap();
            expected.extend(read.lines().cloned());
            expected_bytes += read.payload_bytes();
        }
        sink.requests.lock().unwrap().clear();
        let read = screen
            .capture_line_read(0..64)
            .unwrap()
            .hydrate_with_payload_limit(expected_bytes, || false)
            .unwrap();
        assert_eq!(read.lines().cloned().collect::<Vec<_>>(), expected);
        assert_eq!(read.lines().nth(31).unwrap().as_str(), "换界");
        assert_eq!(read.payload_bytes(), expected_bytes);
        assert!(screen.validates_line_read(&read));
        assert_eq!(*sink.requests.lock().unwrap(), [0..32, 32..64]);
        assert!(screen
            .capture_line_read(0..64)
            .unwrap()
            .hydrate_with_payload_limit(expected_bytes - 1, || false)
            .is_err());
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn owned_line_read_reuses_capture_interval_and_rechecks_before_io() {
        let (screen, sink) = cold_prefetch_fixture(32);
        let canonical = screen.cold_visual_layout.as_ref().unwrap();
        assert!(!canonical.stored_physical());
        assert!(screen.cold_geometry_index.is_none());
        assert!(screen.stored_physical_layout.is_none());
        sink.requests.lock().unwrap().clear();
        sink.metadata_probe_budget.store(0, Ordering::Relaxed);
        assert!(screen
            .capture_line_read(17..18)
            .err()
            .unwrap()
            .is::<ColdReadMetadataBusy>());
        assert!(sink.requests.lock().unwrap().is_empty());

        let mut last_read = None;
        for (requested, row) in [
            (17..18, 17),
            (StableRowIndex::MIN..StableRowIndex::MIN + 1, 0),
        ] {
            sink.metadata_probes.store(0, Ordering::Relaxed);
            sink.metadata_probe_budget.store(1, Ordering::Relaxed);
            let captured = screen.capture_line_read(requested.clone()).unwrap();
            assert_eq!(
                sink.metadata_probes.load(Ordering::Relaxed),
                1,
                "one coherent capture must not re-probe and lose its canonical cache"
            );
            assert!(Arc::ptr_eq(captured.layout.as_ref().unwrap(), canonical));
            assert_eq!(captured.first_row(), row);
            sink.requests.lock().unwrap().clear();
            assert!(captured
                .hydrate(|| false)
                .err()
                .unwrap()
                .is::<ColdReadMetadataBusy>());
            assert!(
                sink.requests.lock().unwrap().is_empty(),
                "busy worker preflight must refuse before backing IO"
            );

            // A fresh attempt needs one capture probe and one worker probe.
            // Neither stage may discard the canonical layout and reindex it.
            sink.metadata_probes.store(0, Ordering::Relaxed);
            sink.metadata_probe_budget.store(2, Ordering::Relaxed);
            let captured = screen.capture_line_read(requested).unwrap();
            assert!(Arc::ptr_eq(captured.layout.as_ref().unwrap(), canonical));
            let read = captured.hydrate(|| false).unwrap();
            assert_eq!(sink.metadata_probes.load(Ordering::Relaxed), 2);
            assert_eq!(
                sink.requests.lock().unwrap().as_slice(),
                std::slice::from_ref(&(row..row + 1)),
                "worker preflight must not trigger a 65-group payload reindex"
            );
            assert_eq!(
                read.lines().next().unwrap().as_str(),
                format!("{row:02}界e\u{301}")
            );
            assert!(
                !screen.validates_line_read(&read),
                "publication still needs fresh metadata authority"
            );
            sink.metadata_probe_budget
                .store(u64::MAX, Ordering::Relaxed);
            assert!(screen.validates_line_read(&read));
            last_read = Some(read);
        }
        assert!(sink.store_scrollback_line(
            0,
            &Line::from_text("new source", &CellAttributes::blank(), 3, None),
            65
        ));
        assert!(
            !screen.validates_line_read(&last_read.unwrap()),
            "same-key destructive source replacement cannot reuse the earlier captured authority"
        );
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn owned_line_read_payload_boundary_and_resident_mutation() {
        let mut screen = test_screen(3, 8, 96);
        screen.lines[0] = Line::from_text("actual row", &CellAttributes::blank(), 1, None);
        let bytes = serde_json::to_vec(&screen.lines[0]).unwrap().len();
        assert!(
            !screen.validates_line_read(&screen.capture_line_read(0..1).unwrap()),
            "uncounted resident plans cannot publish"
        );
        let read = screen
            .capture_line_read(0..1)
            .unwrap()
            .hydrate_with_payload_limit(bytes, || false)
            .unwrap();
        assert_eq!(read.payload_bytes(), bytes);
        assert_eq!(read.lines().next().unwrap().as_str(), "actual row");
        assert!(screen.validates_line_read(&read));
        assert!(!screen.clone().validates_line_read(&read));
        assert!(screen
            .capture_line_read(0..1)
            .unwrap()
            .hydrate_with_payload_limit(bytes - 1, || false)
            .is_err());
        assert!(screen
            .capture_line_read(0..1)
            .unwrap()
            .hydrate(|| true)
            .is_err());
        screen.lines[0] = Line::from_text("changed", &CellAttributes::blank(), 2, None);
        assert!(!screen.validates_line_read(&read));
        assert!(
            read.hydrate(|| false).is_err(),
            "a read cannot amplify its retained rows by hydrating twice"
        );
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn owned_line_read_checks_requested_rows_before_clamping() {
        let screen = test_screen(3, 8, 96);
        assert!(screen
            .capture_line_read(0..ScreenLineRead::MAX_ROWS as StableRowIndex)
            .is_ok());
        assert!(screen
            .capture_line_read(0..ScreenLineRead::MAX_ROWS as StableRowIndex + 1)
            .is_err());
        let read = screen
            .capture_line_read(-10..-8)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        assert_eq!(read.first_row(), 0);
        assert_eq!(read.row_count(), 2);
        assert!(screen.validates_line_read(&read));
        let read = screen
            .capture_line_read(100..102)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        assert_eq!(read.first_row(), 1);
        assert_eq!(read.row_count(), 2);
        assert!(screen.validates_line_read(&read));
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn owned_line_read_capture_budget_is_shared_before_clustered_clone() {
        let mut screen = test_screen(3, 8, 96);
        screen.lines[0] = Line::from_text(&"x".repeat(4096), &CellAttributes::blank(), 1, None);
        screen.lines[0].compress_for_scrollback();
        let mut bytes = usize::MAX;
        let mut work = 65_536;
        let _ = screen.lines[0]
            .try_clone_for_snapshot(&mut bytes, &mut work)
            .unwrap();
        let cost = usize::MAX - bytes;
        assert!(cost >= 4096);
        let mut budget = LineReadCaptureBudget {
            bytes_left: cost - 1,
            work_left: 65_536,
        };
        assert!(screen
            .capture_line_read_with_budget(0..1, &mut budget)
            .is_err());
        assert_eq!(budget.bytes_left, cost - 1);
        let mut budget = LineReadCaptureBudget {
            bytes_left: cost,
            work_left: 65_536,
        };
        let first = screen
            .capture_line_read_with_budget(0..1, &mut budget)
            .unwrap();
        assert_eq!(budget.bytes_left, 0);
        assert!(
            screen
                .capture_line_read_with_budget(0..1, &mut budget)
                .is_err(),
            "separate ranges cannot reset the request budget"
        );
        drop(first);
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn owned_line_read_cold_interval_busy_trim_and_replacement() {
        use crate::config::{
            ScrollbackIntervalCapture, ScrollbackIntervalIdentity, ScrollbackSpillSink,
        };
        #[derive(Debug)]
        struct Sink {
            state: Mutex<(ScrollbackIntervalIdentity, BTreeMap<StableRowIndex, Line>)>,
            io_gate: Mutex<()>,
            reads: AtomicU64,
            synchronous_metadata_calls_remaining: AtomicU64,
            entered: Mutex<Option<std::sync::mpsc::SyncSender<()>>>,
        }
        impl ScrollbackSpillSink for Sink {
            fn store_scrollback_line(&self, _: StableRowIndex, _: &Line, _: usize) -> bool {
                false
            }
            fn load_scrollback_line(&self, row: StableRowIndex) -> Option<Line> {
                self.reads.fetch_add(1, Ordering::Relaxed);
                if let Some(entered) = self.entered.lock().unwrap().take() {
                    entered.send(()).unwrap();
                }
                let _io_gate = self.io_gate.lock().unwrap();
                self.state.lock().unwrap().1.get(&row).cloned()
            }
            fn oldest_scrollback_row(&self) -> Option<StableRowIndex> {
                assert_eq!(
                    self.synchronous_metadata_calls_remaining.compare_exchange(
                        1,
                        0,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ),
                    Ok(1),
                    "owned reads must not call the blocking metadata accessor"
                );
                self.state
                    .lock()
                    .unwrap()
                    .1
                    .first_key_value()
                    .map(|(row, _)| *row)
            }
            fn retained_scrollback_rows(&self) -> usize {
                panic!("blocking metadata accessor")
            }
            fn retained_scrollback_bytes(&self) -> usize {
                panic!("blocking metadata accessor")
            }
            fn snapshot_scrollback(
                &self,
                _: StableRowIndex,
                _: crate::config::ScrollbackSnapshotLimits,
            ) -> Result<crate::config::ScrollbackSnapshot, crate::config::ScrollbackSpillError>
            {
                panic!("bounded read must not request a full snapshot")
            }
            fn replace_scrollback_prefix(
                &self,
                _: Option<crate::config::ScrollbackSnapshotGeneration>,
                _: crate::config::ScrollbackPrefix<'_>,
                _: usize,
            ) -> Result<crate::config::ScrollbackReplaceCommit, crate::config::ScrollbackSpillError>
            {
                panic!("read must not replace storage")
            }
            fn clear_scrollback(
                &self,
            ) -> Result<crate::config::ScrollbackClearCommit, crate::config::ScrollbackSpillError>
            {
                panic!("read must not clear storage")
            }
            fn try_capture_scrollback_interval(&self) -> ScrollbackIntervalCapture {
                let Ok(state) = self.state.try_lock() else {
                    return ScrollbackIntervalCapture::Busy;
                };
                let range = state
                    .1
                    .first_key_value()
                    .zip(state.1.last_key_value())
                    .map(|((first, _), (last, _))| *first..*last + 1);
                state.0.capture(range)
            }
            fn try_capture_scrollback_usage(&self) -> crate::config::ScrollbackUsageCapture {
                let Ok(state) = self.state.try_lock() else {
                    return crate::config::ScrollbackUsageCapture::Busy;
                };
                let count = state.1.len();
                let bytes = state.1.values().map(Screen::estimate_line_bytes).sum();
                crate::config::ScrollbackUsageCapture::Ready(crate::config::ScrollbackUsage {
                    rows: count,
                    bytes,
                })
            }
        }
        let sink = Arc::new(Sink {
            state: Mutex::new((
                ScrollbackIntervalIdentity::default(),
                (-1..4)
                    .map(|row| {
                        (
                            row,
                            Line::from_text(
                                &format!("cold-{row}"),
                                &CellAttributes::blank(),
                                1,
                                None,
                            ),
                        )
                    })
                    .collect(),
            )),
            io_gate: Mutex::new(()),
            reads: AtomicU64::new(0),
            synchronous_metadata_calls_remaining: AtomicU64::new(0),
            entered: Mutex::new(None),
        });
        let mut screen = test_screen_with_config(
            3,
            8,
            96,
            TestTermConfig {
                cold_sink: Some(sink.clone()),
                ..TestTermConfig::default()
            },
        );
        screen.stable_row_index_offset = 4;
        let plan = screen.capture_line_read(1..6).unwrap();
        assert_eq!(
            sink.reads.load(Ordering::Relaxed),
            0,
            "capture performs no backing reads"
        );
        let read = std::thread::spawn(move || plan.hydrate(|| false))
            .join()
            .unwrap()
            .unwrap();
        assert_eq!(read.first_row(), 1);
        assert_eq!(read.row_count(), 5);
        assert_eq!(
            read.lines()
                .take(3)
                .map(|line| line.as_str().to_string())
                .collect::<Vec<_>>(),
            ["cold-1", "cold-2", "cold-3"]
        );
        assert!(screen.validates_line_read(&read));
        {
            let _busy = sink.state.lock().unwrap();
            assert!(screen.capture_line_read(1..2).is_err());
            assert!(!screen.validates_line_read(&read));
            assert_eq!(
                screen.try_validate_line_read(&read),
                Err(ColdReadMetadataBusy)
            );
            assert!(
                screen.capture_line_read(4..5).is_ok(),
                "resident capture does not wait for backing IO"
            );
        }
        assert_eq!(screen.try_validate_line_read(&read), Ok(true));
        sink.state.lock().unwrap().1.remove(&-1);
        assert!(
            !screen.validates_line_read(&read),
            "a whole-prefix visual index must be rebuilt after prefix trim"
        );
        // Retention trims a contiguous prefix; an interior hole cannot be
        // represented by the sink's interval witness.
        sink.state.lock().unwrap().1.remove(&0);
        sink.state.lock().unwrap().1.remove(&1);
        assert!(!screen.validates_line_read(&read));
        let read = screen
            .capture_line_read(2..4)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        sink.state.lock().unwrap().0 = ScrollbackIntervalIdentity::default();
        assert!(
            !screen.validates_line_read(&read),
            "same-coordinate replacement is a new authority"
        );
        let plan = screen.capture_line_read(2..5).unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        *sink.entered.lock().unwrap() = Some(entered_tx);
        // Storage IO can block while its nonblocking interval metadata remains
        // available. Holding `state` would instead refuse hydration at preflight
        // and never exercise the owned plan's independence from Screen guards.
        let blocked_io = sink.io_gate.lock().unwrap();
        let worker = std::thread::spawn(move || plan.hydrate(|| false));
        let entered = entered_rx.recv_timeout(std::time::Duration::from_secs(5));
        // Mutate the real Screen while storage is blocked: no terminal/Screen
        // guard can have escaped in the owned plan.
        screen.lines[0] = Line::from_text("main remains live", &CellAttributes::blank(), 3, None);
        assert!(screen.capture_line_read(4..5).is_ok());
        drop(blocked_io);
        let read = worker.join().unwrap().unwrap();
        assert!(entered.is_ok(), "worker reached backing IO");
        assert!(
            !screen.validates_line_read(&read),
            "resident mutation during IO rejects publication"
        );

        // Real worker-side reflow with complete predecessor/successor context:
        // 4-column source rows occupy the same two coordinates at width5.
        let mut first = Line::from_text("abcd", &CellAttributes::blank(), 4, None);
        first.set_last_cell_was_wrapped(true, 4);
        {
            let mut state = sink.state.lock().unwrap();
            state.0 = ScrollbackIntervalIdentity::default();
            state.1 = vec![
                (0, first),
                (
                    1,
                    Line::from_text("efgh", &CellAttributes::blank(), 4, None),
                ),
            ]
            .into_iter()
            .collect();
        }
        screen.stable_row_index_offset = 2;
        screen.physical_cols = 5;
        let read = screen
            .capture_line_read(0..1)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        assert_eq!(
            read.lines().next().unwrap().as_str(),
            "abcde",
            "successor context contributes its first cell"
        );
        assert!(screen.validates_line_read(&read));
        let read = screen
            .capture_line_read(1..2)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        assert_eq!(
            read.lines().next().unwrap().as_str(),
            "fgh",
            "predecessor context shifts cells without shifting row identity"
        );
        assert!(screen.validates_line_read(&read));
        screen.physical_cols = 3;
        assert!(
            !screen.validates_line_read(&read),
            "width witness rejects pre-resize layout"
        );
        let plan = screen.capture_line_read(0..2).unwrap();
        let failure = plan.failure_witness();
        assert!(failure.matches(&screen));
        let read = plan.hydrate(|| false).unwrap();
        assert!(screen.validates_line_read(&read));
        screen.install_line_read_layout(&read, 5);
        assert_eq!(screen.scrollback_top_stable_row(), -1);
        assert_eq!(screen.reachable_scrollback_rows(), screen.lines.len() + 3);
        let read = screen
            .capture_line_read(-1..2)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        assert_eq!(read.first_row(), -1);
        assert_eq!(
            read.lines()
                .map(|line| line.as_str().to_string())
                .collect::<Vec<_>>(),
            ["abc", "def", "gh"]
        );
        // The synchronous semantic API needs authoritative metadata even when
        // storage is busy. Permit exactly its one metadata read; owned capture,
        // hydration, publication and later native checks retain the panic guard.
        sink.synchronous_metadata_calls_remaining
            .store(1, Ordering::Relaxed);
        let (first, lines) = screen.lines_in_stable_range(-1..2);
        assert_eq!(
            sink.synchronous_metadata_calls_remaining
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(first, -1);
        assert_eq!(
            lines
                .iter()
                .map(|line| line.as_str().to_string())
                .collect::<Vec<_>>(),
            ["abc", "def", "gh"]
        );
        assert_eq!(
            sink.state
                .lock()
                .unwrap()
                .1
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            [0, 1],
            "visual remapping never rewrites stored keys"
        );
        assert!(
            !failure.matches(&screen),
            "publishing a new layout invalidates an old remembered failure"
        );
        let failure = screen.capture_line_read(0..1).unwrap().failure_witness();
        assert!(failure.matches(&screen));
        screen.physical_cols = 5;
        assert!(
            !failure.matches(&screen),
            "a new width can retry a previously refused layout"
        );
        screen.physical_cols = 3;
        sink.state.lock().unwrap().0 = ScrollbackIntervalIdentity::default();
        assert!(
            !failure.matches(&screen),
            "same-coordinate replacement invalidates remembered failure"
        );
        // An unrelated trailing cold/resident seam must not suppress earlier
        // complete logical groups, including groups whose row count changes.
        let mut tail = Line::from_text("part", &CellAttributes::blank(), 6, None);
        tail.set_last_cell_was_wrapped(true, 6);
        {
            let mut state = sink.state.lock().unwrap();
            state.0 = ScrollbackIntervalIdentity::default();
            state.1 = vec![
                (
                    0,
                    Line::from_text("older", &CellAttributes::blank(), 6, None),
                ),
                (1, tail),
            ]
            .into_iter()
            .collect();
        }
        let read = screen
            .capture_line_read(0..1)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        assert_eq!(read.lines().next().unwrap().as_str(), "er");
        assert!(screen.validates_line_read(&read));
        screen.install_line_read_layout(&read, 6);
        assert_eq!(screen.scrollback_top_stable_row(), -1);
        assert_eq!(screen.get_changed_stable_rows(-1..1, 5), [-1, 0]);
        let before_reads = sink.reads.load(Ordering::Relaxed);
        let indexed = screen
            .capture_line_read(-1..0)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        assert_eq!(indexed.lines().next().unwrap().as_str(), "old");
        assert_eq!(
            sink.reads.load(Ordering::Relaxed) - before_reads,
            1,
            "index reuse reads only the intersecting source group, not the trailing seam"
        );
        assert_eq!(
            indexed.cached_lines(-1..1).unwrap().1.len(),
            2,
            "logical context survives a partial viewport request"
        );
        assert!(
            screen
                .capture_line_read(1..2)
                .unwrap()
                .hydrate(|| false)
                .is_err(),
            "seam remains explicitly unavailable until its atomic transaction"
        );
        assert_eq!(screen.refresh_cold_source_observation(), Ok(Some(true)));
        assert_eq!(screen.refresh_cold_source_observation(), Ok(Some(false)));
        sink.state.lock().unwrap().0 = ScrollbackIntervalIdentity::default();
        assert_eq!(
            screen.refresh_cold_source_observation(),
            Ok(Some(true)),
            "same bounds with new source lineage changes wire authority"
        );
        {
            let _busy = sink.state.lock().unwrap();
            assert_eq!(
                screen.refresh_cold_source_observation(),
                Err(ColdReadMetadataBusy)
            );
        }
        // A full-index budget failure is a one-time optimization refusal,
        // not a permanent refusal of a small previously admissible read.
        {
            let mut state = sink.state.lock().unwrap();
            state.0 = ScrollbackIntervalIdentity::default();
            state.1 = (0..41)
                .map(|row| {
                    (
                        row,
                        Line::from_text("small", &CellAttributes::blank(), 7, None),
                    )
                })
                .collect();
            state.1.insert(
                0,
                Line::from_text(&"x".repeat(64 * 1024), &CellAttributes::blank(), 7, None),
            );
        }
        screen.stable_row_index_offset = 41;
        screen.physical_cols = 8;
        assert_eq!(screen.refresh_cold_source_observation(), Ok(Some(true)));
        let plan = screen.capture_line_read(40..41).unwrap();
        let failure = plan.failure_witness();
        assert!(plan
            .hydrate_with_payload_limit(16 * 1024, || false)
            .is_err());
        assert!(failure.retry_without_index());
        let fallback = screen.capture_line_read(40..41).unwrap();
        assert!(!fallback.failure_witness().retry_without_index());
        let fallback = fallback
            .hydrate_with_payload_limit(16 * 1024, || false)
            .unwrap();
        assert_eq!(fallback.first_row(), 40);
        assert_eq!(fallback.lines().next().unwrap().as_str(), "small");
        assert!(screen.validates_line_read(&fallback));
        assert!(
            fallback.layout.is_none(),
            "fallback does not claim a complete geometry index"
        );
        // A slower worker captured before a count-changing extension cannot
        // replace the newer map, even though its immutable source rows remain.
        let mut head = Line::from_text("abcd", &CellAttributes::blank(), 9, None);
        head.set_last_cell_was_wrapped(true, 9);
        {
            let mut state = sink.state.lock().unwrap();
            state.0 = ScrollbackIntervalIdentity::default();
            state.1 = vec![
                (0, head),
                (
                    1,
                    Line::from_text("efgh", &CellAttributes::blank(), 9, None),
                ),
            ]
            .into_iter()
            .collect();
        }
        screen.physical_cols = 3;
        screen.stable_row_index_offset = 2;
        assert!(screen.refresh_cold_source_observation().unwrap().is_some());
        let initial = screen
            .capture_line_read(0..2)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        screen.install_line_read_layout(&initial, 100);
        let older = screen.capture_line_read(0..1).unwrap();
        sink.state.lock().unwrap().1.insert(
            2,
            Line::from_text("ijklmn", &CellAttributes::blank(), 9, None),
        );
        screen.stable_row_index_offset = 3;
        let newer = screen
            .capture_line_read(2..3)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        assert!(screen.validates_line_read(&newer));
        screen.install_line_read_layout(&newer, 101);
        assert_eq!(screen.scrollback_top_stable_row(), -2);
        let older = older.hydrate(|| false).unwrap();
        assert!(
            !screen.validates_line_read(&older),
            "retained source is not authority to roll back a shifted layout"
        );
    }

    #[test]
    fn coordinate_witness_preserves_append_but_rejects_other_models() {
        let mut screen = test_screen(3, 8, 96);
        let witness = screen.capture_coordinate_witness();
        assert!(screen.matches_coordinate_witness(&witness.clone()));
        assert!(!screen.clone().matches_coordinate_witness(&witness));
        assert!(!test_screen(3, 8, 96).matches_coordinate_witness(&witness));
        // Includes hot retention eviction: stable coordinates of retained
        // rows survive, but this witness deliberately does not certify retention.
        for seq in 1..=64 {
            screen.scroll_up(&(0..3), 1, seq, CellAttributes::blank(), bidi_mode());
            assert!(screen.matches_coordinate_witness(&witness));
        }
        screen.scroll_down(&(0..3), 1, 65, CellAttributes::blank(), bidi_mode());
        assert!(!screen.matches_coordinate_witness(&witness));
    }

    #[test]
    fn coordinate_witness_rejects_resize_clear_and_config_aba() {
        let mut screen = test_screen(3, 8, 96);
        let witness = screen.capture_coordinate_witness();
        let cursor = test_cursor(0, 0, 1);
        let cursor = screen.resize(test_size(3, 8, 96), cursor, 1, false);
        assert!(screen.matches_coordinate_witness(&witness));
        let cursor = screen.resize(test_size(3, 4, 96), cursor, 2, false);
        screen.resize(test_size(3, 8, 96), cursor, 3, false);
        assert!(!screen.matches_coordinate_witness(&witness));
        let witness = screen.capture_coordinate_witness();
        screen.erase_scrollback().unwrap();
        assert!(!screen.matches_coordinate_witness(&witness));
        let witness = screen.capture_coordinate_witness();
        let config = Arc::clone(&screen.config);
        screen.set_config(&config);
        assert!(!screen.matches_coordinate_witness(&witness));
    }

    #[test]
    fn coordinate_witness_never_calls_sink_accessors() {
        #[derive(Debug)]
        struct NoSinkAccess;
        impl TerminalConfiguration for NoSinkAccess {
            fn color_palette(&self) -> ColorPalette {
                ColorPalette::default()
            }
            fn scrollback_spill_sink(&self) -> Option<Arc<dyn ScrollbackSpillSink>> {
                panic!("coordinate capture/validation must not access the sink");
            }
        }
        let mut screen = test_screen(3, 8, 96);
        // Install directly to isolate the two methods under test from setup.
        screen.config = Arc::new(NoSinkAccess);
        let witness = screen.capture_coordinate_witness();
        assert!(screen.matches_coordinate_witness(&witness));
        screen.physical_cols += 1;
        assert!(!screen.matches_coordinate_witness(&witness));
    }

    #[test]
    // The empty (`2..2`) and reversed (`2..1`) ranges are the deliberate inputs
    // under test — `stable_range` must clamp both to `0..0`. The literals trip
    // the deny-by-default `clippy::reversed_empty_ranges` correctness lint.
    #[allow(clippy::reversed_empty_ranges)]
    fn stable_range_rejects_empty_and_reversed_ranges() {
        let screen = test_screen(3, 8, 96);

        assert_eq!(screen.stable_range(&(2..2)), 0..0);
        assert_eq!(screen.stable_range(&(2..1)), 0..0);
    }

    #[test]
    fn stable_range_clamps_to_the_requested_row_count() {
        let mut screen = test_screen(5, 8, 96);
        screen.stable_row_index_offset = 10;

        assert_eq!(screen.stable_range(&(11..14)), 1..4);
        assert_eq!(screen.stable_range(&(9..12)), 0..3);
        assert_eq!(screen.stable_range(&(14..16)), 3..5);
        assert_eq!(screen.stable_range(&(16..18)), 3..5);
        assert_eq!(screen.stable_range(&(15..16)), 4..5);
        assert_eq!(screen.stable_range(&(9..16)), 0..5);
        assert_eq!(
            screen.stable_range(&(StableRowIndex::MIN..StableRowIndex::MAX)),
            0..5
        );
    }

    #[test]
    #[allow(clippy::reversed_empty_ranges)]
    fn dirty_rows_intersect_instead_of_clamping_to_nearby_resident_rows() {
        let mut screen = test_screen(5, 8, 96);
        screen.stable_row_index_offset = 10;
        for line in &mut screen.lines {
            line.update_last_change_seqno(7);
        }

        for range in [-2..0, 0..10, 15..16, 16..18, 12..12, 12..11] {
            assert!(
                screen.get_changed_stable_rows(range.clone(), 6).is_empty(),
                "{:?}",
                range
            );
        }
        for (range, expected) in [
            (9..12, vec![10, 11]),
            (14..16, vec![14]),
            (11..14, vec![11, 12, 13]),
            (
                StableRowIndex::MIN..StableRowIndex::MAX,
                vec![10, 11, 12, 13, 14],
            ),
        ] {
            assert_eq!(
                screen.get_changed_stable_rows(range.clone(), 6),
                expected,
                "{range:?}"
            );
            assert!(screen.get_changed_stable_rows(range, 7).is_empty());
        }
    }

    #[test]
    fn stable_range_excludes_unrepresentable_resident_rows() {
        let max_offset = usize::try_from(StableRowIndex::MAX).unwrap();
        for (offset, expected_rows) in [(max_offset - 1, 1), (max_offset, 0), (max_offset + 1, 0)] {
            let mut screen = test_screen(3, 8, 96);
            screen.stable_row_index_offset = offset;
            let requested = (StableRowIndex::MAX - 2)..StableRowIndex::MAX;
            let physical = screen.stable_range(&requested);
            let (first, lines) = screen.lines_in_stable_range(requested);

            assert_eq!(physical, 0..expected_rows, "offset {offset}");
            assert_eq!(lines.len(), expected_rows, "offset {offset}");
            if expected_rows != 0 {
                assert_eq!(first, screen.phys_to_stable_row_index(physical.start));
            }
        }
    }

    #[test]
    fn stable_row_conversions_reject_unrepresentable_offsets() {
        let mut screen = test_screen(3, 8, 96);
        screen.stable_row_index_offset = usize::try_from(StableRowIndex::MAX)
            .unwrap()
            .saturating_add(1);

        assert_eq!(screen.stable_row_to_phys(0), None);
        assert_eq!(screen.phys_to_stable_row_index(0), StableRowIndex::MAX);
    }

    #[test]
    fn stable_row_offset_helpers_saturate_at_numeric_limits() {
        let mut screen = test_screen(3, 8, 96);
        screen.stable_row_index_offset = usize::MAX;

        assert_eq!(
            screen.stable_row_index_for_removed_top(1),
            StableRowIndex::MAX
        );
        screen.advance_stable_row_index_offset(1);
        assert_eq!(screen.stable_row_index_offset, usize::MAX);
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn cold_wrap_rows_enforces_plan_limit_and_preserves_seam() {
        let line = Line::from_text("abcdefghijkl", &CellAttributes::blank(), 7, None);
        let policy = ResizeWrapPolicy::default();
        assert!(Screen::wrap_cold_logical_line(line.clone(), 4, 7, policy, false, 2).is_none());
        let rows = Screen::wrap_cold_logical_line(line, 4, 7, policy, true, 3)
            .expect("exact row budget is admitted");
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(Line::last_cell_was_wrapped));
        assert_eq!(
            rows.iter().map(Line::as_str).collect::<String>(),
            "abcdefghijkl"
        );
        assert!(Screen::wrap_cold_logical_line(Line::new(7), 4, 7, policy, false, 0).is_none());
        assert!(
            Screen::wrap_cold_logical_line(Line::new(7), 4, 7, policy, true, 0)
                .expect("empty aligned seam requires no output rows")
                .is_empty()
        );
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn cold_wrap_rows_counts_natural_breaks_before_output_admission() {
        let line = Line::from_text("one one one one", &CellAttributes::blank(), 7, None);
        assert_eq!(line.len().div_ceil(6), 3);
        let policy = ResizeWrapPolicy::default();
        assert!(Screen::wrap_cold_logical_line(line.clone(), 6, 7, policy, false, 3).is_none());
        let rows = Screen::wrap_cold_logical_line(line, 6, 7, policy, false, 4)
            .expect("actual four-row word layout fits its budget");
        assert_eq!(rows.len(), 4);
        assert_eq!(
            rows.iter().map(Line::as_str).collect::<String>(),
            "one one one one"
        );
        assert!(!rows.last().unwrap().last_cell_was_wrapped());
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn cold_wrap_aligned_seam_preserves_trailing_space() {
        let line = Line::from_text("abc def ", &CellAttributes::blank(), 7, None);
        let policy = ResizeWrapPolicy::default();
        let rows = Screen::wrap_cold_logical_line(line, 4, 7, policy, true, 4)
            .expect("aligned seam layout fits budget");
        let concatenated: String = rows.iter().map(Line::as_str).collect();
        assert_eq!(
            concatenated, "abc def ",
            "aligned cold seam wrap must preserve trailing separator space"
        );
    }

    #[cfg(feature = "use_serde")]
    fn stored_physical_fixture(
        cold_rows: usize,
        retention: usize,
    ) -> (Screen, Arc<TestColdScrollbackSink>) {
        let sink = Arc::new(TestColdScrollbackSink::default());
        let mut screen = test_screen_with_config(
            4,
            20,
            96,
            TestTermConfig {
                scrollback: retention + 1,
                scrollback_tier: crate::config::ScrollbackTierConfig {
                    enabled: true,
                    hot_lines: 1,
                    warm_max_bytes: 0,
                },
                cold_sink: Some(sink.clone()),
                ..TestTermConfig::default()
            },
        );
        for row in 0..cold_rows {
            let mut line = Line::from_text(
                ["café e\u{301} ", "中文🙂", " tail  "][row % 3],
                &CellAttributes::blank(),
                1,
                None,
            );
            line.set_last_cell_was_wrapped(row % 3 != 2, 1);
            assert!(screen.record_scrollback_spill(row as StableRowIndex, &line, 1));
            screen.advance_stable_row_index_offset(1);
        }
        screen.lines[0] = Line::from_text("resident tail", &CellAttributes::blank(), 1, None);
        (screen, sink)
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn stored_physical_first_large_prefix_read_decodes_only_requested_groups() {
        let (mut screen, sink) = stored_physical_fixture(20_001, 30_000);
        let requested = 19_969..19_997;
        let expected: Vec<_> = requested
            .clone()
            .map(|row| sink.load_scrollback_line(row).unwrap())
            .collect();
        let captured = screen.capture_line_read(requested.clone()).unwrap();
        assert!(captured.layout.as_ref().unwrap().stored_physical());
        assert_eq!(
            sink.batch_reads.load(Ordering::Relaxed),
            0,
            "capture must perform no cold IO"
        );
        let ready = captured.hydrate(|| false).unwrap();
        assert_eq!(ready.cold_context, Some(19_968..19_998));
        // This sink deliberately returns two rows per call. Reading the full
        // prefix would require 10,001 calls; only these ten groups are loaded.
        assert_eq!(sink.batch_reads.load(Ordering::Relaxed), 15);
        assert_eq!(ready.lines().cloned().collect::<Vec<_>>(), expected);
        assert!(screen.validates_line_read(&ready));
        screen.install_line_read_layout(&ready, 2);
        assert_eq!(screen.lines_in_stable_range(requested).1, expected);
        assert!(screen.validates_line_read(&ready));
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn cold_layout_comparison_does_not_turn_busy_storage_into_a_layout_change() {
        let (mut screen, sink) = stored_physical_fixture(3, 32);
        let read = screen
            .capture_line_read(0..3)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        assert!(screen.validates_line_read(&read));
        screen.install_line_read_layout(&read, 2);
        assert!(!screen.line_read_changes_layout(&read));
        let sequence = screen.cold_visual_layout_seqno();

        // Model contention starting after the caller's successful source
        // validation. The old comparison sees None and reports a new layout.
        sink.force_busy_probe.store(true, Ordering::Relaxed);
        assert!(screen.current_cold_visual_layout().is_none());
        assert!(!screen.line_read_changes_layout(&read));
        assert_eq!(screen.cold_visual_layout_seqno(), sequence);
        assert!(
            !screen.validates_line_read(&read),
            "stable geometry must not grant source authority while storage is busy"
        );

        sink.force_busy_probe.store(false, Ordering::Relaxed);
        assert!(screen.validates_line_read(&read));
        assert!(sink.store_scrollback_line(
            0,
            &Line::from_text("changed", &CellAttributes::blank(), 3, None),
            32,
        ));
        assert!(!screen.line_read_changes_layout(&read));
        assert!(
            !screen.validates_line_read(&read),
            "same-coordinate content replacement must still reject the old read"
        );

        // A real coordinate change remains a layout change even when the
        // sink cannot currently answer. It must not reuse the old mapping.
        screen.physical_cols = 11;
        screen.invalidate_coordinate_witnesses();
        sink.force_busy_probe.store(true, Ordering::Relaxed);
        assert!(screen.line_read_changes_layout(&read));
        assert!(!screen.validates_line_read(&read));
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn cold_geometry_count_reuse_preserves_trim_width_and_policy_distinctions() {
        let mut styled = CellAttributes::blank();
        styled.set_italic(true);
        let mut fallback = MonospaceKpCostModel::terminal_default();
        fallback.max_dp_states = 0;
        for model in [MonospaceKpCostModel::terminal_default(), fallback] {
            for cols in [0, 1, 3, 5, 12] {
                let mut counts = ColdGeometryRowCounts::new(cols, model);
                let mut scratch = LineWrapWidthPrefixScratch::default();
                let first = LineWrapGeometry::capture(
                    &Line::from_text("abcdefgh    ", &CellAttributes::blank(), 1, None),
                    usize::MAX,
                )
                .unwrap();
                let same = LineWrapGeometry::capture(
                    &Line::from_text("different   ", &styled, 2, None),
                    usize::MAX,
                )
                .unwrap();
                // Equal column counts alone do not authorize reuse: the last
                // non-space token is part of the exact key.
                assert_ne!(first, same);
                let mut prior = None;
                for text in [
                    "abcdefgh    ",
                    "12345678    ",
                    "different   ",
                    "界界界界    ",
                    "界界界界    ",
                    "e\u{301}",
                    "a",
                    "",
                    " ",
                    "abcdefghzzzz",
                ] {
                    let line = Line::from_text(text, &styled, 3, None);
                    let geometry = LineWrapGeometry::capture(&line, usize::MAX).unwrap();
                    let expected =
                        geometry.row_count(cols, model, &mut LineWrapWidthPrefixScratch::default());
                    let reuse_expected = prior.as_ref() == Some(&geometry);
                    prior = Some(geometry.clone());
                    let before = (counts.plans, counts.reuses);
                    assert_eq!(
                        counts.count(geometry, &mut scratch, usize::MAX),
                        Some(expected)
                    );
                    assert_eq!(counts.reuses - before.1, usize::from(reuse_expected));
                    assert_eq!(counts.plans - before.0, usize::from(!reuse_expected));
                }
            }
        }
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn cold_geometry_count_reuse_evicts_before_pressure_and_keeps_planner_admission() {
        let geometry = LineWrapGeometry::capture(
            &Line::from_text("abcdefghijkl  ", &CellAttributes::blank(), 1, None),
            usize::MAX,
        )
        .unwrap();
        let cols = 3;
        let model = MonospaceKpCostModel::terminal_default();
        let mut counts = ColdGeometryRowCounts::new(cols, model);
        let mut scratch = LineWrapWidthPrefixScratch::default();
        let expected = counts
            .count(geometry.clone(), &mut scratch, usize::MAX)
            .unwrap();
        let retained = geometry.retained_bytes();
        assert_eq!(counts.retained_bytes(), retained);
        assert_eq!(counts.reserve(2 * retained, Some(retained)), retained);
        assert_eq!(counts.retained_bytes(), retained);
        let baseline = retained
            + geometry
                .planning_bytes_upper_bound(cols, model, &scratch)
                .unwrap();
        assert_eq!(
            counts.count(geometry.clone(), &mut scratch, baseline - 1),
            None
        );
        assert_eq!(counts.reuses, 0, "a hit cannot bypass baseline admission");
        assert_eq!(
            counts.count(geometry.clone(), &mut scratch, baseline),
            Some(expected)
        );
        assert_eq!(counts.reuses, 1);

        // The next geometry fits without the optional entry. Drop the old
        // allocation first, then admit the same operation at its exact limit.
        assert_eq!(counts.reserve(retained, Some(retained)), retained);
        assert_eq!(counts.retained_bytes(), 0);
        assert_eq!(
            counts.count(geometry.clone(), &mut scratch, baseline),
            Some(expected)
        );
        assert_eq!(counts.plans, 2);
        assert_eq!(counts.reserve(usize::MAX, None), usize::MAX);
        assert_eq!(
            counts.retained_bytes(),
            0,
            "overflow evicts before baseline validation"
        );
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn admitted_geometry_native_parser_corpus_avoids_history_reads() {
        const PARAGRAPHS: usize = 10_000;
        let sink = Arc::new(TestColdScrollbackSink::default());
        let mut terminal = crate::Terminal::new(
            test_size(4, 173, 96),
            Arc::new(TestTermConfig {
                scrollback: 100_000,
                scrollback_tier: crate::config::ScrollbackTierConfig {
                    enabled: true,
                    hot_lines: 1,
                    warm_max_bytes: 0,
                },
                cold_sink: Some(sink.clone()),
                ..TestTermConfig::default()
            }),
            "FrankenTerm",
            "native-geometry-test",
            Box::new(std::io::sink()),
        );
        let paragraph = "Text reflow: ASCII ligatures ffi =>, 界面, e\u{301}, 🚀. ".repeat(9);
        for row in 0..PARAGRAPHS {
            terminal.advance_bytes(format!("{row:05} {paragraph}\r\n"));
        }
        // Move the final complete paragraph past both the visible screen and
        // the one-row hot tier. No synthetic Line packing or direct spill
        // calls may stand in for parser-created admission in this test.
        terminal.advance_bytes(b"\r\n\r\n\r\n\r\n\r\n");
        {
            let rows = sink.rows.lock().unwrap();
            assert!(rows.len() >= PARAGRAPHS * 3);
            assert_eq!(
                (0..3).map(|row| rows[&row].len()).collect::<Vec<_>>(),
                [173, 174, 109],
                "the parser writes the wide glyph before taking its pending wrap"
            );
            assert_eq!(rows[&1].visible_cells().last().unwrap().str(), "面");
            assert_eq!(rows[&2].visible_cells().next().unwrap().str(), ",");
            assert!(rows[&0].last_cell_was_wrapped());
            assert!(rows[&1].last_cell_was_wrapped());
            assert!(!rows[&2].last_cell_was_wrapped());
            let mut first = rows[&0].clone();
            first.append_line(rows[&1].clone(), 1);
            first.append_line(rows[&2].clone(), 1);
            assert_eq!(first.as_str(), format!("00000 {paragraph}"));
        }

        terminal.resize(test_size(4, 123, 96));
        let screen = terminal.screen();
        assert!(screen.stored_physical_layout.is_none());
        let plan = screen.capture_line_read(0..1).unwrap();
        assert!(plan.layout.is_none());
        assert!(plan.geometry.as_ref().unwrap().rows.len() >= PARAGRAPHS * 3);
        sink.batch_reads.store(0, Ordering::Relaxed);
        COLD_GEOMETRY_COUNT_REUSES.with(|n| n.set(0));
        let (geometry, _) = plan
            .geometry_cold_layout(ScreenLineRead::MAX_PAYLOAD_BYTES, &|| false)
            .unwrap()
            .expect("the actual 30k physical rows must fit the geometry budget");
        assert_eq!(sink.batch_reads.load(Ordering::Relaxed), 0);
        assert!(
            COLD_GEOMETRY_COUNT_REUSES.with(|n| n.get()) >= PARAGRAPHS - 1,
            "distinct numbered text must reuse its identical packed geometry"
        );
        let (reference, _) = plan
            .streamed_cold_layout(ScreenLineRead::MAX_PAYLOAD_BYTES, &|| false)
            .unwrap();
        assert!(sink.batch_reads.load(Ordering::Relaxed) >= (PARAGRAPHS * 3 / 2) as u64);
        assert_eq!(geometry.source, reference.source);
        assert_eq!(geometry.visual, reference.visual);
        assert_eq!(geometry.groups, reference.groups);

        sink.batch_reads.store(0, Ordering::Relaxed);
        let fast = plan.hydrate(|| false).unwrap();
        assert!(sink.batch_reads.load(Ordering::Relaxed) < 32);
        assert!(screen.validates_line_read(&fast));
        let mut reference_read = screen.capture_line_read(0..1).unwrap();
        reference_read.geometry = None;
        reference_read.layout = Some(reference);
        let reference_read = reference_read.hydrate(|| false).unwrap();
        assert_eq!(fast.first_row(), reference_read.first_row());
        assert_eq!(
            fast.lines().cloned().collect::<Vec<_>>(),
            reference_read.lines().cloned().collect::<Vec<_>>()
        );

        sink.batch_reads.store(0, Ordering::Relaxed);
        sink.single_reads.store(0, Ordering::Relaxed);
        let before_rows = terminal.screen().lines.clone();
        let before_cursor = terminal.cursor_pos();
        let mut prepared = terminal
            .screen()
            .capture_line_read(0..1)
            .unwrap()
            .prepare_cold_layout(|| false)
            .unwrap();
        assert!(!prepared.source.complete, "metadata is not a text receipt");
        assert!(!terminal.screen().validates_line_read(&prepared.source));
        assert!(terminal
            .screen()
            .validates_prepared_cold_layout(&prepared)
            .unwrap());
        let actual_layout = prepared.source.layout.as_ref().unwrap();
        let reference_layout = reference_read.layout.as_ref().unwrap();
        assert_eq!(actual_layout.source, reference_layout.source);
        assert_eq!(actual_layout.visual, reference_layout.visual);
        assert_eq!(actual_layout.groups, reference_layout.groups);
        let seqno = terminal.current_seqno().checked_add(1).unwrap();
        assert!(terminal
            .screen_mut()
            .install_prepared_cold_layout(&mut prepared, seqno)
            .unwrap());
        assert_eq!(sink.batch_reads.load(Ordering::Relaxed), 0);
        assert_eq!(sink.single_reads.load(Ordering::Relaxed), 0);
        assert_eq!(terminal.screen().lines, before_rows);
        assert_eq!(terminal.cursor_pos(), before_cursor);
        assert!(!terminal
            .screen()
            .validates_prepared_cold_layout(&prepared)
            .unwrap());
        assert!(!terminal
            .screen_mut()
            .install_prepared_cold_layout(&mut prepared, seqno)
            .unwrap());
        let published = terminal
            .screen()
            .capture_line_read(reference_read.first_row()..reference_read.end)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        assert!(terminal.screen().validates_line_read(&published));
        assert_eq!(
            published.lines().cloned().collect::<Vec<_>>(),
            reference_read.lines().cloned().collect::<Vec<_>>()
        );
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn prepared_cold_layout_rejects_stale_source_without_publication() {
        for mutation in 0..7 {
            let (mut screen, sink) = stored_physical_fixture(9, 32);
            let cursor = screen.resize(test_size(4, 11, 96), test_cursor(0, 0, 1), 2, false);
            let mut prepared = screen
                .capture_line_read(0..1)
                .unwrap()
                .prepare_cold_layout(|| false)
                .unwrap();
            assert!(!prepared.source.complete);
            assert!(screen.validates_prepared_cold_layout(&prepared).unwrap());
            match mutation {
                0 => sink.force_busy_probe.store(true, Ordering::Relaxed),
                1 => *sink.interval_identity.lock().unwrap() = Default::default(),
                2 => {
                    sink.rows.lock().unwrap().pop_first();
                }
                3 => {
                    let line = Line::from_text("new source", &CellAttributes::blank(), 3, None);
                    assert!(screen.record_scrollback_spill(9, &line, 3));
                    screen.advance_stable_row_index_offset(1);
                }
                4 => {
                    screen.cold_row_fragments = Some(Arc::new(ColdRowFragments {
                        sink: sink.clone(),
                        interval: prepared.source.cold.as_ref().unwrap().1.clone(),
                        rows: Arc::new(BTreeMap::new()),
                        aligned_frontier: 9,
                        aligned_source_start: 0,
                        cols: 11,
                        dpi: 96,
                        policy: screen.resize_wrap_policy,
                    }));
                }
                5 => {
                    let cursor = screen.resize(test_size(4, 20, 96), cursor, 3, false);
                    screen.resize(test_size(4, 11, 96), cursor, 4, false);
                }
                6 => screen.set_config(&Arc::clone(&screen.config)),
                _ => unreachable!(),
            }
            let before_seqno = screen.cold_visual_seqno;
            let before_layout = screen.cold_visual_layout.clone();
            let expected = if mutation == 0 {
                Err(ColdReadMetadataBusy)
            } else {
                Ok(false)
            };
            assert_eq!(screen.validates_prepared_cold_layout(&prepared), expected);
            assert_eq!(
                screen.install_prepared_cold_layout(&mut prepared, 5),
                expected
            );
            assert!(!prepared.installed);
            assert_eq!(screen.cold_visual_seqno, before_seqno);
            assert!(match (&before_layout, &screen.cold_visual_layout) {
                (None, None) => true,
                (Some(before), Some(after)) => Arc::ptr_eq(before, after),
                _ => false,
            });
            if mutation == 0 {
                sink.force_busy_probe.store(false, Ordering::Relaxed);
                let batch_reads = sink.batch_reads.load(Ordering::Relaxed);
                let single_reads = sink.single_reads.load(Ordering::Relaxed);
                assert!(screen.validates_prepared_cold_layout(&prepared).unwrap());
                assert!(screen
                    .install_prepared_cold_layout(&mut prepared, 5)
                    .unwrap());
                assert_eq!(sink.batch_reads.load(Ordering::Relaxed), batch_reads);
                assert_eq!(sink.single_reads.load(Ordering::Relaxed), single_reads);
            }
        }
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn prepared_cold_layout_cancellation_and_payload_fallback_remain_exact() {
        let (mut screen, sink) = stored_physical_fixture(9, 32);
        screen.resize(test_size(4, 11, 96), test_cursor(0, 0, 1), 2, false);
        assert!(screen
            .capture_line_read(0..1)
            .unwrap()
            .prepare_cold_layout(|| true)
            .is_err());
        assert!(screen.cold_visual_layout.is_none());
        assert_eq!(sink.batch_reads.load(Ordering::Relaxed), 0);
        assert_eq!(sink.single_reads.load(Ordering::Relaxed), 0);
        let expected = screen
            .capture_line_read(0..1)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        let mut capture = screen.capture_line_read(0..1).unwrap();
        capture.geometry = None;
        sink.batch_reads.store(0, Ordering::Relaxed);
        let mut fallback = capture.prepare_cold_layout(|| false).unwrap();
        assert!(fallback.source.complete);
        assert!(sink.batch_reads.load(Ordering::Relaxed) > 0);
        assert!(screen.validates_line_read(&fallback.source));
        assert!(
            expected.first_row() > 0,
            "reflow must move the oldest visual row"
        );
        assert_eq!(expected.row_count(), 1);
        assert_eq!(fallback.source.row_count(), 1);
        assert_eq!(fallback.source.first_row(), expected.first_row());
        assert_eq!(fallback.source.end, expected.end);
        assert_eq!(
            fallback.source.lines().cloned().collect::<Vec<_>>(),
            expected.lines().cloned().collect::<Vec<_>>()
        );
        assert!(screen
            .install_prepared_cold_layout(&mut fallback, 3)
            .unwrap());
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn cold_small_prefix_payload_fallback_preserves_clamped_request_count() {
        for cols in [11, 40] {
            for requested in [0..1, 0..5, 0..10] {
                let (mut screen, sink) = stored_physical_fixture(9, 32);
                screen.resize(test_size(4, cols, 96), test_cursor(0, 0, 1), 2, false);
                let expected = screen
                    .capture_line_read(requested.clone())
                    .unwrap()
                    .hydrate(|| false)
                    .unwrap();
                let mut capture = screen.capture_line_read(requested.clone()).unwrap();
                assert!(capture.geometry.is_some());
                assert!(capture.layout.is_none());
                let resident = capture.resident.clone();
                let endpoint = capture.end;
                capture.geometry = None;
                sink.batch_reads.store(0, Ordering::Relaxed);
                let fallback = capture.hydrate(|| false).unwrap();
                assert!(sink.batch_reads.load(Ordering::Relaxed) > 0);
                assert!(screen.validates_line_read(&fallback));
                assert!(fallback.row_count() > 0);
                assert_eq!(fallback.first_row(), expected.first_row());
                assert_eq!(fallback.end, expected.end);
                assert_eq!(fallback.resident, resident);
                if !resident.is_empty() {
                    assert_eq!(fallback.end, endpoint, "resident endpoint must not move");
                } else {
                    assert_eq!(fallback.resident_first, fallback.end);
                    let available =
                        fallback.layout.as_ref().unwrap().visual.end - fallback.first_row();
                    assert_eq!(
                        fallback.row_count(),
                        (requested.end - requested.start).min(available) as usize
                    );
                }
                assert_eq!(
                    fallback.lines().cloned().collect::<Vec<_>>(),
                    expected.lines().cloned().collect::<Vec<_>>()
                );
                let fallback_layout = fallback.layout.as_ref().unwrap();
                let expected_layout = expected.layout.as_ref().unwrap();
                assert_eq!(fallback_layout.source, expected_layout.source);
                assert_eq!(fallback_layout.visual, expected_layout.visual);
                assert_eq!(fallback_layout.groups, expected_layout.groups);
            }
        }
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn prepared_cold_layout_retains_displaced_metadata_until_worker_drop() {
        let (mut screen, _) = stored_physical_fixture(9, 32);
        let cursor = screen.resize(test_size(4, 11, 96), test_cursor(0, 0, 1), 2, false);
        let mut first = screen
            .capture_line_read(0..1)
            .unwrap()
            .prepare_cold_layout(|| false)
            .unwrap();
        assert!(screen.install_prepared_cold_layout(&mut first, 3).unwrap());
        drop(first);
        let retired = Arc::downgrade(screen.cold_visual_layout.as_ref().unwrap());
        screen.resize(test_size(4, 7, 96), cursor, 4, false);
        let mut next = screen
            .capture_line_read(0..1)
            .unwrap()
            .prepare_cold_layout(|| false)
            .unwrap();
        assert!(screen.install_prepared_cold_layout(&mut next, 5).unwrap());
        assert!(Arc::ptr_eq(
            next.retired_layout.as_ref().unwrap(),
            &retired.upgrade().unwrap()
        ));
        assert!(!screen.install_prepared_cold_layout(&mut next, 6).unwrap());
        assert!(
            retired.upgrade().is_some(),
            "receipt retains replaced allocation"
        );
        drop(next);
        assert!(
            retired.upgrade().is_none(),
            "worker retirement releases allocation"
        );
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn admitted_geometry_changed_width_matches_stream_without_prefix_reads() {
        let (mut screen, sink) = stored_physical_fixture(20_001, 30_000);
        let cursor = test_cursor(0, 0, 1);
        screen.resize(test_size(4, 11, 96), cursor, 2, false);
        assert!(screen.stored_physical_layout.is_none());
        assert!(screen.cold_geometry_index.is_some());
        sink.batch_reads.store(0, Ordering::Relaxed);
        let plan = screen.capture_line_read(0..1).unwrap();
        assert!(plan.layout.is_none());
        assert!(plan.geometry.is_some());
        let (geometry, _) = plan
            .geometry_cold_layout(ScreenLineRead::MAX_PAYLOAD_BYTES, &|| false)
            .unwrap()
            .expect("the Unicode fixture has certified joins");
        assert_eq!(sink.batch_reads.load(Ordering::Relaxed), 0);
        let (reference, _) = plan
            .streamed_cold_layout(ScreenLineRead::MAX_PAYLOAD_BYTES, &|| false)
            .unwrap();
        assert_eq!(geometry.source, reference.source);
        assert_eq!(geometry.visual, reference.visual);
        assert_eq!(geometry.groups, reference.groups);
        assert!(sink.batch_reads.load(Ordering::Relaxed) >= 10_001);

        sink.batch_reads.store(0, Ordering::Relaxed);
        let fast = plan.hydrate(|| false).unwrap();
        let fast_reads = sink.batch_reads.load(Ordering::Relaxed);
        assert!(
            fast_reads < 32,
            "only requested groups should decode: {}",
            fast_reads
        );
        assert!(screen.validates_line_read(&fast));
        let mut payload_only = screen.capture_line_read(0..1).unwrap();
        payload_only.geometry = None;
        let reference = payload_only.hydrate(|| false).unwrap();
        assert_eq!(fast.first_row(), reference.first_row());
        assert_eq!(
            fast.lines().cloned().collect::<Vec<_>>(),
            reference.lines().cloned().collect::<Vec<_>>()
        );
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn admitted_geometry_clips_retention_and_replans_after_geometry_aba() {
        let (mut screen, sink) = stored_physical_fixture(9, 5);
        let cursor = test_cursor(0, 0, 1);
        let cursor = screen.resize(test_size(4, 11, 96), cursor, 2, false);
        screen.resize(test_size(4, 20, 96), cursor, 3, false);
        let captured = screen.capture_line_read(4..5).unwrap();
        assert_eq!(captured.geometry.as_ref().unwrap().source, 4..9);
        sink.batch_reads.store(0, Ordering::Relaxed);
        let (layout, _) = captured
            .geometry_cold_layout(ScreenLineRead::MAX_PAYLOAD_BYTES, &|| false)
            .unwrap()
            .unwrap();
        assert_eq!(sink.batch_reads.load(Ordering::Relaxed), 0);
        let (reference, _) = captured
            .streamed_cold_layout(ScreenLineRead::MAX_PAYLOAD_BYTES, &|| false)
            .unwrap();
        assert_eq!(layout.groups, reference.groups);
        assert_eq!(layout.source.start, 4);
        assert_eq!(layout.groups.first().unwrap().0.start, 4);
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn admitted_geometry_rejects_uncertain_source_and_bounds_capture() {
        let (mut screen, sink) = stored_physical_fixture(6, 32);
        screen.invalidate_coordinate_witnesses();
        let mut budget = LineReadCaptureBudget {
            bytes_left: 1,
            work_left: 65_536,
        };
        let crate::config::ScrollbackIntervalCapture::Ready(interval) =
            sink.try_capture_scrollback_interval()
        else {
            panic!("fixture interval ready");
        };
        let trait_sink: Arc<dyn ScrollbackSpillSink> = sink.clone();
        assert!(screen
            .capture_cold_geometry(&trait_sink, &interval, 6, &mut budget)
            .is_none());
        assert_eq!(budget.bytes_left, 1);
        assert_eq!(budget.work_left, 65_536);
        let mut budget = LineReadCaptureBudget {
            bytes_left: ScreenLineRead::MAX_PAYLOAD_BYTES,
            work_left: 5,
        };
        assert!(screen
            .capture_cold_geometry(&trait_sink, &interval, 6, &mut budget)
            .is_none());
        let other: Arc<dyn ScrollbackSpillSink> = Arc::new(TestColdScrollbackSink::default());
        assert!(screen
            .capture_cold_geometry(&other, &interval, 6, &mut LineReadCaptureBudget::default())
            .is_none());

        let captured = screen.capture_line_read(0..1).unwrap();
        assert!(captured.geometry.is_some());
        let checks = std::cell::Cell::new(0);
        assert!(captured
            .geometry_cold_layout(ScreenLineRead::MAX_PAYLOAD_BYTES, &|| {
                checks.set(checks.get() + 1);
                checks.get() >= 3
            })
            .unwrap_err()
            .to_string()
            .contains("cancelled"));
        assert!(!captured.index_budget_exhausted.load(Ordering::Acquire));
        assert!(captured
            .geometry_cold_layout(1, &|| false)
            .unwrap()
            .is_none());
        assert_eq!(sink.batch_reads.load(Ordering::Relaxed), 0);

        let before = screen
            .capture_line_read(0..1)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        assert!(screen.validates_line_read(&before));
        sink.omit_admission_receipt.store(true, Ordering::Relaxed);
        let line = Line::from_text("unwitnessed", &CellAttributes::blank(), 3, None);
        assert!(screen.record_scrollback_spill(6, &line, 3));
        screen.advance_stable_row_index_offset(1);
        assert!(screen.cold_geometry_index.is_none());
        assert!(!screen.validates_line_read(&before));
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn canonical_geometry_frontier_advance_rejects_old_visual_origin() {
        for cold_rows in [6, 8] {
            let (mut screen, _) = stored_physical_fixture(cold_rows, 32);
            screen.physical_cols = 200;
            screen.invalidate_coordinate_witnesses();
            let old = screen
                .capture_line_read(4..5)
                .unwrap()
                .hydrate(|| false)
                .unwrap();
            assert_eq!(old.layout.as_ref().unwrap().source, 0..6);
            assert_eq!(old.layout.as_ref().unwrap().visual, 4..6);
            assert!(screen.validates_line_read(&old));
            screen.install_line_read_layout(&old, 2);
            if cold_rows == 6 {
                let mut head = Line::from_text("new", &CellAttributes::blank(), 3, None);
                head.set_last_cell_was_wrapped(true, 3);
                assert!(screen.record_scrollback_spill(6, &head, 3));
                screen.advance_stable_row_index_offset(1);
                let tail = Line::from_text("tail", &CellAttributes::blank(), 3, None);
                assert!(screen.record_scrollback_spill(7, &tail, 3));
                screen.advance_stable_row_index_offset(1);
            } else {
                let tail = screen.lines[0].clone();
                assert!(screen.record_scrollback_spill(8, &tail, 3));
                screen.advance_stable_row_index_offset(1);
            }
            assert!(!screen.validates_line_read(&old));
            assert!(screen.current_cold_visual_layout().is_none());
            let first = if cold_rows == 6 { 5 } else { 6 };
            let current = screen
                .capture_line_read(first..first + 1)
                .unwrap()
                .hydrate(|| false)
                .unwrap();
            assert_eq!(current.layout.as_ref().unwrap().visual.start, first);
            assert!(screen.validates_line_read(&current));
            screen.install_line_read_layout(&current, 4);
            assert!(!screen.validates_line_read(&old));
        }
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn canonical_geometry_positive_origin_preserves_owned_requested_rows() {
        let (mut screen, _) = stored_physical_fixture(6, 32);
        screen.physical_cols = 200;
        screen.invalidate_coordinate_witnesses();
        for (request, expected) in [(0..1, 4..5), (0..3, 4..6), (0..7, 4..7)] {
            let ready = screen
                .capture_line_read(request)
                .unwrap()
                .hydrate(|| false)
                .unwrap();
            assert_eq!(ready.first_row(), expected.start);
            assert_eq!(ready.row_count(), (expected.end - expected.start) as usize);
            assert_eq!(ready.end, expected.end);
            assert!(
                ready.geometry.is_none(),
                "worker retires source snapshot after indexing"
            );
            assert!(screen.validates_line_read(&ready));
        }
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn admitted_geometry_replaces_only_fragment_rows_and_preserves_open_seam() {
        let (mut screen, sink) = stored_physical_fixture(8, 32);
        screen.physical_cols = 11;
        screen.invalidate_coordinate_witnesses();
        let crate::config::ScrollbackIntervalCapture::Ready(interval) =
            sink.try_capture_scrollback_interval()
        else {
            panic!("fixture interval ready");
        };
        let replacement = Line::from_text("new fragment", &CellAttributes::blank(), 3, None);
        screen.cold_row_fragments = Some(Arc::new(ColdRowFragments {
            sink: sink.clone(),
            interval,
            rows: Arc::new([(7, replacement)].into()),
            aligned_frontier: 8,
            aligned_source_start: 0,
            cols: 11,
            dpi: 96,
            policy: screen.resize_wrap_policy,
        }));
        let captured = screen.capture_line_read(4..5).unwrap();
        let (layout, _) = captured
            .geometry_cold_layout(ScreenLineRead::MAX_PAYLOAD_BYTES, &|| false)
            .unwrap()
            .unwrap();
        assert_eq!(sink.batch_reads.load(Ordering::Relaxed), 0);
        let (reference, _) = captured
            .streamed_cold_layout(ScreenLineRead::MAX_PAYLOAD_BYTES, &|| false)
            .unwrap();
        assert_eq!(layout.source, reference.source);
        assert_eq!(layout.visual, reference.visual);
        assert_eq!(layout.groups, reference.groups);
        let ready = captured.hydrate(|| false).unwrap();
        assert!(screen.validates_line_read(&ready));
        screen.cold_row_fragments = None;
        assert!(!screen.validates_line_read(&ready));
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn stored_physical_admission_refusal_and_unwitnessed_success_do_not_extend_index() {
        let (mut screen, sink) = stored_physical_fixture(6, 32);
        let offered = Line::from_text("not admitted", &CellAttributes::blank(), 1, None);
        sink.refuse_admission.store(true, Ordering::Relaxed);
        assert!(!screen.record_scrollback_spill(6, &offered, 1));
        assert_eq!(screen.stored_physical_layout.as_ref().unwrap().source, 0..6);
        assert!(sink.load_scrollback_line(6).is_none());
        sink.refuse_admission.store(false, Ordering::Relaxed);
        sink.omit_admission_receipt.store(true, Ordering::Relaxed);
        assert!(screen.record_scrollback_spill(6, &offered, 1));
        screen.advance_stable_row_index_offset(1);
        assert!(screen.stored_physical_layout.is_none());
        assert!(screen.capture_line_read(0..3).unwrap().layout.is_none());
        assert_eq!(sink.load_scrollback_line(6), Some(offered));
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn stored_physical_busy_replacement_and_clear_reject_old_authority() {
        let (mut screen, sink) = stored_physical_fixture(6, 32);
        let ready = screen
            .capture_line_read(0..3)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        sink.force_busy_probe.store(true, Ordering::Relaxed);
        assert!(!screen.validates_line_read(&ready));
        assert!(screen.capture_line_read(0..3).is_err());
        sink.force_busy_probe.store(false, Ordering::Relaxed);
        assert!(screen.validates_line_read(&ready));
        *sink.interval_identity.lock().unwrap() = Default::default();
        assert!(
            !screen.validates_line_read(&ready),
            "identical bounds are not source authority"
        );
        assert!(screen.capture_line_read(0..3).unwrap().layout.is_none());
        sink.clear_scrollback().unwrap();
        let line = Line::from_text("after clear", &CellAttributes::blank(), 1, None);
        assert!(screen.record_scrollback_spill(6, &line, 1));
        screen.advance_stable_row_index_offset(1);
        let after = screen
            .capture_line_read(6..7)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        assert!(after.layout.as_ref().unwrap().stored_physical());
        assert_eq!(after.lines().cloned().collect::<Vec<_>>(), vec![line]);
        assert!(!screen.validates_line_read(&ready));
        assert!(screen.validates_line_read(&after));

        // A different sink is not authorized even if a caller supplies equal
        // row values and forgets to install a new configuration generation.
        let replacement = Arc::new(TestColdScrollbackSink::default());
        for (row, line) in sink.rows.lock().unwrap().iter() {
            assert!(replacement.store_scrollback_line(*row, line, 32));
        }
        screen.config = Arc::new(TestTermConfig {
            cold_sink: Some(replacement),
            ..TestTermConfig::default()
        });
        assert!(!screen.validates_line_read(&after));
        assert!(screen.capture_line_read(6..7).unwrap().layout.is_none());
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn cold_hydration_refuses_changed_source_before_loading_rows() {
        let (screen, sink) = stored_physical_fixture(6, 32);
        let stale = screen.capture_line_read(0..3).unwrap();
        let busy = screen.capture_line_read(0..3).unwrap();
        sink.force_busy_probe.store(true, Ordering::Relaxed);
        let error = busy.hydrate(|| false).err().expect("busy source refused");
        assert!(error.downcast_ref::<ColdReadMetadataBusy>().is_some());
        assert_eq!(sink.batch_reads.load(Ordering::Relaxed), 0);
        sink.force_busy_probe.store(false, Ordering::Relaxed);

        // A real replacement preserves the row bounds but changes lineage.
        let replacement = Line::from_text("replacement", &CellAttributes::blank(), 2, None);
        assert!(sink.store_scrollback_line(0, &replacement, 32));
        let error = stale.hydrate(|| false).err().expect("stale source refused");
        assert!(error.downcast_ref::<ColdReadSourceChanged>().is_some());
        assert_eq!(sink.batch_reads.load(Ordering::Relaxed), 0);

        let (screen, sink) = stored_physical_fixture(6, 32);
        let stale = screen.capture_line_read(0..3).unwrap();
        assert!(sink.store_scrollback_line(6, &replacement, 3));
        let error = stale
            .hydrate(|| false)
            .err()
            .expect("pruned source refused");
        assert!(error.downcast_ref::<ColdReadSourceChanged>().is_some());
        assert_eq!(sink.batch_reads.load(Ordering::Relaxed), 0);

        let (screen, sink) = stored_physical_fixture(6, 32);
        let retained = screen.capture_line_read(0..3).unwrap();
        let expected: Vec<_> = sink
            .rows
            .lock()
            .unwrap()
            .range(0..3)
            .map(|(_, line)| line.clone())
            .collect();
        assert!(sink.store_scrollback_line(6, &replacement, 32));
        let ready = retained.hydrate(|| false).expect("append retains old rows");
        assert_eq!(ready.lines().cloned().collect::<Vec<_>>(), expected);

        // A hole inside unchanged bounds is a missing-row failure, not proof
        // of changed lineage. Do not turn damaged storage into a retry hint.
        let (screen, sink) = stored_physical_fixture(6, 32);
        let damaged = screen.capture_line_read(1..2).unwrap();
        sink.rows.lock().unwrap().remove(&1);
        let error = damaged
            .hydrate(|| false)
            .err()
            .expect("missing row refused");
        assert!(error.downcast_ref::<ColdReadSourceChanged>().is_none());
        assert!(sink.batch_reads.load(Ordering::Relaxed) > 0);
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn stored_physical_retention_clips_group_without_rewrapping_unicode_or_spaces() {
        let (mut screen, sink) = stored_physical_fixture(9, 5);
        assert_eq!(screen.stored_physical_layout.as_ref().unwrap().source, 4..9);
        let ready = screen
            .capture_line_read(4..6)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        assert_eq!(ready.cold_context, Some(4..6));
        let expected: Vec<_> = (4..6)
            .map(|row| sink.load_scrollback_line(row).unwrap())
            .collect();
        assert_eq!(ready.lines().cloned().collect::<Vec<_>>(), expected);
        assert!(ready.lines().next().unwrap().last_cell_was_wrapped());
        assert_eq!(ready.lines().last().unwrap().as_str(), " tail  ");
        screen.install_line_read_layout(&ready, 2);
        sink.rows.lock().unwrap().remove(&4);
        assert!(!screen.validates_line_read(&ready));
        let clipped = screen
            .capture_line_read(5..6)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        assert!(screen.validates_line_read(&clipped));
        assert_eq!(clipped.lines().cloned().collect::<Vec<_>>(), expected[1..]);
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn stored_physical_open_seam_preserves_raw_rows_and_resident_logical_context() {
        let (mut screen, sink) = stored_physical_fixture(8, 32);
        let expected: Vec<_> = (6..8)
            .map(|row| sink.load_scrollback_line(row).unwrap())
            .chain(std::iter::once(screen.lines[0].clone()))
            .collect();
        let ready = screen
            .capture_line_read(6..9)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        assert_eq!(ready.lines().cloned().collect::<Vec<_>>(), expected);
        assert_eq!(
            ready.layout.as_ref().unwrap().kind,
            ColdVisualLayoutKind::StoredPhysical { open_tail: true }
        );
        assert!(screen.validates_line_read(&ready));
        screen.install_line_read_layout(&ready, 2);
        assert_eq!(screen.expand_cold_logical_range(7..8), 6..9);
        assert_eq!(screen.lines_in_stable_range(6..9).1, expected);
        screen.lines[0] = Line::from_text("changed tail", &CellAttributes::blank(), 1, None);
        assert!(!screen.validates_line_read(&ready));
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn stored_physical_spilled_open_tail_refreshes_logical_context() {
        let (mut screen, sink) = stored_physical_fixture(8, 32);
        let closed_prefix = screen
            .capture_line_read(0..3)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        let truncated = screen
            .capture_line_read(6..8)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        screen.install_line_read_layout(&truncated, 2);
        assert_eq!(screen.expand_cold_logical_range(7..8), 6..9);
        let tail = screen.lines.pop_front().unwrap();
        assert!(!tail.last_cell_was_wrapped());
        assert!(screen.record_scrollback_spill(8, &tail, 3));
        screen.advance_stable_row_index_offset(1);
        screen.lines.push_back(Line::new(3));
        assert!(screen.current_cold_visual_layout().is_none());
        assert_eq!(screen.expand_cold_logical_range(7..8), 6..9);
        assert!(!screen.validates_line_read(&truncated));
        assert!(screen.validates_line_read(&closed_prefix));

        let expected: Vec<_> = (6..9)
            .map(|row| sink.load_scrollback_line(row).unwrap())
            .collect();
        let replacement = screen
            .capture_line_read(screen.expand_cold_logical_range(7..8))
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        assert_eq!(replacement.cold_context, Some(6..9));
        assert_eq!(replacement.cached_lines(6..9).unwrap().1, expected);
        assert!(screen.validates_line_read(&replacement));
        screen.install_line_read_layout(&replacement, 3);
        assert!(screen.validates_line_read(&closed_prefix));
        assert!(!screen.validates_line_read(&truncated));
        assert_eq!(screen.expand_cold_logical_range(7..8), 6..9);
        let raw = screen
            .capture_line_read(7..8)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        assert_eq!(raw.lines().cloned().collect::<Vec<_>>(), expected[1..2]);
        assert!(screen.validates_line_read(&raw));
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn stored_physical_spilled_continuation_still_includes_the_resident_tail() {
        let (mut screen, _) = stored_physical_fixture(8, 32);
        screen.lines[0].set_last_cell_was_wrapped(true, 1);
        screen.lines[1] = Line::from_text("logical end", &CellAttributes::blank(), 1, None);
        let truncated = screen
            .capture_line_read(6..8)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        screen.install_line_read_layout(&truncated, 2);
        assert_eq!(screen.expand_cold_logical_range(7..8), 6..10);
        let continuation = screen.lines.pop_front().unwrap();
        assert!(screen.record_scrollback_spill(8, &continuation, 3));
        screen.advance_stable_row_index_offset(1);
        screen.lines.push_back(Line::new(3));
        assert_eq!(screen.expand_cold_logical_range(7..8), 6..10);
        assert!(!screen.validates_line_read(&truncated));
        let replacement = screen
            .capture_line_read(6..10)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        assert!(screen.validates_line_read(&replacement));
        assert_eq!(replacement.row_count(), 4);
        assert_eq!(replacement.lines().last().unwrap().as_str(), "logical end");
        screen.lines[0] = Line::from_text("changed end", &CellAttributes::blank(), 1, None);
        assert!(!screen.validates_line_read(&replacement));
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn stored_physical_closed_group_stays_valid_when_a_new_group_spills() {
        let (mut screen, _) = stored_physical_fixture(6, 32);
        let closed = screen
            .capture_line_read(3..6)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        screen.install_line_read_layout(&closed, 2);
        let mut new_group = screen.lines.pop_front().unwrap();
        new_group.set_last_cell_was_wrapped(true, 3);
        assert!(screen.record_scrollback_spill(6, &new_group, 3));
        screen.advance_stable_row_index_offset(1);
        screen.lines.push_back(Line::new(3));
        assert!(screen.validates_line_read(&closed));
        assert_eq!(screen.expand_cold_logical_range(4..5), 3..6);
        assert_eq!(screen.expand_cold_logical_range(6..7), 6..8);
        assert!(screen
            .capture_line_read(6..8)
            .unwrap()
            .hydrate_with_payload_limit(1, || false)
            .is_err());
        assert!(screen
            .capture_line_read(6..8)
            .unwrap()
            .hydrate(|| true)
            .is_err());
        let mut end = screen.lines.pop_front().unwrap();
        end.set_last_cell_was_wrapped(false, 3);
        assert!(screen.record_scrollback_spill(7, &end, 3));
        screen.advance_stable_row_index_offset(1);
        screen.lines.push_back(Line::new(3));
        let extended = screen
            .capture_line_read(3..8)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        assert!(screen.validates_line_read(&extended));
        screen.install_line_read_layout(&extended, 3);
        assert!(screen.validates_line_read(&closed));
        assert!(!screen.line_read_changes_layout(&closed));
        screen.install_line_read_layout(&closed, 3);
        assert_eq!(screen.expand_cold_logical_range(6..8), 6..8);
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn stored_physical_unwitnessed_tail_cannot_reuse_an_old_open_layout() {
        let (mut screen, sink) = stored_physical_fixture(8, 32);
        let old = screen
            .capture_line_read(6..8)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        screen.install_line_read_layout(&old, 2);
        sink.omit_admission_receipt.store(true, Ordering::Relaxed);
        let tail = screen.lines.pop_front().unwrap();
        assert!(screen.record_scrollback_spill(8, &tail, 3));
        screen.advance_stable_row_index_offset(1);
        screen.lines.push_back(Line::new(3));
        assert!(screen.stored_physical_layout.is_none());
        assert!(screen.current_cold_visual_layout().is_none());
        assert!(!screen.validates_line_read(&old));
        assert!(screen.capture_line_read(7..8).unwrap().layout.is_none());
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn stored_physical_geometry_aba_and_configuration_invalidate_admission_metadata() {
        for size in [
            test_size(4, 21, 96),
            test_size(5, 20, 96),
            test_size(4, 20, 120),
        ] {
            let (mut screen, _) = stored_physical_fixture(6, 32);
            let read = screen
                .capture_line_read(0..3)
                .unwrap()
                .hydrate(|| false)
                .unwrap();
            let cursor = screen.resize(size, CursorPosition::default(), 2, false);
            assert!(screen.stored_physical_layout.is_none());
            screen.resize(test_size(4, 20, 96), cursor, 3, false);
            assert!(!screen.validates_line_read(&read));
            assert!(screen.capture_line_read(0..3).unwrap().layout.is_none());
        }
        let (mut screen, _) = stored_physical_fixture(6, 32);
        let read = screen
            .capture_line_read(0..3)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        let config = Arc::clone(&screen.config);
        screen.install_prepared_config(&config, screen.resize_wrap_policy);
        assert!(screen.stored_physical_layout.is_none());
        assert!(!screen.validates_line_read(&read));
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn stored_physical_loads_remain_charged_and_cancellable() {
        let (screen, sink) = stored_physical_fixture(60, 100);
        assert!(screen
            .capture_line_read(3..33)
            .unwrap()
            .hydrate_with_payload_limit(1, || false)
            .is_err());
        sink.batch_reads.store(0, Ordering::Relaxed);
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let result = screen
            .capture_line_read(3..33)
            .unwrap()
            .hydrate(|| calls.fetch_add(1, Ordering::Relaxed) >= 5);
        assert!(result.is_err());
        assert!(sink.batch_reads.load(Ordering::Relaxed) < 15);
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn stored_physical_payload_charges_final_context_once_at_exact_boundary() {
        let (mut screen, sink) = stored_physical_fixture(8, 32);
        // The same logical context may contain durable vector rows and a
        // resident compressed tail. Both must become fully charged worker
        // rows, without changing text, attributes, or wrapped boundaries.
        screen.lines[0].compress_for_scrollback();
        let mut expected: Vec<_> = (6..8)
            .map(|row| sink.load_scrollback_line(row).unwrap())
            .chain(std::iter::once(screen.lines[0].clone()))
            .collect();
        let captured = screen.capture_line_read(6..9).unwrap();
        let layout = captured.layout.as_ref().unwrap();
        let metadata_bytes = std::mem::size_of::<ColdVisualLayout>()
            + 2 * std::mem::size_of::<usize>()
            + layout.groups.capacity()
                * std::mem::size_of::<(Range<StableRowIndex>, Range<StableRowIndex>)>();
        let mut exact = metadata_bytes;
        for line in &mut expected {
            let _ = line.cells_mut_for_attr_changes_only();
            exact += serde_json::to_vec(line)
                .unwrap()
                .len()
                .max(line.len() * std::mem::size_of::<Cell>() + std::mem::size_of::<Line>());
        }
        let ready = captured
            .hydrate_with_payload_limit(exact, || false)
            .unwrap();
        assert_eq!(ready.payload_bytes(), exact);
        assert_eq!(ready.lines().cloned().collect::<Vec<_>>(), expected);
        assert!(screen.validates_line_read(&ready));
        let rejected = screen
            .capture_line_read(6..9)
            .unwrap()
            .hydrate_with_payload_limit(exact - 1, || false)
            .err()
            .expect("one byte below the complete context must fail");
        assert!(rejected.downcast_ref::<ColdReadPayloadLimit>().is_some());
        assert!(screen.validates_line_read(&ready));
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn stored_physical_payload_keeps_hyperlink_attributes_in_the_charge() {
        let (mut screen, _) = stored_physical_fixture(0, 32);
        let mut attrs = CellAttributes::blank();
        attrs.set_hyperlink(Some(Arc::new(termwiz::hyperlink::Hyperlink::new(format!(
            "https://example.invalid/{}",
            "a".repeat(8_192)
        )))));
        attrs.set_italic(true);
        let line = Line::from_text("linked", &attrs, 1, None);
        assert!(screen.record_scrollback_spill(0, &line, 1));
        screen.advance_stable_row_index_offset(1);
        let rejected = screen
            .capture_line_read(0..1)
            .unwrap()
            .hydrate_with_payload_limit(4_096, || false)
            .err()
            .expect("small text must not hide a large attribute payload");
        assert!(rejected.downcast_ref::<ColdReadPayloadLimit>().is_some());
        let ready = screen
            .capture_line_read(0..1)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        assert_eq!(ready.lines().next().unwrap(), &line);
        assert!(ready.payload_bytes() > 8_192);
        assert!(screen.validates_line_read(&ready));
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn stored_physical_metadata_cap_and_layout_kind_are_real_boundaries() {
        let (mut screen, _) = stored_physical_fixture(3, 32);
        let ready = screen
            .capture_line_read(0..3)
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        let physical = ready.layout.as_ref().unwrap();
        let canonical = ColdVisualLayout {
            kind: ColdVisualLayoutKind::Canonical,
            source: physical.source.clone(),
            visual: physical.visual.clone(),
            resident_frontier: physical.resident_frontier,
            groups: physical.groups.clone(),
            witness: physical.witness.clone(),
            interval: physical.interval.clone(),
        };
        assert!(!physical.extends(&canonical));
        assert!(!canonical.extends(physical));
        screen.cold_visual_layout = Some(Arc::new(canonical));
        assert!(screen.line_read_changes_layout(&ready));
        screen.cold_visual_seqno = 2;
        assert!(!screen.validates_line_read(&ready));

        let stored = screen.stored_physical_layout.as_mut().unwrap();
        let count = StoredPhysicalLayout::MAX_GROUPS;
        let end = count as StableRowIndex;
        stored.groups = (0..end).map(|row| row..row + 1).collect();
        stored.source = 0..end;
        stored.open_tail = false;
        let crate::config::ScrollbackIntervalCapture::Ready(interval) =
            crate::config::ScrollbackIntervalIdentity::default().capture(Some(0..end + 1))
        else {
            panic!("test interval unavailable");
        };
        stored.interval = interval;
        assert!(!stored.append(end, &Line::new(1)));
        assert_eq!(stored.groups.len(), count);
        assert_eq!(stored.source, 0..end);
    }

    #[derive(Debug, Default)]
    pub(crate) struct TestColdScrollbackSink {
        rows: Mutex<BTreeMap<StableRowIndex, Line>>,
        interval_identity: Mutex<crate::config::ScrollbackIntervalIdentity>,
        batch_reads: AtomicU64,
        single_reads: AtomicU64,
        force_busy_probe: AtomicBool,
        refuse_admission: AtomicBool,
        omit_admission_receipt: AtomicBool,
        fail_clear: AtomicBool,
        fail_replace: AtomicBool,
        revision: AtomicU64,
    }

    impl crate::config::ScrollbackSpillSink for TestColdScrollbackSink {
        fn try_capture_scrollback_interval(&self) -> crate::config::ScrollbackIntervalCapture {
            if self.force_busy_probe.load(Ordering::Relaxed) {
                return crate::config::ScrollbackIntervalCapture::Busy;
            }
            let Ok(rows) = self.rows.try_lock() else {
                return crate::config::ScrollbackIntervalCapture::Busy;
            };
            let Ok(identity) = self.interval_identity.try_lock() else {
                return crate::config::ScrollbackIntervalCapture::Busy;
            };
            let range = match (rows.first_key_value(), rows.last_key_value()) {
                (Some((first, _)), Some((last, _))) => {
                    let Some(end) = last.checked_add(1) else {
                        return crate::config::ScrollbackIntervalCapture::Unavailable;
                    };
                    Some(*first..end)
                }
                _ => None,
            };
            identity.capture(range)
        }

        fn try_capture_scrollback_usage(&self) -> crate::config::ScrollbackUsageCapture {
            if self.force_busy_probe.load(Ordering::Relaxed) {
                return crate::config::ScrollbackUsageCapture::Busy;
            }
            let Ok(rows) = self.rows.try_lock() else {
                return crate::config::ScrollbackUsageCapture::Busy;
            };
            let count = rows.len();
            let bytes = rows.values().map(Screen::estimate_line_bytes).sum();
            crate::config::ScrollbackUsageCapture::Ready(crate::config::ScrollbackUsage {
                rows: count,
                bytes,
            })
        }

        fn store_scrollback_line(
            &self,
            stable_row: StableRowIndex,
            line: &Line,
            max_retained_rows: usize,
        ) -> bool {
            self.store_scrollback_line_with_receipt(stable_row, line, max_retained_rows)
                .accepted()
        }

        fn store_scrollback_line_with_receipt(
            &self,
            stable_row: StableRowIndex,
            line: &Line,
            max_retained_rows: usize,
        ) -> crate::config::ScrollbackLineAdmission {
            use crate::config::ScrollbackLineAdmission;
            if max_retained_rows == 0 || self.refuse_admission.load(Ordering::Relaxed) {
                return ScrollbackLineAdmission::Refused;
            }

            let Some(next_revision) = self.revision.load(Ordering::Relaxed).checked_add(1) else {
                return ScrollbackLineAdmission::Refused;
            };

            let mut rows = self.rows.lock().expect("test sink mutex");
            if rows.contains_key(&stable_row) {
                *self.interval_identity.lock().unwrap() = Default::default();
            }
            rows.insert(stable_row, line.clone());
            while rows.len() > max_retained_rows {
                let Some(oldest) = rows.keys().next().copied() else {
                    break;
                };
                rows.remove(&oldest);
            }
            self.revision.store(next_revision, Ordering::Relaxed);
            let interval = if self.omit_admission_receipt.load(Ordering::Relaxed) {
                None
            } else {
                let first = *rows.first_key_value().unwrap().0;
                let end = rows.last_key_value().unwrap().0.checked_add(1);
                match end.map(|end| {
                    self.interval_identity
                        .lock()
                        .unwrap()
                        .capture(Some(first..end))
                }) {
                    Some(crate::config::ScrollbackIntervalCapture::Ready(interval)) => {
                        Some(interval)
                    }
                    _ => None,
                }
            };
            ScrollbackLineAdmission::Admitted { interval }
        }

        fn load_scrollback_line(&self, stable_row: StableRowIndex) -> Option<Line> {
            self.single_reads.fetch_add(1, Ordering::Relaxed);
            self.rows
                .lock()
                .expect("test sink mutex")
                .get(&stable_row)
                .cloned()
        }

        fn load_scrollback_lines(&self, range: Range<StableRowIndex>) -> Vec<Line> {
            self.batch_reads.fetch_add(1, Ordering::Relaxed);
            let rows = self.rows.lock().expect("test sink mutex");
            // Deliberately return short batches to exercise caller continuation.
            range
                .take(2)
                .map_while(|row| rows.get(&row).cloned())
                .collect()
        }

        fn oldest_scrollback_row(&self) -> Option<StableRowIndex> {
            self.rows
                .lock()
                .expect("test sink mutex")
                .keys()
                .next()
                .copied()
        }

        fn retained_scrollback_rows(&self) -> usize {
            self.rows.lock().expect("test sink mutex").len()
        }

        fn retained_scrollback_bytes(&self) -> usize {
            self.rows
                .lock()
                .expect("test sink mutex")
                .values()
                .map(Screen::estimate_line_bytes)
                .sum()
        }

        fn snapshot_scrollback(
            &self,
            expected_newest_exclusive: StableRowIndex,
            limits: crate::config::ScrollbackSnapshotLimits,
        ) -> Result<crate::config::ScrollbackSnapshot, crate::config::ScrollbackSpillError>
        {
            let rows = self.rows.lock().expect("test sink mutex");
            if rows.len() > limits.max_rows {
                return Err(crate::config::ScrollbackSpillError::ResourceLimit {
                    resource: "rows",
                    observed: u64::try_from(rows.len()).unwrap_or(u64::MAX),
                    maximum: u64::try_from(limits.max_rows).unwrap_or(u64::MAX),
                });
            }
            let stored_bytes = rows
                .values()
                .try_fold(0usize, |total, line| {
                    total.checked_add(Screen::estimate_line_bytes(line))
                })
                .ok_or(crate::config::ScrollbackSpillError::ArithmeticOverflow(
                    "decoded_bytes",
                ))?;
            let stored_bytes_u64 = u64::try_from(stored_bytes).map_err(|_| {
                crate::config::ScrollbackSpillError::ArithmeticOverflow("stored_bytes")
            })?;
            if stored_bytes_u64 > limits.max_stored_bytes {
                return Err(crate::config::ScrollbackSpillError::ResourceLimit {
                    resource: "stored_bytes",
                    observed: stored_bytes_u64,
                    maximum: limits.max_stored_bytes,
                });
            }
            if stored_bytes > limits.max_decoded_bytes {
                return Err(crate::config::ScrollbackSpillError::ResourceLimit {
                    resource: "decoded_bytes",
                    observed: stored_bytes_u64,
                    maximum: u64::try_from(limits.max_decoded_bytes).unwrap_or(u64::MAX),
                });
            }
            if stored_bytes_u64 > limits.max_physical_bytes {
                return Err(crate::config::ScrollbackSpillError::ResourceLimit {
                    resource: "physical_bytes",
                    observed: stored_bytes_u64,
                    maximum: limits.max_physical_bytes,
                });
            }

            let oldest = rows.keys().next().copied();
            let mut expected = oldest;
            let mut snapshot_rows = Vec::new();
            snapshot_rows
                .try_reserve_exact(rows.len())
                .map_err(|_| crate::config::ScrollbackSpillError::StorageUnavailable)?;
            for (stable_row, line) in rows.iter() {
                if Some(*stable_row) != expected {
                    return Err(crate::config::ScrollbackSpillError::SnapshotRangeMismatch);
                }
                expected = Some(stable_row.checked_add(1).ok_or(
                    crate::config::ScrollbackSpillError::ArithmeticOverflow("stable_row_range"),
                )?);
                snapshot_rows.push(line.clone());
            }
            if expected.unwrap_or(expected_newest_exclusive) != expected_newest_exclusive {
                return Err(crate::config::ScrollbackSpillError::SnapshotRangeMismatch);
            }
            crate::config::ScrollbackSnapshot::from_contiguous_rows(
                crate::config::ScrollbackSnapshotGeneration::new(
                    [0; 16],
                    self.revision.load(Ordering::Relaxed),
                ),
                crate::config::ScrollbackSnapshotFidelity::ExactSemantic,
                oldest,
                expected_newest_exclusive,
                stored_bytes_u64,
                stored_bytes,
                snapshot_rows,
            )
        }

        fn replace_scrollback_prefix(
            &self,
            expected_generation: Option<crate::config::ScrollbackSnapshotGeneration>,
            prefix: crate::config::ScrollbackPrefix<'_>,
            max_retained_rows: usize,
        ) -> Result<crate::config::ScrollbackReplaceCommit, crate::config::ScrollbackSpillError>
        {
            let mut rows = self.rows.lock().expect("test sink mutex");
            let current_generation = crate::config::ScrollbackSnapshotGeneration::new(
                [0; 16],
                self.revision.load(Ordering::Relaxed),
            );
            match expected_generation {
                Some(expected) if expected != current_generation => {
                    return Err(crate::config::ScrollbackSpillError::SnapshotGenerationMismatch);
                }
                None if !rows.is_empty() || current_generation.revision() != 0 => {
                    return Err(crate::config::ScrollbackSpillError::SnapshotGenerationMismatch);
                }
                _ => {}
            }
            if prefix.row_count() > max_retained_rows {
                return Err(crate::config::ScrollbackSpillError::ResourceLimit {
                    resource: "rows",
                    observed: u64::try_from(prefix.row_count()).unwrap_or(u64::MAX),
                    maximum: u64::try_from(max_retained_rows).unwrap_or(u64::MAX),
                });
            }
            if self.fail_replace.load(Ordering::Relaxed) {
                return Err(crate::config::ScrollbackSpillError::StorageUnavailable);
            }
            let next_revision = current_generation
                .revision()
                .checked_add(1)
                .ok_or(crate::config::ScrollbackSpillError::RevisionExhausted)?;
            let mut replacement = BTreeMap::new();
            if let Some(oldest) = prefix.oldest_stable_row() {
                for (offset, line) in prefix.rows().enumerate() {
                    let offset = StableRowIndex::try_from(offset).map_err(|_| {
                        crate::config::ScrollbackSpillError::ArithmeticOverflow("row_count")
                    })?;
                    let stable_row = oldest.checked_add(offset).ok_or(
                        crate::config::ScrollbackSpillError::ArithmeticOverflow("stable_row_range"),
                    )?;
                    replacement.insert(stable_row, line.clone());
                }
            }
            *rows = replacement;
            *self.interval_identity.lock().unwrap() = Default::default();
            self.revision.store(next_revision, Ordering::Relaxed);
            Ok(crate::config::ScrollbackReplaceCommit::new(
                crate::config::ScrollbackSnapshotGeneration::new([0; 16], next_revision),
                prefix.oldest_stable_row(),
                prefix.newest_stable_row_exclusive(),
            ))
        }

        fn clear_scrollback(
            &self,
        ) -> Result<crate::config::ScrollbackClearCommit, crate::config::ScrollbackSpillError>
        {
            if self.fail_clear.load(Ordering::Relaxed) {
                return Err(crate::config::ScrollbackSpillError::StorageUnavailable);
            }
            let next_revision = self
                .revision
                .load(Ordering::Relaxed)
                .checked_add(1)
                .ok_or(crate::config::ScrollbackSpillError::RevisionExhausted)?;
            let mut rows = self.rows.lock().expect("test sink mutex");
            rows.clear();
            *self.interval_identity.lock().unwrap() = Default::default();
            self.revision.store(next_revision, Ordering::Relaxed);
            Ok(crate::config::ScrollbackClearCommit::new(
                crate::config::ScrollbackSnapshotGeneration::new([0; 16], next_revision),
            ))
        }
    }

    #[derive(Debug)]
    struct RejectingColdScrollbackSink;

    impl crate::config::ScrollbackSpillSink for RejectingColdScrollbackSink {
        fn store_scrollback_line(
            &self,
            _stable_row: StableRowIndex,
            _line: &Line,
            _max_retained_rows: usize,
        ) -> bool {
            false
        }

        fn load_scrollback_line(&self, _stable_row: StableRowIndex) -> Option<Line> {
            None
        }

        fn oldest_scrollback_row(&self) -> Option<StableRowIndex> {
            None
        }

        fn retained_scrollback_rows(&self) -> usize {
            0
        }

        fn retained_scrollback_bytes(&self) -> usize {
            0
        }

        fn try_capture_scrollback_usage(&self) -> crate::config::ScrollbackUsageCapture {
            crate::config::ScrollbackUsageCapture::Ready(crate::config::ScrollbackUsage {
                rows: 0,
                bytes: 0,
            })
        }

        fn snapshot_scrollback(
            &self,
            expected_newest_exclusive: StableRowIndex,
            _limits: crate::config::ScrollbackSnapshotLimits,
        ) -> Result<crate::config::ScrollbackSnapshot, crate::config::ScrollbackSpillError>
        {
            crate::config::ScrollbackSnapshot::from_contiguous_rows(
                crate::config::ScrollbackSnapshotGeneration::new([0; 16], 0),
                crate::config::ScrollbackSnapshotFidelity::ExactSemantic,
                None,
                expected_newest_exclusive,
                0,
                0,
                Vec::new(),
            )
        }

        fn replace_scrollback_prefix(
            &self,
            _expected_generation: Option<crate::config::ScrollbackSnapshotGeneration>,
            _prefix: crate::config::ScrollbackPrefix<'_>,
            _max_retained_rows: usize,
        ) -> Result<crate::config::ScrollbackReplaceCommit, crate::config::ScrollbackSpillError>
        {
            Err(crate::config::ScrollbackSpillError::StorageUnavailable)
        }

        fn clear_scrollback(
            &self,
        ) -> Result<crate::config::ScrollbackClearCommit, crate::config::ScrollbackSpillError>
        {
            Ok(crate::config::ScrollbackClearCommit::new(
                crate::config::ScrollbackSnapshotGeneration::new([0; 16], 0),
            ))
        }
    }

    #[cfg(feature = "use_serde")]
    fn recovered_scrollback_screen(
        cold_snapshot_generation: Option<crate::config::ScrollbackSnapshotGeneration>,
    ) -> Screen {
        let attributes = CellAttributes::blank();
        let parts = ScreenCheckpointParts {
            lines: vec![
                Line::from_text("cold-original", &attributes, 1, None),
                Line::from_text("hot-original", &attributes, 1, None),
                Line::from_text("hot-replayed", &attributes, 2, None),
                Line::from_text("visible-a", &attributes, 2, None),
                Line::from_text("visible-b", &attributes, 2, None),
            ],
            stable_row_index_offset: 10,
            cold_snapshot_generation,
            cold_prefix_line_count: 1,
            allow_scrollback: true,
            keyboard_stack: Vec::new(),
            physical_rows: 2,
            physical_cols: 16,
            dpi: 96,
            saved_cursor: None,
        };
        let replay_config: Arc<dyn TerminalConfiguration> = Arc::new(TestTermConfig {
            scrollback: 5,
            ..TestTermConfig::default()
        });
        Screen::from_validated_checkpoint_parts(parts, &replay_config, 2, bidi_mode())
            .expect("restore recovery screen")
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn recovery_activation_atomically_replaces_exact_prefix_before_resident_removal() {
        let generation = crate::config::ScrollbackSnapshotGeneration::new([0; 16], 0);
        let mut screen = recovered_scrollback_screen(Some(generation));
        let coordinate_witness = screen.capture_coordinate_witness();
        let sink = Arc::new(TestColdScrollbackSink::default());
        let live_config: Arc<dyn TerminalConfiguration> = Arc::new(TestTermConfig {
            scrollback: 5,
            scrollback_tier: crate::config::ScrollbackTierConfig {
                enabled: true,
                hot_lines: 1,
                warm_max_bytes: 0,
            },
            cold_sink: Some(sink.clone()),
            ..TestTermConfig::default()
        });

        screen
            .activate_recovered_scrollback(&live_config)
            .expect("publish and activate exact prefix");
        assert!(!screen.matches_coordinate_witness(&coordinate_witness));

        let rows = sink.rows.lock().expect("test sink mutex");
        assert_eq!(rows.keys().copied().collect::<Vec<_>>(), vec![10, 11]);
        assert_eq!(rows[&10].as_str(), "cold-original");
        assert_eq!(rows[&11].as_str(), "hot-original");
        drop(rows);
        assert_eq!(screen.stable_row_index_offset, 12);
        assert_eq!(screen.lines.len(), 3);
        assert_eq!(
            screen.lines.front().expect("hot row").as_str(),
            "hot-replayed"
        );
        assert!(screen.recovery_scrollback.is_none());
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn recovery_activation_publication_failure_retains_every_resident_row() {
        let generation = crate::config::ScrollbackSnapshotGeneration::new([0; 16], 0);
        let mut screen = recovered_scrollback_screen(Some(generation));
        let before_lines = screen.lines.clone();
        let before_offset = screen.stable_row_index_offset;
        let sink = Arc::new(TestColdScrollbackSink::default());
        sink.fail_replace.store(true, Ordering::Relaxed);
        let live_config: Arc<dyn TerminalConfiguration> = Arc::new(TestTermConfig {
            scrollback: 5,
            scrollback_tier: crate::config::ScrollbackTierConfig {
                enabled: true,
                hot_lines: 1,
                warm_max_bytes: 0,
            },
            cold_sink: Some(sink.clone()),
            ..TestTermConfig::default()
        });

        assert_eq!(
            screen.activate_recovered_scrollback(&live_config),
            Err(ScrollbackActivationError::Spill(
                ScrollbackSpillError::StorageUnavailable,
            )),
        );
        assert_eq!(screen.lines, before_lines);
        assert_eq!(screen.stable_row_index_offset, before_offset);
        assert!(screen.recovery_scrollback.is_some());
        assert_eq!(sink.retained_scrollback_rows(), 0);
    }

    #[test]
    fn resize_padding_borrows_bidi_mode_from_existing_lines() {
        let config: Arc<dyn TerminalConfiguration> = Arc::new(TestTermConfig::default());
        let mut screen = Screen::new(test_size(2, 4, 96), &config, true, 0, rtl_bidi_mode());

        screen.resize(test_size(4, 4, 96), test_cursor(0, 1, 1), 2, false);

        for line in &screen.lines {
            assert_eq!(
                line.bidi_info(),
                (true, ParagraphDirectionHint::RightToLeft)
            );
        }
    }

    #[test]
    fn conpty_resize_padding_borrows_bidi_mode_from_existing_lines() {
        let rtl_mode = rtl_bidi_mode();
        let mut screen = {
            let config: Arc<dyn TerminalConfiguration> = Arc::new(TestTermConfig::default());
            Screen::new(test_size(2, 4, 96), &config, true, 0, rtl_mode)
        };
        let region: Range<VisibleRowIndex> = 0..(screen.physical_rows as VisibleRowIndex);

        for seq in 1..=3 {
            screen.scroll_up(&region, 1, seq, CellAttributes::blank(), rtl_mode);
        }

        let len_before = screen.lines.len();
        screen.resize(test_size(4, 4, 96), test_cursor(0, 1, 10), 11, true);

        assert!(
            screen.lines.len() > len_before,
            "conpty resize should append rows after the cursor"
        );
        for line in screen.lines.iter().skip(len_before) {
            assert_eq!(
                line.bidi_info(),
                (true, ParagraphDirectionHint::RightToLeft)
            );
        }
    }

    #[test]
    fn resize_wrap_policy_defaults_preserve_hot_path_budget() {
        // TestTermConfig::default() explicitly disables scorecard for test isolation.
        // This test verifies the Screen correctly reflects the config it was given.
        let screen = test_screen(3, 4, 96);
        let policy = screen.resize_wrap_policy();
        assert_eq!(
            policy.kp_cost_model,
            MonospaceKpCostModel::terminal_default()
        );
        assert!(
            !policy.scorecard_enabled,
            "scorecard should reflect TestTermConfig default (disabled for test isolation)"
        );
        assert!(
            !policy.readability_gate.enabled,
            "readability gate should reflect TestTermConfig default (disabled for test isolation)"
        );
    }

    #[test]
    fn resize_wrap_production_defaults_enable_scorecard_and_gate() {
        // Verify that the production defaults (ResizeWrapPolicy::default())
        // have scorecard and readability gate enabled with sensible thresholds.
        let policy = ResizeWrapPolicy::default();
        assert!(
            policy.scorecard_enabled,
            "production default should enable scorecard for resize quality telemetry"
        );
        assert!(
            policy.readability_gate.enabled,
            "production default should enable readability gate"
        );
        assert_eq!(
            policy.readability_gate.max_line_badness_delta, 500,
            "production default max_line_badness_delta"
        );
        assert_eq!(
            policy.readability_gate.max_total_badness_delta, 2000,
            "production default max_total_badness_delta"
        );
        assert_eq!(
            policy.readability_gate.max_fallback_ratio_percent, 20,
            "production default max_fallback_ratio_percent"
        );
    }

    #[test]
    fn resize_wrap_policy_seeds_from_terminal_configuration() {
        let mut tuned_model = MonospaceKpCostModel::terminal_default();
        tuned_model.badness_scale = 42_000;
        tuned_model.forced_break_penalty = 7_500;
        tuned_model.lookahead_limit = 24;
        tuned_model.max_dp_states = 2_048;

        let screen = test_screen_with_config(
            4,
            6,
            96,
            TestTermConfig {
                scrollback: 64,
                scrollback_tier: crate::config::ScrollbackTierConfig::default(),
                cold_sink: None,
                kp_cost_model: tuned_model,
                scorecard_enabled: true,
                readability_gate: ResizeReadabilityGatePolicy {
                    enabled: true,
                    max_line_badness_delta: 12_345,
                    max_total_badness_delta: 67_890,
                    max_fallback_ratio_percent: 150,
                },
            },
        );

        let policy = screen.resize_wrap_policy();
        assert_eq!(policy.kp_cost_model, tuned_model);
        assert!(policy.scorecard_enabled);
        assert!(policy.readability_gate.enabled);
        assert_eq!(policy.readability_gate.max_line_badness_delta, 12_345);
        assert_eq!(policy.readability_gate.max_total_badness_delta, 67_890);
        assert_eq!(
            policy.readability_gate.max_fallback_ratio_percent, 100,
            "screen policy should clamp invalid fallback thresholds"
        );
    }

    #[test]
    fn no_match_hyperlink_scans_preserve_cached_resize_layouts() {
        let mut screen = test_screen(3, 4, 96);
        let attrs = CellAttributes::blank();
        screen.lines = VecDeque::from(vec![
            Line::from_text_with_wrapped_last_col("abcd", &attrs, 0),
            Line::from_text("ef", &attrs, 0, None),
            Line::new(0),
        ]);
        let rules = vec![frankenterm_surface::hyperlink::Rule::new(r"https?://\S+", "$0").unwrap()];
        let mut cursor = test_cursor(1, 1, 1);
        for cols in [3, 4, 3] {
            cursor = screen.resize(test_size(3, cols, 96), cursor, 1, false);
            for line in &mut screen.lines {
                line.scan_and_create_hyperlinks(&rules);
                assert!(!line.has_hyperlink());
            }
        }
        assert_eq!(
            screen.rewrap_cache.as_ref().unwrap().wrapped_by_key.len(),
            2,
            "painting no-match hyperlinks must not throw away unchanged resize layouts"
        );
    }

    #[test]
    fn cached_logical_content_survives_distinct_width_plans_and_snapshot_clones() {
        let screen = test_screen(3, 20, 96);
        let logical = CachedLogicalLine {
            line: Line::from_text(
                &"界面 e\u{301} ffi ".repeat(12),
                &CellAttributes::blank(),
                1,
                None,
            ),
            wrap_source: Some(Arc::new(std::sync::OnceLock::new())),
        };
        let snapshot = logical.clone();
        let physical = VecDeque::new();
        let mut scratch = LineWrapWidthPrefixScratch::default();
        for (request, cols) in [8, 8, 13, 5, 21].iter().copied().enumerate() {
            let seqno = 2 + request as SequenceNo;
            let mut policy = screen.resize_wrap_policy;
            policy.scorecard_enabled = request != 4;
            policy.kp_cost_model.badness_scale += request as u64 * 997;
            let builds_before = REFLOW_RETAINED_SOURCE_BUILDS.with(|count| count.get());
            let replans_before = REFLOW_RETAINED_REPLAN_CALLS.with(|count| count.get());
            let (actual, scorecard) = Screen::wrap_logical_line_source_for_resize(
                &snapshot,
                &physical,
                cols,
                seqno,
                policy,
                &mut scratch,
            );
            assert_eq!(
                REFLOW_RETAINED_SOURCE_BUILDS.with(|count| count.get()) - builds_before,
                usize::from(request == 0),
                "only the first request extracts the retained source"
            );
            assert_eq!(
                REFLOW_RETAINED_REPLAN_CALLS.with(|count| count.get()) - replans_before,
                usize::from(request != 0),
                "the initializing request must consume its existing plan"
            );
            let (expected, expected_scorecard) = Screen::wrap_single_logical_line_for_resize(
                logical.line.clone(),
                cols,
                seqno,
                policy,
                &mut scratch,
            );
            let RewrapScratch::Lines(actual) = actual else {
                panic!("expected wrapped rows")
            };
            assert_eq!(actual, expected);
            assert_eq!(scorecard, expected_scorecard);
            assert_eq!(scorecard.is_some(), policy.scorecard_enabled);
            assert!(actual.iter().all(|line| line.current_seqno() == seqno));
            assert!(logical.wrap_source.as_ref().unwrap().get().is_some());
            assert!(std::ptr::eq(
                logical.wrap_source.as_ref().unwrap().get().unwrap(),
                snapshot.wrap_source.as_ref().unwrap().get().unwrap(),
            ));
        }
    }

    #[test]
    fn retained_wrap_concurrent_requests_plan_once_each_for_their_own_policy() {
        let logical = CachedLogicalLine {
            line: Line::from_text(
                &"界面 e\u{301} ffi => 🚀 ".repeat(64),
                &CellAttributes::blank(),
                1,
                None,
            ),
            wrap_source: Some(Arc::new(std::sync::OnceLock::new())),
        };
        let mut tuned_policy = ResizeWrapPolicy::default();
        tuned_policy.kp_cost_model.badness_scale = 42_000;
        tuned_policy.kp_cost_model.forced_break_penalty = 7_500;
        tuned_policy.kp_cost_model.lookahead_limit = 24;
        tuned_policy.kp_cost_model.max_dp_states = 2_048;
        let requests = [(8, 7, ResizeWrapPolicy::default()), (13, 11, tuned_policy)];
        let start = std::sync::Barrier::new(requests.len());
        let counts = std::thread::scope(|scope| {
            let workers = requests.map(|(cols, seqno, policy)| {
                let logical = &logical;
                let start = &start;
                scope.spawn(move || {
                    let builds_before = REFLOW_RETAINED_SOURCE_BUILDS.with(|count| count.get());
                    let replans_before = REFLOW_RETAINED_REPLAN_CALLS.with(|count| count.get());
                    let mut scratch = LineWrapWidthPrefixScratch::default();
                    start.wait();
                    let (actual, scorecard) = Screen::wrap_logical_line_source_for_resize(
                        logical,
                        &VecDeque::new(),
                        cols,
                        seqno,
                        policy,
                        &mut scratch,
                    );
                    let builds =
                        REFLOW_RETAINED_SOURCE_BUILDS.with(|count| count.get()) - builds_before;
                    let replans =
                        REFLOW_RETAINED_REPLAN_CALLS.with(|count| count.get()) - replans_before;
                    let (expected, expected_scorecard) =
                        Screen::wrap_single_logical_line_for_resize(
                            logical.line.clone(),
                            cols,
                            seqno,
                            policy,
                            &mut scratch,
                        );
                    let RewrapScratch::Lines(actual) = actual else {
                        panic!("expected wrapped rows")
                    };
                    assert_eq!(actual, expected);
                    assert_eq!(scorecard, expected_scorecard);
                    assert!(actual.iter().all(|line| line.current_seqno() == seqno));
                    assert_eq!(builds + replans, 1, "one plan per caller");
                    (builds, replans)
                })
            });
            workers.map(|worker| worker.join().expect("retained wrap request must complete"))
        });
        assert_eq!(counts.iter().map(|(builds, _)| builds).sum::<usize>(), 1);
        assert_eq!(counts.iter().map(|(_, replans)| replans).sum::<usize>(), 1);
        assert!(logical.wrap_source.as_ref().unwrap().get().is_some());
    }

    #[test]
    fn resize_cache_snapshot_shares_immutable_arrays_and_keeps_lru_independent() {
        let mut screen = test_screen(3, 20, 96);
        screen.lines = (0..16)
            .map(|index| {
                Line::from_text(
                    &format!("{index:02} 界e\u{301} abcdefghijklmnop"),
                    &CellAttributes::blank(),
                    1,
                    None,
                )
            })
            .collect();
        let cursor = screen.resize(test_size(3, 8, 96), test_cursor(0, 2, 1), 2, false);
        let before = Arc::clone(screen.rewrap_cache.as_ref().unwrap());
        let old_key = WrapCacheKey {
            physical_cols: 8,
            dpi: 96,
        };
        let before_order = before.wrap_key_order.clone();
        assert!(
            before.wrapped_by_key[&old_key].row_prefix.get().is_none(),
            "uncached synchronous reflow must not build a second prefix"
        );
        let mut prepared = screen
            .capture_reflow_preparation(test_size(3, 5, 96), cursor)
            .unwrap();
        assert!(prepared.prepare(|| false));
        let after = prepared.snapshot.rewrap_cache.as_ref().unwrap();
        assert!(!Arc::ptr_eq(&before, after), "cache metadata must detach");
        assert!(Arc::ptr_eq(&before.logical_lines, &after.logical_lines));
        assert!(Arc::ptr_eq(
            &before.wrapped_by_key[&old_key].lines,
            &after.wrapped_by_key[&old_key].lines,
        ));
        assert!(Arc::ptr_eq(
            &before.wrapped_by_key[&old_key].row_prefix,
            &after.wrapped_by_key[&old_key].row_prefix,
        ));
        assert_eq!(before.wrap_key_order, before_order);
        assert_eq!(before.wrapped_by_key.len(), 1);
        assert_eq!(after.wrapped_by_key.len(), 2);

        let mut cache = after.as_ref().clone();
        let hit = cache.get_wrapped(old_key).unwrap();
        let prefix_builds_before = REFLOW_ROW_PREFIX_BUILDS.with(|count| count.get());
        let hit_prefix = Arc::clone(hit.row_prefix());
        assert!(Arc::ptr_eq(
            &hit_prefix,
            before.wrapped_by_key[&old_key].row_prefix(),
        ));
        assert_eq!(
            REFLOW_ROW_PREFIX_BUILDS.with(|count| count.get()),
            prefix_builds_before + 1,
            "all snapshots must reuse the first initialized target prefix"
        );
        assert!(Arc::ptr_eq(
            &hit.lines,
            &before.wrapped_by_key[&old_key].lines,
        ));
        let original_rows = hit.lines.clone();
        for cols in 30..30 + MAX_WRAP_CACHE_ENTRIES {
            cache.insert_wrapped(
                WrapCacheKey {
                    physical_cols: cols,
                    dpi: 96,
                },
                vec![Arc::from(vec![Line::from_text(
                    "retained",
                    &CellAttributes::blank(),
                    1,
                    None,
                )])],
                None,
                None,
            );
        }
        assert_eq!(cache.wrapped_by_key.len(), MAX_WRAP_CACHE_ENTRIES);
        assert!(!cache.wrapped_by_key.contains_key(&old_key));
        assert!(Arc::ptr_eq(
            &original_rows,
            &before.wrapped_by_key[&old_key].lines,
        ));
        assert_eq!(before.wrapped_by_key.len(), 1);
        cache.clear_wraps();
        assert!(cache.wrapped_by_key.is_empty());
        assert!(cache.wrap_key_order.is_empty());
        assert_eq!(before.wrapped_by_key.len(), 1);
    }

    #[test]
    fn retained_wrap_budget_charges_word_metadata_before_admission() {
        let line = Line::from_text("alpha beta gamma", &CellAttributes::blank(), 1, None);
        let width_only_budget = line.len()
            * (std::mem::size_of::<Cell>()
                + std::mem::size_of::<usize>()
                + std::mem::size_of::<u128>())
            + std::mem::size_of::<LineWrapLayout>()
            + std::mem::size_of::<LineWrapWidthPrefixScratch>()
            + std::mem::size_of::<u128>();
        let cache =
            LogicalLineWrapCache::with_token_budget(None, vec![line.clone()], width_only_budget);
        assert!(cache.logical_lines[0].wrap_source.is_none());
        assert_eq!(cache.logical_lines[0].line, line);
        let (actual, _) = Screen::wrap_logical_line_source_for_resize(
            &cache.logical_lines[0],
            &VecDeque::new(),
            8,
            2,
            ResizeWrapPolicy::default(),
            &mut LineWrapWidthPrefixScratch::default(),
        );
        let RewrapScratch::Lines(actual) = actual else {
            panic!("uncached wrapping must remain available under retention pressure")
        };
        assert_eq!(actual, line.wrap(8, 2));
    }

    #[test]
    fn retained_wrap_budget_prefers_recent_lines_without_changing_row_order() {
        let first = Line::from_text("first", &CellAttributes::blank(), 1, None);
        let last = Line::from_text("last!", &CellAttributes::blank(), 1, None);
        let one_line_budget = first.len()
            * (std::mem::size_of::<Cell>()
                + 2 * std::mem::size_of::<u128>()
                + std::mem::size_of::<bool>())
            + (2 * first.len()).max(8) * std::mem::size_of::<usize>()
            + std::mem::size_of::<std::sync::OnceLock<LineWrapLayout>>()
            + std::mem::size_of::<LineWrapWidthPrefixScratch>()
            + std::mem::size_of::<u128>()
            + 6 * std::mem::size_of::<usize>();
        let cache = LogicalLineWrapCache::with_token_budget(
            None,
            vec![first.clone(), last.clone()],
            one_line_budget,
        );
        assert_eq!(cache.logical_lines[0].line, first);
        assert_eq!(cache.logical_lines[1].line, last);
        assert!(cache.logical_lines[0].wrap_source.is_none());
        assert!(cache.logical_lines[1].wrap_source.is_some());
        let screen = test_screen(3, 20, 96);
        let (actual, _) = Screen::wrap_logical_line_source_for_resize(
            &cache.logical_lines[0],
            &VecDeque::new(),
            2,
            2,
            screen.resize_wrap_policy,
            &mut LineWrapWidthPrefixScratch::default(),
        );
        let RewrapScratch::Lines(actual) = actual else {
            panic!("expected wrapped rows")
        };
        assert_eq!(actual, first.wrap(2, 2));
    }

    #[test]
    fn cached_no_match_scan_cannot_suppress_a_new_rule_epoch() {
        use frankenterm_surface::hyperlink::Rule;
        let mut screen = test_screen(4, 6, 96);
        let attrs = CellAttributes::blank();
        screen.lines = VecDeque::from(vec![
            Line::from_text("abcdef", &attrs, 1, None),
            Line::from_text("xy", &attrs, 1, None),
            Line::new(1),
            Line::new(1),
        ]);
        let old_rules = vec![Rule::new("never-matches", "$0").unwrap()];
        for line in &mut screen.lines {
            line.scan_and_create_hyperlinks(&old_rules);
        }
        let cursor = screen.resize(test_size(4, 4, 96), test_cursor(0, 0, 1), 1, false);
        assert!(screen.rewrap_cache.as_ref().unwrap().logical_lines[0]
            .line
            .implicit_hyperlinks_are_scanned());
        let cursor = screen.resize(test_size(4, 3, 96), cursor, 1, false);
        // Rule epochs can change at the SAME sequence. The cache contains a
        // valid old no-match result, but the current lines need the new rules.
        for line in &mut screen.lines {
            line.invalidate_implicit_hyperlinks(1);
        }
        let mut fresh = screen.clone();
        fresh.rewrap_cache = None;
        let actual_cursor = screen.resize(test_size(4, 4, 96), cursor, 1, false);
        let fresh_cursor = fresh.resize(test_size(4, 4, 96), cursor, 1, false);
        assert_eq!(actual_cursor, fresh_cursor);
        assert_eq!(
            screen.rewrap_cache.as_ref().unwrap().wrapped_by_key.len(),
            2
        );
        let new_rules = vec![Rule::new("abcdef|xy", "https://new.example/$0").unwrap()];
        for target in [&mut screen, &mut fresh] {
            for range in Screen::logical_line_physical_ranges(&target.lines) {
                let mut lines: Vec<_> = target
                    .lines
                    .iter_mut()
                    .skip(range.start)
                    .take(range.len())
                    .collect();
                Line::apply_hyperlink_rules(&new_rules, &mut lines);
            }
        }
        assert_eq!(screen.lines, fresh.lines);
        let links: Vec<_> = screen
            .lines
            .iter()
            .flat_map(Line::visible_cells)
            .filter_map(|cell| cell.attrs().hyperlink().cloned())
            .collect();
        assert_eq!(
            links.len(),
            8,
            "new rules must reach wrapped and unwrapped records"
        );
        assert_eq!(
            links
                .iter()
                .filter(|link| link.uri() == "https://new.example/xy")
                .count(),
            2
        );
    }

    #[test]
    fn rewrap_cache_tracks_width_and_dpi_keys() {
        let mut screen = test_screen(3, 4, 96);
        let attrs = CellAttributes::blank();

        screen.lines = VecDeque::from(vec![
            Line::from_text_with_wrapped_last_col("abcd", &attrs, 0),
            Line::from_text("ef", &attrs, 0, None),
            Line::new(0),
        ]);

        let cursor = test_cursor(1, 1, 1);
        let cursor = screen.resize(test_size(3, 3, 96), cursor, 1, false);

        let cache = screen.rewrap_cache.as_ref().expect("rewrap cache to exist");
        assert_eq!(cache.wrapped_by_key.len(), 1);
        assert!(
            cache.wrapped_by_key.contains_key(&WrapCacheKey {
                physical_cols: 3,
                dpi: 96
            }),
            "expected resize key 3x96 in wrap cache"
        );

        let cursor = screen.resize(test_size(3, 4, 96), cursor, 2, false);
        let cache = screen.rewrap_cache.as_ref().expect("rewrap cache to exist");
        assert_eq!(cache.wrapped_by_key.len(), 2);
        assert!(
            cache.wrapped_by_key.contains_key(&WrapCacheKey {
                physical_cols: 4,
                dpi: 96
            }),
            "expected resize key 4x96 in wrap cache"
        );

        let _cursor = screen.resize(test_size(3, 4, 144), cursor, 3, false);
        let cache = screen.rewrap_cache.as_ref().expect("rewrap cache to exist");
        assert_eq!(
            cache.wrapped_by_key.len(),
            0,
            "dpi change should clear wrap cache keys"
        );
    }

    #[test]
    fn prepared_reflow_matches_synchronous_geometry_and_cursor() {
        for (conpty, initial_cursor_y) in [(false, 1), (false, 3), (true, 1), (true, 3)] {
            let mut screen = test_screen(4, 8, 96);
            let attrs = CellAttributes::blank();
            screen.lines = VecDeque::from(vec![
                Line::from_text_with_wrapped_last_col("ab界cdef", &attrs, 1),
                Line::from_text("e\u{301}🦀z", &attrs, 1, None),
                Line::from_text("abcdefghijklmno", &attrs, 1, None),
                Line::new(1),
            ]);
            // Cover both the original last-row cursor and a cursor above a
            // trailing blank, exercising worker/live pruning parity.
            let mut cursor = test_cursor(0, initial_cursor_y, 1);
            for (index, (rows, cols, dpi)) in [
                (3, 3, 96),
                (6, 11, 144),
                (1, 2, 144),
                (4, 8, 96),
                (3, 3, 96),
            ]
            .iter()
            .copied()
            .enumerate()
            {
                let size = test_size(rows, cols, dpi);
                let seqno = index + 2;
                let mut prepared = screen.capture_reflow_preparation(size, cursor).unwrap();
                assert!(prepared.prepare(|| false));
                assert!(screen.matches_reflow_preparation(&prepared, size, cursor));
                let mut synchronous = screen.clone();
                let expected = synchronous.resize(size, cursor, seqno, conpty);
                cursor = screen.resize_with_prepared_reflow(
                    size,
                    cursor,
                    seqno,
                    conpty,
                    Some(&mut prepared),
                );
                assert!(
                    !prepared.ready,
                    "the validated preparation must be consumed"
                );
                assert_eq!(cursor, expected);
                assert_eq!(screen.lines, synchronous.lines);
                assert_eq!(screen.physical_rows, synchronous.physical_rows);
                assert_eq!(screen.physical_cols, synchronous.physical_cols);
                assert_eq!(
                    screen.last_resize_wrap_scorecard,
                    synchronous.last_resize_wrap_scorecard
                );
            }
        }
    }

    #[test]
    fn prepared_reflow_skips_source_rehash_but_stale_work_uses_fresh_source() {
        for stale in [false, true] {
            let mut screen = test_screen_with_scorecard(3, 20);
            screen.lines = (0..12)
                .map(|index| {
                    Line::from_text(
                        &format!("{index:02} 界👩‍💻e\u{301} abcdefghijklmnop"),
                        &CellAttributes::blank(),
                        1,
                        None,
                    )
                })
                .collect();
            let cursor = test_cursor(0, 2, 1);
            let size = test_size(3, 7, 96);
            assert!(screen.rewrap_cache.is_none());
            let mut prepared = screen.capture_reflow_preparation(size, cursor).unwrap();
            REFLOW_SOURCE_SIGNATURE_SCANS.with(|count| count.set(0));
            REFLOW_LAYOUT_LINE_HASHES.with(|count| count.set(0));
            assert!(prepared.prepare(|| false));
            assert_eq!(
                REFLOW_SOURCE_SIGNATURE_SCANS.with(|count| count.get()),
                0,
                "cold preparation must not hash the initial source"
            );
            assert_eq!(
                prepared
                    .snapshot
                    .rewrap_cache
                    .as_ref()
                    .unwrap()
                    .source_signature,
                None,
                "prepared source rows have not authorized a generic cache lookup"
            );
            let key = WrapCacheKey {
                physical_cols: size.cols,
                dpi: size.dpi,
            };
            let prepared_target = &prepared
                .snapshot
                .rewrap_cache
                .as_ref()
                .unwrap()
                .wrapped_by_key[&key];
            let prepared_prefix = Arc::clone(
                prepared_target
                    .row_prefix
                    .get()
                    .expect("worker must initialize the target prefix before commit"),
            );
            let mut expected_prefix = vec![0];
            for chunk in prepared_target.lines.iter() {
                expected_prefix.push(expected_prefix.last().unwrap() + chunk.len());
            }
            assert_eq!(prepared_prefix.as_ref(), expected_prefix.as_slice());
            assert_eq!(REFLOW_LAYOUT_LINE_HASHES.with(|count| count.get()), 0);
            assert!(
                prepared
                    .snapshot
                    .rewrap_cache
                    .as_ref()
                    .unwrap()
                    .published_target
                    .is_none(),
                "unpublished targets must not authorize their source cache"
            );
            if stale {
                // Keep the sequence unchanged: only exact source validation
                // protects this mutation from an old prepared target.
                screen.lines[0].set_cell(0, Cell::new('Z', CellAttributes::blank()), 1);
            }
            let mut expected = screen.clone();
            REFLOW_SOURCE_SIGNATURE_SCANS.with(|count| count.set(0));
            let expected_cursor = expected.resize(size, cursor, 2, false);
            assert_eq!(
                REFLOW_SOURCE_SIGNATURE_SCANS.with(|count| count.get()),
                1,
                "direct cold resizing must retain its initial source hash"
            );
            assert!(expected.last_resize_wrap_scorecard.is_some());
            assert!(expected.last_resize_wrap_gate_payload.is_some());
            REFLOW_SOURCE_SIGNATURE_SCANS.with(|count| count.set(0));
            REFLOW_FULL_LAYOUT_SIGNATURE_SCANS.with(|count| count.set(0));
            REFLOW_ROW_PREFIX_BUILDS.with(|count| count.set(0));
            REFLOW_CURSOR_PREFIX_SCANS.with(|count| count.set(0));
            let actual_cursor =
                screen.resize_with_prepared_reflow(size, cursor, 2, false, Some(&mut prepared));
            let scans = REFLOW_SOURCE_SIGNATURE_SCANS.with(|count| count.get());
            let full_scans = REFLOW_FULL_LAYOUT_SIGNATURE_SCANS.with(|count| count.get());
            let prefix_builds = REFLOW_ROW_PREFIX_BUILDS.with(|count| count.get());
            let cursor_scans = REFLOW_CURSOR_PREFIX_SCANS.with(|count| count.get());
            assert_eq!(prepared.was_applied(), !stale);
            if stale {
                assert!(scans > 0, "stale work must use the source-hashing fallback");
                assert!(
                    prefix_builds > 0,
                    "stale rows require a fresh target prefix"
                );
                assert!(
                    cursor_scans > 0,
                    "stale source requires live cursor mapping"
                );
            } else {
                assert_eq!(scans, 0, "validated commit must not hash the source again");
                assert_eq!(
                    full_scans, 0,
                    "immutable target publication needs no full hash"
                );
                assert_eq!(
                    cursor_scans, 0,
                    "accepted commit must reuse worker cursor mapping"
                );
                assert_eq!(
                    prefix_builds, 0,
                    "commit must not rebuild the prepared prefix"
                );
                assert!(Arc::ptr_eq(
                    &prepared_prefix,
                    screen.rewrap_cache.as_ref().unwrap().wrapped_by_key[&key].row_prefix(),
                ));
            }
            let cache = screen.rewrap_cache.as_ref().unwrap();
            assert_eq!(cache.source_signature, None);
            assert!(cache.matches_published_target(&screen.lines));
            assert_eq!(actual_cursor, expected_cursor);
            assert_eq!(screen.lines, expected.lines);
            assert_eq!(screen.physical_cols, expected.physical_cols);
            assert_eq!(screen.physical_rows, expected.physical_rows);
            assert_eq!(
                screen.stable_row_index_offset,
                expected.stable_row_index_offset
            );
            assert_eq!(
                screen.last_resize_wrap_scorecard,
                expected.last_resize_wrap_scorecard
            );
            assert_eq!(
                screen.last_resize_wrap_gate_payload,
                expected.last_resize_wrap_gate_payload
            );
        }
    }

    #[test]
    fn unsigned_prepared_cache_cannot_authorize_direct_reuse() {
        for mutate_source in [false, true] {
            let mut screen = test_screen_with_scorecard(3, 20);
            screen.lines = (0..12)
                .map(|index| {
                    Line::from_text(
                        &format!("{index:02} 界👩‍💻e\u{301} אב abcdefghijklmnop"),
                        &CellAttributes::blank(),
                        1,
                        None,
                    )
                })
                .collect();
            let cursor = test_cursor(0, 2, 1);
            let size = test_size(3, 7, 96);
            let mut prepared = screen.capture_reflow_preparation(size, cursor).unwrap();
            assert!(prepared.prepare(|| false));
            assert_eq!(
                prepared
                    .snapshot
                    .rewrap_cache
                    .as_ref()
                    .unwrap()
                    .source_signature,
                None
            );
            // Exercise an unsigned cache reaching the generic lookup without
            // the exact-source publication guard, including a same-seqno edit.
            screen.rewrap_cache = prepared.snapshot.rewrap_cache.clone();
            if mutate_source {
                screen.lines[0].set_cell(0, Cell::new('Z', CellAttributes::blank()), 1);
            }
            let mut expected = screen.clone();
            expected.rewrap_cache = None;
            let source_signature = screen.compute_layout_signature();
            REFLOW_SOURCE_SIGNATURE_SCANS.with(|count| count.set(0));
            let (_, _, logical_hit, wrapped_hit, _) = screen
                .logical_wraps_for_resize(size.cols, 2, true, &|| false)
                .unwrap();
            assert!(!logical_hit, "None must never match a source signature");
            assert!(!wrapped_hit, "unsigned prepared rows cannot be reused");
            assert_eq!(
                REFLOW_SOURCE_SIGNATURE_SCANS.with(|count| count.get()),
                1,
                "direct reconstruction must compute a real source signature"
            );
            assert_eq!(
                screen.rewrap_cache.as_ref().unwrap().source_signature,
                Some(source_signature)
            );

            REFLOW_SOURCE_SIGNATURE_SCANS.with(|count| count.set(0));
            let (_, _, logical_hit, wrapped_hit, _) = screen
                .logical_wraps_for_resize(size.cols, 2, true, &|| false)
                .unwrap();
            assert!(
                logical_hit && wrapped_hit,
                "signed direct caches remain reusable"
            );
            assert_eq!(
                REFLOW_SOURCE_SIGNATURE_SCANS.with(|count| count.get()),
                1,
                "a warm hit still validates the complete current source"
            );
            let actual_cursor = screen.resize(size, cursor, 2, false);
            let expected_cursor = expected.resize(size, cursor, 2, false);
            assert_eq!(actual_cursor, expected_cursor);
            assert_eq!(screen.lines, expected.lines);
            assert_eq!(screen.physical_cols, expected.physical_cols);
            assert_eq!(screen.physical_rows, expected.physical_rows);
            assert_eq!(
                screen.stable_row_index_offset,
                expected.stable_row_index_offset
            );
            assert_eq!(
                screen.last_resize_wrap_scorecard,
                expected.last_resize_wrap_scorecard
            );
            assert_eq!(
                screen.last_resize_wrap_gate_payload,
                expected.last_resize_wrap_gate_payload
            );
        }
    }

    #[test]
    fn prepared_reflow_reuses_exact_rows_after_cursor_only_parser_activity() {
        for (action, reuse) in [
            (b"\x1b[?25l".as_slice(), true),
            (b"\x1b[5 q".as_slice(), true),
            // CR deliberately publishes the row dirty; retain that exact
            // source fence even though its printable contents are unchanged.
            (b"\r".as_slice(), false),
            (b"\x1b[3G".as_slice(), true),
        ] {
            let config: Arc<dyn TerminalConfiguration> = Arc::new(TestTermConfig::default());
            let mut term = crate::Terminal::new(
                test_size(4, 20, 96),
                config,
                "prepared-cursor",
                "test",
                Box::new(Vec::<u8>::new()),
            );
            term.advance_bytes("界e\u{301} abcdefghijklmnopqrstuvwxyz".as_bytes());
            let source_cursor = term.cursor_pos();
            let size = test_size(4, 7, 96);
            let mut prepared = term
                .screen()
                .capture_reflow_preparation(size, source_cursor)
                .unwrap();
            assert!(prepared.prepare(|| false));
            assert!(prepared.source_logical_cursor.unwrap().1 > source_cursor.x);
            term.advance_bytes(action);
            let cursor = term.cursor_pos();
            assert_ne!(cursor, source_cursor, "parser action {action:?}");
            assert_eq!(cursor.y, source_cursor.y);
            let seqno = term.current_seqno().checked_add(1).unwrap();
            let mut synchronous = term.screen().clone();
            let expected_cursor = synchronous.resize(size, cursor, seqno, false);
            REFLOW_SOURCE_SIGNATURE_SCANS.with(|count| count.set(0));
            let actual_cursor = term.screen_mut().resize_with_prepared_reflow(
                size,
                cursor,
                seqno,
                false,
                Some(&mut prepared),
            );
            assert_eq!(prepared.was_applied(), reuse, "parser action {:?}", action);
            let scans = REFLOW_SOURCE_SIGNATURE_SCANS.with(|count| count.get());
            if reuse {
                assert_eq!(scans, 0);
            } else {
                assert!(scans > 0, "CR must retain fresh-source fallback");
            }
            assert_eq!(actual_cursor, expected_cursor, "parser action {action:?}");
            assert_eq!(
                term.screen().lines,
                synchronous.lines,
                "parser action {action:?}"
            );
            assert_eq!(actual_cursor.shape, cursor.shape);
            assert_eq!(actual_cursor.visibility, cursor.visibility);
        }
    }

    #[test]
    fn prepared_reflow_rejects_backwards_cursor_sequence_and_offset_overflow() {
        let mut screen = test_screen(3, 8, 96);
        screen.lines[0] = Line::from_text("abcdefgh", &CellAttributes::blank(), 1, None);
        screen.lines[0].set_last_cell_was_wrapped(true, 1);
        screen.lines[1] = Line::from_text("ijkl", &CellAttributes::blank(), 1, None);
        let cursor = test_cursor(2, 1, 2);
        let size = test_size(3, 4, 96);
        let mut prepared = screen.capture_reflow_preparation(size, cursor).unwrap();
        assert!(prepared.prepare(|| false));
        assert!(screen.matches_reflow_preparation(&prepared, size, cursor));
        let backwards = CursorPosition { seqno: 1, ..cursor };
        assert!(prepared.logical_cursor_for(backwards).is_none());
        assert!(!screen.matches_reflow_preparation(&prepared, size, backwards));
        let overflow = CursorPosition {
            x: usize::MAX,
            ..cursor
        };
        assert!(prepared.logical_cursor_for(overflow).is_none());
        assert!(!screen.matches_reflow_preparation(&prepared, size, overflow));
    }

    #[test]
    fn prepared_reflow_rejects_changed_source_and_policy() {
        for mutation in 0..8 {
            let mut screen = test_screen(3, 8, 96);
            let attrs = CellAttributes::blank();
            screen.lines = VecDeque::from(vec![
                Line::from_text("abcdefghijkl", &attrs, 1, None),
                Line::from_text("界e\u{301}z", &attrs, 1, None),
                Line::new(1),
            ]);
            let mut cursor = test_cursor(0, 2, 1);
            let size = test_size(3, 4, 96);
            let mut prepared = screen.capture_reflow_preparation(size, cursor).unwrap();
            assert!(prepared.prepare(|| false));
            match mutation {
                // Deliberately retain the same seqno: content validation must
                // not rely on every mutable caller incrementing it.
                0 => {
                    screen.lines[0].set_cell(0, Cell::new('Z', attrs.clone()), 1);
                }
                1 => {
                    screen.lines[0].set_last_cell_was_wrapped(true, 1);
                }
                2 => {
                    screen.lines.push_front(Line::new(1));
                }
                3 => {
                    screen.stable_row_index_offset += 1;
                }
                4 => {
                    screen.resize_wrap_policy.kp_cost_model.lookahead_limit = 1;
                }
                5 => {
                    cursor.y = 1;
                }
                6 => {
                    screen.dpi = 144;
                }
                7 => {
                    screen.lines[0].set_cell(0, Cell::new('界', attrs.clone()), 1);
                }
                _ => unreachable!(),
            }
            assert!(!screen.matches_reflow_preparation(&prepared, size, cursor));
            let mut synchronous = screen.clone();
            let expected = synchronous.resize(size, cursor, 2, false);
            let actual =
                screen.resize_with_prepared_reflow(size, cursor, 2, false, Some(&mut prepared));
            assert!(prepared.ready, "stale work must not be installed");
            assert_eq!(actual, expected, "mutation {mutation}");
            assert_eq!(screen.lines, synchronous.lines, "mutation {mutation}");
        }
    }

    #[test]
    fn stale_preparation_reuses_unchanged_line_wraps_without_installing_layout() {
        let mut screen = test_screen(3, 80, 96);
        let attrs = CellAttributes::blank();
        screen.lines = (0..64)
            .map(|index| {
                Line::from_text(
                    &format!("{index:04} abc界e\u{301}defghijklmnop"),
                    &attrs,
                    1,
                    None,
                )
            })
            .collect();
        let cursor = test_cursor(0, 2, 1);
        let size = test_size(3, 8, 96);
        let mut prepared = screen.capture_reflow_preparation(size, cursor).unwrap();
        assert!(prepared.prepare(|| false));
        // Same-seqno mutation deliberately rules out seqno-only validation.
        screen
            .lines
            .back_mut()
            .unwrap()
            .set_cell(0, Cell::new('Z', attrs), 1);
        assert!(!screen.matches_reflow_preparation(&prepared, size, cursor));
        let mut synchronous = screen.clone();
        let expected = synchronous.resize(size, cursor, 2, false);
        let actual =
            screen.resize_with_prepared_reflow(size, cursor, 2, false, Some(&mut prepared));
        assert!(!prepared.was_applied(), "stale layout must remain rejected");
        assert_eq!(actual, expected);
        assert_eq!(screen.lines, synchronous.lines);
        assert_eq!(
            screen.last_resize_wrap_scorecard,
            synchronous.last_resize_wrap_scorecard
        );
        assert_eq!(synchronous.rewrap_line_cache_hits, 0);
        assert_eq!(screen.rewrap_line_cache_hits, 63);
    }

    #[test]
    fn prepared_reflow_cancels_between_batches_without_touching_live_screen() {
        let mut screen = test_screen(3, 8, 96);
        let attrs = CellAttributes::blank();
        screen.lines = (0..256)
            .map(|index| {
                Line::from_text(&format!("{index:04} abc界defghijklmnop"), &attrs, 1, None)
            })
            .collect();
        let original = screen.lines.clone();
        let cursor = test_cursor(0, 2, 1);
        let size = test_size(3, 4, 96);
        let mut prepared = screen.capture_reflow_preparation(size, cursor).unwrap();
        let checks = std::cell::Cell::new(0usize);
        assert!(!prepared.prepare(|| {
            checks.set(checks.get() + 1);
            checks.get() == 4
        }));
        assert_eq!(
            checks.get(),
            4,
            "cancellation must be observed before completing all batches"
        );
        assert!(!prepared.ready);
        assert_eq!(screen.lines, original);
        assert_eq!(screen.physical_cols, 8);
    }

    #[test]
    fn prepared_reflow_preserves_live_cold_work_counters() {
        let mut screen = test_screen(3, 8, 96);
        let attrs = CellAttributes::blank();
        screen.lines = (0..256)
            .map(|_| Line::from_text("abcdefghijklmnop", &attrs, 1, None))
            .collect();
        let cursor = test_cursor(0, 2, 1);
        let size = test_size(3, 4, 96);
        let mut prepared = screen.capture_reflow_preparation(size, cursor).unwrap();
        assert!(prepared.prepare(|| false));
        let completed = prepared
            .snapshot
            .cold_scrollback_worker
            .completed_lines_total;
        assert!(completed > 0);
        screen.cold_scrollback_worker.completed_lines_total = 77;
        screen.resize_with_prepared_reflow(size, cursor, 2, false, Some(&mut prepared));
        assert!(prepared.was_applied());
        assert_eq!(
            screen.cold_scrollback_worker.completed_lines_total,
            77 + completed
        );
    }

    #[test]
    fn prepared_reflow_accepts_no_match_scan_and_rechecks_new_rules() {
        use frankenterm_surface::hyperlink::Rule;
        let mut screen = test_screen(3, 6, 96);
        let attrs = CellAttributes::blank();
        screen.lines = VecDeque::from(vec![
            Line::from_text("abcdef", &attrs, 1, None),
            Line::from_text("xy", &attrs, 1, None),
            Line::new(1),
        ]);
        let cursor = test_cursor(0, 1, 1);
        let size = test_size(3, 4, 96);
        let mut prepared = screen.capture_reflow_preparation(size, cursor).unwrap();
        let old_rules = vec![Rule::new("never-matches", "$0").unwrap()];
        for line in &mut screen.lines {
            line.scan_and_create_hyperlinks(&old_rules);
        }
        assert!(prepared.prepare(|| false));
        assert!(screen.matches_reflow_preparation(&prepared, size, cursor));
        screen.resize_with_prepared_reflow(size, cursor, 2, false, Some(&mut prepared));
        assert!(prepared.was_applied());
        let new_rules = vec![Rule::new("xy", "https://new.example/$0").unwrap()];
        for line in &mut screen.lines {
            line.scan_and_create_hyperlinks(&new_rules);
        }
        assert!(screen
            .lines
            .iter()
            .flat_map(Line::visible_cells)
            .any(|cell| {
                cell.attrs()
                    .hyperlink()
                    .is_some_and(|link| link.uri() == "https://new.example/xy")
            }));
    }

    #[test]
    fn published_target_witness_matches_fresh_reflow_and_complete_line_state() {
        let mut attrs = CellAttributes::blank();
        attrs.set_hyperlink(Some(Arc::new(
            frankenterm_escape_parser::hyperlink::Hyperlink::new_with_id(
                "https://example.invalid/reflow",
                "stable-link",
            ),
        )));
        for text in [
            "abcdefghijklmnopqrstuvwxyz0123456789ABCDE".to_string(),
            "界👩‍💻e\u{301}אב".repeat(5),
        ] {
            let mut screen = test_screen(3, 20, 96);
            screen.lines = (0..3)
                .map(|_| Line::from_text(&text, &attrs, 1, None))
                .collect();
            let mut cursor = test_cursor(0, 0, 1);
            let mut reused = 0;
            for (index, cols) in [7, 19, 11, 23, 7, 19, 11, 23].iter().copied().enumerate() {
                let key = WrapCacheKey {
                    physical_cols: cols,
                    dpi: 96,
                };
                if screen
                    .rewrap_cache
                    .as_ref()
                    .and_then(|cache| cache.wrapped_by_key.get(&key))
                    .is_some()
                {
                    reused += 1;
                }
                let mut fresh = screen.clone();
                fresh.rewrap_cache = None;
                fresh.clear_rewrap_line_cache();
                let seqno = index + 2;
                let expected = fresh.resize(test_size(3, cols, 96), cursor, seqno, false);
                cursor = screen.resize(test_size(3, cols, 96), cursor, seqno, false);
                assert_eq!(cursor, expected);
                // Line equality includes cells, attributes, wrap bits and seqno.
                assert_eq!(screen.lines, fresh.lines);
                assert_eq!(
                    screen.stable_row_index_offset,
                    fresh.stable_row_index_offset
                );
                let rebuilt = screen.rebuild_logical_lines_from_physical(seqno);
                let logical_text: Vec<_> = rebuilt
                    .iter()
                    .map(|line| line.line(&screen.lines).as_str().into_owned())
                    .filter(|line| !line.is_empty())
                    .collect();
                assert_eq!(
                    logical_text,
                    vec![text.clone(); 3],
                    "width {cols} must retain the original three logical records"
                );
                let cache = screen.rewrap_cache.as_ref().unwrap();
                assert_eq!(cache.source_signature, None);
                assert!(cache.matches_published_target(&screen.lines));
                assert!(Arc::ptr_eq(
                    cache.published_target.as_ref().unwrap(),
                    &cache.wrapped_by_key[&key].lines,
                ));
            }
            assert!(reused >= 4, "must exercise repeated target layouts");
        }
    }

    #[test]
    fn published_target_preparation_skips_history_hashes_for_cached_and_unseen_widths() {
        let mut screen = test_screen_with_scorecard(3, 40);
        screen.lines = (0..16)
            .map(|index| {
                Line::from_text(
                    &format!("{index:02} 界👩‍💻e\u{301} אב abcdefghijklmnopqrstuvwxyz"),
                    &CellAttributes::blank(),
                    1,
                    None,
                )
            })
            .collect();
        let mut cursor = test_cursor(2, 2, 1);
        for (index, cols) in [7, 19, 11, 7, 19, 5].iter().copied().enumerate() {
            let size = test_size(3, cols, 96);
            let mut fresh = screen.clone();
            fresh.rewrap_cache = None;
            fresh.clear_rewrap_line_cache();
            let expected = fresh.resize(size, cursor, index + 2, false);
            let mut prepared = screen.capture_reflow_preparation(size, cursor).unwrap();
            REFLOW_SOURCE_SIGNATURE_SCANS.with(|count| count.set(0));
            REFLOW_FULL_LAYOUT_SIGNATURE_SCANS.with(|count| count.set(0));
            REFLOW_LAYOUT_LINE_HASHES.with(|count| count.set(0));
            assert!(prepared.prepare(|| false));
            assert_eq!(REFLOW_SOURCE_SIGNATURE_SCANS.with(|count| count.get()), 0);
            assert_eq!(
                REFLOW_FULL_LAYOUT_SIGNATURE_SCANS.with(|count| count.get()),
                0
            );
            assert_eq!(
                REFLOW_LAYOUT_LINE_HASHES.with(|count| count.get()),
                0,
                "preparation at width {cols} must not hash source or target physical rows"
            );
            cursor = screen.resize_with_prepared_reflow(
                size,
                cursor,
                index + 2,
                false,
                Some(&mut prepared),
            );
            assert!(prepared.was_applied());
            assert_eq!(cursor, expected);
            assert_eq!(screen.lines, fresh.lines);
            assert_eq!(
                screen.last_resize_wrap_scorecard,
                fresh.last_resize_wrap_scorecard
            );
            assert!(screen
                .rewrap_cache
                .as_ref()
                .unwrap()
                .matches_published_target(&screen.lines));
        }

        let cache = Arc::clone(screen.rewrap_cache.as_ref().unwrap());
        let rows = screen.lines.clone();
        let mut cancelled = screen
            .capture_reflow_preparation(test_size(3, 13, 96), cursor)
            .unwrap();
        let checks = std::cell::Cell::new(0);
        assert!(!cancelled.prepare(|| {
            checks.set(checks.get() + 1);
            checks.get() == 4
        }));
        assert!(!cancelled.ready && !cancelled.was_applied());
        assert!(Arc::ptr_eq(&cache, screen.rewrap_cache.as_ref().unwrap()));
        assert!(cache.matches_published_target(&screen.lines));
        assert_eq!(screen.lines, rows);
    }

    #[test]
    fn published_target_rejects_same_seqno_cell_attribute_wrap_order_and_length_changes() {
        let mut seed = test_screen(3, 40, 96);
        seed.lines = (0..6)
            .map(|index| {
                Line::from_text(
                    &format!("{index:02} abcdefghijklmnopqrstuvwxyz"),
                    &CellAttributes::blank(),
                    1,
                    None,
                )
            })
            .collect();
        let cursor = seed.resize(test_size(3, 7, 96), test_cursor(0, 2, 1), 2, false);
        assert!(seed
            .rewrap_cache
            .as_ref()
            .unwrap()
            .matches_published_target(&seed.lines));
        for mutation in 0..6 {
            let mut screen = seed.clone();
            match mutation {
                0 => {
                    screen.lines[0].set_cell(0, Cell::new('Z', CellAttributes::blank()), 2);
                }
                1 => {
                    let mut attrs = CellAttributes::blank();
                    attrs.set_italic(true);
                    screen.lines[0].set_cell(0, Cell::new('0', attrs), 2);
                }
                2 => screen.lines[0].set_last_cell_was_wrapped(false, 2),
                3 => screen.lines.swap(0, 1),
                4 => {
                    screen.lines.pop_back();
                }
                _ => screen.lines.push_back(Line::from_text(
                    "new row",
                    &CellAttributes::blank(),
                    2,
                    None,
                )),
            }
            assert!(screen.lines.iter().all(|line| line.current_seqno() == 2));
            assert!(!screen
                .rewrap_cache
                .as_ref()
                .unwrap()
                .matches_published_target(&screen.lines));
            let mut fresh = screen.clone();
            fresh.rewrap_cache = None;
            fresh.clear_rewrap_line_cache();
            let expected = fresh.resize(test_size(3, 11, 96), cursor, 3, false);
            let actual = screen.resize(test_size(3, 11, 96), cursor, 3, false);
            assert_eq!(actual, expected, "mutation {mutation}");
            assert_eq!(screen.lines, fresh.lines, "mutation {mutation}");
            assert_eq!(
                screen.rewrap_cache.as_ref().unwrap().wrapped_by_key.len(),
                1
            );
        }
    }

    #[test]
    fn reflow_pruning_discards_published_target_authority() {
        let mut screen = test_screen_with_config(
            1,
            12,
            96,
            TestTermConfig {
                scrollback: 0,
                ..Default::default()
            },
        );
        screen.lines = VecDeque::from([
            Line::from_text("abcdefghijkl", &CellAttributes::blank(), 1, None),
            Line::new(1),
            Line::new(1),
        ]);
        let cursor = screen.rewrap_lines(
            3,
            1,
            (0, 0),
            2,
            None,
            &mut SelectionAnchorRegistry::default(),
        );
        assert_eq!(cursor, (0, 0));
        assert_eq!(screen.lines.len(), 4, "two trailing blank rows were pruned");
        assert!(
            screen.rewrap_cache.is_none(),
            "pruned targets cannot authorize full logical reuse"
        );
        assert_eq!(screen.lines.back().unwrap().as_str(), "jkl");
    }

    #[test]
    fn image_wraps_keep_fresh_signatures_after_shared_content_mutation() {
        use frankenterm_cell::image::{ImageCell, ImageData, ImageDataType, TextureCoordinate};

        let image = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
            1,
            1,
            vec![1, 2, 3, 4],
        )));
        let mut attrs = CellAttributes::blank();
        attrs.set_image(Box::new(ImageCell::new(
            TextureCoordinate::new_f32(0.0, 0.0),
            TextureCoordinate::new_f32(1.0, 1.0),
            Arc::clone(&image),
        )));
        let mut screen = test_screen(3, 10, 96);
        screen.lines = (0..3)
            .map(|_| Line::from_text("abcdefghijklmnopqrstuvwx", &attrs, 1, None))
            .collect();
        let mut cursor = test_cursor(0, 0, 1);
        let mut prepared = screen
            .capture_reflow_preparation(test_size(3, 8, 96), cursor)
            .unwrap();
        assert!(
            !prepared.prepare(|| false),
            "mutable images must remain synchronous"
        );
        assert!(!screen.lines[0].is_same_reflow_source(&screen.lines[0].clone()));
        for (seqno, cols) in [(2, 8), (3, 5), (4, 8)] {
            cursor = screen.resize(test_size(3, cols, 96), cursor, seqno, false);
            let cache = screen.rewrap_cache.as_ref().unwrap();
            assert_eq!(
                cache.source_signature,
                Some(screen.compute_layout_signature())
            );
            assert!(cache
                .wrapped_by_key
                .values()
                .all(|wrapped| wrapped.has_images));
            assert!(cache.published_target.is_none());
        }
        let before = screen.compute_layout_signature();
        {
            let mut payload = image.data_mut();
            let ImageDataType::Rgba8 { data, .. } = &mut *payload else {
                panic!("expected RGBA payload");
            };
            data[0] = 9;
        }
        assert_ne!(screen.compute_layout_signature(), before);
        let _ = screen.resize(test_size(3, 5, 96), cursor, 5, false);
        let cache = screen.rewrap_cache.as_ref().unwrap();
        assert_eq!(
            cache.source_signature,
            Some(screen.compute_layout_signature())
        );
        assert_eq!(
            cache.wrapped_by_key.len(),
            1,
            "changed image invalidates old wraps"
        );
        assert!(cache
            .wrapped_by_key
            .values()
            .all(|wrapped| wrapped.has_images));
        assert!(cache.published_target.is_none());
    }

    #[test]
    fn rewrap_cache_rebuilds_after_content_mutation() {
        let mut screen = test_screen(3, 4, 96);
        let attrs = CellAttributes::blank();

        screen.lines = VecDeque::from(vec![
            Line::from_text_with_wrapped_last_col("abcd", &attrs, 0),
            Line::from_text("ef", &attrs, 0, None),
            Line::new(0),
        ]);

        let cursor = test_cursor(1, 1, 1);
        let cursor = screen.resize(test_size(3, 3, 96), cursor, 1, false);
        let cursor = screen.resize(test_size(3, 4, 96), cursor, 2, false);
        let cache = screen.rewrap_cache.as_ref().expect("rewrap cache to exist");
        assert_eq!(cache.wrapped_by_key.len(), 2);

        // Use a seqno that is already in play to verify that
        // content-shape based invalidation catches this mutation.
        screen.set_cell(0, 0, &Cell::new('Z', attrs.clone()), 2);

        let _cursor = screen.resize(test_size(3, 3, 96), cursor, 3, false);
        let cache = screen.rewrap_cache.as_ref().expect("rewrap cache to exist");
        assert_eq!(
            cache.wrapped_by_key.len(),
            1,
            "content mutation should rebuild wraps from canonical lines"
        );
        assert!(
            cache.wrapped_by_key.contains_key(&WrapCacheKey {
                physical_cols: 3,
                dpi: 96
            }),
            "rebuilt cache should contain only active resize key"
        );
    }

    #[test]
    fn rewrap_line_cache_reuses_unchanged_lines_after_partial_mutation() {
        let attrs = CellAttributes::blank();
        let mut screen = test_screen(8, 6, 96);
        screen.lines = VecDeque::from(vec![
            Line::from_text("abcdef", &attrs, 0, None),
            Line::from_text("mnopqr", &attrs, 0, None),
            Line::new(0),
        ]);

        let cursor = test_cursor(0, 1, 1);
        let cursor = screen.resize(test_size(8, 3, 96), cursor, 1, false);
        let _cursor = screen.resize(test_size(8, 4, 96), cursor, 2, false);
        assert!(
            !screen.rewrap_line_cache.is_empty(),
            "initial resizes should populate per-line wrap cache"
        );

        screen.set_cell(0, 0, &Cell::new('Z', attrs.clone()), 3);

        let mut cached_path = screen.clone();
        let mut direct_path = screen.clone();
        direct_path.clear_rewrap_line_cache();
        cached_path.rewrap_line_cache_hits = 0;

        let cached_cursor = cached_path.resize(test_size(8, 3, 96), cursor, 4, false);
        let direct_cursor = direct_path.resize(test_size(8, 3, 96), cursor, 4, false);

        let cached_lines: Vec<_> = cached_path
            .lines
            .iter()
            .map(|line| {
                (
                    line.as_str().to_string(),
                    line.current_seqno(),
                    line.last_cell_was_wrapped(),
                )
            })
            .collect();
        let direct_lines: Vec<_> = direct_path
            .lines
            .iter()
            .map(|line| {
                (
                    line.as_str().to_string(),
                    line.current_seqno(),
                    line.last_cell_was_wrapped(),
                )
            })
            .collect();

        assert_eq!(cached_cursor, direct_cursor);
        assert_eq!(cached_lines, direct_lines);
        assert!(
            cached_path.rewrap_line_cache_hits > 0,
            "unchanged logical lines should reuse cached wrap results"
        );
    }

    #[test]
    fn rewrap_shared_lines_materializes_same_as_owned_lines_across_resize() {
        let attrs = CellAttributes::blank();
        let mut seed = test_screen(5, 6, 96);
        seed.lines = VecDeque::from(vec![Line::from_text("abcdef", &attrs, 0, None)]);

        let mut direct_path = seed.clone();
        let mut shared_path = seed.clone();
        let target_cols = 3;
        let resize_seqno = 7;

        let key = WrapLineCacheKey::new(
            &shared_path.lines[0],
            target_cols,
            shared_path.dpi,
            shared_path.resize_wrap_policy,
        );
        let mut width_prefix_scratch = LineWrapWidthPrefixScratch::default();
        let (wrapped_lines, scorecard) = Screen::wrap_single_logical_line_for_resize(
            shared_path.lines[0].clone(),
            target_cols,
            resize_seqno,
            shared_path.resize_wrap_policy,
            &mut width_prefix_scratch,
        );
        shared_path.insert_rewrap_line_cache(
            key,
            CachedWrappedLine {
                lines: Arc::from(wrapped_lines),
                scorecard,
            },
        );
        shared_path.rewrap_line_cache_hits = 0;

        let cursor = test_cursor(0, 0, 1);
        let direct_cursor =
            direct_path.resize(test_size(5, target_cols, 96), cursor, resize_seqno, false);
        let shared_cursor =
            shared_path.resize(test_size(5, target_cols, 96), cursor, resize_seqno, false);

        let materialized_snapshot = |screen: &Screen| {
            screen
                .lines
                .iter()
                .map(|line| {
                    (
                        line.as_str().to_string(),
                        line.last_cell_was_wrapped(),
                        line.current_seqno(),
                    )
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(shared_cursor, direct_cursor);
        assert!(
            shared_path.rewrap_line_cache_hits > 0,
            "shared path must exercise RewrapScratch::SharedLines"
        );
        assert_eq!(
            materialized_snapshot(&shared_path),
            materialized_snapshot(&direct_path),
            "SharedLines(Arc) and Lines materialization must be byte-identical"
        );
    }

    #[test]
    fn resize_reflow_reuses_scratch_buffers() {
        let mut screen = test_screen(4, 6, 96);
        let attrs = CellAttributes::blank();

        screen.lines = VecDeque::from(vec![
            Line::from_text_with_wrapped_last_col("abcdefghijkl", &attrs, 0),
            Line::from_text("mnop", &attrs, 0, None),
            Line::from_text_with_wrapped_last_col("qrstuv", &attrs, 0),
            Line::from_text("wxyz", &attrs, 0, None),
            Line::new(0),
        ]);

        let cursor = test_cursor(2, 3, 1);
        let cursor = screen.resize(test_size(4, 4, 96), cursor, 1, false);
        let first_slots_capacity = screen.rewrap_scratch_slots.capacity();
        let first_prefix_capacity = screen.rewrap_row_prefix_scratch.capacity();
        let first_width_prefix_capacity = screen.rewrap_width_prefix_scratch.capacity();
        let used_slots = screen.rewrap_row_prefix_scratch.len().saturating_sub(1);
        assert!(
            screen.rewrap_scratch_slots[..used_slots]
                .iter()
                .all(Option::is_none),
            "scratch slots should be emptied after materializing wrapped lines"
        );

        let _ = screen.resize(test_size(4, 5, 96), cursor, 2, false);
        assert!(
            screen.rewrap_scratch_slots.capacity() >= first_slots_capacity,
            "rewrap slot buffer should be reused across resize cycles"
        );
        assert!(
            screen.rewrap_row_prefix_scratch.capacity() >= first_prefix_capacity,
            "row-prefix scratch buffer should be reused across resize cycles"
        );
        assert!(
            screen.rewrap_width_prefix_scratch.capacity() >= first_width_prefix_capacity,
            "width-prefix scratch buffer should be reused across resize cycles"
        );
    }

    #[test]
    fn resize_reflow_scratch_move_matches_cached_wrap_output() {
        let attrs = CellAttributes::blank();
        let seed_lines = VecDeque::from(vec![
            Line::from_text_with_wrapped_last_col("abcdefghijkl", &attrs, 0),
            Line::from_text("mnop", &attrs, 0, None),
            Line::from_text_with_wrapped_last_col("qrstuv", &attrs, 0),
            Line::from_text("wxyz", &attrs, 0, None),
            Line::new(0),
        ]);
        let cursor = test_cursor(2, 3, 1);

        let mut cached = test_screen(4, 6, 96);
        cached.lines = seed_lines;
        let cursor = cached.resize(test_size(4, 4, 96), cursor, 1, false);
        let cursor = cached.resize(test_size(4, 6, 96), cursor, 2, false);

        let mut direct = test_screen(cached.physical_rows, cached.physical_cols, cached.dpi);
        direct.lines = cached.lines.clone();
        direct.rewrap_cache = None;

        let cached_cursor = cached.resize(test_size(4, 4, 96), cursor, 3, false);
        let direct_cursor = direct.resize(test_size(4, 4, 96), cursor, 3, false);

        let cached_lines: Vec<_> = cached
            .lines
            .iter()
            .map(|line| {
                (
                    line.as_str().to_string(),
                    line.current_seqno(),
                    line.last_cell_was_wrapped(),
                )
            })
            .collect();
        let direct_lines: Vec<_> = direct
            .lines
            .iter()
            .map(|line| {
                (
                    line.as_str().to_string(),
                    line.current_seqno(),
                    line.last_cell_was_wrapped(),
                )
            })
            .collect();

        assert_eq!(cached_cursor, direct_cursor);
        assert_eq!(cached_lines, direct_lines);
    }

    #[test]
    fn prepared_reflow_reserves_all_wrapped_rows_before_cached_materialization() {
        let mut screen = test_screen(4, 8, 96);
        screen.lines = VecDeque::with_capacity(64);
        for _ in 0..48 {
            screen
                .lines
                .push_back(Line::from_text("abcde", &CellAttributes::blank(), 1, None));
        }
        let cursor = test_cursor(0, 3, 1);
        let size = test_size(4, 4, 96);
        let old_capacity = screen.lines.capacity();
        let mut prepared = screen.capture_reflow_preparation(size, cursor).unwrap();
        assert!(prepared.prepare(|| false));
        let mut expected = screen.clone();
        let expected_cursor = expected.resize(size, cursor, 2, false);
        let target_rows = expected.lines.len();
        assert_eq!(
            target_rows, 96,
            "each five-column hard line wraps into two rows"
        );
        assert!(target_rows > old_capacity && target_rows - old_capacity < old_capacity);
        REFLOW_CACHED_RESERVED_CAPACITY.with(|capacity| capacity.set(0));
        let actual_cursor =
            screen.resize_with_prepared_reflow(size, cursor, 2, false, Some(&mut prepared));
        let reserved = REFLOW_CACHED_RESERVED_CAPACITY.with(|capacity| capacity.get());
        assert!(prepared.was_applied());
        assert!(
            reserved >= target_rows,
            "reserve must precede every wrapped-row push"
        );
        assert_eq!(
            screen.lines.capacity(),
            reserved,
            "materialization must not grow the deque"
        );
        assert_eq!(actual_cursor, expected_cursor);
        assert_eq!(screen.lines, expected.lines);
    }

    #[test]
    fn resize_reserves_configured_capacity_relative_to_live_length() {
        let mut screen = test_screen(4, 8, 96);
        screen.lines = VecDeque::with_capacity(24);
        for _ in 0..4 {
            screen
                .lines
                .push_back(Line::from_text("text", &CellAttributes::blank(), 1, None));
        }
        let requested = 5 + screen.hot_scrollback_size();
        assert!(requested > screen.lines.capacity());
        assert!(
            screen.lines.len() + requested - screen.lines.capacity() <= screen.lines.capacity()
        );
        screen.resize(test_size(5, 8, 96), test_cursor(0, 3, 1), 2, false);
        assert!(screen.lines.capacity() >= requested);
        assert_eq!(screen.lines.len(), 5);
        for line in screen.lines.iter().take(4) {
            assert_eq!(line.as_str(), "text");
        }
    }

    #[test]
    fn rebuild_logical_lines_with_signature_and_ranges_matches_standalone_scans() {
        let attrs = CellAttributes::blank();
        let mut screen = test_screen(4, 6, 96);
        screen.lines = VecDeque::from(vec![
            Line::from_text_with_wrapped_last_col("abcdef", &attrs, 0),
            Line::from_text("ghij", &attrs, 0, None),
            Line::from_text("kl", &attrs, 0, None),
        ]);

        let expected_signature = screen.compute_layout_signature();
        let expected_ranges = Screen::logical_line_physical_ranges(&screen.lines);
        let (logical_lines, fused_signature, fused_ranges) =
            screen.rebuild_logical_lines_from_physical_with_signature_and_ranges(2);

        assert_eq!(fused_signature, expected_signature);
        assert_eq!(fused_ranges, expected_ranges);
        assert_eq!(logical_lines.len(), 2);
        let LogicalLineForResize::PhysicalRange { logical, .. } = &logical_lines[0] else {
            panic!("first-use range must defer its token construction");
        };
        assert!(logical.get().is_none());
        assert_eq!(logical_lines[0].line(&screen.lines).as_str(), "abcdefghij");
        assert!(logical.get().is_some());
        assert!(matches!(
            logical_lines[1],
            LogicalLineForResize::PhysicalLine(2)
        ));
    }

    #[test]
    fn resize_reflow_moves_unwrapped_physical_lines_without_clone() {
        let attrs = CellAttributes::blank();
        let mut screen = test_screen(4, 6, 96);
        screen.lines = VecDeque::from(vec![
            Line::from_text("aa", &attrs, 0, None),
            Line::from_text("abcdef", &attrs, 0, None),
            Line::from_text("zz", &attrs, 0, None),
            Line::new(0),
        ]);

        let logical_lines = screen.rebuild_logical_lines_from_physical(2);
        assert!(matches!(
            logical_lines[0],
            LogicalLineForResize::PhysicalLine(0)
        ));
        assert!(matches!(
            logical_lines[1],
            LogicalLineForResize::PhysicalLine(1)
        ));
        assert!(matches!(
            logical_lines[2],
            LogicalLineForResize::PhysicalLine(2)
        ));

        screen.wrap_logical_lines_for_resize(&logical_lines, 4, 2, None, &|| false);
        assert!(matches!(
            screen.rewrap_scratch_slots[0],
            Some(RewrapScratch::PhysicalLine(0))
        ));
        assert!(matches!(
            screen.rewrap_scratch_slots[1],
            Some(RewrapScratch::Lines(_))
        ));
        assert!(matches!(
            screen.rewrap_scratch_slots[2],
            Some(RewrapScratch::PhysicalLine(2))
        ));

        screen.resize(test_size(4, 4, 96), test_cursor(0, 0, 1), 2, false);

        let moved_aa = screen
            .lines
            .iter()
            .find(|line| line.as_str() == "aa")
            .expect("aa line should survive resize");
        let moved_zz = screen
            .lines
            .iter()
            .find(|line| line.as_str() == "zz")
            .expect("zz line should survive resize");

        assert_eq!(moved_aa.current_seqno(), 2);
        assert_eq!(moved_zz.current_seqno(), 2);
    }

    #[test]
    fn viewport_reflow_plan_prioritizes_viewport_then_near_then_cold() {
        let mut screen = test_screen(4, 4, 96);
        let attrs = CellAttributes::blank();

        screen.lines = VecDeque::from(vec![
            Line::from_text("l0", &attrs, 0, None),
            Line::from_text("l1", &attrs, 0, None),
            Line::from_text_with_wrapped_last_col("l2a", &attrs, 0),
            Line::from_text("l2b", &attrs, 0, None),
            Line::from_text("l3", &attrs, 0, None),
            Line::from_text("l4", &attrs, 0, None),
            Line::from_text_with_wrapped_last_col("l5a", &attrs, 0),
            Line::from_text("l5b", &attrs, 0, None),
            Line::from_text("l6", &attrs, 0, None),
            Line::from_text("l7", &attrs, 0, None),
            Line::from_text("l8", &attrs, 0, None),
            Line::from_text("l9", &attrs, 0, None),
        ]);

        let logical_count = Screen::logical_line_physical_ranges(&screen.lines).len();
        assert_eq!(logical_count, 10);

        let plan = screen.build_viewport_reflow_plan_for_current_snapshot(logical_count);
        assert!(plan.covers_each_logical_line_once(logical_count));
        assert_eq!(plan.batches.len(), 3);
        assert_eq!(plan.batches[0].priority, ReflowBatchPriority::Viewport);
        assert_eq!(plan.batches[0].logical_range, 6..10);
        assert_eq!(plan.batches[1].priority, ReflowBatchPriority::NearViewport);
        assert_eq!(plan.batches[1].logical_range, 3..6);
        assert_eq!(
            plan.batches[2].priority,
            ReflowBatchPriority::ColdScrollback
        );
        assert_eq!(plan.batches[2].logical_range, 0..3);
    }

    #[test]
    fn viewport_first_reflow_records_ready_before_cold_scrollback_completion() {
        let mut screen = test_screen(3, 8, 96);
        let attrs = CellAttributes::blank();
        screen.lines = VecDeque::from(
            (0..12)
                .map(|idx| Line::from_text(&format!("line{idx:02}xx"), &attrs, 0, None))
                .collect::<Vec<_>>(),
        );
        let logical_count = Screen::logical_line_physical_ranges(&screen.lines).len();
        let plan = screen.build_viewport_reflow_plan_for_current_snapshot(logical_count);
        assert!(
            plan.batches
                .iter()
                .any(|batch| batch.priority == ReflowBatchPriority::ColdScrollback),
            "fixture must include cold scrollback work"
        );

        let started = Instant::now();
        let cursor = screen.resize(test_size(3, 4, 96), test_cursor(0, 2, 1), 2, false);
        let total_us = duration_micros_u64(started.elapsed());

        assert_eq!(cursor.seqno, 2);
        assert_eq!(screen.cold_scrollback_worker.active_intent(), None);
        assert!(
            screen.cold_scrollback_worker.completed_lines_total() > 0,
            "cold scrollback should still converge before resize returns"
        );
        assert!(
            screen.last_viewport_first_reflow_us() <= total_us,
            "viewport-first ready marker must precede final cold-scrollback convergence"
        );
    }

    #[test]
    fn duration_micros_u64_is_exact_for_small_and_saturates_at_max() {
        use std::time::Duration;

        // Exact for representable durations (no rounding, no panic).
        assert_eq!(duration_micros_u64(Duration::ZERO), 0);
        assert_eq!(duration_micros_u64(Duration::from_micros(1)), 1);
        assert_eq!(duration_micros_u64(Duration::from_micros(1234)), 1234);
        assert_eq!(duration_micros_u64(Duration::from_millis(5)), 5_000);
        assert_eq!(duration_micros_u64(Duration::from_secs(2)), 2_000_000);

        // Exact right up to the u64 ceiling (as_micros() == u64::MAX still fits).
        assert_eq!(
            duration_micros_u64(Duration::from_micros(u64::MAX)),
            u64::MAX
        );

        // Saturates (rather than panicking or wrapping) once micros exceed u64.
        let just_over = Duration::from_micros(u64::MAX) + Duration::from_micros(1);
        assert_eq!(duration_micros_u64(just_over), u64::MAX);
        assert_eq!(duration_micros_u64(Duration::MAX), u64::MAX);
    }

    #[test]
    fn viewport_reflow_metric_does_not_reuse_a_prior_resize_sample() {
        for size in [
            test_size(4, 8, 96),
            test_size(5, 8, 96),
            test_size(4, 8, 120),
            test_size(4, 12, 96),
        ] {
            let mut screen = test_screen(4, 8, 96);
            screen.last_viewport_first_reflow_us = u64::MAX;
            screen.resize(size, test_cursor(0, 3, 1), 2, false);
            assert_eq!(screen.last_viewport_first_reflow_us(), 0);
        }

        let mut screen = test_screen(4, 8, 96);
        let attrs = CellAttributes::blank();
        screen.lines = VecDeque::from(
            (0..4)
                .map(|_| Line::from_text("abcdefgh", &attrs, 0, None))
                .collect::<Vec<_>>(),
        );
        let _ = screen.logical_wraps_for_resize(4, 2, true, &|| false);
        let (_, _, _, wrap_cache_hit, _) = screen
            .logical_wraps_for_resize(4, 2, true, &|| false)
            .unwrap();
        assert!(wrap_cache_hit, "exercise an actual cached wrap");
        screen.last_viewport_first_reflow_us = u64::MAX;
        screen.resize(test_size(4, 4, 96), test_cursor(0, 3, 1), 2, false);
        assert_eq!(screen.last_viewport_first_reflow_us(), 0);
    }

    #[test]
    fn viewport_first_reflow_records_last_viewport_first_reflow_us() {
        let mut screen = test_screen(3, 8, 96);
        let attrs = CellAttributes::blank();
        screen.lines = VecDeque::from(
            (0..12)
                .map(|idx| Line::from_text(&format!("line{idx:02}xx"), &attrs, 0, None))
                .collect::<Vec<_>>(),
        );

        // Sentinel the field with a value the real recording — bounded by the
        // resize wall-clock window — can never legitimately produce, so the
        // post-resize check proves the reflow path overwrote it rather than
        // leaving it stale. (An untouched field would still read u64::MAX.)
        screen.last_viewport_first_reflow_us = u64::MAX;

        let started = Instant::now();
        let _cursor = screen.resize(test_size(3, 4, 96), test_cursor(0, 2, 1), 2, false);
        let total_us = duration_micros_u64(started.elapsed());

        let recorded = screen.last_viewport_first_reflow_us();
        assert_ne!(
            recorded,
            u64::MAX,
            "viewport reflow must record (overwrite) last_viewport_first_reflow_us"
        );
        assert!(
            recorded <= total_us,
            "recorded reflow micros ({recorded}) must be bounded by the resize window ({total_us})",
            recorded = recorded,
            total_us = total_us,
        );
    }

    #[test]
    fn viewport_first_reflow_is_isomorphic_to_full_scan() {
        let mut screen = test_screen(3, 8, 96);
        let attrs = CellAttributes::blank();
        screen.lines = VecDeque::from(vec![
            Line::from_text("cold0000", &attrs, 0, None),
            Line::from_text("cold1111", &attrs, 0, None),
            Line::from_text("near2222", &attrs, 0, None),
            Line::from_text("near3333", &attrs, 0, None),
            Line::from_text("view4444", &attrs, 0, None),
            Line::from_text("view5555", &attrs, 0, None),
            Line::from_text("view6666", &attrs, 0, None),
        ]);

        let logical_lines = screen.rebuild_logical_lines_from_physical(2);
        let logical_count = logical_lines.len();
        let viewport_plan = screen.build_viewport_reflow_plan_for_current_snapshot(logical_count);
        assert!(viewport_plan
            .batches
            .iter()
            .any(|batch| batch.priority == ReflowBatchPriority::Viewport));
        assert!(viewport_plan
            .batches
            .iter()
            .any(|batch| batch.priority == ReflowBatchPriority::ColdScrollback));

        let full_scan_plan = ViewportReflowPlan::full_scan(logical_count);
        let mut viewport_screen = screen.clone();
        let mut full_scan_screen = screen.clone();
        viewport_screen.wrap_logical_lines_for_resize(
            &logical_lines,
            4,
            2,
            Some(&viewport_plan),
            &|| false,
        );
        full_scan_screen.wrap_logical_lines_for_resize(
            &logical_lines,
            4,
            2,
            Some(&full_scan_plan),
            &|| false,
        );

        let wrapped_snapshot = |screen: &Screen| {
            screen
                .rewrap_scratch_slots
                .iter()
                .take(logical_count)
                .map(|slot| {
                    slot.as_ref()
                        .expect("missing rewrap scratch slot")
                        .shared_chunk(&screen.lines)
                        .iter()
                        .map(|line| {
                            (
                                line.as_str().to_string(),
                                line.last_cell_was_wrapped(),
                                line.current_seqno(),
                            )
                        })
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(
            wrapped_snapshot(&viewport_screen),
            wrapped_snapshot(&full_scan_screen),
            "viewport-first reflow must preserve full-scan wrap points, order, and seqno"
        );
    }

    #[test]
    fn viewport_reflow_plan_is_deterministic_for_identical_snapshot() {
        let mut screen = test_screen(3, 5, 96);
        let attrs = CellAttributes::blank();

        screen.lines = VecDeque::from(vec![
            Line::from_text("aa", &attrs, 0, None),
            Line::from_text_with_wrapped_last_col("bbbb", &attrs, 0),
            Line::from_text("cccc", &attrs, 0, None),
            Line::from_text("dd", &attrs, 0, None),
            Line::from_text("ee", &attrs, 0, None),
            Line::from_text("ff", &attrs, 0, None),
        ]);

        let logical_count = Screen::logical_line_physical_ranges(&screen.lines).len();
        let first = screen.build_viewport_reflow_plan_for_current_snapshot(logical_count);
        let second = screen.build_viewport_reflow_plan_for_current_snapshot(logical_count);

        assert_eq!(first, second);
        assert!(first.covers_each_logical_line_once(logical_count));
    }

    #[test]
    fn viewport_reflow_plan_handles_empty_buffer() {
        let plan = Screen::build_viewport_reflow_plan_from_ranges(&[], 0..0, 0);
        assert!(plan.batches.is_empty());
        assert!(plan.covers_each_logical_line_once(0));
    }

    #[test]
    fn viewport_reflow_plan_handles_huge_buffer_with_bounded_batches() {
        let logical_count = 4096usize;
        let logical_ranges: Vec<Range<usize>> =
            (0..logical_count).map(|idx| idx..idx + 1).collect();
        let visible_range = (logical_count - 32)..logical_count;
        let plan = Screen::build_viewport_reflow_plan_from_ranges(
            &logical_ranges,
            visible_range,
            logical_count,
        );

        assert!(plan.covers_each_logical_line_once(logical_count));
        assert!(plan
            .batches
            .iter()
            .all(|batch| batch.logical_range.len() <= MAX_REFLOW_BATCH_LOGICAL_LINES));
        assert_eq!(
            plan.batches
                .first()
                .expect("non-empty plan for non-empty logical ranges")
                .priority,
            ReflowBatchPriority::Viewport
        );
    }

    #[test]
    fn cold_scrollback_worker_cancels_stale_intent() {
        let mut worker = ColdScrollbackReflowWorker::default();
        worker.begin_intent(1, 42);
        assert_eq!(worker.active_intent(), Some(1));
        assert_eq!(worker.backlog_depth(), 42);
        assert_eq!(worker.cancellation_count(), 0);

        worker.begin_intent(2, 8);
        assert_eq!(worker.active_intent(), Some(2));
        assert_eq!(worker.backlog_depth(), 8);
        assert_eq!(worker.cancellation_count(), 1);
    }

    #[test]
    fn cold_scrollback_worker_tracks_completion_and_throughput() {
        let mut worker = ColdScrollbackReflowWorker::default();
        worker.begin_intent(7, 10);
        worker.complete_cold_batch(7, 4);
        worker.complete_cold_batch(7, 6);
        worker.finish_intent(7, std::time::Duration::from_millis(20), 10);

        assert_eq!(worker.backlog_depth(), 0);
        assert_eq!(worker.active_intent(), None);
        assert_eq!(worker.completed_batches_total(), 2);
        assert_eq!(worker.completed_lines_total(), 10);
        assert_eq!(worker.cancellation_count(), 0);
        assert!(
            worker.completion_throughput_lines_per_sec() >= 500,
            "expected throughput to be non-trivially positive"
        );
    }

    #[test]
    fn cold_scrollback_worker_caps_backlog_depth() {
        let mut worker = ColdScrollbackReflowWorker::default();
        worker.begin_intent(11, COLD_SCROLLBACK_BACKLOG_DEPTH_CAP.saturating_mul(2));
        assert_eq!(worker.backlog_depth(), COLD_SCROLLBACK_BACKLOG_DEPTH_CAP);
        assert_eq!(
            worker.peak_backlog_depth(),
            COLD_SCROLLBACK_BACKLOG_DEPTH_CAP
        );
    }

    #[test]
    fn last_good_frame_tracks_resize_begin_and_commit_lineage() {
        let mut screen = test_screen(3, 4, 96);
        let attrs = CellAttributes::blank();
        screen.lines = VecDeque::from(vec![
            Line::from_text("abcd", &attrs, 0, None),
            Line::from_text("efgh", &attrs, 0, None),
            Line::from_text("ijkl", &attrs, 0, None),
        ]);

        let cursor = test_cursor(1, 2, 7);
        let _ = screen.resize(test_size(3, 3, 96), cursor, 8, false);

        assert_eq!(screen.last_good_frame_lifecycle.capture_count, 2);
        assert!(
            screen.last_good_frame_lifecycle.current_retained_bytes > 0,
            "resize commit should retain a non-empty frame snapshot"
        );
        let frame = screen
            .last_good_frame
            .as_ref()
            .expect("resize commit should preserve last-good-frame");
        assert_eq!(frame.cols, 3);
        assert_eq!(frame.rows, 3);
        assert_eq!(frame.captured_seqno, 8);
        assert_eq!(
            frame.lineage_id,
            screen.last_good_frame_lifecycle.last_lineage_id
        );
        assert!(
            frame.estimated_bytes <= screen.last_good_frame_lifecycle.last_budget_bytes,
            "retained frame must fit within configured byte budget"
        );
    }

    #[test]
    fn last_good_frame_invalidates_on_content_mutation() {
        let mut screen = test_screen(2, 4, 96);
        let attrs = CellAttributes::blank();
        screen.lines = VecDeque::from(vec![
            Line::from_text("abcd", &attrs, 0, None),
            Line::from_text("wxyz", &attrs, 0, None),
        ]);

        let cursor = test_cursor(0, 1, 1);
        let _ = screen.resize(test_size(2, 3, 96), cursor, 2, false);
        assert!(
            screen.last_good_frame.is_some(),
            "resize should capture a retained frame before mutation"
        );
        let prior_invalidations = screen.last_good_frame_lifecycle.invalidation_count;

        screen.set_cell(0, 0, &Cell::new('Z', attrs.clone()), 3);

        assert!(screen.last_good_frame.is_none());
        assert_eq!(
            screen.last_good_frame_lifecycle.invalidation_count,
            prior_invalidations + 1
        );
        assert_eq!(screen.last_good_frame_lifecycle.current_retained_bytes, 0);
    }

    #[test]
    fn last_good_frame_drops_snapshot_when_budget_exceeded() {
        let mut screen = test_screen(1, 2, 96);
        let attrs = CellAttributes::blank();
        screen.lines = VecDeque::from(vec![Line::from_text(
            "this line is intentionally oversized relative to viewport budget",
            &attrs,
            0,
            None,
        )]);

        let estimated = Screen::estimate_frame_bytes(&screen.visible_frame_snapshot());
        let budget = screen.retained_frame_byte_budget();
        assert!(estimated > budget);

        screen.retain_last_good_frame(4, LastGoodFrameTransition::ResizeBegin);

        assert!(screen.last_good_frame.is_none());
        assert_eq!(screen.last_good_frame_lifecycle.drop_over_budget_count, 1);
        assert_eq!(screen.last_good_frame_lifecycle.current_retained_bytes, 0);
    }

    #[test]
    fn last_good_frame_rolls_back_after_forced_resize_commit_failure() {
        let mut screen = test_screen(3, 4, 96);
        let attrs = CellAttributes::blank();
        screen.lines = VecDeque::from(vec![
            Line::from_text("abcd", &attrs, 0, None),
            Line::from_text("efgh", &attrs, 0, None),
            Line::from_text("ijkl", &attrs, 0, None),
        ]);

        let cursor = test_cursor(1, 1, 10);
        let before_lines: Vec<String> = screen
            .visible_lines()
            .into_iter()
            .map(|line| line.as_str().to_string())
            .collect();
        let before_dims = (screen.physical_cols, screen.physical_rows, screen.dpi);

        screen.force_resize_commit_rollback(LastGoodFrameRollbackCause::ForcedFailureInjection);
        let returned_cursor = screen.resize(test_size(4, 3, 144), cursor, 11, false);

        assert_eq!(returned_cursor, cursor);
        assert_eq!(
            (screen.physical_cols, screen.physical_rows, screen.dpi),
            before_dims
        );
        let after_lines: Vec<String> = screen
            .visible_lines()
            .into_iter()
            .map(|line| line.as_str().to_string())
            .collect();
        assert_eq!(after_lines, before_lines);
        assert_eq!(screen.last_good_frame_lifecycle.rollback_count, 1);
        assert_eq!(
            screen
                .last_good_frame_lifecycle
                .rollback_missing_snapshot_count,
            0
        );
    }

    #[test]
    fn forced_rollback_retry_matches_direct_resize_semantics() {
        let attrs = CellAttributes::blank();
        let seed_lines = VecDeque::from(vec![
            Line::from_text("abcd", &attrs, 0, None),
            Line::from_text("efgh", &attrs, 0, None),
            Line::from_text("ijkl", &attrs, 0, None),
        ]);
        let cursor = test_cursor(1, 1, 10);
        let target = test_size(4, 3, 144);

        let mut direct = test_screen(3, 4, 96);
        direct.lines = seed_lines.clone();
        let direct_cursor = direct.resize(target, cursor, 11, false);

        let mut fallback_then_retry = test_screen(3, 4, 96);
        fallback_then_retry.lines = seed_lines;
        fallback_then_retry
            .force_resize_commit_rollback(LastGoodFrameRollbackCause::ForcedFailureInjection);
        let fallback_cursor = fallback_then_retry.resize(target, cursor, 11, false);
        assert_eq!(
            fallback_cursor, cursor,
            "forced rollback path should preserve pre-resize cursor on rejected commit"
        );
        let retry_cursor = fallback_then_retry.resize(target, cursor, 12, false);

        let direct_lines: Vec<String> = direct
            .visible_lines()
            .into_iter()
            .map(|line| line.as_str().to_string())
            .collect();
        let retry_lines: Vec<String> = fallback_then_retry
            .visible_lines()
            .into_iter()
            .map(|line| line.as_str().to_string())
            .collect();

        assert_eq!(retry_cursor.x, direct_cursor.x);
        assert_eq!(retry_cursor.y, direct_cursor.y);
        assert_eq!(retry_cursor.shape, direct_cursor.shape);
        assert_eq!(retry_cursor.visibility, direct_cursor.visibility);
        assert_eq!(
            (
                fallback_then_retry.physical_cols,
                fallback_then_retry.physical_rows,
                fallback_then_retry.dpi
            ),
            (direct.physical_cols, direct.physical_rows, direct.dpi),
            "final geometry should converge after fallback retry"
        );
        assert_eq!(
            retry_lines, direct_lines,
            "final visible frame should match direct resize after fallback retry"
        );
        assert_eq!(
            fallback_then_retry.last_good_frame_lifecycle.rollback_count,
            1
        );
        assert_eq!(
            fallback_then_retry
                .last_good_frame_lifecycle
                .rollback_missing_snapshot_count,
            0
        );
    }

    #[test]
    fn last_good_frame_rollback_tracks_missing_snapshot_failures() {
        let mut screen = test_screen(2, 2, 96);
        assert!(!screen
            .rollback_to_last_good_frame(2, LastGoodFrameRollbackCause::ResizeCommitValidation));
        assert_eq!(screen.last_good_frame_lifecycle.rollback_count, 0);
        assert_eq!(
            screen
                .last_good_frame_lifecycle
                .rollback_missing_snapshot_count,
            1
        );
    }

    #[test]
    fn cursor_consistency_telemetry_records_passes_on_rewrap() {
        let mut screen = test_screen(4, 6, 96);
        let attrs = CellAttributes::blank();
        screen.lines = VecDeque::from(vec![
            Line::from_text_with_wrapped_last_col("abcdef", &attrs, 0),
            Line::from_text("ghij", &attrs, 0, None),
            Line::from_text("klmn", &attrs, 0, None),
            Line::new(0),
        ]);

        let cursor = test_cursor(2, 2, 1);
        let _ = screen.resize(test_size(4, 4, 96), cursor, 1, false);

        assert!(
            screen.cursor_consistency_telemetry.total_checks() >= 1,
            "resize rewrap should emit at least one consistency telemetry check"
        );
        assert_eq!(
            screen.cursor_consistency_telemetry.checks_failed, 0,
            "expected no consistency failures for this deterministic rewrap path"
        );
    }

    #[test]
    fn cursor_consistency_telemetry_records_failures_for_invalid_cursor() {
        let mut screen = test_screen(2, 2, 96);

        screen.record_cursor_consistency_telemetry(9, 0, 99);

        assert_eq!(screen.cursor_consistency_telemetry.checks_passed, 0);
        assert_eq!(screen.cursor_consistency_telemetry.checks_failed, 1);
        assert_eq!(screen.cursor_consistency_telemetry.total_checks(), 1);
    }

    // =========================================================================
    // Core Screen::resize() behavior
    // =========================================================================

    #[test]
    fn resize_changes_physical_dimensions() {
        let mut screen = test_screen(3, 4, 96);
        assert_eq!(screen.physical_rows, 3);
        assert_eq!(screen.physical_cols, 4);

        let cursor = test_cursor(0, 0, 1);
        let _ = screen.resize(test_size(5, 8, 96), cursor, 1, false);
        assert_eq!(screen.physical_rows, 5);
        assert_eq!(screen.physical_cols, 8);
    }

    #[test]
    fn resize_shrink_updates_dimensions() {
        let mut screen = test_screen(8, 10, 96);
        let cursor = test_cursor(0, 0, 1);
        let _ = screen.resize(test_size(3, 4, 96), cursor, 1, false);
        assert_eq!(screen.physical_rows, 3);
        assert_eq!(screen.physical_cols, 4);
    }

    #[test]
    fn resize_updates_dpi() {
        let mut screen = test_screen(3, 4, 96);
        assert_eq!(screen.dpi, 96);

        let cursor = test_cursor(0, 0, 1);
        let _ = screen.resize(test_size(3, 4, 144), cursor, 1, false);
        assert_eq!(screen.dpi, 144);
    }

    #[test]
    fn read_only_physical_lines_handles_wrapped_ring_storage() {
        let mut screen = test_screen(2, 4, 96);
        let attrs = CellAttributes::blank();
        screen.lines = VecDeque::with_capacity(4);
        for text in ["a", "b", "c", "d"] {
            screen
                .lines
                .push_back(Line::from_text(text, &attrs, 1, None));
        }
        screen.lines.pop_front();
        screen.lines.pop_front();
        for text in ["e", "f"] {
            screen
                .lines
                .push_back(Line::from_text(text, &attrs, 1, None));
        }
        let (first, second) = screen.lines.as_slices();
        assert!(!first.is_empty() && !second.is_empty());

        for (range, expected) in [
            (0..4, vec!["c", "d", "e", "f"]),
            (1..3, vec!["d", "e"]),
            (2..4, vec!["e", "f"]),
            (0..1, vec!["c"]),
            (3..3, vec![]),
        ] {
            let mut calls = 0;
            screen.with_phys_lines(range, |lines| {
                calls += 1;
                let text: Vec<_> = lines
                    .iter()
                    .map(|line| line.as_str().into_owned())
                    .collect();
                assert_eq!(text, expected);
            });
            assert_eq!(calls, 1);
        }
    }

    #[test]
    fn reflow_trimmed_shared_tokens_preserve_trailing_virtual_cursor_column() {
        let source = Line::from_text("ab界e\u{301}🚀z   ", &CellAttributes::default(), 1, None);
        let compact = Line::try_compact_logical_rows(core::iter::once(&source), 1).unwrap();
        let mut screen = test_screen(1, 64, 96);
        screen.lines = VecDeque::from([compact.clone()]);
        // Cover the cursor on text and past the final retained nonblank cell.
        for logical_x in [2, source.len() + 2] {
            screen.lines = VecDeque::from([compact.clone()]);
            screen.physical_cols = 64;
            screen.rewrap_cache = Some(Arc::new(LogicalLineWrapCache::with_token_budget(
                Some(screen.compute_layout_signature()),
                vec![compact.clone()],
                0,
            )));
            let mut cursor = (logical_x, 0);
            for cols in [3, 8, 1, 12, 5, 3] {
                cursor = screen.rewrap_lines(
                    cols,
                    1,
                    cursor,
                    2,
                    None,
                    &mut SelectionAnchorRegistry::default(),
                );
                screen.physical_cols = cols;
                assert_eq!(
                    screen.logical_cursor_from_physical(cursor.0, cursor.1),
                    Some((0, logical_x)),
                );
                assert!(screen
                    .rewrap_cache
                    .as_ref()
                    .unwrap()
                    .logical_lines
                    .iter()
                    .all(|line| line.wrap_source.is_none()));
            }
        }
    }

    #[test]
    fn reflow_cursor_tracks_actual_wide_grapheme_rows() {
        let mut screen = test_screen(1, 6, 96);
        screen.lines = VecDeque::from([Line::from_text(
            "界界界",
            &CellAttributes::default(),
            1,
            None,
        )]);
        // The cursor is on the third grapheme, not its trailing spacer.
        let mut cursor = (4, 0);
        for cols in [3, 5, 2, 4, 6, 3] {
            cursor = screen.rewrap_lines(
                cols,
                1,
                cursor,
                2,
                None,
                &mut SelectionAnchorRegistry::default(),
            );
            screen.physical_cols = cols;
            assert_eq!(
                screen.logical_cursor_from_physical(cursor.0, cursor.1),
                Some((0, 4))
            );
            assert!(
                screen.lines[cursor.1]
                    .visible_cells()
                    .any(|cell| cell.cell_index() == cursor.0),
                "width {} mapped cursor {:?} onto a spacer or past the line",
                cols,
                cursor
            );
        }
    }

    #[test]
    fn resize_returns_cursor_position() {
        let mut screen = test_screen(4, 6, 96);
        let attrs = CellAttributes::blank();
        screen.lines = VecDeque::from(vec![
            Line::from_text("aaa", &attrs, 0, None),
            Line::from_text("bbb", &attrs, 0, None),
            Line::from_text("ccc", &attrs, 0, None),
            Line::from_text("ddd", &attrs, 0, None),
        ]);

        let cursor = test_cursor(2, 1, 1);
        let new_cursor = screen.resize(test_size(4, 8, 96), cursor, 1, false);
        // Cursor should be valid within new dimensions
        assert!(
            (new_cursor.x as usize) < screen.physical_cols,
            "cursor x {} should be < cols {}",
            new_cursor.x,
            screen.physical_cols
        );
    }

    #[test]
    fn resize_same_dimensions_preserves_content() {
        let mut screen = test_screen(2, 4, 96);
        let attrs = CellAttributes::blank();
        screen.lines = VecDeque::from(vec![
            Line::from_text("abcd", &attrs, 0, None),
            Line::from_text("efgh", &attrs, 0, None),
        ]);

        let before: Vec<String> = screen
            .visible_lines()
            .into_iter()
            .map(|l| l.as_str().to_string())
            .collect();

        let cursor = test_cursor(0, 0, 1);
        let _ = screen.resize(test_size(2, 4, 96), cursor, 1, false);

        let after: Vec<String> = screen
            .visible_lines()
            .into_iter()
            .map(|l| l.as_str().to_string())
            .collect();
        assert_eq!(before, after);
    }

    #[test]
    fn resize_grow_adds_visible_lines() {
        let mut screen = test_screen(2, 4, 96);
        let attrs = CellAttributes::blank();
        screen.lines = VecDeque::from(vec![
            Line::from_text("abcd", &attrs, 0, None),
            Line::from_text("efgh", &attrs, 0, None),
        ]);

        let cursor = test_cursor(0, 0, 1);
        let _ = screen.resize(test_size(4, 4, 96), cursor, 1, false);

        let visible = screen.visible_lines();
        assert_eq!(visible.len(), 4);
    }

    #[test]
    fn cold_scrollback_worker_zero_backlog() {
        let mut worker = ColdScrollbackReflowWorker::default();
        worker.begin_intent(1, 0);
        assert_eq!(worker.backlog_depth(), 0);
        worker.finish_intent(1, std::time::Duration::from_millis(0), 0);
        assert_eq!(worker.active_intent(), None);
        assert_eq!(worker.completed_batches_total(), 0);
    }

    #[test]
    fn cold_scrollback_worker_peak_backlog_tracks_maximum() {
        let mut worker = ColdScrollbackReflowWorker::default();

        // First intent with backlog 50
        worker.begin_intent(1, 50);
        assert_eq!(worker.peak_backlog_depth(), 50);
        worker.finish_intent(1, std::time::Duration::from_millis(5), 50);

        // Second intent with smaller backlog
        worker.begin_intent(2, 20);
        assert_eq!(worker.peak_backlog_depth(), 50, "peak should persist");
        worker.finish_intent(2, std::time::Duration::from_millis(2), 20);
    }

    #[test]
    fn tiered_scrollback_enforces_hot_line_budget() {
        let mut screen = test_screen_with_config(
            2,
            4,
            96,
            TestTermConfig {
                scrollback: 8,
                scrollback_tier: crate::config::ScrollbackTierConfig {
                    enabled: true,
                    hot_lines: 2,
                    warm_max_bytes: 0,
                },
                cold_sink: Some(Arc::new(TestColdScrollbackSink::default())),
                ..TestTermConfig::default()
            },
        );
        let region: Range<VisibleRowIndex> = 0..(screen.physical_rows as VisibleRowIndex);

        for seq in 1..=16 {
            screen.scroll_up(&region, 1, seq, CellAttributes::blank(), bidi_mode());
        }

        assert!(
            screen.scrollback_rows() <= screen.physical_rows + 2,
            "hot tier should cap in-memory scrollback rows"
        );
        assert!(
            screen.scrollback_tiering.warm_spill_lines_total > 0,
            "scrollback beyond hot budget should spill into tier accounting"
        );
    }

    #[test]
    fn tiered_scrollback_hydrates_evicted_rows_from_cold_sink() {
        let cold_sink = Arc::new(TestColdScrollbackSink::default());
        let mut screen = test_screen_with_config(
            2,
            8,
            96,
            TestTermConfig {
                scrollback: 16,
                scrollback_tier: crate::config::ScrollbackTierConfig {
                    enabled: true,
                    hot_lines: 1,
                    warm_max_bytes: 0,
                },
                cold_sink: Some(cold_sink.clone()),
                ..TestTermConfig::default()
            },
        );
        let attrs = CellAttributes::blank();
        let region: Range<VisibleRowIndex> = 0..(screen.physical_rows as VisibleRowIndex);

        for seq in 1..=8 {
            let bottom = screen.phys_row(screen.physical_rows as VisibleRowIndex - 1);
            screen.lines[bottom] = Line::from_text(&format!("line-{seq}"), &attrs, seq, None);
            screen.scroll_up(&region, 1, seq, attrs.clone(), bidi_mode());
        }

        assert!(
            screen.in_memory_scrollback_rows() <= 1,
            "hot tier should keep only the configured resident scrollback"
        );
        assert!(
            cold_sink.retained_scrollback_rows() > 0,
            "evicted rows should be offered to the cold sink"
        );
        assert_eq!(
            screen.scrollback_top_stable_row(),
            cold_sink
                .oldest_scrollback_row()
                .expect("cold sink oldest row")
        );
        assert!(
            screen.reachable_scrollback_rows() > screen.scrollback_rows(),
            "reachable rows should include cold sink history beyond hot memory"
        );

        let batches_before = cold_sink.batch_reads.load(Ordering::Relaxed);
        let (first, lines) = screen.lines_in_stable_range(1..4);
        let texts: Vec<String> = lines.iter().map(|line| line.as_str().to_string()).collect();
        assert_eq!(first, 1);
        assert_eq!(texts, ["line-1", "line-2", "line-3"]);
        assert!(cold_sink.batch_reads.load(Ordering::Relaxed) > batches_before);

        let status = screen.tiered_scrollback_status();
        assert_eq!(
            status.cold_sink_retained_lines,
            cold_sink.retained_scrollback_rows()
        );
        assert!(status.cold_sink_retained_bytes > 0);
    }

    #[test]
    fn tiered_scrollback_retains_rows_in_memory_when_cold_sink_rejects_them() {
        let mut screen = test_screen_with_config(
            2,
            8,
            96,
            TestTermConfig {
                scrollback: 16,
                scrollback_tier: crate::config::ScrollbackTierConfig {
                    enabled: true,
                    hot_lines: 1,
                    warm_max_bytes: 0,
                },
                cold_sink: Some(Arc::new(RejectingColdScrollbackSink)),
                ..TestTermConfig::default()
            },
        );
        let attrs = CellAttributes::blank();
        let region: Range<VisibleRowIndex> = 0..(screen.physical_rows as VisibleRowIndex);

        for seq in 1..=8 {
            let bottom = screen.phys_row(screen.physical_rows as VisibleRowIndex - 1);
            screen.lines[bottom] = Line::from_text(&format!("line-{seq}"), &attrs, seq, None);
            screen.scroll_up(&region, 1, seq, attrs.clone(), bidi_mode());
        }

        assert!(
            screen.lines.len() > screen.physical_rows + 1,
            "durability failure must relax the hot-memory bound instead of discarding rows"
        );
        assert_eq!(
            screen.stable_row_index_offset, 0,
            "stable identity may advance only after a row is durably accepted"
        );
        assert_eq!(
            screen.scrollback_tiering.warm_spill_lines_total, 0,
            "rejected rows must not be counted as successful cold spill"
        );
    }

    #[test]
    fn tiered_scrollback_without_cold_sink_preserves_unaccepted_rows() {
        let mut screen = test_screen_with_config(
            2,
            8,
            96,
            TestTermConfig {
                scrollback: 16,
                scrollback_tier: crate::config::ScrollbackTierConfig {
                    enabled: true,
                    hot_lines: 1,
                    warm_max_bytes: 0,
                },
                cold_sink: None,
                ..TestTermConfig::default()
            },
        );
        let attrs = CellAttributes::blank();
        for seq in 1..=8 {
            let bottom = screen.phys_row(1);
            screen.lines[bottom] = Line::from_text(&format!("line-{seq}"), &attrs, seq, None);
            screen.scroll_up(&(0..2), 1, seq, attrs.clone(), bidi_mode());
        }
        let retained: Vec<_> = screen
            .lines
            .iter()
            .map(|line| line.as_str().into_owned())
            .filter(|line| line.starts_with("line-"))
            .collect();
        assert_eq!(
            retained,
            (1..=8).map(|seq| format!("line-{seq}")).collect::<Vec<_>>()
        );
        assert_eq!(screen.stable_row_index_offset, 0);
        assert_eq!(screen.scrollback_tiering.warm_spill_lines_total, 0);
        assert_eq!(screen.scrollback_tiering.cold_spill_lines_total, 0);
    }

    #[test]
    fn tiered_scrollback_bounds_hot_memory_during_high_output() {
        let cold_sink = Arc::new(TestColdScrollbackSink::default());
        let hot_lines = 8;
        let configured_scrollback = 128;
        let mut screen = test_screen_with_config(
            4,
            16,
            96,
            TestTermConfig {
                scrollback: configured_scrollback,
                scrollback_tier: crate::config::ScrollbackTierConfig {
                    enabled: true,
                    hot_lines,
                    warm_max_bytes: 0,
                },
                cold_sink: Some(cold_sink.clone()),
                ..TestTermConfig::default()
            },
        );
        let attrs = CellAttributes::blank();
        let region: Range<VisibleRowIndex> = 0..(screen.physical_rows as VisibleRowIndex);

        for seq in 1..=2_048 {
            let bottom = screen.phys_row(screen.physical_rows as VisibleRowIndex - 1);
            let text = format!("high-output-{seq:04}-{}", "x".repeat(120));
            screen.lines[bottom] = Line::from_text(&text, &attrs, seq, None);
            screen.scroll_up(&region, 1, seq, attrs.clone(), bidi_mode());
        }

        assert!(
            screen.in_memory_scrollback_rows() <= hot_lines,
            "screen should keep only the configured hot scrollback resident"
        );
        assert!(
            screen.lines.len() <= screen.physical_rows + hot_lines,
            "visible + hot rows should stay bounded after sustained output"
        );
        assert!(
            cold_sink.retained_scrollback_rows() <= configured_scrollback - hot_lines,
            "cold sink retention should cap old reachable rows"
        );
        assert!(
            screen.reachable_scrollback_rows() <= screen.physical_rows + configured_scrollback,
            "reachable history should not grow beyond configured scrollback"
        );
        assert!(
            screen.reachable_scrollback_rows() > screen.lines.len(),
            "cold sink should preserve reachable history beyond hot memory"
        );

        let oldest = screen.scrollback_top_stable_row();
        let (_first, cold_lines) = screen.lines_in_stable_range(oldest..oldest + 5);
        assert_eq!(cold_lines.len(), 5);
        assert!(
            cold_lines
                .iter()
                .all(|line| line.as_str().starts_with("high-output-")),
            "oldest reachable rows should hydrate from cold storage"
        );

        let status = screen.tiered_scrollback_status();
        assert!(status.cold_sink_retained_bytes > 0);
        assert_eq!(
            status.in_memory_scrollback_rows,
            screen.in_memory_scrollback_rows()
        );
    }

    #[test]
    fn zero_scrollback_preserves_no_history_without_tiering() {
        let mut screen = test_screen_with_config(
            2,
            4,
            96,
            TestTermConfig {
                scrollback: 0,
                scrollback_tier: crate::config::ScrollbackTierConfig {
                    enabled: false,
                    hot_lines: 0,
                    warm_max_bytes: 0,
                },
                ..TestTermConfig::default()
            },
        );
        let region: Range<VisibleRowIndex> = 0..(screen.physical_rows as VisibleRowIndex);

        for seq in 1..=16 {
            screen.scroll_up(&region, 1, seq, CellAttributes::blank(), bidi_mode());
        }

        assert_eq!(
            screen.scrollback_rows(),
            screen.physical_rows,
            "zero scrollback should retain only the visible rows"
        );
    }

    #[test]
    fn zero_scrollback_preserves_no_history_with_tiering_enabled() {
        let mut screen = test_screen_with_config(
            2,
            4,
            96,
            TestTermConfig {
                scrollback: 0,
                scrollback_tier: crate::config::ScrollbackTierConfig {
                    enabled: true,
                    hot_lines: 1000,
                    warm_max_bytes: 64 * 1024 * 1024,
                },
                ..TestTermConfig::default()
            },
        );
        let region: Range<VisibleRowIndex> = 0..(screen.physical_rows as VisibleRowIndex);

        for seq in 1..=16 {
            screen.scroll_up(&region, 1, seq, CellAttributes::blank(), bidi_mode());
        }

        assert_eq!(
            screen.scrollback_rows(),
            screen.physical_rows,
            "tiered mode must not override an explicit zero scrollback budget"
        );
        assert_eq!(
            screen.scrollback_tiering.warm_spill_lines_total, 0,
            "zero scrollback should not accumulate warm-tier spill telemetry"
        );
        assert_eq!(
            screen.scrollback_tiering.cold_spill_lines_total, 0,
            "zero scrollback should not accumulate cold-tier spill telemetry"
        );
        assert_eq!(
            screen.cold_scrollback_worker.completed_lines_total(),
            0,
            "zero scrollback should not drive cold spill worker activity"
        );
    }

    #[test]
    fn tiered_scrollback_spills_overflow_from_warm_to_cold() {
        let warm_max_bytes = std::mem::size_of::<Line>() * 2;
        let mut screen = test_screen_with_config(
            2,
            4,
            96,
            TestTermConfig {
                scrollback: 10,
                scrollback_tier: crate::config::ScrollbackTierConfig {
                    enabled: true,
                    hot_lines: 2,
                    warm_max_bytes,
                },
                cold_sink: Some(Arc::new(TestColdScrollbackSink::default())),
                ..TestTermConfig::default()
            },
        );
        let region: Range<VisibleRowIndex> = 0..(screen.physical_rows as VisibleRowIndex);

        for seq in 1..=32 {
            screen.scroll_up(&region, 1, seq, CellAttributes::blank(), bidi_mode());
        }

        assert!(
            screen.scrollback_tiering.warm_bytes <= warm_max_bytes,
            "warm tier accounting should stay within configured byte budget"
        );
        assert!(
            screen.scrollback_tiering.warm_resident_lines() <= 2,
            "warm tier residency should be bounded by byte budget"
        );
        assert!(
            screen.scrollback_tiering.cold_spill_lines_total > 0,
            "overflow beyond warm budget should roll into cold tier accounting"
        );
    }

    #[test]
    fn tiered_scrollback_cold_spill_updates_worker_metrics() {
        let warm_max_bytes = std::mem::size_of::<Line>() * 2;
        let mut screen = test_screen_with_config(
            2,
            4,
            96,
            TestTermConfig {
                scrollback: 10,
                scrollback_tier: crate::config::ScrollbackTierConfig {
                    enabled: true,
                    hot_lines: 2,
                    warm_max_bytes,
                },
                cold_sink: Some(Arc::new(TestColdScrollbackSink::default())),
                ..TestTermConfig::default()
            },
        );
        let region: Range<VisibleRowIndex> = 0..(screen.physical_rows as VisibleRowIndex);

        for seq in 1..=32 {
            screen.scroll_up(&region, 1, seq, CellAttributes::blank(), bidi_mode());
        }

        let cold_lines = screen.scrollback_tiering.cold_spill_lines_total;
        assert!(cold_lines > 0, "test requires warm→cold overflow to occur");
        assert_eq!(
            screen.cold_scrollback_worker.completed_lines_total(),
            cold_lines,
            "cold worker should account for tiered cold spill line completions"
        );
        assert!(
            screen.cold_scrollback_worker.completed_batches_total() > 0,
            "cold worker should track one or more completion batches"
        );
        assert_eq!(
            screen.cold_scrollback_worker.backlog_depth(),
            0,
            "spill processing should not leave residual backlog"
        );
        assert!(
            screen
                .cold_scrollback_worker
                .completion_throughput_lines_per_sec()
                > 0,
            "cold spill should produce throughput telemetry"
        );
    }

    #[test]
    fn evict_warm_scrollback_flushes_resident_warm_lines() {
        let warm_max_bytes = std::mem::size_of::<Line>() * 64;
        let mut screen = test_screen_with_config(
            2,
            4,
            96,
            TestTermConfig {
                scrollback: 10,
                scrollback_tier: crate::config::ScrollbackTierConfig {
                    enabled: true,
                    hot_lines: 2,
                    warm_max_bytes,
                },
                cold_sink: Some(Arc::new(TestColdScrollbackSink::default())),
                ..TestTermConfig::default()
            },
        );
        let region: Range<VisibleRowIndex> = 0..(screen.physical_rows as VisibleRowIndex);

        for seq in 1..=16 {
            screen.scroll_up(&region, 1, seq, CellAttributes::blank(), bidi_mode());
        }

        let warm_lines_before = screen.scrollback_tiering.warm_resident_lines();
        let warm_bytes_before = screen.scrollback_tiering.warm_bytes;
        assert!(
            warm_lines_before > 0,
            "test requires resident warm-tier lines"
        );
        assert_eq!(
            screen.scrollback_tiering.cold_spill_lines_total, 0,
            "warm tier should still be resident before explicit eviction"
        );

        let evicted = screen.evict_warm_scrollback(999);

        assert_eq!(evicted, warm_lines_before);
        assert_eq!(screen.scrollback_tiering.warm_resident_lines(), 0);
        assert_eq!(screen.scrollback_tiering.warm_bytes, 0);
        assert_eq!(
            screen.scrollback_tiering.cold_spill_lines_total,
            warm_lines_before as u64
        );
        assert_eq!(
            screen.scrollback_tiering.cold_spill_bytes_total,
            warm_bytes_before as u64
        );
        assert_eq!(
            screen.cold_scrollback_worker.completed_lines_total(),
            warm_lines_before as u64
        );
        assert_eq!(screen.cold_scrollback_worker.completed_batches_total(), 1);
        assert_eq!(screen.cold_scrollback_worker.backlog_depth(), 0);
        assert!(
            screen
                .cold_scrollback_worker
                .completion_throughput_lines_per_sec()
                > 0,
            "explicit warm eviction should update cold worker throughput telemetry"
        );
    }

    #[test]
    fn evict_warm_scrollback_noops_when_no_warm_residency_exists() {
        let mut screen = test_screen_with_config(
            2,
            4,
            96,
            TestTermConfig {
                scrollback: 10,
                scrollback_tier: crate::config::ScrollbackTierConfig {
                    enabled: true,
                    hot_lines: 2,
                    warm_max_bytes: std::mem::size_of::<Line>() * 64,
                },
                ..TestTermConfig::default()
            },
        );

        assert_eq!(screen.scrollback_tiering.warm_resident_lines(), 0);
        assert_eq!(screen.evict_warm_scrollback(1000), 0);
        assert_eq!(screen.scrollback_tiering.warm_resident_lines(), 0);
        assert_eq!(screen.scrollback_tiering.cold_spill_lines_total, 0);
        assert_eq!(screen.cold_scrollback_worker.completed_lines_total(), 0);
        assert_eq!(screen.cold_scrollback_worker.completed_batches_total(), 0);
    }

    #[test]
    fn erase_scrollback_retains_hot_and_cold_state_when_clear_does_not_commit() {
        let cold_sink = Arc::new(TestColdScrollbackSink::default());
        let mut screen = test_screen_with_config(
            2,
            4,
            96,
            TestTermConfig {
                scrollback: 10,
                scrollback_tier: crate::config::ScrollbackTierConfig {
                    enabled: true,
                    hot_lines: 2,
                    warm_max_bytes: 0,
                },
                cold_sink: Some(cold_sink.clone()),
                ..TestTermConfig::default()
            },
        );
        let region: Range<VisibleRowIndex> = 0..(screen.physical_rows as VisibleRowIndex);
        for seq in 1..=8 {
            screen.scroll_up(&region, 1, seq, CellAttributes::blank(), bidi_mode());
        }
        let lines_before: Vec<_> = screen
            .lines
            .iter()
            .map(|line| {
                (
                    line.as_str().into_owned(),
                    line.current_seqno(),
                    line.last_cell_was_wrapped(),
                )
            })
            .collect();
        let offset_before = screen.stable_row_index_offset;
        let tiering_before = (
            screen.scrollback_tiering.warm_line_bytes.clone(),
            screen.scrollback_tiering.warm_bytes,
            screen.scrollback_tiering.warm_spill_lines_total,
            screen.scrollback_tiering.warm_spill_bytes_total,
            screen.scrollback_tiering.cold_spill_lines_total,
            screen.scrollback_tiering.cold_spill_bytes_total,
        );
        let cold_rows_before = cold_sink.retained_scrollback_rows();
        cold_sink.fail_clear.store(true, Ordering::Relaxed);
        let coordinate_witness = screen.capture_coordinate_witness();

        assert_eq!(
            screen.erase_scrollback(),
            Err(crate::config::ScrollbackSpillError::StorageUnavailable)
        );
        assert!(screen.matches_coordinate_witness(&coordinate_witness));
        let lines_after: Vec<_> = screen
            .lines
            .iter()
            .map(|line| {
                (
                    line.as_str().into_owned(),
                    line.current_seqno(),
                    line.last_cell_was_wrapped(),
                )
            })
            .collect();
        assert_eq!(lines_after, lines_before);
        assert_eq!(screen.stable_row_index_offset, offset_before);
        assert_eq!(
            (
                screen.scrollback_tiering.warm_line_bytes.clone(),
                screen.scrollback_tiering.warm_bytes,
                screen.scrollback_tiering.warm_spill_lines_total,
                screen.scrollback_tiering.warm_spill_bytes_total,
                screen.scrollback_tiering.cold_spill_lines_total,
                screen.scrollback_tiering.cold_spill_bytes_total,
            ),
            tiering_before
        );
        assert_eq!(cold_sink.retained_scrollback_rows(), cold_rows_before);
    }

    #[test]
    fn erase_scrollback_resets_tiered_scrollback_state_and_worker_metrics() {
        let warm_max_bytes = std::mem::size_of::<Line>() * 2;
        let mut screen = test_screen_with_config(
            2,
            4,
            96,
            TestTermConfig {
                scrollback: 10,
                scrollback_tier: crate::config::ScrollbackTierConfig {
                    enabled: true,
                    hot_lines: 2,
                    warm_max_bytes,
                },
                cold_sink: Some(Arc::new(TestColdScrollbackSink::default())),
                ..TestTermConfig::default()
            },
        );
        let region: Range<VisibleRowIndex> = 0..(screen.physical_rows as VisibleRowIndex);

        for seq in 1..=32 {
            screen.scroll_up(&region, 1, seq, CellAttributes::blank(), bidi_mode());
        }

        assert!(
            screen.scrollback_tiering.warm_spill_lines_total > 0,
            "test requires pre-existing tier telemetry"
        );
        assert!(
            screen.scrollback_tiering.cold_spill_lines_total > 0,
            "test requires pre-existing cold spill telemetry"
        );
        assert!(
            screen.cold_scrollback_worker.completed_lines_total() > 0,
            "test requires pre-existing cold spill worker activity"
        );

        screen
            .erase_scrollback()
            .expect("test scrollback clear should commit");

        assert_eq!(
            screen.scrollback_rows(),
            screen.physical_rows,
            "erase_scrollback should leave only visible rows resident"
        );
        assert!(
            screen.scrollback_tiering.warm_line_bytes.is_empty(),
            "erase_scrollback should clear warm-tier residency tracking"
        );
        assert_eq!(screen.scrollback_tiering.warm_bytes, 0);
        assert_eq!(screen.scrollback_tiering.warm_spill_lines_total, 0);
        assert_eq!(screen.scrollback_tiering.warm_spill_bytes_total, 0);
        assert_eq!(screen.scrollback_tiering.cold_spill_lines_total, 0);
        assert_eq!(screen.scrollback_tiering.cold_spill_bytes_total, 0);
        assert_eq!(screen.cold_scrollback_worker.active_intent(), None);
        assert_eq!(screen.cold_scrollback_worker.backlog_depth(), 0);
        assert_eq!(screen.cold_scrollback_worker.peak_backlog_depth(), 0);
        assert_eq!(
            screen
                .cold_scrollback_worker
                .completion_throughput_lines_per_sec(),
            0
        );
        assert_eq!(screen.cold_scrollback_worker.completed_lines_total(), 0);
        assert_eq!(screen.cold_scrollback_worker.completed_batches_total(), 0);
        assert_eq!(screen.cold_scrollback_worker.cancellation_count(), 0);
    }

    #[test]
    fn tiered_scrollback_status_reports_current_config_and_metrics() {
        let warm_max_bytes = std::mem::size_of::<Line>() * 2;
        let mut screen = test_screen_with_config(
            2,
            4,
            96,
            TestTermConfig {
                scrollback: 10,
                scrollback_tier: crate::config::ScrollbackTierConfig {
                    enabled: true,
                    hot_lines: 2,
                    warm_max_bytes,
                },
                cold_sink: Some(Arc::new(TestColdScrollbackSink::default())),
                ..TestTermConfig::default()
            },
        );
        let region: Range<VisibleRowIndex> = 0..(screen.physical_rows as VisibleRowIndex);

        for seq in 1..=32 {
            screen.scroll_up(&region, 1, seq, CellAttributes::blank(), bidi_mode());
        }

        let status = screen.tiered_scrollback_status();
        assert_eq!(screen.try_tiered_scrollback_status(), Ok(Some(status)));
        assert!(status.tiering_enabled);
        assert_eq!(status.configured_scrollback_rows, 10);
        assert_eq!(status.configured_hot_lines, 2);
        assert_eq!(status.configured_warm_max_bytes, warm_max_bytes);
        assert_eq!(status.visible_rows, 2);
        assert!(
            status.in_memory_scrollback_rows <= 2,
            "hot tier should bound the in-memory scrollback rows"
        );
        assert_eq!(
            status.warm_spill_lines_total,
            status.cold_spill_lines_total + status.warm_resident_lines as u64
        );
        assert_eq!(
            status.warm_spill_bytes_total,
            status.cold_spill_bytes_total + status.warm_resident_bytes as u64
        );
        assert_eq!(
            status.cold_worker_completed_lines_total,
            status.cold_spill_lines_total
        );
        assert!(
            status.cold_worker_completed_batches_total > 0,
            "cold spill should produce completion batches"
        );
        assert!(
            status.cold_worker_completion_throughput_lines_per_sec > 0,
            "cold spill should record throughput telemetry"
        );
    }

    #[test]
    fn erase_scrollback_resets_tiered_scrollback_status_but_keeps_config() {
        let warm_max_bytes = std::mem::size_of::<Line>() * 2;
        let mut screen = test_screen_with_config(
            2,
            4,
            96,
            TestTermConfig {
                scrollback: 10,
                scrollback_tier: crate::config::ScrollbackTierConfig {
                    enabled: true,
                    hot_lines: 2,
                    warm_max_bytes,
                },
                ..TestTermConfig::default()
            },
        );
        let region: Range<VisibleRowIndex> = 0..(screen.physical_rows as VisibleRowIndex);

        for seq in 1..=32 {
            screen.scroll_up(&region, 1, seq, CellAttributes::blank(), bidi_mode());
        }
        screen
            .erase_scrollback()
            .expect("test scrollback clear should commit");

        let status = screen.tiered_scrollback_status();
        assert!(status.tiering_enabled);
        assert_eq!(status.configured_scrollback_rows, 10);
        assert_eq!(status.configured_hot_lines, 2);
        assert_eq!(status.configured_warm_max_bytes, warm_max_bytes);
        assert_eq!(status.visible_rows, 2);
        assert_eq!(status.in_memory_scrollback_rows, 0);
        assert_eq!(status.warm_resident_lines, 0);
        assert_eq!(status.warm_resident_bytes, 0);
        assert_eq!(status.warm_spill_lines_total, 0);
        assert_eq!(status.warm_spill_bytes_total, 0);
        assert_eq!(status.cold_spill_lines_total, 0);
        assert_eq!(status.cold_spill_bytes_total, 0);
        assert_eq!(status.cold_worker_peak_backlog_depth, 0);
        assert_eq!(status.cold_worker_completion_throughput_lines_per_sec, 0);
        assert_eq!(status.cold_worker_completed_lines_total, 0);
        assert_eq!(status.cold_worker_completed_batches_total, 0);
        assert_eq!(status.cold_worker_cancellation_count, 0);
    }

    #[test]
    fn multiple_resizes_preserve_rewrap_cache_integrity() {
        let mut screen = test_screen(3, 6, 96);
        let attrs = CellAttributes::blank();
        screen.lines = VecDeque::from(vec![
            Line::from_text_with_wrapped_last_col("abcdef", &attrs, 0),
            Line::from_text("ghij", &attrs, 0, None),
            Line::from_text("klmn", &attrs, 0, None),
        ]);

        let cursor = test_cursor(0, 0, 1);
        // Resize through multiple widths
        let cursor = screen.resize(test_size(3, 4, 96), cursor, 1, false);
        let cursor = screen.resize(test_size(3, 8, 96), cursor, 2, false);
        let _ = screen.resize(test_size(3, 3, 96), cursor, 3, false);

        // Cache should exist and contain entries
        let cache = screen.rewrap_cache.as_ref().expect("cache should exist");
        assert!(
            !cache.wrapped_by_key.is_empty(),
            "cache should have at least one entry after multiple resizes"
        );
    }

    #[test]
    fn last_good_frame_lifecycle_capture_count_increases_per_resize() {
        let mut screen = test_screen(2, 4, 96);
        let attrs = CellAttributes::blank();
        screen.lines = VecDeque::from(vec![
            Line::from_text("abcd", &attrs, 0, None),
            Line::from_text("efgh", &attrs, 0, None),
        ]);

        let cursor = test_cursor(0, 0, 1);
        let cursor = screen.resize(test_size(2, 3, 96), cursor, 1, false);
        let count_after_first = screen.last_good_frame_lifecycle.capture_count;

        let _ = screen.resize(test_size(2, 5, 96), cursor, 2, false);
        let count_after_second = screen.last_good_frame_lifecycle.capture_count;

        assert!(
            count_after_second > count_after_first,
            "capture count should increase: {} > {}",
            count_after_second,
            count_after_first
        );
    }

    // --- Resize quality / readability gate tests ---

    #[test]
    fn config_change_recomputes_cached_wraps_under_current_policy() {
        let mut screen = test_screen(3, 10, 96);
        let attrs = CellAttributes::blank();
        screen.lines = VecDeque::from(vec![
            Line::from_text_with_wrapped_last_col("abcdefghij", &attrs, 1),
            Line::from_text_with_wrapped_last_col("klmnopqrst", &attrs, 1),
            Line::from_text("uvwx", &attrs, 1, None),
        ]);
        let mut cursor = test_cursor(0, 0, 1);
        for (seqno, cols) in [(2, 8), (3, 5), (4, 8)] {
            cursor = screen.resize(test_size(3, cols, 96), cursor, seqno, false);
        }
        let (_, _, _, cached, _) = screen
            .logical_wraps_for_resize(5, 5, true, &|| false)
            .unwrap();
        assert!(cached, "exercise a previously cached target width");
        assert!(screen.last_resize_wrap_scorecard.is_none());

        // Installing the same policy must keep useful whole-line wraps.
        let unchanged = Arc::clone(&screen.config);
        screen.set_config(&unchanged);
        let (_, _, _, cached, _) = screen
            .logical_wraps_for_resize(5, 5, true, &|| false)
            .unwrap();
        assert!(cached, "unchanged policy should retain cached wraps");

        let scored: Arc<dyn TerminalConfiguration> = Arc::new(TestTermConfig {
            scorecard_enabled: true,
            ..TestTermConfig::default()
        });
        screen.set_config(&scored);
        let mut uncached = screen.clone();
        uncached.rewrap_cache = None;
        let _ = uncached.resize(test_size(3, 5, 96), cursor, 6, false);
        let _ = screen.resize(test_size(3, 5, 96), cursor, 6, false);
        let scorecard = screen
            .last_resize_wrap_scorecard
            .as_ref()
            .expect("newly enabled scoring must execute even at a previously cached width");
        assert!(scorecard.scored_lines > 0);
        assert_eq!(
            screen.last_resize_wrap_scorecard,
            uncached.last_resize_wrap_scorecard
        );
        let text = |screen: &Screen| {
            screen
                .lines
                .iter()
                .map(|line| (line.as_str().into_owned(), line.last_cell_was_wrapped()))
                .collect::<Vec<_>>()
        };
        assert_eq!(text(&screen), text(&uncached));

        screen.set_config(&unchanged);
        assert!(screen.last_resize_wrap_scorecard.is_none());
        assert!(screen.last_resize_wrap_gate_payload.is_none());
    }

    fn test_screen_with_scorecard(rows: usize, cols: usize) -> Screen {
        test_screen_with_config(
            rows,
            cols,
            96,
            TestTermConfig {
                scrollback: 256,
                scrollback_tier: crate::config::ScrollbackTierConfig::default(),
                cold_sink: None,
                kp_cost_model: MonospaceKpCostModel::terminal_default(),
                scorecard_enabled: true,
                readability_gate: ResizeReadabilityGatePolicy {
                    enabled: true,
                    max_line_badness_delta: 500,
                    max_total_badness_delta: 2000,
                    max_fallback_ratio_percent: 20,
                },
            },
        )
    }

    #[test]
    fn cached_width_restores_its_readability_scorecard_and_gate_payload() {
        let mut screen = test_screen_with_scorecard(3, 20);
        let attrs = CellAttributes::blank();
        screen.lines = VecDeque::from(vec![
            Line::from_text(
                "alpha beta gamma delta epsilon zeta eta theta",
                &attrs,
                1,
                None,
            ),
            Line::from_text("one two three four five six seven eight", &attrs, 1, None),
            Line::new(1),
        ]);
        let mut cursor = screen.resize(test_size(3, 8, 96), test_cursor(0, 0, 1), 2, false);
        let narrow_scorecard = screen.last_resize_wrap_scorecard.clone();
        let narrow_payload = screen.last_resize_wrap_gate_payload.clone();
        assert!(narrow_scorecard.as_ref().unwrap().scored_lines > 0);
        cursor = screen.resize(test_size(3, 80, 96), cursor, 3, false);
        assert_ne!(screen.last_resize_wrap_scorecard, narrow_scorecard);
        assert!(screen
            .rewrap_cache
            .as_ref()
            .unwrap()
            .wrapped_by_key
            .contains_key(&WrapCacheKey {
                physical_cols: 8,
                dpi: 96,
            }));
        let _ = screen.resize(test_size(3, 8, 96), cursor, 4, false);
        assert_eq!(screen.last_resize_wrap_scorecard, narrow_scorecard);
        assert_eq!(screen.last_resize_wrap_gate_payload, narrow_payload);
    }

    #[test]
    fn scorecard_records_when_enabled() {
        let mut screen = test_screen_with_scorecard(3, 10);
        let attrs = CellAttributes::blank();

        // Insert a line that will wrap (longer than 10 cols)
        screen.lines = VecDeque::from(vec![
            Line::from_text_with_wrapped_last_col("abcdefghijklmnop", &attrs, 0),
            Line::from_text("rest", &attrs, 0, None),
            Line::new(0),
        ]);

        let cursor = test_cursor(0, 0, 1);
        let _ = screen.resize(test_size(3, 8, 96), cursor, 1, false);

        assert!(
            screen.last_resize_wrap_scorecard.is_some(),
            "scorecard should be populated when scorecard_enabled is true"
        );

        let scorecard = screen.last_resize_wrap_scorecard.as_ref().unwrap();
        assert!(
            scorecard.scored_lines > 0,
            "scorecard should track at least one scored line"
        );
    }

    #[test]
    fn scorecard_not_recorded_when_disabled() {
        let mut screen = test_screen(3, 10, 96);
        let attrs = CellAttributes::blank();

        screen.lines = VecDeque::from(vec![
            Line::from_text_with_wrapped_last_col("abcdefghijklmnop", &attrs, 0),
            Line::from_text("rest", &attrs, 0, None),
            Line::new(0),
        ]);

        let cursor = test_cursor(0, 0, 1);
        let _ = screen.resize(test_size(3, 8, 96), cursor, 1, false);

        assert!(
            screen.last_resize_wrap_scorecard.is_none(),
            "scorecard should be None when scorecard_enabled is false"
        );
    }

    #[test]
    fn gate_status_disabled_when_gate_off() {
        let scorecard = ResizeWrapScorecard {
            scored_lines: 10,
            dp_lines: 5,
            fallback_lines: 5,
            greedy_total_cost: 100,
            selected_total_cost: 200,
            max_badness_delta: 9999,
            total_badness_delta: 99999,
        };
        let policy = ResizeReadabilityGatePolicy {
            enabled: false,
            max_line_badness_delta: 1,
            max_total_badness_delta: 1,
            max_fallback_ratio_percent: 1,
        };
        assert_eq!(
            scorecard.gate_status(policy),
            ResizeWrapGateStatus::Disabled,
            "gate should report Disabled when not enabled, regardless of metrics"
        );
    }

    #[test]
    fn gate_pass_when_within_thresholds() {
        let scorecard = ResizeWrapScorecard {
            scored_lines: 100,
            dp_lines: 95,
            fallback_lines: 5,
            greedy_total_cost: 1000,
            selected_total_cost: 800,
            max_badness_delta: 100,
            total_badness_delta: 500,
        };
        let policy = ResizeReadabilityGatePolicy {
            enabled: true,
            max_line_badness_delta: 500,
            max_total_badness_delta: 2000,
            max_fallback_ratio_percent: 20,
        };
        assert_eq!(
            scorecard.gate_status(policy),
            ResizeWrapGateStatus::Pass,
            "gate should pass when all metrics are within thresholds"
        );
    }

    #[test]
    fn gate_fails_on_line_badness_exceeded() {
        let scorecard = ResizeWrapScorecard {
            scored_lines: 10,
            dp_lines: 10,
            fallback_lines: 0,
            greedy_total_cost: 100,
            selected_total_cost: 80,
            max_badness_delta: 501,
            total_badness_delta: 501,
        };
        let policy = ResizeReadabilityGatePolicy {
            enabled: true,
            max_line_badness_delta: 500,
            max_total_badness_delta: 2000,
            max_fallback_ratio_percent: 20,
        };
        assert_eq!(
            scorecard.gate_status(policy),
            ResizeWrapGateStatus::Fail(ResizeWrapGateFailureReason::LineBadnessDeltaExceeded),
            "gate should fail when max_badness_delta exceeds threshold"
        );
    }

    #[test]
    fn gate_fails_on_total_badness_exceeded() {
        let scorecard = ResizeWrapScorecard {
            scored_lines: 10,
            dp_lines: 10,
            fallback_lines: 0,
            greedy_total_cost: 100,
            selected_total_cost: 80,
            max_badness_delta: 100,
            total_badness_delta: 2001,
        };
        let policy = ResizeReadabilityGatePolicy {
            enabled: true,
            max_line_badness_delta: 500,
            max_total_badness_delta: 2000,
            max_fallback_ratio_percent: 20,
        };
        assert_eq!(
            scorecard.gate_status(policy),
            ResizeWrapGateStatus::Fail(ResizeWrapGateFailureReason::TotalBadnessDeltaExceeded),
            "gate should fail when total_badness_delta exceeds threshold"
        );
    }

    #[test]
    fn gate_fails_on_fallback_ratio_exceeded() {
        let scorecard = ResizeWrapScorecard {
            scored_lines: 10,
            dp_lines: 7,
            fallback_lines: 3,
            greedy_total_cost: 100,
            selected_total_cost: 80,
            max_badness_delta: 100,
            total_badness_delta: 500,
        };
        let policy = ResizeReadabilityGatePolicy {
            enabled: true,
            max_line_badness_delta: 500,
            max_total_badness_delta: 2000,
            max_fallback_ratio_percent: 20,
        };
        // 3/10 = 30% > 20% threshold
        assert_eq!(
            scorecard.gate_status(policy),
            ResizeWrapGateStatus::Fail(ResizeWrapGateFailureReason::FallbackRatioExceeded),
            "gate should fail when fallback ratio exceeds threshold"
        );
    }

    #[test]
    fn gate_pass_at_exact_fallback_boundary() {
        let scorecard = ResizeWrapScorecard {
            scored_lines: 10,
            dp_lines: 8,
            fallback_lines: 2,
            greedy_total_cost: 100,
            selected_total_cost: 80,
            max_badness_delta: 100,
            total_badness_delta: 500,
        };
        let policy = ResizeReadabilityGatePolicy {
            enabled: true,
            max_line_badness_delta: 500,
            max_total_badness_delta: 2000,
            max_fallback_ratio_percent: 20,
        };
        // 2/10 = 20% == 20% threshold (not exceeded)
        assert_eq!(
            scorecard.gate_status(policy),
            ResizeWrapGateStatus::Pass,
            "gate should pass when fallback ratio is exactly at threshold"
        );
    }

    #[test]
    fn scorecard_record_line_accumulates_correctly() {
        let mut scorecard = ResizeWrapScorecard::default();

        scorecard.record_line(MonospaceLineWrapScorecard {
            mode: MonospaceWrapMode::Dp,
            greedy_total_cost: 50,
            selected_total_cost: 40,
            badness_delta: 100,
            greedy_forced_breaks: 0,
            selected_forced_breaks: 0,
            line_count: 1,
            estimated_states: 10,
            evaluated_states: 8,
        });
        scorecard.record_line(MonospaceLineWrapScorecard {
            mode: MonospaceWrapMode::Fallback,
            greedy_total_cost: 30,
            selected_total_cost: 25,
            badness_delta: 200,
            greedy_forced_breaks: 1,
            selected_forced_breaks: 0,
            line_count: 1,
            estimated_states: 5,
            evaluated_states: 5,
        });
        scorecard.record_line(MonospaceLineWrapScorecard {
            mode: MonospaceWrapMode::Dp,
            greedy_total_cost: 20,
            selected_total_cost: 15,
            badness_delta: 50,
            greedy_forced_breaks: 0,
            selected_forced_breaks: 0,
            line_count: 1,
            estimated_states: 8,
            evaluated_states: 6,
        });

        assert_eq!(scorecard.scored_lines, 3);
        assert_eq!(scorecard.dp_lines, 2);
        assert_eq!(scorecard.fallback_lines, 1);
        assert_eq!(scorecard.greedy_total_cost, 100);
        assert_eq!(scorecard.selected_total_cost, 80);
        assert_eq!(scorecard.max_badness_delta, 200);
        assert_eq!(scorecard.total_badness_delta, 350);
    }

    #[test]
    fn scorecard_machine_payload_contains_all_fields() {
        let scorecard = ResizeWrapScorecard {
            scored_lines: 5,
            dp_lines: 4,
            fallback_lines: 1,
            greedy_total_cost: 500,
            selected_total_cost: 400,
            max_badness_delta: 100,
            total_badness_delta: 300,
        };
        let policy = ResizeReadabilityGatePolicy {
            enabled: true,
            max_line_badness_delta: 500,
            max_total_badness_delta: 2000,
            max_fallback_ratio_percent: 20,
        };
        let payload = scorecard.to_machine_payload(policy);
        assert!(payload.contains("\"gate\":\"resize_wrap_readability\""));
        assert!(payload.contains("\"status\":\"pass\""));
        assert!(payload.contains("\"reason\":null"));
        assert!(payload.contains("\"scored_lines\":5"));
        assert!(payload.contains("\"dp_lines\":4"));
        assert!(payload.contains("\"fallback_lines\":1"));
        assert!(payload.contains("\"max_badness_delta\":100"));
        assert!(payload.contains("\"total_badness_delta\":300"));
        assert!(payload.contains("\"enabled\":true"));
        assert!(payload.contains("\"max_line_badness_delta\":500"));
    }

    #[test]
    fn scorecard_machine_payload_reports_failure_reason() {
        let scorecard = ResizeWrapScorecard {
            scored_lines: 1,
            dp_lines: 1,
            fallback_lines: 0,
            greedy_total_cost: 100,
            selected_total_cost: 80,
            max_badness_delta: 600,
            total_badness_delta: 600,
        };
        let policy = ResizeReadabilityGatePolicy {
            enabled: true,
            max_line_badness_delta: 500,
            max_total_badness_delta: 2000,
            max_fallback_ratio_percent: 20,
        };
        let payload = scorecard.to_machine_payload(policy);
        assert!(payload.contains("\"status\":\"fail\""));
        assert!(payload.contains("\"reason\":\"line_badness_delta_exceeded\""));
    }

    #[test]
    fn fallback_ratio_percent_zero_when_no_lines() {
        let scorecard = ResizeWrapScorecard::default();
        assert_eq!(scorecard.fallback_ratio_percent(), 0);
    }

    #[test]
    fn line_badness_priority_over_total_badness() {
        // When both line and total badness exceed thresholds,
        // line badness should be reported first (checked first).
        let scorecard = ResizeWrapScorecard {
            scored_lines: 1,
            dp_lines: 1,
            fallback_lines: 0,
            greedy_total_cost: 100,
            selected_total_cost: 80,
            max_badness_delta: 600,
            total_badness_delta: 3000,
        };
        let policy = ResizeReadabilityGatePolicy {
            enabled: true,
            max_line_badness_delta: 500,
            max_total_badness_delta: 2000,
            max_fallback_ratio_percent: 20,
        };
        assert_eq!(
            scorecard.gate_status(policy),
            ResizeWrapGateStatus::Fail(ResizeWrapGateFailureReason::LineBadnessDeltaExceeded),
            "line badness check should take priority over total badness"
        );
    }

    // --- Content-specific reflow tests (ft-1memj.29) ---

    #[test]
    fn scorecard_clean_code_reflows_with_low_badness() {
        // Short code-like lines should reflow cleanly with low badness.
        let mut screen = test_screen_with_scorecard(5, 40);
        let attrs = CellAttributes::blank();

        screen.lines = VecDeque::from(vec![
            Line::from_text("fn main() {", &attrs, 0, None),
            Line::from_text("    println!(\"hello\");", &attrs, 0, None),
            Line::from_text("}", &attrs, 0, None),
            Line::new(0),
            Line::new(0),
        ]);

        let cursor = test_cursor(0, 3, 1);
        let _ = screen.resize(test_size(5, 30, 96), cursor, 1, false);

        if let Some(ref scorecard) = screen.last_resize_wrap_scorecard {
            // Clean code that fits should have zero or very low badness.
            // Lines are short enough to not require wrapping at 30 cols.
            assert!(
                scorecard.max_badness_delta <= 500,
                "clean code should have low max badness: got {}",
                scorecard.max_badness_delta
            );
        }
    }

    #[test]
    fn scorecard_wide_line_reflows_with_higher_badness() {
        // A very wide line should cause measurable badness when resized narrow.
        let mut screen = test_screen_with_scorecard(3, 120);
        let attrs = CellAttributes::blank();

        // Create a wide line that will definitely wrap
        let wide_line = "x".repeat(100);
        screen.lines = VecDeque::from(vec![
            Line::from_text_with_wrapped_last_col(&wide_line, &attrs, 0),
            Line::new(0),
            Line::new(0),
        ]);

        let cursor = test_cursor(0, 0, 1);
        let _ = screen.resize(test_size(3, 20, 96), cursor, 1, false);

        if let Some(ref scorecard) = screen.last_resize_wrap_scorecard {
            assert!(
                scorecard.scored_lines > 0,
                "wide line should produce scored lines"
            );
        }
    }

    #[test]
    fn scorecard_default_is_zero() {
        let scorecard = ResizeWrapScorecard::default();
        assert_eq!(scorecard.scored_lines, 0);
        assert_eq!(scorecard.dp_lines, 0);
        assert_eq!(scorecard.fallback_lines, 0);
        assert_eq!(scorecard.greedy_total_cost, 0);
        assert_eq!(scorecard.selected_total_cost, 0);
        assert_eq!(scorecard.max_badness_delta, 0);
        assert_eq!(scorecard.total_badness_delta, 0);
    }

    #[test]
    fn gate_fallback_ratio_triggers_before_total_badness() {
        // When fallback ratio is exceeded but total badness is not,
        // the gate should fail with FallbackRatioExceeded.
        let scorecard = ResizeWrapScorecard {
            scored_lines: 10,
            dp_lines: 5,
            fallback_lines: 5,
            greedy_total_cost: 100,
            selected_total_cost: 80,
            max_badness_delta: 50,
            total_badness_delta: 200,
        };
        let policy = ResizeReadabilityGatePolicy {
            enabled: true,
            max_line_badness_delta: 500,
            max_total_badness_delta: 2000,
            max_fallback_ratio_percent: 20,
        };
        // 5/10 = 50% > 20%
        assert_eq!(
            scorecard.gate_status(policy),
            ResizeWrapGateStatus::Fail(ResizeWrapGateFailureReason::FallbackRatioExceeded),
        );
    }

    #[test]
    fn scorecard_gate_pass_for_zero_scored_lines() {
        // Edge case: no lines scored at all — should still pass gate.
        let scorecard = ResizeWrapScorecard::default();
        let policy = ResizeReadabilityGatePolicy {
            enabled: true,
            max_line_badness_delta: 500,
            max_total_badness_delta: 2000,
            max_fallback_ratio_percent: 20,
        };
        assert_eq!(
            scorecard.gate_status(policy),
            ResizeWrapGateStatus::Pass,
            "zero scored lines should not trigger any gate failure"
        );
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn authoritative_geometry_preserves_busy_and_advances_on_real_eviction() {
        let sink = Arc::new(TestColdScrollbackSink::default());
        let source = Line::from_text("cold", &CellAttributes::blank(), 1, None);
        // Plant cold rows -5..0 in the sink
        {
            let mut rows = sink.rows.lock().unwrap();
            for r in -5..0 {
                rows.insert(r, source.clone());
            }
        }
        let mut screen = test_screen_with_config(
            3,
            8,
            96,
            TestTermConfig {
                cold_sink: Some(sink.clone()),
                ..TestTermConfig::default()
            },
        );

        // Before refresh, direct legacy read queries sink directly and sees -5..3
        assert_eq!(screen.scrollback_geometry(), (-5, 8));
        assert_eq!(screen.scrollback_top_stable_row(), -5);

        // Successful refresh establishes cached cold observation
        assert_eq!(screen.refresh_cold_source_observation(), Ok(Some(true)));
        assert_eq!(screen.observed_scrollback_geometry(), Some((-5, 8)));
        assert_eq!(screen.scrollback_geometry(), (-5, 8));

        // Hold exact cold metadata lock on a background thread to simulate writer contention
        let (lock_tx, lock_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let sink_clone = sink.clone();
        let handle = std::thread::spawn(move || {
            let _guard = sink_clone.rows.lock().unwrap();
            lock_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        lock_rx.recv().unwrap();

        // While lock is held:
        // 1. refresh_cold_source_observation returns Err(ColdReadMetadataBusy)
        assert_eq!(
            screen.refresh_cold_source_observation(),
            Err(ColdReadMetadataBusy)
        );
        // 2. Infallible legacy reads fall back to identity-matched cached observation on Busy,
        //    computing newest from live hot rows rather than fabricating eviction to hot_top (0)
        assert_eq!(screen.scrollback_geometry(), (-5, 8));
        assert_eq!(screen.scrollback_top_stable_row(), -5);

        // Release the lock
        release_tx.send(()).unwrap();
        handle.join().unwrap();

        // Plant real eviction: remove rows -5 and -4 from the sink
        {
            let mut rows = sink.rows.lock().unwrap();
            rows.remove(&-5);
            rows.remove(&-4);
        }

        // Direct legacy read after real eviction WITHOUT preceding try_capture refresh
        // must see actual Ready eviction immediately!
        assert_eq!(
            screen.scrollback_geometry(),
            (-3, 6),
            "direct legacy read must reflect real eviction without requiring a preceding refresh"
        );
        assert_eq!(screen.scrollback_top_stable_row(), -3);

        // Refresh updates the authoritative cached observation
        assert_eq!(screen.refresh_cold_source_observation(), Ok(Some(true)));
        assert_eq!(screen.observed_scrollback_geometry(), Some((-3, 6)));

        // Scrollback erase clears observation and resets
        screen.erase_scrollback().unwrap();
        assert_eq!(screen.observed_scrollback_geometry(), None);
        assert_eq!(screen.refresh_cold_source_observation(), Ok(Some(true)));
        assert_eq!(screen.observed_scrollback_geometry(), Some((0, 3)));
        assert_eq!(screen.scrollback_geometry(), (0, 3));

        // When interval capture returns Unavailable, observed_scrollback_geometry returns None
        #[derive(Debug)]
        struct UnavailableSink;
        impl crate::config::ScrollbackSpillSink for UnavailableSink {
            fn try_capture_scrollback_interval(&self) -> crate::config::ScrollbackIntervalCapture {
                crate::config::ScrollbackIntervalCapture::Unavailable
            }
            fn store_scrollback_line(&self, _: StableRowIndex, _: &Line, _: usize) -> bool {
                false
            }
            fn load_scrollback_line(&self, _: StableRowIndex) -> Option<Line> {
                None
            }
            fn oldest_scrollback_row(&self) -> Option<StableRowIndex> {
                None
            }
            fn retained_scrollback_rows(&self) -> usize {
                0
            }
            fn retained_scrollback_bytes(&self) -> usize {
                0
            }
            fn snapshot_scrollback(
                &self,
                _: StableRowIndex,
                _: crate::config::ScrollbackSnapshotLimits,
            ) -> Result<crate::config::ScrollbackSnapshot, crate::config::ScrollbackSpillError>
            {
                Err(crate::config::ScrollbackSpillError::StorageUnavailable)
            }
            fn replace_scrollback_prefix(
                &self,
                _: Option<crate::config::ScrollbackSnapshotGeneration>,
                _: crate::config::ScrollbackPrefix<'_>,
                _: usize,
            ) -> Result<crate::config::ScrollbackReplaceCommit, crate::config::ScrollbackSpillError>
            {
                Err(crate::config::ScrollbackSpillError::StorageUnavailable)
            }
            fn clear_scrollback(
                &self,
            ) -> Result<crate::config::ScrollbackClearCommit, crate::config::ScrollbackSpillError>
            {
                Err(crate::config::ScrollbackSpillError::StorageUnavailable)
            }
        }
        let mut unavail_screen = test_screen_with_config(
            3,
            8,
            96,
            TestTermConfig {
                cold_sink: Some(Arc::new(UnavailableSink)),
                ..TestTermConfig::default()
            },
        );
        assert_eq!(unavail_screen.refresh_cold_source_observation(), Ok(None));
        assert_eq!(unavail_screen.observed_scrollback_geometry(), None);
    }
}
