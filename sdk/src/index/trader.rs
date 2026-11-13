use std::time::Duration;

use anyhow::{bail, Result};
use borsh::BorshDeserialize;
use perps_program::circuits;
use solana_address::Address;
use spr_client::Rpc;
use spr_keypair::{ShieldedAddress, ShieldedKeypair};
use spr_program::DataUtxo;
use spr_transaction::{
    serialization::confidential::Confidential, AssetRegistry, DecodeCx, ShieldedTransaction,
    UtxoSerialization,
};

use super::{
    poll::{collect_tagged, index_until},
    scan::{decode_quote, resolve_mint, resolve_registered, unified_slots},
};
use crate::{
    err,
    shared::INDEXED_TREE_ID,
    state::{check_trader_identity, QuoteUtxo},
    QuoteMarkerData,
};

/// A quote a trader discovered, addressed to it by a maker.
#[derive(Debug)]
pub struct TraderQuote {
    pub quote_utxo: QuoteUtxo,
    pub maker_pubkey: Address,
    /// The quote UTXO's position in its tree.
    pub tree_id: u16,
    pub leaf_index: u64,
}

/// A quote decrypted from a quote transaction, before the maker it names is
/// checked against the user registry.
pub struct TraderQuoteCandidate {
    /// The decoded quote UTXO, whose state the SDK checked against the
    /// committed leaf.
    pub quote: DataUtxo<circuits::QuoteTerms>,
    pub maker_pubkey: Address,
    pub quote_utxo_hash: [u8; 32],
}

/// The quote the marker tagged to `keypair`'s trader names in `tx`, or `None`
/// when `tx` has no such marker or does not decode as a quote for it.
///
/// Rule: an attacker-controlled field never aborts the scan. Anyone can post
/// a marker and a quote ciphertext under the trader's view tag, so everything
/// read before the quote UTXO hash is checked against the committed leaf,
/// including the asset id resolved in `registry`, skips the transaction when
/// it does not decode. Only errors an attacker cannot produce propagate.
pub fn scan_trader(
    tx: &ShieldedTransaction,
    keypair: &ShieldedKeypair,
    registry: &AssetRegistry,
) -> Result<Option<TraderQuoteCandidate>> {
    let trader_tag = keypair
        .signing_pubkey()
        .confidential_view_tag()
        .map_err(err)?;
    Ok(tx
        .messages
        .iter()
        .find(|message| message.view_tag == trader_tag)
        .and_then(|marker_message| decode_candidate(tx, keypair, &marker_message.data, registry)))
}

/// Decodes the quote `marker_data` names in `tx`, or `None` when any of it,
/// all attacker-controlled, does not decode.
fn decode_candidate(
    tx: &ShieldedTransaction,
    keypair: &ShieldedKeypair,
    marker_data: &[u8],
    registry: &AssetRegistry,
) -> Option<TraderQuoteCandidate> {
    let marker = QuoteMarkerData::try_from_slice(marker_data).ok()?;
    let (slot_index, slot, body) = unified_slots(tx)
        .find(|(_, slot, _)| slot.output_context.hash == marker.quote_utxo_hash)?;
    let cx = DecodeCx::for_slot(&keypair.viewing_key, tx, slot_index);
    let plaintext = Confidential::decode(&body, &cx).ok()?;
    let asset = resolve_mint(registry, plaintext.asset_id).ok()?;
    // The client writes the maker signer into the marker and the program
    // checks it; the quote UTXO is owned by that maker's authority.
    let maker_pubkey = Address::new_from_array(marker.maker_pubkey);
    let quote = decode_quote(tx, slot, slot_index, plaintext, asset, &maker_pubkey).ok()?;
    // The circuit reads a non-canonical designated trader as another trader,
    // so the quote is refused before its maker is resolved in the registry.
    check_trader_identity(&quote.data.taker_pk_fe).ok()?;
    Some(TraderQuoteCandidate {
        quote,
        maker_pubkey,
        quote_utxo_hash: marker.quote_utxo_hash,
    })
}

impl TraderQuoteCandidate {
    /// Accepts the quote if the maker it names is `registered_maker`, the
    /// address the user registry holds for the marker's maker pubkey, and the
    /// opening recomputes the quote UTXO hash the marker names.
    pub fn into_quote(self, registered_maker: ShieldedAddress) -> Result<TraderQuote> {
        let tree_id = self.quote.utxo.tree_id;
        let leaf_index = self.quote.utxo.leaf_index;
        let quote_utxo = QuoteUtxo::from_data_utxo(self.quote, registered_maker)?;
        if quote_utxo.hash(INDEXED_TREE_ID)? != self.quote_utxo_hash {
            bail!("reconstructed quote utxo hash does not match the committed leaf");
        }
        Ok(TraderQuote {
            quote_utxo,
            maker_pubkey: self.maker_pubkey,
            tree_id,
            leaf_index,
        })
    }
}

/// Finds the quotes addressed to `keypair`'s trader, by the view tag of its
/// signing key, and checks each quote's maker against the user registry.
pub fn index_trader_quotes<I: Rpc, R: Rpc>(
    keypair: &ShieldedKeypair,
    registry: &AssetRegistry,
    indexer: &I,
    rpc: &R,
    timeout: Duration,
) -> Result<Vec<TraderQuote>> {
    let owner_tag = keypair
        .signing_pubkey()
        .confidential_view_tag()
        .map_err(err)?;
    index_until(timeout, "quotes for the trader", || {
        collect_tagged(owner_tag, indexer, |tx| {
            let Some(candidate) = scan_trader(tx, keypair, registry)? else {
                return Ok(None);
            };
            // A maker missing from the registry skips the transaction; RPC
            // errors propagate.
            let Some(maker) = resolve_registered(rpc, candidate.maker_pubkey)? else {
                return Ok(None);
            };
            // Same rule as `scan_trader`: a quote naming another maker or an
            // opening off the committed leaf is attacker-controlled and
            // skips.
            Ok(candidate.into_quote(maker).ok())
        })
    })
}
