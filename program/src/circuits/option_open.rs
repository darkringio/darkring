use spr_program::{
    circuit::{
        CheckedTransaction, ConfidentialTransaction, DataUtxo, PublicInputs, TokenUtxos, Utxo,
        UtxoTrait,
    },
    input, require_eq, CircuitError, ClientInputs, Field, ShieldedAddress, TxContext, ZkProgram,
};

use super::{
    option_terms::spend_option_quote, shared::assert_blinding_seed, OptionQuoteTerms, OptionTerms,
};

/// A trader accepts an option quote: the maker's collateral and the trader's
/// premium move into one option UTXO that commits the contract's terms.
#[input]
#[derive(ZkProgram)]
pub struct OptionOpen {
    pub private: OptionOpenPrivateInputs,
    pub public: OptionOpenPublicInputs,
}

#[input]
#[derive(ClientInputs)]
pub struct OptionOpenPrivateInputs {
    pub tx_context: TxContext,
    pub terms: OptionQuoteTerms,
    /// The quote UTXO, which commits `terms` and holds the maker's
    /// collateral.
    pub quote_utxo: Utxo,
    /// The trader's premium input, of the quote's collateral asset. Its
    /// owner becomes the contract's trader and receives the change.
    pub trader_in: Utxo,
}

#[input]
#[derive(PublicInputs)]
pub struct OptionOpenPublicInputs {
    /// The transact's `expiry_unix_ts`; the program checks `now <= expiry`.
    pub expiry: u64,
    /// The first SPP input's nullifier, which the program reads from the
    /// transact.
    pub first_nullifier: Field,
    /// The option UTXO's address: the maker's authority PDA with the maker's
    /// viewing key. The program derives the PDA from the maker the
    /// instruction names and hashes the address, which binds the option UTXO
    /// to the PDA that only the program signs for.
    pub option_owner: ShieldedAddress,
}

impl OptionOpen {
    /// 1. Check the blinding seed derives from the quote opening.
    /// 2. Spend the quote UTXO, which commits the terms.
    /// 3. Check the quote terms and the public expiry.
    /// 4. Spend the trader's premium input, of the quote's asset, and check
    ///    the designated trader.
    /// 5. Create the option UTXO, owned by the public `option_owner`, and
    ///    move the collateral and the premium into it; the rest of the
    ///    trader's input returns to the trader as change.
    /// 6. Set the contract's terms from the quote; the premium input's owner
    ///    is the trader.
    /// 7. Check the transaction: inputs `[quote UTXO, trader input]`,
    ///    outputs `[option UTXO, change]`.
    /// 8. Check the first nullifier the output blindings derive from is the
    ///    published one.
    pub fn execute(&self) -> Result<CheckedTransaction, CircuitError> {
        let private = &self.private;
        let public = &self.public;

        // 1. Both parties hold the quote opening. With the published first
        // nullifier, the seed lets the maker derive the option UTXO's
        // blinding without the trader's ciphertext.
        assert_blinding_seed(
            &private.tx_context,
            crate::OPTION_OPEN_BLINDING_DOMAIN,
            &private.quote_utxo.blinding,
        )?;

        // 2. Spend the quote UTXO, which commits the terms.
        let mut quote = spend_option_quote(&private.quote_utxo, &private.terms)?;

        // 3. Check the quote terms and the public expiry.
        quote.assert_valid();
        require_eq(public.expiry, quote.expiry);

        // 4. Spend the trader's premium input and check the designated
        // trader.
        let mut trader_in = TokenUtxos::new_mut(core::array::from_ref(&private.trader_in))?;
        require_eq(trader_in.asset(), quote.asset());
        let trader = trader_in.owner();
        quote.assert_trader(&trader)?;

        // 5. Move the collateral and the premium into the option UTXO.
        let mut option = DataUtxo::<OptionTerms>::new_init(public.option_owner.owner())
            .with_asset(&quote.asset())?;
        let premium = quote.premium;
        quote.transfer_all(&mut option)?;
        trader_in.transfer(&mut option, &premium)?;

        // 6. Set the contract's terms.
        option.feed_hi = quote.feed_hi;
        option.feed_lo = quote.feed_lo;
        option.kind = quote.kind;
        option.size = quote.size;
        option.strike = quote.strike;
        option.premium = premium;
        option.term_end = quote.term_end;
        option.maker = quote.maker;
        option.trader = trader;

        // 7. Check the transaction. The change is registered last: it is an
        // empty output when the input holds exactly the premium.
        let checked = ConfidentialTransaction::new(&private.tx_context, public)
            .with(quote)
            .with(option)
            .with(trader_in)
            .check()?;

        // 8. Check the published first nullifier.
        require_eq(checked.first_nullifier, public.first_nullifier);
        Ok(checked)
    }
}
