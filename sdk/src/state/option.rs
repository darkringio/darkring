//! The state of a dated contract: an option or a prediction. The quote a
//! maker offers ([`OptionQuoteUtxo`]) and the contract a trader opens by
//! accepting it ([`OptionUtxo`]), each a UTXO owned by the maker's authority
//! PDA, as a perp's quote and position are.

use anyhow::{ensure, Result};
use perps_program::circuits::{
    self,
    option_payout::{is_matured, is_prediction, may_settle, option_payout, KIND_MAX},
    OptionQuoteTermsClient, OptionTermsClient,
};
use spr_keypair::{ShieldedAddress, X25519Pubkey};
use spr_program::{
    circuit::DataHash, ClientInputs, DataUtxo, EncryptionCodec, InputRecords, ProgramOwner,
};
use spr_transaction::{Mint, SppProofOutputUtxo, WalletUtxo};

pub use perps_program::circuits::option_payout::{KIND_ABOVE, KIND_BELOW, KIND_CALL, KIND_PUT};

use super::quote::check_trader_identity;
use crate::{
    err, maker_authority,
    shared::{feed_halves, feed_id, option_open_blinding_seed, option_settle_blinding_seed},
};

/// Output slot of the option UTXO in the option open transaction.
pub const OPTION_SLOT: u32 = 0;
/// Output slot of the trader's payout in the option settle transaction.
pub const OPTION_TRADER_PAYOUT_SLOT: u32 = 0;
/// Output slot of the maker's payout in the option settle transaction.
pub const OPTION_MAKER_PAYOUT_SLOT: u32 = 1;

/// The terms of an option quote: the contract a maker offers, until when it
/// can be accepted and by whom.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OptionQuoteTerms {
    /// The oracle price feed id.
    pub feed_id: [u8; 32],
    /// [`KIND_CALL`], [`KIND_PUT`], [`KIND_ABOVE`] or [`KIND_BELOW`].
    pub kind: u8,
    /// The size of a call or a put, scaled by `SIZE_SCALE`: it pays
    /// `size * |price - strike| / SIZE_SCALE` on the right side of the
    /// strike. A prediction does not read it.
    pub size: u64,
    /// The strike, in the feed's price ticks.
    pub strike: u64,
    /// What the trader pays to open the contract.
    pub premium: u64,
    /// Unix seconds; the end of the contract's term.
    pub term_end: u64,
    /// The maker's shielded address: receives its share at settle and the
    /// collateral back on cancel; its Solana key signs the cancel.
    pub maker: ShieldedAddress,
    /// Unix seconds; until when the quote can be accepted.
    pub expiry: u64,
    /// The designated trader's owner identity, or `None` for a quote anyone
    /// holding the opening may accept.
    pub trader: Option<[u8; 32]>,
}

impl OptionQuoteTerms {
    /// The terms as the circuits take them. Fails on terms the circuits
    /// refuse.
    pub fn circuit_terms(&self) -> Result<OptionQuoteTermsClient> {
        ensure!(self.kind <= KIND_MAX, "no such kind of contract");
        ensure!(self.strike != 0, "the strike is zero");
        ensure!(self.premium != 0, "the premium is zero");
        ensure!(self.term_end != 0, "the contract has no end");
        ensure!(
            is_prediction(self.kind) || self.size != 0,
            "the size is zero"
        );
        let taker_pk_fe = self.trader.unwrap_or_default();
        check_trader_identity(&taker_pk_fe)?;
        let (feed_hi, feed_lo) = feed_halves(&self.feed_id);
        Ok(OptionQuoteTermsClient {
            feed_hi,
            feed_lo,
            kind: self.kind,
            size: self.size,
            strike: self.strike,
            premium: self.premium,
            term_end: self.term_end,
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
    pub fn state(&self) -> Result<circuits::OptionQuoteTerms> {
        circuits::OptionQuoteTerms::from_client(self.circuit_terms()?, &mut InputRecords::new())
            .map_err(err)
    }

    /// The terms of a decoded quote UTXO state whose maker is `maker`. Fails
    /// if the state names another maker or holds terms the circuits refuse.
    pub fn from_state(state: &circuits::OptionQuoteTerms, maker: ShieldedAddress) -> Result<Self> {
        check_trader_identity(&state.taker_pk_fe)?;
        let terms = Self {
            feed_id: feed_id(state.feed_hi, state.feed_lo),
            kind: state.kind,
            size: state.size,
            strike: state.strike,
            premium: state.premium,
            term_end: state.term_end,
            maker,
            expiry: state.expiry,
            trader: Some(state.taker_pk_fe).filter(|id| *id != [0; 32]),
        };
        ensure!(terms.state()? == *state, "the quote names another maker");
        Ok(terms)
    }

    fn utxo_data(&self) -> Result<(Vec<u8>, [u8; 32])> {
        let state = self.state()?;
        let terms_hash = DataHash::hash(&state).map_err(err)?;
        let data_hash =
            spr_program::hasher::data_hash(&spr_program::to_bytes(&terms_hash)).map_err(err)?;
        Ok((state.serialize_for_encryption().map_err(err)?, data_hash))
    }

    /// The terms of the contract that opens when `trader` accepts the quote.
    pub fn option_terms(&self, trader: ShieldedAddress) -> OptionTerms {
        OptionTerms {
            feed_id: self.feed_id,
            kind: self.kind,
            size: self.size,
            strike: self.strike,
            premium: self.premium,
            term_end: self.term_end,
            maker: self.maker,
            trader,
        }
    }
}

/// The option quote UTXO: the maker's collateral in the collateral mint,
/// owned by the maker's authority PDA under the zero nullifier key, with the
/// terms as its state. Its opening is the complete spend capability; the
/// program signs for the PDA only through `option_open` and `option_cancel`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OptionQuoteUtxo {
    pub terms: OptionQuoteTerms,
    /// Derived by the quote transaction from its first nullifier, blinding
    /// seed and output slot.
    pub blinding: [u8; 32],
    pub collateral_mint: Mint,
    /// What the maker locked: with the premium, the most the contract pays.
    pub collateral: u64,
}

impl OptionQuoteUtxo {
    /// The quote of a decoded quote UTXO whose maker is `maker`.
    pub fn from_data_utxo(
        quote: DataUtxo<circuits::OptionQuoteTerms>,
        maker: ShieldedAddress,
    ) -> Result<Self> {
        Ok(Self {
            terms: OptionQuoteTerms::from_state(&quote.data, maker)?,
            blinding: quote.utxo.utxo.blinding,
            collateral_mint: quote.utxo.utxo.asset,
            collateral: quote.utxo.utxo.amount,
        })
    }

    /// The quote output. The owner's viewing key does not enter the
    /// commitment; the quote encrypts the output to the trader's.
    pub fn output_utxo(&self, viewing_pubkey: X25519Pubkey) -> Result<SppProofOutputUtxo> {
        let (payload, data_hash) = self.terms.utxo_data()?;
        Ok(SppProofOutputUtxo {
            asset: self.collateral_mint,
            amount: self.collateral,
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

    /// The most the contract can pay: the collateral and the premium.
    pub fn total(&self) -> Result<u64> {
        self.collateral
            .checked_add(self.terms.premium)
            .ok_or_else(|| anyhow::anyhow!("the collateral and the premium overflow"))
    }

    /// The contract `trader` opened with the open transaction whose first
    /// nullifier is `first_nullifier`, from the quote opening alone: the open
    /// circuit fixes its blinding seed to [`option_open_blinding_seed`], and
    /// the option is output slot [`OPTION_SLOT`]. No ciphertext is needed.
    pub fn derived_option(
        &self,
        trader: ShieldedAddress,
        first_nullifier: &[u8; 32],
    ) -> Result<OptionUtxo> {
        let blinding_seed = option_open_blinding_seed(&self.blinding)?;
        let seed = spr_program::derive_output_blinding_seed(first_nullifier, &blinding_seed)
            .map_err(err)?;
        let blinding =
            spr_program::derive_transact_output_blinding(first_nullifier, &seed, OPTION_SLOT)
                .map_err(err)?;
        Ok(OptionUtxo {
            terms: self.terms.option_terms(trader),
            blinding,
            collateral_mint: self.collateral_mint,
            total: self.total()?,
        })
    }
}

/// The terms of an open contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OptionTerms {
    pub feed_id: [u8; 32],
    pub kind: u8,
    pub size: u64,
    pub strike: u64,
    pub premium: u64,
    pub term_end: u64,
    /// The maker's shielded address. The option UTXO commits its owner; the
    /// viewing key only encrypts its payout.
    pub maker: ShieldedAddress,
    /// The trader's shielded address, likewise.
    pub trader: ShieldedAddress,
}

impl OptionTerms {
    /// The terms as the circuits take them.
    pub fn circuit_terms(&self) -> Result<OptionTermsClient> {
        let (feed_hi, feed_lo) = feed_halves(&self.feed_id);
        let mut records = InputRecords::new();
        Ok(OptionTermsClient {
            feed_hi,
            feed_lo,
            kind: self.kind,
            size: self.size,
            strike: self.strike,
            premium: self.premium,
            term_end: self.term_end,
            maker: *records.address(&self.maker).map_err(err)?.owner(),
            trader: *records.address(&self.trader).map_err(err)?.owner(),
        })
    }

    /// The maker's authority PDA, the owner of the option UTXO.
    pub fn maker_authority(&self) -> Result<ProgramOwner> {
        Ok(maker_authority(&self.maker.solana_address().map_err(err)?))
    }

    /// The option UTXO's state, which the SDK hashes and encodes.
    pub fn state(&self) -> Result<circuits::OptionTerms> {
        circuits::OptionTerms::from_client(self.circuit_terms()?, &mut InputRecords::new())
            .map_err(err)
    }

    /// The terms of a decoded option UTXO state between `maker` and
    /// `trader`. Fails if the state names other parties.
    pub fn from_state(
        state: &circuits::OptionTerms,
        maker: ShieldedAddress,
        trader: ShieldedAddress,
    ) -> Result<Self> {
        let terms = Self {
            feed_id: feed_id(state.feed_hi, state.feed_lo),
            kind: state.kind,
            size: state.size,
            strike: state.strike,
            premium: state.premium,
            term_end: state.term_end,
            maker,
            trader,
        };
        ensure!(
            terms.state()? == *state,
            "the contract names another maker or trader"
        );
        Ok(terms)
    }

    fn utxo_data(&self) -> Result<(Vec<u8>, [u8; 32])> {
        let state = self.state()?;
        let terms_hash = DataHash::hash(&state).map_err(err)?;
        let data_hash =
            spr_program::hasher::data_hash(&spr_program::to_bytes(&terms_hash)).map_err(err)?;
        Ok((state.serialize_for_encryption().map_err(err)?, data_hash))
    }
}

/// The option UTXO: the premium and the maker's collateral in the collateral
/// mint, owned by the maker's authority PDA under the zero nullifier key,
/// with the terms as its state. The program signs for the PDA only through
/// `option_settle`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OptionUtxo {
    pub terms: OptionTerms,
    /// Derived by the open transaction from its first nullifier, the quote's
    /// blinding and the output slot.
    pub blinding: [u8; 32],
    pub collateral_mint: Mint,
    /// The premium and the collateral: the most the contract pays.
    pub total: u64,
}

impl OptionUtxo {
    /// The contract of a decoded option UTXO between `maker` and `trader`.
    pub fn from_data_utxo(
        option: DataUtxo<circuits::OptionTerms>,
        maker: ShieldedAddress,
        trader: ShieldedAddress,
    ) -> Result<Self> {
        Ok(Self {
            terms: OptionTerms::from_state(&option.data, maker, trader)?,
            blinding: option.utxo.utxo.blinding,
            collateral_mint: option.utxo.utxo.asset,
            total: option.utxo.utxo.amount,
        })
    }

    /// The option output, encrypted to the maker's viewing key.
    pub fn output_utxo(&self) -> Result<SppProofOutputUtxo> {
        let (payload, data_hash) = self.terms.utxo_data()?;
        Ok(SppProofOutputUtxo {
            asset: self.collateral_mint,
            amount: self.total,
            blinding: self.blinding,
            owner_address: Some(
                self.terms
                    .maker_authority()?
                    .address(self.terms.maker.viewing_pubkey)
                    .map_err(err)?,
            ),
            ..Default::default()
        }
        .with_utxo_data(payload, data_hash))
    }

    /// The option UTXO's commitment in tree `tree_id`.
    pub fn hash(&self, tree_id: u16) -> Result<[u8; 32]> {
        self.output_utxo()?.hash(tree_id).map_err(err)
    }

    /// The option UTXO as an SPP input at `leaf_index` of tree `tree_id`.
    pub fn to_input_utxo(&self, tree_id: u16, leaf_index: u64) -> Result<WalletUtxo> {
        self.terms
            .maker_authority()?
            .input(&self.output_utxo()?, tree_id, leaf_index)
            .map_err(err)
    }

    /// The trader's share of the contract at `price`; the maker receives the
    /// rest. The same function the settle circuit runs.
    pub fn trader_payout(&self, price: u64) -> u64 {
        option_payout(
            self.terms.kind,
            self.terms.size,
            self.terms.strike,
            self.total,
            price,
        )
    }

    /// The maker's share of the contract at `price`.
    pub fn maker_payout(&self, price: u64) -> u64 {
        self.total.saturating_sub(self.trader_payout(price))
    }

    /// Whether the contract has matured at an oracle price published at
    /// `publish_time`: from then on either party may settle it.
    pub fn is_matured(&self, publish_time: u64) -> bool {
        is_matured(publish_time, self.terms.term_end)
    }

    /// Whether the trader (`by_trader`) or else the maker may settle at an
    /// oracle price published at `publish_time`.
    pub fn may_settle(&self, by_trader: bool, publish_time: u64) -> bool {
        may_settle(
            self.terms.kind,
            by_trader,
            !by_trader,
            self.is_matured(publish_time),
        )
    }

    /// Both payouts of the settle at `price` whose first nullifier is
    /// `first_nullifier`, trader's then maker's, from the option opening
    /// alone: the settle circuit fixes its blinding seed to
    /// [`option_settle_blinding_seed`].
    pub fn derived_payouts(
        &self,
        first_nullifier: &[u8; 32],
        price: u64,
    ) -> Result<(SppProofOutputUtxo, SppProofOutputUtxo)> {
        let blinding_seed = option_settle_blinding_seed(&self.blinding)?;
        let seed = spr_program::derive_output_blinding_seed(first_nullifier, &blinding_seed)
            .map_err(err)?;
        let output = |owner: ShieldedAddress, amount: u64, slot: u32| -> Result<_> {
            Ok(SppProofOutputUtxo {
                asset: self.collateral_mint,
                amount,
                blinding: spr_program::derive_transact_output_blinding(
                    first_nullifier,
                    &seed,
                    slot,
                )
                .map_err(err)?,
                owner_address: Some(owner),
                ..Default::default()
            })
        };
        Ok((
            output(
                self.terms.trader,
                self.trader_payout(price),
                OPTION_TRADER_PAYOUT_SLOT,
            )?,
            output(
                self.terms.maker,
                self.maker_payout(price),
                OPTION_MAKER_PAYOUT_SLOT,
            )?,
        ))
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

    fn terms(kind: u8) -> OptionQuoteTerms {
        OptionQuoteTerms {
            feed_id: core::array::from_fn(|i| 0xf0 ^ i as u8),
            kind,
            size: if is_prediction(kind) { 0 } else { 10_000_000 },
            strike: 10_000_000_000,
            premium: 4_000_000,
            term_end: 1_700_003_600,
            maker: address(21),
            expiry: 1_700_000_000,
            trader: None,
        }
    }

    fn quote(terms: OptionQuoteTerms) -> OptionQuoteUtxo {
        OptionQuoteUtxo {
            terms,
            blinding: blinding(7),
            collateral_mint: Mint::new(Address::new_from_array([4; 32]), 2),
            collateral: 26_000_000,
        }
    }

    #[test]
    fn a_quote_of_every_kind_decodes_from_its_utxo() {
        for kind in 0..=KIND_MAX {
            let quote = quote(terms(kind));
            let input = quote.to_input_utxo(0, 3).unwrap();
            assert_eq!(
                OptionQuoteUtxo::from_data_utxo(
                    DataUtxo::try_from(input).unwrap(),
                    quote.terms.maker
                )
                .unwrap(),
                quote
            );
        }
    }

    #[test]
    fn an_option_quote_does_not_decode_as_a_perp_quote() {
        let quote = quote(terms(KIND_CALL));
        let input = quote.to_input_utxo(0, 3).unwrap();
        assert!(DataUtxo::<circuits::QuoteTerms>::try_from(input.clone()).is_err());
        assert!(DataUtxo::<circuits::OptionTerms>::try_from(input).is_err());
    }

    #[test]
    fn terms_the_circuits_refuse_do_not_build() {
        let good = terms(KIND_CALL);
        for bad in [
            OptionQuoteTerms {
                kind: KIND_MAX + 1,
                ..good.clone()
            },
            OptionQuoteTerms {
                strike: 0,
                ..good.clone()
            },
            OptionQuoteTerms {
                premium: 0,
                ..good.clone()
            },
            OptionQuoteTerms {
                term_end: 0,
                ..good.clone()
            },
            OptionQuoteTerms {
                size: 0,
                ..good.clone()
            },
            OptionQuoteTerms {
                trader: Some([0xff; 32]),
                ..good.clone()
            },
        ] {
            assert!(bad.circuit_terms().is_err());
        }
        assert!(good.circuit_terms().is_ok());
        // A prediction needs no size.
        assert!(terms(KIND_ABOVE).circuit_terms().is_ok());
    }

    #[test]
    fn the_derived_option_holds_the_collateral_and_the_premium() {
        let quote = quote(terms(KIND_PUT));
        let trader = address(31);
        let option = quote.derived_option(trader, &blinding(9)).unwrap();
        assert_eq!(option.total, 30_000_000);
        assert_eq!(option.terms, quote.terms.option_terms(trader));
        let decoded = DataUtxo::try_from(option.to_input_utxo(0, 4).unwrap()).unwrap();
        assert_eq!(
            OptionUtxo::from_data_utxo(decoded, quote.terms.maker, trader).unwrap(),
            option
        );
        let decoded = DataUtxo::try_from(option.to_input_utxo(0, 4).unwrap()).unwrap();
        assert!(OptionUtxo::from_data_utxo(decoded, quote.terms.maker, address(32)).is_err());
        // A put of 1 SOL struck at 100 pays 5 at 95 and is capped at 30.
        assert_eq!(option.trader_payout(9_500_000_000), 5_000_000);
        assert_eq!(option.maker_payout(9_500_000_000), 25_000_000);
        assert_eq!(option.trader_payout(1), 30_000_000);
        // The trader may exercise early; the maker only at the end.
        assert!(option.may_settle(true, 0));
        assert!(!option.may_settle(false, 1_700_003_599));
        assert!(option.may_settle(false, 1_700_003_600));
    }
}
