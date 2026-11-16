use anyhow::{anyhow, ensure, Result};
use perps_program::circuits::{
    OptionQuoteClient, OptionQuotePrivateInputsClient, OptionQuotePublicInputsClient,
};
use solana_address::Address;
use spr_client::SprIndexer;
use spr_keypair::{ShieldedAddress, ShieldedKeypair};
use spr_program::{ProgramProofInputs, TransactionOptions};
use spr_transaction::{instructions::transact::SppProofInputs, WalletUtxo};

use crate::{
    err,
    instructions::quote::{Quote, QuoteMarker, CHANGE_SLOT, QUOTE_SLOT},
    maker_authority,
    proof::{self, CircuitId},
    shared::{field, identity, prove_transact},
    state::{OptionQuoteTerms, OptionQuoteUtxo},
};

pub struct OptionQuoteTransactionParams {
    pub terms: OptionQuoteTerms,
    /// The collateral locked in the quote UTXO; the rest of `source` is
    /// change. With the premium it is the most the contract pays.
    pub collateral: u64,
    /// The spent source UTXO of the collateral mint, owned by the maker, who
    /// signs for it. Its tree is the output tree.
    pub source: WalletUtxo,
    /// The trader the quote output is encrypted to and the marker is tagged
    /// for.
    pub trader_address: ShieldedAddress,
    /// The maker's Solana address, the fee payer and the marker's signer.
    pub payer: Address,
}

pub struct OptionQuoteTransaction {
    /// The trader's viewing key, the quote UTXO address's viewing key.
    pub viewing_pubkey: [u8; 33],
    pub spp_proof_inputs: SppProofInputs,
    pub proof_inputs: ProgramProofInputs,
    /// The quote UTXO the transaction creates, with its derived blinding.
    pub quote_utxo: OptionQuoteUtxo,
    /// The change output's blinding.
    pub change_blinding: [u8; 32],
}

impl OptionQuoteTransactionParams {
    /// 1. Check the terms name the maker and the maker owns the source.
    /// 2. Build the program transaction.
    /// 3. Read the quote UTXO and the change it created.
    /// 4. Encrypt the outputs and add the trader's marker message.
    pub fn build(self, maker: &ShieldedKeypair) -> Result<OptionQuoteTransaction> {
        // 1. Check the terms name the maker and the maker owns the source.
        let sender = maker.shielded_address().map_err(err)?;
        ensure!(
            self.terms.maker == sender,
            "the quote terms name another maker"
        );
        ensure!(
            self.payer == sender.solana_address().map_err(err)?,
            "the payer is not the maker's Solana address"
        );
        ensure!(
            self.source.utxo.owner == maker.signing_pubkey(),
            "the source is not owned by the maker"
        );
        ensure!(self.collateral != 0, "the collateral is zero");
        let collateral_mint = self.source.utxo.asset;
        let tree_id = self.source.tree_id;

        // 2. Build the program transaction.
        let terms = self.terms.circuit_terms()?;
        let transaction = OptionQuoteClient {
            private: OptionQuotePrivateInputsClient {
                tx_context: TransactionOptions {
                    blinding_seed: None,
                    output_tree_id: Some(tree_id),
                },
                source: self.source,
                collateral: self.collateral,
                feed_hi: terms.feed_hi,
                feed_lo: terms.feed_lo,
                kind: terms.kind,
                size: terms.size,
                strike: terms.strike,
                premium: terms.premium,
                term_end: terms.term_end,
                expiry: terms.expiry,
                taker_pk_fe: terms.taker_pk_fe,
            },
            public: OptionQuotePublicInputsClient {
                maker_owner_pk_field: field(identity(&self.payer)?)?,
                quote_owner: maker_authority(&self.payer)
                    .address(self.trader_address.viewing_pubkey)
                    .map_err(err)?,
            },
        }
        .prepare(&sender, self.payer)
        .map_err(err)?;
        let finalized = transaction.finalized;

        // 3. Read the quote UTXO and the change it created.
        let outputs = finalized.output_utxos();
        let change = outputs
            .get(CHANGE_SLOT)
            .ok_or_else(|| anyhow!("the quote transaction has no change output"))?;
        let quote_output = outputs
            .get(QUOTE_SLOT)
            .ok_or_else(|| anyhow!("the quote transaction has no quote output"))?;
        let quote_utxo = OptionQuoteUtxo {
            terms: self.terms,
            blinding: quote_output.blinding,
            collateral_mint,
            collateral: self.collateral,
        };
        let quote_utxo_hash = quote_output.hash(tree_id).map_err(err)?;
        ensure!(
            quote_utxo.hash(tree_id)? == quote_utxo_hash,
            "the quote output does not commit the quote terms"
        );
        let change_blinding = change.blinding;

        // 4. Encrypt the outputs and add the marker, which the program
        // checks and forwards unchanged.
        let key = maker
            .get_transaction_viewing_key(&finalized.first_nullifier().map_err(err)?)
            .map_err(err)?;
        let mut spp_proof_inputs = finalized.encrypt_with_viewing_key(&key).map_err(err)?;
        spp_proof_inputs.external_data.messages.push(
            QuoteMarker {
                quote_utxo_hash,
                maker_pubkey: self.payer,
                trader_address: self.trader_address,
            }
            .message()
            .map_err(err)?,
        );
        Ok(OptionQuoteTransaction {
            viewing_pubkey: *self.trader_address.viewing_pubkey.as_bytes(),
            spp_proof_inputs,
            proof_inputs: transaction.proof_inputs,
            quote_utxo,
            change_blinding,
        })
    }
}

impl OptionQuoteTransaction {
    /// 1. Prove the SPP transaction, whose real input the maker owns.
    /// 2. Prove the option quote circuit. The result's
    ///    [`Quote::option_instruction`] is the instruction to send.
    pub fn prove(
        self,
        indexer: &SprIndexer,
        tree: Address,
        maker: &ShieldedKeypair,
    ) -> Result<Quote> {
        let payer = self.spp_proof_inputs.payer;
        let spp_proof = prove_transact(indexer, self.spp_proof_inputs, maker)?;
        let quote_proof = proof::prove(CircuitId::OptionQuote, &self.proof_inputs)?;
        Ok(Quote {
            viewing_pubkey: self.viewing_pubkey,
            payer,
            tree,
            quote_proof,
            spp_proof,
        })
    }
}
