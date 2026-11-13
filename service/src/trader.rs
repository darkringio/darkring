//! The trading accounts. Each belongs to someone's browser: the browser
//! holds the key that signs, and hands this process only the account's
//! viewing and nullifier keys, for as long as its session lasts. With those
//! the desk finds the quotes addressed to the account, builds and proves its
//! opens and closes, and sends each transaction back to the browser to sign.
//!
//! One thread serves every account, one job at a time: each job is a proof.

use std::{
    collections::HashMap,
    sync::{
        mpsc::{Receiver, RecvTimeoutError, Sender},
        Arc,
    },
    thread::sleep,
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Result};
use perps_sdk::{
    index::index_trader_quotes,
    instructions::{open::OpenTransactionParams, settle::SettleTransactionParams},
    state::{PositionUtxo, QuoteUtxo},
    ViewKeys,
};
use solana_address::Address;
use solana_keypair::Keypair;
use solana_signature::Signature;
use solana_signer::{Signer, SignerError};
use spr_client::{ComputeBudgetConfig, Rpc};
use spr_keypair::PublicKey;
use spr_test_utils::{
    test_validator_asserts::wait_for_merkle_proof,
    wallet::{sync_wallet, ClientEd25519WalletAuthority, Filter, SyncWalletAuthority, Wallet},
};
use spr_transaction::WalletUtxo;
use spr_user_registry_interface::{
    instruction::{register, RegisterData},
    user_record_pda,
};

use crate::{
    chain::{market_of, now_ms, solana_keypair, Chain, Config, MAX_PRICE_AGE_SECS, USDC},
    desk::{Busy, PositionView, QuoteView, Shared},
    maker::{id_of, MakerJob},
};

/// What the faucet shields, and what a new account starts with.
const FAUCET: u64 = 5_000 * USDC;
/// The faucet does not top up an account that already holds this much.
const FAUCET_CEILING: u64 = 25_000 * USDC;
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
const CLOSE_ATTEMPTS: u8 = 3;
/// How long a browser has to sign a transaction it was sent.
const SIGN_WAIT: Duration = Duration::from_secs(45);
const QUOTE_SEARCH: Duration = Duration::from_secs(20);
/// How far from the mark a market order may open, in hundredths of a percent.
const TAKE_TOLERANCE_BPS: u64 = 100;

pub enum TraderJob {
    /// A browser opened a session for this account.
    Login {
        account: String,
        authority: Box<ClientEd25519WalletAuthority>,
    },
    /// A maker posted this quote for the account to find.
    Discover {
        account: String,
        posted: Box<QuoteUtxo>,
        maker: usize,
        signature: String,
    },
    /// A maker could not quote.
    NoQuote {
        account: String,
        maker: String,
        reason: String,
    },
    Open {
        account: String,
        quote_id: String,
    },
    Close {
        account: String,
        position_id: String,
    },
    Faucet {
        account: String,
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
            | Self::Discover { account, .. }
            | Self::NoQuote { account, .. }
            | Self::Open { account, .. }
            | Self::Close { account, .. }
            | Self::Faucet { account }
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
    leaf: u64,
    maker: usize,
}

/// One account with a session open: its keys, and what it holds.
struct Seat {
    /// The account's Solana address, which is also its name on the desk.
    account: String,
    pubkey: Address,
    keys: ViewKeys,
    authority: ClientEd25519WalletAuthority,
    wallet: Wallet,
    signer: BrowserSigner,
    /// Each live quote, its leaf in the tree and its maker.
    quotes: HashMap<String, (QuoteUtxo, u64, usize)>,
    positions: HashMap<String, Position>,
    last_faucet: Option<Instant>,
}

impl Seat {
    /// Decrypts whatever arrived since the last call and returns the
    /// private balance.
    fn sync(&mut self, chain: &Chain) -> Result<u64> {
        sync_wallet(&mut self.wallet, &self.authority, &chain.client)
            .map_err(|e| anyhow!("sync wallet: {e:?}"))?;
        Ok(self.wallet.balance(chain.mint, None)?.amount)
    }

    /// One note worth at least `amount`: a transaction here spends one note.
    fn note(&self, chain: &Chain, amount: u64) -> Result<WalletUtxo> {
        self.wallet
            .balance(chain.mint, Some(Filter::MinAmount(amount)))?
            .utxos
            .first()
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "no single note in your balance covers {:.2}; the balance is split across smaller notes",
                    amount as f64 / USDC as f64
                )
            })
    }
}

struct Trader {
    chain: Chain,
    deployer: Keypair,
    shared: Arc<Shared>,
    makers: Vec<Sender<MakerJob>>,
    seats: HashMap<String, Seat>,
    /// When the desk last paid an account's fees, for the hourly limit.
    grants: Vec<Instant>,
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
            (Err(error), false) => eprintln!("trader: {error:#}"),
            (Ok(()), false) => {}
        }
    }
}

impl Trader {
    fn boot(config: &Config, shared: &Arc<Shared>, makers: &[Sender<MakerJob>]) -> Result<Self> {
        let chain = Chain::connect(config)?;
        let deployer = solana_keypair(config, "devnet-deployer.json")?;
        let mut trader = Self {
            chain,
            deployer,
            shared: Arc::clone(shared),
            makers: makers.to_vec(),
            seats: HashMap::new(),
            grants: Vec::new(),
        };
        trader.tick();
        Ok(trader)
    }

    fn handle(&mut self, job: TraderJob) -> Result<()> {
        match job {
            TraderJob::Login { account, authority } => self.login(&account, *authority),
            TraderJob::Discover {
                account,
                posted,
                maker,
                signature,
            } => {
                let outcome = self.with_seat(&account, |trader, seat| {
                    trader.discover(seat, &posted, maker, &signature)
                });
                self.shared.lock().book(&account).quote_answered();
                // One maker failing to reach the account is not the request
                // failing: the others may still answer.
                if let Err(error) = &outcome {
                    eprintln!("trader: discovering a quote: {error:#}");
                }
                self.take_best(&account);
                Ok(())
            }
            TraderJob::NoQuote {
                account,
                maker,
                reason,
            } => {
                {
                    let mut desk = self.shared.lock();
                    let book = desk.book(&account);
                    book.quote_answered();
                    if book.awaiting_quotes == 0 && book.quotes.is_empty() {
                        book.error = Some(format!("{maker} could not quote: {reason}"));
                    }
                }
                self.take_best(&account);
                Ok(())
            }
            TraderJob::Open { account, quote_id } => {
                self.with_seat(&account, |trader, seat| trader.open(seat, &quote_id))
            }
            TraderJob::Close {
                account,
                position_id,
            } => self.with_seat(&account, |trader, seat| trader.close(seat, &position_id)),
            TraderJob::Faucet { account } => {
                self.with_seat(&account, |trader, seat| trader.faucet(seat))
            }
            TraderJob::Liquidated {
                account,
                position_id,
            } => {
                if !self.seats.contains_key(&account) {
                    return Ok(());
                }
                self.with_seat(&account, |trader, seat| {
                    seat.positions.remove(&position_id);
                    trader.refresh_balance(seat)
                })
            }
            TraderJob::Recovered {
                account,
                position_id,
                position,
                leaf,
                maker,
            } => {
                if let Some(seat) = self.seats.get_mut(&account) {
                    seat.positions.insert(
                        position_id,
                        Position {
                            utxo: *position,
                            leaf,
                            maker,
                        },
                    );
                }
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
    fn login(&mut self, account: &str, authority: ClientEd25519WalletAuthority) -> Result<()> {
        if self.seats.contains_key(account) {
            // The same account in another tab: it is already seated.
            return self.with_seat(account, |trader, seat| trader.refresh_balance(seat));
        }
        let address = authority
            .shielded_address()
            .map_err(|e| anyhow!("account address: {e:?}"))?;
        let pubkey = authority.solana_pubkey();
        let keys = ViewKeys {
            address,
            nullifier_key: authority
                .spend_nullifier_key()
                .map_err(|e| anyhow!("account keys: {e:?}"))?,
            viewing_key: authority
                .viewing_keys()
                .map_err(|e| anyhow!("account keys: {e:?}"))?
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("the account has no viewing key"))?,
        };
        let wallet = Wallet::new(address, self.chain.assets.clone())
            .map_err(|e| anyhow!("account wallet: {e:?}"))?;
        let mut seat = Seat {
            account: account.into(),
            pubkey,
            keys,
            authority,
            wallet,
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
        let balance = seat.sync(&self.chain)?;
        let is_new = {
            let mut desk = self.shared.lock();
            let book = desk.book(account);
            book.positions.is_empty() && book.ledger.is_empty()
        };
        if balance == 0 && is_new {
            self.shared.label(account, "Adding test dollars");
            self.chain.shield(&self.deployer, &address, FAUCET)?;
            seat.last_faucet = Some(Instant::now());
        }
        self.refresh_balance(&mut seat)?;
        // Whatever the account left open is with the makers.
        for maker in &self.makers {
            let _ = maker.send(MakerJob::Resend {
                account: account.into(),
            });
        }
        self.seats.insert(account.into(), seat);
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
                nullifier_pubkey: seat.keys.address.nullifier_pubkey,
                viewing_pubkey: *seat.keys.address.viewing_pubkey.as_bytes(),
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

    fn refresh_balance(&mut self, seat: &mut Seat) -> Result<()> {
        let balance = seat.sync(&self.chain)?;
        self.shared.lock().book(&seat.account).balance = balance.to_string();
        Ok(())
    }

    /// Reads the oracles, drops the quotes whose time is up, and unseats
    /// the accounts whose last session has closed: their keys go with them.
    fn tick(&mut self) {
        let marks = self.chain.marks();
        let now = now_ms();
        let mut desk = self.shared.lock();
        if let Ok(marks) = marks {
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
        }
        for account in gone {
            self.seats.remove(&account);
            let book = desk.book(&account);
            book.quotes.clear();
            book.busy = None;
            book.sign = None;
            book.error = None;
            println!("trader: unseated {account}");
        }
    }

    /// Finds `posted` the way any trader would: by scanning the indexer for
    /// quotes encrypted to this account, and checking the maker each names
    /// against the user registry.
    fn discover(
        &mut self,
        seat: &mut Seat,
        posted: &QuoteUtxo,
        maker: usize,
        signature: &str,
    ) -> Result<()> {
        let client = &self.chain.client;
        let quote = index_trader_quotes(
            &seat.keys,
            &seat.wallet.registry,
            client.indexer(),
            client.rpc(),
            QUOTE_SEARCH,
        )?
        .into_iter()
        .find(|quote| &quote.quote_utxo == posted)
        .ok_or_else(|| anyhow!("the quote is not among those addressed to this account"))?;
        let terms = &quote.quote_utxo.terms;
        let market = market_of(&terms.feed_id)?;
        let id = id_of(&quote.quote_utxo.hash(self.chain.tree_id)?);
        let mut desk = self.shared.lock();
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
            maker_margin: quote.quote_utxo.maker_margin.to_string(),
            maintenance_margin: terms.maintenance_margin.to_string(),
            expires_at: (terms.expiry as i64 - OPEN_HEADROOM_SECS).max(0) as u64 * 1000,
            signature: signature.into(),
        };
        desk.book(&seat.account).quotes.push(view);
        drop(desk);
        seat.quotes
            .insert(id, (quote.quote_utxo, quote.leaf_index, maker));
        Ok(())
    }

    /// A market order's second half. Once every maker has answered, opens
    /// the quote with the best price, as long as that price is still within
    /// `TAKE_TOLERANCE_BPS` of the mark the account saw when it asked.
    fn take_best(&mut self, account: &str) {
        let chosen = {
            let mut desk = self.shared.lock();
            let book = desk.book(account);
            if book.awaiting_quotes > 0 {
                return;
            }
            let Some(mark) = book.take.take() else {
                return;
            };
            let price = |quote: &QuoteView| quote.entry_price.parse::<u64>().unwrap_or(0);
            let margin = |quote: &QuoteView| quote.maker_margin.parse::<u64>().unwrap_or(0);
            // The best price for a long is the lowest, for a short the
            // highest; between equal prices, the higher profit cap.
            let best = book.quotes.iter().max_by_key(|quote| {
                let price = price(quote);
                let rank = if quote.side == "long" {
                    u64::MAX - price
                } else {
                    price
                };
                (rank, margin(quote))
            });
            let Some(best) = best else {
                book.busy = None;
                if book.error.is_none() {
                    book.error = Some("no maker answered; try again".into());
                }
                return;
            };
            if price(best).abs_diff(mark) > mark / 10_000 * TAKE_TOLERANCE_BPS {
                book.busy = None;
                book.error = Some(
                    "the price moved while the makers were quoting, so nothing was opened; \
                     their offers are below if you still want one"
                        .into(),
                );
                return;
            }
            let id = best.id.clone();
            book.busy = Some(Busy {
                quote_id: Some(id.clone()),
                ..Busy::new("take", &format!("Opening with {}", best.maker.name))
            });
            id
        };
        match self.with_seat(account, |trader, seat| trader.open(seat, &chosen)) {
            Ok(()) => {
                let mut desk = self.shared.lock();
                let book = desk.book(account);
                book.busy = None;
                book.sign = None;
            }
            Err(error) => self.shared.fail(account, &error),
        }
    }

    /// Accepts a quote: both margins move into one private position. The
    /// account's other quotes go off its book: they were offers for the same
    /// trade, and their makers take the margin back when they expire.
    fn open(&mut self, seat: &mut Seat, quote_id: &str) -> Result<()> {
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
        seat.sync(&self.chain)?;
        let trader_input = seat.note(&self.chain, quote_utxo.terms.trader_margin)?;
        let transaction = OpenTransactionParams {
            quote_utxo,
            quote_leaf_index,
            trader_input,
            payer: seat.pubkey,
            tree_id: self.chain.tree_id,
        }
        .build(&seat.keys)?;
        let opened = transaction.position_utxo.clone();
        let client = &self.chain.client;
        let instruction = transaction
            .prove(client.indexer(), self.chain.tree, &seat.keys)?
            .instruction()?;
        self.shared.label(&seat.account, "Sending to Solana");
        let signature = self.chain.send(&seat.signer, instruction)?;
        self.chain.indexed(signature)?;
        let hash = opened.hash(self.chain.tree_id)?;
        let leaf = wait_for_merkle_proof(client.indexer(), self.chain.tree, hash).leaf_index;
        let id = id_of(&hash);

        self.shared.update(|desk| {
            desk.closed_quotes.push(quote_id.into());
            let book = desk.book(&seat.account);
            book.quotes.clear();
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
        if let Some(sender) = self.makers.get(maker) {
            let _ = sender.send(MakerJob::Opened {
                account: seat.account.clone(),
                quote_id: quote_id.into(),
                position_id: id.clone(),
                position: Box::new(opened.clone()),
                leaf,
            });
        }
        seat.positions.insert(
            id,
            Position {
                utxo: opened,
                leaf,
                maker,
            },
        );
        self.refresh_balance(seat)
    }

    /// Closes at whatever the oracle account reads. The proof commits to one
    /// price, so a close that lands just after Pyth updated the account is
    /// proved again at the new price.
    fn close(&mut self, seat: &mut Seat, position_id: &str) -> Result<()> {
        let (utxo, leaf, maker) = {
            let position = seat
                .positions
                .get(position_id)
                .ok_or_else(|| anyhow!("that position is not open"))?;
            (position.utxo.clone(), position.leaf, position.maker)
        };
        let market = market_of(&utxo.terms.feed_id)?;
        let mut attempt = 0;
        let (price, payout, signature) = loop {
            attempt += 1;
            let mark = self.chain.settle_mark(market)?;
            let price = mark.price;
            let transaction = SettleTransactionParams {
                position_utxo: utxo.clone(),
                position_leaf_index: leaf,
                oracle: mark.account,
                price,
                payer: seat.pubkey,
                tree_id: self.chain.tree_id,
            }
            .build(&seat.keys)?;
            let payout = transaction.trader_payout;
            let instruction = transaction
                .prove(self.chain.client.indexer(), self.chain.tree)?
                .instruction()?;
            self.shared.label(&seat.account, "Sending to Solana");
            match self.chain.send(&seat.signer, instruction) {
                Ok(signature) => break (price, payout, signature),
                Err(error) if attempt < CLOSE_ATTEMPTS => {
                    eprintln!("trader: close attempt {attempt} at {price}: {error:#}");
                    self.shared.label(&seat.account, "Proving the close again");
                }
                Err(error) => return Err(error),
            }
        };
        self.chain.indexed(signature)?;

        self.shared.update(|desk| {
            let book = desk.book(&seat.account);
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
        self.refresh_balance(seat)
    }

    fn faucet(&mut self, seat: &mut Seat) -> Result<()> {
        if seat.sync(&self.chain)? >= FAUCET_CEILING {
            bail!("you already have plenty of test dollars");
        }
        if seat
            .last_faucet
            .is_some_and(|at| at.elapsed() < FAUCET_COOLDOWN)
        {
            bail!("the faucet just paid you; give it a minute");
        }
        self.chain
            .shield(&self.deployer, &seat.keys.address, FAUCET)?;
        seat.last_faucet = Some(Instant::now());
        self.refresh_balance(seat)
    }
}

pub const fn max_price_age() -> i64 {
    MAX_PRICE_AGE_SECS
}
