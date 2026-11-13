use spr_program::{poseidon, require_eq, CircuitError, Field, TxContext};

/// The transaction's blinding seed is `Poseidon(tag, opening_blinding)`, so
/// anyone holding the opening of the spent quote or position UTXO derives
/// every output blinding from the seed and the first nullifier.
pub(super) fn assert_blinding_seed(
    context: &TxContext,
    tag: u32,
    opening_blinding: &Field,
) -> Result<(), CircuitError> {
    require_eq(
        context.blinding_seed,
        poseidon(&[Field::from(tag), *opening_blinding])?,
    );
    Ok(())
}
