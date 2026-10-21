//! The HTTP client the billing workers use to reach the payments API.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

const BASE_BACKOFF: Duration = Duration::from_millis(50);

/// Retries this client will spend before it stops retrying, so one bad
/// upstream cannot turn into an unbounded amount of extra load.
const RETRY_BUDGET: u32 = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The server answered, with a status we did not want.
    Status(u16),
    /// The request never completed: connection reset, timeout, DNS failure.
    Transport(String),
}

pub struct Request {
    pub method: &'static str,
    pub path: String,
    pub body: Option<Vec<u8>>,
}

/// One round trip. The real implementation talks to the network; tests and
/// the offline simulator substitute their own.
pub trait Transport {
    fn round_trip(&self, req: &Request, timeout: Duration) -> Result<Vec<u8>, Error>;
}

pub struct Client<T: Transport> {
    transport: T,
    timeout: Duration,
    max_retries: u32,
    budget: AtomicU32,
    successes: AtomicU64,
}

impl<T: Transport> Client<T> {
    pub fn new(transport: T, timeout: Duration) -> Self {
        Self {
            transport,
            timeout,
            max_retries: 3,
            budget: AtomicU32::new(RETRY_BUDGET),
            successes: AtomicU64::new(0),
        }
    }

    /// Send one request, retrying failures with exponential backoff.
    /// The caller's timeout bounds the call.
    pub fn send(&self, req: &Request) -> Result<Vec<u8>, Error> {
        let mut attempt: u32 = 0;
        loop {
            match self.transport.round_trip(req, self.timeout) {
                Ok(body) => {
                    self.successes.fetch_add(1, Ordering::Relaxed);
                    return Ok(body);
                }
                Err(e) => {
                    if attempt >= self.max_retries {
                        return Err(e);
                    }
                    if self.budget.fetch_sub(1, Ordering::Relaxed) == 0 {
                        return Err(e);
                    }
                    // Capped so a long run of failures cannot shift past the
                    // width of the multiplier.
                    let shift = attempt.min(6);
                    std::thread::sleep(BASE_BACKOFF * (1 << shift));
                    attempt += 1;
                }
            }
        }
    }

    /// Requests that eventually succeeded, for the worker's log line.
    pub fn successes(&self) -> u64 {
        self.successes.load(Ordering::Relaxed)
    }
}
