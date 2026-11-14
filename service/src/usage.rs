//! Staying inside what the RPC provider allows, and keeping its address out
//! of what a browser is shown.
//!
//! The desk reaches Solana and the Rings indexer through one provider, on a
//! key with a usage limit. Three things here keep that from going wrong:
//!
//! - **What an error may say.** The provider's URL carries the key, and the
//!   HTTP client names the URL in its errors. [`shown`] is the only way an
//!   error reaches a browser: it drops every URL, and says in plain words
//!   when the provider is refusing.
//! - **Resting when refused.** Once the provider answers that the limit is
//!   reached, asking again every few seconds helps nobody. [`note`] starts
//!   a rest that doubles up to five minutes; [`resting`] says when to hold
//!   off; [`recovered`] ends it at the first answer that works.
//! - **Asking less when nobody is here.** [`pace`] is how often the oracle
//!   prices are read: often while somebody trades, seldom while the desk is
//!   empty.

use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        OnceLock,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// What a browser is told while the provider refuses the desk.
pub const REFUSED: &str = "The desk can’t reach devnet right now: its RPC provider says the usage limit is reached. Nothing was sent, and your balance and positions are untouched. Try again later.";

/// The first rest after a refusal, and the longest, in seconds.
const FIRST_REST_SECS: u64 = 30;
const LONGEST_REST_SECS: u64 = 300;

/// How often the oracle prices are read while an account is connected,
/// while somebody is only looking, while positions are open with nobody
/// looking, and while the desk is empty.
const PACE_TRADING: Duration = Duration::from_secs(3);
const PACE_WATCHED: Duration = Duration::from_secs(6);
const PACE_HOLDING: Duration = Duration::from_secs(30);
const PACE_EMPTY: Duration = Duration::from_secs(90);

/// The provider key, so it can be struck from anything that still holds it.
static KEY: OnceLock<String> = OnceLock::new();
/// When the current rest ends, in unix milliseconds, and how long it is.
static REST_UNTIL: AtomicU64 = AtomicU64::new(0);
static REST_SECS: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// Remembers the provider key, to strike it from any text on its way out.
pub fn keep_secret(key: &str) {
    if key.len() >= 8 {
        let _ = KEY.set(key.to_owned());
    }
}

/// Whether `text` is the provider saying the desk has asked too much.
pub fn refused(text: &str) -> bool {
    text.contains("429") || text.contains("Too Many Requests") || text.contains("max usage reached")
}

/// `text` with every URL cut down to its host, and the key struck out.
pub fn scrub(text: &str) -> String {
    let mut clean = String::with_capacity(text.len());
    let mut rest = text;
    // Whichever scheme comes first in what is left.
    let next = |text: &str| match (text.find("http://"), text.find("https://")) {
        (Some(plain), Some(secure)) => Some(plain.min(secure)),
        (plain, secure) => plain.or(secure),
    };
    while let Some(start) = next(rest) {
        clean.push_str(&rest[..start]);
        let url = &rest[start..];
        let end = url
            .find(|c: char| c.is_whitespace() || matches!(c, ')' | '"' | '\'' | '>' | ','))
            .unwrap_or(url.len());
        let after_scheme = url.find("://").map_or(0, |at| at + 3);
        let host_end = url[after_scheme..end]
            .find(['/', '?', '#'])
            .map_or(end, |at| after_scheme + at);
        clean.push_str(&url[..host_end]);
        rest = &url[end..];
    }
    clean.push_str(rest);
    match KEY.get() {
        Some(key) => clean.replace(key.as_str(), "REDACTED"),
        None => clean,
    }
}

/// `error` as a browser may be shown it.
pub fn shown(error: &anyhow::Error) -> String {
    shown_text(&format!("{error:#}"))
}

/// `text`, an error already written out, as a browser may be shown it.
pub fn shown_text(text: &str) -> String {
    if refused(text) {
        REFUSED.to_owned()
    } else {
        scrub(text)
    }
}

/// Looks at an error from the provider and starts a rest if it is a refusal.
pub fn note(error: &anyhow::Error) {
    if refused(&format!("{error:#}")) {
        rest();
    }
}

/// Starts a rest, twice as long as the last one, up to the longest.
pub fn rest() {
    let now = now_ms();
    // One refusal is seen by several threads at once: one rest covers them.
    if REST_UNTIL.load(Ordering::Relaxed) > now {
        return;
    }
    let last = REST_SECS.load(Ordering::Relaxed);
    let next = (last * 2).clamp(FIRST_REST_SECS, LONGEST_REST_SECS);
    REST_SECS.store(next, Ordering::Relaxed);
    REST_UNTIL.store(now + next * 1000, Ordering::Relaxed);
    eprintln!("usage: the provider is refusing requests; holding off for {next} s");
}

/// Whether the provider's last word was a refusal. Between rests the desk
/// asks again, so this stays true until an answer comes back.
pub fn refusing() -> bool {
    REST_SECS.load(Ordering::Relaxed) != 0
}

/// Whether the desk is holding off from the provider.
pub fn resting() -> bool {
    REST_UNTIL.load(Ordering::Relaxed) > now_ms()
}

/// How long the current rest still has to run.
pub fn rest_left() -> Duration {
    Duration::from_millis(REST_UNTIL.load(Ordering::Relaxed).saturating_sub(now_ms()))
}

/// The provider answered: the next refusal starts from a short rest again.
pub fn recovered() {
    if REST_SECS.swap(0, Ordering::Relaxed) != 0 {
        REST_UNTIL.store(0, Ordering::Relaxed);
        println!("usage: the provider is answering again");
    }
}

/// How often to read the oracle prices, given who needs them.
pub fn pace(trading: bool, watched: bool, holding: bool) -> Duration {
    if trading {
        PACE_TRADING
    } else if watched {
        PACE_WATCHED
    } else if holding {
        PACE_HOLDING
    } else {
        PACE_EMPTY
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_url_keeps_its_host_and_loses_the_rest() {
        let text = "rpc error: get_balance Abc: HTTP status client error (500) for url (https://devnet.example-rpc.com/?api-key=0000-secret)";
        let clean = scrub(text);
        assert!(!clean.contains("secret"));
        assert!(!clean.contains("api-key"));
        assert!(clean.contains("for url (https://devnet.example-rpc.com)"));
        assert_eq!(
            scrub("see http://a.example/x?y=1 and https://b.example/v1/spr?k=2, then stop"),
            "see http://a.example and https://b.example, then stop"
        );
        assert_eq!(scrub("nothing to hide"), "nothing to hide");
        assert_eq!(
            scrub("ends in https://c.example"),
            "ends in https://c.example"
        );
    }

    #[test]
    fn a_refusal_is_said_in_plain_words() {
        let error = anyhow::anyhow!(
            "sync wallet: IndexerUnavailable(\"HTTP response error 429 Too Many Requests: max usage reached\")"
        );
        assert_eq!(shown(&error), REFUSED);
        assert!(refused("HTTP status client error (429 Too Many Requests) for url (https://x.example/?api-key=k)"));
        assert!(!refused("the quote has expired"));
        assert_eq!(
            shown(&anyhow::anyhow!("the quote has expired")),
            "the quote has expired"
        );
    }

    #[test]
    fn the_prices_are_read_less_often_the_emptier_the_desk() {
        assert_eq!(pace(true, true, true), PACE_TRADING);
        assert_eq!(pace(false, true, true), PACE_WATCHED);
        assert_eq!(pace(false, false, true), PACE_HOLDING);
        assert_eq!(pace(false, false, false), PACE_EMPTY);
        assert!(
            PACE_TRADING < PACE_WATCHED && PACE_WATCHED < PACE_HOLDING && PACE_HOLDING < PACE_EMPTY
        );
    }

    #[test]
    fn a_rest_doubles_and_ends_when_the_provider_answers() {
        recovered();
        assert!(!resting());
        rest();
        assert!(resting() && refusing());
        assert_eq!(REST_SECS.load(Ordering::Relaxed), FIRST_REST_SECS);
        // A second refusal inside the rest does not lengthen it.
        rest();
        assert_eq!(REST_SECS.load(Ordering::Relaxed), FIRST_REST_SECS);
        // Once it has run out the desk asks again, and is still refused
        // until an answer says otherwise. The next rest is twice as long.
        REST_UNTIL.store(0, Ordering::Relaxed);
        assert!(!resting() && refusing());
        rest();
        assert_eq!(REST_SECS.load(Ordering::Relaxed), FIRST_REST_SECS * 2);
        recovered();
        assert!(!resting() && !refusing());
        assert_eq!(REST_SECS.load(Ordering::Relaxed), 0);
    }
}
