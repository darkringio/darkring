#![allow(dead_code)]

use mollusk_svm::Mollusk;
use perps_program::{
    instructions::{
        cancel_quote::CancelQuoteIxData,
        open::OpenIxData,
        oracle::{encode_price_update, ORACLE_PROGRAM_ID},
        quote::{QuoteIxData, QuoteMarkerData},
        settle::SettleIxData,
    },
    maker_authority_seeds, tag,
};
use solana_account::Account;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;
use spr_interface::{
    instruction::instruction_data::transact::{
        CircuitId, InputUtxo, MessageData, OwnerTag, TransactIxData, TransactOutput, TransactProof,
        TreeContext,
    },
    N_PUBLIC_SLOTS, SHIELDED_POOL_PROGRAM_ID,
};
use spr_program::CompressedProof;

/// The maker the fixture's authority is derived from: the cancel's maker
/// signer and the `maker` argument of open and settle.
pub const MAKER: [u8; 32] = [22; 32];

/// The fee payer, and quote's maker signer.
pub const PAYER: [u8; 32] = [21; 32];

/// The settling signer of the fixture settle.
pub const SETTLER: [u8; 32] = [24; 32];

/// The oracle price account of the fixture settle.
pub const ORACLE: [u8; 32] = [25; 32];

/// The feed of the fixture's oracle account.
pub const FEED_ID: [u8; 32] = [26; 32];

/// The hash of the fixture transact's quote output (output 1).
pub const QUOTE_UTXO_HASH: [u8; 32] = [6; 32];

const SBF_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/deploy");

#[derive(Clone, Copy, Debug)]
pub enum Wrapper {
    Quote,
    Open,
    Settle,
    CancelQuote,
    OptionQuote,
    OptionOpen,
    OptionSettle,
    OptionCancel,
}

impl Wrapper {
    pub const ALL: [Self; 8] = [
        Self::Quote,
        Self::Open,
        Self::Settle,
        Self::CancelQuote,
        Self::OptionQuote,
        Self::OptionOpen,
        Self::OptionSettle,
        Self::OptionCancel,
    ];

    /// The option instructions, each of which takes the accounts and the
    /// data of its perp counterpart ([`Self::base`]).
    pub const OPTIONS: [Self; 4] = [
        Self::OptionQuote,
        Self::OptionOpen,
        Self::OptionSettle,
        Self::OptionCancel,
    ];

    pub fn tag(self) -> u8 {
        match self {
            Self::Quote => tag::QUOTE,
            Self::Open => tag::OPEN,
            Self::Settle => tag::SETTLE,
            Self::CancelQuote => tag::CANCEL_QUOTE,
            Self::OptionQuote => tag::OPTION_QUOTE,
            Self::OptionOpen => tag::OPTION_OPEN,
            Self::OptionSettle => tag::OPTION_SETTLE,
            Self::OptionCancel => tag::OPTION_CANCEL,
        }
    }

    /// The perp instruction whose accounts and data this one takes.
    pub fn base(self) -> Self {
        match self {
            Self::Quote | Self::OptionQuote => Self::Quote,
            Self::Open | Self::OptionOpen => Self::Open,
            Self::Settle | Self::OptionSettle => Self::Settle,
            Self::CancelQuote | Self::OptionCancel => Self::CancelQuote,
        }
    }

    /// The maker whose authority PDA the instruction signs for: quote's
    /// payer, the `maker` argument of open and settle, cancel's maker signer.
    fn authority_maker(self) -> [u8; 32] {
        match self.base() {
            Self::Quote => PAYER,
            _ => MAKER,
        }
    }

    fn authority_address(self, bump: &[u8; 1]) -> Option<Pubkey> {
        let maker = self.authority_maker();
        let [seed, maker_seed] = maker_authority_seeds(&maker);
        Pubkey::create_program_address(&[seed, maker_seed, bump], &program_id()).ok()
    }

    /// The canonical bump of the authority PDA, which the SDK passes.
    pub fn canonical_bump(self) -> u8 {
        Pubkey::find_program_address(
            &maker_authority_seeds(&self.authority_maker()),
            &program_id(),
        )
        .1
    }

    /// The highest bump that derives no program address: an on-curve point.
    pub fn invalid_bump(self) -> u8 {
        (0..=u8::MAX)
            .rev()
            .find(|bump| self.authority_address(&[*bump]).is_none())
            .expect("some bump derives an on-curve point")
    }
}

fn program_id() -> Pubkey {
    Pubkey::new_from_array(*perps_program::ID.as_array())
}

pub fn setup_mollusk() -> (Mollusk, Pubkey) {
    spr_test_utils::mollusk::mollusk_with_program(
        SBF_DIR,
        *perps_program::ID.as_array(),
        "perps_program",
    )
}

pub fn account(lamports: u64) -> Account {
    Account {
        lamports,
        data: Vec::new(),
        owner: Pubkey::new_from_array([0; 32]),
        executable: false,
        rent_epoch: 0,
    }
}

/// An oracle price account holding `data`, owned by the oracle program.
pub fn oracle_account(data: Vec<u8>) -> Account {
    Account {
        lamports: 1_000_000_000,
        data,
        owner: Pubkey::new_from_array(*ORACLE_PROGRAM_ID.as_array()),
        executable: false,
        rent_epoch: 0,
    }
}

/// A fully verified price update of the fixture feed.
pub fn price_update(price: i64, publish_time: i64) -> Vec<u8> {
    encode_price_update(&FEED_ID, price, publish_time).to_vec()
}

pub fn transact(messages: Vec<MessageData>) -> TransactIxData {
    TransactIxData {
        expiry_unix_ts: u64::MAX,
        private_tx_hash: [1; 32],
        circuit: CircuitId::ConfidentialSigner(1, 2, N_PUBLIC_SLOTS as u8),
        tx_viewing_pk: [2; 33],
        salt: [3; 16],
        proof: TransactProof::zeroed(),
        inputs: Vec::new(),
        interface_transfers: Vec::new(),
        data_hash: None,
        ring_data_hash: None,
        outputs: vec![
            TransactOutput {
                utxo_hash: [4; 32],
                owner_tag: OwnerTag::Inline([5; 32]),
                data: None,
            },
            TransactOutput {
                utxo_hash: QUOTE_UTXO_HASH,
                owner_tag: OwnerTag::Inline([7; 32]),
                data: Some(vec![8; 16]),
            },
        ],
        messages,
        tree_contexts: vec![TreeContext {
            utxo_tree_root_index: 0,
            nullifier_tree_root_index: 0,
        }],
    }
}

/// [`transact`] with one input, whose nullifier the open and settle proofs
/// take as a public input.
pub fn transact_with_input() -> TransactIxData {
    let mut data = transact(Vec::new());
    data.inputs.push(InputUtxo {
        nullifier_hash: [1; 32],
        tree_index: 0,
    });
    data
}

/// A marker message whose data is the [`QuoteMarkerData`] of
/// `quote_utxo_hash` and `maker_pubkey`.
pub fn marker(quote_utxo_hash: [u8; 32], maker_pubkey: [u8; 32]) -> MessageData {
    MessageData {
        view_tag: [9; 32],
        data: borsh::to_vec(&QuoteMarkerData {
            quote_utxo_hash,
            maker_pubkey,
        })
        .expect("serialize marker"),
    }
}

/// The marker the program expects of the fixture quote.
pub fn valid_marker() -> MessageData {
    marker(QUOTE_UTXO_HASH, PAYER)
}

/// The fixture's transact data: a quote carries the valid marker.
pub fn fixture_transact(wrapper: Wrapper) -> TransactIxData {
    transact(if matches!(wrapper.base(), Wrapper::Quote) {
        vec![valid_marker()]
    } else {
        Vec::new()
    })
}

pub fn wrapper_data(wrapper: Wrapper) -> Vec<u8> {
    wrapper_data_with(wrapper, fixture_transact(wrapper))
}

/// Serialize a wrapper body around a caller-customized transact payload, so
/// negatives can tamper with individual transact fields while staying
/// wire-valid.
pub fn wrapper_data_with(wrapper: Wrapper, transact: TransactIxData) -> Vec<u8> {
    wrapper_data_with_bump(wrapper, transact, wrapper.canonical_bump())
}

/// [`wrapper_data_with`] with `authority_bump` in place of the canonical bump.
pub fn wrapper_data_with_bump(
    wrapper: Wrapper,
    transact: TransactIxData,
    authority_bump: u8,
) -> Vec<u8> {
    let proof = CompressedProof {
        a: [10; 32],
        b: [11; 64],
        c: [12; 32],
    };
    let body = match wrapper.base() {
        Wrapper::Quote => wincode::serialize(&QuoteIxData {
            proof,
            viewing_pubkey: [13; 33],
            authority_bump,
            transact,
        }),
        Wrapper::Open => wincode::serialize(&OpenIxData {
            proof,
            maker: MAKER,
            viewing_pubkey: [13; 33],
            authority_bump,
            transact,
        }),
        Wrapper::Settle => wincode::serialize(&SettleIxData {
            proof,
            maker: MAKER,
            authority_bump,
            transact,
        }),
        Wrapper::CancelQuote => wincode::serialize(&CancelQuoteIxData {
            proof,
            quote_expiry: 0,
            authority_bump,
            transact,
        }),
        _ => unreachable!("base is a perp instruction"),
    }
    .expect("serialize wrapper");
    let mut data = vec![wrapper.tag()];
    data.extend_from_slice(&body);
    data
}

/// The fixture instruction of `wrapper` and its accounts. A settle reads a
/// fresh price of 100 from [`ORACLE`].
pub fn fixture(wrapper: Wrapper) -> (Instruction, Vec<(Pubkey, Account)>) {
    let payer = Pubkey::new_from_array(PAYER);
    let maker = Pubkey::new_from_array(MAKER);
    let tree = Pubkey::new_from_array([23; 32]);
    let spp_id = Pubkey::new_from_array(SHIELDED_POOL_PROGRAM_ID);
    let (authority, _) =
        Pubkey::find_program_address(&maker_authority_seeds(&MAKER), &program_id());

    let mut metas = vec![AccountMeta::new(payer, true)];
    let mut accounts = vec![(payer, account(1_000_000_000))];
    match wrapper.base() {
        Wrapper::CancelQuote => {
            metas.push(AccountMeta::new_readonly(maker, true));
            accounts.push((maker, account(1_000_000_000)));
        }
        Wrapper::Settle => {
            let settler = Pubkey::new_from_array(SETTLER);
            let oracle = Pubkey::new_from_array(ORACLE);
            metas.push(AccountMeta::new_readonly(settler, true));
            accounts.push((settler, account(1_000_000_000)));
            metas.push(AccountMeta::new_readonly(oracle, false));
            accounts.push((oracle, oracle_account(price_update(100, 0))));
        }
        _ => {}
    }
    metas.push(AccountMeta::new(payer, true));
    accounts.push((payer, account(1_000_000_000)));
    metas.push(AccountMeta::new(tree, false));
    accounts.push((tree, account(1_000_000_000)));
    if !matches!(wrapper.base(), Wrapper::Quote) {
        metas.push(AccountMeta::new_readonly(authority, false));
        accounts.push((authority, account(1_000_000_000)));
    }
    metas.push(AccountMeta::new_readonly(spp_id, false));
    accounts.push((
        spp_id,
        mollusk_svm::program::create_program_account_loader_v3(&spp_id),
    ));

    (
        Instruction {
            program_id: program_id(),
            accounts: metas,
            data: wrapper_data(wrapper),
        },
        accounts,
    )
}

/// Replaces the data of the fixture's oracle account.
pub fn set_oracle(accounts: &mut [(Pubkey, Account)], oracle: Account) {
    let address = Pubkey::new_from_array(ORACLE);
    for (key, account) in accounts.iter_mut() {
        if *key == address {
            *account = oracle.clone();
        }
    }
}

/// The veil instructions' fixtures: `veil_in` and `veil_out` around the
/// fixture transact, with the veil PDA among the accounts.
pub mod veil {
    use perps_program::{
        instructions::veil::{VeilInIxData, VeilOutIxData},
        VEIL_PDA_SEED,
    };

    use super::*;

    pub fn canonical_bump() -> u8 {
        Pubkey::find_program_address(&[VEIL_PDA_SEED], &program_id()).1
    }

    /// The highest bump that derives no program address.
    pub fn invalid_bump() -> u8 {
        (0..=u8::MAX)
            .rev()
            .find(|bump| {
                Pubkey::create_program_address(&[VEIL_PDA_SEED, &[*bump]], &program_id()).is_err()
            })
            .expect("some bump derives an on-curve point")
    }

    fn proof() -> CompressedProof {
        CompressedProof {
            a: [10; 32],
            b: [11; 64],
            c: [12; 32],
        }
    }

    pub fn in_data(transact: TransactIxData, authority_bump: u8) -> Vec<u8> {
        let mut data = vec![tag::VEIL_IN];
        data.extend_from_slice(
            &wincode::serialize(&VeilInIxData {
                proof: proof(),
                viewing_pubkey: [13; 33],
                authority_bump,
                transact,
            })
            .expect("serialize veil_in"),
        );
        data
    }

    pub fn out_data(transact: TransactIxData, authority_bump: u8) -> Vec<u8> {
        let mut data = vec![tag::VEIL_OUT];
        data.extend_from_slice(
            &wincode::serialize(&VeilOutIxData {
                proof: proof(),
                authority_bump,
                transact,
            })
            .expect("serialize veil_out"),
        );
        data
    }

    /// An instruction with `data` and the accounts both veil instructions
    /// take: the payer, then the SPP accounts with the veil PDA among them.
    pub fn fixture(data: Vec<u8>) -> (Instruction, Vec<(Pubkey, Account)>) {
        let payer = Pubkey::new_from_array(PAYER);
        let tree = Pubkey::new_from_array([23; 32]);
        let spp_id = Pubkey::new_from_array(SHIELDED_POOL_PROGRAM_ID);
        let (authority, _) = Pubkey::find_program_address(&[VEIL_PDA_SEED], &program_id());
        let metas = vec![
            AccountMeta::new(payer, true),
            AccountMeta::new(payer, true),
            AccountMeta::new(tree, false),
            AccountMeta::new_readonly(authority, false),
            AccountMeta::new_readonly(spp_id, false),
        ];
        let accounts = vec![
            (payer, account(1_000_000_000)),
            (payer, account(1_000_000_000)),
            (tree, account(1_000_000_000)),
            (authority, account(1_000_000_000)),
            (
                spp_id,
                mollusk_svm::program::create_program_account_loader_v3(&spp_id),
            ),
        ];
        (
            Instruction {
                program_id: program_id(),
                accounts: metas,
                data,
            },
            accounts,
        )
    }
}
