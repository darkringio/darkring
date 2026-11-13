use borsh::{BorshDeserialize, BorshSerialize};
use spr_program::{
    circuit::{DataUtxo, Utxo, UtxoTrait},
    input, poseidon, require, require_eq, CircuitError, ClientInputs, Field, UtxoOwner, UtxoState,
};

/// The terms of a maker's quote, the quote UTXO's state
/// (`DataUtxo<QuoteTerms>`). The quote UTXO holds the maker's margin; a
/// trader accepts the quote by adding `trader_margin`, which opens the
/// position. Every term is private: only the maker and the trader it is
/// encrypted to can read it.
#[input]
#[derive(Default, PartialEq, Eq, BorshSerialize, BorshDeserialize, UtxoState, ClientInputs)]
pub struct QuoteTerms {
    /// The oracle price feed id, as the big-endian high and low halves of its
    /// 32 bytes. Two integers rather than a byte array, so that settle can
    /// compare them with public inputs the program reads from the oracle
    /// account.
    pub feed_hi: u128,
    pub feed_lo: u128,
    /// The trader's direction; the maker takes the other side.
    pub trader_is_long: bool,
    /// The position size, scaled by [`SIZE_SCALE`](super::SIZE_SCALE).
    pub size: u64,
    /// The entry price, in the feed's price ticks.
    pub entry_price: u64,
    /// The margin the trader must add to open the position.
    pub trader_margin: u64,
    /// The maker may settle once the trader's equity is at or below this.
    pub maintenance_margin: u64,
    /// The maker, the owner of the source the quote spends: receives its
    /// share at settle and the margin back on cancel.
    pub maker: UtxoOwner,
    /// Unix seconds; the quote can be accepted until then and cancelled
    /// after. Revealed at open and cancel and checked against the Clock by
    /// the program.
    pub expiry: u64,
    /// The designated trader's owner identity (`solana_owner_identity` of its
    /// Solana address) as 32 big-endian bytes, or zero for a quote anyone
    /// holding the opening may accept.
    pub taker_pk_fe: [u8; 32],
}

impl QuoteTerms {
    /// The terms describe a position that can open and is not liquidatable
    /// at its own entry price. The integer inputs are range-checked on entry.
    pub(super) fn assert_valid(&self) {
        require(self.size != 0);
        require(self.entry_price != 0);
        require(self.trader_margin != 0);
        require(self.maintenance_margin < self.trader_margin);
    }

    /// An open quote (`taker_pk_fe == 0`) accepts any trader; otherwise the
    /// trader input's owner must be `Poseidon(taker_pk_fe, nullifier_pk)`,
    /// with the input owner's own nullifier public key. One constraint:
    /// `taker_pk_fe * (owner_hash - Poseidon(taker_pk_fe, nullifier_pk)) == 0`.
    pub(super) fn assert_trader(&self, trader: &UtxoOwner) -> Result<(), CircuitError> {
        let taker_pk_fe = self.taker_pk_fe_field();
        let designated = poseidon(&[taker_pk_fe, trader.nullifier_pk()])?;
        require_eq(taker_pk_fe * (trader.hash()? - designated), Field::zero());
        Ok(())
    }

    /// `taker_pk_fe` as a field element: its bytes packed big-endian, a
    /// linear combination with no constraint of its own.
    fn taker_pk_fe_field(&self) -> Field {
        let mut packed = Field::zero();
        for byte in self.taker_pk_fe.iter() {
            packed = packed * Field::from(256u64) + Field::from(*byte);
        }
        packed
    }
}

/// The terms of an open position, the position UTXO's state
/// (`DataUtxo<PositionTerms>`). The position UTXO holds both margins; the
/// maker's is its amount less `trader_margin`. The open circuit copies every
/// term from the accepted quote and adds the trader.
#[input]
#[derive(Default, PartialEq, Eq, BorshSerialize, BorshDeserialize, UtxoState, ClientInputs)]
pub struct PositionTerms {
    pub feed_hi: u128,
    pub feed_lo: u128,
    pub trader_is_long: bool,
    pub size: u64,
    pub entry_price: u64,
    pub trader_margin: u64,
    pub maintenance_margin: u64,
    pub maker: UtxoOwner,
    /// The trader, the owner of the margin input open spends: receives its
    /// share at settle and may settle at any time.
    pub trader: UtxoOwner,
}

/// Spends the quote UTXO, which commits `terms` and holds a nonzero margin,
/// and leaves no output. The owner stays a free witness: SPP spends the quote
/// UTXO only with the maker's authority PDA signature.
pub(super) fn spend_quote(
    input: &Utxo,
    terms: &QuoteTerms,
) -> Result<DataUtxo<QuoteTerms>, CircuitError> {
    let quote = DataUtxo::new_mut(input, terms)?.close();
    require(quote.amount()? != 0);
    Ok(quote)
}
