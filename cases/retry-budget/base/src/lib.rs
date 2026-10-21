//! The HTTP client the billing workers use to reach the payments API.

use std::time::Duration;

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
}

impl<T: Transport> Client<T> {
    pub fn new(transport: T, timeout: Duration) -> Self {
        Self { transport, timeout }
    }

    /// Send one request. The caller's timeout bounds the call.
    pub fn send(&self, req: &Request) -> Result<Vec<u8>, Error> {
        self.transport.round_trip(req, self.timeout)
    }
}
