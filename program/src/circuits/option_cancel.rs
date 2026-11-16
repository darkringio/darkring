use spr_program::{
    circuit::{
        CheckedTransaction, ConfidentialTransaction, PublicInputs, TokenUtxos, Utxo, UtxoTrait,
    },
    input, require_eq, CircuitError, ClientInputs, Field, TxContext, ZkProgram,
};

use super::{option_terms::spend_option_quote, OptionQuoteTerms};

/// A maker reclaims the collateral of an option quote nobody accepted before
/// it expired.
#[input]
#[derive(ZkProgram)]
pub struct OptionCancel {
    pub private: OptionCancelPrivateInputs,
    pub public: OptionCancelPublicInputs,
}

#[input]
#[derive(ClientInputs)]
pub struct OptionCancelPrivateInputs {
    pub tx_context: TxContext,
    pub terms: OptionQuoteTerms,
    /// The quote UTXO, which commits `terms`.
    pub quote_utxo: Utxo,
}

#[input]
#[derive(PublicInputs)]
pub struct OptionCancelPublicInputs {
    /// The instruction's `quote_expiry`; the program checks `now > expiry`.
    pub expiry: u64,
    /// `hash_bytes` of the maker signer's tagged pubkey, which the program
    /// computes from the signer account.
    pub maker_owner_pk_field: Field,
}

impl OptionCancel {
    /// 1. Spend the quote UTXO, which commits the terms.
    /// 2. Check the public expiry.
    /// 3. Check the signer is the maker.
    /// 4. Refund the whole collateral to the maker.
    /// 5. Check the transaction: input `[quote UTXO]`, output `[collateral]`.
    pub fn execute(&self) -> Result<CheckedTransaction, CircuitError> {
        let private = &self.private;
        let public = &self.public;

        // 1. Spend the quote UTXO, which commits the terms.
        let mut quote = spend_option_quote(&private.quote_utxo, &private.terms)?;

        // 2. Check the public expiry.
        require_eq(public.expiry, quote.expiry);

        // 3. The terms commit the maker's owner, whose signing key identity
        // the program computes from the signer.
        let maker = quote.maker;
        require_eq(maker.key().identity()?, public.maker_owner_pk_field);

        // 4. Refund the whole collateral to the maker.
        let mut refund = TokenUtxos::new_init(&maker, &quote.asset());
        quote.transfer_all(&mut refund)?;

        // 5. Check the transaction.
        ConfidentialTransaction::new(&private.tx_context, public)
            .with(quote)
            .with(refund)
            .check()
    }
}
