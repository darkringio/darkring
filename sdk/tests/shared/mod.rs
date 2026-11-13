#![allow(dead_code)]

use perps_program::circuits as circuit;
use perps_sdk::{
    maker_authority,
    proof::CircuitId,
    shared::{feed_halves, identity, open_blinding_seed, settle_blinding_seed},
    state::{PositionTerms, PositionUtxo, QuoteTerms, QuoteUtxo},
};
use solana_address::Address;
use solana_keypair::Keypair;
use spr_keypair::{ShieldedAddress, ShieldedKeypair};
use spr_program::{
    circuit::CheckedTransaction, to_bytes, ClientInputs, Field, InputRecords, ProgramProofInputs,
    ProgramTransaction, TxContext,
};
use spr_transaction::{instructions::transact::FinalizedTransaction, Data, Mint, Utxo, WalletUtxo};
use std::ops::{Deref, DerefMut};

/// The tree the spent UTXOs are in. Non-zero, and different from the output
/// tree, so a swapped or dropped tree id changes the commitments the proof
/// binds.
pub const INPUT_TREE_ID: u16 = 3;
pub const OUTPUT_TREE_ID: u16 = 7;
pub const EXPIRY: u64 = 1_700_000_000;
pub const QUOTE_LEAF_INDEX: u64 = 5;
pub const POSITION_LEAF_INDEX: u64 = 8;

/// Price ticks per dollar on a feed with exponent -8, and base units per
/// unit of a 6-decimal collateral.
pub const USD: u64 = 100_000_000;
pub const USDC: u64 = 1_000_000;
/// One whole unit of the underlying: a dollar of price movement is one USDC.
pub const SIZE: u64 = 10_000_000;
pub const ENTRY_PRICE: u64 = 100 * USD;
pub const TRADER_MARGIN: u64 = 20 * USDC;
pub const MAKER_MARGIN: u64 = 30 * USDC;
pub const MAINTENANCE_MARGIN: u64 = 5 * USDC;
pub const FEED_ID: [u8; 32] = [
    0xef, 0x0d, 0x8b, 0x6f, 0xda, 0x2c, 0xeb, 0xa4, 0x1d, 0xa1, 0x5d, 0x40, 0x95, 0xd1, 0xda, 0x39,
    0x2a, 0x0d, 0x2f, 0x8e, 0xd0, 0xc6, 0xc7, 0xbc, 0x0f, 0x4c, 0xfa, 0xc8, 0xc2, 0x80, 0xb5, 0x6d,
];

pub trait Execute: spr_program::xark::Input {
    fn execute(&self) -> Result<CheckedTransaction, spr_program::CircuitError>;
}

macro_rules! program {
    ($($name:ident),*) => {$(
        impl Execute for circuit::$name {
            fn execute(&self) -> Result<CheckedTransaction, spr_program::CircuitError> {
                circuit::$name::execute(self)
            }
        }
        pub type $name = Fixture<circuit::$name>;
    )*};
}
program!(Quote, Open, Settle, CancelQuote);

/// A program's inputs and the native records they were converted from.
pub struct Fixture<P> {
    pub program: P,
    records: InputRecords,
    /// The transaction's sender: the maker for quote and cancel, the trader
    /// for open, either for settle.
    pub sender: ShieldedKeypair,
}

impl<P> Deref for Fixture<P> {
    type Target = P;
    fn deref(&self) -> &P {
        &self.program
    }
}

impl<P> DerefMut for Fixture<P> {
    fn deref_mut(&mut self) -> &mut P {
        &mut self.program
    }
}

impl<P: Execute> Fixture<P> {
    pub fn sender_address(&self) -> ShieldedAddress {
        self.sender.shielded_address().unwrap()
    }

    pub fn create_program_transaction(&self) -> anyhow::Result<ProgramTransaction> {
        Ok(self.records.create_program_transaction(
            &self.program,
            self.execute()?,
            &self.sender_address(),
            Address::default(),
        )?)
    }

    pub fn create_finalized_transaction(&self) -> anyhow::Result<FinalizedTransaction> {
        Ok(self.create_program_transaction()?.finalized)
    }

    /// Converts `address` as a circuit input and records its viewing key.
    pub fn address(&mut self, address: ShieldedAddress) -> spr_program::ShieldedAddress {
        self.records.address(&address).unwrap()
    }

    /// Converts `mint` as a circuit input and records it.
    pub fn asset(&mut self, mint: Mint) -> [u8; 32] {
        self.records.asset(mint).unwrap()
    }

    /// Converts `utxo` as a circuit input and records it.
    pub fn utxo(&mut self, utxo: &WalletUtxo) -> spr_program::circuit::Utxo {
        self.records.utxo(utxo).unwrap()
    }
}

pub fn keypair() -> ShieldedKeypair {
    ShieldedKeypair::from_keypair(&Keypair::new()).unwrap()
}

pub fn collateral_mint() -> Mint {
    Mint::new(Address::new_from_array([3; 32]), 2)
}

pub fn other_mint() -> Mint {
    Mint::new(Address::new_from_array([4; 32]), 3)
}

pub fn fe(value: u8) -> [u8; 32] {
    let mut bytes = [0; 32];
    bytes[31] = value;
    bytes
}

pub fn field(value: u8) -> Field {
    Field::from(value)
}

pub fn field_bytes(value: Field) -> [u8; 32] {
    to_bytes(&value)
}

pub fn context(seed: &[u8; 32]) -> TxContext {
    InputRecords::context(seed, Some(OUTPUT_TREE_ID)).unwrap()
}

pub fn solana_address(keypair: &ShieldedKeypair) -> Address {
    keypair
        .shielded_address()
        .unwrap()
        .solana_address()
        .unwrap()
}

/// The owner identity of a keypair's Solana address.
pub fn identity_of(keypair: &ShieldedKeypair) -> [u8; 32] {
    identity(&solana_address(keypair)).unwrap()
}

pub fn identity_field(keypair: &ShieldedKeypair) -> Field {
    spr_program::from_bytes(&identity_of(keypair)).unwrap()
}

/// A long quote of [`SIZE`] at [`ENTRY_PRICE`] by `maker`.
pub fn quote_terms(maker: &ShieldedKeypair, trader: Option<[u8; 32]>) -> QuoteTerms {
    QuoteTerms {
        feed_id: FEED_ID,
        trader_is_long: true,
        size: SIZE,
        entry_price: ENTRY_PRICE,
        trader_margin: TRADER_MARGIN,
        maintenance_margin: MAINTENANCE_MARGIN,
        maker: maker.shielded_address().unwrap(),
        expiry: EXPIRY,
        trader,
    }
}

pub fn quote_utxo(terms: QuoteTerms) -> QuoteUtxo {
    QuoteUtxo {
        terms,
        blinding: fe(7),
        collateral_mint: collateral_mint(),
        maker_margin: MAKER_MARGIN,
    }
}

/// A position between `maker` and `trader` holding both margins.
pub fn position_utxo(
    maker: &ShieldedKeypair,
    trader: &ShieldedKeypair,
    long: bool,
) -> PositionUtxo {
    PositionUtxo {
        terms: PositionTerms {
            feed_id: FEED_ID,
            trader_is_long: long,
            size: SIZE,
            entry_price: ENTRY_PRICE,
            trader_margin: TRADER_MARGIN,
            maintenance_margin: MAINTENANCE_MARGIN,
            maker: maker.shielded_address().unwrap(),
            trader: trader.shielded_address().unwrap(),
        },
        blinding: fe(9),
        collateral_mint: collateral_mint(),
        total: TRADER_MARGIN + MAKER_MARGIN,
    }
}

/// `amount` of `mint` owned by `owner`, at leaf 6 of [`INPUT_TREE_ID`].
pub fn wallet_utxo(owner: &ShieldedKeypair, mint: Mint, amount: u64) -> WalletUtxo {
    spr_test_utils::utxo::wallet(
        Utxo {
            owner: owner.signing_pubkey(),
            asset: mint,
            amount,
            blinding: fe(13),
            ring_program_id: None,
            data: Data::default(),
        },
        &owner.nullifier_key,
        INPUT_TREE_ID,
        6,
        None,
        None,
    )
    .unwrap()
}

/// The address of a UTXO of `maker`'s authority PDA encrypted to `viewer`.
pub fn authority_address(maker: &ShieldedKeypair, viewer: &ShieldedKeypair) -> ShieldedAddress {
    maker_authority(&solana_address(maker))
        .address(viewer.viewing_pubkey())
        .unwrap()
}

/// A quote locking `maker_margin` of a `maker_margin + change` source owned
/// by the maker, designated to and encrypted for a fresh trader.
pub fn quote_inputs(maker_margin: u64, change: u64) -> Quote {
    let maker = keypair();
    let trader = keypair();
    let quote_address = authority_address(&maker, &trader);
    let source = wallet_utxo(&maker, collateral_mint(), maker_margin + change);
    let (feed_hi, feed_lo) = feed_halves(&FEED_ID);
    let mut fixture = Fixture {
        program: circuit::Quote {
            private: circuit::QuotePrivateInputs {
                tx_context: context(&fe(41)),
                source: spr_program::circuit::Utxo::dummy(),
                maker_margin,
                feed_hi,
                feed_lo,
                trader_is_long: true,
                size: SIZE,
                entry_price: ENTRY_PRICE,
                trader_margin: TRADER_MARGIN,
                maintenance_margin: MAINTENANCE_MARGIN,
                expiry: EXPIRY,
                taker_pk_fe: identity_of(&trader),
            },
            public: circuit::QuotePublicInputs {
                maker_owner_pk_field: identity_field(&maker),
                quote_owner: spr_program::ShieldedAddress::default(),
            },
        },
        records: InputRecords::new(),
        sender: maker,
    };
    fixture.program.private.source = fixture.utxo(&source);
    fixture.program.public.quote_owner = fixture.address(quote_address);
    fixture
}

/// An open of `quote` at [`QUOTE_LEAF_INDEX`] by `trader`, whose margin
/// input holds `margin_input`, under the quote's open blinding seed.
pub fn open_inputs(quote: &QuoteUtxo, trader: ShieldedKeypair, margin_input: u64) -> Open {
    let quote_input = quote
        .to_input_utxo(INPUT_TREE_ID, QUOTE_LEAF_INDEX)
        .unwrap();
    let trader_input = wallet_utxo(&trader, collateral_mint(), margin_input);
    let position_address = quote
        .terms
        .maker_authority()
        .unwrap()
        .address(quote.terms.maker.viewing_pubkey)
        .unwrap();
    let mut fixture = Fixture {
        program: circuit::Open {
            private: circuit::OpenPrivateInputs {
                tx_context: context(&open_blinding_seed(&quote.blinding).unwrap()),
                terms: circuit::QuoteTerms::default(),
                quote_utxo: spr_program::circuit::Utxo::dummy(),
                trader_in: spr_program::circuit::Utxo::dummy(),
            },
            public: circuit::OpenPublicInputs {
                expiry: quote.terms.expiry,
                first_nullifier: spr_program::from_bytes(&quote_input.nullifier).unwrap(),
                position_owner: spr_program::ShieldedAddress::default(),
            },
        },
        records: InputRecords::new(),
        sender: trader,
    };
    fixture.program.private.terms = circuit::QuoteTerms::from_client(
        quote.terms.circuit_terms().unwrap(),
        &mut fixture.records,
    )
    .unwrap();
    fixture.program.private.quote_utxo = fixture.utxo(&quote_input);
    fixture.program.private.trader_in = fixture.utxo(&trader_input);
    fixture.program.public.position_owner = fixture.address(position_address);
    fixture
}

/// A settle of `position` at [`POSITION_LEAF_INDEX`] by `settler` at `price`,
/// under the position's settle blinding seed.
pub fn settle_inputs(position: &PositionUtxo, settler: ShieldedKeypair, price: u64) -> Settle {
    let position_input = position
        .to_input_utxo(INPUT_TREE_ID, POSITION_LEAF_INDEX)
        .unwrap();
    let (feed_hi, feed_lo) = feed_halves(&position.terms.feed_id);
    let mut fixture = Fixture {
        program: circuit::Settle {
            private: circuit::SettlePrivateInputs {
                tx_context: context(&settle_blinding_seed(&position.blinding).unwrap()),
                terms: circuit::PositionTerms::default(),
                position_utxo: spr_program::circuit::Utxo::dummy(),
                trader_address: spr_program::ShieldedAddress::default(),
                maker_address: spr_program::ShieldedAddress::default(),
            },
            public: circuit::SettlePublicInputs {
                feed_hi,
                feed_lo,
                price,
                signer_owner_pk_field: identity_field(&settler),
                first_nullifier: spr_program::from_bytes(&position_input.nullifier).unwrap(),
            },
        },
        records: InputRecords::new(),
        sender: settler,
    };
    fixture.program.private.terms = circuit::PositionTerms::from_client(
        position.terms.circuit_terms().unwrap(),
        &mut fixture.records,
    )
    .unwrap();
    fixture.program.private.position_utxo = fixture.utxo(&position_input);
    fixture.program.private.trader_address = fixture.address(position.terms.trader);
    fixture.program.private.maker_address = fixture.address(position.terms.maker);
    fixture
}

/// A cancel of an open quote of a fresh maker at [`QUOTE_LEAF_INDEX`].
pub fn cancel_inputs() -> CancelQuote {
    let maker = keypair();
    let quote = quote_utxo(quote_terms(&maker, None));
    let quote_input = quote
        .to_input_utxo(INPUT_TREE_ID, QUOTE_LEAF_INDEX)
        .unwrap();
    let mut fixture = Fixture {
        program: circuit::CancelQuote {
            private: circuit::CancelQuotePrivateInputs {
                tx_context: context(&fe(43)),
                terms: circuit::QuoteTerms::default(),
                quote_utxo: spr_program::circuit::Utxo::dummy(),
            },
            public: circuit::CancelQuotePublicInputs {
                expiry: EXPIRY,
                maker_owner_pk_field: identity_field(&maker),
            },
        },
        records: InputRecords::new(),
        sender: maker,
    };
    fixture.program.private.terms = circuit::QuoteTerms::from_client(
        quote.terms.circuit_terms().unwrap(),
        &mut fixture.records,
    )
    .unwrap();
    fixture.program.private.quote_utxo = fixture.utxo(&quote_input);
    fixture
}

/// Proves `inputs` with the checked circuit, verifies the proof against `vk`
/// with the transaction's public hash, and checks a tampered public hash
/// fails. Returns the program transaction and the proof.
pub fn prove_and_verify<P: Execute>(
    id: CircuitId,
    inputs: &Fixture<P>,
    vk: &groth16_solana::groth16::Groth16Verifyingkey,
) -> (ProgramTransaction, spr_program::CompressedProof) {
    let tx = inputs.create_program_transaction().unwrap();
    let (_private, circuit) = perps_sdk::proof::checked_circuit(id).unwrap();
    circuit.check_constrained(tx.proof_inputs.clone()).unwrap();
    let proof = perps_sdk::proof::prove(id, &tx.proof_inputs).unwrap();
    assert!(
        proof.a.iter().any(|&byte| byte != 0),
        "proof_a must not be all zero"
    );
    let a = groth16_solana::decompression::decompress_g1(&proof.a).unwrap();
    let b = groth16_solana::decompression::decompress_g2(&proof.b).unwrap();
    let c = groth16_solana::decompression::decompress_g1(&proof.c).unwrap();
    groth16_solana::groth16::Groth16Verifier::new(&a, &b, &c, &[tx.public_hash], vk)
        .unwrap()
        .verify()
        .unwrap();
    let mut tampered = tx.public_hash;
    tampered[31] ^= 1;
    assert!(
        groth16_solana::groth16::Groth16Verifier::new(&a, &b, &c, &[tampered], vk)
            .unwrap()
            .verify()
            .is_err(),
        "verification must fail for a tampered public input"
    );
    (tx, proof)
}

/// Checks that the native execution and the compiled circuit both accept
/// `inputs`, without proving.
pub fn assert_valid<P: Execute>(id: CircuitId, inputs: &Fixture<P>) -> ProgramTransaction {
    let tx = inputs.create_program_transaction().unwrap();
    let (_private, circuit) = perps_sdk::proof::checked_circuit(id).unwrap();
    circuit.check_constrained(tx.proof_inputs.clone()).unwrap();
    tx
}

/// Checks that both the native execution and the compiled circuit refuse
/// `inputs`.
pub fn assert_invalid<P: Execute>(id: CircuitId, inputs: &Fixture<P>) {
    let native = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        spr_program::checked(|| inputs.execute())
    }));
    assert!(
        !matches!(native, Ok(Ok(_))),
        "the native circuit accepted invalid inputs"
    );
    let (_private, circuit) = perps_sdk::proof::checked_circuit(id).unwrap();
    let error = circuit
        .check(ProgramProofInputs::new(&inputs.program, Field::zero()))
        .expect_err("the compiled circuit must reject invalid inputs");
    assert!(
        error.to_string().contains("program.execute()"),
        "expected the program's constraint to fail before the outer public-hash check: {error}"
    );
}
