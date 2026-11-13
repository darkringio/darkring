//! The trader: discovers the quotes addressed to it, opens one, and closes
//! its positions at the oracle price.

use std::{
    collections::HashMap,
    sync::{
        mpsc::{Receiver, RecvTimeoutError, Sender},
        Arc,
    },
    thread::sleep,
    time::Duration,
};

use anyhow::{anyhow, Result};
use perps_sdk::{
    index::index_trader_quotes,
    instructions::{open::OpenTransactionParams, settle::SettleTransactionParams},
    state::{PositionUtxo, QuoteUtxo},
};
use solana_keypair::Keypair;
use spr_test_utils::test_validator_asserts::wait_for_merkle_proof;

use crate::{
    chain::{now_ms, solana_keypair, Actor, Chain, Config, MAX_PRICE_AGE_SECS, ORACLE, USDC},
    desk::{Busy, PositionView, QuoteView, Shared},
    maker::{id_of, MakerJob},
};

/// What the faucet shields, and what a new trader starts with.
const FAUCET: u64 = 5_000 * USDC;
/// The desk stops offering a quote this long before it expires on chain: an
/// open has to be proved and land first.
const OPEN_HEADROOM_SECS: i64 = 40;
const TICK: Duration = Duration::from_secs(3);
const CLOSE_ATTEMPTS: u8 = 3;

pub enum TraderJob {
    /// A maker posted this quote for the trader to find.
    Discover {
        posted: Box<QuoteUtxo>,
        maker: usize,
        signature: String,
    },
    /// A maker could not quote.
    NoQuote {
        maker: String,
        reason: String,
    },
    Open {
        quote_id: String,
    },
    Close {
        position_id: String,
    },
    Faucet,
    /// A maker settled this position.
    Liquidated {
        position_id: String,
    },
    /// A maker found this position open on chain after a restart.
    Recovered {
        position_id: String,
        position: Box<PositionUtxo>,
        leaf: u64,
        maker: usize,
    },
}

struct Position {
    utxo: PositionUtxo,
    leaf: u64,
    maker: usize,
}

struct Trader {
    chain: Chain,
    actor: Actor,
    deployer: Keypair,
    shared: Arc<Shared>,
    makers: Vec<Sender<MakerJob>>,
    /// Each live quote, its leaf in the tree and its maker.
    quotes: HashMap<String, (QuoteUtxo, u64, usize)>,
    positions: HashMap<String, Position>,
}

pub fn run(
    config: Config,
    shared: Arc<Shared>,
    jobs: Receiver<TraderJob>,
    makers: Vec<Sender<MakerJob>>,
) {
    let mut trader = loop {
        match Trader::boot(&config, &shared, &makers) {
            Ok(trader) => break trader,
            Err(error) => {
                eprintln!("trader: {error:#}; trying again");
                sleep(Duration::from_secs(15));
            }
        }
    };
    shared.lock().booted();
    println!("trader: ready");
    loop {
        let job = match jobs.recv_timeout(TICK) {
            Ok(job) => job,
            Err(RecvTimeoutError::Timeout) => {
                trader.tick();
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let outcome = match job {
            TraderJob::Discover {
                posted,
                maker,
                signature,
            } => {
                let outcome = trader.discover(&posted, maker, &signature);
                shared.lock().quote_answered();
                // One maker failing to reach the trader is not the request
                // failing: the others may still answer.
                if let Err(error) = &outcome {
                    eprintln!("trader: discovering a quote: {error:#}");
                }
                Ok(())
            }
            TraderJob::NoQuote { maker, reason } => {
                let mut desk = shared.lock();
                desk.quote_answered();
                if desk.awaiting_quotes == 0 && desk.quotes.is_empty() {
                    desk.error = Some(format!("{maker} could not quote: {reason}"));
                }
                Ok(())
            }
            TraderJob::Open { quote_id } => trader.open(&quote_id),
            TraderJob::Close { position_id } => trader.close(&position_id),
            TraderJob::Faucet => trader.faucet(),
            TraderJob::Liquidated { position_id } => {
                trader.positions.remove(&position_id);
                trader.refresh_balance()
            }
            TraderJob::Recovered {
                position_id,
                position,
                leaf,
                maker,
            } => {
                trader.positions.insert(
                    position_id,
                    Position {
                        utxo: *position,
                        leaf,
                        maker,
                    },
                );
                Ok(())
            }
        };
        match outcome {
            Ok(()) => {
                let mut desk = shared.lock();
                if desk.busy.as_ref().is_some_and(|busy| busy.kind != "quotes") {
                    desk.busy = None;
                }
            }
            Err(error) => shared.fail(&error),
        }
    }
}

impl Trader {
    fn boot(config: &Config, shared: &Arc<Shared>, makers: &[Sender<MakerJob>]) -> Result<Self> {
        let chain = Chain::connect(config)?;
        let mut actor = Actor::load(config, &chain, "devnet-trader.json")?;
        let deployer = solana_keypair(config, "devnet-deployer.json")?;
        if actor.sync(&chain)? < 100 * USDC {
            chain.shield(&deployer, &actor.keypair, FAUCET)?;
        }
        let trader = Self {
            chain,
            actor,
            deployer,
            shared: Arc::clone(shared),
            makers: makers.to_vec(),
            quotes: HashMap::new(),
            positions: HashMap::new(),
        };
        let mut trader = trader;
        trader.refresh_balance()?;
        trader.tick();
        Ok(trader)
    }

    fn refresh_balance(&mut self) -> Result<()> {
        let balance = self.actor.sync(&self.chain)?;
        self.shared.lock().balance = balance.to_string();
        Ok(())
    }

    /// Reads the oracle and drops the quotes whose time is up.
    fn tick(&mut self) {
        let oracle = self.chain.oracle();
        let now = now_ms();
        let mut desk = self.shared.lock();
        if let Ok(oracle) = oracle {
            desk.price = oracle.price.to_string();
            desk.publish_time = oracle.publish_time;
        }
        desk.quotes.retain(|quote| quote.expires_at > now);
        let live: Vec<&str> = desk.quotes.iter().map(|quote| quote.id.as_str()).collect();
        self.quotes.retain(|id, _| live.contains(&id.as_str()));
    }

    /// Finds `posted` the way any trader would: by scanning the indexer for
    /// quotes encrypted to this wallet, and checking the maker each names
    /// against the user registry.
    fn discover(&mut self, posted: &QuoteUtxo, maker: usize, signature: &str) -> Result<()> {
        let client = &self.chain.client;
        let quote = index_trader_quotes(
            &self.actor.keypair,
            &self.actor.wallet.registry,
            client.indexer(),
            client.rpc(),
            Duration::from_secs(60),
        )?
        .into_iter()
        .find(|quote| &quote.quote_utxo == posted)
        .ok_or_else(|| anyhow!("the quote is not among those addressed to this wallet"))?;
        let terms = &quote.quote_utxo.terms;
        let id = id_of(&quote.quote_utxo.hash(self.chain.tree_id)?);
        let mut desk = self.shared.lock();
        let view = QuoteView {
            id: id.clone(),
            maker: desk
                .makers
                .get(maker)
                .cloned()
                .ok_or_else(|| anyhow!("no maker {maker}"))?,
            side: if terms.trader_is_long {
                "long"
            } else {
                "short"
            }
            .into(),
            size: terms.size.to_string(),
            entry_price: terms.entry_price.to_string(),
            trader_margin: terms.trader_margin.to_string(),
            maker_margin: quote.quote_utxo.maker_margin.to_string(),
            maintenance_margin: terms.maintenance_margin.to_string(),
            expires_at: (terms.expiry as i64 - OPEN_HEADROOM_SECS).max(0) as u64 * 1000,
            signature: signature.into(),
        };
        desk.quotes.push(view);
        drop(desk);
        self.quotes
            .insert(id, (quote.quote_utxo, quote.leaf_index, maker));
        Ok(())
    }

    /// Accepts a quote: both margins move into one sealed position.
    fn open(&mut self, quote_id: &str) -> Result<()> {
        let (quote_utxo, quote_leaf_index, maker) = self
            .quotes
            .get(quote_id)
            .cloned()
            .ok_or_else(|| anyhow!("that quote has expired"))?;
        let view = self
            .shared
            .lock()
            .quotes
            .iter()
            .find(|view| view.id == quote_id)
            .cloned()
            .ok_or_else(|| anyhow!("that quote has expired"))?;
        self.actor.sync(&self.chain)?;
        let trader_input = self
            .actor
            .note(&self.chain, quote_utxo.terms.trader_margin)?;
        let transaction = OpenTransactionParams {
            quote_utxo,
            quote_leaf_index,
            trader_input,
            payer: self.actor.pubkey,
            tree_id: self.chain.tree_id,
        }
        .build(&self.actor.keypair)?;
        let opened = transaction.position_utxo.clone();
        let client = &self.chain.client;
        let instruction = transaction
            .prove(client.indexer(), self.chain.tree, &self.actor.keypair)?
            .instruction()?;
        self.label("Sending to Solana");
        let signature = self.chain.send(&self.actor.keypair, instruction)?;
        self.chain.indexed(signature)?;
        let hash = opened.hash(self.chain.tree_id)?;
        let leaf = wait_for_merkle_proof(client.indexer(), self.chain.tree, hash).leaf_index;
        let id = id_of(&hash);

        let wallet = self.actor.pubkey.to_string();
        self.shared.update(|desk| {
            desk.quotes.retain(|quote| quote.id != quote_id);
            let frame = desk.positions.len() as u32 + 1;
            desk.positions.push(PositionView {
                id: id.clone(),
                frame,
                maker: view.maker.clone(),
                side: view.side.clone(),
                size: view.size.clone(),
                entry_price: view.entry_price.clone(),
                trader_margin: view.trader_margin.clone(),
                total: opened.total.to_string(),
                maintenance_margin: view.maintenance_margin.clone(),
                opened_at: now_ms(),
                status: "open".into(),
                settle_price: None,
                payout: None,
                settled_at: None,
                open_signature: signature.to_string(),
                settle_signature: None,
            });
            desk.closed_quotes.push(quote_id.into());
            desk.log(
                "open",
                &wallet,
                Some(&view.maker.address),
                None,
                &[
                    "size",
                    "direction",
                    "entry price",
                    "both margins",
                    "liquidation price",
                ],
                &signature.to_string(),
            );
        });
        self.quotes.remove(quote_id);
        if let Some(sender) = self.makers.get(maker) {
            let _ = sender.send(MakerJob::Opened {
                quote_id: quote_id.into(),
                position_id: id.clone(),
                position: Box::new(opened.clone()),
                leaf,
            });
        }
        self.positions.insert(
            id,
            Position {
                utxo: opened,
                leaf,
                maker,
            },
        );
        self.refresh_balance()
    }

    /// Closes at whatever the oracle account reads. The proof commits to one
    /// price, so a close that lands just after Pyth updated the account is
    /// proved again at the new price.
    fn close(&mut self, position_id: &str) -> Result<()> {
        let (utxo, leaf, maker) = {
            let position = self
                .positions
                .get(position_id)
                .ok_or_else(|| anyhow!("that position is not open"))?;
            (position.utxo.clone(), position.leaf, position.maker)
        };
        let mut attempt = 0;
        let (price, payout, signature) = loop {
            attempt += 1;
            let price = self.chain.settle_price()?;
            let transaction = SettleTransactionParams {
                position_utxo: utxo.clone(),
                position_leaf_index: leaf,
                oracle: ORACLE,
                price,
                payer: self.actor.pubkey,
                tree_id: self.chain.tree_id,
            }
            .build(&self.actor.keypair)?;
            let payout = transaction.trader_payout;
            let instruction = transaction
                .prove(self.chain.client.indexer(), self.chain.tree)?
                .instruction()?;
            self.label("Sending to Solana");
            match self.chain.send(&self.actor.keypair, instruction) {
                Ok(signature) => break (price, payout, signature),
                Err(error) if attempt < CLOSE_ATTEMPTS => {
                    eprintln!("trader: close attempt {attempt} at {price}: {error:#}");
                    self.label("Proving the close again");
                }
                Err(error) => return Err(error),
            }
        };
        self.chain.indexed(signature)?;

        let wallet = self.actor.pubkey.to_string();
        self.shared.update(|desk| {
            let mut counterparty = None;
            if let Some(position) = desk
                .positions
                .iter_mut()
                .find(|position| position.id == position_id)
            {
                position.status = "closed".into();
                position.settle_price = Some(price.to_string());
                position.payout = Some(payout.to_string());
                position.settled_at = Some(now_ms());
                position.settle_signature = Some(signature.to_string());
                counterparty = Some(position.maker.address.clone());
            }
            desk.log(
                "settle",
                &wallet,
                counterparty.as_deref(),
                Some(price),
                &["both payouts", "profit or loss"],
                &signature.to_string(),
            );
        });
        self.positions.remove(position_id);
        if let Some(sender) = self.makers.get(maker) {
            let _ = sender.send(MakerJob::Closed {
                position_id: position_id.into(),
            });
        }
        self.refresh_balance()
    }

    fn faucet(&mut self) -> Result<()> {
        self.chain
            .shield(&self.deployer, &self.actor.keypair, FAUCET)?;
        self.refresh_balance()
    }

    fn label(&self, label: &str) {
        if let Some(busy) = self.shared.lock().busy.as_mut() {
            busy.label = label.into();
        }
    }
}

/// What a fresh desk reports before any actor has connected.
pub fn boot_notice() -> Busy {
    Busy::new("boot", "Connecting to devnet")
}

pub const fn max_price_age() -> i64 {
    MAX_PRICE_AGE_SECS
}
