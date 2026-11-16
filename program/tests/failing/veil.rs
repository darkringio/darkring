use perps_program::error::PerpError;
use solana_program_error::ProgramError;
use spr_account_checks::AccountError;
use spr_test_utils::mollusk::expect_err_exact;

use crate::common::{
    setup_mollusk, transact, transact_with_input,
    veil::{canonical_bump, fixture, in_data, invalid_bump, out_data},
};

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

/// Both veil instructions pass their checks and reach the proof check, which
/// the placeholder proof fails.
#[test]
fn each_veil_instruction_reaches_its_proof_check() {
    let (mollusk, _) = setup_mollusk();
    for data in [
        in_data(transact(Vec::new()), canonical_bump()),
        out_data(transact_with_input(), canonical_bump()),
    ] {
        let (instruction, accounts) = fixture(data);
        expect(
            &mollusk,
            &instruction,
            &accounts,
            PerpError::ProofVerificationFailed,
        );
    }
}

#[test]
fn an_unsigned_payer_is_rejected_exactly() {
    let (mollusk, _) = setup_mollusk();
    for data in [
        in_data(transact(Vec::new()), canonical_bump()),
        out_data(transact_with_input(), canonical_bump()),
    ] {
        let (mut instruction, accounts) = fixture(data);
        // The payer is listed twice: as the fee payer and among SPP's.
        let payer = instruction.accounts.first().expect("payer meta").pubkey;
        for meta in &mut instruction.accounts {
            if meta.pubkey == payer {
                meta.is_signer = false;
            }
        }
        expect_err_exact(
            &mollusk,
            &instruction,
            &accounts,
            ProgramError::Custom(u32::from(AccountError::InvalidSigner)),
        );
    }
}

/// A bump that derives no program address fails right after decoding.
#[test]
fn an_invalid_veil_bump_is_rejected_exactly() {
    let (mollusk, _) = setup_mollusk();
    for data in [
        in_data(transact(Vec::new()), invalid_bump()),
        out_data(transact_with_input(), invalid_bump()),
    ] {
        let (instruction, accounts) = fixture(data);
        expect(
            &mollusk,
            &instruction,
            &accounts,
            PerpError::InvalidVeilBump,
        );
    }
}

/// A `veil_out` publishes the nullifier of the note it spends: without an
/// input there is none to prove against.
#[test]
fn a_veil_out_without_an_input_is_rejected_exactly() {
    let (mollusk, _) = setup_mollusk();
    let (instruction, accounts) = fixture(out_data(transact(Vec::new()), canonical_bump()));
    expect(
        &mollusk,
        &instruction,
        &accounts,
        PerpError::InvalidInstructionData,
    );
}

/// Truncated arguments and transact data that does not parse are refused.
#[test]
fn malformed_veil_data_is_rejected_exactly() {
    let (mollusk, _) = setup_mollusk();
    for data in [
        in_data(transact(Vec::new()), canonical_bump()),
        out_data(transact_with_input(), canonical_bump()),
    ] {
        let edits: [fn(&mut Vec<u8>); 3] = [
            |data| data.truncate(4),
            |data| {
                data.pop();
            },
            |data| data.push(0),
        ];
        for edit in edits {
            let (mut instruction, accounts) = fixture(data.clone());
            edit(&mut instruction.data);
            expect(
                &mollusk,
                &instruction,
                &accounts,
                PerpError::InvalidInstructionData,
            );
        }
    }
}
