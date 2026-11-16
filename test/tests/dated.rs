//! Dated contracts (options and predictions) and the veil, end to end on a
//! real localnet: validator, Photon indexer and prover.

mod shared;

use std::time::Duration;

use anyhow::{anyhow, Result};
use perps_program::error::PerpError;
use perps_sdk::{
    index::{index_maker_option_quotes, index_maker_options, index_veil_notes},
    instructions::{
        option::{
            OptionCancelTransactionParams, OptionOpenTransactionParams,
            OptionQuoteTransactionParams, OptionSettleTransactionParams,
        },
        veil::{VeilInTransactionParams, VeilOutTransactionParams},
    },
    shared::identity,
    state::{claim_of, OptionQuoteTerms, OptionQuoteUtxo, OptionUtxo, KIND_ABOVE, KIND_CALL},
};
use shared::*;
use spr_client::{user_registry::ensure_registered, Rpc};
use spr_test_utils::{test_validator_asserts::wait_for_merkle_proof, wallet::Filter};

const FAR_EXPIRY: u64 = 2_000_000_000;
/// What the trader pays for each contract here, and what the maker locks.
const PREMIUM: u64 = 4 * USDC;
const COLLATERAL: u64 = 26 * USDC;
const STRIKE: u64 = 100 * USD;

fn register(env: &TestEnv) -> Result<()> {
    for party in [&env.maker, &env.trader] {
        ensure_registered(env.localnet.client.rpc(), &party.keypair, &party.keypair)
            .map_err(|e| anyhow!("register: {e:?}"))?;
    }
    Ok(())
}

/// The maker quotes a contract of `kind` struck at $100, designated to the
/// trader, from a note of its own that covers the collateral.
fn post_quote(env: &mut TestEnv, kind: u8, term_end: u64, expiry: u64) -> Result<OptionQuoteUtxo> {
    env.maker.synced_balance(&env.localnet.client, env.mint)?;
    let source = env
        .maker
        .balance(env.mint, Some(Filter::MinAmount(COLLATERAL)))?
        .utxos
        .first()
        .cloned()
        .ok_or_else(|| anyhow!("no maker note of {COLLATERAL}"))?;
    let client = &env.localnet.client;
    let maker_pubkey = env.maker.keypair.shielded_address()?.solana_address()?;
    let trader = env.trader.keypair.shielded_address()?;
    let transaction = OptionQuoteTransactionParams {
        terms: OptionQuoteTerms {
            feed_id: FEED_ID,
            kind,
            size: if kind >= KIND_ABOVE { 0 } else { SIZE },
            strike: STRIKE,
            premium: PREMIUM,
            term_end,
            maker: env.maker.keypair.shielded_address()?,
            expiry,
            trader: Some(identity(&trader.solana_address()?)?),
        },
        collateral: COLLATERAL,
        source,
        trader_address: trader,
        payer: maker_pubkey,
    }
    .build(&env.maker.keypair)?;
    let quote = transaction.quote_utxo.clone();
    let instruction = transaction
        .prove(client.indexer(), env.localnet.tree, &env.maker.keypair)?
        .option_instruction()?;
    let signature = send(client.rpc(), &env.maker.keypair, instruction)?;
    client
        .confirm_private_transaction_sync(signature)
        .map_err(|e| anyhow!("confirm option quote indexed: {e:?}"))?;
    report(env, "option quote", &signature)?;
    Ok(quote)
}

/// The trader opens `quote`, whose opening the maker handed it, with a note
/// of its own that covers the premium.
fn open(env: &mut TestEnv, quote: &OptionQuoteUtxo) -> Result<(OptionUtxo, u64)> {
    env.trader.synced_balance(&env.localnet.client, env.mint)?;
    let trader_input = env
        .trader
        .balance(env.mint, Some(Filter::MinAmount(PREMIUM)))?
        .utxos
        .first()
        .cloned()
        .ok_or_else(|| anyhow!("no trader note of {PREMIUM}"))?;
    let client = &env.localnet.client;
    let quote_leaf = wait_for_merkle_proof(
        client.indexer(),
        env.localnet.tree,
        quote.hash(env.localnet.tree_id)?,
    )
    .leaf_index;
    let transaction = OptionOpenTransactionParams {
        quote_utxo: quote.clone(),
        quote_leaf_index: quote_leaf,
        trader_input,
        payer: env.trader.keypair.shielded_address()?.solana_address()?,
        tree_id: env.localnet.tree_id,
    }
    .build(&env.trader.keypair)?;
    let option = transaction.option_utxo.clone();
    let instruction = transaction
        .prove(client.indexer(), env.localnet.tree, &env.trader.keypair)?
        .option_instruction()?;
    let signature = send(client.rpc(), &env.trader.keypair, instruction)?;
    client
        .confirm_private_transaction_sync(signature)
        .map_err(|e| anyhow!("confirm option open indexed: {e:?}"))?;
    report(env, "option open", &signature)?;
    let leaf = wait_for_merkle_proof(
        client.indexer(),
        env.localnet.tree,
        option.hash(env.localnet.tree_id)?,
    )
    .leaf_index;
    Ok((option, leaf))
}

/// Waits until the validator's clock has reached `time`.
fn wait_until(env: &TestEnv, time: u64) -> Result<()> {
    for _ in 0..240 {
        if clock_now(env.localnet.client.rpc())? >= time as i64 {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(anyhow!("the validator clock never reached {time}"))
}

// A prediction and a call, each from quote to settle, and a quote nobody
// took.
//
// Flow:
//   1. The maker quotes a prediction that the price ends at or above $100,
//      ending in a few seconds: 26 of collateral against a premium of 4.
//      The trader opens it; the maker finds it from the indexer.
//   2. Before the end nobody can settle it. The SDK refuses both parties,
//      and a proof made for a later publish time than the oracle's fails on
//      chain: the program hashes the oracle's own publish time into the
//      proof's public input.
//   3. The term ends with the oracle at $104. The maker settles: all 30 to
//      the trader.
//   4. The maker quotes a call of one unit struck at $100, ending far in the
//      future. The trader opens it, and with the oracle at $107 exercises it
//      long before the end: 7 to the trader, 23 to the maker.
//   5. The maker quotes once more, the quote expires unopened, and the maker
//      takes its collateral back.
//
// Net: trader 50 - 4 + 30 - 4 + 7 = 79, maker 100 - 26 - 26 + 23 = 71.
#[test]
fn a_prediction_and_a_call_run_their_course() -> Result<()> {
    let mut env = setup(4)?;
    register(&env)?;
    let maker_pubkey = env.maker.keypair.shielded_address()?.solana_address()?;
    let trader_pubkey = env.trader.keypair.shielded_address()?.solana_address()?;

    // 1. A prediction, ending shortly.
    set_oracle_price(&env, STRIKE)?;
    let term_end = clock_now(env.localnet.client.rpc())? as u64 + 45;
    let quote = post_quote(&mut env, KIND_ABOVE, term_end, FAR_EXPIRY)?;
    let found = index_maker_option_quotes(
        &env.maker.keypair,
        &env.maker.registry,
        env.localnet.client.indexer(),
        Duration::from_secs(60),
    )?;
    assert!(found.iter().any(|found| found.quote_utxo == quote));
    let (option, leaf) = open(&mut env, &quote)?;
    assert_eq!(option.total, PREMIUM + COLLATERAL);
    let discovered = index_maker_options(
        &env.maker.keypair,
        &env.maker.registry,
        env.localnet.client.indexer(),
        env.localnet.client.rpc(),
        Duration::from_secs(60),
    )?
    .pop()
    .ok_or_else(|| anyhow!("no option discovered"))?;
    assert_eq!(discovered.option_utxo, option);
    assert_eq!(discovered.leaf_index, leaf);

    // 2. Before the end: the SDK refuses both parties.
    let now = clock_now(env.localnet.client.rpc())? as u64;
    assert!(
        now < term_end,
        "the fixture's term ended before the test got here"
    );
    set_oracle_price(&env, 104 * USD)?;
    let settle = |payer, publish_time| OptionSettleTransactionParams {
        option_utxo: option.clone(),
        option_leaf_index: leaf,
        oracle: ORACLE,
        price: 104 * USD,
        publish_time,
        payer,
        tree_id: env.localnet.tree_id,
    };
    for (payer, keypair) in [
        (maker_pubkey, &env.maker.keypair),
        (trader_pubkey, &env.trader.keypair),
    ] {
        assert!(settle(payer, now).build(keypair).is_err());
    }
    // A proof for the end of the term, sent while the oracle's price is
    // older than that: it does not verify.
    let early = settle(maker_pubkey, term_end)
        .build(&env.maker.keypair)?
        .prove(env.localnet.client.indexer(), env.localnet.tree)?
        .option_instruction()?;
    let error = send(env.localnet.client.rpc(), &env.maker.keypair, early)
        .err()
        .ok_or_else(|| anyhow!("a prediction settled before its end"))?;
    assert_custom_error(
        &error,
        PerpError::ProofVerificationFailed as u32,
        "a settle proved for another publish time",
    );

    // 3. The term ends; the oracle publishes $104 after it.
    wait_until(&env, term_end)?;
    set_oracle_price(&env, 104 * USD)?;
    let published = clock_now(env.localnet.client.rpc())? as u64;
    // The price account holds the clock at the moment it was written, which
    // the helper read just before: read the account's own publish time.
    let account = env
        .localnet
        .client
        .rpc()
        .get_account(ORACLE)?
        .ok_or_else(|| anyhow!("no oracle account"))?;
    let publish_time = perps_sdk::OraclePrice::parse(&account.data)
        .map_err(|e| anyhow!("oracle: {e:?}"))?
        .publish_time as u64;
    assert!(publish_time >= term_end && publish_time <= published);
    let transaction = settle(maker_pubkey, publish_time).build(&env.maker.keypair)?;
    assert_eq!(
        (transaction.trader_payout, transaction.maker_payout),
        (PREMIUM + COLLATERAL, 0)
    );
    let instruction = transaction
        .prove(env.localnet.client.indexer(), env.localnet.tree)?
        .option_instruction()?;
    let signature = send(env.localnet.client.rpc(), &env.maker.keypair, instruction)?;
    env.localnet
        .client
        .confirm_private_transaction_sync(signature)
        .map_err(|e| anyhow!("confirm option settle indexed: {e:?}"))?;
    report(&env, "option settle", &signature)?;
    assert_eq!(
        env.trader.synced_balance(&env.localnet.client, env.mint)?,
        TRADER_SHIELD - PREMIUM + PREMIUM + COLLATERAL
    );
    assert_eq!(
        env.maker.synced_balance(&env.localnet.client, env.mint)?,
        MAKER_SHIELD - COLLATERAL
    );

    // 4. A call, exercised by the trader long before its end.
    let quote = post_quote(&mut env, KIND_CALL, FAR_EXPIRY, FAR_EXPIRY)?;
    let (option, leaf) = open(&mut env, &quote)?;
    set_oracle_price(&env, 107 * USD)?;
    let account = env
        .localnet
        .client
        .rpc()
        .get_account(ORACLE)?
        .ok_or_else(|| anyhow!("no oracle account"))?;
    let publish_time = perps_sdk::OraclePrice::parse(&account.data)
        .map_err(|e| anyhow!("oracle: {e:?}"))?
        .publish_time as u64;
    let exercise = |payer| OptionSettleTransactionParams {
        option_utxo: option.clone(),
        option_leaf_index: leaf,
        oracle: ORACLE,
        price: 107 * USD,
        publish_time,
        payer,
        tree_id: env.localnet.tree_id,
    };
    // The maker cannot end it early.
    assert!(exercise(maker_pubkey).build(&env.maker.keypair).is_err());
    let transaction = exercise(trader_pubkey).build(&env.trader.keypair)?;
    assert_eq!(
        (transaction.trader_payout, transaction.maker_payout),
        (7 * USDC, 23 * USDC)
    );
    let instruction = transaction
        .prove(env.localnet.client.indexer(), env.localnet.tree)?
        .option_instruction()?;
    let signature = send(env.localnet.client.rpc(), &env.trader.keypair, instruction)?;
    env.localnet
        .client
        .confirm_private_transaction_sync(signature)
        .map_err(|e| anyhow!("confirm option exercise indexed: {e:?}"))?;
    report(&env, "option exercise", &signature)?;
    assert_eq!(
        env.trader.synced_balance(&env.localnet.client, env.mint)?,
        79 * USDC
    );
    assert_eq!(
        env.maker.synced_balance(&env.localnet.client, env.mint)?,
        71 * USDC
    );

    // 5. A quote nobody takes: the maker reclaims its collateral once the
    // quote has expired, and not before.
    let expiry = clock_now(env.localnet.client.rpc())? as u64 + 40;
    let quote = post_quote(&mut env, KIND_CALL, FAR_EXPIRY, expiry)?;
    assert_eq!(
        env.maker.synced_balance(&env.localnet.client, env.mint)?,
        71 * USDC - COLLATERAL
    );
    let client = &env.localnet.client;
    let quote_leaf = wait_for_merkle_proof(
        client.indexer(),
        env.localnet.tree,
        quote.hash(env.localnet.tree_id)?,
    )
    .leaf_index;
    let cancel = || -> Result<_> {
        OptionCancelTransactionParams {
            quote_utxo: quote.clone(),
            quote_leaf_index: quote_leaf,
            payer: maker_pubkey,
            tree_id: env.localnet.tree_id,
        }
        .build(&env.maker.keypair)?
        .prove(client.indexer(), env.localnet.tree)?
        .option_instruction()
    };
    if (clock_now(client.rpc())? as u64) < expiry {
        let error = send(client.rpc(), &env.maker.keypair, cancel()?)
            .err()
            .ok_or_else(|| anyhow!("a live quote was cancelled"))?;
        assert_custom_error(
            &error,
            PerpError::QuoteNotYetExpired as u32,
            "a cancel before the expiry",
        );
    }
    wait_until(&env, expiry + 1)?;
    let signature = send(client.rpc(), &env.maker.keypair, cancel()?)?;
    client
        .confirm_private_transaction_sync(signature)
        .map_err(|e| anyhow!("confirm option cancel indexed: {e:?}"))?;
    report(&env, "option cancel", &signature)?;
    assert_eq!(
        env.maker.synced_balance(&env.localnet.client, env.mint)?,
        71 * USDC
    );
    Ok(())
}

/// A veil secret: any canonical field element.
fn secret(byte: u8) -> [u8; 32] {
    let mut secret = [byte; 32];
    secret[0] = 0;
    secret
}

// Money goes behind the veil from one address and comes out at another, and
// nothing on chain says the two are the same money.
//
// Flow:
//   1. The trader puts 40 of its 50 behind the veil, under a secret.
//   2. It finds the note again from the indexer.
//   3. The maker, which has never been named by that note, pays it out to
//      itself with the secret. The maker signs only as the fee payer.
//   4. The note is spent: it does not pay out twice.
//   5. The maker now holds two notes, of 100 and 40. It puts 120 behind the
//      veil from both at once, and the trader pays that note out.
//
// Net: trader 50 - 40 + 120 = 130, maker 100 + 40 - 120 = 20.
#[test]
fn money_goes_through_the_veil() -> Result<()> {
    let mut env = setup(5)?;
    let maker_pubkey = env.maker.keypair.shielded_address()?.solana_address()?;
    let trader_pubkey = env.trader.keypair.shielded_address()?.solana_address()?;

    // 1. The trader's 40 goes behind the veil.
    let sources = env
        .trader
        .balance(env.mint, Some(Filter::MinAmount(40 * USDC)))?
        .utxos;
    let client = &env.localnet.client;
    let transaction = VeilInTransactionParams {
        sources: sources.into_iter().take(1).collect(),
        amount: 40 * USDC,
        claim: claim_of(&secret(0x5a))?,
        payer: trader_pubkey,
        tree_id: env.localnet.tree_id,
    }
    .build(&env.trader.keypair)?;
    assert_eq!(transaction.change, 10 * USDC);
    let note = transaction.note.clone();
    let instruction = transaction
        .prove(client.indexer(), env.localnet.tree, &env.trader.keypair)?
        .instruction()?;
    let signature = send(client.rpc(), &env.trader.keypair, instruction)?;
    client
        .confirm_private_transaction_sync(signature)
        .map_err(|e| anyhow!("confirm veil in indexed: {e:?}"))?;
    report(&env, "veil in", &signature)?;
    assert_eq!(
        env.trader.synced_balance(&env.localnet.client, env.mint)?,
        10 * USDC
    );

    // 2. The depositor finds its note again; nobody else can read it.
    let client = &env.localnet.client;
    let found = index_veil_notes(&env.trader.keypair, &env.trader.registry, client.indexer())?;
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].note, note);
    let leaf = found[0].leaf_index;
    assert!(
        index_veil_notes(&env.maker.keypair, &env.maker.registry, client.indexer())?.is_empty()
    );

    // 3. The maker pays the note out to itself. A wrong secret does not
    // build.
    let maker_address = env.maker.keypair.shielded_address()?;
    let tree_id = env.localnet.tree_id;
    let pay_out = |secret: [u8; 32]| VeilOutTransactionParams {
        note: note.clone(),
        leaf_index: leaf,
        secret,
        recipient: maker_address,
        payer: maker_pubkey,
        tree_id,
    };
    assert!(pay_out(secret(0x5b)).build(&env.maker.keypair).is_err());
    let transaction = pay_out(secret(0x5a)).build(&env.maker.keypair)?;
    assert_eq!(transaction.amount, 40 * USDC);
    let instruction = transaction
        .prove(client.indexer(), env.localnet.tree)?
        .instruction()?;
    let signature = send(client.rpc(), &env.maker.keypair, instruction)?;
    client
        .confirm_private_transaction_sync(signature)
        .map_err(|e| anyhow!("confirm veil out indexed: {e:?}"))?;
    report(&env, "veil out", &signature)?;
    assert_eq!(
        env.maker.synced_balance(&env.localnet.client, env.mint)?,
        MAKER_SHIELD + 40 * USDC
    );

    // 4. The note is spent.
    let client = &env.localnet.client;
    let again = pay_out(secret(0x5a))
        .build(&env.maker.keypair)?
        .prove(client.indexer(), env.localnet.tree);
    let spent = match again {
        Err(_) => true,
        Ok(out) => send(client.rpc(), &env.maker.keypair, out.instruction()?).is_err(),
    };
    assert!(spent, "a veil note must not pay out twice");
    assert!(
        index_veil_notes(&env.trader.keypair, &env.trader.registry, client.indexer())?.is_empty()
    );

    // 5. The maker gathers both its notes into one veil note of 120.
    let sources = env.maker.balance(env.mint, None)?.utxos;
    assert_eq!(
        sources.len(),
        2,
        "the maker holds its deposit and the payout"
    );
    let transaction = VeilInTransactionParams {
        sources,
        amount: 120 * USDC,
        claim: claim_of(&secret(0x33))?,
        payer: maker_pubkey,
        tree_id: env.localnet.tree_id,
    }
    .build(&env.maker.keypair)?;
    assert_eq!(transaction.change, 20 * USDC);
    let note = transaction.note.clone();
    let instruction = transaction
        .prove(client.indexer(), env.localnet.tree, &env.maker.keypair)?
        .instruction()?;
    let signature = send(client.rpc(), &env.maker.keypair, instruction)?;
    client
        .confirm_private_transaction_sync(signature)
        .map_err(|e| anyhow!("confirm second veil in indexed: {e:?}"))?;
    report(&env, "veil in, two notes", &signature)?;
    let leaf = wait_for_merkle_proof(
        client.indexer(),
        env.localnet.tree,
        note.hash(env.localnet.tree_id)?,
    )
    .leaf_index;
    let transaction = VeilOutTransactionParams {
        note,
        leaf_index: leaf,
        secret: secret(0x33),
        recipient: env.trader.keypair.shielded_address()?,
        payer: trader_pubkey,
        tree_id: env.localnet.tree_id,
    }
    .build(&env.trader.keypair)?;
    let instruction = transaction
        .prove(client.indexer(), env.localnet.tree)?
        .instruction()?;
    let signature = send(client.rpc(), &env.trader.keypair, instruction)?;
    client
        .confirm_private_transaction_sync(signature)
        .map_err(|e| anyhow!("confirm second veil out indexed: {e:?}"))?;
    report(&env, "veil out to the trader", &signature)?;
    assert_eq!(
        env.trader.synced_balance(&env.localnet.client, env.mint)?,
        130 * USDC
    );
    assert_eq!(
        env.maker.synced_balance(&env.localnet.client, env.mint)?,
        20 * USDC
    );
    Ok(())
}
