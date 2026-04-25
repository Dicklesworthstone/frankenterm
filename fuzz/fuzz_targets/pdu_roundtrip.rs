#![no_main]
//! PDU encode→decode roundtrip fuzzer.
//!
//! The existing `pdu_stream_decode` target only exercises the decode side with
//! random bytes — it rarely reaches the varbincode payload because random bytes
//! almost never produce a valid frame header. This target complements it by
//! synthesising structured PDUs, encoding them, and asserting the decoded PDU
//! round-trips under all three compression modes. Invariants checked:
//!
//! - Encoded bytes always decode back to an equal PDU.
//! - Serial number is preserved exactly.
//! - Compression mode does not alter the decoded semantic payload.
//! - Two back-to-back encodes of the same PDU produce byte-identical frames
//!   (determinism), when compression mode is fixed.

use codec::{
    ErrorResponse, GetCodecVersion, GetCodecVersionResponse, KillPane, LivenessResponse,
    PaneFocused, PaneRemoved, Pdu, Ping, Pong, RenameWorkspace, Resize, SendPaste,
    SetActiveWorkspace, SetFocusedPane, SetPaneZoomed, SetWindowWorkspace, TabAddedToWindow,
    TabResized, TabTitleChanged, UnitResponse, WindowTitleChanged, WindowWorkspaceChanged,
    WriteToPane,
};
use frankenterm_term::TerminalSize;
use libfuzzer_sys::arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use std::io::Cursor;
use std::path::PathBuf;

const MAX_STRING_LEN: usize = 256;
const MAX_BYTES_LEN: usize = 4096;

#[derive(Debug)]
enum FuzzPdu {
    Ping,
    Pong,
    UnitResponse,
    GetCodecVersion,
    GetCodecVersionResponse {
        codec_vers: u16,
        version_string: String,
        executable_path: String,
        config_file_path: Option<String>,
    },
    ErrorResponse(String),
    LivenessResponse {
        pane_id: u32,
        is_alive: bool,
    },
    PaneRemoved(u32),
    KillPane(u32),
    PaneFocused(u32),
    SetFocusedPane(u32),
    TabResized(u32),
    TabAddedToWindow {
        tab_id: u32,
        window_id: u32,
    },
    TabTitleChanged {
        tab_id: u32,
        title: String,
    },
    WindowTitleChanged {
        window_id: u32,
        title: String,
    },
    WindowWorkspaceChanged {
        window_id: u32,
        workspace: String,
    },
    SetWindowWorkspace {
        window_id: u32,
        workspace: String,
    },
    SetActiveWorkspace(String),
    RenameWorkspace {
        old_workspace: String,
        new_workspace: String,
    },
    WriteToPane {
        pane_id: u32,
        data: Vec<u8>,
    },
    SendPaste {
        pane_id: u32,
        data: String,
    },
    Resize {
        containing_tab_id: u32,
        pane_id: u32,
        rows: u16,
        cols: u16,
        pixel_width: u16,
        pixel_height: u16,
        dpi: u16,
    },
    SetPaneZoomed {
        containing_tab_id: u32,
        pane_id: u32,
        zoomed: bool,
    },
}

#[derive(Debug, Clone, Copy)]
enum CompressionChoice {
    Auto,
    None,
    Compressed,
}

impl<'a> Arbitrary<'a> for CompressionChoice {
    fn arbitrary(u: &mut Unstructured<'a>) -> libfuzzer_sys::arbitrary::Result<Self> {
        Ok(match u.int_in_range(0..=2u8)? {
            0 => Self::Auto,
            1 => Self::None,
            _ => Self::Compressed,
        })
    }
}

impl CompressionChoice {
    fn to_mode(self) -> codec::CompressionMode {
        match self {
            Self::Auto => codec::CompressionMode::Auto,
            Self::None => codec::CompressionMode::None,
            Self::Compressed => codec::CompressionMode::Compressed,
        }
    }
}

#[derive(Debug)]
struct FuzzCase {
    pdu: FuzzPdu,
    serial: u64,
    compression: CompressionChoice,
}

impl<'a> Arbitrary<'a> for FuzzCase {
    fn arbitrary(u: &mut Unstructured<'a>) -> libfuzzer_sys::arbitrary::Result<Self> {
        let pdu = FuzzPdu::arbitrary(u)?;
        let serial = u64::arbitrary(u)?;
        let compression = CompressionChoice::arbitrary(u)?;
        Ok(Self {
            pdu,
            serial,
            compression,
        })
    }
}

impl<'a> Arbitrary<'a> for FuzzPdu {
    fn arbitrary(u: &mut Unstructured<'a>) -> libfuzzer_sys::arbitrary::Result<Self> {
        let tag = u.int_in_range(0u8..=23)?;
        Ok(match tag {
            0 => Self::Ping,
            1 => Self::Pong,
            2 => Self::UnitResponse,
            3 => Self::GetCodecVersion,
            4 => Self::GetCodecVersionResponse {
                codec_vers: u16::arbitrary(u)?,
                version_string: bounded_string(u)?,
                executable_path: bounded_string(u)?,
                config_file_path: Option::<String>::arbitrary(u)?.map(|s| bound_str(&s)),
            },
            5 => Self::ErrorResponse(bounded_string(u)?),
            6 => Self::LivenessResponse {
                pane_id: u32::arbitrary(u)?,
                is_alive: bool::arbitrary(u)?,
            },
            7 => Self::PaneRemoved(u32::arbitrary(u)?),
            8 => Self::KillPane(u32::arbitrary(u)?),
            9 => Self::PaneFocused(u32::arbitrary(u)?),
            10 => Self::SetFocusedPane(u32::arbitrary(u)?),
            11 => Self::TabResized(u32::arbitrary(u)?),
            12 => Self::TabAddedToWindow {
                tab_id: u32::arbitrary(u)?,
                window_id: u32::arbitrary(u)?,
            },
            13 => Self::TabTitleChanged {
                tab_id: u32::arbitrary(u)?,
                title: bounded_string(u)?,
            },
            14 => Self::WindowTitleChanged {
                window_id: u32::arbitrary(u)?,
                title: bounded_string(u)?,
            },
            15 => Self::WindowWorkspaceChanged {
                window_id: u32::arbitrary(u)?,
                workspace: bounded_string(u)?,
            },
            16 => Self::SetWindowWorkspace {
                window_id: u32::arbitrary(u)?,
                workspace: bounded_string(u)?,
            },
            17 => Self::SetActiveWorkspace(bounded_string(u)?),
            18 => Self::RenameWorkspace {
                old_workspace: bounded_string(u)?,
                new_workspace: bounded_string(u)?,
            },
            19 => Self::WriteToPane {
                pane_id: u32::arbitrary(u)?,
                data: bounded_bytes(u)?,
            },
            20 => Self::SendPaste {
                pane_id: u32::arbitrary(u)?,
                data: bounded_string(u)?,
            },
            21 => Self::Resize {
                containing_tab_id: u32::arbitrary(u)?,
                pane_id: u32::arbitrary(u)?,
                rows: u16::arbitrary(u)?,
                cols: u16::arbitrary(u)?,
                pixel_width: u16::arbitrary(u)?,
                pixel_height: u16::arbitrary(u)?,
                dpi: u16::arbitrary(u)?,
            },
            22 => Self::SetPaneZoomed {
                containing_tab_id: u32::arbitrary(u)?,
                pane_id: u32::arbitrary(u)?,
                zoomed: bool::arbitrary(u)?,
            },
            _ => Self::Ping,
        })
    }
}

fn bounded_string(u: &mut Unstructured<'_>) -> libfuzzer_sys::arbitrary::Result<String> {
    let raw: String = Arbitrary::arbitrary(u)?;
    Ok(bound_str(&raw))
}

fn bound_str(raw: &str) -> String {
    raw.chars().take(MAX_STRING_LEN).collect()
}

fn bounded_bytes(u: &mut Unstructured<'_>) -> libfuzzer_sys::arbitrary::Result<Vec<u8>> {
    let len = u.int_in_range(0..=MAX_BYTES_LEN)?;
    Ok(u.bytes(len)?.to_vec())
}

fn build_pdu(fp: &FuzzPdu) -> Pdu {
    match fp {
        FuzzPdu::Ping => Pdu::Ping(Ping {}),
        FuzzPdu::Pong => Pdu::Pong(Pong {}),
        FuzzPdu::UnitResponse => Pdu::UnitResponse(UnitResponse {}),
        FuzzPdu::GetCodecVersion => Pdu::GetCodecVersion(GetCodecVersion {}),
        FuzzPdu::GetCodecVersionResponse {
            codec_vers,
            version_string,
            executable_path,
            config_file_path,
        } => Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
            codec_vers: *codec_vers as usize,
            version_string: version_string.clone(),
            executable_path: PathBuf::from(executable_path),
            config_file_path: config_file_path.as_ref().map(PathBuf::from),
        }),
        FuzzPdu::ErrorResponse(reason) => Pdu::ErrorResponse(ErrorResponse {
            reason: reason.clone(),
        }),
        FuzzPdu::LivenessResponse { pane_id, is_alive } => {
            Pdu::LivenessResponse(LivenessResponse {
                pane_id: *pane_id as usize,
                is_alive: *is_alive,
            })
        }
        FuzzPdu::PaneRemoved(pane_id) => Pdu::PaneRemoved(PaneRemoved {
            pane_id: *pane_id as usize,
        }),
        FuzzPdu::KillPane(pane_id) => Pdu::KillPane(KillPane {
            pane_id: *pane_id as usize,
        }),
        FuzzPdu::PaneFocused(pane_id) => Pdu::PaneFocused(PaneFocused {
            pane_id: *pane_id as usize,
        }),
        FuzzPdu::SetFocusedPane(pane_id) => Pdu::SetFocusedPane(SetFocusedPane {
            pane_id: *pane_id as usize,
        }),
        FuzzPdu::TabResized(tab_id) => Pdu::TabResized(TabResized {
            tab_id: *tab_id as usize,
        }),
        FuzzPdu::TabAddedToWindow { tab_id, window_id } => {
            Pdu::TabAddedToWindow(TabAddedToWindow {
                tab_id: *tab_id as usize,
                window_id: *window_id as usize,
            })
        }
        FuzzPdu::TabTitleChanged { tab_id, title } => Pdu::TabTitleChanged(TabTitleChanged {
            tab_id: *tab_id as usize,
            title: title.clone(),
        }),
        FuzzPdu::WindowTitleChanged { window_id, title } => {
            Pdu::WindowTitleChanged(WindowTitleChanged {
                window_id: *window_id as usize,
                title: title.clone(),
            })
        }
        FuzzPdu::WindowWorkspaceChanged {
            window_id,
            workspace,
        } => Pdu::WindowWorkspaceChanged(WindowWorkspaceChanged {
            window_id: *window_id as usize,
            workspace: workspace.clone(),
        }),
        FuzzPdu::SetWindowWorkspace {
            window_id,
            workspace,
        } => Pdu::SetWindowWorkspace(SetWindowWorkspace {
            window_id: *window_id as usize,
            workspace: workspace.clone(),
        }),
        FuzzPdu::SetActiveWorkspace(workspace) => Pdu::SetActiveWorkspace(SetActiveWorkspace {
            workspace: workspace.clone(),
        }),
        FuzzPdu::RenameWorkspace {
            old_workspace,
            new_workspace,
        } => Pdu::RenameWorkspace(RenameWorkspace {
            old_workspace: old_workspace.clone(),
            new_workspace: new_workspace.clone(),
        }),
        FuzzPdu::WriteToPane { pane_id, data } => Pdu::WriteToPane(WriteToPane {
            pane_id: *pane_id as usize,
            data: data.clone(),
        }),
        FuzzPdu::SendPaste { pane_id, data } => Pdu::SendPaste(SendPaste {
            pane_id: *pane_id as usize,
            data: data.clone(),
        }),
        FuzzPdu::Resize {
            containing_tab_id,
            pane_id,
            rows,
            cols,
            pixel_width,
            pixel_height,
            dpi,
        } => Pdu::Resize(Resize {
            containing_tab_id: *containing_tab_id as usize,
            pane_id: *pane_id as usize,
            size: TerminalSize {
                rows: *rows as usize,
                cols: *cols as usize,
                pixel_width: *pixel_width as usize,
                pixel_height: *pixel_height as usize,
                dpi: *dpi as u32,
            },
        }),
        FuzzPdu::SetPaneZoomed {
            containing_tab_id,
            pane_id,
            zoomed,
        } => Pdu::SetPaneZoomed(SetPaneZoomed {
            containing_tab_id: *containing_tab_id as usize,
            pane_id: *pane_id as usize,
            zoomed: *zoomed,
        }),
    }
}

fuzz_target!(|case: FuzzCase| {
    let pdu = build_pdu(&case.pdu);
    let mode = case.compression.to_mode();

    // Encode once.
    let mut encoded = Vec::new();
    if pdu
        .encode_with_mode(&mut encoded, case.serial, mode)
        .is_err()
    {
        return;
    }

    // Decode must produce an equal PDU with the same serial.
    let decoded = Pdu::decode(Cursor::new(&encoded[..])).expect("encoded PDU must decode cleanly");
    assert_eq!(decoded.serial, case.serial, "serial mismatch");
    assert_eq!(decoded.pdu, pdu, "decoded PDU does not equal original");
    assert_eq!(decoded.pdu.pdu_name(), pdu.pdu_name(), "pdu_name drift");

    // Re-encoding with the same mode must be byte-identical (determinism).
    let mut encoded_again = Vec::new();
    pdu.encode_with_mode(&mut encoded_again, case.serial, mode)
        .expect("second encode must succeed");
    assert_eq!(
        encoded, encoded_again,
        "encode is non-deterministic for fixed mode"
    );

    // stream_decode on a buffer containing exactly one frame must also work
    // and must consume all bytes.
    let mut stream_buf = encoded.clone();
    let streamed = Pdu::stream_decode(&mut stream_buf)
        .expect("stream_decode errored on a well-formed frame")
        .expect("stream_decode returned None for a complete frame");
    assert_eq!(streamed.serial, case.serial);
    assert_eq!(streamed.pdu, pdu);
    assert!(
        stream_buf.is_empty(),
        "stream_decode left trailing bytes for single frame"
    );
});
