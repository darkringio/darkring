use anyhow::{ensure, Result};
use perps_program::circuits::{
    self,
    payout::{is_liquidatable, trader_payout},
    PositionTermsClient,
};
use spr_keypair::ShieldedAddress;
use spr_program::{
    circuit::DataHash, ClientInputs, DataUtxo, EncryptionCodec, InputRecords, ProgramOwner,
};
use spr_transaction::{Mint, SppProofOutputUtxo, WalletUtxo};

use crate::{
    err, maker_authority,
    shared::{feed_halves, feed_id, settle_blinding_seed},
};

/// Output slot of the trader's payout in the settle transaction.
pub const TRADER_PAYOUT_SLOT: u32 = 0;
/// Output slot of the maker's payout in the settle transaction.
pub const MAKER_PAYOUT_SLOT: u32 = 1;

/// The terms of an open position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PositionTerms {
    pub feed_id: [u8; 32],
    pub trader_is_long: bool,
    pub size: u64,
    pub entry_price: u64,
    pub trader_margin: u64,
    pub maintenance_margin: u64,
    /// The maker's shielded address. The position UTXO commits its owner;
    /// the viewing key only encrypts its payout.
    pub maker: ShieldedAddress,
    /// The trader's shielded address, likewise.
    pub trader: ShieldedAddress,
}

impl PositionTerms {
    /// The terms as the circuits take them.
    pub fn circuit_terms(&self) -> Result<PositionTermsClient> {
        let (feed_hi, feed_lo) = feed_halves(&self.feed_id);
        let mut records = InputRecords::new();
        Ok(PositionTermsClient {
            feed_hi,
            feed_lo,
            trader_is_long: self.trader_is_long,
            size: self.size,
            entry_price: self.entry_price,
            trader_margin: self.trader_margin,
            maintenance_margin: self.maintenance_margin,
            maker: *records.address(&self.maker).map_err(err)?.owner(),
            trader: *records.address(&self.trader).map_err(err)?.owner(),
        })
    }

    /// The maker's authority PDA, the owner of the position UTXO.
    pub fn maker_authority(&self) -> Result<ProgramOwner> {
        Ok(maker_authority(&self.maker.solana_address().map_err(err)?))
    }

    /// The position UTXO's state, which the SDK hashes and encodes.
    pub fn state(&self) -> Result<circuits::PositionTerms> {
        circuits::PositionTerms::from_client(self.circuit_terms()?, &mut InputRecords::new())
            .map_err(err)
    }

    /// The terms of a decoded position UTXO state between `maker` and
    /// `trader`. Fails if the state names other parties.
    pub fn from_state(
        state: &circuits::PositionTerms,
        maker: ShieldedAddress,
        trader: ShieldedAddress,
    ) -> Result<Self> {
        let terms = Self {
            feed_id: feed_id(state.feed_hi, state.feed_lo),
            trader_is_long: state.trader_is_long,
            size: state.size,
            entry_price: state.entry_price,
            trader_margin: state.trader_margin,
            maintenance_margin: state.maintenance_margin,
            maker,
            trader,
        };
        ensure!(
            terms.state()? == *state,
            "the position names another maker or trader"
        );
        Ok(terms)
    }

    /// The position UTXO's encrypted `utxo_data`, the state's Borsh encoding,
    /// and its data hash, `Poseidon(DATA, state hash)`, from one state.
    fn utxo_data(&self) -> Result<(Vec<u8>, [u8; 32])> {
        let state = self.state()?;
        let terms_hash = DataHash::hash(&state).map_err(err)?;
        let data_hash =
            spr_program::hasher::data_hash(&spr_program::to_bytes(&terms_hash)).map_err(err)?;
        Ok((state.serialize_for_encryption().map_err(err)?, data_hash))
    }
}

/// The position UTXO: both margins in the collateral mint, owned by the
/// maker's authority PDA under the zero nullifier key, with the terms as its
/// state. Its opening (terms and blinding) is the complete spend capability;
/// the program signs for the PDA only through `settle`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PositionUtxo {
    pub terms: PositionTerms,
    /// Derived by the open transaction from its first nullifier, the quote's
    /// blinding and the output slot.
    pub blinding: [u8; 32],
    pub collateral_mint: Mint,
    /// Both margins: the trader's `terms.trader_margin` and the maker's.
    pub total: u64,
}

impl PositionUtxo {
    /// The position of a decoded position UTXO between `maker` and `trader`.
    /// The SDK's decode ([`DataUtxo::try_from`]) has checked the state
    /// against the UTXO's commitment.
    pub fn from_data_utxo(
        position: DataUtxo<circuits::PositionTerms>,
        maker: ShieldedAddress,
        trader: ShieldedAddress,
    ) -> Result<Self> {
        Ok(Self {
            terms: PositionTerms::from_state(&position.data, maker, trader)?,
            blinding: position.utxo.utxo.blinding,
            collateral_mint: position.utxo.utxo.asset,
            total: position.utxo.utxo.amount,
        })
    }

    /// The maker's margin, the position's amount less the trader's margin.
    pub fn maker_margin(&self) -> u64 {
        self.total.saturating_sub(self.terms.trader_margin)
    }

    /// The position output, encrypted to the maker's viewing key.
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

    /// The position UTXO's commitment in tree `tree_id`.
    pub fn hash(&self, tree_id: u16) -> Result<[u8; 32]> {
        self.output_utxo()?.hash(tree_id).map_err(err)
    }

    /// The position UTXO as an SPP input at `leaf_index` of tree `tree_id`.
    pub fn to_input_utxo(&self, tree_id: u16, leaf_index: u64) -> Result<WalletUtxo> {
        self.terms
            .maker_authority()?
            .input(&self.output_utxo()?, tree_id, leaf_index)
            .map_err(err)
    }

    /// The trader's share of the position at `price`; the maker receives the
    /// rest. The same function the settle circuit runs.
    pub fn trader_payout(&self, price: u64) -> u64 {
        trader_payout(
            self.terms.trader_is_long,
            self.terms.size,
            self.terms.entry_price,
            self.terms.trader_margin,
            self.total,
            price,
        )
    }

    /// The maker's share of the position at `price`.
    pub fn maker_payout(&self, price: u64) -> u64 {
        self.total.saturating_sub(self.trader_payout(price))
    }

    /// Whether the maker may settle at `price`: the trader's equity is at or
    /// below the maintenance margin.
    pub fn is_liquidatable(&self, price: u64) -> bool {
        is_liquidatable(self.trader_payout(price), self.terms.maintenance_margin)
    }

    /// Both payouts of the settle at `price` whose first nullifier is
    /// `first_nullifier`, trader's then maker's, from the position opening
    /// alone: the settle circuit fixes its blinding seed to
    /// [`settle_blinding_seed`]. The party that did not settle recovers its
    /// payout with these, whatever ciphertext the settler published.
    pub fn derived_payouts(
        &self,
        first_nullifier: &[u8; 32],
        price: u64,
    ) -> Result<(SppProofOutputUtxo, SppProofOutputUtxo)> {
        let blinding_seed = settle_blinding_seed(&self.blinding)?;
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
                TRADER_PAYOUT_SLOT,
            )?,
            output(
                self.terms.maker,
                self.maker_payout(price),
                MAKER_PAYOUT_SLOT,
            )?,
        ))
    }
}
