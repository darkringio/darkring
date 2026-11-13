//! What a maker hands a trader's browser so it can spend a quote or a
//! position: the UTXO's opening, as JSON.
//!
//! A quote and a position are both owned by the maker's authority, and the
//! maker knows every value in them: it wrote the quote, and the position
//! derives from it. The trader's browser needs the same values to prove an
//! open or a close. The desk gives them to the trader's own session and to
//! nobody else. The browser checks them against the UTXO's commitment
//! before it uses them, and a proof built on values that are not on chain
//! does not verify.

use serde_json::{json, Value};
use spr_keypair::ShieldedAddress;
use spr_transaction::{DataRecord, WalletUtxo};

use crate::desk::hex;

/// `input` in the shape the browser's program package takes a UTXO in, and
/// the shielded address of the `maker` whose authority owns it: the open
/// and the close encrypt the maker's outputs to its viewing key. Bytes are
/// hex, integers that do not fit a JSON number are strings.
pub fn opening(input: &WalletUtxo, maker: &ShieldedAddress) -> Value {
    let records: Vec<Value> = input
        .utxo
        .data
        .records
        .iter()
        .map(|record| {
            let (kind, bytes) = match record {
                DataRecord::RingData(bytes) => ("ringData", bytes),
                DataRecord::UtxoData(bytes) => ("utxoData", bytes),
                DataRecord::Memo(bytes) => ("memo", bytes),
            };
            json!({ "kind": kind, "bytes": hex(bytes) })
        })
        .collect();
    json!({
        "owner": hex(input.utxo.owner.as_bytes()),
        "mint": input.utxo.asset.asset.to_string(),
        "assetId": input.utxo.asset.asset_id.to_string(),
        "amount": input.utxo.amount.to_string(),
        "blinding": hex(&input.utxo.blinding),
        "records": records,
        "nullifierPubkey": hex(&input.nullifier_pubkey),
        "hash": hex(&input.utxo_hash),
        "nullifier": hex(&input.nullifier),
        "treeId": input.tree_id,
        "leaf": input.leaf_index.to_string(),
        "maker": hex(&maker.to_bytes()),
    })
}
