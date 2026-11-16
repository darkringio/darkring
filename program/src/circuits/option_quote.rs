use spr_program::{
    circuit::{
        CheckedTransaction, ConfidentialTransaction, DataUtxo, PublicInputs, TokenUtxos, Utxo,
        UtxoTrait,
    },
    input, require, require_eq, CircuitError, ClientInputs, Field, ShieldedAddress, TxContext,
    ZkProgram,
};

use super::OptionQuoteTerms;

/// A maker quotes a dated contract: it locks its collateral in an option
/// quote UTXO that commits the terms of the contract it offers.
#[input]
#[derive(ZkProgram)]
pub struct OptionQuote {
    pub private: OptionQuotePrivateInputs,
    pub public: OptionQuotePublicInputs,
}

#[input]
#[derive(ClientInputs)]
pub struct OptionQuotePrivateInputs {
    pub tx_context: TxContext,
    /// The maker's spent source UTXO. SPP authorizes the spend with the
    /// maker's signature; its owner becomes the quote's maker and receives
    /// the change.
    pub source: Utxo,
    /// The collateral locked in the quote UTXO: with the premium, the most
    /// the contract can pay.
    pub collateral: u64,
    /// The quote terms other than the maker; see [`OptionQuoteTerms`].
    pub feed_hi: u128,
    pub feed_lo: u128,
    pub kind: u8,
    pub size: u64,
    pub strike: u64,
    pub premium: u64,
    pub term_end: u64,
    pub expiry: u64,
    pub taker_pk_fe: [u8; 32],
}

#[input]
#[derive(PublicInputs)]
pub struct OptionQuotePublicInputs {
    /// `hash_bytes` of the maker signer's tagged pubkey, which the program
    /// computes from the signer account.
    pub maker_owner_pk_field: Field,
    /// The quote UTXO's address: the maker's authority PDA with the trader's
    /// viewing key, so the trader can decrypt the quote. The program derives
    /// the PDA from the maker signer and hashes the address, which binds the
    /// quote UTXO to the PDA that only the program signs for.
    pub quote_owner: ShieldedAddress,
}

impl OptionQuote {
    /// 1. Spend the source UTXO and check its owner is the maker signer.
    /// 2. Create the quote UTXO, owned by the public `quote_owner`, and lock
    ///    `collateral` in it; the rest of the source returns to its owner as
    ///    change.
    /// 3. Set and check the quote terms; the source owner is the maker.
    /// 4. Check the transaction: outputs `[change, quote UTXO]`.
    pub fn execute(&self) -> Result<CheckedTransaction, CircuitError> {
        let private = &self.private;

        // 1. Spend the source UTXO of the maker signer.
        let mut tokens = TokenUtxos::new_mut(core::array::from_ref(&private.source))?;
        let maker = tokens.owner();
        require_eq(maker.key().identity()?, self.public.maker_owner_pk_field);

        // 2. Lock `collateral` in the quote UTXO.
        require(private.collateral != 0);
        let mut quote = DataUtxo::<OptionQuoteTerms>::new_init(self.public.quote_owner.owner())
            .with_asset(&tokens.asset())?;
        tokens.transfer(&mut quote, &private.collateral)?;

        // 3. Set and check the quote terms.
        quote.feed_hi = private.feed_hi;
        quote.feed_lo = private.feed_lo;
        quote.kind = private.kind;
        quote.size = private.size;
        quote.strike = private.strike;
        quote.premium = private.premium;
        quote.term_end = private.term_end;
        quote.maker = maker;
        quote.expiry = private.expiry;
        quote.taker_pk_fe = private.taker_pk_fe;
        quote.assert_valid();

        // 4. Check the transaction: outputs `[change, quote UTXO]`.
        ConfidentialTransaction::new(&private.tx_context, &self.public)
            .with(tokens)
            .with(quote)
            .check()
    }
}
