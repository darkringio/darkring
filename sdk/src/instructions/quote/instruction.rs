use anyhow::Result;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;
use spr_interface::instruction::instruction_data::transact::{MessageData, TransactIxData};
use spr_keypair::ShieldedAddress;
use spr_transaction::TransactionError;

use crate::{
    err, maker_authority_bump, maker_authority_pda, shared::spp_accounts, tag, QuoteIxData,
    QuoteMarkerData, QuoteProof,
};

pub struct QuoteMarker {
    pub quote_utxo_hash: [u8; 32],
    pub maker_pubkey: Pubkey,
    pub trader_address: ShieldedAddress,
}

impl QuoteMarker {
    pub fn message(self) -> Result<MessageData, TransactionError> {
        Ok(MessageData {
            view_tag: self.trader_address.signing_pubkey.confidential_view_tag()?,
            data: borsh::to_vec(&QuoteMarkerData {
                quote_utxo_hash: self.quote_utxo_hash,
                maker_pubkey: self.maker_pubkey.to_bytes(),
            })
            .map_err(|e| TransactionError::Serialize(e.to_string()))?,
        })
    }
}

pub struct Quote {
    /// The maker: fee payer, marker signer and owner of the spent source.
    pub payer: Pubkey,
    /// The viewing key of the quote UTXO's address, the trader's.
    pub viewing_pubkey: [u8; 33],
    pub tree: Pubkey,
    pub quote_proof: QuoteProof,
    pub spp_proof: TransactIxData,
}

impl Quote {
    pub fn instruction(self) -> Result<Instruction> {
        let Self {
            payer,
            viewing_pubkey,
            tree,
            quote_proof,
            spp_proof,
        } = self;

        // The owner signers: the maker for the source it spends, then the
        // maker's authority for the data-bearing quote output, which the
        // program signs for during the CPI.
        let mut accounts = vec![AccountMeta::new(payer, true)];
        accounts.extend(spp_accounts(
            &payer,
            payer,
            tree,
            vec![payer, maker_authority_pda(&payer)],
            &spp_proof,
        ));
        let serialized_ix = wincode::serialize(&QuoteIxData {
            proof: quote_proof,
            viewing_pubkey,
            authority_bump: maker_authority_bump(&payer),
            transact: spp_proof,
        })
        .map_err(err)?;
        let mut data = vec![tag::QUOTE];
        data.extend_from_slice(&serialized_ix);
        Ok(Instruction {
            program_id: perps_program::ID,
            accounts,
            data,
        })
    }
}
