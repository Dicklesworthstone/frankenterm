//! `os_signpost` intervals for the FrankenTerm pipeline (ft-yccm0.1.5),
//! shown by Instruments under Points of Interest.
//!
//! Intervals are off unless `FT_SIGNPOST=1` is set at startup (or
//! [`set_signposts_enabled`] turns them on). With them off, beginning an
//! interval is one relaxed atomic load. On platforms other than macOS every
//! function is a no-op and no interval is ever requested.
//!
//! Names are a fixed set ([`SignpostName`]) because, as the C
//! `os_signpost_interval_begin` macro does, each name and the (empty) format
//! string must live in the image's `__TEXT,__oslogstring` section, where the
//! logging system resolves them relative to the image.
//!
//! # UNSAFE-CONTRACT
//!
//! The macOS implementation makes four libsystem_trace calls, each in its own
//! `unsafe` block, and declares one `unsafe impl`:
//!
//! 1. `os_log_create(subsystem, category)` takes two NUL-terminated C string
//!    literals that live for the whole process and returns a log handle (or
//!    NULL, which disables signposts). The handle is never released: the
//!    process keeps one for its lifetime, as the C API intends.
//! 2. `os_signpost_enabled(log)` and `os_signpost_id_generate(log)` read the
//!    non-NULL handle from (1). A generated id of `OS_SIGNPOST_ID_NULL` or
//!    `OS_SIGNPOST_ID_INVALID` is never emitted, matching the C macro.
//! 3. `_os_signpost_emit_with_name_impl(dso, log, type, id, name, format,
//!    buf, size)` is exactly what the C macro calls: `dso` is this image's
//!    `__dso_handle`; `log` is the handle from (1); `type` is
//!    `OS_SIGNPOST_INTERVAL_BEGIN` (1) or `_END` (2); `id` came from (2);
//!    `name` and `format` point to NUL-terminated statics in
//!    `__TEXT,__oslogstring,cstring_literals`; `buf` points to a 2-byte local
//!    holding the os_log buffer header for a format with no arguments
//!    (summary flags 0, argument count 0), and `size` is 2. Nothing is
//!    retained after the call.
//! 4. `Log` (the raw `os_log_t`) is `Send + Sync`: os_log handles are
//!    documented as safe to use from any thread.

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};

/// An interval the pipeline can mark. The string is what Instruments shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SignpostName {
    /// A PTY read on the gather thread.
    PtyRead,
    /// Applying one parsed batch of output to the terminal.
    ParseBatch,
    /// Holding a pane's terminal mutex, by holder kind.
    TerminalLockParser,
    TerminalLockPaint,
    TerminalLockMouse,
    TerminalLockResize,
    TerminalLockSelection,
    TerminalLockOther,
    RenderSnapshot,
    GeometryRebuild,
    GpuEncode,
    Commit,
    PresentCallback,
}

impl SignpostName {
    pub const ALL: [SignpostName; 13] = [
        SignpostName::PtyRead,
        SignpostName::ParseBatch,
        SignpostName::TerminalLockParser,
        SignpostName::TerminalLockPaint,
        SignpostName::TerminalLockMouse,
        SignpostName::TerminalLockResize,
        SignpostName::TerminalLockSelection,
        SignpostName::TerminalLockOther,
        SignpostName::RenderSnapshot,
        SignpostName::GeometryRebuild,
        SignpostName::GpuEncode,
        SignpostName::Commit,
        SignpostName::PresentCallback,
    ];

    /// The NUL-terminated interval name.
    pub fn as_bytes_with_nul(self) -> &'static [u8] {
        names::bytes(self)
    }

    /// The interval name without its NUL terminator.
    pub fn as_str(self) -> &'static str {
        let bytes = self.as_bytes_with_nul();
        std::str::from_utf8(&bytes[..bytes.len() - 1]).expect("signpost names are ASCII")
    }
}

/// Names and the empty format string, placed where `os_signpost` expects
/// them (see UNSAFE-CONTRACT 3).
mod names {
    use super::SignpostName;

    macro_rules! oslog_strings {
        ($($static:ident = $text:literal;)*) => {
            $(
                #[cfg_attr(
                    target_os = "macos",
                    link_section = "__TEXT,__oslogstring,cstring_literals"
                )]
                pub(super) static $static: [u8; $text.len()] = *$text;
            )*
        };
    }

    /// The empty format string every interval is emitted with (macOS only).
    #[cfg_attr(
        target_os = "macos",
        link_section = "__TEXT,__oslogstring,cstring_literals"
    )]
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(super) static EMPTY_FORMAT: [u8; 1] = [0];

    oslog_strings! {
        PTY_READ = b"pty read\0";
        PARSE_BATCH = b"parse batch\0";
        LOCK_PARSER = b"terminal lock: parser\0";
        LOCK_PAINT = b"terminal lock: paint\0";
        LOCK_MOUSE = b"terminal lock: mouse\0";
        LOCK_RESIZE = b"terminal lock: resize\0";
        LOCK_SELECTION = b"terminal lock: selection\0";
        LOCK_OTHER = b"terminal lock: other\0";
        RENDER_SNAPSHOT = b"render snapshot\0";
        GEOMETRY_REBUILD = b"geometry rebuild\0";
        GPU_ENCODE = b"gpu encode\0";
        COMMIT = b"commit\0";
        PRESENT_CALLBACK = b"present callback\0";
    }

    pub(super) fn bytes(name: SignpostName) -> &'static [u8] {
        match name {
            SignpostName::PtyRead => &PTY_READ,
            SignpostName::ParseBatch => &PARSE_BATCH,
            SignpostName::TerminalLockParser => &LOCK_PARSER,
            SignpostName::TerminalLockPaint => &LOCK_PAINT,
            SignpostName::TerminalLockMouse => &LOCK_MOUSE,
            SignpostName::TerminalLockResize => &LOCK_RESIZE,
            SignpostName::TerminalLockSelection => &LOCK_SELECTION,
            SignpostName::TerminalLockOther => &LOCK_OTHER,
            SignpostName::RenderSnapshot => &RENDER_SNAPSHOT,
            SignpostName::GeometryRebuild => &GEOMETRY_REBUILD,
            SignpostName::GpuEncode => &GPU_ENCODE,
            SignpostName::Commit => &COMMIT,
            SignpostName::PresentCallback => &PRESENT_CALLBACK,
        }
    }
}

const STATE_UNREAD: u8 = 0;
const STATE_OFF: u8 = 1;
const STATE_ON: u8 = 2;

static STATE: AtomicU8 = AtomicU8::new(STATE_UNREAD);
static REQUESTED: AtomicU64 = AtomicU64::new(0);

/// Whether intervals are emitted: `FT_SIGNPOST=1` at first use, unless
/// [`set_signposts_enabled`] decided otherwise.
pub fn signposts_enabled() -> bool {
    match STATE.load(Ordering::Relaxed) {
        STATE_ON => true,
        STATE_OFF => false,
        _ => {
            let on = std::env::var_os("FT_SIGNPOST").is_some_and(|value| value == "1");
            // A concurrent explicit setting wins over the environment.
            let _ = STATE.compare_exchange(
                STATE_UNREAD,
                if on { STATE_ON } else { STATE_OFF },
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
            STATE.load(Ordering::Relaxed) == STATE_ON
        }
    }
}

/// Turn intervals on or off for the whole process, overriding `FT_SIGNPOST`.
pub fn set_signposts_enabled(enabled: bool) {
    STATE.store(
        if enabled { STATE_ON } else { STATE_OFF },
        Ordering::Relaxed,
    );
}

/// Intervals begun while signposts were enabled on macOS. Whether the OS
/// records each one depends on `os_signpost_enabled` (for Points of Interest,
/// typically only while Instruments is recording).
pub fn signpost_intervals_requested() -> u64 {
    REQUESTED.load(Ordering::Relaxed)
}

/// An open interval; it ends when dropped. Inert when signposts are off.
#[derive(Debug)]
#[must_use = "an interval ends when this guard is dropped"]
pub struct SignpostInterval {
    // Held only so that dropping it ends the interval.
    _open: Option<imp::Interval>,
}

impl SignpostInterval {
    /// An interval that marks nothing.
    pub const fn inert() -> Self {
        Self { _open: None }
    }
}

/// Begin an interval named `name`.
pub fn signpost_interval(name: SignpostName) -> SignpostInterval {
    if !signposts_enabled() {
        return SignpostInterval::inert();
    }
    SignpostInterval {
        _open: imp::begin(name, &REQUESTED),
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{names, SignpostName};
    use std::ffi::{c_char, c_void};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::OnceLock;

    const INTERVAL_BEGIN: u8 = 0x01;
    const INTERVAL_END: u8 = 0x02;
    const ID_NULL: u64 = 0;
    const ID_INVALID: u64 = !0;

    extern "C" {
        /// The Mach-O header of the image containing this code.
        static __dso_handle: u8;
        fn os_log_create(subsystem: *const c_char, category: *const c_char) -> *mut c_void;
        fn os_signpost_enabled(log: *mut c_void) -> bool;
        fn os_signpost_id_generate(log: *mut c_void) -> u64;
        fn _os_signpost_emit_with_name_impl(
            dso: *mut c_void,
            log: *mut c_void,
            kind: u8,
            id: u64,
            name: *const c_char,
            format: *const c_char,
            buf: *mut u8,
            size: u32,
        );
    }

    #[derive(Debug)]
    struct Log(*mut c_void);

    // SAFETY: UNSAFE-CONTRACT 4. An os_log_t may be used from any thread.
    unsafe impl Send for Log {}
    // SAFETY: UNSAFE-CONTRACT 4.
    unsafe impl Sync for Log {}

    fn log() -> Option<&'static Log> {
        static LOG: OnceLock<Option<Log>> = OnceLock::new();
        LOG.get_or_init(|| {
            // SAFETY: UNSAFE-CONTRACT 1. Both arguments are NUL-terminated
            // literals with static lifetime.
            let log = unsafe {
                os_log_create(
                    c"com.frankenterm.pipeline".as_ptr(),
                    c"PointsOfInterest".as_ptr(),
                )
            };
            (!log.is_null()).then_some(Log(log))
        })
        .as_ref()
    }

    #[derive(Debug)]
    pub(super) struct Interval {
        log: &'static Log,
        id: u64,
        name: SignpostName,
    }

    fn emit(log: &Log, kind: u8, id: u64, name: SignpostName) {
        let mut buf = [0u8; 2];
        // SAFETY: UNSAFE-CONTRACT 3. `log` is non-NULL from `log()`; `id` was
        // generated for it and is neither NULL nor INVALID; the name and the
        // empty format are NUL-terminated statics in __oslogstring; `buf` is a
        // live 2-byte header for zero arguments.
        unsafe {
            _os_signpost_emit_with_name_impl(
                std::ptr::addr_of!(__dso_handle).cast_mut().cast(),
                log.0,
                kind,
                id,
                name.as_bytes_with_nul().as_ptr().cast(),
                names::EMPTY_FORMAT.as_ptr().cast(),
                buf.as_mut_ptr(),
                2,
            );
        }
    }

    pub(super) fn begin(name: SignpostName, requested: &AtomicU64) -> Option<Interval> {
        requested.fetch_add(1, Ordering::Relaxed);
        let log = log()?;
        // SAFETY: UNSAFE-CONTRACT 2. `log.0` is the non-NULL handle.
        if !unsafe { os_signpost_enabled(log.0) } {
            return None;
        }
        // SAFETY: UNSAFE-CONTRACT 2.
        let id = unsafe { os_signpost_id_generate(log.0) };
        if id == ID_NULL || id == ID_INVALID {
            return None;
        }
        emit(log, INTERVAL_BEGIN, id, name);
        Some(Interval { log, id, name })
    }

    impl Drop for Interval {
        fn drop(&mut self) {
            emit(self.log, INTERVAL_END, self.id, self.name);
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::SignpostName;
    use std::sync::atomic::AtomicU64;

    #[derive(Debug)]
    pub(super) enum Interval {}

    pub(super) fn begin(_name: SignpostName, _requested: &AtomicU64) -> Option<Interval> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_nul_terminated_ascii_and_distinct() {
        let mut seen = std::collections::HashSet::new();
        for name in SignpostName::ALL {
            let bytes = name.as_bytes_with_nul();
            assert_eq!(bytes.last(), Some(&0), "{name:?}");
            assert!(!bytes[..bytes.len() - 1].contains(&0), "{name:?}");
            assert!(name.as_str().is_ascii());
            assert!(seen.insert(name.as_str()), "duplicate name {name:?}");
        }
        assert_eq!(SignpostName::ParseBatch.as_str(), "parse batch");
        assert_eq!(names::EMPTY_FORMAT, [0]);
    }

    #[test]
    fn intervals_are_requested_only_when_enabled_and_only_on_macos() {
        set_signposts_enabled(false);
        let before = signpost_intervals_requested();
        drop(signpost_interval(SignpostName::ParseBatch));
        assert_eq!(signpost_intervals_requested(), before);

        set_signposts_enabled(true);
        assert!(signposts_enabled());
        let interval = signpost_interval(SignpostName::ParseBatch);
        let after = signpost_intervals_requested();
        drop(interval);
        if cfg!(target_os = "macos") {
            assert!(after > before);
        } else {
            assert_eq!(after, before);
        }
        set_signposts_enabled(false);
    }
}
