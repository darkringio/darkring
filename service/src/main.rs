//! The devnet desk. Holds one trader wallet and three maker wallets, runs
//! each on a thread of its own against devnet, and serves the web app a
//! small JSON API on localhost:
//!
//!   GET  /api/state    the whole desk
//!   POST /api/quotes   {"side":"long","sol":10,"leverage":8}
//!   POST /api/open     {"quoteId":"..."}
//!   POST /api/close    {"positionId":"..."}
//!   POST /api/faucet   shield another 5,000 test dollars to the trader
//!
//! Needs HELIUS_API_KEY (or DEVNET_RPC_URL and DEVNET_SPR_URL), ZOLANA_CLI_BIN
//! for the local prover, and the wallets scripts/devnet-setup.sh makes.

mod chain;
mod desk;
mod maker;
mod trader;

use std::{
    env,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{mpsc, Arc},
    thread,
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use spr_client::ProverLaunch;
use spr_keypair::{ShieldedKeypair, SigningKey};

use crate::{
    chain::Config,
    desk::{Busy, Desk, MakerView, Shared},
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

/// Size units per SOL, as the program counts size.
const SIZE_PER_SOL: f64 = 10_000_000.0;

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
        keys_dir: root.join("keys"),
        state_file: env::var_os("DESK_STATE_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join(".desk/state.json")),
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

    // The pool proofs are made on this machine, by the release prover with
    // the release keys: no witness leaves it.
    let home = env::var("HOME").context("HOME is not set")?;
    ProverLaunch::new_with_cli(env::var("ZOLANA_CLI_BIN").context("ZOLANA_CLI_BIN is not set")?)?
        .with_keys_dir(Path::new(&home).join(".config/zolana/proving-keys"))?
        .spawn()?;

    let trader_address = shielded(&config, "devnet-trader.json")?.shielded_address()?;
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

    let shared = Arc::new(Shared::load(
        config.state_file.clone(),
        Desk {
            network: "devnet".into(),
            program: perps_program::ID.to_string(),
            wallet: trader_address.solana_address()?.to_string(),
            balance: "0".into(),
            price: "0".into(),
            max_price_age: trader::max_price_age(),
            makers: makers.clone(),
            busy: Some(trader::boot_notice()),
            booting: MAKERS.len() + 1,
            ..Desk::default()
        },
    ));

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
            maker::run(
                config,
                spec,
                view,
                trader_address,
                shared,
                inbox,
                trader_jobs,
            );
        });
    }
    {
        let (config, shared, makers) = (config.clone(), Arc::clone(&shared), maker_jobs.clone());
        thread::spawn(move || trader::run(config, shared, trader_inbox, makers));
    }

    let listener = TcpListener::bind(("127.0.0.1", config.port))
        .with_context(|| format!("binding 127.0.0.1:{}", config.port))?;
    println!("desk: listening on http://127.0.0.1:{}", config.port);
    for stream in listener.incoming().flatten() {
        let (shared, trader_jobs, maker_jobs) =
            (Arc::clone(&shared), trader_jobs.clone(), maker_jobs.clone());
        thread::spawn(move || {
            if let Err(error) = serve(stream, &shared, &trader_jobs, &maker_jobs) {
                eprintln!("desk: request: {error:#}");
            }
        });
    }
    Ok(())
}

fn gone<T>(_: mpsc::SendError<T>) -> anyhow::Error {
    anyhow!("the desk has stopped")
}

/// Answers one HTTP request and closes the connection.
fn serve(
    mut stream: TcpStream,
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
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0; length.min(64 * 1024)];
    reader.read_exact(&mut body)?;

    let (status, payload) = match route(method, path, &body, shared, trader, makers) {
        Ok(payload) => ("200 OK", payload),
        Err(error) => (
            "400 Bad Request",
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

fn route(
    method: &str,
    path: &str,
    body: &[u8],
    shared: &Shared,
    trader: &mpsc::Sender<TraderJob>,
    makers: &[mpsc::Sender<MakerJob>],
) -> Result<String> {
    let path = path.split('?').next().unwrap_or("");
    if (method, path) == ("GET", "/api/state") {
        return Ok(shared.snapshot());
    }
    if method != "POST" {
        bail!("no such route");
    }
    let body: Value = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(body).context("the body is not JSON")?
    };
    let text = |name: &str| {
        body.get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("missing {name}"))
    };

    // One action at a time: each is a proof and a transaction.
    let begin = |busy: Busy| -> Result<()> {
        let mut desk = shared.lock();
        if !desk.ready {
            bail!("the desk is still connecting to devnet");
        }
        if desk.busy.is_some() {
            bail!("the desk is busy");
        }
        desk.error = None;
        desk.busy = Some(busy);
        Ok(())
    };

    match path {
        "/api/quotes" => {
            let long = match text("side")?.as_str() {
                "long" => true,
                "short" => false,
                _ => bail!("side is long or short"),
            };
            let sol = body.get("sol").and_then(Value::as_f64).unwrap_or(0.0);
            let leverage = body.get("leverage").and_then(Value::as_u64).unwrap_or(0);
            if !(sol > 0.0 && sol <= 100_000.0) || !(1..=50).contains(&leverage) {
                bail!("size or leverage out of range");
            }
            let size = (sol * SIZE_PER_SOL).round() as u64;
            begin(Busy::new("quotes", "Asking three makers"))?;
            {
                let mut desk = shared.lock();
                desk.quotes.clear();
                desk.awaiting_quotes = makers.len();
            }
            for maker in makers {
                maker
                    .send(MakerJob::Quote {
                        long,
                        size,
                        leverage,
                    })
                    .map_err(gone)?;
            }
        }
        "/api/open" => {
            let quote_id = text("quoteId")?;
            begin(Busy {
                quote_id: Some(quote_id.clone()),
                ..Busy::new("open", "Proving the open")
            })?;
            trader.send(TraderJob::Open { quote_id }).map_err(gone)?;
        }
        "/api/close" => {
            let position_id = text("positionId")?;
            begin(Busy {
                position_id: Some(position_id.clone()),
                ..Busy::new("close", "Proving the close")
            })?;
            trader
                .send(TraderJob::Close { position_id })
                .map_err(gone)?;
        }
        "/api/faucet" => {
            begin(Busy::new("faucet", "Shielding test dollars"))?;
            trader.send(TraderJob::Faucet).map_err(gone)?;
        }
        _ => bail!("no such route"),
    }
    Ok(shared.snapshot())
}
