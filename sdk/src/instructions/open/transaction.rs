use anyhow::{anyhow, ensure, Result};
use perps_program::circuits::{OpenClient, OpenPrivateInputsClient, OpenPublicInputsClient};
use solana_address::Address;
use spr_client::SprIndexer;
use spr_program::{ProgramProofInputs, TransactionOptions};
use spr_transaction::{instructions::transact::SppProofInputs, WalletUtxo};

use super::Open;
use crate::{
    err,
    party::Party,
    proof::{self, CircuitId},
    shared::{field, identity, open_blinding_seed, prove_transact, WithAuthorityInputs},
    state::{PositionUtxo, QuoteUtxo},
};

/// Output slot of the position UTXO in the open transaction.
pub const POSITION_SLOT: usize = crate::state::POSITION_SLOT as usize;
/// Output slot of the trader's change in the open transaction. The slot is
/// empty when the margin input holds exactly the margin.
pub const CHANGE_SLOT: usize = 1;

pub struct OpenTransactionParams {
    pub quote_utxo: QuoteUtxo,
    /// The quote UTXO's leaf index in the output tree.
    pub quote_leaf_index: u64,
    /// The trader's UTXO of the collateral mint holding at least the trader
    /// margin, in the output tree. The rest returns to the trader as change.
    pub trader_input: WalletUtxo,
    pub payer: Address,
    /// The output tree, which also holds both inputs.
    pub tree_id: u16,
}

pub struct OpenTransaction {
    /// The quote's maker, whose authority PDA the program signs for.
    pub maker: Address,
    /// The maker's viewing key, the position UTXO address's viewing key,
    /// which the program hashes into the open proof's public
    /// `position_owner`.
    pub viewing_pubkey: [u8; 33],
    pub spp_proof_inputs: SppProofInputs,
    pub proof_inputs: ProgramProofInputs,
    /// The quote UTXO's nullifier, the transaction's first nullifier.
    pub first_nullifier: [u8; 32],
    /// The position UTXO the transaction creates. The maker derives the same
    /// with [`QuoteUtxo::derived_position`].
    pub position_utxo: PositionUtxo,
}

impl OpenTransactionParams {
    /// 1. Check the margin input covers the margin and the trader may accept.
    /// 2. Build the program transaction under the quote's open blinding
    ///    seed, with the quote expiry as the transact expiry.
    /// 3. Check the maker can derive the position from the quote opening.
    /// 4. Encrypt the outputs.
    ///
    /// `trader` need not hold the key that signs: see [`Party`].
    pub fn build(self, trader: &impl Party) -> Result<OpenTransaction> {
        // 1. Check the margin input and the designated trader.
        let sender = trader.shielded_address()?;
        let terms = &self.quote_utxo.terms;
        ensure!(
            self.trader_input.utxo.asset == self.quote_utxo.collateral_mint,
            "the margin input is not of the quote's collateral mint"
        );
        ensure!(
            self.trader_input.utxo.amount >= terms.trader_margin,
            "the margin input holds less than the trader margin"
        );
        ensure!(
            self.trader_input.utxo.owner == sender.signing_pubkey,
            "the margin input is not owned by the trader"
        );
        ensure!(
            self.trader_input.tree_id == self.tree_id,
            "the margin input is in tree {}, not in the output tree {}",
            self.trader_input.tree_id,
            self.tree_id
        );
        if let Some(designated) = terms.trader {
            ensure!(
                identity(&sender.solana_address().map_err(err)?)? == designated,
                "the quote designates another trader"
            );
        }
        let maker = terms.maker.solana_address().map_err(err)?;
        let quote_input = self
            .quote_utxo
            .to_input_utxo(self.tree_id, self.quote_leaf_index)?;
        let first_nullifier = quote_input.nullifier;

        // 2. Build the program transaction. The position UTXO's owner is the
        // maker's authority with the maker's viewing key, which the program
        // derives from the maker the instruction names.
        let transaction = OpenClient {
            private: OpenPrivateInputsClient {
                tx_context: TransactionOptions {
                    blinding_seed: Some(open_blinding_seed(&self.quote_utxo.blinding)?),
                    output_tree_id: Some(self.tree_id),
                },
                terms: terms.circuit_terms()?,
                quote_utxo: quote_input,
                trader_in: self.trader_input,
            },
            public: OpenPublicInputsClient {
                expiry: terms.expiry,
                first_nullifier: field(first_nullifier)?,
                position_owner: terms
                    .maker_authority()?
                    .address(terms.maker.viewing_pubkey)
                    .map_err(err)?,
            },
        }
        .prepare(&sender, self.payer)
        .map_err(err)?;
        let finalized = transaction.finalized.with_expiry_unix_ts(terms.expiry);
        ensure!(
            finalized.first_nullifier().map_err(err)? == first_nullifier,
            "the quote UTXO must be the first input"
        );

        // 3. Check the maker can derive the position.
        let position_utxo = self.quote_utxo.derived_position(sender, &first_nullifier)?;
        let outputs = finalized.output_utxos();
        let position_output = outputs
            .get(POSITION_SLOT)
            .ok_or_else(|| anyhow!("the open transaction has no position output"))?;
        ensure!(
            position_output.hash(self.tree_id).map_err(err)? == position_utxo.hash(self.tree_id)?,
            "the position does not derive from the quote opening"
        );

        // 4. Encrypt the outputs.
        let key = trader
            .viewing_key()
            .get_transaction_viewing_key(&first_nullifier)
            .map_err(err)?;
        let spp_proof_inputs = finalized.encrypt_with_viewing_key(&key).map_err(err)?;
        Ok(OpenTransaction {
            maker,
            viewing_pubkey: *terms.maker.viewing_pubkey.as_bytes(),
            spp_proof_inputs,
            proof_inputs: transaction.proof_inputs,
            first_nullifier,
            position_utxo,
        })
    }
}

impl OpenTransaction {
    /// 1. Prove the SPP transaction with the authority and trader keys.
    /// 2. Prove the open circuit and build the instruction.
    pub fn prove(self, indexer: &SprIndexer, tree: Address, trader: &impl Party) -> Result<Open> {
        let payer = self.spp_proof_inputs.payer;
        // 1. Prove the SPP transaction.
        let spp_proof = prove_transact(
            indexer,
            self.spp_proof_inputs,
            &WithAuthorityInputs(trader.nullifier_key()),
        )?;
        // 2. Prove the open circuit and build the instruction.
        let open_proof = proof::prove(CircuitId::Open, &self.proof_inputs)?;
        Ok(Open {
            payer,
            maker: self.maker,
            viewing_pubkey: self.viewing_pubkey,
            tree,
            open_proof,
            spp_proof,
        })
    }
}
