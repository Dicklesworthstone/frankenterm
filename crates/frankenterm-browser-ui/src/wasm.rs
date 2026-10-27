//! JavaScript bindings for the fleet viewer and terminal codec (wasm32 only).

use js_sys::{Object, Reflect, Uint8Array, Uint32Array};
use wasm_bindgen::prelude::*;

use crate::model::{StreamState, View};
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

    /// Explicit inspection target from the selected pane or detection row.
    /// Return a decimal string, never a JavaScript Number that rounds u64 IDs.
    #[wasm_bindgen(js_name = selectedPaneId)]
    pub fn selected_pane_id(&self) -> Option<String> {
        let model = self.inner.model();
        let pane_id = match model.active_view() {
            View::Panes => model.panes().get(model.selected()).map(|pane| pane.pane_id),
            View::Events => model.events().get(model.selected()).map(|event| event.pane_id),
        };
        pane_id.map(|id| id.to_string())
    }

    /// Flat cells and spans for the renderer's applyPatchBatchFlat method.
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

fn wire_error(error: frankenterm_terminal_wire::WireError) -> JsValue {
    JsValue::from_str(error.code())
}

/// Check the JS array length before copying into WASM memory. A fixed bounded
/// subarray prevents a length-tracking view from enlarging the copied window.
fn wire_bytes(array: &Uint8Array) -> Result<Vec<u8>, JsValue> {
    let length = array.length();
    let byte_count = usize::try_from(length)
        .map_err(|_| wire_error(frankenterm_terminal_wire::WireError::FrameTooLarge))?;
    if byte_count > frankenterm_terminal_wire::MAX_FRAME_BYTES {
        return Err(wire_error(frankenterm_terminal_wire::WireError::FrameTooLarge));
    }
    let bounded = array.subarray(0, length);
    let mut bytes = vec![0; byte_count];
    bounded.copy_to(&mut bytes);
    Ok(bytes)
}

/// Explicit binding to the shared Rust codec. It opens no network connection
/// and writes no terminal input. Network frames never enter through a string
/// or slice ABI that would copy unbounded data before Rust can check its size.
#[wasm_bindgen]
pub struct TerminalWireClient {
    inner: crate::terminal::TerminalClient,
}

#[wasm_bindgen]
impl TerminalWireClient {
    #[wasm_bindgen(constructor)]
    pub fn new(hello_frame: &Uint8Array) -> Result<TerminalWireClient, JsValue> {
        let bytes = wire_bytes(hello_frame)?;
        Ok(Self {
            inner: crate::terminal::TerminalClient::from_hello_frame(&bytes).map_err(wire_error)?,
        })
    }

    #[wasm_bindgen(js_name = helloFrame)]
    pub fn hello_frame(&mut self) -> Result<Vec<u8>, JsValue> {
        self.inner.hello_frame().map_err(wire_error)
    }

    #[wasm_bindgen(js_name = acceptWelcome)]
    pub fn accept_welcome(&mut self, frame: &Uint8Array) -> Result<(), JsValue> {
        self.inner.accept_welcome(&wire_bytes(frame)?).map_err(wire_error)
    }

    /// Canonical JSON after Rust validation; all decimal IDs remain strings.
    #[wasm_bindgen(js_name = decodeServer)]
    pub fn decode_server(&self, frame: &Uint8Array) -> Result<String, JsValue> {
        self.inner.decode_server(&wire_bytes(frame)?).map_err(wire_error)
    }

    #[wasm_bindgen(js_name = encodeRequest)]
    pub fn encode_request(&mut self, message: &Uint8Array) -> Result<Vec<u8>, JsValue> {
        let bytes = wire_bytes(message)?;
        let json = std::str::from_utf8(&bytes)
            .map_err(|_| wire_error(frankenterm_terminal_wire::WireError::Malformed))?;
        self.inner.encode_request(json).map_err(wire_error)
    }

    #[wasm_bindgen(js_name = bindScope)]
    pub fn bind_scope(&mut self, scope_json: &str, dimensions_json: &str, delivered: &str) -> Result<(), JsValue> {
        self.inner.bind_scope(scope_json, dimensions_json, delivered).map_err(wire_error)
    }

    #[wasm_bindgen(js_name = unbindScope)]
    pub fn unbind_scope(&mut self, scope_json: &str) -> Result<(), JsValue> {
        self.inner.unbind_scope(scope_json).map_err(wire_error)
    }

    #[wasm_bindgen(js_name = updateDimensions)]
    pub fn update_dimensions(&mut self, scope_json: &str, dimensions_json: &str) -> Result<(), JsValue> {
        self.inner.update_dimensions(scope_json, dimensions_json).map_err(wire_error)
    }

    /// Host-model application only. This emits no ACK and observes no paint.
    #[wasm_bindgen(js_name = recordDelivery)]
    pub fn record_delivery(&mut self, scope_json: &str, previous: &str, sequence: &str) -> Result<(), JsValue> {
        self.inner.record_delivery(scope_json, previous, sequence).map_err(wire_error)
    }

    #[wasm_bindgen(js_name = recordLease)]
    pub fn record_lease(&mut self, scope_json: &str, fence: &str, active: bool) -> Result<(), JsValue> {
        self.inner.record_lease(scope_json, fence, active).map_err(wire_error)
    }
}
