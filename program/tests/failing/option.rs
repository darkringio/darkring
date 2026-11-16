//! The option instructions take their perp counterparts' accounts and data,
//! so each check a perp instruction makes before its proof is repeated here
//! against the option tag. `dispatch.rs` covers the bump and the malformed
//! data for all eight.

use perps_program::{error::PerpError, instructions::oracle::MAX_PRICE_AGE_SECS};
use solana_program_error::ProgramError;
use spr_account_checks::AccountError;
use spr_test_utils::mollusk::expect_err_exact;

use crate::common::{
    fixture, marker, oracle_account, price_update, set_oracle, setup_mollusk, transact,
    transact_with_input, wrapper_data_with, Wrapper, PAYER, QUOTE_UTXO_HASH,
};

type Fixture = (
    solana_instruction::Instruction,
    Vec<(solana_pubkey::Pubkey, solana_account::Account)>,
);

/// The fixture of `wrapper` with what its proof reads present: one input for
/// the instructions that publish the first nullifier.
fn ready(wrapper: Wrapper) -> Fixture {
    let (mut instruction, accounts) = fixture(wrapper);
    if matches!(wrapper, Wrapper::OptionOpen | Wrapper::OptionSettle) {
        instruction.data = wrapper_data_with(wrapper, transact_with_input());
    }
    (instruction, accounts)
}

fn expect(mollusk: &mollusk_svm::Mollusk, fixture: &Fixture, error: PerpError) {
    expect_err_exact(
        mollusk,
        &fixture.0,
        &fixture.1,
        ProgramError::Custom(error as u32),
    );
}

/// Every option instruction passes its checks and reaches its own proof
/// check, which the placeholder proof fails. A cancel needs the clock past
/// the fixture's expiry of zero.
#[test]
fn each_option_instruction_reaches_its_proof_check() {
    let (mut mollusk, _) = setup_mollusk();
    mollusk.sysvars.clock.unix_timestamp = 1;
    for wrapper in Wrapper::OPTIONS {
        expect(
            &mollusk,
            &ready(wrapper),
            PerpError::ProofVerificationFailed,
        );
    }
}

/// A placeholder proof that a perp instruction refuses is refused under the
/// option tag too: the tags verify against different keys, and neither
/// accepts the bytes.
#[test]
fn a_quote_without_its_marker_is_rejected_exactly() {
    let (mollusk, _) = setup_mollusk();
    let (mut instruction, accounts) = fixture(Wrapper::OptionQuote);
    instruction.data = wrapper_data_with(Wrapper::OptionQuote, transact(Vec::new()));
    expect(
        &mollusk,
        &(instruction.clone(), accounts.clone()),
        PerpError::InvalidMarkerMessage,
    );
    // A marker naming another maker.
    instruction.data = wrapper_data_with(
        Wrapper::OptionQuote,
        transact(vec![marker(QUOTE_UTXO_HASH, [99; 32])]),
    );
    expect(
        &mollusk,
        &(instruction.clone(), accounts.clone()),
        PerpError::InvalidMarkerData,
    );
    // A marker naming another output.
    instruction.data = wrapper_data_with(
        Wrapper::OptionQuote,
        transact(vec![marker([98; 32], PAYER)]),
    );
    expect(
        &mollusk,
        &(instruction, accounts),
        PerpError::InvalidMarkerData,
    );
}

#[test]
fn an_open_past_the_quote_expiry_is_rejected_exactly() {
    let (mut mollusk, _) = setup_mollusk();
    let (mut instruction, accounts) = fixture(Wrapper::OptionOpen);
    let mut data = transact_with_input();
    data.expiry_unix_ts = 100;
    instruction.data = wrapper_data_with(Wrapper::OptionOpen, data);
    mollusk.sysvars.clock.unix_timestamp = 100;
    expect(
        &mollusk,
        &(instruction.clone(), accounts.clone()),
        PerpError::ProofVerificationFailed,
    );
    mollusk.sysvars.clock.unix_timestamp = 101;
    expect(&mollusk, &(instruction, accounts), PerpError::QuoteExpired);
}

#[test]
fn a_cancel_before_the_quote_expiry_is_rejected_exactly() {
    let (mollusk, _) = setup_mollusk();
    // The fixture's expiry is zero and the clock starts there.
    expect(
        &mollusk,
        &ready(Wrapper::OptionCancel),
        PerpError::QuoteNotYetExpired,
    );
}

#[test]
fn a_settle_needs_its_signer_and_a_first_nullifier() {
    let (mollusk, _) = setup_mollusk();
    let (mut instruction, accounts) = ready(Wrapper::OptionSettle);
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
    expect(
        &mollusk,
        &fixture(Wrapper::OptionSettle),
        PerpError::InvalidInstructionData,
    );
}

/// An option settles against the same oracle rules as a perp: a fully
/// verified, positive, fresh price of the oracle program.
#[test]
fn a_settle_refuses_a_bad_oracle_price_exactly() {
    let (mut mollusk, _) = setup_mollusk();

    let (instruction, mut accounts) = ready(Wrapper::OptionSettle);
    let mut forged = crate::common::account(1_000_000_000);
    forged.data = price_update(100, 0);
    set_oracle(&mut accounts, forged);
    expect(
        &mollusk,
        &(instruction, accounts),
        PerpError::InvalidOracleAccount,
    );

    let (instruction, mut accounts) = ready(Wrapper::OptionSettle);
    let mut partial = price_update(100, 0);
    partial[8 + 32] = 0;
    set_oracle(&mut accounts, oracle_account(partial));
    expect(
        &mollusk,
        &(instruction, accounts),
        PerpError::OracleNotFullyVerified,
    );

    for price in [0, -1] {
        let (instruction, mut accounts) = ready(Wrapper::OptionSettle);
        set_oracle(&mut accounts, oracle_account(price_update(price, 0)));
        expect(
            &mollusk,
            &(instruction, accounts),
            PerpError::InvalidOraclePrice,
        );
    }

    // A price published before the epoch has no publish time to prove with.
    let (instruction, mut accounts) = ready(Wrapper::OptionSettle);
    set_oracle(&mut accounts, oracle_account(price_update(100, -1)));
    expect(
        &mollusk,
        &(instruction, accounts),
        PerpError::InvalidOraclePrice,
    );

    mollusk.sysvars.clock.unix_timestamp = MAX_PRICE_AGE_SECS + 1;
    expect(
        &mollusk,
        &ready(Wrapper::OptionSettle),
        PerpError::StaleOraclePrice,
    );
}
