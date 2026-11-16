//! A depositor's veil notes, found again from the chain.

use anyhow::Result;
use perps_program::circuits;
use spr_client::Rpc;
use spr_transaction::AssetRegistry;

use super::poll::{collect_transactions, spent_among};
use crate::{err, party::Party, state::VeilNoteUtxo, veil_authority};

/// A veil note its depositor found, with where it sits in its tree.
#[derive(Debug)]
pub struct FoundVeilNote {
    pub note: VeilNoteUtxo,
    pub tree_id: u16,
    pub leaf_index: u64,
}

/// The veil notes `depositor` put behind the veil that nobody has paid out
/// yet. A `veil_in` encrypts its note to the depositor's own viewing key, so
/// only the depositor reads it back; the secret that pays it out is not on
/// chain and the depositor has to have kept it.
///
/// Every veil note of every depositor carries the veil PDA's tag, so this
/// reads all of them and keeps the ones that decrypt.
pub fn index_veil_notes<I: Rpc>(
    depositor: &impl Party,
    registry: &AssetRegistry,
    indexer: &I,
) -> Result<Vec<FoundVeilNote>> {
    let authority = veil_authority();
    let transactions = collect_transactions(authority.confidential_view_tag(), indexer)?;
    let notes = authority
        .decrypt_data_utxos::<circuits::VeilNote>(depositor.viewing_key(), &transactions, registry)
        .map_err(err)?;
    let spent = spent_among(
        indexer,
        notes.iter().map(|note| note.utxo.nullifier).collect(),
    )?;
    Ok(notes
        .into_iter()
        .filter(|note| !spent.contains(&note.utxo.nullifier))
        .map(|note| FoundVeilNote {
            tree_id: note.utxo.tree_id,
            leaf_index: note.utxo.leaf_index,
            note: VeilNoteUtxo::from_data_utxo(note),
        })
        .collect())
}
