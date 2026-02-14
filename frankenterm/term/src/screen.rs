#![allow(clippy::range_plus_one)]
use super::*;
use crate::config::BidiMode;
use crossbeam::thread;
use frankenterm_surface::SequenceNo;
use log::debug;
use std::collections::{hash_map::DefaultHasher, HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Instant;
use termwiz::input::KeyboardEncoding;

/// Holds the model of a screen.  This can either be the primary screen
/// which includes lines of scrollback text, or the alternate screen
/// which holds no scrollback.  The intent is to have one instance of
/// Screen for each of these things.
#[derive(Debug, Clone)]
pub struct Screen {
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
    rewrap_cache: Option<LogicalLineWrapCache>,
    cold_scrollback_worker: ColdScrollbackReflowWorker,
}

const MAX_WRAP_CACHE_ENTRIES: usize = 6;
const MAX_REFLOW_BATCH_LOGICAL_LINES: usize = 64;
const REFLOW_OVERSCAN_ROW_MULTIPLIER: usize = 1;
const REFLOW_OVERSCAN_ROW_CAP: usize = 256;
const COLD_SCROLLBACK_BACKLOG_DEPTH_CAP: usize = 1_048_576;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct WrapCacheKey {
    physical_cols: usize,
    dpi: u32,
}

#[derive(Debug, Clone)]
struct LogicalLineWrapCache {
    source_signature: u64,
    logical_lines: Vec<Line>,
    wrapped_by_key: HashMap<WrapCacheKey, Vec<Vec<Line>>>,
    wrap_key_order: VecDeque<WrapCacheKey>,
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

impl ColdScrollbackReflowWorker {
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
    fn new(source_signature: u64, logical_lines: Vec<Line>) -> Self {
        Self {
            source_signature,
            logical_lines,
            wrapped_by_key: HashMap::new(),
            wrap_key_order: VecDeque::new(),
        }
    }

    fn touch_key(&mut self, key: WrapCacheKey) {
        if let Some(idx) = self.wrap_key_order.iter().position(|k| *k == key) {
            self.wrap_key_order.remove(idx);
        }
        self.wrap_key_order.push_back(key);
    }

    fn get_wrapped(&mut self, key: WrapCacheKey) -> Option<Vec<Vec<Line>>> {
        let wrapped = self.wrapped_by_key.get(&key).cloned();
        if wrapped.is_some() {
            self.touch_key(key);
        }
        wrapped
    }

    fn insert_wrapped(&mut self, key: WrapCacheKey, wrapped: Vec<Vec<Line>>) {
        if !self.wrapped_by_key.contains_key(&key)
            && self.wrapped_by_key.len() >= MAX_WRAP_CACHE_ENTRIES
        {
            if let Some(evicted) = self.wrap_key_order.pop_front() {
                self.wrapped_by_key.remove(&evicted);
            }
        }
        self.wrapped_by_key.insert(key, wrapped);
        self.touch_key(key);
    }

    fn clear_wraps(&mut self) {
        self.wrapped_by_key.clear();
        self.wrap_key_order.clear();
    }
}

fn scrollback_size(config: &Arc<dyn TerminalConfiguration>, allow_scrollback: bool) -> usize {
    if allow_scrollback {
        config.scrollback_size()
    } else {
        0
    }
}

impl Screen {
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
        let physical_rows = size.rows.max(1);
        let physical_cols = size.cols.max(1);

        let mut lines =
            VecDeque::with_capacity(physical_rows + scrollback_size(config, allow_scrollback));
        for _ in 0..physical_rows {
            let mut line = Line::new(seqno);
            bidi_mode.apply_to_line(&mut line, seqno);
            lines.push_back(line);
        }

        Screen {
            lines,
            config: Arc::clone(config),
            allow_scrollback,
            physical_rows,
            physical_cols,
            stable_row_index_offset: 0,
            dpi: size.dpi,
            keyboard_stack: vec![],
            saved_cursor: None,
            rewrap_cache: None,
            cold_scrollback_worker: ColdScrollbackReflowWorker::default(),
        }
    }

    pub fn full_reset(&mut self) {
        self.keyboard_stack.clear();
    }

    fn scrollback_size(&self) -> usize {
        scrollback_size(&self.config, self.allow_scrollback)
    }

    fn mark_visible_lines_dirty(&mut self, seqno: SequenceNo) {
        let start = self.lines.len().saturating_sub(self.physical_rows);
        for idx in start..self.lines.len() {
            self.lines[idx].update_last_change_seqno(seqno);
        }
    }

    fn compute_layout_signature_for_lines<'a, I>(lines: I) -> u64
    where
        I: IntoIterator<Item = &'a Line>,
    {
        let mut hasher = DefaultHasher::new();
        let mut line_count = 0usize;
        for line in lines {
            line_count += 1;
            line.len().hash(&mut hasher);
            line.last_cell_was_wrapped().hash(&mut hasher);
            line.compute_shape_hash().hash(&mut hasher);
        }
        line_count.hash(&mut hasher);
        hasher.finish()
    }

    fn compute_layout_signature(&self) -> u64 {
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
        let mut logical_idx = 0usize;
        let mut prefix_len = 0usize;

        for (phys_idx, line) in self.lines.iter().enumerate() {
            if phys_idx == cursor_y {
                return Some((logical_idx, cursor_x + prefix_len));
            }

            if line.last_cell_was_wrapped() {
                prefix_len += line.len();
            } else {
                logical_idx += 1;
                prefix_len = 0;
            }
        }

        None
    }

    fn rebuild_logical_lines_from_physical(&self, seqno: SequenceNo) -> Vec<Line> {
        let mut logical_lines: Vec<Line> = Vec::with_capacity(self.lines.len());
        let mut logical_line: Option<Line> = None;

        for mut line in self.lines.iter().cloned() {
            line.update_last_change_seqno(seqno);
            let was_wrapped = line.last_cell_was_wrapped();

            if was_wrapped {
                line.set_last_cell_was_wrapped(false, seqno);
            }

            let line = match logical_line.take() {
                None => line,
                Some(mut prior) => {
                    prior.append_line(line, seqno);
                    prior
                }
            };

            if was_wrapped {
                logical_line.replace(line);
                continue;
            }

            logical_lines.push(line);
        }

        if let Some(line) = logical_line.take() {
            logical_lines.push(line);
        }

        logical_lines
    }

    fn logical_wraps_for_resize(
        &mut self,
        source_signature: u64,
        physical_cols: usize,
        seqno: SequenceNo,
    ) -> (Vec<Vec<Line>>, usize, bool, bool, usize) {
        let wrap_key = WrapCacheKey {
            physical_cols,
            dpi: self.dpi,
        };
        let mut logical_cache_hit = false;
        let mut wrap_cache_hit = false;
        let mut cache = self.rewrap_cache.take();

        if let Some(entry) = cache.as_mut() {
            if entry.source_signature == source_signature {
                logical_cache_hit = true;
                let logical_count = entry.logical_lines.len();
                if let Some(wrapped) = entry.get_wrapped(wrap_key) {
                    wrap_cache_hit = true;
                    let cache_entries = entry.wrapped_by_key.len();
                    self.rewrap_cache = cache;
                    return (
                        wrapped,
                        logical_count,
                        logical_cache_hit,
                        wrap_cache_hit,
                        cache_entries,
                    );
                }

                let wrapped = self.wrap_logical_lines_for_resize(
                    entry.logical_lines.clone(),
                    physical_cols,
                    seqno,
                    Some(&self.build_viewport_reflow_plan_for_current_snapshot(logical_count)),
                );
                entry.insert_wrapped(wrap_key, wrapped.clone());
                let cache_entries = entry.wrapped_by_key.len();
                self.rewrap_cache = cache;
                return (
                    wrapped,
                    logical_count,
                    logical_cache_hit,
                    wrap_cache_hit,
                    cache_entries,
                );
            }
        }

        let logical_lines = self.rebuild_logical_lines_from_physical(seqno);
        let logical_count = logical_lines.len();
        let wrapped = self.wrap_logical_lines_for_resize(
            logical_lines.clone(),
            physical_cols,
            seqno,
            Some(&self.build_viewport_reflow_plan_for_current_snapshot(logical_count)),
        );
        let mut new_cache = LogicalLineWrapCache::new(source_signature, logical_lines);
        new_cache.insert_wrapped(wrap_key, wrapped.clone());
        let cache_entries = new_cache.wrapped_by_key.len();
        self.rewrap_cache = Some(new_cache);
        (
            wrapped,
            logical_count,
            logical_cache_hit,
            wrap_cache_hit,
            cache_entries,
        )
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

    fn wrap_logical_lines_for_resize(
        &mut self,
        logical_lines: Vec<Line>,
        physical_cols: usize,
        seqno: SequenceNo,
        reflow_plan: Option<&ViewportReflowPlan>,
    ) -> Vec<Vec<Line>> {
        let logical_count = logical_lines.len();
        if logical_count == 0 {
            return vec![];
        }

        let mut wrapped_by_index: Vec<Option<Vec<Line>>> = vec![None; logical_count];
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
        let mut cold_lines_completed = 0usize;
        let mut cold_batches_completed = 0usize;

        let worker_count = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(logical_count);

        for (batch_idx, batch) in plan.batches.iter().enumerate() {
            let batch_len = batch
                .logical_range
                .end
                .saturating_sub(batch.logical_range.start);
            if batch_len == 0 {
                continue;
            }

            debug!(
                "reflow planner batch={} logical={}..{} priority={} rationale={}",
                batch_idx,
                batch.logical_range.start,
                batch.logical_range.end,
                batch.priority.as_str(),
                batch.priority.rationale()
            );

            let batch_workers = worker_count.min(batch_len);
            if batch_workers <= 1 || batch_len < batch_workers.saturating_mul(8) {
                for idx in batch.logical_range.clone() {
                    let line = logical_lines[idx].clone();
                    let wrapped = if line.len() <= physical_cols {
                        vec![line]
                    } else {
                        line.wrap(physical_cols, seqno)
                    };
                    wrapped_by_index[idx] = Some(wrapped);
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
            let _ = thread::scope(|scope| {
                let mut handles = Vec::with_capacity(batch_workers);
                for worker_idx in 0..batch_workers {
                    let start = batch.logical_range.start + worker_idx * chunk_size;
                    if start >= batch.logical_range.end {
                        break;
                    }
                    let end = (start + chunk_size).min(batch.logical_range.end);
                    let logical_slice = &logical_lines[start..end];
                    handles.push(scope.spawn(move |_| {
                        let mut wrapped = Vec::with_capacity(end - start);
                        for (offset, line) in logical_slice.iter().enumerate() {
                            let idx = start + offset;
                            let wrapped_lines = if line.len() <= physical_cols {
                                vec![line.clone()]
                            } else {
                                line.clone().wrap(physical_cols, seqno)
                            };
                            wrapped.push((idx, wrapped_lines));
                        }
                        wrapped
                    }));
                }

                for handle in handles {
                    for (idx, lines) in handle.join().unwrap() {
                        wrapped_by_index[idx] = Some(lines);
                    }
                }
            });

            if batch.priority == ReflowBatchPriority::ColdScrollback {
                self.cold_scrollback_worker
                    .complete_cold_batch(seqno, batch_len);
                cold_lines_completed = cold_lines_completed.saturating_add(batch_len);
                cold_batches_completed = cold_batches_completed.saturating_add(1);
            }
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
            self.cold_scrollback_worker.completion_throughput_lines_per_sec(),
            self.cold_scrollback_worker.cancellation_count()
        );

        for (idx, slot) in wrapped_by_index.iter_mut().enumerate() {
            if slot.is_some() {
                continue;
            }
            let line = logical_lines[idx].clone();
            let wrapped = if line.len() <= physical_cols {
                vec![line]
            } else {
                line.wrap(physical_cols, seqno)
            };
            *slot = Some(wrapped);
        }

        wrapped_by_index
            .into_iter()
            .map(|lines| lines.expect("missing wrapped line result after planner"))
            .collect()
    }

    fn rewrap_lines(
        &mut self,
        physical_cols: usize,
        physical_rows: usize,
        cursor_x: usize,
        cursor_y: PhysRowIndex,
        seqno: SequenceNo,
    ) -> (usize, PhysRowIndex) {
        let started = Instant::now();
        let old_cols = self.physical_cols;
        let original_len = self.lines.len();
        let estimated_capacity = if physical_cols >= self.physical_cols {
            original_len
        } else {
            original_len
                .saturating_mul(self.physical_cols.max(1))
                .checked_div(physical_cols.max(1))
                .unwrap_or(original_len)
                .max(original_len)
        };
        let source_signature = self.compute_layout_signature();
        let logical_cursor = self.logical_cursor_from_physical(cursor_x, cursor_y);
        let (wrapped, logical_count, logical_cache_hit, wrap_cache_hit, cache_entries) =
            self.logical_wraps_for_resize(source_signature, physical_cols, seqno);
        let mut adjusted_cursor = (cursor_x, cursor_y);
        let wrapped_count: usize = wrapped.iter().map(Vec::len).sum();
        if let Some((logical_idx, logical_x)) = logical_cursor {
            let num_lines = logical_x / physical_cols;
            let last_x = logical_x - (num_lines * physical_cols);
            let row_base = wrapped
                .iter()
                .take(logical_idx)
                .map(Vec::len)
                .sum::<usize>();
            adjusted_cursor = (last_x, row_base + num_lines);

            // Special case: if the cursor lands in column zero, we'll
            // lose track of its logical association with the wrapped
            // line and it won't resize with the line correctly.
            // Put it back on the prior line. The cursor is now
            // technically outside of the viewport width.
            if adjusted_cursor.0 == 0 && adjusted_cursor.1 > 0 {
                if physical_cols < self.physical_cols {
                    // getting smaller: preserve its original position
                    // on the prior line
                    adjusted_cursor.0 = cursor_x;
                } else {
                    // getting larger; we were most likely in column 1
                    // or somewhere close. Jump to the end of the
                    // prior line.
                    adjusted_cursor.0 = physical_cols;
                }
                adjusted_cursor.1 -= 1;
            }
        }

        let mut rewrapped = VecDeque::with_capacity(estimated_capacity.max(physical_rows));
        let mut pruned_rows = 0usize;
        for lines in wrapped {
            for mut line in lines {
                line.update_last_change_seqno(seqno);
                rewrapped.push_back(line);
            }
        }
        self.lines = rewrapped;

        // If we resized narrower and generated additional lines,
        // we may need to scroll the lines to make room.  However,
        // if the bottom line(s) are whitespace, we'll prune those
        // out first in the rewrap case so that we don't lose any
        // real information off the top of the scrollback
        let capacity = physical_rows + self.scrollback_size();
        while self.lines.len() > capacity
            && self.lines.back().map(Line::is_whitespace).unwrap_or(false)
        {
            self.lines.pop_back();
            pruned_rows += 1;
        }

        if pruned_rows > 0 {
            self.rewrap_cache = None;
        } else {
            let layout_signature = self.compute_layout_signature();
            if let Some(cache) = self.rewrap_cache.as_mut() {
                cache.source_signature = layout_signature;
            }
        }

        let final_cache_entries = self
            .rewrap_cache
            .as_ref()
            .map(|cache| cache.wrapped_by_key.len())
            .unwrap_or(0);

        debug!(
            "rewrap_lines cols={}→{} physical_lines={} logical_lines={} rewrapped_lines={} cache.logical={} cache.wrap={} cache.entries={} pruned_rows={} elapsed_ms={}",
            old_cols,
            physical_cols,
            original_len,
            logical_count,
            wrapped_count,
            logical_cache_hit,
            wrap_cache_hit,
            final_cache_entries.max(cache_entries),
            pruned_rows,
            started.elapsed().as_millis()
        );

        adjusted_cursor
    }

    /// Resize the physical, viewable portion of the screen
    pub fn resize(
        &mut self,
        size: TerminalSize,
        cursor: CursorPosition,
        seqno: SequenceNo,
        is_conpty: bool,
    ) -> CursorPosition {
        let physical_rows = size.rows.max(1);
        let physical_cols = size.cols.max(1);

        if physical_rows == self.physical_rows
            && physical_cols == self.physical_cols
            && size.dpi == self.dpi
        {
            return cursor;
        }
        log::debug!(
            "resize screen to {physical_cols}x{physical_rows} dpi={}",
            size.dpi
        );
        let dpi_changed = self.dpi != size.dpi;
        self.dpi = size.dpi;
        if dpi_changed {
            if let Some(cache) = self.rewrap_cache.as_mut() {
                cache.clear_wraps();
            }
        }

        // pre-prune blank lines that range from the cursor position to the end of the display;
        // this avoids growing the scrollback size when rapidly switching between normal and
        // maximized states.
        let cursor_phys = self.phys_row(cursor.y);
        for _ in cursor_phys + 1..self.lines.len() {
            if self.lines.back().map(Line::is_whitespace).unwrap_or(false) {
                self.lines.pop_back();
            }
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
                    self.rewrap_lines(physical_cols, physical_rows, cursor.x, cursor_phys, seqno)
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

        let capacity = physical_rows + self.scrollback_size();
        let current_capacity = self.lines.capacity();
        if capacity > current_capacity {
            self.lines.reserve(capacity - current_capacity);
        }

        // If we resized wider and the rewrap resulted in fewer
        // lines than the viewport size, or we resized taller,
        // pad us back out to the viewport size
        while self.lines.len() < physical_rows {
            // FIXME: borrow bidi mode from line
            self.lines.push_back(Line::new(seqno));
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
                // FIXME: borrow bidi mode from line
                self.lines.push_back(Line::new(seqno));
            }
        } else {
            // Compute the new cursor location; this is logically the inverse
            // of the phys_row() function, but given the revised cursor_y
            // (the rewrap adjusted physical row of the cursor).  This
            // computes its new VisibleRowIndex given the new viewport size.
            new_cursor_y = cursor_y as VisibleRowIndex
                - (self.lines.len() as VisibleRowIndex - physical_rows as VisibleRowIndex);
        }

        self.physical_rows = physical_rows;
        self.physical_cols = physical_cols;
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

    /// Returns the number of occupied rows of scrollback
    pub fn scrollback_rows(&self) -> usize {
        self.lines.len()
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
        let line_idx = self.phys_row(y);
        let line = self.line_mut(line_idx);
        line.erase_cell_with_margin(x, right_margin, seqno, blank_attr);
    }

    /// Set a cell.  the x and y coordinates are relative to the visible screeen
    /// origin.  0,0 is the top left.
    pub fn set_cell(&mut self, x: usize, y: VisibleRowIndex, cell: &Cell, seqno: SequenceNo) {
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
        let line_idx = self.phys_row(y);
        let line = self.line_mut(line_idx);
        line.set_cell_grapheme(x, text, width, attr, seqno);
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
        let line_idx = self.phys_row(y);
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
        ((self.lines.len() - self.physical_rows) as ScrollbackOrVisibleRowIndex + row).max(0)
            as usize
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
        let range_len = (range.end - range.start) as usize;

        let first = match self.stable_row_to_phys(range.start) {
            Some(first) => first,
            None => {
                return 0..range_len.min(self.lines.len());
            }
        };

        let last = match self.stable_row_to_phys(range.end.saturating_sub(1)) {
            Some(last) => last,
            None => {
                let last = self.lines.len() - 1;
                return last.saturating_sub(range_len)..last + 1;
            }
        };

        first..last + 1
    }

    /// Translate a range of VisibleRowIndex to a range of PhysRowIndex.
    /// The resultant range will be invalidated by inserting or removing rows!
    #[inline]
    pub fn phys_range(&self, range: &Range<VisibleRowIndex>) -> Range<PhysRowIndex> {
        self.phys_row(range.start)..self.phys_row(range.end)
    }

    #[inline]
    pub fn phys_to_stable_row_index(&self, phys: PhysRowIndex) -> StableRowIndex {
        (phys + self.stable_row_index_offset) as StableRowIndex
    }

    #[inline]
    pub fn stable_row_to_phys(&self, stable: StableRowIndex) -> Option<PhysRowIndex> {
        let idx = stable - self.stable_row_index_offset as isize;
        if idx < 0 || idx >= self.lines.len() as isize {
            // Index is no longer valid
            None
        } else {
            Some(idx as PhysRowIndex)
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
        log::debug!(
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
        let phys_scroll = self.phys_range(scroll_region);
        let num_rows = num_rows.min(phys_scroll.end - phys_scroll.start);
        let scrollback_ok = scroll_region.start == 0 && self.allow_scrollback;
        let insert_at_end = scroll_region.end as usize == self.physical_rows;

        debug!(
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
        } else {
            let max_allowed = self.physical_rows + self.scrollback_size();
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
        let (to_remove, to_add) = {
            for _ in 0..to_move {
                let mut line = self.lines.remove(remove_idx).unwrap();
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
                    self.lines.insert(phys_scroll.end - 1, line);
                }
            }
            // We may still have some lines to add at the bottom, so
            // return revised counts for remove/add
            (lines_removed - to_move, num_rows - to_move)
        };

        // Perform the removal
        for _ in 0..to_remove {
            self.lines.remove(remove_idx);
        }

        if remove_idx == 0 && scrollback_ok {
            self.stable_row_index_offset += lines_removed;
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
    }

    pub fn erase_scrollback(&mut self) {
        let len = self.lines.len();
        let to_clear = len - self.physical_rows;
        for _ in 0..to_clear {
            self.lines.pop_front();
            if self.allow_scrollback {
                self.stable_row_index_offset += 1;
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
        debug!("scroll_down {:?} {}", scroll_region, num_rows);
        let phys_scroll = self.phys_range(scroll_region);
        let num_rows = num_rows.min(phys_scroll.end - phys_scroll.start);

        let middle = phys_scroll.end - num_rows;

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

    pub fn get_changed_stable_rows(
        &self,
        stable_lines: Range<StableRowIndex>,
        seqno: SequenceNo,
    ) -> Vec<StableRowIndex> {
        let phys = self.stable_range(&stable_lines);
        let mut set = vec![];
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
        for line in &second[second_range] {
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
            if prior.len() + back_len > MAX_LOGICAL_LINE_LEN {
                break;
            }
            back_len += prior.len();
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
                    if total_len > 0 && total_len + line.len() > MAX_LOGICAL_LINE_LEN {
                        break;
                    }
                    end_inclusive = idx;
                    total_len += line.len();
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
            if prior.len() + back_len > MAX_LOGICAL_LINE_LEN {
                break;
            }
            back_len += prior.len();
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
                    if total_len > 0 && total_len + line.len() > MAX_LOGICAL_LINE_LEN {
                        break;
                    }
                    end_inclusive = idx;
                    total_len += line.len();
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
mod tests {
    use super::*;
    use crate::color::ColorPalette;
    use frankenterm_bidi::ParagraphDirectionHint;
    use frankenterm_cell::{Cell, CellAttributes};
    use frankenterm_surface::{CursorShape, CursorVisibility};

    use std::sync::Arc;

    #[derive(Debug)]
    struct TestTermConfig {
        scrollback: usize,
    }

    impl TerminalConfiguration for TestTermConfig {
        fn scrollback_size(&self) -> usize {
            self.scrollback
        }

        fn color_palette(&self) -> ColorPalette {
            ColorPalette::default()
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

    fn test_screen(rows: usize, cols: usize, dpi: u32) -> Screen {
        let config: Arc<dyn TerminalConfiguration> = Arc::new(TestTermConfig { scrollback: 32 });
        Screen::new(test_size(rows, cols, dpi), &config, true, 0, bidi_mode())
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
}
