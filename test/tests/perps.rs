mod shared;

use std::time::Duration;

use anyhow::{anyhow, Result};
use perps_program::error::PerpError;
use perps_sdk::{
    index::{index_maker_positions, index_maker_quotes, index_trader_quotes},
    instructions::{
        cancel_quote::CancelQuoteTransactionParams, open::OpenTransactionParams,
        quote::QuoteTransactionParams, settle::SettleTransactionParams,
    },
    shared::identity,
    state::{PositionUtxo, QuoteTerms, QuoteUtxo},
};
use shared::*;
use spr_client::{user_registry::ensure_registered, Rpc};
use spr_test_utils::{test_validator_asserts::wait_for_merkle_proof, wallet::Filter};
use spr_transaction::SppProofOutputUtxo;

const FAR_EXPIRY: u64 = 2_000_000_000;

/// The quote every test posts: the trader long one unit from $100.
fn terms(env: &TestEnv, expiry: u64, designated: bool) -> Result<QuoteTerms> {
    let trader = env.trader.keypair.shielded_address()?.solana_address()?;
    Ok(QuoteTerms {
        feed_id: FEED_ID,
        trader_is_long: true,
        size: SIZE,
        entry_price: ENTRY_PRICE,
        trader_margin: TRADER_MARGIN,
        maintenance_margin: MAINTENANCE_MARGIN,
        maker: env.maker.keypair.shielded_address()?,
        expiry,
        trader: designated.then(|| identity(&trader)).transpose()?,
    })
}

/// Registers both parties, then the maker posts the quote. Returns the quote
/// as the maker built it.
fn post_quote(env: &TestEnv, expiry: u64, designated: bool) -> Result<QuoteUtxo> {
    let client = &env.localnet.client;
    for party in [&env.maker, &env.trader] {
        ensure_registered(client.rpc(), &party.keypair, &party.keypair)
            .map_err(|e| anyhow!("register: {e:?}"))?;
    }
    let maker_pubkey = env.maker.keypair.shielded_address()?.solana_address()?;
    let transaction = QuoteTransactionParams {
        terms: terms(env, expiry, designated)?,
        maker_margin: MAKER_MARGIN,
        source: env.maker_input.clone(),
        trader_address: env.trader.keypair.shielded_address()?,
        payer: maker_pubkey,
    }
    .build(&env.maker.keypair)?;
    let quote_utxo = transaction.quote_utxo.clone();
    let instruction = transaction
        .prove(client.indexer(), env.localnet.tree, &env.maker.keypair)?
        .instruction()?;
    let signature = send(client.rpc(), &env.maker.keypair, instruction)?;
    client
        .confirm_private_transaction_sync(signature)
        .map_err(|e| anyhow!("confirm quote indexed: {e:?}"))?;
    report(env, "quote", &signature)?;
    Ok(quote_utxo)
}

/// The trader discovers the quote addressed to it and opens the position.
/// Returns the position as the trader built it.
fn discover_and_open(env: &TestEnv, posted: &QuoteUtxo) -> Result<PositionUtxo> {
    let client = &env.localnet.client;
    let trader_pubkey = env.trader.keypair.shielded_address()?.solana_address()?;

    // The trader finds the marker, decrypts the quote and checks the maker
    // it names against the user registry.
    let quote = index_trader_quotes(
        &env.trader.keypair,
        &env.trader.registry,
        client.indexer(),
        client.rpc(),
        Duration::from_secs(60),
    )?
    .pop()
    .ok_or_else(|| anyhow!("no quote discovered"))?;
    assert_eq!(&quote.quote_utxo, posted);
    assert_eq!(
        quote.maker_pubkey,
        env.maker.keypair.shielded_address()?.solana_address()?
    );
    let quote_state = wait_for_merkle_proof(
        client.indexer(),
        env.localnet.tree,
        quote.quote_utxo.hash(env.localnet.tree_id)?,
    );
    assert_eq!(quote_state.leaf_index, quote.leaf_index);

    let trader_input = env
        .trader
        .balance(env.mint, Some(Filter::MinAmount(TRADER_MARGIN)))?
        .utxos
        .first()
        .cloned()
        .ok_or_else(|| anyhow!("no spendable note of {TRADER_MARGIN}"))?;
    let transaction = OpenTransactionParams {
        quote_utxo: quote.quote_utxo,
        quote_leaf_index: quote.leaf_index,
        trader_input,
        payer: trader_pubkey,
        tree_id: env.localnet.tree_id,
    }
    .build(&env.trader.keypair)?;
    let position_utxo = transaction.position_utxo.clone();
    let instruction = transaction
        .prove(client.indexer(), env.localnet.tree, &env.trader.keypair)?
        .instruction()?;
    let signature = send(client.rpc(), &env.trader.keypair, instruction)?;
    client
        .confirm_private_transaction_sync(signature)
        .map_err(|e| anyhow!("confirm open indexed: {e:?}"))?;
    report(env, "open", &signature)?;
    Ok(position_utxo)
}

/// The maker finds the position a trader opened against its quote, from the
/// indexer alone.
fn maker_discovers(env: &TestEnv, opened: &PositionUtxo) -> Result<u64> {
    let client = &env.localnet.client;
    let position = index_maker_positions(
        &env.maker.keypair,
        &env.maker.registry,
        client.indexer(),
        client.rpc(),
        Duration::from_secs(60),
    )?
    .pop()
    .ok_or_else(|| anyhow!("no position discovered"))?;
    assert_eq!(&position.position_utxo, opened);
    let state = wait_for_merkle_proof(
        client.indexer(),
        env.localnet.tree,
        opened.hash(env.localnet.tree_id)?,
    );
    assert_eq!(state.leaf_index, position.leaf_index);
    Ok(position.leaf_index)
}

// A trader opens a hidden long and closes it at a profit, on a real localnet
// (validator + Photon indexer + prover).
//
// Flow:
//   1. Fund (in setup): 100 of collateral is shielded to the maker, 50 to
//      the trader.
//   2. Quote: the maker locks 30 of margin in a quote UTXO designated to the
//      trader: long one unit from $100 against 20 of trader margin.
//   3. Discover and open: the trader finds the quote and adds its 20; both
//      margins sit in one position UTXO owned by the maker's authority PDA.
//   4. The maker discovers the position from the indexer.
//   5. The oracle moves to $105. The trader closes: 25 to the trader, 25 to
//      the maker.
//   6. Both payouts are indexed, and each wallet decrypts its own.
//
// Net: maker 100 -> 95, trader 50 -> 55. On chain: who quoted, opened and
// settled, and the price the settle read. Not the size, the direction, the
// margins, the entry price or what either side received.
#[test]
fn trader_opens_and_closes_at_a_profit() -> Result<()> {
    let mut env = setup(1)?;
    let trader_pubkey = env.trader.keypair.shielded_address()?.solana_address()?;

    let quote = post_quote(&env, FAR_EXPIRY, true)?;
    let position = discover_and_open(&env, &quote)?;
    assert_eq!(position.total, MAKER_MARGIN + TRADER_MARGIN);
    let leaf_index = maker_discovers(&env, &position)?;

    // 5. The price rises to $105 and the trader closes.
    let price = 105 * USD;
    set_oracle_price(&env, price)?;
    let client = &env.localnet.client;
    let transaction = SettleTransactionParams {
        position_utxo: position.clone(),
        position_leaf_index: leaf_index,
        oracle: ORACLE,
        price,
        payer: trader_pubkey,
        tree_id: env.localnet.tree_id,
    }
    .build(&env.trader.keypair)?;
    assert_eq!(
        (transaction.trader_payout, transaction.maker_payout),
        (25 * USDC, 25 * USDC)
    );
    let first_nullifier = transaction.first_nullifier;
    let instruction = transaction
        .prove(client.indexer(), env.localnet.tree)?
        .instruction()?;
    let signature = send(client.rpc(), &env.trader.keypair, instruction)?;
    client
        .confirm_private_transaction_sync(signature)
        .map_err(|e| anyhow!("confirm settle indexed: {e:?}"))?;
    report(&env, "settle", &signature)?;

    // 6. Both payouts are in the tree, at the commitments either party
    // derives from the position opening, the price and the first nullifier.
    let (trader_payout, maker_payout) = position.derived_payouts(&first_nullifier, price)?;
    assert_eq!(
        (trader_payout.amount, maker_payout.amount),
        (25 * USDC, 25 * USDC)
    );
    client
        .indexer()
        .get_merkle_proofs(
            env.localnet.tree,
            vec![
                trader_payout.hash(env.localnet.tree_id)?,
                maker_payout.hash(env.localnet.tree_id)?,
            ],
            None,
        )
        .map_err(|e| anyhow!("settle outputs index: {e}"))?;

    // Each wallet decrypts its own payout: the balances are the deposits,
    // less the margins, plus the payouts.
    assert_eq!(
        env.trader.synced_balance(&env.localnet.client, env.mint)?,
        55 * USDC
    );
    assert_eq!(
        env.maker.synced_balance(&env.localnet.client, env.mint)?,
        95 * USDC
    );

    // The position is spent: no second settle.
    let again = SettleTransactionParams {
        position_utxo: position,
        position_leaf_index: leaf_index,
        oracle: ORACLE,
        price,
        payer: trader_pubkey,
        tree_id: env.localnet.tree_id,
    }
    .build(&env.trader.keypair)?
    .prove(client.indexer(), env.localnet.tree);
    let spent = match again {
        Err(_) => true,
        Ok(settle) => send(client.rpc(), &env.trader.keypair, settle.instruction()?).is_err(),
    };
    assert!(spent, "a settled position must not settle twice");
    Ok(())
}

// A maker cannot settle a healthy position, and liquidates one once the
// price has moved against the trader.
//
// Flow:
//   1-4. As above: quote, open, the maker discovers the position.
//   5. With the oracle at the entry price the SDK refuses a maker settle.
//      The maker then proves a settle at $85, where the position is
//      liquidatable, and submits it while the oracle still reads $100: the
//      program hashes the oracle's price into the proof's public input, so
//      the proof does not verify.
//   6. The oracle falls to $85. The same instruction now settles: 5 to the
//      trader, 45 to the maker.
//
// Net: maker 100 -> 115, trader 50 -> 35.
#[test]
fn maker_liquidates_only_when_the_price_allows() -> Result<()> {
    let mut env = setup(2)?;
    let maker_pubkey = env.maker.keypair.shielded_address()?.solana_address()?;

    let quote = post_quote(&env, FAR_EXPIRY, true)?;
    let position = discover_and_open(&env, &quote)?;
    let leaf_index = maker_discovers(&env, &position)?;
    let client = &env.localnet.client;
    let settle = |price: u64| SettleTransactionParams {
        position_utxo: position.clone(),
        position_leaf_index: leaf_index,
        oracle: ORACLE,
        price,
        payer: maker_pubkey,
        tree_id: env.localnet.tree_id,
    };

    // 5. A healthy position: the SDK refuses, and so does the chain.
    set_oracle_price(&env, ENTRY_PRICE)?;
    assert!(!position.is_liquidatable(ENTRY_PRICE));
    let refused = settle(ENTRY_PRICE)
        .build(&env.maker.keypair)
        .err()
        .ok_or_else(|| anyhow!("the SDK built a maker settle of a healthy position"))?;
    assert!(refused.to_string().contains("liquidatable"));

    let liquidation_price = 85 * USD;
    assert!(position.is_liquidatable(liquidation_price));
    let transaction = settle(liquidation_price).build(&env.maker.keypair)?;
    assert_eq!(
        (transaction.trader_payout, transaction.maker_payout),
        (5 * USDC, 45 * USDC)
    );
    let first_nullifier = transaction.first_nullifier;
    let instruction = transaction
        .prove(client.indexer(), env.localnet.tree)?
        .instruction()?;
    let error = send(client.rpc(), &env.maker.keypair, instruction.clone())
        .expect_err("a liquidation at a price the oracle does not report must fail");
    assert_custom_error(
        &error,
        PerpError::ProofVerificationFailed as u32,
        "ProofVerificationFailed",
    );

    // A stale price is refused too, before the proof is looked at. The
    // account is rewritten with a publish time far in the past.
    set_stale_oracle_price(&env, liquidation_price)?;
    let error = send(client.rpc(), &env.maker.keypair, instruction.clone())
        .expect_err("a liquidation at a stale price must fail");
    assert_custom_error(
        &error,
        PerpError::StaleOraclePrice as u32,
        "StaleOraclePrice",
    );

    // 6. The oracle falls to $85 and the same instruction settles.
    set_oracle_price(&env, liquidation_price)?;
    let signature = send(client.rpc(), &env.maker.keypair, instruction)?;
    client
        .confirm_private_transaction_sync(signature)
        .map_err(|e| anyhow!("confirm liquidation indexed: {e:?}"))?;
    report(&env, "settle (liquidation)", &signature)?;

    let (trader_payout, maker_payout) =
        position.derived_payouts(&first_nullifier, liquidation_price)?;
    client
        .indexer()
        .get_merkle_proofs(
            env.localnet.tree,
            vec![
                trader_payout.hash(env.localnet.tree_id)?,
                maker_payout.hash(env.localnet.tree_id)?,
            ],
            None,
        )
        .map_err(|e| anyhow!("liquidation outputs index: {e}"))?;
    assert_eq!(
        env.trader.synced_balance(&env.localnet.client, env.mint)?,
        35 * USDC
    );
    assert_eq!(
        env.maker.synced_balance(&env.localnet.client, env.mint)?,
        115 * USDC
    );
    Ok(())
}

// A quote nobody accepted in time cannot be opened, and its maker reclaims
// the margin.
//
// Flow:
//   1. The maker posts an open quote whose expiry is already in the past.
//   2. The trader's open fails: the quote has expired.
//   3. The maker rediscovers its quote from the indexer and cancels it.
//   4. The refund is indexed and the maker's balance is whole again.
#[test]
fn expired_quote_cannot_open_and_is_cancelled() -> Result<()> {
    // The committed quote expiry is already in the past, so the maker can
    // cancel at once: the program requires `now > quote_expiry`.
    const PAST_EXPIRY: u64 = 1_000_000;
    let mut env = setup(3)?;
    let maker_address = env.maker.keypair.shielded_address()?;
    let maker_pubkey = maker_address.solana_address()?;
    let trader_pubkey = env.trader.keypair.shielded_address()?.solana_address()?;

    let posted = post_quote(&env, PAST_EXPIRY, false)?;
    let client = &env.localnet.client;

    // 2. The trader builds an open; the transact carries the quote's expiry,
    // which is past, so nothing accepts it.
    let quote_state = wait_for_merkle_proof(
        client.indexer(),
        env.localnet.tree,
        posted.hash(env.localnet.tree_id)?,
    );
    let trader_input = env
        .trader
        .balance(env.mint, Some(Filter::MinAmount(TRADER_MARGIN)))?
        .utxos
        .first()
        .cloned()
        .ok_or_else(|| anyhow!("no spendable note of {TRADER_MARGIN}"))?;
    let open = OpenTransactionParams {
        quote_utxo: posted.clone(),
        quote_leaf_index: quote_state.leaf_index,
        trader_input,
        payer: trader_pubkey,
        tree_id: env.localnet.tree_id,
    }
    .build(&env.trader.keypair)?
    .prove(client.indexer(), env.localnet.tree, &env.trader.keypair);
    match open {
        // The prover may already refuse a transaction past its deadline.
        Err(error) => println!("EXPIRED OPEN refused before sending: {error:#}"),
        Ok(open) => {
            let error = send(client.rpc(), &env.trader.keypair, open.instruction()?)
                .expect_err("an open of an expired quote must fail");
            assert_custom_error(&error, PerpError::QuoteExpired as u32, "QuoteExpired");
            println!("EXPIRED OPEN refused on chain: QuoteExpired");
        }
    }

    // 3. The maker rediscovers the quote and cancels it.
    let quote = index_maker_quotes(
        &env.maker.keypair,
        &env.maker.registry,
        client.indexer(),
        Duration::from_secs(60),
    )?
    .pop()
    .ok_or_else(|| anyhow!("no own quote discovered"))?;
    assert_eq!(quote.quote_utxo, posted);
    assert_eq!(
        quote.trader_viewing_pubkey,
        env.trader.keypair.viewing_pubkey()
    );
    assert_eq!(quote.leaf_index, quote_state.leaf_index);
    let transaction = CancelQuoteTransactionParams {
        quote_utxo: quote.quote_utxo.clone(),
        quote_leaf_index: quote.leaf_index,
        payer: maker_pubkey,
        tree_id: env.localnet.tree_id,
    }
    .build(&env.maker.keypair)?;
    let refund_hash = SppProofOutputUtxo {
        asset: quote.quote_utxo.collateral_mint,
        amount: quote.quote_utxo.maker_margin,
        blinding: transaction.margin_output_blinding,
        owner_address: Some(maker_address),
        ..Default::default()
    }
    .hash(env.localnet.tree_id)
    .map_err(|e| anyhow!("refund hash: {e:?}"))?;
    let instruction = transaction
        .prove(client.indexer(), env.localnet.tree)?
        .instruction()?;
    let signature = send(client.rpc(), &env.maker.keypair, instruction)?;
    client
        .confirm_private_transaction_sync(signature)
        .map_err(|e| anyhow!("confirm cancel indexed: {e:?}"))?;
    report(&env, "cancel_quote", &signature)?;

    // 4. The refund is indexed and the maker holds its whole deposit again.
    client
        .indexer()
        .get_merkle_proofs(env.localnet.tree, vec![refund_hash], None)
        .map_err(|e| anyhow!("cancel output index: {e}"))?;
    assert_eq!(
        env.maker.synced_balance(&env.localnet.client, env.mint)?,
        MAKER_SHIELD
    );
    Ok(())
}
