//! Reads a price from a Pyth price update account.
//!
//! The layout is the Pyth Solana receiver's `PriceUpdateV2`, an Anchor
//! account in Borsh:
//!
//! ```text
//! discriminator       [u8; 8]   sha256("account:PriceUpdateV2")[..8]
//! write_authority     [u8; 32]
//! verification_level  u8        0 = Partial { num_signatures: u8 }, 1 = Full
//! feed_id             [u8; 32]
//! price               i64
//! conf                u64
//! exponent            i32
//! publish_time        i64
//! ...                           previous publish time, EMA price, posted slot
//! ```
//!
//! Only a fully verified update is accepted, so the fields after
//! `verification_level` sit at fixed offsets.

use pinocchio::{AccountView, Address};

use crate::error::PerpError;

/// The Pyth Solana receiver program, which owns every price update account.
pub const ORACLE_PROGRAM_ID: Address =
    Address::from_str_const("rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ");

/// A settlement price may be at most this many seconds older than the Clock.
///
/// Whoever settles chooses the price update account, so within this window
/// it can pick the update that suits it best. A shorter window narrows that
/// choice; a longer one tolerates a slower oracle.
pub const MAX_PRICE_AGE_SECS: i64 = 60;

const DISCRIMINATOR: [u8; 8] = [0x22, 0xf1, 0x23, 0x63, 0x9d, 0x7e, 0xf4, 0xcd];
const VERIFICATION_LEVEL_OFFSET: usize = 8 + 32;
const VERIFICATION_LEVEL_FULL: u8 = 1;
const FEED_ID_OFFSET: usize = VERIFICATION_LEVEL_OFFSET + 1;
const PRICE_OFFSET: usize = FEED_ID_OFFSET + 32;
const PUBLISH_TIME_OFFSET: usize = PRICE_OFFSET + 8 + 8 + 4;
const MIN_LEN: usize = PUBLISH_TIME_OFFSET + 8;

/// A fully verified, positive price and the feed it belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OraclePrice {
    /// The feed id as the big-endian high and low halves of its 32 bytes,
    /// the form the position terms commit it in.
    pub feed_hi: u128,
    pub feed_lo: u128,
    /// The price in the feed's price ticks.
    pub price: u64,
    /// Unix seconds.
    pub publish_time: i64,
}

impl OraclePrice {
    /// Reads the price from `account`, which must be a price update owned by
    /// the oracle program, and checks that it is fresh at `now`.
    pub fn read(account: &AccountView, now: i64) -> Result<Self, PerpError> {
        if !account.owned_by(&ORACLE_PROGRAM_ID) {
            return Err(PerpError::InvalidOracleAccount);
        }
        let data = account
            .try_borrow()
            .map_err(|_| PerpError::InvalidOracleAccount)?;
        let price = Self::parse(&data)?;
        price.check_fresh(now)?;
        Ok(price)
    }

    /// Decodes a fully verified `PriceUpdateV2` with a positive price.
    pub fn parse(data: &[u8]) -> Result<Self, PerpError> {
        if data.len() < MIN_LEN || data.get(..8) != Some(DISCRIMINATOR.as_slice()) {
            return Err(PerpError::InvalidOracleAccount);
        }
        if data.get(VERIFICATION_LEVEL_OFFSET) != Some(&VERIFICATION_LEVEL_FULL) {
            return Err(PerpError::OracleNotFullyVerified);
        }
        let feed_hi = u128::from_be_bytes(read(data, FEED_ID_OFFSET)?);
        let feed_lo = u128::from_be_bytes(read(data, FEED_ID_OFFSET + 16)?);
        let price = i64::from_le_bytes(read(data, PRICE_OFFSET)?);
        let publish_time = i64::from_le_bytes(read(data, PUBLISH_TIME_OFFSET)?);
        if price <= 0 {
            return Err(PerpError::InvalidOraclePrice);
        }
        Ok(Self {
            feed_hi,
            feed_lo,
            price: price as u64,
            publish_time,
        })
    }

    /// `publish_time + MAX_PRICE_AGE_SECS >= now`, the receiver SDK's own
    /// staleness rule.
    pub fn check_fresh(&self, now: i64) -> Result<(), PerpError> {
        if self.publish_time.saturating_add(MAX_PRICE_AGE_SECS) >= now {
            Ok(())
        } else {
            Err(PerpError::StaleOraclePrice)
        }
    }
}

fn read<const N: usize>(data: &[u8], offset: usize) -> Result<[u8; N], PerpError> {
    data.get(offset..offset + N)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(PerpError::InvalidOracleAccount)
}

/// The bytes of a fully verified price update, for tests and local fixtures.
pub fn encode_price_update(feed_id: &[u8; 32], price: i64, publish_time: i64) -> [u8; 134] {
    // `PriceUpdateV2::LEN`: the account is allocated for the longer
    // `Partial` verification level, so a `Full` update ends one byte early.
    let mut data = [0u8; 134];
    let mut offset = 0;
    let mut put = |bytes: &[u8]| {
        if let Some(slot) = data.get_mut(offset..offset + bytes.len()) {
            slot.copy_from_slice(bytes);
        }
        offset += bytes.len();
    };
    put(&DISCRIMINATOR);
    put(&[0u8; 32]); // write authority
    put(&[VERIFICATION_LEVEL_FULL]);
    put(feed_id);
    put(&price.to_le_bytes());
    put(&0u64.to_le_bytes()); // confidence
    put(&(-8i32).to_le_bytes()); // exponent
    put(&publish_time.to_le_bytes());
    put(&publish_time.saturating_sub(1).to_le_bytes()); // previous publish time
    put(&price.to_le_bytes()); // EMA price
    put(&0u64.to_le_bytes()); // EMA confidence
    put(&0u64.to_le_bytes()); // posted slot
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed() -> [u8; 32] {
        core::array::from_fn(|i| i as u8 + 1)
    }

    #[test]
    fn parses_a_full_update() {
        let data = encode_price_update(&feed(), 12_345_678_900, 1_700_000_000);
        let price = OraclePrice::parse(&data).unwrap();
        assert_eq!(price.price, 12_345_678_900);
        assert_eq!(price.publish_time, 1_700_000_000);
        let mut id = [0u8; 32];
        id[..16].copy_from_slice(&price.feed_hi.to_be_bytes());
        id[16..].copy_from_slice(&price.feed_lo.to_be_bytes());
        assert_eq!(id, feed());
    }

    #[test]
    fn refuses_other_accounts() {
        let data = encode_price_update(&feed(), 1, 1);
        assert_eq!(
            OraclePrice::parse(&data[..MIN_LEN - 1]),
            Err(PerpError::InvalidOracleAccount)
        );
        let mut wrong = data;
        wrong[0] ^= 1;
        assert_eq!(
            OraclePrice::parse(&wrong),
            Err(PerpError::InvalidOracleAccount)
        );
        assert_eq!(
            OraclePrice::parse(&[]),
            Err(PerpError::InvalidOracleAccount)
        );
    }

    #[test]
    fn refuses_a_partially_verified_update() {
        let mut data = encode_price_update(&feed(), 1, 1);
        data[VERIFICATION_LEVEL_OFFSET] = 0;
        assert_eq!(
            OraclePrice::parse(&data),
            Err(PerpError::OracleNotFullyVerified)
        );
    }

    #[test]
    fn refuses_a_price_that_is_not_positive() {
        for price in [0, -1, i64::MIN] {
            let data = encode_price_update(&feed(), price, 1);
            assert_eq!(
                OraclePrice::parse(&data),
                Err(PerpError::InvalidOraclePrice)
            );
        }
    }

    #[test]
    fn freshness_window() {
        let data = encode_price_update(&feed(), 1, 1_000);
        let price = OraclePrice::parse(&data).unwrap();
        assert_eq!(price.check_fresh(1_000), Ok(()));
        assert_eq!(price.check_fresh(1_000 + MAX_PRICE_AGE_SECS), Ok(()));
        assert_eq!(
            price.check_fresh(1_001 + MAX_PRICE_AGE_SECS),
            Err(PerpError::StaleOraclePrice)
        );
        // An update published slightly ahead of the Clock is fresh.
        assert_eq!(price.check_fresh(990), Ok(()));
    }
}
