//! Thread quality-of-service (QoS) classes: the macOS scheduling hint that
//! decides whether Apple silicon runs a thread on performance or efficiency
//! cores.
//!
//! A thread left at the default class can be placed on efficiency cores while
//! a flood of terminal output is being ingested, which roughly halves
//! throughput (ft-yccm0.3.1.3). Each FrankenTerm thread asks for the class
//! that matches its work:
//!
//! | Thread | Class |
//! |---|---|
//! | PTY gather and parse (`mux-read-pane-N`, `mux-parse-pane-N`) | [`ThreadQos::UserInitiated`] |
//! | Metal render thread while the window is focused | [`ThreadQos::UserInteractive`] |
//! | Durable scrollback writer | [`ThreadQos::Utility`] |
//!
//! On every platform other than macOS the functions are no-ops:
//! [`set_current_thread_qos`] returns `Ok(false)` and
//! [`current_thread_qos`] returns `None`.
//!
//! # UNSAFE-CONTRACT
//!
//! The macOS implementation makes three libpthread calls, each in its own
//! `unsafe` block:
//!
//! 1. `pthread_set_qos_class_self_np(class, 0)` changes only the calling
//!    thread. `class` is always one of the five named classes, never
//!    `QOS_CLASS_UNSPECIFIED`, which the call rejects. The relative priority
//!    is 0, inside the documented range `QOS_MIN_RELATIVE_PRIORITY..=0`. No
//!    pointers are involved; a nonzero return is an errno value, reported as
//!    an `io::Error`.
//! 2. `pthread_self()` has no preconditions and returns the calling thread's
//!    handle, which stays valid while that thread runs.
//! 3. `pthread_get_qos_class_np(thread, class, priority)` reads the calling
//!    thread's class through that handle. Both out-pointers point to locals
//!    that live for the whole call. The class is written into a `u32` local, the exact size and
//!    representation of the C `qos_class_t` (an `unsigned int` enum), rather
//!    than into libc's Rust `qos_class_t` enum: a value a future OS might add
//!    therefore never becomes an invalid Rust enum discriminant. It is mapped
//!    through [`ThreadQos::from_raw`], and an unknown value reads as `None`.

/// A thread QoS class. The raw values are the C `qos_class_t` constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThreadQos {
    /// Work the user is interacting with right now: the Metal render thread
    /// while its window is focused (ft-yccm0.4.1.2). Demote it when the
    /// window loses focus.
    UserInteractive,
    /// Work the user started and is waiting for: PTY gather and parse.
    UserInitiated,
    /// The default class of a thread nobody classified.
    Default,
    /// Long-running work that need not finish promptly: the durable
    /// scrollback writer (ft-yccm0.2.1.1).
    Utility,
    /// Work the user never waits for.
    Background,
}

impl ThreadQos {
    /// Every class, from most to least urgent.
    pub const ALL: [ThreadQos; 5] = [
        ThreadQos::UserInteractive,
        ThreadQos::UserInitiated,
        ThreadQos::Default,
        ThreadQos::Utility,
        ThreadQos::Background,
    ];

    /// The C `qos_class_t` value (`QOS_CLASS_*` in `sys/qos.h`).
    pub const fn raw(self) -> u32 {
        match self {
            ThreadQos::UserInteractive => 0x21,
            ThreadQos::UserInitiated => 0x19,
            ThreadQos::Default => 0x15,
            ThreadQos::Utility => 0x11,
            ThreadQos::Background => 0x09,
        }
    }

    /// Maps a C `qos_class_t` value back to a class. `QOS_CLASS_UNSPECIFIED`
    /// (0) and values this build does not know map to `None`.
    pub fn from_raw(raw: u32) -> Option<Self> {
        Self::ALL.into_iter().find(|qos| qos.raw() == raw)
    }
}

/// Sets the calling thread's QoS class.
///
/// Returns `Ok(true)` once macOS has applied it and `Ok(false)` on platforms
/// without QoS classes, where this is a no-op.
pub fn set_current_thread_qos(qos: ThreadQos) -> std::io::Result<bool> {
    imp::set_current_thread_qos(qos)
}

/// The calling thread's QoS class. `None` on platforms without QoS classes,
/// or when the class is unspecified or unknown to this build.
pub fn current_thread_qos() -> Option<ThreadQos> {
    imp::current_thread_qos()
}

#[cfg(target_os = "macos")]
mod imp {
    use super::ThreadQos;

    pub(super) fn class(qos: ThreadQos) -> libc::qos_class_t {
        match qos {
            ThreadQos::UserInteractive => libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE,
            ThreadQos::UserInitiated => libc::qos_class_t::QOS_CLASS_USER_INITIATED,
            ThreadQos::Default => libc::qos_class_t::QOS_CLASS_DEFAULT,
            ThreadQos::Utility => libc::qos_class_t::QOS_CLASS_UTILITY,
            ThreadQos::Background => libc::qos_class_t::QOS_CLASS_BACKGROUND,
        }
    }

    pub(super) fn set_current_thread_qos(qos: ThreadQos) -> std::io::Result<bool> {
        // SAFETY: UNSAFE-CONTRACT 1. Affects only the calling thread; `class`
        // is a named class and relative priority 0 is in range.
        let rc = unsafe { libc::pthread_set_qos_class_self_np(class(qos), 0) };
        if rc == 0 {
            Ok(true)
        } else {
            Err(std::io::Error::from_raw_os_error(rc))
        }
    }

    pub(super) fn current_thread_qos() -> Option<ThreadQos> {
        // SAFETY: UNSAFE-CONTRACT 2. `pthread_self` has no preconditions and
        // always returns the calling thread's handle.
        let thread = unsafe { libc::pthread_self() };
        let mut raw: u32 = 0;
        let mut priority: libc::c_int = 0;
        // SAFETY: UNSAFE-CONTRACT 3. `thread` is the calling thread; both
        // out-pointers point to live locals, and the class lands in a `u32`,
        // never in a Rust enum.
        let rc = unsafe {
            libc::pthread_get_qos_class_np(
                thread,
                std::ptr::addr_of_mut!(raw).cast::<libc::qos_class_t>(),
                &mut priority,
            )
        };
        if rc == 0 {
            ThreadQos::from_raw(raw)
        } else {
            None
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::ThreadQos;

    pub(super) fn set_current_thread_qos(_qos: ThreadQos) -> std::io::Result<bool> {
        Ok(false)
    }

    pub(super) fn current_thread_qos() -> Option<ThreadQos> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_values_match_the_c_constants_and_round_trip() {
        assert_eq!(ThreadQos::UserInteractive.raw(), 0x21);
        assert_eq!(ThreadQos::UserInitiated.raw(), 0x19);
        assert_eq!(ThreadQos::Default.raw(), 0x15);
        assert_eq!(ThreadQos::Utility.raw(), 0x11);
        assert_eq!(ThreadQos::Background.raw(), 0x09);
        for qos in ThreadQos::ALL {
            assert_eq!(ThreadQos::from_raw(qos.raw()), Some(qos));
        }
        assert_eq!(ThreadQos::from_raw(0), None, "QOS_CLASS_UNSPECIFIED");
        assert_eq!(ThreadQos::from_raw(0x22), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn raw_values_match_libc() {
        for qos in ThreadQos::ALL {
            assert_eq!(imp::class(qos) as u32, qos.raw(), "{qos:?}");
        }
    }

    /// Each class is set on a fresh thread and read back from the kernel.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_thread_reads_back_the_class_it_set() {
        for qos in ThreadQos::ALL {
            let observed = std::thread::spawn(move || {
                let applied = set_current_thread_qos(qos).expect("set QoS");
                (applied, current_thread_qos())
            })
            .join()
            .expect("QoS thread");
            assert_eq!(observed, (true, Some(qos)), "{qos:?}");
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn the_helper_is_a_no_op_off_macos() {
        let observed = std::thread::spawn(|| {
            let applied = set_current_thread_qos(ThreadQos::UserInitiated).expect("no-op");
            (applied, current_thread_qos())
        })
        .join()
        .expect("QoS thread");
        assert_eq!(observed, (false, None));
    }
}
