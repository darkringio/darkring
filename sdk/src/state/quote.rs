use anyhow::{ensure, Result};
use perps_program::circuits::{self, QuoteTermsClient};
use spr_hasher::primitives::is_canonical_bn254_scalar_be;
use spr_keypair::{ShieldedAddress, X25519Pubkey};
use spr_program::{
    circuit::DataHash, ClientInputs, DataUtxo, EncryptionCodec, InputRecords, ProgramOwner,
};
use spr_transaction::{Mint, SppProofOutputUtxo, WalletUtxo};

use super::{PositionTerms, PositionUtxo};
use crate::{
    err, maker_authority,
    shared::{feed_halves, feed_id, open_blinding_seed},
};

/// Output slot of the position UTXO in the open transaction.
pub const POSITION_SLOT: u32 = 0;

/// The terms of a quote: the position a maker offers, until when it can be
/// accepted and by whom.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuoteTerms {
    /// The oracle price feed id.
    pub feed_id: [u8; 32],
    /// The trader's direction; the maker takes the other side.
    pub trader_is_long: bool,
    /// The position size, scaled by `SIZE_SCALE`: the profit or loss in
    /// collateral base units is `size * |price - entry_price| / SIZE_SCALE`.
    pub size: u64,
    /// The entry price, in the feed's price ticks.
    pub entry_price: u64,
    /// The margin the trader adds to open the position.
    pub trader_margin: u64,
    /// The maker may settle once the trader's equity is at or below this.
    pub maintenance_margin: u64,
    /// The maker's shielded address: receives its share at settle and the
    /// margin back on cancel; its Solana key signs the cancel. The quote UTXO
    /// commits its owner; the viewing key only encrypts its payouts.
    pub maker: ShieldedAddress,
    /// Unix seconds.
    pub expiry: u64,
    /// The designated trader's owner identity ([`crate::shared::identity`]
    /// of its Solana address), or `None` for a quote anyone holding the
    /// opening may accept. It must be a canonical field element
    /// ([`check_trader_identity`]); every builder refuses another.
    pub trader: Option<[u8; 32]>,
}

/// Checks a designated trader identity is a canonical BN254 scalar field
/// element, below the modulus. The circuit packs its 32 big-endian bytes into
/// one field element, so a value at or above the modulus wraps to another
/// trader than the one the bytes name. Zero (an open quote) and every
/// `solana_owner_identity` are canonical.
pub fn check_trader_identity(taker_pk_fe: &[u8; 32]) -> Result<()> {
    ensure!(
        is_canonical_bn254_scalar_be(taker_pk_fe),
        "the designated trader is not a canonical field element"
    );
    Ok(())
}

impl QuoteTerms {
    /// The terms as the circuits take them. Fails on terms the circuits
    /// refuse: a zero size, entry price or trader margin, a maintenance
    /// margin that is not below the trader margin, or a trader identity that
    /// is not a canonical field element.
    pub fn circuit_terms(&self) -> Result<QuoteTermsClient> {
        ensure!(self.size != 0, "the position size is zero");
        ensure!(self.entry_price != 0, "the entry price is zero");
        ensure!(self.trader_margin != 0, "the trader margin is zero");
        ensure!(
            self.maintenance_margin < self.trader_margin,
            "the maintenance margin must be below the trader margin"
        );
        let taker_pk_fe = self.trader.unwrap_or_default();
        check_trader_identity(&taker_pk_fe)?;
        let (feed_hi, feed_lo) = feed_halves(&self.feed_id);
        Ok(QuoteTermsClient {
            feed_hi,
            feed_lo,
            trader_is_long: self.trader_is_long,
            size: self.size,
            entry_price: self.entry_price,
            trader_margin: self.trader_margin,
            maintenance_margin: self.maintenance_margin,
            maker: *InputRecords::new()
                .address(&self.maker)
                .map_err(err)?
                .owner(),
            expiry: self.expiry,
            taker_pk_fe,
        })
    }

    /// The maker's authority PDA, the owner of the quote UTXO.
    pub fn maker_authority(&self) -> Result<ProgramOwner> {
        Ok(maker_authority(&self.maker.solana_address().map_err(err)?))
    }

    /// The quote UTXO's state, which the SDK hashes and encodes.
    pub fn state(&self) -> Result<circuits::QuoteTerms> {
        circuits::QuoteTerms::from_client(self.circuit_terms()?, &mut InputRecords::new())
            .map_err(err)
    }

    /// The terms of a decoded quote UTXO state whose maker is `maker`. Fails
    /// if the state names another maker or holds terms the circuits refuse.
    pub fn from_state(state: &circuits::QuoteTerms, maker: ShieldedAddress) -> Result<Self> {
        check_trader_identity(&state.taker_pk_fe)?;
        let terms = Self {
            feed_id: feed_id(state.feed_hi, state.feed_lo),
            trader_is_long: state.trader_is_long,
            size: state.size,
            entry_price: state.entry_price,
            trader_margin: state.trader_margin,
            maintenance_margin: state.maintenance_margin,
            maker,
            expiry: state.expiry,
            trader: Some(state.taker_pk_fe).filter(|id| *id != [0; 32]),
        };
        ensure!(terms.state()? == *state, "the quote names another maker");
        Ok(terms)
    }

    /// The quote UTXO's encrypted `utxo_data`, the state's Borsh encoding,
    /// and its data hash, `Poseidon(DATA, state hash)`, from one state.
    fn utxo_data(&self) -> Result<(Vec<u8>, [u8; 32])> {
        let state = self.state()?;
        let terms_hash = DataHash::hash(&state).map_err(err)?;
        let data_hash =
            spr_program::hasher::data_hash(&spr_program::to_bytes(&terms_hash)).map_err(err)?;
        Ok((state.serialize_for_encryption().map_err(err)?, data_hash))
    }

    /// The terms of the position that opens when `trader` accepts the quote.
    pub fn position_terms(&self, trader: ShieldedAddress) -> PositionTerms {
        PositionTerms {
            feed_id: self.feed_id,
            trader_is_long: self.trader_is_long,
            size: self.size,
            entry_price: self.entry_price,
            trader_margin: self.trader_margin,
            maintenance_margin: self.maintenance_margin,
            maker: self.maker,
            trader,
        }
    }
}

/// The quote UTXO: the maker's margin in the collateral mint, owned by the
/// maker's authority PDA under the zero nullifier key, with the terms as its
/// state. Its opening (terms and blinding) is the complete spend capability;
/// the program signs for the PDA only through `open` and `cancel_quote`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuoteUtxo {
    pub terms: QuoteTerms,
    /// Derived by the quote transaction from its first nullifier, blinding
    /// seed and output slot.
    pub blinding: [u8; 32],
    pub collateral_mint: Mint,
    pub maker_margin: u64,
}

impl QuoteUtxo {
    /// The quote of a decoded quote UTXO whose maker is `maker`. The SDK's
    /// decode ([`DataUtxo::try_from`]) has checked the state against the
    /// UTXO's commitment.
    pub fn from_data_utxo(
        quote: DataUtxo<circuits::QuoteTerms>,
        maker: ShieldedAddress,
    ) -> Result<Self> {
        Ok(Self {
            terms: QuoteTerms::from_state(&quote.data, maker)?,
            blinding: quote.utxo.utxo.blinding,
            collateral_mint: quote.utxo.utxo.asset,
            maker_margin: quote.utxo.utxo.amount,
        })
    }

    /// The quote output. The owner's viewing key does not enter the
    /// commitment; quote encrypts the output to the trader's.
    pub fn output_utxo(&self, viewing_pubkey: X25519Pubkey) -> Result<SppProofOutputUtxo> {
        let (payload, data_hash) = self.terms.utxo_data()?;
        Ok(SppProofOutputUtxo {
            asset: self.collateral_mint,
            amount: self.maker_margin,
            blinding: self.blinding,
            owner_address: Some(
                self.terms
                    .maker_authority()?
                    .address(viewing_pubkey)
                    .map_err(err)?,
            ),
            ..Default::default()
        }
        .with_utxo_data(payload, data_hash))
    }

    /// The quote UTXO's commitment in tree `tree_id`.
    pub fn hash(&self, tree_id: u16) -> Result<[u8; 32]> {
        self.output_utxo(self.terms.maker.viewing_pubkey)?
            .hash(tree_id)
            .map_err(err)
    }

    /// The quote UTXO as an SPP input at `leaf_index` of tree `tree_id`.
    /// Anyone holding the opening can build it; spending needs the PDA
    /// signature only the program gives.
    pub fn to_input_utxo(&self, tree_id: u16, leaf_index: u64) -> Result<WalletUtxo> {
        self.terms
            .maker_authority()?
            .input(
                &self.output_utxo(self.terms.maker.viewing_pubkey)?,
                tree_id,
                leaf_index,
            )
            .map_err(err)
    }

    /// The position `trader` opened with the open transaction whose first
    /// nullifier is `first_nullifier`, from the quote opening alone: the open
    /// circuit fixes its blinding seed to [`open_blinding_seed`], and the
    /// position is output slot [`POSITION_SLOT`]. No ciphertext is needed.
    pub fn derived_position(
        &self,
        trader: ShieldedAddress,
        first_nullifier: &[u8; 32],
    ) -> Result<PositionUtxo> {
        let blinding_seed = open_blinding_seed(&self.blinding)?;
        let seed = spr_program::derive_output_blinding_seed(first_nullifier, &blinding_seed)
            .map_err(err)?;
        let blinding =
            spr_program::derive_transact_output_blinding(first_nullifier, &seed, POSITION_SLOT)
                .map_err(err)?;
        let total = self
            .maker_margin
            .checked_add(self.terms.trader_margin)
            .ok_or_else(|| anyhow::anyhow!("the two margins overflow"))?;
        Ok(PositionUtxo {
            terms: self.terms.position_terms(trader),
            blinding,
            collateral_mint: self.collateral_mint,
            total,
        })
    }
}

#[cfg(test)]
mod tests {
    use solana_address::Address;
    use solana_keypair::Keypair;
    use spr_keypair::ShieldedKeypair;

    use super::*;

    fn address(seed: u8) -> ShieldedAddress {
        ShieldedKeypair::from_keypair(&Keypair::new_from_array([seed; 32]))
            .unwrap()
            .shielded_address()
            .unwrap()
    }

    fn blinding(byte: u8) -> [u8; 32] {
        let mut blinding = [byte; 32];
        blinding[0] = 0;
        blinding
    }

    fn terms(trader: Option<[u8; 32]>) -> QuoteTerms {
        QuoteTerms {
            feed_id: core::array::from_fn(|i| 0xf0 ^ i as u8),
            trader_is_long: true,
            size: 10_000_000,
            entry_price: 10_000_000_000,
            trader_margin: 20_000_000,
            maintenance_margin: 5_000_000,
            maker: address(21),
            expiry: 1_700_000_000,
            trader,
        }
    }

    fn quote(terms: QuoteTerms) -> QuoteUtxo {
        QuoteUtxo {
            terms,
            blinding: blinding(7),
            collateral_mint: Mint::new(Address::new_from_array([4; 32]), 2),
            maker_margin: 30_000_000,
        }
    }

    /// A real trader identity, `solana_owner_identity` of a Solana address.
    fn real_identity() -> [u8; 32] {
        crate::shared::identity(&Address::new_from_array([13u8; 32])).unwrap()
    }

    #[test]
    fn quote_utxo_decodes_designated_and_open_terms() {
        for terms in [terms(Some(real_identity())), terms(None)] {
            let quote = quote(terms);
            let input = quote.to_input_utxo(0, 3).unwrap();
            assert_eq!(
                QuoteUtxo::from_data_utxo(DataUtxo::try_from(input).unwrap(), quote.terms.maker)
                    .unwrap(),
                quote
            );
        }
    }

    #[test]
    fn quote_of_another_maker_is_refused() {
        let quote = quote(terms(None));
        let decoded = DataUtxo::try_from(quote.to_input_utxo(0, 3).unwrap()).unwrap();
        assert!(QuoteUtxo::from_data_utxo(decoded, address(22)).is_err());
    }

    #[test]
    fn quote_utxo_with_another_payload_is_refused() {
        let quote = quote(terms(None));
        let mut input = quote.to_input_utxo(0, 3).unwrap();
        let mut other = quote.terms.clone();
        other.entry_price += 1;
        input.utxo.data = SppProofOutputUtxo::default()
            .with_utxo_data(other.utxo_data().unwrap().0, [0; 32])
            .data;
        assert!(DataUtxo::<circuits::QuoteTerms>::try_from(input).is_err());
    }

    #[test]
    fn terms_the_circuits_refuse_do_not_build() {
        let good = terms(None);
        for bad in [
            QuoteTerms {
                size: 0,
                ..good.clone()
            },
            QuoteTerms {
                entry_price: 0,
                ..good.clone()
            },
            QuoteTerms {
                trader_margin: 0,
                ..good.clone()
            },
            QuoteTerms {
                maintenance_margin: good.trader_margin,
                ..good.clone()
            },
            // Not a canonical field element.
            QuoteTerms {
                trader: Some([0xff; 32]),
                ..good.clone()
            },
            QuoteTerms {
                trader: Some(spr_hasher::primitives::BN254_SCALAR_MODULUS_BE),
                ..good.clone()
            },
        ] {
            assert!(bad.circuit_terms().is_err());
            assert!(bad.state().is_err());
        }
        assert!(good.circuit_terms().is_ok());
        assert_eq!(
            terms(Some(real_identity()))
                .circuit_terms()
                .unwrap()
                .taker_pk_fe,
            real_identity()
        );
    }

    #[test]
    fn the_derived_position_holds_both_margins_under_the_quote_terms() {
        let quote = quote(terms(None));
        let trader = address(31);
        let position = quote.derived_position(trader, &blinding(9)).unwrap();
        assert_eq!(position.total, 50_000_000);
        assert_eq!(position.maker_margin(), 30_000_000);
        assert_eq!(position.terms, quote.terms.position_terms(trader));
        assert_ne!(position.blinding, quote.blinding);
        // The position decodes from its own UTXO.
        let decoded = DataUtxo::try_from(position.to_input_utxo(0, 4).unwrap()).unwrap();
        assert_eq!(
            PositionUtxo::from_data_utxo(decoded, quote.terms.maker, trader).unwrap(),
            position
        );
        // Not under another trader or maker.
        let decoded = DataUtxo::try_from(position.to_input_utxo(0, 4).unwrap()).unwrap();
        assert!(PositionUtxo::from_data_utxo(decoded, quote.terms.maker, address(32)).is_err());
    }
}
