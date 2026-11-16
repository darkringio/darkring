//! A veil note: money owned by the program's veil PDA that pays out to
//! whoever knows the note's secret. See `circuits::veil` in the program.

use anyhow::{ensure, Result};
use perps_program::circuits::{self, veil_claim};
use spr_keypair::X25519Pubkey;
use spr_program::{circuit::DataHash, DataUtxo, EncryptionCodec, Field, ProgramOwner};
use spr_transaction::{Mint, SppProofOutputUtxo, WalletUtxo};

use crate::{err, shared::field, veil_authority};

/// Output slot of the veil note in the `veil_in` transaction.
pub const VEIL_NOTE_SLOT: usize = 0;
/// Output slot of the depositor's change in the `veil_in` transaction. The
/// slot is empty when the notes spent hold exactly the amount.
pub const VEIL_CHANGE_SLOT: usize = 1;
/// Output slot of the payout in the `veil_out` transaction.
pub const VEIL_PAYOUT_SLOT: usize = 0;

/// The claim a veil note commits for `secret`, `Poseidon(VEIL, secret)`, as
/// the bytes the note stores. `secret` must be a canonical field element.
pub fn claim_of(secret: &[u8; 32]) -> Result<[u8; 32]> {
    let claim = veil_claim(&secret_field(secret)?).map_err(err)?;
    Ok(spr_program::to_bytes(&claim))
}

/// `secret` as the field element the circuits take it as.
pub fn secret_field(secret: &[u8; 32]) -> Result<Field> {
    field(*secret).map_err(|_| anyhow::anyhow!("the veil secret is not a canonical field element"))
}

/// The veil PDA as a UTXO owner, under the zero nullifier key: the one owner
/// of every veil note.
pub fn veil_owner() -> ProgramOwner {
    veil_authority()
}

/// A veil note: an amount of one mint owned by the veil PDA, with a claim as
/// its state. Its opening and the secret behind its claim are the complete
/// spend capability; the program signs for the PDA only through `veil_out`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VeilNoteUtxo {
    /// `Poseidon(VEIL, secret)`.
    pub claim: [u8; 32],
    /// Derived by the `veil_in` transaction from its first nullifier,
    /// blinding seed and output slot.
    pub blinding: [u8; 32],
    pub mint: Mint,
    pub amount: u64,
}

impl VeilNoteUtxo {
    /// The note of a decoded veil UTXO. The SDK's decode
    /// ([`DataUtxo::try_from`]) has checked the state against the UTXO's
    /// commitment.
    pub fn from_data_utxo(note: DataUtxo<circuits::VeilNote>) -> Self {
        Self {
            claim: note.data.claim,
            blinding: note.utxo.utxo.blinding,
            mint: note.utxo.utxo.asset,
            amount: note.utxo.utxo.amount,
        }
    }

    /// The note's state.
    pub fn state(&self) -> circuits::VeilNote {
        circuits::VeilNote { claim: self.claim }
    }

    /// Whether `secret` opens the note's claim.
    pub fn opens_with(&self, secret: &[u8; 32]) -> bool {
        claim_of(secret).is_ok_and(|claim| claim == self.claim)
    }

    /// The note output, encrypted to `viewing_pubkey`. The viewing key does
    /// not enter the commitment.
    pub fn output_utxo(&self, viewing_pubkey: X25519Pubkey) -> Result<SppProofOutputUtxo> {
        let state = self.state();
        let state_hash = DataHash::hash(&state).map_err(err)?;
        let data_hash =
            spr_program::hasher::data_hash(&spr_program::to_bytes(&state_hash)).map_err(err)?;
        Ok(SppProofOutputUtxo {
            asset: self.mint,
            amount: self.amount,
            blinding: self.blinding,
            owner_address: Some(veil_owner().address(viewing_pubkey).map_err(err)?),
            ..Default::default()
        }
        .with_utxo_data(state.serialize_for_encryption().map_err(err)?, data_hash))
    }

    /// The note's commitment in tree `tree_id`. Any viewing key gives the
    /// same hash: it only says who can read the ciphertext.
    pub fn hash(&self, tree_id: u16) -> Result<[u8; 32]> {
        self.output_utxo(placeholder_viewing_key()?)?
            .hash(tree_id)
            .map_err(err)
    }

    /// The note as an SPP input at `leaf_index` of tree `tree_id`. Anyone
    /// holding the opening can build it; spending needs the PDA signature
    /// the program gives only on a proof of the secret.
    pub fn to_input_utxo(&self, tree_id: u16, leaf_index: u64) -> Result<WalletUtxo> {
        ensure!(self.amount != 0, "the veil note is empty");
        veil_owner()
            .input(
                &self.output_utxo(placeholder_viewing_key()?)?,
                tree_id,
                leaf_index,
            )
            .map_err(err)
    }
}

/// A well-formed viewing key for where one is needed but not committed: the
/// X25519 base point.
fn placeholder_viewing_key() -> Result<X25519Pubkey> {
    let mut u = [0u8; 32];
    u[0] = 9;
    X25519Pubkey::from_u(u).map_err(err)
}
