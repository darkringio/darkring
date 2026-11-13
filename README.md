<p align="center">
  <a href="https://darkring.io"><img src=".github/assets/banner.jpg" alt="Darkring: private perps on Solana" width="100%"></a>
</p>

<p align="center">
  Trade perps on Solana without anyone seeing your size, direction, entry or liquidation price.<br>
  On-chain it is two addresses and a proof.
</p>

<p align="center">
  <a href="https://app.darkring.io"><img alt="Launch the app" src="https://img.shields.io/badge/app-app.darkring.io-d8a645?style=flat-square&labelColor=0a1711"></a>
  <a href="https://darkring.io"><img alt="Website" src="https://img.shields.io/badge/website-darkring.io-d8a645?style=flat-square&labelColor=0a1711"></a>
  <a href="https://darkring.io/docs"><img alt="Docs" src="https://img.shields.io/badge/docs-darkring.io%2Fdocs-d8a645?style=flat-square&labelColor=0a1711"></a>
  <a href="https://x.com/darkringio"><img alt="Darkring on X" src="https://img.shields.io/badge/X-%40darkringio-d8a645?style=flat-square&labelColor=0a1711&logo=x&logoColor=e8e6d9"></a>
  <a href="https://github.com/darkringio"><img alt="Darkring on GitHub" src="https://img.shields.io/badge/GitHub-darkringio-d8a645?style=flat-square&labelColor=0a1711&logo=github&logoColor=e8e6d9"></a>
</p>

<p align="center">
  <img alt="Network: Solana devnet" src="https://img.shields.io/badge/network-Solana%20devnet-8fa89a?style=flat-square&labelColor=0a1711&logo=solana&logoColor=e8e6d9">
  <img alt="Status: not audited" src="https://img.shields.io/badge/status-not%20audited-8fa89a?style=flat-square&labelColor=0a1711">
  <img alt="Built on Solana Privacy Rings" src="https://img.shields.io/badge/built%20on-Solana%20Privacy%20Rings-8fa89a?style=flat-square&labelColor=0a1711">
  <img alt="Written in Rust" src="https://img.shields.io/badge/written%20in-Rust-8fa89a?style=flat-square&labelColor=0a1711&logo=rust&logoColor=e8e6d9">
</p>

# Darkring

Darkring is a perpetuals protocol where the position itself is private. You go long or short with leverage, like on any perps venue, and nobody watching the chain can tell what you hold. It is built on [Solana Privacy Rings](https://github.com/helius-labs/zolana), the shielded pool by Helius.

This repository is the protocol: the on-chain program, the SDK, the desk service and the tests. The website and the trading app live in [darkring-app](https://github.com/darkringio/darkring-app).

> [!WARNING]
> **Devnet only. Not audited.** The circuits use insecure test keys made from a fixed public seed, so anyone can forge the proofs, and the program builds only with the `insecure-test-setup` feature. Do not use it with real funds. A mainnet deployment needs its own trusted setup, verifying keys, program ID and audit.

## Contents

- [What the chain sees](#what-the-chain-sees)
- [How a position works](#how-a-position-works)
- [What is in this repository](#what-is-in-this-repository)
- [Live on devnet](#live-on-devnet)
- [Build and test](#build-and-test)
- [Run the desk](#run-the-desk)
- [Not built yet](#not-built-yet)
- [Links](#links)

## What the chain sees

| Public | Hidden |
| --- | --- |
| Both addresses | Size |
| When a quote is posted | Long or short |
| When a position opens | Entry price |
| Which token is the collateral | Both margins |
| At close: the market and the oracle price | Leverage |
| | Liquidation price |
| | PnL and payouts |

The public side is enough for anyone to check that every position is fully funded and every payout is right. Two things to know:

- The maker on the other side can see your position. It is their money against yours.
- Moving money in or out of a private balance is public, like any token transfer. What happens while it is inside is what is hidden.

<p align="center">
  <a href="https://app.darkring.io"><img src=".github/assets/desk.jpg" alt="The Darkring desk on devnet with two open positions" width="100%"></a>
  <br>
  <sub>The desk on devnet. Only the trader and the maker can read what is on this screen.</sub>
</p>

## How a position works

There is no order book and no shared pool. Every position is a deal between a trader and one maker, who puts up the money that pays the trader if they are right.

```mermaid
sequenceDiagram
    autonumber
    participant T as Trader
    participant M as Maker
    participant D as Darkring program
    participant R as Rings pool

    T-->>M: asks for a price
    M->>D: quote, with a proof
    D->>R: maker margin locked in a note only the trader can read
    T->>D: open, with a proof
    D->>R: both margins into one position note
    Note over T,M: The position is open. Neither side can take money out alone.
    alt the trader closes, at any time
        T->>D: settle at the Pyth price
    else the trader's margin is down to maintenance
        M->>D: settle, proving the liquidation
    end
    D->>R: each side is paid into its private balance
```

1. **Quote.** A maker locks its margin in a shielded note addressed to the trader, with the entry price and terms inside.
2. **Open.** The trader accepts. Both margins move into one position note that neither side can spend alone.
3. **Settle.** Either side closes at the Pyth price, and the program pays both private balances in the same transaction. The trader can close whenever they like. The maker can close only once the trader's margin has fallen to the maintenance level, and proves that without revealing where the liquidation price is.
4. **Cancel.** A quote nobody took goes back to its maker after it expires.

| | |
| --- | --- |
| Most you can lose | Your margin |
| Most you can win | The maker's margin |
| Liquidation | When your margin minus your loss equals the maintenance margin set in the quote |
| Fees | No trading fee and no funding rate in this build. Makers earn the spread. |

The [docs](https://darkring.io/docs) explain all of this for traders, with a worked example.

## What is in this repository

| Path | What it is |
| --- | --- |
| [`program/`](program) | The Solana program and its four circuits: quote, open, settle and cancel quote. It verifies its own proof and hands the shielded transfer to the Rings pool. It stores no state. |
| [`sdk/`](sdk) | Rust SDK. Builds and proves each transaction, and finds a trader's quotes and a maker's positions. |
| [`service/`](service) | The devnet desk: three makers quoting eleven markets, and the HTTP API behind app.darkring.io. Traders sign in their browser; the service never holds a key that can sign for them. |
| [`test/`](test) | End-to-end tests: `tests/perps.rs` against a local validator, `tests/devnet.rs` against devnet. |
| [`scripts/`](scripts) | Tool setup, the devnet deploy, devnet wallets and the desk launcher. Each script's header says what it needs. |
| [`deploy/`](deploy) | The systemd unit for the desk service. |
| [`docs/reference/`](docs/reference) | The design of the Rings swap example this program grew out of. |

## Live on devnet

| | Address |
| --- | --- |
| Darkring program | [`HY5QBm2yMqKDRnYSU11qi5ZeD3m2hL4WUUT2WLTzvWNF`](https://explorer.solana.com/address/HY5QBm2yMqKDRnYSU11qi5ZeD3m2hL4WUUT2WLTzvWNF?cluster=devnet) |
| Test dollar mint | [`3vAru2sJGnNQAbYLQK8eAJGkPsDirj3uMKR6s2qzjG8r`](https://explorer.solana.com/address/3vAru2sJGnNQAbYLQK8eAJGkPsDirj3uMKR6s2qzjG8r?cluster=devnet) |
| Rings shielded pool | [`ring1GaLCuJnkUMVkX2JeZLTTX17wKsV4fZmPVzXANW`](https://explorer.solana.com/address/ring1GaLCuJnkUMVkX2JeZLTTX17wKsV4fZmPVzXANW?cluster=devnet) |

**Markets.** SOL, BTC, ETH, HYPE, JUP, RAY, PYTH, WIF, BONK, FARTCOIN and PUMP, each against the dollar. A market needs nothing deployed, only a Pyth price feed, so the list is a table in [`service/src/chain.rs`](service/src/chain.rs).

**Prices.** Pyth. The devnet build accepts a price up to 10 minutes old, because devnet price accounts refresh slowly; the default build accepts 60 seconds. The desk pauses a market while its price is too old to settle at.

## Build and test

The Zolana SDK is pinned to a Git revision in [`Cargo.toml`](Cargo.toml). Two scripts fetch the matching CLI and local tools, and print the variables to export:

```sh
scripts/setup-zk-tools.sh      # the Zolana CLI that matches the pinned SDK
scripts/build-localnet.sh      # prover, indexer, local validator and pool program
```

With those set, run a position's whole life against a local validator:

```sh
cargo test --release -p perps-test --test perps
```

The devnet run spends devnet SOL and is ignored by default. It needs a devnet RPC, a Rings indexer URL and the wallets `scripts/devnet-setup.sh` makes. The header of [`test/tests/devnet.rs`](test/tests/devnet.rs) lists them.

```sh
cargo test --release -p perps-test --test devnet -- --ignored --nocapture
```

## Run the desk

```sh
scripts/run-desk.sh
```

This starts the desk service on `127.0.0.1:5374`. It needs a `.env` with `HELIUS_API_KEY` and the devnet wallets in `keys/`. Neither is in this repository.

A trader's wallet signs one message, and the browser derives a separate trading account from that signature. The service builds and proves each transaction with view keys it holds in memory for the session, then hands the transaction back to the browser to sign. So the service can see a connected account's balance and positions, and cannot move its funds.

## Not built yet

- **Proving in the browser.** Today the desk service builds the proofs, which is why it can see connected accounts' positions.
- **Deposits and withdrawals of your own tokens.** Accounts are funded with test dollars from a faucet.
- **Open makers.** The three makers are ours. Who provides liquidity on mainnet is an open question.
- **Funding.** There is no funding rate. Only the makers' pricing keeps quotes near spot.
- **A trusted setup and an audit.** See the warning at the top.

## Links

| | |
| --- | --- |
| Trading app | https://app.darkring.io |
| Website | https://darkring.io |
| Docs | https://darkring.io/docs |
| Risks | https://darkring.io/risks |
| X | [@darkringio](https://x.com/darkringio) |
| GitHub | [darkringio](https://github.com/darkringio) |
| Web app source | [darkring-app](https://github.com/darkringio/darkring-app) |

Built on [Solana Privacy Rings](https://github.com/helius-labs/zolana) by Helius, with prices from [Pyth](https://pyth.network). Darkring is an independent project and is not affiliated with or endorsed by Helius, the Solana Foundation or Pyth.
