//! What the web app sees: one snapshot, shared by the actors' threads.
//!
//! Amounts are decimal strings, since they do not fit a JSON number: prices
//! in oracle ticks, sizes in size units, dollars in base units.

use std::{
    path::PathBuf,
    sync::{Mutex, MutexGuard},
};

use serde::{Deserialize, Serialize};

use crate::chain::now_ms;

const LEDGER_LENGTH: usize = 60;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MakerView {
    pub name: String,
    pub address: String,
    /// Distance of the quoted entry from the mark, in basis points.
    pub spread_bps: u64,
    /// The maker's margin as a multiple of the trader's: the profit cap.
    pub coverage: f64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteView {
    pub id: String,
    pub maker: MakerView,
    pub side: String,
    pub size: String,
    pub entry_price: String,
    pub trader_margin: String,
    pub maker_margin: String,
    pub maintenance_margin: String,
    /// Unix milliseconds after which the desk no longer offers the quote.
    pub expires_at: u64,
    pub signature: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionView {
    pub id: String,
    pub frame: u32,
    pub maker: MakerView,
    pub side: String,
    pub size: String,
    pub entry_price: String,
    pub trader_margin: String,
    /// Both margins.
    pub total: String,
    pub maintenance_margin: String,
    pub opened_at: u64,
    /// `open`, `closed` (the trader settled) or `liquidated` (the maker did).
    pub status: String,
    pub settle_price: Option<String>,
    pub payout: Option<String>,
    pub settled_at: Option<u64>,
    pub open_signature: String,
    pub settle_signature: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventView {
    pub id: String,
    pub at: u64,
    /// `quote`, `open`, `settle` or `reclaim`.
    pub kind: String,
    /// The wallet that signed.
    pub actor: String,
    pub counterparty: Option<String>,
    /// A settle reveals the oracle price it read, nothing else.
    pub price: Option<String>,
    /// What the transaction carries but nobody else can read.
    pub sealed: Vec<String>,
    pub signature: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Busy {
    pub kind: String,
    pub label: String,
    pub position_id: Option<String>,
    pub quote_id: Option<String>,
}

impl Busy {
    pub fn new(kind: &str, label: &str) -> Self {
        Self {
            kind: kind.into(),
            label: label.into(),
            position_id: None,
            quote_id: None,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Desk {
    /// False until every wallet is connected, registered and funded.
    pub ready: bool,
    pub network: String,
    pub program: String,
    pub mint: String,
    /// The trader's Solana address.
    pub wallet: String,
    /// The trader's sealed balance.
    pub balance: String,
    /// The oracle account's price and when Pyth published it (unix seconds).
    pub price: String,
    pub publish_time: i64,
    /// How old a price the program still settles at, in seconds.
    pub max_price_age: i64,
    pub makers: Vec<MakerView>,
    pub quotes: Vec<QuoteView>,
    pub positions: Vec<PositionView>,
    pub ledger: Vec<EventView>,
    pub busy: Option<Busy>,
    /// Why the last action failed, until the next one starts.
    pub error: Option<String>,
    /// Makers that have not yet answered the current request.
    #[serde(skip)]
    pub awaiting_quotes: usize,
    /// Quotes that are spent or reclaimed, by id, so a restart does not try
    /// to cancel them again.
    #[serde(skip)]
    pub closed_quotes: Vec<String>,
    /// Actors still connecting.
    #[serde(skip)]
    pub booting: usize,
    #[serde(skip)]
    pub sequence: u64,
}

/// What survives a restart. Open positions are matched again with what the
/// makers find on chain; the rest is history only this file holds.
#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Saved {
    positions: Vec<PositionView>,
    ledger: Vec<EventView>,
    closed_quotes: Vec<String>,
}

pub struct Shared {
    desk: Mutex<Desk>,
    file: PathBuf,
}

impl Shared {
    pub fn load(file: PathBuf, desk: Desk) -> Self {
        let saved: Saved = std::fs::read_to_string(&file)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        let sequence = saved.ledger.len() as u64 + saved.positions.len() as u64;
        Self {
            desk: Mutex::new(Desk {
                positions: saved.positions,
                ledger: saved.ledger,
                closed_quotes: saved.closed_quotes,
                sequence,
                ..desk
            }),
            file,
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, Desk> {
        self.desk
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn snapshot(&self) -> String {
        serde_json::to_string(&*self.lock()).unwrap_or_else(|_| "{}".into())
    }

    /// Changes the desk and writes what must survive a restart.
    pub fn update<T>(&self, change: impl FnOnce(&mut Desk) -> T) -> T {
        let mut desk = self.lock();
        let result = change(&mut desk);
        let saved = Saved {
            positions: desk.positions.clone(),
            ledger: desk.ledger.clone(),
            closed_quotes: desk.closed_quotes.clone(),
        };
        drop(desk);
        if let Some(parent) = self.file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(text) = serde_json::to_string(&saved) {
            let _ = std::fs::write(&self.file, text);
        }
        result
    }

    /// Ends the current action with `error` on show.
    pub fn fail(&self, error: &anyhow::Error) {
        eprintln!("desk: {error:#}");
        let mut desk = self.lock();
        desk.busy = None;
        desk.awaiting_quotes = 0;
        desk.error = Some(format!("{error:#}"));
    }
}

impl Desk {
    pub fn log(
        &mut self,
        kind: &str,
        actor: &str,
        counterparty: Option<&str>,
        price: Option<u64>,
        sealed: &[&str],
        signature: &str,
    ) {
        self.sequence += 1;
        self.ledger.insert(
            0,
            EventView {
                id: format!("tx-{}", self.sequence),
                at: now_ms(),
                kind: kind.into(),
                actor: actor.into(),
                counterparty: counterparty.map(Into::into),
                price: price.map(|price| price.to_string()),
                sealed: sealed.iter().map(|item| (*item).into()).collect(),
                signature: signature.into(),
            },
        );
        self.ledger.truncate(LEDGER_LENGTH);
    }

    /// One actor is connected, registered and funded.
    pub fn booted(&mut self) {
        self.booting = self.booting.saturating_sub(1);
        if self.booting == 0 {
            self.ready = true;
            if self.busy.as_ref().is_some_and(|busy| busy.kind == "boot") {
                self.busy = None;
            }
        }
    }

    /// One maker has answered, with a quote or without.
    pub fn quote_answered(&mut self) {
        self.awaiting_quotes = self.awaiting_quotes.saturating_sub(1);
        if self.awaiting_quotes == 0 && self.busy.as_ref().is_some_and(|busy| busy.kind == "quotes")
        {
            self.busy = None;
        }
    }
}
