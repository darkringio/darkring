use spr_program::{
    circuit::{
        CheckedTransaction, ConfidentialTransaction, DataUtxo, PublicInputs, TokenUtxos, Utxo,
        UtxoTrait,
    },
    input, require_eq, CircuitError, ClientInputs, Field, ShieldedAddress, TxContext, ZkProgram,
};

use super::{perp_terms::spend_quote, shared::assert_blinding_seed, PositionTerms, QuoteTerms};

/// A trader accepts a quote: the quote's margin and the trader's margin move
/// into one position UTXO that commits the position terms.
#[input]
#[derive(ZkProgram)]
pub struct Open {
    pub private: OpenPrivateInputs,
    pub public: OpenPublicInputs,
}

#[input]
#[derive(ClientInputs)]
pub struct OpenPrivateInputs {
    pub tx_context: TxContext,
    pub terms: QuoteTerms,
    /// The quote UTXO, which commits `terms` and holds the maker's margin.
    pub quote_utxo: Utxo,
    /// The trader's margin input, of the quote's collateral asset. Its owner
    /// becomes the position's trader and receives the change.
    pub trader_in: Utxo,
}

#[input]
#[derive(PublicInputs)]
pub struct OpenPublicInputs {
    /// The transact's `expiry_unix_ts`; the program checks `now <= expiry`.
    pub expiry: u64,
    /// The first SPP input's nullifier, which the program reads from the
    /// transact.
    pub first_nullifier: Field,
    /// The position UTXO's address: the maker's authority PDA with the
    /// maker's viewing key. The program derives the PDA from the maker the
    /// instruction names and hashes the address, which binds the position
    /// UTXO to the PDA that only the program signs for.
    pub position_owner: ShieldedAddress,
}

impl Open {
    /// 1. Check the blinding seed derives from the quote opening.
    /// 2. Spend the quote UTXO, which commits the terms.
    /// 3. Check the quote terms and the public expiry.
    /// 4. Spend the trader's margin input, of the quote's asset, and check
    ///    the designated trader.
    /// 5. Create the position UTXO, owned by the public `position_owner`,
    ///    and move both margins into it; the rest of the trader's input
    ///    returns to the trader as change.
    /// 6. Set the position terms from the quote; the margin input's owner is
    ///    the trader.
    /// 7. Check the transaction: inputs `[quote UTXO, trader input]`,
    ///    outputs `[position UTXO, change]`.
    /// 8. Check the first nullifier the output blindings derive from is the
    ///    published one.
    pub fn execute(&self) -> Result<CheckedTransaction, CircuitError> {
        let private = &self.private;
        let public = &self.public;

        // 1. Both parties hold the quote opening. With the published first
        // nullifier, the seed lets the maker derive the position UTXO's
        // blinding without the trader's ciphertext.
        assert_blinding_seed(
            &private.tx_context,
            crate::OPEN_BLINDING_DOMAIN,
            &private.quote_utxo.blinding,
        )?;

        // 2. Spend the quote UTXO, which commits the terms.
        let mut quote = spend_quote(&private.quote_utxo, &private.terms)?;

        // 3. Check the quote terms and the public expiry.
        quote.assert_valid();
        require_eq(public.expiry, quote.expiry);

        // 4. Spend the trader's margin input and check the designated trader.
        let mut trader_in = TokenUtxos::new_mut(core::array::from_ref(&private.trader_in))?;
        require_eq(trader_in.asset(), quote.asset());
        let trader = trader_in.owner();
        quote.assert_trader(&trader)?;

        // 5. Move both margins into the position UTXO.
        let mut position = DataUtxo::<PositionTerms>::new_init(public.position_owner.owner())
            .with_asset(&quote.asset())?;
        let trader_margin = quote.trader_margin;
        quote.transfer_all(&mut position)?;
        trader_in.transfer(&mut position, &trader_margin)?;

        // 6. Set the position terms.
        position.feed_hi = quote.feed_hi;
        position.feed_lo = quote.feed_lo;
        position.trader_is_long = quote.trader_is_long;
        position.size = quote.size;
        position.entry_price = quote.entry_price;
        position.trader_margin = trader_margin;
        position.maintenance_margin = quote.maintenance_margin;
        position.maker = quote.maker;
        position.trader = trader;

        // 7. Check the transaction. The change is registered last: it is an
        // empty output when the input holds exactly the margin.
        let checked = ConfidentialTransaction::new(&private.tx_context, public)
            .with(quote)
            .with(position)
            .with(trader_in)
            .check()?;

        // 8. Check the published first nullifier.
        require_eq(checked.first_nullifier, public.first_nullifier);
        Ok(checked)
    }
}
