//! A run's RPC answers on disk, so the run can be repeated with no network.
//!
//! A DIRECTORY, NOT ONE FILE: `index.json` maps each request (method + params, the key
//! `rpc::request_key` builds) to a file under `answers/`, and each answer is gzipped on
//! its own. One file per answer keeps every file far below the repository's 1 MB
//! added-file ceiling (a Sierra class is up to ~2.7 MB of JSON, ~0.5 MB gzipped), and
//! keeps a re-recorded transcript's diff to the answers that changed.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;

use eyre::{eyre, Context, Result};
use serde_json::{json, Value};

pub fn write(dir: &Path, endpoint: &str, answers: &BTreeMap<String, Value>) -> Result<usize> {
    let ans = dir.join("answers");
    std::fs::create_dir_all(&ans)?;
    let mut index = BTreeMap::new();
    for (i, (key, v)) in answers.iter().enumerate() {
        let name = format!("{i:05}.json.gz");
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        gz.write_all(serde_json::to_string(v)?.as_bytes())?;
        std::fs::write(ans.join(&name), gz.finish()?)?;
        index.insert(key.clone(), name);
    }
    // WHEN the answers were taken, because a replay's own clock says nothing about the
    // chain: a snapshot's `capturedAt` is the instant the data came off the node.
    let recorded_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let doc = json!({
        "format": "codetracer-cairo-onchain/rpc-transcript@1",
        "endpoint": endpoint,
        "recordedAtUnix": recorded_at,
        "answers": index,
    });
    std::fs::write(
        dir.join("index.json"),
        serde_json::to_string_pretty(&doc)? + "\n",
    )?;
    Ok(answers.len())
}

pub fn read(dir: &Path) -> Result<(String, BTreeMap<String, Value>)> {
    let idx_path = dir.join("index.json");
    let doc: Value = serde_json::from_str(
        &std::fs::read_to_string(&idx_path).with_context(|| format!("{}", idx_path.display()))?,
    )?;
    if doc["format"] != "codetracer-cairo-onchain/rpc-transcript@1" {
        return Err(eyre!(
            "{}: not a transcript this tool wrote ({})",
            idx_path.display(),
            doc["format"]
        ));
    }
    let mut out = BTreeMap::new();
    for (key, file) in doc["answers"].as_object().cloned().unwrap_or_default() {
        let f = file
            .as_str()
            .ok_or_else(|| eyre!("index entry for {key} is not a file name"))?;
        let bytes = std::fs::read(dir.join("answers").join(f))?;
        let mut s = String::new();
        flate2::read::GzDecoder::new(&bytes[..]).read_to_string(&mut s)?;
        out.insert(key, serde_json::from_str(&s)?);
    }
    Ok((
        doc["endpoint"].as_str().unwrap_or("transcript").to_string(),
        out,
    ))
}
