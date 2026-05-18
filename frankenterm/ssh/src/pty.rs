use crate::runtime::channel::{Receiver, TryRecvError, bounded};
use crate::session::{SessionRequest, SessionSender, SignalChannel};
use crate::sessioninner::{ChannelId, ChannelInfo, DescriptorState};
use crate::sessionwrap::SessionWrap;
use filedescriptor::{FileDescriptor, socketpair};
use portable_pty::{ExitStatus, PtySize};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::{Mutex, MutexGuard};

fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            mutex.clear_poison();
            poisoned.into_inner()
        }
    }
}

#[derive(Debug)]
pub(crate) struct NewPty {
    pub term: String,
    pub size: PtySize,
    pub command_line: Option<String>,
    pub env: Option<HashMap<String, String>>,
}

#[derive(Debug)]
pub(crate) struct ResizePty {
    pub channel: ChannelId,
    pub size: PtySize,
}

#[derive(Debug)]
pub struct SshPty {
    pub(crate) channel: ChannelId,
    pub(crate) tx: Option<SessionSender>,
    pub(crate) reader: FileDescriptor,
    pub(crate) writer: FileDescriptor,
    pub(crate) size: Mutex<PtySize>,
}

impl std::io::Write for SshPty {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.writer.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

impl portable_pty::MasterPty for SshPty {
    fn resize(&self, size: PtySize) -> anyhow::Result<()> {
        let tx = self
            .tx
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("ssh pty session sender is unavailable"))?;
        tx.try_send(SessionRequest::ResizePty(
            ResizePty {
                channel: self.channel,
                size,
            },
            None,
        ))?;

        *lock_or_recover(&self.size) = size;
        Ok(())
    }

    fn get_size(&self) -> anyhow::Result<PtySize> {
        Ok(*lock_or_recover(&self.size))
    }

    fn try_clone_reader(&self) -> anyhow::Result<Box<dyn Read + Send + 'static>> {
        let reader = self.reader.try_clone()?;
        Ok(Box::new(reader))
    }

    fn take_writer(&self) -> anyhow::Result<Box<dyn Write + Send + 'static>> {
        let writer = self.writer.try_clone()?;
        Ok(Box::new(writer))
    }

    #[cfg(unix)]
    fn process_group_leader(&self) -> Option<i32> {
        // It's not local, so there's no meaningful leader
        None
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

#[derive(Debug)]
pub struct SshChildProcess {
    pub(crate) channel: ChannelId,
    pub(crate) tx: Option<SessionSender>,
    pub(crate) exit: Receiver<ExitStatus>,
    pub(crate) exited: Option<ExitStatus>,
}

impl SshChildProcess {
    pub async fn async_wait(&mut self) -> std::io::Result<ExitStatus> {
        if let Some(status) = self.exited.as_ref() {
            return Ok(status.clone());
        }
        match self.exit.recv().await {
            Ok(status) => {
                self.exited.replace(status.clone());
                Ok(status)
            }
            Err(_) => {
                let status = ExitStatus::with_exit_code(1);
                self.exited.replace(status.clone());
                Ok(status)
            }
        }
    }
}

impl portable_pty::Child for SshChildProcess {
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        if let Some(status) = self.exited.as_ref() {
            return Ok(Some(status.clone()));
        }
        match self.exit.try_recv() {
            Ok(status) => {
                self.exited.replace(status.clone());
                Ok(Some(status))
            }
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Closed) => {
                let status = ExitStatus::with_exit_code(1);
                self.exited.replace(status.clone());
                Ok(Some(status))
            }
        }
    }

    fn wait(&mut self) -> std::io::Result<portable_pty::ExitStatus> {
        if let Some(status) = self.exited.as_ref() {
            return Ok(status.clone());
        }
        match crate::runtime::block_on(self.exit.recv()) {
            Ok(status) => {
                self.exited.replace(status.clone());
                Ok(status)
            }
            Err(_) => {
                let status = ExitStatus::with_exit_code(1);
                self.exited.replace(status.clone());
                Ok(status)
            }
        }
    }

    fn process_id(&self) -> Option<u32> {
        None
    }

    #[cfg(windows)]
    fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
        None
    }
}

impl portable_pty::ChildKiller for SshChildProcess {
    fn kill(&mut self) -> std::io::Result<()> {
        if let Some(tx) = self.tx.as_ref() {
            tx.try_send(SessionRequest::SignalChannel(SignalChannel {
                channel: self.channel,
                signame: "HUP",
            }))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        }
        Ok(())
    }

    fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
        Box::new(SshChildKiller {
            tx: self.tx.clone(),
            channel: self.channel,
        })
    }
}

#[derive(Debug, Clone)]
struct SshChildKiller {
    pub(crate) tx: Option<SessionSender>,
    pub(crate) channel: ChannelId,
}

impl portable_pty::ChildKiller for SshChildKiller {
    fn kill(&mut self) -> std::io::Result<()> {
        if let Some(tx) = self.tx.as_ref() {
            tx.try_send(SessionRequest::SignalChannel(SignalChannel {
                channel: self.channel,
                signame: "HUP",
            }))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        }
        Ok(())
    }

    fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
        Box::new(SshChildKiller {
            tx: self.tx.clone(),
            channel: self.channel,
        })
    }
}

impl crate::sessioninner::SessionInner {
    pub fn new_pty(
        &mut self,
        sess: &mut SessionWrap,
        newpty: NewPty,
    ) -> anyhow::Result<(SshPty, SshChildProcess)> {
        sess.set_blocking(true);

        let mut channel = sess.open_session()?;

        if let Some("yes") = self.config.get("forwardagent").map(|s| s.as_str()) {
            if self.identity_agent().is_some() {
                if let Err(err) = channel.request_auth_agent_forwarding() {
                    log::error!("Failed to request agent forwarding: {:#}", err);
                }
            }
        }

        channel.request_pty(&newpty)?;

        if let Some(env) = &newpty.env {
            for (key, val) in env {
                if let Err(err) = channel.request_env(key, val) {
                    // Depending on the server configuration, a given
                    // setenv request may not succeed, but that doesn't
                    // prevent the connection from being set up.
                    if !self.shown_accept_env_error {
                        log::warn!(
                            "ssh: setenv {}={} failed: {}. \
                            Check the AcceptEnv setting on the ssh server side. \
                            Additional errors with setting env vars in this \
                            session will be logged at debug log level.",
                            key,
                            val,
                            err
                        );
                        self.shown_accept_env_error = true;
                    } else {
                        log::debug!(
                            "ssh: setenv {}={} failed: {}. \
                             Check the AcceptEnv setting on the ssh server side.",
                            key,
                            val,
                            err
                        );
                    }
                }
            }
        }

        if let Some(cmd) = &newpty.command_line {
            channel.request_exec(cmd)?;
        } else {
            channel.request_shell()?;
        }

        let channel_id = self.next_channel_id;
        self.next_channel_id += 1;

        let (write_to_stdin, mut read_from_stdin) = socketpair()?;
        let (mut write_to_stdout, read_from_stdout) = socketpair()?;
        let write_to_stderr = write_to_stdout.try_clone()?;

        read_from_stdin.set_non_blocking(true)?;
        write_to_stdout.set_non_blocking(true)?;

        let ssh_pty = SshPty {
            channel: channel_id,
            tx: None,
            reader: read_from_stdout,
            writer: write_to_stdin,
            size: Mutex::new(newpty.size),
        };

        let (exit_tx, exit_rx) = bounded(1);

        let child = SshChildProcess {
            channel: channel_id,
            tx: None,
            exit: exit_rx,
            exited: None,
        };

        let info = ChannelInfo {
            channel_id,
            channel,
            exit: Some(exit_tx),
            exited: false,
            descriptors: [
                DescriptorState {
                    fd: Some(read_from_stdin),
                    buf: VecDeque::with_capacity(8192),
                },
                DescriptorState {
                    fd: Some(write_to_stdout),
                    buf: VecDeque::with_capacity(8192),
                },
                DescriptorState {
                    fd: Some(write_to_stderr),
                    buf: VecDeque::with_capacity(8192),
                },
            ],
        };

        self.channels.insert(channel_id, info);

        Ok((ssh_pty, child))
    }

    pub fn resize_pty(&mut self, resize: ResizePty) -> anyhow::Result<()> {
        let info = self
            .channels
            .get_mut(&resize.channel)
            .ok_or_else(|| anyhow::anyhow!("invalid channel id {}", resize.channel))?;
        info.channel.resize_pty(&resize)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use portable_pty::{Child, ChildKiller, MasterPty};
    use std::io::Read;
    use std::sync::{Arc, Mutex};

    fn test_session_sender() -> (SessionSender, Receiver<SessionRequest>, FileDescriptor) {
        let (tx, rx) = bounded(1);
        let (writer, reader) = socketpair().expect("socketpair failed");
        (
            SessionSender {
                tx,
                pipe: Arc::new(Mutex::new(writer)),
            },
            rx,
            reader,
        )
    }

    fn assert_pipe_wakeup(mut reader: FileDescriptor) {
        let mut wake = [0u8; 1];
        reader
            .read_exact(&mut wake)
            .expect("failed to read wake byte");
        assert_eq!(wake, [b'x']);
    }

    fn test_pty(tx: Option<SessionSender>, size: PtySize) -> SshPty {
        let (reader, writer) = socketpair().expect("socketpair failed");
        SshPty {
            channel: 17,
            tx,
            reader,
            writer,
            size: Mutex::new(size),
        }
    }

    #[test]
    fn ssh_pty_resize_without_sender_returns_error() {
        let initial = PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
        };
        let resized = PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 1200,
            pixel_height: 900,
        };
        let pty = test_pty(None, initial);

        let err = pty.resize(resized).expect_err("resize should fail");

        assert!(err.to_string().contains("session sender is unavailable"));
        assert_eq!(
            pty.get_size().expect("size should remain readable"),
            initial
        );
    }

    #[test]
    fn ssh_pty_size_recovers_poisoned_lock() {
        let (sender, rx, _reader) = test_session_sender();
        let initial = PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
        };
        let resized = PtySize {
            rows: 41,
            cols: 121,
            pixel_width: 1210,
            pixel_height: 910,
        };
        let pty = test_pty(Some(sender), initial);

        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = pty.size.lock().unwrap();
            panic!("poison size");
        }));

        assert!(pty.size.is_poisoned());
        assert_eq!(
            pty.get_size().expect("poisoned size should recover"),
            initial
        );
        assert!(!pty.size.is_poisoned());
        pty.resize(resized).expect("resize should recover lock");
        assert_eq!(pty.get_size().expect("resized size"), resized);

        match rx.try_recv().expect("resize request missing") {
            SessionRequest::ResizePty(resize, None) => {
                assert_eq!(resize.channel, 17);
                assert_eq!(resize.size, resized);
            }
            other => panic!("expected ResizePty request, got {:?}", other),
        }
    }

    #[test]
    fn ssh_child_wait_closed_channel_returns_failure_status_and_caches_it() {
        let (exit_tx, exit_rx) = bounded(1);
        drop(exit_tx);

        let mut child = SshChildProcess {
            channel: 7,
            tx: None,
            exit: exit_rx,
            exited: None,
        };

        let status = child.wait().expect("wait failed");
        assert_eq!(status.exit_code(), 1);
        assert_eq!(child.exited.as_ref().map(ExitStatus::exit_code), Some(1));
        assert_eq!(
            child
                .try_wait()
                .expect("try_wait failed")
                .expect("cached exit status missing")
                .exit_code(),
            1
        );
    }

    #[test]
    fn ssh_child_async_wait_closed_channel_returns_failure_status_and_caches_it() {
        let (exit_tx, exit_rx) = bounded(1);
        drop(exit_tx);

        let mut child = SshChildProcess {
            channel: 9,
            tx: None,
            exit: exit_rx,
            exited: None,
        };

        let status = crate::runtime::block_on(child.async_wait()).expect("async_wait failed");
        assert_eq!(status.exit_code(), 1);
        assert_eq!(child.exited.as_ref().map(ExitStatus::exit_code), Some(1));
    }

    #[test]
    fn ssh_child_kill_posts_hup_signal_and_wakes_pipe() {
        let (sender, rx, reader) = test_session_sender();
        let (_exit_tx, exit_rx) = bounded(1);
        let mut child = SshChildProcess {
            channel: 11,
            tx: Some(sender),
            exit: exit_rx,
            exited: None,
        };

        child.kill().expect("kill failed");

        match rx.try_recv().expect("signal request missing") {
            SessionRequest::SignalChannel(signal) => {
                assert_eq!(signal.channel, 11);
                assert_eq!(signal.signame, "HUP");
            }
            other => panic!("expected SignalChannel request, got {:?}", other),
        }
        assert_pipe_wakeup(reader);
    }
}
