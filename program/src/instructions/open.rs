use light_program_profiler::profile;
use pinocchio::{
    sysvars::{clock::Clock, Sysvar},
    AccountView, ProgramResult,
};
use spr_account_checks::AccountIterator;
use spr_interface::instruction::instruction_data::transact::TransactIxData;
use wincode::{SchemaRead, SchemaWrite};

use crate::{
    error::PerpError,
    instructions::shared::{check_within_window, IxData, MakerAuthority},
    verifier::open,
};

pub use spr_program::CompressedProof as OpenProof;

/// The open arguments the program reads ahead of the transact bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, SchemaRead, SchemaWrite)]
pub struct OpenArgs {
    pub proof: OpenProof,
    /// See [`OpenIxData::maker`].
    pub maker: [u8; 32],
    /// See [`OpenIxData::viewing_pubkey`].
    pub viewing_pubkey: [u8; 33],
    /// See [`OpenIxData::authority_bump`].
    pub authority_bump: u8,
}

/// The open instruction data as a client writes it: [`OpenArgs`], then the
/// transact bytes, which the program reads in place ([`IxData`]) and forwards
/// to SPP unchanged.
#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite)]
pub struct OpenIxData {
    pub proof: OpenProof,
    /// The quote's maker (the marker's `maker_pubkey`), whose authority PDA
    /// the program signs for. A wrong maker signs for a PDA that does not
    /// own the quote UTXO, and SPP rejects the spend.
    pub maker: [u8; 32],
    /// The viewing key of the position UTXO's address, the maker's, which
    /// the position output is encrypted to. The open proof's public
    /// `position_owner` is the maker's authority PDA with this viewing key.
    pub viewing_pubkey: [u8; 33],
    /// The bump of `maker`'s authority PDA; see [`MakerAuthority::new`].
    pub authority_bump: u8,
    pub transact: TransactIxData,
}

/// 1. Check the fee payer.
/// 2. Decode the instruction and derive the maker's authority PDA.
/// 3. Check `now <= expiry`, the expiry being the transact's `expiry_unix_ts`.
/// 4. Verify the open proof against the expiry, the first nullifier and the
///    position UTXO's owner, the maker's authority PDA.
/// 5. Invoke SPP with the maker's authority PDA signer, which authorizes the
///    quote UTXO spend and the position output; the trader signs for the
///    margin it owns.
#[inline(never)]
#[profile]
pub fn process_open_ix(accounts: &mut [AccountView], data: &[u8]) -> ProgramResult {
    // 1. Check the fee payer.
    let mut iter = AccountIterator::new(accounts);
    iter.next_signer_mut("payer")?;

    // 2. Decode the instruction and derive the authority PDA.
    let IxData {
        args:
            OpenArgs {
                proof,
                maker,
                viewing_pubkey,
                authority_bump,
            },
        transact,
        transact_bytes,
    } = IxData::<OpenArgs>::parse(data)?;
    let authority = MakerAuthority::new(&maker, authority_bump)?;

    // 3. Check `now <= expiry`.
    let clock = Clock::get()?;
    check_within_window(clock.unix_timestamp, transact.expiry_unix_ts)?;

    // 4. Verify the open proof. SPP accepts a data-bearing output owned by
    // any owner signer, the trader included; the proof binds the position
    // UTXO to the PDA.
    let first_nullifier = transact
        .inputs
        .first()
        .ok_or(PerpError::InvalidInstructionData)?
        .nullifier_hash;
    let public_inputs = open::PublicInputs {
        expiry: transact.expiry_unix_ts,
        first_nullifier,
        position_owner: authority.utxo_owner(&viewing_pubkey)?,
    };
    verify_open_proof(&proof, public_inputs, transact.private_tx_hash)?;

    // 5. Invoke SPP with the maker's authority PDA signer.
    let spp_accounts = iter.remaining()?;
    authority.cpi_spp_transact_signed(spp_accounts, transact_bytes)
}

/// Hashes the open proof's public inputs and verifies the proof.
#[inline(never)]
#[profile]
fn verify_open_proof(
    proof: &OpenProof,
    public_inputs: open::PublicInputs,
    private_tx_hash: &[u8; 32],
) -> ProgramResult {
    open::Verifier {
        proof,
        public_inputs,
        private_tx_hash,
        transfers: &[],
    }
    .verify()
    .map_err(PerpError::from)?;
    Ok(())
}
