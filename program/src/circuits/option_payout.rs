//! Settlement math of a dated contract: an option or a prediction.
//!
//! Plain integer code with no SDK types, like `payout`, so the same function
//! runs natively on the client, compiles into the option settle circuit, and
//! is unit-tested on its own.
//!
//! The trader pays a premium and the maker puts up collateral; both sit in
//! one UTXO (`total`). Settling pays the trader what the contract is worth
//! at the oracle price, never more than `total`, and the maker the rest. The
//! premium is spent: the trader gets nothing back when the contract ends
//! worthless.

use super::payout::mul_div_scale;

/// A call: pays `size * (price - strike)` when the price ends above the
/// strike.
pub const KIND_CALL: u8 = 0;
/// A put: pays `size * (strike - price)` when the price ends below the
/// strike.
pub const KIND_PUT: u8 = 1;
/// A prediction that the price ends at or above the strike: pays the whole
/// of `total` if it does, nothing if it does not.
pub const KIND_ABOVE: u8 = 2;
/// A prediction that the price ends below the strike.
pub const KIND_BELOW: u8 = 3;

/// The highest valid kind.
pub const KIND_MAX: u8 = KIND_BELOW;

/// Whether `kind` pays all or nothing.
pub fn is_prediction(kind: u8) -> bool {
    kind >= KIND_ABOVE
}

/// The trader's share of `total` when the contract settles at `price`; the
/// maker receives `total - option_payout`.
///
/// - A call or a put pays its intrinsic value, `size * |price - strike| /
///   SIZE_SCALE` on the right side of the strike, capped at `total`.
/// - A prediction pays `total` when it came true and nothing otherwise.
///   "Above" is true at the strike itself; "below" is strictly below.
///
/// The intrinsic value rounds down, so a fractional base unit stays with the
/// maker. `kind` must be at most [`KIND_MAX`], which the quote circuit
/// requires.
// Explicit branches and bitwise operators, as in `payout`: this function is
// compiled into the settle circuit.
pub fn option_payout(kind: u8, size: u64, strike: u64, total: u64, price: u64) -> u64 {
    let price_up = price >= strike;
    let diff = if price_up {
        price - strike
    } else {
        strike - price
    };
    let wants_up = (kind == KIND_CALL) | (kind == KIND_ABOVE);
    let right_side = price_up == wants_up;
    if is_prediction(kind) {
        if right_side {
            total
        } else {
            0
        }
    } else if right_side {
        // Saturated at `u64::MAX`, which is at least any total.
        let value = mul_div_scale(size, diff);
        if value >= total {
            total
        } else {
            value
        }
    } else {
        0
    }
}

/// Whether a contract whose term ends at `term_end` has matured at an oracle
/// price published at `publish_time`, both in unix seconds. From then on
/// either party may settle it.
pub fn is_matured(publish_time: u64, term_end: u64) -> bool {
    publish_time >= term_end
}

/// Whether a party may settle: the trader may exercise a call or a put at any
/// time, for its value at that moment; a prediction settles only once it has
/// matured; and the maker settles only what has matured.
pub fn may_settle(kind: u8, by_trader: bool, by_maker: bool, matured: bool) -> bool {
    (by_trader & !is_prediction(kind)) | ((by_trader | by_maker) & matured)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 1 SOL against USDC on a feed with exponent -8.
    const ONE_SOL: u64 = 10_000_000;
    const USD: u64 = 100_000_000; // price ticks per dollar
    const USDC: u64 = 1_000_000; // base units per USDC

    #[test]
    fn a_call_pays_what_the_price_is_above_the_strike() {
        let pay = |price| option_payout(KIND_CALL, ONE_SOL, 100 * USD, 30 * USDC, price);
        assert_eq!(pay(90 * USD), 0);
        assert_eq!(pay(100 * USD), 0);
        assert_eq!(pay(105 * USD), 5 * USDC);
        // Capped at the whole position.
        assert_eq!(pay(130 * USD), 30 * USDC);
        assert_eq!(pay(500 * USD), 30 * USDC);
    }

    #[test]
    fn a_put_pays_what_the_price_is_below_the_strike() {
        let pay = |price| option_payout(KIND_PUT, ONE_SOL, 100 * USD, 30 * USDC, price);
        assert_eq!(pay(110 * USD), 0);
        assert_eq!(pay(100 * USD), 0);
        assert_eq!(pay(95 * USD), 5 * USDC);
        assert_eq!(pay(70 * USD), 30 * USDC);
        assert_eq!(pay(0), 30 * USDC);
    }

    #[test]
    fn a_prediction_pays_all_or_nothing() {
        let above = |price| option_payout(KIND_ABOVE, 0, 100 * USD, 30 * USDC, price);
        assert_eq!(above(100 * USD - 1), 0);
        // True at the strike itself.
        assert_eq!(above(100 * USD), 30 * USDC);
        assert_eq!(above(200 * USD), 30 * USDC);
        let below = |price| option_payout(KIND_BELOW, 0, 100 * USD, 30 * USDC, price);
        assert_eq!(below(100 * USD - 1), 30 * USDC);
        assert_eq!(below(100 * USD), 0);
        assert_eq!(below(200 * USD), 0);
    }

    #[test]
    fn exactly_one_of_two_opposite_predictions_pays() {
        for price in [0, 1, 100 * USD - 1, 100 * USD, 100 * USD + 1, u64::MAX] {
            let above = option_payout(KIND_ABOVE, 0, 100 * USD, 7, price);
            let below = option_payout(KIND_BELOW, 0, 100 * USD, 7, price);
            assert_eq!(above + below, 7, "price {price}");
        }
    }

    #[test]
    fn the_value_rounds_down_and_never_exceeds_the_total() {
        // 150 ticks on 1 SOL is 1.5 base units.
        assert_eq!(
            option_payout(KIND_CALL, ONE_SOL, 100 * USD, 30 * USDC, 100 * USD + 150),
            1
        );
        assert_eq!(
            option_payout(KIND_PUT, ONE_SOL, 100 * USD, 30 * USDC, 100 * USD - 150),
            1
        );
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..200_000 {
            let size = next() >> (next() % 64);
            let strike = next() >> (next() % 64);
            let price = next() >> (next() % 64);
            let total = next() >> (next() % 64);
            for kind in 0..=KIND_MAX {
                let paid = option_payout(kind, size, strike, total, price);
                assert!(paid <= total);
                if is_prediction(kind) {
                    assert!(paid == 0 || paid == total);
                } else {
                    let exact = (size as u128) * (price.abs_diff(strike) as u128)
                        / (super::super::payout::SIZE_SCALE as u128);
                    let in_the_money = (price >= strike) == (kind == KIND_CALL);
                    let expected = if in_the_money {
                        exact.min(total as u128) as u64
                    } else {
                        0
                    };
                    assert_eq!(
                        paid, expected,
                        "kind={kind} size={size} strike={strike} total={total} price={price}"
                    );
                }
            }
        }
    }

    #[test]
    fn who_may_settle_and_when() {
        for kind in [KIND_CALL, KIND_PUT] {
            // The trader exercises whenever it likes.
            assert!(may_settle(kind, true, false, false));
            assert!(may_settle(kind, true, false, true));
            // The maker only once the term has ended.
            assert!(!may_settle(kind, false, true, false));
            assert!(may_settle(kind, false, true, true));
        }
        for kind in [KIND_ABOVE, KIND_BELOW] {
            assert!(!may_settle(kind, true, false, false));
            assert!(!may_settle(kind, false, true, false));
            assert!(may_settle(kind, true, false, true));
            assert!(may_settle(kind, false, true, true));
        }
        // Nobody else, ever.
        for kind in 0..=KIND_MAX {
            assert!(!may_settle(kind, false, false, false));
            assert!(!may_settle(kind, false, false, true));
        }
    }

    #[test]
    fn maturity_starts_at_the_end_of_the_term() {
        assert!(!is_matured(999, 1_000));
        assert!(is_matured(1_000, 1_000));
        assert!(is_matured(1_001, 1_000));
    }
}
