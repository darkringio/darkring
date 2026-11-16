//! The veil's two circuits.

use crate::shared::*;
use perps_program::verifier::{veil_in, veil_out};
use perps_sdk::{proof::CircuitId, state::claim_of, veil_authority};

#[test]
fn each_veil_vk_has_one_public_input() {
    for key in [&veil_in::VERIFYINGKEY, &veil_out::VERIFYINGKEY] {
        assert_eq!(key.nr_pubinputs, 1);
        assert!(key.vk_commitment.is_none());
    }
}

#[test]
fn a_veil_in_moves_the_amount_into_a_note_of_the_veil() {
    // One note that holds more than the amount: the rest is change.
    let inputs = veil_in_inputs(40 * USDC, &[55 * USDC]);
    let finalized = inputs.create_finalized_transaction().unwrap();
    let outputs = finalized.output_utxos();
    // The note, then the change.
    assert_eq!(
        (outputs[0].amount, outputs[1].amount),
        (40 * USDC, 15 * USDC)
    );
    assert_eq!(
        outputs[0].owner_address.unwrap().owner_hash().unwrap(),
        veil_authority().owner_hash().unwrap()
    );
    assert_eq!(outputs[1].owner_address, Some(inputs.sender_address()));
    // The note is the one the SDK describes from the claim and the blinding.
    let mut note = veil_note(40 * USDC);
    note.blinding = outputs[0].blinding;
    assert_eq!(
        outputs[0].hash(OUTPUT_TREE_ID).unwrap(),
        note.hash(OUTPUT_TREE_ID).unwrap()
    );

    // Proved, and checked by the program's verifier against the note's
    // owner.
    let (tx, proof) = prove_and_verify(CircuitId::VeilIn, &inputs, &veil_in::VERIFYINGKEY);
    let private_tx_hash = tx.finalized.padding_independent_private_tx_hash().unwrap();
    let verifies = |note_owner| {
        veil_in::Verifier {
            proof: &proof,
            public_inputs: veil_in::PublicInputs { note_owner },
            private_tx_hash: &private_tx_hash,
            transfers: &[],
        }
        .verify()
        .is_ok()
    };
    assert!(verifies(field_bytes(
        inputs.program.public.note_owner.hash().unwrap()
    )));
    // A note owned by anything but the veil fails.
    assert!(!verifies(fe(3)));
}

#[test]
fn a_veil_in_gathers_several_notes() {
    // Four notes, spent whole: no change output is left.
    let inputs = veil_in_inputs(100 * USDC, &[10 * USDC, 20 * USDC, 30 * USDC, 40 * USDC]);
    let finalized = inputs.create_finalized_transaction().unwrap();
    let outputs = finalized.output_utxos();
    assert_eq!(outputs[0].amount, 100 * USDC);
    assert!(outputs.iter().skip(1).all(|output| output.amount == 0));
    assert_valid(CircuitId::VeilIn, &inputs);
    // Two notes with change.
    let inputs = veil_in_inputs(25 * USDC, &[10 * USDC, 20 * USDC]);
    assert_eq!(
        inputs
            .create_finalized_transaction()
            .unwrap()
            .output_utxos()[1]
            .amount,
        5 * USDC
    );
    assert_valid(CircuitId::VeilIn, &inputs);
}

#[test]
fn a_veil_in_the_circuit_refuses() {
    // Nothing to hide.
    let mut inputs = veil_in_inputs(40 * USDC, &[55 * USDC]);
    inputs.program.private.amount = 0;
    assert_invalid(CircuitId::VeilIn, &inputs);
    // More than the notes hold.
    let mut inputs = veil_in_inputs(40 * USDC, &[55 * USDC]);
    inputs.program.private.amount = 55 * USDC + 1;
    assert_invalid(CircuitId::VeilIn, &inputs);
}

#[test]
fn a_veil_out_pays_the_whole_note_to_whoever_knows_the_secret() {
    let note = veil_note(40 * USDC);
    // The recipient is nobody the note has ever named.
    let recipient = keypair();
    let inputs = veil_out_inputs(&note, &VEIL_SECRET, recipient.shielded_address().unwrap());
    let finalized = inputs.create_finalized_transaction().unwrap();
    let outputs = finalized.output_utxos();
    // The payout; whatever follows is padding.
    assert_eq!(outputs[0].amount, 40 * USDC);
    assert!(outputs.iter().skip(1).all(|output| output.amount == 0));
    assert_eq!(
        outputs[0].owner_address,
        Some(recipient.shielded_address().unwrap())
    );

    let (tx, proof) = prove_and_verify(CircuitId::VeilOut, &inputs, &veil_out::VERIFYINGKEY);
    let private_tx_hash = tx.finalized.padding_independent_private_tx_hash().unwrap();
    let first_nullifier = tx.finalized.first_nullifier().unwrap();
    let verifies = |first_nullifier| {
        veil_out::Verifier {
            proof: &proof,
            public_inputs: veil_out::PublicInputs { first_nullifier },
            private_tx_hash: &private_tx_hash,
            transfers: &[],
        }
        .verify()
        .is_ok()
    };
    assert!(verifies(first_nullifier));
    // The proof is for this note's nullifier and no other.
    assert!(!verifies(fe(17)));
}

#[test]
fn a_veil_out_the_circuit_refuses() {
    let note = veil_note(40 * USDC);
    let recipient = keypair().shielded_address().unwrap();
    // Another secret.
    let mut wrong = VEIL_SECRET;
    wrong[31] ^= 1;
    assert_invalid(
        CircuitId::VeilOut,
        &veil_out_inputs(&note, &wrong, recipient),
    );
    // A claim other than the one the note commits, with a secret that opens
    // it: the note does not open as that state.
    let mut inputs = veil_out_inputs(&note, &wrong, recipient);
    inputs.program.private.terms.claim = claim_of(&wrong).unwrap();
    assert_invalid(CircuitId::VeilOut, &inputs);
    // Another nullifier than the note's.
    let mut inputs = veil_out_inputs(&note, &VEIL_SECRET, recipient);
    inputs.program.public.first_nullifier = field(17);
    assert_invalid(CircuitId::VeilOut, &inputs);
}

#[test]
fn the_sdk_knows_which_secret_opens_a_note() {
    let note = veil_note(1);
    assert!(note.opens_with(&VEIL_SECRET));
    let mut wrong = VEIL_SECRET;
    wrong[31] ^= 1;
    assert!(!note.opens_with(&wrong));
    // Not a canonical field element: no claim.
    assert!(claim_of(&[0xff; 32]).is_err());
}
