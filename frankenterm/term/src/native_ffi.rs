//! Audited native C boundary for the canonical terminal.
//!
//! Unsafe contract: callers provide valid, aligned, nonoverlapping initialized
//! byte buffers for each indicated range and writable scalar outputs. Raw
//! pointers are checked for null/alignment/finite lengths, borrowed only inside
//! their call, and never stored. Rust-owned data is reachable only through the
//! generation registry. No pointer cast conveys thread ownership. A mutex
//! serializes registry/model operations; no callback, await, task or transport
//! I/O occurs under it. Every operation uses the canonical recoverable-panic
//! boundary, and recovery poisons the affected owner before returning to C.
//!
//! This feature is an explicitly scoped FFI exception in the existing vendored
//! term crate, which does not inherit the workspace's unsafe-code prohibition.
//! No `unsafe impl Send/Sync`, layout transmutation or Rust reference crosses
//! the ABI. See the complete public contract in include/frankenterm_terminal.h.

#![deny(unsafe_op_in_unsafe_fn)]

use crate::native::{
    NativeKeyPhase, NativeTerminal, NativeTerminalConfig, NativeTerminalError as Error,
    MAX_FEED_BYTES, MAX_FRAME_BYTES, MAX_INPUT_BYTES, MAX_REPLY_BYTES, MAX_TEXT_BYTES,
};
use crate::{BoundedReplyDisposition, TerminalSize};
use frankenterm_sigpipe::{catch_recoverable, RecoverablePanicSite};
use std::convert::TryFrom;
use std::mem::size_of;
use std::panic::AssertUnwindSafe;
use std::sync::{LazyLock, Mutex};

const SESSION_SLOTS: usize = 16;
const MAX_TOTAL_RESIDENT_CELLS: usize = 262144;
const FRAME_KIND: u64 = 1 << 63;
const MAX_GENERATION: u64 = (FRAME_KIND - 1) >> 4;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct FtTerminalV1Config {
    abi_version: u32,
    struct_size: u32,
    columns: u32,
    rows: u32,
    pixel_width: u32,
    pixel_height: u32,
    scrollback_rows: u32,
    reserved: u32,
    channel_id: [u8; 16],
    transport_id: [u8; 16],
    route_revision: u64,
    font_revision: u64,
}

const _: [(); 80] = [(); size_of::<FtTerminalV1Config>()];
const _: [(); 32] = [(); std::mem::offset_of!(FtTerminalV1Config, channel_id)];
const _: [(); 64] = [(); std::mem::offset_of!(FtTerminalV1Config, route_revision)];

impl FtTerminalV1Config {
    fn validate(self) -> Result<NativeTerminalConfig, Error> {
        if self.abi_version != 1 || self.struct_size != 80 || self.reserved != 0 {
            return Err(Error::Invalid);
        }
        let config = NativeTerminalConfig {
            size: geometry(self.columns, self.rows, self.pixel_width, self.pixel_height),
            scrollback_rows: self.scrollback_rows as usize,
            channel_id: self.channel_id,
            transport_id: self.transport_id,
            route_revision: self.route_revision,
            font_revision: self.font_revision,
        };
        config.validate()?;
        Ok(config)
    }
}

fn geometry(columns: u32, rows: u32, pixel_width: u32, pixel_height: u32) -> TerminalSize {
    TerminalSize {
        cols: columns as usize,
        rows: rows as usize,
        pixel_width: pixel_width as usize,
        pixel_height: pixel_height as usize,
        dpi: 0,
    }
}

#[derive(Default)]
struct Slot {
    generation: u64,
    frame_generation: u64,
    // Independent of model configuration: a failed mutation may have changed
    // one screen before it updates that configuration. Retain the conservative
    // reservation after panic until this exact owner is released.
    charged_cells: usize,
    terminal: Option<NativeTerminal>,
    frame: Option<Vec<u8>>,
}

struct Registry {
    slots: [Slot; SESSION_SLOTS],
}

impl Registry {
    fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| Slot::default()),
        }
    }

    fn index(&self, handle: u64, frame: bool) -> Result<usize, Error> {
        if (handle & FRAME_KIND != 0) != frame {
            return Err(Error::Stale);
        }
        let index = (handle & 15) as usize;
        let generation = (handle & !FRAME_KIND) >> 4;
        let slot = &self.slots[index];
        let expected = if frame {
            slot.frame_generation
        } else {
            slot.generation
        };
        if generation == 0
            || generation != expected
            || slot.terminal.is_none()
            || (frame && slot.frame.is_none())
        {
            return Err(Error::Stale);
        }
        Ok(index)
    }

    fn session(&mut self, handle: u64) -> Result<&mut NativeTerminal, Error> {
        let index = self.index(handle, false)?;
        self.slots[index].terminal.as_mut().ok_or(Error::Stale)
    }

    fn resident_cells_except(&self, excluded: Option<usize>) -> Result<usize, Error> {
        self.slots
            .iter()
            .enumerate()
            .filter(|(index, _)| Some(*index) != excluded)
            .try_fold(0usize, |total, (_, slot)| {
                total.checked_add(slot.charged_cells).ok_or(Error::Capacity)
            })
    }

    fn create(&mut self, config: NativeTerminalConfig) -> Result<u64, Error> {
        config.validate()?;
        let cells = config.resident_cell_budget()?;
        if cells > MAX_TOTAL_RESIDENT_CELLS - self.resident_cells_except(None)? {
            return Err(Error::Capacity);
        }
        let index = self
            .slots
            .iter()
            .position(|slot| {
                slot.terminal.is_none()
                    && slot.charged_cells == 0
                    && slot.generation < MAX_GENERATION
            })
            .ok_or(Error::Capacity)?;
        // Construction completes before the generation/slot becomes visible.
        let terminal = NativeTerminal::new(config)?;
        let slot = &mut self.slots[index];
        slot.generation += 1;
        slot.charged_cells = cells;
        slot.terminal = Some(terminal);
        Ok((slot.generation << 4) | index as u64)
    }

    fn resize(&mut self, handle: u64, size: TerminalSize) -> Result<(), Error> {
        self.resize_with(handle, size, NativeTerminal::resize)
    }

    fn resize_with(
        &mut self,
        handle: u64,
        size: TerminalSize,
        resize: impl FnOnce(&mut NativeTerminal, TerminalSize) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let index = self.index(handle, false)?;
        let mut proposed = self.slots[index]
            .terminal
            .as_ref()
            .ok_or(Error::Stale)?
            .config();
        if proposed.size == size {
            return resize(
                self.slots[index].terminal.as_mut().ok_or(Error::Stale)?,
                size,
            );
        }
        proposed.size = size;
        let cells = proposed.resident_cell_budget()?;
        let old_charge = self.slots[index].charged_cells;
        // Reflow temporarily owns old and new row representations. A panic
        // can also leave primary/alternate screens at different geometries.
        // Reserve both before mutation; do not infer ownership from whichever
        // config fields happened to update before an unwind.
        let reservation = old_charge.checked_add(cells).ok_or(Error::Capacity)?;
        if reservation > MAX_TOTAL_RESIDENT_CELLS - self.resident_cells_except(Some(index))? {
            return Err(Error::Capacity);
        }
        let slot = &mut self.slots[index];
        slot.charged_cells = reservation;
        match resize(slot.terminal.as_mut().ok_or(Error::Stale)?, size) {
            Ok(()) => {
                slot.charged_cells = cells;
                Ok(())
            }
            // NativeTerminal returns ordinary resize errors only before
            // mutating the model. A panic skips this arm and keeps reservation.
            Err(error) => {
                slot.charged_cells = old_charge;
                Err(error)
            }
        }
    }

    fn release(&mut self, handle: u64) -> Result<(), Error> {
        let index = self.index(handle, false)?;
        // Release is unconditional cleanup, including an abandoned reply
        // lease. Its bytes are not replayed or sent anywhere. Any previously
        // admitted downstream data remains that independent owner's duty.
        let slot = &mut self.slots[index];
        slot.frame.take();
        let mut terminal = slot.terminal.take().ok_or(Error::Stale)?;
        terminal.poison();
        drop(terminal);
        slot.charged_cells = 0;
        Ok(())
    }

    fn acquire_frame(&mut self, handle: u64, viewport: Option<i64>) -> Result<(u64, u32), Error> {
        let index = self.index(handle, false)?;
        let slot = &mut self.slots[index];
        if slot.frame.is_some() {
            return Err(Error::Busy);
        }
        if slot.frame_generation == MAX_GENERATION {
            return Err(Error::SequenceExhausted);
        }
        let frame = slot
            .terminal
            .as_mut()
            .ok_or(Error::Stale)?
            .capture_frame(viewport)?;
        let length = u32::try_from(frame.len()).map_err(|_| Error::Capacity)?;
        slot.frame_generation += 1;
        slot.frame = Some(frame);
        Ok((
            FRAME_KIND | (slot.frame_generation << 4) | index as u64,
            length,
        ))
    }

    fn copy_frame(&self, handle: u64, offset: usize, output: &mut [u8]) -> Result<usize, Error> {
        let index = self.index(handle, true)?;
        let frame = self.slots[index].frame.as_ref().ok_or(Error::Stale)?;
        if offset > frame.len() || output.len() > MAX_FRAME_BYTES {
            return Err(Error::Invalid);
        }
        let count = output.len().min(frame.len() - offset);
        output[..count].copy_from_slice(&frame[offset..offset + count]);
        Ok(count)
    }

    fn release_frame(&mut self, handle: u64) -> Result<(), Error> {
        let index = self.index(handle, true)?;
        self.slots[index].frame.take();
        Ok(())
    }

    fn poison(&mut self, handle: u64) {
        if let Ok(index) = self.index(handle, handle & FRAME_KIND != 0) {
            if let Some(terminal) = self.slots[index].terminal.as_mut() {
                terminal.poison();
            }
        }
    }
}

static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| Mutex::new(Registry::new()));

fn boundary(owner: Option<u64>, operation: impl FnOnce(&mut Registry) -> Result<(), Error>) -> u32 {
    frankenterm_sigpipe::install_recoverable_boundary_hook_once();
    // The guard lives outside catch_recoverable, so a caught operation panic
    // does not poison the mutex or force unrelated sessions to fail. It does
    // poison the affected model, whose future state must not be trusted.
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

fn check_pointer<T>(pointer: *const T) -> Result<(), Error> {
    if pointer.is_null() || !pointer.is_aligned() {
        Err(Error::Invalid)
    } else {
        Ok(())
    }
}

/// SAFETY: pointer is valid writable storage for T, unique with respect to
/// every input/output range in this call. Initializing scalar outputs does not
/// require the caller's prior value to have been initialized.
unsafe fn output_scalar<T>(pointer: *mut T, value: T) -> Result<(), Error> {
    check_pointer(pointer)?;
    unsafe {
        pointer.write(value);
    }
    Ok(())
}

/// SAFETY: the caller guarantees this initialized, immutable byte range is
/// valid for this call. Only its length/null preconditions can be checked here.
unsafe fn input_bytes<'a>(
    pointer: *const u8,
    length: u32,
    maximum: usize,
) -> Result<&'a [u8], Error> {
    let length = length as usize;
    if length > maximum {
        return Err(Error::Capacity);
    }
    if length == 0 {
        return Ok(&[]);
    }
    check_pointer(pointer)?;
    Ok(unsafe { std::slice::from_raw_parts(pointer, length) })
}

/// SAFETY: the caller guarantees this initialized byte range is exclusively
/// writable for this call and does not overlap inputs or scalar outputs.
unsafe fn output_bytes<'a>(
    pointer: *mut u8,
    capacity: u32,
    maximum: usize,
) -> Result<&'a mut [u8], Error> {
    let capacity = capacity as usize;
    if capacity > maximum {
        return Err(Error::Capacity);
    }
    if capacity == 0 {
        return Ok(&mut []);
    }
    check_pointer(pointer)?;
    Ok(unsafe { std::slice::from_raw_parts_mut(pointer, capacity) })
}

fn utf8(bytes: &[u8]) -> Result<&str, Error> {
    std::str::from_utf8(bytes).map_err(|_| Error::Invalid)
}

fn phase(value: u32) -> Result<NativeKeyPhase, Error> {
    match value {
        0 => Ok(NativeKeyPhase::Down),
        1 => Ok(NativeKeyPhase::Repeated),
        2 => Ok(NativeKeyPhase::Up),
        3 => Ok(NativeKeyPhase::Press),
        _ => Err(Error::Invalid),
    }
}

#[no_mangle]
pub unsafe extern "C" fn ft_terminal_v1_create(
    config: *const FtTerminalV1Config,
    session: *mut u64,
) -> u32 {
    boundary(None, |registry| {
        unsafe {
            output_scalar(session, 0)?;
        }
        check_pointer(config)?;
        // Read only the version/size prefix before accepting the full layout.
        let version = unsafe { std::ptr::addr_of!((*config).abi_version).read() };
        let size = unsafe { std::ptr::addr_of!((*config).struct_size).read() };
        if version != 1 || size != 80 {
            return Err(Error::Invalid);
        }
        let config = unsafe { config.read() }.validate()?;
        let handle = registry.create(config)?;
        unsafe {
            output_scalar(session, handle)?;
        }
        Ok(())
    })
}

#[no_mangle]
pub extern "C" fn ft_terminal_v1_close(session: u64) -> u32 {
    boundary(Some(session), |registry| registry.session(session)?.close())
}

#[no_mangle]
pub extern "C" fn ft_terminal_v1_release(session: u64) -> u32 {
    boundary(Some(session), |registry| registry.release(session))
}

#[no_mangle]
pub unsafe extern "C" fn ft_terminal_v1_feed(session: u64, bytes: *const u8, length: u32) -> u32 {
    boundary(Some(session), |registry| {
        let bytes = unsafe { input_bytes(bytes, length, MAX_FEED_BYTES)? };
        registry.session(session)?.feed(bytes)
    })
}

#[no_mangle]
pub extern "C" fn ft_terminal_v1_resize(
    session: u64,
    columns: u32,
    rows: u32,
    pixel_width: u32,
    pixel_height: u32,
) -> u32 {
    boundary(Some(session), |registry| {
        registry.resize(session, geometry(columns, rows, pixel_width, pixel_height))
    })
}

#[no_mangle]
pub extern "C" fn ft_terminal_v1_set_font_revision(session: u64, revision: u64) -> u32 {
    boundary(Some(session), |registry| {
        registry.session(session)?.set_font_revision(revision)
    })
}

#[no_mangle]
pub unsafe extern "C" fn ft_terminal_v1_encode_key(
    session: u64,
    usage: u32,
    modifiers: u32,
    phase_value: u32,
    characters: *const u8,
    characters_length: u32,
    output: *mut u8,
    capacity: u32,
    written: *mut u32,
) -> u32 {
    boundary(Some(session), |registry| {
        unsafe {
            output_scalar(written, 0)?;
        }
        let phase = phase(phase_value)?;
        let characters = utf8(unsafe { input_bytes(characters, characters_length, 256)? })?;
        let output = unsafe { output_bytes(output, capacity, MAX_INPUT_BYTES)? };
        let count = registry
            .session(session)?
            .encode_key(usage, modifiers, phase, characters, output)?;
        unsafe {
            output_scalar(written, count as u32)?;
        }
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn ft_terminal_v1_encode_text(
    session: u64,
    text: *const u8,
    length: u32,
    output: *mut u8,
    capacity: u32,
    written: *mut u32,
) -> u32 {
    boundary(Some(session), |registry| {
        unsafe {
            output_scalar(written, 0)?;
        }
        let text = utf8(unsafe { input_bytes(text, length, MAX_TEXT_BYTES)? })?;
        let output = unsafe { output_bytes(output, capacity, MAX_INPUT_BYTES)? };
        let count = registry.session(session)?.encode_text(text, output)?;
        unsafe {
            output_scalar(written, count as u32)?;
        }
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn ft_terminal_v1_prepare_paste(
    session: u64,
    text: *const u8,
    length: u32,
    output: *mut u8,
    capacity: u32,
    written: *mut u32,
    flags: *mut u32,
) -> u32 {
    boundary(Some(session), |registry| {
        unsafe {
            output_scalar(written, 0)?;
            output_scalar(flags, 0)?;
        }
        let text = utf8(unsafe { input_bytes(text, length, MAX_INPUT_BYTES)? })?;
        let output = unsafe { output_bytes(output, capacity, MAX_INPUT_BYTES)? };
        let prepared = registry.session(session)?.prepare_paste(text, output)?;
        unsafe {
            output_scalar(written, prepared.bytes as u32)?;
            output_scalar(flags, u32::from(prepared.bracketed))?;
        }
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn ft_terminal_v1_reply_copy(
    session: u64,
    output: *mut u8,
    capacity: u32,
    length: *mut u32,
    sequence: *mut u64,
) -> u32 {
    boundary(Some(session), |registry| {
        unsafe {
            output_scalar(length, 0)?;
            output_scalar(sequence, 0)?;
        }
        let output = unsafe { output_bytes(output, capacity, MAX_REPLY_BYTES)? };
        if let Some(reply) = registry.session(session)?.copy_reply(output)? {
            unsafe {
                output_scalar(length, reply.bytes as u32)?;
                output_scalar(sequence, reply.sequence)?;
            }
        }
        Ok(())
    })
}

#[no_mangle]
pub extern "C" fn ft_terminal_v1_reply_settle(
    session: u64,
    sequence: u64,
    disposition: u32,
) -> u32 {
    boundary(Some(session), |registry| {
        let disposition = match disposition {
            0 => BoundedReplyDisposition::Accepted,
            1 => BoundedReplyDisposition::Dropped,
            _ => return Err(Error::Invalid),
        };
        registry
            .session(session)?
            .settle_reply(sequence, disposition)
    })
}

#[no_mangle]
pub unsafe extern "C" fn ft_terminal_v1_frame_acquire(
    session: u64,
    viewport_top: i64,
    frame: *mut u64,
    length: *mut u32,
) -> u32 {
    boundary(Some(session), |registry| {
        unsafe {
            output_scalar(frame, 0)?;
            output_scalar(length, 0)?;
        }
        let viewport = if viewport_top == i64::MIN {
            None
        } else {
            Some(viewport_top)
        };
        let (handle, count) = registry.acquire_frame(session, viewport)?;
        unsafe {
            output_scalar(frame, handle)?;
            output_scalar(length, count)?;
        }
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn ft_terminal_v1_frame_copy(
    frame: u64,
    offset: u32,
    output: *mut u8,
    capacity: u32,
    written: *mut u32,
) -> u32 {
    boundary(Some(frame), |registry| {
        unsafe {
            output_scalar(written, 0)?;
        }
        let output = unsafe { output_bytes(output, capacity, MAX_FRAME_BYTES)? };
        let count = registry.copy_frame(frame, offset as usize, output)?;
        unsafe {
            output_scalar(written, count as u32)?;
        }
        Ok(())
    })
}

#[no_mangle]
pub extern "C" fn ft_terminal_v1_frame_release(frame: u64) -> u32 {
    boundary(Some(frame), |registry| registry.release_frame(frame))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> FtTerminalV1Config {
        FtTerminalV1Config {
            abi_version: 1,
            struct_size: 80,
            columns: 12,
            rows: 3,
            pixel_width: 120,
            pixel_height: 60,
            scrollback_rows: 8,
            reserved: 0,
            channel_id: [1; 16],
            transport_id: [2; 16],
            route_revision: 1,
            font_revision: 1,
        }
    }

    #[test]
    fn generation_and_handle_kind_fence_session_and_frame_reuse() {
        let mut registry = Registry::new();
        let first = registry.create(config().validate().unwrap()).unwrap();
        let (frame, length) = registry.acquire_frame(first, None).unwrap();
        assert!(length >= 256);
        assert!(matches!(registry.session(frame), Err(Error::Stale)));
        assert_eq!(registry.release_frame(first), Err(Error::Stale));
        registry.release_frame(frame).unwrap();
        assert_eq!(registry.release_frame(frame), Err(Error::Stale));
        let (next_frame, _) = registry.acquire_frame(first, None).unwrap();
        assert_ne!(frame, next_frame);
        registry.release(first).unwrap();
        assert_eq!(registry.release(first), Err(Error::Stale));
        assert_eq!(
            registry.copy_frame(next_frame, 0, &mut [0; 4]),
            Err(Error::Stale)
        );
        let next = registry.create(config().validate().unwrap()).unwrap();
        assert_ne!(first, next);
        assert!(matches!(registry.session(first), Err(Error::Stale)));
    }

    #[test]
    fn frame_is_owned_across_mutation_and_checked_before_copy() {
        let mut registry = Registry::new();
        let session = registry.create(config().validate().unwrap()).unwrap();
        registry.session(session).unwrap().feed(b"old").unwrap();
        let (frame, length) = registry.acquire_frame(session, None).unwrap();
        assert_eq!(registry.acquire_frame(session, None), Err(Error::Busy));
        let mut before = vec![0; length as usize];
        registry.copy_frame(frame, 0, &mut before).unwrap();
        registry.session(session).unwrap().feed(b"new").unwrap();
        let mut after = vec![0; length as usize];
        registry.copy_frame(frame, 0, &mut after).unwrap();
        assert_eq!(before, after);
        let mut refused = [0xa5; 4];
        assert_eq!(
            registry.copy_frame(frame, length as usize + 1, &mut refused),
            Err(Error::Invalid)
        );
        assert_eq!(refused, [0xa5; 4]);
        registry.release(session).unwrap();
    }

    #[test]
    fn forced_release_frees_an_abandoned_reply_lease_without_replay() {
        let mut registry = Registry::new();
        let session = registry.create(config().validate().unwrap()).unwrap();
        registry.session(session).unwrap().feed(b"\x1b[6n").unwrap();
        let reply = registry
            .session(session)
            .unwrap()
            .copy_reply(&mut [0; 64])
            .unwrap()
            .unwrap();
        assert_eq!(registry.session(session).unwrap().close(), Err(Error::Busy));
        registry.release(session).unwrap();
        let replacement = registry.create(config().validate().unwrap()).unwrap();
        assert_ne!(session, replacement);
        assert_eq!(
            registry
                .session(replacement)
                .unwrap()
                .copy_reply(&mut [0; 64])
                .unwrap(),
            None
        );
        assert_eq!(
            registry
                .session(replacement)
                .unwrap()
                .settle_reply(reply.sequence, BoundedReplyDisposition::Accepted),
            Err(Error::Stale)
        );
    }

    #[test]
    fn aggregate_model_budget_is_reserved_before_creation_and_resize() {
        let mut registry = Registry::new();
        let mut large = config().validate().unwrap();
        large.size.rows = 64;
        large.size.cols = 512;
        large.scrollback_rows = 0;
        let sessions: Vec<_> = (0..4).map(|_| registry.create(large).unwrap()).collect();
        assert_eq!(
            registry.create(config().validate().unwrap()),
            Err(Error::Capacity)
        );
        registry.release(sessions[0]).unwrap();
        let small = registry.create(config().validate().unwrap()).unwrap();
        registry.create(config().validate().unwrap()).unwrap();
        // 60 rows plus 8 history rows at 512 cols fits this session exactly,
        // but not the aggregate budget with the other small session retained.
        assert_eq!(
            registry.resize(small, geometry(512, 60, 0, 0)),
            Err(Error::Capacity)
        );
    }

    #[cfg(panic = "unwind")]
    #[test]
    fn resize_panic_keeps_peak_reservation_until_definitive_release() {
        let mut registry = Registry::new();
        let session = registry.create(config().validate().unwrap()).unwrap();
        let index = registry.index(session, false).unwrap();
        let old_charge = registry.slots[index].charged_cells;
        let size = geometry(24, 6, 240, 120);
        let result = catch_recoverable(
            RecoverablePanicSite::NativeTerminalBridge,
            AssertUnwindSafe(|| {
                registry.resize_with(session, size, |terminal, size| {
                    terminal.resize(size)?;
                    panic!("resize ownership settlement probe");
                })
            }),
        );
        assert!(result.is_err());
        registry.poison(session);
        let new_charge = (2 * size.rows + 8) * size.cols;
        assert_eq!(
            registry.resident_cells_except(None).unwrap(),
            old_charge + new_charge
        );
        assert_eq!(registry.slots[index].charged_cells, old_charge + new_charge);
        registry.release(session).unwrap();
        assert_eq!(registry.resident_cells_except(None).unwrap(), 0);
    }

    #[test]
    fn c_abi_validates_version_utf8_nulls_and_recovers_without_unwinding() {
        let mut session = 0;
        let mut invalid = config();
        invalid.struct_size = 79;
        // SAFETY: every pointer refers to a live uniquely borrowed local with
        // the exact declared range; byte arrays are initialized and disjoint.
        unsafe {
            assert_eq!(
                ft_terminal_v1_create(&invalid, &mut session),
                Error::Invalid as u32
            );
            assert_eq!(session, 0);
            assert_eq!(ft_terminal_v1_create(&config(), &mut session), 0);
            let mut output = [0xa5; 64];
            let mut written = 9;
            assert_eq!(
                ft_terminal_v1_encode_text(
                    session,
                    [0xff].as_ptr(),
                    1,
                    output.as_mut_ptr(),
                    output.len() as u32,
                    &mut written
                ),
                Error::Invalid as u32
            );
            assert_eq!(written, 0);
            assert_eq!(output, [0xa5; 64]);
            assert_eq!(
                ft_terminal_v1_feed(session, std::ptr::null(), 1),
                Error::Invalid as u32
            );
            assert_eq!(ft_terminal_v1_feed(session, std::ptr::null(), 0), 0);
        }
        assert_eq!(
            boundary(Some(session), |_| -> Result<(), Error> {
                panic!("native boundary probe")
            }),
            Error::Panic as u32
        );
        assert_eq!(
            ft_terminal_v1_set_font_revision(session, 2),
            Error::Closed as u32
        );
        assert_eq!(ft_terminal_v1_release(session), 0);
        assert_eq!(ft_terminal_v1_release(session), Error::Stale as u32);
    }
}
