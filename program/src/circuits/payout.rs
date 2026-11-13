//! Settlement math of a Darkring position.
//!
//! Plain integer code with no SDK types, so the same function runs natively
//! on the client, compiles into the settle circuit, and is unit-tested on its
//! own (`rustc --test program/src/circuits/payout.rs`).
//!
//! A position is a two-party contract: the trader's margin and the maker's
//! margin sit together in one position UTXO (`total`), and settling splits
//! `total` between them at the oracle price. Nothing is ever paid from
//! outside the position, so each side's profit is capped by the other side's
//! margin and its loss by its own.

/// Fixed-point scale of `size`: the position's profit or loss in collateral
/// base units is `size * |price - entry_price| / SIZE_SCALE`.
///
/// `size` is therefore "collateral base units per oracle price tick, times
/// `SIZE_SCALE`". For 1 SOL against USDC (6 decimals) on a feed with exponent
/// -8, one tick is 1e-8 USD = 0.01 USDC base units, so `size = 10_000_000`.
pub const SIZE_SCALE: u64 = 1_000_000_000;

/// The trader's share of `total` when the position settles at `price`; the
/// maker receives `total - trader_payout`.
///
/// - The trader wins when the price moved in its direction: it receives its
///   margin plus the profit, capped at `total`.
/// - Otherwise it receives its margin minus the loss, floored at zero.
///
/// The profit or loss rounds down, so a fractional base unit stays with the
/// losing side. The result never exceeds `total` when
/// `trader_margin <= total`, which the open circuit guarantees.
// The subtractions are written as explicit branches rather than
// `saturating_sub`: this function is compiled into the settle circuit, and
// plain comparisons and subtractions are what the circuit build is known to
// accept.
#[allow(clippy::implicit_saturating_sub)]
pub fn trader_payout(
    trader_is_long: bool,
    size: u64,
    entry_price: u64,
    trader_margin: u64,
    total: u64,
    price: u64,
) -> u64 {
    let price_up = price >= entry_price;
    let diff = if price_up {
        price - entry_price
    } else {
        entry_price - price
    };
    // Saturated at `u64::MAX`, which is at least any margin or total, so the
    // caps below treat it like the exact value.
    let pnl = mul_div_scale(size, diff);

    // At `price == entry_price` the profit or loss is zero, so either branch
    // pays the margin back.
    let trader_wins = price_up == trader_is_long;
    if trader_wins {
        // min(trader_margin + pnl, total), without leaving u64.
        let room = if trader_margin >= total {
            0
        } else {
            total - trader_margin
        };
        if pnl >= room {
            total
        } else {
            trader_margin + pnl
        }
    } else if pnl >= trader_margin {
        0
    } else {
        trader_margin - pnl
    }
}

// The long division below takes 32-bit limbs, so the divisor must fit one.
const _: () = assert!(SIZE_SCALE != 0 && SIZE_SCALE < (1 << 32));

/// `floor(a * b / SIZE_SCALE)`, saturated at `u64::MAX`.
///
/// Written with 64-bit operations only. The circuit build accepts native
/// `u64` arithmetic, but not 128-bit division, which compiles to a software
/// routine that counts leading zeros of a witness.
fn mul_div_scale(a: u64, b: u64) -> u64 {
    const MASK: u64 = 0xFFFF_FFFF;
    let (a1, a0) = (a >> 32, a & MASK);
    let (b1, b0) = (b >> 32, b & MASK);

    // Schoolbook multiplication: each partial product of two 32-bit limbs
    // fits u64, and so does each column sum below.
    let p00 = a0 * b0;
    let p01 = a0 * b1;
    let p10 = a1 * b0;
    let p11 = a1 * b1;

    // The 128-bit product as 32-bit limbs, least significant first.
    let l0 = p00 & MASK;
    let c1 = (p00 >> 32) + (p01 & MASK) + (p10 & MASK);
    let l1 = c1 & MASK;
    let c2 = (c1 >> 32) + (p01 >> 32) + (p10 >> 32) + (p11 & MASK);
    let l2 = c2 & MASK;
    let l3 = (c2 >> 32) + (p11 >> 32);

    // Long division by a divisor below 2^32, most significant limb first.
    // Each remainder is below the divisor, so `(r << 32) | limb` fits u64
    // and each quotient limb fits 32 bits.
    let q3 = l3 / SIZE_SCALE;
    let r3 = l3 % SIZE_SCALE;
    let n2 = (r3 << 32) | l2;
    let q2 = n2 / SIZE_SCALE;
    let r2 = n2 % SIZE_SCALE;
    let n1 = (r2 << 32) | l1;
    let q1 = n1 / SIZE_SCALE;
    let r1 = n1 % SIZE_SCALE;
    let n0 = (r1 << 32) | l0;
    let q0 = n0 / SIZE_SCALE;

    // The quotient is q3 * 2^96 + q2 * 2^64 + q1 * 2^32 + q0.
    if (q3 | q2) != 0 {
        u64::MAX
    } else {
        (q1 << 32) | q0
    }
}

/// A position is liquidatable when the trader's equity at the settlement
/// price is at or below the maintenance margin. Only then may the maker
/// settle; the trader may settle at any time.
pub fn is_liquidatable(trader_payout: u64, maintenance_margin: u64) -> bool {
    trader_payout <= maintenance_margin
}

#[cfg(test)]
mod tests {
    use super::*;

    // 1 SOL against USDC on a feed with exponent -8.
    const ONE_SOL: u64 = 10_000_000;
    const USD: u64 = 100_000_000; // price ticks per dollar
    const USDC: u64 = 1_000_000; // base units per USDC

    fn settle(long: bool, margin: u64, total: u64, price: u64) -> (u64, u64) {
        let trader = trader_payout(long, ONE_SOL, 100 * USD, margin, total, price);
        (trader, total - trader)
    }

    #[test]
    fn flat_price_returns_both_margins() {
        assert_eq!(
            settle(true, 20 * USDC, 50 * USDC, 100 * USD),
            (20 * USDC, 30 * USDC)
        );
        assert_eq!(
            settle(false, 20 * USDC, 50 * USDC, 100 * USD),
            (20 * USDC, 30 * USDC)
        );
    }

    #[test]
    fn long_gains_when_price_rises() {
        // +$5 on 1 SOL.
        assert_eq!(
            settle(true, 20 * USDC, 50 * USDC, 105 * USD),
            (25 * USDC, 25 * USDC)
        );
    }

    #[test]
    fn long_loses_when_price_falls() {
        assert_eq!(
            settle(true, 20 * USDC, 50 * USDC, 95 * USD),
            (15 * USDC, 35 * USDC)
        );
    }

    #[test]
    fn short_mirrors_long() {
        assert_eq!(
            settle(false, 20 * USDC, 50 * USDC, 95 * USD),
            (25 * USDC, 25 * USDC)
        );
        assert_eq!(
            settle(false, 20 * USDC, 50 * USDC, 105 * USD),
            (15 * USDC, 35 * USDC)
        );
    }

    #[test]
    fn profit_is_capped_by_the_maker_margin() {
        // +$40 would pay 60, but the position only holds 50.
        assert_eq!(
            settle(true, 20 * USDC, 50 * USDC, 140 * USD),
            (50 * USDC, 0)
        );
        // Exactly at the cap.
        assert_eq!(
            settle(true, 20 * USDC, 50 * USDC, 130 * USD),
            (50 * USDC, 0)
        );
    }

    #[test]
    fn loss_is_capped_by_the_trader_margin() {
        // -$30 would leave -10.
        assert_eq!(settle(true, 20 * USDC, 50 * USDC, 70 * USD), (0, 50 * USDC));
        // Exactly wiped out.
        assert_eq!(settle(true, 20 * USDC, 50 * USDC, 80 * USD), (0, 50 * USDC));
        // A price of zero is an ordinary, very low price.
        assert_eq!(settle(true, 20 * USDC, 50 * USDC, 0), (0, 50 * USDC));
    }

    #[test]
    fn rounding_leaves_the_fraction_with_the_loser() {
        // 1 tick on 1 SOL is 0.01 base units: rounds to no movement.
        assert_eq!(
            settle(true, 20 * USDC, 50 * USDC, 100 * USD + 1),
            (20 * USDC, 30 * USDC)
        );
        assert_eq!(
            settle(true, 20 * USDC, 50 * USDC, 100 * USD - 1),
            (20 * USDC, 30 * USDC)
        );
        // 150 ticks is 1.5 base units: the winner gets 1.
        assert_eq!(
            settle(true, 20 * USDC, 50 * USDC, 100 * USD + 150),
            (20 * USDC + 1, 30 * USDC - 1)
        );
        assert_eq!(
            settle(true, 20 * USDC, 50 * USDC, 100 * USD - 150),
            (20 * USDC - 1, 30 * USDC + 1)
        );
    }

    /// The settlement the 64-bit code must reproduce, in 128-bit arithmetic.
    fn reference(long: bool, size: u64, entry: u64, margin: u64, total: u64, price: u64) -> u64 {
        let pnl = (size as u128) * (price.abs_diff(entry) as u128) / (SIZE_SCALE as u128);
        let (margin, total) = (margin as u128, total as u128);
        let payout = if (price >= entry) == long {
            (margin + pnl).min(total)
        } else {
            margin.saturating_sub(pnl)
        };
        payout as u64
    }

    #[test]
    fn mul_div_matches_128_bit_arithmetic() {
        let reference = |a: u64, b: u64| {
            let exact = (a as u128) * (b as u128) / (SIZE_SCALE as u128);
            u64::try_from(exact).unwrap_or(u64::MAX)
        };
        let edges = [
            0u64,
            1,
            2,
            SIZE_SCALE - 1,
            SIZE_SCALE,
            SIZE_SCALE + 1,
            (1 << 32) - 1,
            1 << 32,
            (1 << 32) + 1,
            (1 << 63) - 1,
            1 << 63,
            u64::MAX - 1,
            u64::MAX,
        ];
        for &a in &edges {
            for &b in &edges {
                assert_eq!(mul_div_scale(a, b), reference(a, b), "{a} * {b}");
            }
        }
        // A deterministic pseudo-random sweep over mixed magnitudes.
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..200_000 {
            let a = next() >> (next() % 64);
            let b = next() >> (next() % 64);
            assert_eq!(mul_div_scale(a, b), reference(a, b), "{a} * {b}");
        }
    }

    #[test]
    fn payout_matches_128_bit_reference() {
        let mut state = 0xD1B5_4A32_D192_ED03u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..200_000 {
            let size = next() >> (next() % 64);
            let entry = next() >> (next() % 64);
            let price = next() >> (next() % 64);
            let total = next() >> (next() % 64);
            // The circuit requires `trader_margin <= total`.
            let margin = if total == 0 {
                0
            } else {
                next() % (total + (total != u64::MAX) as u64)
            };
            for long in [true, false] {
                assert_eq!(
                    trader_payout(long, size, entry, margin, total, price),
                    reference(long, size, entry, margin, total, price),
                    "long={long} size={size} entry={entry} margin={margin} total={total} price={price}"
                );
            }
        }
    }

    #[test]
    fn extreme_values_do_not_overflow() {
        let total = u64::MAX;
        assert_eq!(
            trader_payout(true, u64::MAX, 0, u64::MAX / 2, total, u64::MAX),
            total
        );
        assert_eq!(
            trader_payout(false, u64::MAX, 0, u64::MAX / 2, total, u64::MAX),
            0
        );
        assert_eq!(
            trader_payout(true, u64::MAX, u64::MAX, u64::MAX / 2, total, 0),
            0
        );
        assert_eq!(trader_payout(true, 0, 5, 7, 9, u64::MAX), 7);
    }

    #[test]
    fn payout_never_exceeds_total_and_is_monotonic() {
        // A deterministic sweep: for a long the payout rises with the price,
        // for a short it falls, and it always stays within the position.
        let margins = [1u64, 7 * USDC, 20 * USDC, 50 * USDC];
        let sizes = [1u64, ONE_SOL / 1000, ONE_SOL, 250 * ONE_SOL];
        for &margin in &margins {
            for extra in [0u64, 1, 30 * USDC] {
                let total = margin + extra;
                for &size in &sizes {
                    for long in [true, false] {
                        let mut previous: Option<u64> = None;
                        for step in 0..=400u64 {
                            let price = step * USD / 2; // $0 to $200
                            let payout = trader_payout(long, size, 100 * USD, margin, total, price);
                            assert!(payout <= total);
                            if let Some(previous) = previous {
                                if long {
                                    assert!(payout >= previous);
                                } else {
                                    assert!(payout <= previous);
                                }
                            }
                            previous = Some(payout);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn liquidation_threshold() {
        // Long 1 SOL at $100 with 20 margin and 5 maintenance: liquidatable
        // once the price is $85 or lower.
        let maintenance = 5 * USDC;
        let at =
            |price: u64| is_liquidatable(settle(true, 20 * USDC, 50 * USDC, price).0, maintenance);
        assert!(!at(100 * USD));
        assert!(!at(85 * USD + 100));
        assert!(at(85 * USD));
        assert!(at(60 * USD));
        // The same position short is liquidatable at $115 or higher.
        let at_short =
            |price: u64| is_liquidatable(settle(false, 20 * USDC, 50 * USDC, price).0, maintenance);
        assert!(!at_short(115 * USD - 100));
        assert!(at_short(115 * USD));
    }
}
