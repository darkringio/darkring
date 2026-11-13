use std::time::Duration;

use anyhow::Result;
use perps_program::circuits;
use solana_address::Address;
use spr_client::Rpc;
use spr_keypair::{ShieldedKeypair, X25519Pubkey};
use spr_transaction::{
    serialization::confidential::Confidential, AssetRegistry, ShieldedTransaction,
};

use super::{
    poll::{collect_tagged, collect_transactions, index_until},
    scan::{decode_quote, resolve_mint, resolve_registered, unified_slots},
};
use crate::{
    err, maker_authority,
    state::{PositionUtxo, QuoteUtxo},
};

/// A quote rediscovered by its maker from its own quote transaction.
#[derive(Debug)]
pub struct MakerQuote {
    pub quote_utxo: QuoteUtxo,
    pub trader_viewing_pubkey: X25519Pubkey,
    /// The quote UTXO's position in its tree.
    pub tree_id: u16,
    pub leaf_index: u64,
}

/// An open position a maker discovered: a trader accepted one of its quotes.
#[derive(Debug)]
pub struct MakerPosition {
    pub position_utxo: PositionUtxo,
    /// The position UTXO's position in its tree.
    pub tree_id: u16,
    pub leaf_index: u64,
}

/// Maker-side quote rediscovery: the per-transaction viewing key re-derives
/// from the maker's viewing key and the first input's nullifier (a match
/// against `tx_viewing_pk` proves the maker authored the transaction). Each
/// unified slot embeds its recipient viewing pubkey, so that key decrypts
/// every slot from the sender side directly; the opening is accepted only if
/// it decodes as a quote UTXO matching the slot's committed leaf and the
/// terms name this maker.
pub fn scan_maker_quote(
    tx: &ShieldedTransaction,
    keypair: &ShieldedKeypair,
    registry: &AssetRegistry,
) -> Result<Option<MakerQuote>> {
    let (Some(tx_viewing_pk), Some(salt)) = (tx.tx_viewing_pk, tx.salt) else {
        return Ok(None);
    };
    let Some(tx_viewing_key) = tx.nullifiers.iter().find_map(|nullifier| {
        keypair
            .get_transaction_viewing_key(nullifier)
            .ok()
            .filter(|key| key.pubkey() == tx_viewing_pk)
    }) else {
        return Ok(None);
    };
    let maker_address = keypair.shielded_address().map_err(err)?;
    let maker_pubkey = maker_address.solana_address().map_err(err)?;
    for (slot_index, slot, body) in unified_slots(tx) {
        let Ok(trader_viewing_pubkey) = Confidential::embedded_viewing_pk(&body) else {
            continue;
        };
        let Ok(plaintext) =
            Confidential::decrypt_with_tx_key(&tx_viewing_key, &body, salt, slot_index)
        else {
            continue;
        };
        let Ok(quote) = resolve_mint(registry, plaintext.asset_id)
            .and_then(|asset| decode_quote(tx, slot, slot_index, plaintext, asset, &maker_pubkey))
        else {
            continue;
        };
        let (tree_id, leaf_index) = (quote.utxo.tree_id, quote.utxo.leaf_index);
        let Ok(quote_utxo) = QuoteUtxo::from_data_utxo(quote, maker_address) else {
            continue;
        };
        return Ok(Some(MakerQuote {
            quote_utxo,
            trader_viewing_pubkey,
            tree_id,
            leaf_index,
        }));
    }
    Ok(None)
}

/// Finds the maker's quotes by the view tag of its authority PDA, the owner
/// of every quote UTXO it posts. A quote a trader has since accepted or the
/// maker cancelled is still returned: spending it then fails.
pub fn index_maker_quotes<I: Rpc>(
    keypair: &ShieldedKeypair,
    registry: &AssetRegistry,
    indexer: &I,
    timeout: Duration,
) -> Result<Vec<MakerQuote>> {
    let maker_pubkey = keypair
        .shielded_address()
        .and_then(|address| address.solana_address())
        .map_err(err)?;
    let tag = maker_authority(&maker_pubkey).confidential_view_tag();
    index_until(timeout, "maker quotes", || {
        collect_tagged(tag, indexer, |tx| scan_maker_quote(tx, keypair, registry))
    })
}

/// Finds the open positions against the maker's quotes. An open transaction
/// encrypts the position UTXO, owned by the maker's authority PDA, to the
/// maker's viewing key; positions a settle has since spent are left out. The
/// trader each position names is resolved in the user registry, and a
/// position whose trader is not registered there, or does not match the
/// record, is skipped: the maker could not address its payout.
pub fn index_maker_positions<I: Rpc, R: Rpc>(
    keypair: &ShieldedKeypair,
    registry: &AssetRegistry,
    indexer: &I,
    rpc: &R,
    timeout: Duration,
) -> Result<Vec<MakerPosition>> {
    let maker_address = keypair.shielded_address().map_err(err)?;
    let maker_pubkey = maker_address.solana_address().map_err(err)?;
    let authority = maker_authority(&maker_pubkey);
    let tag = authority.confidential_view_tag();
    index_until(timeout, "maker positions", || {
        let transactions = collect_transactions(tag, indexer)?;
        let positions = authority
            .decrypt_data_utxos::<circuits::PositionTerms>(
                &keypair.viewing_key,
                &transactions,
                registry,
            )
            .map_err(err)?;
        let mut found = Vec::new();
        for position in positions {
            let (_, trader_key, _) = position.data.trader.preimage();
            let Some(trader) = resolve_registered(rpc, Address::new_from_array(trader_key))? else {
                continue;
            };
            let (tree_id, leaf_index) = (position.utxo.tree_id, position.utxo.leaf_index);
            let Ok(position_utxo) = PositionUtxo::from_data_utxo(position, maker_address, trader)
            else {
                continue;
            };
            found.push(MakerPosition {
                position_utxo,
                tree_id,
                leaf_index,
            });
        }
        Ok(found)
    })
}
