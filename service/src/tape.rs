//! Price history: the candles the web app's chart draws, and each market's
//! last 24 hours for its header.
//!
//! The program settles at Pyth's price and nothing here changes that. Pyth's
//! own history needs a paid key since 2026, so the history comes from
//! Kraken's public market data, which lists every market the desk does
//! against the dollar and tracks the same prices to within a few cents. The
//! app draws these candles and moves the last one with the oracle price.
//!
//! The service fetches and the browsers ask the service, so a trader's
//! browser never talks to Kraken.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
    thread::sleep,
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use crate::{
    chain::{markets, Market},
    desk::Shared,
};

/// The candle widths the chart offers, in minutes: the ones Kraken serves.
pub const INTERVALS: [u32; 6] = [1, 5, 15, 60, 240, 1440];
/// How long a fetched series is served before it is fetched again.
const FRESH: Duration = Duration::from_secs(20);
/// How often every market's last 24 hours are read.
const DAY_EVERY: Duration = Duration::from_secs(90);
/// Kraken asks public callers to stay near one request a second.
const PAUSE: Duration = Duration::from_millis(1100);

fn http() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(12))
            .user_agent("darkring-desk")
            .build()
            .expect("an HTTP client")
    })
}

/// One candle: when it opened, in unix seconds, then open, high, low, close
/// in dollars.
type Candle = (i64, f64, f64, f64, f64);

/// `market`'s candles of `interval` minutes, oldest first: about 720 of them.
fn fetch(market: &Market, interval: u32) -> Result<Vec<Candle>> {
    let url = format!(
        "https://api.kraken.com/0/public/OHLC?pair={}USD&interval={interval}",
        market.symbol
    );
    let body: Value = http()
        .get(url)
        .send()
        .and_then(|response| response.json())
        .map_err(|_| anyhow!("the price history did not answer"))?;
    if let Some(error) = body["error"].as_array().and_then(|errors| errors.first()) {
        bail!("the price history refused {}: {error}", market.symbol);
    }
    // The result holds the pair under Kraken's own name for it, and `last`.
    let rows = body["result"]
        .as_object()
        .and_then(|result| result.iter().find(|(name, _)| name.as_str() != "last"))
        .and_then(|(_, rows)| rows.as_array())
        .ok_or_else(|| anyhow!("the price history has no {}", market.symbol))?;
    let number = |value: &Value| value.as_str().and_then(|text| text.parse::<f64>().ok());
    Ok(rows
        .iter()
        .filter_map(|row| {
            Some((
                row.get(0)?.as_i64()?,
                number(row.get(1)?)?,
                number(row.get(2)?)?,
                number(row.get(3)?)?,
                number(row.get(4)?)?,
            ))
        })
        .collect())
}

/// `market`'s candles as the app takes them, fetched at most once every
/// `FRESH` however many browsers ask.
pub fn candles(market: &'static Market, interval: u32) -> Result<Arc<Value>> {
    type Cache = Mutex<HashMap<(&'static str, u32), (Instant, Arc<Value>)>>;
    static CACHE: OnceLock<Cache> = OnceLock::new();
    if !INTERVALS.contains(&interval) {
        bail!("no such interval");
    }
    let cache = CACHE.get_or_init(Default::default);
    let key = (market.symbol, interval);
    // Held across the fetch: a second browser waits for the first's answer
    // instead of asking Kraken again.
    let mut cached = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((at, answer)) = cached.get(&key) {
        if at.elapsed() < FRESH {
            return Ok(Arc::clone(answer));
        }
    }
    let series = match fetch(market, interval) {
        Ok(series) => series,
        // A stale series is better than none.
        Err(error) => match cached.get(&key) {
            Some((_, answer)) => return Ok(Arc::clone(answer)),
            None => return Err(error),
        },
    };
    let answer = Arc::new(json!({
        "market": market.symbol,
        "interval": interval,
        "candles": series
            .iter()
            .map(|(time, open, high, low, close)| json!([time, open, high, low, close]))
            .collect::<Vec<_>>(),
    }));
    cached.insert(key, (Instant::now(), Arc::clone(&answer)));
    Ok(answer)
}

/// Keeps every market's last 24 hours on the desk: the price a day ago, and
/// the day's high and low. Runs for as long as the process does.
pub fn run(shared: Arc<Shared>) {
    loop {
        for market in markets() {
            match fetch(market, 60) {
                Ok(series) if !series.is_empty() => {
                    // The last 24 full hours and the one in progress.
                    let day = &series[series.len().saturating_sub(25)..];
                    let open = day[0].1;
                    let high = day.iter().map(|candle| candle.2).fold(f64::MIN, f64::max);
                    let low = day.iter().map(|candle| candle.3).fold(f64::MAX, f64::min);
                    let mut desk = shared.lock();
                    if let Some(view) = desk
                        .markets
                        .iter_mut()
                        .find(|view| view.symbol == market.symbol)
                    {
                        view.day = Some([open, high, low]);
                    }
                }
                Ok(_) => {}
                Err(error) => eprintln!("tape: {error:#}"),
            }
            sleep(PAUSE);
        }
        sleep(DAY_EVERY);
    }
}
