//! Values the harness computes on their own threads while the lanes run.
//!
//! hyperfine and `vsghostty.py` time the whole process, and ghostty-bench
//! reads its input without hashing it or spawning anything. Hashing a
//! `--from-file` corpus on the ingest thread cost a third of `long_lines`'s
//! ingest time, and spawning `git` and `sysctl` there a few milliseconds a
//! run.

use std::sync::{Mutex, OnceLock};
use std::thread::{self, JoinHandle};

/// A value that is known, or being computed on its own thread.
pub struct Deferred<T> {
    value: OnceLock<T>,
    pending: Mutex<Option<JoinHandle<T>>>,
}

impl<T: Send + 'static> Deferred<T> {
    pub fn ready(value: T) -> Self {
        Self {
            value: OnceLock::from(value),
            pending: Mutex::new(None),
        }
    }

    pub fn spawn(compute: impl FnOnce() -> T + Send + 'static) -> Self {
        Self {
            value: OnceLock::new(),
            pending: Mutex::new(Some(thread::spawn(compute))),
        }
    }

    /// The value, waiting for its thread the first time.
    pub fn get(&self) -> &T {
        self.value.get_or_init(|| {
            self.pending
                .lock()
                .expect("a deferred value's lock is never poisoned")
                .take()
                .expect("a value not yet known is being computed")
                .join()
                .expect("computing a deferred value does not panic")
        })
    }
}
