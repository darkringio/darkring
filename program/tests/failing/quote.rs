use perps_program::error::PerpError;
use solana_program_error::ProgramError;
use spr_account_checks::AccountError;
use spr_test_utils::mollusk::expect_err_exact;

use crate::common::{
    fixture, marker, setup_mollusk, transact, valid_marker, wrapper_data, wrapper_data_with,
    Wrapper, PAYER, QUOTE_UTXO_HASH,
};

#[test]
fn missing_accounts_are_rejected_exactly() {
    let (mollusk, program_id) = setup_mollusk();
    let instruction = solana_instruction::Instruction {
        program_id,
        accounts: Vec::new(),
        data: wrapper_data(Wrapper::Quote),
    };
    expect_err_exact(
        &mollusk,
        &instruction,
        &[],
        ProgramError::Custom(u32::from(AccountError::NotEnoughAccountKeys)),
    );
}

#[test]
fn readonly_payer_is_rejected_exactly() {
    let (mollusk, _) = setup_mollusk();
    let (mut instruction, accounts) = fixture(Wrapper::Quote);
    // Both metas name the payer, so Solana unions their privileges.
    for index in [0, 1] {
        instruction
            .accounts
            .get_mut(index)
            .expect("payer meta")
            .is_writable = false;
    }
    expect_err_exact(
        &mollusk,
        &instruction,
        &accounts,
        ProgramError::Custom(u32::from(AccountError::AccountNotMutable)),
    );
}

#[test]
fn unsigned_payer_is_rejected_exactly() {
    let (mollusk, _) = setup_mollusk();
    let (mut instruction, accounts) = fixture(Wrapper::Quote);
    for index in [0, 1] {
        instruction
            .accounts
            .get_mut(index)
            .expect("payer meta")
            .is_signer = false;
    }
    expect_err_exact(
        &mollusk,
        &instruction,
        &accounts,
        ProgramError::Custom(u32::from(AccountError::InvalidSigner)),
    );
}

/// The fixture's valid marker passes the marker check and reaches the proof
/// check, which its placeholder proof fails.
#[test]
fn valid_marker_reaches_the_proof_check() {
    let (mollusk, _) = setup_mollusk();
    let (instruction, accounts) = fixture(Wrapper::Quote);
    expect_err_exact(
        &mollusk,
        &instruction,
        &accounts,
        ProgramError::Custom(PerpError::ProofVerificationFailed as u32),
    );
}

/// A marker naming another quote UTXO, another maker, or holding no data
/// fails the marker check.
#[test]
fn wrong_marker_data_is_rejected_exactly() {
    let (mollusk, _) = setup_mollusk();
    let mut empty = valid_marker();
    empty.data.clear();
    let mut extended = valid_marker();
    extended.data.push(0);
    for message in [
        marker([7; 32], PAYER),
        marker(QUOTE_UTXO_HASH, [7; 32]),
        empty,
        extended,
    ] {
        let (mut instruction, accounts) = fixture(Wrapper::Quote);
        instruction.data = wrapper_data_with(Wrapper::Quote, transact(vec![message]));
        expect_err_exact(
            &mollusk,
            &instruction,
            &accounts,
            ProgramError::Custom(PerpError::InvalidMarkerData as u32),
        );
    }
}

/// The quote carries exactly one message: none, or a valid marker with a
/// second message, fails.
#[test]
fn zero_or_two_messages_are_rejected_exactly() {
    let (mollusk, _) = setup_mollusk();
    for messages in [Vec::new(), vec![valid_marker(), valid_marker()]] {
        let (mut instruction, accounts) = fixture(Wrapper::Quote);
        instruction.data = wrapper_data_with(Wrapper::Quote, transact(messages));
        expect_err_exact(
            &mollusk,
            &instruction,
            &accounts,
            ProgramError::Custom(PerpError::InvalidMarkerMessage as u32),
        );
    }
}
