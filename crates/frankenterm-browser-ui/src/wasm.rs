//! JavaScript bindings for the fleet viewer (wasm32 only).

use js_sys::{Object, Reflect, Uint32Array};
use wasm_bindgen::prelude::*;

use crate::model::StreamState;
use crate::runner::FleetRunner;

/// The fleet viewer as the browser host sees it.
#[wasm_bindgen]
pub struct FleetViewer {
    inner: FleetRunner,
}

#[wasm_bindgen]
impl FleetViewer {
    #[wasm_bindgen(constructor)]
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            inner: FleetRunner::new(cols, rows),
        }
    }

    pub fn init(&mut self) -> Result<(), JsValue> {
        self.inner.init().map_err(|e| JsValue::from_str(&e))
    }

    /// Process queued input; returns whether a new frame was rendered.
    pub fn step(&mut self) -> Result<bool, JsValue> {
        self.inner
            .step()
            .map(|outcome| outcome.rendered)
            .map_err(|e| JsValue::from_str(&e))
    }

    #[wasm_bindgen(js_name = pushEncodedInput)]
    pub fn push_encoded_input(&mut self, json: &str) -> bool {
        self.inner.push_encoded_input(json)
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> bool {
        self.inner.resize(cols, rows)
    }

    #[wasm_bindgen(js_name = setTime)]
    pub fn set_time(&mut self, now_ms: f64) {
        self.inner.set_time(now_ms);
    }

    /// Returns the pane count, or throws the decoding error.
    #[wasm_bindgen(js_name = ingestPanes)]
    pub fn ingest_panes(&mut self, body: &str) -> Result<u32, JsValue> {
        self.inner
            .ingest_panes(body)
            .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
            .map_err(|e| JsValue::from_str(&e.0))
    }

    /// Returns the detection count, or throws the decoding error.
    #[wasm_bindgen(js_name = ingestEvents)]
    pub fn ingest_events(&mut self, body: &str) -> Result<u32, JsValue> {
        self.inner
            .ingest_events(body)
            .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
            .map_err(|e| JsValue::from_str(&e.0))
    }

    #[wasm_bindgen(js_name = ingestStreamFrame)]
    pub fn ingest_stream_frame(&mut self, payload: &str) -> Result<(), JsValue> {
        self.inner
            .ingest_stream_frame(payload)
            .map_err(|e| JsValue::from_str(&e.0))
    }

    /// 0 = disconnected, 1 = connecting, 2 = live.
    #[wasm_bindgen(js_name = setStreamState)]
    pub fn set_stream_state(&mut self, state: u8) {
        self.inner.set_stream_state(match state {
            2 => StreamState::Live,
            1 => StreamState::Connecting,
            _ => StreamState::Disconnected,
        });
    }

    #[wasm_bindgen(js_name = reportError)]
    pub fn report_error(&mut self, message: &str) {
        self.inner.report_error(message);
    }

    #[wasm_bindgen(js_name = takeRefreshRequest)]
    pub fn take_refresh_request(&mut self) -> bool {
        self.inner.take_refresh_request()
    }

    /// `{cells, spans}` for the renderer's `applyPatchBatchFlat(spans, cells)`.
    #[wasm_bindgen(js_name = takeFlatPatches)]
    pub fn take_flat_patches(&mut self) -> JsValue {
        self.inner.prepare_flat_patches();
        let object = Object::new();
        let _ = Reflect::set(
            &object,
            &"cells".into(),
            &Uint32Array::from(self.inner.flat_cells()).into(),
        );
        let _ = Reflect::set(
            &object,
            &"spans".into(),
            &Uint32Array::from(self.inner.flat_spans()).into(),
        );
        object.into()
    }

    /// Text of the last frame, rows joined by newlines (accessibility mirror).
    #[wasm_bindgen(js_name = screenText)]
    pub fn screen_text(&self) -> String {
        self.inner.screen_text().join("\n")
    }
}
