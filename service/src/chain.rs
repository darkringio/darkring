//! The devnet connection every actor makes for itself, and the few things
//! they all do with it.

use std::{
    collections::HashSet,
    path::PathBuf,
    sync::OnceLock,
    thread::sleep,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Result};
use perps_sdk::{shared::feed_halves, OraclePrice};
use solana_address::Address;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_signature::Signature;
use solana_signer::Signer;
use spr_api::BlockingSprApi;
use spr_client::{
    fetch_asset_id, user_registry::ensure_registered, ComputeBudgetConfig, ProofDataSource,
    ProverClient, Rpc, SolanaRpc, SprClient, SprIndexer,
};
use spr_interface::{pda, state::tree::read_tree_id};
use spr_keypair::{ShieldedAddress, ShieldedKeypair, SigningKey};
use spr_test_utils::{
    test_validator_asserts::wait_for_indexed_utxo,
    wallet::{sync_wallet, Deposit, DepositParams, Filter, Wallet},
};
use spr_transaction::{AssetRegistry, WalletUtxo};

use crate::{desk::unhex, usage};

// The whole per-transaction budget: an instruction verifies an SPP proof and
// its own.
const TRANSACT_COMPUTE_UNIT_LIMIT: u32 = 1_400_000;
/// About how far the indexer is behind a confirmed transaction: asking it
/// sooner is usually a request wasted.
const INDEXER_LAG: Duration = Duration::from_millis(450);
/// After that, how long to wait before each further try, in milliseconds.
const LEAF_WAITS_MS: [u64; 6] = [0, 500, 1000, 2000, 4000, 8000];

/// A market: a Pyth feed, and the price update accounts kept for it on
/// devnet. The program settles a position against whichever feed its quote
/// named, read from any account that holds a fully verified price for it,
/// so a market needs nothing deployed: only a feed that is kept fresh.
pub struct Market {
    pub symbol: &'static str,
    pub name: &'static str,
    pub feed: [u8; 32],
    /// Every account the desk reads this feed from. It uses whichever holds
    /// the newest price.
    pub oracles: Vec<Address>,
    /// The feed's exponent: a price tick is ten to this power, in dollars.
    pub expo: i32,
}

impl Market {
    /// Size units in one whole unit of the asset. Size counts collateral
    /// base units per price tick, times the program's size scale, so this
    /// is 10^(6 + expo + 9).
    pub fn size_per_unit(&self) -> f64 {
        10f64.powi(15 + self.expo)
    }
}

/// A market's price, and the account it was read from: a settle names the
/// account, and the program reads the same price there.
#[derive(Clone, Copy)]
pub struct Mark {
    pub price: u64,
    pub publish_time: i64,
    pub account: Address,
}

/// Symbol, name, feed id, price accounts, exponent. These are the crypto
/// feeds kept fresh on devnet that trade around the clock. Pyth's sponsored
/// accounts (the last one listed for each market) are refreshed about every
/// five minutes. SOL, BTC, ETH and HYPE also have accounts on another shard
/// of Pyth's push oracle that were being refreshed every 70 seconds when
/// this was written (2026-11-13); those are listed first, and the desk falls
/// back to the sponsored account whenever it holds the newer price.
/// Metals, currencies and stocks have accounts too, but their prices stop
/// outside market hours, and a position nobody can close over a weekend is
/// not one to offer.
const MARKETS: [(&str, &str, &str, &[&str], i32); 11] = [
    (
        "SOL",
        "Solana",
        "ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d",
        &[
            "6bWEn5B8eJCRAek5acd3R7d4Sx3e7JWvt84srqqfYgt",
            "7UVimffxr9ow1uXYxsr4LHAcV58mLzhmwaeKvJ1pjLiE",
        ],
        -8,
    ),
    (
        "BTC",
        "Bitcoin",
        "e62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43",
        &[
            "CQanas4srymHLigVyu56ynJhKZzKYJvBa45VAsTgXSvM",
            "4cSM2e6rvbGQUFiJbqytoVMi5GgghSMr8LwVrT9VPSPo",
        ],
        -8,
    ),
    (
        "ETH",
        "Ethereum",
        "ff61491a931112ddf1bd8147cd1b641375f79f5825126d665480874634fd0ace",
        &[
            "4Ruku1Seg5iaUQT9pQmbfR9RB1r4iYWmiZhrh9A2idwo",
            "42amVS4KgzR9rA28tkVYqVXjq9Qa8dcZQMbH5EYFX6XC",
        ],
        -8,
    ),
    (
        "HYPE",
        "Hyperliquid",
        "4279e31cc369bbcc2faf022b382b080e32a8e689ff20fbc530d2a603eb6cd98b",
        &["3dndBghRKt4ZmuCSMDRNM2urEZKagoM4o15YHtN2LbeJ"],
        -8,
    ),
    (
        "JUP",
        "Jupiter",
        "0a0408d619e9380abad35060f9192039ed5042fa6f82301d0e48bb52be830996",
        &["7dbob1psH1iZBS7qPsm3Kwbf5DzSXK8Jyg31CTgTnxH5"],
        -8,
    ),
    (
        "RAY",
        "Raydium",
        "91568baa8beb53db23eb3fb7f22c6e8bd303d103919e19733f2bb642d3e7987a",
        &["Hhipna3EoWR7u8pDruUg8RxhP5F6XLh6SEHMVDmZhWi8"],
        -8,
    ),
    (
        "PYTH",
        "Pyth Network",
        "0bbf28e9a841a1cc788f6a361b17ca072d0ea3098a1e5df1c3922d06719579ff",
        &["8vjchtMuJNY4oFQdTi8yCe6mhCaNBFaUbktT482TpLPS"],
        -8,
    ),
    (
        "WIF",
        "dogwifhat",
        "4ca4beeca86f0d164160323817a4e42b10010a724c2217c6ee41b54cd4cc61fc",
        &["6B23K3tkb51vLZA14jcEQVCA1pfHptzEHFA93V5dYwbT"],
        -8,
    ),
    (
        "BONK",
        "Bonk",
        "72b021217ca3fe68922a19aaf990109cb9d84e9ad004b4d2025ad6f529314419",
        &["DBE3N8uNjhKPRHfANdwGvCZghWXyLPdqdSbEW2XFwBiX"],
        -10,
    ),
    (
        "FARTCOIN",
        "Fartcoin",
        "58cd29ef0e714c5affc44f269b2c1899a52da4169d7acc147b9da692e6953608",
        &["2t8eUbYKjidMs3uSeYM9jXM9uudYZwGkSeTB4TKjmvnC"],
        -8,
    ),
    (
        "PUMP",
        "Pump.fun",
        "7a01fca212788bba7c5bf8c9efd576a8a722f070d2c17596ff7bb609b8d5c3b9",
        &["HMm3GPbdnqGwbkTnUUqCFsH8AMHDdEC3Lg8gcPD3HJSH"],
        -8,
    ),
];

pub fn markets() -> &'static [Market] {
    static TABLE: OnceLock<Vec<Market>> = OnceLock::new();
    TABLE.get_or_init(|| {
        MARKETS
            .iter()
            .map(|&(symbol, name, feed, oracles, expo)| Market {
                symbol,
                name,
                feed: unhex(feed).expect("a feed id is 32 bytes of hex"),
                oracles: oracles
                    .iter()
                    .map(|oracle| Address::from_str_const(oracle))
                    .collect(),
                expo,
            })
            .collect()
    })
}

pub fn market(symbol: &str) -> Option<&'static Market> {
    markets().iter().find(|market| market.symbol == symbol)
}

/// The market a quote's or a position's feed id names.
pub fn market_of(feed: &[u8; 32]) -> Result<&'static Market> {
    markets()
        .iter()
        .find(|market| &market.feed == feed)
        .ok_or_else(|| anyhow!("the desk has no market for that feed"))
}

/// The devnet program's price window (its `devnet-oracle` feature), and how
/// much of it a settle may use: the rest is left for proving and landing.
pub const MAX_PRICE_AGE_SECS: i64 = 600;
pub const SETTLE_PRICE_AGE_SECS: i64 = 560;

/// Base units per test dollar.
pub const USDC: u64 = 1_000_000;

#[derive(Clone)]
pub struct Config {
    pub rpc_url: String,
    pub spr_url: String,
    pub keys_dir: PathBuf,
    pub state_file: PathBuf,
    pub port: u16,
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// The first 32 bytes of a Solana CLI keypair file: the ed25519 seed.
fn seed(config: &Config, name: &str) -> Result<[u8; 32]> {
    let path = config.keys_dir.join(name);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("{}: run scripts/devnet-setup.sh", path.display()))?;
    let bytes: Vec<u8> = serde_json::from_str(&text)?;
    bytes
        .get(..32)
        .and_then(|seed| seed.try_into().ok())
        .ok_or_else(|| anyhow!("{} is not a keypair file", path.display()))
}

pub fn solana_keypair(config: &Config, name: &str) -> Result<Keypair> {
    Ok(Keypair::new_from_array(seed(config, name)?))
}

pub struct Chain {
    pub client: SprClient<SolanaRpc>,
    pub tree: Address,
    pub tree_id: u16,
    /// The test dollar, and the id the pool knows it by.
    pub mint: Address,
    pub asset_id: u64,
    pub assets: AssetRegistry,
}

impl Chain {
    pub fn connect(config: &Config) -> Result<Self> {
        let client = SprClient::new(
            SolanaRpc::new(config.rpc_url.clone()),
            // Through a client that counts every request: see `usage`.
            SprIndexer::with_api(BlockingSprApi::with_client(
                &config.spr_url,
                usage::Counted::new(),
            )),
            ProverClient::default(),
        )
        .with_proof_data_source(ProofDataSource::Client);
        client.rpc().assert_executable(&perps_program::ID)?;
        let tree = pda::tree(0);
        let tree_id = client
            .rpc()
            .get_account(tree)?
            .and_then(|account| read_tree_id(&account.data))
            .ok_or_else(|| anyhow!("no default tree {tree}"))?;
        let mint = solana_keypair(config, "devnet-mint.json")?.pubkey();
        let asset_id = fetch_asset_id(client.rpc(), mint)
            .map_err(|e| anyhow!("the test dollar is not registered with the pool: {e:?}"))?;
        let mut assets = AssetRegistry::default();
        assets.insert(asset_id, mint)?;
        Ok(Self {
            client,
            tree,
            tree_id,
            mint,
            asset_id,
            assets,
        })
    }

    pub fn send(&self, payer: &dyn Signer, ix: Instruction) -> Result<Signature> {
        Ok(self.client.rpc().create_and_send_transaction(
            std::slice::from_ref(&ix),
            payer.pubkey(),
            &[payer],
            ComputeBudgetConfig::new(TRANSACT_COMPUTE_UNIT_LIMIT),
        )?)
    }

    /// Where the output with this `hash` sits in the tree, once the indexer
    /// has the transaction that made it.
    ///
    /// One question does two jobs: an answer means the transaction is
    /// indexed, and the answer is the leaf. The indexer is about half a
    /// second behind the chain, so the first question waits that long and
    /// is usually the only one.
    pub fn leaf(&self, hash: [u8; 32]) -> Result<u64> {
        sleep(INDEXER_LAG);
        let mut last = None;
        for wait in LEAF_WAITS_MS {
            sleep(Duration::from_millis(wait));
            match self
                .client
                .indexer()
                .get_merkle_proofs(self.tree, vec![hash], None)
            {
                Ok(response) => {
                    if let Some(proof) = response.proofs.into_iter().next() {
                        return Ok(proof.leaf_index);
                    }
                }
                Err(error) => last = Some(format!("{error:?}")),
            }
        }
        Err(anyhow!(
            "the indexer did not pick up the transaction: {}",
            last.unwrap_or_else(|| "it has no proof for its output".into())
        ))
    }

    /// The newest price any of `market`'s accounts holds for its feed,
    /// whatever its age.
    pub fn mark(&self, market: &Market) -> Result<Mark> {
        let accounts = self
            .client
            .rpc()
            .get_multiple_accounts(market.oracles.clone())?;
        newest(market, &market.oracles, &accounts)
            .ok_or_else(|| anyhow!("no price account for {}", market.symbol))
    }

    /// Every market's newest price, in the order of [`markets`], read in
    /// one call. A market none of whose accounts parses is `None`.
    pub fn marks(&self) -> Result<Vec<Option<Mark>>> {
        let addresses: Vec<Address> = markets()
            .iter()
            .flat_map(|market| market.oracles.iter().copied())
            .collect();
        let accounts = self.client.rpc().get_multiple_accounts(addresses.clone())?;
        let mut from = 0;
        Ok(markets()
            .iter()
            .map(|market| {
                let to = from + market.oracles.len();
                let mark = newest(market, &addresses[from..to], &accounts[from..to]);
                from = to;
                mark
            })
            .collect())
    }

    /// `market`'s newest price, if a settle sent now would still be inside
    /// the program's window when it lands.
    pub fn settle_mark(&self, market: &Market) -> Result<Mark> {
        let mark = self.mark(market)?;
        let age = now() - mark.publish_time;
        if age > SETTLE_PRICE_AGE_SECS {
            anyhow::bail!(
                "the {} price is {age} seconds old; Pyth refreshes it within a few minutes",
                market.symbol
            );
        }
        Ok(mark)
    }
}

/// The newest price for `market`'s feed among `accounts`, which are the
/// accounts at `addresses`. An account that does not parse, or holds another
/// feed, is passed over.
fn newest(
    market: &Market,
    addresses: &[Address],
    accounts: &[Option<solana_account::Account>],
) -> Option<Mark> {
    addresses
        .iter()
        .zip(accounts)
        .filter_map(|(address, account)| {
            let price = OraclePrice::parse(&account.as_ref()?.data).ok()?;
            ((price.feed_hi, price.feed_lo) == feed_halves(&market.feed)).then_some(Mark {
                price: price.price,
                publish_time: price.publish_time,
                account: *address,
            })
        })
        .max_by_key(|mark| mark.publish_time)
}

impl Chain {
    /// Shields `amount` test dollars from the deployer's token account to
    /// `recipient`, and waits for the note to be indexed.
    pub fn shield(
        &self,
        deployer: &Keypair,
        recipient: &ShieldedAddress,
        amount: u64,
    ) -> Result<Signature> {
        let funding = pda::associated_token_address(&deployer.pubkey(), &self.mint);
        let deposit = Deposit::new(DepositParams {
            recipient,
            asset: self.mint,
            amount,
            spl_token_account: Some(funding),
            spl_token_program: Some(pda::spl_token_program_id()),
            memo: None,
        })?;
        let view_tag = deposit.view_tag();
        let signature = deposit.send(&self.client, deployer, self.tree, deployer)?;
        // Asking before the indexer can have it is a question wasted.
        sleep(INDEXER_LAG * 2);
        wait_for_indexed_utxo(&self.client, view_tag, signature);
        Ok(signature)
    }
}

impl Chain {
    /// Sends `lamports` of devnet SOL from `from` to `to`, for fees.
    pub fn transfer_sol(&self, from: &Keypair, to: Address, lamports: u64) -> Result<Signature> {
        // The System program's Transfer: its index, then the amount.
        let mut data = 2u32.to_le_bytes().to_vec();
        data.extend_from_slice(&lamports.to_le_bytes());
        let instruction = Instruction {
            program_id: Address::default(),
            accounts: vec![
                AccountMeta::new(from.pubkey(), true),
                AccountMeta::new(to, false),
            ],
            data,
        };
        Ok(self.client.rpc().create_and_send_transaction(
            std::slice::from_ref(&instruction),
            from.pubkey(),
            &[from],
            ComputeBudgetConfig::for_instruction_count(1),
        )?)
    }
}

/// One wallet the desk holds the keys of: a maker's, with the notes it has
/// decrypted so far.
pub struct Actor {
    pub keypair: ShieldedKeypair,
    pub wallet: Wallet,
    pub pubkey: Address,
}

impl Actor {
    pub fn load(config: &Config, chain: &Chain, file: &str) -> Result<Self> {
        let keypair =
            ShieldedKeypair::from_keypair(SigningKey::from_ed25519_bytes(&seed(config, file)?))?;
        let address = keypair.shielded_address()?;
        let pubkey = address.solana_address()?;
        let wallet = Wallet::new(address, chain.assets.clone())
            .map_err(|e| anyhow!("wallet {file}: {e:?}"))?;
        ensure_registered(chain.client.rpc(), &keypair, &keypair)
            .map_err(|e| anyhow!("register {file}: {e:?}"))?;
        Ok(Self {
            keypair,
            wallet,
            pubkey,
        })
    }

    /// Decrypts whatever arrived since the last call and returns the sealed
    /// balance.
    pub fn sync(&mut self, chain: &Chain) -> Result<u64> {
        sync_wallet(&mut self.wallet, &self.keypair, &chain.client)
            .map_err(|e| anyhow!("sync wallet: {e:?}"))?;
        Ok(self.wallet.balance(chain.mint, None)?.amount)
    }

    /// Whether the wallet still lists the note with this hash as unspent.
    pub fn lists(&self, chain: &Chain, hash: &[u8; 32]) -> bool {
        self.wallet
            .balance(chain.mint, None)
            .is_ok_and(|balance| balance.utxos.iter().any(|note| &note.utxo_hash == hash))
    }

    /// One note worth at least `amount`, leaving out the ones in `used`: a
    /// transaction here spends one note, and the wallet goes on listing a
    /// note this process has spent until it reads its notes again.
    pub fn note(&self, chain: &Chain, amount: u64, used: &HashSet<[u8; 32]>) -> Result<WalletUtxo> {
        self.wallet
            .balance(chain.mint, Some(Filter::MinAmount(amount)))?
            .utxos
            .into_iter()
            .find(|note| !used.contains(&note.utxo_hash))
            .ok_or_else(|| {
                anyhow!(
                    "no single sealed note covers {:.2}; the balance is split across smaller notes",
                    amount as f64 / USDC as f64
                )
            })
    }
}

/// How long each step of one action took, for the log: `4.2s (sync 0.9,
/// prove 1.4, send 1.9)`.
pub struct Laps {
    started: std::time::Instant,
    last: std::time::Instant,
    laps: Vec<(&'static str, f32)>,
}

impl Laps {
    pub fn start() -> Self {
        let now = std::time::Instant::now();
        Self {
            started: now,
            last: now,
            laps: Vec::new(),
        }
    }

    /// Ends the step called `name`.
    pub fn mark(&mut self, name: &'static str) {
        let now = std::time::Instant::now();
        self.laps
            .push((name, now.duration_since(self.last).as_secs_f32()));
        self.last = now;
    }
}

impl std::fmt::Display for Laps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:.1}s (", self.started.elapsed().as_secs_f32())?;
        for (index, (name, seconds)) in self.laps.iter().enumerate() {
            let comma = if index == 0 { "" } else { ", " };
            write!(f, "{comma}{name} {seconds:.1}")?;
        }
        f.write_str(")")
    }
}
