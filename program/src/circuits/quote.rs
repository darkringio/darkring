use spr_program::{
    circuit::{
        CheckedTransaction, ConfidentialTransaction, DataUtxo, PublicInputs, TokenUtxos, Utxo,
        UtxoTrait,
    },
    input, require, require_eq, CircuitError, ClientInputs, Field, ShieldedAddress, TxContext,
    ZkProgram,
};

use super::QuoteTerms;

/// A maker posts a quote: it locks its margin in a quote UTXO that commits
/// the terms of the position it offers.
#[input]
#[derive(ZkProgram)]
pub struct Quote {
    pub private: QuotePrivateInputs,
    pub public: QuotePublicInputs,
}

#[input]
#[derive(ClientInputs)]
pub struct QuotePrivateInputs {
    pub tx_context: TxContext,
    /// The maker's spent source UTXO. SPP authorizes the spend with the
    /// maker's signature; its owner becomes the quote's maker and receives
    /// the change.
    pub source: Utxo,
    /// The margin locked in the quote UTXO.
    pub maker_margin: u64,
    /// The quote terms other than the maker; see [`QuoteTerms`].
    pub feed_hi: u128,
    pub feed_lo: u128,
    pub trader_is_long: bool,
    pub size: u64,
    pub entry_price: u64,
    pub trader_margin: u64,
    pub maintenance_margin: u64,
    pub expiry: u64,
    pub taker_pk_fe: [u8; 32],
}

#[input]
#[derive(PublicInputs)]
pub struct QuotePublicInputs {
    /// `hash_bytes` of the maker signer's tagged pubkey, which the program
    /// computes from the signer account.
    pub maker_owner_pk_field: Field,
    /// The quote UTXO's address: the maker's authority PDA with the trader's
    /// viewing key, so the trader can decrypt the quote. The program derives
    /// the PDA from the maker signer and hashes the address, which binds the
    /// quote UTXO to the PDA that only the program signs for.
    pub quote_owner: ShieldedAddress,
}

impl Quote {
    /// 1. Spend the source UTXO and check its owner is the maker signer.
    /// 2. Create the quote UTXO, owned by the public `quote_owner`, and lock
    ///    `maker_margin` in it; the rest of the source returns to its owner
    ///    as change.
    /// 3. Set and check the quote terms; the source owner is the maker.
    /// 4. Check the transaction: outputs `[change, quote UTXO]`.
    pub fn execute(&self) -> Result<CheckedTransaction, CircuitError> {
        let private = &self.private;

        // 1. Spend the source UTXO of the maker signer.
        let mut tokens = TokenUtxos::new_mut(core::array::from_ref(&private.source))?;
        let maker = tokens.owner();
        require_eq(maker.key().identity()?, self.public.maker_owner_pk_field);

        // 2. Lock `maker_margin` in the quote UTXO.
        require(private.maker_margin != 0);
        let mut quote = DataUtxo::<QuoteTerms>::new_init(self.public.quote_owner.owner())
            .with_asset(&tokens.asset())?;
        tokens.transfer(&mut quote, &private.maker_margin)?;

        // 3. Set and check the quote terms.
        quote.feed_hi = private.feed_hi;
        quote.feed_lo = private.feed_lo;
        quote.trader_is_long = private.trader_is_long;
        quote.size = private.size;
        quote.entry_price = private.entry_price;
        quote.trader_margin = private.trader_margin;
        quote.maintenance_margin = private.maintenance_margin;
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
