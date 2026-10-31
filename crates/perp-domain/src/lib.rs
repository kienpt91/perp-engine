//! Pure domain layer of an educational perpetual-futures exchange.
//!
//! This crate has no dependencies beyond `std` and performs no I/O: no files,
//! sockets, clock or randomness. Time arrives inside events, so the exchange is
//! a deterministic function of its ordered event stream.
//!
//! **Not for real funds.** See the README for what is and is not modelled.
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

pub mod codec;
pub mod error;
pub mod fixed;
pub mod sha256;
pub mod types;
pub mod views;
