//! One position's whole life on devnet: the deployed perps program, the
//! Rings pool Helius runs there, and the live Pyth SOL/USD price.
//!
//! Ignored by default, because it spends devnet SOL and needs:
//!
//!   DEVNET_RPC_URL   a devnet Solana RPC
//!   DEVNET_SPR_URL   the Rings indexer, `https://devnet.helius-rpc.com/v1/spr?api-key=...`
//!   ZOLANA_CLI_BIN   the CLI that launches the local prover
//!
//! and the wallets and test dollar `scripts/devnet-setup.sh` makes in
//! `keys/`. The pool proof is made by a prover on this machine, so no
//! witness leaves it; the indexer only serves Merkle paths and ciphertexts.
//!
//!   cargo test --release -p perps-test --test devnet -- --ignored --nocapture

use std::{env, path::Path, thread::sleep, time::Duration};

use anyhow::{anyhow, bail, ensure, Context, Result};
use perps_sdk::{
    index::{index_maker_positions, index_trader_quotes},
    instructions::{
        open::OpenTransactionParams, quote::QuoteTransactionParams,
        settle::SettleTransactionParams,
    },
    shared::identity,
    state::QuoteTerms,
    OraclePrice, MAX_PRICE_AGE_SECS,
};
use solana_address::Address;
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_signature::Signature;
use solana_signer::Signer;
use spr_client::{
    fetch_asset_id, user_registry::ensure_registered, ComputeBudgetConfig, ProofDataSource,
    ProverClient, ProverLaunch, Rpc, SolanaRpc, SprClient, SprIndexer,
};
use spr_interface::{pda, state::tree::read_tree_id};
use spr_keypair::{ShieldedKeypair, SigningKey};
use spr_program::instruction::CreateSplInterface;
use spr_test_utils::{
    test_validator_asserts::{wait_for_indexed_utxo, wait_for_merkle_proof},
    wallet::{sync_wallet, Deposit, DepositParams, Filter, Wallet},
};
use spr_transaction::AssetRegistry;

// The whole per-transaction budget: an instruction verifies an SPP proof and
// its own.
const TRANSACT_COMPUTE_UNIT_LIMIT: u32 = 1_400_000;

/// Base units per test dollar.
const USDC: u64 = 1_000_000;
/// One SOL of exposure, at whatever the oracle reads when the quote is made.
const SIZE: u64 = 10_000_000;
const TRADER_MARGIN: u64 = 20 * USDC;
const MAKER_MARGIN: u64 = 30 * USDC;
const MAINTENANCE_MARGIN: u64 = 5 * USDC;
/// What a wallet is topped up to when it cannot cover its margin.
const SHIELD: u64 = 100 * USDC;

/// Pyth SOL/USD, and the price update account Pyth keeps current for it on
/// devnet.
const FEED_ID: [u8; 32] = [
    0xef, 0x0d, 0x8b, 0x6f, 0xda, 0x2c, 0xeb, 0xa4, 0x1d, 0xa1, 0x5d, 0x40, 0x95, 0xd1, 0xda, 0x39,
    0x2a, 0x0d, 0x2f, 0x8e, 0xd0, 0xc6, 0xc7, 0xbc, 0x0f, 0x4c, 0xfa, 0xc8, 0xc2, 0x80, 0xb5, 0x6d,
];
const ORACLE: Address = Address::from_str_const("7UVimffxr9ow1uXYxsr4LHAcV58mLzhmwaeKvJ1pjLiE");

fn var(name: &str) -> Result<String> {
    env::var(name).map_err(|_| anyhow!("{name} is not set; see the top of test/tests/devnet.rs"))
}

/// The first 32 bytes of a Solana CLI keypair file: the ed25519 seed.
fn seed(name: &str) -> Result<[u8; 32]> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../keys")
        .join(name);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("{}: run scripts/devnet-setup.sh", path.display()))?;
    let bytes: Vec<u8> = serde_json::from_str(&text)?;
    bytes
        .get(..32)
        .and_then(|seed| seed.try_into().ok())
        .ok_or_else(|| anyhow!("{} is not a keypair file", path.display()))
}

fn shielded(name: &str) -> Result<ShieldedKeypair> {
    Ok(ShieldedKeypair::from_keypair(
        SigningKey::from_ed25519_bytes(&seed(name)?),
    )?)
}

fn send(rpc: &SolanaRpc, payer: &dyn Signer, ix: Instruction) -> Result<Signature> {
    Ok(rpc.create_and_send_transaction(
        std::slice::from_ref(&ix),
        payer.pubkey(),
        &[payer],
        ComputeBudgetConfig::new(TRANSACT_COMPUTE_UNIT_LIMIT),
    )?)
}

fn explorer(label: &str, signature: &Signature) {
    println!("DEVNET {label}: https://explorer.solana.com/tx/{signature}?cluster=devnet");
}

/// The oracle account's price, whatever its age.
fn oracle_price(rpc: &SolanaRpc) -> Result<OraclePrice> {
    let account = rpc
        .get_account(ORACLE)?
        .ok_or_else(|| anyhow!("no oracle account {ORACLE}"))?;
    OraclePrice::parse(&account.data).map_err(|e| anyhow!("oracle account: {e:?}"))
}

/// Waits for the oracle account's next update and returns it young enough
/// that a settle proved now still lands inside the program's window.
///
/// Pyth refreshes this account on devnet about every five minutes, and the
/// program accepts a price for [`MAX_PRICE_AGE_SECS`], so a settle against
/// it only lands in the minute after an update.
fn fresh_price(rpc: &SolanaRpc) -> Result<OraclePrice> {
    for _ in 0..240 {
        let price = oracle_price(rpc)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs() as i64;
        if now - price.publish_time <= MAX_PRICE_AGE_SECS / 3 {
            return Ok(price);
        }
        sleep(Duration::from_secs(2));
    }
    bail!("the oracle account was not updated for eight minutes")
}

#[test]
#[ignore = "runs against devnet; see the top of this file"]
fn a_position_lives_on_devnet() -> Result<()> {
    let rpc_url = var("DEVNET_RPC_URL")?;
    let spr_url = var("DEVNET_SPR_URL")?;

    // The pool proof is made here, by the release prover with the release
    // keys: the pool on devnet is the same binary the localnet tests load.
    let home = var("HOME")?;
    ProverLaunch::new_with_cli(var("ZOLANA_CLI_BIN")?)?
        .with_keys_dir(Path::new(&home).join(".config/zolana/proving-keys"))?
        .spawn()?;

    let rpc = SolanaRpc::new(rpc_url);
    rpc.assert_executable(&perps_program::ID)?;
    let client = SprClient::new(rpc, SprIndexer::new(&spr_url), ProverClient::default())
        .with_proof_data_source(ProofDataSource::Client);
    let tree = pda::tree(0);
    let tree_id = client
        .rpc()
        .get_account(tree)?
        .and_then(|account| read_tree_id(&account.data))
        .ok_or_else(|| anyhow!("no default tree {tree}"))?;

    // The test dollar, registered with the pool on first use. Anyone may
    // register a mint on devnet.
    let deployer = Keypair::new_from_array(seed("devnet-deployer.json")?);
    let mint = Keypair::new_from_array(seed("devnet-mint.json")?).pubkey();
    if client
        .rpc()
        .get_account(pda::spl_asset_registry(&mint))?
        .is_none()
    {
        let instruction = CreateSplInterface {
            authority: deployer.pubkey(),
            mint,
            token_program: pda::spl_token_program_id(),
        }
        .instruction();
        let signature = client.rpc().create_and_send_transaction(
            std::slice::from_ref(&instruction),
            deployer.pubkey(),
            &[&deployer],
            ComputeBudgetConfig::for_instruction_count(1),
        )?;
        explorer("register the test dollar", &signature);
    }
    let asset_id = fetch_asset_id(client.rpc(), mint)?;
    let mut assets = AssetRegistry::default();
    assets.insert(asset_id, mint)?;
    println!("DEVNET test dollar {mint} is pool asset {asset_id}");

    let maker_keypair = shielded("devnet-maker.json")?;
    let trader_keypair = shielded("devnet-trader.json")?;
    let maker_address = maker_keypair.shielded_address()?;
    let trader_address = trader_keypair.shielded_address()?;
    let maker_pubkey = maker_address.solana_address()?;
    let trader_pubkey = trader_address.solana_address()?;
    let mut maker =
        Wallet::new(maker_address, assets.clone()).map_err(|e| anyhow!("maker wallet: {e:?}"))?;
    let mut trader =
        Wallet::new(trader_address, assets.clone()).map_err(|e| anyhow!("trader wallet: {e:?}"))?;

    // Shield test dollars to whichever wallet cannot cover its margin with
    // one note.
    let funding = pda::associated_token_address(&deployer.pubkey(), &mint);
    let mut deposits = Vec::new();
    for (wallet, keypair, margin) in [
        (&mut maker, &maker_keypair, MAKER_MARGIN),
        (&mut trader, &trader_keypair, TRADER_MARGIN),
    ] {
        sync_wallet(wallet, keypair, &client).map_err(|e| anyhow!("sync: {e:?}"))?;
        if !wallet
            .balance(mint, Some(Filter::MinAmount(margin)))?
            .utxos
            .is_empty()
        {
            continue;
        }
        let deposit = Deposit::new(DepositParams {
            recipient: &keypair.shielded_address()?,
            asset: mint,
            amount: SHIELD,
            spl_token_account: Some(funding),
            spl_token_program: Some(pda::spl_token_program_id()),
            memo: None,
        })?;
        let view_tag = deposit.view_tag();
        let signature = deposit.send(&client, &deployer, tree, &deployer)?;
        explorer("shield", &signature);
        deposits.push((view_tag, signature));
    }
    for (view_tag, signature) in deposits {
        wait_for_indexed_utxo(&client, view_tag, signature);
    }
    sync_wallet(&mut maker, &maker_keypair, &client).map_err(|e| anyhow!("sync: {e:?}"))?;
    sync_wallet(&mut trader, &trader_keypair, &client).map_err(|e| anyhow!("sync: {e:?}"))?;
    let maker_before = maker.balance(mint, None)?.amount;
    let trader_before = trader.balance(mint, None)?.amount;
    println!("DEVNET sealed balances: maker {maker_before}, trader {trader_before}");

    for keypair in [&maker_keypair, &trader_keypair] {
        ensure_registered(client.rpc(), keypair, keypair)
            .map_err(|e| anyhow!("register: {e:?}"))?;
    }

    // 1. The maker quotes the trader a long of one SOL at the oracle price.
    // The entry price is a term of the quote, so the oracle's age does not
    // matter here.
    let entry = oracle_price(client.rpc())?;
    println!("DEVNET oracle reads {} ticks at the quote", entry.price);
    let terms = QuoteTerms {
        feed_id: FEED_ID,
        trader_is_long: true,
        size: SIZE,
        entry_price: entry.price,
        trader_margin: TRADER_MARGIN,
        maintenance_margin: MAINTENANCE_MARGIN,
        maker: maker_keypair.shielded_address()?,
        expiry: 2_000_000_000,
        trader: Some(identity(&trader_pubkey)?),
    };
    let source = maker
        .balance(mint, Some(Filter::MinAmount(MAKER_MARGIN)))?
        .utxos
        .first()
        .cloned()
        .ok_or_else(|| anyhow!("the maker has no note of {MAKER_MARGIN}"))?;
    let transaction = QuoteTransactionParams {
        terms,
        maker_margin: MAKER_MARGIN,
        source,
        trader_address: trader_keypair.shielded_address()?,
        payer: maker_pubkey,
    }
    .build(&maker_keypair)?;
    let posted = transaction.quote_utxo.clone();
    let instruction = transaction
        .prove(client.indexer(), tree, &maker_keypair)?
        .instruction()?;
    let signature = send(client.rpc(), &maker_keypair, instruction)?;
    explorer("quote", &signature);
    client
        .confirm_private_transaction_sync(signature)
        .map_err(|e| anyhow!("confirm quote indexed: {e:?}"))?;

    // 2. The trader finds the quote addressed to it and opens.
    let quote = index_trader_quotes(
        &trader_keypair,
        &trader.registry,
        client.indexer(),
        client.rpc(),
        Duration::from_secs(120),
    )?
    .into_iter()
    .find(|quote| quote.quote_utxo == posted)
    .ok_or_else(|| anyhow!("the trader did not discover the quote"))?;
    ensure!(quote.maker_pubkey == maker_pubkey, "wrong maker on the quote");
    wait_for_merkle_proof(client.indexer(), tree, quote.quote_utxo.hash(tree_id)?);
    sync_wallet(&mut trader, &trader_keypair, &client).map_err(|e| anyhow!("sync: {e:?}"))?;
    let trader_input = trader
        .balance(mint, Some(Filter::MinAmount(TRADER_MARGIN)))?
        .utxos
        .first()
        .cloned()
        .ok_or_else(|| anyhow!("the trader has no note of {TRADER_MARGIN}"))?;
    let transaction = OpenTransactionParams {
        quote_utxo: quote.quote_utxo,
        quote_leaf_index: quote.leaf_index,
        trader_input,
        payer: trader_pubkey,
        tree_id,
    }
    .build(&trader_keypair)?;
    let opened = transaction.position_utxo.clone();
    let instruction = transaction
        .prove(client.indexer(), tree, &trader_keypair)?
        .instruction()?;
    let signature = send(client.rpc(), &trader_keypair, instruction)?;
    explorer("open", &signature);
    client
        .confirm_private_transaction_sync(signature)
        .map_err(|e| anyhow!("confirm open indexed: {e:?}"))?;
    assert_eq!(opened.total, MAKER_MARGIN + TRADER_MARGIN);

    // 3. The maker finds the position from the indexer alone.
    let position = index_maker_positions(
        &maker_keypair,
        &maker.registry,
        client.indexer(),
        client.rpc(),
        Duration::from_secs(120),
    )?
    .into_iter()
    .find(|position| position.position_utxo == opened)
    .ok_or_else(|| anyhow!("the maker did not discover the position"))?;
    wait_for_merkle_proof(client.indexer(), tree, opened.hash(tree_id)?);

    // 4. The trader closes at whatever the oracle reads. The proof commits
    // to one price, so a settle that lands after the oracle moved is proved
    // again at the new price.
    let mut settled = None;
    for attempt in 1..=5 {
        let price = fresh_price(client.rpc())?.price;
        let transaction = SettleTransactionParams {
            position_utxo: opened.clone(),
            position_leaf_index: position.leaf_index,
            oracle: ORACLE,
            price,
            payer: trader_pubkey,
            tree_id,
        }
        .build(&trader_keypair)?;
        let payouts = (transaction.trader_payout, transaction.maker_payout);
        let instruction = transaction.prove(client.indexer(), tree)?.instruction()?;
        match send(client.rpc(), &trader_keypair, instruction) {
            Ok(signature) => {
                explorer("settle", &signature);
                client
                    .confirm_private_transaction_sync(signature)
                    .map_err(|e| anyhow!("confirm settle indexed: {e:?}"))?;
                settled = Some((price, payouts));
                break;
            }
            Err(error) => println!("DEVNET settle attempt {attempt} at {price}: {error:#}"),
        }
    }
    let (price, (trader_payout, maker_payout)) =
        settled.ok_or_else(|| anyhow!("no settle landed in five attempts"))?;
    println!(
        "DEVNET settled at {price} ticks (entry {}): trader {trader_payout}, maker {maker_payout}",
        entry.price
    );
    assert_eq!(trader_payout + maker_payout, MAKER_MARGIN + TRADER_MARGIN);

    // 5. Each wallet decrypts its own payout.
    sync_wallet(&mut maker, &maker_keypair, &client).map_err(|e| anyhow!("sync: {e:?}"))?;
    sync_wallet(&mut trader, &trader_keypair, &client).map_err(|e| anyhow!("sync: {e:?}"))?;
    assert_eq!(
        trader.balance(mint, None)?.amount,
        trader_before - TRADER_MARGIN + trader_payout
    );
    assert_eq!(
        maker.balance(mint, None)?.amount,
        maker_before - MAKER_MARGIN + maker_payout
    );
    Ok(())
}
