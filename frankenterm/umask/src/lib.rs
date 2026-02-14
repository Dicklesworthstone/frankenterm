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

    #[cfg(unix)]
    #[test]
    fn saved_umask_returns_none_before_any_saver() {
        let _g = TEST_LOCK.lock().unwrap();
        // Ensure no saver is active by dropping any leftovers
        SAVED_UMASK.lock().unwrap().take();
        assert!(UmaskSaver::saved_umask().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn saved_umask_captures_prior_mask_value() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o012) };
        let _saver = UmaskSaver::new();
        // saved_umask should return 0o012 (the mask that was active before new())
        assert_eq!(UmaskSaver::saved_umask(), Some(0o012));
        drop(_saver);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn saver_sets_exactly_077() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o022) };
        let _saver = UmaskSaver::new();
        // Query the current umask by setting and restoring
        let current = unsafe { umask(0o077) };
        unsafe { umask(current) };
        assert_eq!(current, 0o077);
        drop(_saver);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn drop_restores_from_zero_initial_mask() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o000) };
        {
            let _saver = UmaskSaver::new();
            // umask is now 0o077
        }
        let restored = unsafe { umask(original) };
        assert_eq!(restored, 0o000);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn drop_restores_from_maximally_restrictive_mask() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o777) };
        {
            let _saver = UmaskSaver::new();
        }
        let restored = unsafe { umask(original) };
        assert_eq!(restored, 0o777);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn sequential_create_drop_cycles_are_independent() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o022) };

        for _ in 0..5 {
            {
                let _saver = UmaskSaver::new();
                assert!(UmaskSaver::saved_umask().is_some());
            }
            let after = unsafe { umask(0o022) };
            assert_eq!(after, 0o022);
            assert!(UmaskSaver::saved_umask().is_none());
        }

        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn default_and_new_set_same_mask() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o022) };

        let saver_new = UmaskSaver::new();
        let mask_from_new = unsafe { umask(0o077) };
        unsafe { umask(mask_from_new) };
        drop(saver_new);

        unsafe { umask(0o022) };
        let saver_default = UmaskSaver::default();
        let mask_from_default = unsafe { umask(0o077) };
        unsafe { umask(mask_from_default) };
        drop(saver_default);

        assert_eq!(mask_from_new, mask_from_default);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn saved_umask_updates_on_nested_creation() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o022) };

        let _outer = UmaskSaver::new();
        // outer captured 0o022
        assert_eq!(UmaskSaver::saved_umask(), Some(0o022));

        let _inner = UmaskSaver::new();
        // inner captured 0o077 (set by outer)
        assert_eq!(UmaskSaver::saved_umask(), Some(0o077));

        drop(_inner);
        // After inner drop, saved_umask is cleared (taken by inner's drop)
        // Note: inner's drop calls take(), so saved_umask is None
        // Then outer still holds its mask field
        drop(_outer);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn saver_with_already_restrictive_mask() {
        let _g = TEST_LOCK.lock().unwrap();
        // Start with 0o077 — same as what UmaskSaver sets
        let original = unsafe { umask(0o077) };
        {
            let _saver = UmaskSaver::new();
            // Should still be 0o077
            let current = unsafe { umask(0o077) };
            assert_eq!(current, 0o077);
            unsafe { umask(current) };
        }
        // Restore to 0o077
        let restored = unsafe { umask(original) };
        assert_eq!(restored, 0o077);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn saved_umask_with_various_initial_values() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o022) };

        for &mask in &[0o000, 0o022, 0o077, 0o133, 0o777] {
            unsafe { umask(mask) };
            let _saver = UmaskSaver::new();
            assert_eq!(
                UmaskSaver::saved_umask(),
                Some(mask),
                "saved_umask should capture initial mask 0o{:03o}",
                mask
            );
            drop(_saver);
        }

        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn drop_restores_odd_mask_values() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o022) };

        for &mask in &[0o001, 0o010, 0o100, 0o137, 0o755] {
            unsafe { umask(mask) };
            {
                let _saver = UmaskSaver::new();
            }
            let restored = unsafe { umask(mask) };
            assert_eq!(
                restored, mask,
                "drop should restore mask 0o{:03o}",
                mask
            );
        }

        unsafe { umask(original) };
    }

    #[cfg(not(unix))]
    #[test]
    fn non_unix_new_does_not_panic() {
        let _g = TEST_LOCK.lock().unwrap();
        let _saver = UmaskSaver::new();
        // On non-unix, UmaskSaver is essentially a no-op
    }

    #[cfg(not(unix))]
    #[test]
    fn non_unix_default_does_not_panic() {
        let _g = TEST_LOCK.lock().unwrap();
        let _saver = UmaskSaver::default();
    }

    #[cfg(not(unix))]
    #[test]
    fn non_unix_drop_does_not_panic() {
        let _g = TEST_LOCK.lock().unwrap();
        let saver = UmaskSaver::new();
        drop(saver);
    }
}
