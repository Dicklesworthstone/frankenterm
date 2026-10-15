//! Decoding of `ft web` responses and `ft.stream.v1` stream frames.
//!
//! Unknown fields are ignored so a newer server stays readable, but a body
//! that is not the expected shape is an error the viewer shows; it is never
//! read as an empty fleet.

use serde::Deserialize;

/// Bytes accepted from one host-supplied body. `/panes` and `/events` are
/// bounded server-side; this keeps a misbehaving proxy from exhausting memory.
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// A decoding failure, with a short reason fit for the status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireError(pub String);

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Deserialize)]
struct Envelope<T> {
    ok: bool,
    #[serde(default = "Option::default")]
    data: Option<T>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_code: Option<String>,
}

fn decode_envelope<T: for<'de> Deserialize<'de>>(body: &str) -> Result<T, WireError> {
    if body.len() > MAX_BODY_BYTES {
        return Err(WireError(format!("response over {MAX_BODY_BYTES} bytes")));
    }
    let envelope: Envelope<T> =
        serde_json::from_str(body).map_err(|e| WireError(format!("unreadable response: {e}")))?;
    if !envelope.ok {
        let code = envelope.error_code.unwrap_or_else(|| "error".to_string());
        let message = envelope.error.unwrap_or_default();
        return Err(WireError(format!("{code}: {message}")));
    }
    envelope
        .data
        .ok_or_else(|| WireError("response has no data".to_string()))
}

/// One pane as `/panes` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Pane {
    pub pane_id: u64,
    pub domain: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    pub last_seen_at: i64,
}

#[derive(Debug, Deserialize)]
struct PanesData {
    panes: Vec<Pane>,
}

/// Decode a `/panes` response body.
pub fn decode_panes(body: &str) -> Result<Vec<Pane>, WireError> {
    decode_envelope::<PanesData>(body).map(|data| data.panes)
}

/// One detection, from `/events` or a live stream frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    /// Persisted event id; `None` for a live event the watcher did not store.
    pub id: Option<i64>,
    pub pane_id: u64,
    pub rule_id: String,
    pub severity: String,
    pub detected_at_ms: i64,
}

#[derive(Debug, Deserialize)]
struct StoredEvent {
    id: i64,
    pane_id: u64,
    rule_id: String,
    severity: String,
    detected_at: i64,
}

#[derive(Debug, Deserialize)]
struct EventsData {
    events: Vec<StoredEvent>,
}

/// Decode an `/events` response body, preserving the server's order.
pub fn decode_events(body: &str) -> Result<Vec<Detection>, WireError> {
    decode_envelope::<EventsData>(body).map(|data| {
        data.events
            .into_iter()
            .map(|e| Detection {
                id: Some(e.id),
                pane_id: e.pane_id,
                rule_id: e.rule_id,
                severity: e.severity,
                detected_at_ms: e.detected_at,
            })
            .collect()
    })
}

/// What one `ft.stream.v1` frame on `/stream/events` means to the viewer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamUpdate {
    /// The subscription is established.
    Ready,
    /// A detection, live or replayed after a reconnect.
    Detection(Detection),
    /// A pane appeared on the mux.
    PaneDiscovered {
        pane_id: u64,
        domain: String,
        title: String,
    },
    /// A pane left the mux.
    PaneDisappeared { pane_id: u64 },
    /// Capture lost continuity on a pane.
    Gap { pane_id: u64 },
    /// The server dropped frames for this client; the view may be incomplete.
    Lag,
    /// A frame the viewer has no use for (workflow progress, user vars, ...).
    Other,
}

#[derive(Debug, Deserialize)]
struct Frame {
    schema: String,
    kind: String,
    #[serde(default)]
    ts_ms: i64,
    #[serde(default)]
    data: serde_json::Value,
}

/// `ft.stream.v1` is the only stream schema this viewer reads.
pub const STREAM_SCHEMA: &str = "ft.stream.v1";

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BusEvent {
    PatternDetected {
        pane_id: u64,
        detection: BusDetection,
        #[serde(default)]
        event_id: Option<i64>,
    },
    PaneDiscovered {
        pane_id: u64,
        domain: String,
        title: String,
    },
    PaneDisappeared {
        pane_id: u64,
    },
    GapDetected {
        pane_id: u64,
    },
    #[serde(other)]
    Unhandled,
}

#[derive(Debug, Deserialize)]
struct BusDetection {
    rule_id: String,
    severity: String,
}

/// Decode the `data:` payload of one `/stream/events` SSE message.
pub fn decode_stream_frame(payload: &str) -> Result<StreamUpdate, WireError> {
    if payload.len() > MAX_BODY_BYTES {
        return Err(WireError(format!(
            "stream frame over {MAX_BODY_BYTES} bytes"
        )));
    }
    let frame: Frame = serde_json::from_str(payload)
        .map_err(|e| WireError(format!("unreadable stream frame: {e}")))?;
    if frame.schema != STREAM_SCHEMA {
        return Err(WireError(format!(
            "unsupported stream schema {}",
            frame.schema
        )));
    }
    match frame.kind.as_str() {
        "ready" => Ok(StreamUpdate::Ready),
        "lag" => Ok(StreamUpdate::Lag),
        "event" => {
            let event = frame
                .data
                .get("event")
                .cloned()
                .ok_or_else(|| WireError("event frame without an event".to_string()))?;
            let event: BusEvent = serde_json::from_value(event)
                .map_err(|e| WireError(format!("unreadable stream event: {e}")))?;
            Ok(match event {
                BusEvent::PatternDetected {
                    pane_id,
                    detection,
                    event_id,
                } => StreamUpdate::Detection(Detection {
                    id: event_id,
                    pane_id,
                    rule_id: detection.rule_id,
                    severity: detection.severity,
                    detected_at_ms: frame.ts_ms,
                }),
                BusEvent::PaneDiscovered {
                    pane_id,
                    domain,
                    title,
                } => StreamUpdate::PaneDiscovered {
                    pane_id,
                    domain,
                    title,
                },
                BusEvent::PaneDisappeared { pane_id } => StreamUpdate::PaneDisappeared { pane_id },
                BusEvent::GapDetected { pane_id } => StreamUpdate::Gap { pane_id },
                BusEvent::Unhandled => StreamUpdate::Other,
            })
        }
        _ => Ok(StreamUpdate::Other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panes_envelope_decodes_and_ignores_unknown_fields() {
        let body = r#"{"ok":true,"version":"0.15.21","data":{"total":2,"panes":[
            {"pane_id":3,"pane_uuid":"u","domain":"local","title":"codex","cwd":"/w","window_id":1,"tab_id":2,"first_seen_at":1,"last_seen_at":9},
            {"pane_id":4,"domain":"SSH:box","first_seen_at":1,"last_seen_at":5,"future":true}]}}"#;
        let panes = decode_panes(body).expect("panes");
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].title.as_deref(), Some("codex"));
        assert_eq!(panes[1].title, None);
        assert_eq!(panes[1].domain, "SSH:box");
    }

    #[test]
    fn error_envelope_is_an_error_not_an_empty_fleet() {
        let body = r#"{"ok":false,"error":"Failed to query panes","error_code":"storage_error","version":"x"}"#;
        assert_eq!(
            decode_panes(body),
            Err(WireError(
                "storage_error: Failed to query panes".to_string()
            ))
        );
        assert!(decode_panes("<html>proxy error</html>").is_err());
        assert!(decode_panes(r#"{"ok":true,"version":"x"}"#).is_err());
    }

    #[test]
    fn events_keep_server_order_and_ids() {
        let body = r#"{"ok":true,"version":"x","data":{"total":2,"events":[
            {"id":12,"pane_id":3,"rule_id":"codex.usage.reached","event_type":"usage","severity":"warning","confidence":0.9,"detected_at":200},
            {"id":11,"pane_id":4,"rule_id":"claude_code.approval_needed","event_type":"prompt","severity":"info","confidence":1.0,"detected_at":100}]}}"#;
        let events = decode_events(body).expect("events");
        assert_eq!(
            events.iter().map(|e| e.id).collect::<Vec<_>>(),
            [Some(12), Some(11)]
        );
        assert_eq!(events[0].rule_id, "codex.usage.reached");
    }

    #[test]
    fn stream_frames_map_to_updates() {
        let detection = r#"{"schema":"ft.stream.v1","stream":"events","kind":"event","seq":2,"ts_ms":777,
            "data":{"replayed":false,"event":{"type":"pattern_detected","pane_id":3,"pane_uuid":null,"event_id":41,
            "detection":{"rule_id":"codex.usage.reached","agent_type":"codex","event_type":"usage","severity":"critical","confidence":0.95,"extracted":{},"matched_text":"x"}}}}"#;
        assert_eq!(
            decode_stream_frame(detection),
            Ok(StreamUpdate::Detection(Detection {
                id: Some(41),
                pane_id: 3,
                rule_id: "codex.usage.reached".to_string(),
                severity: "critical".to_string(),
                detected_at_ms: 777,
            }))
        );
        let ready = r#"{"schema":"ft.stream.v1","stream":"events","kind":"ready","seq":1,"ts_ms":1,"data":{"channel":"all"}}"#;
        assert_eq!(decode_stream_frame(ready), Ok(StreamUpdate::Ready));
        let lag = r#"{"schema":"ft.stream.v1","stream":"events","kind":"lag","seq":3,"ts_ms":1,"data":{"missed":4}}"#;
        assert_eq!(decode_stream_frame(lag), Ok(StreamUpdate::Lag));
        let gone = r#"{"schema":"ft.stream.v1","stream":"events","kind":"event","seq":4,"ts_ms":1,"data":{"event":{"type":"pane_disappeared","pane_id":9}}}"#;
        assert_eq!(
            decode_stream_frame(gone),
            Ok(StreamUpdate::PaneDisappeared { pane_id: 9 })
        );
        let workflow = r#"{"schema":"ft.stream.v1","stream":"events","kind":"event","seq":5,"ts_ms":1,"data":{"event":{"type":"workflow_step","workflow_id":"w","step_name":"s","result":"ok"}}}"#;
        assert_eq!(decode_stream_frame(workflow), Ok(StreamUpdate::Other));
    }

    #[test]
    fn foreign_schema_and_garbage_frames_are_rejected() {
        let other = r#"{"schema":"ft.stream.v2","stream":"events","kind":"ready","seq":1,"ts_ms":1,"data":{}}"#;
        assert!(decode_stream_frame(other).is_err());
        assert!(decode_stream_frame("keepalive").is_err());
        let no_event = r#"{"schema":"ft.stream.v1","stream":"events","kind":"event","seq":1,"ts_ms":1,"data":{}}"#;
        assert!(decode_stream_frame(no_event).is_err());
    }
}
