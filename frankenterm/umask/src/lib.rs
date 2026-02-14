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

    #[cfg(unix)]
    #[test]
    fn individual_permission_bits_owner_read() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o022) };
        unsafe { umask(0o400) }; // owner read denied
        {
            let _saver = UmaskSaver::new();
            assert_eq!(UmaskSaver::saved_umask(), Some(0o400));
        }
        let restored = unsafe { umask(original) };
        assert_eq!(restored, 0o400);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn individual_permission_bits_group_write() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o022) };
        unsafe { umask(0o020) }; // group write denied
        {
            let _saver = UmaskSaver::new();
            assert_eq!(UmaskSaver::saved_umask(), Some(0o020));
        }
        let restored = unsafe { umask(original) };
        assert_eq!(restored, 0o020);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn individual_permission_bits_other_execute() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o022) };
        unsafe { umask(0o001) }; // other execute denied
        {
            let _saver = UmaskSaver::new();
            assert_eq!(UmaskSaver::saved_umask(), Some(0o001));
        }
        let restored = unsafe { umask(original) };
        assert_eq!(restored, 0o001);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn rapid_create_drop_with_changing_masks() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o022) };

        let masks = [0o000, 0o111, 0o222, 0o333, 0o444, 0o555, 0o666, 0o777];
        for &mask in &masks {
            unsafe { umask(mask) };
            let _saver = UmaskSaver::new();
            assert_eq!(UmaskSaver::saved_umask(), Some(mask));
            drop(_saver);
            // After drop, should restore to mask
            let restored = unsafe { umask(mask) };
            assert_eq!(restored, mask);
        }

        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn saved_umask_is_none_between_savers() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o022) };

        SAVED_UMASK.lock().unwrap().take();
        assert!(UmaskSaver::saved_umask().is_none());

        {
            let _saver = UmaskSaver::new();
            assert!(UmaskSaver::saved_umask().is_some());
        }
        assert!(UmaskSaver::saved_umask().is_none());

        {
            let _saver = UmaskSaver::new();
            assert!(UmaskSaver::saved_umask().is_some());
        }
        assert!(UmaskSaver::saved_umask().is_none());

        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn saver_mask_field_captures_prior_value() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o055) };

        let saver = UmaskSaver::new();
        // The saver's mask field should hold 0o055 (the prior mask)
        // We can verify through saved_umask() which reads from the global
        assert_eq!(UmaskSaver::saved_umask(), Some(0o055));
        drop(saver);

        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn drop_clears_global_even_with_nonzero_mask() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o123) };

        {
            let _saver = UmaskSaver::new();
            assert_eq!(UmaskSaver::saved_umask(), Some(0o123));
        }
        // Drop must clear the global
        assert!(UmaskSaver::saved_umask().is_none());

        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn mask_value_077_is_active_during_saver_lifetime() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o000) };

        let _saver = UmaskSaver::new();
        // Query current mask (set-and-restore pattern)
        let active = unsafe { umask(0o077) };
        unsafe { umask(active) };
        assert_eq!(active, 0o077, "UmaskSaver should set mask to 0o077");

        drop(_saver);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn triple_nested_savers() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o022) };

        let _outer = UmaskSaver::new();
        assert_eq!(UmaskSaver::saved_umask(), Some(0o022));

        let _mid = UmaskSaver::new();
        assert_eq!(UmaskSaver::saved_umask(), Some(0o077));

        let _inner = UmaskSaver::new();
        assert_eq!(UmaskSaver::saved_umask(), Some(0o077));

        drop(_inner);
        drop(_mid);
        drop(_outer);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn saver_from_each_permission_category() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o022) };

        // Test all three permission categories independently
        for &mask in &[0o700, 0o070, 0o007] {
            unsafe { umask(mask) };
            {
                let _saver = UmaskSaver::new();
                assert_eq!(UmaskSaver::saved_umask(), Some(mask));
            }
            let restored = unsafe { umask(mask) };
            assert_eq!(restored, mask);
        }

        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn create_drop_preserves_umask_value_end_to_end() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o027) };

        // Create, verify active mask, drop, verify restoration
        let saver = UmaskSaver::new();
        let during = unsafe { umask(0o077) };
        unsafe { umask(during) };
        assert_eq!(during, 0o077);
        drop(saver);

        let after = unsafe { umask(0o027) };
        assert_eq!(after, 0o027);

        unsafe { umask(original) };
    }

    // ── Individual permission bits (remaining 6) ────────────

    #[cfg(unix)]
    #[test]
    fn individual_permission_bits_owner_write() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o200) };
        {
            let _saver = UmaskSaver::new();
            assert_eq!(UmaskSaver::saved_umask(), Some(0o200));
        }
        let restored = unsafe { umask(original) };
        assert_eq!(restored, 0o200);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn individual_permission_bits_owner_execute() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o100) };
        {
            let _saver = UmaskSaver::new();
            assert_eq!(UmaskSaver::saved_umask(), Some(0o100));
        }
        let restored = unsafe { umask(original) };
        assert_eq!(restored, 0o100);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn individual_permission_bits_group_read() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o040) };
        {
            let _saver = UmaskSaver::new();
            assert_eq!(UmaskSaver::saved_umask(), Some(0o040));
        }
        let restored = unsafe { umask(original) };
        assert_eq!(restored, 0o040);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn individual_permission_bits_group_execute() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o010) };
        {
            let _saver = UmaskSaver::new();
            assert_eq!(UmaskSaver::saved_umask(), Some(0o010));
        }
        let restored = unsafe { umask(original) };
        assert_eq!(restored, 0o010);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn individual_permission_bits_other_read() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o004) };
        {
            let _saver = UmaskSaver::new();
            assert_eq!(UmaskSaver::saved_umask(), Some(0o004));
        }
        let restored = unsafe { umask(original) };
        assert_eq!(restored, 0o004);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn individual_permission_bits_other_write() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o002) };
        {
            let _saver = UmaskSaver::new();
            assert_eq!(UmaskSaver::saved_umask(), Some(0o002));
        }
        let restored = unsafe { umask(original) };
        assert_eq!(restored, 0o002);
        unsafe { umask(original) };
    }

    // ── Common umask patterns ─────────────────────────────

    #[cfg(unix)]
    #[test]
    fn common_pattern_022_roundtrips() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o022) };
        { let _s = UmaskSaver::new(); }
        let restored = unsafe { umask(original) };
        assert_eq!(restored, 0o022);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn common_pattern_027_roundtrips() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o027) };
        { let _s = UmaskSaver::new(); }
        let restored = unsafe { umask(original) };
        assert_eq!(restored, 0o027);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn common_pattern_002_roundtrips() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o002) };
        { let _s = UmaskSaver::new(); }
        let restored = unsafe { umask(original) };
        assert_eq!(restored, 0o002);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn common_pattern_077_roundtrips() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o077) };
        { let _s = UmaskSaver::new(); }
        let restored = unsafe { umask(original) };
        assert_eq!(restored, 0o077);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn common_pattern_007_roundtrips() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o007) };
        { let _s = UmaskSaver::new(); }
        let restored = unsafe { umask(original) };
        assert_eq!(restored, 0o007);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn common_pattern_037_roundtrips() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o037) };
        { let _s = UmaskSaver::new(); }
        let restored = unsafe { umask(original) };
        assert_eq!(restored, 0o037);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn common_pattern_066_roundtrips() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o066) };
        { let _s = UmaskSaver::new(); }
        let restored = unsafe { umask(original) };
        assert_eq!(restored, 0o066);
        unsafe { umask(original) };
    }

    // ── Saved umask behavioral tests ──────────────────────

    #[cfg(unix)]
    #[test]
    fn saved_umask_reflects_latest_saver() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o033) };
        let _saver = UmaskSaver::new();
        assert_eq!(UmaskSaver::saved_umask(), Some(0o033));
        drop(_saver);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn saved_umask_is_mode_t_value() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o055) };
        let _saver = UmaskSaver::new();
        let saved = UmaskSaver::saved_umask().unwrap();
        assert_eq!(saved as u32, 0o055u32);
        drop(_saver);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn two_sequential_savers_independent_saved_values() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o011) };

        {
            let _s1 = UmaskSaver::new();
            assert_eq!(UmaskSaver::saved_umask(), Some(0o011));
        }
        assert!(UmaskSaver::saved_umask().is_none());

        unsafe { umask(0o044) };
        {
            let _s2 = UmaskSaver::new();
            assert_eq!(UmaskSaver::saved_umask(), Some(0o044));
        }
        assert!(UmaskSaver::saved_umask().is_none());

        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn saved_umask_none_cleared_after_nested_drops() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o022) };

        let outer = UmaskSaver::new();
        let inner = UmaskSaver::new();
        drop(inner);
        drop(outer);
        assert!(UmaskSaver::saved_umask().is_none());

        unsafe { umask(original) };
    }

    // ── Mask restore correctness with specific values ─────

    #[cfg(unix)]
    #[test]
    fn restore_mask_0o111() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o111) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o111);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn restore_mask_0o222() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o222) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o222);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn restore_mask_0o333() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o333) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o333);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn restore_mask_0o444() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o444) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o444);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn restore_mask_0o555() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o555) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o555);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn restore_mask_0o666() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o666) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o666);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn restore_mask_0o234() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o234) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o234);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn restore_mask_0o567() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o567) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o567);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn restore_mask_0o012() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o012) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o012);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn restore_mask_0o345() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o345) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o345);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn restore_mask_0o076() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o076) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o076);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn restore_mask_0o543() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o543) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o543);
        unsafe { umask(original) };
    }

    // ── Active mask verification during saver ─────────────

    #[cfg(unix)]
    #[test]
    fn active_mask_is_077_from_permissive_start() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o000) };
        let _saver = UmaskSaver::new();
        let active = unsafe { umask(0o077) };
        unsafe { umask(active) };
        assert_eq!(active, 0o077);
        drop(_saver);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn active_mask_is_077_from_restrictive_start() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o777) };
        let _saver = UmaskSaver::new();
        let active = unsafe { umask(0o077) };
        unsafe { umask(active) };
        assert_eq!(active, 0o077);
        drop(_saver);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn active_mask_is_077_from_typical_start() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o022) };
        let _saver = UmaskSaver::new();
        let active = unsafe { umask(0o077) };
        unsafe { umask(active) };
        assert_eq!(active, 0o077);
        drop(_saver);
        unsafe { umask(original) };
    }

    // ── Multiple cycles with verification ─────────────────

    #[cfg(unix)]
    #[test]
    fn ten_cycles_all_restore_correctly() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o022) };

        for i in 0..10u32 {
            let mask = (i * 37) as mode_t & 0o777;
            unsafe { umask(mask) };
            { let _s = UmaskSaver::new(); }
            let restored = unsafe { umask(mask) };
            assert_eq!(restored, mask, "cycle {i} mask 0o{mask:03o}");
        }

        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn ten_cycles_saved_umask_transitions() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o022) };

        for i in 0..10u32 {
            let mask = ((i + 1) * 23) as mode_t & 0o777;
            unsafe { umask(mask) };
            assert!(UmaskSaver::saved_umask().is_none());
            let _s = UmaskSaver::new();
            assert_eq!(UmaskSaver::saved_umask(), Some(mask));
            drop(_s);
            assert!(UmaskSaver::saved_umask().is_none());
        }

        unsafe { umask(original) };
    }

    // ── Pair permission patterns ──────────────────────────

    #[cfg(unix)]
    #[test]
    fn pair_owner_rw() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o600) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o600);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn pair_group_rw() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o060) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o060);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn pair_other_rw() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o006) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o006);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn pair_owner_rx() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o500) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o500);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn pair_group_rx() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o050) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o050);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn pair_other_rx() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o005) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o005);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn pair_owner_wx() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o300) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o300);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn pair_group_wx() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o030) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o030);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn pair_other_wx() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o003) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o003);
        unsafe { umask(original) };
    }

    // ── Cross-category patterns ───────────────────────────

    #[cfg(unix)]
    #[test]
    fn cross_owner_r_group_w_other_x() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o421) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o421);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn cross_owner_x_group_r_other_w() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o142) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o142);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn cross_owner_w_group_x_other_r() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o214) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o214);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn cross_owner_rw_group_rx() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o650) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o650);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn cross_group_rw_other_rx() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o065) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o065);
        unsafe { umask(original) };
    }

    // ── Nesting depth tests ───────────────────────────────

    #[cfg(unix)]
    #[test]
    fn four_deep_nested_savers() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o022) };

        let a = UmaskSaver::new();
        let b = UmaskSaver::new();
        let c = UmaskSaver::new();
        let d = UmaskSaver::new();
        drop(d);
        drop(c);
        drop(b);
        drop(a);

        let r = unsafe { umask(original) };
        assert_eq!(r, 0o022);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn five_deep_nested_all_saved_umask_077() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o022) };

        let a = UmaskSaver::new();
        assert_eq!(UmaskSaver::saved_umask(), Some(0o022));
        let b = UmaskSaver::new();
        assert_eq!(UmaskSaver::saved_umask(), Some(0o077));
        let c = UmaskSaver::new();
        assert_eq!(UmaskSaver::saved_umask(), Some(0o077));
        let d = UmaskSaver::new();
        assert_eq!(UmaskSaver::saved_umask(), Some(0o077));
        let e = UmaskSaver::new();
        assert_eq!(UmaskSaver::saved_umask(), Some(0o077));

        drop(e);
        drop(d);
        drop(c);
        drop(b);
        drop(a);
        unsafe { umask(original) };
    }

    // ── Default trait equivalence ─────────────────────────

    #[cfg(unix)]
    #[test]
    fn default_sets_077_like_new() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o022) };
        let _saver = UmaskSaver::default();
        let active = unsafe { umask(0o077) };
        unsafe { umask(active) };
        assert_eq!(active, 0o077);
        drop(_saver);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn default_captures_saved_umask() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o033) };
        let _saver = UmaskSaver::default();
        assert_eq!(UmaskSaver::saved_umask(), Some(0o033));
        drop(_saver);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn default_restores_on_drop() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o044) };
        { let _s = UmaskSaver::default(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o044);
        unsafe { umask(original) };
    }

    // ── Alternating new/default cycles ────────────────────

    #[cfg(unix)]
    #[test]
    fn alternating_new_default_five_cycles() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o022) };

        for i in 0..5 {
            unsafe { umask(0o022) };
            if i % 2 == 0 {
                let _s = UmaskSaver::new();
            } else {
                let _s = UmaskSaver::default();
            }
            let r = unsafe { umask(0o022) };
            assert_eq!(r, 0o022, "cycle {i}");
        }

        unsafe { umask(original) };
    }

    // ── Mask stability under repeated queries ─────────────

    #[cfg(unix)]
    #[test]
    fn saved_umask_stable_across_repeated_reads() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o055) };
        let _saver = UmaskSaver::new();
        for _ in 0..10 {
            assert_eq!(UmaskSaver::saved_umask(), Some(0o055));
        }
        drop(_saver);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn active_mask_stable_across_repeated_queries() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o022) };
        let _saver = UmaskSaver::new();
        for _ in 0..5 {
            let m = unsafe { umask(0o077) };
            unsafe { umask(m) };
            assert_eq!(m, 0o077);
        }
        drop(_saver);
        unsafe { umask(original) };
    }

    // ── Boundary: single-bit masks ────────────────────────

    #[cfg(unix)]
    #[test]
    fn single_bit_001_roundtrips() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o001) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o001);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn single_bit_002_roundtrips() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o002) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o002);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn single_bit_004_roundtrips() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o004) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o004);
        unsafe { umask(original) };
    }

    // ── Saved umask transitions in nested create/drops ────

    #[cfg(unix)]
    #[test]
    fn nested_saved_umask_after_each_drop() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o022) };
        SAVED_UMASK.lock().unwrap().take();

        assert!(UmaskSaver::saved_umask().is_none());
        let a = UmaskSaver::new();
        assert_eq!(UmaskSaver::saved_umask(), Some(0o022));
        let b = UmaskSaver::new();
        assert_eq!(UmaskSaver::saved_umask(), Some(0o077));
        drop(b);
        // b's drop calls take(), so saved_umask is None
        assert!(UmaskSaver::saved_umask().is_none());
        drop(a);
        assert!(UmaskSaver::saved_umask().is_none());

        unsafe { umask(original) };
    }

    // ── Verify the invariant: new() always sets 0o077 ────

    #[cfg(unix)]
    #[test]
    fn new_always_sets_077_regardless_of_prior_value() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o022) };

        for &mask in &[0o000, 0o022, 0o077, 0o777, 0o123, 0o456] {
            unsafe { umask(mask) };
            let _s = UmaskSaver::new();
            let active = unsafe { umask(0o077) };
            unsafe { umask(active) };
            assert_eq!(active, 0o077, "from mask 0o{mask:03o}");
            drop(_s);
        }

        unsafe { umask(original) };
    }

    // ── Additional restore patterns ───────────────────────

    #[cfg(unix)]
    #[test]
    fn restore_mask_0o015() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o015) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o015);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn restore_mask_0o246() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o246) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o246);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn restore_mask_0o351() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o351) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o351);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn restore_mask_0o472() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o472) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o472);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn restore_mask_0o613() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o613) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o613);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn restore_mask_0o724() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o724) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o724);
        unsafe { umask(original) };
    }

    // ── Exhaustive all-9-bits individual tests ────────────

    #[cfg(unix)]
    #[test]
    fn single_bit_010_roundtrips() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o010) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o010);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn single_bit_020_roundtrips() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o020) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o020);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn single_bit_040_roundtrips() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o040) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o040);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn single_bit_100_roundtrips() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o100) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o100);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn single_bit_200_roundtrips() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o200) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o200);
        unsafe { umask(original) };
    }

    #[cfg(unix)]
    #[test]
    fn single_bit_400_roundtrips() {
        let _g = TEST_LOCK.lock().unwrap();
        let original = unsafe { umask(0o400) };
        { let _s = UmaskSaver::new(); }
        let r = unsafe { umask(original) };
        assert_eq!(r, 0o400);
        unsafe { umask(original) };
    }

    // ── Non-unix platform tests ───────────────────────────

    #[cfg(not(unix))]
    #[test]
    fn non_unix_new_does_not_panic() {
        let _g = TEST_LOCK.lock().unwrap();
        let _saver = UmaskSaver::new();
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
