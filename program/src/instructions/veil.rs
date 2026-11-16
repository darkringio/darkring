//! The veil's two instructions: `veil_in` puts money into a note owned by
//! the program's veil PDA, and `veil_out` pays such a note out to whoever
//! proves knowledge of its secret. See `circuits::veil`.

use light_program_profiler::profile;
use pinocchio::{
    cpi::{Seed, Signer},
    error::ProgramError,
    AccountView, Address, ProgramResult,
};
use spr_account_checks::AccountIterator;
use spr_interface::instruction::instruction_data::transact::TransactIxData;
use spr_program::{
    cpi::{SppTransactAccounts, TransactAccountsError},
    program::PdaOwner,
    verifier::address_hash,
    CompressedProof,
};
use wincode::{SchemaRead, SchemaWrite};

use crate::{
    error::PerpError,
    instructions::shared::IxData,
    verifier::{veil_in, veil_out},
};

/// The veil PDA, `[VEIL_PDA_SEED, [bump]]`: the owner of every veil note
/// under nullifier secret 0. Only the program signs for it, so a veil note
/// moves only through `veil_out`.
pub struct VeilAuthority {
    address: Address,
    bump: u8,
}

impl VeilAuthority {
    /// The PDA with the bump the client passes, so the derivation costs one
    /// `create_program_address`. The SDK passes the canonical bump; another
    /// bump that derives a program address names a PDA that owns no veil
    /// note the SDK made.
    pub fn new(bump: u8) -> Result<Self, PerpError> {
        let address = Address::create_program_address(&[crate::VEIL_PDA_SEED, &[bump]], &crate::ID)
            .map_err(|_| PerpError::InvalidVeilBump)?;
        Ok(Self { address, bump })
    }

    /// The public form of the veil PDA's shielded address under
    /// `viewing_pubkey`: the owner a `veil_in` proof must give its note.
    pub fn utxo_owner(&self, viewing_pubkey: &[u8; 33]) -> Result<[u8; 32], PerpError> {
        let owner = PdaOwner::new(&self.address).map_err(|_| PerpError::HashingFailed)?;
        Ok(address_hash(owner.owner_hash(), viewing_pubkey)?)
    }

    /// Invokes SPP `transact` with `spp_accounts`, signed by the veil PDA,
    /// which must be among them.
    #[inline(never)]
    #[profile]
    pub fn cpi_spp_transact_signed(
        &self,
        spp_accounts: &[AccountView],
        transact: &[u8],
    ) -> ProgramResult {
        let signer_pdas = [&self.address];
        let spp = SppTransactAccounts::new(spp_accounts, &signer_pdas).map_err(
            |error| -> ProgramError {
                match error {
                    TransactAccountsError::InvalidSppProgram => {
                        PerpError::InvalidShieldedPoolProgram.into()
                    }
                    TransactAccountsError::MissingPdaSigner { .. } => {
                        PerpError::MissingVeilAuthority.into()
                    }
                    TransactAccountsError::NotEnoughAccounts => ProgramError::NotEnoughAccountKeys,
                }
            },
        )?;
        let bump = [self.bump];
        let seeds = [Seed::from(crate::VEIL_PDA_SEED), Seed::from(&bump)];
        spp.invoke::<16>(transact, &[Signer::from(&seeds)])
    }
}

pub use spr_program::CompressedProof as VeilProof;

/// The `veil_in` arguments the program reads ahead of the transact bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, SchemaRead, SchemaWrite)]
pub struct VeilInArgs {
    pub proof: VeilProof,
    /// See [`VeilInIxData::viewing_pubkey`].
    pub viewing_pubkey: [u8; 33],
    /// See [`VeilInIxData::authority_bump`].
    pub authority_bump: u8,
}

/// The `veil_in` instruction data as a client writes it.
#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite)]
pub struct VeilInIxData {
    pub proof: VeilProof,
    /// The viewing key of the veil note's address, which the note is
    /// encrypted to. The proof's public `note_owner` is the veil PDA with
    /// this viewing key.
    pub viewing_pubkey: [u8; 33],
    /// The bump of the veil PDA; see [`VeilAuthority::new`].
    pub authority_bump: u8,
    pub transact: TransactIxData,
}

/// The `veil_out` arguments the program reads ahead of the transact bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, SchemaRead, SchemaWrite)]
pub struct VeilOutArgs {
    pub proof: VeilProof,
    /// See [`VeilOutIxData::authority_bump`].
    pub authority_bump: u8,
}

/// The `veil_out` instruction data as a client writes it.
#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite)]
pub struct VeilOutIxData {
    pub proof: VeilProof,
    /// The bump of the veil PDA; see [`VeilAuthority::new`].
    pub authority_bump: u8,
    pub transact: TransactIxData,
}

/// 1. Check the fee payer.
/// 2. Decode the instruction and derive the veil PDA.
/// 3. Verify the proof against the veil note's owner, the veil PDA.
/// 4. Invoke SPP with the veil PDA signer, which authorizes the note output;
///    the depositor signs for the notes it spends.
#[inline(never)]
#[profile]
pub fn process_veil_in_ix(accounts: &mut [AccountView], data: &[u8]) -> ProgramResult {
    // 1. Check the fee payer.
    let mut iter = AccountIterator::new(accounts);
    iter.next_signer_mut("payer")?;

    // 2. Decode the instruction and derive the veil PDA.
    let IxData {
        args:
            VeilInArgs {
                proof,
                viewing_pubkey,
                authority_bump,
            },
        transact,
        transact_bytes,
    } = IxData::<VeilInArgs>::parse(data)?;
    let authority = VeilAuthority::new(authority_bump)?;

    // 3. Verify the proof. SPP accepts a data-bearing output owned by any
    // owner signer; the proof binds the note to the PDA.
    let public_inputs = veil_in::PublicInputs {
        note_owner: authority.utxo_owner(&viewing_pubkey)?,
    };
    verify_veil_in_proof(&proof, public_inputs, transact.private_tx_hash)?;

    // 4. Invoke SPP with the veil PDA signer.
    let spp_accounts = iter.remaining()?;
    authority.cpi_spp_transact_signed(spp_accounts, transact_bytes)
}

/// 1. Check the fee payer.
/// 2. Decode the instruction and derive the veil PDA.
/// 3. Verify the proof against the first nullifier. The proof shows the
///    sender knows the secret of the note the transaction spends.
/// 4. Invoke SPP with the veil PDA signer, which authorizes the note spend.
#[inline(never)]
#[profile]
pub fn process_veil_out_ix(accounts: &mut [AccountView], data: &[u8]) -> ProgramResult {
    // 1. Check the fee payer. Nobody else signs: the secret is the right to
    // the note.
    let mut iter = AccountIterator::new(accounts);
    iter.next_signer_mut("payer")?;

    // 2. Decode the instruction and derive the veil PDA.
    let IxData {
        args: VeilOutArgs {
            proof,
            authority_bump,
        },
        transact,
        transact_bytes,
    } = IxData::<VeilOutArgs>::parse(data)?;
    let authority = VeilAuthority::new(authority_bump)?;

    // 3. Verify the proof.
    let first_nullifier = transact
        .inputs
        .first()
        .ok_or(PerpError::InvalidInstructionData)?
        .nullifier_hash;
    let public_inputs = veil_out::PublicInputs { first_nullifier };
    verify_veil_out_proof(&proof, public_inputs, transact.private_tx_hash)?;

    // 4. Invoke SPP with the veil PDA signer.
    let spp_accounts = iter.remaining()?;
    authority.cpi_spp_transact_signed(spp_accounts, transact_bytes)
}

#[inline(never)]
#[profile]
fn verify_veil_in_proof(
    proof: &CompressedProof,
    public_inputs: veil_in::PublicInputs,
    private_tx_hash: &[u8; 32],
) -> ProgramResult {
    veil_in::Verifier {
        proof,
        public_inputs,
        private_tx_hash,
        transfers: &[],
    }
    .verify()
    .map_err(PerpError::from)?;
    Ok(())
}

#[inline(never)]
#[profile]
fn verify_veil_out_proof(
    proof: &CompressedProof,
    public_inputs: veil_out::PublicInputs,
    private_tx_hash: &[u8; 32],
) -> ProgramResult {
    veil_out::Verifier {
        proof,
        public_inputs,
        private_tx_hash,
        transfers: &[],
    }
    .verify()
    .map_err(PerpError::from)?;
    Ok(())
}
