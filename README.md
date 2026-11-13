# Darkring

Private perpetuals on Solana, built on [Solana Privacy Rings](https://github.com/helius-labs/zolana) by Helius.

You go long or short with leverage. Your size, direction, entry price, margin, leverage, liquidation price and PnL stay hidden. The chain sees two addresses and a proof that the numbers add up.

This repository is the protocol: the on-chain program, the SDK, the desk service and the tests. The website and the trading app are in [darkring-app](https://github.com/darkringio/darkring-app).

- Trade on devnet: https://app.darkring.io
- How it works, in plain words: https://darkring.io/docs

> [!WARNING]
> **Devnet only. Not audited.** The circuits use insecure test keys made from a fixed public seed, so anyone can forge the proofs, and the program builds only with the `insecure-test-setup` feature. Do not use it with real funds. A mainnet deployment needs its own trusted setup, verifying keys, program ID and audit.

## How a position works

There is no order book and no shared pool. Every position is a deal between a trader and one maker.

1. **Quote.** A maker locks its margin in a shielded note addressed to the trader, with the entry price and terms inside. Only the trader can read it.
2. **Open.** The trader accepts, and both margins move into one position note that neither side can spend alone.
3. **Settle.** Either side closes at the Pyth price and the program pays both private balances in the same transaction. The trader can close at any time. The maker can close only once the trader's margin has fallen to the maintenance level, and proves that without revealing the liquidation price.
4. **Cancel.** A quote nobody took goes back to its maker after it expires.

The trader cannot lose more than their margin, and cannot win more than the maker's.

## What is here

| Path | What it is |
| --- | --- |
| `program/` | The Solana program and its four circuits (quote, open, settle, cancel quote). It verifies its own proof and hands the shielded transfer to the Rings pool. It stores no state. |
| `sdk/` | Rust SDK: builds and proves each transaction, and finds a trader's quotes and a maker's positions. |
| `service/` | The devnet desk: three makers that quote eleven markets, and the HTTP API behind app.darkring.io. Traders' transactions are signed in their browser; the service never holds a key that can sign for them. |
| `test/` | End-to-end tests: `tests/perps.rs` against a local validator, `tests/devnet.rs` against devnet. |
| `scripts/` | Tool setup, the devnet deploy, devnet wallets and the desk launcher. Each script's header says what it needs. |
| `deploy/` | The systemd unit for the desk service. |
| `docs/reference/` | The design of the Rings swap example this program grew out of. |

## On devnet

| | Address |
| --- | --- |
| Darkring program | `HY5QBm2yMqKDRnYSU11qi5ZeD3m2hL4WUUT2WLTzvWNF` |
| Test dollar mint | `3vAru2sJGnNQAbYLQK8eAJGkPsDirj3uMKR6s2qzjG8r` |
| Rings shielded pool | `ring1GaLCuJnkUMVkX2JeZLTTX17wKsV4fZmPVzXANW` |

Prices come from Pyth. The devnet build accepts a price up to 10 minutes old, because devnet price accounts refresh slowly; the default build accepts 60 seconds.

## Build and test

The Zolana SDK is pinned to a Git revision in `Cargo.toml`. The matching CLI and local tools are fetched by two scripts:

```sh
scripts/setup-zk-tools.sh      # the Zolana CLI that matches the pinned SDK
scripts/build-localnet.sh      # prover, indexer, local validator and pool program
```

Both print the variables to export. With those set:

```sh
cargo test --release -p perps-test --test perps
```

The devnet run spends devnet SOL and is ignored by default. It needs a devnet RPC, a Rings indexer URL and the wallets `scripts/devnet-setup.sh` makes; the header of `test/tests/devnet.rs` lists them.

```sh
cargo test --release -p perps-test --test devnet -- --ignored --nocapture
```

## Run the desk

`scripts/run-desk.sh` starts the service on `127.0.0.1:5374`. It needs a `.env` with `HELIUS_API_KEY` and the devnet wallets in `keys/`. Neither is in this repository.

## Contact

[@darkringio](https://x.com/darkringio) on X.
