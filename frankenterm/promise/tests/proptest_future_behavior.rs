use anyhow::anyhow;
use promise::{Future, Promise};
use proptest::prelude::*;
use std::future::Future as StdFuture;
use std::pin::Pin;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

fn noop_waker() -> Waker {
    fn noop(_: *const ()) {}
    fn clone(p: *const ()) -> RawWaker {
        RawWaker::new(p, &VTABLE)
    }
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
}

fn arb_small_string() -> impl Strategy<Value = String> {
    proptest::collection::vec(any::<char>(), 0..24).prop_map(|chars| chars.into_iter().collect())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn future_ok_is_immediately_ready_for_arbitrary_i64(value in any::<i64>()) {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut fut = Future::ok(value);

        match StdFuture::poll(Pin::new(&mut fut), &mut cx) {
            Poll::Ready(Ok(got)) => prop_assert_eq!(got, value),
            other => prop_assert!(false, "expected Ready(Ok(_)), got {other:?}"),
        }
    }

    #[test]
    fn promise_last_write_wins_before_poll(
        first in arb_small_string(),
        second in arb_small_string(),
    ) {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut promise = Promise::new();
        let mut fut = promise.get_future().expect("future");

        prop_assert!(promise.ok(first));
        prop_assert!(promise.err(anyhow!(second.clone())));

        match StdFuture::poll(Pin::new(&mut fut), &mut cx) {
            Poll::Ready(Err(err)) => prop_assert_eq!(err.to_string(), second),
            other => prop_assert!(false, "expected Ready(Err(_)), got {other:?}"),
        }
    }
}
