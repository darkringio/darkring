use anyhow::{anyhow, ensure, Result};
use perps_program::circuits::{SettleClient, SettlePrivateInputsClient, SettlePublicInputsClient};
use solana_address::Address;
use spr_client::SprIndexer;
use spr_keypair::ShieldedKeypair;
use spr_program::{ProgramOwner, ProgramProofInputs, TransactionOptions};
use spr_transaction::instructions::transact::SppProofInputs;

use super::Settle;
use crate::{
    err,
    proof::{self, CircuitId},
    shared::{feed_halves, field, identity, prove_transact, settle_blinding_seed},
    state::PositionUtxo,
};

/// Output slot of the trader's payout in the settle transaction.
pub const TRADER_PAYOUT_SLOT: usize = crate::state::TRADER_PAYOUT_SLOT as usize;
/// Output slot of the maker's payout in the settle transaction.
pub const MAKER_PAYOUT_SLOT: usize = crate::state::MAKER_PAYOUT_SLOT as usize;

pub struct SettleTransactionParams {
    pub position_utxo: PositionUtxo,
    /// The position UTXO's leaf index in the output tree.
    pub position_leaf_index: u64,
    /// The oracle price update account to settle against.
    pub oracle: Address,
    /// The price that account holds, in the feed's price ticks. The program
    /// reads the account itself; a proof for another price fails on chain.
    pub price: u64,
    pub payer: Address,
    /// The output tree, which also holds the position UTXO.
    pub tree_id: u16,
}

pub struct SettleTransaction {
    /// The position's maker, whose authority PDA the program signs for.
    pub maker: Address,
    /// The settling party's Solana address.
    pub signer: Address,
    pub oracle: Address,
    pub spp_proof_inputs: SppProofInputs,
    pub proof_inputs: ProgramProofInputs,
    /// The position UTXO's nullifier, the transaction's first nullifier.
    pub first_nullifier: [u8; 32],
    /// What the trader and the maker receive.
    pub trader_payout: u64,
    pub maker_payout: u64,
}

impl SettleTransactionParams {
    /// 1. Check the settler may settle at this price: the trader always, the
    ///    maker only on a liquidatable position.
    /// 2. Build the program transaction under the position's settle blinding
    ///    seed.
    /// 3. Check both parties can derive their payouts from the position
    ///    opening.
    /// 4. Encrypt the outputs.
    pub fn build(self, settler: &ShieldedKeypair) -> Result<SettleTransaction> {
        // 1. Check the settler may settle.
        let sender = settler.shielded_address().map_err(err)?;
        let terms = &self.position_utxo.terms;
        let signer = sender.solana_address().map_err(err)?;
        if sender == terms.maker {
            ensure!(
                self.position_utxo.is_liquidatable(self.price),
                "the maker can settle only a liquidatable position"
            );
        } else {
            ensure!(
                sender == terms.trader,
                "only the position's trader or maker can settle it"
            );
        }
        let maker = terms.maker.solana_address().map_err(err)?;
        let position_input = self
            .position_utxo
            .to_input_utxo(self.tree_id, self.position_leaf_index)?;
        let first_nullifier = position_input.nullifier;
        let (feed_hi, feed_lo) = feed_halves(&terms.feed_id);

        // 2. Build the program transaction.
        let transaction = SettleClient {
            private: SettlePrivateInputsClient {
                tx_context: TransactionOptions {
                    blinding_seed: Some(settle_blinding_seed(&self.position_utxo.blinding)?),
                    output_tree_id: Some(self.tree_id),
                },
                terms: terms.circuit_terms()?,
                position_utxo: position_input,
                trader_address: terms.trader,
                maker_address: terms.maker,
            },
            public: SettlePublicInputsClient {
                feed_hi,
                feed_lo,
                price: self.price,
                signer_owner_pk_field: field(identity(&signer)?)?,
                first_nullifier: field(first_nullifier)?,
            },
        }
        .prepare(&sender, self.payer)
        .map_err(err)?;
        let finalized = transaction.finalized;
        ensure!(
            finalized.first_nullifier().map_err(err)? == first_nullifier,
            "the position UTXO must be the first input"
        );

        // 3. Check both payouts derive from the position opening.
        let (trader_output, maker_output) = self
            .position_utxo
            .derived_payouts(&first_nullifier, self.price)?;
        let outputs = finalized.output_utxos();
        for (slot, expected, party) in [
            (TRADER_PAYOUT_SLOT, &trader_output, "trader"),
            (MAKER_PAYOUT_SLOT, &maker_output, "maker"),
        ] {
            let output = outputs
                .get(slot)
                .ok_or_else(|| anyhow!("the settle transaction has no {party} payout"))?;
            ensure!(
                output.hash(self.tree_id).map_err(err)?
                    == expected.hash(self.tree_id).map_err(err)?,
                "the {party} payout does not derive from the position opening"
            );
        }

        // 4. Encrypt the outputs.
        let key = settler
            .get_transaction_viewing_key(&first_nullifier)
            .map_err(err)?;
        let spp_proof_inputs = finalized.encrypt_with_viewing_key(&key).map_err(err)?;
        Ok(SettleTransaction {
            maker,
            signer,
            oracle: self.oracle,
            spp_proof_inputs,
            proof_inputs: transaction.proof_inputs,
            first_nullifier,
            trader_payout: trader_output.amount,
            maker_payout: maker_output.amount,
        })
    }
}

impl SettleTransaction {
    /// 1. Prove the SPP transaction, whose input the maker's authority owns.
    /// 2. Prove the settle circuit and build the instruction.
    pub fn prove(self, indexer: &SprIndexer, tree: Address) -> Result<Settle> {
        let payer = self.spp_proof_inputs.payer;
        // 1. Prove the SPP transaction.
        let spp_proof = prove_transact(
            indexer,
            self.spp_proof_inputs,
            &ProgramOwner::nullifier_key(),
        )?;
        // 2. Prove the settle circuit and build the instruction.
        let settle_proof = proof::prove(CircuitId::Settle, &self.proof_inputs)?;
        Ok(Settle {
            payer,
            signer: self.signer,
            oracle: self.oracle,
            maker: self.maker,
            tree,
            settle_proof,
            spp_proof,
        })
    }
}
