use light_program_profiler::profile;
use pinocchio::{
    sysvars::{clock::Clock, Sysvar},
    AccountView, ProgramResult,
};
use spr_account_checks::AccountIterator;
use spr_hasher::primitives::solana_owner_identity;
use spr_interface::instruction::instruction_data::transact::TransactIxData;
use wincode::{SchemaRead, SchemaWrite};

use crate::{
    error::PerpError,
    instructions::shared::{check_after_window, IxData, MakerAuthority},
    verifier::cancel_quote,
};

pub use spr_program::CompressedProof as CancelQuoteProof;

/// The cancel arguments the program reads ahead of the transact bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, SchemaRead, SchemaWrite)]
pub struct CancelQuoteArgs {
    pub proof: CancelQuoteProof,
    /// See [`CancelQuoteIxData::quote_expiry`].
    pub quote_expiry: u64,
    /// See [`CancelQuoteIxData::authority_bump`].
    pub authority_bump: u8,
}

/// The cancel instruction data as a client writes it: [`CancelQuoteArgs`],
/// then the transact data, which the program reads in place ([`IxData`]) and
/// forwards to SPP unchanged.
#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite)]
pub struct CancelQuoteIxData {
    pub proof: CancelQuoteProof,
    /// The committed quote `expiry` the cancel proof reveals as a public
    /// input. Separate from `transact.expiry_unix_ts` (the SPP relayer
    /// deadline): cancel requires `now > quote_expiry`, which is necessarily
    /// in the past, whereas SPP rejects a `transact` whose `expiry_unix_ts`
    /// is in the past.
    pub quote_expiry: u64,
    /// The bump of the maker's authority PDA; see [`MakerAuthority::new`].
    pub authority_bump: u8,
    pub transact: TransactIxData,
}

/// 1. Check the fee payer and the maker signer.
/// 2. Decode the instruction and derive the maker's authority PDA.
/// 3. Check `now > quote_expiry`.
/// 4. Verify the cancel proof against the expiry and the maker.
/// 5. Invoke SPP with the maker's authority PDA signer.
#[inline(never)]
#[profile]
pub fn process_cancel_quote_ix(accounts: &mut [AccountView], data: &[u8]) -> ProgramResult {
    // 1. The maker signs the cancel; the proof checks the quote's committed
    // maker against this pubkey's identity, so only the maker can cancel.
    let mut iter = AccountIterator::new(accounts);
    iter.next_signer_mut("payer")?;
    let maker = *iter.next_signer("maker")?.address().as_array();
    let maker_owner_pk_field = solana_owner_identity(&maker).map_err(PerpError::from)?;

    // 2. Decode the instruction and derive the authority PDA.
    let IxData {
        args:
            CancelQuoteArgs {
                proof,
                quote_expiry,
                authority_bump,
            },
        transact,
        transact_bytes,
    } = IxData::<CancelQuoteArgs>::parse(data)?;
    let authority = MakerAuthority::new(&maker, authority_bump)?;

    // 3. Check `now > quote_expiry`.
    let clock = Clock::get()?;
    check_after_window(clock.unix_timestamp, quote_expiry)?;

    // 4. Verify the cancel proof.
    let public_inputs = cancel_quote::PublicInputs {
        expiry: quote_expiry,
        maker_owner_pk_field,
    };
    verify_cancel_quote_proof(&proof, public_inputs, transact.private_tx_hash)?;

    // 5. Invoke SPP with the maker's authority PDA signer.
    let spp_accounts = iter.remaining()?;
    authority.cpi_spp_transact_signed(spp_accounts, transact_bytes)
}

/// Hashes the cancel proof's public inputs and verifies the proof.
#[inline(never)]
#[profile]
fn verify_cancel_quote_proof(
    proof: &CancelQuoteProof,
    public_inputs: cancel_quote::PublicInputs,
    private_tx_hash: &[u8; 32],
) -> ProgramResult {
    cancel_quote::Verifier {
        proof,
        public_inputs,
        private_tx_hash,
        transfers: &[],
    }
    .verify()
    .map_err(PerpError::from)?;
    Ok(())
}
