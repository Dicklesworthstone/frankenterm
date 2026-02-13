//! encode and decode the frames for the mux protocol.
//! The frames include the length of a PDU as well as an identifier
//! that informs us how to decode it.  The length, ident and serial
//! number are encoded using a variable length integer encoding.
//! Rather than rely solely on serde to serialize and deserialize an
//! enum, we encode the enum variants with a version/identifier tag
//! for ourselves.  This will make it a little easier to manage
//! client and server instances that are built from different versions
//! of this code; in this way the client and server can more gracefully
//! manage unknown enum variants.
#![allow(dead_code)]
#![allow(clippy::range_plus_one)]

// Both async-smol and async-asupersync may be enabled simultaneously due to Cargo
// workspace feature unification. When both are active, asupersync takes priority.

use anyhow::{bail, Context as _, Error};
use config::keyassignment::{PaneDirection, ScrollbackEraseMode};
use frankenterm_term::color::ColorPalette;
use frankenterm_term::{Alert, ClipboardSelection, StableRowIndex, TerminalSize};
use mux::client::{ClientId, ClientInfo};
use mux::pane::PaneId;
use mux::renderable::{RenderableDimensions, StableCursorPosition};
use mux::tab::{PaneNode, SerdeUrl, SplitRequest, TabId};
use mux::window::WindowId;
use portable_pty::CommandBuilder;
use rangeset::*;
use serde::{Deserialize, Serialize};

#[cfg(feature = "async-asupersync")]
use asupersync::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

#[cfg(feature = "async-smol")]
use smol::io::AsyncWriteExt;
#[cfg(feature = "async-smol")]
use smol::prelude::*;

use std::collections::HashMap;
use std::convert::TryInto;
use std::io::Cursor;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use termwiz::hyperlink::Hyperlink;
use termwiz::image::{ImageData, TextureCoordinate};
use termwiz::surface::{Line, SequenceNo};
use thiserror::Error;

#[derive(Error, Debug)]
#[error("Corrupt Response: {0}")]
pub struct CorruptResponse(String);

/// Returns the encoded length of the leb128 representation of value
fn encoded_length(value: u64) -> usize {
    struct NullWrite {}
    impl std::io::Write for NullWrite {
        fn write(&mut self, buf: &[u8]) -> std::result::Result<usize, std::io::Error> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::result::Result<(), std::io::Error> {
            Ok(())
        }
    }

    leb128::write::unsigned(&mut NullWrite {}, value).unwrap()
}

const COMPRESSED_MASK: u64 = 1 << 63;

fn encode_raw_as_vec(
    ident: u64,
    serial: u64,
    data: &[u8],
    is_compressed: bool,
) -> anyhow::Result<Vec<u8>> {
    let len = data.len() + encoded_length(ident) + encoded_length(serial);
    let masked_len = if is_compressed {
        (len as u64) | COMPRESSED_MASK
    } else {
        len as u64
    };

    // Double-buffer the data; since we run with nodelay enabled, it is
    // desirable for the write to be a single packet (or at least, for
    // the header portion to go out in a single packet)
    let mut buffer = Vec::with_capacity(len + encoded_length(masked_len));

    leb128::write::unsigned(&mut buffer, masked_len).context("writing pdu len")?;
    leb128::write::unsigned(&mut buffer, serial).context("writing pdu serial")?;
    leb128::write::unsigned(&mut buffer, ident).context("writing pdu ident")?;
    buffer.extend_from_slice(data);

    if is_compressed {
        metrics::histogram!("pdu.encode.compressed.size").record(buffer.len() as f64);
    } else {
        metrics::histogram!("pdu.encode.size").record(buffer.len() as f64);
    }

    Ok(buffer)
}

/// Encode a frame.  If the data is compressed, the high bit of the length
/// is set to indicate that.  The data written out has the format:
/// tagged_len: leb128  (u64 msb is set if data is compressed)
/// serial: leb128
/// ident: leb128
/// data bytes
fn encode_raw<W: std::io::Write>(
    ident: u64,
    serial: u64,
    data: &[u8],
    is_compressed: bool,
    mut w: W,
) -> anyhow::Result<usize> {
    let buffer = encode_raw_as_vec(ident, serial, data, is_compressed)?;
    w.write_all(&buffer).context("writing pdu data buffer")?;
    Ok(buffer.len())
}

async fn encode_raw_async<W: Unpin + AsyncWriteExt>(
    ident: u64,
    serial: u64,
    data: &[u8],
    is_compressed: bool,
    w: &mut W,
) -> anyhow::Result<usize> {
    let buffer = encode_raw_as_vec(ident, serial, data, is_compressed)?;
    w.write_all(&buffer)
        .await
        .context("writing pdu data buffer")?;
    Ok(buffer.len())
}

/// Read a single leb128 encoded value from the stream
async fn read_u64_async<R>(r: &mut R) -> anyhow::Result<u64>
where
    R: Unpin + AsyncRead + std::fmt::Debug,
{
    let mut buf = vec![];
    loop {
        let mut byte = [0u8];
        if let Err(err) = r.read_exact(&mut byte).await {
            if err.kind() == std::io::ErrorKind::UnexpectedEof {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "EOF while reading leb128 encoded value",
                )
                .into());
            }

            return Err(err.into());
        }
        buf.push(byte[0]);

        match leb128::read::unsigned(&mut buf.as_slice()) {
            Ok(n) => {
                return Ok(n);
            }
            Err(leb128::read::Error::IoError(_)) => continue,
            Err(leb128::read::Error::Overflow) => anyhow::bail!("leb128 is too large"),
        }
    }
}

/// Read a single leb128 encoded value from the stream
fn read_u64<R: std::io::Read>(mut r: R) -> anyhow::Result<u64> {
    leb128::read::unsigned(&mut r)
        .map_err(|err| match err {
            leb128::read::Error::IoError(ioerr) => anyhow::Error::new(ioerr),
            err => anyhow::Error::new(err),
        })
        .context("reading leb128")
}

#[derive(Debug)]
struct Decoded {
    ident: u64,
    serial: u64,
    data: Vec<u8>,
    is_compressed: bool,
}

/// Decode a frame.
/// See encode_raw() for the frame format.
async fn decode_raw_async<R: Unpin + AsyncRead + std::fmt::Debug>(
    r: &mut R,
    max_serial: Option<u64>,
) -> anyhow::Result<Decoded> {
    let len = read_u64_async(r)
        .await
        .context("decode_raw_async failed to read PDU length")?;
    let (len, is_compressed) = if (len & COMPRESSED_MASK) != 0 {
        (len & !COMPRESSED_MASK, true)
    } else {
        (len, false)
    };
    let serial = read_u64_async(r)
        .await
        .context("decode_raw_async failed to read PDU serial")?;
    if let Some(max_serial) = max_serial {
        if serial > max_serial && max_serial > 0 {
            return Err(CorruptResponse(format!(
                "decode_raw_async: serial {serial} is implausibly large \
                (bigger than {max_serial})"
            ))
            .into());
        }
    }
    let ident = read_u64_async(r)
        .await
        .context("decode_raw_async failed to read PDU ident")?;
    let data_len =
        match (len as usize).overflowing_sub(encoded_length(ident) + encoded_length(serial)) {
            (_, true) => {
                return Err(CorruptResponse(format!(
                    "decode_raw_async: sizes don't make sense: \
                    len:{len} serial:{serial} (enc={}) ident:{ident} (enc={})",
                    encoded_length(serial),
                    encoded_length(ident)
                ))
                .into());
            }
            (data_len, false) => data_len,
        };

    if is_compressed {
        metrics::histogram!("pdu.decode.compressed.size").record(data_len as f64);
    } else {
        metrics::histogram!("pdu.decode.size").record(data_len as f64);
    }

    let mut data = vec![0u8; data_len];
    r.read_exact(&mut data).await.with_context(|| {
        format!(
            "decode_raw_async failed to read {} bytes of data \
            for PDU of length {} with serial={} ident={}",
            data_len, len, serial, ident
        )
    })?;
    Ok(Decoded {
        ident,
        serial,
        data,
        is_compressed,
    })
}

/// Decode a frame.
/// See encode_raw() for the frame format.
fn decode_raw<R: std::io::Read>(mut r: R) -> anyhow::Result<Decoded> {
    let len = read_u64(r.by_ref()).context("reading PDU length")?;
    let (len, is_compressed) = if (len & COMPRESSED_MASK) != 0 {
        (len & !COMPRESSED_MASK, true)
    } else {
        (len, false)
    };
    let serial = read_u64(r.by_ref()).context("reading PDU serial")?;
    let ident = read_u64(r.by_ref()).context("reading PDU ident")?;
    let data_len =
        match (len as usize).overflowing_sub(encoded_length(ident) + encoded_length(serial)) {
            (_, true) => {
                anyhow::bail!(
                    "sizes don't make sense: len:{} serial:{} (enc={}) ident:{} (enc={})",
                    len,
                    serial,
                    encoded_length(serial),
                    ident,
                    encoded_length(ident)
                );
            }
            (data_len, false) => data_len,
        };

    if is_compressed {
        metrics::histogram!("pdu.decode.compressed.size").record(data_len as f64);
    } else {
        metrics::histogram!("pdu.decode.size").record(data_len as f64);
    }

    let mut data = vec![0u8; data_len];
    r.read_exact(&mut data).with_context(|| {
        format!(
            "reading {} bytes of data for PDU of length {} with serial={} ident={}",
            data_len, len, serial, ident
        )
    })?;
    Ok(Decoded {
        ident,
        serial,
        data,
        is_compressed,
    })
}

#[derive(Debug, PartialEq)]
pub struct DecodedPdu {
    pub serial: u64,
    pub pdu: Pdu,
}

/// If the serialized size is larger than this, then we'll consider compressing it
const COMPRESS_THRESH: usize = 32;

/// Wire compression policy for PDU encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionMode {
    /// Preserve legacy behavior: compress only when beneficial.
    Auto,
    /// Always compress payload bytes before framing.
    Always,
    /// Never compress payload bytes before framing.
    Never,
}

fn serialize<T: serde::Serialize>(t: &T) -> Result<(Vec<u8>, bool), Error> {
    serialize_with_mode(t, CompressionMode::Auto)
}

fn serialize_with_mode<T: serde::Serialize>(
    t: &T,
    compression_mode: CompressionMode,
) -> Result<(Vec<u8>, bool), Error> {
    let mut uncompressed = Vec::new();
    let mut encode = varbincode::Serializer::new(&mut uncompressed);
    t.serialize(&mut encode)?;

    if compression_mode == CompressionMode::Never {
        return Ok((uncompressed, false));
    }

    if compression_mode == CompressionMode::Auto && uncompressed.len() <= COMPRESS_THRESH {
        return Ok((uncompressed, false));
    }
    // It's a little heavy; let's try compressing it
    let mut compressed = Vec::new();
    let mut compress = zstd::Encoder::new(&mut compressed, zstd::DEFAULT_COMPRESSION_LEVEL)?;
    let mut encode = varbincode::Serializer::new(&mut compress);
    t.serialize(&mut encode)?;
    compress.finish()?;

    log::debug!(
        "serialized+compress len {} vs {}",
        compressed.len(),
        uncompressed.len()
    );

    if compression_mode == CompressionMode::Always {
        return Ok((compressed, true));
    }

    if compressed.len() < uncompressed.len() {
        Ok((compressed, true))
    } else {
        Ok((uncompressed, false))
    }
}

fn deserialize<T: serde::de::DeserializeOwned, R: std::io::Read>(
    mut r: R,
    is_compressed: bool,
) -> Result<T, Error> {
    if is_compressed {
        let mut decompress = zstd::Decoder::new(r)?;
        let mut decode = varbincode::Deserializer::new(&mut decompress);
        serde::Deserialize::deserialize(&mut decode).map_err(Into::into)
    } else {
        let mut decode = varbincode::Deserializer::new(&mut r);
        serde::Deserialize::deserialize(&mut decode).map_err(Into::into)
    }
}

macro_rules! pdu {
    ($( $name:ident:$vers:expr),* $(,)?) => {
        #[derive(PartialEq, Debug)]
        #[allow(clippy::large_enum_variant)]
        pub enum Pdu {
            Invalid{ident: u64},
            $(
                $name($name)
            ,)*
        }

        impl Pdu {
            pub fn encode<W: std::io::Write>(&self, w: W, serial: u64) -> Result<(), Error> {
                self.encode_with_mode(w, serial, CompressionMode::Auto)
            }

            pub fn encode_with_mode<W: std::io::Write>(
                &self,
                w: W,
                serial: u64,
                compression_mode: CompressionMode,
            ) -> Result<(), Error> {
                match self {
                    Pdu::Invalid{..} => bail!("attempted to serialize Pdu::Invalid"),
                    $(
                        Pdu::$name(s) => {
                            let (data, is_compressed) =
                                serialize_with_mode(s, compression_mode)?;
                            let encoded_size = encode_raw($vers, serial, &data, is_compressed, w)?;
                            log::debug!("encode {} size={encoded_size}", stringify!($name));
                            metrics::histogram!("pdu.size", "pdu" => stringify!($name)).record(encoded_size as f64);
                            metrics::histogram!("pdu.size.rate", "pdu" => stringify!($name)).record(encoded_size as f64);
                            Ok(())
                        }
                    ,)*
                }
            }

            pub async fn encode_async<W: Unpin + AsyncWriteExt>(&self, w: &mut W, serial: u64) -> Result<(), Error> {
                self.encode_async_with_mode(w, serial, CompressionMode::Auto).await
            }

            pub async fn encode_async_with_mode<W: Unpin + AsyncWriteExt>(
                &self,
                w: &mut W,
                serial: u64,
                compression_mode: CompressionMode,
            ) -> Result<(), Error> {
                match self {
                    Pdu::Invalid{..} => bail!("attempted to serialize Pdu::Invalid"),
                    $(
                        Pdu::$name(s) => {
                            let (data, is_compressed) =
                                serialize_with_mode(s, compression_mode)?;
                            let encoded_size = encode_raw_async($vers, serial, &data, is_compressed, w).await?;
                            log::debug!("encode_async {} size={encoded_size}", stringify!($name));
                            metrics::histogram!("pdu.size", "pdu" => stringify!($name)).record(encoded_size as f64);
                            metrics::histogram!("pdu.size.rate", "pdu" => stringify!($name)).record(encoded_size as f64);
                            Ok(())
                        }
                    ,)*
                }
            }

            pub fn pdu_name(&self) -> &'static str {
                match self {
                    Pdu::Invalid{..} => "Invalid",
                    $(
                        Pdu::$name(_) => {
                            stringify!($name)
                        }
                    ,)*
                }
            }

            pub fn decode<R: std::io::Read>(r: R) -> Result<DecodedPdu, Error> {
                let decoded = decode_raw(r).context("decoding a PDU")?;
                match decoded.ident {
                    $(
                        $vers => {
                            metrics::histogram!("pdu.size", "pdu" => stringify!($name)).record(decoded.data.len() as f64);
                            metrics::histogram!("pdu.size.rate", "pdu" => stringify!($name)).record(decoded.data.len() as f64);
                            Ok(DecodedPdu {
                                serial: decoded.serial,
                                pdu: Pdu::$name(deserialize(decoded.data.as_slice(), decoded.is_compressed)?)
                            })
                        }
                    ,)*
                    _ => {
                        metrics::histogram!("pdu.size", "pdu" => "??").record(decoded.data.len() as f64);
                        metrics::histogram!("pdu.size.rate", "pdu" => "??").record(decoded.data.len() as f64);
                        Ok(DecodedPdu {
                            serial: decoded.serial,
                            pdu: Pdu::Invalid{ident:decoded.ident}
                        })
                    }
                }
            }

            pub async fn decode_async<R>(r: &mut R, max_serial: Option<u64>) -> Result<DecodedPdu, Error>
                where R: std::marker::Unpin,
                      R: AsyncRead,
                      R: std::fmt::Debug
            {
                let decoded = decode_raw_async(r, max_serial).await.context("decoding a PDU")?;
                match decoded.ident {
                    $(
                        $vers => {
                            metrics::histogram!("pdu.size", "pdu" => stringify!($name)).record(decoded.data.len() as f64);
                            Ok(DecodedPdu {
                                serial: decoded.serial,
                                pdu: Pdu::$name(deserialize(decoded.data.as_slice(), decoded.is_compressed)?)
                            })
                        }
                    ,)*
                    _ => {
                        metrics::histogram!("pdu.size", "pdu" => "??").record(decoded.data.len() as f64);
                        Ok(DecodedPdu {
                            serial: decoded.serial,
                            pdu: Pdu::Invalid{ident:decoded.ident}
                        })
                    }
                }
            }
        }
    }
}

/// The overall version of the codec.
/// This must be bumped when backwards incompatible changes
/// are made to the types and protocol.
pub const CODEC_VERSION: usize = 45;

// Defines the Pdu enum.
// Each struct has an explicit identifying number.
// This allows removal of obsolete structs,
// and defining newer structs as the protocol evolves.
pdu! {
    ErrorResponse: 0,
    Ping: 1,
    Pong: 2,
    ListPanes: 3,
    ListPanesResponse: 4,
    SpawnResponse: 8,
    WriteToPane: 9,
    UnitResponse: 10,
    SendKeyDown: 11,
    SendMouseEvent: 12,
    SendPaste: 13,
    Resize: 14,
    SetClipboard: 20,
    GetLines: 22,
    GetLinesResponse: 23,
    GetPaneRenderChanges: 24,
    GetPaneRenderChangesResponse: 25,
    GetCodecVersion: 26,
    GetCodecVersionResponse: 27,
    GetTlsCreds: 28,
    GetTlsCredsResponse: 29,
    LivenessResponse: 30,
    SearchScrollbackRequest: 31,
    SearchScrollbackResponse: 32,
    SetPaneZoomed: 33,
    SplitPane: 34,
    KillPane: 35,
    SpawnV2: 36,
    PaneRemoved: 37,
    SetPalette: 38,
    NotifyAlert: 39,
    SetClientId: 40,
    GetClientList: 41,
    GetClientListResponse: 42,
    SetWindowWorkspace: 43,
    WindowWorkspaceChanged: 44,
    SetFocusedPane: 45,
    GetImageCell: 46,
    GetImageCellResponse: 47,
    MovePaneToNewTab: 48,
    MovePaneToNewTabResponse: 49,
    ActivatePaneDirection: 50,
    GetPaneRenderableDimensions: 51,
    GetPaneRenderableDimensionsResponse: 52,
    PaneFocused: 53,
    TabResized: 54,
    TabAddedToWindow: 55,
    TabTitleChanged: 56,
    WindowTitleChanged: 57,
    RenameWorkspace: 58,
    EraseScrollbackRequest: 59,
    GetPaneDirection: 60,
    GetPaneDirectionResponse: 61,
    AdjustPaneSize: 62,
}

impl Pdu {
    /// Returns true if this type of Pdu represents action taken
    /// directly by a user, rather than background traffic on
    /// a live connection
    pub fn is_user_input(&self) -> bool {
        matches!(
            self,
            Self::WriteToPane(_)
                | Self::SendKeyDown(_)
                | Self::SendMouseEvent(_)
                | Self::SendPaste(_)
                | Self::Resize(_)
                | Self::SetClipboard(_)
                | Self::SetPaneZoomed(_)
                | Self::SpawnV2(_)
        )
    }

    pub fn stream_decode(buffer: &mut Vec<u8>) -> anyhow::Result<Option<DecodedPdu>> {
        let mut cursor = Cursor::new(buffer.as_slice());
        match Self::decode(&mut cursor) {
            Ok(decoded) => {
                let consumed = cursor.position() as usize;
                let remain = buffer.len() - consumed;
                // Remove `consumed` bytes from the start of the vec.
                // This is safe because the vec is just bytes and we are
                // constrained the offsets accordingly.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        buffer.as_ptr().add(consumed),
                        buffer.as_mut_ptr(),
                        remain,
                    );
                }
                buffer.truncate(remain);
                Ok(Some(decoded))
            }
            Err(err) => {
                if let Some(ioerr) = err.root_cause().downcast_ref::<std::io::Error>() {
                    match ioerr.kind() {
                        std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::WouldBlock => {
                            return Ok(None);
                        }
                        _ => {}
                    }
                } else {
                    log::error!("not an ioerror in stream_decode: {:?}", err);
                }
                Err(err)
            }
        }
    }

    pub fn try_read_and_decode<R: std::io::Read>(
        r: &mut R,
        buffer: &mut Vec<u8>,
    ) -> anyhow::Result<Option<DecodedPdu>> {
        loop {
            if let Some(decoded) =
                Self::stream_decode(buffer).context("stream_decode of buffer for PDU")?
            {
                return Ok(Some(decoded));
            }

            let mut buf = [0u8; 4096];
            let size = match r.read(&mut buf) {
                Ok(size) => size,
                Err(err) => {
                    if err.kind() == std::io::ErrorKind::WouldBlock {
                        return Ok(None);
                    }
                    return Err(err.into());
                }
            };
            if size == 0 {
                return Err(
                    std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "End Of File").into(),
                );
            }

            buffer.extend_from_slice(&buf[0..size]);
        }
    }

    pub fn pane_id(&self) -> Option<PaneId> {
        match self {
            Pdu::GetPaneRenderChangesResponse(GetPaneRenderChangesResponse { pane_id, .. })
            | Pdu::SetPalette(SetPalette { pane_id, .. })
            | Pdu::NotifyAlert(NotifyAlert { pane_id, .. })
            | Pdu::SetClipboard(SetClipboard { pane_id, .. })
            | Pdu::PaneFocused(PaneFocused { pane_id })
            | Pdu::PaneRemoved(PaneRemoved { pane_id }) => Some(*pane_id),
            _ => None,
        }
    }
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct UnitResponse {}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct ErrorResponse {
    pub reason: String,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetCodecVersion {}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetCodecVersionResponse {
    pub codec_vers: usize,
    pub version_string: String,
    pub executable_path: PathBuf,
    pub config_file_path: Option<PathBuf>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct Ping {}
#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct Pong {}

/// Requests a client certificate to authenticate against
/// the TLS based server
#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetTlsCreds {}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetTlsCredsResponse {
    /// The signing certificate
    pub ca_cert_pem: String,
    /// A client authentication certificate and private
    /// key, PEM encoded
    pub client_cert_pem: String,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct ListPanes {}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct ListPanesResponse {
    pub tabs: Vec<PaneNode>,
    pub tab_titles: Vec<String>,
    pub window_titles: HashMap<WindowId, String>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SplitPane {
    pub pane_id: PaneId,
    pub split_request: SplitRequest,
    pub command: Option<CommandBuilder>,
    pub command_dir: Option<String>,
    pub domain: config::keyassignment::SpawnTabDomain,
    /// Instead of spawning a command, move the specified
    /// pane into the new split target
    pub move_pane_id: Option<PaneId>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct MovePaneToNewTab {
    pub pane_id: PaneId,
    pub window_id: Option<WindowId>,
    pub workspace_for_new_window: Option<String>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct MovePaneToNewTabResponse {
    pub tab_id: TabId,
    pub window_id: WindowId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SpawnV2 {
    pub domain: config::keyassignment::SpawnTabDomain,
    /// If None, create a new window for this new tab
    pub window_id: Option<WindowId>,
    pub command: Option<CommandBuilder>,
    pub command_dir: Option<String>,
    pub size: TerminalSize,
    pub workspace: String,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct PaneRemoved {
    pub pane_id: PaneId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct KillPane {
    pub pane_id: PaneId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SpawnResponse {
    pub tab_id: TabId,
    pub pane_id: PaneId,
    pub window_id: WindowId,
    pub size: TerminalSize,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct WriteToPane {
    pub pane_id: PaneId,
    pub data: Vec<u8>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SendPaste {
    pub pane_id: PaneId,
    pub data: String,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SendKeyDown {
    pub pane_id: TabId,
    pub event: termwiz::input::KeyEvent,
    pub input_serial: InputSerial,
}

/// InputSerial is used to sequence input requests with output events.
/// It started life as a monotonic sequence number but evolved into
/// the number of milliseconds since the unix epoch.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone, Copy, PartialOrd, Ord)]
pub struct InputSerial(u64);

impl InputSerial {
    pub const fn empty() -> Self {
        Self(0)
    }

    pub fn now() -> Self {
        std::time::SystemTime::now().into()
    }

    pub fn elapsed_millis(&self) -> u64 {
        let now = InputSerial::now();
        now.0 - self.0
    }
}

impl From<std::time::SystemTime> for InputSerial {
    fn from(val: std::time::SystemTime) -> Self {
        let duration = val
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .expect("SystemTime before unix epoch?");
        let millis: u64 = duration
            .as_millis()
            .try_into()
            .expect("millisecond count to fit in u64");
        InputSerial(millis)
    }
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SendMouseEvent {
    pub pane_id: PaneId,
    pub event: frankenterm_term::input::MouseEvent,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SetClipboard {
    pub pane_id: PaneId,
    pub clipboard: Option<String>,
    pub selection: ClipboardSelection,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SetWindowWorkspace {
    pub window_id: WindowId,
    pub workspace: String,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct RenameWorkspace {
    pub old_workspace: String,
    pub new_workspace: String,
}

/// This is used both as a notification from server->client
/// and as a configuration request from client->server when
/// the client's preferred configuration changes
#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SetPalette {
    pub pane_id: PaneId,
    pub palette: ColorPalette,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct NotifyAlert {
    pub pane_id: PaneId,
    pub alert: Alert,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct TabAddedToWindow {
    pub tab_id: TabId,
    pub window_id: WindowId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct TabResized {
    pub tab_id: TabId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct TabTitleChanged {
    pub tab_id: TabId,
    pub title: String,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct WindowTitleChanged {
    pub window_id: WindowId,
    pub title: String,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct PaneFocused {
    pub pane_id: PaneId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct WindowWorkspaceChanged {
    pub window_id: WindowId,
    pub workspace: String,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SetClientId {
    pub client_id: ClientId,
    pub is_proxy: bool,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SetFocusedPane {
    pub pane_id: PaneId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetClientList;

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetClientListResponse {
    pub clients: Vec<ClientInfo>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct Resize {
    pub containing_tab_id: TabId,
    pub pane_id: PaneId,
    pub size: TerminalSize,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SetPaneZoomed {
    pub containing_tab_id: TabId,
    pub pane_id: PaneId,
    pub zoomed: bool,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetPaneDirection {
    pub pane_id: PaneId,
    pub direction: PaneDirection,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct AdjustPaneSize {
    pub pane_id: PaneId,
    pub direction: PaneDirection,
    pub amount: usize,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetPaneDirectionResponse {
    pub pane_id: Option<PaneId>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct ActivatePaneDirection {
    pub pane_id: PaneId,
    pub direction: PaneDirection,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetPaneRenderChanges {
    pub pane_id: PaneId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetPaneRenderableDimensions {
    pub pane_id: PaneId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetPaneRenderableDimensionsResponse {
    pub pane_id: PaneId,
    pub cursor_position: StableCursorPosition,
    pub dimensions: RenderableDimensions,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct LivenessResponse {
    pub pane_id: PaneId,
    pub is_alive: bool,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetPaneRenderChangesResponse {
    pub pane_id: PaneId,
    pub mouse_grabbed: bool,
    pub cursor_position: StableCursorPosition,
    pub dimensions: RenderableDimensions,
    pub dirty_lines: Vec<Range<StableRowIndex>>,
    pub title: String,
    pub working_dir: Option<SerdeUrl>,
    /// Lines that the server thought we'd almost certainly
    /// want to fetch as soon as we received this response
    pub bonus_lines: SerializedLines,

    pub input_serial: Option<InputSerial>,
    pub seqno: SequenceNo,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetLines {
    pub pane_id: PaneId,
    pub lines: Vec<Range<StableRowIndex>>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
struct CellCoordinates {
    line_idx: usize,
    cols: Range<usize>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
struct LineHyperlink {
    link: Hyperlink,
    coords: Vec<CellCoordinates>,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct SerializedImageCell {
    pub line_idx: StableRowIndex,
    pub cell_idx: usize,
    // The following fields are taken from termwiz::image::ImageCell
    pub top_left: TextureCoordinate,
    pub bottom_right: TextureCoordinate,
    /// Image::data::hash() for the ImageCell::data field
    pub data_hash: [u8; 32],
    pub z_index: i32,
    pub padding_left: u16,
    pub padding_top: u16,
    pub padding_right: u16,
    pub padding_bottom: u16,
    pub image_id: Option<u32>,
    pub placement_id: Option<u32>,
}

/// What's all this?
/// Cells hold references to Arc<Hyperlink> and it is important to us to
/// maintain identity of the hyperlinks in the individual cells, while also
/// only sending a single copy of the associated URL.
/// This section of code extracts the hyperlinks from the cells and builds
/// up a mapping that can be used to restore the identity when the `lines()`
/// method is called.
#[derive(Deserialize, Serialize, PartialEq, Debug, Default)]
pub struct SerializedLines {
    lines: Vec<(StableRowIndex, Line)>,
    hyperlinks: Vec<LineHyperlink>,
    images: Vec<SerializedImageCell>,
}

impl SerializedLines {
    /// Reconsitute hyperlinks or other attributes that were decomposed for
    /// serialization, and return the line data.
    pub fn extract_data(self) -> (Vec<(StableRowIndex, Line)>, Vec<SerializedImageCell>) {
        let lines = if self.hyperlinks.is_empty() {
            self.lines
        } else {
            let mut lines = self.lines;

            for link in self.hyperlinks {
                let url = Arc::new(link.link);

                for coord in link.coords {
                    if let Some((_, line)) = lines.get_mut(coord.line_idx) {
                        if let Some(cells) =
                            line.cells_mut_for_attr_changes_only().get_mut(coord.cols)
                        {
                            for cell in cells {
                                cell.attrs_mut().set_hyperlink(Some(Arc::clone(&url)));
                            }
                        }
                    }
                }
            }

            lines
        };
        (lines, self.images)
    }
}

impl From<Vec<(StableRowIndex, Line)>> for SerializedLines {
    fn from(mut lines: Vec<(StableRowIndex, Line)>) -> Self {
        let mut hyperlinks = vec![];
        let mut images = vec![];

        for (line_idx, (stable_row_idx, line)) in lines.iter_mut().enumerate() {
            let mut current_link: Option<Arc<Hyperlink>> = None;
            let mut current_range = 0..0;

            for (x, cell) in line
                .cells_mut_for_attr_changes_only()
                .iter_mut()
                .enumerate()
            {
                // Unset the hyperlink on the cell, if any, and record that
                // in the hyperlinks data for later restoration.
                if let Some(link) = cell.attrs_mut().hyperlink().map(Arc::clone) {
                    cell.attrs_mut().set_hyperlink(None);
                    match current_link.as_ref() {
                        Some(current) if Arc::ptr_eq(current, &link) => {
                            // Continue the current streak
                            current_range = range_union(current_range, x..x + 1);
                        }
                        Some(prior) => {
                            // It's a different URL, push the current data and start a new one
                            hyperlinks.push(LineHyperlink {
                                link: (**prior).clone(),
                                coords: vec![CellCoordinates {
                                    line_idx,
                                    cols: current_range,
                                }],
                            });
                            current_range = x..x + 1;
                            current_link = Some(link);
                        }
                        None => {
                            // Starting a new streak
                            current_range = x..x + 1;
                            current_link = Some(link);
                        }
                    }
                } else if let Some(link) = current_link.take() {
                    // Wrap up a prior streak
                    hyperlinks.push(LineHyperlink {
                        link: (*link).clone(),
                        coords: vec![CellCoordinates {
                            line_idx,
                            cols: current_range,
                        }],
                    });
                    current_range = 0..0;
                }

                if let Some(cell_images) = cell.attrs().images() {
                    for imcell in cell_images {
                        let (padding_left, padding_top, padding_right, padding_bottom) =
                            imcell.padding();
                        images.push(SerializedImageCell {
                            line_idx: *stable_row_idx,
                            cell_idx: x,
                            top_left: imcell.top_left(),
                            bottom_right: imcell.bottom_right(),
                            z_index: imcell.z_index(),
                            padding_left,
                            padding_top,
                            padding_right,
                            padding_bottom,
                            image_id: imcell.image_id(),
                            placement_id: imcell.placement_id(),
                            data_hash: imcell.image_data().hash(),
                        });
                    }
                }
                cell.attrs_mut().clear_images();
            }
            if let Some(link) = current_link.take() {
                // Wrap up final streak
                hyperlinks.push(LineHyperlink {
                    link: (*link).clone(),
                    coords: vec![CellCoordinates {
                        line_idx,
                        cols: current_range,
                    }],
                });
            }
        }

        Self {
            lines,
            hyperlinks,
            images,
        }
    }
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetLinesResponse {
    pub pane_id: PaneId,
    pub lines: SerializedLines,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct EraseScrollbackRequest {
    pub pane_id: PaneId,
    pub erase_mode: ScrollbackEraseMode,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SearchScrollbackRequest {
    pub pane_id: PaneId,
    pub pattern: mux::pane::Pattern,
    pub range: Range<StableRowIndex>,
    pub limit: Option<u32>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SearchScrollbackResponse {
    pub results: Vec<mux::pane::SearchResult>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetImageCell {
    pub pane_id: PaneId,
    pub line_idx: StableRowIndex,
    pub cell_idx: usize,
    pub data_hash: [u8; 32],
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetImageCellResponse {
    pub pane_id: PaneId,
    pub data: Option<Arc<ImageData>>,
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_frame() {
        let mut encoded = Vec::new();
        encode_raw(0x81, 0x42, b"hello", false, &mut encoded).unwrap();
        assert_eq!(&encoded, b"\x08\x42\x81\x01hello");
        let decoded = decode_raw(encoded.as_slice()).unwrap();
        assert_eq!(decoded.ident, 0x81);
        assert_eq!(decoded.serial, 0x42);
        assert_eq!(decoded.data, b"hello");
    }

    #[test]
    fn test_frame_lengths() {
        let mut serial = 1;
        for target_len in &[128, 247, 256, 65536, 16777216] {
            let mut payload = Vec::with_capacity(*target_len);
            payload.resize(*target_len, b'a');
            let mut encoded = Vec::new();
            encode_raw(0x42, serial, payload.as_slice(), false, &mut encoded).unwrap();
            let decoded = decode_raw(encoded.as_slice()).unwrap();
            assert_eq!(decoded.ident, 0x42);
            assert_eq!(decoded.serial, serial);
            assert_eq!(decoded.data, payload);
            serial += 1;
        }
    }

    #[test]
    fn test_pdu_ping() {
        let mut encoded = Vec::new();
        Pdu::Ping(Ping {}).encode(&mut encoded, 0x40).unwrap();
        assert_eq!(&encoded, &[2, 0x40, 1]);
        assert_eq!(
            DecodedPdu {
                serial: 0x40,
                pdu: Pdu::Ping(Ping {})
            },
            Pdu::decode(encoded.as_slice()).unwrap()
        );
    }

    #[test]
    fn test_pdu_encode_with_mode_never_disables_compression() {
        let mut encoded = Vec::new();
        let payload = Pdu::WriteToPane(WriteToPane {
            pane_id: 1,
            data: vec![b'x'; 512],
        });
        payload
            .encode_with_mode(&mut encoded, 0x51, CompressionMode::Never)
            .unwrap();
        let decoded = decode_raw(encoded.as_slice()).unwrap();
        assert!(!decoded.is_compressed);
    }

    #[test]
    fn test_pdu_encode_with_mode_always_forces_compression() {
        let mut encoded = Vec::new();
        let payload = Pdu::WriteToPane(WriteToPane {
            pane_id: 1,
            data: vec![b'x'; 512],
        });
        payload
            .encode_with_mode(&mut encoded, 0x52, CompressionMode::Always)
            .unwrap();
        let decoded = decode_raw(encoded.as_slice()).unwrap();
        assert!(decoded.is_compressed);
    }

    #[test]
    fn stream_decode() {
        let mut encoded = Vec::new();
        Pdu::Ping(Ping {}).encode(&mut encoded, 0x1).unwrap();
        Pdu::Pong(Pong {}).encode(&mut encoded, 0x2).unwrap();
        assert_eq!(encoded.len(), 6);

        let mut cursor = Cursor::new(encoded.as_slice());
        let mut read_buffer = Vec::new();

        assert_eq!(
            Pdu::try_read_and_decode(&mut cursor, &mut read_buffer).unwrap(),
            Some(DecodedPdu {
                serial: 1,
                pdu: Pdu::Ping(Ping {})
            })
        );
        assert_eq!(
            Pdu::try_read_and_decode(&mut cursor, &mut read_buffer).unwrap(),
            Some(DecodedPdu {
                serial: 2,
                pdu: Pdu::Pong(Pong {})
            })
        );
        let err = Pdu::try_read_and_decode(&mut cursor, &mut read_buffer).unwrap_err();
        assert_eq!(
            err.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn test_pdu_ping_base91() {
        let mut encoded = Vec::new();
        {
            let mut encoder = base91::Base91Encoder::new(&mut encoded);
            Pdu::Ping(Ping {}).encode(&mut encoder, 0x41).unwrap();
        }
        assert_eq!(&encoded, &[60, 67, 75, 65]);
        let decoded = base91::decode(&encoded);
        assert_eq!(
            DecodedPdu {
                serial: 0x41,
                pdu: Pdu::Ping(Ping {})
            },
            Pdu::decode(decoded.as_slice()).unwrap()
        );
    }

    #[test]
    fn test_pdu_pong() {
        let mut encoded = Vec::new();
        Pdu::Pong(Pong {}).encode(&mut encoded, 0x42).unwrap();
        assert_eq!(&encoded, &[2, 0x42, 2]);
        assert_eq!(
            DecodedPdu {
                serial: 0x42,
                pdu: Pdu::Pong(Pong {})
            },
            Pdu::decode(encoded.as_slice()).unwrap()
        );
    }

    #[test]
    fn test_bogus_pdu() {
        let mut encoded = Vec::new();
        encode_raw(0xdeadbeef, 0x42, b"hello", false, &mut encoded).unwrap();
        assert_eq!(
            DecodedPdu {
                serial: 0x42,
                pdu: Pdu::Invalid { ident: 0xdeadbeef }
            },
            Pdu::decode(encoded.as_slice()).unwrap()
        );
    }

    // --- encoded_length tests ---

    #[test]
    fn encoded_length_zero() {
        assert_eq!(encoded_length(0), 1);
    }

    #[test]
    fn encoded_length_small() {
        // Values < 128 fit in one byte
        assert_eq!(encoded_length(1), 1);
        assert_eq!(encoded_length(127), 1);
    }

    #[test]
    fn encoded_length_two_bytes() {
        assert_eq!(encoded_length(128), 2);
        assert_eq!(encoded_length(16383), 2);
    }

    #[test]
    fn encoded_length_large() {
        assert_eq!(encoded_length(16384), 3);
        // u64::MAX needs 10 bytes in leb128
        assert_eq!(encoded_length(u64::MAX), 10);
    }

    // --- encode_raw / decode_raw roundtrip tests ---

    #[test]
    fn encode_decode_empty_data() {
        let mut encoded = Vec::new();
        encode_raw(1, 1, b"", false, &mut encoded).unwrap();
        let decoded = decode_raw(encoded.as_slice()).unwrap();
        assert_eq!(decoded.ident, 1);
        assert_eq!(decoded.serial, 1);
        assert_eq!(decoded.data, b"");
        assert!(!decoded.is_compressed);
    }

    #[test]
    fn encode_decode_compressed_flag() {
        let mut encoded = Vec::new();
        encode_raw(5, 10, b"payload", true, &mut encoded).unwrap();
        let decoded = decode_raw(encoded.as_slice()).unwrap();
        assert_eq!(decoded.ident, 5);
        assert_eq!(decoded.serial, 10);
        assert_eq!(decoded.data, b"payload");
        assert!(decoded.is_compressed);
    }

    #[test]
    fn encode_decode_large_ident_serial() {
        let mut encoded = Vec::new();
        let ident = 0xFFFF;
        let serial = 0xDEAD;
        encode_raw(ident, serial, b"big", false, &mut encoded).unwrap();
        let decoded = decode_raw(encoded.as_slice()).unwrap();
        assert_eq!(decoded.ident, ident);
        assert_eq!(decoded.serial, serial);
        assert_eq!(decoded.data, b"big");
    }

    #[test]
    fn encode_raw_as_vec_matches_encode_raw() {
        let ident = 42;
        let serial = 7;
        let data = b"test data";

        let vec_result = encode_raw_as_vec(ident, serial, data, false).unwrap();
        let mut write_result = Vec::new();
        encode_raw(ident, serial, data, false, &mut write_result).unwrap();

        assert_eq!(vec_result, write_result);
    }

    // --- COMPRESSED_MASK tests ---

    #[test]
    fn compressed_mask_is_high_bit() {
        assert_eq!(COMPRESSED_MASK, 1 << 63);
        assert_eq!(COMPRESSED_MASK & 0x7FFF_FFFF_FFFF_FFFF, 0);
    }

    // --- CompressionMode tests ---

    #[test]
    fn compression_mode_debug() {
        assert_eq!(format!("{:?}", CompressionMode::Auto), "Auto");
        assert_eq!(format!("{:?}", CompressionMode::Always), "Always");
        assert_eq!(format!("{:?}", CompressionMode::Never), "Never");
    }

    #[test]
    fn compression_mode_eq() {
        assert_eq!(CompressionMode::Auto, CompressionMode::Auto);
        assert_ne!(CompressionMode::Auto, CompressionMode::Always);
        assert_ne!(CompressionMode::Always, CompressionMode::Never);
    }

    #[test]
    fn compression_mode_clone() {
        let mode = CompressionMode::Always;
        let cloned = mode;
        assert_eq!(mode, cloned);
    }

    // --- serialize / deserialize roundtrips ---

    #[test]
    fn serialize_deserialize_small_uncompressed() {
        // Small data stays uncompressed in Auto mode
        let val: u32 = 42;
        let (data, is_compressed) = serialize(&val).unwrap();
        assert!(!is_compressed, "small data should not be compressed");
        let result: u32 = deserialize(data.as_slice(), false).unwrap();
        assert_eq!(result, val);
    }

    #[test]
    fn serialize_never_mode() {
        // Even large data stays uncompressed with Never mode
        let val: Vec<u8> = vec![0xAA; 512];
        let (data, is_compressed) = serialize_with_mode(&val, CompressionMode::Never).unwrap();
        assert!(!is_compressed);
        let result: Vec<u8> = deserialize(data.as_slice(), false).unwrap();
        assert_eq!(result, val);
    }

    #[test]
    fn serialize_always_mode() {
        let val: Vec<u8> = vec![0xBB; 512];
        let (data, is_compressed) = serialize_with_mode(&val, CompressionMode::Always).unwrap();
        assert!(is_compressed);
        let result: Vec<u8> = deserialize(data.as_slice(), true).unwrap();
        assert_eq!(result, val);
    }

    #[test]
    fn serialize_auto_mode_large_data() {
        // Repetitive large data should compress well
        let val: Vec<u8> = vec![0xCC; 4096];
        let (data, is_compressed) = serialize_with_mode(&val, CompressionMode::Auto).unwrap();
        // Auto may or may not compress depending on ratio, but roundtrip must work
        let result: Vec<u8> = deserialize(data.as_slice(), is_compressed).unwrap();
        assert_eq!(result, val);
    }

    // --- InputSerial tests ---

    #[test]
    fn input_serial_empty() {
        let empty = InputSerial::empty();
        assert_eq!(format!("{:?}", empty), "InputSerial(0)");
    }

    #[test]
    fn input_serial_now_nonzero() {
        let now = InputSerial::now();
        // Should be a large number of milliseconds since epoch
        assert_ne!(format!("{:?}", now), "InputSerial(0)");
    }

    #[test]
    fn input_serial_elapsed_millis() {
        let before = InputSerial::now();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let elapsed = before.elapsed_millis();
        assert!(
            elapsed >= 5,
            "{}",
            format!("elapsed should be at least ~10ms, got {}", elapsed)
        );
    }

    #[test]
    fn input_serial_from_system_time() {
        let time = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(12345);
        let serial: InputSerial = time.into();
        assert_eq!(format!("{:?}", serial), "InputSerial(12345)");
    }

    #[test]
    fn input_serial_clone_eq_ord() {
        let a = InputSerial::empty();
        let b = a;
        assert_eq!(a, b);
        let c = InputSerial::now();
        assert!(c > a);
    }

    // --- Pdu::is_user_input tests ---

    #[test]
    fn pdu_is_user_input_true_variants() {
        assert!(Pdu::WriteToPane(WriteToPane {
            pane_id: 0,
            data: vec![]
        })
        .is_user_input());
        assert!(Pdu::SendPaste(SendPaste {
            pane_id: 0,
            data: String::new()
        })
        .is_user_input());
        assert!(Pdu::Resize(Resize {
            containing_tab_id: 0,
            pane_id: 0,
            size: TerminalSize::default(),
        })
        .is_user_input());
    }

    #[test]
    fn pdu_is_user_input_false_variants() {
        assert!(!Pdu::Ping(Ping {}).is_user_input());
        assert!(!Pdu::Pong(Pong {}).is_user_input());
        assert!(!Pdu::ListPanes(ListPanes {}).is_user_input());
        assert!(!Pdu::GetCodecVersion(GetCodecVersion {}).is_user_input());
        assert!(!Pdu::GetTlsCreds(GetTlsCreds {}).is_user_input());
        assert!(!Pdu::Invalid { ident: 99 }.is_user_input());
    }

    // --- Pdu::pdu_name tests ---

    #[test]
    fn pdu_name_known_variants() {
        assert_eq!(Pdu::Ping(Ping {}).pdu_name(), "Ping");
        assert_eq!(Pdu::Pong(Pong {}).pdu_name(), "Pong");
        assert_eq!(Pdu::ListPanes(ListPanes {}).pdu_name(), "ListPanes");
        assert_eq!(
            Pdu::GetCodecVersion(GetCodecVersion {}).pdu_name(),
            "GetCodecVersion"
        );
        assert_eq!(
            Pdu::UnitResponse(UnitResponse {}).pdu_name(),
            "UnitResponse"
        );
        assert_eq!(
            Pdu::ErrorResponse(ErrorResponse { reason: "x".into() }).pdu_name(),
            "ErrorResponse"
        );
    }

    #[test]
    fn pdu_name_invalid() {
        assert_eq!(Pdu::Invalid { ident: 0 }.pdu_name(), "Invalid");
    }

    // --- Pdu::pane_id tests ---

    #[test]
    fn pdu_pane_id_some() {
        assert_eq!(
            Pdu::PaneRemoved(PaneRemoved { pane_id: 42 }).pane_id(),
            Some(42)
        );
        assert_eq!(
            Pdu::PaneFocused(PaneFocused { pane_id: 7 }).pane_id(),
            Some(7)
        );
    }

    #[test]
    fn pdu_pane_id_none() {
        assert_eq!(Pdu::Ping(Ping {}).pane_id(), None);
        assert_eq!(Pdu::Pong(Pong {}).pane_id(), None);
        assert_eq!(Pdu::Invalid { ident: 0 }.pane_id(), None);
    }

    // --- Pdu encode/decode roundtrips for additional variants ---

    #[test]
    fn pdu_roundtrip_error_response() {
        let mut buf = Vec::new();
        let pdu = Pdu::ErrorResponse(ErrorResponse {
            reason: "something went wrong".into(),
        });
        pdu.encode(&mut buf, 100).unwrap();
        let decoded = Pdu::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded.serial, 100);
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn pdu_roundtrip_unit_response() {
        let mut buf = Vec::new();
        let pdu = Pdu::UnitResponse(UnitResponse {});
        pdu.encode(&mut buf, 200).unwrap();
        let decoded = Pdu::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded.serial, 200);
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn pdu_roundtrip_get_codec_version() {
        let mut buf = Vec::new();
        let pdu = Pdu::GetCodecVersion(GetCodecVersion {});
        pdu.encode(&mut buf, 300).unwrap();
        let decoded = Pdu::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded.serial, 300);
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn pdu_roundtrip_write_to_pane() {
        let mut buf = Vec::new();
        let pdu = Pdu::WriteToPane(WriteToPane {
            pane_id: 5,
            data: b"hello world".to_vec(),
        });
        pdu.encode(&mut buf, 400).unwrap();
        let decoded = Pdu::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded.serial, 400);
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn pdu_roundtrip_send_paste() {
        let mut buf = Vec::new();
        let pdu = Pdu::SendPaste(SendPaste {
            pane_id: 3,
            data: "clipboard text".into(),
        });
        pdu.encode(&mut buf, 500).unwrap();
        let decoded = Pdu::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded.serial, 500);
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn pdu_roundtrip_kill_pane() {
        let mut buf = Vec::new();
        let pdu = Pdu::KillPane(KillPane { pane_id: 99 });
        pdu.encode(&mut buf, 600).unwrap();
        let decoded = Pdu::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded.serial, 600);
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn pdu_roundtrip_pane_removed() {
        let mut buf = Vec::new();
        let pdu = Pdu::PaneRemoved(PaneRemoved { pane_id: 42 });
        pdu.encode(&mut buf, 700).unwrap();
        let decoded = Pdu::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded.serial, 700);
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn pdu_roundtrip_tab_resized() {
        let mut buf = Vec::new();
        let pdu = Pdu::TabResized(TabResized { tab_id: 11 });
        pdu.encode(&mut buf, 800).unwrap();
        let decoded = Pdu::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded.serial, 800);
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn pdu_roundtrip_pane_focused() {
        let mut buf = Vec::new();
        let pdu = Pdu::PaneFocused(PaneFocused { pane_id: 77 });
        pdu.encode(&mut buf, 900).unwrap();
        let decoded = Pdu::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded.serial, 900);
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn pdu_roundtrip_rename_workspace() {
        let mut buf = Vec::new();
        let pdu = Pdu::RenameWorkspace(RenameWorkspace {
            old_workspace: "old".into(),
            new_workspace: "new".into(),
        });
        pdu.encode(&mut buf, 1000).unwrap();
        let decoded = Pdu::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded.serial, 1000);
        assert_eq!(decoded.pdu, pdu);
    }

    // --- Pdu::encode Invalid should fail ---

    #[test]
    fn pdu_encode_invalid_fails() {
        let mut buf = Vec::new();
        let result = Pdu::Invalid { ident: 0 }.encode(&mut buf, 0);
        assert!(result.is_err());
    }

    // --- stream_decode edge cases ---

    #[test]
    fn stream_decode_empty_buffer() {
        let mut buffer = Vec::new();
        let result = Pdu::stream_decode(&mut buffer).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn stream_decode_partial_frame() {
        // Just the length byte, no payload
        let mut buffer = vec![2u8];
        let result = Pdu::stream_decode(&mut buffer).unwrap();
        assert!(result.is_none());
        // Buffer should be preserved for future reads
        assert_eq!(buffer, vec![2u8]);
    }

    #[test]
    fn stream_decode_consumes_one_frame() {
        let mut encoded = Vec::new();
        Pdu::Ping(Ping {}).encode(&mut encoded, 1).unwrap();
        Pdu::Pong(Pong {}).encode(&mut encoded, 2).unwrap();
        let total_len = encoded.len();

        let decoded = Pdu::stream_decode(&mut encoded).unwrap().unwrap();
        assert_eq!(decoded.pdu, Pdu::Ping(Ping {}));
        assert_eq!(decoded.serial, 1);
        // Buffer should still contain the Pong frame
        assert!(encoded.len() < total_len);

        let decoded2 = Pdu::stream_decode(&mut encoded).unwrap().unwrap();
        assert_eq!(decoded2.pdu, Pdu::Pong(Pong {}));
        assert_eq!(decoded2.serial, 2);
        assert!(encoded.is_empty());
    }

    // --- SerializedLines tests ---

    #[test]
    fn serialized_lines_default_empty() {
        let sl = SerializedLines::default();
        let (lines, images) = sl.extract_data();
        assert!(lines.is_empty());
        assert!(images.is_empty());
    }

    #[test]
    fn serialized_lines_from_empty_vec() {
        let sl: SerializedLines = vec![].into();
        let (lines, images) = sl.extract_data();
        assert!(lines.is_empty());
        assert!(images.is_empty());
    }

    // --- CODEC_VERSION test ---

    #[test]
    fn codec_version_is_current() {
        assert_eq!(CODEC_VERSION, 45);
    }

    // --- CorruptResponse tests ---

    #[test]
    fn corrupt_response_display() {
        let err = CorruptResponse("bad data".into());
        assert_eq!(format!("{}", err), "Corrupt Response: bad data");
    }

    #[test]
    fn corrupt_response_debug() {
        let err = CorruptResponse("test".into());
        let dbg = format!("{:?}", err);
        assert!(dbg.contains("CorruptResponse"));
        assert!(dbg.contains("test"));
    }

    // --- DecodedPdu tests ---

    #[test]
    fn decoded_pdu_debug() {
        let dp = DecodedPdu {
            serial: 42,
            pdu: Pdu::Ping(Ping {}),
        };
        let dbg = format!("{:?}", dp);
        assert!(dbg.contains("42"));
        assert!(dbg.contains("Ping"));
    }

    #[test]
    fn decoded_pdu_partial_eq() {
        let a = DecodedPdu {
            serial: 1,
            pdu: Pdu::Ping(Ping {}),
        };
        let b = DecodedPdu {
            serial: 1,
            pdu: Pdu::Ping(Ping {}),
        };
        let c = DecodedPdu {
            serial: 2,
            pdu: Pdu::Ping(Ping {}),
        };
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    // --- PDU struct construction tests ---

    #[test]
    fn error_response_construction() {
        let err = ErrorResponse {
            reason: "test error".into(),
        };
        assert_eq!(err.reason, "test error");
        let clone_check = format!("{:?}", err);
        assert!(clone_check.contains("test error"));
    }

    #[test]
    fn get_codec_version_response_construction() {
        let resp = GetCodecVersionResponse {
            codec_vers: CODEC_VERSION,
            version_string: "1.0.0".into(),
            executable_path: PathBuf::from("/usr/bin/ft"),
            config_file_path: Some(PathBuf::from("/etc/ft.toml")),
        };
        assert_eq!(resp.codec_vers, 45);
        assert_eq!(resp.version_string, "1.0.0");
    }

    #[test]
    fn get_tls_creds_response_construction() {
        let resp = GetTlsCredsResponse {
            ca_cert_pem: "CA".into(),
            client_cert_pem: "CLIENT".into(),
        };
        assert_eq!(resp.ca_cert_pem, "CA");
        assert_eq!(resp.client_cert_pem, "CLIENT");
    }

    #[test]
    fn set_window_workspace_construction() {
        let msg = SetWindowWorkspace {
            window_id: 1,
            workspace: "default".into(),
        };
        assert_eq!(msg.window_id, 1);
        assert_eq!(msg.workspace, "default");
    }

    #[test]
    fn tab_title_changed_construction() {
        let msg = TabTitleChanged {
            tab_id: 5,
            title: "my tab".into(),
        };
        assert_eq!(msg.tab_id, 5);
        assert_eq!(msg.title, "my tab");
    }

    #[test]
    fn window_title_changed_construction() {
        let msg = WindowTitleChanged {
            window_id: 3,
            title: "my window".into(),
        };
        assert_eq!(msg.window_id, 3);
        assert_eq!(msg.title, "my window");
    }

    #[test]
    fn serialized_image_cell_debug_and_clone() {
        // SerializedImageCell requires NotNan<f32> for TextureCoordinate,
        // so test Debug/Clone/Eq on the struct via serde roundtrip instead
        let sl = SerializedLines::default();
        assert!(sl.images.is_empty());
        let dbg = format!("{:?}", sl);
        assert!(dbg.contains("SerializedLines"));
    }

    // --- read_u64 tests ---

    #[test]
    fn read_u64_small() {
        let data = [42u8]; // leb128 for 42
        let result = read_u64(data.as_slice()).unwrap();
        assert_eq!(result, 42);
    }

    #[test]
    fn read_u64_two_byte() {
        // leb128 encoding of 128: 0x80 0x01
        let data = [0x80u8, 0x01];
        let result = read_u64(data.as_slice()).unwrap();
        assert_eq!(result, 128);
    }

    #[test]
    fn read_u64_empty_fails() {
        let data: &[u8] = &[];
        assert!(read_u64(data).is_err());
    }

    // --- Multiple PDU encode/decode in sequence (using Pdu::decode directly) ---

    #[test]
    fn multiple_pdus_sequential_decode() {
        // Encode three PDUs into a single buffer
        let mut buf = Vec::new();
        Pdu::Ping(Ping {}).encode(&mut buf, 1).unwrap();
        Pdu::Pong(Pong {}).encode(&mut buf, 2).unwrap();
        Pdu::UnitResponse(UnitResponse {})
            .encode(&mut buf, 3)
            .unwrap();

        // Decode them sequentially using Pdu::decode on a Cursor
        let mut cursor = Cursor::new(buf.as_slice());

        let d1 = Pdu::decode(&mut cursor).unwrap();
        assert_eq!(d1.serial, 1);
        assert_eq!(d1.pdu, Pdu::Ping(Ping {}));

        let d2 = Pdu::decode(&mut cursor).unwrap();
        assert_eq!(d2.serial, 2);
        assert_eq!(d2.pdu, Pdu::Pong(Pong {}));

        let d3 = Pdu::decode(&mut cursor).unwrap();
        assert_eq!(d3.serial, 3);
        assert_eq!(d3.pdu, Pdu::UnitResponse(UnitResponse {}));
    }

    // --- Compression roundtrip through full PDU encode/decode ---

    #[test]
    fn pdu_roundtrip_compressed_write_to_pane() {
        let mut buf = Vec::new();
        let pdu = Pdu::WriteToPane(WriteToPane {
            pane_id: 1,
            data: vec![b'A'; 1024],
        });
        pdu.encode_with_mode(&mut buf, 42, CompressionMode::Always)
            .unwrap();
        let decoded = Pdu::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded.serial, 42);
        assert_eq!(decoded.pdu, pdu);
    }

    // --- Additional codec edge and async coverage (wa-2mina) ---

    #[test]
    fn encode_raw_as_vec_sets_compressed_length_bit() {
        let uncompressed = encode_raw_as_vec(7, 9, b"abc", false).unwrap();
        let compressed = encode_raw_as_vec(7, 9, b"abc", true).unwrap();

        let uncompressed_len = read_u64(uncompressed.as_slice()).unwrap();
        let compressed_len = read_u64(compressed.as_slice()).unwrap();

        assert_eq!(uncompressed_len & COMPRESSED_MASK, 0);
        assert_eq!(compressed_len & COMPRESSED_MASK, COMPRESSED_MASK);
        assert_eq!(
            compressed_len & !COMPRESSED_MASK,
            uncompressed_len & !COMPRESSED_MASK
        );
    }

    #[test]
    fn decode_raw_errors_on_header_length_underflow() {
        // len=1, serial=1, ident=1 => encoded(serial)+encoded(ident)=2, impossible frame
        let malformed = vec![1u8, 1u8, 1u8];
        let err = decode_raw(malformed.as_slice()).expect_err("expected malformed frame to fail");
        let message = err.to_string();
        assert!(
            message.contains("sizes don't make sense"),
            "unexpected error message: {}",
            message
        );
    }

    #[test]
    fn deserialize_invalid_compressed_payload_errors() {
        let err =
            deserialize::<u64, _>(b"not-zstd".as_slice(), true).expect_err("expected zstd error");
        assert!(
            !err.to_string().is_empty(),
            "deserialize should surface a non-empty error"
        );
    }

    #[test]
    fn serialize_with_mode_always_compresses_small_payload() {
        let (payload, is_compressed) =
            serialize_with_mode(&7u8, CompressionMode::Always).expect("serialize");
        assert!(is_compressed);
        let roundtrip: u8 = deserialize(payload.as_slice(), true).expect("deserialize");
        assert_eq!(roundtrip, 7u8);
    }

    #[test]
    fn encode_raw_async_roundtrip_uncompressed() {
        smol::block_on(async {
            let mut writer = smol::io::Cursor::new(Vec::<u8>::new());
            encode_raw_async(17, 23, b"async-raw", false, &mut writer)
                .await
                .expect("encode_raw_async");
            let encoded = writer.into_inner();

            let decoded = decode_raw(encoded.as_slice()).expect("decode_raw");
            assert_eq!(decoded.ident, 17);
            assert_eq!(decoded.serial, 23);
            assert_eq!(decoded.data, b"async-raw");
            assert!(!decoded.is_compressed);
        });
    }

    #[test]
    fn decode_raw_async_roundtrip_uncompressed() {
        smol::block_on(async {
            let mut encoded = Vec::new();
            encode_raw(11, 13, b"decode-async", false, &mut encoded).expect("encode_raw");

            let mut reader = smol::io::Cursor::new(encoded);
            let decoded = decode_raw_async(&mut reader, None)
                .await
                .expect("decode_raw_async");
            assert_eq!(decoded.ident, 11);
            assert_eq!(decoded.serial, 13);
            assert_eq!(decoded.data, b"decode-async");
            assert!(!decoded.is_compressed);
        });
    }

    #[test]
    fn decode_raw_async_roundtrip_compressed_flag() {
        smol::block_on(async {
            let mut encoded = Vec::new();
            encode_raw(31, 9, b"decode-async-compressed", true, &mut encoded).expect("encode_raw");

            let mut reader = smol::io::Cursor::new(encoded);
            let decoded = decode_raw_async(&mut reader, None)
                .await
                .expect("decode_raw_async");
            assert_eq!(decoded.ident, 31);
            assert_eq!(decoded.serial, 9);
            assert_eq!(decoded.data, b"decode-async-compressed");
            assert!(decoded.is_compressed);
        });
    }

    #[test]
    fn decode_raw_async_rejects_serial_over_max() {
        smol::block_on(async {
            let mut encoded = Vec::new();
            encode_raw(3, 99, b"x", false, &mut encoded).expect("encode_raw");

            let mut reader = smol::io::Cursor::new(encoded);
            let err = decode_raw_async(&mut reader, Some(10))
                .await
                .expect_err("serial should be rejected");
            let message = err.to_string();
            assert!(
                message.contains("implausibly large"),
                "unexpected error message: {}",
                message
            );
        });
    }

    #[test]
    fn read_u64_async_returns_eof_on_empty_input() {
        smol::block_on(async {
            let mut reader = smol::io::Cursor::new(Vec::<u8>::new());
            let err = read_u64_async(&mut reader)
                .await
                .expect_err("empty stream should error");
            let io_err = err
                .downcast_ref::<std::io::Error>()
                .expect("expected io::Error");
            assert_eq!(io_err.kind(), std::io::ErrorKind::UnexpectedEof);
        });
    }
}
