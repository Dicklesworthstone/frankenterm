use crate::tmux::{RefTmuxRemotePane, TmuxCmdQueue, TmuxDomainState, TmuxEnqueueError};
use crate::tmux_commands::{KillPane, Resize, SendKeys};
use crate::DomainId;
use filedescriptor::FileDescriptor;
use parking_lot::{Condvar, Mutex};
use portable_pty::{Child, ChildKiller, ExitStatus, MasterPty};
use std::convert::TryFrom;
use std::io::{Read, Write};
use std::sync::{Arc, Weak};
use termwiz::tmux_cc::TmuxPaneId;

const TMUX_WRITE_CHUNK_BYTES: usize = 16 * 1024;

/// A local tmux pane(tab) based on a tmux pty
#[derive(Debug)]
pub(crate) struct TmuxPty {
    pub domain_id: DomainId,
    pub master_pane: RefTmuxRemotePane,
    pub reader: FileDescriptor,
    pub cmd_queue: Arc<Mutex<TmuxCmdQueue>>,
}

fn u64_to_u16_saturating(value: u64) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

struct TmuxPtyWriter {
    domain_id: DomainId,
    master_pane: RefTmuxRemotePane,
    cmd_queue: Arc<Mutex<TmuxCmdQueue>>,
}

impl Write for TmuxPtyWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let pane_id = {
            let pane_lock = self.master_pane.lock();
            pane_lock.pane_id
        };
        let accepted_len = buf.len().min(TMUX_WRITE_CHUNK_BYTES);
        let command = Box::new(SendKeys {
            pane: pane_id,
            keys: buf[..accepted_len].to_vec(),
        });
        log::trace!("pane:{}, content:{:?}", pane_id, &buf[..accepted_len]);
        let enqueue_result = {
            let mut cmd_queue = self.cmd_queue.lock();
            cmd_queue.push_back(command)
        };
        match enqueue_result {
            Ok(()) => {
                TmuxDomainState::schedule_send_next_command(self.domain_id);
                Ok(accepted_len)
            }
            Err(TmuxEnqueueError::Closed) => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                TmuxEnqueueError::Closed,
            )),
            Err(TmuxEnqueueError::Full) => Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                TmuxEnqueueError::Full,
            )),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Write for TmuxPty {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let pane_id = {
            let pane_lock = self.master_pane.lock();
            pane_lock.pane_id
        };
        let accepted_len = buf.len().min(TMUX_WRITE_CHUNK_BYTES);
        let command = Box::new(SendKeys {
            pane: pane_id,
            keys: buf[..accepted_len].to_vec(),
        });
        log::trace!("pane:{}, content:{:?}", pane_id, &buf[..accepted_len]);
        let enqueue_result = {
            let mut cmd_queue = self.cmd_queue.lock();
            cmd_queue.push_back(command)
        };
        match enqueue_result {
            Ok(()) => {
                TmuxDomainState::schedule_send_next_command(self.domain_id);
                Ok(accepted_len)
            }
            Err(TmuxEnqueueError::Closed) => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                TmuxEnqueueError::Closed,
            )),
            Err(TmuxEnqueueError::Full) => Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                TmuxEnqueueError::Full,
            )),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct TmuxChildState {
    exit_status: Mutex<Option<ExitStatus>>,
    exit_condvar: Condvar,
}

impl TmuxChildState {
    pub(crate) fn new() -> Self {
        Self {
            exit_status: Mutex::new(None),
            exit_condvar: Condvar::new(),
        }
    }

    pub(crate) fn try_wait(&self) -> Option<ExitStatus> {
        self.exit_status.lock().clone()
    }

    pub(crate) fn wait(&self) -> ExitStatus {
        let mut exit_status = self.exit_status.lock();
        loop {
            if let Some(status) = exit_status.clone() {
                return status;
            }
            self.exit_condvar.wait(&mut exit_status);
        }
    }

    pub(crate) fn mark_exited(&self, status: ExitStatus) {
        let mut exit_status = self.exit_status.lock();
        if exit_status.is_none() {
            *exit_status = Some(status);
        }
        self.exit_condvar.notify_all();
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TmuxChild {
    child_state: Arc<TmuxChildState>,
    killer: TmuxChildKiller,
}

impl TmuxChild {
    pub(crate) fn new(
        domain_id: DomainId,
        pane_id: TmuxPaneId,
        cmd_queue: Arc<Mutex<TmuxCmdQueue>>,
        child_state: Arc<TmuxChildState>,
        owner: Weak<TmuxDomainState>,
    ) -> Self {
        Self {
            killer: TmuxChildKiller {
                domain_id,
                pane_id,
                cmd_queue,
                child_state: Arc::clone(&child_state),
                owner,
            },
            child_state,
        }
    }
}

impl Child for TmuxChild {
    fn try_wait(&mut self) -> std::io::Result<Option<portable_pty::ExitStatus>> {
        Ok(self.child_state.try_wait())
    }

    fn wait(&mut self) -> std::io::Result<portable_pty::ExitStatus> {
        Ok(self.child_state.wait())
    }

    fn process_id(&self) -> Option<u32> {
        None
    }

    #[cfg(windows)]
    fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
        None
    }
}

#[derive(Clone, Debug)]
struct TmuxChildKiller {
    domain_id: DomainId,
    pane_id: TmuxPaneId,
    cmd_queue: Arc<Mutex<TmuxCmdQueue>>,
    child_state: Arc<TmuxChildState>,
    owner: Weak<TmuxDomainState>,
}

impl ChildKiller for TmuxChildKiller {
    fn kill(&mut self) -> std::io::Result<()> {
        if self.child_state.try_wait().is_some() {
            return Ok(());
        }

        let enqueue_result = {
            let mut cmd_queue = self.cmd_queue.lock();
            cmd_queue.push_back(Box::new(KillPane {
                pane_id: self.pane_id,
            }))
        };
        match enqueue_result {
            Ok(()) => {
                TmuxDomainState::schedule_send_next_command(self.domain_id);
                self.child_state
                    .mark_exited(ExitStatus::with_signal("tmux kill-pane"));
                Ok(())
            }
            Err(TmuxEnqueueError::Closed) => {
                self.child_state
                    .mark_exited(ExitStatus::with_signal("tmux domain detached"));
                Ok(())
            }
            Err(TmuxEnqueueError::Full) => {
                let Some(owner) = self.owner.upgrade() else {
                    self.child_state.mark_exited(ExitStatus::with_signal(
                        "tmux domain unavailable during kill",
                    ));
                    return Ok(());
                };
                log::error!(
                    "kill-pane admission was rejected for tmux domain {}; failing the domain \
                     closed so local removal cannot masquerade as a successful remote kill",
                    self.domain_id
                );
                owner.transition_to_exit_and_schedule_detach();
                self.child_state.mark_exited(ExitStatus::with_signal(
                    "tmux domain fail-closed after kill rejection",
                ));
                Ok(())
            }
        }
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(self.clone())
    }
}

impl ChildKiller for TmuxChild {
    fn kill(&mut self) -> std::io::Result<()> {
        self.killer.kill()
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(self.killer.clone())
    }
}

impl MasterPty for TmuxPty {
    fn resize(&self, size: portable_pty::PtySize) -> Result<(), anyhow::Error> {
        let pane_id = self.master_pane.lock().pane_id;
        let enqueue_result = {
            let mut cmd_queue = self.cmd_queue.lock();
            cmd_queue.push_back(Box::new(Resize { size, pane_id }))
        };
        match enqueue_result {
            Ok(()) => {
                TmuxDomainState::schedule_send_next_command(self.domain_id);
                Ok(())
            }
            Err(err) => Err(anyhow::anyhow!(
                "cannot resize pane in tmux domain {}: {}",
                self.domain_id,
                err
            )),
        }
    }

    fn get_size(&self) -> Result<portable_pty::PtySize, anyhow::Error> {
        let pane = self.master_pane.lock();
        Ok(portable_pty::PtySize {
            rows: u64_to_u16_saturating(pane.pane_height),
            cols: u64_to_u16_saturating(pane.pane_width),
            pixel_width: 0,
            pixel_height: 0,
        })
    }

    fn try_clone_reader(&self) -> Result<Box<dyn Read + Send>, anyhow::Error> {
        Ok(Box::new(self.reader.try_clone()?))
    }

    fn take_writer(&self) -> Result<Box<dyn Write + Send>, anyhow::Error> {
        Ok(Box::new(TmuxPtyWriter {
            domain_id: self.domain_id,
            master_pane: self.master_pane.clone(),
            cmd_queue: self.cmd_queue.clone(),
        }))
    }

    #[cfg(unix)]
    fn process_group_leader(&self) -> Option<libc::pid_t> {
        return None;
    }

    #[cfg(unix)]
    fn as_raw_fd(&self) -> Option<std::os::fd::RawFd> {
        None
    }

    #[cfg(unix)]
    fn tty_name(&self) -> Option<std::path::PathBuf> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::{TmuxDomain, TmuxPaneOutputState, TmuxRemotePane, CMD_QUEUE_MAX_DEPTH};
    use crate::tmux_commands::ListCommands;
    use crate::Mux;
    use promise::spawn::ScopedExecutor;
    use std::sync::{Arc as StdArc, MutexGuard as StdMutexGuard};

    struct ScopedMux {
        prior: Option<StdArc<Mux>>,
        _executor: ScopedExecutor,
        _guard: StdMutexGuard<'static, ()>,
    }

    impl ScopedMux {
        fn install(mux: StdArc<Mux>) -> Self {
            let guard = crate::MUX_TEST_LOCK
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let executor = ScopedExecutor::new();
            let prior = Mux::try_get();
            Mux::set_mux(&mux);
            Self {
                prior,
                _executor: executor,
                _guard: guard,
            }
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

    fn remote_pane(pane_id: TmuxPaneId) -> RefTmuxRemotePane {
        let (_output_read, output_write) =
            filedescriptor::socketpair().expect("tmux test output socketpair");
        Arc::new(Mutex::new(TmuxRemotePane {
            local_pane_id: 1,
            output_write,
            child_state: Arc::new(TmuxChildState::new()),
            session_id: 1,
            window_id: 1,
            pane_id,
            cursor_x: 0,
            cursor_y: 0,
            pane_width: 80,
            pane_height: 24,
            pane_left: 0,
            pane_top: 0,
            output_state: TmuxPaneOutputState::Ready,
        }))
    }

    #[test]
    fn tmux_child_try_wait_is_pending_until_signaled() {
        let child_state = Arc::new(TmuxChildState::new());
        let mut child = TmuxChild::new(
            1,
            42,
            Arc::new(Mutex::new(TmuxCmdQueue::new())),
            child_state.clone(),
            Weak::new(),
        );

        assert!(child.try_wait().expect("try_wait").is_none());

        child_state.mark_exited(ExitStatus::with_exit_code(0));

        let status = child
            .try_wait()
            .expect("try_wait after signal")
            .expect("exit status");
        assert_eq!(status.exit_code(), 0);
    }

    #[test]
    fn tmux_pty_size_conversion_saturates() {
        assert_eq!(u64_to_u16_saturating(24), 24);
        assert_eq!(u64_to_u16_saturating(u64::from(u16::MAX) + 1), u16::MAX);
    }

    #[test]
    fn tmux_child_kill_enqueues_remote_kill_and_marks_signal_status() {
        let _mux = ScopedMux::install(StdArc::new(Mux::new(None)));
        let child_state = Arc::new(TmuxChildState::new());
        let cmd_queue = Arc::new(Mutex::new(TmuxCmdQueue::new()));
        let mut child = TmuxChild::new(7, 99, cmd_queue.clone(), child_state.clone(), Weak::new());

        child.kill().expect("kill");

        let queued = cmd_queue.lock();
        assert_eq!(queued.len(), 1);
        assert_eq!(
            queued.front().expect("queued command").get_command(7),
            "kill-pane -t %99\n"
        );
        drop(queued);

        let status = child_state.try_wait().expect("child marked exited");
        assert_eq!(status.signal(), Some("tmux kill-pane"));
    }

    #[test]
    fn tmux_child_kill_fails_domain_closed_when_mailbox_is_full() {
        let _mux = ScopedMux::install(StdArc::new(Mux::new(None)));
        let domain = TmuxDomain::new(99);
        {
            let mut queue = domain.inner.cmd_queue.lock();
            for _ in 0..CMD_QUEUE_MAX_DEPTH {
                queue
                    .push_back(Box::new(ListCommands))
                    .expect("test setup should fill the mailbox exactly");
            }
        }
        let child_state = Arc::new(TmuxChildState::new());
        let mut child = TmuxChild::new(
            domain.inner.domain_id,
            123,
            Arc::clone(&domain.inner.cmd_queue),
            Arc::clone(&child_state),
            Arc::downgrade(&domain.inner),
        );

        child
            .kill()
            .expect("a full kill mailbox must fail the domain closed");

        assert!(
            domain.inner.cmd_queue.lock().is_closed(),
            "kill rejection must close the owning domain mailbox",
        );
        assert_eq!(
            child_state
                .try_wait()
                .expect("fail-closed kill should terminalize the local child")
                .signal(),
            Some("tmux domain fail-closed after kill rejection"),
        );
    }

    #[test]
    fn tmux_pty_writer_reports_consumed_bytes_and_preserves_send_keys() {
        let _mux = ScopedMux::install(StdArc::new(Mux::new(None)));
        let cmd_queue = Arc::new(Mutex::new(TmuxCmdQueue::new()));
        let mut writer = TmuxPtyWriter {
            domain_id: 7,
            master_pane: remote_pane(99),
            cmd_queue: Arc::clone(&cmd_queue),
        };

        assert_eq!(writer.write(b"ab").expect("accepted tmux write"), 2);
        assert_eq!(writer.write(b"").expect("empty tmux write"), 0);

        let queue = cmd_queue.lock();
        assert_eq!(queue.len(), 1);
        assert_eq!(
            queue.front().expect("send-keys command").get_command(7),
            "send-keys -t %99 0x61 0x62 \r"
        );
    }

    #[test]
    fn tmux_pty_writer_chunks_large_paste_before_mailbox_allocation() {
        let _mux = ScopedMux::install(StdArc::new(Mux::new(None)));
        let cmd_queue = Arc::new(Mutex::new(TmuxCmdQueue::new()));
        let mut writer = TmuxPtyWriter {
            domain_id: 7,
            master_pane: remote_pane(99),
            cmd_queue: Arc::clone(&cmd_queue),
        };
        let paste = vec![b'x'; TMUX_WRITE_CHUNK_BYTES + 1];

        assert_eq!(
            writer.write(&paste).expect("first paste chunk"),
            TMUX_WRITE_CHUNK_BYTES
        );
        let queue = cmd_queue.lock();
        let (_, keys) = queue
            .front()
            .and_then(|command| command.as_send_keys())
            .expect("send-keys payload");
        assert_eq!(keys.len(), TMUX_WRITE_CHUNK_BYTES);
    }

    #[test]
    fn tmux_pty_writer_and_resize_fail_explicitly_after_mailbox_close() {
        let _mux = ScopedMux::install(StdArc::new(Mux::new(None)));
        let cmd_queue = Arc::new(Mutex::new(TmuxCmdQueue::new()));
        let abandoned_commands = { cmd_queue.lock().close() };
        drop(abandoned_commands);
        let master_pane = remote_pane(100);
        let (reader, _writer) = filedescriptor::socketpair().expect("tmux test pty socketpair");
        let mut pty = TmuxPty {
            domain_id: 8,
            master_pane,
            reader,
            cmd_queue,
        };

        let write_err = pty.write(b"x").expect_err("closed write must fail");
        assert_eq!(write_err.kind(), std::io::ErrorKind::BrokenPipe);
        let resize_err = pty
            .resize(portable_pty::PtySize {
                rows: 40,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect_err("closed resize must fail");
        assert!(resize_err.to_string().contains("closed"));
    }
}
