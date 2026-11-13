use crate::shared::*;
use perps_program::verifier::cancel_quote::{PublicInputs, Verifier, VERIFYINGKEY};
use perps_sdk::proof::CircuitId;
use spr_program::CompressedProof;

const CIRCUIT: CircuitId = CircuitId::CancelQuote;

#[test]
fn program_vk_has_one_public_input() {
    assert_eq!(VERIFYINGKEY.nr_pubinputs, 1);
    assert!(VERIFYINGKEY.vk_commitment.is_none());
    assert_eq!(VERIFYINGKEY.vk_ic.len(), 2);
}

#[test]
fn cancel_refunds_the_maker_and_binds_public_inputs() {
    // 1. The whole margin returns to the maker.
    let inputs = cancel_inputs();
    let finalized = inputs.create_finalized_transaction().unwrap();
    let outputs = finalized.output_utxos();
    let refund = outputs.first().unwrap();
    assert_eq!(
        (refund.amount, refund.asset, refund.owner_address),
        (
            MAKER_MARGIN,
            collateral_mint(),
            Some(inputs.sender_address())
        )
    );

    // 2. Prove, and verify with the program's verifier from the quote
    // expiry and the maker signer.
    let (_, proof) = prove_and_verify(CIRCUIT, &inputs, &VERIFYINGKEY);
    let private_tx_hash = finalized.padding_independent_private_tx_hash().unwrap();
    let maker = identity_of(&inputs.sender);
    assert!(verify(&proof, EXPIRY, maker, &private_tx_hash));

    // 3. Another expiry, signer or transaction fails.
    let mut tampered = private_tx_hash;
    tampered[31] ^= 0x01;
    assert!(!verify(&proof, EXPIRY + 1, maker, &private_tx_hash));
    assert!(!verify(
        &proof,
        EXPIRY,
        identity_of(&keypair()),
        &private_tx_hash
    ));
    assert!(!verify(&proof, EXPIRY, maker, &tampered));
}

#[test]
fn cancel_rejects_another_signer_expiry_and_terms() {
    let mutations: [fn(&mut CancelQuote); 5] = [
        |inputs| inputs.program.public.maker_owner_pk_field = identity_field(&keypair()),
        |inputs| inputs.program.public.expiry = EXPIRY - 1,
        |inputs| inputs.program.private.terms.expiry = EXPIRY - 1,
        |inputs| {
            let other = keypair().shielded_address().unwrap();
            inputs.program.private.terms.maker = *inputs.address(other).owner();
        },
        |inputs| inputs.program.private.quote_utxo.data_hash = field(99),
    ];
    for mutate in mutations {
        let mut inputs = cancel_inputs();
        mutate(&mut inputs);
        assert_invalid(CIRCUIT, &inputs);
    }
}

fn verify(
    proof: &CompressedProof,
    expiry: u64,
    maker_owner_pk_field: [u8; 32],
    private_tx_hash: &[u8; 32],
) -> bool {
    Verifier {
        proof,
        public_inputs: PublicInputs {
            expiry,
            maker_owner_pk_field,
        },
        private_tx_hash,
        transfers: &[],
    }
    .verify()
    .is_ok()
}
