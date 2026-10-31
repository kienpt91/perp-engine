//! Infrastructure adapters: they implement the ports of `perp-app` (file
//! write-ahead log, snapshot files) and drive its use cases (TCP server).
//!
//! Everything here is `std` only: no async runtime, no third-party crates.
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

pub mod json;
pub mod token;
pub mod protocol;
