//! The veil's two transactions: `veil_in` moves money from a depositor's
//! notes into a veil note, and `veil_out` pays a veil note out to any owner
//! on knowledge of its secret. See `circuits::veil` in the program.

use anyhow::{anyhow, ensure, Result};
use perps_program::circuits::{
    VeilInClient, VeilInPrivateInputsClient, VeilInPublicInputsClient, VeilNoteClient,
    VeilOutClient, VeilOutPrivateInputsClient, VeilOutPublicInputsClient, VEIL_INPUTS,
};
use solana_address::Address;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;
use spr_client::SprIndexer;
use spr_interface::instruction::instruction_data::transact::TransactIxData;
use spr_keypair::ShieldedAddress;
use spr_program::{ProgramOwner, ProgramProofInputs, TransactionOptions};
use spr_transaction::{instructions::transact::SppProofInputs, WalletUtxo};

use crate::{
    err,
    party::Party,
    proof::{self, CircuitId},
    shared::{field, prove_transact, spp_accounts_of, WithAuthorityInputs},
    state::veil::{secret_field, VeilNoteUtxo, VEIL_CHANGE_SLOT, VEIL_NOTE_SLOT, VEIL_PAYOUT_SLOT},
    tag, veil_authority, veil_bump, veil_pda, VeilInIxData, VeilOutIxData, VeilProof,
};

/// How many of the depositor's notes one `veil_in` can gather.
pub const MAX_SOURCES: usize = VEIL_INPUTS;

pub struct VeilInTransactionParams {
    /// The depositor's notes to spend, of one mint, at most [`MAX_SOURCES`],
    /// in the output tree. What is not moved returns as change.
    pub sources: Vec<WalletUtxo>,
    /// What the veil note holds.
    pub amount: u64,
    /// The note's claim, [`crate::state::claim_of`] a secret.
    pub claim: [u8; 32],
    pub payer: Address,
    /// The output tree, which also holds the sources.
    pub tree_id: u16,
}

pub struct VeilInTransaction {
    /// The depositor, who signs for the notes it spends.
    pub depositor: Address,
    /// The viewing key the veil note is encrypted to: the depositor's.
    pub viewing_pubkey: [u8; 33],
    pub spp_proof_inputs: SppProofInputs,
    pub proof_inputs: ProgramProofInputs,
    /// The veil note the transaction creates, with its derived blinding.
    pub note: VeilNoteUtxo,
    /// What comes back to the depositor.
    pub change: u64,
}

impl VeilInTransactionParams {
    /// 1. Check the sources are the depositor's and cover the amount.
    /// 2. Build the program transaction.
    /// 3. Read the veil note it created.
    /// 4. Encrypt the outputs.
    pub fn build(self, depositor: &impl Party) -> Result<VeilInTransaction> {
        // 1. Check the sources.
        let sender = depositor.shielded_address()?;
        ensure!(self.amount != 0, "the amount is zero");
        ensure!(
            !self.sources.is_empty() && self.sources.len() <= MAX_SOURCES,
            "a veil deposit spends one to {MAX_SOURCES} notes"
        );
        let mint = self.sources[0].utxo.asset;
        let mut held = 0u64;
        for source in &self.sources {
            ensure!(
                source.utxo.owner == sender.signing_pubkey,
                "a source is not owned by the depositor"
            );
            ensure!(source.utxo.asset == mint, "the sources are of two mints");
            ensure!(
                source.tree_id == self.tree_id,
                "a source is in tree {}, not in the output tree {}",
                source.tree_id,
                self.tree_id
            );
            held = held
                .checked_add(source.utxo.amount)
                .ok_or_else(|| anyhow!("the sources overflow"))?;
        }
        ensure!(held >= self.amount, "the sources hold less than the amount");

        // 2. Build the program transaction. The note's owner is the veil
        // PDA with the depositor's own viewing key, so the depositor can
        // read the note back.
        let transaction = VeilInClient {
            private: VeilInPrivateInputsClient {
                tx_context: TransactionOptions {
                    blinding_seed: None,
                    output_tree_id: Some(self.tree_id),
                },
                sources: self.sources,
                amount: self.amount,
                claim: self.claim,
            },
            public: VeilInPublicInputsClient {
                note_owner: veil_authority()
                    .address(sender.viewing_pubkey)
                    .map_err(err)?,
            },
        }
        .prepare(&sender, self.payer)
        .map_err(err)?;
        let finalized = transaction.finalized;

        // 3. Read the veil note.
        let outputs = finalized.output_utxos();
        let output = outputs
            .get(VEIL_NOTE_SLOT)
            .ok_or_else(|| anyhow!("the veil transaction has no note output"))?;
        let note = VeilNoteUtxo {
            claim: self.claim,
            blinding: output.blinding,
            mint,
            amount: self.amount,
        };
        ensure!(
            note.hash(self.tree_id)? == output.hash(self.tree_id).map_err(err)?,
            "the note output does not commit the claim"
        );
        let change = outputs
            .get(VEIL_CHANGE_SLOT)
            .map_or(0, |change| change.amount);

        // 4. Encrypt the outputs.
        let key = depositor
            .viewing_key()
            .get_transaction_viewing_key(&finalized.first_nullifier().map_err(err)?)
            .map_err(err)?;
        let spp_proof_inputs = finalized.encrypt_with_viewing_key(&key).map_err(err)?;
        Ok(VeilInTransaction {
            depositor: sender.solana_address().map_err(err)?,
            viewing_pubkey: *sender.viewing_pubkey.as_bytes(),
            spp_proof_inputs,
            proof_inputs: transaction.proof_inputs,
            note,
            change,
        })
    }
}

impl VeilInTransaction {
    /// 1. Prove the SPP transaction, whose inputs the depositor owns.
    /// 2. Prove the `veil_in` circuit and build the instruction.
    pub fn prove(
        self,
        indexer: &SprIndexer,
        tree: Address,
        depositor: &impl Party,
    ) -> Result<VeilIn> {
        let payer = self.spp_proof_inputs.payer;
        let spp_proof = prove_transact(
            indexer,
            self.spp_proof_inputs,
            &WithAuthorityInputs(depositor.nullifier_key()),
        )?;
        let proof = proof::prove(CircuitId::VeilIn, &self.proof_inputs)?;
        Ok(VeilIn {
            payer,
            depositor: self.depositor,
            viewing_pubkey: self.viewing_pubkey,
            tree,
            proof,
            spp_proof,
        })
    }
}

#[derive(Clone)]
pub struct VeilIn {
    pub payer: Pubkey,
    /// The owner of the notes spent, who signs for them.
    pub depositor: Pubkey,
    /// The viewing key of the veil note's address.
    pub viewing_pubkey: [u8; 33],
    pub tree: Pubkey,
    pub proof: VeilProof,
    pub spp_proof: TransactIxData,
}

impl VeilIn {
    pub fn instruction(self) -> Result<Instruction> {
        // The depositor signs for its notes; the veil PDA owns the
        // data-bearing note output, and the program signs for it in the CPI.
        let mut accounts = vec![AccountMeta::new(self.payer, true)];
        accounts.extend(spp_accounts_of(
            veil_pda(),
            self.payer,
            self.tree,
            vec![self.depositor, veil_pda()],
            &self.spp_proof,
        ));
        let body = wincode::serialize(&VeilInIxData {
            proof: self.proof,
            viewing_pubkey: self.viewing_pubkey,
            authority_bump: veil_bump(),
            transact: self.spp_proof,
        })
        .map_err(err)?;
        let mut data = vec![tag::VEIL_IN];
        data.extend_from_slice(&body);
        Ok(Instruction {
            program_id: perps_program::ID,
            accounts,
            data,
        })
    }
}

pub struct VeilOutTransactionParams {
    pub note: VeilNoteUtxo,
    /// The note's leaf index in the output tree.
    pub leaf_index: u64,
    /// The secret behind the note's claim, a canonical field element.
    pub secret: [u8; 32],
    /// Who is paid the note's whole amount.
    pub recipient: ShieldedAddress,
    pub payer: Address,
    /// The output tree, which also holds the note.
    pub tree_id: u16,
}

pub struct VeilOutTransaction {
    pub spp_proof_inputs: SppProofInputs,
    pub proof_inputs: ProgramProofInputs,
    /// The veil note's nullifier, the transaction's first nullifier.
    pub first_nullifier: [u8; 32],
    /// What the recipient receives: the whole note.
    pub amount: u64,
}

impl VeilOutTransactionParams {
    /// 1. Check the secret opens the note.
    /// 2. Build the program transaction.
    /// 3. Encrypt the payout to the recipient.
    ///
    /// `sender` is whoever sends the transaction: its keys only seal the
    /// payout's ciphertext. It need not be the recipient or the depositor.
    pub fn build(self, sender: &impl Party) -> Result<VeilOutTransaction> {
        // 1. Check the secret.
        ensure!(
            self.note.opens_with(&self.secret),
            "that secret does not open the veil note"
        );
        let input = self.note.to_input_utxo(self.tree_id, self.leaf_index)?;
        let first_nullifier = input.nullifier;

        // 2. Build the program transaction.
        let transaction = VeilOutClient {
            private: VeilOutPrivateInputsClient {
                tx_context: TransactionOptions {
                    blinding_seed: None,
                    output_tree_id: Some(self.tree_id),
                },
                terms: VeilNoteClient {
                    claim: self.note.claim,
                },
                note_utxo: input,
                secret: secret_field(&self.secret)?,
                recipient: self.recipient,
            },
            public: VeilOutPublicInputsClient {
                first_nullifier: field(first_nullifier)?,
            },
        }
        .prepare(&sender.shielded_address()?, self.payer)
        .map_err(err)?;
        let finalized = transaction.finalized;
        ensure!(
            finalized.first_nullifier().map_err(err)? == first_nullifier,
            "the veil note must be the first input"
        );
        let amount = finalized
            .output_utxos()
            .get(VEIL_PAYOUT_SLOT)
            .ok_or_else(|| anyhow!("the veil transaction has no payout"))?
            .amount;
        ensure!(
            amount == self.note.amount,
            "the payout is not the whole note"
        );

        // 3. Encrypt the payout.
        let key = sender
            .viewing_key()
            .get_transaction_viewing_key(&first_nullifier)
            .map_err(err)?;
        let spp_proof_inputs = finalized.encrypt_with_viewing_key(&key).map_err(err)?;
        Ok(VeilOutTransaction {
            spp_proof_inputs,
            proof_inputs: transaction.proof_inputs,
            first_nullifier,
            amount,
        })
    }
}

impl VeilOutTransaction {
    /// 1. Prove the SPP transaction, whose input the veil PDA owns.
    /// 2. Prove the `veil_out` circuit and build the instruction.
    pub fn prove(self, indexer: &SprIndexer, tree: Address) -> Result<VeilOut> {
        let payer = self.spp_proof_inputs.payer;
        let spp_proof = prove_transact(
            indexer,
            self.spp_proof_inputs,
            &ProgramOwner::nullifier_key(),
        )?;
        let proof = proof::prove(CircuitId::VeilOut, &self.proof_inputs)?;
        Ok(VeilOut {
            payer,
            tree,
            proof,
            spp_proof,
        })
    }
}

#[derive(Clone)]
pub struct VeilOut {
    pub payer: Pubkey,
    pub tree: Pubkey,
    pub proof: VeilProof,
    pub spp_proof: TransactIxData,
}

impl VeilOut {
    pub fn instruction(self) -> Result<Instruction> {
        // The note is the only input, owned by the veil PDA. Nobody but the
        // fee payer signs.
        let mut accounts = vec![AccountMeta::new(self.payer, true)];
        accounts.extend(spp_accounts_of(
            veil_pda(),
            self.payer,
            self.tree,
            vec![veil_pda()],
            &self.spp_proof,
        ));
        let body = wincode::serialize(&VeilOutIxData {
            proof: self.proof,
            authority_bump: veil_bump(),
            transact: self.spp_proof,
        })
        .map_err(err)?;
        let mut data = vec![tag::VEIL_OUT];
        data.extend_from_slice(&body);
        Ok(Instruction {
            program_id: perps_program::ID,
            accounts,
            data,
        })
    }
}
