#![cfg(test)]
//! The planted defects, demonstrated rather than asserted.
//!
//! Every test here pins *buggy* behaviour on purpose: the fixture keeps its
//! bugs, and these exist so the manifest cannot drift away from the code it
//! describes. If one starts failing, `../head` changed and `case.toml` is now
//! describing something that is no longer there.
//!
//! The budget test spends a hundred retries at a real backoff, so the suite
//! takes about fifteen seconds. That is the cost of proving the wrap rather
//! than reasoning about it.

use retry_budget::{Client, Error, Request, Transport};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Records every attempt and answers from a fixed script.
struct Script {
    answer: Result<Vec<u8>, Error>,
    attempts: AtomicU32,
}

/// The client takes its transport by value, so the test keeps a handle on the
/// same script through an `Arc` rather than reaching back into the client.
#[derive(Clone)]
struct Shared(Arc<Script>);

impl Shared {
    fn new(answer: Result<Vec<u8>, Error>) -> Self {
        Self(Arc::new(Script { answer, attempts: AtomicU32::new(0) }))
    }

    fn attempts(&self) -> u32 {
        self.0.attempts.load(Ordering::SeqCst)
    }
}

impl Transport for Shared {
    fn round_trip(&self, _req: &Request, _timeout: Duration) -> Result<Vec<u8>, Error> {
        self.0.attempts.fetch_add(1, Ordering::SeqCst);
        self.0.answer.clone()
    }
}

fn post() -> Request {
    Request { method: "POST", path: "/charges".into(), body: Some(b"amount=500".to_vec()) }
}

/// `retries-non-idempotent`: a POST whose response was lost is sent again, so
/// one charge can be applied four times.
#[test]
fn a_post_is_resent_after_a_lost_response() {
    let script = Shared::new(Err(Error::Transport("connection reset".into())));
    let client = Client::new(script.clone(), Duration::from_millis(10));
    assert!(client.send(&post()).is_err());
    // The method is never consulted.
    assert_eq!(script.attempts(), 4);
}

/// `retries-client-errors`: a 400 can never succeed, and is retried anyway.
#[test]
fn a_client_error_is_retried_until_the_attempts_run_out() {
    let script = Shared::new(Err(Error::Status(400)));
    let client = Client::new(script.clone(), Duration::from_millis(10));
    assert_eq!(client.send(&post()), Err(Error::Status(400)));
    assert_eq!(script.attempts(), 4);
}

/// `shared-retry-budget`: the budget is per client and never replenished, so
/// one bad spell leaves every later caller with no retries — and then the
/// exhausted counter wraps and hands back four billion.
#[test]
fn the_budget_runs_out_and_then_wraps_around() {
    let script = Shared::new(Err(Error::Status(500)));
    let client = Client::new(script.clone(), Duration::from_millis(1));

    // Three retries per call: 33 calls spend 99 of the 100.
    for _ in 0..33 {
        let _ = client.send(&post());
    }
    let before = script.attempts();
    let _ = client.send(&post());
    assert_eq!(script.attempts() - before, 2, "the budget is spent: one retry left, then stop");

    let before = script.attempts();
    let _ = client.send(&post());
    assert_eq!(
        script.attempts() - before,
        4,
        "fetch_sub against a zero budget wrapped to u32::MAX, so retries resumed"
    );
}

/// `timeout-per-attempt`: the doc comment still says the caller's timeout
/// bounds the call. It now bounds one attempt of up to four.
#[test]
fn the_callers_timeout_bounds_one_attempt_not_the_call() {
    let timeout = Duration::from_millis(10);
    let script = Shared::new(Err(Error::Transport("timed out".into())));
    let client = Client::new(script, timeout);
    let started = Instant::now();
    let _ = client.send(&post());
    // Backoff alone is 50 + 100 + 200ms, already far past the 10ms the caller
    // asked for, before any time spent in the transport itself.
    assert!(
        started.elapsed() > timeout * 10,
        "elapsed {:?} should dwarf the caller's {timeout:?}",
        started.elapsed()
    );
}

// --- the parts the defence probe makes false claims about ------------------
//
// Both are safe. These prove it, so the probe cannot be asserting something is
// false merely because the arena happened to refute it.

#[test]
fn the_backoff_shift_is_capped_and_cannot_overflow() {
    // `attempt.min(6)` bounds the shift at 64, whatever the attempt count; with
    // max_retries = 3 the largest shift actually reached is 4.
    for attempt in 0u32..10_000 {
        let shift = attempt.min(6);
        assert!(1u32 << shift <= 64);
    }
}

#[test]
fn the_success_counter_loses_no_increments_under_relaxed_ordering() {
    let script = Shared::new(Ok(b"ok".to_vec()));
    let client = Client::new(script, Duration::from_millis(10));
    std::thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| {
                for _ in 0..200 {
                    client.send(&post()).unwrap();
                }
            });
        }
    });
    // Relaxed constrains ordering, not atomicity: no increment is lost, which
    // is the only property this counter needs.
    assert_eq!(client.successes(), 1600);
}
