use anyhow::Result;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;
use spr_interface::instruction::instruction_data::transact::TransactIxData;

use crate::{
    err, maker_authority_bump, maker_authority_pda, shared::spp_accounts, tag, OpenIxData,
    OpenProof,
};

#[derive(Clone)]
pub struct Open {
    /// The trader: fee payer and owner of the spent margin.
    pub payer: Pubkey,
    /// The quote's maker, whose authority PDA owns the quote UTXO and the
    /// position UTXO, and signs for both.
    pub maker: Pubkey,
    /// The viewing key of the position UTXO's address, the maker's.
    pub viewing_pubkey: [u8; 33],
    pub tree: Pubkey,
    pub open_proof: OpenProof,
    pub spp_proof: TransactIxData,
}

impl Open {
    pub fn instruction(self) -> Result<Instruction> {
        let Self {
            payer,
            maker,
            viewing_pubkey,
            tree,
            open_proof,
            spp_proof,
        } = self;

        // The quote is the first input; the trader's margin is the second.
        // The trader also pays, but must appear explicitly in the owner
        // slots. The authority also owns the data-bearing position output.
        let mut accounts = vec![AccountMeta::new(payer, true)];
        accounts.extend(spp_accounts(
            &maker,
            payer,
            tree,
            vec![maker_authority_pda(&maker), payer],
            &spp_proof,
        ));
        let serialized_ix = wincode::serialize(&OpenIxData {
            proof: open_proof,
            maker: maker.to_bytes(),
            viewing_pubkey,
            authority_bump: maker_authority_bump(&maker),
            transact: spp_proof,
        })
        .map_err(err)?;
        let mut data = vec![tag::OPEN];
        data.extend_from_slice(&serialized_ix);
        Ok(Instruction {
            program_id: perps_program::ID,
            accounts,
            data,
        })
    }
}
