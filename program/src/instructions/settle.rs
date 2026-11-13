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
    instructions::{
        oracle::OraclePrice,
        shared::{IxData, MakerAuthority},
    },
    verifier::settle,
};

pub use spr_program::CompressedProof as SettleProof;

/// The settle arguments the program reads ahead of the transact bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, SchemaRead, SchemaWrite)]
pub struct SettleArgs {
    pub proof: SettleProof,
    /// See [`SettleIxData::maker`].
    pub maker: [u8; 32],
    /// See [`SettleIxData::authority_bump`].
    pub authority_bump: u8,
}

/// The settle instruction data as a client writes it: [`SettleArgs`], then
/// the transact bytes, which the program reads in place ([`IxData`]) and
/// forwards to SPP unchanged.
#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite)]
pub struct SettleIxData {
    pub proof: SettleProof,
    /// The position's maker, whose authority PDA the program signs for. A
    /// wrong maker signs for a PDA that does not own the position UTXO, and
    /// SPP rejects the spend.
    pub maker: [u8; 32],
    /// The bump of `maker`'s authority PDA; see [`MakerAuthority::new`].
    pub authority_bump: u8,
    pub transact: TransactIxData,
}

/// 1. Check the fee payer and the settling signer.
/// 2. Read a fresh price from the oracle account.
/// 3. Decode the instruction and derive the maker's authority PDA.
/// 4. Verify the settle proof against the oracle's feed and price, the
///    signer and the first nullifier. The proof shows the signer may settle
///    at this price: the trader always, the maker only on a liquidation.
/// 5. Invoke SPP with the maker's authority PDA signer, which authorizes the
///    position UTXO spend.
#[inline(never)]
#[profile]
pub fn process_settle_ix(accounts: &mut [AccountView], data: &[u8]) -> ProgramResult {
    // 1. The trader or the maker signs the settle. The proof compares this
    // pubkey's identity with the owners the position commits, tagged as a
    // Solana key like the owner hashes.
    let mut iter = AccountIterator::new(accounts);
    iter.next_signer_mut("payer")?;
    let signer = *iter.next_signer("signer")?.address().as_array();
    let signer_owner_pk_field = solana_owner_identity(&signer).map_err(PerpError::from)?;

    // 2. Read the price. The settler chooses the account; the program takes
    // any fully verified update of the oracle program that is fresh, and the
    // proof checks that its feed is the position's.
    let clock = Clock::get()?;
    let oracle = OraclePrice::read(iter.next_non_mut("oracle")?, clock.unix_timestamp)?;

    // 3. Decode the instruction and derive the authority PDA.
    let IxData {
        args:
            SettleArgs {
                proof,
                maker,
                authority_bump,
            },
        transact,
        transact_bytes,
    } = IxData::<SettleArgs>::parse(data)?;
    let authority = MakerAuthority::new(&maker, authority_bump)?;

    // 4. Verify the settle proof.
    let first_nullifier = transact
        .inputs
        .first()
        .ok_or(PerpError::InvalidInstructionData)?
        .nullifier_hash;
    let public_inputs = settle::PublicInputs {
        feed_hi: oracle.feed_hi,
        feed_lo: oracle.feed_lo,
        price: oracle.price,
        signer_owner_pk_field,
        first_nullifier,
    };
    verify_settle_proof(&proof, public_inputs, transact.private_tx_hash)?;

    // 5. Invoke SPP with the maker's authority PDA signer.
    let spp_accounts = iter.remaining()?;
    authority.cpi_spp_transact_signed(spp_accounts, transact_bytes)
}

/// Hashes the settle proof's public inputs and verifies the proof.
#[inline(never)]
#[profile]
fn verify_settle_proof(
    proof: &SettleProof,
    public_inputs: settle::PublicInputs,
    private_tx_hash: &[u8; 32],
) -> ProgramResult {
    settle::Verifier {
        proof,
        public_inputs,
        private_tx_hash,
        transfers: &[],
    }
    .verify()
    .map_err(PerpError::from)?;
    Ok(())
}
