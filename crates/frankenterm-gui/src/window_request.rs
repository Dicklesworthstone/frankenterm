//! Main-thread admission for the Lua `window:*` methods that wait for an
//! answer from their GUI window.
//!
//! Such a method sends its window a request and waits for the reply.
//! `Window::notify` drops a request that the GUI main thread does not admit,
//! so the method failed with "receiving on a closed channel". An
//! update-status handler calling `window:leader_is_active()` failed that way
//! while remote domains reattached and filled the main thread's Interactive
//! queue (2026-11-13). The methods now reserve admission themselves: a full
//! queue is retried while it drains, and any other refusal names its cause.

use promise::spawn::{
    MainThreadReservationOutcome, MainThreadServiceClass, MainThreadSpawnReservation,
};
use std::future::Future;
use std::time::Duration;

/// How long, in total, a Lua window method waits for room on a full GUI main
/// thread before it fails.
pub const WINDOW_REQUEST_ADMISSION_WAIT: Duration = Duration::from_secs(2);

const WINDOW_REQUEST_ESTIMATED_BYTES: usize = 4 * 1024;
const FIRST_RETRY_DELAY: Duration = Duration::from_millis(2);
const MAX_RETRY_DELAY: Duration = Duration::from_millis(50);

/// One admission attempt.
#[derive(Debug)]
pub enum Attempt<T> {
    Admitted(T),
    /// The queue is full; it drains as the main thread runs.
    Full(String),
    /// Refused for a reason that waiting does not change.
    Refused(String),
}

/// Reserve the GUI main thread for one Lua `window:<method>()` request,
/// waiting up to [`WINDOW_REQUEST_ADMISSION_WAIT`] while its queue is full.
pub async fn reserve_window_request(
    method: &'static str,
) -> anyhow::Result<MainThreadSpawnReservation> {
    retry_while_full(
        method,
        WINDOW_REQUEST_ADMISSION_WAIT,
        || match promise::spawn::try_reserve_main_thread(
            MainThreadServiceClass::Interactive,
            WINDOW_REQUEST_ESTIMATED_BYTES,
        ) {
            MainThreadReservationOutcome::Reserved(reservation) => Attempt::Admitted(reservation),
            MainThreadReservationOutcome::RetryableFull(rejection) => {
                Attempt::Full(format!("{rejection:?}"))
            }
            refused => Attempt::Refused(format!("{refused:?}")),
        },
        promise::spawn::sleep,
    )
    .await
}

/// Run `attempt` until it is admitted. While it reports a full queue, sleep
/// with a growing delay, for at most `wait` of sleep in total.
pub async fn retry_while_full<T, S, F>(
    method: &'static str,
    wait: Duration,
    mut attempt: impl FnMut() -> Attempt<T>,
    mut sleep: S,
) -> anyhow::Result<T>
where
    S: FnMut(Duration) -> F,
    F: Future<Output = ()>,
{
    let mut slept = Duration::ZERO;
    let mut delay = FIRST_RETRY_DELAY;
    loop {
        match attempt() {
            Attempt::Admitted(admitted) => return Ok(admitted),
            Attempt::Full(reason) if slept < wait => {
                let pause = delay.min(wait - slept);
                sleep(pause).await;
                slept += pause;
                delay = (delay * 2).min(MAX_RETRY_DELAY);
                log::debug!("window:{method}(): GUI main thread full, retrying: {reason}");
            }
            Attempt::Full(reason) => anyhow::bail!(
                "window:{method}(): the GUI main thread stayed full for {wait:?} and did not \
                 take the request: {reason}"
            ),
            Attempt::Refused(reason) => anyhow::bail!(
                "window:{method}(): the GUI main thread did not take the request: {reason}"
            ),
        }
    }
}

/// The error for an admitted request whose window never answered.
pub fn window_request_unanswered(method: &'static str) -> anyhow::Error {
    anyhow::anyhow!("window:{method}(): the window closed before it answered")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn instant_sleep(
        log: &RefCell<Vec<Duration>>,
    ) -> impl FnMut(Duration) -> futures::future::Ready<()> + '_ {
        move |pause| {
            log.borrow_mut().push(pause);
            futures::future::ready(())
        }
    }

    #[test]
    fn a_full_queue_is_retried_until_admitted() {
        let slept = RefCell::new(Vec::new());
        let mut attempts = 0;
        let admitted = futures::executor::block_on(retry_while_full(
            "leader_is_active",
            Duration::from_secs(2),
            || {
                attempts += 1;
                if attempts < 4 {
                    Attempt::Full("RetryableFull".to_string())
                } else {
                    Attempt::Admitted(attempts)
                }
            },
            instant_sleep(&slept),
        ))
        .unwrap();
        assert_eq!(admitted, 4);
        assert_eq!(
            *slept.borrow(),
            vec![
                Duration::from_millis(2),
                Duration::from_millis(4),
                Duration::from_millis(8)
            ]
        );
    }

    #[test]
    fn a_queue_that_stays_full_fails_after_the_wait_and_names_it() {
        let slept = RefCell::new(Vec::new());
        let wait = Duration::from_millis(300);
        let error = futures::executor::block_on(retry_while_full(
            "leader_is_active",
            wait,
            || Attempt::<()>::Full("RetryableFull(queue 1)".to_string()),
            instant_sleep(&slept),
        ))
        .unwrap_err();
        let total: Duration = slept.borrow().iter().sum();
        assert_eq!(total, wait);
        assert!(slept.borrow().iter().all(|pause| *pause <= MAX_RETRY_DELAY));
        let message = format!("{error:#}");
        assert!(
            message.contains("window:leader_is_active()")
                && message.contains("stayed full")
                && message.contains("RetryableFull(queue 1)"),
            "{}",
            message
        );
    }

    #[test]
    fn a_refusal_that_waiting_cannot_change_fails_at_once() {
        let slept = RefCell::new(Vec::new());
        let mut attempts = 0;
        let error = futures::executor::block_on(retry_while_full(
            "is_focused",
            Duration::from_secs(2),
            || {
                attempts += 1;
                Attempt::<()>::Refused("SchedulerUnavailable".to_string())
            },
            instant_sleep(&slept),
        ))
        .unwrap_err();
        assert_eq!(attempts, 1);
        assert!(slept.borrow().is_empty());
        let message = format!("{error:#}");
        assert!(
            message.contains("window:is_focused()") && message.contains("SchedulerUnavailable"),
            "{}",
            message
        );
    }

    #[test]
    fn an_unanswered_request_says_the_window_closed() {
        let message = format!("{:#}", window_request_unanswered("active_pane"));
        assert_eq!(
            message,
            "window:active_pane(): the window closed before it answered"
        );
    }
}
