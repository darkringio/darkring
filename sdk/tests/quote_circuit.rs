use crate::shared::*;
use perps_program::verifier::quote::{PublicInputs, Verifier, VERIFYINGKEY};
use perps_sdk::{
    instructions::quote::{QuoteTransactionParams, CHANGE_SLOT, QUOTE_SLOT},
    proof::CircuitId,
};
use spr_program::CompressedProof;

const CIRCUIT: CircuitId = CircuitId::Quote;

#[test]
fn program_vk_has_one_public_input() {
    assert_eq!(VERIFYINGKEY.nr_pubinputs, 1);
    assert!(VERIFYINGKEY.vk_commitment.is_none());
    assert_eq!(VERIFYINGKEY.vk_ic.len(), 2);
}

#[test]
fn quote_proves_and_binds_public_inputs() {
    // 1. The margin moves into the quote UTXO; the rest is change.
    let inputs = quote_inputs(MAKER_MARGIN, 750);
    let finalized = inputs.create_finalized_transaction().unwrap();
    let outputs = finalized.output_utxos();
    let change = outputs.get(CHANGE_SLOT).unwrap();
    let quote = outputs.get(QUOTE_SLOT).unwrap();
    assert_eq!(
        (
            change.amount,
            change.owner_address,
            quote.amount,
            quote.asset
        ),
        (
            750,
            Some(inputs.sender_address()),
            MAKER_MARGIN,
            collateral_mint()
        )
    );

    // 2. Prove, and verify with the program's verifier from the maker signer
    // and the quote owner the program derives.
    let (_, proof) = prove_and_verify(CIRCUIT, &inputs, &VERIFYINGKEY);
    let private_tx_hash = finalized.padding_independent_private_tx_hash().unwrap();
    let maker = identity_of(&inputs.sender);
    let owner = field_bytes(inputs.program.public.quote_owner.hash().unwrap());
    assert!(verify(&proof, maker, owner, &private_tx_hash));

    // 3. Another maker, quote owner or transaction fails.
    let mut tampered = private_tx_hash;
    tampered[31] ^= 0x01;
    assert!(!verify(
        &proof,
        identity_of(&keypair()),
        owner,
        &private_tx_hash
    ));
    assert!(!verify(&proof, maker, fe(17), &private_tx_hash));
    assert!(!verify(&proof, maker, owner, &tampered));
}

#[test]
fn quote_rejects_terms_that_cannot_open_and_a_source_of_another_owner() {
    let mutations: [fn(&mut Quote); 8] = [
        |inputs| inputs.program.private.maker_margin = 0,
        |inputs| inputs.program.private.size = 0,
        |inputs| inputs.program.private.entry_price = 0,
        |inputs| inputs.program.private.trader_margin = 0,
        // Liquidatable at its own entry price.
        |inputs| inputs.program.private.maintenance_margin = TRADER_MARGIN,
        |inputs| inputs.program.private.maintenance_margin = TRADER_MARGIN + 1,
        // More margin than the source holds.
        |inputs| inputs.program.private.maker_margin = MAKER_MARGIN + 751,
        // The source is not the maker signer's.
        |inputs| inputs.program.public.maker_owner_pk_field = identity_field(&keypair()),
    ];
    for mutate in mutations {
        let mut inputs = quote_inputs(MAKER_MARGIN, 750);
        mutate(&mut inputs);
        assert_invalid(CIRCUIT, &inputs);
    }
}

#[test]
fn sdk_builds_the_quote_the_circuit_commits() {
    let maker = keypair();
    let trader = keypair();
    let terms = quote_terms(&maker, Some(identity_of(&trader)));
    let transaction = QuoteTransactionParams {
        terms: terms.clone(),
        maker_margin: MAKER_MARGIN,
        source: wallet_utxo(&maker, collateral_mint(), MAKER_MARGIN + 9),
        trader_address: trader.shielded_address().unwrap(),
        payer: solana_address(&maker),
    }
    .build(&maker)
    .unwrap();
    assert_eq!(transaction.quote_utxo.terms, terms);
    assert_eq!(transaction.quote_utxo.maker_margin, MAKER_MARGIN);
    let outputs = &transaction.spp_proof_inputs.external_data.outputs;
    assert_eq!(
        outputs.get(QUOTE_SLOT).unwrap().utxo_hash,
        transaction.quote_utxo.hash(INPUT_TREE_ID).unwrap()
    );
    // The transaction's only message is the trader's marker.
    assert_eq!(transaction.spp_proof_inputs.external_data.messages.len(), 1);

    // The SDK refuses terms the circuit refuses, and another maker's terms.
    for bad in [
        perps_sdk::state::QuoteTerms {
            maintenance_margin: TRADER_MARGIN,
            ..terms.clone()
        },
        perps_sdk::state::QuoteTerms {
            size: 0,
            ..terms.clone()
        },
        perps_sdk::state::QuoteTerms {
            maker: keypair().shielded_address().unwrap(),
            ..terms.clone()
        },
    ] {
        assert!(QuoteTransactionParams {
            terms: bad,
            maker_margin: MAKER_MARGIN,
            source: wallet_utxo(&maker, collateral_mint(), MAKER_MARGIN + 9),
            trader_address: trader.shielded_address().unwrap(),
            payer: solana_address(&maker),
        }
        .build(&maker)
        .is_err());
    }
}

fn verify(
    proof: &CompressedProof,
    maker_owner_pk_field: [u8; 32],
    quote_owner: [u8; 32],
    private_tx_hash: &[u8; 32],
) -> bool {
    Verifier {
        proof,
        public_inputs: PublicInputs {
            maker_owner_pk_field,
            quote_owner,
        },
        private_tx_hash,
        transfers: &[],
    }
    .verify()
    .is_ok()
}
