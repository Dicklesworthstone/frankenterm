//! JavaScript bindings for the fleet viewer and applied terminal model.

use js_sys::{Object, Reflect, Uint8Array, Uint32Array};
use wasm_bindgen::prelude::*;

use crate::model::{StreamState, View};
use crate::runner::FleetRunner;

#[wasm_bindgen]
pub struct FleetViewer { inner: FleetRunner }

#[wasm_bindgen]
impl FleetViewer {
    #[wasm_bindgen(constructor)]
    pub fn new(cols: u16, rows: u16) -> Self { Self { inner: FleetRunner::new(cols, rows) } }

    pub fn init(&mut self) -> Result<(), JsValue> {
        self.inner.init().map_err(|e| JsValue::from_str(&e))
    }

    pub fn step(&mut self) -> Result<bool, JsValue> {
        self.inner.step().map(|outcome| outcome.rendered).map_err(|e| JsValue::from_str(&e))
    }

    #[wasm_bindgen(js_name = pushEncodedInput)]
    pub fn push_encoded_input(&mut self, json: &str) -> bool { self.inner.push_encoded_input(json) }
    pub fn resize(&mut self, cols: u16, rows: u16) -> bool { self.inner.resize(cols, rows) }
    #[wasm_bindgen(js_name = setTime)]
    pub fn set_time(&mut self, now_ms: f64) { self.inner.set_time(now_ms); }

    #[wasm_bindgen(js_name = ingestPanes)]
    pub fn ingest_panes(&mut self, body: &str) -> Result<u32, JsValue> {
        self.inner.ingest_panes(body).map(|n| u32::try_from(n).unwrap_or(u32::MAX))
            .map_err(|e| JsValue::from_str(&e.0))
    }
    #[wasm_bindgen(js_name = ingestEvents)]
    pub fn ingest_events(&mut self, body: &str) -> Result<u32, JsValue> {
        self.inner.ingest_events(body).map(|n| u32::try_from(n).unwrap_or(u32::MAX))
            .map_err(|e| JsValue::from_str(&e.0))
    }
    #[wasm_bindgen(js_name = ingestStreamFrame)]
    pub fn ingest_stream_frame(&mut self, payload: &str) -> Result<(), JsValue> {
        self.inner.ingest_stream_frame(payload).map_err(|e| JsValue::from_str(&e.0))
    }
    #[wasm_bindgen(js_name = setStreamState)]
    pub fn set_stream_state(&mut self, state: u8) {
        self.inner.set_stream_state(match state {
            2 => StreamState::Live, 1 => StreamState::Connecting, _ => StreamState::Disconnected,
        });
    }
    #[wasm_bindgen(js_name = reportError)]
    pub fn report_error(&mut self, message: &str) { self.inner.report_error(message); }
    #[wasm_bindgen(js_name = takeRefreshRequest)]
    pub fn take_refresh_request(&mut self) -> bool { self.inner.take_refresh_request() }

    /// Return exact decimal identity, never a JavaScript Number.
    #[wasm_bindgen(js_name = selectedPaneId)]
    pub fn selected_pane_id(&self) -> Option<String> {
        let model = self.inner.model();
        let pane_id = match model.active_view() {
            View::Panes => model.panes().get(model.selected()).map(|pane| pane.pane_id),
            View::Events => model.events().get(model.selected()).map(|event| event.pane_id),
        };
        pane_id.map(|id| id.to_string())
    }

    #[wasm_bindgen(js_name = takeFlatPatches)]
    pub fn take_flat_patches(&mut self) -> JsValue {
        self.inner.prepare_flat_patches();
        let object = Object::new();
        let _ = Reflect::set(&object, &"cells".into(), &Uint32Array::from(self.inner.flat_cells()).into());
        let _ = Reflect::set(&object, &"spans".into(), &Uint32Array::from(self.inner.flat_spans()).into());
        object.into()
    }
    #[wasm_bindgen(js_name = screenText)]
    pub fn screen_text(&self) -> String { self.inner.screen_text().join("\n") }
}

fn wire_error(error: frankenterm_terminal_wire::WireError) -> JsValue { JsValue::from_str(error.code()) }

/// Network byte admission occurs before copying the JS-owned array into WASM.
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

/// The host supplies monotonic milliseconds (normally performance.now()). The
/// model retains its own high-water mark; a backwards sample cannot extend work.
fn wire_time(now_ms: f64) -> Result<u64, JsValue> {
    if !now_ms.is_finite() || now_ms < 0.0 || now_ms > 9_007_199_254_740_991.0 {
        return Err(wire_error(frankenterm_terminal_wire::WireError::InvalidNumber));
    }
    Ok(now_ms as u64)
}

/// Shared Rust validation plus a real bounded terminal model. This class opens
/// no socket and sends no pane input; callers forward its returned wire bytes.
#[wasm_bindgen]
pub struct TerminalWireClient { inner: crate::terminal::TerminalClient }

#[wasm_bindgen]
impl TerminalWireClient {
    #[wasm_bindgen(constructor)]
    pub fn new(hello_frame: &Uint8Array) -> Result<TerminalWireClient, JsValue> {
        let bytes = wire_bytes(hello_frame)?;
        Ok(Self { inner: crate::terminal::TerminalClient::from_hello_frame(&bytes).map_err(wire_error)? })
    }
    #[wasm_bindgen(js_name = helloFrame)]
    pub fn hello_frame(&mut self) -> Result<Vec<u8>, JsValue> { self.inner.hello_frame().map_err(wire_error) }
    #[wasm_bindgen(js_name = acceptWelcome)]
    pub fn accept_welcome(&mut self, frame: &Uint8Array) -> Result<(), JsValue> {
        self.inner.accept_welcome(&wire_bytes(frame)?).map_err(wire_error)
    }

    /// Decode-only; this does not change the screen or create ACK authority.
    #[wasm_bindgen(js_name = decodeServer)]
    pub fn decode_server(&self, frame: &Uint8Array) -> Result<String, JsValue> {
        self.inner.decode_server(&wire_bytes(frame)?).map_err(wire_error)
    }

    /// Apply a typed snapshot/delta to the Rust screen and return a bounded
    /// outcome. Non-state events/receipts remain explicit in the outcome.
    #[wasm_bindgen(js_name = applyServer)]
    pub fn apply_server(&mut self, frame: &Uint8Array, now_ms: f64) -> Result<String, JsValue> {
        self.inner.apply_server_json(&wire_bytes(frame)?, wire_time(now_ms)?).map_err(wire_error)
    }

    #[wasm_bindgen(js_name = encodeRequest)]
    pub fn encode_request(&mut self, message: &Uint8Array) -> Result<Vec<u8>, JsValue> {
        let bytes = wire_bytes(message)?;
        let json = std::str::from_utf8(&bytes)
            .map_err(|_| wire_error(frankenterm_terminal_wire::WireError::Malformed))?;
        self.inner.encode_request(json).map_err(wire_error)
    }

    /// The advertised minimum is not an applied model baseline.
    #[wasm_bindgen(js_name = bindScope)]
    pub fn bind_scope(&mut self, scope_json: &str, dimensions_json: &str, minimum: &str) -> Result<(), JsValue> {
        self.inner.bind_scope(scope_json, dimensions_json, minimum).map_err(wire_error)
    }
    #[wasm_bindgen(js_name = unbindScope)]
    pub fn unbind_scope(&mut self, scope_json: &str) -> Result<(), JsValue> {
        self.inner.unbind_scope(scope_json).map_err(wire_error)
    }
    #[wasm_bindgen(js_name = updateDimensions)]
    pub fn update_dimensions(&mut self, scope_json: &str, dimensions_json: &str) -> Result<(), JsValue> {
        self.inner.update_dimensions(scope_json, dimensions_json).map_err(wire_error)
    }
    /// Verification only; host counters cannot manufacture application.
    #[wasm_bindgen(js_name = recordDelivery)]
    pub fn record_delivery(&mut self, scope_json: &str, previous: &str, sequence: &str) -> Result<(), JsValue> {
        self.inner.record_delivery(scope_json, previous, sequence).map_err(wire_error)
    }
    #[wasm_bindgen(js_name = recordLease)]
    pub fn record_lease(&mut self, scope_json: &str, fence: &str, active: bool) -> Result<(), JsValue> {
        self.inner.record_lease(scope_json, fence, active).map_err(wire_error)
    }

    /// Empty bytes mean no newly applied baseline awaits acknowledgment.
    #[wasm_bindgen(js_name = takeAppliedAck)]
    pub fn take_applied_ack(&mut self, scope_json: &str) -> Result<Vec<u8>, JsValue> {
        self.inner.take_applied_ack(scope_json).map(Option::unwrap_or_default).map_err(wire_error)
    }
    /// Empty bytes mean no recovery is needed, the cooldown is active, or the
    /// finite retry budget is exhausted. screenMetadata exposes the state.
    #[wasm_bindgen(js_name = takeRecoveryRequest)]
    pub fn take_recovery_request(&mut self, scope_json: &str, request_id: &str, now_ms: f64) -> Result<Vec<u8>, JsValue> {
        self.inner.take_recovery_request(scope_json, request_id, wire_time(now_ms)?)
            .map(Option::unwrap_or_default).map_err(wire_error)
    }
    #[wasm_bindgen(js_name = expireSnapshots)]
    pub fn expire_snapshots(&mut self, now_ms: f64) -> Result<u32, JsValue> {
        Ok(u32::try_from(self.inner.expire_snapshots(wire_time(now_ms)?)).unwrap_or(u32::MAX))
    }
    #[wasm_bindgen(js_name = abortSnapshot)]
    pub fn abort_snapshot(&mut self, scope_json: &str) -> Result<(), JsValue> {
        self.inner.abort_snapshot(scope_json).map_err(wire_error)
    }

    #[wasm_bindgen(js_name = screenMetadata)]
    pub fn screen_metadata(&self, scope_json: &str) -> Result<String, JsValue> {
        self.inner.screen_metadata_json(scope_json).map_err(wire_error)
    }
    /// One complete rich row plus only its referenced link/image metadata.
    #[wasm_bindgen(js_name = screenRow)]
    pub fn screen_row(&self, scope_json: &str, row: u16) -> Result<String, JsValue> {
        self.inner.screen_row_json(scope_json, row).map_err(wire_error)
    }
    #[wasm_bindgen(js_name = screenTextPage)]
    pub fn screen_text_page(&self, scope_json: &str, first_row: u16, row_count: u16) -> Result<String, JsValue> {
        self.inner.screen_text_page(scope_json, first_row, row_count).map_err(wire_error)
    }
}
