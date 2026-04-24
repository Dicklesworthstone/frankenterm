use crate::tmux::{RefTmuxRemotePane, TmuxCmdQueue, TmuxDomainState};
use crate::tmux_commands::{KillPane, Resize, SendKeys};
use crate::DomainId;
use filedescriptor::FileDescriptor;
use parking_lot::{Condvar, Mutex};
use portable_pty::{Child, ChildKiller, ExitStatus, MasterPty};
use std::io::{Read, Write};
use std::sync::Arc;
use termwiz::tmux_cc::TmuxPaneId;

/// A local tmux pane(tab) based on a tmux pty
#[derive(Debug)]
pub(crate) struct TmuxPty {
    pub domain_id: DomainId,
    pub master_pane: RefTmuxRemotePane,
    pub reader: FileDescriptor,
    pub cmd_queue: Arc<Mutex<TmuxCmdQueue>>,
}

struct TmuxPtyWriter {
    domain_id: DomainId,
    master_pane: RefTmuxRemotePane,
    cmd_queue: Arc<Mutex<TmuxCmdQueue>>,
}

impl Write for TmuxPtyWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let pane_id = {
            let pane_lock = self.master_pane.lock();
            pane_lock.pane_id
        };
        log::trace!("pane:{}, content:{:?}", &pane_id, buf);
        let mut cmd_queue = self.cmd_queue.lock();
        cmd_queue.push_back(Box::new(SendKeys {
            pane: pane_id,
            keys: buf.to_vec(),
        }));
        TmuxDomainState::schedule_send_next_command(self.domain_id);
        Ok(0)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Write for TmuxPty {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let pane_id = {
            let pane_lock = self.master_pane.lock();
            pane_lock.pane_id
        };
        log::trace!("pane:{}, content:{:?}", &pane_id, buf);
        let mut cmd_queue = self.cmd_queue.lock();
        cmd_queue.push_back(Box::new(SendKeys {
            pane: pane_id,
            keys: buf.to_vec(),
        }));
        TmuxDomainState::schedule_send_next_command(self.domain_id);
        Ok(0)
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
    ) -> Self {
        Self {
            killer: TmuxChildKiller {
                domain_id,
                pane_id,
                cmd_queue,
                child_state: Arc::clone(&child_state),
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
}

impl ChildKiller for TmuxChildKiller {
    fn kill(&mut self) -> std::io::Result<()> {
        if self.child_state.try_wait().is_some() {
            return Ok(());
        }

        let mut cmd_queue = self.cmd_queue.lock();
        cmd_queue.push_back(Box::new(KillPane {
            pane_id: self.pane_id,
        }));
        drop(cmd_queue);

        TmuxDomainState::schedule_send_next_command(self.domain_id);
        self.child_state
            .mark_exited(ExitStatus::with_signal("tmux kill-pane"));
        Ok(())
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
        let mut cmd_queue = self.cmd_queue.lock();
        cmd_queue.push_back(Box::new(Resize {
            size,
            pane_id: self.master_pane.lock().pane_id,
        }));
        TmuxDomainState::schedule_send_next_command(self.domain_id);
        Ok(())
    }

    fn get_size(&self) -> Result<portable_pty::PtySize, anyhow::Error> {
        let pane = self.master_pane.lock();
        Ok(portable_pty::PtySize {
            rows: pane.pane_height as u16,
            cols: pane.pane_width as u16,
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
    use crate::Mux;
    use promise::spawn::ScopedExecutor;
    use std::sync::{Arc as StdArc, Mutex as StdMutex, MutexGuard as StdMutexGuard};

    static MUX_TEST_LOCK: StdMutex<()> = StdMutex::new(());

    struct ScopedMux {
        prior: Option<StdArc<Mux>>,
        _executor: ScopedExecutor,
        _guard: StdMutexGuard<'static, ()>,
    }

    impl ScopedMux {
        fn install(mux: StdArc<Mux>) -> Self {
            let guard = MUX_TEST_LOCK.lock().unwrap_or_else(|err| err.into_inner());
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

    #[test]
    fn tmux_child_try_wait_is_pending_until_signaled() {
        let child_state = Arc::new(TmuxChildState::new());
        let mut child = TmuxChild::new(
            1,
            42,
            Arc::new(Mutex::new(TmuxCmdQueue::new())),
            child_state.clone(),
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
    fn tmux_child_kill_enqueues_remote_kill_and_marks_signal_status() {
        let _mux = ScopedMux::install(StdArc::new(Mux::new(None)));
        let child_state = Arc::new(TmuxChildState::new());
        let cmd_queue = Arc::new(Mutex::new(TmuxCmdQueue::new()));
        let mut child = TmuxChild::new(7, 99, cmd_queue.clone(), child_state.clone());

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
}
