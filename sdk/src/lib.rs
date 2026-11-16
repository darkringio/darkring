//! Client SDK for the Darkring program: quote and position state,
//! transaction builders, Xark proofs, instruction builders and discovery.
pub mod index;
pub mod instructions;
pub mod party;
pub mod proof;
pub mod shared;
pub mod state;

pub use party::{Party, ViewKeys};
/// The program's wire types, which the instruction builders' fields name.
pub use perps_program::{
    circuits::SIZE_SCALE,
    instructions::{
        cancel_quote::{CancelQuoteIxData, CancelQuoteProof},
        open::{OpenIxData, OpenProof},
        oracle::{encode_price_update, OraclePrice, MAX_PRICE_AGE_SECS, ORACLE_PROGRAM_ID},
        quote::{QuoteIxData, QuoteMarkerData, QuoteProof},
        settle::{SettleIxData, SettleProof},
        veil::{VeilInIxData, VeilOutIxData, VeilProof},
    },
    maker_authority_seeds, tag, MAKER_AUTHORITY_PDA_SEED, VEIL_PDA_SEED,
};
use solana_address::Address;
use spr_program::ProgramOwner;

/// The authority PDA of `maker`, `[MAKER_AUTHORITY_PDA_SEED, maker]`, which
/// the program signs with (`invoke_signed`) to spend the maker's quote and
/// position UTXOs. It owns them, holds no data, and is never created.
pub fn maker_authority_pda(maker: &Address) -> Address {
    *maker_authority(maker).pda()
}

/// The canonical bump of `maker`'s authority PDA, which the instruction
/// builders pass as `authority_bump`.
pub fn maker_authority_bump(maker: &Address) -> u8 {
    Address::find_program_address(&maker_authority_seeds(maker.as_array()), &perps_program::ID).1
}

/// The authority PDA of `maker` as a UTXO owner, under the zero nullifier
/// key. Its view tag finds the maker's quote and position UTXOs.
pub fn maker_authority(maker: &Address) -> ProgramOwner {
    ProgramOwner::find(&maker_authority_seeds(maker.as_array()), &perps_program::ID)
}

/// The veil PDA, `[VEIL_PDA_SEED]`: the one owner of every veil note.
pub fn veil_pda() -> Address {
    *veil_authority().pda()
}

/// The canonical bump of the veil PDA, which the instruction builders pass.
pub fn veil_bump() -> u8 {
    Address::find_program_address(&[VEIL_PDA_SEED], &perps_program::ID).1
}

/// The veil PDA as a UTXO owner, under the zero nullifier key. Its view tag
/// finds every veil note.
pub fn veil_authority() -> ProgramOwner {
    ProgramOwner::find(&[VEIL_PDA_SEED], &perps_program::ID)
}

pub(crate) fn err(e: impl core::fmt::Debug) -> anyhow::Error {
    anyhow::anyhow!("{e:?}")
}
