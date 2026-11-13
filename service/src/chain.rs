//! The devnet connection every actor makes for itself, and the few things
//! they all do with it.

use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Result};
use perps_sdk::OraclePrice;
use solana_address::Address;
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_signature::Signature;
use solana_signer::Signer;
use spr_client::{
    fetch_asset_id, user_registry::ensure_registered, ComputeBudgetConfig, ProofDataSource,
    ProverClient, Rpc, SolanaRpc, SprClient, SprIndexer,
};
use spr_interface::{pda, state::tree::read_tree_id};
use spr_keypair::{ShieldedKeypair, SigningKey};
use spr_test_utils::{
    test_validator_asserts::wait_for_indexed_utxo,
    wallet::{sync_wallet, Deposit, DepositParams, Filter, Wallet},
};
use spr_transaction::{AssetRegistry, WalletUtxo};

// The whole per-transaction budget: an instruction verifies an SPP proof and
// its own.
const TRANSACT_COMPUTE_UNIT_LIMIT: u32 = 1_400_000;

/// Pyth SOL/USD, and the price update account Pyth keeps for it on devnet.
pub const FEED_ID: [u8; 32] = [
    0xef, 0x0d, 0x8b, 0x6f, 0xda, 0x2c, 0xeb, 0xa4, 0x1d, 0xa1, 0x5d, 0x40, 0x95, 0xd1, 0xda, 0x39,
    0x2a, 0x0d, 0x2f, 0x8e, 0xd0, 0xc6, 0xc7, 0xbc, 0x0f, 0x4c, 0xfa, 0xc8, 0xc2, 0x80, 0xb5, 0x6d,
];
pub const ORACLE: Address = Address::from_str_const("7UVimffxr9ow1uXYxsr4LHAcV58mLzhmwaeKvJ1pjLiE");

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
    /// The test dollar.
    pub mint: Address,
    pub assets: AssetRegistry,
}

impl Chain {
    pub fn connect(config: &Config) -> Result<Self> {
        let client = SprClient::new(
            SolanaRpc::new(config.rpc_url.clone()),
            SprIndexer::new(&config.spr_url),
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

    /// Waits until the indexer has the transaction's outputs.
    pub fn indexed(&self, signature: Signature) -> Result<()> {
        self.client
            .confirm_private_transaction_sync(signature)
            .map_err(|e| anyhow!("the indexer did not pick up {signature}: {e:?}"))?;
        Ok(())
    }

    /// The oracle account's price, whatever its age.
    pub fn oracle(&self) -> Result<OraclePrice> {
        let account = self
            .client
            .rpc()
            .get_account(ORACLE)?
            .ok_or_else(|| anyhow!("no oracle account {ORACLE}"))?;
        OraclePrice::parse(&account.data).map_err(|e| anyhow!("oracle account: {e:?}"))
    }

    /// The oracle price, if a settle sent now would still be inside the
    /// program's window when it lands.
    pub fn settle_price(&self) -> Result<u64> {
        let price = self.oracle()?;
        let age = now() - price.publish_time;
        if age > SETTLE_PRICE_AGE_SECS {
            anyhow::bail!(
                "the oracle price is {age} seconds old; Pyth refreshes it within a few minutes"
            );
        }
        Ok(price.price)
    }

    /// Shields `amount` test dollars from the deployer's token account to
    /// `recipient`, and waits for the note to be indexed.
    pub fn shield(
        &self,
        deployer: &Keypair,
        recipient: &ShieldedKeypair,
        amount: u64,
    ) -> Result<Signature> {
        let funding = pda::associated_token_address(&deployer.pubkey(), &self.mint);
        let deposit = Deposit::new(DepositParams {
            recipient: &recipient.shielded_address()?,
            asset: self.mint,
            amount,
            spl_token_account: Some(funding),
            spl_token_program: Some(pda::spl_token_program_id()),
            memo: None,
        })?;
        let view_tag = deposit.view_tag();
        let signature = deposit.send(&self.client, deployer, self.tree, deployer)?;
        wait_for_indexed_utxo(&self.client, view_tag, signature);
        Ok(signature)
    }
}

/// One wallet: its keys and the notes it has decrypted so far.
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

    /// One note worth at least `amount`: a transaction here spends one note.
    pub fn note(&self, chain: &Chain, amount: u64) -> Result<WalletUtxo> {
        self.wallet
            .balance(chain.mint, Some(Filter::MinAmount(amount)))?
            .utxos
            .first()
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "no single sealed note covers {:.2}; the balance is split across smaller notes",
                    amount as f64 / USDC as f64
                )
            })
    }
}
