//! Host-driven runner: the [`FleetModel`] inside ftui-web's `StepProgram`.
//!
//! The browser host calls these methods from its animation-frame loop and
//! forwards the flat patches to the renderer. Nothing here blocks, spawns or
//! touches the network, so the same runner is exercised by native tests.

use core::time::Duration;

use ftui_core::event::Event;
use ftui_render::buffer::Buffer;
use ftui_web::step_program::StepProgram;

use crate::model::{FleetModel, StreamState};
use crate::wire::WireError;

/// Grid bounds accepted from the host.
pub const MIN_COLS: u16 = 20;
pub const MIN_ROWS: u16 = 4;
pub const MAX_COLS: u16 = 1000;
pub const MAX_ROWS: u16 = 400;

/// What one [`FleetRunner::step`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepOutcome {
    pub rendered: bool,
    pub events_processed: u32,
    pub frame_idx: u64,
}

/// The fleet viewer program plus the flat patch buffers for the renderer.
pub struct FleetRunner {
    program: StepProgram<FleetModel>,
    flat_cells: Vec<u32>,
    flat_spans: Vec<u32>,
    /// The frame whose patches were last handed to the renderer.
    presented: Option<Buffer>,
}

fn clamp_size(cols: u16, rows: u16) -> (u16, u16) {
    (
        cols.clamp(MIN_COLS, MAX_COLS),
        rows.clamp(MIN_ROWS, MAX_ROWS),
    )
}

impl FleetRunner {
    #[must_use]
    pub fn new(cols: u16, rows: u16) -> Self {
        let (cols, rows) = clamp_size(cols, rows);
        Self {
            program: StepProgram::new(FleetModel::new(), cols, rows),
            flat_cells: Vec::new(),
            flat_spans: Vec::new(),
            presented: None,
        }
    }

    /// Initialize the program and render the first frame.
    pub fn init(&mut self) -> Result<(), String> {
        self.program.init().map_err(|e| format!("{e:?}"))
    }

    /// Process queued input and redraw if anything changed.
    pub fn step(&mut self) -> Result<StepOutcome, String> {
        let result = self.program.step().map_err(|e| format!("{e:?}"))?;
        Ok(StepOutcome {
            rendered: result.rendered,
            events_processed: result.events_processed,
            frame_idx: result.frame_idx,
        })
    }

    /// Queue renderer input (the renderer's encoded-input JSON).
    /// Returns false for input the parser does not map to an event, or when
    /// the input queue is full.
    pub fn push_encoded_input(&mut self, json: &str) -> bool {
        match ftui_web::input_parser::parse_encoded_input_to_event(json) {
            Ok(Some(event)) => self.program.push_event(event).is_ok(),
            _ => false,
        }
    }

    /// Queue a terminal-style event directly (tests and host shortcuts).
    pub fn push_event(&mut self, event: Event) -> bool {
        self.program.push_event(event).is_ok()
    }

    /// Resize the grid; returns false when the input queue is full.
    pub fn resize(&mut self, cols: u16, rows: u16) -> bool {
        let (cols, rows) = clamp_size(cols, rows);
        self.program.resize(cols, rows).is_ok()
    }

    /// Advance program time (drives ticks) and the model's wall clock.
    pub fn set_time(&mut self, now_ms: f64) {
        let ms = if now_ms.is_finite() && now_ms > 0.0 {
            now_ms as i64
        } else {
            0
        };
        self.program.set_time(Duration::from_millis(ms as u64));
        self.program.model_mut().set_now_ms(ms);
        // Ages in the header change with time even when no data arrives.
        self.program.request_redraw();
    }

    pub fn ingest_panes(&mut self, body: &str) -> Result<usize, WireError> {
        let result = self.program.model_mut().ingest_panes(body);
        self.program.request_redraw();
        result
    }

    pub fn ingest_events(&mut self, body: &str) -> Result<usize, WireError> {
        let result = self.program.model_mut().ingest_events(body);
        self.program.request_redraw();
        result
    }

    pub fn ingest_stream_frame(&mut self, payload: &str) -> Result<(), WireError> {
        let result = self.program.model_mut().ingest_stream_frame(payload);
        self.program.request_redraw();
        result
    }

    pub fn set_stream_state(&mut self, state: StreamState) {
        self.program.model_mut().set_stream_state(state);
        self.program.request_redraw();
    }

    pub fn report_error(&mut self, message: &str) {
        self.program.model_mut().report_error(message);
        self.program.request_redraw();
    }

    /// True once after the operator asked for a refresh.
    pub fn take_refresh_request(&mut self) -> bool {
        self.program.model_mut().take_refresh_request()
    }

    /// Flatten the last frame's patches into `[bg, fg, glyph, attrs]` cells
    /// and `[offset, len]` spans, the renderer's `applyPatchBatchFlat` input.
    pub fn prepare_flat_patches(&mut self) {
        let outputs = self.program.take_outputs();
        let batch = outputs.flatten_patches_u32();
        self.flat_cells = batch.cells;
        self.flat_spans = batch.spans;
        if outputs.last_buffer.is_some() {
            self.presented = outputs.last_buffer;
        }
    }

    #[must_use]
    pub fn flat_cells(&self) -> &[u32] {
        &self.flat_cells
    }

    #[must_use]
    pub fn flat_spans(&self) -> &[u32] {
        &self.flat_spans
    }

    #[must_use]
    pub fn size(&self) -> (u16, u16) {
        self.program.size()
    }

    #[must_use]
    pub fn model(&self) -> &FleetModel {
        self.program.model()
    }

    /// The text of the last rendered frame, one string per row: the bounded
    /// mirror the host exposes to accessibility and to browser tests.
    #[must_use]
    pub fn screen_text(&self) -> Vec<String> {
        // A frame rendered but not yet handed over is newer than the presented one.
        let Some(buffer) = self
            .program
            .outputs()
            .last_buffer
            .as_ref()
            .or(self.presented.as_ref())
        else {
            return Vec::new();
        };
        (0..buffer.height())
            .map(|y| {
                (0..buffer.width())
                    .map(|x| {
                        buffer
                            .get(x, y)
                            .and_then(|cell| cell.content.as_char())
                            .unwrap_or(' ')
                    })
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ftui_core::event::{KeyCode, KeyEvent, KeyEventKind, Modifiers};

    const PANES: &str = r#"{"ok":true,"version":"x","data":{"total":1,"panes":[
        {"pane_id":3,"domain":"local","title":"agent-3","first_seen_at":1,"last_seen_at":1}]}}"#;

    #[test]
    fn first_frame_is_a_full_grid_of_patches() {
        let mut runner = FleetRunner::new(80, 24);
        runner.init().expect("init");
        runner.prepare_flat_patches();
        let cells: usize = runner
            .flat_spans()
            .chunks(2)
            .map(|span| span[1] as usize)
            .sum();
        assert_eq!(cells, 80 * 24, "first frame covers the grid");
        assert_eq!(runner.flat_cells().len(), cells * 4, "four words per cell");
    }

    #[test]
    fn ingest_then_step_repaints_only_what_changed() {
        let mut runner = FleetRunner::new(80, 24);
        runner.init().expect("init");
        runner.prepare_flat_patches();
        runner.set_time(5_000.0);
        assert_eq!(runner.ingest_panes(PANES), Ok(1));
        let outcome = runner.step().expect("step");
        assert!(outcome.rendered);
        runner.prepare_flat_patches();
        let changed: usize = runner
            .flat_spans()
            .chunks(2)
            .map(|span| span[1] as usize)
            .sum();
        assert!(
            changed > 0 && changed < 80 * 24,
            "incremental patch, got {changed}"
        );
        assert!(
            runner
                .screen_text()
                .iter()
                .any(|row| row.contains("agent-3")),
            "{:?}",
            runner.screen_text()
        );
    }

    #[test]
    fn renderer_input_drives_the_model() {
        let mut runner = FleetRunner::new(80, 24);
        runner.init().expect("init");
        // The renderer's encoded form of a Tab key press.
        assert!(runner.push_encoded_input(
            r#"{"kind":"key","phase":"down","key":"Tab","code":"Tab","mods":0,"repeat":false}"#
        ));
        runner.step().expect("step");
        assert_eq!(runner.model().active_view(), crate::model::View::Events);
        assert!(!runner.push_encoded_input("not json"));
        assert!(runner.push_event(Event::Key(KeyEvent {
            code: KeyCode::Char('1'),
            modifiers: Modifiers::empty(),
            kind: KeyEventKind::Press,
        })));
        runner.step().expect("step");
        assert_eq!(runner.model().active_view(), crate::model::View::Panes);
    }

    #[test]
    fn sizes_are_clamped() {
        assert_eq!(FleetRunner::new(0, 0).size(), (MIN_COLS, MIN_ROWS));
        assert_eq!(
            FleetRunner::new(u16::MAX, u16::MAX).size(),
            (MAX_COLS, MAX_ROWS)
        );
        let mut runner = FleetRunner::new(80, 24);
        runner.init().expect("init");
        assert!(runner.resize(120, 40));
        runner.step().expect("step");
        assert_eq!(runner.size(), (120, 40));
    }
}
