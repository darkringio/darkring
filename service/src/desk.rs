//! What the web app sees, shared by the actors' threads: the market, which
//! is the same for everyone, and one book per trading account, which only
//! that account's sessions can read.
//!
//! Amounts are decimal strings, since they do not fit a JSON number: prices
//! in oracle ticks, sizes in size units, dollars in base units.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Mutex, MutexGuard},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use spr_keypair::ShieldedAddress;

use crate::chain::now_ms;

/// A session nobody has used for this long is closed.
const SESSION_IDLE: Duration = Duration::from_secs(2 * 60 * 60);

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

/// A market and its mark price.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketView {
    pub symbol: String,
    pub name: String,
    /// The Pyth feed id, in hex: a browser checks a quote and a position
    /// against it before it proves.
    pub feed: String,
    /// The feed's exponent: a price tick is ten to this power, in dollars.
    pub expo: i32,
    /// The oracle account's price and when Pyth published it (unix seconds).
    pub price: String,
    pub publish_time: i64,
    pub oracle: String,
}

fn sol() -> String {
    "SOL".into()
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteView {
    pub id: String,
    pub market: String,
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
    /// The market's symbol. Positions recorded before there was more than
    /// one market are SOL.
    #[serde(default = "sol")]
    pub market: String,
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
    /// A settle reveals the market and the oracle price it read, nothing
    /// else.
    #[serde(default)]
    pub market: Option<String>,
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

/// A transaction the account's browser has to sign: the desk assembles and
/// sends it, but the key that signs never leaves the browser.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SignRequest {
    pub id: u64,
    /// The transaction message, in hex.
    pub message: String,
}

/// One trading account's side of the desk. The account's balance is not
/// here: only its own browser can read its notes.
#[derive(Default)]
pub struct Book {
    pub quotes: Vec<QuoteView>,
    pub positions: Vec<PositionView>,
    pub ledger: Vec<EventView>,
    pub busy: Option<Busy>,
    /// Why the last action failed, until the next one starts.
    pub error: Option<String>,
    pub sign: Option<SignRequest>,
    /// The browser's answer to `sign`, until the signer collects it.
    signed: Option<(u64, [u8; 64])>,
    /// Makers that have not yet answered the current request.
    pub awaiting_quotes: usize,
    /// The opening of each live quote and each open position, by id, for
    /// the account's browser to prove with: see `opening.rs`. Held in
    /// memory only.
    pub openings: HashMap<String, Value>,
    /// Whether the faucet has paid this account its first test dollars.
    pub funded: bool,
    sequence: u64,
}

struct Session {
    account: String,
    address: ShieldedAddress,
    seen: Instant,
}

#[derive(Default)]
pub struct Desk {
    /// False until every maker is connected, registered and funded.
    pub ready: bool,
    pub network: String,
    pub program: String,
    /// The test dollar, and the id the pool knows it by.
    pub mint: String,
    pub asset_id: u64,
    pub markets: Vec<MarketView>,
    /// How old a price the program still settles at, in seconds.
    pub max_price_age: i64,
    pub makers: Vec<MakerView>,
    /// The books, by the account's Solana address.
    pub books: HashMap<String, Book>,
    /// Quotes that are spent or reclaimed, by id, so a restart does not try
    /// to cancel them again.
    pub closed_quotes: Vec<String>,
    /// Actors still connecting.
    pub booting: usize,
    sessions: HashMap<String, Session>,
    next_sign: u64,
}

/// What survives a restart. Open positions are matched again with what the
/// makers find on chain; the rest is history only this file holds. No key
/// is ever written here.
#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Saved {
    books: HashMap<String, SavedBook>,
    closed_quotes: Vec<String>,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct SavedBook {
    positions: Vec<PositionView>,
    ledger: Vec<EventView>,
    funded: bool,
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
        let books = saved
            .books
            .into_iter()
            .map(|(account, book)| {
                let sequence = book.ledger.len() as u64 + book.positions.len() as u64;
                let book = Book {
                    // An account with history was funded before this was
                    // recorded.
                    funded: book.funded || sequence > 0,
                    positions: book.positions,
                    ledger: book.ledger,
                    sequence,
                    ..Book::default()
                };
                (account, book)
            })
            .collect();
        Self {
            desk: Mutex::new(Desk {
                books,
                closed_quotes: saved.closed_quotes,
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

    /// The market, and `account`'s book when there is one.
    pub fn snapshot(&self, account: Option<&str>) -> Value {
        let desk = self.lock();
        let empty = Book::default();
        let book = account
            .and_then(|account| desk.books.get(account))
            .unwrap_or(&empty);
        json!({
            "ready": desk.ready,
            "network": desk.network,
            "program": desk.program,
            "mint": desk.mint,
            "assetId": desk.asset_id.to_string(),
            "markets": desk.markets,
            "maxPriceAge": desk.max_price_age,
            "makers": desk.makers,
            "session": account.is_some(),
            "wallet": account.unwrap_or(""),
            "quotes": book.quotes,
            "awaiting": book.awaiting_quotes,
            "openings": book.openings,
            "positions": book.positions,
            "ledger": book.ledger,
            "busy": book.busy,
            "error": book.error,
            "sign": book.sign,
        })
    }

    /// Changes the desk and writes what must survive a restart.
    pub fn update<T>(&self, change: impl FnOnce(&mut Desk) -> T) -> T {
        let mut desk = self.lock();
        let result = change(&mut desk);
        let saved = Saved {
            books: desk
                .books
                .iter()
                .filter(|(_, book)| {
                    book.funded || !book.positions.is_empty() || !book.ledger.is_empty()
                })
                .map(|(account, book)| {
                    let book = SavedBook {
                        positions: book.positions.clone(),
                        ledger: book.ledger.clone(),
                        funded: book.funded,
                    };
                    (account.clone(), book)
                })
                .collect(),
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

    /// Ends `account`'s current action with `error` on show.
    pub fn fail(&self, account: &str, error: &anyhow::Error) {
        eprintln!("desk: {error:#}");
        let mut desk = self.lock();
        let book = desk.book(account);
        book.busy = None;
        book.sign = None;
        book.awaiting_quotes = 0;
        book.error = Some(format!("{error:#}"));
    }

    /// Shows `label` on `account`'s current action.
    pub fn label(&self, account: &str, label: &str) {
        if let Some(busy) = self.lock().book(account).busy.as_mut() {
            busy.label = label.into();
        }
    }

    /// Asks `account`'s browser to sign `message`, and returns the request's
    /// id.
    pub fn request_signature(&self, account: &str, message: &[u8]) -> u64 {
        let mut desk = self.lock();
        desk.next_sign += 1;
        let id = desk.next_sign;
        let book = desk.book(account);
        book.signed = None;
        book.sign = Some(SignRequest {
            id,
            message: hex(message),
        });
        id
    }

    /// The browser's signature for request `id`, once it has arrived.
    pub fn signature(&self, account: &str, id: u64) -> Option<[u8; 64]> {
        let mut desk = self.lock();
        let book = desk.book(account);
        match book.signed {
            Some((signed, signature)) if signed == id => {
                book.signed = None;
                book.sign = None;
                Some(signature)
            }
            _ => None,
        }
    }

    /// Stops waiting for request `id`.
    pub fn withdraw_request(&self, account: &str, id: u64) {
        let mut desk = self.lock();
        let book = desk.book(account);
        if book.sign.as_ref().is_some_and(|sign| sign.id == id) {
            book.sign = None;
        }
    }

    /// The browser's answer to request `id`. False when nothing is waiting
    /// for it any more.
    pub fn signed(&self, account: &str, id: u64, signature: [u8; 64]) -> bool {
        let mut desk = self.lock();
        let book = desk.book(account);
        if !book.sign.as_ref().is_some_and(|sign| sign.id == id) {
            return false;
        }
        book.signed = Some((id, signature));
        true
    }
}

impl Desk {
    pub fn book(&mut self, account: &str) -> &mut Book {
        self.books.entry(account.into()).or_default()
    }

    /// One actor is connected, registered and funded.
    pub fn booted(&mut self) {
        self.booting = self.booting.saturating_sub(1);
        if self.booting == 0 {
            self.ready = true;
        }
    }

    pub fn open_session(&mut self, token: String, account: &str, address: ShieldedAddress) {
        self.sessions.insert(
            token,
            Session {
                account: account.into(),
                address,
                seen: Instant::now(),
            },
        );
    }

    /// The account `token` speaks for and its address, if the session is
    /// still open.
    pub fn session(&mut self, token: &str) -> Option<(String, ShieldedAddress)> {
        let session = self.sessions.get_mut(token)?;
        session.seen = Instant::now();
        Some((session.account.clone(), session.address))
    }

    pub fn close_session(&mut self, token: &str) {
        self.sessions.remove(token);
    }

    /// Closes the sessions nobody has used for a while.
    pub fn expire_sessions(&mut self) {
        self.sessions
            .retain(|_, session| session.seen.elapsed() < SESSION_IDLE);
    }

    /// Whether any session still speaks for `account`.
    pub fn has_session(&self, account: &str) -> bool {
        self.sessions
            .values()
            .any(|session| session.account == account)
    }
}

impl Book {
    pub fn log(
        &mut self,
        kind: &str,
        actor: &str,
        counterparty: Option<&str>,
        settled: Option<(&str, u64)>,
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
                market: settled.map(|(market, _)| market.into()),
                price: settled.map(|(_, price)| price.to_string()),
                sealed: sealed.iter().map(|item| (*item).into()).collect(),
                signature: signature.into(),
            },
        );
        self.ledger.truncate(LEDGER_LENGTH);
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

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The bytes of a hex string of exactly `N` of them.
pub fn unhex<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != N * 2 || !text.is_ascii() {
        return None;
    }
    let mut bytes = [0u8; N];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(bytes)
}

/// The bytes of a hex string of any even length.
pub fn unhex_vec(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    (0..text.len() / 2)
        .map(|index| u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok())
        .collect()
}
