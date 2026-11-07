//! A blocking Starknet JSON-RPC client, and the one place this crate talks to a node.
//!
//! Every answer is classified rather than collapsed: `absent` (the method does not
//! exist, `-32601`), `policy` (rate limits, allowlists — retried), and the Starknet
//! spec's own not-found codes, which are ANSWERS (a contract with no class at a block
//! is a fact about the chain, not a failure of the endpoint).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::time::Duration;

use eyre::{bail, eyre, Result};
use serde_json::{json, Value};

/// Starknet spec error codes this crate distinguishes.
pub const CONTRACT_NOT_FOUND: i64 = 20;
pub const CLASS_HASH_NOT_FOUND: i64 = 28;

#[derive(Debug, Clone)]
pub enum Answer {
    Ok(Value),
    /// A JSON-RPC error object the node returned, kept whole.
    Error {
        code: i64,
        message: String,
        data: Option<Value>,
    },
}

pub struct Rpc {
    url: String,
    agent: ureq::Agent,
    retries: u32,
    /// Every request this run made and the answer it got, in order, so the run can be
    /// written out as a transcript and replayed offline. Keyed by the request's
    /// canonical text (method + params), which is what a replay looks answers up by.
    transcript: RefCell<BTreeMap<String, Value>>,
    /// When set, answers come from here and the network is never touched; a request
    /// the transcript does not hold is refused by name.
    replay: Option<BTreeMap<String, Value>>,
    pub calls: RefCell<u64>,
}

pub fn request_key(method: &str, params: &Value) -> String {
    format!(
        "{method} {}",
        serde_json::to_string(params).unwrap_or_default()
    )
}

impl Rpc {
    pub fn new(url: &str) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(180))
            .build();
        Self {
            url: url.to_string(),
            agent,
            retries: 12,
            transcript: RefCell::new(BTreeMap::new()),
            replay: None,
            calls: RefCell::new(0),
        }
    }

    pub fn replaying(url_label: &str, transcript: BTreeMap<String, Value>) -> Self {
        let mut rpc = Self::new(url_label);
        rpc.replay = Some(transcript);
        rpc
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn transcript(&self) -> BTreeMap<String, Value> {
        self.transcript.borrow().clone()
    }

    /// One call; the whole JSON-RPC envelope's `result` or `error`.
    pub fn answer(&self, method: &str, params: Value) -> Result<Answer> {
        *self.calls.borrow_mut() += 1;
        let key = request_key(method, &params);
        let body = if let Some(replay) = &self.replay {
            match replay.get(&key) {
                Some(v) => v.clone(),
                None => bail!(
                    "the transcript holds no answer for {key}; an offline run cannot ask \
                     the network, so this request is refused rather than guessed"
                ),
            }
        } else {
            let mut last_err = String::new();
            let mut got = None;
            let started = std::time::Instant::now();
            for attempt in 0..=self.retries {
                let req = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
                match self.agent.post(&self.url).send_json(req) {
                    Ok(resp) => {
                        let v: Value = resp.into_json()?;
                        got = Some(v);
                        break;
                    }
                    // POLICY refusals (429/503/…) are retried with backoff; an HTTP error
                    // whose body is a JSON-RPC error is an answer and is returned.
                    Err(ureq::Error::Status(code, resp)) => {
                        let text = resp.into_string().unwrap_or_default();
                        if let Ok(v) = serde_json::from_str::<Value>(&text) {
                            if v.get("error").is_some() && code != 429 {
                                got = Some(v);
                                break;
                            }
                        }
                        last_err = format!("HTTP {code}: {}", &text[..text.len().min(200)]);
                    }
                    Err(e) => last_err = e.to_string(),
                }
                std::thread::sleep(Duration::from_millis(500 * (attempt as u64 + 1)));
            }
            if std::env::var_os("CODETRACER_CAIRO_ONCHAIN_LOG_RPC").is_some() {
                eprintln!(
                    "rpc {:>6}ms {}",
                    started.elapsed().as_millis(),
                    &key[..key.len().min(160)]
                );
            }
            let v = got.ok_or_else(|| {
                eyre!(
                    "{method} was not answered by {} after retries: {last_err}",
                    self.url
                )
            })?;
            // Only the result/error member is kept; the id and envelope are this run's.
            let kept = match (v.get("result"), v.get("error")) {
                (Some(r), _) => json!({ "result": r }),
                (None, Some(e)) => json!({ "error": e }),
                _ => bail!("{method}: the answer carries neither a result nor an error: {v}"),
            };
            self.transcript.borrow_mut().insert(key, kept.clone());
            kept
        };
        if let Some(r) = body.get("result") {
            return Ok(Answer::Ok(r.clone()));
        }
        let e = &body["error"];
        Ok(Answer::Error {
            code: e["code"].as_i64().unwrap_or(0),
            message: e["message"].as_str().unwrap_or("").to_string(),
            data: e.get("data").cloned(),
        })
    }

    /// One call whose error is a refusal.
    pub fn call(&self, method: &str, params: Value) -> Result<Value> {
        match self.answer(method, params.clone())? {
            Answer::Ok(v) => Ok(v),
            Answer::Error {
                code,
                message,
                data,
            } => bail!(
                "{method} {params} was refused by {}: code {code} {message} {}",
                self.url,
                data.map(|d| d.to_string()).unwrap_or_default()
            ),
        }
    }
}
