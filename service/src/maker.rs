//! A maker: answers quote requests with a sealed quote on chain, takes back
//! the margin of quotes nobody opened, settles a perp the moment the oracle
//! price makes it liquidatable, and settles a dated contract (an option or
//! a prediction) at the first oracle price after its term ends.

use std::{
    collections::HashSet,
    sync::{
        mpsc::{Receiver, RecvTimeoutError, Sender},
        Arc,
    },
    thread::sleep,
    time::Duration,
};

use anyhow::{anyhow, bail, ensure, Result};
use perps_sdk::{
    index::{
        maker_option_quotes_among, maker_options_among, maker_positions_among, maker_quotes_among,
        maker_transactions,
    },
    instructions::{
        cancel_quote::CancelQuoteTransactionParams,
        option::{
            OptionCancelTransactionParams, OptionQuoteTransactionParams,
            OptionSettleTransactionParams,
        },
        quote::QuoteTransactionParams,
        settle::SettleTransactionParams,
    },
    shared::identity,
    state::{OptionQuoteTerms, QuoteTerms, KIND_ABOVE, KIND_CALL},
    SIZE_SCALE,
};
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use spr_keypair::ShieldedAddress;

use crate::{
    chain::{
        market_of, now, now_ms, solana_keypair, Actor, Chain, Config, Laps, Mark, Market, USDC,
    },
    deal::{AnyPosition, AnyQuote},
    desk::{MakerView, Shared},
    pricing,
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

/// What an account asks a maker to quote.
#[derive(Clone, Copy)]
pub enum Ask {
    /// A perp of `size` at `leverage`.
    Perp {
        long: bool,
        size: u64,
        leverage: u64,
    },
    /// A dated contract ending at `term_end`, struck at `strike`: a call or
    /// a put of `size`, or a prediction the account stakes `stake` on.
    Option {
        kind: u8,
        size: u64,
        strike: u64,
        stake: u64,
        term_end: u64,
    },
}

/// A quote a maker has worked out and not yet posted: its terms, and what
/// the maker locks behind it.
enum Draft {
    Perp(QuoteTerms, u64),
    Option(OptionQuoteTerms, u64),
}

impl Draft {
    /// What the maker locks in the quote.
    fn locked(&self) -> u64 {
        match self {
            Self::Perp(_, margin) | Self::Option(_, margin) => *margin,
        }
    }
}

pub enum MakerJob {
    /// `account` asked for a quote; `trader` is its address.
    Quote {
        account: String,
        market: &'static Market,
        trader: ShieldedAddress,
        ask: Ask,
    },
    /// `account` has a session again: tell it what it has open here.
    Resend { account: String },
    /// The trader opened this maker's quote.
    Opened {
        account: String,
        quote_id: String,
        position_id: String,
        position: Box<AnyPosition>,
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
    quote: AnyQuote,
    leaf: u64,
    attempts: u8,
}

struct Held {
    id: String,
    /// The trader's Solana address: the account whose position this is.
    account: String,
    position: AnyPosition,
    leaf: u64,
}

/// The account a position belongs to: its trader's Solana address.
fn account_of(position: &AnyPosition) -> Result<String> {
    Ok(position.trader().solana_address()?.to_string())
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
    /// Reading the notes costs dozens of indexer requests, and nothing but
    /// this maker's own quotes spends them. So they are read at the start
    /// and then only when no note the maker knows of, and has not used,
    /// covers a quote: not on a timer, and not before every quote. A quote
    /// keeps its own change (`Actor::learn`), so that is seldom: the maker
    /// goes on spending the change of its last quote.
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
                ask,
            }) => {
                if let Err(error) = maker.quote(&account, market, trader, ask) {
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

    /// Finds what this maker left on chain before a restart: positions and
    /// dated contracts still open against it, and quotes whose margin it has
    /// yet to take back. Everything of the maker's carries its authority's
    /// tag, so its transactions are read once and searched four times.
    fn recover(&mut self) -> Result<()> {
        let client = &self.chain.client;
        let registry = &self.actor.wallet.registry;
        let transactions =
            maker_transactions(&self.actor.keypair, client.indexer()).unwrap_or_default();
        let mut found: Vec<(AnyPosition, u64)> = Vec::new();
        if !transactions.is_empty() {
            for position in maker_positions_among(
                &self.actor.keypair,
                registry,
                client.indexer(),
                client.rpc(),
                &transactions,
            )
            .unwrap_or_default()
            {
                found.push((
                    AnyPosition::Perp(position.position_utxo),
                    position.leaf_index,
                ));
            }
            for option in maker_options_among(
                &self.actor.keypair,
                registry,
                client.indexer(),
                client.rpc(),
                &transactions,
            )
            .unwrap_or_default()
            {
                found.push((AnyPosition::Option(option.option_utxo), option.leaf_index));
            }
        }
        let mut open = Vec::new();
        for (position, leaf) in found {
            let id = id_of(&position.hash(self.chain.tree_id)?);
            let account = account_of(&position)?;
            let Ok(market) = market_of(position.feed_id()) else {
                continue;
            };
            let view = position.view(id.clone(), market.symbol, self.view.clone(), "");
            self.shared.update(|desk| {
                let book = desk.book(&account);
                if !book.positions.iter().any(|position| position.id == id) {
                    let frame = book.positions.len() as u32 + 1;
                    book.positions
                        .push(crate::desk::PositionView { frame, ..view });
                }
            });
            open.push(id.clone());
            self.held.push(Held {
                id,
                account,
                position,
                leaf,
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

        let mut quotes: Vec<(AnyQuote, u64)> = Vec::new();
        for quote in
            maker_quotes_among(&self.actor.keypair, registry, &transactions).unwrap_or_default()
        {
            quotes.push((AnyQuote::Perp(quote.quote_utxo), quote.leaf_index));
        }
        for quote in maker_option_quotes_among(&self.actor.keypair, registry, &transactions)
            .unwrap_or_default()
        {
            quotes.push((AnyQuote::Option(quote.quote_utxo), quote.leaf_index));
        }
        let closed = self.shared.lock().closed_quotes.clone();
        for (quote, leaf) in quotes {
            let id = id_of(&quote.hash(self.chain.tree_id)?);
            if closed.contains(&id) {
                continue;
            }
            self.pending.push(Pending {
                id,
                account: None,
                quote,
                leaf,
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

    /// Works out the quote this maker gives for `ask`: a perp at the oracle
    /// price moved by its spread, or a dated contract priced by `pricing`.
    fn draft(&self, market: &Market, trader: &ShieldedAddress, ask: Ask) -> Result<Draft> {
        let mark = self.chain.mark(market)?.price;
        let maker = self.actor.keypair.shielded_address()?;
        let expiry = (now() + QUOTE_LIFETIME_SECS) as u64;
        let designated = Some(identity(&trader.solana_address()?)?);
        match ask {
            Ask::Perp {
                long,
                size,
                leverage,
            } => {
                let spread = mark / 10_000 * self.spec.spread_bps;
                let entry_price = if long { mark + spread } else { mark - spread };
                let notional =
                    u64::try_from(size as u128 * entry_price as u128 / SIZE_SCALE as u128)?;
                let trader_margin = notional / leverage.max(1);
                let maker_margin = trader_margin / 10 * self.spec.coverage_tenths;
                ensure!(
                    trader_margin > 0 && maker_margin > 0,
                    "the size is too small"
                );
                Ok(Draft::Perp(
                    QuoteTerms {
                        feed_id: market.feed,
                        trader_is_long: long,
                        size,
                        entry_price,
                        trader_margin,
                        maintenance_margin: trader_margin / 5,
                        maker,
                        expiry,
                        trader: designated,
                    },
                    maker_margin,
                ))
            }
            Ask::Option {
                kind,
                size,
                strike,
                stake,
                term_end,
            } => {
                // A price tick in dollars, and the contract in the model's
                // units: dollars and seconds.
                let tick = 10f64.powi(market.expo);
                let spot = mark as f64 * tick;
                let seconds = term_end as f64 - now() as f64;
                ensure!(seconds >= 60.0, "that contract ends too soon");
                let volatility = pricing::volatility(market.symbol);
                let (premium, total) = if kind >= KIND_ABOVE {
                    let chance = pricing::quoted_chance(
                        spot,
                        strike as f64 * tick,
                        seconds,
                        volatility,
                        kind == KIND_ABOVE,
                    );
                    (stake, pricing::prediction_total(stake, chance))
                } else {
                    // How far past the strike the contract pays, in ticks: a
                    // put cannot pay past a price of zero.
                    let cap = (pricing::cap_distance(spot, seconds, volatility) / tick).round();
                    let cap_ticks = if kind == KIND_CALL {
                        cap as u64
                    } else {
                        (cap as u64).min(strike.saturating_sub(1))
                    };
                    let total =
                        u64::try_from(size as u128 * cap_ticks as u128 / SIZE_SCALE as u128)?;
                    let units = size as f64 / market.size_per_unit();
                    let per_unit = pricing::premium_per_unit(
                        spot,
                        strike as f64 * tick,
                        cap_ticks as f64 * tick,
                        seconds,
                        volatility,
                        kind == KIND_CALL,
                    );
                    ((per_unit * units * USDC as f64).ceil() as u64, total)
                };
                ensure!(
                    premium as f64 >= pricing::MIN_PREMIUM_USD * USDC as f64,
                    "that contract costs less than a dollar; make it larger"
                );
                ensure!(
                    total as f64 <= pricing::MAX_PAYOUT_USD * USDC as f64,
                    "that contract could pay more than {:.0} dollars; make it smaller",
                    pricing::MAX_PAYOUT_USD
                );
                ensure!(
                    premium < total,
                    "that strike is so far in the money the contract would cost all it can pay"
                );
                Ok(Draft::Option(
                    OptionQuoteTerms {
                        feed_id: market.feed,
                        kind,
                        size,
                        strike,
                        premium,
                        term_end,
                        maker,
                        expiry,
                        trader: designated,
                    },
                    total - premium,
                ))
            }
        }
    }

    /// Posts the quote this maker gives `trader` for `ask`, and hands its
    /// opening to the trader's account.
    fn quote(
        &mut self,
        account: &str,
        market: &Market,
        trader: ShieldedAddress,
        ask: Ask,
    ) -> Result<()> {
        let draft = self.draft(market, &trader, ask)?;
        let locked = draft.locked();

        let mut lap = Laps::start();
        let asked = usage::indexer_requests();
        let source = match self.actor.note(&self.chain, locked, &self.used) {
            Ok(source) => source,
            // Every note this maker knows of is used or too small: the
            // change from its earlier quotes, and what closed positions
            // paid it, are notes it has not read yet.
            Err(_) => {
                self.refresh()?;
                self.actor.note(&self.chain, locked, &self.used)?
            }
        };
        let spending = source.utxo_hash;
        lap.mark("notes");
        let client = &self.chain.client;
        let (posted, instruction): (AnyQuote, Instruction) = match draft {
            Draft::Perp(terms, maker_margin) => {
                let transaction = QuoteTransactionParams {
                    terms,
                    maker_margin,
                    source,
                    trader_address: trader,
                    payer: self.actor.pubkey,
                }
                .build(&self.actor.keypair)?;
                let posted = AnyQuote::Perp(transaction.quote_utxo.clone());
                let instruction = transaction
                    .prove(client.indexer(), self.chain.tree, &self.actor.keypair)?
                    .instruction()?;
                (posted, instruction)
            }
            Draft::Option(terms, collateral) => {
                let transaction = OptionQuoteTransactionParams {
                    terms,
                    collateral,
                    source,
                    trader_address: trader,
                    payer: self.actor.pubkey,
                }
                .build(&self.actor.keypair)?;
                let posted = AnyQuote::Option(transaction.quote_utxo.clone());
                let instruction = transaction
                    .prove(client.indexer(), self.chain.tree, &self.actor.keypair)?
                    .option_instruction()?;
                (posted, instruction)
            }
        };
        lap.mark("prove");
        let signature = self.chain.send(&self.actor.keypair, instruction)?;
        self.used.insert(spending);
        lap.mark("send");
        let hash = posted.hash(self.chain.tree_id)?;
        // One question tells where the quote sits and hands back the
        // change, which the next quote can spend without the wallet being
        // read again: that read is some thirty-five requests and seven
        // seconds of somebody's order.
        let landed = self.chain.landed(signature)?;
        let leaf = landed.leaf(&hash)?;
        let learned = match self.actor.learn(&self.chain, &landed) {
            Ok(learned) => learned,
            // Nothing is lost: the wallet finds the change when it is read.
            Err(error) => {
                eprintln!("maker {}: {error:#}", self.spec.name);
                0
            }
        };
        lap.mark("leaf");
        println!(
            "maker {}: quoted in {lap}, with {} indexer requests, and kept {learned} note of change",
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
        // While the provider refuses, asking again only prolongs it; and
        // when the desk has stopped for the day, so have its makers.
        if usage::paused().is_some() {
            return;
        }
        self.reclaim();
        self.settle_due();
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
            .filter(|&index| self.pending[index].quote.expiry() as i64 + 8 < now)
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
        let indexer = self.chain.client.indexer();
        let instruction = match &pending.quote {
            AnyQuote::Perp(quote) => CancelQuoteTransactionParams {
                quote_utxo: quote.clone(),
                quote_leaf_index: pending.leaf,
                payer: self.actor.pubkey,
                tree_id: self.chain.tree_id,
            }
            .build(&self.actor.keypair)?
            .prove(indexer, self.chain.tree)?
            .instruction()?,
            AnyQuote::Option(quote) => OptionCancelTransactionParams {
                quote_utxo: quote.clone(),
                quote_leaf_index: pending.leaf,
                payer: self.actor.pubkey,
                tree_id: self.chain.tree_id,
            }
            .build(&self.actor.keypair)?
            .prove(indexer, self.chain.tree)?
            .option_instruction()?,
        };
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

    /// Settles what this maker may settle now: every perp the oracle price
    /// has made liquidatable, and every dated contract whose term has ended.
    fn settle_due(&mut self) {
        if self.held.is_empty() {
            return;
        }
        // The prices the desk already reads say which positions are worth
        // a look, so one that is nowhere near being settled costs no read
        // of its own.
        let seen: Vec<(String, u64, i64)> = {
            let desk = self.shared.lock();
            desk.markets
                .iter()
                .filter_map(|view| {
                    Some((
                        view.symbol.clone(),
                        view.price.parse().ok()?,
                        view.publish_time,
                    ))
                })
                .collect()
        };
        // One fresh read of each market with something to settle: the
        // settle is proved at the price the program will read.
        let mut prices: Vec<(&'static Market, Mark)> = Vec::new();
        let mut due = Vec::new();
        for (index, held) in self.held.iter().enumerate() {
            let Ok(market) = market_of(held.position.feed_id()) else {
                continue;
            };
            let may = |price: u64, publish_time: i64| match &held.position {
                AnyPosition::Perp(position) => price > 0 && position.is_liquidatable(price),
                AnyPosition::Option(option) => {
                    publish_time > 0 && option.may_settle(false, publish_time as u64)
                }
            };
            let near = seen
                .iter()
                .find(|(symbol, _, _)| symbol.as_str() == market.symbol)
                .is_some_and(|(_, price, publish_time)| may(*price, *publish_time));
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
            if may(mark.price, mark.publish_time) {
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
                Err(error) => eprintln!(
                    "maker {}: settling {id}: {}",
                    self.spec.name,
                    usage::scrub(&format!("{error:#}"))
                ),
            }
        }
    }

    fn settle(&mut self, index: usize, market: &Market, mark: Mark) -> Result<()> {
        let price = mark.price;
        let held = &self.held[index];
        let indexer = self.chain.client.indexer();
        let (payout, instruction, status, sealed): (u64, Instruction, &str, &[&str]) =
            match &held.position {
                AnyPosition::Perp(position) => {
                    let transaction = SettleTransactionParams {
                        position_utxo: position.clone(),
                        position_leaf_index: held.leaf,
                        oracle: mark.account,
                        price,
                        payer: self.actor.pubkey,
                        tree_id: self.chain.tree_id,
                    }
                    .build(&self.actor.keypair)?;
                    (
                        transaction.trader_payout,
                        transaction.prove(indexer, self.chain.tree)?.instruction()?,
                        "liquidated",
                        &["both payouts", "why it could settle"],
                    )
                }
                AnyPosition::Option(option) => {
                    let Ok(publish_time) = u64::try_from(mark.publish_time) else {
                        bail!("the oracle's publish time is before the epoch");
                    };
                    let transaction = OptionSettleTransactionParams {
                        option_utxo: option.clone(),
                        option_leaf_index: held.leaf,
                        oracle: mark.account,
                        price,
                        publish_time,
                        payer: self.actor.pubkey,
                        tree_id: self.chain.tree_id,
                    }
                    .build(&self.actor.keypair)?;
                    (
                        transaction.trader_payout,
                        transaction
                            .prove(indexer, self.chain.tree)?
                            .option_instruction()?,
                        "expired",
                        &["both payouts", "what the contract was"],
                    )
                }
            };
        // The maker's share comes back as a new note, found the next time
        // it reads its notes.
        let signature = self.chain.send(&self.actor.keypair, instruction)?;
        println!(
            "maker {}: settled {} at {price}, {status}",
            self.spec.name, held.id
        );
        let id = held.id.clone();
        let account = held.account.clone();
        self.shared.update(|desk| {
            let book = desk.book(&account);
            if let Some(position) = book.positions.iter_mut().find(|position| position.id == id) {
                position.status = status.into();
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
                sealed,
                &signature.to_string(),
            );
        });
        Ok(())
    }
}
