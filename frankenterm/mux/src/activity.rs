//! Keeps track of the number of user-initiated activities
use crate::Mux;
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNT: AtomicUsize = AtomicUsize::new(0);

fn try_increment_count(counter: &AtomicUsize) -> bool {
    let mut current = counter.load(Ordering::SeqCst);
    loop {
        let Some(next) = current.checked_add(1) else {
            return false;
        };
        match counter.compare_exchange_weak(current, next, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(actual) => current = actual,
        }
    }
}

/// Create and hold on to an Activity while you are processing
/// the direct result of a user initiated action, such as preparing
/// to open a window.
/// Once you have opened the window, drop the activity.
/// The activity is used to keep the frontend alive even if there
/// may be no windows present in the mux.
pub struct Activity {
    counted: bool,
}

impl Activity {
    pub fn new() -> Self {
        Self {
            counted: try_increment_count(&COUNT),
        }
    }

    pub fn count() -> usize {
        COUNT.load(Ordering::SeqCst)
    }
}

impl Drop for Activity {
    fn drop(&mut self) {
        if self.counted {
            COUNT.fetch_sub(1, Ordering::SeqCst);
        }

        if !promise::spawn::is_scheduler_configured() {
            return;
        }

        promise::spawn::spawn_into_main_thread(async move {
            if let Some(mux) = Mux::try_get() {
                mux.prune_dead_windows();
            }
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_increment_count_refuses_to_wrap() {
        let counter = AtomicUsize::new(usize::MAX);

        assert!(!try_increment_count(&counter));
        assert_eq!(counter.load(Ordering::SeqCst), usize::MAX);
    }
}
