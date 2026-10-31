//! Application layer: use cases and ports around the `perp-domain` exchange.
//!
//! * [`ports`] declares what the application needs from the outside world
//!   (event log, snapshot store, clock).
//! * [`service`] is the use-case orchestration: recovery, logging events ahead
//!   of applying them, periodic snapshots, replay.
//! * [`api`] is the driving port used by network front ends.
//! * [`fakes`] are in-memory implementations of the ports and [`contract`]
//!   holds the shared contract suites that every implementation must pass.
//!
//! This crate depends on `perp-domain` only and performs no I/O itself.
#![forbid(unsafe_code)]
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )
)]
#![warn(missing_docs)]

pub mod api;
pub mod ports;
pub mod fakes;
