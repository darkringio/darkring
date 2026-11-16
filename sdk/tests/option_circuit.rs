//! The four circuits of a dated contract: an option or a prediction.

use crate::shared::*;
use perps_program::verifier::{option_cancel, option_open, option_quote, option_settle};
use perps_sdk::{
    proof::CircuitId,
    shared::feed_halves,
    state::{OptionUtxo, KIND_ABOVE, KIND_BELOW, KIND_CALL, KIND_PUT},
};
use spr_keypair::ShieldedKeypair;
use spr_program::CompressedProof;

const KINDS: [u8; 4] = [KIND_CALL, KIND_PUT, KIND_ABOVE, KIND_BELOW];

/// A contract of `kind` and its two parties.
fn parties(kind: u8) -> (OptionUtxo, ShieldedKeypair, ShieldedKeypair) {
    let maker = keypair();
    let trader = keypair();
    (option_utxo(&maker, &trader, kind), maker, trader)
}

/// The amounts of the two payouts a settle of `inputs` creates.
fn payouts(inputs: &OptionSettle) -> (u64, u64) {
    let finalized = inputs.create_finalized_transaction().unwrap();
    let outputs = finalized.output_utxos();
    assert_eq!(outputs.len(), 2, "a settle always creates both payouts");
    let terms = &inputs.program.private.terms;
    assert_eq!(
        spr_program::to_bytes(&terms.trader.hash().unwrap()),
        outputs[0].owner_address.unwrap().owner_hash().unwrap()
    );
    assert_eq!(
        spr_program::to_bytes(&terms.maker.hash().unwrap()),
        outputs[1].owner_address.unwrap().owner_hash().unwrap()
    );
    (outputs[0].amount, outputs[1].amount)
}

#[test]
fn each_option_vk_has_one_public_input() {
    for key in [
        &option_quote::VERIFYINGKEY,
        &option_open::VERIFYINGKEY,
        &option_settle::VERIFYINGKEY,
        &option_cancel::VERIFYINGKEY,
    ] {
        assert_eq!(key.nr_pubinputs, 1);
        assert!(key.vk_commitment.is_none());
    }
}

#[test]
fn a_quote_of_every_kind_locks_the_collateral_and_proves() {
    for kind in KINDS {
        let inputs = option_quote_inputs(kind, COLLATERAL, 750);
        let outputs = inputs.create_finalized_transaction().unwrap();
        let outputs = outputs.output_utxos();
        // The change, then the quote.
        assert_eq!((outputs[0].amount, outputs[1].amount), (750, COLLATERAL));
        assert_valid(CircuitId::OptionQuote, &inputs);
    }
    // One full proof, checked by the program's verifier.
    let inputs = option_quote_inputs(KIND_ABOVE, COLLATERAL, 0);
    let (tx, proof) =
        prove_and_verify(CircuitId::OptionQuote, &inputs, &option_quote::VERIFYINGKEY);
    let private_tx_hash = tx.finalized.padding_independent_private_tx_hash().unwrap();
    let quote_owner = field_bytes(inputs.program.public.quote_owner.hash().unwrap());
    let verifies = |maker, owner| {
        option_quote::Verifier {
            proof: &proof,
            public_inputs: option_quote::PublicInputs {
                maker_owner_pk_field: maker,
                quote_owner: owner,
            },
            private_tx_hash: &private_tx_hash,
            transfers: &[],
        }
        .verify()
        .is_ok()
    };
    assert!(verifies(identity_of(&inputs.sender), quote_owner));
    // Another maker, or a quote owned by anything but its authority, fails.
    assert!(!verifies(identity_of(&keypair()), quote_owner));
    assert!(!verifies(identity_of(&inputs.sender), fe(3)));
}

#[test]
fn a_quote_the_circuit_refuses() {
    // No collateral.
    let mut inputs = option_quote_inputs(KIND_CALL, COLLATERAL, 0);
    inputs.program.private.collateral = 0;
    assert_invalid(CircuitId::OptionQuote, &inputs);
    // More collateral than the source holds.
    let mut inputs = option_quote_inputs(KIND_CALL, COLLATERAL, 0);
    inputs.program.private.collateral = COLLATERAL + 1;
    assert_invalid(CircuitId::OptionQuote, &inputs);
    // No such kind.
    let mut inputs = option_quote_inputs(KIND_CALL, COLLATERAL, 0);
    inputs.program.private.kind = 4;
    assert_invalid(CircuitId::OptionQuote, &inputs);
    // No strike, no premium, no end.
    let mut inputs = option_quote_inputs(KIND_CALL, COLLATERAL, 0);
    inputs.program.private.strike = 0;
    assert_invalid(CircuitId::OptionQuote, &inputs);
    let mut inputs = option_quote_inputs(KIND_CALL, COLLATERAL, 0);
    inputs.program.private.premium = 0;
    assert_invalid(CircuitId::OptionQuote, &inputs);
    let mut inputs = option_quote_inputs(KIND_ABOVE, COLLATERAL, 0);
    inputs.program.private.term_end = 0;
    assert_invalid(CircuitId::OptionQuote, &inputs);
    // A call of no size; a prediction needs none.
    let mut inputs = option_quote_inputs(KIND_CALL, COLLATERAL, 0);
    inputs.program.private.size = 0;
    assert_invalid(CircuitId::OptionQuote, &inputs);
    let mut inputs = option_quote_inputs(KIND_BELOW, COLLATERAL, 0);
    inputs.program.private.size = 0;
    assert_valid(CircuitId::OptionQuote, &inputs);
    // A maker that is not the source's owner.
    let mut inputs = option_quote_inputs(KIND_CALL, COLLATERAL, 0);
    inputs.program.public.maker_owner_pk_field = identity_field(&keypair());
    assert_invalid(CircuitId::OptionQuote, &inputs);
}

#[test]
fn an_open_moves_the_premium_and_the_collateral_into_one_contract() {
    for kind in KINDS {
        let maker = keypair();
        let trader = keypair();
        let quote = option_quote_utxo(option_quote_terms(&maker, kind, None));
        let inputs = option_open_inputs(&quote, trader, PREMIUM + 900);
        let finalized = inputs.create_finalized_transaction().unwrap();
        let outputs = finalized.output_utxos();
        // The contract, then the trader's change.
        assert_eq!(
            (outputs[0].amount, outputs[1].amount),
            (PREMIUM + COLLATERAL, 900)
        );
        // The maker derives the same contract from the quote opening.
        let derived = quote
            .derived_option(
                inputs.sender_address(),
                &finalized.first_nullifier().unwrap(),
            )
            .unwrap();
        assert_eq!(
            outputs[0].hash(OUTPUT_TREE_ID).unwrap(),
            derived.hash(OUTPUT_TREE_ID).unwrap()
        );
        assert_valid(CircuitId::OptionOpen, &inputs);
    }
    let maker = keypair();
    let quote = option_quote_utxo(option_quote_terms(&maker, KIND_PUT, None));
    let inputs = option_open_inputs(&quote, keypair(), PREMIUM);
    prove_and_verify(CircuitId::OptionOpen, &inputs, &option_open::VERIFYINGKEY);
}

#[test]
fn an_open_the_circuit_refuses() {
    let maker = keypair();
    // A premium input that does not cover the premium.
    let quote = option_quote_utxo(option_quote_terms(&maker, KIND_CALL, None));
    assert_invalid(
        CircuitId::OptionOpen,
        &option_open_inputs(&quote, keypair(), PREMIUM - 1),
    );
    // A trader the quote does not designate.
    let designated = keypair();
    let quote = option_quote_utxo(option_quote_terms(
        &maker,
        KIND_CALL,
        Some(identity_of(&designated)),
    ));
    assert_invalid(
        CircuitId::OptionOpen,
        &option_open_inputs(&quote, keypair(), PREMIUM),
    );
    assert_valid(
        CircuitId::OptionOpen,
        &option_open_inputs(&quote, designated, PREMIUM),
    );
    // Another expiry than the quote's.
    let quote = option_quote_utxo(option_quote_terms(&maker, KIND_CALL, None));
    let mut inputs = option_open_inputs(&quote, keypair(), PREMIUM);
    inputs.program.public.expiry += 1;
    assert_invalid(CircuitId::OptionOpen, &inputs);
    // A blinding seed that is not the quote's.
    let mut inputs = option_open_inputs(&quote, keypair(), PREMIUM);
    inputs.program.private.tx_context = context(&fe(99));
    assert_invalid(CircuitId::OptionOpen, &inputs);
    // Terms other than the ones the quote commits.
    let mut inputs = option_open_inputs(&quote, keypair(), PREMIUM);
    inputs.program.private.terms.strike += 1;
    assert_invalid(CircuitId::OptionOpen, &inputs);
}

#[test]
fn a_settle_pays_what_the_contract_is_worth() {
    // (kind, price, trader, maker) on 4 of premium and 26 of collateral.
    let cases = [
        (KIND_CALL, 90 * USD, 0, 30 * USDC),
        (KIND_CALL, 100 * USD, 0, 30 * USDC),
        (KIND_CALL, 107 * USD, 7 * USDC, 23 * USDC),
        (KIND_CALL, 130 * USD, 30 * USDC, 0),
        (KIND_CALL, 300 * USD, 30 * USDC, 0),
        (KIND_PUT, 110 * USD, 0, 30 * USDC),
        (KIND_PUT, 93 * USD, 7 * USDC, 23 * USDC),
        (KIND_PUT, 10 * USD, 30 * USDC, 0),
        (KIND_ABOVE, 100 * USD - 1, 0, 30 * USDC),
        (KIND_ABOVE, 100 * USD, 30 * USDC, 0),
        (KIND_BELOW, 100 * USD - 1, 30 * USDC, 0),
        (KIND_BELOW, 100 * USD, 0, 30 * USDC),
    ];
    for (kind, price, trader_payout, maker_payout) in cases {
        let (option, maker, _trader) = parties(kind);
        assert_eq!(
            (option.trader_payout(price), option.maker_payout(price)),
            (trader_payout, maker_payout)
        );
        // The maker settles at the end of the term.
        let inputs = option_settle_inputs(&option, maker, price, TERM_END);
        assert_eq!(
            payouts(&inputs),
            (trader_payout, maker_payout),
            "kind={kind} price={price}"
        );
        assert_valid(CircuitId::OptionSettle, &inputs);
    }
}

#[test]
fn who_may_settle_and_when() {
    for kind in KINDS {
        let prediction = kind >= KIND_ABOVE;
        // Before the end: the trader of a call or a put, and nobody else.
        let (option, maker, trader) = parties(kind);
        let early = option_settle_inputs(&option, trader, 107 * USD, TERM_END - 1);
        if prediction {
            assert_invalid(CircuitId::OptionSettle, &early);
        } else {
            assert_valid(CircuitId::OptionSettle, &early);
        }
        assert_invalid(
            CircuitId::OptionSettle,
            &option_settle_inputs(&option, maker, 107 * USD, TERM_END - 1),
        );
        // From the end on: either party.
        let (option, maker, trader) = parties(kind);
        assert_valid(
            CircuitId::OptionSettle,
            &option_settle_inputs(&option, trader, 107 * USD, TERM_END),
        );
        assert_valid(
            CircuitId::OptionSettle,
            &option_settle_inputs(&option, maker, 107 * USD, TERM_END + 500),
        );
        // Never a stranger.
        let (option, _maker, _trader) = parties(kind);
        assert_invalid(
            CircuitId::OptionSettle,
            &option_settle_inputs(&option, keypair(), 107 * USD, TERM_END + 500),
        );
    }
}

#[test]
fn a_settle_the_circuit_refuses() {
    // Another feed than the contract's.
    let (option, maker, _trader) = parties(KIND_CALL);
    let mut inputs = option_settle_inputs(&option, maker, 107 * USD, TERM_END);
    inputs.program.public.feed_lo ^= 1;
    assert_invalid(CircuitId::OptionSettle, &inputs);
    // A blinding seed that is not the contract's.
    let (option, maker, _trader) = parties(KIND_CALL);
    let mut inputs = option_settle_inputs(&option, maker, 107 * USD, TERM_END);
    inputs.program.private.tx_context = context(&fe(99));
    assert_invalid(CircuitId::OptionSettle, &inputs);
    // Terms other than the ones the contract commits: a later end would let
    // nobody settle, an earlier one would let the maker settle early.
    let (option, maker, _trader) = parties(KIND_ABOVE);
    let mut inputs = option_settle_inputs(&option, maker, 107 * USD, TERM_END - 10);
    inputs.program.private.terms.term_end = TERM_END - 10;
    assert_invalid(CircuitId::OptionSettle, &inputs);
    // A perp position is not an option: its UTXO does not open as one.
    let (option, maker, trader) = parties(KIND_CALL);
    let position = position_utxo(&maker, &trader, true);
    let mut inputs = option_settle_inputs(&option, maker, 107 * USD, TERM_END);
    let foreign = position
        .to_input_utxo(INPUT_TREE_ID, OPTION_LEAF_INDEX)
        .unwrap();
    inputs.program.private.option_utxo = inputs.utxo(&foreign);
    assert_invalid(CircuitId::OptionSettle, &inputs);
}

#[test]
fn a_settle_proof_binds_its_public_inputs() {
    let (option, maker, _trader) = parties(KIND_ABOVE);
    let price = 104 * USD;
    let inputs = option_settle_inputs(&option, maker, price, TERM_END + 7);
    assert_eq!(payouts(&inputs), (30 * USDC, 0));
    let (tx, proof) = prove_and_verify(
        CircuitId::OptionSettle,
        &inputs,
        &option_settle::VERIFYINGKEY,
    );
    let private_tx_hash = tx.finalized.padding_independent_private_tx_hash().unwrap();
    let first_nullifier = tx.finalized.first_nullifier().unwrap();
    let (feed_hi, feed_lo) = feed_halves(&FEED_ID);
    let signer = identity_of(&inputs.sender);
    let public = |price, publish_time, signer, first_nullifier| option_settle::PublicInputs {
        feed_hi,
        feed_lo,
        price,
        publish_time,
        signer_owner_pk_field: signer,
        first_nullifier,
    };
    assert!(verify(
        &proof,
        public(price, TERM_END + 7, signer, first_nullifier),
        &private_tx_hash
    ));
    // Another price, publish time, signer or first nullifier fails: a proof
    // made for one oracle update does not pass for another.
    for inputs in [
        public(price + 1, TERM_END + 7, signer, first_nullifier),
        public(price, TERM_END + 8, signer, first_nullifier),
        public(price, TERM_END - 1, signer, first_nullifier),
        public(
            price,
            TERM_END + 7,
            identity_of(&keypair()),
            first_nullifier,
        ),
        public(price, TERM_END + 7, signer, fe(17)),
    ] {
        assert!(!verify(&proof, inputs, &private_tx_hash));
    }
}

fn verify(
    proof: &CompressedProof,
    public_inputs: option_settle::PublicInputs,
    private_tx_hash: &[u8; 32],
) -> bool {
    option_settle::Verifier {
        proof,
        public_inputs,
        private_tx_hash,
        transfers: &[],
    }
    .verify()
    .is_ok()
}

#[test]
fn a_cancel_refunds_the_maker_and_only_the_maker() {
    let inputs = option_cancel_inputs();
    let finalized = inputs.create_finalized_transaction().unwrap();
    let outputs = finalized.output_utxos();
    // The refund; whatever follows is padding.
    assert_eq!(outputs[0].amount, COLLATERAL);
    assert_eq!(outputs[0].owner_address, Some(inputs.sender_address()));
    assert!(outputs.iter().skip(1).all(|output| output.amount == 0));
    prove_and_verify(
        CircuitId::OptionCancel,
        &inputs,
        &option_cancel::VERIFYINGKEY,
    );
    // Another signer, or another expiry than the quote's.
    let mut inputs = option_cancel_inputs();
    inputs.program.public.maker_owner_pk_field = identity_field(&keypair());
    assert_invalid(CircuitId::OptionCancel, &inputs);
    let mut inputs = option_cancel_inputs();
    inputs.program.public.expiry -= 1;
    assert_invalid(CircuitId::OptionCancel, &inputs);
}
