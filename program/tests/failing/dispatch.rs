use mollusk_svm::result::ProgramResult;
use perps_program::error::PerpError;
use solana_instruction::Instruction;
use solana_program_error::ProgramError;
use spr_test_utils::mollusk::expect_err_exact;

use crate::common::{fixture, fixture_transact, setup_mollusk, wrapper_data_with_bump, Wrapper};

#[test]
fn missing_instruction_tag_is_rejected_exactly() {
    let (mollusk, program_id) = setup_mollusk();
    let instruction = Instruction {
        program_id,
        accounts: Vec::new(),
        data: Vec::new(),
    };
    expect_err_exact(
        &mollusk,
        &instruction,
        &[],
        ProgramError::InvalidInstructionData,
    );
}

#[test]
fn unknown_instruction_tags_are_rejected_exactly() {
    let (mollusk, program_id) = setup_mollusk();
    for tag in [0u8, 1, 6, 0xff] {
        let instruction = Instruction {
            program_id,
            accounts: Vec::new(),
            data: vec![tag],
        };
        expect_err_exact(
            &mollusk,
            &instruction,
            &[],
            ProgramError::InvalidInstructionData,
        );
    }
}

/// A bump that derives no program address fails right after decoding, before
/// the marker, window and proof checks, in every instruction.
#[test]
fn invalid_authority_bump_is_rejected_exactly() {
    let (mollusk, _) = setup_mollusk();
    for wrapper in Wrapper::ALL {
        let (mut instruction, accounts) = fixture(wrapper);
        instruction.data =
            wrapper_data_with_bump(wrapper, fixture_transact(wrapper), wrapper.invalid_bump());
        let result = mollusk.process_instruction(&instruction, &accounts);
        assert_eq!(
            result.program_result,
            ProgramResult::Failure(ProgramError::Custom(
                PerpError::InvalidMakerAuthorityBump as u32
            )),
            "{wrapper:?} must reject an invalid authority bump"
        );
    }
}

/// Every instruction refuses truncated arguments and transact data that does
/// not parse: its last byte dropped, or a byte appended.
#[test]
fn malformed_instruction_data_is_rejected_exactly() {
    let (mollusk, _) = setup_mollusk();
    for wrapper in Wrapper::ALL {
        let edits: [fn(&mut Vec<u8>); 3] = [
            |data| data.truncate(4),
            |data| {
                data.pop();
            },
            |data| data.push(0),
        ];
        for edit in edits {
            let (mut instruction, accounts) = fixture(wrapper);
            edit(&mut instruction.data);
            expect_err_exact(
                &mollusk,
                &instruction,
                &accounts,
                ProgramError::Custom(PerpError::InvalidInstructionData as u32),
            );
        }
    }
}
