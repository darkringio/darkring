use crate::shared::*;
use perps_program::verifier::open::{PublicInputs, Verifier, VERIFYINGKEY};
use perps_sdk::{
    instructions::open::{OpenTransactionParams, CHANGE_SLOT, POSITION_SLOT},
    proof::CircuitId,
    shared::open_blinding_seed,
};
use spr_program::CompressedProof;

const CIRCUIT: CircuitId = CircuitId::Open;

/// An open of a quote designated to `trader` (or open to anyone with
/// `designated: false`), sent by `trader`, whose margin input holds
/// `margin_input`.
fn inputs(designated: bool, margin_input: u64) -> Open {
    let trader = keypair();
    let quote = quote_utxo(quote_terms(
        &keypair(),
        designated.then(|| identity_of(&trader)),
    ));
    open_inputs(&quote, trader, margin_input)
}

#[test]
fn program_vk_has_one_public_input() {
    assert_eq!(VERIFYINGKEY.nr_pubinputs, 1);
    assert!(VERIFYINGKEY.vk_commitment.is_none());
    assert_eq!(VERIFYINGKEY.vk_ic.len(), 2);
}

#[test]
fn designated_open_proves_and_binds_public_inputs() {
    // 1. Both margins move into the position; the rest returns as change.
    let inputs = inputs(true, TRADER_MARGIN + 3 * USDC);
    let finalized = inputs.create_finalized_transaction().unwrap();
    let outputs = finalized.output_utxos();
    let position = outputs.get(POSITION_SLOT).unwrap();
    let change = outputs.get(CHANGE_SLOT).unwrap();
    assert_eq!(
        (
            position.asset,
            position.amount,
            change.asset,
            change.amount,
            change.owner_address,
        ),
        (
            collateral_mint(),
            MAKER_MARGIN + TRADER_MARGIN,
            collateral_mint(),
            3 * USDC,
            Some(inputs.sender_address()),
        )
    );
    let position_owner = position.owner_address.unwrap();
    assert_eq!(
        position_owner.owner_hash().unwrap(),
        field_bytes(inputs.program.public.position_owner.owner().hash().unwrap())
    );

    // 2. Prove, and verify with the program's verifier from the transact's
    // expiry, first nullifier and private transaction hash, and the position
    // owner the program derives from the maker.
    let (_, proof) = prove_and_verify(CIRCUIT, &inputs, &VERIFYINGKEY);
    let private_tx_hash = finalized.padding_independent_private_tx_hash().unwrap();
    let first_nullifier = finalized.first_nullifier().unwrap();
    assert_eq!(
        first_nullifier,
        field_bytes(inputs.program.public.first_nullifier)
    );
    let owner = field_bytes(inputs.program.public.position_owner.hash().unwrap());
    assert!(verify(
        &proof,
        EXPIRY,
        first_nullifier,
        owner,
        &private_tx_hash
    ));

    // 3. Another expiry, first nullifier, position owner or transaction
    // fails. A position under another maker's authority, or under the
    // trader's own key, is another owner.
    let mut tampered = private_tx_hash;
    tampered[31] ^= 0x01;
    let other_maker = keypair();
    let other_owner = spr_program::verifier::address_hash(
        &authority_address(&other_maker, &other_maker)
            .owner_hash()
            .unwrap(),
        other_maker.viewing_pubkey().as_bytes(),
    )
    .unwrap();
    assert!(!verify(
        &proof,
        EXPIRY + 1,
        first_nullifier,
        owner,
        &private_tx_hash
    ));
    assert!(!verify(&proof, EXPIRY, fe(17), owner, &private_tx_hash));
    assert!(!verify(
        &proof,
        EXPIRY,
        first_nullifier,
        other_owner,
        &private_tx_hash
    ));
    assert!(!verify(&proof, EXPIRY, first_nullifier, owner, &tampered));
}

#[test]
fn the_position_commits_the_quote_terms_and_the_trader() {
    let quote = quote_utxo(quote_terms(&keypair(), None));
    let inputs = open_inputs(&quote, keypair(), TRADER_MARGIN);
    let finalized = inputs.create_finalized_transaction().unwrap();
    let trader = inputs.sender_address();
    // The maker reads the same position from the quote opening: the quote's
    // terms, with the margin input's owner as the trader.
    let derived = quote
        .derived_position(trader, &finalized.first_nullifier().unwrap())
        .unwrap();
    assert_eq!(derived.terms, quote.terms.position_terms(trader));
    assert_eq!(
        derived.hash(OUTPUT_TREE_ID).unwrap(),
        finalized
            .output_utxos()
            .get(POSITION_SLOT)
            .unwrap()
            .hash(OUTPUT_TREE_ID)
            .unwrap()
    );
    // Another trader's address commits another position.
    let other = quote
        .derived_position(
            keypair().shielded_address().unwrap(),
            &finalized.first_nullifier().unwrap(),
        )
        .unwrap();
    assert_ne!(
        other.hash(OUTPUT_TREE_ID).unwrap(),
        derived.hash(OUTPUT_TREE_ID).unwrap()
    );
}

#[test]
fn open_quote_is_accepted_by_an_arbitrary_trader() {
    // No designated trader accepts any margin input owner.
    let inputs = inputs(false, TRADER_MARGIN);
    assert_eq!(inputs.program.private.terms.taker_pk_fe, [0; 32]);
    prove_and_verify(CIRCUIT, &inputs, &VERIFYINGKEY);
}

#[test]
fn an_exact_margin_input_leaves_no_change() {
    let inputs = inputs(true, TRADER_MARGIN);
    let finalized = inputs.create_finalized_transaction().unwrap();
    let outputs = finalized.output_utxos();
    assert_eq!(
        outputs.get(POSITION_SLOT).unwrap().amount,
        MAKER_MARGIN + TRADER_MARGIN
    );
    assert_valid(CIRCUIT, &inputs);
}

#[test]
fn designated_quote_rejects_another_trader() {
    // 1. Designate one trader and open with another.
    let quote = quote_utxo(quote_terms(&keypair(), Some(identity_of(&keypair()))));
    let inputs = open_inputs(&quote, keypair(), TRADER_MARGIN);
    // 2. Native execution and the compiled circuit both refuse it.
    assert_invalid(CIRCUIT, &inputs);
    // 3. The SDK refuses to build it.
    let trader = inputs.sender;
    let error = OpenTransactionParams {
        quote_utxo: quote,
        quote_leaf_index: QUOTE_LEAF_INDEX,
        trader_input: wallet_utxo(&trader, collateral_mint(), TRADER_MARGIN),
        payer: solana_address::Address::default(),
        tree_id: INPUT_TREE_ID,
    }
    .build(&trader)
    .err()
    .expect("another trader must be refused");
    assert!(error.to_string().contains("designates another trader"));
}

#[test]
fn open_rejects_an_unrecoverable_blinding_seed_and_first_nullifier() {
    // A seed other than the quote's, or a first nullifier other than the
    // published one, would hide the position from the quote opening.
    let mutations: [fn(&mut Open); 3] = [
        |inputs| inputs.program.private.tx_context = context(&fe(29)),
        |inputs| inputs.program.public.first_nullifier = field(17),
        |inputs| inputs.program.private.quote_utxo.blinding = field(29),
    ];
    for mutate in mutations {
        let mut inputs = inputs(true, TRADER_MARGIN);
        mutate(&mut inputs);
        assert_invalid(CIRCUIT, &inputs);
    }
}

#[test]
fn open_rejects_wrong_terms_and_margin_input() {
    // Tamper one term, quote UTXO or margin input constraint at a time.
    let mutations: [fn(&mut Open); 12] = [
        |inputs| inputs.program.public.expiry = EXPIRY + 1,
        |inputs| inputs.program.private.terms.size += 1,
        |inputs| inputs.program.private.terms.size = 0,
        |inputs| inputs.program.private.terms.entry_price += 1,
        |inputs| inputs.program.private.terms.trader_margin -= 1,
        |inputs| inputs.program.private.terms.maintenance_margin = 0,
        |inputs| inputs.program.private.terms.trader_is_long = false,
        |inputs| inputs.program.private.terms.feed_lo ^= 1,
        |inputs| {
            let other = keypair().shielded_address().unwrap();
            inputs.program.private.terms.maker = *inputs.address(other).owner();
        },
        |inputs| inputs.program.private.quote_utxo.data_hash = field(99),
        // A margin input of another asset, or smaller than the margin.
        |inputs| inputs.program.private.trader_in.asset = [9; 32],
        |inputs| inputs.program.private.trader_in.amount = TRADER_MARGIN - 1,
    ];
    for mutate in mutations {
        let mut inputs = inputs(true, TRADER_MARGIN);
        mutate(&mut inputs);
        assert_invalid(CIRCUIT, &inputs);
    }
}

#[test]
fn maker_derives_the_position_without_the_open_ciphertext() {
    // 1. Build the open through the SDK.
    let trader = keypair();
    let quote = quote_utxo(quote_terms(&keypair(), Some(identity_of(&trader))));
    let transaction = OpenTransactionParams {
        quote_utxo: quote.clone(),
        quote_leaf_index: QUOTE_LEAF_INDEX,
        trader_input: wallet_utxo(&trader, collateral_mint(), TRADER_MARGIN + USDC),
        payer: solana_address::Address::default(),
        tree_id: INPUT_TREE_ID,
    }
    .build(&trader)
    .unwrap();
    assert_eq!(
        transaction.spp_proof_inputs.external_data.expiry_unix_ts,
        EXPIRY
    );
    assert_eq!(
        open_blinding_seed(&quote.blinding).unwrap(),
        transaction.spp_proof_inputs.blinding_seed
    );

    // 2. The maker derives the position from the quote opening, the trader's
    // address and the first nullifier alone.
    let committed = transaction
        .spp_proof_inputs
        .external_data
        .outputs
        .get(POSITION_SLOT)
        .unwrap()
        .utxo_hash;
    let derived = quote
        .derived_position(
            trader.shielded_address().unwrap(),
            &transaction.first_nullifier,
        )
        .unwrap();
    assert_eq!(derived.hash(INPUT_TREE_ID).unwrap(), committed);
    assert_eq!(derived, transaction.position_utxo);
    assert_eq!(derived.total, MAKER_MARGIN + TRADER_MARGIN);

    // 3. Another first nullifier or trader derives another position.
    let wrong = quote
        .derived_position(trader.shielded_address().unwrap(), &fe(17))
        .unwrap();
    assert_ne!(wrong.hash(INPUT_TREE_ID).unwrap(), committed);
    let wrong = quote
        .derived_position(
            keypair().shielded_address().unwrap(),
            &transaction.first_nullifier,
        )
        .unwrap();
    assert_ne!(wrong.hash(INPUT_TREE_ID).unwrap(), committed);
}

fn verify(
    proof: &CompressedProof,
    expiry: u64,
    first_nullifier: [u8; 32],
    position_owner: [u8; 32],
    private_tx_hash: &[u8; 32],
) -> bool {
    Verifier {
        proof,
        public_inputs: PublicInputs {
            expiry,
            first_nullifier,
            position_owner,
        },
        private_tx_hash,
        transfers: &[],
    }
    .verify()
    .is_ok()
}
