use anyhow::{anyhow, ensure, Result};
use perps_program::circuits::{
    OptionOpenClient, OptionOpenPrivateInputsClient, OptionOpenPublicInputsClient,
};
use solana_address::Address;
use spr_client::SprIndexer;
use spr_program::{ProgramProofInputs, TransactionOptions};
use spr_transaction::{instructions::transact::SppProofInputs, WalletUtxo};

use crate::{
    err,
    instructions::open::Open,
    party::Party,
    proof::{self, CircuitId},
    shared::{field, identity, option_open_blinding_seed, prove_transact, WithAuthorityInputs},
    state::{OptionQuoteUtxo, OptionUtxo, OPTION_SLOT},
};

pub struct OptionOpenTransactionParams {
    pub quote_utxo: OptionQuoteUtxo,
    /// The quote UTXO's leaf index in the output tree.
    pub quote_leaf_index: u64,
    /// The trader's UTXO of the collateral mint holding at least the
    /// premium, in the output tree. The rest returns to the trader as
    /// change.
    pub trader_input: WalletUtxo,
    pub payer: Address,
    /// The output tree, which also holds both inputs.
    pub tree_id: u16,
}

pub struct OptionOpenTransaction {
    /// The quote's maker, whose authority PDA the program signs for.
    pub maker: Address,
    /// The maker's viewing key, the option UTXO address's viewing key.
    pub viewing_pubkey: [u8; 33],
    pub spp_proof_inputs: SppProofInputs,
    pub proof_inputs: ProgramProofInputs,
    /// The quote UTXO's nullifier, the transaction's first nullifier.
    pub first_nullifier: [u8; 32],
    /// The option UTXO the transaction creates. The maker derives the same
    /// with [`OptionQuoteUtxo::derived_option`].
    pub option_utxo: OptionUtxo,
}

impl OptionOpenTransactionParams {
    /// 1. Check the premium input covers the premium and the trader may
    ///    accept.
    /// 2. Build the program transaction under the quote's open blinding
    ///    seed, with the quote expiry as the transact expiry.
    /// 3. Check the maker can derive the contract from the quote opening.
    /// 4. Encrypt the outputs.
    pub fn build(self, trader: &impl Party) -> Result<OptionOpenTransaction> {
        // 1. Check the premium input and the designated trader.
        let sender = trader.shielded_address()?;
        let terms = &self.quote_utxo.terms;
        ensure!(
            self.trader_input.utxo.asset == self.quote_utxo.collateral_mint,
            "the premium input is not of the quote's collateral mint"
        );
        ensure!(
            self.trader_input.utxo.amount >= terms.premium,
            "the premium input holds less than the premium"
        );
        ensure!(
            self.trader_input.utxo.owner == sender.signing_pubkey,
            "the premium input is not owned by the trader"
        );
        ensure!(
            self.trader_input.tree_id == self.tree_id,
            "the premium input is in tree {}, not in the output tree {}",
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

        // 2. Build the program transaction.
        let transaction = OptionOpenClient {
            private: OptionOpenPrivateInputsClient {
                tx_context: TransactionOptions {
                    blinding_seed: Some(option_open_blinding_seed(&self.quote_utxo.blinding)?),
                    output_tree_id: Some(self.tree_id),
                },
                terms: terms.circuit_terms()?,
                quote_utxo: quote_input,
                trader_in: self.trader_input,
            },
            public: OptionOpenPublicInputsClient {
                expiry: terms.expiry,
                first_nullifier: field(first_nullifier)?,
                option_owner: terms
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

        // 3. Check the maker can derive the contract.
        let option_utxo = self.quote_utxo.derived_option(sender, &first_nullifier)?;
        let outputs = finalized.output_utxos();
        let option_output = outputs
            .get(OPTION_SLOT as usize)
            .ok_or_else(|| anyhow!("the open transaction has no option output"))?;
        ensure!(
            option_output.hash(self.tree_id).map_err(err)? == option_utxo.hash(self.tree_id)?,
            "the contract does not derive from the quote opening"
        );

        // 4. Encrypt the outputs.
        let key = trader
            .viewing_key()
            .get_transaction_viewing_key(&first_nullifier)
            .map_err(err)?;
        let spp_proof_inputs = finalized.encrypt_with_viewing_key(&key).map_err(err)?;
        Ok(OptionOpenTransaction {
            maker,
            viewing_pubkey: *terms.maker.viewing_pubkey.as_bytes(),
            spp_proof_inputs,
            proof_inputs: transaction.proof_inputs,
            first_nullifier,
            option_utxo,
        })
    }
}

impl OptionOpenTransaction {
    /// 1. Prove the SPP transaction with the authority and trader keys.
    /// 2. Prove the option open circuit. The result's
    ///    [`Open::option_instruction`] is the instruction to send.
    pub fn prove(self, indexer: &SprIndexer, tree: Address, trader: &impl Party) -> Result<Open> {
        let payer = self.spp_proof_inputs.payer;
        let spp_proof = prove_transact(
            indexer,
            self.spp_proof_inputs,
            &WithAuthorityInputs(trader.nullifier_key()),
        )?;
        let open_proof = proof::prove(CircuitId::OptionOpen, &self.proof_inputs)?;
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
