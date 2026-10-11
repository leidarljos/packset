//! Loopback HTTP client for packsetd.
//!
//! Reads `PACKSET_URL` or `INSIDE_MEMORY_URL`. search/get against packsetd; no SQLite.
//! Does not open LMDB.

use serde::{Deserialize, Serialize};
use std::env;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How long one request may take: `PACKSET_TIMEOUT_MS`, else thirty seconds.
/// A write that waits behind thirty others on a busy seat is late, not failed.
fn timeout() -> Duration {
    std::env::var("PACKSET_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map_or(Duration::from_secs(30), Duration::from_millis)
}

/// The wait before a busy writer is asked again, tripled each time.
const BUSY_WAIT: Duration = Duration::from_millis(50);
/// How many times to ask again.
const BUSY_RETRIES: u32 = 3;

/// Send `req`, with `body` when there is one, inside `budget`, and send it
/// again when the writer answers busy. The writer answers 503 only when its
/// queue or its lane is full, before any of the request runs, so a write
/// sent again is not stored twice.
fn send(
    req: ureq::Request,
    body: Option<&serde_json::Value>,
    budget: Duration,
) -> Result<ureq::Response, Box<ureq::Error>> {
    let started = Instant::now();
    let mut wait = BUSY_WAIT;
    let mut retries = 0;
    loop {
        let left = budget
            .saturating_sub(started.elapsed())
            .max(Duration::from_millis(1));
        let attempt = req.clone().timeout(left);
        let answered = match body {
            Some(json) => attempt.send_json(json),
            None => attempt.call(),
        };
        match answered {
            Err(ureq::Error::Status(503, _))
                if retries < BUSY_RETRIES && started.elapsed() + wait < budget =>
            {
                std::thread::sleep(wait);
                wait *= 3;
                retries += 1;
            }
            other => return other.map_err(Box::new),
        }
    }
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

/// Load `~/.config/ljos/env` (KEY=VALUE) when the process has not set
/// those keys. `ljos`, `packset`, and `packset-mcp` then share one pack.
pub fn load_seat_env() {
    let Some(home) = env::var_os("HOME") else {
        return;
    };
    let path = PathBuf::from(home).join(".config/ljos/env");
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim();
        if k.is_empty() || env::var_os(k).is_some() {
            continue;
        }
        env::set_var(k, v.trim());
    }
}

/// The workspace the seat's memory lives in: `PACKSET_WORKSPACE` after
/// loading the seat env file, else `seat`. Not `default`, and not the
/// working directory's git remote: those two are how one harness's
/// remember missed the other's sitting.
#[must_use]
pub fn resolved_workspace() -> String {
    load_seat_env();
    env::var("PACKSET_WORKSPACE")
        .ok()
        .map(|w| w.trim().to_string())
        .filter(|w| !w.is_empty())
        .unwrap_or_else(|| "seat".to_string())
}

/// `PACKSET_PORT` (`GROK_MEM_PORT` is an alias), else [`DEFAULT_PORT`].
#[must_use]
pub fn default_port() -> u16 {
    env::var("PACKSET_PORT")
        .or_else(|_| env::var("GROK_MEM_PORT"))
        .ok()
        .and_then(|raw| raw.trim().parse().ok())
        .unwrap_or(DEFAULT_PORT)
}

/// The token file a writer leaves in its pack home: `PACKSET_TOKEN_FILE`,
/// else `token` under `PACKSET_HOME` (`GROKINSIDE_HOME`,
/// `GROK_INSIDE_MEMORY_HOME`), else under the default home
/// ([`packset_core::home::resolve`]). packsetd resolves its home the same
/// way.
#[must_use]
pub fn token_path() -> Option<PathBuf> {
    if let Some(file) = env::var_os("PACKSET_TOKEN_FILE").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(file));
    }
    packset_core::home::resolve().map(|home| home.join(TOKEN_FILE))
}

/// The name of the token file in a pack home.
pub const TOKEN_FILE: &str = "token";

/// The token this seat shows the writer: `PACKSET_TOKEN`, else the first
/// line of [`token_path`]. `None` when there is neither, and the writer then
/// answers 401 to everything but `/health`.
#[must_use]
pub fn token() -> Option<String> {
    if let Ok(t) = env::var("PACKSET_TOKEN") {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Some(t);
        }
    }
    let text = std::fs::read_to_string(token_path()?).ok()?;
    let t = text.lines().next()?.trim().to_string();
    (!t.is_empty()).then_some(t)
}

/// `request` with this seat's token as a bearer header. A caller that speaks
/// to the writer outside [`PacksetClient`] wraps its request in this.
///
/// The token stays home when another user's process holds the writer's
/// loopback port: anyone can answer `/health` with `packsetd ok`, and a
/// listener that got there first would otherwise collect the token.
pub fn authorize(request: ureq::Request) -> ureq::Request {
    if foreign_listener(request.url()) {
        return request;
    }
    match token() {
        Some(t) => request.set("Authorization", &format!("Bearer {t}")),
        None => request,
    }
}

/// The port of a loopback `url`, `None` for any other host.
fn loopback_port(url: &str) -> Option<u16> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !port.contains(']') => (host, port.parse().ok()?),
        _ => (authority, 80),
    };
    matches!(host, "127.0.0.1" | "localhost" | "[::1]").then_some(port)
}

/// The owners of the sockets listening on `port`, from one `/proc/net/tcp`
/// or `tcp6` table: local address is the second column, state the fourth,
/// uid the eighth.
fn listen_uids(table: &str, port: u16) -> Vec<u32> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let local = fields.get(1)?;
            if fields.get(3) != Some(&"0A") {
                return None;
            }
            let hex = local.rsplit(':').next()?;
            if u16::from_str_radix(hex, 16).ok()? != port {
                return None;
            }
            fields.get(7)?.parse().ok()
        })
        .collect()
}

/// Whether every socket listening on the loopback port of `url` belongs to
/// a user other than this one. Unknown is not foreign: off Linux, or when
/// the tables cannot be read, the token goes as before.
fn foreign_listener(url: &str) -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        let Some(port) = loopback_port(url) else {
            return false;
        };
        let Ok(me) = std::fs::metadata("/proc/self").map(|m| m.uid()) else {
            return false;
        };
        let owners: Vec<u32> = ["/proc/net/tcp", "/proc/net/tcp6"]
            .iter()
            .filter_map(|t| std::fs::read_to_string(t).ok())
            .flat_map(|text| listen_uids(&text, port))
            .collect();
        !owners.is_empty() && owners.iter().all(|uid| *uid != me)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = url;
        false
    }
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
    /// The atom's entities: `seat:<name>` names the seat that wrote it,
    /// `persona:<name>` a persona's own claim, `habit:<name>` a reading.
    #[serde(default)]
    pub entities: Vec<String>,
    #[serde(default)]
    pub ballots: Option<u32>,
    #[serde(default)]
    pub of: Option<u32>,
}

/// A refusal, carrying the reason the writer gave in its body.
fn refused(url: &str, e: Box<ureq::Error>) -> Error {
    match *e {
        ureq::Error::Status(code, response) => {
            let text = response.into_string().unwrap_or_default();
            let reason = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v.get("error").and_then(|r| r.as_str()).map(str::to_string))
                .unwrap_or(text);
            let reason = reason.trim();
            if code == 401 {
                return Error::Bad(format!(
                    "{url}: 401: {reason}; this seat's token does not match the writer's, \
                     so it is another user's writer or PACKSET_HOME names another pack"
                ));
            }
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
        load_seat_env();
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
        let body = send(
            authorize(ureq::get(&url)).query("cwd", abs.to_string_lossy().as_ref()),
            None,
            timeout(),
        )
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
        let body = send(authorize(ureq::get(&url)), None, timeout())
            .map_err(|e| refused(&url, e))?
            .into_string()?;
        Ok(body)
    }

    pub fn get_atom(&self, workspace: &str, id: &str) -> Result<serde_json::Value, Error> {
        let encoded = path_seg(id);
        let url = format!("{}/v1/atoms/{encoded}", self.base);
        let resp = match send(
            authorize(ureq::get(&url)).query("workspace", workspace),
            None,
            timeout(),
        ) {
            Ok(resp) => resp,
            Err(e) if matches!(*e, ureq::Error::Status(404, _)) => {
                return Err(Error::Bad(format!("no atom {id}")));
            }
            Err(e) => return Err(Error::Http(e)),
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
        let mut req = authorize(ureq::get(&url)).query("workspace", workspace);
        if let Some(at) = as_of {
            req = req.query("as_of", at);
        }
        let mut body: serde_json::Value = send(req, None, timeout())
            .map_err(|e| refused(&url, e))?
            .into_json()?;
        let atoms = body
            .get_mut("atoms")
            .map(serde_json::Value::take)
            .unwrap_or(serde_json::Value::Array(vec![]));
        Ok(serde_json::from_value(atoms)?)
    }

    /// [`Self::atoms_as_of`] without the dense vectors, for a reader of
    /// texts, review clocks or rules: the vectors are most of the bytes. A
    /// writer older than the `embedding=omit` query sends them anyway.
    ///
    /// # Errors
    ///
    /// The pack not answering, or an answer that is not atoms.
    pub fn atoms_without_vectors(
        &self,
        workspace: &str,
        as_of: Option<&str>,
    ) -> Result<Vec<serde_json::Value>, Error> {
        let url = format!("{}/v1/atoms", self.base);
        let mut req = authorize(ureq::get(&url))
            .query("workspace", workspace)
            .query("embedding", "omit");
        if let Some(at) = as_of {
            req = req.query("as_of", at);
        }
        let mut body: serde_json::Value = send(req, None, timeout())
            .map_err(|e| refused(&url, e))?
            .into_json()?;
        let atoms = body
            .get_mut("atoms")
            .map(serde_json::Value::take)
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
        let mut req = authorize(ureq::get(&url))
            .query("workspace", workspace)
            .query("q", q)
            .query("limit", &limit.to_string());
        if let Some(at) = as_of {
            req = req.query("as_of", at);
        }
        if rerank {
            req = req.query("rerank", "1");
        }
        let body: serde_json::Value = send(req, None, budget)
            .map_err(|e| refused(&url, e))?
            .into_json()?;
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
        let mut req = authorize(ureq::get(&url));
        if let Some(workspace) = workspace {
            req = req.query("workspace", workspace);
        }
        Ok(send(req, None, timeout())
            .map_err(|e| refused(&url, e))?
            .into_json()?)
    }

    /// The set a workspace is pinned to.
    ///
    /// # Errors
    ///
    /// The request's, or a body that is not JSON.
    pub fn pin(&self, workspace: &str) -> Result<serde_json::Value, Error> {
        let url = format!("{}/v1/pin", self.base);
        Ok(send(
            authorize(ureq::get(&url)).query("workspace", workspace),
            None,
            timeout(),
        )
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
        Ok(send(
            authorize(ureq::put(&url)),
            Some(&serde_json::json!({ "workspace": workspace, "name": name })),
            timeout(),
        )
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
        let body: serde_json::Value = send(
            authorize(ureq::get(&url)).query("workspace", workspace),
            None,
            timeout(),
        )
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

    /// The live atoms of one kind in a workspace: personas, trust rows,
    /// habits, without the rest of the pack.
    ///
    /// # Errors
    ///
    /// The request's, or a body that is not JSON.
    pub fn atoms_of_kind(
        &self,
        workspace: &str,
        kind: &str,
    ) -> Result<Vec<serde_json::Value>, Error> {
        let url = format!("{}/v1/atoms", self.base);
        let body: serde_json::Value = send(
            authorize(ureq::get(&url))
                .query("workspace", workspace)
                .query("kind", kind),
            None,
            timeout(),
        )
        .map_err(|e| refused(&url, e))?
        .into_json()?;
        Ok(body
            .get("atoms")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default())
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
        let body: serde_json::Value = send(
            authorize(ureq::get(&url))
                .query("workspace", workspace)
                .query("accession", accession),
            None,
            timeout(),
        )
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
        let resp = match send(authorize(ureq::post(&url)), Some(&body), timeout()) {
            Ok(resp) => resp,
            Err(e) if matches!(*e, ureq::Error::Status(404, _)) => {
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
        let body: serde_json::Value = send(
            authorize(ureq::post(&url)),
            Some(&serde_json::json!({
                "workspace": workspace,
                "id": id,
                "recalled": recalled,
            })),
            timeout(),
        )
        .map_err(|e| refused(&url, e))?
        .into_json()?;
        Ok(body)
    }

    /// The link graph's communities, largest first.
    /// The claims the link graph turns on, highest first.
    pub fn hubs(&self, workspace: &str, limit: usize) -> Result<serde_json::Value, Error> {
        let url = format!("{}/v1/hubs", self.base);
        let body: serde_json::Value = send(
            authorize(ureq::get(&url))
                .query("workspace", workspace)
                .query("limit", &limit.to_string()),
            None,
            timeout(),
        )
        .map_err(|e| refused(&url, e))?
        .into_json()?;
        Ok(body)
    }

    pub fn islands(&self, workspace: &str) -> Result<serde_json::Value, Error> {
        let url = format!("{}/v1/islands", self.base);
        let body: serde_json::Value = send(
            authorize(ureq::get(&url)).query("workspace", workspace),
            None,
            timeout(),
        )
        .map_err(|e| refused(&url, e))?
        .into_json()?;
        Ok(body)
    }

    /// Claims that fired together: their links gain weight.
    pub fn fire(&self, workspace: &str, ids: &[String]) -> Result<serde_json::Value, Error> {
        self.fire_as(workspace, ids, None)
    }

    /// [`Self::fire`] through a persona's lens: the weights move under its
    /// name and the seat's stand.
    ///
    /// # Errors
    ///
    /// The request's, or a body that is not JSON.
    pub fn fire_as(
        &self,
        workspace: &str,
        ids: &[String],
        lens: Option<&str>,
    ) -> Result<serde_json::Value, Error> {
        let url = format!("{}/v1/fire", self.base);
        let body: serde_json::Value = send(
            authorize(ureq::post(&url)),
            Some(
                &serde_json::json!({"workspace": workspace, "ids": ids, "as": lens.unwrap_or("")}),
            ),
            timeout(),
        )
        .map_err(|e| refused(&url, e))?
        .into_json()?;
        Ok(body)
    }

    /// Consolidate the workspace: every claim that replaces an earlier one
    /// closes it (the write-time rule, run over what is held). `apply`
    /// false reports the pairs and writes nothing.
    pub fn consolidate(&self, workspace: &str, apply: bool) -> Result<serde_json::Value, Error> {
        let url = format!("{}/v1/consolidate", self.base);
        let body: serde_json::Value = send(
            authorize(ureq::post(&url)),
            Some(&serde_json::json!({"workspace": workspace, "apply": apply})),
            timeout(),
        )
        .map_err(|e| refused(&url, e))?
        .into_json()?;
        Ok(body)
    }

    /// Sweep a workspace for neglect: reviews left due past twice their
    /// interval lapse, and the third miss forgets a never-recalled lesson.
    ///
    /// # Errors
    ///
    /// The request's, or a body that is not JSON.
    pub fn sweep(&self, workspace: &str) -> Result<serde_json::Value, Error> {
        let url = format!("{}/v1/sweep", self.base);
        let body: serde_json::Value = send(
            authorize(ureq::post(&url)),
            Some(&serde_json::json!({"workspace": workspace})),
            timeout(),
        )
        .map_err(|e| refused(&url, e))?
        .into_json()?;
        Ok(body)
    }

    /// Drop a scratch workspace's atoms whole, no tombstones and no deed.
    /// Retraction stays [`Self::delete_atom`]; this is what a per-run
    /// scratch workspace calls on its way out.
    ///
    /// # Errors
    ///
    /// The request's, or a body that is not JSON.
    pub fn forget_workspace(&self, workspace: &str) -> Result<serde_json::Value, Error> {
        let url = format!("{}/v1/forget", self.base);
        let body: serde_json::Value = send(
            authorize(ureq::post(&url)),
            Some(&serde_json::json!({"workspace": workspace})),
            timeout(),
        )
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
        self.activate_as(workspace, q, limit, fire, None)
    }

    /// [`Self::activate`] through a persona's lens: the spread follows the
    /// weights that persona wrote, and a fire writes them.
    ///
    /// # Errors
    ///
    /// The request's, or a body that is not JSON.
    pub fn activate_as(
        &self,
        workspace: &str,
        q: &str,
        limit: u32,
        fire: bool,
        lens: Option<&str>,
    ) -> Result<serde_json::Value, Error> {
        let url = format!("{}/v1/activate", self.base);
        let mut req = authorize(ureq::get(&url))
            .query("workspace", workspace)
            .query("q", q)
            .query("limit", &limit.to_string())
            .query("fire", if fire { "1" } else { "0" });
        if let Some(name) = lens.filter(|n| !n.trim().is_empty()) {
            req = req.query("as", name);
        }
        let body: serde_json::Value = send(req, None, timeout())
            .map_err(|e| refused(&url, e))?
            .into_json()?;
        Ok(body)
    }

    /// The live atoms of one named set in a workspace: a persona's own
    /// conclusions, a pinned slice.
    ///
    /// # Errors
    ///
    /// The request's, or a body that is not JSON.
    pub fn atoms_in_set(
        &self,
        workspace: &str,
        set: &str,
    ) -> Result<Vec<serde_json::Value>, Error> {
        let url = format!("{}/v1/pack", self.base);
        let body: serde_json::Value = send(
            authorize(ureq::get(&url))
                .query("workspace", workspace)
                .query("set", set),
            None,
            timeout(),
        )
        .map_err(|e| refused(&url, e))?
        .into_json()?;
        Ok(body
            .get("atoms")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    pub fn post_atom(&self, atom: &serde_json::Value) -> Result<serde_json::Value, Error> {
        let url = format!("{}/v1/atoms", self.base);
        let body: serde_json::Value = send(authorize(ureq::post(&url)), Some(atom), timeout())
            .map_err(|e| refused(&url, e))?
            .into_json()?;
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Both workspace tests below rewrite process-global environment, so
    // they hold this lock: without it the runner's parallel threads
    // interleave one test's set-and-restore with the other's read, and
    // the suite fails one run in five on a polluted read.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn only_a_loopback_url_has_a_port_to_check() {
        assert_eq!(loopback_port("http://127.0.0.1:8761/health"), Some(8761));
        assert_eq!(loopback_port("http://localhost:18462"), Some(18462));
        assert_eq!(loopback_port("http://[::1]:8761/v1/search?q=a"), Some(8761));
        assert_eq!(loopback_port("http://127.0.0.1/health"), Some(80));
        assert_eq!(loopback_port("http://example.org:8761/"), None);
        assert_eq!(loopback_port("http://[::1]/"), Some(80));
    }

    #[test]
    fn the_owner_of_a_listening_port_is_read_off_the_table() {
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:223A 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1001        0 4242 1 0 100 0 0 10 0
   1: 0100007F:223A 0100007F:9C40 01 00000000:00000000 00:00000000 00000000  1000        0 4343 1 0 20 4 30 10 -1
   2: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 4444 1 0 100 0 0 10 0
";
        assert_eq!(listen_uids(table, 8762), vec![1001], "only the listener");
        assert_eq!(listen_uids(table, 8080), vec![1000]);
        assert!(listen_uids(table, 9).is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_port_this_user_holds_is_not_foreign() {
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = held.local_addr().unwrap().port();
        assert!(!foreign_listener(&format!(
            "http://127.0.0.1:{port}/health"
        )));
        drop(held);
        assert!(
            !foreign_listener(&format!("http://127.0.0.1:{port}/health")),
            "nobody listening is not foreign"
        );
    }

    #[test]
    fn resolved_workspace_reads_ljos_env_not_default() {
        let _env = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("packset-ljos-env-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".config/ljos")).unwrap();
        std::fs::write(
            dir.join(".config/ljos/env"),
            "PACKSET_WORKSPACE=git:example.com/seat/notes\n",
        )
        .unwrap();
        let old_home = env::var("HOME").ok();
        let old_ws = env::var("PACKSET_WORKSPACE").ok();
        unsafe {
            env::remove_var("PACKSET_WORKSPACE");
            env::set_var("HOME", &dir);
        }
        let got = resolved_workspace();
        unsafe {
            match old_home {
                Some(h) => env::set_var("HOME", h),
                None => env::remove_var("HOME"),
            }
            match old_ws {
                Some(w) => env::set_var("PACKSET_WORKSPACE", w),
                None => env::remove_var("PACKSET_WORKSPACE"),
            }
        }
        assert_eq!(got, "git:example.com/seat/notes");
    }

    #[test]
    fn resolved_workspace_without_env_is_seat_not_default() {
        let _env = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("packset-no-ljos-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let old_home = env::var("HOME").ok();
        let old_ws = env::var("PACKSET_WORKSPACE").ok();
        unsafe {
            env::remove_var("PACKSET_WORKSPACE");
            env::set_var("HOME", &dir);
        }
        let got = resolved_workspace();
        unsafe {
            match old_home {
                Some(h) => env::set_var("HOME", h),
                None => env::remove_var("HOME"),
            }
            match old_ws {
                Some(w) => env::set_var("PACKSET_WORKSPACE", w),
                None => env::remove_var("PACKSET_WORKSPACE"),
            }
        }
        assert_eq!(got, "seat");
        assert_ne!(got, "default");
    }

    /// A writer that answers busy to its first `busy` requests and counts every
    /// request.
    fn busy_for(
        busy: usize,
    ) -> (
        PacksetClient,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::sync::atomic::Ordering;
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let client = PacksetClient::new(format!("http://{}", listener.local_addr().unwrap()));
        let seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = std::sync::Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(&stream);
                let mut length = 0;
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                    line.clear();
                }
                let mut body = vec![0u8; length];
                let _ = reader.read_exact(&mut body);
                let (status, text) = if counted.fetch_add(1, Ordering::SeqCst) < busy {
                    ("503 Service Unavailable", r#"{"error":"busy"}"#)
                } else {
                    ("200 OK", r#"{"ok":true}"#)
                };
                let _ = (&stream).write_all(
                    format!(
                        "HTTP/1.1 {status}\r\nRetry-After: 1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                        text.len()
                    )
                    .as_bytes(),
                );
            }
        });
        (client, seen)
    }

    #[test]
    fn a_busy_writer_is_asked_again_for_a_read_and_a_write() {
        use std::sync::atomic::Ordering;
        let _env = ENV_LOCK.lock().unwrap();
        let (client, seen) = busy_for(2);
        assert_eq!(client.status(None).unwrap()["ok"], true);
        assert_eq!(seen.load(Ordering::SeqCst), 3);
        let (client, seen) = busy_for(1);
        let stored = client
            .post_atom(&serde_json::json!({"text": "one write", "workspace": "w"}))
            .unwrap();
        assert_eq!(stored["ok"], true);
        assert_eq!(
            seen.load(Ordering::SeqCst),
            2,
            "the write was not sent again"
        );
    }

    #[test]
    fn a_writer_busy_past_three_retries_is_a_refusal() {
        use std::sync::atomic::Ordering;
        let _env = ENV_LOCK.lock().unwrap();
        let (client, seen) = busy_for(usize::MAX);
        let err = client.status(None).unwrap_err().to_string();
        assert!(err.contains("503") && err.contains("busy"), "{err}");
        assert_eq!(seen.load(Ordering::SeqCst), 4);
    }

    /// A 100 ms budget leaves no room for the 150 ms wait.
    #[test]
    fn the_retries_stop_where_the_budget_does() {
        use std::sync::atomic::Ordering;
        let _env = ENV_LOCK.lock().unwrap();
        let old = env::var_os("PACKSET_TIMEOUT_MS");
        unsafe { env::set_var("PACKSET_TIMEOUT_MS", "100") };
        let (client, seen) = busy_for(usize::MAX);
        let started = Instant::now();
        let answered = client.status(None);
        let took = started.elapsed();
        unsafe {
            match old {
                Some(v) => env::set_var("PACKSET_TIMEOUT_MS", v),
                None => env::remove_var("PACKSET_TIMEOUT_MS"),
            }
        }
        assert!(answered.is_err());
        assert_eq!(seen.load(Ordering::SeqCst), 2);
        assert!(took < Duration::from_millis(300), "{took:?}");
    }
}
