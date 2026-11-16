//! A maker's dated contracts, found again from the chain: the option quotes
//! it posted and the contracts traders opened against them. The perp
//! counterparts are in `maker`.

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
    poll::{collect_tagged, collect_transactions, index_until, spent_among},
    scan::{decode_owned, resolve_mint, resolve_registered, unified_slots},
};
use crate::{
    err, maker_authority,
    state::{OptionQuoteUtxo, OptionUtxo},
};

/// An option quote rediscovered by its maker from its own quote transaction.
#[derive(Debug)]
pub struct MakerOptionQuote {
    pub quote_utxo: OptionQuoteUtxo,
    pub trader_viewing_pubkey: X25519Pubkey,
    /// The quote UTXO's position in its tree.
    pub tree_id: u16,
    pub leaf_index: u64,
}

/// An open contract a maker discovered: a trader accepted one of its option
/// quotes.
#[derive(Debug)]
pub struct MakerOption {
    pub option_utxo: OptionUtxo,
    /// The option UTXO's position in its tree.
    pub tree_id: u16,
    pub leaf_index: u64,
}

/// Maker-side option quote rediscovery, as [`super::scan_maker_quote`] is for
/// a perp: the per-transaction viewing key re-derives from the maker's
/// viewing key and an input's nullifier, and the opening is accepted only if
/// it decodes as an option quote UTXO matching the slot's committed leaf and
/// the terms name this maker.
pub fn scan_maker_option_quote(
    tx: &ShieldedTransaction,
    keypair: &ShieldedKeypair,
    registry: &AssetRegistry,
) -> Result<Option<MakerOptionQuote>> {
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
        let Ok(quote) = resolve_mint(registry, plaintext.asset_id).and_then(|asset| {
            decode_owned::<circuits::OptionQuoteTerms>(
                tx,
                slot,
                slot_index,
                plaintext,
                asset,
                &maker_pubkey,
            )
        }) else {
            continue;
        };
        let (tree_id, leaf_index) = (quote.utxo.tree_id, quote.utxo.leaf_index);
        let Ok(quote_utxo) = OptionQuoteUtxo::from_data_utxo(quote, maker_address) else {
            continue;
        };
        return Ok(Some(MakerOptionQuote {
            quote_utxo,
            trader_viewing_pubkey,
            tree_id,
            leaf_index,
        }));
    }
    Ok(None)
}

/// Finds the maker's option quotes by the view tag of its authority PDA. A
/// quote a trader has since accepted or the maker cancelled is still
/// returned: spending it then fails.
pub fn index_maker_option_quotes<I: Rpc>(
    keypair: &ShieldedKeypair,
    registry: &AssetRegistry,
    indexer: &I,
    timeout: Duration,
) -> Result<Vec<MakerOptionQuote>> {
    let maker_pubkey = keypair
        .shielded_address()
        .and_then(|address| address.solana_address())
        .map_err(err)?;
    let tag = maker_authority(&maker_pubkey).confidential_view_tag();
    index_until(timeout, "maker option quotes", || {
        collect_tagged(tag, indexer, |tx| {
            scan_maker_option_quote(tx, keypair, registry)
        })
    })
}

/// Finds the open contracts against the maker's option quotes, leaving out
/// those a settle has since spent. The trader each names is resolved in the
/// user registry, and a contract whose trader is not registered there is
/// skipped: the maker could not address its payout.
pub fn index_maker_options<I: Rpc, R: Rpc>(
    keypair: &ShieldedKeypair,
    registry: &AssetRegistry,
    indexer: &I,
    rpc: &R,
    timeout: Duration,
) -> Result<Vec<MakerOption>> {
    let maker_address = keypair.shielded_address().map_err(err)?;
    let maker_pubkey = maker_address.solana_address().map_err(err)?;
    let authority = maker_authority(&maker_pubkey);
    let tag = authority.confidential_view_tag();
    index_until(timeout, "maker options", || {
        let transactions = collect_transactions(tag, indexer)?;
        let options = authority
            .decrypt_data_utxos::<circuits::OptionTerms>(
                &keypair.viewing_key,
                &transactions,
                registry,
            )
            .map_err(err)?;
        let spent = spent_among(
            indexer,
            options.iter().map(|option| option.utxo.nullifier).collect(),
        )?;
        let mut found = Vec::new();
        for option in options {
            if spent.contains(&option.utxo.nullifier) {
                continue;
            }
            let (_, trader_key, _) = option.data.trader.preimage();
            let Some(trader) = resolve_registered(rpc, Address::new_from_array(trader_key))? else {
                continue;
            };
            let (tree_id, leaf_index) = (option.utxo.tree_id, option.utxo.leaf_index);
            let Ok(option_utxo) = OptionUtxo::from_data_utxo(option, maker_address, trader) else {
                continue;
            };
            found.push(MakerOption {
                option_utxo,
                tree_id,
                leaf_index,
            });
        }
        Ok(found)
    })
}
