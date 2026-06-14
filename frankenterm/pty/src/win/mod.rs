use crate::{Child, ChildKiller, ExitStatus};
use anyhow::Context as _;
use std::io::{Error as IoError, ErrorKind, Result as IoResult};
use std::os::windows::io::AsRawHandle;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use winapi::shared::minwindef::DWORD;
use winapi::um::minwinbase::STILL_ACTIVE;
use winapi::um::processthreadsapi::*;
use winapi::um::synchapi::WaitForSingleObject;
use winapi::um::winbase::INFINITE;

pub mod conpty;
mod procthreadattr;
mod psuedocon;

use filedescriptor::OwnedHandle;

fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| {
        mutex.clear_poison();
        poisoned.into_inner()
    })
}

#[derive(Clone, Copy, Debug)]
struct HandleCloneError {
    kind: ErrorKind,
    raw_os_error: Option<i32>,
}

impl HandleCloneError {
    fn into_io_error(self) -> IoError {
        if let Some(code) = self.raw_os_error {
            IoError::from_raw_os_error(code)
        } else {
            IoError::new(self.kind, "failed to clone Windows process handle")
        }
    }

    fn to_io_error(&self) -> IoError {
        (*self).into_io_error()
    }
}

impl From<IoError> for HandleCloneError {
    fn from(error: IoError) -> Self {
        Self {
            kind: error.kind(),
            raw_os_error: error.raw_os_error(),
        }
    }
}

impl From<HandleCloneError> for IoError {
    fn from(error: HandleCloneError) -> Self {
        error.into_io_error()
    }
}

fn clone_handle(handle: &OwnedHandle) -> Result<OwnedHandle, HandleCloneError> {
    handle.try_clone().map_err(HandleCloneError::from)
}

#[derive(Debug)]
pub struct WinChild {
    proc: Mutex<OwnedHandle>,
    waiter: Mutex<Option<Arc<Mutex<Option<Waker>>>>>,
}

impl WinChild {
    fn is_complete(&mut self) -> IoResult<Option<ExitStatus>> {
        let mut status: DWORD = 0;
        let proc = clone_handle(&lock_or_recover(&self.proc))?;
        let res = unsafe { GetExitCodeProcess(proc.as_raw_handle() as _, &mut status) };
        if res != 0 {
            if status == STILL_ACTIVE {
                Ok(None)
            } else {
                Ok(Some(ExitStatus::with_exit_code(status)))
            }
        } else {
            Ok(None)
        }
    }

    fn do_kill(&mut self) -> IoResult<()> {
        let proc = clone_handle(&lock_or_recover(&self.proc))?;
        let res = unsafe { TerminateProcess(proc.as_raw_handle() as _, 1) };
        // TerminateProcess returns non-zero (TRUE) on success
        if res == 0 {
            Err(IoError::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl ChildKiller for WinChild {
    fn kill(&mut self) -> IoResult<()> {
        self.do_kill()
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        let proc = clone_handle(&lock_or_recover(&self.proc));
        Box::new(WinChildKiller { proc })
    }
}

#[derive(Debug)]
pub struct WinChildKiller {
    proc: Result<OwnedHandle, HandleCloneError>,
}

impl ChildKiller for WinChildKiller {
    fn kill(&mut self) -> IoResult<()> {
        let proc = self.proc.as_ref().map_err(HandleCloneError::to_io_error)?;
        let res = unsafe { TerminateProcess(proc.as_raw_handle() as _, 1) };
        // TerminateProcess returns non-zero (TRUE) on success
        if res == 0 {
            Err(IoError::last_os_error())
        } else {
            Ok(())
        }
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        let proc = self
            .proc
            .as_ref()
            .map_or_else(|error| Err(*error), clone_handle);
        Box::new(WinChildKiller { proc })
    }
}

impl Child for WinChild {
    fn try_wait(&mut self) -> IoResult<Option<ExitStatus>> {
        self.is_complete()
    }

    fn wait(&mut self) -> IoResult<ExitStatus> {
        if let Ok(Some(status)) = self.try_wait() {
            return Ok(status);
        }
        let proc = clone_handle(&lock_or_recover(&self.proc))?;
        unsafe {
            WaitForSingleObject(proc.as_raw_handle() as _, INFINITE);
        }
        let mut status: DWORD = 0;
        let res = unsafe { GetExitCodeProcess(proc.as_raw_handle() as _, &mut status) };
        if res != 0 {
            Ok(ExitStatus::with_exit_code(status))
        } else {
            Err(IoError::last_os_error())
        }
    }

    fn process_id(&self) -> Option<u32> {
        let res = unsafe { GetProcessId(lock_or_recover(&self.proc).as_raw_handle() as _) };
        if res == 0 { None } else { Some(res) }
    }

    fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
        let proc = lock_or_recover(&self.proc);
        Some(proc.as_raw_handle())
    }
}

impl std::future::Future for WinChild {
    type Output = anyhow::Result<ExitStatus>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<anyhow::Result<ExitStatus>> {
        match self.is_complete() {
            Ok(Some(status)) => Poll::Ready(Ok(status)),
            Err(err) => Poll::Ready(Err(err).context("Failed to retrieve process exit status")),
            Ok(None) => {
                struct PassHandleToWaiterThread(pub OwnedHandle);
                // SAFETY: Windows process handles are process-wide values; this
                // wrapper moves a cloned handle to a waiter thread that only waits.
                unsafe impl Send for PassHandleToWaiterThread {}

                let spawn_error = {
                    let mut waiter = lock_or_recover(&self.waiter);
                    if let Some(waker_slot) = waiter.as_ref() {
                        *lock_or_recover(waker_slot.as_ref()) = Some(cx.waker().clone());
                        None
                    } else {
                        let handle =
                            PassHandleToWaiterThread(lock_or_recover(&self.proc).try_clone()?);
                        let waker_slot = Arc::new(Mutex::new(Some(cx.waker().clone())));
                        let waiter_waker_slot = Arc::clone(&waker_slot);
                        let spawn_result = std::thread::Builder::new()
                            .name("pty-win-child-wait".to_string())
                            .spawn(move || {
                                unsafe {
                                    WaitForSingleObject(handle.0.as_raw_handle() as _, INFINITE);
                                }
                                let waker = lock_or_recover(waiter_waker_slot.as_ref()).take();
                                if let Some(waker) = waker {
                                    waker.wake();
                                }
                            });
                        match spawn_result {
                            Ok(_) => {
                                *waiter = Some(waker_slot);
                                None
                            }
                            Err(err) => Some(err),
                        }
                    }
                };
                if let Some(err) = spawn_error {
                    return Poll::Ready(Err(anyhow::Error::new(err)
                        .context("Failed to spawn Windows process waiter thread")));
                }
                Poll::Pending
            }
        }
    }
}
