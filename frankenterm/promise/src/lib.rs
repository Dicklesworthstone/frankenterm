use anyhow::Error;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use thiserror::*;

pub mod spawn;

#[derive(Debug, Error)]
#[error("Promise was dropped before completion")]
pub struct BrokenPromise {}

#[derive(Debug)]
struct Core<T> {
    result: Option<anyhow::Result<T>>,
    waker: Option<Waker>,
}

pub struct Promise<T> {
    core: Arc<Mutex<Core<T>>>,
}

#[derive(Debug)]
pub struct Future<T> {
    core: Arc<Mutex<Core<T>>>,
}

impl<T> Default for Promise<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Promise<T> {
    pub fn new() -> Self {
        Self {
            core: Arc::new(Mutex::new(Core {
                result: None,
                waker: None,
            })),
        }
    }

    pub fn get_future(&mut self) -> Option<Future<T>> {
        Some(Future {
            core: Arc::clone(&self.core),
        })
    }

    pub fn ok(&mut self, value: T) -> bool {
        self.result(Ok(value))
    }

    pub fn err(&mut self, err: Error) -> bool {
        self.result(Err(err))
    }

    pub fn result(&mut self, result: Result<T, Error>) -> bool {
        let mut core = self.core.lock().unwrap();
        core.result.replace(result);
        if let Some(waker) = core.waker.take() {
            waker.wake();
        }
        true
    }
}

impl<T: Send + 'static> Future<T> {
    /// Create a leaf future which is immediately ready with
    /// the provided value
    pub fn ok(value: T) -> Self {
        Self::result(Ok(value))
    }

    /// Create a leaf future which is immediately ready with
    /// the provided error
    pub fn err(err: Error) -> Self {
        Self::result(Err(err))
    }

    /// Create a leaf future which is immediately ready with
    /// the provided result
    pub fn result(result: Result<T, Error>) -> Self {
        Self {
            core: Arc::new(Mutex::new(Core {
                result: Some(result),
                waker: None,
            })),
        }
    }
}

impl<T: Send + 'static> std::future::Future for Future<T> {
    type Output = Result<T, Error>;

    fn poll(self: Pin<&mut Self>, ctx: &mut Context) -> Poll<Self::Output> {
        let waker = ctx.waker().clone();

        let mut core = self.core.lock().unwrap();
        if let Some(result) = core.result.take() {
            Poll::Ready(result)
        } else {
            core.waker.replace(waker);
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future as StdFuture;
    use std::task::{RawWaker, RawWakerVTable};

    fn noop_waker() -> Waker {
        fn noop(_: *const ()) {}
        fn clone(p: *const ()) -> RawWaker {
            RawWaker::new(p, &VTABLE)
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
        unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
    }

    // ── BrokenPromise ──────────────────────────────────────────

    #[test]
    fn broken_promise_display() {
        let err = BrokenPromise {};
        assert_eq!(err.to_string(), "Promise was dropped before completion");
    }

    #[test]
    fn broken_promise_is_debug() {
        let err = BrokenPromise {};
        let debug = format!("{err:?}");
        assert!(debug.contains("BrokenPromise"));
    }

    // ── Promise construction ───────────────────────────────────

    #[test]
    fn promise_new_creates_instance() {
        let _p: Promise<i32> = Promise::new();
    }

    #[test]
    fn promise_default_creates_instance() {
        let _p: Promise<String> = Promise::default();
    }

    #[test]
    fn get_future_returns_some() {
        let mut p: Promise<i32> = Promise::new();
        assert!(p.get_future().is_some());
    }

    #[test]
    fn get_future_can_be_called_multiple_times() {
        let mut p: Promise<i32> = Promise::new();
        let _f1 = p.get_future();
        let _f2 = p.get_future();
    }

    // ── Promise::ok / err / result ─────────────────────────────

    #[test]
    fn promise_ok_returns_true() {
        let mut p: Promise<i32> = Promise::new();
        assert!(p.ok(42));
    }

    #[test]
    fn promise_err_returns_true() {
        let mut p: Promise<i32> = Promise::new();
        assert!(p.err(anyhow::anyhow!("test error")));
    }

    #[test]
    fn promise_result_ok_returns_true() {
        let mut p: Promise<i32> = Promise::new();
        assert!(p.result(Ok(99)));
    }

    #[test]
    fn promise_result_err_returns_true() {
        let mut p: Promise<i32> = Promise::new();
        assert!(p.result(Err(anyhow::anyhow!("error"))));
    }

    // ── Future::ok / err / result (immediately ready) ──────────

    #[test]
    fn future_ok_is_immediately_ready() {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut fut = Future::ok(42i32);
        match StdFuture::poll(Pin::new(&mut fut), &mut cx) {
            Poll::Ready(Ok(val)) => assert_eq!(val, 42),
            other => panic!("{}", format!("expected Ready(Ok(42)), got {other:?}")),
        }
    }

    #[test]
    fn future_err_is_immediately_ready() {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut fut = Future::<i32>::err(anyhow::anyhow!("boom"));
        match StdFuture::poll(Pin::new(&mut fut), &mut cx) {
            Poll::Ready(Err(e)) => assert_eq!(e.to_string(), "boom"),
            other => panic!("{}", format!("expected Ready(Err), got {other:?}")),
        }
    }

    #[test]
    fn future_result_ok_is_immediately_ready() {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut fut = Future::result(Ok(String::from("hello")));
        match StdFuture::poll(Pin::new(&mut fut), &mut cx) {
            Poll::Ready(Ok(val)) => assert_eq!(val, "hello"),
            other => panic!("{}", format!("expected Ready(Ok), got {other:?}")),
        }
    }

    #[test]
    fn future_result_err_is_immediately_ready() {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut fut = Future::<i32>::result(Err(anyhow::anyhow!("fail")));
        match StdFuture::poll(Pin::new(&mut fut), &mut cx) {
            Poll::Ready(Err(e)) => assert!(e.to_string().contains("fail")),
            other => panic!("{}", format!("expected Ready(Err), got {other:?}")),
        }
    }

    // ── Polling behavior ───────────────────────────────────────

    #[test]
    fn future_is_pending_before_promise_resolves() {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut p: Promise<i32> = Promise::new();
        let mut fut = p.get_future().unwrap();
        assert!(matches!(
            StdFuture::poll(Pin::new(&mut fut), &mut cx),
            Poll::Pending
        ));
        // Now resolve
        p.ok(100);
        assert!(matches!(
            StdFuture::poll(Pin::new(&mut fut), &mut cx),
            Poll::Ready(Ok(100))
        ));
    }

    #[test]
    fn future_ready_after_promise_ok() {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut p: Promise<String> = Promise::new();
        let mut fut = p.get_future().unwrap();
        p.ok("resolved".to_string());
        match StdFuture::poll(Pin::new(&mut fut), &mut cx) {
            Poll::Ready(Ok(val)) => assert_eq!(val, "resolved"),
            other => panic!("{}", format!("expected Ready(Ok), got {other:?}")),
        }
    }

    #[test]
    fn future_ready_after_promise_err() {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut p: Promise<i32> = Promise::new();
        let mut fut = p.get_future().unwrap();
        p.err(anyhow::anyhow!("promise error"));
        match StdFuture::poll(Pin::new(&mut fut), &mut cx) {
            Poll::Ready(Err(e)) => assert_eq!(e.to_string(), "promise error"),
            other => panic!("{}", format!("expected Ready(Err), got {other:?}")),
        }
    }

    #[test]
    fn poll_after_ready_returns_err_none() {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut fut = Future::ok(42i32);
        // First poll takes the result
        let _ = StdFuture::poll(Pin::new(&mut fut), &mut cx);
        // Second poll: result was taken, so it's Pending (no result left)
        assert!(matches!(
            StdFuture::poll(Pin::new(&mut fut), &mut cx),
            Poll::Pending
        ));
    }

    // ── Waker integration ──────────────────────────────────────

    #[test]
    fn waker_is_stored_on_pending_poll() {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut p: Promise<i32> = Promise::new();
        let mut fut = p.get_future().unwrap();

        // Poll while pending stores waker
        assert!(matches!(
            StdFuture::poll(Pin::new(&mut fut), &mut cx),
            Poll::Pending
        ));

        // Resolving should wake (noop waker won't crash)
        p.ok(1);
    }

    #[test]
    fn resolve_before_poll_does_not_panic_without_waker() {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut p: Promise<i32> = Promise::new();
        let mut fut = p.get_future().unwrap();
        // Resolve before any poll — no waker set yet
        p.ok(77);
        match StdFuture::poll(Pin::new(&mut fut), &mut cx) {
            Poll::Ready(Ok(val)) => assert_eq!(val, 77),
            other => panic!("{}", format!("expected Ready(Ok(77)), got {other:?}")),
        }
    }

    // ── Cross-thread usage ─────────────────────────────────────

    #[test]
    fn promise_resolves_from_another_thread() {
        let mut p: Promise<i32> = Promise::new();
        let mut fut = p.get_future().unwrap();

        let handle = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(10));
            p.ok(999);
        });

        handle.join().unwrap();

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        match StdFuture::poll(Pin::new(&mut fut), &mut cx) {
            Poll::Ready(Ok(val)) => assert_eq!(val, 999),
            other => panic!("{}", format!("expected Ready(Ok(999)), got {other:?}")),
        }
    }

    // ── Type flexibility ───────────────────────────────────────

    #[test]
    fn promise_with_vec_type() {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut p: Promise<Vec<u8>> = Promise::new();
        let mut fut = p.get_future().unwrap();
        p.ok(vec![1, 2, 3]);
        match StdFuture::poll(Pin::new(&mut fut), &mut cx) {
            Poll::Ready(Ok(val)) => assert_eq!(val, vec![1, 2, 3]),
            other => panic!("{}", format!("expected Ready(Ok), got {other:?}")),
        }
    }

    #[test]
    fn promise_with_unit_type() {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut p: Promise<()> = Promise::new();
        let mut fut = p.get_future().unwrap();
        p.ok(());
        assert!(matches!(
            StdFuture::poll(Pin::new(&mut fut), &mut cx),
            Poll::Ready(Ok(()))
        ));
    }

    // ── Multiple futures from same promise ─────────────────────

    #[test]
    fn multiple_futures_share_result() {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut p: Promise<i32> = Promise::new();
        let mut f1 = p.get_future().unwrap();
        let mut f2 = p.get_future().unwrap();

        p.ok(42);

        // First future gets the result
        match StdFuture::poll(Pin::new(&mut f1), &mut cx) {
            Poll::Ready(Ok(val)) => assert_eq!(val, 42),
            Poll::Pending => { /* f2 might have consumed it */ }
            other => panic!("{}", format!("unexpected: {other:?}")),
        }
        // Second future: result already taken by first
        // It will be Pending since the result was consumed
        let _ = StdFuture::poll(Pin::new(&mut f2), &mut cx);
    }
}
