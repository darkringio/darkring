//! A maker: answers quote requests with a sealed quote on chain, takes back
//! the margin of quotes nobody opened, and settles a position the moment the
//! oracle price makes it liquidatable.

use std::{
    collections::HashSet,
    sync::{
        mpsc::{Receiver, RecvTimeoutError, Sender},
        Arc,
    },
    thread::sleep,
    time::Duration,
};

use anyhow::{anyhow, Result};
use perps_sdk::{
    index::{index_maker_positions, index_maker_quotes},
    instructions::{
        cancel_quote::CancelQuoteTransactionParams, quote::QuoteTransactionParams,
        settle::SettleTransactionParams,
    },
    shared::identity,
    state::{PositionUtxo, QuoteTerms, QuoteUtxo},
    SIZE_SCALE,
};
use solana_keypair::Keypair;
use spr_keypair::ShieldedAddress;

use crate::{
    chain::{
        market_of, now, now_ms, solana_keypair, Actor, Chain, Config, Laps, Mark, Market, USDC,
    },
    desk::{MakerView, PositionView, Shared},
    trader::TraderJob,
    usage,
};

/// Seconds a quote can be opened for, on chain. The desk shows less: an
/// open has to be proved and land before this runs out.
pub const QUOTE_LIFETIME_SECS: i64 = 150;
/// A maker whose largest note is below this is topped up when it starts.
const FLOAT_FLOOR: u64 = 3_000 * USDC;
const FLOAT: u64 = 20_000 * USDC;
const TICK: Duration = Duration::from_secs(4);
const CANCEL_ATTEMPTS: u8 = 3;

pub struct MakerSpec {
    pub index: usize,
    pub name: &'static str,
    pub file: &'static str,
    pub spread_bps: u64,
    /// The maker's margin per ten of the trader's.
    pub coverage_tenths: u64,
}

pub enum MakerJob {
    /// `account` asked for a quote; `trader` is its address.
    Quote {
        account: String,
        market: &'static Market,
        trader: ShieldedAddress,
        long: bool,
        size: u64,
        leverage: u64,
    },
    /// `account` has a session again: tell it what it has open here.
    Resend { account: String },
    /// The trader opened this maker's quote.
    Opened {
        account: String,
        quote_id: String,
        position_id: String,
        position: Box<PositionUtxo>,
        leaf: u64,
    },
    /// The trader settled.
    Closed { position_id: String },
}

/// The first eight bytes of a UTXO hash, in hex: how the desk names a quote
/// or a position.
pub fn id_of(hash: &[u8; 32]) -> String {
    hash[..8].iter().map(|byte| format!("{byte:02x}")).collect()
}

struct Pending {
    id: String,
    /// The account the quote was for, when this run posted it.
    account: Option<String>,
    quote: QuoteUtxo,
    leaf: u64,
    attempts: u8,
}

struct Held {
    id: String,
    /// The trader's Solana address: the account whose position this is.
    account: String,
    position: PositionUtxo,
    leaf: u64,
}

/// The account a position belongs to: its trader's Solana address.
fn account_of(position: &PositionUtxo) -> Result<String> {
    Ok(position.terms.trader.solana_address()?.to_string())
}

struct Maker {
    spec: MakerSpec,
    view: MakerView,
    chain: Chain,
    actor: Actor,
    shared: Arc<Shared>,
    trader: Sender<TraderJob>,
    pending: Vec<Pending>,
    held: Vec<Held>,
    /// The notes this maker has spent since it last read them, by hash. The
    /// wallet still lists them, so a quote must not pick one again.
    ///
    /// Reading the notes costs several indexer requests, and nothing but
    /// this maker's own quotes spends them. So they are read at the start
    /// and then only when no note the maker knows of, and has not used,
    /// covers a quote: not on a timer, and not before every quote.
    used: HashSet<[u8; 32]>,
}

pub fn run(
    config: Config,
    spec: MakerSpec,
    view: MakerView,
    shared: Arc<Shared>,
    jobs: Receiver<MakerJob>,
    trader: Sender<TraderJob>,
) {
    let name = spec.name;
    let mut maker = loop {
        match Maker::boot(&config, &spec, &view, &shared, &trader) {
            Ok(maker) => break maker,
            Err(error) => {
                eprintln!(
                    "maker {name}: {}; trying again",
                    usage::scrub(&format!("{error:#}"))
                );
                usage::note(&error);
                sleep(Duration::from_secs(15).max(usage::rest_left()));
            }
        }
    };
    maker.spec = spec;
    // Connecting took answers from the provider: any rest is over.
    usage::recovered();
    shared.lock().booted();
    println!("maker {name}: ready");
    loop {
        match jobs.recv_timeout(TICK) {
            Ok(MakerJob::Quote {
                account,
                market,
                trader,
                long,
                size,
                leverage,
            }) => {
                if let Err(error) = maker.quote(&account, market, trader, long, size, leverage) {
                    eprintln!(
                        "maker {name}: no quote: {}",
                        usage::scrub(&format!("{error:#}"))
                    );
                    usage::note(&error);
                    let _ = maker.trader.send(TraderJob::NoQuote {
                        account,
                        maker: name.into(),
                        reason: format!("{error:#}"),
                    });
                }
            }
            Ok(MakerJob::Resend { account }) => maker.resend(&account),
            Ok(MakerJob::Opened {
                account,
                quote_id,
                position_id,
                position,
                leaf,
            }) => {
                maker.pending.retain(|pending| pending.id != quote_id);
                maker.held.push(Held {
                    id: position_id,
                    account,
                    position: *position,
                    leaf,
                });
            }
            Ok(MakerJob::Closed { position_id }) => {
                maker.held.retain(|held| held.id != position_id);
            }
            Err(RecvTimeoutError::Timeout) => maker.tick(),
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

impl Maker {
    fn boot(
        config: &Config,
        spec: &MakerSpec,
        view: &MakerView,
        shared: &Arc<Shared>,
        trader: &Sender<TraderJob>,
    ) -> Result<Self> {
        let chain = Chain::connect(config)?;
        let mut actor = Actor::load(config, &chain, spec.file)?;
        actor.sync(&chain)?;
        if actor.note(&chain, FLOAT_FLOOR, &HashSet::new()).is_err() {
            let deployer: Keypair = solana_keypair(config, "devnet-deployer.json")?;
            chain.shield(&deployer, &actor.keypair.shielded_address()?, FLOAT)?;
            actor.sync(&chain)?;
        }
        let mut maker = Self {
            spec: MakerSpec {
                index: spec.index,
                name: spec.name,
                file: spec.file,
                spread_bps: spec.spread_bps,
                coverage_tenths: spec.coverage_tenths,
            },
            view: view.clone(),
            chain,
            actor,
            shared: Arc::clone(shared),
            trader: trader.clone(),
            pending: Vec::new(),
            held: Vec::new(),
            used: HashSet::new(),
        };
        maker.recover()?;
        Ok(maker)
    }

    /// Finds what this maker left on chain before a restart: positions still
    /// open against it, and quotes whose margin it has yet to take back.
    fn recover(&mut self) -> Result<()> {
        let client = &self.chain.client;
        // A zero timeout is one pass; an empty pass is reported as a timeout.
        let positions = index_maker_positions(
            &self.actor.keypair,
            &self.actor.wallet.registry,
            client.indexer(),
            client.rpc(),
            Duration::ZERO,
        )
        .unwrap_or_default();
        let mut open = Vec::new();
        for found in positions {
            let id = id_of(&found.position_utxo.hash(self.chain.tree_id)?);
            let terms = &found.position_utxo.terms;
            let account = account_of(&found.position_utxo)?;
            let Ok(market) = market_of(&terms.feed_id) else {
                continue;
            };
            let view = PositionView {
                id: id.clone(),
                frame: 0,
                market: market.symbol.into(),
                maker: self.view.clone(),
                side: if terms.trader_is_long {
                    "long"
                } else {
                    "short"
                }
                .into(),
                size: terms.size.to_string(),
                entry_price: terms.entry_price.to_string(),
                trader_margin: terms.trader_margin.to_string(),
                total: found.position_utxo.total.to_string(),
                maintenance_margin: terms.maintenance_margin.to_string(),
                opened_at: now_ms(),
                status: "open".into(),
                settle_price: None,
                payout: None,
                settled_at: None,
                open_signature: String::new(),
                settle_signature: None,
            };
            self.shared.update(|desk| {
                let book = desk.book(&account);
                if !book.positions.iter().any(|position| position.id == id) {
                    let frame = book.positions.len() as u32 + 1;
                    book.positions.push(PositionView { frame, ..view });
                }
            });
            open.push(id.clone());
            self.held.push(Held {
                id,
                account,
                position: found.position_utxo,
                leaf: found.leaf_index,
            });
        }
        // A position the file calls open that is no longer on chain was
        // settled while nobody was recording.
        let address = self.view.address.clone();
        self.shared.update(|desk| {
            for book in desk.books.values_mut() {
                book.positions.retain(|position| {
                    position.status != "open"
                        || position.maker.address != address
                        || open.contains(&position.id)
                });
            }
        });

        let quotes = index_maker_quotes(
            &self.actor.keypair,
            &self.actor.wallet.registry,
            client.indexer(),
            Duration::ZERO,
        )
        .unwrap_or_default();
        let closed = self.shared.lock().closed_quotes.clone();
        for found in quotes {
            let id = id_of(&found.quote_utxo.hash(self.chain.tree_id)?);
            if closed.contains(&id) {
                continue;
            }
            self.pending.push(Pending {
                id,
                account: None,
                quote: found.quote_utxo,
                leaf: found.leaf_index,
                // Most of these were opened long ago: one try each.
                attempts: CANCEL_ATTEMPTS - 1,
            });
        }
        Ok(())
    }

    /// Tells `account`, which has just opened a session, about the positions
    /// it has open against this maker.
    fn resend(&self, account: &str) {
        for held in self.held.iter().filter(|held| held.account == account) {
            let _ = self.trader.send(TraderJob::Recovered {
                account: account.into(),
                position_id: held.id.clone(),
                position: Box::new(held.position.clone()),
                leaf: held.leaf,
                maker: self.spec.index,
            });
        }
    }

    /// Posts a quote for `trader` at the oracle price, moved by this maker's
    /// spread, and hands its opening to the trader's account.
    fn quote(
        &mut self,
        account: &str,
        market: &Market,
        trader: ShieldedAddress,
        long: bool,
        size: u64,
        leverage: u64,
    ) -> Result<()> {
        let mark = self.chain.mark(market)?.price;
        let spread = mark / 10_000 * self.spec.spread_bps;
        let entry_price = if long { mark + spread } else { mark - spread };
        let notional = u64::try_from(size as u128 * entry_price as u128 / SIZE_SCALE as u128)?;
        let trader_margin = notional / leverage.max(1);
        let maker_margin = trader_margin / 10 * self.spec.coverage_tenths;
        anyhow::ensure!(
            trader_margin > 0 && maker_margin > 0,
            "the size is too small"
        );

        let mut lap = Laps::start();
        let asked = usage::indexer_requests();
        let source = match self.actor.note(&self.chain, maker_margin, &self.used) {
            Ok(source) => source,
            // Every note this maker knows of is used or too small: the
            // change from its earlier quotes, and what closed positions
            // paid it, are notes it has not read yet.
            Err(_) => {
                self.refresh()?;
                self.actor.note(&self.chain, maker_margin, &self.used)?
            }
        };
        let spending = source.utxo_hash;
        lap.mark("notes");
        let terms = QuoteTerms {
            feed_id: market.feed,
            trader_is_long: long,
            size,
            entry_price,
            trader_margin,
            maintenance_margin: trader_margin / 5,
            maker: self.actor.keypair.shielded_address()?,
            expiry: (now() + QUOTE_LIFETIME_SECS) as u64,
            trader: Some(identity(&trader.solana_address()?)?),
        };
        let transaction = QuoteTransactionParams {
            terms,
            maker_margin,
            source,
            trader_address: trader,
            payer: self.actor.pubkey,
        }
        .build(&self.actor.keypair)?;
        let posted = transaction.quote_utxo.clone();
        let client = &self.chain.client;
        let instruction = transaction
            .prove(client.indexer(), self.chain.tree, &self.actor.keypair)?
            .instruction()?;
        lap.mark("prove");
        let signature = self.chain.send(&self.actor.keypair, instruction)?;
        self.used.insert(spending);
        lap.mark("send");
        let hash = posted.hash(self.chain.tree_id)?;
        let leaf = self.chain.leaf(hash)?;
        lap.mark("leaf");
        println!(
            "maker {}: quoted in {lap}, with {} indexer requests",
            self.spec.name,
            usage::indexer_requests() - asked
        );
        let id = id_of(&hash);
        self.shared.update(|desk| {
            desk.book(account).log(
                "quote",
                &self.view.address,
                None,
                None,
                &["who it is for", "every term", "margin locked"],
                &signature.to_string(),
            );
        });
        self.pending.push(Pending {
            id,
            account: Some(account.into()),
            quote: posted.clone(),
            leaf,
            attempts: 0,
        });
        self.trader
            .send(TraderJob::Posted {
                account: account.into(),
                posted: Box::new(posted),
                leaf,
                maker: self.spec.index,
                signature: signature.to_string(),
            })
            .map_err(|_| anyhow!("the trader is gone"))
    }

    fn tick(&mut self) {
        // While the provider refuses, asking again only prolongs it.
        if usage::resting() {
            return;
        }
        self.reclaim();
        self.liquidate();
    }

    /// Reads the maker's notes from the indexer. A note stays in `used`
    /// until the wallet stops listing it: a read that comes before the
    /// indexer has the spending transaction would otherwise offer it again.
    fn refresh(&mut self) -> Result<()> {
        self.actor.sync(&self.chain)?;
        let (actor, chain) = (&self.actor, &self.chain);
        self.used.retain(|hash| actor.lists(chain, hash));
        Ok(())
    }

    /// Takes back the margin of quotes that expired unopened.
    fn reclaim(&mut self) {
        let now = now();
        let due: Vec<usize> = (0..self.pending.len())
            .filter(|&index| self.pending[index].quote.terms.expiry as i64 + 8 < now)
            .collect();
        for index in due.into_iter().rev() {
            match self.cancel(index) {
                Ok(()) => {
                    let done = self.pending.remove(index);
                    self.shared.update(|desk| desk.closed_quotes.push(done.id));
                }
                Err(error) => {
                    let pending = &mut self.pending[index];
                    pending.attempts += 1;
                    eprintln!(
                        "maker {}: reclaiming quote {} (attempt {}): {error:#}",
                        self.spec.name, pending.id, pending.attempts
                    );
                    if pending.attempts >= CANCEL_ATTEMPTS {
                        let done = self.pending.remove(index);
                        self.shared.update(|desk| desk.closed_quotes.push(done.id));
                    }
                }
            }
        }
    }

    fn cancel(&mut self, index: usize) -> Result<()> {
        let pending = &self.pending[index];
        println!("maker {}: reclaiming quote {}", self.spec.name, pending.id);
        let transaction = CancelQuoteTransactionParams {
            quote_utxo: pending.quote.clone(),
            quote_leaf_index: pending.leaf,
            payer: self.actor.pubkey,
            tree_id: self.chain.tree_id,
        }
        .build(&self.actor.keypair)?;
        let instruction = transaction
            .prove(self.chain.client.indexer(), self.chain.tree)?
            .instruction()?;
        // The margin comes back as a new note. Nothing here needs it
        // indexed: the maker finds it the next time it reads its notes.
        let signature = self.chain.send(&self.actor.keypair, instruction)?;
        if let Some(account) = &pending.account {
            self.shared.update(|desk| {
                desk.book(account).log(
                    "reclaim",
                    &self.view.address,
                    None,
                    None,
                    &["margin returned"],
                    &signature.to_string(),
                );
            });
        }
        Ok(())
    }

    /// Settles every position the oracle price has made liquidatable.
    fn liquidate(&mut self) {
        if self.held.is_empty() {
            return;
        }
        // The prices the desk already reads say which positions are worth
        // a look, so a position that is nowhere near its liquidation price
        // costs no read of its own.
        let seen: Vec<(String, u64)> = {
            let desk = self.shared.lock();
            desk.markets
                .iter()
                .filter_map(|view| Some((view.symbol.clone(), view.price.parse().ok()?)))
                .collect()
        };
        // One fresh read of each market with a position to settle: the
        // settle is proved at the price the program will read.
        let mut prices: Vec<(&'static Market, Mark)> = Vec::new();
        let mut due = Vec::new();
        for (index, held) in self.held.iter().enumerate() {
            let Ok(market) = market_of(&held.position.terms.feed_id) else {
                continue;
            };
            let near = seen
                .iter()
                .find(|(symbol, _)| symbol.as_str() == market.symbol)
                .is_some_and(|(_, price)| *price > 0 && held.position.is_liquidatable(*price));
            if !near {
                continue;
            }
            let known = prices
                .iter()
                .find(|(seen, _)| seen.symbol == market.symbol)
                .map(|(_, mark)| *mark);
            let mark = match known {
                Some(mark) => mark,
                None => {
                    let Ok(mark) = self.chain.settle_mark(market) else {
                        continue;
                    };
                    prices.push((market, mark));
                    mark
                }
            };
            if held.position.is_liquidatable(mark.price) {
                due.push((index, market, mark));
            }
        }
        for (index, market, mark) in due.into_iter().rev() {
            let id = self.held[index].id.clone();
            let account = self.held[index].account.clone();
            // The trader may be closing it this very moment.
            let closing = {
                let mut desk = self.shared.lock();
                desk.book(&account)
                    .busy
                    .as_ref()
                    .is_some_and(|busy| busy.position_id.as_deref() == Some(id.as_str()))
            };
            if closing {
                continue;
            }
            match self.settle(index, market, mark) {
                Ok(()) => {
                    self.held.remove(index);
                    let _ = self.trader.send(TraderJob::Liquidated {
                        account,
                        position_id: id,
                    });
                }
                Err(error) => eprintln!("maker {}: liquidating {id}: {error:#}", self.spec.name),
            }
        }
    }

    fn settle(&mut self, index: usize, market: &Market, mark: Mark) -> Result<()> {
        let price = mark.price;
        let held = &self.held[index];
        let transaction = SettleTransactionParams {
            position_utxo: held.position.clone(),
            position_leaf_index: held.leaf,
            oracle: mark.account,
            price,
            payer: self.actor.pubkey,
            tree_id: self.chain.tree_id,
        }
        .build(&self.actor.keypair)?;
        let payout = transaction.trader_payout;
        let instruction = transaction
            .prove(self.chain.client.indexer(), self.chain.tree)?
            .instruction()?;
        // The maker's share comes back as a new note, found the next time
        // it reads its notes.
        let signature = self.chain.send(&self.actor.keypair, instruction)?;
        let id = held.id.clone();
        let account = held.account.clone();
        self.shared.update(|desk| {
            let book = desk.book(&account);
            if let Some(position) = book.positions.iter_mut().find(|position| position.id == id) {
                position.status = "liquidated".into();
                position.settle_price = Some(price.to_string());
                position.payout = Some(payout.to_string());
                position.settled_at = Some(now_ms());
                position.settle_signature = Some(signature.to_string());
            }
            // A maker's settle names only the maker: the trader stays unnamed.
            book.log(
                "settle",
                &self.view.address,
                None,
                Some((market.symbol, price)),
                &["both payouts", "why it could settle"],
                &signature.to_string(),
            );
        });
        Ok(())
    }
}
