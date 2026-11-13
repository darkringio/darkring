use anyhow::{anyhow, ensure, Result};
use perps_program::circuits::{QuoteClient, QuotePrivateInputsClient, QuotePublicInputsClient};
use solana_address::Address;
use spr_client::SprIndexer;
use spr_keypair::{ShieldedAddress, ShieldedKeypair};
use spr_program::{ProgramProofInputs, TransactionOptions};
use spr_transaction::{instructions::transact::SppProofInputs, WalletUtxo};

use super::{Quote, QuoteMarker};
use crate::{
    err, maker_authority,
    proof::{self, CircuitId},
    shared::{field, identity, prove_transact},
    state::{QuoteTerms, QuoteUtxo},
};

/// Output slot of the change in the quote transaction.
pub const CHANGE_SLOT: usize = 0;
/// Output slot of the quote UTXO in the quote transaction; the program reads
/// the quote UTXO hash for the marker from it.
pub const QUOTE_SLOT: usize = 1;

pub struct QuoteTransactionParams {
    pub terms: QuoteTerms,
    /// The margin locked in the quote UTXO; the rest of `source` is change.
    pub maker_margin: u64,
    /// The spent source UTXO of the collateral mint, owned by the maker, who
    /// signs for it. The rest of it returns to the maker as change. Its tree
    /// is the output tree.
    pub source: WalletUtxo,
    /// The trader the quote output is encrypted to and the marker is tagged
    /// for. It may differ from `terms.trader`, the identity allowed to
    /// accept.
    pub trader_address: ShieldedAddress,
    /// The maker's Solana address, the fee payer and the signer the client
    /// writes into the marker. The program checks the marker names this
    /// signer, and the quote proof checks it against the source owner.
    pub payer: Address,
}

pub struct QuoteTransaction {
    /// The trader's viewing key, the quote UTXO address's viewing key, which
    /// the program hashes into the quote proof's public `quote_owner`.
    pub viewing_pubkey: [u8; 33],
    pub spp_proof_inputs: SppProofInputs,
    pub proof_inputs: ProgramProofInputs,
    /// The quote UTXO the transaction creates, with its derived blinding.
    pub quote_utxo: QuoteUtxo,
    /// The change output's blinding.
    pub change_blinding: [u8; 32],
}

impl QuoteTransactionParams {
    /// 1. Check the terms name the maker and the maker owns the source.
    /// 2. Build the program transaction.
    /// 3. Read the quote UTXO and the change it created.
    /// 4. Encrypt the outputs and add the trader's marker message.
    pub fn build(self, maker: &ShieldedKeypair) -> Result<QuoteTransaction> {
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
        ensure!(self.maker_margin != 0, "the maker margin is zero");
        let collateral_mint = self.source.utxo.asset;
        let tree_id = self.source.tree_id;

        // 2. Build the program transaction. The circuit takes the maker from
        // the source owner; the quote UTXO's owner is the maker's authority
        // with the trader's viewing key, which the program derives from the
        // maker signer.
        let terms = self.terms.circuit_terms()?;
        let transaction = QuoteClient {
            private: QuotePrivateInputsClient {
                tx_context: TransactionOptions {
                    blinding_seed: None,
                    output_tree_id: Some(tree_id),
                },
                source: self.source,
                maker_margin: self.maker_margin,
                feed_hi: terms.feed_hi,
                feed_lo: terms.feed_lo,
                trader_is_long: terms.trader_is_long,
                size: terms.size,
                entry_price: terms.entry_price,
                trader_margin: terms.trader_margin,
                maintenance_margin: terms.maintenance_margin,
                expiry: terms.expiry,
                taker_pk_fe: terms.taker_pk_fe,
            },
            public: QuotePublicInputsClient {
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
        let quote_utxo = QuoteUtxo {
            terms: self.terms,
            blinding: quote_output.blinding,
            collateral_mint,
            maker_margin: self.maker_margin,
        };
        let quote_utxo_hash = quote_output.hash(tree_id).map_err(err)?;
        ensure!(
            quote_utxo.hash(tree_id)? == quote_utxo_hash,
            "the quote output does not commit the quote terms"
        );
        let change_blinding = change.blinding;

        // 4. Encrypt the outputs and add the marker, the transaction's only
        // message. The program checks its data is the `QuoteMarkerData` of
        // the quote output and the payer, and forwards it unchanged.
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
        Ok(QuoteTransaction {
            viewing_pubkey: *self.trader_address.viewing_pubkey.as_bytes(),
            spp_proof_inputs,
            proof_inputs: transaction.proof_inputs,
            quote_utxo,
            change_blinding,
        })
    }
}

impl QuoteTransaction {
    /// 1. Prove the SPP transaction, whose real input the maker owns.
    /// 2. Prove the quote circuit and build the instruction.
    pub fn prove(
        self,
        indexer: &SprIndexer,
        tree: Address,
        maker: &ShieldedKeypair,
    ) -> Result<Quote> {
        let payer = self.spp_proof_inputs.payer;
        // 1. Prove the SPP transaction.
        let spp_proof = prove_transact(indexer, self.spp_proof_inputs, maker)?;
        // 2. Prove the quote circuit and build the instruction.
        let quote_proof = proof::prove(CircuitId::Quote, &self.proof_inputs)?;
        Ok(Quote {
            viewing_pubkey: self.viewing_pubkey,
            payer,
            tree,
            quote_proof,
            spp_proof,
        })
    }
}
