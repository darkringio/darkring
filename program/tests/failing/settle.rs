use perps_program::{error::PerpError, instructions::oracle::MAX_PRICE_AGE_SECS};
use solana_program_error::ProgramError;
use spr_account_checks::AccountError;
use spr_test_utils::mollusk::expect_err_exact;

use crate::common::{
    account, fixture, oracle_account, price_update, set_oracle, setup_mollusk, transact_with_input,
    wrapper_data_with, Wrapper,
};

/// The fixture settle with one input, so the first nullifier is present.
fn settle() -> (
    solana_instruction::Instruction,
    Vec<(solana_pubkey::Pubkey, solana_account::Account)>,
) {
    let (mut instruction, accounts) = fixture(Wrapper::Settle);
    instruction.data = wrapper_data_with(Wrapper::Settle, transact_with_input());
    (instruction, accounts)
}

fn expect(
    mollusk: &mollusk_svm::Mollusk,
    instruction: &solana_instruction::Instruction,
    accounts: &[(solana_pubkey::Pubkey, solana_account::Account)],
    error: PerpError,
) {
    expect_err_exact(
        mollusk,
        instruction,
        accounts,
        ProgramError::Custom(error as u32),
    );
}

/// A fresh, fully verified price passes every oracle check and reaches the
/// proof check, which the placeholder proof fails.
#[test]
fn a_fresh_price_reaches_the_proof_check() {
    let (mollusk, _) = setup_mollusk();
    let (instruction, accounts) = settle();
    expect(
        &mollusk,
        &instruction,
        &accounts,
        PerpError::ProofVerificationFailed,
    );
}

#[test]
fn unsigned_settler_is_rejected_exactly() {
    let (mollusk, _) = setup_mollusk();
    let (mut instruction, accounts) = settle();
    instruction
        .accounts
        .get_mut(1)
        .expect("settler meta")
        .is_signer = false;
    expect_err_exact(
        &mollusk,
        &instruction,
        &accounts,
        ProgramError::Custom(u32::from(AccountError::InvalidSigner)),
    );
}

/// An account another program owns is not a price, whatever its data.
#[test]
fn an_oracle_account_of_another_owner_is_rejected_exactly() {
    let (mollusk, _) = setup_mollusk();
    let (instruction, mut accounts) = settle();
    let mut forged = account(1_000_000_000);
    forged.data = price_update(100, 0);
    set_oracle(&mut accounts, forged);
    expect(
        &mollusk,
        &instruction,
        &accounts,
        PerpError::InvalidOracleAccount,
    );
}

#[test]
fn an_oracle_account_that_is_not_a_price_update_is_rejected_exactly() {
    let (mollusk, _) = setup_mollusk();
    let mut wrong_discriminator = price_update(100, 0);
    wrong_discriminator[0] ^= 1;
    let mut truncated = price_update(100, 0);
    truncated.truncate(100);
    for data in [wrong_discriminator, truncated, Vec::new()] {
        let (instruction, mut accounts) = settle();
        set_oracle(&mut accounts, oracle_account(data));
        expect(
            &mollusk,
            &instruction,
            &accounts,
            PerpError::InvalidOracleAccount,
        );
    }
}

#[test]
fn a_partially_verified_price_is_rejected_exactly() {
    let (mollusk, _) = setup_mollusk();
    let (instruction, mut accounts) = settle();
    let mut partial = price_update(100, 0);
    partial[8 + 32] = 0;
    set_oracle(&mut accounts, oracle_account(partial));
    expect(
        &mollusk,
        &instruction,
        &accounts,
        PerpError::OracleNotFullyVerified,
    );
}

#[test]
fn a_price_that_is_not_positive_is_rejected_exactly() {
    let (mollusk, _) = setup_mollusk();
    for price in [0, -1] {
        let (instruction, mut accounts) = settle();
        set_oracle(&mut accounts, oracle_account(price_update(price, 0)));
        expect(
            &mollusk,
            &instruction,
            &accounts,
            PerpError::InvalidOraclePrice,
        );
    }
}

/// The price may be at most `MAX_PRICE_AGE_SECS` older than the clock.
#[test]
fn a_stale_price_is_rejected_exactly() {
    let (mut mollusk, _) = setup_mollusk();
    let (instruction, accounts) = settle();
    mollusk.sysvars.clock.unix_timestamp = MAX_PRICE_AGE_SECS;
    expect(
        &mollusk,
        &instruction,
        &accounts,
        PerpError::ProofVerificationFailed,
    );
    mollusk.sysvars.clock.unix_timestamp = MAX_PRICE_AGE_SECS + 1;
    expect(
        &mollusk,
        &instruction,
        &accounts,
        PerpError::StaleOraclePrice,
    );
}

#[test]
fn missing_first_nullifier_is_rejected_exactly() {
    let (mollusk, _) = setup_mollusk();
    let (instruction, accounts) = fixture(Wrapper::Settle);
    expect(
        &mollusk,
        &instruction,
        &accounts,
        PerpError::InvalidInstructionData,
    );
}
