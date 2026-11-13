use perps_sdk::{
    instructions::{cancel_quote::CancelQuote, open::Open, quote::Quote, settle::Settle},
    maker_authority_pda,
};
use solana_address::Address;
use solana_instruction::AccountMeta;
use spr_interface::{
    instruction::{
        instruction_data::transact::{
            InputUtxo, OwnerTag, TransactIxData, TransactOutput, TreeContext,
        },
        CircuitId, TransactProof,
    },
    N_PUBLIC_SLOTS,
};
use spr_program::{instruction::Transact, CompressedProof};

fn proof() -> CompressedProof {
    CompressedProof {
        a: [0; 32],
        b: [0; 64],
        c: [0; 32],
    }
}

/// A transact of `inputs` inputs and two outputs.
fn transact(inputs: u8) -> TransactIxData {
    TransactIxData {
        expiry_unix_ts: u64::MAX,
        private_tx_hash: [0; 32],
        circuit: CircuitId::ConfidentialSigner(inputs, 2, N_PUBLIC_SLOTS as u8),
        tx_viewing_pk: [2; 33],
        salt: [0; 16],
        proof: TransactProof::zeroed(),
        inputs: (1..=inputs)
            .map(|value| InputUtxo {
                nullifier_hash: [value; 32],
                tree_index: 0,
            })
            .collect(),
        interface_transfers: Vec::new(),
        data_hash: None,
        ring_data_hash: None,
        outputs: [3, 4]
            .map(|value| TransactOutput {
                utxo_hash: [value; 32],
                owner_tag: OwnerTag::Inline([value; 32]),
                data: None,
            })
            .to_vec(),
        messages: Vec::new(),
        tree_contexts: vec![TreeContext {
            utxo_tree_root_index: 0,
            nullifier_tree_root_index: 0,
        }],
    }
}

/// The SPP accounts of `data` with `owner_signers`, where `maker`'s
/// authority does not sign at transaction entry: the program supplies its
/// signature during the CPI.
fn spp_accounts(
    maker: &Address,
    payer: Address,
    tree: Address,
    owner_signers: Vec<Address>,
    data: TransactIxData,
) -> Vec<AccountMeta> {
    let mut accounts = Transact {
        payer,
        input_trees: vec![tree],
        output_tree: tree,
        owner_signers,
        interface_transfer_accounts: Vec::new(),
        data,
    }
    .instruction()
    .accounts;
    for account in &mut accounts {
        if account.pubkey == maker_authority_pda(maker) {
            account.is_signer = false;
        }
    }
    accounts
}

#[test]
fn quote_forwards_the_paying_maker_and_its_authority_as_owner_signers() {
    // The maker owns the spent source; its authority owns the data-bearing
    // quote output.
    let payer = Address::new_unique();
    let tree = Address::new_unique();
    let data = transact(1);
    let expected = spp_accounts(
        &payer,
        payer,
        tree,
        vec![payer, maker_authority_pda(&payer)],
        data.clone(),
    );
    let instruction = Quote {
        payer,
        viewing_pubkey: [2; 33],
        tree,
        spp_proof: data,
        quote_proof: proof(),
    }
    .instruction()
    .expect("quote");
    assert_eq!(instruction.program_id, perps_program::ID);
    assert_eq!(instruction.data.first(), Some(&perps_sdk::tag::QUOTE));
    assert_eq!(
        instruction.accounts.first(),
        Some(&AccountMeta::new(payer, true))
    );
    assert_eq!(instruction.accounts.get(1..), Some(expected.as_slice()));
    // The authority is present and does not sign at transaction entry.
    assert!(instruction
        .accounts
        .iter()
        .any(|account| account.pubkey == maker_authority_pda(&payer) && !account.is_signer));
}

#[test]
fn open_forwards_the_makers_authority_and_the_paying_trader_as_owner_signers() {
    // The maker's authority owns the quote input and the position output;
    // the trader owns its margin input.
    let payer = Address::new_unique();
    let maker = Address::new_unique();
    let tree = Address::new_unique();
    let data = transact(2);
    let expected = spp_accounts(
        &maker,
        payer,
        tree,
        vec![maker_authority_pda(&maker), payer],
        data.clone(),
    );
    let instruction = Open {
        payer,
        maker,
        viewing_pubkey: [2; 33],
        tree,
        spp_proof: data,
        open_proof: proof(),
    }
    .instruction()
    .expect("open");
    assert_eq!(instruction.data.first(), Some(&perps_sdk::tag::OPEN));
    assert_eq!(
        instruction.accounts.first(),
        Some(&AccountMeta::new(payer, true))
    );
    assert_eq!(instruction.accounts.get(1..), Some(expected.as_slice()));
}

#[test]
fn settle_puts_the_signer_and_the_oracle_ahead_of_the_transact_accounts() {
    let payer = Address::new_unique();
    let signer = Address::new_unique();
    let oracle = Address::new_unique();
    let maker = Address::new_unique();
    let tree = Address::new_unique();
    let data = transact(1);
    let expected = spp_accounts(
        &maker,
        payer,
        tree,
        vec![maker_authority_pda(&maker)],
        data.clone(),
    );
    let instruction = Settle {
        payer,
        signer,
        oracle,
        maker,
        tree,
        spp_proof: data,
        settle_proof: proof(),
    }
    .instruction()
    .expect("settle");
    assert_eq!(instruction.data.first(), Some(&perps_sdk::tag::SETTLE));
    assert_eq!(
        instruction.accounts.get(..3),
        Some(
            [
                AccountMeta::new(payer, true),
                AccountMeta::new_readonly(signer, true),
                AccountMeta::new_readonly(oracle, false),
            ]
            .as_slice()
        )
    );
    assert_eq!(instruction.accounts.get(3..), Some(expected.as_slice()));
}

#[test]
fn cancel_puts_the_maker_signer_ahead_of_the_transact_accounts() {
    let payer = Address::new_unique();
    let maker = Address::new_unique();
    let tree = Address::new_unique();
    let data = transact(1);
    let expected = spp_accounts(
        &maker,
        payer,
        tree,
        vec![maker_authority_pda(&maker)],
        data.clone(),
    );
    let instruction = CancelQuote {
        maker,
        payer,
        tree,
        spp_proof: data,
        cancel_proof: proof(),
        quote_expiry: 7,
    }
    .instruction()
    .expect("cancel");
    assert_eq!(
        instruction.data.first(),
        Some(&perps_sdk::tag::CANCEL_QUOTE)
    );
    assert_eq!(
        instruction.accounts.get(..2),
        Some(
            [
                AccountMeta::new(payer, true),
                AccountMeta::new_readonly(maker, true),
            ]
            .as_slice()
        )
    );
    assert_eq!(instruction.accounts.get(2..), Some(expected.as_slice()));
}
