//! A maker: answers quote requests with a sealed quote on chain, takes back
//! the margin of quotes nobody opened, and settles a position the moment the
//! oracle price makes it liquidatable.

use std::{
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
use solana_address::Address;
use solana_keypair::Keypair;
use spr_keypair::ShieldedAddress;
use spr_test_utils::test_validator_asserts::wait_for_merkle_proof;

use crate::{
    chain::{now, now_ms, solana_keypair, Actor, Chain, Config, FEED_ID, ORACLE, USDC},
    desk::{MakerView, PositionView, Shared},
    trader::TraderJob,
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
    Quote {
        long: bool,
        size: u64,
        leverage: u64,
    },
    /// The trader opened this maker's quote.
    Opened {
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
    quote: QuoteUtxo,
    leaf: u64,
    attempts: u8,
}

struct Held {
    id: String,
    position: PositionUtxo,
    leaf: u64,
}

struct Maker {
    spec: MakerSpec,
    view: MakerView,
    chain: Chain,
    actor: Actor,
    trader_address: ShieldedAddress,
    trader_pubkey: Address,
    shared: Arc<Shared>,
    trader: Sender<TraderJob>,
    pending: Vec<Pending>,
    held: Vec<Held>,
}

pub fn run(
    config: Config,
    spec: MakerSpec,
    view: MakerView,
    trader_address: ShieldedAddress,
    shared: Arc<Shared>,
    jobs: Receiver<MakerJob>,
    trader: Sender<TraderJob>,
) {
    let name = spec.name;
    let mut maker = loop {
        match Maker::boot(&config, &spec, &view, trader_address, &shared, &trader) {
            Ok(maker) => break maker,
            Err(error) => {
                eprintln!("maker {name}: {error:#}; trying again");
                sleep(Duration::from_secs(15));
            }
        }
    };
    maker.spec = spec;
    shared.lock().booted();
    println!("maker {name}: ready");
    loop {
        match jobs.recv_timeout(TICK) {
            Ok(MakerJob::Quote {
                long,
                size,
                leverage,
            }) => {
                if let Err(error) = maker.quote(long, size, leverage) {
                    eprintln!("maker {name}: no quote: {error:#}");
                    let _ = maker.trader.send(TraderJob::NoQuote {
                        maker: name.into(),
                        reason: format!("{error:#}"),
                    });
                }
            }
            Ok(MakerJob::Opened {
                quote_id,
                position_id,
                position,
                leaf,
            }) => {
                maker.pending.retain(|pending| pending.id != quote_id);
                maker.held.push(Held {
                    id: position_id,
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
        trader_address: ShieldedAddress,
        shared: &Arc<Shared>,
        trader: &Sender<TraderJob>,
    ) -> Result<Self> {
        let chain = Chain::connect(config)?;
        let mut actor = Actor::load(config, &chain, spec.file)?;
        actor.sync(&chain)?;
        if actor.note(&chain, FLOAT_FLOOR).is_err() {
            let deployer: Keypair = solana_keypair(config, "devnet-deployer.json")?;
            chain.shield(&deployer, &actor.keypair, FLOAT)?;
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
            trader_pubkey: trader_address.solana_address()?,
            trader_address,
            chain,
            actor,
            shared: Arc::clone(shared),
            trader: trader.clone(),
            pending: Vec::new(),
            held: Vec::new(),
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
            if terms.trader != self.trader_address {
                continue;
            }
            let view = PositionView {
                id: id.clone(),
                frame: 0,
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
                if !desk.positions.iter().any(|position| position.id == id) {
                    let frame = desk.positions.len() as u32 + 1;
                    desk.positions.push(PositionView { frame, ..view });
                }
            });
            let _ = self.trader.send(TraderJob::Recovered {
                position_id: id.clone(),
                position: Box::new(found.position_utxo.clone()),
                leaf: found.leaf_index,
                maker: self.spec.index,
            });
            open.push(id.clone());
            self.held.push(Held {
                id,
                position: found.position_utxo,
                leaf: found.leaf_index,
            });
        }
        // A position the file calls open that is no longer on chain was
        // settled while nobody was recording.
        let address = self.view.address.clone();
        self.shared.update(|desk| {
            desk.positions.retain(|position| {
                position.status != "open"
                    || position.maker.address != address
                    || open.contains(&position.id)
            });
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
                quote: found.quote_utxo,
                leaf: found.leaf_index,
                // Most of these were opened long ago: one try each.
                attempts: CANCEL_ATTEMPTS - 1,
            });
        }
        Ok(())
    }

    /// Posts a quote for the trader at the oracle price, moved by this
    /// maker's spread, and hands it to the trader to discover.
    fn quote(&mut self, long: bool, size: u64, leverage: u64) -> Result<()> {
        let mark = self.chain.oracle()?.price;
        let spread = mark / 10_000 * self.spec.spread_bps;
        let entry_price = if long { mark + spread } else { mark - spread };
        let notional = u64::try_from(size as u128 * entry_price as u128 / SIZE_SCALE as u128)?;
        let trader_margin = notional / leverage.max(1);
        let maker_margin = trader_margin / 10 * self.spec.coverage_tenths;
        anyhow::ensure!(
            trader_margin > 0 && maker_margin > 0,
            "the size is too small"
        );

        self.actor.sync(&self.chain)?;
        let source = self.actor.note(&self.chain, maker_margin)?;
        let terms = QuoteTerms {
            feed_id: FEED_ID,
            trader_is_long: long,
            size,
            entry_price,
            trader_margin,
            maintenance_margin: trader_margin / 5,
            maker: self.actor.keypair.shielded_address()?,
            expiry: (now() + QUOTE_LIFETIME_SECS) as u64,
            trader: Some(identity(&self.trader_pubkey)?),
        };
        let transaction = QuoteTransactionParams {
            terms,
            maker_margin,
            source,
            trader_address: self.trader_address,
            payer: self.actor.pubkey,
        }
        .build(&self.actor.keypair)?;
        let posted = transaction.quote_utxo.clone();
        let client = &self.chain.client;
        let instruction = transaction
            .prove(client.indexer(), self.chain.tree, &self.actor.keypair)?
            .instruction()?;
        let signature = self.chain.send(&self.actor.keypair, instruction)?;
        self.chain.indexed(signature)?;
        let hash = posted.hash(self.chain.tree_id)?;
        let leaf = wait_for_merkle_proof(client.indexer(), self.chain.tree, hash).leaf_index;
        let id = id_of(&hash);
        self.shared.update(|desk| {
            desk.log(
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
            quote: posted.clone(),
            leaf,
            attempts: 0,
        });
        self.trader
            .send(TraderJob::Discover {
                posted: Box::new(posted),
                maker: self.spec.index,
                signature: signature.to_string(),
            })
            .map_err(|_| anyhow!("the trader is gone"))
    }

    fn tick(&mut self) {
        self.reclaim();
        self.liquidate();
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
        let signature = self.chain.send(&self.actor.keypair, instruction)?;
        self.chain.indexed(signature)?;
        self.shared.update(|desk| {
            desk.log(
                "reclaim",
                &self.view.address,
                None,
                None,
                &["margin returned"],
                &signature.to_string(),
            );
        });
        Ok(())
    }

    /// Settles every position the oracle price has made liquidatable.
    fn liquidate(&mut self) {
        if self.held.is_empty() {
            return;
        }
        let Ok(price) = self.chain.settle_price() else {
            return;
        };
        let due: Vec<usize> = (0..self.held.len())
            .filter(|&index| self.held[index].position.is_liquidatable(price))
            .collect();
        for index in due.into_iter().rev() {
            let id = self.held[index].id.clone();
            // The trader may be closing it this very moment.
            let closing = {
                let desk = self.shared.lock();
                desk.busy
                    .as_ref()
                    .is_some_and(|busy| busy.position_id.as_deref() == Some(id.as_str()))
            };
            if closing {
                continue;
            }
            match self.settle(index, price) {
                Ok(()) => {
                    self.held.remove(index);
                    let _ = self.trader.send(TraderJob::Liquidated { position_id: id });
                }
                Err(error) => eprintln!("maker {}: liquidating {id}: {error:#}", self.spec.name),
            }
        }
    }

    fn settle(&mut self, index: usize, price: u64) -> Result<()> {
        let held = &self.held[index];
        let transaction = SettleTransactionParams {
            position_utxo: held.position.clone(),
            position_leaf_index: held.leaf,
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
        let signature = self.chain.send(&self.actor.keypair, instruction)?;
        self.chain.indexed(signature)?;
        let id = held.id.clone();
        self.shared.update(|desk| {
            if let Some(position) = desk.positions.iter_mut().find(|position| position.id == id) {
                position.status = "liquidated".into();
                position.settle_price = Some(price.to_string());
                position.payout = Some(payout.to_string());
                position.settled_at = Some(now_ms());
                position.settle_signature = Some(signature.to_string());
            }
            // A maker's settle names only the maker: the trader stays unnamed.
            desk.log(
                "settle",
                &self.view.address,
                None,
                Some(price),
                &["both payouts", "why it could settle"],
                &signature.to_string(),
            );
        });
        Ok(())
    }
}
