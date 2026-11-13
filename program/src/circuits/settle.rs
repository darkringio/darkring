use spr_program::{
    circuit::{
        CheckedTransaction, ConfidentialTransaction, DataUtxo, PublicInputs, TokenUtxos, Utxo,
        UtxoTrait,
    },
    input, require, require_eq, CircuitError, ClientInputs, Field, ShieldedAddress, TxContext,
    ZkProgram,
};

use super::{
    payout::{is_liquidatable, trader_payout},
    shared::assert_blinding_seed,
    PositionTerms,
};

/// Settles a position at the oracle price: splits the position UTXO between
/// the trader and the maker. The trader may settle at any time (a close);
/// the maker only when the trader's equity is at or below the maintenance
/// margin (a liquidation). The proof shows the split and the right to settle
/// without revealing the size, the direction, the margins, the entry price
/// or the liquidation price.
#[input]
#[derive(ZkProgram)]
pub struct Settle {
    pub private: SettlePrivateInputs,
    pub public: SettlePublicInputs,
}

#[input]
#[derive(ClientInputs)]
pub struct SettlePrivateInputs {
    pub tx_context: TxContext,
    pub terms: PositionTerms,
    /// The position UTXO, which commits `terms` and holds both margins.
    pub position_utxo: Utxo,
    /// The parties' shielded addresses, whose viewing keys the payouts are
    /// encrypted to. The circuit does not read them: the payout owners are
    /// the committed `terms.trader` and `terms.maker`.
    pub trader_address: ShieldedAddress,
    pub maker_address: ShieldedAddress,
}

#[input]
#[derive(PublicInputs)]
pub struct SettlePublicInputs {
    /// The feed id of the oracle account the program read the price from,
    /// as the big-endian high and low halves of its 32 bytes.
    pub feed_hi: u128,
    pub feed_lo: u128,
    /// The oracle price, in the feed's price ticks. The program reads it
    /// from the oracle account and checks that it is fresh.
    pub price: u64,
    /// `hash_bytes` of the settling signer's tagged pubkey, which the
    /// program computes from the signer account.
    pub signer_owner_pk_field: Field,
    /// The first SPP input's nullifier, which the program reads from the
    /// transact.
    pub first_nullifier: Field,
}

impl Settle {
    /// 1. Check the blinding seed derives from the position opening.
    /// 2. Spend the position UTXO, which commits the terms.
    /// 3. Check the price comes from the position's feed.
    /// 4. Compute the trader's payout at the public price.
    /// 5. Check the signer may settle: the trader always, the maker only on
    ///    a liquidatable position.
    /// 6. Pay the trader its payout and the maker the rest.
    /// 7. Check the transaction: input `[position UTXO]`, outputs
    ///    `[trader payout, maker payout]`.
    /// 8. Check the first nullifier the output blindings derive from is the
    ///    published one.
    pub fn execute(&self) -> Result<CheckedTransaction, CircuitError> {
        let private = &self.private;
        let public = &self.public;

        // 1. Both parties hold the position opening and can compute the
        // payouts from the public price. With the published first nullifier,
        // the seed lets the party that did not settle recover its payout
        // even if the settler omits or corrupts its ciphertext.
        assert_blinding_seed(
            &private.tx_context,
            crate::SETTLE_BLINDING_DOMAIN,
            &private.position_utxo.blinding,
        )?;

        // 2. Spend the position UTXO, which commits the terms.
        let mut position = DataUtxo::new_mut(&private.position_utxo, &private.terms)?.close();
        let total = position.amount()?;

        // 3. The price is the position's feed's.
        require_eq(public.feed_hi, position.feed_hi);
        require_eq(public.feed_lo, position.feed_lo);

        // 4. The trader's payout; the maker receives the rest.
        require(position.trader_margin <= total);
        let payout = trader_payout(
            position.trader_is_long,
            position.size,
            position.entry_price,
            position.trader_margin,
            total,
            public.price,
        );

        // 5. The trader may always settle; the maker only when the trader's
        // equity is at or below the maintenance margin. Bitwise operators,
        // so that neither side of the condition is skipped.
        let trader = position.trader;
        let maker = position.maker;
        let by_trader = trader.key().identity()? == public.signer_owner_pk_field;
        let by_maker = maker.key().identity()? == public.signer_owner_pk_field;
        let liquidatable = is_liquidatable(payout, position.maintenance_margin);
        require(by_trader | (by_maker & liquidatable));

        // 6. Pay both parties. Each payout keeps a real output at a zero
        // balance, so the transaction has the same shape whether or not a
        // side was wiped out.
        let asset = position.asset();
        let mut trader_output = TokenUtxos::new_init(&trader, &asset).with_zero_output();
        let mut maker_output = TokenUtxos::new_init(&maker, &asset).with_zero_output();
        position.transfer(&mut trader_output, &payout)?;
        position.transfer_all(&mut maker_output)?;

        // 7. Check the transaction.
        let checked = ConfidentialTransaction::new(&private.tx_context, public)
            .with(position)
            .with(trader_output)
            .with(maker_output)
            .check()?;

        // 8. Check the published first nullifier.
        require_eq(checked.first_nullifier, public.first_nullifier);
        Ok(checked)
    }
}
