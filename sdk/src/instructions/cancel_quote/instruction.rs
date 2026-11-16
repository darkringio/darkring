use anyhow::Result;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;
use spr_interface::instruction::instruction_data::transact::TransactIxData;

use crate::{
    err, maker_authority_bump, maker_authority_pda, shared::spp_accounts, tag, CancelQuoteIxData,
    CancelQuoteProof,
};

pub struct CancelQuote {
    /// The maker's ed25519 pubkey, a dedicated readonly signer the program
    /// checks against the cancel proof's committed maker.
    pub maker: Pubkey,
    pub payer: Pubkey,
    pub tree: Pubkey,
    pub cancel_proof: CancelQuoteProof,
    pub quote_expiry: u64,
    pub spp_proof: TransactIxData,
}

impl CancelQuote {
    /// The perp instruction.
    pub fn instruction(self) -> Result<Instruction> {
        self.tagged(tag::CANCEL_QUOTE)
    }

    /// The same accounts and data under the option program's tag: the
    /// instruction of a dated contract, which verifies the option proof.
    pub fn option_instruction(self) -> Result<Instruction> {
        self.tagged(tag::OPTION_CANCEL)
    }

    fn tagged(self, tag: u8) -> Result<Instruction> {
        let Self {
            maker,
            payer,
            tree,
            cancel_proof,
            quote_expiry,
            spp_proof,
        } = self;

        // The maker is a dedicated readonly signer after the fee payer; the
        // quote is the only input, owned by the maker's authority.
        let mut accounts = vec![
            AccountMeta::new(payer, true),
            AccountMeta::new_readonly(maker, true),
        ];
        accounts.extend(spp_accounts(
            &maker,
            payer,
            tree,
            vec![maker_authority_pda(&maker)],
            &spp_proof,
        ));
        let serialized_ix = wincode::serialize(&CancelQuoteIxData {
            proof: cancel_proof,
            quote_expiry,
            authority_bump: maker_authority_bump(&maker),
            transact: spp_proof,
        })
        .map_err(err)?;
        let mut data = vec![tag];
        data.extend_from_slice(&serialized_ix);
        Ok(Instruction {
            program_id: perps_program::ID,
            accounts,
            data,
        })
    }
}
