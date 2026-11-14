//! The browser's road to Helius.
//!
//! A trader's browser reads its own notes from the Rings indexer and has
//! the Rings pool proof made by Helius's prover, which runs in an enclave
//! the browser attests and encrypts every request to. Both sit behind the
//! desk's Helius key, which must not reach a browser. So the browser sends
//! those requests here, with its session token, and this module passes them
//! on with the key added. What passes through is what the browser would
//! have sent Helius itself: indexer queries, and ciphertext only the
//! enclave can open.

use std::{sync::OnceLock, time::Duration};

use anyhow::{anyhow, bail, ensure, Result};
use serde_json::Value;

use crate::{chain::Config, usage};

/// The JSON-RPC methods a browser may call on the Solana node: reads the
/// Rings client makes. The desk sends every transaction itself.
const RPC_METHODS: [&str; 7] = [
    "getAccountInfo",
    "getMultipleAccounts",
    "getLatestBlockhash",
    "getSlot",
    "getBlockHeight",
    "getSignatureStatuses",
    "getProgramAccounts",
];

/// Request headers that are the browser's business with this process, or
/// the connection's, and do not go on.
const HELD_BACK: [&str; 16] = [
    "host",
    "authorization",
    "cookie",
    "connection",
    "content-length",
    "accept-encoding",
    "origin",
    "referer",
    "user-agent",
    "te",
    "upgrade",
    "transfer-encoding",
    "keep-alive",
    "proxy-authorization",
    "x-real-ip",
    "x-forwarded-for",
];

/// Response headers that describe Helius's connection, not the answer.
const NOT_RETURNED: [&str; 6] = [
    "connection",
    "transfer-encoding",
    "content-length",
    "content-encoding",
    "set-cookie",
    "keep-alive",
];

pub struct Answer {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

fn http() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .expect("an HTTP client")
    })
}

/// Where a request for `target`, the path and query after `/api`, goes.
/// The key rides in the query of the configured URLs.
fn upstream(config: &Config, method: &str, target: &str, body: &[u8]) -> Result<String> {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    ensure!(
        !query.contains("api-key") && !query.contains("api_key"),
        "no such route"
    );
    if path == "/rpc" {
        ensure!(method == "POST", "no such route");
        let request: Value =
            serde_json::from_slice(body).map_err(|_| anyhow!("the body is not JSON"))?;
        let calls = match &request {
            Value::Array(calls) => calls.iter().collect(),
            call => vec![call],
        };
        ensure!(!calls.is_empty() && calls.len() <= 16, "too many calls");
        for call in calls {
            let name = call.get("method").and_then(Value::as_str).unwrap_or("");
            if !RPC_METHODS.contains(&name) {
                bail!("the desk does not relay {name}");
            }
        }
        return Ok(config.rpc_url.clone());
    }
    let rest = path
        .strip_prefix("/rings")
        .filter(|rest| rest.is_empty() || rest.starts_with('/'))
        .ok_or_else(|| anyhow!("no such route"))?;
    ensure!(
        !rest.contains("..")
            && rest
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"/_-.".contains(&byte)),
        "no such route"
    );
    let (base, key) = config
        .spr_url
        .split_once('?')
        .unwrap_or((config.spr_url.as_str(), ""));
    let query = [key, query]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("&");
    Ok(format!("{}{rest}?{query}", base.trim_end_matches('/')))
}

/// Passes one request on and returns Helius's answer as it came.
pub fn relay(
    config: &Config,
    method: &str,
    target: &str,
    headers: &[(String, String)],
    body: Vec<u8>,
) -> Result<Answer> {
    let url = upstream(config, method, target, &body)?;
    // While the provider refuses the desk it refuses the browsers behind it too.
    ensure!(!usage::resting(), usage::REFUSED);
    let method = match method {
        "GET" => reqwest::Method::GET,
        "POST" => reqwest::Method::POST,
        _ => bail!("no such route"),
    };
    let mut request = http().request(method, url);
    for (name, value) in headers {
        if !HELD_BACK.contains(&name.to_ascii_lowercase().as_str()) {
            request = request.header(name, value);
        }
    }
    // An error here names the URL, and the URL holds the key.
    let response = request
        .body(body)
        .send()
        .map_err(|_| anyhow!("Helius did not answer"))?;
    let status = response.status().as_u16();
    if status == 429 {
        usage::rest();
    }
    let headers = response
        .headers()
        .iter()
        .filter(|(name, _)| !NOT_RETURNED.contains(&name.as_str()))
        .filter(|(name, _)| !name.as_str().starts_with("access-control-"))
        .filter_map(|(name, value)| Some((name.to_string(), value.to_str().ok()?.to_owned())))
        .collect();
    let body = response
        .bytes()
        .map_err(|_| anyhow!("Helius did not answer"))?
        .to_vec();
    Ok(Answer {
        status,
        headers,
        body,
    })
}
