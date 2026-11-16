use borsh::{BorshDeserialize, BorshSerialize};
use spr_program::{
    circuit::{DataUtxo, Utxo, UtxoTrait},
    input, poseidon, require, require_eq, CircuitError, ClientInputs, Field, UtxoOwner, UtxoState,
};

use super::option_payout::{is_prediction, KIND_MAX};

/// The terms of a maker's quote for a dated contract, the option quote
/// UTXO's state (`DataUtxo<OptionQuoteTerms>`). The quote UTXO holds the
/// maker's collateral; a trader accepts the quote by adding `premium`, which
/// opens the contract. Every term is private: only the maker and the trader
/// it is encrypted to can read it.
#[input]
#[derive(Default, PartialEq, Eq, BorshSerialize, BorshDeserialize, UtxoState, ClientInputs)]
pub struct OptionQuoteTerms {
    /// The oracle price feed id, as the big-endian high and low halves of its
    /// 32 bytes, which settle compares with the oracle account's.
    pub feed_hi: u128,
    pub feed_lo: u128,
    /// What the contract is: a call, a put, or a prediction that the price
    /// ends above or below the strike. See [`super::option_payout`].
    pub kind: u8,
    /// The size of a call or a put, scaled by
    /// [`SIZE_SCALE`](super::SIZE_SCALE). A prediction does not read it.
    pub size: u64,
    /// The strike, in the feed's price ticks.
    pub strike: u64,
    /// What the trader pays to open the contract.
    pub premium: u64,
    /// Unix seconds; the end of the contract's term. From the first oracle
    /// price published at or after it, either party may settle.
    pub term_end: u64,
    /// The maker, the owner of the source the quote spends: receives its
    /// share at settle and the collateral back on cancel.
    pub maker: UtxoOwner,
    /// Unix seconds; the quote can be accepted until then and cancelled
    /// after. Revealed at open and cancel and checked against the Clock by
    /// the program.
    pub expiry: u64,
    /// The designated trader's owner identity as 32 big-endian bytes, or
    /// zero for a quote anyone holding the opening may accept.
    pub taker_pk_fe: [u8; 32],
}

impl OptionQuoteTerms {
    /// The terms describe a contract that can open and can settle. The
    /// integer inputs are range-checked on entry.
    pub(super) fn assert_valid(&self) {
        require(self.kind <= KIND_MAX);
        require(self.strike != 0);
        require(self.premium != 0);
        // A contract without an end could never be settled by its maker, and
        // a prediction without one by nobody.
        require(self.term_end != 0);
        // A call or a put of no size is worth nothing at any price.
        require(is_prediction(self.kind) | (self.size != 0));
    }

    /// An open quote (`taker_pk_fe == 0`) accepts any trader; otherwise the
    /// trader input's owner must be `Poseidon(taker_pk_fe, nullifier_pk)`,
    /// with the input owner's own nullifier public key.
    pub(super) fn assert_trader(&self, trader: &UtxoOwner) -> Result<(), CircuitError> {
        let mut taker_pk_fe = Field::zero();
        for byte in self.taker_pk_fe.iter() {
            taker_pk_fe = taker_pk_fe * Field::from(256u64) + Field::from(*byte);
        }
        let designated = poseidon(&[taker_pk_fe, trader.nullifier_pk()])?;
        require_eq(taker_pk_fe * (trader.hash()? - designated), Field::zero());
        Ok(())
    }
}

/// The terms of an open contract, the option UTXO's state
/// (`DataUtxo<OptionTerms>`). The option UTXO holds the premium and the
/// maker's collateral. The open circuit copies every term from the accepted
/// quote and adds the trader.
#[input]
#[derive(Default, PartialEq, Eq, BorshSerialize, BorshDeserialize, UtxoState, ClientInputs)]
pub struct OptionTerms {
    pub feed_hi: u128,
    pub feed_lo: u128,
    pub kind: u8,
    pub size: u64,
    pub strike: u64,
    /// What the trader paid. Settle does not read it; it is what the trader's
    /// records and a maker that restarts need to tell the result.
    pub premium: u64,
    pub term_end: u64,
    pub maker: UtxoOwner,
    /// The trader, the owner of the premium input open spends: receives its
    /// share at settle.
    pub trader: UtxoOwner,
}

/// Spends the option quote UTXO, which commits `terms` and holds a nonzero
/// collateral, and leaves no output. The owner stays a free witness: SPP
/// spends the quote UTXO only with the maker's authority PDA signature.
pub(super) fn spend_option_quote(
    input: &Utxo,
    terms: &OptionQuoteTerms,
) -> Result<DataUtxo<OptionQuoteTerms>, CircuitError> {
    let quote = DataUtxo::new_mut(input, terms)?.close();
    require(quote.amount()? != 0);
    Ok(quote)
}
