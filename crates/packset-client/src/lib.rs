//! Loopback HTTP client for packsetd.
//!
//! Reads `PACKSET_URL` or `INSIDE_MEMORY_URL`. search/get against packsetd; no SQLite.
//! Does not open LMDB.

use serde::{Deserialize, Serialize};
use std::env;
use std::time::Duration;

/// How long one request may take: `PACKSET_TIMEOUT_MS`, else thirty seconds.
/// A write that waits behind thirty others on a busy seat is late, not failed.
fn timeout() -> Duration {
    std::env::var("PACKSET_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map_or(Duration::from_secs(30), Duration::from_millis)
}

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

/// The port a writer listens on when nothing names one. The command line,
/// the server and this client agree on it, so a seat needs no variable set.
pub const DEFAULT_PORT: u16 = 8761;

/// `PACKSET_PORT` (`GROK_MEM_PORT` is an alias), else [`DEFAULT_PORT`].
#[must_use]
pub fn default_port() -> u16 {
    env::var("PACKSET_PORT")
        .or_else(|_| env::var("GROK_MEM_PORT"))
        .ok()
        .and_then(|raw| raw.trim().parse().ok())
        .unwrap_or(DEFAULT_PORT)
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
    /// A workspace pinned by the caller; `None` reads the environment and
    /// the working directory.
    workspace: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hit {
    pub id: Option<String>,
    pub text: String,
    #[serde(default)]
    pub score: f64,
    #[serde(default)]
    pub kind: String,
    /// When the memory was written, the writer's clock, RFC 3339. Absent
    /// on card paragraphs, which have no clock.
    #[serde(default)]
    pub ts: Option<String>,
    /// How many of the panel's ballots named this hit, and how many ran.
    /// Two of three is agreement; one of three is one scorer's opinion.
    #[serde(default)]
    pub ballots: Option<u32>,
    #[serde(default)]
    pub of: Option<u32>,
}

/// A refusal, carrying the reason the writer gave in its body.
fn refused(url: &str, e: ureq::Error) -> Error {
    match e {
        ureq::Error::Status(code, response) => {
            let text = response.into_string().unwrap_or_default();
            let reason = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v.get("error").and_then(|r| r.as_str()).map(str::to_string))
                .unwrap_or(text);
            let reason = reason.trim();
            if reason.is_empty() {
                Error::Bad(format!("{url}: status code {code}"))
            } else {
                Error::Bad(format!("{url}: {code}: {reason}"))
            }
        }
        other => Error::Http(Box::new(other)),
    }
}

impl PacksetClient {
    pub fn new(base: impl Into<String>) -> Self {
        let mut base = base.into();
        while base.ends_with('/') {
            base.pop();
        }
        Self {
            base,
            workspace: None,
        }
    }

    /// Pin the workspace this client speaks for, ahead of `PACKSET_WORKSPACE`
    /// and the working directory. A seat that is one memory across every
    /// repository it works in sets this once.
    #[must_use]
    pub fn with_workspace(mut self, workspace: impl Into<String>) -> Self {
        let workspace = workspace.into();
        self.workspace = (!workspace.is_empty()).then_some(workspace);
        self
    }

    /// The writer the seat talks to, with nothing set: `PACKSET_URL`
    /// (`INSIDE_MEMORY_URL` is an alias), else the loopback port the command
    /// line starts a writer on, `PACKSET_PORT` (`GROK_MEM_PORT`) or 8761.
    /// `PACKSET_URL=off` is the one way to have no pack.
    pub fn from_env() -> Result<Self, Error> {
        let url = env::var("PACKSET_URL")
            .or_else(|_| env::var("INSIDE_MEMORY_URL"))
            .ok()
            .filter(|url| !url.is_empty());
        match url {
            Some(url) if url == "off" => Err(Error::NoUrl),
            Some(url) => Ok(Self::new(url)),
            None => Ok(Self::new(format!("http://127.0.0.1:{}", default_port()))),
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn workspace(&self) -> String {
        if let Some(w) = &self.workspace {
            return w.clone();
        }
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
            .timeout(timeout())
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
        let url = format!("{}/health", self.base);
        let body = ureq::get(&url)
            .timeout(timeout())
            .call()
            .map_err(|e| refused(&url, e))?
            .into_string()?;
        Ok(body)
    }

    pub fn get_atom(&self, workspace: &str, id: &str) -> Result<serde_json::Value, Error> {
        let encoded = path_seg(id);
        let url = format!("{}/v1/atoms/{encoded}", self.base);
        let resp = match ureq::get(&url)
            .query("workspace", workspace)
            .timeout(timeout())
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
        self.atoms_as_of(workspace, None)
    }

    /// Live-now atoms, or the ones that were live at `as_of`.
    ///
    /// # Errors
    ///
    /// The request's, or a body that is not JSON.
    pub fn atoms_as_of(
        &self,
        workspace: &str,
        as_of: Option<&str>,
    ) -> Result<Vec<serde_json::Value>, Error> {
        let url = format!("{}/v1/atoms", self.base);
        let mut req = ureq::get(&url)
            .query("workspace", workspace)
            .timeout(timeout());
        if let Some(at) = as_of {
            req = req.query("as_of", at);
        }
        let body: serde_json::Value = req.call().map_err(|e| refused(&url, e))?.into_json()?;
        let atoms = body
            .get("atoms")
            .cloned()
            .unwrap_or(serde_json::Value::Array(vec![]));
        Ok(serde_json::from_value(atoms)?)
    }

    pub fn search(&self, workspace: &str, q: &str, limit: u32) -> Result<Vec<Hit>, Error> {
        self.search_as_of(workspace, q, limit, None)
    }

    /// Ranked hits, optionally over the atoms that were live at `as_of`.
    ///
    /// # Errors
    ///
    /// The request's, or a body that is not JSON.
    pub fn search_as_of(
        &self,
        workspace: &str,
        q: &str,
        limit: u32,
        as_of: Option<&str>,
    ) -> Result<Vec<Hit>, Error> {
        self.search_opts(workspace, q, limit, as_of, false)
    }

    /// Ranked hits, optionally dated and optionally through the measured
    /// cross-encoder stage.
    ///
    /// Off by default. On, the writer spends a forward pass per candidate and
    /// the request waits for that rather than the usual five-second budget.
    ///
    /// # Errors
    ///
    /// The request's, or a body that is not a hit list.
    pub fn search_opts(
        &self,
        workspace: &str,
        q: &str,
        limit: u32,
        as_of: Option<&str>,
        rerank: bool,
    ) -> Result<Vec<Hit>, Error> {
        let url = format!("{}/v1/search", self.base);
        let budget = if rerank {
            Duration::from_secs(60).max(timeout())
        } else {
            timeout()
        };
        let mut req = ureq::get(&url)
            .query("workspace", workspace)
            .query("q", q)
            .query("limit", &limit.to_string())
            .timeout(budget);
        if let Some(at) = as_of {
            req = req.query("as_of", at);
        }
        if rerank {
            req = req.query("rerank", "1");
        }
        let body: serde_json::Value = req.call().map_err(|e| refused(&url, e))?.into_json()?;
        let hits = body
            .get("hits")
            .cloned()
            .unwrap_or(serde_json::Value::Array(vec![]));
        Ok(serde_json::from_value(hits)?)
    }

    /// Seat home, atom counts by kind, pin, index and embedder.
    ///
    /// # Errors
    ///
    /// The request's, or a body that is not JSON.
    pub fn status(&self, workspace: Option<&str>) -> Result<serde_json::Value, Error> {
        let url = format!("{}/v1/status", self.base);
        let mut req = ureq::get(&url).timeout(timeout());
        if let Some(workspace) = workspace {
            req = req.query("workspace", workspace);
        }
        Ok(req.call().map_err(|e| refused(&url, e))?.into_json()?)
    }

    /// The set a workspace is pinned to.
    ///
    /// # Errors
    ///
    /// The request's, or a body that is not JSON.
    pub fn pin(&self, workspace: &str) -> Result<serde_json::Value, Error> {
        let url = format!("{}/v1/pin", self.base);
        Ok(ureq::get(&url)
            .query("workspace", workspace)
            .timeout(timeout())
            .call()
            .map_err(|e| refused(&url, e))?
            .into_json()?)
    }

    /// Pin a workspace to a set.
    ///
    /// # Errors
    ///
    /// The request's, or a body that is not JSON.
    pub fn set_pin(&self, workspace: &str, name: &str) -> Result<serde_json::Value, Error> {
        let url = format!("{}/v1/pin", self.base);
        Ok(ureq::put(&url)
            .timeout(timeout())
            .send_json(serde_json::json!({ "workspace": workspace, "name": name }))
            .map_err(|e| refused(&url, e))?
            .into_json()?)
    }

    /// The deed accessions a workspace's live atoms cite, sorted.
    ///
    /// The accession is the only identifier crossing the tracker, the pack and
    /// the deed store, so this is what `deedar evidence -` reads.
    ///
    /// # Errors
    ///
    /// The request's, or a body that is not JSON.
    pub fn accessions(&self, workspace: &str) -> Result<Vec<String>, Error> {
        let url = format!("{}/v1/accessions", self.base);
        let body: serde_json::Value = ureq::get(&url)
            .query("workspace", workspace)
            .timeout(timeout())
            .call()
            .map_err(|e| refused(&url, e))?
            .into_json()?;
        let found = body
            .get("accessions")
            .cloned()
            .unwrap_or(serde_json::Value::Array(vec![]));
        Ok(serde_json::from_value(found)?)
    }
    /// Every live atom in a workspace.
    ///
    /// The bodies, not the join keys: this is what a handover carries when
    /// somebody is given what the seat learned rather than only what it cites.
    ///
    /// # Errors
    ///
    /// The request's, or a body that is not JSON.
    pub fn atoms(&self, workspace: &str) -> Result<Vec<serde_json::Value>, Error> {
        self.atoms_as_of(workspace, None)
    }

    /// The live atoms in a workspace that cite one deed accession.
    ///
    /// # Errors
    ///
    /// The request's, or a body that is not JSON.
    pub fn citers(
        &self,
        workspace: &str,
        accession: &str,
    ) -> Result<Vec<serde_json::Value>, Error> {
        let url = format!("{}/v1/citers", self.base);
        let body: serde_json::Value = ureq::get(&url)
            .query("workspace", workspace)
            .query("accession", accession)
            .timeout(timeout())
            .call()
            .map_err(|e| refused(&url, e))?
            .into_json()?;
        let found = body
            .get("atoms")
            .cloned()
            .unwrap_or(serde_json::Value::Array(vec![]));
        Ok(serde_json::from_value(found)?)
    }

    /// Tombstone one atom. The daemon keeps the record and drops the index
    /// entry, so a forgotten atom stops being recalled without the pack losing
    /// the fact that it once held it.
    ///
    /// `why` is the deed accession that withdrew the claim, and the daemon
    /// refuses one that is not an accession. It rides onto the tombstone beside
    /// the text, so the retraction and what it retracted read back together.
    ///
    /// # Errors
    ///
    /// [`Error::Bad`] when the workspace does not hold that atom, else the
    /// request's or a body that is not JSON.
    pub fn delete_atom(
        &self,
        workspace: &str,
        id: &str,
        why: Option<&str>,
    ) -> Result<serde_json::Value, Error> {
        let url = format!("{}/v1/atoms/delete", self.base);
        let mut body = serde_json::json!({
            "workspace": workspace,
            "id": id,
        });
        if let Some(accession) = why {
            body["why"] = serde_json::Value::String(accession.to_string());
        }
        let resp = match ureq::post(&url).timeout(timeout()).send_json(body) {
            Ok(resp) => resp,
            Err(ureq::Error::Status(404, _)) => {
                return Err(Error::Bad(format!("no atom {id}")));
            }
            Err(e) => return Err(refused(&url, e)),
        };
        Ok(resp.into_json()?)
    }

    /// Move one atom along the review clock: recalled, or lapsed.
    pub fn grade(
        &self,
        workspace: &str,
        id: &str,
        recalled: bool,
    ) -> Result<serde_json::Value, Error> {
        let url = format!("{}/v1/grade", self.base);
        let body: serde_json::Value = ureq::post(&url)
            .timeout(timeout())
            .send_json(serde_json::json!({
                "workspace": workspace,
                "id": id,
                "recalled": recalled,
            }))
            .map_err(|e| refused(&url, e))?
            .into_json()?;
        Ok(body)
    }

    /// The link graph's communities, largest first.
    /// The claims the link graph turns on, highest first.
    pub fn hubs(&self, workspace: &str, limit: usize) -> Result<serde_json::Value, Error> {
        let url = format!("{}/v1/hubs", self.base);
        let body: serde_json::Value = ureq::get(&url)
            .query("workspace", workspace)
            .query("limit", &limit.to_string())
            .timeout(timeout())
            .call()
            .map_err(|e| refused(&url, e))?
            .into_json()?;
        Ok(body)
    }

    pub fn islands(&self, workspace: &str) -> Result<serde_json::Value, Error> {
        let url = format!("{}/v1/islands", self.base);
        let body: serde_json::Value = ureq::get(&url)
            .query("workspace", workspace)
            .timeout(timeout())
            .call()
            .map_err(|e| refused(&url, e))?
            .into_json()?;
        Ok(body)
    }

    /// Claims that fired together: their links gain weight.
    pub fn fire(&self, workspace: &str, ids: &[String]) -> Result<serde_json::Value, Error> {
        let url = format!("{}/v1/fire", self.base);
        let body: serde_json::Value = ureq::post(&url)
            .timeout(timeout())
            .send_json(serde_json::json!({"workspace": workspace, "ids": ids}))
            .map_err(|e| refused(&url, e))?
            .into_json()?;
        Ok(body)
    }

    /// Consolidate the workspace: every claim that replaces an earlier one
    /// closes it (the write-time rule, run over what is held). `apply`
    /// false reports the pairs and writes nothing.
    pub fn consolidate(&self, workspace: &str, apply: bool) -> Result<serde_json::Value, Error> {
        let url = format!("{}/v1/consolidate", self.base);
        let body: serde_json::Value = ureq::post(&url)
            .timeout(timeout())
            .send_json(serde_json::json!({"workspace": workspace, "apply": apply}))
            .map_err(|e| refused(&url, e))?
            .into_json()?;
        Ok(body)
    }

    /// The memories a cue activates, strongest first; with `fire`, the top
    /// of them fire together.
    pub fn activate(
        &self,
        workspace: &str,
        q: &str,
        limit: u32,
        fire: bool,
    ) -> Result<serde_json::Value, Error> {
        let url = format!("{}/v1/activate", self.base);
        let body: serde_json::Value = ureq::get(&url)
            .query("workspace", workspace)
            .query("q", q)
            .query("limit", &limit.to_string())
            .query("fire", if fire { "1" } else { "0" })
            .timeout(timeout())
            .call()
            .map_err(|e| refused(&url, e))?
            .into_json()?;
        Ok(body)
    }

    pub fn post_atom(&self, atom: &serde_json::Value) -> Result<serde_json::Value, Error> {
        let url = format!("{}/v1/atoms", self.base);
        let body: serde_json::Value = ureq::post(&url)
            .timeout(timeout())
            .send_json(atom.clone())
            .map_err(|e| refused(&url, e))?
            .into_json()?;
        Ok(body)
    }
}
