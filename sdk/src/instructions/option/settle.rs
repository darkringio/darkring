use anyhow::{anyhow, ensure, Result};
use perps_program::circuits::{
    OptionSettleClient, OptionSettlePrivateInputsClient, OptionSettlePublicInputsClient,
};
use solana_address::Address;
use spr_client::SprIndexer;
use spr_program::{ProgramOwner, ProgramProofInputs, TransactionOptions};
use spr_transaction::instructions::transact::SppProofInputs;

use crate::{
    err,
    instructions::settle::Settle,
    party::Party,
    proof::{self, CircuitId},
    shared::{feed_halves, field, identity, option_settle_blinding_seed, prove_transact},
    state::{OptionUtxo, OPTION_MAKER_PAYOUT_SLOT, OPTION_TRADER_PAYOUT_SLOT},
};

pub struct OptionSettleTransactionParams {
    pub option_utxo: OptionUtxo,
    /// The option UTXO's leaf index in the output tree.
    pub option_leaf_index: u64,
    /// The oracle price update account to settle against.
    pub oracle: Address,
    /// The price that account holds, in the feed's price ticks, and when the
    /// oracle published it, in unix seconds. The program reads the account
    /// itself; a proof for another price or time fails on chain.
    pub price: u64,
    pub publish_time: u64,
    pub payer: Address,
    /// The output tree, which also holds the option UTXO.
    pub tree_id: u16,
}

pub struct OptionSettleTransaction {
    /// The contract's maker, whose authority PDA the program signs for.
    pub maker: Address,
    /// The settling party's Solana address.
    pub signer: Address,
    pub oracle: Address,
    pub spp_proof_inputs: SppProofInputs,
    pub proof_inputs: ProgramProofInputs,
    /// The option UTXO's nullifier, the transaction's first nullifier.
    pub first_nullifier: [u8; 32],
    /// What the trader and the maker receive.
    pub trader_payout: u64,
    pub maker_payout: u64,
}

impl OptionSettleTransactionParams {
    /// 1. Check the settler may settle at this publish time.
    /// 2. Build the program transaction under the contract's settle blinding
    ///    seed.
    /// 3. Check both parties can derive their payouts from the option
    ///    opening.
    /// 4. Encrypt the outputs.
    pub fn build(self, settler: &impl Party) -> Result<OptionSettleTransaction> {
        // 1. Check the settler may settle.
        let sender = settler.shielded_address()?;
        let terms = &self.option_utxo.terms;
        let signer = sender.solana_address().map_err(err)?;
        let by_trader = sender == terms.trader;
        ensure!(
            by_trader || sender == terms.maker,
            "only the contract's trader or maker can settle it"
        );
        ensure!(
            self.option_utxo.may_settle(by_trader, self.publish_time),
            "the contract cannot be settled before its term ends"
        );
        let maker = terms.maker.solana_address().map_err(err)?;
        let option_input = self
            .option_utxo
            .to_input_utxo(self.tree_id, self.option_leaf_index)?;
        let first_nullifier = option_input.nullifier;
        let (feed_hi, feed_lo) = feed_halves(&terms.feed_id);

        // 2. Build the program transaction.
        let transaction = OptionSettleClient {
            private: OptionSettlePrivateInputsClient {
                tx_context: TransactionOptions {
                    blinding_seed: Some(option_settle_blinding_seed(&self.option_utxo.blinding)?),
                    output_tree_id: Some(self.tree_id),
                },
                terms: terms.circuit_terms()?,
                option_utxo: option_input,
                trader_address: terms.trader,
                maker_address: terms.maker,
            },
            public: OptionSettlePublicInputsClient {
                feed_hi,
                feed_lo,
                price: self.price,
                publish_time: self.publish_time,
                signer_owner_pk_field: field(identity(&signer)?)?,
                first_nullifier: field(first_nullifier)?,
            },
        }
        .prepare(&sender, self.payer)
        .map_err(err)?;
        let finalized = transaction.finalized;
        ensure!(
            finalized.first_nullifier().map_err(err)? == first_nullifier,
            "the option UTXO must be the first input"
        );

        // 3. Check both payouts derive from the option opening.
        let (trader_output, maker_output) = self
            .option_utxo
            .derived_payouts(&first_nullifier, self.price)?;
        let outputs = finalized.output_utxos();
        for (slot, expected, party) in [
            (OPTION_TRADER_PAYOUT_SLOT as usize, &trader_output, "trader"),
            (OPTION_MAKER_PAYOUT_SLOT as usize, &maker_output, "maker"),
        ] {
            let output = outputs
                .get(slot)
                .ok_or_else(|| anyhow!("the settle transaction has no {party} payout"))?;
            ensure!(
                output.hash(self.tree_id).map_err(err)?
                    == expected.hash(self.tree_id).map_err(err)?,
                "the {party} payout does not derive from the option opening"
            );
        }

        // 4. Encrypt the outputs.
        let key = settler
            .viewing_key()
            .get_transaction_viewing_key(&first_nullifier)
            .map_err(err)?;
        let spp_proof_inputs = finalized.encrypt_with_viewing_key(&key).map_err(err)?;
        Ok(OptionSettleTransaction {
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

impl OptionSettleTransaction {
    /// 1. Prove the SPP transaction, whose input the maker's authority owns.
    /// 2. Prove the option settle circuit. The result's
    ///    [`Settle::option_instruction`] is the instruction to send.
    pub fn prove(self, indexer: &SprIndexer, tree: Address) -> Result<Settle> {
        let payer = self.spp_proof_inputs.payer;
        let spp_proof = prove_transact(
            indexer,
            self.spp_proof_inputs,
            &ProgramOwner::nullifier_key(),
        )?;
        let settle_proof = proof::prove(CircuitId::OptionSettle, &self.proof_inputs)?;
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
