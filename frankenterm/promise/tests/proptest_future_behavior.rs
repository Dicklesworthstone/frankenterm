use anyhow::anyhow;
use promise::{Future, Promise};
use proptest::prelude::*;
use std::future::Future as StdFuture;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

fn noop_waker() -> Waker {
    Waker::noop().clone()
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
    fn promise_first_write_wins_before_poll(
        first in arb_small_string(),
        second in arb_small_string(),
    ) {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut promise = Promise::new();
        let mut fut = promise.get_future().expect("future");

        let expected = first.clone();
        prop_assert!(promise.ok(first));
        prop_assert!(!promise.err(anyhow!(second)));

        match StdFuture::poll(Pin::new(&mut fut), &mut cx) {
            Poll::Ready(Ok(value)) => prop_assert_eq!(value, expected),
            other => prop_assert!(false, "expected Ready(Ok(_)), got {other:?}"),
        }
    }
}
