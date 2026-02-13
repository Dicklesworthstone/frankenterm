#[cfg(unix)]
use libc::{mode_t, umask};
#[cfg(unix)]
use std::sync::Mutex;

#[cfg(unix)]
lazy_static::lazy_static! {
static ref SAVED_UMASK: Mutex<Option<libc::mode_t>> = Mutex::new(None);
}

/// Unfortunately, novice unix users can sometimes be running
/// with an overly permissive umask so we take care to install
/// a more restrictive mask while we might be creating things
/// in the filesystem.
/// This struct locks down the umask for its lifetime, restoring
/// the prior umask when it is dropped.
pub struct UmaskSaver {
    #[cfg(unix)]
    mask: mode_t,
}

impl UmaskSaver {
    pub fn new() -> Self {
        let me = Self {
            #[cfg(unix)]
            mask: unsafe { umask(0o077) },
        };

        #[cfg(unix)]
        {
            SAVED_UMASK.lock().unwrap().replace(me.mask);
        }

        me
    }

    /// Retrieves the mask saved by a UmaskSaver, without
    /// having a reference to the UmaskSaver.
    /// This is only meaningful if a single UmaskSaver is
    /// used in a program.
    #[cfg(unix)]
    pub fn saved_umask() -> Option<mode_t> {
        *SAVED_UMASK.lock().unwrap()
    }
}

impl Default for UmaskSaver {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for UmaskSaver {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            umask(self.mask);
            SAVED_UMASK.lock().unwrap().take();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // UmaskSaver mutates process-global state, so tests must be serialized.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[cfg(unix)]
    #[test]
    fn umask_saver_new_sets_restrictive_mask() {
        let _g = TEST_LOCK.lock().unwrap();
        // Get current umask
        let before = unsafe { umask(0o022) };
        unsafe { umask(before) };

        let saver = UmaskSaver::new();
        // While saver is alive, umask should be 0o077
        let current = unsafe { umask(0o077) };
        assert_eq!(current, 0o077);
        unsafe { umask(current) };
        drop(saver);
    }

    #[cfg(unix)]
    #[test]
    fn umask_saver_restores_on_drop() {
        let _g = TEST_LOCK.lock().unwrap();
        // Set a known umask
        let original = unsafe { umask(0o022) };

        {
            let _saver = UmaskSaver::new();
            // umask is now 0o077
        }
        // After drop, should be restored to 0o022
        let restored = unsafe { umask(original) };
        assert_eq!(restored, 0o022);
        // Restore the truly original umask
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn saved_umask_returns_some_while_active() {
        let _g = TEST_LOCK.lock().unwrap();
        let _saver = UmaskSaver::new();
        let saved = UmaskSaver::saved_umask();
        assert!(saved.is_some());
    }

    #[cfg(unix)]
    #[test]
    fn saved_umask_returns_none_after_drop() {
        let _g = TEST_LOCK.lock().unwrap();
        {
            let _saver = UmaskSaver::new();
        }
        let saved = UmaskSaver::saved_umask();
        assert!(saved.is_none());
    }

    #[test]
    fn default_creates_instance() {
        let _g = TEST_LOCK.lock().unwrap();
        let _saver = UmaskSaver::default();
    }

    #[test]
    fn new_creates_instance() {
        let _g = TEST_LOCK.lock().unwrap();
        let _saver = UmaskSaver::new();
    }

    #[cfg(unix)]
    #[test]
    fn nested_savers_restore_correctly() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o022) };

        {
            let _outer = UmaskSaver::new();
            // Inner saver captures the 0o077 set by outer
            {
                let _inner = UmaskSaver::new();
                // Both active, saved_umask reflects inner's captured value
            }
            // Inner dropped, should restore to 0o077
            let after_inner = unsafe { umask(0o077) };
            assert_eq!(after_inner, 0o077);
            unsafe { umask(after_inner) };
        }
        // Outer dropped, should restore to 0o022
        let after_outer = unsafe { umask(original) };
        assert_eq!(after_outer, 0o022);
        unsafe { umask(original) };
    }
}
