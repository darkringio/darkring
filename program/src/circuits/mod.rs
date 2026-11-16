//! The Darkring circuits: Rust that runs natively on the client to build
//! the SPP transaction, and compiles through Xark for proving.
//!
//! Perps: `quote`, `open`, `settle` and `cancel_quote` over the
//! `perp_terms` states, with the settlement math in `payout`.
//!
//! Dated contracts (options and predictions): `option_quote`,
//! `option_open`, `option_settle` and `option_cancel` over the
//! `option_terms` states, with the settlement math in `option_payout`.
//!
//! The veil: `veil_in` and `veil_out` over `VeilNote`.
mod cancel_quote;
mod open;
mod option_cancel;
mod option_open;
pub mod option_payout;
mod option_quote;
mod option_settle;
mod option_terms;
pub mod payout;
mod perp_terms;
mod quote;
mod settle;
mod shared;
mod veil;

pub use cancel_quote::*;
pub use open::*;
pub use option_cancel::*;
pub use option_open::*;
pub use option_quote::*;
pub use option_settle::*;
pub use option_terms::*;
pub use payout::SIZE_SCALE;
pub use perp_terms::*;
pub use quote::*;
pub use settle::*;
pub use veil::*;
