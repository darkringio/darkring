//! The devnet desk. Runs three maker wallets, each on a thread of its own
//! against devnet, and serves the web app a small JSON API on localhost.
//!
//! Traders bring their own account, and its keys stay in their browser. The
//! browser reads the account's notes, builds each open and close and proves
//! it; this process is handed the finished proofs, assembles the program's
//! instruction, and sends the transaction back for the browser to sign. A
//! session is opened by a signature of the account's key, and tells this
//! process the account's shielded address, which is public: makers encrypt
//! their quotes to it.
//!
//!   GET  /api/state    the market, and the session's account
//!   POST /api/session  {"address","shielded":"<hex>","at":<ms>,"signature":"<hex>"}
//!   POST /api/logout
//!   POST /api/sign     {"id":7,"signature":"<hex>"}
//!   POST /api/quotes   {"market":"BTC","side":"long","amount":0.01,"leverage":8,"take":false}
//!   POST /api/open     {"quoteId","proof":"<hex>","transact":"<hex>"}
//!   POST /api/close    {"positionId","oracle","price","proof":"<hex>","transact":"<hex>"}
//!   POST /api/faucet   shield another 5,000 test dollars to the account
//!   any  /api/rings…   the Rings indexer and enclave prover at Helius
//!   POST /api/rpc      a Solana node, for reads
//!
//! Everything but the first two needs `Authorization: Bearer <token>`.
//!
//! Needs HELIUS_API_KEY (or DEVNET_RPC_URL and DEVNET_SPR_URL), ZOLANA_CLI_BIN
//! for the local prover the makers use, and the wallets
//! scripts/devnet-setup.sh makes.

mod chain;
mod desk;
mod maker;
mod opening;
mod relay;
mod trader;

use std::{
    env,
    fs::File,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{mpsc, Arc},
    thread,
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use perps_sdk::OpenProof;
use serde_json::{json, Value};
use solana_address::Address;
use spr_client::ProverLaunch;
use spr_interface::instruction::instruction_data::transact::TransactIxData;
use spr_keypair::{PublicKey, ShieldedAddress, ShieldedKeypair, SigningKey};

use crate::{
    chain::{market, markets, Config},
    desk::{hex, unhex, unhex_vec, Busy, Desk, MakerView, MarketView, Shared},
    maker::{MakerJob, MakerSpec},
    trader::TraderJob,
};

/// The three makers. Spread is how far each quotes from the mark; coverage
/// is its margin per ten of the trader's, which caps the trader's profit.
const MAKERS: [(&str, &str, u64, u64); 3] = [
    ("Halden", "devnet-maker.json", 5, 15),
    ("Selwyn", "devnet-maker-2.json", 3, 10),
    ("Amory", "devnet-maker-3.json", 9, 30),
];

/// How far a session request's clock may be from this machine's.
const SESSION_SKEW_MS: u64 = 5 * 60 * 1000;
/// The largest body the API reads, and the largest it relays to Helius.
const MAX_BODY: usize = 64 * 1024;
const MAX_RELAY_BODY: usize = 1024 * 1024;

/// The maker a market order opens with: the tightest spread, then the
/// higher profit cap. The web app's ticket shows the same maker's numbers.
fn best_maker() -> usize {
    (0..MAKERS.len())
        .min_by_key(|&index| (MAKERS[index].2, u64::MAX - MAKERS[index].3))
        .unwrap_or(0)
}

/// The most a position may be worth at entry, in dollars: a bound on what
/// the desk will try to quote, far above what a test balance can open.
const MAX_NOTIONAL_USD: f64 = 5_000_000.0;

fn config() -> Result<Config> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let key = env::var("HELIUS_API_KEY").ok();
    let from_key = |path: &str| {
        key.as_ref()
            .map(|key| format!("https://devnet.helius-rpc.com/{path}?api-key={key}"))
    };
    Ok(Config {
        rpc_url: env::var("DEVNET_RPC_URL")
            .ok()
            .or_else(|| from_key(""))
            .ok_or_else(|| anyhow!("set HELIUS_API_KEY or DEVNET_RPC_URL"))?,
        spr_url: env::var("DEVNET_SPR_URL")
            .ok()
            .or_else(|| from_key("v1/spr"))
            .ok_or_else(|| anyhow!("set HELIUS_API_KEY or DEVNET_SPR_URL"))?,
        keys_dir: env::var_os("DESK_KEYS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("keys")),
        state_file: env::var_os("DESK_STATE_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join(".desk/accounts.json")),
        port: env::var("DESK_PORT")
            .ok()
            .and_then(|port| port.parse().ok())
            .unwrap_or(5374),
    })
}

fn shielded(config: &Config, file: &str) -> Result<ShieldedKeypair> {
    let path = config.keys_dir.join(file);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("{}: run scripts/devnet-setup.sh", path.display()))?;
    let bytes: Vec<u8> = serde_json::from_str(&text)?;
    let seed: [u8; 32] = bytes
        .get(..32)
        .and_then(|seed| seed.try_into().ok())
        .ok_or_else(|| anyhow!("{} is not a keypair file", path.display()))?;
    Ok(ShieldedKeypair::from_keypair(
        SigningKey::from_ed25519_bytes(&seed),
    )?)
}

fn main() -> Result<()> {
    let config = config()?;

    // The makers' pool proofs are made on this machine, by the release
    // prover with the release keys. A trader's are made by Helius's enclave
    // prover, which the trader's browser reaches through `relay`.
    let home = env::var("HOME").context("HOME is not set")?;
    ProverLaunch::new_with_cli(env::var("ZOLANA_CLI_BIN").context("ZOLANA_CLI_BIN is not set")?)?
        .with_keys_dir(Path::new(&home).join(".config/zolana/proving-keys"))?
        .spawn()?;

    let mut makers = Vec::new();
    for (name, file, spread_bps, coverage_tenths) in MAKERS {
        let address = shielded(&config, file)?
            .shielded_address()?
            .solana_address()?;
        makers.push(MakerView {
            name: name.into(),
            address: address.to_string(),
            spread_bps,
            coverage: coverage_tenths as f64 / 10.0,
        });
    }

    let mut desk = Desk::default();
    desk.network = "devnet".into();
    desk.program = perps_program::ID.to_string();
    desk.markets = markets()
        .iter()
        .map(|market| MarketView {
            symbol: market.symbol.into(),
            name: market.name.into(),
            feed: hex(&market.feed),
            expo: market.expo,
            price: "0".into(),
            publish_time: 0,
            oracle: market.oracles[0].to_string(),
        })
        .collect();
    desk.max_price_age = trader::max_price_age();
    desk.makers = makers.clone();
    desk.booting = MAKERS.len() + 1;
    let shared = Arc::new(Shared::load(config.state_file.clone(), desk));

    let (trader_jobs, trader_inbox) = mpsc::channel::<TraderJob>();
    let mut maker_jobs = Vec::new();
    for (index, (name, file, spread_bps, coverage_tenths)) in MAKERS.into_iter().enumerate() {
        let (jobs, inbox) = mpsc::channel::<MakerJob>();
        maker_jobs.push(jobs);
        let (config, shared, trader_jobs) =
            (config.clone(), Arc::clone(&shared), trader_jobs.clone());
        let view = makers[index].clone();
        thread::spawn(move || {
            let spec = MakerSpec {
                index,
                name,
                file,
                spread_bps,
                coverage_tenths,
            };
            maker::run(config, spec, view, shared, inbox, trader_jobs);
        });
    }
    {
        let (config, shared, makers) = (config.clone(), Arc::clone(&shared), maker_jobs.clone());
        let own = trader_jobs.clone();
        thread::spawn(move || trader::run(config, shared, trader_inbox, own, makers));
    }

    let listener = TcpListener::bind(("127.0.0.1", config.port))
        .with_context(|| format!("binding 127.0.0.1:{}", config.port))?;
    println!("desk: listening on http://127.0.0.1:{}", config.port);
    for stream in listener.incoming().flatten() {
        let (shared, trader_jobs, maker_jobs) =
            (Arc::clone(&shared), trader_jobs.clone(), maker_jobs.clone());
        let config = config.clone();
        thread::spawn(move || {
            if let Err(error) = serve(stream, &config, &shared, &trader_jobs, &maker_jobs) {
                eprintln!("desk: request: {error:#}");
            }
        });
    }
    Ok(())
}

fn gone<T>(_: mpsc::SendError<T>) -> anyhow::Error {
    anyhow!("the desk has stopped")
}

/// The request named a session that is not open: the browser has to open
/// one again.
#[derive(Debug)]
struct NoSession;

impl std::fmt::Display for NoSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("your session has ended; connect your wallet again")
    }
}

impl std::error::Error for NoSession {}

/// Answers one HTTP request and closes the connection.
fn serve(
    mut stream: TcpStream,
    config: &Config,
    shared: &Shared,
    trader: &mpsc::Sender<TraderJob>,
    makers: &[mpsc::Sender<MakerJob>],
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let mut length = 0usize;
    let mut token = None;
    let mut headers = Vec::new();
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            } else if name.eq_ignore_ascii_case("authorization") {
                token = value.trim().strip_prefix("Bearer ").map(str::to_owned);
            }
            headers.push((name.trim().to_owned(), value.trim().to_owned()));
        }
    }
    let relayed = path
        .strip_prefix("/api")
        .filter(|rest| rest.starts_with("/rings") || rest.starts_with("/rpc"));
    let limit = if relayed.is_some() {
        MAX_RELAY_BODY
    } else {
        MAX_BODY
    };
    if length > limit {
        write!(
            stream,
            "HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )?;
        return Ok(());
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;

    // A request for Helius, from a browser with a session: passed on with
    // the desk's key, and answered with whatever Helius said.
    if let Some(target) = relayed {
        let known = token
            .as_deref()
            .is_some_and(|token| shared.lock().session(token).is_some());
        let answer = if known {
            relay::relay(config, method, target, &headers, body)
        } else {
            Err(NoSession.into())
        };
        return match answer {
            Ok(answer) => {
                write!(stream, "HTTP/1.1 {} Relayed\r\n", answer.status)?;
                for (name, value) in &answer.headers {
                    write!(stream, "{name}: {value}\r\n")?;
                }
                write!(
                    stream,
                    "Content-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
                    answer.body.len()
                )?;
                stream.write_all(&answer.body)?;
                Ok(())
            }
            Err(error) => {
                let status = if error.is::<NoSession>() {
                    "401 Unauthorized"
                } else {
                    "502 Bad Gateway"
                };
                let payload = json!({ "error": format!("{error:#}") }).to_string();
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                )?;
                Ok(())
            }
        };
    }

    let request = Request {
        method,
        path: path.split('?').next().unwrap_or(""),
        token: token.as_deref(),
        body: &body,
    };
    let (status, payload) = match route(&request, shared, trader, makers) {
        Ok(payload) => ("200 OK", payload.to_string()),
        Err(error) => (
            if error.is::<NoSession>() {
                "401 Unauthorized"
            } else {
                "400 Bad Request"
            },
            json!({ "error": format!("{error:#}") }).to_string(),
        ),
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    )?;
    Ok(())
}

struct Request<'a> {
    method: &'a str,
    path: &'a str,
    token: Option<&'a str>,
    body: &'a [u8],
}

/// What an account's key signs to open a session: its address, its
/// shielded address in hex, and the time in unix milliseconds.
fn session_message(address: &str, shielded: &str, at: u64) -> String {
    format!("darkring session\n{address}\n{shielded}\n{at}")
}

/// A program proof and the pool's transact data, as a browser sends them:
/// the proof's three points one after another, and the transact data in the
/// encoding the pool reads, both in hex. Both are public once they land.
fn proofs(proof: &str, transact: &str) -> Result<(OpenProof, TransactIxData)> {
    let bytes: [u8; 128] = unhex(proof).ok_or_else(|| anyhow!("proof is not 128 bytes of hex"))?;
    let mut proof = OpenProof {
        a: [0; 32],
        b: [0; 64],
        c: [0; 32],
    };
    proof.a.copy_from_slice(&bytes[..32]);
    proof.b.copy_from_slice(&bytes[32..96]);
    proof.c.copy_from_slice(&bytes[96..]);
    let bytes = unhex_vec(transact).ok_or_else(|| anyhow!("transact is not hex"))?;
    let transact = wincode::deserialize_exact::<TransactIxData>(&bytes)
        .map_err(|_| anyhow!("transact is not the pool's transact data"))?;
    Ok((proof, transact))
}

/// A session token: 32 bytes from the system's generator, in hex.
fn new_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut bytes))
        .context("reading /dev/urandom")?;
    Ok(hex(&bytes))
}

fn route(
    request: &Request,
    shared: &Shared,
    trader: &mpsc::Sender<TraderJob>,
    makers: &[mpsc::Sender<MakerJob>],
) -> Result<Value> {
    // Whoever holds an open session's token speaks for its account.
    let session = match request.token {
        Some(token) => Some(shared.lock().session(token).ok_or(NoSession)?),
        None => None,
    };
    if (request.method, request.path) == ("GET", "/api/state") {
        return Ok(shared.snapshot(session.as_ref().map(|(account, _)| account.as_str())));
    }
    if request.method != "POST" {
        bail!("no such route");
    }
    let body: Value = if request.body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(request.body).context("the body is not JSON")?
    };
    let text = |name: &str| {
        body.get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("missing {name}"))
    };

    if request.path == "/api/session" {
        if !shared.lock().ready {
            bail!("the desk is still connecting to devnet");
        }
        let address = Address::from_str(&text("address")?)
            .map_err(|_| anyhow!("address is not a Solana address"))?;
        let shielded_hex = text("shielded")?;
        let at = body
            .get("at")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("missing at"))?;
        let signature: [u8; 64] = unhex(&text("signature")?)
            .ok_or_else(|| anyhow!("signature is not 64 bytes of hex"))?;
        // Only the account's key could have signed this, and only just now.
        if chain::now_ms().abs_diff(at) > SESSION_SKEW_MS {
            bail!("this device's clock is off by more than five minutes");
        }
        let message = session_message(&address.to_string(), &shielded_hex, at);
        if !PublicKey::from_ed25519(address.as_array())
            .verify_message(message.as_bytes(), &signature)
        {
            bail!("the signature is not this account's");
        }
        let shielded = unhex(&shielded_hex)
            .and_then(|bytes| ShieldedAddress::from_bytes(&bytes).ok())
            .filter(|shielded| shielded.solana_address().ok() == Some(address))
            .ok_or_else(|| anyhow!("shielded is not this account's address"))?;
        let account = address.to_string();
        let token = new_token()?;
        {
            let mut desk = shared.lock();
            desk.open_session(token.clone(), &account, shielded);
            let book = desk.book(&account);
            // A second tab joins whatever the first is doing.
            if book.busy.is_none() {
                book.error = None;
                book.busy = Some(Busy::new("login", "Setting up your account"));
                trader
                    .send(TraderJob::Login {
                        account: account.clone(),
                        address: Box::new(shielded),
                    })
                    .map_err(gone)?;
            }
        }
        let mut snapshot = shared.snapshot(Some(&account));
        snapshot["token"] = Value::String(token);
        return Ok(snapshot);
    }

    let (account, address) = session.ok_or(NoSession)?;
    let account = account.as_str();

    // One action at a time for each account: each is a transaction.
    let begin = |busy: Busy| -> Result<()> {
        let mut desk = shared.lock();
        let book = desk.book(account);
        if book.busy.is_some() {
            bail!("the desk is still on your last request");
        }
        book.error = None;
        book.busy = Some(busy);
        Ok(())
    };

    match request.path {
        "/api/logout" => {
            if let Some(token) = request.token {
                shared.lock().close_session(token);
            }
            return Ok(shared.snapshot(None));
        }
        "/api/sign" => {
            let id = body
                .get("id")
                .and_then(Value::as_u64)
                .ok_or_else(|| anyhow!("missing id"))?;
            let signature: [u8; 64] = unhex(&text("signature")?)
                .ok_or_else(|| anyhow!("signature is not 64 bytes of hex"))?;
            if !shared.signed(account, id, signature) {
                bail!("nothing is waiting for that signature");
            }
        }
        "/api/quotes" => {
            let long = match text("side")?.as_str() {
                "long" => true,
                "short" => false,
                _ => bail!("side is long or short"),
            };
            let symbol = text("market").unwrap_or_else(|_| "SOL".into());
            let market = market(&symbol).ok_or_else(|| anyhow!("no such market"))?;
            let amount = body.get("amount").and_then(Value::as_f64).unwrap_or(0.0);
            let leverage = body.get("leverage").and_then(Value::as_u64).unwrap_or(0);
            let (mark, published) = shared
                .lock()
                .markets
                .iter()
                .find(|view| view.symbol == symbol)
                .map(|view| (view.price.parse::<f64>().unwrap_or(0.0), view.publish_time))
                .unwrap_or((0.0, 0));
            // A position opened at a price too old to settle at could not be
            // closed again until the price moves on.
            let age = chain::now() - published;
            if age > chain::SETTLE_PRICE_AGE_SECS {
                bail!(
                    "the {symbol} price has not updated for {} minutes, so {symbol} cannot be \
                     traded until it does",
                    age / 60
                );
            }
            let notional = amount * mark * 10f64.powi(market.expo);
            if !(amount > 0.0 && notional.is_finite() && notional <= MAX_NOTIONAL_USD)
                || !(1..=50).contains(&leverage)
            {
                bail!("size or leverage out of range");
            }
            let size = (amount * market.size_per_unit()).round();
            if !(size >= 1.0 && size < u64::MAX as f64) {
                bail!("size out of range");
            }
            let size = size as u64;
            // A market order goes to the maker with the best price alone:
            // the others would quote further from the mark, and each quote
            // is a transaction that locks a maker's margin.
            let take = body.get("take").and_then(Value::as_bool).unwrap_or(false);
            let asked: Vec<&mpsc::Sender<MakerJob>> = if take {
                makers.get(best_maker()).into_iter().collect()
            } else {
                makers.iter().collect()
            };
            begin(Busy::new(
                "quotes",
                if take {
                    "Asking for the best price"
                } else {
                    "Asking three makers"
                },
            ))?;
            {
                let mut desk = shared.lock();
                let book = desk.book(account);
                book.quotes.clear();
                book.awaiting_quotes = asked.len();
            }
            for maker in asked {
                maker
                    .send(MakerJob::Quote {
                        account: account.into(),
                        market,
                        trader: address,
                        long,
                        size,
                        leverage,
                    })
                    .map_err(gone)?;
            }
        }
        "/api/open" => {
            let quote_id = text("quoteId")?;
            let (proof, transact) = proofs(&text("proof")?, &text("transact")?)?;
            begin(Busy {
                quote_id: Some(quote_id.clone()),
                ..Busy::new("open", "Sending to Solana")
            })?;
            trader
                .send(TraderJob::Open {
                    account: account.into(),
                    quote_id,
                    proof: Box::new(proof),
                    transact: Box::new(transact),
                })
                .map_err(gone)?;
        }
        "/api/close" => {
            let position_id = text("positionId")?;
            let oracle = Address::from_str(&text("oracle")?)
                .map_err(|_| anyhow!("oracle is not a Solana address"))?;
            let price = text("price")?
                .parse::<u64>()
                .map_err(|_| anyhow!("price is not a number of ticks"))?;
            let (proof, transact) = proofs(&text("proof")?, &text("transact")?)?;
            begin(Busy {
                position_id: Some(position_id.clone()),
                ..Busy::new("close", "Sending to Solana")
            })?;
            trader
                .send(TraderJob::Close {
                    account: account.into(),
                    position_id,
                    oracle,
                    price,
                    proof: Box::new(proof),
                    transact: Box::new(transact),
                })
                .map_err(gone)?;
        }
        "/api/faucet" => {
            begin(Busy::new("faucet", "Adding test dollars"))?;
            trader
                .send(TraderJob::Faucet {
                    account: account.into(),
                })
                .map_err(gone)?;
        }
        _ => bail!("no such route"),
    }
    Ok(shared.snapshot(Some(account)))
}
