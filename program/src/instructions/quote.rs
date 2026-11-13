use borsh::{BorshDeserialize, BorshSerialize};
use light_program_profiler::profile;
use pinocchio::{AccountView, ProgramResult};
use spr_account_checks::AccountIterator;
use spr_hasher::primitives::solana_owner_identity;
use spr_interface::instruction::instruction_data::transact::{TransactIxData, TransactIxDataRef};
use wincode::{SchemaRead, SchemaWrite};

use crate::{
    error::PerpError,
    instructions::shared::{IxData, MakerAuthority},
    verifier::quote,
};

/// The quote transaction's single message: the quote UTXO's hash and the
/// maker, Borsh-encoded (64 bytes), which the trader's discovery reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshDeserialize, BorshSerialize)]
pub struct QuoteMarkerData {
    pub quote_utxo_hash: [u8; 32],
    pub maker_pubkey: [u8; 32],
}

pub use spr_program::CompressedProof as QuoteProof;

/// The quote arguments the program reads ahead of the transact bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, SchemaRead, SchemaWrite)]
pub struct QuoteArgs {
    pub proof: QuoteProof,
    /// See [`QuoteIxData::viewing_pubkey`].
    pub viewing_pubkey: [u8; 33],
    /// See [`QuoteIxData::authority_bump`].
    pub authority_bump: u8,
}

/// The quote instruction data as a client writes it: [`QuoteArgs`], then the
/// transact data, which the program reads in place ([`IxData`]) and forwards
/// to SPP unchanged.
#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite)]
pub struct QuoteIxData {
    pub proof: QuoteProof,
    /// The viewing key of the quote UTXO's address, the trader's, which the
    /// quote output is encrypted to. The quote proof's public `quote_owner`
    /// is the maker's authority PDA with this viewing key.
    pub viewing_pubkey: [u8; 33],
    /// The bump of the maker's authority PDA; see [`MakerAuthority::new`].
    pub authority_bump: u8,
    /// Carries exactly one message, the marker, whose data is
    /// [`QuoteMarkerData`] of the quote output and the maker signer.
    pub transact: TransactIxData,
}

const QUOTE_OUTPUT_INDEX: usize = 1;

/// 1. Check the maker signer.
/// 2. Decode the instruction and derive the maker's authority PDA.
/// 3. Check the marker message names the quote UTXO and the maker.
/// 4. Verify the quote proof against the transaction, the maker and the
///    quote UTXO's owner, the maker's authority PDA.
/// 5. Invoke SPP with the authority PDA signer, which authorizes the quote
///    output; the maker signs for the source it owns.
#[inline(never)]
#[profile]
pub fn process_quote_ix(accounts: &mut [AccountView], data: &[u8]) -> ProgramResult {
    // 1. Check the maker signer. The quote proof checks the source owner's
    // identity against this pubkey, so the marker names the quote's maker.
    let mut iter = AccountIterator::new(accounts);
    let maker_pubkey = *iter.next_signer_mut("payer")?.address().as_array();

    // 2. Decode the instruction and derive the authority PDA.
    let IxData {
        args:
            QuoteArgs {
                proof,
                viewing_pubkey,
                authority_bump,
            },
        transact,
        transact_bytes,
    } = IxData::<QuoteArgs>::parse(data)?;
    let authority = MakerAuthority::new(&maker_pubkey, authority_bump)?;

    // 3. Check the marker message. SPP hashes the messages into the external
    // data its proof covers, so the program checks the client's marker
    // instead of writing it, and forwards the bytes unchanged.
    check_marker(&transact, maker_pubkey)?;

    // 4. Verify the quote proof against the transaction, the maker and the
    // quote UTXO's owner. SPP accepts a data-bearing output owned by any
    // owner signer, the maker included; the proof binds it to the PDA.
    let public_inputs = quote::PublicInputs {
        maker_owner_pk_field: solana_owner_identity(&maker_pubkey).map_err(PerpError::from)?,
        quote_owner: authority.utxo_owner(&viewing_pubkey)?,
    };
    verify_quote_proof(&proof, public_inputs, transact.private_tx_hash)?;

    // 5. Invoke SPP with the maker's authority PDA signer.
    let spp_accounts = iter.remaining()?;
    authority.cpi_spp_transact_signed(spp_accounts, transact_bytes)
}

/// The transaction carries exactly one message, whose data is the
/// [`QuoteMarkerData`] of the quote output's hash and `maker_pubkey`.
fn check_marker(transact: &TransactIxDataRef, maker_pubkey: [u8; 32]) -> ProgramResult {
    let quote_utxo_hash = transact
        .outputs
        .get(QUOTE_OUTPUT_INDEX)
        .ok_or(PerpError::InvalidInstructionData)?
        .utxo_hash;
    let [marker_message] = transact.messages.as_slice() else {
        return Err(PerpError::InvalidMarkerMessage.into());
    };
    // Compare the Borsh encoding, `quote_utxo_hash || maker_pubkey`, in
    // place. Slice equality also checks the lengths, so the data is 64 bytes.
    let (marker_quote_utxo_hash, marker_maker_pubkey) = marker_message
        .data
        .split_at_checked(32)
        .ok_or(PerpError::InvalidMarkerData)?;
    if marker_quote_utxo_hash != quote_utxo_hash.as_slice()
        || marker_maker_pubkey != maker_pubkey.as_slice()
    {
        return Err(PerpError::InvalidMarkerData.into());
    }
    Ok(())
}

/// Hashes the quote proof's public inputs and verifies the proof.
#[inline(never)]
#[profile]
fn verify_quote_proof(
    proof: &QuoteProof,
    public_inputs: quote::PublicInputs,
    private_tx_hash: &[u8; 32],
) -> ProgramResult {
    quote::Verifier {
        proof,
        public_inputs,
        private_tx_hash,
        transfers: &[],
    }
    .verify()
    .map_err(PerpError::from)?;
    Ok(())
}
