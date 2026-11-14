//! The trading accounts. Each belongs to someone's browser, which holds
//! every key of the account: the one that signs, and the viewing and
//! nullifier keys its notes are read and spent with. The browser reads its
//! own balance, builds each open and close, and proves it. This process is
//! handed the finished proofs, which are public the moment they land. It
//! puts them in the program's instruction, sends the transaction back to
//! the browser to sign, and sends it to Solana.
//!
//! So of a trader, the desk knows what its makers know as the other side of
//! the trade, and nothing more: not the balance, not the other notes.
//!
//! One thread serves every account, one job at a time.

use std::{
    collections::HashMap,
    sync::{
        mpsc::{Receiver, RecvTimeoutError, Sender},
        Arc,
    },
    thread::sleep,
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, ensure, Result};
use perps_sdk::{
    instructions::{open::Open, settle::Settle},
    state::{PositionUtxo, QuoteUtxo},
    OpenProof, SettleProof,
};
use solana_address::Address;
use solana_keypair::Keypair;
use solana_signature::Signature;
use solana_signer::{Signer, SignerError};
use spr_client::{ComputeBudgetConfig, Rpc};
use spr_interface::instruction::instruction_data::transact::TransactIxData;
use spr_keypair::{PublicKey, ShieldedAddress};
use spr_user_registry_interface::{
    instruction::{register, RegisterData},
    user_record_pda,
};

use crate::{
    chain::{market_of, now_ms, solana_keypair, Chain, Config, Laps, MAX_PRICE_AGE_SECS, USDC},
    desk::{PositionView, QuoteView, Shared},
    maker::{id_of, MakerJob},
    opening::opening,
    usage,
};

/// What the faucet shields, and what a new account starts with.
const FAUCET: u64 = 5_000 * USDC;
const FAUCET_COOLDOWN: Duration = Duration::from_secs(60);
/// Devnet SOL for transaction fees: an account below the floor is sent the
/// grant, which pays for about five hundred transactions.
const FEE_FLOOR: u64 = 1_000_000;
const FEE_GRANT: u64 = 5_000_000;
/// How many accounts the desk pays fees for in an hour.
const GRANTS_PER_HOUR: usize = 120;
/// The desk stops offering a quote this long before it expires on chain: an
/// open has to be proved and land first.
const OPEN_HEADROOM_SECS: i64 = 40;
const TICK: Duration = Duration::from_secs(3);
/// How long a browser has to sign a transaction it was sent.
const SIGN_WAIT: Duration = Duration::from_secs(45);

pub enum TraderJob {
    /// A browser opened a session for this account.
    Login {
        account: String,
        address: Box<ShieldedAddress>,
    },
    /// A maker posted this quote for the account, at `leaf` of the tree.
    Posted {
        account: String,
        posted: Box<QuoteUtxo>,
        leaf: u64,
        maker: usize,
        signature: String,
    },
    /// A maker could not quote.
    NoQuote {
        account: String,
        maker: String,
        reason: String,
    },
    /// The account's browser proved the open of this quote.
    Open {
        account: String,
        quote_id: String,
        proof: Box<OpenProof>,
        transact: Box<TransactIxData>,
    },
    /// The account's browser proved the close of this position at `price`,
    /// the price it read in `oracle`.
    Close {
        account: String,
        position_id: String,
        oracle: Address,
        price: u64,
        proof: Box<SettleProof>,
        transact: Box<TransactIxData>,
    },
    Faucet {
        account: String,
    },
    /// An open has landed: find the position's leaf in the tree, which the
    /// account's browser needs to close it and its maker to settle it.
    Located {
        account: String,
        quote_id: String,
        position_id: String,
        position: Box<PositionUtxo>,
        maker: usize,
    },
    /// A maker settled this position.
    Liquidated {
        account: String,
        position_id: String,
    },
    /// A maker holds this position of the account's, open on chain.
    Recovered {
        account: String,
        position_id: String,
        position: Box<PositionUtxo>,
        leaf: u64,
        maker: usize,
    },
}

impl TraderJob {
    fn account(&self) -> &str {
        match self {
            Self::Login { account, .. }
            | Self::Posted { account, .. }
            | Self::NoQuote { account, .. }
            | Self::Open { account, .. }
            | Self::Close { account, .. }
            | Self::Faucet { account }
            | Self::Located { account, .. }
            | Self::Liquidated { account, .. }
            | Self::Recovered { account, .. } => account,
        }
    }

    /// Whether the job is something the account asked for and is waiting on.
    fn is_action(&self) -> bool {
        matches!(
            self,
            Self::Login { .. } | Self::Open { .. } | Self::Close { .. } | Self::Faucet { .. }
        )
    }
}

/// An account's signing key, as far as this process ever sees it: a request
/// put where the account's browser will find it, and a wait for the answer.
struct BrowserSigner {
    shared: Arc<Shared>,
    account: String,
    pubkey: Address,
}

impl Signer for BrowserSigner {
    fn try_pubkey(&self) -> Result<Address, SignerError> {
        Ok(self.pubkey)
    }

    fn try_sign_message(&self, message: &[u8]) -> Result<Signature, SignerError> {
        let id = self.shared.request_signature(&self.account, message);
        let deadline = Instant::now() + SIGN_WAIT;
        loop {
            if let Some(signature) = self.shared.signature(&self.account, id) {
                let key = PublicKey::from_ed25519(self.pubkey.as_array());
                if !key.verify_message(message, &signature) {
                    return Err(SignerError::Custom(
                        "the browser signed with another key; connect your wallet again".into(),
                    ));
                }
                return Ok(Signature::from(signature));
            }
            if Instant::now() >= deadline {
                self.shared.withdraw_request(&self.account, id);
                return Err(SignerError::Custom(
                    "the browser did not sign in time; keep this tab open and try again".into(),
                ));
            }
            sleep(Duration::from_millis(40));
        }
    }

    fn is_interactive(&self) -> bool {
        true
    }
}

struct Position {
    utxo: PositionUtxo,
    maker: usize,
}

/// One account with a session open. No key of the account's is here.
struct Seat {
    /// The account's Solana address, which is also its name on the desk.
    account: String,
    pubkey: Address,
    /// The account's shielded address: what a maker encrypts a quote to.
    address: ShieldedAddress,
    signer: BrowserSigner,
    /// Each live quote, its leaf in the tree and its maker.
    quotes: HashMap<String, (QuoteUtxo, u64, usize)>,
    positions: HashMap<String, Position>,
    last_faucet: Option<Instant>,
}

struct Trader {
    chain: Chain,
    deployer: Keypair,
    shared: Arc<Shared>,
    makers: Vec<Sender<MakerJob>>,
    /// This thread's own queue, for work that follows an action.
    own: Sender<TraderJob>,
    seats: HashMap<String, Seat>,
    /// When the desk last paid an account's fees, for the hourly limit.
    grants: Vec<Instant>,
    /// When the oracles were last read.
    ticked: Instant,
}

pub fn run(
    config: Config,
    shared: Arc<Shared>,
    jobs: Receiver<TraderJob>,
    own: Sender<TraderJob>,
    makers: Vec<Sender<MakerJob>>,
) {
    let mut trader = loop {
        match Trader::boot(&config, &shared, &own, &makers) {
            Ok(trader) => break trader,
            Err(error) => {
                eprintln!(
                    "trader: {}; trying again",
                    usage::scrub(&format!("{error:#}"))
                );
                usage::note(&error);
                sleep(Duration::from_secs(15).max(usage::rest_left()));
            }
        }
    };
    // Connecting took answers from the provider: any rest is over.
    usage::recovered();
    shared.lock().booted();
    println!("trader: ready");
    loop {
        let job = match jobs.recv_timeout(TICK) {
            Ok(job) => job,
            Err(RecvTimeoutError::Timeout) => {
                if trader.due() {
                    trader.tick();
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let account = job.account().to_owned();
        let action = job.is_action();
        match (trader.handle(job), action) {
            (Ok(()), true) => {
                let mut desk = shared.lock();
                let book = desk.book(&account);
                book.busy = None;
                book.sign = None;
            }
            (Err(error), true) => shared.fail(&account, &error),
            (Err(error), false) => {
                eprintln!("trader: {}", usage::scrub(&format!("{error:#}")));
                usage::note(&error);
            }
            (Ok(()), false) => {}
        }
        // A browser proves a close at the price it reads here, so the price
        // must not go stale while the jobs keep coming.
        if trader.due() {
            trader.tick();
        }
    }
}

impl Trader {
    fn boot(
        config: &Config,
        shared: &Arc<Shared>,
        own: &Sender<TraderJob>,
        makers: &[Sender<MakerJob>],
    ) -> Result<Self> {
        let chain = Chain::connect(config)?;
        let deployer = solana_keypair(config, "devnet-deployer.json")?;
        let mut trader = Self {
            chain,
            deployer,
            shared: Arc::clone(shared),
            makers: makers.to_vec(),
            own: own.clone(),
            seats: HashMap::new(),
            grants: Vec::new(),
            ticked: Instant::now(),
        };
        {
            let mut desk = trader.shared.lock();
            desk.mint = trader.chain.mint.to_string();
            desk.asset_id = trader.chain.asset_id;
        }
        trader.tick();
        Ok(trader)
    }

    fn handle(&mut self, job: TraderJob) -> Result<()> {
        match job {
            TraderJob::Login { account, address } => self.login(&account, *address),
            TraderJob::Posted {
                account,
                posted,
                leaf,
                maker,
                signature,
            } => {
                let outcome = match self.seats.get_mut(&account) {
                    Some(seat) => Self::posted(
                        &self.chain,
                        &self.shared,
                        seat,
                        *posted,
                        leaf,
                        maker,
                        &signature,
                    ),
                    // The account left while the maker was quoting.
                    None => Ok(()),
                };
                self.shared.lock().book(&account).quote_answered();
                outcome
            }
            TraderJob::NoQuote {
                account,
                maker,
                reason,
            } => {
                let mut desk = self.shared.lock();
                let book = desk.book(&account);
                book.quote_answered();
                if book.awaiting_quotes == 0 && book.quotes.is_empty() {
                    book.error = Some(if usage::refused(&reason) {
                        usage::REFUSED.to_owned()
                    } else {
                        format!("{maker} could not quote: {}", usage::scrub(&reason))
                    });
                }
                Ok(())
            }
            TraderJob::Open {
                account,
                quote_id,
                proof,
                transact,
            } => self.with_seat(&account, |trader, seat| {
                trader.open(seat, &quote_id, *proof, *transact)
            }),
            TraderJob::Close {
                account,
                position_id,
                oracle,
                price,
                proof,
                transact,
            } => self.with_seat(&account, |trader, seat| {
                trader.close(seat, &position_id, oracle, price, *proof, *transact)
            }),
            TraderJob::Faucet { account } => {
                self.with_seat(&account, |trader, seat| trader.faucet(seat))
            }
            TraderJob::Located {
                account,
                quote_id,
                position_id,
                position,
                maker,
            } => {
                let hash = position.hash(self.chain.tree_id)?;
                let leaf = self.chain.leaf(hash)?;
                // The account may have left meanwhile: its maker still has
                // to know, and hands the opening back on the next session.
                if self.seats.contains_key(&account) {
                    let input = position.to_input_utxo(self.chain.tree_id, leaf)?;
                    self.shared
                        .lock()
                        .book(&account)
                        .openings
                        .insert(position_id.clone(), opening(&input, &position.terms.maker));
                }
                if let Some(sender) = self.makers.get(maker) {
                    let _ = sender.send(MakerJob::Opened {
                        account,
                        quote_id,
                        position_id,
                        position,
                        leaf,
                    });
                }
                Ok(())
            }
            TraderJob::Liquidated {
                account,
                position_id,
            } => {
                if let Some(seat) = self.seats.get_mut(&account) {
                    seat.positions.remove(&position_id);
                }
                self.shared
                    .lock()
                    .book(&account)
                    .openings
                    .remove(&position_id);
                Ok(())
            }
            TraderJob::Recovered {
                account,
                position_id,
                position,
                leaf,
                maker,
            } => {
                let Some(seat) = self.seats.get_mut(&account) else {
                    return Ok(());
                };
                let input = position.to_input_utxo(self.chain.tree_id, leaf)?;
                self.shared
                    .lock()
                    .book(&account)
                    .openings
                    .insert(position_id.clone(), opening(&input, &position.terms.maker));
                seat.positions.insert(
                    position_id,
                    Position {
                        utxo: *position,
                        maker,
                    },
                );
                Ok(())
            }
        }
    }

    /// Runs `act` on `account`'s seat, which is out of the table meanwhile.
    fn with_seat(
        &mut self,
        account: &str,
        act: impl FnOnce(&mut Self, &mut Seat) -> Result<()>,
    ) -> Result<()> {
        let mut seat = self
            .seats
            .remove(account)
            .ok_or_else(|| anyhow!("your session has ended; connect your wallet again"))?;
        let outcome = act(self, &mut seat);
        self.seats.insert(account.into(), seat);
        outcome
    }

    /// Seats an account: pays its first fees, publishes its address so
    /// makers can find its positions again, and funds a new one.
    fn login(&mut self, account: &str, address: ShieldedAddress) -> Result<()> {
        if self.seats.contains_key(account) {
            // The same account in another tab: it is already seated.
            return Ok(());
        }
        let pubkey = address
            .solana_address()
            .map_err(|e| anyhow!("account address: {e:?}"))?;
        let mut seat = Seat {
            account: account.into(),
            pubkey,
            address,
            signer: BrowserSigner {
                shared: Arc::clone(&self.shared),
                account: account.into(),
                pubkey,
            },
            quotes: HashMap::new(),
            positions: HashMap::new(),
            last_faucet: None,
        };
        self.grant_fees(&seat)?;
        self.register(&seat)?;
        // The desk cannot read the account's balance, so it remembers which
        // accounts it has funded.
        let funded = self.shared.lock().book(account).funded;
        if !funded {
            self.shared.label(account, "Adding test dollars");
            self.chain.shield(&self.deployer, &address, FAUCET)?;
            seat.last_faucet = Some(Instant::now());
            self.shared.update(|desk| desk.book(account).funded = true);
        }
        self.seats.insert(account.into(), seat);
        // Whatever the account left open is with the makers.
        for maker in &self.makers {
            let _ = maker.send(MakerJob::Resend {
                account: account.into(),
            });
        }
        println!("trader: seated {account}");
        Ok(())
    }

    /// Sends the account devnet SOL for fees when it is short of it.
    fn grant_fees(&mut self, seat: &Seat) -> Result<()> {
        if self.chain.client.rpc().get_balance(seat.pubkey)? >= FEE_FLOOR {
            return Ok(());
        }
        self.grants
            .retain(|at| at.elapsed() < Duration::from_secs(3600));
        if self.grants.len() >= GRANTS_PER_HOUR {
            bail!("the desk has set up all the new accounts it can this hour; try again later");
        }
        self.chain
            .transfer_sol(&self.deployer, seat.pubkey, FEE_GRANT)?;
        self.grants.push(Instant::now());
        Ok(())
    }

    /// Publishes the account's address in the user registry, where a maker
    /// looks up the trader a position names. The account signs it.
    fn register(&self, seat: &Seat) -> Result<()> {
        let (record, _) = user_record_pda(&seat.pubkey);
        if self.chain.client.rpc().get_account(record)?.is_some() {
            return Ok(());
        }
        self.shared.label(&seat.account, "Registering your account");
        let instruction = register(
            record,
            seat.pubkey,
            seat.pubkey,
            RegisterData {
                nullifier_pubkey: seat.address.nullifier_pubkey,
                viewing_pubkey: *seat.address.viewing_pubkey.as_bytes(),
            },
        );
        self.chain.client.rpc().create_and_send_transaction(
            std::slice::from_ref(&instruction),
            seat.pubkey,
            &[&seat.signer],
            ComputeBudgetConfig::for_instruction_count(1),
        )?;
        Ok(())
    }

    /// Whether it is time to read the oracles again. They are read every
    /// few seconds while somebody trades or watches, and seldom while the
    /// desk is empty: every read counts against the provider's limit.
    fn due(&self) -> bool {
        let pace = {
            let desk = self.shared.lock();
            usage::pace(desk.trading(), desk.watched(), desk.holding())
        };
        self.ticked.elapsed() >= pace
    }

    /// Reads the oracles, drops the quotes whose time is up, and unseats
    /// the accounts whose last session has closed.
    fn tick(&mut self) {
        self.ticked = Instant::now();
        // While the provider refuses, asking again only prolongs it.
        let marks = if usage::resting() {
            None
        } else {
            match self.chain.marks() {
                Ok(marks) => {
                    usage::recovered();
                    Some(marks)
                }
                Err(error) => {
                    usage::note(&error);
                    None
                }
            }
        };
        let now = now_ms();
        let mut desk = self.shared.lock();
        if let Some(marks) = marks {
            for (view, mark) in desk.markets.iter_mut().zip(marks) {
                if let Some(mark) = mark {
                    view.price = mark.price.to_string();
                    view.publish_time = mark.publish_time;
                    view.oracle = mark.account.to_string();
                }
            }
        }
        desk.expire_sessions();
        let mut gone = Vec::new();
        for (account, seat) in &mut self.seats {
            if !desk.has_session(account) {
                gone.push(account.clone());
                continue;
            }
            let book = desk.book(account);
            book.quotes.retain(|quote| quote.expires_at > now);
            let live: Vec<&str> = book.quotes.iter().map(|quote| quote.id.as_str()).collect();
            seat.quotes.retain(|id, _| live.contains(&id.as_str()));
            book.openings
                .retain(|id, _| seat.quotes.contains_key(id) || seat.positions.contains_key(id));
        }
        for account in gone {
            self.seats.remove(&account);
            let book = desk.book(&account);
            book.quotes.clear();
            book.openings.clear();
            book.busy = None;
            book.sign = None;
            book.error = None;
            println!("trader: unseated {account}");
        }
    }

    /// Puts a quote a maker posted on the account's book, with the opening
    /// the account's browser needs to accept it.
    fn posted(
        chain: &Chain,
        shared: &Shared,
        seat: &mut Seat,
        posted: QuoteUtxo,
        leaf: u64,
        maker: usize,
        signature: &str,
    ) -> Result<()> {
        let terms = &posted.terms;
        let market = market_of(&terms.feed_id)?;
        let input = posted.to_input_utxo(chain.tree_id, leaf)?;
        let id = id_of(&input.utxo_hash);
        let mut desk = shared.lock();
        let view = QuoteView {
            id: id.clone(),
            market: market.symbol.into(),
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
            maker_margin: posted.maker_margin.to_string(),
            maintenance_margin: terms.maintenance_margin.to_string(),
            expires_at: (terms.expiry as i64 - OPEN_HEADROOM_SECS).max(0) as u64 * 1000,
            signature: signature.into(),
        };
        let book = desk.book(&seat.account);
        book.quotes.push(view);
        book.openings
            .insert(id.clone(), opening(&input, &terms.maker));
        drop(desk);
        seat.quotes.insert(id, (posted, leaf, maker));
        Ok(())
    }

    /// Sends the open the account's browser proved: both margins move into
    /// one private position. The account's other quotes go off its book:
    /// they were offers for the same trade, and their makers take the margin
    /// back when they expire.
    fn open(
        &mut self,
        seat: &mut Seat,
        quote_id: &str,
        proof: OpenProof,
        transact: TransactIxData,
    ) -> Result<()> {
        let expired = || anyhow!("that quote has expired");
        let (quote_utxo, quote_leaf_index, maker) =
            seat.quotes.get(quote_id).cloned().ok_or_else(expired)?;
        let view = self
            .shared
            .lock()
            .book(&seat.account)
            .quotes
            .iter()
            .find(|view| view.id == quote_id)
            .cloned()
            .ok_or_else(expired)?;
        let terms = &quote_utxo.terms;
        // The quote is the open's first input, and the open circuit fixes
        // the position to the quote's opening and that nullifier: the desk
        // knows the position without reading anything of the trader's.
        let first_nullifier = quote_utxo
            .to_input_utxo(self.chain.tree_id, quote_leaf_index)?
            .nullifier;
        let opened = quote_utxo.derived_position(seat.address, &first_nullifier)?;
        let instruction = Open {
            payer: seat.pubkey,
            maker: terms
                .maker
                .solana_address()
                .map_err(|e| anyhow!("maker address: {e:?}"))?,
            viewing_pubkey: *terms.maker.viewing_pubkey.as_bytes(),
            tree: self.chain.tree,
            open_proof: proof,
            spp_proof: transact,
        }
        .instruction()?;
        self.shared.label(&seat.account, "Sending to Solana");
        let mut lap = Laps::start();
        let signature = self.chain.send(&seat.signer, instruction)?;
        lap.mark("sign and send");
        println!("trader: open sent in {lap}");
        // The position is open from here. Where it sits in the tree is
        // looked up next (`Located`), without keeping the trader waiting.
        let id = id_of(&opened.hash(self.chain.tree_id)?);

        self.shared.update(|desk| {
            desk.closed_quotes.push(quote_id.into());
            let book = desk.book(&seat.account);
            book.quotes.clear();
            book.openings
                .retain(|held, _| seat.positions.contains_key(held));
            let frame = book.positions.len() as u32 + 1;
            book.positions.push(PositionView {
                id: id.clone(),
                frame,
                market: view.market.clone(),
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
            book.log(
                "open",
                &seat.account,
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
        seat.quotes.clear();
        let _ = self.own.send(TraderJob::Located {
            account: seat.account.clone(),
            quote_id: quote_id.into(),
            position_id: id.clone(),
            position: Box::new(opened.clone()),
            maker,
        });
        seat.positions.insert(
            id,
            Position {
                utxo: opened,
                maker,
            },
        );
        Ok(())
    }

    /// Sends the close the account's browser proved. The proof commits to
    /// one price, the one the browser read in `oracle`; the program reads
    /// the same account, so a close that lands just after Pyth updated it
    /// fails there, and the browser proves again at the new price.
    fn close(
        &mut self,
        seat: &mut Seat,
        position_id: &str,
        oracle: Address,
        price: u64,
        proof: SettleProof,
        transact: TransactIxData,
    ) -> Result<()> {
        let (utxo, maker) = {
            let position = seat
                .positions
                .get(position_id)
                .ok_or_else(|| anyhow!("that position is not open"))?;
            (position.utxo.clone(), position.maker)
        };
        let market = market_of(&utxo.terms.feed_id)?;
        ensure!(
            market.oracles.contains(&oracle),
            "that is not a price account of {}",
            market.symbol
        );
        let payout = utxo.trader_payout(price);
        let instruction = Settle {
            payer: seat.pubkey,
            signer: seat.pubkey,
            oracle,
            maker: utxo
                .terms
                .maker
                .solana_address()
                .map_err(|e| anyhow!("maker address: {e:?}"))?,
            tree: self.chain.tree,
            settle_proof: proof,
            spp_proof: transact,
        }
        .instruction()?;
        self.shared.label(&seat.account, "Sending to Solana");
        let mut lap = Laps::start();
        let signature = self.chain.send(&seat.signer, instruction)?;
        lap.mark("sign and send");
        println!("trader: close sent in {lap}");

        self.shared.update(|desk| {
            let book = desk.book(&seat.account);
            book.openings.remove(position_id);
            let mut counterparty = None;
            if let Some(position) = book
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
            book.log(
                "settle",
                &seat.account,
                counterparty.as_deref(),
                Some((market.symbol, price)),
                &["both payouts", "profit or loss"],
                &signature.to_string(),
            );
        });
        seat.positions.remove(position_id);
        if let Some(sender) = self.makers.get(maker) {
            let _ = sender.send(MakerJob::Closed {
                position_id: position_id.into(),
            });
        }
        Ok(())
    }

    /// Shields more test dollars to the account. The desk cannot see what
    /// the account holds, so the only limit is how often.
    fn faucet(&mut self, seat: &mut Seat) -> Result<()> {
        if seat
            .last_faucet
            .is_some_and(|at| at.elapsed() < FAUCET_COOLDOWN)
        {
            bail!("the faucet just paid you; give it a minute");
        }
        self.chain.shield(&self.deployer, &seat.address, FAUCET)?;
        seat.last_faucet = Some(Instant::now());
        Ok(())
    }
}

pub const fn max_price_age() -> i64 {
    MAX_PRICE_AGE_SECS
}
