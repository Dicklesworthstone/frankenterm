//! Audited native C boundary for the phone's mux client
//! (`frankenterm-mobile-mux`): the protocol a FrankenTerm mux server speaks,
//! as a byte-in/byte-out state machine. The app owns the SSH channel and the
//! terminals; this boundary owns only protocol state.
//!
//! Same contract as `native_ffi`: callers provide valid, aligned,
//! nonoverlapping initialized byte buffers and writable scalar outputs;
//! pointers are checked, borrowed only inside their call and never stored.
//! Rust-owned state is reachable only through generation-checked handles.
//! A mutex serializes operations; no callback, await or I/O runs under it.
//! A panic poisons only the affected session. See
//! include/frankenterm_terminal.h for the public contract.

#![deny(unsafe_op_in_unsafe_fn)]

use crate::native::NativeTerminalError as Error;
use crate::native_ffi::{input_bytes, output_bytes, output_scalar, utf8};
use frankenterm_mobile_mux::{Event, SearchQuery, Session};
use frankenterm_sigpipe::{catch_recoverable, RecoverablePanicSite};
use std::convert::TryFrom;
use std::panic::AssertUnwindSafe;
use std::sync::{LazyLock, Mutex};

const SLOTS: usize = 16;
const MAX_GENERATION: u64 = u64::MAX >> 4;
/// Channel bytes one receive call may deliver.
pub const MAX_RECEIVE_BYTES: usize = 1 << 20;
/// Input bytes one write or paste may carry.
pub const MAX_WRITE_BYTES: usize = 1 << 16;
/// The most bytes one outgoing or event copy may return.
pub const MAX_COPY_BYTES: usize = 1 << 24;

const EVENT_NONE: u32 = 0;
const EVENT_READY: u32 = 1;
const EVENT_PANES: u32 = 2;
const EVENT_OUTPUT: u32 = 3;
const EVENT_PANE_TITLE: u32 = 4;
const EVENT_PANE_GONE: u32 = 5;
const EVENT_FAILED: u32 = 6;
const EVENT_SEARCH_RESULTS: u32 = 7;
const EVENT_CONTEXT: u32 = 8;
const EVENT_PHONE_SIZE: u32 = 9;
const EVENT_MOUSE_GRAB: u32 = 10;
const EVENT_ACTIVITY: u32 = 11;
const EVENT_INCARNATION: u32 = 12;
const EVENT_PHONE_SIZE_RESTORED: u32 = 13;
const EVENT_PANE_WAITING: u32 = 14;
const EVENT_PANE_IDENTITY: u32 = 15;
/// Search flags.
const SEARCH_CASE_SENSITIVE: u32 = 1;
const SEARCH_REGEX: u32 = 2;

#[derive(Default)]
struct Slot {
    generation: u64,
    session: Option<Session>,
    poisoned: bool,
    outgoing: Vec<u8>,
    event: Vec<u8>,
}

struct Registry {
    slots: [Slot; SLOTS],
}

impl Registry {
    fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| Slot::default()),
        }
    }

    fn index(&self, handle: u64) -> Result<usize, Error> {
        let index = (handle & 15) as usize;
        let generation = handle >> 4;
        let slot = &self.slots[index];
        if generation == 0 || generation != slot.generation || slot.session.is_none() {
            return Err(Error::Stale);
        }
        Ok(index)
    }

    fn slot(&mut self, handle: u64) -> Result<&mut Slot, Error> {
        let index = self.index(handle)?;
        let slot = &mut self.slots[index];
        if slot.poisoned {
            return Err(Error::Closed);
        }
        Ok(slot)
    }

    fn session(&mut self, handle: u64) -> Result<&mut Session, Error> {
        self.slot(handle)?.session.as_mut().ok_or(Error::Stale)
    }

    fn create(&mut self) -> Result<u64, Error> {
        let index = self
            .slots
            .iter()
            .position(|slot| slot.session.is_none() && slot.generation < MAX_GENERATION)
            .ok_or(Error::Capacity)?;
        let slot = &mut self.slots[index];
        slot.generation += 1;
        slot.session = Some(Session::new());
        slot.poisoned = false;
        slot.outgoing.clear();
        slot.event.clear();
        Ok((slot.generation << 4) | index as u64)
    }

    fn release(&mut self, handle: u64) -> Result<(), Error> {
        let index = self.index(handle)?;
        let slot = &mut self.slots[index];
        slot.session = None;
        slot.poisoned = false;
        slot.outgoing = Vec::new();
        slot.event = Vec::new();
        Ok(())
    }

    fn poison(&mut self, handle: u64) {
        if let Ok(index) = self.index(handle) {
            self.slots[index].poisoned = true;
        }
    }

    /// Copies the next bytes to send; copied bytes count as taken.
    fn outgoing(&mut self, handle: u64, output: &mut [u8]) -> Result<usize, Error> {
        let slot = self.slot(handle)?;
        if let Some(session) = slot.session.as_mut() {
            slot.outgoing.extend_from_slice(&session.take_outgoing());
        }
        let count = output.len().min(slot.outgoing.len());
        output[..count].copy_from_slice(&slot.outgoing[..count]);
        slot.outgoing.drain(..count);
        Ok(count)
    }

    /// Moves the next event's payload into the slot for `event_copy`. The
    /// event has already left the session: one that cannot be represented is
    /// reported as FAILED rather than lost, and the slot never keeps the
    /// previous event's payload.
    fn next_event(&mut self, handle: u64) -> Result<(u32, u64, u32, u32, u32), Error> {
        let slot = self.slot(handle)?;
        let event = slot.session.as_mut().ok_or(Error::Stale)?.next_event();
        let (kind, pane, rows, cols, payload) = event_parts(event)
            .and_then(|parts| {
                u32::try_from(parts.4.len())
                    .map(|_| parts)
                    .map_err(|_| Error::Capacity)
            })
            .unwrap_or_else(|_| {
                (
                    EVENT_FAILED,
                    0,
                    0,
                    0,
                    b"an event from the mux server could not be delivered".to_vec(),
                )
            });
        let length = u32::try_from(payload.len()).map_err(|_| Error::Capacity)?;
        slot.event = payload;
        Ok((kind, pane, rows, cols, length))
    }

    fn event_copy(
        &mut self,
        handle: u64,
        offset: usize,
        output: &mut [u8],
    ) -> Result<usize, Error> {
        let slot = self.slot(handle)?;
        if offset > slot.event.len() {
            return Err(Error::Invalid);
        }
        let count = output.len().min(slot.event.len() - offset);
        output[..count].copy_from_slice(&slot.event[offset..offset + count]);
        Ok(count)
    }
}

/// An event as (kind, pane, rows, cols, payload).
fn event_parts(event: Option<Event>) -> Result<(u32, u64, u32, u32, Vec<u8>), Error> {
    Ok(match event {
        None => (EVENT_NONE, 0, 0, 0, Vec::new()),
        Some(Event::Ready { server_version }) => {
            (EVENT_READY, 0, 0, 0, server_version.into_bytes())
        }
        Some(Event::Incarnation { id }) => (EVENT_INCARNATION, 0, 0, 0, id.to_vec()),
        Some(Event::PaneIdentity { pane_id, identity }) => {
            let mut payload = Vec::new();
            if let Some(identity) = identity {
                payload.reserve_exact(48);
                payload.extend_from_slice(&identity.session_incarnation);
                payload.extend_from_slice(&identity.durable_pane_id);
                payload.extend_from_slice(&identity.pane_registration);
            }
            (EVENT_PANE_IDENTITY, pane_id as u64, 0, 0, payload)
        }
        Some(Event::Panes(panes)) => (
            EVENT_PANES,
            0,
            0,
            0,
            serde_json::to_vec(&panes).map_err(|_| Error::Invalid)?,
        ),
        Some(Event::Output {
            pane_id,
            rows,
            cols,
            bytes,
        }) => (
            EVENT_OUTPUT,
            pane_id as u64,
            count(rows)?,
            count(cols)?,
            bytes,
        ),
        Some(Event::PaneTitle { pane_id, title }) => {
            (EVENT_PANE_TITLE, pane_id as u64, 0, 0, title.into_bytes())
        }
        Some(Event::PaneGone { pane_id }) => (EVENT_PANE_GONE, pane_id as u64, 0, 0, Vec::new()),
        Some(Event::Failed { reason }) => (EVENT_FAILED, 0, 0, 0, reason.into_bytes()),
        Some(Event::MouseGrab { pane_id, grabbed }) => (
            EVENT_MOUSE_GRAB,
            pane_id as u64,
            0,
            0,
            vec![u8::from(grabbed)],
        ),
        Some(Event::Activity { pane_id }) => (EVENT_ACTIVITY, pane_id as u64, 0, 0, Vec::new()),
        Some(Event::PaneWaiting { pane_id, waiting }) => (
            EVENT_PANE_WAITING,
            pane_id as u64,
            0,
            0,
            vec![u8::from(waiting)],
        ),
        Some(Event::PhoneSizeRestored { pane_id }) => {
            (EVENT_PHONE_SIZE_RESTORED, pane_id as u64, 0, 0, Vec::new())
        }
        Some(Event::PhoneSize {
            pane_id,
            desktop,
            rows,
            cols,
        }) => (
            EVENT_PHONE_SIZE,
            pane_id as u64,
            count(rows)?,
            count(cols)?,
            match desktop {
                Some(size) => serde_json::to_vec(&size).map_err(|_| Error::Invalid)?,
                None => Vec::new(),
            },
        ),
        Some(Event::SearchResults {
            search,
            pane_id,
            matches,
            searched_rows,
            error,
        }) => (
            EVENT_SEARCH_RESULTS,
            pane_id as u64,
            0,
            0,
            serde_json::to_vec(&serde_json::json!({
                "search": search,
                "matches": matches,
                "searched_rows": searched_rows,
                "error": error,
            }))
            .map_err(|_| Error::Invalid)?,
        ),
        Some(Event::Context {
            request,
            pane_id,
            lines,
        }) => (
            EVENT_CONTEXT,
            pane_id as u64,
            0,
            0,
            serde_json::to_vec(&serde_json::json!({ "request": request, "lines": lines }))
                .map_err(|_| Error::Invalid)?,
        ),
    })
}

fn count(value: usize) -> Result<u32, Error> {
    u32::try_from(value).map_err(|_| Error::Invalid)
}

fn pane(value: u64) -> Result<usize, Error> {
    usize::try_from(value).map_err(|_| Error::Invalid)
}

static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| Mutex::new(Registry::new()));

fn boundary(owner: Option<u64>, operation: impl FnOnce(&mut Registry) -> Result<(), Error>) -> u32 {
    frankenterm_sigpipe::install_recoverable_boundary_hook_once();
    let mut registry = REGISTRY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match catch_recoverable(
        RecoverablePanicSite::NativeTerminalBridge,
        AssertUnwindSafe(|| operation(&mut registry)),
    ) {
        Ok(Ok(())) => 0,
        Ok(Err(error)) => error as u32,
        Err(_) => {
            if let Some(owner) = owner {
                registry.poison(owner);
            }
            Error::Panic as u32
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn ft_mux_v1_create(session: *mut u64) -> u32 {
    boundary(None, |registry| {
        unsafe {
            output_scalar(session, 0)?;
        }
        let handle = registry.create()?;
        unsafe {
            output_scalar(session, handle)?;
        }
        Ok(())
    })
}

#[no_mangle]
pub extern "C" fn ft_mux_v1_release(session: u64) -> u32 {
    boundary(None, |registry| registry.release(session))
}

#[no_mangle]
pub unsafe extern "C" fn ft_mux_v1_receive(session: u64, bytes: *const u8, length: u32) -> u32 {
    boundary(Some(session), |registry| {
        let bytes = unsafe { input_bytes(bytes, length, MAX_RECEIVE_BYTES)? };
        registry.session(session)?.receive(bytes);
        Ok(())
    })
}

#[no_mangle]
pub extern "C" fn ft_mux_v1_closed(session: u64) -> u32 {
    boundary(Some(session), |registry| {
        registry.session(session)?.closed();
        Ok(())
    })
}

/// Advance the caller's monotonic clock and report the next absolute input
/// wake. As with the other scalar outputs, both pointers must be writable,
/// aligned, and nonoverlapping. Validate them before changing session state.
#[no_mangle]
pub unsafe extern "C" fn ft_mux_v1_tick(
    session: u64,
    now_ns: u64,
    has_wake: *mut u32,
    wake_ns: *mut u64,
) -> u32 {
    boundary(Some(session), |registry| {
        unsafe {
            output_scalar(has_wake, 0)?;
            output_scalar(wake_ns, 0)?;
        }
        let slot = registry.slot(session)?;
        let state = slot.session.as_mut().ok_or(Error::Stale)?;
        if !state.advance_time(now_ns) {
            // Bytes already copied to the owner are its responsibility. Do
            // not release any remaining local egress after deadline/clock
            // failure; CLOSED tells the owner to stop sending and tear down.
            slot.outgoing.clear();
            return Err(Error::Closed);
        }
        if let Some(wake) = state.next_input_wake_ns() {
            unsafe {
                output_scalar(has_wake, 1)?;
                output_scalar(wake_ns, wake)?;
            }
        }
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn ft_mux_v1_outgoing(
    session: u64,
    output: *mut u8,
    capacity: u32,
    written: *mut u32,
) -> u32 {
    boundary(Some(session), |registry| {
        unsafe {
            output_scalar(written, 0)?;
        }
        let output = unsafe { output_bytes(output, capacity, MAX_COPY_BYTES)? };
        let count = registry.outgoing(session, output)?;
        unsafe {
            output_scalar(written, count as u32)?;
        }
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn ft_mux_v1_next_event(
    session: u64,
    kind: *mut u32,
    pane_id: *mut u64,
    rows: *mut u32,
    cols: *mut u32,
    length: *mut u32,
) -> u32 {
    boundary(Some(session), |registry| {
        unsafe {
            output_scalar(kind, EVENT_NONE)?;
            output_scalar(pane_id, 0)?;
            output_scalar(rows, 0)?;
            output_scalar(cols, 0)?;
            output_scalar(length, 0)?;
        }
        let (event, pane, event_rows, event_cols, event_length) = registry.next_event(session)?;
        unsafe {
            output_scalar(kind, event)?;
            output_scalar(pane_id, pane)?;
            output_scalar(rows, event_rows)?;
            output_scalar(cols, event_cols)?;
            output_scalar(length, event_length)?;
        }
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn ft_mux_v1_event_copy(
    session: u64,
    offset: u32,
    output: *mut u8,
    capacity: u32,
    written: *mut u32,
) -> u32 {
    boundary(Some(session), |registry| {
        unsafe {
            output_scalar(written, 0)?;
        }
        let output = unsafe { output_bytes(output, capacity, MAX_COPY_BYTES)? };
        let count = registry.event_copy(session, offset as usize, output)?;
        unsafe {
            output_scalar(written, count as u32)?;
        }
        Ok(())
    })
}

#[no_mangle]
pub extern "C" fn ft_mux_v1_refresh_panes(session: u64) -> u32 {
    boundary(Some(session), |registry| {
        registry.session(session)?.refresh_panes();
        Ok(())
    })
}

#[no_mangle]
pub extern "C" fn ft_mux_v1_inspect_pane_identity(session: u64, pane_id: u64) -> u32 {
    boundary(Some(session), |registry| {
        let pane_id = pane(pane_id)?;
        let session = registry.session(session)?;
        if !session.is_ready() {
            return Err(Error::Closed);
        }
        if !session.inspect_pane_identity(pane_id) {
            return Err(Error::Capacity);
        }
        Ok(())
    })
}

#[no_mangle]
pub extern "C" fn ft_mux_v1_show(session: u64, pane_id: u64) -> u32 {
    boundary(Some(session), |registry| {
        let pane_id = pane(pane_id)?;
        registry.session(session)?.show(pane_id);
        Ok(())
    })
}

#[no_mangle]
pub extern "C" fn ft_mux_v1_hide(session: u64, pane_id: u64) -> u32 {
    boundary(Some(session), |registry| {
        let pane_id = pane(pane_id)?;
        registry.session(session)?.hide(pane_id);
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn ft_mux_v1_write(
    session: u64,
    pane_id: u64,
    bytes: *const u8,
    length: u32,
) -> u32 {
    boundary(Some(session), |registry| {
        let pane_id = pane(pane_id)?;
        let bytes = unsafe { input_bytes(bytes, length, MAX_WRITE_BYTES)? };
        registry.session(session)?.write(pane_id, bytes);
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn ft_mux_v1_paste(
    session: u64,
    pane_id: u64,
    text: *const u8,
    length: u32,
) -> u32 {
    boundary(Some(session), |registry| {
        let pane_id = pane(pane_id)?;
        let text = utf8(unsafe { input_bytes(text, length, MAX_WRITE_BYTES)? })?;
        registry.session(session)?.paste(pane_id, text);
        Ok(())
    })
}

#[no_mangle]
pub extern "C" fn ft_mux_v1_resize(session: u64, pane_id: u64, rows: u32, cols: u32) -> u32 {
    boundary(Some(session), |registry| {
        let pane_id = pane(pane_id)?;
        if rows == 0 || cols == 0 || rows > 512 || cols > 512 {
            return Err(Error::Invalid);
        }
        registry
            .session(session)?
            .resize(pane_id, rows as usize, cols as usize);
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn ft_mux_v1_search(
    session: u64,
    pane_id: u64,
    text: *const u8,
    length: u32,
    flags: u32,
    search: *mut u64,
) -> u32 {
    boundary(Some(session), |registry| {
        unsafe {
            output_scalar(search, 0)?;
        }
        let pane_id = pane(pane_id)?;
        if flags & !(SEARCH_CASE_SENSITIVE | SEARCH_REGEX) != 0 || length == 0 {
            return Err(Error::Invalid);
        }
        let text = utf8(unsafe { input_bytes(text, length, 4096)? })?;
        let query = SearchQuery {
            text: text.to_owned(),
            case_sensitive: flags & SEARCH_CASE_SENSITIVE != 0,
            regex: flags & SEARCH_REGEX != 0,
        };
        let id = registry
            .session(session)?
            .search(pane_id, &query)
            .ok_or(Error::Closed)?;
        unsafe {
            output_scalar(search, id)?;
        }
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn ft_mux_v1_context(
    session: u64,
    pane_id: u64,
    first_row: i64,
    count: u32,
    request: *mut u64,
) -> u32 {
    boundary(Some(session), |registry| {
        unsafe {
            output_scalar(request, 0)?;
        }
        let pane_id = pane(pane_id)?;
        let first = isize::try_from(first_row).map_err(|_| Error::Invalid)?;
        let end = first
            .checked_add(isize::try_from(count).map_err(|_| Error::Invalid)?)
            .ok_or(Error::Invalid)?;
        let id = registry
            .session(session)?
            .context(pane_id, first..end)
            .ok_or(Error::Invalid)?;
        unsafe {
            output_scalar(request, id)?;
        }
        Ok(())
    })
}

#[no_mangle]
pub extern "C" fn ft_mux_v1_wheel(
    session: u64,
    pane_id: u64,
    steps: i32,
    column: u32,
    row: u32,
) -> u32 {
    boundary(Some(session), |registry| {
        let pane_id = pane(pane_id)?;
        if steps == 0 || column > 4096 || row > 4096 {
            return Err(Error::Invalid);
        }
        registry
            .session(session)?
            .wheel(pane_id, steps, column as usize, row as usize);
        Ok(())
    })
}

#[no_mangle]
pub extern "C" fn ft_mux_v1_click(session: u64, pane_id: u64, column: u32, row: u32) -> u32 {
    boundary(Some(session), |registry| {
        let pane_id = pane(pane_id)?;
        if column > 4096 || row > 4096 {
            return Err(Error::Invalid);
        }
        registry
            .session(session)?
            .click(pane_id, column as usize, row as usize);
        Ok(())
    })
}

#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn ft_mux_v1_adopt_phone_size(
    session: u64,
    pane_id: u64,
    desktop_rows: u32,
    desktop_cols: u32,
    desktop_pixel_width: u32,
    desktop_pixel_height: u32,
    desktop_dpi: u32,
    rows: u32,
    cols: u32,
    adopted: *mut u32,
) -> u32 {
    boundary(Some(session), |registry| {
        unsafe {
            output_scalar(adopted, 0)?;
        }
        let pane_id = pane(pane_id)?;
        let cells = 1..=4096u32;
        if !cells.contains(&desktop_rows)
            || !cells.contains(&desktop_cols)
            || !(1..=512).contains(&rows)
            || !(1..=512).contains(&cols)
        {
            return Err(Error::Invalid);
        }
        let desktop = frankenterm_mobile_mux::wire::TerminalSize {
            rows: desktop_rows as usize,
            cols: desktop_cols as usize,
            pixel_width: desktop_pixel_width as usize,
            pixel_height: desktop_pixel_height as usize,
            dpi: desktop_dpi,
        };
        let taken = registry.session(session)?.adopt_phone_size(
            pane_id,
            desktop,
            rows as usize,
            cols as usize,
        );
        unsafe {
            output_scalar(adopted, u32::from(taken))?;
        }
        Ok(())
    })
}

#[no_mangle]
pub extern "C" fn ft_mux_v1_restore_size(session: u64, pane_id: u64) -> u32 {
    boundary(Some(session), |registry| {
        let pane_id = pane(pane_id)?;
        registry.session(session)?.restore_size(pane_id);
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn ft_mux_v1_restoring(session: u64, restoring: *mut u32) -> u32 {
    boundary(Some(session), |registry| {
        unsafe {
            output_scalar(restoring, 0)?;
        }
        let pending = registry.session(session)?.restoring();
        unsafe {
            output_scalar(restoring, u32::from(pending))?;
        }
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn ft_mux_v1_unanswered_input(
    session: u64,
    pane_id: u64,
    unanswered: *mut u32,
) -> u32 {
    boundary(Some(session), |registry| {
        unsafe {
            output_scalar(unanswered, 0)?;
        }
        let pane_id = pane(pane_id)?;
        let pending = registry.session(session)?.unanswered_input(pane_id);
        unsafe {
            output_scalar(unanswered, u32::from(pending))?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_identity_event_preserves_all_three_witnesses_and_distinguishes_unavailable() {
        let identity = frankenterm_mobile_mux::wire::PaneIdentity {
            session_incarnation: [0x11; 16],
            durable_pane_id: [0x22; 16],
            pane_registration: [0x33; 16],
        };
        let (kind, pane, rows, cols, payload) = event_parts(Some(Event::PaneIdentity {
            pane_id: 42,
            identity: Some(identity),
        }))
        .unwrap();
        assert_eq!((kind, pane, rows, cols), (EVENT_PANE_IDENTITY, 42, 0, 0));
        assert_eq!(payload.len(), 48);
        assert_eq!(&payload[..16], &identity.session_incarnation);
        assert_eq!(&payload[16..32], &identity.durable_pane_id);
        assert_eq!(&payload[32..], &identity.pane_registration);
        assert_eq!(
            event_parts(Some(Event::PaneIdentity {
                pane_id: 42,
                identity: None
            }))
            .unwrap(),
            (EVENT_PANE_IDENTITY, 42, 0, 0, Vec::new())
        );
    }

    fn take_outgoing(session: u64) -> Vec<u8> {
        let mut output = vec![0u8; 4096];
        let mut written = 0u32;
        // SAFETY: live local buffers with their exact lengths.
        let status = unsafe {
            ft_mux_v1_outgoing(
                session,
                output.as_mut_ptr(),
                output.len() as u32,
                &mut written,
            )
        };
        assert_eq!(status, 0);
        output.truncate(written as usize);
        output
    }

    #[test]
    fn clock_ticks_validate_outputs_before_mutation_and_fence_failed_egress() {
        let mut session = 0;
        // SAFETY: live local outputs, or an explicitly null output verified
        // to be rejected before any state mutation.
        unsafe {
            assert_eq!(ft_mux_v1_create(&mut session), 0);
            let (mut pending, mut wake) = (7u32, 7u64);
            assert_eq!(
                ft_mux_v1_tick(session, 10, &mut pending, std::ptr::null_mut()),
                Error::Invalid as u32
            );
            assert_eq!(pending, 0);
            assert_eq!(
                ft_mux_v1_tick(session, 5, &mut pending, &mut wake),
                0,
                "the invalid call must not have advanced the clock to 10"
            );
            assert_eq!((pending, wake), (0, 0));
            assert_eq!(ft_mux_v1_tick(session, 5, &mut pending, &mut wake), 0);
            assert_eq!(
                ft_mux_v1_tick(session, 4, &mut pending, &mut wake),
                Error::Closed as u32
            );
            assert_eq!((pending, wake), (0, 0));
            assert!(
                take_outgoing(session).is_empty(),
                "a backward tick cannot release queued bytes"
            );
            assert_eq!(ft_mux_v1_release(session), 0);
            assert_eq!(
                ft_mux_v1_tick(session, 6, &mut pending, &mut wake),
                Error::Stale as u32
            );
        }
    }

    #[test]
    fn a_session_handshakes_and_reports_a_refused_protocol_through_the_c_abi() {
        let mut session = 0u64;
        // SAFETY: a live local scalar.
        assert_eq!(unsafe { ft_mux_v1_create(&mut session) }, 0);
        assert_ne!(session, 0);
        // The first request is GetCodecVersion, serial 1.
        assert_eq!(take_outgoing(session), [2, 1, 26]);
        assert!(take_outgoing(session).is_empty());

        // A reply from a server newer than this client: codec_vers 72, version "x".
        let reply = [5u8, 1, 27, 72, 1, b'x'];
        // SAFETY: a live local buffer with its exact length.
        assert_eq!(
            unsafe { ft_mux_v1_receive(session, reply.as_ptr(), reply.len() as u32) },
            0
        );
        let (mut kind, mut pane, mut rows, mut cols, mut length) = (9u32, 9u64, 9u32, 9u32, 0u32);
        // SAFETY: live local scalars.
        let status = unsafe {
            ft_mux_v1_next_event(
                session,
                &mut kind,
                &mut pane,
                &mut rows,
                &mut cols,
                &mut length,
            )
        };
        assert_eq!((status, kind, pane, rows, cols), (0, EVENT_FAILED, 0, 0, 0));
        let mut reason = vec![0u8; length as usize];
        let mut written = 0u32;
        // SAFETY: a live local buffer with its exact length.
        let status =
            unsafe { ft_mux_v1_event_copy(session, 0, reason.as_mut_ptr(), length, &mut written) };
        assert_eq!((status, written), (0, length));
        let reason = String::from_utf8(reason).unwrap();
        assert!(reason.contains("72"), "{}", reason);

        // SAFETY: live local scalars.
        let status = unsafe {
            ft_mux_v1_next_event(
                session,
                &mut kind,
                &mut pane,
                &mut rows,
                &mut cols,
                &mut length,
            )
        };
        assert_eq!((status, kind, length), (0, EVENT_NONE, 0));

        assert_eq!(ft_mux_v1_resize(session, 1, 0, 80), Error::Invalid as u32);
        assert_eq!(ft_mux_v1_wheel(session, 1, 0, 0, 0), Error::Invalid as u32);
        assert_eq!(
            ft_mux_v1_wheel(session, 1, -3, 5000, 0),
            Error::Invalid as u32
        );
        assert_eq!(ft_mux_v1_release(session), 0);
        assert_eq!(ft_mux_v1_release(session), Error::Stale as u32);
        // SAFETY: a null pointer with length zero is the empty input.
        assert_eq!(
            unsafe { ft_mux_v1_receive(session, std::ptr::null(), 0) },
            Error::Stale as u32
        );
    }

    #[test]
    fn searches_wait_for_the_handshake_and_refuse_bad_arguments() {
        let mut session = 0u64;
        // SAFETY: a live local scalar.
        assert_eq!(unsafe { ft_mux_v1_create(&mut session) }, 0);
        let text = b"needle";
        let mut search = 9u64;
        // SAFETY: live local buffers and scalars with their exact lengths.
        unsafe {
            assert_eq!(
                ft_mux_v1_search(session, 1, text.as_ptr(), text.len() as u32, 0, &mut search),
                Error::Closed as u32,
                "no search before the server answered"
            );
            assert_eq!(search, 0);
            assert_eq!(
                ft_mux_v1_search(session, 1, text.as_ptr(), text.len() as u32, 4, &mut search),
                Error::Invalid as u32
            );
            assert_eq!(
                ft_mux_v1_search(session, 1, text.as_ptr(), 0, 0, &mut search),
                Error::Invalid as u32
            );
            let mut request = 9u64;
            assert_eq!(
                ft_mux_v1_context(session, 1, 0, 2000, &mut request),
                Error::Invalid as u32
            );
            assert_eq!(request, 0);
        }
        assert_eq!(ft_mux_v1_release(session), 0);
    }

    #[test]
    fn invalid_pointers_and_text_are_refused() {
        let mut session = 0u64;
        // SAFETY: a live local scalar.
        assert_eq!(unsafe { ft_mux_v1_create(&mut session) }, 0);
        // SAFETY: deliberately null with a nonzero length; refused before use.
        assert_eq!(
            unsafe { ft_mux_v1_receive(session, std::ptr::null(), 4) },
            Error::Invalid as u32
        );
        let invalid = [0xffu8, 0xfe];
        // SAFETY: a live local buffer with its exact length.
        assert_eq!(
            unsafe { ft_mux_v1_paste(session, 1, invalid.as_ptr(), invalid.len() as u32) },
            Error::Invalid as u32
        );
        let mut written = 7u32;
        // SAFETY: deliberately null output; refused before use.
        assert_eq!(
            unsafe { ft_mux_v1_outgoing(session, std::ptr::null_mut(), 8, &mut written) },
            Error::Invalid as u32
        );
        assert_eq!(written, 0);
        let mut restoring = 7u32;
        // SAFETY: a live local scalar.
        assert_eq!(unsafe { ft_mux_v1_restoring(session, &mut restoring) }, 0);
        assert_eq!(restoring, 0, "nothing given back, nothing to wait for");
        // SAFETY: deliberately null output; refused before use.
        assert_eq!(
            unsafe { ft_mux_v1_restoring(session, std::ptr::null_mut()) },
            Error::Invalid as u32
        );
        let mut unanswered = 7u32;
        // SAFETY: a live local scalar.
        assert_eq!(
            unsafe { ft_mux_v1_unanswered_input(session, 1, &mut unanswered) },
            0
        );
        assert_eq!(unanswered, 0, "nothing sent, nothing to wait for");
        // SAFETY: deliberately null output; refused before use.
        assert_eq!(
            unsafe { ft_mux_v1_unanswered_input(session, 1, std::ptr::null_mut()) },
            Error::Invalid as u32
        );
        assert_eq!(ft_mux_v1_release(session), 0);
        // SAFETY: a live local scalar.
        assert_eq!(
            unsafe { ft_mux_v1_unanswered_input(session, 1, &mut unanswered) },
            Error::Stale as u32
        );
        // SAFETY: a live local scalar.
        assert_eq!(
            unsafe { ft_mux_v1_restoring(session, &mut restoring) },
            Error::Stale as u32
        );
    }
}
