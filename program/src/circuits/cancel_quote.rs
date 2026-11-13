use spr_program::{
    circuit::{
        CheckedTransaction, ConfidentialTransaction, PublicInputs, TokenUtxos, Utxo, UtxoTrait,
    },
    input, require_eq, CircuitError, ClientInputs, Field, TxContext, ZkProgram,
};

use super::{perp_terms::spend_quote, QuoteTerms};

/// A maker reclaims the margin of a quote nobody accepted before it expired.
#[input]
#[derive(ZkProgram)]
pub struct CancelQuote {
    pub private: CancelQuotePrivateInputs,
    pub public: CancelQuotePublicInputs,
}

#[input]
#[derive(ClientInputs)]
pub struct CancelQuotePrivateInputs {
    pub tx_context: TxContext,
    pub terms: QuoteTerms,
    /// The quote UTXO, which commits `terms`.
    pub quote_utxo: Utxo,
}

#[input]
#[derive(PublicInputs)]
pub struct CancelQuotePublicInputs {
    /// The instruction's `quote_expiry`; the program checks `now > expiry`.
    pub expiry: u64,
    /// `hash_bytes` of the maker signer's tagged pubkey, which the program
    /// computes from the signer account.
    pub maker_owner_pk_field: Field,
}

impl CancelQuote {
    /// 1. Spend the quote UTXO, which commits the terms.
    /// 2. Check the public expiry.
    /// 3. Check the signer is the maker.
    /// 4. Refund the whole margin to the maker.
    /// 5. Check the transaction: input `[quote UTXO]`, output `[margin]`.
    pub fn execute(&self) -> Result<CheckedTransaction, CircuitError> {
        let private = &self.private;
        let public = &self.public;

        // 1. Spend the quote UTXO, which commits the terms.
        let mut quote = spend_quote(&private.quote_utxo, &private.terms)?;

        // 2. Check the public expiry.
        require_eq(public.expiry, quote.expiry);

        // 3. The terms commit the maker's owner, whose signing key identity
        // the program computes from the signer.
        let maker = quote.maker;
        require_eq(maker.key().identity()?, public.maker_owner_pk_field);

        // 4. Refund the whole margin to the maker.
        let mut margin_output = TokenUtxos::new_init(&maker, &quote.asset());
        quote.transfer_all(&mut margin_output)?;

        // 5. Check the transaction.
        ConfidentialTransaction::new(&private.tx_context, public)
            .with(quote)
            .with(margin_output)
            .check()
    }
}
