use anyhow::Result;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;
use spr_interface::instruction::instruction_data::transact::TransactIxData;

use crate::{
    err, maker_authority_bump, maker_authority_pda, shared::spp_accounts, tag, SettleIxData,
    SettleProof,
};

#[derive(Clone)]
pub struct Settle {
    pub payer: Pubkey,
    /// The settling party's Solana pubkey, the trader's or the maker's: a
    /// dedicated readonly signer the program checks against the owners the
    /// position commits.
    pub signer: Pubkey,
    /// The oracle price update account the program reads the price from.
    pub oracle: Pubkey,
    /// The position's maker, whose authority PDA owns the position UTXO and
    /// signs its spend.
    pub maker: Pubkey,
    pub tree: Pubkey,
    pub settle_proof: SettleProof,
    pub spp_proof: TransactIxData,
}

impl Settle {
    pub fn instruction(self) -> Result<Instruction> {
        let Self {
            payer,
            signer,
            oracle,
            maker,
            tree,
            settle_proof,
            spp_proof,
        } = self;

        // The settling signer and the oracle account follow the fee payer;
        // the position is the only input, owned by the maker's authority.
        let mut accounts = vec![
            AccountMeta::new(payer, true),
            AccountMeta::new_readonly(signer, true),
            AccountMeta::new_readonly(oracle, false),
        ];
        accounts.extend(spp_accounts(
            &maker,
            payer,
            tree,
            vec![maker_authority_pda(&maker)],
            &spp_proof,
        ));
        let serialized_ix = wincode::serialize(&SettleIxData {
            proof: settle_proof,
            maker: maker.to_bytes(),
            authority_bump: maker_authority_bump(&maker),
            transact: spp_proof,
        })
        .map_err(err)?;
        let mut data = vec![tag::SETTLE];
        data.extend_from_slice(&serialized_ix);
        Ok(Instruction {
            program_id: perps_program::ID,
            accounts,
            data,
        })
    }
}
