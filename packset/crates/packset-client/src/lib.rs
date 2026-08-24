//! Loopback HTTP client for packsetd.
//!
//! Reads `PACKSET_URL` or `INSIDE_MEMORY_URL`. search/get against packsetd; no SQLite.
//! Does not open LMDB.

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

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn workspace(&self) -> String {
        if let Ok(w) = env::var("PACKSET_WORKSPACE") {
            if !w.is_empty() {
                return w;
            }
        }
        let cwd = env::var("GROKOS_WORKSPACE")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from)
            .or_else(|| env::current_dir().ok())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        self.workspace_for_cwd(&cwd)
    }

    /// Workspace id from `/v1/identity` for `cwd`, or `dir:<abs>` if that call fails.
    pub fn workspace_for_cwd(&self, cwd: &std::path::Path) -> String {
        let abs = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let url = format!("{}/v1/identity", self.base);
        let body = ureq::get(&url)
            .query("cwd", abs.to_string_lossy().as_ref())
            .timeout(TIMEOUT)
            .call()
            .ok()
            .and_then(|r| r.into_string().ok());
        if let Some(body) = body {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&body) {
                if let Some(ws) = val.get("workspace").and_then(|v| v.as_str()) {
                    if !ws.is_empty() {
                        return ws.to_string();
                    }
                }
            }
        }
        format!("dir:{}", abs.display())
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
        let resp = match ureq::get(&url)
            .query("workspace", workspace)
            .timeout(TIMEOUT)
            .call()
        {
            Ok(resp) => resp,
            Err(ureq::Error::Status(404, _)) => {
                return Err(Error::Bad(format!("no atom {id}")));
            }
            Err(e) => return Err(Error::Http(Box::new(e))),
        };
        Ok(resp.into_json()?)
    }

    pub fn list_atoms(&self, workspace: &str) -> Result<Vec<serde_json::Value>, Error> {
        let url = format!("{}/v1/atoms", self.base);
        let body: serde_json::Value = ureq::get(&url)
            .query("workspace", workspace)
            .timeout(TIMEOUT)
            .call()
            .map_err(|e| Error::Http(Box::new(e)))?
            .into_json()?;
        let atoms = body
            .get("atoms")
            .cloned()
            .unwrap_or(serde_json::Value::Array(vec![]));
        Ok(serde_json::from_value(atoms)?)
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
