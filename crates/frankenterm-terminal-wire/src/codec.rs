use std::io::{self, Write};

use serde::Serialize;

use crate::{ClientFrame, MAX_FRAME_BYTES, ServerFrame, WireError};

/// Check the byte limit before serde can allocate or inspect any JSON. Both
/// directions retain serde_json's recursion limit and reject trailing data.
pub fn decode_client(bytes: &[u8]) -> Result<ClientFrame, WireError> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(WireError::FrameTooLarge);
    }
    let frame: ClientFrame = serde_json::from_slice(bytes).map_err(|_| WireError::Malformed)?;
    frame.validate()?;
    Ok(frame)
}

pub fn decode_server(bytes: &[u8]) -> Result<ServerFrame, WireError> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(WireError::FrameTooLarge);
    }
    let frame: ServerFrame = serde_json::from_slice(bytes).map_err(|_| WireError::Malformed)?;
    frame.validate()?;
    Ok(frame)
}

pub fn encode_client(frame: &ClientFrame) -> Result<Vec<u8>, WireError> {
    frame.validate()?;
    encode_bounded(frame)
}

pub fn encode_server(frame: &ServerFrame) -> Result<Vec<u8>, WireError> {
    frame.validate()?;
    encode_bounded(frame)
}

struct BoundedWriter {
    bytes: Vec<u8>,
    limit_hit: bool,
}

impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let remaining = MAX_FRAME_BYTES.saturating_sub(self.bytes.len());
        if bytes.len() > remaining {
            self.limit_hit = true;
            return Err(io::Error::other("terminal_wire_frame_too_large"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Never serialize into an unbounded temporary and truncate afterwards. A
/// rejected frame's partial bytes are dropped and cannot reach a transport.
fn encode_bounded<T: Serialize>(value: &T) -> Result<Vec<u8>, WireError> {
    let mut writer = BoundedWriter {
        bytes: Vec::new(),
        limit_hit: false,
    };
    if serde_json::to_writer(&mut writer, value).is_err() {
        return Err(if writer.limit_hit {
            WireError::FrameTooLarge
        } else {
            WireError::Malformed
        });
    }
    Ok(writer.bytes)
}
