#[cfg(all(not(feature = "build-circuits"), not(feature = "insecure-test-setup")))]
compile_error!("the perps verifying keys require the insecure-test-setup feature");

#[cfg(feature = "circuits")]
pub mod circuits;

pub mod error;
// The Xark circuit build (`--cfg xark`) compiles only the circuits: the
// instructions and the generated verifiers they call are the Solana program.
#[cfg(all(not(feature = "build-circuits"), not(xark)))]
pub mod instructions;
#[cfg(all(not(feature = "build-circuits"), not(xark)))]
#[rustfmt::skip] // Keep the generator's exact output for `zk compile --check`.
#[allow(clippy::useless_conversion)] // The generator widens every integer, `u128` included.
pub mod verifier;

#[cfg(all(not(feature = "build-circuits"), not(xark)))]
use pinocchio::{address::address_eq, error::ProgramError, AccountView, Address, ProgramResult};

#[cfg(all(not(feature = "build-circuits"), not(xark)))]
use crate::instructions::{
    process_cancel_quote_ix, process_open_ix, process_quote_ix, process_settle_ix,
};

pub mod tag {
    pub const QUOTE: u8 = 2;
    pub const OPEN: u8 = 3;
    pub const SETTLE: u8 = 4;
    pub const CANCEL_QUOTE: u8 = 5;
}

pub const MAKER_AUTHORITY_PDA_SEED: &[u8] = b"maker_authority";

/// The seeds of a maker's authority PDA, `[MAKER_AUTHORITY_PDA_SEED, maker]`.
/// The PDA owns the maker's quote and position UTXOs, so a maker finds them
/// by deriving it.
pub fn maker_authority_seeds(maker: &[u8; 32]) -> [&[u8]; 2] {
    [MAKER_AUTHORITY_PDA_SEED, maker]
}

/// Domain of the open transaction's blinding seed,
/// `Poseidon(DPOP, quote_utxo_blinding)`: the maker derives the position
/// UTXO's blinding from the quote opening and the first nullifier.
pub const OPEN_BLINDING_DOMAIN: u32 = u32::from_be_bytes(*b"DPOP");

/// Domain of the settle transaction's blinding seed,
/// `Poseidon(DPST, position_utxo_blinding)`: either party derives its payout
/// from the position opening and the first nullifier.
pub const SETTLE_BLINDING_DOMAIN: u32 = u32::from_be_bytes(*b"DPST");

#[cfg(all(feature = "bpf-entrypoint", not(feature = "build-circuits"), not(xark)))]
mod entrypoint {
    pinocchio::entrypoint!(crate::process_instruction);
}

pinocchio::address::declare_id!("HY5QBm2yMqKDRnYSU11qi5ZeD3m2hL4WUUT2WLTzvWNF");

#[cfg(all(not(feature = "build-circuits"), not(xark)))]
pub fn process_instruction(
    program_id: &Address,
    accounts: &mut [AccountView],
    instruction_data: &[u8],
) -> ProgramResult {
    if !address_eq(program_id, &crate::ID) {
        return Err(ProgramError::IncorrectProgramId);
    }

    let (ix_tag, ix_data) = instruction_data
        .split_first()
        .ok_or(ProgramError::InvalidInstructionData)?;

    match *ix_tag {
        tag::QUOTE => process_quote_ix(accounts, ix_data),
        tag::OPEN => process_open_ix(accounts, ix_data),
        tag::SETTLE => process_settle_ix(accounts, ix_data),
        tag::CANCEL_QUOTE => process_cancel_quote_ix(accounts, ix_data),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}
