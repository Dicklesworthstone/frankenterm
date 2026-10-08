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
//! # The task role caps every class (ft-yccm0.6)
//!
//! macOS squashes user-interactive and user-initiated requests to the
//! default class's priority (31) in a process without an application task
//! role. Any process not launched as an app has none: one started from a
//! shell, tmux or a test harness. The request still succeeds and
//! [`current_thread_qos`] reads it back, but the scheduler runs the thread at
//! 31, level with every unclassified busy thread on the host. A loaded host
//! then starves the very threads that asked to come first.
//! [`ensure_application_task_role`] gives such a process the role macOS
//! keeps for "may render UI, focus unknown" (`TASK_DEFAULT_APPLICATION`),
//! which lifts the cap, and [`set_current_thread_qos`] calls it before the
//! first request above default, so every caller gets the class it asks for:
//! the GUI, the headless mux server, tests. [`current_thread_base_priority`]
//! reads the priority the class actually gives (see
//! [`ThreadQos::base_priority`]).
//!
//! On every platform other than macOS the functions are no-ops:
//! [`set_current_thread_qos`] returns `Ok(false)`, [`current_thread_qos`]
//! and [`current_thread_base_priority`] return `None`, and
//! [`ensure_application_task_role`] returns
//! [`ApplicationTaskRole::Unsupported`].
//!
//! # UNSAFE-CONTRACT
//!
//! The macOS implementation makes these calls, each in its own `unsafe`
//! block:
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
//! 4. Reading `mach_task_self_` by value: a `mach_port_t` static libSystem
//!    sets to the task's own port before `main` and never changes. It is
//!    declared here with libSystem's type because libc deprecates both its
//!    declaration and its `mach_task_self()` wrapper.
//! 5. `task_policy_get(task, TASK_CATEGORY_POLICY, policy, count,
//!    get_default)` reads the calling task's `task_category_policy`, a
//!    single `integer_t` role, into an `i32` local. `count` is a local set to
//!    1, the policy's size in `integer_t`s, and `get_default` is an `i32`
//!    local (`boolean_t` is 4 bytes). The task is the caller's own port.
//! 6. `task_policy_set(task, TASK_CATEGORY_POLICY, policy, 1)` sets only the
//!    calling task's role, and only to `TASK_DEFAULT_APPLICATION`, a role an
//!    unprivileged task may give itself. `policy` points to an `i32` local
//!    that lives for the call.
//! 7. `pthread_threadid_np(pthread_self(), id)` writes the calling thread's
//!    64-bit id into a `u64` local. `proc_pidinfo(getpid(),
//!    PROC_PIDTHREADID64INFO, id, buffer, size)` fills a zeroed local
//!    `libc::proc_threadinfo` (plain integers and a `c_char` array, valid
//!    when zeroed) with the calling thread's info, and `size` is exactly that
//!    struct's size. The result is used only when the call reports filling
//!    all of it.

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

    /// The base scheduling priority macOS runs a thread of this class at,
    /// once its process has an application task role (the kernel's
    /// `thread_qos_policy_params`). Without one, user-interactive and
    /// user-initiated threads run at the default class's 31.
    pub const fn base_priority(self) -> i32 {
        match self {
            ThreadQos::UserInteractive => 46,
            ThreadQos::UserInitiated => 37,
            ThreadQos::Default => 31,
            ThreadQos::Utility => 20,
            ThreadQos::Background => 4,
        }
    }
}

/// What [`ensure_application_task_role`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplicationTaskRole {
    /// The process already had a role, the raw `task_role_t` (for example
    /// one macOS gives a process launched as an app); left alone.
    AlreadyAssigned(i32),
    /// The process had none; it now has `TASK_DEFAULT_APPLICATION`.
    Assigned,
    /// Not macOS: there are no task roles.
    Unsupported,
}

/// Set by [`keep_launched_task_role`].
static KEEP_LAUNCHED_TASK_ROLE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
/// Whether [`set_current_thread_qos`] has given the process its role.
static TASK_ROLE_ENSURED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Sets the calling thread's QoS class.
///
/// Returns `Ok(true)` once macOS has applied it and `Ok(false)` on platforms
/// without QoS classes, where this is a no-op. A class above default only
/// takes effect in a process with an application task role, so the first
/// such request gives the process one ([`ensure_application_task_role`]),
/// unless [`keep_launched_task_role`] or `FT_KEEP_TASK_ROLE=1` in the
/// environment said not to (an A/B of ft-yccm0.6 in one build; see the
/// module docs).
pub fn set_current_thread_qos(qos: ThreadQos) -> std::io::Result<bool> {
    if qos.base_priority() > ThreadQos::Default.base_priority() {
        ensure_task_role_once();
    }
    imp::set_current_thread_qos(qos)
}

fn ensure_task_role_once() {
    use std::sync::atomic::Ordering;

    static KEEP_FROM_ENV: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if KEEP_LAUNCHED_TASK_ROLE.load(Ordering::Relaxed)
        || TASK_ROLE_ENSURED.load(Ordering::Acquire)
        || *KEEP_FROM_ENV
            .get_or_init(|| std::env::var_os("FT_KEEP_TASK_ROLE").is_some_and(|value| value == "1"))
    {
        return;
    }
    match ensure_application_task_role() {
        Ok(_) => TASK_ROLE_ENSURED.store(true, Ordering::Release),
        Err(err) => log::warn!(
            "no application task role ({err}): QoS classes above default run at the default \
             priority"
        ),
    }
}

/// From now on, [`set_current_thread_qos`] leaves the process's task role as
/// it was launched, so classes above default may run at the default
/// priority. For an A/B of ft-yccm0.6 in one build.
pub fn keep_launched_task_role() {
    KEEP_LAUNCHED_TASK_ROLE.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// The calling thread's QoS class, as requested. `None` on platforms without
/// QoS classes, or when the class is unspecified or unknown to this build.
pub fn current_thread_qos() -> Option<ThreadQos> {
    imp::current_thread_qos()
}

/// The calling thread's base scheduling priority, which is what its QoS
/// class actually gives it (compare [`ThreadQos::base_priority`]). `None` off
/// macOS, or when the kernel does not report it.
pub fn current_thread_base_priority() -> Option<i32> {
    imp::current_thread_base_priority()
}

/// Gives the process an application task role when it has none, so the QoS
/// classes its threads ask for take effect (ft-yccm0.6). A role macOS
/// already gave the process (as it does an app it launched) is left alone;
/// none at all becomes `TASK_DEFAULT_APPLICATION`, "may render UI, focus
/// unknown". Call it early in a process that shows a window, before it
/// starts threads that ask for user-initiated or user-interactive QoS.
pub fn ensure_application_task_role() -> std::io::Result<ApplicationTaskRole> {
    imp::ensure_application_task_role()
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

    // In libSystem. libc does not declare the task_policy calls
    // (mach/task_policy.h), and deprecates its own mach_task_self_.
    extern "C" {
        static mach_task_self_: libc::mach_port_t;
        fn task_policy_get(
            task: libc::mach_port_t,
            flavor: libc::c_uint,
            policy_info: *mut libc::c_int,
            policy_info_count: *mut libc::c_uint,
            get_default: *mut libc::c_int,
        ) -> libc::kern_return_t;
        fn task_policy_set(
            task: libc::mach_port_t,
            flavor: libc::c_uint,
            policy_info: *mut libc::c_int,
            policy_info_count: libc::c_uint,
        ) -> libc::kern_return_t;
    }

    const TASK_CATEGORY_POLICY: libc::c_uint = 1;
    const TASK_UNSPECIFIED: libc::c_int = 0;
    pub(super) const TASK_DEFAULT_APPLICATION: libc::c_int = 7;
    // sys/proc_info.h: the thread named by its 64-bit id.
    const PROC_PIDTHREADID64INFO: libc::c_int = 15;

    fn task_self() -> libc::mach_port_t {
        // SAFETY: UNSAFE-CONTRACT 4. Read by value; set before main and
        // never changed.
        unsafe { mach_task_self_ }
    }

    fn kern_error(call: &str, rc: libc::kern_return_t) -> std::io::Error {
        std::io::Error::other(format!("{call} failed: kern_return_t {rc}"))
    }

    pub(super) fn current_task_role() -> std::io::Result<libc::c_int> {
        let mut role: libc::c_int = TASK_UNSPECIFIED;
        let mut count: libc::c_uint = 1;
        let mut get_default: libc::c_int = 0;
        // SAFETY: UNSAFE-CONTRACT 5. The caller's own task; every pointer is
        // to a live local of the size the call writes.
        let rc = unsafe {
            task_policy_get(
                task_self(),
                TASK_CATEGORY_POLICY,
                &mut role,
                &mut count,
                &mut get_default,
            )
        };
        if rc == 0 {
            Ok(role)
        } else {
            Err(kern_error("task_policy_get", rc))
        }
    }

    pub(super) fn ensure_application_task_role() -> std::io::Result<super::ApplicationTaskRole> {
        let role = current_task_role()?;
        if role != TASK_UNSPECIFIED {
            return Ok(super::ApplicationTaskRole::AlreadyAssigned(role));
        }
        let mut role = TASK_DEFAULT_APPLICATION;
        // SAFETY: UNSAFE-CONTRACT 6. Sets only the caller's own role, to one
        // an unprivileged task may give itself.
        let rc = unsafe { task_policy_set(task_self(), TASK_CATEGORY_POLICY, &mut role, 1) };
        if rc == 0 {
            Ok(super::ApplicationTaskRole::Assigned)
        } else {
            Err(kern_error("task_policy_set", rc))
        }
    }

    pub(super) fn current_thread_base_priority() -> Option<i32> {
        let mut id: u64 = 0;
        // SAFETY: UNSAFE-CONTRACT 7. The calling thread; `id` is a live local.
        let rc = unsafe { libc::pthread_threadid_np(libc::pthread_self(), &mut id) };
        if rc != 0 {
            return None;
        }
        // SAFETY: UNSAFE-CONTRACT 7. Plain integers and a c_char array: a
        // zeroed proc_threadinfo is a valid value.
        let mut info: libc::proc_threadinfo = unsafe { std::mem::zeroed() };
        let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_threadinfo>()).ok()?;
        // SAFETY: UNSAFE-CONTRACT 7. `info` is a live local of exactly `size`
        // bytes.
        let filled = unsafe {
            libc::proc_pidinfo(
                libc::getpid(),
                PROC_PIDTHREADID64INFO,
                id,
                std::ptr::addr_of_mut!(info).cast::<libc::c_void>(),
                size,
            )
        };
        (filled == size).then_some(info.pth_priority)
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

    pub(super) fn current_thread_base_priority() -> Option<i32> {
        None
    }

    pub(super) fn ensure_application_task_role() -> std::io::Result<super::ApplicationTaskRole> {
        Ok(super::ApplicationTaskRole::Unsupported)
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

    /// ft-yccm0.6: a test binary, like a GUI a harness starts, is launched
    /// from a shell and has no application task role, so macOS runs
    /// user-initiated and user-interactive threads at the default priority.
    /// Once the process has a role, every class runs at its own base
    /// priority; a second call leaves the role alone.
    #[cfg(target_os = "macos")]
    #[test]
    fn with_an_application_role_each_class_runs_at_its_own_priority() {
        let first = ensure_application_task_role().expect("ensure the role");
        assert!(
            matches!(
                first,
                ApplicationTaskRole::Assigned | ApplicationTaskRole::AlreadyAssigned(_)
            ),
            "{first:?}"
        );
        assert_eq!(
            ensure_application_task_role().expect("ensure the role again"),
            ApplicationTaskRole::AlreadyAssigned(imp::current_task_role().expect("read the role"))
        );
        assert_ne!(imp::current_task_role().expect("read the role"), 0);
        for qos in ThreadQos::ALL {
            assert_eq!(
                base_priority_after_asking_for(qos),
                Some(qos.base_priority()),
                "{qos:?}"
            );
        }
    }

    /// The base priority a fresh thread runs at once it asked for `qos`,
    /// read before anything waits on it: a thread blocked joining it would
    /// lend it its own priority (a turnstile push), so a utility or
    /// background thread joined from a default thread reads 31.
    #[cfg(target_os = "macos")]
    fn base_priority_after_asking_for(qos: ThreadQos) -> Option<i32> {
        let (report, observed) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            set_current_thread_qos(qos).expect("set QoS");
            report
                .send(current_thread_base_priority())
                .expect("report the priority");
        });
        let observed = observed.recv().expect("the QoS thread reports");
        thread.join().expect("QoS thread");
        observed
    }

    /// ft-yccm0.6: in a process nothing gave a role (this test binary, rerun
    /// on this one test, since the role is process-wide), a thread's first
    /// request above default gives the process its role, so the class takes
    /// effect.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_first_request_above_default_takes_effect_in_a_fresh_process() {
        const NAME: &str =
            "thread_qos::tests::a_first_request_above_default_takes_effect_in_a_fresh_process";
        if std::env::var_os("PROCINFO_QOS_FRESH_PROCESS").is_some() {
            assert_eq!(imp::current_task_role().expect("read the role"), 0);
            assert_eq!(
                base_priority_after_asking_for(ThreadQos::UserInitiated),
                Some(ThreadQos::UserInitiated.base_priority())
            );
            assert_eq!(
                imp::current_task_role().expect("read the role"),
                imp::TASK_DEFAULT_APPLICATION
            );
            return;
        }
        let child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", NAME, "--test-threads", "1"])
            .env("PROCINFO_QOS_FRESH_PROCESS", "1")
            .env_remove("FT_KEEP_TASK_ROLE")
            .output()
            .expect("run the test in a fresh process");
        let stdout = String::from_utf8_lossy(&child.stdout);
        assert!(
            child.status.success() && stdout.contains("1 passed"),
            "fresh process: {}\n{stdout}\n{}",
            child.status,
            String::from_utf8_lossy(&child.stderr)
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn the_helper_is_a_no_op_off_macos() {
        let observed = std::thread::spawn(|| {
            let applied = set_current_thread_qos(ThreadQos::UserInitiated).expect("no-op");
            (
                applied,
                current_thread_qos(),
                current_thread_base_priority(),
            )
        })
        .join()
        .expect("QoS thread");
        assert_eq!(observed, (false, None, None));
        assert_eq!(
            ensure_application_task_role().expect("no-op"),
            ApplicationTaskRole::Unsupported
        );
    }
}
