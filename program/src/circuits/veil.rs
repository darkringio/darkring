//! The veil: notes that belong to whoever knows their secret.
//!
//! In the Rings pool every note names its owner in the clear; only amounts
//! are sealed. So moving money from one address to another, even through a
//! fresh address in between, leaves a trail anyone can follow.
//!
//! A veil note is owned by one address of this program, the same for every
//! veil note, and commits the hash of a secret. `veil_in` puts money into
//! one; `veil_out` pays one out, whole, to any owner, on a proof of knowing
//! its secret. Spending a note publishes its nullifier and not which note it
//! was, so the chain shows that some veil note was paid out to that owner
//! and cannot tell whose it was. The amounts on both sides stay sealed.

use borsh::{BorshDeserialize, BorshSerialize};
use spr_program::{
    circuit::{
        CheckedTransaction, ConfidentialTransaction, DataUtxo, PublicInputs, TokenUtxos, Utxo,
        UtxoTrait,
    },
    input, poseidon, require, require_eq, ArrayVec, CircuitError, ClientInputs, Field,
    ShieldedAddress, TxContext, UtxoState, ZkProgram,
};

/// How many of the depositor's notes one `veil_in` can gather.
pub const VEIL_INPUTS: usize = 4;

/// The state of a veil note (`DataUtxo<VeilNote>`).
#[input]
#[derive(Default, PartialEq, Eq, BorshSerialize, BorshDeserialize, UtxoState, ClientInputs)]
pub struct VeilNote {
    /// `Poseidon(VEIL, secret)` as 32 big-endian bytes. Whoever knows
    /// `secret`, and the note's opening, may pay the note out.
    pub claim: [u8; 32],
}

/// The claim a veil note commits for `secret`.
pub fn veil_claim(secret: &Field) -> Result<Field, CircuitError> {
    poseidon(&[Field::from(crate::VEIL_CLAIM_DOMAIN), *secret])
}

/// A note's claim as a field element: its bytes packed big-endian, a linear
/// combination with no constraint of its own. Bytes at or above the modulus
/// wrap, which only makes a note its own depositor wrote harder to claim.
fn claim_field(claim: &[u8; 32]) -> Field {
    let mut packed = Field::zero();
    for byte in claim.iter() {
        packed = packed * Field::from(256u64) + Field::from(*byte);
    }
    packed
}

/// Puts money behind the veil: moves `amount` from the depositor's notes
/// into a new veil note.
#[input]
#[derive(ZkProgram)]
pub struct VeilIn {
    pub private: VeilInPrivateInputs,
    pub public: VeilInPublicInputs,
}

#[input]
#[derive(ClientInputs)]
pub struct VeilInPrivateInputs {
    pub tx_context: TxContext,
    /// The depositor's spent notes, of one asset. SPP authorizes the spend
    /// with the depositor's signature; what is not moved returns as change.
    pub sources: ArrayVec<Utxo, VEIL_INPUTS>,
    /// What the veil note holds.
    pub amount: u64,
    /// The note's claim; see [`VeilNote`].
    pub claim: [u8; 32],
}

#[input]
#[derive(PublicInputs)]
pub struct VeilInPublicInputs {
    /// The veil note's address: the program's veil PDA with a viewing key of
    /// the depositor's choosing. The program derives the PDA and hashes the
    /// address, which binds the note to the PDA that only the program signs
    /// for.
    pub note_owner: ShieldedAddress,
}

impl VeilIn {
    /// 1. Spend the depositor's notes.
    /// 2. Create the veil note, owned by the public `note_owner`, and move
    ///    `amount` into it.
    /// 3. Set the note's claim.
    /// 4. Check the transaction: outputs `[veil note, change]`.
    pub fn execute(&self) -> Result<CheckedTransaction, CircuitError> {
        let private = &self.private;

        // 1. Spend the depositor's notes.
        let mut tokens = TokenUtxos::new_mut(&private.sources)?;

        // 2. Move `amount` into the veil note.
        require(private.amount != 0);
        let mut note = DataUtxo::<VeilNote>::new_init(self.public.note_owner.owner())
            .with_asset(&tokens.asset())?;
        tokens.transfer(&mut note, &private.amount)?;

        // 3. Set the claim.
        note.claim = private.claim;

        // 4. Check the transaction. The change is registered last: it is an
        // empty output when the notes hold exactly the amount.
        ConfidentialTransaction::new(&private.tx_context, &self.public)
            .with(note)
            .with(tokens)
            .check()
    }
}

/// Pays a veil note out, whole, to `recipient`, on knowledge of its secret.
#[input]
#[derive(ZkProgram)]
pub struct VeilOut {
    pub private: VeilOutPrivateInputs,
    pub public: VeilOutPublicInputs,
}

#[input]
#[derive(ClientInputs)]
pub struct VeilOutPrivateInputs {
    pub tx_context: TxContext,
    pub terms: VeilNote,
    /// The veil note, which commits `terms`.
    pub note_utxo: Utxo,
    /// The secret behind the note's claim.
    pub secret: Field,
    /// Who is paid: its owner receives the note's whole amount, encrypted to
    /// its viewing key.
    pub recipient: ShieldedAddress,
}

#[input]
#[derive(PublicInputs)]
pub struct VeilOutPublicInputs {
    /// The first SPP input's nullifier, which the program reads from the
    /// transact.
    pub first_nullifier: Field,
}

impl VeilOut {
    /// 1. Spend the veil note, which commits the claim.
    /// 2. Check the secret opens the claim.
    /// 3. Pay the whole note to the recipient.
    /// 4. Check the transaction: input `[veil note]`, output `[payout]`.
    /// 5. Check the published first nullifier.
    pub fn execute(&self) -> Result<CheckedTransaction, CircuitError> {
        let private = &self.private;
        let public = &self.public;

        // 1. Spend the veil note. Its owner stays a free witness: SPP spends
        // it only with the veil PDA's signature, which only this program
        // gives, and only here.
        let mut note = DataUtxo::new_mut(&private.note_utxo, &private.terms)?.close();
        require(note.amount()? != 0);

        // 2. The secret opens the claim.
        require_eq(claim_field(&note.claim), veil_claim(&private.secret)?);

        // 3. Pay the whole note out.
        let mut payout = TokenUtxos::new_init(private.recipient.owner(), &note.asset());
        note.transfer_all(&mut payout)?;

        // 4. Check the transaction.
        let checked = ConfidentialTransaction::new(&private.tx_context, public)
            .with(note)
            .with(payout)
            .check()?;

        // 5. Check the published first nullifier, which ties the proof to
        // the note the transaction spends.
        require_eq(checked.first_nullifier, public.first_nullifier);
        Ok(checked)
    }
}
