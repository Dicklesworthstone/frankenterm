use crate::domain::DomainId;
use crate::pane::{
    CachePolicy, CloseReason, ForEachPaneLogicalLine, LogicalLine, Pane, PaneId, Pattern,
    SearchResult, WithPaneLines,
};
use crate::renderable::*;
use crate::tmux::{TmuxDomain, TmuxDomainState};
use crate::{Domain, Mux, MuxNotification};
use anyhow::Error;
use async_trait::async_trait;
use config::keyassignment::ScrollbackEraseMode;
use config::{configuration, ExitBehavior, ExitBehaviorMessaging};
use fancy_regex::Regex;
use frankenterm_dynamic::Value;
use frankenterm_term::color::ColorPalette;
use frankenterm_term::{
    Alert, AlertHandler, Clipboard, DownloadHandler, KeyCode, KeyModifiers, MouseEvent, Progress,
    SemanticZone, StableRowIndex, Terminal, TerminalConfiguration, TerminalSize,
};
use parking_lot::{MappedMutexGuard, Mutex, MutexGuard};
use portable_pty::{Child, ChildKiller, ExitStatus, MasterPty, PtySize};
use procinfo::LocalProcessInfo;
use rangeset::RangeSet;
use smol::channel::{bounded, Receiver, TryRecvError};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::convert::TryInto;
use std::io::{Result as IoResult, Write};
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};
use termwiz::escape::csi::{Sgr, CSI};
use termwiz::escape::{Action, DeviceControlMode};
use termwiz::input::KeyboardEncoding;
use termwiz::surface::{Line, SequenceNo};
use url::Url;

const PROC_INFO_CACHE_TTL: Duration = Duration::from_millis(300);

#[derive(Debug)]
enum ProcessState {
    Running {
        child_waiter: Receiver<IoResult<ExitStatus>>,
        pid: Option<u32>,
        signaller: Box<dyn ChildKiller + Sync>,
        // Whether we've explicitly killed the child
        killed: bool,
    },
    DeadPendingClose {
        killed: bool,
    },
    Dead,
}

struct CachedProcInfo {
    root: LocalProcessInfo,
    updated: Instant,
    foreground: LocalProcessInfo,
}

/// This is a bit horrible; it can take 700us to tcgetpgrp, so if we have
/// 10 tabs open and run the mouse over them, hovering them each in turn,
/// we can spend 7ms per evaluation of the tab bar state on fetching those
/// pids alone, which can easily lead to stuttering when moving the mouse
/// over all of the tabs.
///
/// This implements a cache holding that fg process and the often queried
/// cwd and process path that allows for stale reads to proceed quickly
/// while the writes can happen in a background thread.
#[cfg(unix)]
#[derive(Clone)]
struct CachedLeaderInfo {
    updated: Instant,
    fd: std::os::fd::RawFd,
    pid: u32,
    path: Option<std::path::PathBuf>,
    current_working_dir: Option<std::path::PathBuf>,
    updating: bool,
}

#[cfg(unix)]
impl CachedLeaderInfo {
    fn new(fd: Option<std::os::fd::RawFd>) -> Self {
        let mut me = Self {
            updated: Instant::now(),
            fd: fd.unwrap_or(-1),
            pid: 0,
            path: None,
            current_working_dir: None,
            updating: false,
        };
        me.update();
        me
    }

    fn can_update(&self) -> bool {
        self.fd != -1 && !self.updating
    }

    fn update(&mut self) {
        self.pid = unsafe { libc::tcgetpgrp(self.fd) } as u32;
        if self.pid > 0 {
            self.path = LocalProcessInfo::executable_path(self.pid);
            self.current_working_dir = LocalProcessInfo::current_working_dir(self.pid);
        } else {
            self.path.take();
            self.current_working_dir.take();
        }
        self.updated = Instant::now();
        self.updating = false;
    }

    fn expired(&self) -> bool {
        self.updated.elapsed() > PROC_INFO_CACHE_TTL
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum LocalPaneConnectionState {
    Connecting,
    Connected,
}

#[derive(Clone, Copy)]
struct PendingResize {
    seq: u64,
    size: TerminalSize,
    pty_size: PtySize,
    enqueued_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ResizeEnqueueOutcome {
    seq: u64,
    replaced_seq: Option<u64>,
    spawn_worker: bool,
    queue_depth_hint: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ResizeCancellationToken {
    seq: u64,
}

impl ResizeCancellationToken {
    fn new(seq: u64) -> Self {
        Self { seq }
    }
}

#[derive(Default)]
struct ResizeQueueState {
    pending: Option<PendingResize>,
    next_seq: u64,
    worker_running: bool,
}

impl ResizeQueueState {
    fn enqueue(
        &mut self,
        size: TerminalSize,
        pty_size: PtySize,
        enqueued_at: Instant,
    ) -> ResizeEnqueueOutcome {
        self.next_seq = self.next_seq.wrapping_add(1);
        let seq = self.next_seq;
        let replaced_seq = self.pending.as_ref().map(|pending| pending.seq);
        let spawn_worker = !self.worker_running;
        let queue_depth_hint = if self.worker_running { 2 } else { 1 };

        if spawn_worker {
            self.worker_running = true;
        }

        self.pending = Some(PendingResize {
            seq,
            size,
            pty_size,
            enqueued_at,
        });

        ResizeEnqueueOutcome {
            seq,
            replaced_seq,
            spawn_worker,
            queue_depth_hint,
        }
    }

    fn dequeue_for_worker(&mut self) -> Option<PendingResize> {
        if let Some(pending) = self.pending.take() {
            return Some(pending);
        }

        self.worker_running = false;
        None
    }

    fn superseded_by(&self, token: ResizeCancellationToken) -> Option<u64> {
        (self.next_seq > token.seq).then_some(self.next_seq)
    }
}

#[derive(Clone, Copy)]
struct ResizeApplyMetrics {
    current_size: TerminalSize,
    target_size: TerminalSize,
    probe_lock_wait: Duration,
    pty_lock_wait: Duration,
    pty_resize_elapsed: Duration,
    pty_resize_attempts: usize,
    pty_retry_backoff_elapsed: Duration,
    terminal_apply_lock_wait: Duration,
    terminal_resize_elapsed: Duration,
    noop: bool,
    cancelled: bool,
    cancelled_stage: Option<&'static str>,
    superseded_by_seq: Option<u64>,
}

#[derive(Clone, Copy)]
struct ResizeRetryPolicy {
    max_attempts: usize,
    base_backoff: Duration,
    max_backoff: Duration,
}

#[derive(Debug, Clone, Copy, Default)]
struct ResizeRetryStats {
    attempts: usize,
    backoff_elapsed: Duration,
}

fn pty_resize_retry_policy() -> ResizeRetryPolicy {
    ResizeRetryPolicy {
        max_attempts: 3,
        base_backoff: Duration::from_millis(2),
        max_backoff: Duration::from_millis(25),
    }
}

fn retry_backoff_for_attempt(policy: ResizeRetryPolicy, attempt: usize) -> Duration {
    if attempt == 0 {
        return Duration::default();
    }

    let shift = attempt.saturating_sub(1).min(20) as u32;
    let factor = 1u32 << shift;
    policy
        .base_backoff
        .saturating_mul(factor)
        .min(policy.max_backoff)
}

fn retry_with_backoff<T, E, F>(
    policy: ResizeRetryPolicy,
    mut op: F,
) -> Result<(T, ResizeRetryStats), (E, ResizeRetryStats)>
where
    F: FnMut(usize) -> Result<T, E>,
{
    let mut stats = ResizeRetryStats::default();
    for attempt in 1..=policy.max_attempts {
        stats.attempts = attempt;
        match op(attempt) {
            Ok(value) => return Ok((value, stats)),
            Err(err) => {
                if attempt == policy.max_attempts {
                    return Err((err, stats));
                }
                let backoff = retry_backoff_for_attempt(policy, attempt);
                stats.backoff_elapsed += backoff;
                std::thread::sleep(backoff);
            }
        }
    }

    unreachable!("retry loop must return from success or terminal failure")
}

pub struct LocalPane {
    pane_id: PaneId,
    terminal: Arc<Mutex<Terminal>>,
    process: Mutex<ProcessState>,
    pty: Arc<Mutex<Box<dyn MasterPty>>>,
    resize_queue: Arc<Mutex<ResizeQueueState>>,
    writer: Mutex<Box<dyn Write + Send>>,
    domain_id: DomainId,
    tmux_domain: Mutex<Option<Arc<TmuxDomainState>>>,
    proc_list: Mutex<Option<CachedProcInfo>>,
    #[cfg(unix)]
    leader: Arc<Mutex<Option<CachedLeaderInfo>>>,
    command_description: String,
}

#[async_trait(?Send)]
impl Pane for LocalPane {
    fn pane_id(&self) -> PaneId {
        self.pane_id
    }

    fn get_metadata(&self) -> Value {
        #[allow(unused_mut)]
        let mut map: BTreeMap<Value, Value> = BTreeMap::new();

        #[cfg(unix)]
        if let Some(tio) = self.pty.lock().get_termios() {
            use nix::sys::termios::LocalFlags;
            // Detect whether we might be in password input mode.
            // If local echo is disabled and canonical input mode
            // is enabled, then we assume that we're in some kind
            // of password-entry mode.
            let pw_input = !tio.local_flags.contains(LocalFlags::ECHO)
                && tio.local_flags.contains(LocalFlags::ICANON);
            map.insert(
                Value::String("password_input".to_string()),
                Value::Bool(pw_input),
            );
        }

        Value::Object(map.into())
    }

    fn get_cursor_position(&self) -> StableCursorPosition {
        let mut cursor = terminal_get_cursor_position(&mut self.terminal.lock());
        if self.tmux_domain.lock().is_some() {
            cursor.visibility = termwiz::surface::CursorVisibility::Hidden;
        }
        cursor
    }

    fn get_keyboard_encoding(&self) -> KeyboardEncoding {
        if self.tmux_domain.lock().is_some() {
            KeyboardEncoding::Xterm
        } else {
            self.terminal.lock().get_keyboard_encoding()
        }
    }

    fn get_current_seqno(&self) -> SequenceNo {
        self.terminal.lock().current_seqno()
    }

    fn get_changed_since(
        &self,
        lines: Range<StableRowIndex>,
        seqno: SequenceNo,
    ) -> RangeSet<StableRowIndex> {
        terminal_get_dirty_lines(&mut self.terminal.lock(), lines, seqno)
    }

    fn for_each_logical_line_in_stable_range_mut(
        &self,
        lines: Range<StableRowIndex>,
        for_line: &mut dyn ForEachPaneLogicalLine,
    ) {
        terminal_for_each_logical_line_in_stable_range_mut(
            &mut self.terminal.lock(),
            lines,
            for_line,
        );
    }

    fn with_lines_mut(&self, lines: Range<StableRowIndex>, with_lines: &mut dyn WithPaneLines) {
        terminal_with_lines_mut(&mut self.terminal.lock(), lines, with_lines)
    }

    fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
        crate::pane::impl_get_lines_via_with_lines(self, lines)
    }

    fn get_logical_lines(&self, lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
        crate::pane::impl_get_logical_lines_via_get_lines(self, lines)
    }

    fn get_dimensions(&self) -> RenderableDimensions {
        terminal_get_dimensions(&mut self.terminal.lock())
    }

    fn copy_user_vars(&self) -> HashMap<String, String> {
        self.terminal.lock().user_vars().clone()
    }

    fn exit_behavior(&self) -> Option<ExitBehavior> {
        // If we are ssh, and we've not yet fully connected,
        // then override exit_behavior so that we can show
        // connection issues
        let mut pty = self.pty.lock();
        let is_ssh_connecting = pty
            .downcast_mut::<crate::ssh::WrappedSshPty>()
            .map(|s| s.is_connecting())
            .unwrap_or(false);
        let is_failed_spawn = pty.is::<crate::domain::FailedSpawnPty>();

        if is_ssh_connecting || is_failed_spawn {
            Some(ExitBehavior::CloseOnCleanExit)
        } else {
            None
        }
    }

    fn kill(&self) {
        let mut proc = self.process.lock();
        log::debug!(
            "killing process in pane {}, state is {:?}",
            self.pane_id,
            proc
        );
        match &mut *proc {
            ProcessState::Running {
                signaller, killed, ..
            } => {
                let _ = signaller.kill();
                *killed = true;
            }
            ProcessState::DeadPendingClose { killed } => {
                *killed = true;
            }
            _ => {}
        }
    }

    fn is_dead(&self) -> bool {
        let mut proc = self.process.lock();

        const EXIT_BEHAVIOR: &str = "This message is shown because \
            \x1b]8;;https://wezterm.org/\
            config/lua/config/exit_behavior.html\
            \x1b\\exit_behavior\x1b]8;;\x1b\\";

        let mut terse = String::new();
        let mut brief = String::new();
        let mut trailer = String::new();
        let cmd = &self.command_description;

        match &mut *proc {
            ProcessState::Running {
                child_waiter,
                killed,
                ..
            } => {
                let status = match child_waiter.try_recv() {
                    Ok(Ok(s)) => Some(s),
                    Err(TryRecvError::Empty) => None,
                    _ => Some(ExitStatus::with_exit_code(1)),
                };

                if let Some(status) = status {
                    let success = match status.success() {
                        true => true,
                        false => configuration()
                            .clean_exit_codes
                            .contains(&status.exit_code()),
                    };

                    match (
                        self.exit_behavior()
                            .unwrap_or_else(|| configuration().exit_behavior),
                        success,
                        killed,
                    ) {
                        (ExitBehavior::Close, _, _) => *proc = ProcessState::Dead,
                        (ExitBehavior::CloseOnCleanExit, false, _) => {
                            brief = format!("⚠️  Process {cmd} didn't exit cleanly");
                            terse = format!("{status}.");
                            trailer = format!("{EXIT_BEHAVIOR}=\"CloseOnCleanExit\"");

                            *proc = ProcessState::DeadPendingClose { killed: false }
                        }
                        (ExitBehavior::CloseOnCleanExit, ..) => *proc = ProcessState::Dead,
                        (ExitBehavior::Hold, success, false) => {
                            trailer = format!("{EXIT_BEHAVIOR}=\"Hold\"");

                            if success {
                                brief = format!("👍 Process {cmd} completed.");
                                terse = "done".to_string();
                            } else {
                                brief = format!("⚠️  Process {cmd} didn't exit cleanly");
                                terse = format!("{status}");
                            }
                            *proc = ProcessState::DeadPendingClose { killed: false }
                        }
                        (ExitBehavior::Hold, _, true) => *proc = ProcessState::Dead,
                    }
                    log::debug!("child terminated, new state is {:?}", proc);
                }
            }
            ProcessState::DeadPendingClose { killed } => {
                if *killed {
                    *proc = ProcessState::Dead;
                    log::debug!("child state -> {:?}", proc);
                }
            }
            ProcessState::Dead => {}
        }

        let mut notify = None;
        if !terse.is_empty() {
            match configuration().exit_behavior_messaging {
                ExitBehaviorMessaging::Verbose => {
                    if terse == "done" {
                        notify = Some(format!("\r\n{brief}\r\n{trailer}"));
                    } else {
                        notify = Some(format!("\r\n{brief}\r\n{terse}\r\n{trailer}"));
                    }
                }
                ExitBehaviorMessaging::Brief => {
                    if terse == "done" {
                        notify = Some(format!("\r\n{brief}"));
                    } else {
                        notify = Some(format!("\r\n{brief}\r\n{terse}"));
                    }
                }
                ExitBehaviorMessaging::Terse => {
                    notify = Some(format!("\r\n[{terse}]"));
                }
                ExitBehaviorMessaging::None => {}
            }
        }

        if let Some(notify) = notify {
            emit_output_for_pane(self.pane_id, &notify);
        }

        match &*proc {
            ProcessState::Running { .. } => false,
            ProcessState::DeadPendingClose { .. } => false,
            ProcessState::Dead => true,
        }
    }

    fn set_clipboard(&self, clipboard: &Arc<dyn Clipboard>) {
        self.terminal.lock().set_clipboard(clipboard);
    }

    fn set_download_handler(&self, handler: &Arc<dyn DownloadHandler>) {
        self.terminal.lock().set_download_handler(handler);
    }

    fn set_config(&self, config: Arc<dyn TerminalConfiguration>) {
        self.terminal.lock().set_config(config);
    }

    fn get_config(&self) -> Option<Arc<dyn TerminalConfiguration>> {
        Some(self.terminal.lock().get_config())
    }

    fn perform_actions(&self, actions: Vec<termwiz::escape::Action>) {
        self.terminal.lock().perform_actions(actions)
    }

    fn mouse_event(&self, event: MouseEvent) -> Result<(), Error> {
        Mux::get().record_input_for_current_identity();
        self.terminal.lock().mouse_event(event)
    }

    fn key_down(&self, key: KeyCode, mods: KeyModifiers) -> Result<(), Error> {
        Mux::get().record_input_for_current_identity();
        if self.tmux_domain.lock().is_some() {
            log::trace!("key: {:?}", key);
            if key == KeyCode::Char('q') {
                self.terminal.lock().send_paste("detach\n")?;
            }
            return Ok(());
        } else {
            self.terminal.lock().key_down(key, mods)
        }
    }

    fn key_up(&self, key: KeyCode, mods: KeyModifiers) -> Result<(), Error> {
        Mux::get().record_input_for_current_identity();
        self.terminal.lock().key_up(key, mods)
    }

    fn resize(&self, size: TerminalSize) -> Result<(), Error> {
        self.enqueue_resize(size)
    }

    fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
        Mux::get().record_input_for_current_identity();
        MutexGuard::map(self.writer.lock(), |writer| {
            let w: &mut dyn std::io::Write = writer;
            w
        })
    }

    fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
        Ok(Some(self.pty.lock().try_clone_reader()?))
    }

    fn send_paste(&self, text: &str) -> Result<(), Error> {
        Mux::get().record_input_for_current_identity();
        if self.tmux_domain.lock().is_some() {
            Ok(())
        } else {
            self.terminal.lock().send_paste(text)
        }
    }

    fn get_title(&self) -> String {
        let title = self.terminal.lock().get_title().to_string();
        // If the title is the default pane title, then try to spice
        // things up a bit by returning the process basename instead
        if title == "wezterm" {
            if let Some(proc_name) = self.get_foreground_process_name(CachePolicy::AllowStale) {
                let proc_name = std::path::Path::new(&proc_name);
                if let Some(name) = proc_name.file_name() {
                    return name.to_string_lossy().to_string();
                }
            }
        }

        title
    }

    fn get_progress(&self) -> Progress {
        self.terminal.lock().get_progress()
    }

    fn palette(&self) -> ColorPalette {
        self.terminal.lock().palette()
    }

    fn domain_id(&self) -> DomainId {
        self.domain_id
    }

    fn erase_scrollback(&self, erase_mode: ScrollbackEraseMode) {
        match erase_mode {
            ScrollbackEraseMode::ScrollbackOnly => {
                self.terminal.lock().erase_scrollback();
            }
            ScrollbackEraseMode::ScrollbackAndViewport => {
                self.terminal.lock().erase_scrollback_and_viewport();
            }
        }
    }

    fn focus_changed(&self, focused: bool) {
        self.terminal.lock().focus_changed(focused);
    }

    fn has_unseen_output(&self) -> bool {
        self.terminal.lock().has_unseen_output()
    }

    fn is_mouse_grabbed(&self) -> bool {
        if self.tmux_domain.lock().is_some() {
            false
        } else {
            self.terminal.lock().is_mouse_grabbed()
        }
    }

    fn is_alt_screen_active(&self) -> bool {
        if self.tmux_domain.lock().is_some() {
            false
        } else {
            self.terminal.lock().is_alt_screen_active()
        }
    }

    fn get_current_working_dir(&self, policy: CachePolicy) -> Option<Url> {
        self.terminal
            .lock()
            .get_current_dir()
            .cloned()
            .or_else(|| self.divine_current_working_dir(policy))
    }

    fn tty_name(&self) -> Option<String> {
        #[cfg(unix)]
        {
            let name = self.pty.lock().tty_name()?;
            Some(name.to_string_lossy().into_owned())
        }

        #[cfg(windows)]
        {
            None
        }
    }

    fn get_foreground_process_info(&self, policy: CachePolicy) -> Option<LocalProcessInfo> {
        #[cfg(unix)]
        if let Some(pid) = self.pty.lock().process_group_leader() {
            return LocalProcessInfo::with_root_pid(pid as u32);
        }

        self.divine_foreground_process(policy)
    }

    fn get_foreground_process_name(&self, policy: CachePolicy) -> Option<String> {
        #[cfg(unix)]
        {
            let leader = self.get_leader(policy);
            if let Some(path) = &leader.path {
                return Some(path.to_string_lossy().to_string());
            }
            return None;
        }

        #[cfg(windows)]
        if let Some(fg) = self.divine_foreground_process(policy) {
            return Some(fg.executable.to_string_lossy().to_string());
        }

        #[allow(unreachable_code)]
        None
    }

    fn can_close_without_prompting(&self, _reason: CloseReason) -> bool {
        if let Some(info) = self.divine_process_list(CachePolicy::FetchImmediate) {
            log::trace!(
                "can_close_without_prompting? procs in pane {:#?}",
                info.root
            );

            let hook_result = {
                #[cfg(feature = "lua")]
                {
                    config::run_immediate_with_lua_config(|lua| {
                        let lua = match lua {
                            Some(lua) => lua,
                            None => return Ok(None),
                        };
                        let v = config::lua::emit_sync_callback(
                            &*lua,
                            ("mux-is-process-stateful".to_string(), (info.root.clone())),
                        )?;
                        match v {
                            mlua::Value::Nil => Ok(None),
                            mlua::Value::Boolean(v) => Ok(Some(v)),
                            _ => Ok(None),
                        }
                    })
                }
                #[cfg(not(feature = "lua"))]
                {
                    Ok::<Option<bool>, Error>(None)
                }
            };

            fn default_stateful_check(proc_list: &LocalProcessInfo) -> bool {
                // Fig uses `figterm` a pseudo terminal for a lot of functionality, it runs between
                // the shell and terminal. Unfortunately it is typically named `<shell> (figterm)`,
                // which prevents the statuful check from passing. This strips the suffix from the
                // process name to allow the check to pass.
                let names = proc_list
                    .flatten_to_exe_names()
                    .into_iter()
                    .map(|s| match s.strip_suffix(" (figterm)") {
                        Some(s) => s.into(),
                        None => s,
                    })
                    .collect::<HashSet<_>>();

                let skip = configuration()
                    .skip_close_confirmation_for_processes_named
                    .iter()
                    .cloned()
                    .collect::<HashSet<_>>();

                if !names.is_subset(&skip) {
                    // There are other processes running than are listed,
                    // so we consider this to be stateful
                    return true;
                }
                false
            }

            let is_stateful = match hook_result {
                Ok(None) => default_stateful_check(&info.root),
                Ok(Some(s)) => s,
                Err(err) => {
                    log::error!(
                        "Error while running mux-is-process-stateful \
                         hook: {:#}, falling back to default behavior",
                        err
                    );
                    default_stateful_check(&info.root)
                }
            };

            !is_stateful
        } else {
            #[cfg(unix)]
            {
                // If the process is dead but exit_behavior is holding the
                // window, we don't need to prompt to confirm closing.
                // That is detectable as no longer having a process group leader.
                if self.pty.lock().process_group_leader().is_none() {
                    return true;
                }
            }

            false
        }
    }

    fn get_semantic_zones(&self) -> anyhow::Result<Vec<SemanticZone>> {
        let mut term = self.terminal.lock();
        term.get_semantic_zones()
    }

    async fn search(
        &self,
        pattern: Pattern,
        range: Range<StableRowIndex>,
        limit: Option<u32>,
    ) -> anyhow::Result<Vec<SearchResult>> {
        let term = self.terminal.lock();
        let screen = term.screen();

        enum CompiledPattern {
            CaseSensitiveString(String),
            CaseInSensitiveString(String),
            Regex(Regex),
        }

        let pattern = match pattern {
            Pattern::CaseSensitiveString(s) => CompiledPattern::CaseSensitiveString(s),
            Pattern::CaseInSensitiveString(s) => {
                // normalize the case so we match everything lowercase
                CompiledPattern::CaseInSensitiveString(s.to_lowercase())
            }
            Pattern::Regex(r) => CompiledPattern::Regex(Regex::new(&r)?),
        };

        let mut results = vec![];
        let mut uniq_matches: HashMap<String, usize> = HashMap::new();

        screen.for_each_logical_line_in_stable_range(range, |sr, lines| {
            if let Some(limit) = limit {
                if results.len() == limit as usize {
                    // We've reach the limit, stop iteration.
                    return false;
                }
            }

            if lines.is_empty() {
                // Nothing to do on this iteration, carry on with the next.
                return true;
            }
            let haystack = if lines.len() == 1 {
                lines[0].as_str()
            } else {
                let mut s = String::new();
                for line in lines {
                    s.push_str(&line.as_str());
                }
                Cow::Owned(s)
            };
            let stable_idx = sr.start;

            if haystack.is_empty() {
                return true;
            }

            let haystack = match &pattern {
                CompiledPattern::CaseInSensitiveString(_) => Cow::Owned(haystack.to_lowercase()),
                _ => haystack,
            };
            let mut coords = None;

            match &pattern {
                CompiledPattern::CaseInSensitiveString(s)
                | CompiledPattern::CaseSensitiveString(s) => {
                    for (idx, s) in haystack.match_indices(s) {
                        found_match(
                            s,
                            idx,
                            lines,
                            stable_idx,
                            &mut uniq_matches,
                            &mut coords,
                            &mut results,
                        );
                    }
                }
                CompiledPattern::Regex(re) => {
                    // Allow for the regex to contain captures
                    for capture_res in re.captures_iter(&haystack) {
                        if let Ok(c) = capture_res {
                            // Look for the captures in reverse order, as index==0 is
                            // the whole matched string.  We can't just call
                            // `c.iter().rev()` as the capture iterator isn't double-ended.
                            for idx in (0..c.len()).rev() {
                                if let Some(m) = c.get(idx) {
                                    found_match(
                                        m.as_str(),
                                        m.start(),
                                        lines,
                                        stable_idx,
                                        &mut uniq_matches,
                                        &mut coords,
                                        &mut results,
                                    );
                                    break;
                                }
                            }
                        }
                    }
                }
            }

            // Keep iterating
            true
        });

        #[derive(Copy, Clone, Debug)]
        struct Coord {
            byte_idx: usize,
            grapheme_idx: usize,
            stable_row: StableRowIndex,
        }

        fn found_match(
            text: &str,
            byte_idx: usize,
            lines: &[&Line],
            stable_idx: StableRowIndex,
            uniq_matches: &mut HashMap<String, usize>,
            coords: &mut Option<Vec<Coord>>,
            results: &mut Vec<SearchResult>,
        ) {
            if coords.is_none() {
                coords.replace(make_coords(lines, stable_idx));
            }
            let coords = coords.as_ref().unwrap();

            let match_id = match uniq_matches.get(text).copied() {
                Some(id) => id,
                None => {
                    let id = uniq_matches.len();
                    uniq_matches.insert(text.to_owned(), id);
                    id
                }
            };
            let (start_x, start_y) = haystack_idx_to_coord(byte_idx, coords);
            let (end_x, end_y) = haystack_idx_to_coord(byte_idx + text.len(), coords);
            results.push(SearchResult {
                start_x,
                start_y,
                end_x,
                end_y,
                match_id,
            });
        }

        fn make_coords(lines: &[&Line], stable_row: StableRowIndex) -> Vec<Coord> {
            let mut byte_idx = 0;
            let mut coords = vec![];

            for (row_idx, line) in lines.iter().enumerate() {
                for cell in line.visible_cells() {
                    coords.push(Coord {
                        byte_idx,
                        grapheme_idx: cell.cell_index(),
                        stable_row: stable_row + row_idx as StableRowIndex,
                    });
                    byte_idx += cell.str().len();
                }
            }

            coords
        }

        fn haystack_idx_to_coord(idx: usize, coords: &[Coord]) -> (usize, StableRowIndex) {
            let c = coords
                .binary_search_by(|ele| ele.byte_idx.cmp(&idx))
                .or_else(|i| -> Result<usize, usize> { Ok(i) })
                .unwrap();
            let coord = coords.get(c).map(|c| *c).unwrap_or_else(|| {
                let last = coords.last().unwrap();
                Coord {
                    grapheme_idx: last.grapheme_idx + 1,
                    ..*last
                }
            });
            (coord.grapheme_idx, coord.stable_row)
        }

        Ok(results)
    }
}

struct LocalPaneDCSHandler {
    pane_id: PaneId,
    tmux_domain: Option<Arc<TmuxDomainState>>,
}

pub(crate) fn emit_output_for_pane(pane_id: PaneId, message: &str) {
    let mut parser = termwiz::escape::parser::Parser::new();
    let mut actions = vec![Action::CSI(CSI::Sgr(Sgr::Reset))];
    parser.parse(message.as_bytes(), |action| actions.push(action));

    promise::spawn::spawn_into_main_thread(async move {
        let mux = Mux::get();
        if let Some(pane) = mux.get_pane(pane_id) {
            pane.perform_actions(actions);
            mux.notify(MuxNotification::PaneOutput(pane_id));
        }
    })
    .detach();
}

impl frankenterm_term::DeviceControlHandler for LocalPaneDCSHandler {
    fn handle_device_control(&mut self, control: termwiz::escape::DeviceControlMode) {
        match control {
            DeviceControlMode::Enter(mode) => {
                if !mode.ignored_extra_intermediates
                    && mode.params.len() == 1
                    && mode.params[0] == 1000
                    && mode.intermediates.is_empty()
                {
                    log::info!("tmux -CC mode requested");

                    // Create a new domain to host these tmux tabs
                    let domain = TmuxDomain::new(self.pane_id);
                    let tmux_domain = Arc::clone(&domain.inner);

                    let domain: Arc<dyn Domain> = Arc::new(domain);
                    let mux = Mux::get();
                    mux.add_domain(&domain);

                    if let Some(pane) = mux.get_pane(self.pane_id) {
                        let pane = pane.downcast_ref::<LocalPane>().unwrap();
                        pane.tmux_domain.lock().replace(Arc::clone(&tmux_domain));

                        emit_output_for_pane(
                            self.pane_id,
                            "\r\n[This pane is running tmux control mode. Press q to detach]",
                        );
                    }

                    self.tmux_domain.replace(tmux_domain);

                // TODO: do we need to proactively list available tabs here?
                // if so we should arrange to call domain.attach() and make
                // it do the right thing.
                } else if configuration().log_unknown_escape_sequences {
                    log::warn!("unknown DeviceControlMode::Enter {:?}", mode,);
                }
            }
            DeviceControlMode::Exit => {
                if let Some(tmux) = self.tmux_domain.take() {
                    let mux = Mux::get();
                    if let Some(pane) = mux.get_pane(self.pane_id) {
                        let pane = pane.downcast_ref::<LocalPane>().unwrap();
                        pane.tmux_domain.lock().take();
                    }
                    mux.domain_was_detached(tmux.domain_id);
                }
            }
            DeviceControlMode::Data(c) => {
                if configuration().log_unknown_escape_sequences {
                    log::warn!(
                        "unhandled DeviceControlMode::Data {:x} {}",
                        c,
                        (c as char).escape_debug()
                    );
                }
            }
            DeviceControlMode::TmuxEvents(events) => {
                if let Some(tmux) = self.tmux_domain.as_ref() {
                    tmux.advance(events);
                } else {
                    log::warn!("unhandled DeviceControlMode::TmuxEvents {:?}", &events);
                }
            }
            _ => {
                if configuration().log_unknown_escape_sequences {
                    log::warn!("unhandled: {:?}", control);
                }
            }
        }
    }
}

struct LocalPaneNotifHandler {
    pane_id: PaneId,
}

impl AlertHandler for LocalPaneNotifHandler {
    fn alert(&mut self, alert: Alert) {
        let pane_id = self.pane_id;
        promise::spawn::spawn_into_main_thread(async move {
            let mux = Mux::get();
            match &alert {
                Alert::WindowTitleChanged(title) => {
                    if let Some((_domain, window_id, _tab_id)) = mux.resolve_pane_id(pane_id) {
                        if let Some(mut window) = mux.get_window_mut(window_id) {
                            window.set_title(title);
                        }
                    }
                }
                Alert::TabTitleChanged(title) => {
                    if let Some((_domain, _window_id, tab_id)) = mux.resolve_pane_id(pane_id) {
                        if let Some(tab) = mux.get_tab(tab_id) {
                            tab.set_title(title.as_deref().unwrap_or(""));
                        }
                    }
                }
                _ => {}
            }

            mux.notify(MuxNotification::Alert { pane_id, alert });
        })
        .detach();
    }
}

/// This is a little gross; on some systems, our pipe reader will continue
/// to be blocked in read even after the child process has died.
/// We need to wake up and notice that the child terminated in order
/// for our state to wind down.
/// This block schedules a background thread to wait for the child
/// to terminate, and then nudge the muxer to check for dead processes.
/// Without this, typing `exit` in `cmd.exe` would keep the pane around
/// until something else triggered the mux to prune dead processes.
fn split_child(
    mut process: Box<dyn Child>,
) -> (
    Receiver<IoResult<ExitStatus>>,
    Box<dyn ChildKiller + Sync>,
    Option<u32>,
) {
    let pid = process.process_id();
    let signaller = process.clone_killer();

    let (tx, rx) = bounded(1);

    std::thread::spawn(move || {
        let status = process.wait();
        tx.try_send(status).ok();
        promise::spawn::spawn_into_main_thread(async move {
            let mux = Mux::get();
            mux.prune_dead_windows();
        })
        .detach();
    });

    (rx, signaller, pid)
}

impl LocalPane {
    fn enqueue_resize(&self, size: TerminalSize) -> Result<(), Error> {
        let pty_size = PtySize {
            rows: size.rows.try_into()?,
            cols: size.cols.try_into()?,
            pixel_width: size.pixel_width.try_into()?,
            pixel_height: size.pixel_height.try_into()?,
        };
        let enqueued_at = Instant::now();

        let outcome = {
            let mut queue = self.resize_queue.lock();
            queue.enqueue(size, pty_size, enqueued_at)
        };

        log::trace!(
            "LocalPane::resize enqueue pane_id={} seq={} target={}x{} replaced_seq={:?} queue_depth_hint={} worker_spawned={}",
            self.pane_id,
            outcome.seq,
            size.cols,
            size.rows,
            outcome.replaced_seq,
            outcome.queue_depth_hint,
            outcome.spawn_worker
        );

        if outcome.spawn_worker {
            Self::spawn_resize_worker(
                self.pane_id,
                Arc::clone(&self.terminal),
                Arc::clone(&self.pty),
                Arc::clone(&self.resize_queue),
            );
        }

        Ok(())
    }

    fn spawn_resize_worker(
        pane_id: PaneId,
        terminal: Arc<Mutex<Terminal>>,
        pty: Arc<Mutex<Box<dyn MasterPty>>>,
        resize_queue: Arc<Mutex<ResizeQueueState>>,
    ) {
        let worker_queue = Arc::clone(&resize_queue);
        let spawn_result =
            std::thread::Builder::new()
                .name(format!("pane-resize-{}", pane_id))
                .spawn(move || {
                    while let Some(pending) = {
                        let mut queue = worker_queue.lock();
                        queue.dequeue_for_worker()
                    } {
                        let queue_wait = pending.enqueued_at.elapsed();
                        let completion_start = Instant::now();
                        let token = ResizeCancellationToken::new(pending.seq);
                        match Self::apply_resize_sync(
                            pane_id,
                            terminal.as_ref(),
                            pty.as_ref(),
                            pending.size,
                            pending.pty_size,
                            || {
                                let queue = worker_queue.lock();
                                queue.superseded_by(token)
                            },
                        ) {
                            Ok(metrics) => {
                                if metrics.cancelled {
                                    log::trace!(
                                        "LocalPane::resize cancelled pane_id={} seq={} superseded_by_seq={} stage={} queue_wait_us={} completion_us={} current={}x{} target={}x{} probe_lock_wait_us={} pty_lock_wait_us={} pty_resize_us={} pty_resize_attempts={} pty_retry_backoff_us={} terminal_apply_lock_wait_us={} terminal_resize_us={}",
                                        pane_id,
                                        pending.seq,
                                        metrics.superseded_by_seq.unwrap_or_default(),
                                        metrics.cancelled_stage.unwrap_or("unknown"),
                                        queue_wait.as_micros(),
                                        completion_start.elapsed().as_micros(),
                                        metrics.current_size.cols,
                                        metrics.current_size.rows,
                                        metrics.target_size.cols,
                                        metrics.target_size.rows,
                                        metrics.probe_lock_wait.as_micros(),
                                        metrics.pty_lock_wait.as_micros(),
                                        metrics.pty_resize_elapsed.as_micros(),
                                        metrics.pty_resize_attempts,
                                        metrics.pty_retry_backoff_elapsed.as_micros(),
                                        metrics.terminal_apply_lock_wait.as_micros(),
                                        metrics.terminal_resize_elapsed.as_micros(),
                                    );
                                } else {
                                    log::trace!(
                                        "LocalPane::resize complete pane_id={} seq={} queue_wait_us={} completion_us={} noop={} current={}x{} target={}x{} probe_lock_wait_us={} pty_lock_wait_us={} pty_resize_us={} pty_resize_attempts={} pty_retry_backoff_us={} terminal_apply_lock_wait_us={} terminal_resize_us={}",
                                        pane_id,
                                        pending.seq,
                                        queue_wait.as_micros(),
                                        completion_start.elapsed().as_micros(),
                                        metrics.noop,
                                        metrics.current_size.cols,
                                        metrics.current_size.rows,
                                        metrics.target_size.cols,
                                        metrics.target_size.rows,
                                        metrics.probe_lock_wait.as_micros(),
                                        metrics.pty_lock_wait.as_micros(),
                                        metrics.pty_resize_elapsed.as_micros(),
                                        metrics.pty_resize_attempts,
                                        metrics.pty_retry_backoff_elapsed.as_micros(),
                                        metrics.terminal_apply_lock_wait.as_micros(),
                                        metrics.terminal_resize_elapsed.as_micros(),
                                    );
                                }
                            }
                            Err(err) => {
                                log::error!(
                                    "LocalPane::resize error pane_id={} seq={} target={}x{} error={:#}",
                                    pane_id,
                                    pending.seq,
                                    pending.size.cols,
                                    pending.size.rows,
                                    err
                                );
                            }
                        }
                    }
                });

        if let Err(err) = spawn_result {
            log::error!(
                "failed to spawn resize worker pane_id={} error={:#}",
                pane_id,
                err
            );
            let mut queue = resize_queue.lock();
            queue.worker_running = false;
        }
    }

    fn apply_resize_sync(
        pane_id: PaneId,
        terminal: &Mutex<Terminal>,
        pty: &Mutex<Box<dyn MasterPty>>,
        size: TerminalSize,
        pty_size: PtySize,
        mut superseded_by: impl FnMut() -> Option<u64>,
    ) -> Result<ResizeApplyMetrics, Error> {
        let terminal_probe_lock_start = Instant::now();
        let current_size = terminal.lock().get_size();
        let terminal_probe_lock_wait = terminal_probe_lock_start.elapsed();

        if current_size == size {
            return Ok(ResizeApplyMetrics {
                current_size,
                target_size: size,
                probe_lock_wait: terminal_probe_lock_wait,
                pty_lock_wait: Duration::default(),
                pty_resize_elapsed: Duration::default(),
                pty_resize_attempts: 0,
                pty_retry_backoff_elapsed: Duration::default(),
                terminal_apply_lock_wait: Duration::default(),
                terminal_resize_elapsed: Duration::default(),
                noop: true,
                cancelled: false,
                cancelled_stage: None,
                superseded_by_seq: None,
            });
        }

        if let Some(superseded_by_seq) = superseded_by() {
            return Ok(ResizeApplyMetrics {
                current_size,
                target_size: size,
                probe_lock_wait: terminal_probe_lock_wait,
                pty_lock_wait: Duration::default(),
                pty_resize_elapsed: Duration::default(),
                pty_resize_attempts: 0,
                pty_retry_backoff_elapsed: Duration::default(),
                terminal_apply_lock_wait: Duration::default(),
                terminal_resize_elapsed: Duration::default(),
                noop: false,
                cancelled: true,
                cancelled_stage: Some("before_pty_resize"),
                superseded_by_seq: Some(superseded_by_seq),
            });
        }

        let policy = pty_resize_retry_policy();
        let mut pty_lock_wait = Duration::default();
        let mut pty_resize_elapsed = Duration::default();
        let (_, retry_stats) = retry_with_backoff(policy, |attempt| {
            let pty_lock_start = Instant::now();
            let pty = pty.lock();
            pty_lock_wait += pty_lock_start.elapsed();
            let pty_resize_start = Instant::now();
            let result = pty.resize(pty_size);
            pty_resize_elapsed += pty_resize_start.elapsed();
            drop(pty);
            if let Err(err) = result {
                log::warn!(
                    "LocalPane::resize pty retry pane_id={} attempt={}/{} target={}x{} error={:#}",
                    pane_id,
                    attempt,
                    policy.max_attempts,
                    size.cols,
                    size.rows,
                    err
                );
                return Err(err);
            }
            Ok(())
        })
        .map_err(|(err, stats)| {
            err.context(format!(
                "pty resize failed after {} attempts for pane_id={} target={}x{}",
                stats.attempts, pane_id, size.cols, size.rows
            ))
        })?;

        if let Some(superseded_by_seq) = superseded_by() {
            return Ok(ResizeApplyMetrics {
                current_size,
                target_size: size,
                probe_lock_wait: terminal_probe_lock_wait,
                pty_lock_wait,
                pty_resize_elapsed,
                pty_resize_attempts: retry_stats.attempts,
                pty_retry_backoff_elapsed: retry_stats.backoff_elapsed,
                terminal_apply_lock_wait: Duration::default(),
                terminal_resize_elapsed: Duration::default(),
                noop: false,
                cancelled: true,
                cancelled_stage: Some("before_terminal_apply"),
                superseded_by_seq: Some(superseded_by_seq),
            });
        }

        let terminal_apply_lock_start = Instant::now();
        let mut terminal = terminal.lock();
        let terminal_apply_lock_wait = terminal_apply_lock_start.elapsed();
        let terminal_resize_start = Instant::now();
        terminal.resize(size);
        let terminal_resize_elapsed = terminal_resize_start.elapsed();

        Ok(ResizeApplyMetrics {
            current_size,
            target_size: size,
            probe_lock_wait: terminal_probe_lock_wait,
            pty_lock_wait,
            pty_resize_elapsed,
            pty_resize_attempts: retry_stats.attempts,
            pty_retry_backoff_elapsed: retry_stats.backoff_elapsed,
            terminal_apply_lock_wait,
            terminal_resize_elapsed,
            noop: false,
            cancelled: false,
            cancelled_stage: None,
            superseded_by_seq: None,
        })
    }

    pub fn new(
        pane_id: PaneId,
        mut terminal: Terminal,
        process: Box<dyn Child + Send>,
        pty: Box<dyn MasterPty>,
        writer: Box<dyn Write + Send>,
        domain_id: DomainId,
        command_description: String,
    ) -> Self {
        let (process, signaller, pid) = split_child(process);

        terminal.set_device_control_handler(Box::new(LocalPaneDCSHandler {
            pane_id,
            tmux_domain: None,
        }));
        terminal.set_notification_handler(Box::new(LocalPaneNotifHandler { pane_id }));

        Self {
            pane_id,
            terminal: Arc::new(Mutex::new(terminal)),
            process: Mutex::new(ProcessState::Running {
                child_waiter: process,
                pid,
                signaller,
                killed: false,
            }),
            pty: Arc::new(Mutex::new(pty)),
            resize_queue: Arc::new(Mutex::new(ResizeQueueState::default())),
            writer: Mutex::new(writer),
            domain_id,
            tmux_domain: Mutex::new(None),
            proc_list: Mutex::new(None),
            #[cfg(unix)]
            leader: Arc::new(Mutex::new(None)),
            command_description,
        }
    }

    #[cfg(unix)]
    fn get_leader(&self, policy: CachePolicy) -> CachedLeaderInfo {
        let mut leader = self.leader.lock();

        if policy == CachePolicy::FetchImmediate {
            leader.replace(CachedLeaderInfo::new(self.pty.lock().as_raw_fd()));
        } else if let Some(info) = leader.as_mut() {
            // If stale, queue up some work in another thread to update.
            // Right now, we'll return the stale data.
            if info.expired() && info.can_update() {
                info.updating = true;
                let leader_ref = Arc::clone(&self.leader);
                std::thread::spawn(move || {
                    let mut leader = leader_ref.lock();
                    if let Some(leader) = leader.as_mut() {
                        leader.update();
                    }
                });
            }
        } else {
            leader.replace(CachedLeaderInfo::new(self.pty.lock().as_raw_fd()));
        }

        (*leader).clone().unwrap()
    }

    fn divine_current_working_dir(&self, policy: CachePolicy) -> Option<Url> {
        #[cfg(unix)]
        {
            let leader = self.get_leader(policy);
            if let Some(path) = &leader.current_working_dir {
                return Url::from_directory_path(path).ok();
            }
            return None;
        }

        #[cfg(windows)]
        if let Some(fg) = self.divine_foreground_process(policy) {
            return Url::from_directory_path(fg.cwd).ok();
        }

        #[allow(unreachable_code)]
        None
    }

    fn divine_process_list(
        &self,
        policy: CachePolicy,
    ) -> Option<MappedMutexGuard<'_, CachedProcInfo>> {
        if let ProcessState::Running { pid: Some(pid), .. } = &*self.process.lock() {
            let mut proc_list = self.proc_list.lock();

            let expired = policy == CachePolicy::FetchImmediate
                || proc_list
                    .as_ref()
                    .map(|info| info.updated.elapsed() > PROC_INFO_CACHE_TTL)
                    .unwrap_or(true);

            if expired {
                log::trace!("CachedProcInfo expired, refresh");
                let root = LocalProcessInfo::with_root_pid(*pid)?;

                // Windows doesn't have any job control or session concept,
                // so we infer that the equivalent to the process group
                // leader is the most recently spawned program running
                // in the console
                let mut youngest = &root;

                fn find_youngest<'a>(
                    proc: &'a LocalProcessInfo,
                    youngest: &mut &'a LocalProcessInfo,
                ) {
                    if proc.start_time >= youngest.start_time {
                        *youngest = proc;
                    }

                    for child in proc.children.values() {
                        #[cfg(windows)]
                        if child.console == 0 {
                            continue;
                        }
                        find_youngest(child, youngest);
                    }
                }

                find_youngest(&root, &mut youngest);
                let mut foreground = youngest.clone();
                foreground.children.clear();

                proc_list.replace(CachedProcInfo {
                    root,
                    foreground,
                    updated: Instant::now(),
                });
                log::trace!("CachedProcInfo updated");
            }

            return Some(MutexGuard::map(proc_list, |info| info.as_mut().unwrap()));
        }
        None
    }

    #[allow(dead_code)]
    fn divine_foreground_process(&self, policy: CachePolicy) -> Option<LocalProcessInfo> {
        if let Some(info) = self.divine_process_list(policy) {
            Some(info.foreground.clone())
        } else {
            None
        }
    }
}

impl Drop for LocalPane {
    fn drop(&mut self) {
        // Avoid lingering zombies if we can, but don't block forever.
        // <https://github.com/wezterm/wezterm/issues/558>
        if let ProcessState::Running { signaller, .. } = &mut *self.process.lock() {
            let _ = signaller.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term_size(cols: usize, rows: usize) -> TerminalSize {
        TerminalSize {
            cols,
            rows,
            pixel_width: cols,
            pixel_height: rows,
            dpi: 96,
        }
    }

    fn pty_size(cols: u16, rows: u16) -> PtySize {
        PtySize {
            cols,
            rows,
            pixel_width: cols,
            pixel_height: rows,
        }
    }

    #[derive(Default)]
    struct ResizeReplayHarness {
        queue: ResizeQueueState,
        in_flight: Option<PendingResize>,
        completed: Vec<u64>,
        cancelled: Vec<u64>,
        causality: Vec<String>,
    }

    impl ResizeReplayHarness {
        fn enqueue(&mut self, cols: usize, rows: usize) -> ResizeEnqueueOutcome {
            let size = term_size(cols, rows);
            let pty = pty_size(cols as u16, rows as u16);
            let outcome = self.queue.enqueue(size, pty, Instant::now());
            self.causality.push(format!(
                "intent seq={} target={}x{} replaced_seq={:?} spawn_worker={}",
                outcome.seq, cols, rows, outcome.replaced_seq, outcome.spawn_worker
            ));
            outcome
        }

        fn start_next(&mut self) -> Option<PendingResize> {
            if self.in_flight.is_some() {
                return None;
            }

            let pending = self.queue.dequeue_for_worker();
            if let Some(pending) = pending {
                self.causality.push(format!(
                    "start seq={} target={}x{}",
                    pending.seq, pending.size.cols, pending.size.rows
                ));
                self.in_flight = Some(pending);
            }
            pending
        }

        fn complete_current(&mut self) -> Option<PendingResize> {
            let completed = self.in_flight.take()?;
            self.causality.push(format!(
                "complete seq={} target={}x{}",
                completed.seq, completed.size.cols, completed.size.rows
            ));
            self.completed.push(completed.seq);
            Some(completed)
        }

        fn boundary_cancel_current_if_superseded(&mut self) -> bool {
            let active = match self.in_flight {
                Some(active) => active,
                None => return false,
            };

            let token = ResizeCancellationToken::new(active.seq);
            let Some(latest_seq) = self.queue.superseded_by(token) else {
                return false;
            };

            let cancelled = self.in_flight.take().expect("in-flight resize must exist");
            self.cancelled.push(cancelled.seq);
            self.causality.push(format!(
                "cancel seq={} superseded_by={latest_seq}",
                cancelled.seq
            ));
            true
        }

        fn causality_contains(&self, needle: &str) -> bool {
            self.causality.iter().any(|line| line.contains(needle))
        }
    }

    #[test]
    fn retry_with_backoff_succeeds_after_transient_failures() {
        let policy = ResizeRetryPolicy {
            max_attempts: 5,
            base_backoff: Duration::default(),
            max_backoff: Duration::default(),
        };
        let mut seen_attempts = Vec::new();

        let result = retry_with_backoff(policy, |attempt| {
            seen_attempts.push(attempt);
            if attempt < 3 {
                Err("transient")
            } else {
                Ok("ok")
            }
        });

        let (value, stats) = result.expect("retry should eventually succeed");
        assert_eq!(value, "ok");
        assert_eq!(stats.attempts, 3);
        assert_eq!(stats.backoff_elapsed, Duration::default());
        assert_eq!(seen_attempts, vec![1, 2, 3]);
    }

    #[test]
    fn retry_with_backoff_reports_terminal_failure_after_budget() {
        let policy = ResizeRetryPolicy {
            max_attempts: 3,
            base_backoff: Duration::default(),
            max_backoff: Duration::default(),
        };
        let mut seen_attempts = 0usize;

        let result: Result<(&'static str, ResizeRetryStats), (&'static str, ResizeRetryStats)> =
            retry_with_backoff(policy, |_| {
                seen_attempts += 1;
                Err("persistent")
            });

        let (err, stats) = result.expect_err("retry should fail after max attempts");
        assert_eq!(err, "persistent");
        assert_eq!(stats.attempts, 3);
        assert_eq!(stats.backoff_elapsed, Duration::default());
        assert_eq!(seen_attempts, 3);
    }

    #[test]
    fn retry_backoff_is_monotonic_and_capped() {
        let policy = ResizeRetryPolicy {
            max_attempts: 6,
            base_backoff: Duration::from_millis(2),
            max_backoff: Duration::from_millis(5),
        };

        let d1 = retry_backoff_for_attempt(policy, 1);
        let d2 = retry_backoff_for_attempt(policy, 2);
        let d3 = retry_backoff_for_attempt(policy, 3);
        let d4 = retry_backoff_for_attempt(policy, 4);

        assert!(d1 <= d2);
        assert!(d2 <= d3);
        assert!(d3 <= d4);
        assert_eq!(d1, Duration::from_millis(2));
        assert_eq!(d2, Duration::from_millis(4));
        assert_eq!(d3, Duration::from_millis(5));
        assert_eq!(d4, Duration::from_millis(5));
    }

    #[test]
    fn resize_queue_coalesces_latest_pending_when_worker_is_running() {
        let mut queue = ResizeQueueState::default();
        let now = Instant::now();

        let first = queue.enqueue(term_size(80, 24), pty_size(80, 24), now);
        assert_eq!(first.seq, 1);
        assert!(first.spawn_worker);
        assert_eq!(first.replaced_seq, None);
        assert_eq!(first.queue_depth_hint, 1);

        let in_flight = queue
            .dequeue_for_worker()
            .expect("first request must be available for worker");
        assert_eq!(in_flight.seq, 1);

        let second = queue.enqueue(term_size(100, 30), pty_size(100, 30), now);
        assert_eq!(second.seq, 2);
        assert!(!second.spawn_worker);
        assert_eq!(second.replaced_seq, None);
        assert_eq!(second.queue_depth_hint, 2);

        let third = queue.enqueue(term_size(120, 40), pty_size(120, 40), now);
        assert_eq!(third.seq, 3);
        assert!(!third.spawn_worker);
        assert_eq!(third.replaced_seq, Some(2));
        assert_eq!(third.queue_depth_hint, 2);

        let next = queue
            .dequeue_for_worker()
            .expect("coalesced request must be available");
        assert_eq!(next.seq, 3);
        assert_eq!(next.size, term_size(120, 40));
        assert_eq!(next.pty_size, pty_size(120, 40));

        assert!(queue.dequeue_for_worker().is_none());
        assert!(!queue.worker_running);
    }

    #[test]
    fn resize_queue_marks_worker_idle_when_empty() {
        let mut queue = ResizeQueueState::default();
        let now = Instant::now();

        let first = queue.enqueue(term_size(90, 25), pty_size(90, 25), now);
        assert!(first.spawn_worker);
        assert!(queue.dequeue_for_worker().is_some());
        assert!(queue.worker_running);

        assert!(queue.dequeue_for_worker().is_none());
        assert!(!queue.worker_running);

        let second = queue.enqueue(term_size(91, 25), pty_size(91, 25), now);
        assert!(second.spawn_worker);
        assert_eq!(second.queue_depth_hint, 1);
    }

    #[test]
    fn resize_queue_stress_preserves_latest_intent_only() {
        let mut queue = ResizeQueueState::default();
        let now = Instant::now();

        let first = queue.enqueue(term_size(80, 24), pty_size(80, 24), now);
        assert!(first.spawn_worker);
        let _ = queue.dequeue_for_worker();

        for n in 0..1000u16 {
            let cols = 100 + n;
            let rows = 40 + (n % 10);
            let _ = queue.enqueue(
                term_size(cols as usize, rows as usize),
                pty_size(cols, rows),
                now,
            );
        }

        let pending = queue
            .dequeue_for_worker()
            .expect("latest coalesced request should remain");
        assert_eq!(pending.size.cols, 1099);
        assert_eq!(pending.size.rows, 49);
        assert_eq!(pending.pty_size.cols, 1099);
        assert_eq!(pending.pty_size.rows, 49);
    }

    #[test]
    fn resize_queue_cancellation_token_reports_when_intent_is_superseded() {
        let mut queue = ResizeQueueState::default();
        let now = Instant::now();

        let first = queue.enqueue(term_size(80, 24), pty_size(80, 24), now);
        let token = ResizeCancellationToken::new(first.seq);
        assert_eq!(queue.superseded_by(token), None);

        let second = queue.enqueue(term_size(100, 30), pty_size(100, 30), now);
        assert_eq!(queue.superseded_by(token), Some(second.seq));
        assert_eq!(
            queue.superseded_by(ResizeCancellationToken::new(second.seq)),
            None
        );
    }

    #[test]
    fn replay_cancellation_race_coalesces_to_latest_intent() {
        let mut replay = ResizeReplayHarness::default();

        let first = replay.enqueue(80, 24);
        assert!(first.spawn_worker);
        let in_flight = replay.start_next().expect("first intent should start");
        assert_eq!(in_flight.seq, 1);

        let second = replay.enqueue(120, 30);
        assert_eq!(second.replaced_seq, None);
        let third = replay.enqueue(140, 40);
        assert_eq!(third.replaced_seq, Some(2));

        assert!(replay.boundary_cancel_current_if_superseded());
        let coalesced = replay
            .start_next()
            .expect("latest coalesced intent should start");
        assert_eq!(coalesced.seq, 3);
        replay
            .complete_current()
            .expect("coalesced intent should complete");

        assert_eq!(replay.cancelled, vec![1]);
        assert_eq!(replay.completed, vec![3]);
        assert!(replay.causality_contains("intent seq=3"));
        assert!(replay.causality_contains("replaced_seq=Some(2)"));
        assert!(replay.causality_contains("cancel seq=1 superseded_by=3"));
        assert!(replay.causality_contains("complete seq=3"));
    }

    #[test]
    fn replay_prevents_out_of_order_completion() {
        let mut replay = ResizeReplayHarness::default();

        replay.enqueue(90, 30);
        replay.start_next().expect("first intent should start");

        replay.enqueue(100, 30);
        replay.enqueue(110, 30);

        // Worker has one in-flight request; next start attempt must be deferred.
        assert!(replay.start_next().is_none());

        let first_complete = replay.complete_current().expect("first should complete");
        assert_eq!(first_complete.seq, 1);

        let second_start = replay
            .start_next()
            .expect("latest pending should now start");
        assert_eq!(second_start.seq, 3);
        replay
            .complete_current()
            .expect("second in-flight should complete");

        assert_eq!(replay.completed, vec![1, 3]);
    }

    #[test]
    fn replay_rapid_resizes_emit_intent_to_completion_causality_chain() {
        let mut replay = ResizeReplayHarness::default();

        replay.enqueue(80, 24);
        replay.start_next().expect("first intent should start");

        for i in 0..200usize {
            let _ = replay.enqueue(100 + i, 30 + (i % 5));
        }

        replay.complete_current().expect("first should complete");
        let latest = replay.start_next().expect("latest pending should start");
        replay.complete_current().expect("latest should complete");

        assert!(latest.seq > 1);
        assert!(replay.causality_contains("intent seq=1"));
        assert!(replay.causality_contains("start seq=1"));
        assert!(replay.causality_contains("complete seq=1"));
        assert!(
            replay
                .causality
                .iter()
                .any(|line| line.contains("replaced_seq=Some(")),
            "expected at least one coalescing replacement entry"
        );
        assert!(replay.causality_contains(&format!("complete seq={}", latest.seq)));
    }
}
