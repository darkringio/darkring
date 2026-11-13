//! Who a transaction is built for. Building and proving a transaction needs
//! a party's address and its viewing and nullifier keys, but never the key
//! that signs for it: that one can stay somewhere else, such as a browser.

use anyhow::Result;
use spr_keypair::{NullifierKey, ShieldedAddress, ShieldedKeypair, ViewingKey};

use crate::err;

/// A trader or a maker, as far as building and proving go.
pub trait Party {
    fn shielded_address(&self) -> Result<ShieldedAddress>;
    /// Decrypts what is addressed to the party and encrypts what it sends.
    fn viewing_key(&self) -> &ViewingKey;
    /// Derives the nullifiers of the notes the party spends.
    fn nullifier_key(&self) -> &NullifierKey;
}

impl Party for ShieldedKeypair {
    fn shielded_address(&self) -> Result<ShieldedAddress> {
        ShieldedKeypair::shielded_address(self).map_err(err)
    }

    fn viewing_key(&self) -> &ViewingKey {
        &self.viewing_key
    }

    fn nullifier_key(&self) -> &NullifierKey {
        &self.nullifier_key
    }
}

/// A party whose signing key is held elsewhere: its address and the two
/// keys expanded from its derivation seed. Enough to read its notes and to
/// build and prove its transactions, not enough to send one.
#[derive(Clone)]
pub struct ViewKeys {
    pub address: ShieldedAddress,
    pub nullifier_key: NullifierKey,
    pub viewing_key: ViewingKey,
}

impl Party for ViewKeys {
    fn shielded_address(&self) -> Result<ShieldedAddress> {
        Ok(self.address)
    }

    fn viewing_key(&self) -> &ViewingKey {
        &self.viewing_key
    }

    fn nullifier_key(&self) -> &NullifierKey {
        &self.nullifier_key
    }
}
