use crate::shared::*;
use perps_program::verifier::settle::{PublicInputs, Verifier, VERIFYINGKEY};
use perps_sdk::{
    instructions::settle::{SettleTransactionParams, MAKER_PAYOUT_SLOT, TRADER_PAYOUT_SLOT},
    proof::CircuitId,
    shared::{feed_halves, settle_blinding_seed},
    state::PositionUtxo,
};
use solana_address::Address;
use spr_keypair::ShieldedKeypair;
use spr_program::CompressedProof;

const CIRCUIT: CircuitId = CircuitId::Settle;

/// A long (or short) position and its two parties.
fn parties(long: bool) -> (PositionUtxo, ShieldedKeypair, ShieldedKeypair) {
    let maker = keypair();
    let trader = keypair();
    (position_utxo(&maker, &trader, long), maker, trader)
}

/// The amounts and owners of the two payouts a settle of `inputs` creates.
fn payouts(inputs: &Settle) -> (u64, u64) {
    let finalized = inputs.create_finalized_transaction().unwrap();
    let outputs = finalized.output_utxos();
    assert_eq!(outputs.len(), 2, "a settle always creates both payouts");
    let trader = outputs.get(TRADER_PAYOUT_SLOT).unwrap();
    let maker = outputs.get(MAKER_PAYOUT_SLOT).unwrap();
    let terms = &inputs.program.private.terms;
    assert_eq!(
        (trader.asset, maker.asset),
        (collateral_mint(), collateral_mint())
    );
    assert_eq!(
        spr_program::to_bytes(&terms.trader.hash().unwrap()),
        trader.owner_address.unwrap().owner_hash().unwrap()
    );
    assert_eq!(
        spr_program::to_bytes(&terms.maker.hash().unwrap()),
        maker.owner_address.unwrap().owner_hash().unwrap()
    );
    (trader.amount, maker.amount)
}

#[test]
fn program_vk_has_one_public_input() {
    assert_eq!(VERIFYINGKEY.nr_pubinputs, 1);
    assert!(VERIFYINGKEY.vk_commitment.is_none());
    assert_eq!(VERIFYINGKEY.vk_ic.len(), 2);
}

#[test]
fn trader_closes_at_a_profit_and_the_proof_binds_its_public_inputs() {
    // 1. A long from $100 closed at $105 on one unit: +5 to the trader.
    let (position, _maker, trader) = parties(true);
    let price = 105 * USD;
    let inputs = settle_inputs(&position, trader, price);
    assert_eq!(payouts(&inputs), (25 * USDC, 25 * USDC));

    // 2. Prove, and verify with the program's verifier from the oracle's
    // feed and price, the signer and the transact's first nullifier.
    let (tx, proof) = prove_and_verify(CIRCUIT, &inputs, &VERIFYINGKEY);
    let private_tx_hash = tx.finalized.padding_independent_private_tx_hash().unwrap();
    let first_nullifier = tx.finalized.first_nullifier().unwrap();
    let (feed_hi, feed_lo) = feed_halves(&FEED_ID);
    let signer = identity_of(&inputs.sender);
    let public = |feed_hi, feed_lo, price, signer, first_nullifier| PublicInputs {
        feed_hi,
        feed_lo,
        price,
        signer_owner_pk_field: signer,
        first_nullifier,
    };
    assert!(verify(
        &proof,
        public(feed_hi, feed_lo, price, signer, first_nullifier),
        &private_tx_hash
    ));

    // 3. Another feed, price, signer, first nullifier or transaction fails.
    let mut tampered = private_tx_hash;
    tampered[31] ^= 0x01;
    for (inputs, hash) in [
        (
            public(feed_hi ^ 1, feed_lo, price, signer, first_nullifier),
            private_tx_hash,
        ),
        (
            public(feed_hi, feed_lo ^ 1, price, signer, first_nullifier),
            private_tx_hash,
        ),
        (
            public(feed_hi, feed_lo, price + 1, signer, first_nullifier),
            private_tx_hash,
        ),
        (
            public(feed_hi, feed_lo, price - 1, signer, first_nullifier),
            private_tx_hash,
        ),
        (
            public(
                feed_hi,
                feed_lo,
                price,
                identity_of(&keypair()),
                first_nullifier,
            ),
            private_tx_hash,
        ),
        (
            public(feed_hi, feed_lo, price, signer, fe(17)),
            private_tx_hash,
        ),
        (
            public(feed_hi, feed_lo, price, signer, first_nullifier),
            tampered,
        ),
    ] {
        assert!(!verify(&proof, inputs, &hash));
    }
}

#[test]
fn trader_settles_at_any_price_with_the_exact_payouts() {
    // (direction, price, trader payout, maker payout) on 20 + 30 of margin.
    let cases = [
        // Flat.
        (true, 100 * USD, 20 * USDC, 30 * USDC),
        // A loss.
        (true, 95 * USD, 15 * USDC, 35 * USDC),
        // Deep in loss, where the maker could also settle.
        (true, 82 * USD, 2 * USDC, 48 * USDC),
        // Wiped out exactly, and beyond: the loss is capped at the margin.
        (true, 80 * USD, 0, 50 * USDC),
        (true, 70 * USD, 0, 50 * USDC),
        // The profit is capped at the maker's margin, exactly and beyond.
        (true, 130 * USD, 50 * USDC, 0),
        (true, 140 * USD, 50 * USDC, 0),
        // Rounding: 150 ticks is 1.5 base units, and the fraction stays
        // with the loser.
        (true, 100 * USD + 150, 20 * USDC + 1, 30 * USDC - 1),
        (true, 100 * USD - 150, 20 * USDC - 1, 30 * USDC + 1),
        // A short mirrors the long.
        (false, 95 * USD, 25 * USDC, 25 * USDC),
        (false, 105 * USD, 15 * USDC, 35 * USDC),
        (false, 60 * USD, 50 * USDC, 0),
        (false, 125 * USD, 0, 50 * USDC),
    ];
    for (long, price, trader_payout, maker_payout) in cases {
        let (position, _maker, trader) = parties(long);
        assert_eq!(
            (position.trader_payout(price), position.maker_payout(price)),
            (trader_payout, maker_payout)
        );
        let inputs = settle_inputs(&position, trader, price);
        assert_eq!(
            payouts(&inputs),
            (trader_payout, maker_payout),
            "long={long} price={price}"
        );
        assert_valid(CIRCUIT, &inputs);
    }
}

#[test]
fn a_wiped_out_side_still_gets_a_real_zero_output() {
    // The transaction has the same two-output shape whether or not a side
    // was wiped out, so its shape does not reveal the outcome.
    for price in [70 * USD, 140 * USD] {
        let (position, _maker, trader) = parties(true);
        let inputs = settle_inputs(&position, trader, price);
        let finalized = inputs.create_finalized_transaction().unwrap();
        assert_eq!(finalized.output_utxos().len(), 2);
        let key = inputs
            .sender
            .get_transaction_viewing_key(&finalized.first_nullifier().unwrap())
            .unwrap();
        let proof_inputs = finalized.encrypt_with_viewing_key(&key).unwrap();
        assert!(proof_inputs
            .external_data
            .outputs
            .iter()
            .all(|output| output.utxo_hash != [0; 32]));
        assert_eq!(proof_inputs.external_data.outputs.len(), 2);
        prove_and_verify(CIRCUIT, &inputs, &VERIFYINGKEY);
    }
}

#[test]
fn maker_is_refused_on_a_healthy_position() {
    // The trader's equity is above the maintenance margin at each price.
    for (long, price) in [
        (true, 100 * USD),
        (true, 140 * USD),
        (true, 90 * USD),
        // One base unit above the maintenance margin.
        (true, 85 * USD + 100),
        (false, 100 * USD),
        (false, 115 * USD - 100),
    ] {
        let (position, maker, _trader) = parties(long);
        assert!(!position.is_liquidatable(price));
        let inputs = settle_inputs(&position, maker, price);
        assert_invalid(CIRCUIT, &inputs);
        // The SDK refuses to build it.
        let maker = inputs.sender;
        let error = SettleTransactionParams {
            position_utxo: position,
            position_leaf_index: POSITION_LEAF_INDEX,
            oracle: Address::default(),
            price,
            payer: Address::default(),
            tree_id: INPUT_TREE_ID,
        }
        .build(&maker)
        .err()
        .expect("a maker settle of a healthy position must be refused");
        assert!(error.to_string().contains("liquidatable"));
    }
}

#[test]
fn maker_liquidates_at_or_below_the_maintenance_margin() {
    // (direction, price, trader payout).
    let cases = [
        // Exactly at the maintenance margin.
        (true, 85 * USD, 5 * USDC),
        (true, 82 * USD, 2 * USDC),
        (true, 60 * USD, 0),
        (false, 115 * USD, 5 * USDC),
        (false, 150 * USD, 0),
    ];
    for (long, price, trader_payout) in cases {
        let (position, maker, _trader) = parties(long);
        assert!(position.is_liquidatable(price));
        let inputs = settle_inputs(&position, maker, price);
        assert_eq!(
            payouts(&inputs),
            (trader_payout, TRADER_MARGIN + MAKER_MARGIN - trader_payout)
        );
        assert_valid(CIRCUIT, &inputs);
    }
    // One liquidation proves end to end.
    let (position, maker, _trader) = parties(true);
    prove_and_verify(
        CIRCUIT,
        &settle_inputs(&position, maker, 85 * USD),
        &VERIFYINGKEY,
    );
}

#[test]
fn a_third_party_cannot_settle() {
    // Neither on a healthy nor on a liquidatable position.
    for price in [100 * USD, 60 * USD] {
        let (position, _maker, _trader) = parties(true);
        let inputs = settle_inputs(&position, keypair(), price);
        assert_invalid(CIRCUIT, &inputs);
        let stranger = inputs.sender;
        assert!(SettleTransactionParams {
            position_utxo: position,
            position_leaf_index: POSITION_LEAF_INDEX,
            oracle: Address::default(),
            price,
            payer: Address::default(),
            tree_id: INPUT_TREE_ID,
        }
        .build(&stranger)
        .is_err());
    }
}

#[test]
fn settle_rejects_another_feed() {
    let (position, _maker, trader) = parties(true);
    let mutations: [fn(&mut Settle); 2] = [
        |inputs| inputs.program.public.feed_hi ^= 1,
        |inputs| inputs.program.public.feed_lo ^= 1,
    ];
    for mutate in mutations {
        let mut inputs = settle_inputs(&position, trader.clone(), 105 * USD);
        mutate(&mut inputs);
        assert_invalid(CIRCUIT, &inputs);
    }
}

#[test]
fn settle_rejects_an_unrecoverable_blinding_seed_and_first_nullifier() {
    // A seed other than the position's, or a first nullifier other than the
    // published one, would hide a payout from the position opening.
    let mutations: [fn(&mut Settle); 3] = [
        |inputs| inputs.program.private.tx_context = context(&fe(29)),
        |inputs| inputs.program.public.first_nullifier = field(17),
        |inputs| inputs.program.private.position_utxo.blinding = field(29),
    ];
    for mutate in mutations {
        let (position, _maker, trader) = parties(true);
        let mut inputs = settle_inputs(&position, trader, 105 * USD);
        mutate(&mut inputs);
        assert_invalid(CIRCUIT, &inputs);
    }
}

#[test]
fn settle_rejects_terms_the_position_does_not_commit() {
    // Each would change the split: the position UTXO commits every term.
    let mutations: [fn(&mut Settle); 8] = [
        |inputs| inputs.program.private.terms.size += 1,
        |inputs| inputs.program.private.terms.entry_price -= USD,
        |inputs| inputs.program.private.terms.trader_margin += USDC,
        |inputs| inputs.program.private.terms.maintenance_margin = TRADER_MARGIN + MAKER_MARGIN,
        |inputs| inputs.program.private.terms.trader_is_long = false,
        |inputs| {
            let other = keypair().shielded_address().unwrap();
            inputs.program.private.terms.trader = *inputs.address(other).owner();
        },
        |inputs| {
            let other = keypair().shielded_address().unwrap();
            inputs.program.private.terms.maker = *inputs.address(other).owner();
        },
        |inputs| inputs.program.private.position_utxo.data_hash = field(99),
    ];
    for mutate in mutations {
        let (position, _maker, trader) = parties(true);
        let mut inputs = settle_inputs(&position, trader, 105 * USD);
        mutate(&mut inputs);
        assert_invalid(CIRCUIT, &inputs);
    }
}

#[test]
fn a_position_amount_other_than_the_committed_one_commits_another_transaction() {
    // The settle circuit splits whatever amount its witnessed input holds;
    // it is the SPP proof that opens the real position UTXO. A witness with
    // another amount commits another input hash, so it cannot produce the
    // transaction hash SPP verifies for the real position.
    let (position, _maker, trader) = parties(true);
    let honest = settle_inputs(&position, trader.clone(), 105 * USD)
        .create_program_transaction()
        .unwrap()
        .finalized
        .padding_independent_private_tx_hash()
        .unwrap();
    let mut inputs = settle_inputs(&position, trader, 105 * USD);
    inputs.program.private.position_utxo.amount += USDC;
    let forged = inputs
        .create_program_transaction()
        .ok()
        .map(|tx| tx.finalized.padding_independent_private_tx_hash().unwrap());
    assert_ne!(forged, Some(honest));
}

#[test]
fn both_parties_recover_their_payouts_from_the_position_opening() {
    // 1. Build a close through the SDK.
    let (position, _maker, trader) = parties(true);
    let price = 112 * USD;
    let transaction = SettleTransactionParams {
        position_utxo: position.clone(),
        position_leaf_index: POSITION_LEAF_INDEX,
        oracle: Address::default(),
        price,
        payer: Address::default(),
        tree_id: INPUT_TREE_ID,
    }
    .build(&trader)
    .unwrap();
    assert_eq!(
        (transaction.trader_payout, transaction.maker_payout),
        (32 * USDC, 18 * USDC)
    );
    assert_eq!(
        settle_blinding_seed(&position.blinding).unwrap(),
        transaction.spp_proof_inputs.blinding_seed
    );

    // 2. Either party derives both payouts from the position opening, the
    // public price and the first nullifier alone.
    let outputs = &transaction.spp_proof_inputs.external_data.outputs;
    let (trader_payout, maker_payout) = position
        .derived_payouts(&transaction.first_nullifier, price)
        .unwrap();
    assert_eq!(
        trader_payout.hash(INPUT_TREE_ID).unwrap(),
        outputs.get(TRADER_PAYOUT_SLOT).unwrap().utxo_hash
    );
    assert_eq!(
        maker_payout.hash(INPUT_TREE_ID).unwrap(),
        outputs.get(MAKER_PAYOUT_SLOT).unwrap().utxo_hash
    );

    // 3. Another first nullifier or price derives other payouts.
    let (wrong, _) = position.derived_payouts(&fe(17), price).unwrap();
    assert_ne!(
        wrong.hash(INPUT_TREE_ID).unwrap(),
        outputs.get(TRADER_PAYOUT_SLOT).unwrap().utxo_hash
    );
    let (wrong, _) = position
        .derived_payouts(&transaction.first_nullifier, price + USD)
        .unwrap();
    assert_ne!(
        wrong.hash(INPUT_TREE_ID).unwrap(),
        outputs.get(TRADER_PAYOUT_SLOT).unwrap().utxo_hash
    );
}

fn verify(
    proof: &CompressedProof,
    public_inputs: PublicInputs,
    private_tx_hash: &[u8; 32],
) -> bool {
    Verifier {
        proof,
        public_inputs,
        private_tx_hash,
        transfers: &[],
    }
    .verify()
    .is_ok()
}
