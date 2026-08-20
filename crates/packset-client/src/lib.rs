//! Loopback HTTP client for packsetd.
//!
//! Reads `PACKSET_URL` or `INSIDE_MEMORY_URL`. Does not open LMDB.

use serde::{Deserialize, Serialize};
use std::env;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(5);

fn path_seg(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for b in id.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("packset url missing")]
    NoUrl,
    #[error("http: {0}")]
    Http(#[from] Box<ureq::Error>),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("bad response: {0}")]
    Bad(String),
}

#[derive(Debug, Clone)]
pub struct PacksetClient {
    base: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hit {
    pub id: Option<String>,
    pub text: String,
    #[serde(default)]
    pub score: f64,
    #[serde(default)]
    pub kind: String,
}

impl PacksetClient {
    pub fn new(base: impl Into<String>) -> Self {
        let mut base = base.into();
        while base.ends_with('/') {
            base.pop();
        }
        Self { base }
    }

    pub fn from_env() -> Result<Self, Error> {
        let url = env::var("PACKSET_URL")
            .or_else(|_| env::var("INSIDE_MEMORY_URL"))
            .map_err(|_| Error::NoUrl)?;
        if url.is_empty() || url == "off" {
            return Err(Error::NoUrl);
        }
        Ok(Self::new(url))
    }

    pub fn health(&self) -> Result<String, Error> {
        let body = ureq::get(&format!("{}/health", self.base))
            .timeout(TIMEOUT)
            .call()
            .map_err(|e| Error::Http(Box::new(e)))?
            .into_string()?;
        Ok(body)
    }

    pub fn get_atom(&self, workspace: &str, id: &str) -> Result<serde_json::Value, Error> {
        let encoded = path_seg(id);
        let url = format!("{}/v1/atoms/{encoded}", self.base);
        let resp = ureq::get(&url)
            .query("workspace", workspace)
            .timeout(TIMEOUT)
            .call()
            .map_err(|e| Error::Http(Box::new(e)))?;
        if resp.status() == 404 {
            return Err(Error::Bad(format!("no atom {id}")));
        }
        Ok(resp.into_json()?)
    }

    pub fn search(&self, workspace: &str, q: &str, limit: u32) -> Result<Vec<Hit>, Error> {
        let url = format!("{}/v1/search", self.base);
        let body: serde_json::Value = ureq::get(&url)
            .query("workspace", workspace)
            .query("q", q)
            .query("limit", &limit.to_string())
            .timeout(TIMEOUT)
            .call()
            .map_err(|e| Error::Http(Box::new(e)))?
            .into_json()?;
        let hits = body
            .get("hits")
            .cloned()
            .unwrap_or(serde_json::Value::Array(vec![]));
        Ok(serde_json::from_value(hits)?)
    }

    pub fn post_atom(&self, atom: &serde_json::Value) -> Result<serde_json::Value, Error> {
        let url = format!("{}/v1/atoms", self.base);
        let body: serde_json::Value = ureq::post(&url)
            .timeout(TIMEOUT)
            .send_json(atom.clone())
            .map_err(|e| Error::Http(Box::new(e)))?
            .into_json()?;
        Ok(body)
    }
}
