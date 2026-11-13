#![allow(dead_code)]

use anyhow::{anyhow, bail, Context, Result};
use perps_sdk::{encode_price_update, ORACLE_PROGRAM_ID};
use serde_json::{json, Value};
use solana_address::Address;
use solana_instruction::Instruction;
use solana_signature::Signature;
use solana_signer::Signer;
use spr_client::{ComputeBudgetConfig, Rpc, SolanaRpc};
use spr_keypair::{ShieldedKeypair, SigningKey};
use spr_program_test::{
    fixture,
    localnet::{FixtureLocalnet, LocalnetPaths, LocalnetPorts},
};
use spr_test_utils::test_validator_asserts::wait_for_indexed_utxo;
use spr_test_utils::wallet::{sync_wallet, Deposit, DepositParams, Filter, Wallet};
use spr_transaction::{AssetRegistry, WalletUtxo};
use spr_user_registry_interface::user_registry_program_id;

use std::{
    env,
    path::{Path, PathBuf},
};

// The whole per-transaction budget: an instruction verifies an SPP proof and
// its own.
const TRANSACT_COMPUTE_UNIT_LIMIT: u32 = 1_400_000;

/// Price ticks per dollar on a feed with exponent -8, and base units per
/// unit of the 6-decimal-style collateral the tests use.
pub const USD: u64 = 100_000_000;
pub const USDC: u64 = 1_000_000;

/// What each party shields in setup.
pub const MAKER_SHIELD: u64 = 100 * USDC;
pub const TRADER_SHIELD: u64 = 50 * USDC;

/// The position every test quotes: long one unit from $100, 20 of trader
/// margin against 30 of maker margin, liquidatable at 5 of equity.
pub const SIZE: u64 = 10_000_000;
pub const ENTRY_PRICE: u64 = 100 * USD;
pub const TRADER_MARGIN: u64 = 20 * USDC;
pub const MAKER_MARGIN: u64 = 30 * USDC;
pub const MAINTENANCE_MARGIN: u64 = 5 * USDC;

/// The feed the tests settle against, and the account its price lives in.
pub const FEED_ID: [u8; 32] = [
    0xef, 0x0d, 0x8b, 0x6f, 0xda, 0x2c, 0xeb, 0xa4, 0x1d, 0xa1, 0x5d, 0x40, 0x95, 0xd1, 0xda, 0x39,
    0x2a, 0x0d, 0x2f, 0x8e, 0xd0, 0xc6, 0xc7, 0xbc, 0x0f, 0x4c, 0xfa, 0xc8, 0xc2, 0x80, 0xb5, 0x6d,
];
pub const ORACLE: Address = Address::new_from_array([0x0a; 32]);

// Each actor is one ed25519 identity: the wallet's signing key doubles as the
// Solana fee payer, and the wallet holds the asset registry and the synced
// spendable notes.
pub struct TestEnv {
    /// The localnet with its client and default tree. Dropping it stops the
    /// validator, so it lives as long as the test.
    pub localnet: FixtureLocalnet,
    pub rpc_url: String,
    pub maker: TestWallet,
    /// The maker's collateral UTXO the quote spends.
    pub maker_input: WalletUtxo,
    pub trader: TestWallet,
    /// The collateral mint.
    pub mint: Address,
}

pub struct TestWallet {
    pub wallet: Wallet,
    pub keypair: ShieldedKeypair,
}

impl std::ops::Deref for TestWallet {
    type Target = Wallet;
    fn deref(&self) -> &Self::Target {
        &self.wallet
    }
}

impl std::ops::DerefMut for TestWallet {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.wallet
    }
}

impl TestWallet {
    /// Syncs the wallet and returns its spendable collateral balance.
    pub fn synced_balance(
        &mut self,
        env_client: &spr_client::SprClient<SolanaRpc>,
        mint: Address,
    ) -> Result<u64> {
        sync_wallet(&mut self.wallet, &self.keypair, env_client)
            .map_err(|e| anyhow!("sync wallet: {e:?}"))?;
        Ok(self.wallet.balance(mint, None)?.amount)
    }
}

/// `file` under this workspace's `target/deploy`, where `cargo build-sbf`
/// writes the program.
fn deploy_path(file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../target/deploy")
        .join(file)
}

/// `ZOLANA_PROVER_KEYS_DIR`, else `~/.config/zolana/proving-keys`, the
/// directory `zolana test-env` keeps the proving keys in.
fn proving_keys_dir() -> Result<PathBuf> {
    if let Ok(dir) = env::var("ZOLANA_PROVER_KEYS_DIR") {
        return Ok(dir.into());
    }
    let home = env::var("HOME").map_err(|_| anyhow!("HOME is not set"))?;
    Ok(Path::new(&home).join(".config/zolana/proving-keys"))
}

/// The CLI, prover, Photon, surfpool and pool program come from the
/// environment (`ZOLANA_CLI_BIN`, `PROVER_BIN`, `ZOLANA_PHOTON_BIN`,
/// `SURFPOOL_BIN`, `SHIELDED_POOL_PROGRAM_PATH`). The keys and the scratch
/// directory are set here.
fn localnet_paths() -> Result<LocalnetPaths> {
    Ok(LocalnetPaths {
        proving_keys_dir: proving_keys_dir()?,
        scratch_dir: Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/localnet"),
        ..LocalnetPaths::workspace()
    })
}

/// The user registry program, `USER_REGISTRY_PROGRAM_PATH`, which
/// `scripts/build-localnet.sh` downloads.
fn user_registry_program_path() -> Result<PathBuf> {
    env::var("USER_REGISTRY_PROGRAM_PATH")
        .map(PathBuf::from)
        .map_err(|_| {
            anyhow!("USER_REGISTRY_PROGRAM_PATH is not set; see scripts/build-localnet.sh")
        })
}

fn shielded(actor: u8) -> Result<ShieldedKeypair> {
    let seed: [u8; 32] = fixture::actor(actor).to_bytes()[..32]
        .try_into()
        .expect("ed25519 seed is the first 32 bytes");
    Ok(ShieldedKeypair::from_keypair(
        SigningKey::from_ed25519_bytes(&seed),
    )?)
}

/// Boot the localnet of test number `test` ([`LocalnetPorts::for_test`]);
/// tests running in parallel take distinct numbers. Shields collateral to
/// the maker and the trader.
pub fn setup(test: u16) -> Result<TestEnv> {
    let ports = LocalnetPorts::for_test(test)?;
    let localnet = FixtureLocalnet::start(
        "dark-perps",
        ports,
        vec![
            (perps_program::ID, deploy_path("perps_program.so")),
            (user_registry_program_id(), user_registry_program_path()?),
        ],
        &localnet_paths()?,
    )?;
    let payer = fixture::payer();

    let mint = fixture::spl_mint();
    let mut assets = AssetRegistry::default();
    assets.insert(fixture::SPL_ASSET_ID, mint)?;
    let funding = fixture::payer_token_account();

    let maker_keypair = shielded(0)?;
    let trader_keypair = shielded(1)?;
    let maker_address = maker_keypair
        .shielded_address()
        .map_err(|e| anyhow!("maker address: {e:?}"))?;
    let trader_address = trader_keypair
        .shielded_address()
        .map_err(|e| anyhow!("trader address: {e:?}"))?;

    // Fund the actors: shield collateral to both.
    let mut deposits = Vec::new();
    for (recipient, amount) in [
        (&maker_address, MAKER_SHIELD),
        (&trader_address, TRADER_SHIELD),
    ] {
        let deposit = Deposit::new(DepositParams {
            recipient,
            asset: mint,
            amount,
            spl_token_account: Some(funding),
            spl_token_program: Some(spr_interface::pda::spl_token_program_id()),
            memo: None,
        })?;
        let view_tag = deposit.view_tag();
        let signature = deposit.send(&localnet.client, &payer, localnet.tree, &payer)?;
        deposits.push((view_tag, signature));
    }
    for (view_tag, signature) in deposits {
        wait_for_indexed_utxo(&localnet.client, view_tag, signature);
    }
    let mut maker_wallet =
        Wallet::new(maker_address, assets.clone()).map_err(|e| anyhow!("maker wallet: {e:?}"))?;
    let mut trader_wallet =
        Wallet::new(trader_address, assets.clone()).map_err(|e| anyhow!("trader wallet: {e:?}"))?;
    sync_wallet(&mut maker_wallet, &maker_keypair, &localnet.client)
        .map_err(|e| anyhow!("sync maker deposit: {e:?}"))?;
    sync_wallet(&mut trader_wallet, &trader_keypair, &localnet.client)
        .map_err(|e| anyhow!("sync trader deposit: {e:?}"))?;
    let maker_input = maker_wallet
        .balance(mint, Some(Filter::MinAmount(MAKER_SHIELD)))?
        .utxos
        .first()
        .cloned()
        .ok_or_else(|| anyhow!("no maker note of {MAKER_SHIELD}"))?;

    Ok(TestEnv {
        localnet,
        rpc_url: ports.rpc_url(),
        maker: TestWallet {
            wallet: maker_wallet,
            keypair: maker_keypair,
        },
        maker_input,
        trader: TestWallet {
            wallet: trader_wallet,
            keypair: trader_keypair,
        },
        mint,
    })
}

// Submit a single (large) instruction as a transaction **v1** message: its
// 4096-byte limit is what holds the account list and both proofs. v1 carries
// the compute ceilings in the message header. `payer` signs and pays.
pub fn send(rpc: &SolanaRpc, payer: &dyn Signer, ix: Instruction) -> Result<Signature> {
    Ok(rpc.create_and_send_transaction(
        std::slice::from_ref(&ix),
        payer.pubkey(),
        &[payer],
        ComputeBudgetConfig::new(TRANSACT_COMPUTE_UNIT_LIMIT),
    )?)
}

/// One JSON-RPC call against the localnet validator.
fn rpc_call(rpc_url: &str, method: &str, params: Value) -> Result<Value> {
    let response: Value = reqwest::blocking::Client::new()
        .post(rpc_url)
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))
        .send()
        .with_context(|| format!("{method} request"))?
        .json()
        .with_context(|| format!("{method} response"))?;
    if let Some(error) = response.get("error") {
        bail!("{method} failed: {error}");
    }
    Ok(response.get("result").cloned().unwrap_or(Value::Null))
}

/// The validator Clock's unix timestamp.
pub fn clock_now(rpc: &SolanaRpc) -> Result<i64> {
    let clock = rpc
        .get_account(Address::from_str_const(
            "SysvarC1ock11111111111111111111111111111111",
        ))?
        .ok_or_else(|| anyhow!("no Clock sysvar account"))?;
    let bytes: [u8; 8] = clock
        .data
        .get(32..40)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| anyhow!("short Clock sysvar"))?;
    Ok(i64::from_le_bytes(bytes))
}

/// Writes the oracle price account: a fully verified price update of
/// [`FEED_ID`] at `price`, published at the validator's current clock, owned
/// by the oracle program. Uses surfpool's `surfnet_setAccount` cheatcode;
/// on a real cluster only the oracle program writes such an account.
pub fn set_oracle_price(env: &TestEnv, price: u64) -> Result<()> {
    let now = clock_now(env.localnet.client.rpc())?;
    write_oracle(env, price, now)
}

/// [`set_oracle_price`] with a publish time a day behind the validator's
/// clock, which the program refuses as stale.
pub fn set_stale_oracle_price(env: &TestEnv, price: u64) -> Result<()> {
    let now = clock_now(env.localnet.client.rpc())?;
    write_oracle(env, price, now - 86_400)
}

fn write_oracle(env: &TestEnv, price: u64, publish_time: i64) -> Result<()> {
    let rpc = env.localnet.client.rpc();
    let data = encode_price_update(&FEED_ID, i64::try_from(price)?, publish_time);
    let hex: String = data.iter().map(|byte| format!("{byte:02x}")).collect();
    rpc_call(
        &env.rpc_url,
        "surfnet_setAccount",
        json!([
            ORACLE.to_string(),
            {
                "lamports": 10_000_000u64,
                "data": hex,
                "owner": ORACLE_PROGRAM_ID.to_string(),
                "executable": false,
            }
        ]),
    )?;
    // Read it back: the program must see exactly these bytes.
    let account = rpc
        .get_account(ORACLE)?
        .ok_or_else(|| anyhow!("the oracle account was not written"))?;
    if account.data != data || account.owner != ORACLE_PROGRAM_ID {
        bail!("the oracle account does not hold the price update");
    }
    Ok(())
}

/// The compute units a confirmed transaction used and its serialized size.
pub fn report(env: &TestEnv, label: &str, signature: &Signature) -> Result<(u64, usize)> {
    let result = rpc_call(
        &env.rpc_url,
        "getTransaction",
        json!([
            signature.to_string(),
            { "encoding": "base64", "maxSupportedTransactionVersion": 1, "commitment": "confirmed" }
        ]),
    )?;
    let units = result
        .pointer("/meta/computeUnitsConsumed")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("{label}: no compute units in {result}"))?;
    let encoded = result
        .pointer("/transaction/0")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("{label}: no transaction bytes"))?;
    // Base64 without decoding: four characters per three bytes, less padding.
    let padding = encoded
        .bytes()
        .rev()
        .take_while(|byte| *byte == b'=')
        .count();
    let bytes = encoded.len() / 4 * 3 - padding;
    println!("MEASURED {label}: {units} compute units, {bytes} byte transaction");
    Ok((units, bytes))
}

/// The custom program error code a failed send reports, as the validator
/// prints it.
pub fn assert_custom_error(error: &anyhow::Error, code: u32, what: &str) {
    assert!(
        format!("{error:#}").contains(&format!("custom program error: {code:#x}")),
        "expected {what} ({code:#x}): {error:#}"
    );
}
