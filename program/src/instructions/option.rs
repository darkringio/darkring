//! The four instructions of a dated contract (an option or a prediction).
//!
//! Each is its perp counterpart with another proof: the same accounts, the
//! same instruction data ([`QuoteArgs`], [`OpenArgs`], [`SettleArgs`],
//! [`CancelQuoteArgs`]) and the same maker authority PDA, which owns a
//! maker's option quotes and option UTXOs as it owns its perp quotes and
//! positions. A UTXO's state commits its type, so a perp proof never opens
//! an option UTXO and an option proof never opens a perp's.
//!
//! What differs is settle: it also hands the proof the time the oracle
//! published its price, which the proof compares with the contract's end.

use light_program_profiler::profile;
use pinocchio::{
    sysvars::{clock::Clock, Sysvar},
    AccountView, ProgramResult,
};
use spr_account_checks::AccountIterator;
use spr_hasher::primitives::solana_owner_identity;
use spr_program::CompressedProof;

use crate::{
    error::PerpError,
    instructions::{
        cancel_quote::CancelQuoteArgs,
        open::OpenArgs,
        oracle::OraclePrice,
        quote::{check_marker, QuoteArgs},
        settle::SettleArgs,
        shared::{check_after_window, check_within_window, IxData, MakerAuthority},
    },
    verifier::{option_cancel, option_open, option_quote, option_settle},
};

/// 1. Check the maker signer.
/// 2. Decode the instruction and derive the maker's authority PDA.
/// 3. Check the marker message names the quote UTXO and the maker.
/// 4. Verify the option quote proof against the transaction, the maker and
///    the quote UTXO's owner, the maker's authority PDA.
/// 5. Invoke SPP with the authority PDA signer.
#[inline(never)]
#[profile]
pub fn process_option_quote_ix(accounts: &mut [AccountView], data: &[u8]) -> ProgramResult {
    // 1. Check the maker signer.
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

    // 3. Check the marker message.
    check_marker(&transact, maker_pubkey)?;

    // 4. Verify the proof.
    let public_inputs = option_quote::PublicInputs {
        maker_owner_pk_field: solana_owner_identity(&maker_pubkey).map_err(PerpError::from)?,
        quote_owner: authority.utxo_owner(&viewing_pubkey)?,
    };
    verify_option_quote_proof(&proof, public_inputs, transact.private_tx_hash)?;

    // 5. Invoke SPP with the maker's authority PDA signer.
    let spp_accounts = iter.remaining()?;
    authority.cpi_spp_transact_signed(spp_accounts, transact_bytes)
}

/// 1. Check the fee payer.
/// 2. Decode the instruction and derive the maker's authority PDA.
/// 3. Check `now <= expiry`, the expiry being the transact's `expiry_unix_ts`.
/// 4. Verify the option open proof against the expiry, the first nullifier
///    and the option UTXO's owner, the maker's authority PDA.
/// 5. Invoke SPP with the maker's authority PDA signer; the trader signs for
///    the premium it owns.
#[inline(never)]
#[profile]
pub fn process_option_open_ix(accounts: &mut [AccountView], data: &[u8]) -> ProgramResult {
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

    // 4. Verify the proof.
    let first_nullifier = transact
        .inputs
        .first()
        .ok_or(PerpError::InvalidInstructionData)?
        .nullifier_hash;
    let public_inputs = option_open::PublicInputs {
        expiry: transact.expiry_unix_ts,
        first_nullifier,
        option_owner: authority.utxo_owner(&viewing_pubkey)?,
    };
    verify_option_open_proof(&proof, public_inputs, transact.private_tx_hash)?;

    // 5. Invoke SPP with the maker's authority PDA signer.
    let spp_accounts = iter.remaining()?;
    authority.cpi_spp_transact_signed(spp_accounts, transact_bytes)
}

/// 1. Check the fee payer and the settling signer.
/// 2. Read a fresh price, and when it was published, from the oracle
///    account.
/// 3. Decode the instruction and derive the maker's authority PDA.
/// 4. Verify the option settle proof against the oracle's feed, price and
///    publish time, the signer and the first nullifier. The proof shows the
///    signer may settle at this time: the trader of a call or a put always,
///    anyone of the two once the contract's term has ended.
/// 5. Invoke SPP with the maker's authority PDA signer.
#[inline(never)]
#[profile]
pub fn process_option_settle_ix(accounts: &mut [AccountView], data: &[u8]) -> ProgramResult {
    // 1. The trader or the maker signs the settle.
    let mut iter = AccountIterator::new(accounts);
    iter.next_signer_mut("payer")?;
    let signer = *iter.next_signer("signer")?.address().as_array();
    let signer_owner_pk_field = solana_owner_identity(&signer).map_err(PerpError::from)?;

    // 2. Read the price and its publish time. The settler chooses the
    // account; the program takes any fully verified update of the oracle
    // program that is fresh, and the proof checks that its feed is the
    // contract's and that it is not older than the contract's end.
    let clock = Clock::get()?;
    let oracle = OraclePrice::read(iter.next_non_mut("oracle")?, clock.unix_timestamp)?;
    let publish_time =
        u64::try_from(oracle.publish_time).map_err(|_| PerpError::InvalidOraclePrice)?;

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

    // 4. Verify the proof.
    let first_nullifier = transact
        .inputs
        .first()
        .ok_or(PerpError::InvalidInstructionData)?
        .nullifier_hash;
    let public_inputs = option_settle::PublicInputs {
        feed_hi: oracle.feed_hi,
        feed_lo: oracle.feed_lo,
        price: oracle.price,
        publish_time,
        signer_owner_pk_field,
        first_nullifier,
    };
    verify_option_settle_proof(&proof, public_inputs, transact.private_tx_hash)?;

    // 5. Invoke SPP with the maker's authority PDA signer.
    let spp_accounts = iter.remaining()?;
    authority.cpi_spp_transact_signed(spp_accounts, transact_bytes)
}

/// 1. Check the fee payer and the maker signer.
/// 2. Decode the instruction and derive the maker's authority PDA.
/// 3. Check `now > quote_expiry`.
/// 4. Verify the option cancel proof against the expiry and the maker.
/// 5. Invoke SPP with the maker's authority PDA signer.
#[inline(never)]
#[profile]
pub fn process_option_cancel_ix(accounts: &mut [AccountView], data: &[u8]) -> ProgramResult {
    // 1. The maker signs the cancel.
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

    // 4. Verify the proof.
    let public_inputs = option_cancel::PublicInputs {
        expiry: quote_expiry,
        maker_owner_pk_field,
    };
    verify_option_cancel_proof(&proof, public_inputs, transact.private_tx_hash)?;

    // 5. Invoke SPP with the maker's authority PDA signer.
    let spp_accounts = iter.remaining()?;
    authority.cpi_spp_transact_signed(spp_accounts, transact_bytes)
}

#[inline(never)]
#[profile]
fn verify_option_quote_proof(
    proof: &CompressedProof,
    public_inputs: option_quote::PublicInputs,
    private_tx_hash: &[u8; 32],
) -> ProgramResult {
    option_quote::Verifier {
        proof,
        public_inputs,
        private_tx_hash,
        transfers: &[],
    }
    .verify()
    .map_err(PerpError::from)?;
    Ok(())
}

#[inline(never)]
#[profile]
fn verify_option_open_proof(
    proof: &CompressedProof,
    public_inputs: option_open::PublicInputs,
    private_tx_hash: &[u8; 32],
) -> ProgramResult {
    option_open::Verifier {
        proof,
        public_inputs,
        private_tx_hash,
        transfers: &[],
    }
    .verify()
    .map_err(PerpError::from)?;
    Ok(())
}

#[inline(never)]
#[profile]
fn verify_option_settle_proof(
    proof: &CompressedProof,
    public_inputs: option_settle::PublicInputs,
    private_tx_hash: &[u8; 32],
) -> ProgramResult {
    option_settle::Verifier {
        proof,
        public_inputs,
        private_tx_hash,
        transfers: &[],
    }
    .verify()
    .map_err(PerpError::from)?;
    Ok(())
}

#[inline(never)]
#[profile]
fn verify_option_cancel_proof(
    proof: &CompressedProof,
    public_inputs: option_cancel::PublicInputs,
    private_tx_hash: &[u8; 32],
) -> ProgramResult {
    option_cancel::Verifier {
        proof,
        public_inputs,
        private_tx_hash,
        transfers: &[],
    }
    .verify()
    .map_err(PerpError::from)?;
    Ok(())
}
