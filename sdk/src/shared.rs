use anyhow::{ensure, Result};
use solana_address::Address;
use solana_instruction::AccountMeta;
use spr_client::{ProofAuthority, SprIndexer, TransferInput};
use spr_hasher::primitives::solana_owner_identity;
use spr_interface::instruction::instruction_data::transact::TransactIxData;
use spr_keypair::ShieldedKeypair;
use spr_program::{instruction::Transact, poseidon, Field, ProgramOwner};
use spr_transaction::instructions::transact::SppProofInputs;

use crate::{err, maker_authority_pda};

/// Raw id of the tree a rediscovered leaf was appended to. The tree id is the
/// second element of a UTXO commitment, so the indexer must hash a candidate
/// opening under the same id as the leaf it compares against. The examples
/// use the localnet's single default tree, whose raw id is 0.
pub const INDEXED_TREE_ID: u16 = 0;

pub(crate) fn field(bytes: [u8; 32]) -> Result<Field> {
    spr_program::from_bytes(&bytes).map_err(err)
}

/// A Solana address's owner identity, `hash_bytes` of its tagged pubkey: the
/// value the program computes from a signer and a quote's designated trader.
pub fn identity(address: &Address) -> Result<[u8; 32]> {
    solana_owner_identity(address.as_array()).map_err(err)
}

/// An oracle feed id as the big-endian high and low halves of its 32 bytes,
/// the form the quote and position terms commit it in.
pub fn feed_halves(feed_id: &[u8; 32]) -> (u128, u128) {
    let (hi, lo) = feed_id.split_at(16);
    (
        u128::from_be_bytes(hi.try_into().expect("16 bytes")),
        u128::from_be_bytes(lo.try_into().expect("16 bytes")),
    )
}

/// The feed id of its two halves; the inverse of [`feed_halves`].
pub fn feed_id(feed_hi: u128, feed_lo: u128) -> [u8; 32] {
    let mut id = [0; 32];
    let (hi, lo) = id.split_at_mut(16);
    hi.copy_from_slice(&feed_hi.to_be_bytes());
    lo.copy_from_slice(&feed_lo.to_be_bytes());
    id
}

fn blinding_seed(domain: u32, opening_blinding: &[u8; 32]) -> Result<[u8; 32]> {
    let seed = poseidon(&[Field::from(domain), field(*opening_blinding)?]).map_err(err)?;
    Ok(spr_program::to_bytes(&seed))
}

/// The open's blinding seed, `Poseidon(DPOP, quote_blinding)`, which the open
/// circuit enforces. Maker and trader both hold the quote blinding, so either
/// derives the position UTXO from it and the open's first nullifier.
pub fn open_blinding_seed(quote_blinding: &[u8; 32]) -> Result<[u8; 32]> {
    blinding_seed(perps_program::OPEN_BLINDING_DOMAIN, quote_blinding)
}

/// The settle's blinding seed, `Poseidon(DPST, position_blinding)`, which the
/// settle circuit enforces. Maker and trader both hold the position blinding,
/// so either derives both payouts from it and the settle's first nullifier.
pub fn settle_blinding_seed(position_blinding: &[u8; 32]) -> Result<[u8; 32]> {
    blinding_seed(perps_program::SETTLE_BLINDING_DOMAIN, position_blinding)
}

/// Proves an SPP transaction through `indexer`, which serves the Merkle
/// proofs, completing the inputs with `authority`, and checks that the SPP
/// proof covers the transaction the program proof covers.
pub(crate) fn prove_transact(
    indexer: &SprIndexer,
    spp_proof_inputs: SppProofInputs,
    authority: &dyn ProofAuthority,
) -> Result<TransactIxData> {
    let private_tx_hash = spp_proof_inputs
        .padding_independent_private_tx_hash()
        .map_err(err)?;
    let spp = indexer
        .prove_transact(spp_proof_inputs, authority)
        .map_err(err)?;
    ensure!(
        spp.private_tx_hash == private_tx_hash,
        "the SPP proof and the program proof cover different transactions"
    );
    Ok(spp)
}

/// The proof authority of a transaction that spends a UTXO of a maker's
/// authority PDA, under the zero nullifier key, next to the user's own.
pub struct WithAuthorityInputs<'a>(pub &'a ShieldedKeypair);

impl ProofAuthority for WithAuthorityInputs<'_> {
    fn complete_inputs(&self, inputs: &mut [TransferInput]) -> Result<(), spr_client::ClientError> {
        for input in inputs.iter_mut() {
            if input.nullifier_secret.is_some() {
                continue;
            }
            let input = core::slice::from_mut(input);
            // The zero key refuses an input whose nullifier it does not derive.
            if ProgramOwner::nullifier_key()
                .complete_inputs(input)
                .is_err()
            {
                self.0.complete_inputs(input)?;
            }
        }
        Ok(())
    }
}

/// The SPP `transact` accounts of `data` in one tree, as the program
/// forwards them: payer, output tree, the SPP and System programs, the input
/// tree, one nullifier PDA per input, then `owner_signers`. `maker`'s
/// authority PDA, when among the owner signers, does not sign at transaction
/// entry: the program supplies its signature during the CPI.
pub(crate) fn spp_accounts(
    maker: &Address,
    payer: Address,
    tree: Address,
    owner_signers: Vec<Address>,
    data: &TransactIxData,
) -> Vec<AccountMeta> {
    let authority = maker_authority_pda(maker);
    let mut accounts = Transact {
        payer,
        input_trees: vec![tree],
        output_tree: tree,
        owner_signers,
        interface_transfer_accounts: Vec::new(),
        data: data.clone(),
    }
    .instruction()
    .accounts;
    for account in &mut accounts {
        if account.pubkey == authority {
            account.is_signer = false;
        }
    }
    accounts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feed_id_round_trips_through_its_halves() {
        let id: [u8; 32] = core::array::from_fn(|i| (i as u8).wrapping_mul(37).wrapping_add(11));
        let (hi, lo) = feed_halves(&id);
        assert_eq!(feed_id(hi, lo), id);
        assert_eq!(feed_halves(&[0; 32]), (0, 0));
        assert_eq!(feed_halves(&[0xff; 32]), (u128::MAX, u128::MAX));
    }

    #[test]
    fn blinding_seeds_are_domain_separated() {
        let blinding = {
            let mut bytes = [7u8; 32];
            bytes[0] = 0;
            bytes
        };
        assert_ne!(
            open_blinding_seed(&blinding).unwrap(),
            settle_blinding_seed(&blinding).unwrap()
        );
    }
}
