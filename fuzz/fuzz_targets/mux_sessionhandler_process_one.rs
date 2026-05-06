#![no_main]

use codec::{
    DecodedPdu, ErrorResponse, GetClientList, GetCodecVersion, GetPaneRenderChanges, KillPane,
    ListPanes, ListPanesTabStacks, Pdu, Ping, Pong, RenameWorkspace, SendPaste, SetActiveWorkspace,
    SetClientId, SetFocusedPane, SetWindowWorkspace, UnitResponse, WriteToPane,
};
use frankenterm_mux_server_impl::sessionhandler::{PduSender, SessionHandler};
use libfuzzer_sys::arbitrary::{Arbitrary, Result as ArbitraryResult, Unstructured};
use libfuzzer_sys::fuzz_target;
use mux::{Mux, client::ClientId};
use promise::spawn::SimpleExecutor;
use std::sync::{Arc, Mutex};

const MAX_INPUT_LEN: usize = 16 * 1024;
const MAX_FRAMES: usize = 8;
const MAX_STRING_LEN: usize = 128;
const MAX_BYTES_LEN: usize = 1024;
const MAX_ID: usize = 4096;

static GLOBAL_STATE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug)]
struct FuzzCase {
    install_mux: bool,
    frames: Vec<FuzzFrame>,
}

impl<'a> Arbitrary<'a> for FuzzCase {
    fn arbitrary(u: &mut Unstructured<'a>) -> ArbitraryResult<Self> {
        let install_mux = bool::arbitrary(u)?;
        let frame_count = u.int_in_range(0..=MAX_FRAMES)?;
        let mut frames = Vec::with_capacity(frame_count);
        for _ in 0..frame_count {
            frames.push(FuzzFrame::arbitrary(u)?);
        }
        Ok(Self {
            install_mux,
            frames,
        })
    }
}

#[derive(Debug)]
struct FuzzFrame {
    serial: u64,
    pdu: FuzzPdu,
}

impl<'a> Arbitrary<'a> for FuzzFrame {
    fn arbitrary(u: &mut Unstructured<'a>) -> ArbitraryResult<Self> {
        Ok(Self {
            serial: u64::arbitrary(u)?,
            pdu: FuzzPdu::arbitrary(u)?,
        })
    }
}

#[derive(Debug)]
enum FuzzPdu {
    Ping,
    PongAsRequest,
    UnitResponseAsRequest,
    ErrorResponseAsRequest(String),
    Invalid {
        ident: u64,
    },
    GetCodecVersion,
    ListPanes,
    ListPanesTabStacks,
    GetClientList,
    SetClientId {
        is_proxy: bool,
        hostname: String,
        username: String,
        pid: u32,
        epoch: u64,
        id: usize,
        ssh_auth_sock: Option<String>,
    },
    SetWindowWorkspace {
        window_id: usize,
        workspace: String,
    },
    SetActiveWorkspace(String),
    RenameWorkspace {
        old_workspace: String,
        new_workspace: String,
    },
    SetFocusedPane(usize),
    KillPane(usize),
    GetPaneRenderChanges(usize),
    WriteToPane {
        pane_id: usize,
        data: Vec<u8>,
    },
    SendPaste {
        pane_id: usize,
        data: String,
    },
}

impl<'a> Arbitrary<'a> for FuzzPdu {
    fn arbitrary(u: &mut Unstructured<'a>) -> ArbitraryResult<Self> {
        Ok(match u.int_in_range(0u8..=18)? {
            0 => Self::Ping,
            1 => Self::PongAsRequest,
            2 => Self::UnitResponseAsRequest,
            3 => Self::ErrorResponseAsRequest(bounded_string(u)?),
            4 => Self::Invalid {
                ident: u64::arbitrary(u)?,
            },
            5 => Self::GetCodecVersion,
            6 => Self::ListPanes,
            7 => Self::ListPanesTabStacks,
            8 => Self::GetClientList,
            9 => Self::SetClientId {
                is_proxy: bool::arbitrary(u)?,
                hostname: bounded_string(u)?,
                username: bounded_string(u)?,
                pid: u32::arbitrary(u)?,
                epoch: u64::arbitrary(u)?,
                id: usize::arbitrary(u)?,
                ssh_auth_sock: Option::<bool>::arbitrary(u)?
                    .map(|_| bounded_string(u))
                    .transpose()?,
            },
            10 => Self::SetWindowWorkspace {
                window_id: bounded_id(u)?,
                workspace: bounded_string(u)?,
            },
            11 => Self::SetActiveWorkspace(bounded_string(u)?),
            12 => Self::RenameWorkspace {
                old_workspace: bounded_string(u)?,
                new_workspace: bounded_string(u)?,
            },
            13 => Self::SetFocusedPane(bounded_id(u)?),
            14 => Self::KillPane(bounded_id(u)?),
            15 => Self::GetPaneRenderChanges(bounded_id(u)?),
            16 => Self::WriteToPane {
                pane_id: bounded_id(u)?,
                data: bounded_bytes(u)?,
            },
            17 => Self::SendPaste {
                pane_id: bounded_id(u)?,
                data: bounded_string(u)?,
            },
            _ => Self::Ping,
        })
    }
}

impl FuzzPdu {
    fn into_pdu(self) -> Pdu {
        match self {
            Self::Ping => Pdu::Ping(Ping {}),
            Self::PongAsRequest => Pdu::Pong(Pong {}),
            Self::UnitResponseAsRequest => Pdu::UnitResponse(UnitResponse {}),
            Self::ErrorResponseAsRequest(reason) => Pdu::ErrorResponse(ErrorResponse { reason }),
            Self::Invalid { ident } => Pdu::Invalid { ident },
            Self::GetCodecVersion => Pdu::GetCodecVersion(GetCodecVersion {}),
            Self::ListPanes => Pdu::ListPanes(ListPanes {}),
            Self::ListPanesTabStacks => Pdu::ListPanesTabStacks(ListPanesTabStacks {}),
            Self::GetClientList => Pdu::GetClientList(GetClientList),
            Self::SetClientId {
                is_proxy,
                hostname,
                username,
                pid,
                epoch,
                id,
                ssh_auth_sock,
            } => Pdu::SetClientId(SetClientId {
                client_id: ClientId {
                    hostname,
                    username,
                    pid,
                    epoch,
                    id,
                    ssh_auth_sock,
                },
                is_proxy,
            }),
            Self::SetWindowWorkspace {
                window_id,
                workspace,
            } => Pdu::SetWindowWorkspace(SetWindowWorkspace {
                window_id,
                workspace,
            }),
            Self::SetActiveWorkspace(workspace) => {
                Pdu::SetActiveWorkspace(SetActiveWorkspace { workspace })
            }
            Self::RenameWorkspace {
                old_workspace,
                new_workspace,
            } => Pdu::RenameWorkspace(RenameWorkspace {
                old_workspace,
                new_workspace,
            }),
            Self::SetFocusedPane(pane_id) => Pdu::SetFocusedPane(SetFocusedPane { pane_id }),
            Self::KillPane(pane_id) => Pdu::KillPane(KillPane { pane_id }),
            Self::GetPaneRenderChanges(pane_id) => {
                Pdu::GetPaneRenderChanges(GetPaneRenderChanges { pane_id })
            }
            Self::WriteToPane { pane_id, data } => Pdu::WriteToPane(WriteToPane { pane_id, data }),
            Self::SendPaste { pane_id, data } => Pdu::SendPaste(SendPaste { pane_id, data }),
        }
    }
}

struct ScopedMux {
    prior: Option<Arc<Mux>>,
}

impl ScopedMux {
    fn install_empty() -> Self {
        let prior = Mux::try_get();
        Mux::set_mux(&Arc::new(Mux::new(None)));
        Self { prior }
    }

    fn shutdown_current() -> Self {
        let prior = Mux::try_get();
        Mux::shutdown();
        Self { prior }
    }
}

impl Drop for ScopedMux {
    fn drop(&mut self) {
        if let Some(prior) = self.prior.take() {
            Mux::set_mux(&prior);
        } else {
            Mux::shutdown();
        }
    }
}

fn capturing_sender() -> (PduSender, Arc<Mutex<Vec<DecodedPdu>>>) {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let captured_clone = Arc::clone(&captured);
    let sender = PduSender::new(move |pdu| {
        captured_clone
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .push(pdu);
        Ok(())
    });
    (sender, captured)
}

fn bounded_string(u: &mut Unstructured<'_>) -> ArbitraryResult<String> {
    let len = u.int_in_range(0..=MAX_STRING_LEN)?;
    Ok(String::from_utf8_lossy(u.bytes(len)?).into_owned())
}

fn bounded_bytes(u: &mut Unstructured<'_>) -> ArbitraryResult<Vec<u8>> {
    let len = u.int_in_range(0..=MAX_BYTES_LEN)?;
    Ok(u.bytes(len)?.to_vec())
}

fn bounded_id(u: &mut Unstructured<'_>) -> ArbitraryResult<usize> {
    u.int_in_range(0..=MAX_ID)
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_LEN {
        return;
    }

    let mut u = Unstructured::new(data);
    let Ok(case) = FuzzCase::arbitrary(&mut u) else {
        return;
    };

    let _global_guard = GLOBAL_STATE_LOCK
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    let _executor = SimpleExecutor::new();
    let _mux_guard = if case.install_mux {
        ScopedMux::install_empty()
    } else {
        ScopedMux::shutdown_current()
    };
    let (sender, captured) = capturing_sender();
    let mut handler = SessionHandler::new(sender);

    for frame in case.frames {
        handler.process_one(DecodedPdu {
            serial: frame.serial,
            pdu: frame.pdu.into_pdu(),
        });

        let mut responses = captured.lock().unwrap_or_else(|err| err.into_inner());
        for response in responses.drain(..) {
            if matches!(response.pdu, Pdu::Invalid { .. }) {
                continue;
            }
            let mut encoded = Vec::new();
            let _ = response.pdu.encode(&mut encoded, response.serial);
        }
    }
});
