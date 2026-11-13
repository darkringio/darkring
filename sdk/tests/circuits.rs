//! Circuit tests: each checks or proves with the checksum-pinned artifacts in
//! `target/zk/perps-program`, built by `zolana zk compile --test`, and checks
//! proofs with the committed verifiers.
mod cancel_quote_circuit;
mod open_circuit;
mod quote_circuit;
mod settle_circuit;
mod shared;

#[test]
fn circuits_encode_only_their_specified_utxo_inputs() {
    // 1. Inspect the fixed input schema passed to each compiled circuit.
    let maker = shared::keypair();
    let trader = shared::keypair();
    let quote = shared::quote_utxo(shared::quote_terms(&maker, None));
    let position = shared::position_utxo(&maker, &trader, true);
    let cases = [
        (
            "quote",
            shared::quote_inputs(shared::MAKER_MARGIN, 750)
                .create_program_transaction()
                .unwrap()
                .proof_inputs
                .encoded(),
            1,
        ),
        (
            "open",
            shared::open_inputs(&quote, shared::keypair(), shared::TRADER_MARGIN)
                .create_program_transaction()
                .unwrap()
                .proof_inputs
                .encoded(),
            2,
        ),
        (
            "settle",
            shared::settle_inputs(&position, trader, shared::ENTRY_PRICE)
                .create_program_transaction()
                .unwrap()
                .proof_inputs
                .encoded(),
            1,
        ),
        (
            "cancel_quote",
            shared::cancel_inputs()
                .create_program_transaction()
                .unwrap()
                .proof_inputs
                .encoded(),
            1,
        ),
    ];

    // 2. Reject unused UTXO inputs introduced by a padding default.
    for (name, leaves, expected) in cases {
        let inputs = leaves
            .keys()
            .filter(|name| name.ends_with(".domain"))
            .count();
        assert_eq!(inputs, expected, "{name} encodes unexpected input UTXOs");
    }
}
