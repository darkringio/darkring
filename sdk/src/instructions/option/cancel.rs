use anyhow::{anyhow, ensure, Result};
use perps_program::circuits::{
    OptionCancelClient, OptionCancelPrivateInputsClient, OptionCancelPublicInputsClient,
};
use solana_address::Address;
use spr_client::SprIndexer;
use spr_keypair::ShieldedKeypair;
use spr_program::{ProgramOwner, ProgramProofInputs, TransactionOptions};
use spr_transaction::instructions::transact::SppProofInputs;

use crate::{
    err,
    instructions::cancel_quote::{CancelQuote, MARGIN_OUTPUT_SLOT},
    proof::{self, CircuitId},
    shared::{field, identity, prove_transact},
    state::OptionQuoteUtxo,
};

pub struct OptionCancelTransactionParams {
    pub quote_utxo: OptionQuoteUtxo,
    /// The quote UTXO's leaf index in the output tree.
    pub quote_leaf_index: u64,
    pub payer: Address,
    /// The output tree, which also holds the quote UTXO.
    pub tree_id: u16,
}

pub struct OptionCancelTransaction {
    maker: Address,
    quote_expiry: u64,
    pub spp_proof_inputs: SppProofInputs,
    pub proof_inputs: ProgramProofInputs,
    /// The refunded collateral output's blinding.
    pub refund_blinding: [u8; 32],
}

impl OptionCancelTransactionParams {
    /// 1. Check the maker cancels.
    /// 2. Build the program transaction.
    /// 3. Encrypt the refund to the maker.
    pub fn build(self, maker: &ShieldedKeypair) -> Result<OptionCancelTransaction> {
        // 1. Check the maker cancels.
        let sender = maker.shielded_address().map_err(err)?;
        let terms = &self.quote_utxo.terms;
        ensure!(
            terms.maker == sender,
            "only the quote's maker can cancel it"
        );
        let maker_address = sender.solana_address().map_err(err)?;

        // 2. Build the program transaction.
        let transaction = OptionCancelClient {
            private: OptionCancelPrivateInputsClient {
                tx_context: TransactionOptions {
                    blinding_seed: None,
                    output_tree_id: Some(self.tree_id),
                },
                terms: terms.circuit_terms()?,
                quote_utxo: self
                    .quote_utxo
                    .to_input_utxo(self.tree_id, self.quote_leaf_index)?,
            },
            public: OptionCancelPublicInputsClient {
                expiry: terms.expiry,
                maker_owner_pk_field: field(identity(&maker_address)?)?,
            },
        }
        .prepare(&sender, self.payer)
        .map_err(err)?;
        let finalized = transaction.finalized;
        let refund_blinding = finalized
            .output_utxos()
            .get(MARGIN_OUTPUT_SLOT)
            .ok_or_else(|| anyhow!("the cancel transaction has no refund output"))?
            .blinding;

        // 3. Encrypt the refund to the maker.
        let key = maker
            .get_transaction_viewing_key(&finalized.first_nullifier().map_err(err)?)
            .map_err(err)?;
        let spp_proof_inputs = finalized.encrypt_with_viewing_key(&key).map_err(err)?;
        Ok(OptionCancelTransaction {
            maker: maker_address,
            quote_expiry: terms.expiry,
            spp_proof_inputs,
            proof_inputs: transaction.proof_inputs,
            refund_blinding,
        })
    }
}

impl OptionCancelTransaction {
    /// 1. Prove the SPP transaction, whose input the maker's authority owns.
    /// 2. Prove the option cancel circuit. The result's
    ///    [`CancelQuote::option_instruction`] is the instruction to send.
    pub fn prove(self, indexer: &SprIndexer, tree: Address) -> Result<CancelQuote> {
        let payer = self.spp_proof_inputs.payer;
        let spp_proof = prove_transact(
            indexer,
            self.spp_proof_inputs,
            &ProgramOwner::nullifier_key(),
        )?;
        let cancel_proof = proof::prove(CircuitId::OptionCancel, &self.proof_inputs)?;
        Ok(CancelQuote {
            maker: self.maker,
            payer,
            tree,
            cancel_proof,
            quote_expiry: self.quote_expiry,
            spp_proof,
        })
    }
}
