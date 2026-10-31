//! Driving port: what network front ends (the TCP server) may ask of the
//! application. The server adapter depends on this trait only.

use perp_domain::error::Error;
use perp_domain::types::{AccountId, EventBody, MarketId, Output};
use perp_domain::views::{AccountView, BookView, Summary};
use std::fmt;

/// Failure of a use case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// The domain rejected the event (it is still in the log).
    Engine(Error),
    /// The log or snapshot store failed; the event was not applied.
    Storage(String),
    /// The service is in degraded read-only mode (for example the log is full):
    /// queries still work, state-changing events are refused and nothing was
    /// logged or applied. This is recoverable by an operator and is not fatal.
    Degraded(String),
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Engine(e) => write!(f, "{e}"),
            ApiError::Storage(s) => write!(f, "storage: {s}"),
            ApiError::Degraded(s) => write!(f, "read-only: {s}"),
        }
    }
}

impl std::error::Error for ApiError {}

/// Use cases offered to external clients. The implementation owns the clock:
/// it stamps each submitted body with a non-decreasing timestamp.
pub trait ExchangeApi: Send + 'static {
    /// Stamp, log and apply one event body.
    fn submit(&mut self, body: EventBody) -> Result<Vec<Output>, ApiError>;
    /// Top of book for a market.
    fn book(&self, market: MarketId, depth: usize) -> Option<BookView>;
    /// Account state.
    fn account(&self, id: AccountId) -> Option<AccountView>;
    /// Engine-wide counters.
    fn summary(&self) -> Summary;
}
