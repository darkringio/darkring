//! What a maker asks for a dated contract: a prediction's odds and the
//! premium of a call or a put.
//!
//! The model is the textbook one: the price moves as a lognormal with no
//! drift, at a volatility set per market below. The web app runs the same
//! arithmetic (`app/src/engine/options.ts`) to show a price before anyone
//! is asked for a quote, from the numbers [`params`] puts in the desk's
//! state, so the two have to change together. The cases at the bottom of
//! this file are the app's test cases too.
//!
//! None of it is on chain. The program holds what the two sides put in and
//! pays by the contract's terms; how a maker arrives at its terms is the
//! maker's business.

use serde_json::{json, Value};

/// The terms a contract can run for, in seconds.
pub const TERMS_SECS: [u64; 4] = [900, 3_600, 14_400, 86_400];
/// What a maker adds to the chance it works out for a prediction, as its
/// margin: three points.
const EDGE: f64 = 0.03;
/// The lowest and the highest chance a maker prices a prediction at: a
/// stake never pays more than about thirty-three times, or less than three
/// percent over itself.
const CHANCE_FLOOR: f64 = 0.03;
const CHANCE_CEIL: f64 = 0.97;
/// What a maker adds to the fair premium of a call or a put.
const MARKUP: f64 = 0.08;
/// How far past its strike a call or a put keeps paying: this many standard
/// deviations of the price over the contract's term, and at least
/// [`CAP_FLOOR`] of the price.
const CAP_SIGMAS: f64 = 3.0;
const CAP_FLOOR: f64 = 0.02;
/// How far from the price a strike may be.
pub const STRIKE_RANGE: f64 = 0.5;
/// The most one contract may pay, and the least it may cost, in dollars.
pub const MAX_PAYOUT_USD: f64 = 5_000.0;
pub const MIN_PREMIUM_USD: f64 = 1.0;

const YEAR_SECS: f64 = 31_557_600.0;

/// A market's volatility, as a standard deviation of its yearly return.
/// Round numbers near what each has shown; a maker that wanted to make
/// money would measure them.
pub fn volatility(symbol: &str) -> f64 {
    match symbol {
        "BTC" => 0.55,
        "ETH" => 0.70,
        "SOL" => 0.85,
        "HYPE" | "JUP" => 1.00,
        "RAY" | "PYTH" => 1.10,
        "BONK" => 1.20,
        "WIF" => 1.30,
        "PUMP" => 1.40,
        "FARTCOIN" => 1.60,
        _ => 1.00,
    }
}

/// The model's numbers, for the web app to price with.
pub fn params() -> Value {
    json!({
        "terms": TERMS_SECS,
        "edge": EDGE,
        "chanceFloor": CHANCE_FLOOR,
        "chanceCeil": CHANCE_CEIL,
        "markup": MARKUP,
        "capSigmas": CAP_SIGMAS,
        "capFloor": CAP_FLOOR,
        "strikeRange": STRIKE_RANGE,
        "maxPayout": MAX_PAYOUT_USD,
        "minPremium": MIN_PREMIUM_USD,
    })
}

/// The standard normal distribution function, by Abramowitz and Stegun's
/// formula 7.1.26 for the error function: good to seven decimal places.
pub fn normal_cdf(x: f64) -> f64 {
    let z = x.abs() / std::f64::consts::SQRT_2;
    let t = 1.0 / (1.0 + 0.327_591_1 * z);
    let poly = t
        * (0.254_829_592
            + t * (-0.284_496_736
                + t * (1.421_413_741 + t * (-1.453_152_027 + t * 1.061_405_429))));
    let erf = 1.0 - poly * (-z * z).exp();
    if x >= 0.0 {
        0.5 * (1.0 + erf)
    } else {
        0.5 * (1.0 - erf)
    }
}

/// The chance the price, now `spot`, ends at or above `strike` after
/// `seconds`, at `volatility`.
pub fn chance_above(spot: f64, strike: f64, seconds: f64, volatility: f64) -> f64 {
    let years = seconds.max(1.0) / YEAR_SECS;
    let spread = volatility * years.sqrt();
    let d2 = ((spot / strike).ln() - 0.5 * spread * spread) / spread;
    normal_cdf(d2)
}

/// The chance a maker prices a prediction at: the model's, plus its edge,
/// kept between the floor and the ceiling.
pub fn quoted_chance(spot: f64, strike: f64, seconds: f64, volatility: f64, above: bool) -> f64 {
    let up = chance_above(spot, strike, seconds, volatility);
    let chance = if above { up } else { 1.0 - up };
    (chance + EDGE).clamp(CHANCE_FLOOR, CHANCE_CEIL)
}

/// What a prediction pays in all for a `stake`, in base units: the stake
/// divided by the quoted chance.
pub fn prediction_total(stake: u64, chance: f64) -> u64 {
    (stake as f64 / chance).floor() as u64
}

/// The fair value of a call (or a put) on one unit, struck at `strike`.
fn vanilla(spot: f64, strike: f64, seconds: f64, volatility: f64, call: bool) -> f64 {
    let years = seconds.max(1.0) / YEAR_SECS;
    let spread = volatility * years.sqrt();
    let d1 = ((spot / strike).ln() + 0.5 * spread * spread) / spread;
    let d2 = d1 - spread;
    if call {
        spot * normal_cdf(d1) - strike * normal_cdf(d2)
    } else {
        strike * normal_cdf(-d2) - spot * normal_cdf(-d1)
    }
}

/// How far past its strike a call or a put keeps paying, in dollars of
/// price: where the maker's collateral runs out.
pub fn cap_distance(spot: f64, seconds: f64, volatility: f64) -> f64 {
    let years = seconds.max(1.0) / YEAR_SECS;
    spot * (CAP_SIGMAS * volatility * years.sqrt()).max(CAP_FLOOR)
}

/// The premium a maker asks for a call (or a put) on one unit, struck at
/// `strike` and paying no further than `cap` past it, in dollars: the fair
/// value of the capped contract, plus the markup.
pub fn premium_per_unit(
    spot: f64,
    strike: f64,
    cap: f64,
    seconds: f64,
    volatility: f64,
    call: bool,
) -> f64 {
    let far = if call {
        strike + cap
    } else {
        // A put cannot pay past a price of zero.
        (strike - cap).max(strike * 1e-6)
    };
    let fair = vanilla(spot, strike, seconds, volatility, call)
        - vanilla(spot, far, seconds, volatility, call);
    fair.max(0.0) * (1.0 + MARKUP)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(left: f64, right: f64, within: f64) -> bool {
        (left - right).abs() <= within
    }

    #[test]
    fn the_normal_distribution_function() {
        assert!(near(normal_cdf(0.0), 0.5, 1e-7));
        assert!(near(normal_cdf(1.0), 0.841_344_7, 1e-6));
        assert!(near(normal_cdf(-1.0), 0.158_655_3, 1e-6));
        assert!(near(normal_cdf(1.96), 0.975_002_1, 1e-6));
        assert!(near(normal_cdf(-3.0), 0.001_349_9, 1e-6));
        assert!(normal_cdf(9.0) <= 1.0 && normal_cdf(-9.0) >= 0.0);
    }

    // The app's `options.test.ts` holds these same cases.
    #[test]
    fn a_prediction_at_the_price_is_about_even() {
        // SOL at 100, an hour, 85% volatility.
        let up = chance_above(100.0, 100.0, 3_600.0, 0.85);
        assert!(near(up, 0.498_189, 1e-5), "{up}");
        assert!(near(
            quoted_chance(100.0, 100.0, 3_600.0, 0.85, true),
            up + 0.03,
            1e-9
        ));
        assert!(near(
            quoted_chance(100.0, 100.0, 3_600.0, 0.85, false),
            1.0 - up + 0.03,
            1e-9
        ));
    }

    #[test]
    fn a_far_strike_is_priced_at_the_floor_or_the_ceiling() {
        assert_eq!(quoted_chance(100.0, 140.0, 900.0, 0.85, true), 0.03);
        assert_eq!(quoted_chance(100.0, 140.0, 900.0, 0.85, false), 0.97);
        // A stake of 10 dollars at the floor pays 333.33 in all.
        assert_eq!(prediction_total(10_000_000, 0.03), 333_333_333);
        assert_eq!(prediction_total(10_000_000, 0.97), 10_309_278);
    }

    #[test]
    fn the_odds_move_with_the_strike_and_the_time() {
        let chance = |strike, seconds| chance_above(100.0, strike, seconds, 0.85);
        assert!(chance(101.0, 3_600.0) < chance(100.0, 3_600.0));
        assert!(chance(99.0, 3_600.0) > chance(100.0, 3_600.0));
        // More time brings a far strike closer to even.
        assert!(chance(105.0, 86_400.0) > chance(105.0, 900.0));
        assert!(near(chance(101.0, 3_600.0), 0.135_544, 1e-5));
    }

    #[test]
    fn a_call_and_a_put_at_the_price() {
        // BTC at 60,000, a day, 55% volatility: the cap is 3 standard
        // deviations of the day's move.
        let cap = cap_distance(60_000.0, 86_400.0, 0.55);
        assert!(near(cap, 5_180.12, 0.01), "{cap}");
        let call = premium_per_unit(60_000.0, 60_000.0, cap, 86_400.0, 0.55, true);
        let put = premium_per_unit(60_000.0, 60_000.0, cap, 86_400.0, 0.55, false);
        assert!(near(call, 742.81, 0.01), "{call}");
        assert!(near(put, 743.53, 0.01), "{put}");
        // A short term's cap is the floor: two percent of the price.
        assert!(near(cap_distance(100.0, 900.0, 0.85), 2.0, 1e-9));
    }

    #[test]
    fn a_premium_never_reaches_what_the_contract_can_pay_at_the_price() {
        for (spot, strike, seconds, volatility) in [
            (100.0, 100.0, 900.0, 0.85),
            (100.0, 103.0, 3_600.0, 0.85),
            (100.0, 97.0, 14_400.0, 1.6),
            (60_000.0, 61_000.0, 86_400.0, 0.55),
        ] {
            let cap = cap_distance(spot, seconds, volatility);
            for call in [true, false] {
                let premium = premium_per_unit(spot, strike, cap, seconds, volatility, call);
                assert!(premium > 0.0 && premium < cap, "{premium} of {cap}");
            }
        }
    }
}
