use std::io::Error as IoError;
use std::ptr::{null, null_mut};
use winapi::um::handleapi::CloseHandle;
use winapi::um::synchapi::{CreateEventW, ResetEvent, SetEvent};
use winapi::um::winnt::HANDLE;

pub struct EventHandle(pub HANDLE);
unsafe impl Send for EventHandle {}
unsafe impl Sync for EventHandle {}

impl Drop for EventHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

impl EventHandle {
    pub fn new_manual_reset() -> anyhow::Result<Self> {
        let handle = unsafe { CreateEventW(null_mut(), 1, 0, null()) };
        if handle.is_null() {
            return Err(IoError::last_os_error().into());
        }
        Ok(Self(handle))
    }

    pub fn set_event(&self) -> std::io::Result<()> {
        if unsafe { SetEvent(self.0) } == 0 {
            Err(IoError::last_os_error())
        } else {
            Ok(())
        }
    }

    pub fn reset_event(&self) -> std::io::Result<()> {
        if unsafe { ResetEvent(self.0) } == 0 {
            Err(IoError::last_os_error())
        } else {
            Ok(())
        }
    }
}
