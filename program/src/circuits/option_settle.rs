use spr_program::{
    circuit::{
        CheckedTransaction, ConfidentialTransaction, DataUtxo, PublicInputs, TokenUtxos, Utxo,
        UtxoTrait,
    },
    input, require, require_eq, CircuitError, ClientInputs, Field, ShieldedAddress, TxContext,
    ZkProgram,
};

use super::{
    option_payout::{is_matured, may_settle, option_payout},
    shared::assert_blinding_seed,
    OptionTerms,
};

/// Settles a dated contract at the oracle price: splits the option UTXO
/// between the trader and the maker. The trader may exercise a call or a put
/// at any time; either party may settle any contract once an oracle price
/// published at or after the end of its term is in. The proof shows the
/// split and the right to settle without revealing the kind, the strike, the
/// size, the premium or when the term ends.
#[input]
#[derive(ZkProgram)]
pub struct OptionSettle {
    pub private: OptionSettlePrivateInputs,
    pub public: OptionSettlePublicInputs,
}

#[input]
#[derive(ClientInputs)]
pub struct OptionSettlePrivateInputs {
    pub tx_context: TxContext,
    pub terms: OptionTerms,
    /// The option UTXO, which commits `terms` and holds the premium and the
    /// collateral.
    pub option_utxo: Utxo,
    /// The parties' shielded addresses, whose viewing keys the payouts are
    /// encrypted to. The circuit does not read them: the payout owners are
    /// the committed `terms.trader` and `terms.maker`.
    pub trader_address: ShieldedAddress,
    pub maker_address: ShieldedAddress,
}

#[input]
#[derive(PublicInputs)]
pub struct OptionSettlePublicInputs {
    /// The feed id of the oracle account the program read the price from,
    /// as the big-endian high and low halves of its 32 bytes.
    pub feed_hi: u128,
    pub feed_lo: u128,
    /// The oracle price, in the feed's price ticks. The program reads it
    /// from the oracle account and checks that it is fresh.
    pub price: u64,
    /// When the oracle published that price, in unix seconds, which the
    /// program reads from the same account.
    pub publish_time: u64,
    /// `hash_bytes` of the settling signer's tagged pubkey, which the
    /// program computes from the signer account.
    pub signer_owner_pk_field: Field,
    /// The first SPP input's nullifier, which the program reads from the
    /// transact.
    pub first_nullifier: Field,
}

impl OptionSettle {
    /// 1. Check the blinding seed derives from the option opening.
    /// 2. Spend the option UTXO, which commits the terms.
    /// 3. Check the price comes from the contract's feed.
    /// 4. Compute the trader's payout at the public price.
    /// 5. Check the signer may settle at the public publish time.
    /// 6. Pay the trader its payout and the maker the rest.
    /// 7. Check the transaction: input `[option UTXO]`, outputs
    ///    `[trader payout, maker payout]`.
    /// 8. Check the first nullifier the output blindings derive from is the
    ///    published one.
    pub fn execute(&self) -> Result<CheckedTransaction, CircuitError> {
        let private = &self.private;
        let public = &self.public;

        // 1. Both parties hold the option opening and can compute the
        // payouts from the public price. With the published first nullifier,
        // the seed lets the party that did not settle recover its payout
        // even if the settler omits or corrupts its ciphertext.
        assert_blinding_seed(
            &private.tx_context,
            crate::OPTION_SETTLE_BLINDING_DOMAIN,
            &private.option_utxo.blinding,
        )?;

        // 2. Spend the option UTXO, which commits the terms.
        let mut option = DataUtxo::new_mut(&private.option_utxo, &private.terms)?.close();
        let total = option.amount()?;

        // 3. The price is the contract's feed's.
        require_eq(public.feed_hi, option.feed_hi);
        require_eq(public.feed_lo, option.feed_lo);

        // 4. The trader's payout; the maker receives the rest.
        let payout = option_payout(option.kind, option.size, option.strike, total, public.price);

        // 5. Who may settle, and when. Bitwise operators inside
        // `may_settle`, so that neither side of the condition is skipped.
        let trader = option.trader;
        let maker = option.maker;
        let by_trader = trader.key().identity()? == public.signer_owner_pk_field;
        let by_maker = maker.key().identity()? == public.signer_owner_pk_field;
        let matured = is_matured(public.publish_time, option.term_end);
        require(may_settle(option.kind, by_trader, by_maker, matured));

        // 6. Pay both parties. Each payout keeps a real output at a zero
        // balance, so the transaction has the same shape whoever won.
        let asset = option.asset();
        let mut trader_output = TokenUtxos::new_init(&trader, &asset).with_zero_output();
        let mut maker_output = TokenUtxos::new_init(&maker, &asset).with_zero_output();
        option.transfer(&mut trader_output, &payout)?;
        option.transfer_all(&mut maker_output)?;

        // 7. Check the transaction.
        let checked = ConfidentialTransaction::new(&private.tx_context, public)
            .with(option)
            .with(trader_output)
            .with(maker_output)
            .check()?;

        // 8. Check the published first nullifier.
        require_eq(checked.first_nullifier, public.first_nullifier);
        Ok(checked)
    }
}
