use perps_program::error::PerpError;
use solana_program_error::ProgramError;
use spr_test_utils::mollusk::expect_err_exact;

use crate::common::{
    fixture, setup_mollusk, transact, transact_with_input, wrapper_data_with, Wrapper,
};

#[test]
fn open_of_an_expired_quote_is_rejected_exactly() {
    let (mut mollusk, _) = setup_mollusk();
    let (mut instruction, accounts) = fixture(Wrapper::Open);
    // The transact's expiry is the quote's; the clock is past it.
    let mut data = transact(Vec::new());
    data.expiry_unix_ts = 5;
    instruction.data = wrapper_data_with(Wrapper::Open, data);
    mollusk.sysvars.clock.unix_timestamp = 6;
    expect_err_exact(
        &mollusk,
        &instruction,
        &accounts,
        ProgramError::Custom(PerpError::QuoteExpired as u32),
    );
}

#[test]
fn missing_first_nullifier_is_rejected_exactly() {
    let (mollusk, _) = setup_mollusk();
    let (instruction, accounts) = fixture(Wrapper::Open);
    expect_err_exact(
        &mollusk,
        &instruction,
        &accounts,
        ProgramError::Custom(PerpError::InvalidInstructionData as u32),
    );
}

#[test]
fn oversized_private_tx_hash_fails_hashing_exactly() {
    let (mollusk, _) = setup_mollusk();
    let (mut instruction, accounts) = fixture(Wrapper::Open);
    // 0xFF-filled bytes exceed the BN254 modulus, so the public-input
    // Poseidon hash fails before proof verification.
    let mut data = transact_with_input();
    data.private_tx_hash = [0xFF; 32];
    instruction.data = wrapper_data_with(Wrapper::Open, data);
    expect_err_exact(
        &mollusk,
        &instruction,
        &accounts,
        ProgramError::Custom(PerpError::HashingFailed as u32),
    );
}

#[test]
fn a_placeholder_proof_is_rejected_exactly() {
    let (mollusk, _) = setup_mollusk();
    let (mut instruction, accounts) = fixture(Wrapper::Open);
    instruction.data = wrapper_data_with(Wrapper::Open, transact_with_input());
    expect_err_exact(
        &mollusk,
        &instruction,
        &accounts,
        ProgramError::Custom(PerpError::ProofVerificationFailed as u32),
    );
}
