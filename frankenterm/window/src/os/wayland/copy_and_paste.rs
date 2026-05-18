use anyhow::{anyhow, bail};
use smithay_client_toolkit as toolkit;
use std::io::{ErrorKind, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::{Arc, Mutex};
use toolkit::data_device_manager::data_offer::SelectionOffer;
use toolkit::data_device_manager::{ReadPipe, WritePipe};
use toolkit::primary_selection::device::PrimarySelectionDeviceHandler;
use toolkit::primary_selection::selection::PrimarySelectionSourceHandler;
use wayland_protocols::wp::primary_selection::zv1::client::zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1;
use wayland_protocols::wp::primary_selection::zv1::client::zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1;

use crate::{Clipboard, ConnectionOps};

use super::data_device::TEXT_MIME_TYPE;
use super::state::WaylandState;

#[derive(Default)]
pub struct CopyAndPaste {
    data_offer: Option<SelectionOffer>,
}

impl std::fmt::Debug for CopyAndPaste {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        fmt.debug_struct("CopyAndPaste")
            .field("data_offer", &self.data_offer.is_some())
            .finish()
    }
}

impl CopyAndPaste {
    pub(super) fn create() -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Default::default()))
    }

    pub(super) fn get_clipboard_data(&mut self, clipboard: Clipboard) -> anyhow::Result<ReadPipe> {
        let Some(conn) = crate::Connection::get() else {
            bail!("Wayland connection is unavailable while reading clipboard");
        };
        let conn = conn.wayland();
        let wayland_state = conn.wayland_state.borrow();
        let primary_selection = if let Clipboard::PrimarySelection = clipboard {
            wayland_state.primary_selection_device.as_ref()
        } else {
            None
        };

        match primary_selection {
            Some(primary_selection) => {
                let offer = primary_selection
                    .data()
                    .selection_offer()
                    .ok_or_else(|| anyhow!("no primary selection offer"))?;
                let pipe = offer.receive(TEXT_MIME_TYPE.to_string())?;
                Ok(pipe)
            }
            None => {
                let offer = self
                    .data_offer
                    .as_ref()
                    .ok_or_else(|| anyhow!("no data offer"))?;
                let pipe = offer.receive(TEXT_MIME_TYPE.to_string())?;
                Ok(pipe)
            }
        }
    }

    pub(super) fn set_clipboard_data(&mut self, clipboard: Clipboard, data: String) {
        let Some(conn) = crate::Connection::get() else {
            log::warn!("Wayland connection is unavailable while setting clipboard");
            return;
        };
        let conn = conn.wayland();
        let qh = conn.event_queue.borrow().handle();
        let mut wayland_state = conn.wayland_state.borrow_mut();
        let last_serial = *wayland_state.last_serial.borrow();

        let primary_selection = if let Clipboard::PrimarySelection = clipboard {
            wayland_state.primary_selection_device.as_ref()
        } else {
            None
        };

        match primary_selection {
            Some(primary_selection) => {
                let Some(manager) = wayland_state.primary_selection_manager.as_ref() else {
                    log::warn!(
                        "Wayland primary selection device is present without a selection manager"
                    );
                    return;
                };
                let source = manager.create_selection_source(&qh, [TEXT_MIME_TYPE]);
                source.set_selection(&primary_selection, last_serial);
                wayland_state
                    .primary_selection_source
                    .replace((source, data));
            }
            None => {
                let Some(data_device) = wayland_state.data_device.as_ref() else {
                    log::warn!("Wayland data device is unavailable while setting clipboard");
                    return;
                };
                let source = wayland_state
                    .data_device_manager_state
                    .create_copy_paste_source(&qh, vec![TEXT_MIME_TYPE]);
                source.set_selection(data_device, last_serial);
                wayland_state.copy_paste_source.replace((source, data));
            }
        }
    }

    pub(super) fn confirm_selection(&mut self, offer: SelectionOffer) {
        self.data_offer.replace(offer);
    }
}

impl WaylandState {
    pub(super) fn resolve_copy_and_paste(&mut self) -> Option<Arc<Mutex<CopyAndPaste>>> {
        let active_surface_id = self.active_surface_id.borrow();
        let Some(active_surface_id) = active_surface_id.as_ref() else {
            log::warn!("Wayland clipboard selection arrived without an active surface");
            return None;
        };
        let Some(pending) = self.surface_to_pending.get(active_surface_id) else {
            log::warn!(
                "Wayland clipboard selection arrived without pending surface state for {:?}",
                active_surface_id
            );
            return None;
        };

        match pending.lock() {
            Ok(pending) => Some(Arc::clone(&pending.copy_and_paste)),
            Err(_) => {
                log::error!(
                    "Wayland pending surface lock was poisoned while resolving clipboard selection"
                );
                None
            }
        }
    }
}

pub(super) fn write_selection_to_pipe(fd: WritePipe, text: &str) {
    if let Err(e) = write_pipe_with_timeout(fd, text.as_bytes()) {
        log::error!("while sending primary selection to pipe: {}", e);
    }
}

pub(super) fn set_pipe_nonblocking(fd: RawFd) -> anyhow::Result<()> {
    let flags = loop {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags != -1 {
            break flags;
        }

        let err = std::io::Error::last_os_error();
        if err.kind() == ErrorKind::Interrupted {
            continue;
        }
        bail!("failed to read pipe status flags: {err}");
    };

    if flags & libc::O_NONBLOCK != 0 {
        return Ok(());
    }

    loop {
        let status = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        if status != -1 {
            return Ok(());
        }

        let err = std::io::Error::last_os_error();
        if err.kind() == ErrorKind::Interrupted {
            continue;
        }
        bail!("failed to change non-blocking mode: {err}");
    }
}

fn write_pipe_with_timeout(mut file: WritePipe, data: &[u8]) -> anyhow::Result<()> {
    set_pipe_nonblocking(file.as_raw_fd())?;

    let mut pfd = libc::pollfd {
        fd: file.as_raw_fd(),
        events: libc::POLLOUT,
        revents: 0,
    };

    let mut buf = data;

    while !buf.is_empty() {
        pfd.revents = 0;
        let poll_result = unsafe { libc::poll(&mut pfd, 1, 3000) };
        if poll_result > 0 {
            let unavailable = libc::POLLERR | libc::POLLHUP | libc::POLLNVAL;
            if pfd.revents & unavailable != 0 {
                bail!("write pipe became unavailable: revents={:#x}", pfd.revents);
            }
            if pfd.revents & libc::POLLOUT == 0 {
                continue;
            }

            match file.write(buf) {
                Ok(0) => bail!("zero byte write"),
                Ok(size) => buf = &buf[size..],
                Err(e) if matches!(e.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock) => {
                    continue;
                }
                Err(e) => bail!("error writing to pipe: {e}"),
            }
        } else if poll_result == 0 {
            bail!("timed out writing to pipe");
        } else {
            let err = std::io::Error::last_os_error();
            if err.kind() == ErrorKind::Interrupted {
                continue;
            }
            bail!("error polling write pipe: {err}");
        }
    }

    Ok(())
}

impl PrimarySelectionDeviceHandler for WaylandState {
    fn selection(
        &mut self,
        _conn: &wayland_client::Connection,
        _qh: &wayland_client::QueueHandle<Self>,
        _primary_selection_device: &ZwpPrimarySelectionDeviceV1,
    ) {
        // TODO: do we need to do anything here?
    }
}

impl PrimarySelectionSourceHandler for WaylandState {
    fn send_request(
        &mut self,
        _conn: &wayland_client::Connection,
        _qh: &wayland_client::QueueHandle<Self>,
        source: &ZwpPrimarySelectionSourceV1,
        mime: String,
        write_pipe: toolkit::data_device_manager::WritePipe,
    ) {
        if mime != TEXT_MIME_TYPE {
            return;
        };

        if let Some((ps_source, data)) = &self.primary_selection_source {
            if ps_source.inner() != source {
                return;
            }
            write_selection_to_pipe(write_pipe, data);
        }
    }

    fn cancelled(
        &mut self,
        _conn: &wayland_client::Connection,
        _qh: &wayland_client::QueueHandle<Self>,
        source: &ZwpPrimarySelectionSourceV1,
    ) {
        self.primary_selection_source.take();
        source.destroy();
    }
}

#[cfg(test)]
mod tests {
    use super::{set_pipe_nonblocking, write_pipe_with_timeout};
    use smithay_client_toolkit::data_device_manager::WritePipe;
    use std::fs::File;
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};

    fn pipe_pair() -> (OwnedFd, OwnedFd) {
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }

    #[test]
    fn set_pipe_nonblocking_preserves_existing_status_flags() {
        let (_read_fd, write_fd) = pipe_pair();
        let fd = write_fd.as_raw_fd();
        let original = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert_ne!(original, -1);
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_SETFL, original | libc::O_APPEND) },
            0
        );

        set_pipe_nonblocking(fd).unwrap();

        let updated = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert_ne!(updated, -1);
        assert_ne!(updated & libc::O_NONBLOCK, 0);
        assert_ne!(updated & libc::O_APPEND, 0);
    }

    #[test]
    fn write_pipe_with_timeout_writes_large_payload() {
        let (read_fd, write_fd) = pipe_pair();
        let reader = std::thread::spawn(move || {
            let mut file = File::from(read_fd);
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).unwrap();
            bytes
        });
        let payload = vec![b'x'; 128 * 1024];
        let write_pipe = unsafe { WritePipe::from_raw_fd(write_fd.into_raw_fd()) };

        write_pipe_with_timeout(write_pipe, &payload).unwrap();

        assert_eq!(reader.join().unwrap(), payload);
    }
}
