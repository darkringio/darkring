use perps_program::error::PerpError;
use solana_program_error::ProgramError;
use spr_account_checks::AccountError;
use spr_test_utils::mollusk::expect_err_exact;

use crate::common::{fixture, setup_mollusk, transact, wrapper_data_with, Wrapper};

#[test]
fn cancel_of_an_unexpired_quote_is_rejected_exactly() {
    let (mollusk, _) = setup_mollusk();
    let (instruction, accounts) = fixture(Wrapper::CancelQuote);
    // The fixture commits `quote_expiry = 0` and Mollusk's default clock is
    // 0: cancel requires `now > quote_expiry`.
    expect_err_exact(
        &mollusk,
        &instruction,
        &accounts,
        ProgramError::Custom(PerpError::QuoteNotYetExpired as u32),
    );
}

#[test]
fn unsigned_maker_is_rejected_exactly() {
    let (mollusk, _) = setup_mollusk();
    let (mut instruction, accounts) = fixture(Wrapper::CancelQuote);
    instruction
        .accounts
        .get_mut(1)
        .expect("maker meta")
        .is_signer = false;
    expect_err_exact(
        &mollusk,
        &instruction,
        &accounts,
        ProgramError::Custom(u32::from(AccountError::InvalidSigner)),
    );
}

#[test]
fn oversized_private_tx_hash_fails_hashing_exactly() {
    let (mut mollusk, _) = setup_mollusk();
    let (mut instruction, accounts) = fixture(Wrapper::CancelQuote);
    // Move past the quote expiry so the cancel window passes and the
    // over-modulus public input is what fails.
    mollusk.sysvars.clock.unix_timestamp = 1;
    let mut data = transact(Vec::new());
    data.private_tx_hash = [0xFF; 32];
    instruction.data = wrapper_data_with(Wrapper::CancelQuote, data);
    expect_err_exact(
        &mollusk,
        &instruction,
        &accounts,
        ProgramError::Custom(PerpError::HashingFailed as u32),
    );
}

#[test]
fn a_placeholder_proof_is_rejected_exactly() {
    let (mut mollusk, _) = setup_mollusk();
    let (instruction, accounts) = fixture(Wrapper::CancelQuote);
    mollusk.sysvars.clock.unix_timestamp = 1;
    expect_err_exact(
        &mollusk,
        &instruction,
        &accounts,
        ProgramError::Custom(PerpError::ProofVerificationFailed as u32),
    );
}
