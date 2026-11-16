//! A quote and what it opens into, whichever kind of deal it is: a perp, or
//! a dated contract (an option or a prediction).
//!
//! The two kinds share everything the desk does around them: a maker posts
//! a quote UTXO under its authority, a trader's browser proves the open and
//! later the close, and the desk assembles the instruction. What differs is
//! the terms inside and which of the program's instructions carries them,
//! and that is all these two enums hold.

use anyhow::Result;
use perps_sdk::state::{
    OptionQuoteUtxo, OptionUtxo, PositionUtxo, QuoteUtxo, KIND_ABOVE, KIND_BELOW, KIND_CALL,
    KIND_PUT,
};
use spr_keypair::ShieldedAddress;
use spr_transaction::WalletUtxo;

use crate::{
    chain::now_ms,
    desk::{MakerView, PositionView, QuoteView},
};

/// How the desk and the web app name a dated contract's kind.
pub fn kind_name(kind: u8) -> &'static str {
    match kind {
        KIND_CALL => "call",
        KIND_PUT => "put",
        KIND_ABOVE => "above",
        KIND_BELOW => "below",
        _ => "unknown",
    }
}

/// The kind a name stands for.
pub fn kind_named(name: &str) -> Option<u8> {
    [KIND_CALL, KIND_PUT, KIND_ABOVE, KIND_BELOW]
        .into_iter()
        .find(|kind| kind_name(*kind) == name)
}

/// Whether a contract gains when the price rises: a call, or a prediction
/// that it ends above. The web app draws these as it draws a long.
fn rises(kind: u8) -> bool {
    kind == KIND_CALL || kind == KIND_ABOVE
}

fn side(up: bool) -> String {
    if up { "long" } else { "short" }.into()
}

#[derive(Clone)]
pub enum AnyQuote {
    Perp(QuoteUtxo),
    Option(OptionQuoteUtxo),
}

impl AnyQuote {
    pub fn is_option(&self) -> bool {
        matches!(self, Self::Option(_))
    }

    pub fn hash(&self, tree_id: u16) -> Result<[u8; 32]> {
        match self {
            Self::Perp(quote) => quote.hash(tree_id),
            Self::Option(quote) => quote.hash(tree_id),
        }
    }

    pub fn to_input_utxo(&self, tree_id: u16, leaf: u64) -> Result<WalletUtxo> {
        match self {
            Self::Perp(quote) => quote.to_input_utxo(tree_id, leaf),
            Self::Option(quote) => quote.to_input_utxo(tree_id, leaf),
        }
    }

    pub fn maker(&self) -> &ShieldedAddress {
        match self {
            Self::Perp(quote) => &quote.terms.maker,
            Self::Option(quote) => &quote.terms.maker,
        }
    }

    pub fn feed_id(&self) -> &[u8; 32] {
        match self {
            Self::Perp(quote) => &quote.terms.feed_id,
            Self::Option(quote) => &quote.terms.feed_id,
        }
    }

    /// Unix seconds until which the quote can be opened, on chain.
    pub fn expiry(&self) -> u64 {
        match self {
            Self::Perp(quote) => quote.terms.expiry,
            Self::Option(quote) => quote.terms.expiry,
        }
    }

    /// What `trader` opens with the transaction whose first nullifier is
    /// `first_nullifier`: both circuits fix it to the quote's opening.
    pub fn derived(
        &self,
        trader: ShieldedAddress,
        first_nullifier: &[u8; 32],
    ) -> Result<AnyPosition> {
        Ok(match self {
            Self::Perp(quote) => {
                AnyPosition::Perp(quote.derived_position(trader, first_nullifier)?)
            }
            Self::Option(quote) => {
                AnyPosition::Option(quote.derived_option(trader, first_nullifier)?)
            }
        })
    }

    /// The quote as the web app lists it. A dated contract uses the perp's
    /// fields for what corresponds: its strike as the entry price, its
    /// premium as the trader's margin, its collateral as the maker's.
    pub fn view(
        &self,
        id: String,
        market: &str,
        maker: MakerView,
        expires_at: u64,
        signature: &str,
    ) -> QuoteView {
        match self {
            Self::Perp(quote) => QuoteView {
                id,
                market: market.into(),
                maker,
                kind: "perp".into(),
                side: side(quote.terms.trader_is_long),
                size: quote.terms.size.to_string(),
                entry_price: quote.terms.entry_price.to_string(),
                trader_margin: quote.terms.trader_margin.to_string(),
                maker_margin: quote.maker_margin.to_string(),
                maintenance_margin: quote.terms.maintenance_margin.to_string(),
                term_end: None,
                expires_at,
                signature: signature.into(),
            },
            Self::Option(quote) => QuoteView {
                id,
                market: market.into(),
                maker,
                kind: kind_name(quote.terms.kind).into(),
                side: side(rises(quote.terms.kind)),
                size: quote.terms.size.to_string(),
                entry_price: quote.terms.strike.to_string(),
                trader_margin: quote.terms.premium.to_string(),
                maker_margin: quote.collateral.to_string(),
                maintenance_margin: "0".into(),
                term_end: Some(quote.terms.term_end),
                expires_at,
                signature: signature.into(),
            },
        }
    }
}

#[derive(Clone)]
pub enum AnyPosition {
    Perp(PositionUtxo),
    Option(OptionUtxo),
}

impl AnyPosition {
    pub fn is_option(&self) -> bool {
        matches!(self, Self::Option(_))
    }

    pub fn hash(&self, tree_id: u16) -> Result<[u8; 32]> {
        match self {
            Self::Perp(position) => position.hash(tree_id),
            Self::Option(option) => option.hash(tree_id),
        }
    }

    pub fn to_input_utxo(&self, tree_id: u16, leaf: u64) -> Result<WalletUtxo> {
        match self {
            Self::Perp(position) => position.to_input_utxo(tree_id, leaf),
            Self::Option(option) => option.to_input_utxo(tree_id, leaf),
        }
    }

    pub fn maker(&self) -> &ShieldedAddress {
        match self {
            Self::Perp(position) => &position.terms.maker,
            Self::Option(option) => &option.terms.maker,
        }
    }

    pub fn trader(&self) -> &ShieldedAddress {
        match self {
            Self::Perp(position) => &position.terms.trader,
            Self::Option(option) => &option.terms.trader,
        }
    }

    pub fn feed_id(&self) -> &[u8; 32] {
        match self {
            Self::Perp(position) => &position.terms.feed_id,
            Self::Option(option) => &option.terms.feed_id,
        }
    }

    /// What the trader receives if it settles at `price`.
    pub fn trader_payout(&self, price: u64) -> u64 {
        match self {
            Self::Perp(position) => position.trader_payout(price),
            Self::Option(option) => option.trader_payout(price),
        }
    }

    /// What nobody but the two sides can read of an open, for the ledger.
    pub fn sealed(&self) -> &'static [&'static str] {
        match self {
            Self::Perp(_) => &[
                "size",
                "direction",
                "entry price",
                "both margins",
                "liquidation price",
            ],
            Self::Option(_) => &[
                "what kind of contract",
                "strike",
                "when it ends",
                "premium",
                "what it can pay",
            ],
        }
    }

    /// The position as the web app lists it, open since now. A dated
    /// contract uses the perp's fields as its quote does.
    pub fn view(
        &self,
        id: String,
        market: &str,
        maker: MakerView,
        open_signature: &str,
    ) -> PositionView {
        let (kind, up, size, entry_price, trader_margin, total, maintenance, term_end) = match self
        {
            Self::Perp(position) => (
                "perp",
                position.terms.trader_is_long,
                position.terms.size,
                position.terms.entry_price,
                position.terms.trader_margin,
                position.total,
                position.terms.maintenance_margin,
                None,
            ),
            Self::Option(option) => (
                kind_name(option.terms.kind),
                rises(option.terms.kind),
                option.terms.size,
                option.terms.strike,
                option.terms.premium,
                option.total,
                0,
                Some(option.terms.term_end),
            ),
        };
        PositionView {
            id,
            frame: 0,
            market: market.into(),
            maker,
            kind: kind.into(),
            side: side(up),
            size: size.to_string(),
            entry_price: entry_price.to_string(),
            trader_margin: trader_margin.to_string(),
            total: total.to_string(),
            maintenance_margin: maintenance.to_string(),
            term_end,
            opened_at: now_ms(),
            status: "open".into(),
            settle_price: None,
            payout: None,
            settled_at: None,
            open_signature: open_signature.into(),
            settle_signature: None,
        }
    }
}
