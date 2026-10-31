//! Educational deterministic perpetual-futures exchange core.
//!
//! **Not for real funds.** See the README for what is and is not modelled.
#![forbid(unsafe_code)]
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)
)]
#![warn(missing_docs)]
pub mod error;
pub mod fixed;
pub mod sha256;
pub mod codec;
