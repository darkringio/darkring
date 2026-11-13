//! The dark perps circuits: Rust that runs natively on the client to build
//! the SPP transaction, and compiles through Xark for proving.
//!
//! `quote`, `open`, `settle` and `cancel_quote` over the `perp_terms`
//! states, with the settlement math in `payout`.
mod cancel_quote;
mod open;
pub mod payout;
mod perp_terms;
mod quote;
mod settle;
mod shared;

pub use cancel_quote::*;
pub use open::*;
pub use payout::SIZE_SCALE;
pub use perp_terms::*;
pub use quote::*;
pub use settle::*;
