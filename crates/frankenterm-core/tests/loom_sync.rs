//! Loom model-checking for the wa-3d14m sync primitive contracts.
//!
//! These tests model the invariants the `runtime_async::{Mutex, RwLock,
//! Semaphore}` surface must preserve after the tokio -> asupersync migration.
//! Loom cannot instrument tokio/asupersync internals directly, so we verify the
//! concurrency contracts with loom-native primitives and a small ticketed
//! semaphore model.

use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::sync::{Arc, Condvar, Mutex, RwLock};
use loom::thread;

fn update_max(max_seen: &AtomicUsize, candidate: usize) {
    let mut current = max_seen.load(Ordering::SeqCst);
    while candidate > current {
        match max_seen.compare_exchange(current, candidate, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => break,
            Err(observed) => current = observed,
        }
    }
}

#[test]
fn loom_mutex_preserves_mutual_exclusion() {
    loom::model(|| {
        let value = Arc::new(Mutex::new(0usize));
        let inside = Arc::new(AtomicUsize::new(0));
        let max_inside = Arc::new(AtomicUsize::new(0));

        let v1 = Arc::clone(&value);
        let i1 = Arc::clone(&inside);
        let m1 = Arc::clone(&max_inside);
        let t1 = thread::spawn(move || {
            let mut guard = v1.lock().unwrap();
            let previously_inside = i1.fetch_add(1, Ordering::SeqCst);
            update_max(&m1, previously_inside + 1);
            assert_eq!(previously_inside, 0, "mutex allowed overlapping writers");
            *guard += 1;
            i1.fetch_sub(1, Ordering::SeqCst);
        });

        let v2 = Arc::clone(&value);
        let i2 = Arc::clone(&inside);
        let m2 = Arc::clone(&max_inside);
        let t2 = thread::spawn(move || {
            let mut guard = v2.lock().unwrap();
            let previously_inside = i2.fetch_add(1, Ordering::SeqCst);
            update_max(&m2, previously_inside + 1);
            assert_eq!(previously_inside, 0, "mutex allowed overlapping writers");
            *guard += 2;
            i2.fetch_sub(1, Ordering::SeqCst);
        });

        t1.join().unwrap();
        t2.join().unwrap();

        assert_eq!(*value.lock().unwrap(), 3);
        assert_eq!(max_inside.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn loom_rwlock_preserves_reader_writer_invariant() {
    loom::model(|| {
        let value = Arc::new(RwLock::new(0usize));
        let active_readers = Arc::new(AtomicUsize::new(0));
        let active_writers = Arc::new(AtomicUsize::new(0));
        let max_readers = Arc::new(AtomicUsize::new(0));

        let lock_reader_a = Arc::clone(&value);
        let active_readers_a = Arc::clone(&active_readers);
        let writers_a = Arc::clone(&active_writers);
        let max_a = Arc::clone(&max_readers);
        let reader_a = thread::spawn(move || {
            let guard = lock_reader_a.read().unwrap();
            let prior = active_readers_a.fetch_add(1, Ordering::SeqCst);
            update_max(&max_a, prior + 1);
            assert_eq!(
                writers_a.load(Ordering::SeqCst),
                0,
                "writer overlapped with reader"
            );
            assert_eq!(*guard, 0);
            thread::yield_now();
            active_readers_a.fetch_sub(1, Ordering::SeqCst);
        });

        let lock_reader_b = Arc::clone(&value);
        let active_readers_b = Arc::clone(&active_readers);
        let writers_b = Arc::clone(&active_writers);
        let max_b = Arc::clone(&max_readers);
        let reader_b = thread::spawn(move || {
            let guard = lock_reader_b.read().unwrap();
            let prior = active_readers_b.fetch_add(1, Ordering::SeqCst);
            update_max(&max_b, prior + 1);
            assert_eq!(
                writers_b.load(Ordering::SeqCst),
                0,
                "writer overlapped with reader"
            );
            assert_eq!(*guard, 0);
            thread::yield_now();
            active_readers_b.fetch_sub(1, Ordering::SeqCst);
        });

        let lock_writer = Arc::clone(&value);
        let readers_c = Arc::clone(&active_readers);
        let writers_c = Arc::clone(&active_writers);
        let writer = thread::spawn(move || {
            let mut guard = lock_writer.write().unwrap();
            let prior_writers = writers_c.fetch_add(1, Ordering::SeqCst);
            assert_eq!(prior_writers, 0, "multiple writers entered simultaneously");
            assert_eq!(
                readers_c.load(Ordering::SeqCst),
                0,
                "reader overlapped with writer"
            );
            *guard += 1;
            writers_c.fetch_sub(1, Ordering::SeqCst);
        });

        reader_a.join().unwrap();
        reader_b.join().unwrap();
        writer.join().unwrap();

        assert_eq!(*value.read().unwrap(), 1);
        assert!(max_readers.load(Ordering::SeqCst) <= 2);
    });
}

#[derive(Debug)]
struct LoomTicketSemaphore {
    state: Mutex<LoomTicketSemaphoreState>,
    cv: Condvar,
}

#[derive(Debug)]
struct LoomTicketSemaphoreState {
    available: usize,
    serving_ticket: usize,
}

impl LoomTicketSemaphore {
    fn new(permits: usize) -> Self {
        Self {
            state: Mutex::new(LoomTicketSemaphoreState {
                available: permits,
                serving_ticket: 0,
            }),
            cv: Condvar::new(),
        }
    }

    fn acquire_owned(semaphore: Arc<Self>, ticket: usize) -> LoomSemaphorePermit {
        {
            let mut state = semaphore.state.lock().unwrap();
            while state.available == 0 || state.serving_ticket != ticket {
                state = semaphore.cv.wait(state).unwrap();
            }
            state.available -= 1;
            state.serving_ticket += 1;
        }
        LoomSemaphorePermit { semaphore }
    }

    fn release(&self) {
        let mut state = self.state.lock().unwrap();
        state.available += 1;
        self.cv.notify_all();
    }

    fn available_permits(&self) -> usize {
        self.state.lock().unwrap().available
    }
}

#[derive(Debug)]
struct LoomSemaphorePermit {
    semaphore: Arc<LoomTicketSemaphore>,
}

impl Drop for LoomSemaphorePermit {
    fn drop(&mut self) {
        self.semaphore.release();
    }
}

#[test]
fn loom_semaphore_never_exceeds_capacity() {
    loom::model(|| {
        let semaphore = Arc::new(LoomTicketSemaphore::new(2));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));

        let sem_a = Arc::clone(&semaphore);
        let active_a = Arc::clone(&in_flight);
        let max_a = Arc::clone(&max_in_flight);
        let t1 = thread::spawn(move || {
            let _permit = LoomTicketSemaphore::acquire_owned(sem_a, 0);
            let current = active_a.fetch_add(1, Ordering::SeqCst) + 1;
            update_max(&max_a, current);
            assert!(current <= 2, "semaphore exceeded configured capacity");
            thread::yield_now();
            active_a.fetch_sub(1, Ordering::SeqCst);
        });

        let sem_b = Arc::clone(&semaphore);
        let active_b = Arc::clone(&in_flight);
        let max_b = Arc::clone(&max_in_flight);
        let t2 = thread::spawn(move || {
            let _permit = LoomTicketSemaphore::acquire_owned(sem_b, 1);
            let current = active_b.fetch_add(1, Ordering::SeqCst) + 1;
            update_max(&max_b, current);
            assert!(current <= 2, "semaphore exceeded configured capacity");
            thread::yield_now();
            active_b.fetch_sub(1, Ordering::SeqCst);
        });

        let sem_c = Arc::clone(&semaphore);
        let active_c = Arc::clone(&in_flight);
        let max_c = Arc::clone(&max_in_flight);
        let t3 = thread::spawn(move || {
            let _permit = LoomTicketSemaphore::acquire_owned(sem_c, 2);
            let current = active_c.fetch_add(1, Ordering::SeqCst) + 1;
            update_max(&max_c, current);
            assert!(current <= 2, "semaphore exceeded configured capacity");
            thread::yield_now();
            active_c.fetch_sub(1, Ordering::SeqCst);
        });

        t1.join().unwrap();
        t2.join().unwrap();
        t3.join().unwrap();

        assert_eq!(semaphore.available_permits(), 2);
        assert!(max_in_flight.load(Ordering::SeqCst) <= 2);
    });
}

#[test]
fn loom_semaphore_honors_fifo_waiter_order() {
    loom::model(|| {
        let semaphore = Arc::new(LoomTicketSemaphore::new(1));
        let order = Arc::new(Mutex::new(Vec::new()));

        let initial_permit = LoomTicketSemaphore::acquire_owned(Arc::clone(&semaphore), 0);

        let sem_first = Arc::clone(&semaphore);
        let order_first = Arc::clone(&order);
        let first = thread::spawn(move || {
            let _permit = LoomTicketSemaphore::acquire_owned(sem_first, 1);
            order_first.lock().unwrap().push(1usize);
        });

        let sem_second = Arc::clone(&semaphore);
        let order_second = Arc::clone(&order);
        let second = thread::spawn(move || {
            let _permit = LoomTicketSemaphore::acquire_owned(sem_second, 2);
            order_second.lock().unwrap().push(2usize);
        });

        thread::yield_now();
        drop(initial_permit);

        first.join().unwrap();
        second.join().unwrap();

        assert_eq!(*order.lock().unwrap(), vec![1, 2]);
        assert_eq!(semaphore.available_permits(), 1);
    });
}
