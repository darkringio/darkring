//! Moving the test dollar between a wallet and an account's private balance.
//!
//! A deposit takes tokens from the person's wallet into the Rings pool, as
//! a note of their trading account's; a withdrawal spends the account's
//! notes and pays tokens out to the wallet. The browser builds both whole:
//! it reads the notes, has the pool proof made, and signs, with the
//! account's key and, for a deposit, the wallet's. This process only sends
//! what it is handed, as it sends an open or a close.
//!
//! It still looks at what it sends. The route is for moving an account's
//! own money, not a way to have the desk's RPC key send anything at all, so
//! a transaction goes on only if the session's account pays its fee and it
//! calls nothing but the Rings pool, the program that makes the wallet's
//! token account, and the compute budget.

use std::{thread::sleep, time::Duration};

use anyhow::{anyhow, bail, ensure, Result};
use serde_json::{json, Value};
use solana_address::Address;
use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;
use spr_interface::instruction::{
    instruction_data::transact::{InterfaceTransfer, TransactIxData},
    tag,
};

/// The Rings pool, the associated token account program, and the compute
/// budget: everything a deposit or a withdrawal calls.
const ALLOWED: [Address; 3] = [
    Address::from_str_const("ring1GaLCuJnkUMVkX2JeZLTTX17wKsV4fZmPVzXANW"),
    Address::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"),
    Address::from_str_const("ComputeBudget111111111111111111111111111111"),
];

/// The largest transaction Solana carries (the version 1 format's limit).
pub const MAX_WIRE: usize = 4096;
/// How long to wait for a sent transaction to confirm. A blockhash lasts
/// about a minute, so past this it will not land.
const CONFIRM_FOR: Duration = Duration::from_secs(75);
const CONFIRM_EVERY: Duration = Duration::from_millis(500);

/// Which way the money moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Deposit,
    Withdraw,
}

impl Kind {
    pub fn named(name: &str) -> Option<Self> {
        match name {
            "deposit" => Some(Self::Deposit),
            "withdraw" => Some(Self::Withdraw),
            _ => None,
        }
    }

    /// The name the web app and the ledger know it by.
    pub fn name(self) -> &'static str {
        match self {
            Self::Deposit => "deposit",
            Self::Withdraw => "withdraw",
        }
    }
}

/// The transaction in `wire`, if it is one this route sends: `account`
/// pays its fee, and it calls only the allowed programs.
pub fn checked(wire: &[u8], account: &Address) -> Result<VersionedTransaction> {
    ensure!(wire.len() <= MAX_WIRE, "the transaction is too large");
    let transaction: VersionedTransaction =
        wincode::deserialize(wire).map_err(|_| anyhow!("that is not a Solana transaction"))?;
    let message = &transaction.message;
    let keys = message.static_account_keys();
    ensure!(
        keys.first() == Some(account),
        "the transaction's fee is not paid by your trading account"
    );
    // Addresses from a lookup table are not in the transaction to be seen.
    ensure!(
        message
            .address_table_lookups()
            .is_none_or(|lookups| lookups.is_empty()),
        "the transaction reads addresses from a lookup table"
    );
    let instructions = message.instructions();
    ensure!(!instructions.is_empty(), "the transaction does nothing");
    for instruction in instructions {
        let program = keys
            .get(usize::from(instruction.program_id_index))
            .ok_or_else(|| anyhow!("the transaction names a program it does not list"))?;
        ensure!(
            ALLOWED.contains(program),
            "the desk only sends deposits to and withdrawals from the Rings pool"
        );
    }
    ensure!(
        transaction.signatures.len() == usize::from(message.header().num_required_signatures),
        "the transaction is not fully signed"
    );
    Ok(transaction)
}

/// A transaction whose fee the desk's relayer pays, signed by the relayer
/// and ready to send: a deposit from a wallet to a one-time address, or a
/// withdrawal from a one-time address to a wallet. Those are the two ends
/// of a private deposit and a private withdrawal: the one-time address has
/// no SOL of its own, and the trading account must not be seen paying for
/// it.
///
/// The relayer's signature is on the whole transaction, so everything in it
/// that could spend the relayer's own funds is refused. Beyond what
/// [`checked`] asks, the relayer may be named only where a fee payer is: as
/// the first account of a pool `transact` or of the instruction that makes
/// a token account. It may not be the depositor of a deposit, and a
/// `transact` it pays for may only withdraw: a deposit inside one would be
/// paid from an account that signed, and the relayer signed.
pub fn relayed(wire: &[u8], relayer: &Keypair) -> Result<Vec<u8>> {
    let mut transaction = checked(wire, &relayer.pubkey())
        .map_err(|error| anyhow!("{error}").context("a relayed transaction"))?;
    let message = &transaction.message;
    let keys = message.static_account_keys();
    for instruction in message.instructions() {
        let program = keys[usize::from(instruction.program_id_index)];
        // Where the relayer, account 0 of the transaction, is named.
        let named: Vec<usize> = instruction
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, account)| **account == 0)
            .map(|(position, _)| position)
            .collect();
        let as_payer_only = named.iter().all(|position| *position == 0);
        if program == ALLOWED[0] {
            match instruction.data.split_first() {
                Some((&tag::DEPOSIT, _)) => ensure!(
                    named.is_empty(),
                    "the relayer cannot be the one who deposits"
                ),
                Some((&tag::TRANSACT, data)) => {
                    ensure!(as_payer_only, "the relayer only pays the fee");
                    let transact = wincode::deserialize_exact::<TransactIxData>(data)
                        .map_err(|_| anyhow!("that is not the pool's transact data"))?;
                    ensure!(
                        transact.interface_transfers.iter().all(|transfer| matches!(
                            transfer,
                            InterfaceTransfer::SplWithdrawal { .. }
                        )),
                        "a relayed transaction may only withdraw tokens"
                    );
                }
                _ => bail!("the relayer only pays for deposits and withdrawals"),
            }
        } else if program == ALLOWED[1] {
            ensure!(as_payer_only, "the relayer only pays the fee");
        } else {
            ensure!(named.is_empty(), "the relayer only pays the fee");
        }
    }
    let signature = relayer.sign_message(&transaction.message.serialize());
    *transaction
        .signatures
        .first_mut()
        .ok_or_else(|| anyhow!("the transaction has no signature slots"))? = signature;
    wincode::serialize(&transaction).map_err(|_| anyhow!("the transaction does not serialize"))
}

/// `bytes` in base64, the encoding a Solana node takes a transaction in.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut text = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let group = u32::from(chunk[0]) << 16
            | u32::from(*chunk.get(1).unwrap_or(&0)) << 8
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for index in 0..4 {
            if index <= chunk.len() {
                let six = (group >> (18 - 6 * index)) & 63;
                text.push(char::from(ALPHABET[six as usize]));
            } else {
                text.push('=');
            }
        }
    }
    text
}

/// One JSON-RPC call to the node at `rpc_url`. An error from the HTTP
/// client would name the URL, and the URL holds the provider's key, so
/// none is passed on.
fn call(
    http: &reqwest::blocking::Client,
    rpc_url: &str,
    method: &str,
    params: Value,
) -> Result<Value> {
    let answer: Value = http
        .post(rpc_url)
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))
        .send()
        .map_err(|_| anyhow!("the Solana node did not answer"))?
        .json()
        .map_err(|_| anyhow!("the Solana node sent something that is not JSON"))?;
    if let Some(error) = answer.get("error") {
        // The last log line says what the program objected to.
        let log = error["data"]["logs"]
            .as_array()
            .and_then(|logs| logs.iter().rev().find_map(Value::as_str))
            .map(|line| format!(" ({line})"))
            .unwrap_or_default();
        bail!(
            "Solana refused the transaction: {}{log}",
            error["message"].as_str().unwrap_or("no reason given")
        );
    }
    Ok(answer["result"].clone())
}

/// Sends the signed transaction in `wire` through the node at `rpc_url`
/// and waits until it is confirmed. Returns its signature.
pub fn broadcast(rpc_url: &str, wire: &[u8]) -> Result<String> {
    let http = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;
    let signature = call(
        &http,
        rpc_url,
        "sendTransaction",
        json!([base64(wire), { "encoding": "base64", "preflightCommitment": "confirmed" }]),
    )?
    .as_str()
    .ok_or_else(|| anyhow!("the Solana node gave no signature"))?
    .to_owned();
    let started = std::time::Instant::now();
    while started.elapsed() < CONFIRM_FOR {
        sleep(CONFIRM_EVERY);
        let statuses = call(&http, rpc_url, "getSignatureStatuses", json!([[signature]]))?;
        let status = &statuses["value"][0];
        if !status["err"].is_null() {
            bail!("the transaction failed on chain: {}", status["err"]);
        }
        if matches!(
            status["confirmationStatus"].as_str(),
            Some("confirmed" | "finalized")
        ) {
            return Ok(signature);
        }
    }
    bail!("the transaction was sent but did not confirm in time; check your balance before trying again")
}

#[cfg(test)]
mod tests {
    use solana_message::{
        compiled_instruction::CompiledInstruction, legacy, MessageHeader, VersionedMessage,
    };
    use solana_signature::Signature;

    use super::*;

    const ACCOUNT: Address =
        Address::from_str_const("FtN8MXujMbeaygN4hbYrwQWbVm7EREWLCTAABoG1fZFK");
    const OTHER: Address = Address::from_str_const("7qzWDro62NMvyMxepZy3w7zKVCi2eTfQiobzaNNTstaA");
    const SYSTEM: Address = Address::from_str_const("11111111111111111111111111111111");

    /// A legacy transaction paid by `payer` that calls `programs` in turn.
    fn wire(payer: Address, programs: &[Address], signatures: usize) -> Vec<u8> {
        let mut account_keys = vec![payer];
        account_keys.extend_from_slice(programs);
        let message = legacy::Message {
            header: MessageHeader {
                num_required_signatures: 1,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: programs.len() as u8,
            },
            account_keys,
            recent_blockhash: Default::default(),
            instructions: (0..programs.len())
                .map(|index| CompiledInstruction {
                    program_id_index: index as u8 + 1,
                    accounts: vec![0],
                    data: vec![1, 2, 3],
                })
                .collect(),
        };
        let transaction = VersionedTransaction {
            signatures: vec![Signature::default(); signatures],
            message: VersionedMessage::Legacy(message),
        };
        wincode::serialize(&transaction).expect("a transaction serializes")
    }

    #[test]
    fn a_pool_transaction_the_account_pays_for_goes_on() {
        assert!(checked(&wire(ACCOUNT, &[ALLOWED[1], ALLOWED[0]], 1), &ACCOUNT).is_ok());
    }

    #[test]
    fn another_payer_is_turned_away() {
        let error = checked(&wire(OTHER, &[ALLOWED[0]], 1), &ACCOUNT).unwrap_err();
        assert!(error
            .to_string()
            .contains("not paid by your trading account"));
    }

    #[test]
    fn another_program_is_turned_away() {
        let error = checked(&wire(ACCOUNT, &[ALLOWED[0], SYSTEM], 1), &ACCOUNT).unwrap_err();
        assert!(error.to_string().contains("only sends deposits"));
    }

    #[test]
    fn a_missing_signature_and_an_empty_transaction_are_turned_away() {
        assert!(checked(&wire(ACCOUNT, &[ALLOWED[0]], 0), &ACCOUNT).is_err());
        assert!(checked(&wire(ACCOUNT, &[], 1), &ACCOUNT).is_err());
        assert!(checked(b"not a transaction", &ACCOUNT).is_err());
        assert!(checked(&vec![0; MAX_WIRE + 1], &ACCOUNT).is_err());
    }

    /// A legacy transaction paid by `payer`, with one instruction of
    /// `program` over the accounts at `accounts` (indexes into `keys`, where
    /// the payer is 0) carrying `data`.
    fn relay_wire(payer: &Keypair, program: Address, accounts: Vec<u8>, data: Vec<u8>) -> Vec<u8> {
        let message = legacy::Message {
            header: MessageHeader {
                num_required_signatures: 1,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 1,
            },
            // The payer, another account, the program.
            account_keys: vec![payer.pubkey(), OTHER, program],
            recent_blockhash: Default::default(),
            instructions: vec![CompiledInstruction {
                program_id_index: 2,
                accounts,
                data,
            }],
        };
        let transaction = VersionedTransaction {
            signatures: vec![Signature::default()],
            message: VersionedMessage::Legacy(message),
        };
        wincode::serialize(&transaction).expect("a transaction serializes")
    }

    /// The pool's transact data with these public transfers.
    fn transact(transfers: Vec<InterfaceTransfer>) -> Vec<u8> {
        use spr_interface::{
            instruction::instruction_data::transact::{CircuitId, TransactProof},
            N_PUBLIC_SLOTS,
        };
        let data = TransactIxData {
            expiry_unix_ts: u64::MAX,
            private_tx_hash: [1; 32],
            circuit: CircuitId::ConfidentialSigner(1, 2, N_PUBLIC_SLOTS as u8),
            tx_viewing_pk: [2; 33],
            salt: [3; 16],
            proof: TransactProof::zeroed(),
            inputs: Vec::new(),
            interface_transfers: transfers,
            data_hash: None,
            ring_data_hash: None,
            outputs: Vec::new(),
            messages: Vec::new(),
            tree_contexts: Vec::new(),
        };
        let mut bytes = vec![tag::TRANSACT];
        bytes.extend_from_slice(&wincode::serialize(&data).expect("transact data serializes"));
        bytes
    }

    #[test]
    fn the_relayer_signs_a_withdrawal_it_only_pays_for() {
        let relayer = Keypair::new();
        let withdrawal = transact(vec![InterfaceTransfer::SplWithdrawal {
            amount: 5,
            spl_interface_bump: 255,
        }]);
        // Named first among the pool's accounts, as the fee payer is.
        let wire = relay_wire(&relayer, ALLOWED[0], vec![0, 1], withdrawal);
        let signed = relayed(&wire, &relayer).expect("a withdrawal is relayed");
        let transaction: VersionedTransaction = wincode::deserialize(&signed).unwrap();
        assert!(transaction.signatures.first().unwrap().verify(
            relayer.pubkey().as_array(),
            &transaction.message.serialize()
        ));
        // A transfer with no public leg at all is fine too.
        assert!(relayed(
            &relay_wire(&relayer, ALLOWED[0], vec![0, 1], transact(Vec::new())),
            &relayer
        )
        .is_ok());
    }

    #[test]
    fn the_relayer_signs_a_deposit_somebody_else_makes() {
        let relayer = Keypair::new();
        let wire = relay_wire(&relayer, ALLOWED[0], vec![1], vec![tag::DEPOSIT, 9, 9]);
        assert!(relayed(&wire, &relayer).is_ok());
    }

    #[test]
    fn nothing_that_spends_the_relayers_own_money_is_signed() {
        let relayer = Keypair::new();
        let refused = |program, accounts, data: Vec<u8>| {
            relayed(&relay_wire(&relayer, program, accounts, data), &relayer).is_err()
        };
        // The relayer as the depositor of a deposit.
        assert!(refused(ALLOWED[0], vec![1, 0], vec![tag::DEPOSIT, 9, 9]));
        // A deposit inside a transact the relayer pays for, of SOL or of
        // tokens: it would be taken from an account that signed.
        for transfer in [
            InterfaceTransfer::SolDeposit { amount: 5 },
            InterfaceTransfer::SplDeposit {
                amount: 5,
                spl_interface_bump: 255,
            },
            InterfaceTransfer::SolWithdrawal { amount: 5 },
        ] {
            assert!(refused(ALLOWED[0], vec![0, 1], transact(vec![transfer])));
        }
        // The relayer named anywhere but first in a transact.
        assert!(refused(ALLOWED[0], vec![1, 0], transact(Vec::new())));
        // Any other instruction of the pool, and data that is not transact
        // data.
        assert!(refused(ALLOWED[0], vec![0], vec![0xee]));
        assert!(refused(ALLOWED[0], vec![0], vec![tag::TRANSACT, 1, 2, 3]));
        assert!(refused(ALLOWED[0], vec![0], Vec::new()));
        // Named by the compute budget, or second by the token account
        // program.
        assert!(refused(ALLOWED[2], vec![0], vec![2]));
        assert!(refused(ALLOWED[1], vec![1, 0], vec![1]));
        // Another program, or another fee payer.
        assert!(refused(SYSTEM, vec![1], vec![2]));
        let other = Keypair::new();
        assert!(relayed(
            &relay_wire(&other, ALLOWED[0], vec![1], vec![tag::DEPOSIT]),
            &relayer
        )
        .is_err());
    }

    #[test]
    fn base64_pads_as_the_node_expects() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0xfb, 0xff]), "+/8=");
    }
}
