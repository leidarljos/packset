//! The `/v1` surface.
//!
//! Loopback only, and `127.0.0.1` rather than `localhost`: the name resolves
//! to whatever the resolver says, which on a dual-stack seat is not always the
//! interface the writer bound. The address is the contract.
//!
//! This layer decodes and encodes and nothing else. What a verb means lives in
//! [`crate::service`].

use std::collections::HashMap;
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use packset_core::record::AtomError;
use serde_json::{json, Map, Value};

use crate::cards::WriteError;
use crate::service::{RetiredId, Service};
use crate::wire::{self, Method};

/// The address the writer will bind, and no other.
pub const LOOPBACK: &str = "127.0.0.1";
/// The port the clients look for.
pub const DEFAULT_PORT: u16 = 8761;
/// Workers when `PACKSET_WORKERS` does not name a number. Not the core count.
pub const DEFAULT_WORKERS: usize = 4;
/// The ceiling on workers. 32 threads each kept a 64 MB malloc arena
/// (2 GB idle) on a 32-thread laptop.
pub const MAX_WORKERS: usize = 8;
/// Connections that may wait for a worker before the next is told busy.
pub const DEFAULT_QUEUE: usize = 128;
/// The ceiling on the queue: each waiting connection holds a descriptor.
pub const MAX_QUEUE: usize = 4096;
/// How long the writer stays quiet before it returns free heap and drops
/// cached database pages, when `PACKSET_IDLE_TRIM` is unset.
pub const DEFAULT_IDLE_TRIM: Duration = Duration::from_secs(60);

/// How long a quiet stretch lasts before idle memory is released.
/// `off`, `none` and `0` never release.
#[must_use]
pub fn idle_trim_after() -> Duration {
    idle_after(std::env::var("PACKSET_IDLE_TRIM").ok().as_deref())
}

fn idle_after(raw: Option<&str>) -> Duration {
    match raw.map(str::trim) {
        None | Some("") => DEFAULT_IDLE_TRIM,
        Some("off" | "none" | "0") => Duration::ZERO,
        Some(n) => n
            .parse::<u64>()
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_IDLE_TRIM),
    }
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// After `wait` with no request that does real work, give free heap and
/// cold database pages back. Health and status probes do not count: a
/// herd of them would otherwise keep the clock moving and the release
/// would never run. One release per quiet stretch: trimming on every
/// check would fault the pages back in on the next request and straight
/// out again.
fn start_idle_release(service: Arc<Service>, busy: Arc<AtomicU64>, wait: Duration) {
    if wait.is_zero() {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("packsetd-idle".to_string())
        .spawn(move || {
            let step = Duration::from_secs(1).min(wait);
            let wait_ms = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX);
            let mut released = false;
            loop {
                std::thread::sleep(step);
                let quiet = unix_millis().saturating_sub(busy.load(Ordering::Relaxed));
                if quiet >= wait_ms {
                    if !released {
                        service.store().release_idle();
                        released = true;
                    }
                } else {
                    released = false;
                }
            }
        });
    if let Err(err) = spawned {
        eprintln!("packsetd: cannot start idle release: {err}");
    }
}

/// How many requests this writer will answer at once.
///
/// A fixed pool pulling from one queue makes a burst wait instead of a
/// machine swap.
#[must_use]
pub fn worker_count() -> usize {
    positive_env("PACKSET_WORKERS").map_or(DEFAULT_WORKERS, |n| n.min(MAX_WORKERS))
}

/// How many accepted connections may wait for a worker, `PACKSET_QUEUE`;
/// the lane takes the same bound.
#[must_use]
pub fn queue_depth() -> usize {
    positive_env("PACKSET_QUEUE").map_or(DEFAULT_QUEUE, |n| n.min(MAX_QUEUE))
}

fn positive_env(name: &str) -> Option<usize> {
    std::env::var_os(name)
        .and_then(|raw| raw.to_str().and_then(|s| s.trim().parse::<usize>().ok()))
        .filter(|n| *n > 0)
}

/// What a route decided to answer.
struct Answer {
    code: u16,
    body: Value,
}

impl Answer {
    fn ok(body: Value) -> Self {
        Self { code: 200, body }
    }

    fn err(code: u16, message: impl std::fmt::Display) -> Self {
        Self {
            code,
            body: json!({ "error": message.to_string() }),
        }
    }
}

/// Serve until the process is stopped.
///
/// # Errors
///
/// Fails when the address cannot be bound.
/// `token` is what every request but `GET /health` must carry as a bearer
/// header; `None` admits every local user, and is only for `PACKSET_AUTH=off`.
pub fn serve(
    service: Arc<Service>,
    panel: packset_core::Panel,
    host: &str,
    port: u16,
    token: Option<String>,
) -> anyhow::Result<()> {
    if host != LOOPBACK {
        anyhow::bail!("packsetd listens on {LOOPBACK} only");
    }
    let listener = TcpListener::bind((host, port))
        .map_err(|e| anyhow::anyhow!("cannot bind {host}:{port}: {e}"))?;
    let workers = worker_count();
    let queue = queue_depth();
    eprintln!(
        "packsetd: listening on http://{host}:{port} with {workers} workers and room for {queue} waiting"
    );
    let busy = Arc::new(AtomicU64::new(unix_millis()));
    start_idle_release(Arc::clone(&service), Arc::clone(&busy), idle_trim_after());
    wire::serve(&listener, workers, queue, move |request| {
        note_busy(&busy, &request.method, &request.target);
        handle(&service, &panel, token.as_deref(), request)
    });
    Ok(())
}

/// Stamp `busy` for a request that can encode, scan or write. A cheap
/// probe leaves the clock where it was.
fn note_busy(busy: &AtomicU64, method: &Method, target: &str) {
    if !wire::is_cheap(method, target) {
        busy.store(unix_millis(), Ordering::Relaxed);
    }
}

fn handle(
    service: &Service,
    panel: &packset_core::Panel,
    token: Option<&str>,
    request: &wire::Request,
) -> wire::Response {
    let (path, query) = split_query(&request.target);
    if request.method == Method::Get && path == "/health" {
        return wire::Response::text(200, "packsetd ok");
    }
    if let Some(refusal) = refuse_unauthorized(token, request) {
        return refusal;
    }
    let body = if matches!(request.method, Method::Post | Method::Put) {
        match read_json(&request.body) {
            Ok(map) => map,
            Err(message) => return encode(&Answer::err(400, message)),
        }
    } else {
        Map::new()
    };
    let answer = route(service, panel, &request.method, path, &query, &body);
    encode(&answer)
}

/// A 401 for a request without the writer's token; `None` lets it through.
fn refuse_unauthorized(token: Option<&str>, request: &wire::Request) -> Option<wire::Response> {
    let token = token?;
    if crate::auth::admits(request.authorization.as_deref(), token) {
        return None;
    }
    Some(encode(&Answer::err(
        401,
        "packsetd answers its owner only: send the token in the pack home's `token` file as `Authorization: Bearer TOKEN`",
    )))
}

fn route(
    service: &Service,
    panel: &packset_core::Panel,
    method: &Method,
    path: &str,
    query: &HashMap<String, String>,
    body: &Map<String, Value>,
) -> Answer {
    match (method, path) {
        // The old name answered here once; a client still asking for it is
        // reading a store this writer does not serve.
        (Method::Get, "/__inside_memd/health") => Answer::err(404, "not found"),
        (Method::Get, "/v1/status") => {
            let workspace = query.get("workspace").filter(|w| !w.is_empty());
            answer(service.status(workspace.map(String::as_str), panel))
        }
        (Method::Get, "/v1/workspaces") => match service.store().workspaces() {
            Ok(found) => Answer::ok(json!({
                "workspaces": found
                    .into_iter()
                    .map(|(name, live)| json!({"name": name, "live": live}))
                    .collect::<Vec<_>>()
            })),
            Err(e) => Answer::err(400, e),
        },
        (Method::Get, "/v1/pin") => match required(query, "workspace") {
            Err(a) => a,
            Ok(workspace) => answer(service.pin_payload(&workspace)),
        },
        (Method::Get, "/v1/pack") => match required(query, "workspace") {
            Err(a) => a,
            Ok(workspace) => {
                let set = query.get("set").filter(|s| !s.is_empty());
                answer(service.pack(&workspace, set.map(String::as_str)))
            }
        },
        (Method::Get, "/v1/set") => match required(query, "workspace") {
            Err(a) => a,
            Ok(workspace) => match required(query, "name") {
                Err(a) => a,
                Ok(name) => answer(service.pack(&workspace, Some(&name))),
            },
        },
        (Method::Get, "/v1/atoms") => match required(query, "workspace") {
            Err(a) => a,
            Ok(workspace) => {
                // `embedding=omit` leaves the vectors out: they are nine
                // tenths of the listing, and a client reading texts, review
                // clocks or rules parses every float for nothing.
                let omit = query.get("embedding").map(String::as_str) == Some("omit");
                match as_of_stamp(query) {
                    Err(a) => a,
                    Ok(Some(at)) => answer(service.as_of(&workspace, &at).map(|mut body| {
                        if omit {
                            if let Some(atoms) = body["atoms"].as_array_mut() {
                                for atom in atoms.iter_mut() {
                                    if let Some(map) = atom.as_object_mut() {
                                        map.remove("embedding");
                                    }
                                }
                            }
                        }
                        body
                    })),
                    Ok(None) => match service.store().live(&workspace) {
                        // `kind` narrows the answer to one kind, so a roster
                        // of personas does not carry every lesson's embedding.
                        // The cache keeps the vector as floats. The answer
                        // puts the JSON array back, which is what a client
                        // already reads.
                        Ok(atoms) => {
                            let vectors = service.store().vectors(&workspace);
                            let kind = query
                                .get("kind")
                                .map(String::as_str)
                                .filter(|k| !k.is_empty());
                            if kind.is_none() && !omit {
                                Answer::ok(json!({
                                    "atoms": atoms
                                        .iter()
                                        .map(|atom| listed(atom, &vectors, false))
                                        .collect::<Vec<_>>()
                                }))
                            } else {
                                Answer::ok(json!({
                                    "atoms": atoms
                                        .iter()
                                        .filter(|a| kind.is_none_or(|k| {
                                            a.get("kind").and_then(Value::as_str) == Some(k)
                                        }))
                                        .map(|atom| listed(atom, &vectors, omit))
                                        .collect::<Vec<_>>()
                                }))
                            }
                        }
                        Err(e) => Answer::err(400, e),
                    },
                }
            }
        },
        // The deed accessions a workspace's live atoms cite, so `deedar
        // evidence -` and `deedar current -` cover a pack the way they cover a
        // tracker. Plain strings rather than atoms: the caller wants the join
        // key, and asking for /v1/atoms to get it means shipping every body.
        (Method::Get, "/v1/accessions") => match required(query, "workspace") {
            Err(a) => a,
            Ok(workspace) => match service.accessions(&workspace) {
                Ok(found) => Answer::ok(json!({
                    "workspace": workspace,
                    "accessions": found,
                })),
                Err(e) => Answer::err(400, e),
            },
        },
        // The other direction: one accession, and the live atoms that cite it.
        (Method::Get, "/v1/citers") => match required(query, "workspace") {
            Err(a) => a,
            Ok(workspace) => match required(query, "accession") {
                Err(a) => a,
                Ok(accession) => match service.citers(&workspace, &accession) {
                    Ok(found) => Answer::ok(json!({
                        "workspace": workspace,
                        "accession": accession,
                        "atoms": found,
                    })),
                    Err(e) => Answer::err(400, e),
                },
            },
        },
        (Method::Get, _) if path.starts_with("/v1/atoms/") => {
            let id = &path["/v1/atoms/".len()..];
            if id.is_empty() || id.contains('/') {
                return Answer::err(404, "not found");
            }
            match required(query, "workspace") {
                Err(a) => a,
                Ok(workspace) => match service.store().get(&workspace, id) {
                    Err(e) => Answer::err(400, e),
                    Ok(None) => Answer::err(404, "no atom"),
                    Ok(Some(mut atom)) => match as_of_stamp(query) {
                        Err(a) => a,
                        Ok(at) => {
                            let dated = at.is_some();
                            let now = at.unwrap_or_else(packset_core::clock::utcnow);
                            let live = if dated {
                                packset_core::record::is_live_at(&atom, &now)
                            } else {
                                packset_core::record::is_live(&atom, &now)
                            };
                            if live {
                                packset_core::record::fill_origin(&mut atom);
                                Answer::ok(Value::Object(atom))
                            } else {
                                Answer::err(404, "no atom")
                            }
                        }
                    },
                },
            }
        }
        (Method::Get, "/v1/identity") => {
            let cwd = query.get("cwd").cloned().unwrap_or_else(|| ".".into());
            let harness = query
                .get("harness")
                .filter(|h| !h.is_empty())
                .cloned()
                .unwrap_or_else(|| "any".into());
            match crate::workspace::identity(
                std::path::Path::new(&cwd),
                packset_core::identity::Strategy::PerRepo,
                &harness,
                None,
                None,
                0,
                None,
            ) {
                Ok(value) => Answer::ok(value),
                Err(message) => Answer::err(400, message),
            }
        }
        (Method::Get, "/v1/rules") => {
            let cwd = query.get("cwd").cloned().unwrap_or_else(|| ".".into());
            let with_body = truthy(query.get("body").map(String::as_str));
            Answer::ok(crate::context::rules_payload(
                std::path::Path::new(&cwd),
                &service.home().user_path(),
                with_body,
            ))
        }
        (Method::Get, "/v1/skills") => {
            let cwd = query.get("cwd").cloned().unwrap_or_else(|| ".".into());
            let name = query.get("name").filter(|n| !n.is_empty());
            // Global skills live under the seat's own home, not the pack home:
            // a pack can be moved between seats and a skill catalog cannot.
            let home = std::env::var_os("HOME")
                .map_or_else(|| std::path::PathBuf::from("."), std::path::PathBuf::from);
            Answer::ok(crate::context::skills_payload(
                std::path::Path::new(&cwd),
                &home,
                name.map(String::as_str),
            ))
        }
        (Method::Get, "/v1/map") => {
            let cwd = query.get("cwd").cloned().unwrap_or_else(|| ".".into());
            Answer::ok(crate::context::repo_map(std::path::Path::new(&cwd)))
        }
        (Method::Post, "/v1/sweep") => match required(body, "workspace") {
            Err(a) => a,
            Ok(workspace) => answer(service.sweep(&workspace)),
        },
        (Method::Post, "/v1/forget") => match required(body, "workspace") {
            Err(a) => a,
            Ok(workspace) => answer(service.forget_workspace(&workspace)),
        },
        (Method::Post, "/v1/consolidate") => match required(body, "workspace") {
            Err(a) => a,
            Ok(workspace) => {
                let apply = body.get("apply").and_then(Value::as_bool).unwrap_or(false);
                answer(service.consolidate(&workspace, apply))
            }
        },
        (Method::Post, "/v1/fire") => match required(body, "workspace") {
            Err(a) => a,
            Ok(workspace) => {
                let ids: Vec<String> = body
                    .get("ids")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                if ids.len() < 2 {
                    return Answer::err(400, "ids: two or more claims that fired together");
                }
                let lens = body
                    .get("as")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty());
                answer(service.fire_as(&workspace, &ids, lens))
            }
        },
        (Method::Get, "/v1/islands") => match required(query, "workspace") {
            Err(a) => a,
            Ok(workspace) => answer(service.islands(&workspace)),
        },
        (Method::Get, "/v1/hubs") => match required(query, "workspace") {
            Err(a) => a,
            Ok(workspace) => {
                let limit = query
                    .get("limit")
                    .and_then(|l| l.parse::<usize>().ok())
                    .unwrap_or(10);
                answer(service.hubs(&workspace, limit))
            }
        },
        (Method::Get, "/v1/activate") => match required(query, "workspace") {
            Err(a) => a,
            Ok(workspace) => {
                let limit = query
                    .get("limit")
                    .and_then(|l| l.parse::<usize>().ok())
                    .unwrap_or(24);
                let q = query.get("q").cloned().unwrap_or_default();
                if q.trim().is_empty() {
                    return Answer::err(400, "q required: the cue that activates");
                }
                let fire = crate::embed::requested(query.get("fire").map(String::as_str));
                // `as` is a persona's lens: its weights on the way in and out.
                let lens = query
                    .get("as")
                    .map(String::as_str)
                    .filter(|s| !s.trim().is_empty());
                answer(service.activate_as(&workspace, &q, limit, panel, fire, lens))
            }
        },
        (Method::Get, "/v1/search") => match required(query, "workspace") {
            Err(a) => a,
            Ok(workspace) => {
                let limit = match query.get("limit").filter(|l| !l.is_empty()) {
                    None => 16usize,
                    Some(raw) => match raw.parse::<i64>() {
                        Ok(v) => v.max(0) as usize,
                        Err(_) => return Answer::err(400, "limit must be an integer"),
                    },
                };
                let q = query.get("q").cloned().unwrap_or_default();
                let set = query.get("set").filter(|s| !s.is_empty());
                let rerank = crate::embed::requested(query.get("rerank").map(String::as_str));
                let rank = match packset_core::search::Rank::parse(
                    query.get("rank").map(String::as_str),
                ) {
                    Ok(rank) => rank,
                    Err(reason) => return Answer::err(400, reason),
                };
                match as_of_stamp(query) {
                    Err(a) => a,
                    Ok(at) => answer(
                        service.search_ranked(
                            &workspace,
                            &q,
                            limit,
                            set.map(String::as_str),
                            panel,
                            at.as_deref(),
                            rerank,
                            query
                                .get("kind")
                                .map(String::as_str)
                                .filter(|k| !k.is_empty()),
                            rank,
                        ),
                    ),
                }
            }
        },
        (Method::Get, "/v1/recall") => match required(query, "workspace") {
            Err(a) => a,
            Ok(workspace) => {
                let limit = match query.get("limit").filter(|l| !l.is_empty()) {
                    None => None,
                    Some(raw) => match raw.parse::<i64>() {
                        Ok(v) => Some(v),
                        Err(_) => return Answer::err(400, "limit must be an integer"),
                    },
                };
                let seeds: Vec<String> = query
                    .get("seed")
                    .map(|raw| {
                        raw.split(',')
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                let hints = packset_core::recall::Hints {
                    text: query.get("q").cloned().unwrap_or_default(),
                    entities: Vec::new(),
                };
                match service.store().live(&workspace) {
                    Err(e) => Answer::err(400, e),
                    Ok(atoms) => {
                        let vectors = service.store().vectors(&workspace);
                        let mut recalled = packset_core::recall::recall(
                            &atoms,
                            &seeds,
                            &hints,
                            limit,
                            &packset_core::clock::utcnow(),
                            query
                                .get("kind")
                                .map(String::as_str)
                                .filter(|k| !k.is_empty()),
                        );
                        for atom in &mut recalled {
                            crate::store::restore_embedding(atom, &vectors);
                        }
                        Answer::ok(json!({ "atoms": recalled }))
                    }
                }
            }
        },
        (Method::Get, "/v1/attach") => match required(query, "workspace") {
            Err(a) => a,
            Ok(workspace) => {
                let peek = truthy(query.get("peek").map(String::as_str));
                let slot = if peek {
                    service.peek_attach(&workspace)
                } else {
                    service.take_attach(&workspace)
                };
                let slot = slot.unwrap_or_default();
                Answer::ok(json!({
                    "workspace": workspace,
                    "text": slot.text,
                    "label": slot.label,
                }))
            }
        },
        (Method::Put, "/v1/pin") => match required(body, "workspace") {
            Err(a) => a,
            Ok(workspace) => {
                let name = body.get("set").and_then(Value::as_str).unwrap_or("");
                match service.set_pin(&workspace, name) {
                    Ok(pinned) => Answer::ok(json!({"workspace": workspace, "set": pinned})),
                    Err(e) => Answer::err(400, e),
                }
            }
        },
        (Method::Put, "/v1/set") => match required(body, "workspace") {
            Err(a) => a,
            Ok(workspace) => {
                let name = body
                    .get("name")
                    .or_else(|| body.get("set"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                match service.write_set(&workspace, name, body) {
                    Err(WriteError::Overflow(o)) => Answer::err(413, o),
                    Err(other) => Answer::err(400, other),
                    Ok(stored) => answer(service.pack(&workspace, Some(&stored))),
                }
            }
        },
        (Method::Put, "/v1/user") => {
            let text = body.get("text").and_then(Value::as_str).unwrap_or("");
            card_answer(service.set_user(text))
        }
        (Method::Put, "/v1/memory") => {
            // No workspace required, which is the writer being replaced: an
            // empty name slugs to `workspace` and the card lands there rather
            // than being refused. Odd, and load-bearing for anything already
            // sending one.
            let workspace = body.get("workspace").and_then(Value::as_str).unwrap_or("");
            let text = body.get("text").and_then(Value::as_str).unwrap_or("");
            card_answer(service.set_memory(workspace, text))
        }
        (Method::Get, "/v1/proposals") => match required(query, "workspace") {
            Err(a) => a,
            Ok(workspace) => Answer::ok(json!({"proposals": service.proposals(&workspace)})),
        },
        (Method::Post, "/v1/proposals") => cheap_answer(service.propose(body)),
        (Method::Post, "/v1/proposals/accept") => {
            let workspace = body.get("workspace").and_then(Value::as_str).unwrap_or("");
            let id = body.get("id").and_then(Value::as_str).unwrap_or("");
            if workspace.is_empty() || id.is_empty() {
                return Answer::err(400, "workspace and id required");
            }
            cheap_answer(service.accept(workspace, id).map(written))
        }
        (Method::Post, "/v1/compact") => match required(body, "workspace") {
            Err(a) => a,
            Ok(workspace) => {
                let day = body
                    .get("day")
                    .and_then(Value::as_str)
                    .filter(|d| !d.is_empty());
                let transcript = body.get("transcript").and_then(Value::as_str);
                cheap_answer(service.compact(&workspace, day, transcript))
            }
        },
        (Method::Post, "/v1/atoms") => answer(service.add(body.clone()).map(written)),
        (Method::Post, "/v1/atoms/update") => {
            let (Some(workspace), Some(id)) = (
                body.get("workspace").and_then(Value::as_str),
                body.get("id").and_then(Value::as_str),
            ) else {
                return Answer::err(400, "workspace and id required");
            };
            let empty = Map::new();
            let fields = body
                .get("fields")
                .and_then(Value::as_object)
                .unwrap_or(&empty);
            answer(service.update(workspace, id, fields).map(written))
        }
        (Method::Post, "/v1/atoms/delete") => {
            let (Some(workspace), Some(id)) = (
                body.get("workspace").and_then(Value::as_str),
                body.get("id").and_then(Value::as_str),
            ) else {
                return Answer::err(400, "workspace and id required");
            };
            let why = body.get("why").and_then(Value::as_str);
            answer(service.delete_atom(workspace, id, why).map(written))
        }
        (Method::Post, "/v1/grade") => {
            let workspace = body.get("workspace").and_then(Value::as_str).unwrap_or("");
            let id = body.get("id").and_then(Value::as_str).unwrap_or("");
            if workspace.is_empty() || id.is_empty() {
                return Answer::err(400, "workspace and id required");
            }
            let recalled = match body.get("recalled") {
                None | Some(Value::Null) => true,
                Some(Value::Bool(b)) => *b,
                Some(Value::String(s)) => {
                    !matches!(s.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no")
                }
                Some(other) => other.as_i64().unwrap_or(1) != 0,
            };
            answer(service.grade(workspace, id, recalled).map(written))
        }
        (Method::Post, "/v1/attach") => match required(body, "workspace") {
            Err(a) => a,
            Ok(workspace) => {
                let text = match body.get("text") {
                    Some(Value::String(s)) => s.clone(),
                    // No text at all means the body names a file to read, so a
                    // client can hand over a log without carrying it.
                    Some(Value::Null) | None => crate::context::read_attach_source(
                        body.get("path").and_then(Value::as_str).unwrap_or(""),
                        crate::context::ATTACH_CAP,
                    ),
                    Some(other) => other.to_string(),
                };
                let label = body.get("label").and_then(Value::as_str).unwrap_or("");
                Answer::ok(service.put_attach(&workspace, &text, label))
            }
        },
        _ => Answer::err(404, "not found"),
    }
}

/// Turn a service result into an answer, keeping the store's own message.
/// One atom as a listing answers it. The cache holds floats and a null
/// slot; the answer carries the JSON array unless the caller left vectors
/// out. A record with no vector stays as the cache has it.
fn listed(
    atom: &Map<String, Value>,
    vectors: &std::collections::HashMap<String, std::sync::Arc<[f32]>>,
    omit: bool,
) -> Value {
    let id = atom.get("id").and_then(Value::as_str);
    let has_vector = id.is_some_and(|id| vectors.contains_key(id));
    if !omit && !has_vector {
        return Value::Object(packset_core::record::shown(atom).into_owned());
    }
    let mut copy = atom.clone();
    if omit {
        copy.remove("embedding");
    } else {
        crate::store::restore_embedding(&mut copy, vectors);
    }
    packset_core::record::fill_origin(&mut copy);
    Value::Object(copy)
}

/// What a write answers: the stored claim without its vector. The writer
/// needs the id, the text and what it superseded; the 384 floats were most
/// of every reply and no client reads them back from a write.
fn written(mut atom: Map<String, Value>) -> Map<String, Value> {
    atom.remove("embedding");
    packset_core::record::fill_origin(&mut atom);
    atom
}

fn answer<T: Into<Value>>(result: anyhow::Result<T>) -> Answer {
    match result {
        Ok(value) => Answer::ok(value.into()),
        Err(e) => {
            // A retired id is a conflict with the tombstone, not a malformed
            // body. Every other refusal stays 400.
            let code = if e.downcast_ref::<RetiredId>().is_some() {
                409
            } else {
                400
            };
            Answer::err(code, root_message(&e))
        }
    }
}

/// The innermost message, which is the one a client can act on.
fn root_message(err: &anyhow::Error) -> String {
    if let Some(atom) = err.downcast_ref::<AtomError>() {
        return atom.0.clone();
    }
    err.to_string()
}

/// A refused cheap-model job answers 403: the caller is not wrong about the
/// request, it is asking at a point in the cycle where the job does not run.
fn cheap_answer<T: Into<Value>>(result: anyhow::Result<T>) -> Answer {
    match result {
        Ok(value) => Answer::ok(value.into()),
        Err(e) => {
            if let Some(cheap) = e.downcast_ref::<crate::proposals::CheapError>() {
                Answer::err(403, &cheap.0)
            } else {
                Answer::err(400, root_message(&e))
            }
        }
    }
}

/// Overflow answers 413, because the client can shorten and retry; anything
/// else about a card is a refusal it has to fix.
fn card_answer(result: Result<(), WriteError>) -> Answer {
    match result {
        Ok(()) => Answer::ok(json!({"ok": true})),
        Err(WriteError::Overflow(o)) => Answer::err(413, o),
        Err(other) => Answer::err(400, other),
    }
}

trait Lookup {
    fn lookup(&self, key: &str) -> Option<String>;
}

impl Lookup for HashMap<String, String> {
    fn lookup(&self, key: &str) -> Option<String> {
        self.get(key).filter(|v| !v.is_empty()).cloned()
    }
}

impl Lookup for Map<String, Value> {
    fn lookup(&self, key: &str) -> Option<String> {
        self.get(key)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    }
}

/// A dated retrieve stamp, or none when the caller asked for live-now.
///
/// The query may spell the instant the way parse_millis already accepts
/// (`+00:00`, a space instead of `T`, missing millis). The window compare
/// is a string compare, so the value that leaves here is the store form.
fn as_of_stamp(query: &HashMap<String, String>) -> Result<Option<String>, Answer> {
    match query.get("as_of").filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(raw) => match packset_core::clock::canonicalize(raw) {
            Some(at) => Ok(Some(at)),
            None => Err(Answer::err(400, "as_of must be a timestamp")),
        },
    }
}

fn required<L: Lookup>(source: &L, key: &str) -> Result<String, Answer> {
    source
        .lookup(key)
        .ok_or_else(|| Answer::err(400, format!("{key} required")))
}

fn truthy(raw: Option<&str>) -> bool {
    matches!(raw, Some("1" | "true" | "yes"))
}

fn read_json(bytes: &[u8]) -> Result<Map<String, Value>, String> {
    let raw = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    if raw.trim().is_empty() {
        return Ok(Map::new());
    }
    let value: Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;
    value
        .as_object()
        .cloned()
        .ok_or_else(|| "JSON object required".to_string())
}

fn split_query(url: &str) -> (&str, HashMap<String, String>) {
    let Some((path, raw)) = url.split_once('?') else {
        return (url, HashMap::new());
    };
    let mut out = HashMap::new();
    for pair in raw.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        // First wins, matching a parse that takes element zero of the list.
        out.entry(percent_decode(key))
            .or_insert_with(|| percent_decode(value));
    }
    (path, out)
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn encode(answer: &Answer) -> wire::Response {
    let body = serde_json::to_string(&answer.body).unwrap_or_else(|_| "{}".into());
    wire::Response::json(answer.code, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_workers_is_four_not_core_count() {
        assert_eq!(DEFAULT_WORKERS, 4);
        assert_eq!(MAX_WORKERS, 8);
        assert_eq!(wire::dear_limit(DEFAULT_WORKERS), 3);
        assert_eq!(wire::dear_limit(1), 1);
    }

    #[test]
    fn a_probe_does_not_stamp_the_writer_busy() {
        let busy = AtomicU64::new(7);
        for target in [
            "/health",
            "/v1/status?workspace=seat",
            "/v1/workspaces",
            "/__inside_memd/health",
            "/v1/atoms?workspace=seat&embedding=omit",
        ] {
            note_busy(&busy, &Method::Get, target);
        }
        assert_eq!(busy.load(Ordering::Relaxed), 7);
        note_busy(&busy, &Method::Post, "/v1/atoms");
        assert!(busy.load(Ordering::Relaxed) > 7);
    }

    #[test]
    fn health_status_and_workspaces_stay_off_the_search_lane() {
        assert!(wire::is_cheap(&Method::Get, "/health"));
        assert!(wire::is_cheap(&Method::Get, "/v1/status?workspace=seat"));
        assert!(wire::is_cheap(&Method::Get, "/v1/workspaces"));
        assert!(wire::is_cheap(&Method::Get, "/__inside_memd/health"));
        assert!(!wire::is_cheap(
            &Method::Get,
            "/v1/search?workspace=seat&q=fusion"
        ));
        assert!(!wire::is_cheap(&Method::Get, "/v1/atoms?workspace=seat"));
        assert!(!wire::is_cheap(&Method::Post, "/health"));
        assert!(!wire::is_cheap(&Method::Post, "/v1/atoms"));
    }

    #[test]
    fn a_query_splits_and_decodes() {
        let (path, query) = split_query("/v1/pack?workspace=git%3Agithub.com%2FHaoZeke%2Fvissue");
        assert_eq!(path, "/v1/pack");
        assert_eq!(
            query.get("workspace").map(String::as_str),
            Some("git:github.com/HaoZeke/vissue")
        );
    }

    #[test]
    fn a_path_with_no_query_is_left_alone() {
        let (path, query) = split_query("/v1/workspaces");
        assert_eq!(path, "/v1/workspaces");
        assert!(query.is_empty());
    }

    #[test]
    fn the_first_value_of_a_repeated_key_wins() {
        let (_, query) = split_query("/v1/pack?workspace=a&workspace=b");
        assert_eq!(query.get("workspace").map(String::as_str), Some("a"));
    }

    #[test]
    fn a_flag_reads_the_three_spellings_and_nothing_else() {
        assert!(truthy(Some("1")));
        assert!(truthy(Some("true")));
        assert!(truthy(Some("yes")));
        assert!(!truthy(Some("on")));
        assert!(!truthy(Some("")));
        assert!(!truthy(None));
    }

    #[test]
    fn idle_trim_is_a_minute_unless_it_is_turned_off() {
        assert_eq!(idle_after(None), DEFAULT_IDLE_TRIM);
        assert_eq!(idle_after(Some("")), DEFAULT_IDLE_TRIM);
        assert_eq!(idle_after(Some("off")), Duration::ZERO);
        assert_eq!(idle_after(Some("none")), Duration::ZERO);
        assert_eq!(idle_after(Some("0")), Duration::ZERO);
        assert_eq!(idle_after(Some("15")), Duration::from_secs(15));
        assert_eq!(idle_after(Some("later")), DEFAULT_IDLE_TRIM);
    }

    #[test]
    fn an_as_of_stamp_is_checked() {
        let mut q = HashMap::new();
        assert!(matches!(as_of_stamp(&q), Ok(None)));
        q.insert("as_of".into(), "2024-06-01T00:00:00.000Z".into());
        assert!(matches!(
            as_of_stamp(&q),
            Ok(Some(ref s)) if s == "2024-06-01T00:00:00.000Z"
        ));
        q.insert("as_of".into(), "not-a-date".into());
        assert!(matches!(as_of_stamp(&q), Err(a) if a.code == 400));
    }

    #[test]
    fn a_plus_offset_as_of_agrees_with_the_store_form() {
        let mut q = HashMap::new();
        q.insert("as_of".into(), "2024-06-01T00:00:00+00:00".into());
        assert!(matches!(
            as_of_stamp(&q),
            Ok(Some(ref s)) if s == "2024-06-01T00:00:00.000Z"
        ));
        q.insert("as_of".into(), "2024-06-01 00:00:00".into());
        assert!(matches!(
            as_of_stamp(&q),
            Ok(Some(ref s)) if s == "2024-06-01T00:00:00.000Z"
        ));
        q.insert("as_of".into(), "2024-06-01 00:00:00.000Z".into());
        assert!(matches!(
            as_of_stamp(&q),
            Ok(Some(ref s)) if s == "2024-06-01T00:00:00.000Z"
        ));
    }

    #[test]
    fn a_plus_is_a_space_and_a_stray_percent_survives() {
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    fn dated_pack() -> (tempfile::TempDir, Service) {
        let dir = tempfile::tempdir().unwrap();
        let svc = Service::open(crate::home::Home::new(dir.path())).unwrap();
        svc.store()
            .upsert(
                &json!({
                    "id": "old",
                    "workspace": "w",
                    "text": "The default fuse is Borda.",
                    "kind": "voice",
                    "valid_from": "2024-01-01T00:00:00.000Z",
                    "valid_to": "2024-12-01T00:00:00.000Z"
                })
                .as_object()
                .unwrap()
                .clone(),
            )
            .unwrap();
        svc.store()
            .upsert(
                &json!({
                    "id": "neu",
                    "workspace": "w",
                    "text": "The default fuse is CombMNZ.",
                    "kind": "voice",
                    "valid_from": "2024-12-01T00:00:00.000Z"
                })
                .as_object()
                .unwrap()
                .clone(),
            )
            .unwrap();
        (dir, svc)
    }

    fn hit_ids(answer: &Answer) -> Vec<&str> {
        answer.body["hits"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|h| h["id"].as_str())
            .collect()
    }

    #[test]
    fn dated_search_returns_the_claim_live_then() {
        let (_dir, svc) = dated_pack();
        let panel = packset_core::Panel::default();
        let mut q = HashMap::new();
        q.insert("workspace".into(), "w".into());
        q.insert("q".into(), "Borda".into());
        q.insert("as_of".into(), "2024-06-01T00:00:00.000Z".into());
        let then = route(&svc, &panel, &Method::Get, "/v1/search", &q, &Map::new());
        assert_eq!(then.code, 200, "{:?}", then.body);
        assert_eq!(hit_ids(&then), vec!["old"], "{:?}", then.body);
        assert_eq!(then.body["as_of"], json!("2024-06-01T00:00:00.000Z"));
        q.insert("as_of".into(), "not-a-date".into());
        let bad = route(&svc, &panel, &Method::Get, "/v1/search", &q, &Map::new());
        assert_eq!(bad.code, 400, "{:?}", bad.body);
        assert_eq!(bad.body["error"], json!("as_of must be a timestamp"));
        q.remove("as_of");
        let now = route(&svc, &panel, &Method::Get, "/v1/search", &q, &Map::new());
        assert_eq!(now.code, 200, "{:?}", now.body);
        assert!(
            !hit_ids(&now).contains(&"old"),
            "live-now search still drops it: {:?}",
            now.body
        );
    }

    /// A record with no origin is not the person's. Every answer that hands
    /// it to a client says `unknown`, and the stored row stays without one.
    #[test]
    fn a_record_with_no_origin_reads_as_unknown() {
        let (_dir, svc) = dated_pack();
        let panel = packset_core::Panel::default();
        let get = |path: &str, extra: &[(&str, &str)]| {
            let mut q: HashMap<String, String> = HashMap::new();
            q.insert("workspace".into(), "w".into());
            for (key, value) in extra {
                q.insert((*key).into(), (*value).into());
            }
            let got = route(&svc, &panel, &Method::Get, path, &q, &Map::new());
            assert_eq!(got.code, 200, "{path} {extra:?}: {:?}", got.body);
            got.body
        };
        for (path, extra, list) in [
            ("/v1/atoms", vec![], "atoms"),
            ("/v1/atoms", vec![("embedding", "omit")], "atoms"),
            ("/v1/atoms", vec![("kind", "voice")], "atoms"),
            (
                "/v1/atoms",
                vec![("as_of", "2024-06-01T00:00:00.000Z")],
                "atoms",
            ),
            ("/v1/pack", vec![], "atoms"),
            ("/v1/search", vec![("q", "fuse")], "hits"),
            ("/v1/recall", vec![], "atoms"),
        ] {
            let body = get(path, &extra);
            let rows = body[list].as_array().unwrap();
            assert!(!rows.is_empty(), "{path} {extra:?}: {body:?}");
            for row in rows {
                assert_eq!(row["origin"], json!("unknown"), "{path} {extra:?}: {row:?}");
            }
        }
        assert_eq!(get("/v1/atoms/neu", &[])["origin"], json!("unknown"));
        let updated = route(
            &svc,
            &panel,
            &Method::Post,
            "/v1/atoms/update",
            &HashMap::new(),
            json!({"workspace": "w", "id": "neu", "fields": {"pinned": true}})
                .as_object()
                .unwrap(),
        );
        assert_eq!(updated.code, 200, "{:?}", updated.body);
        assert_eq!(updated.body["origin"], json!("unknown"));
        let stored = svc.store().get("w", "neu").unwrap().unwrap();
        assert!(stored.get("origin").is_none(), "{stored:?}");
    }

    fn post(svc: &Service, path: &str, body: &Value) -> Answer {
        let panel = packset_core::Panel::default();
        let got = route(
            svc,
            &panel,
            &Method::Post,
            path,
            &HashMap::new(),
            body.as_object().unwrap(),
        );
        assert_eq!(got.code, 200, "{path}: {:?}", got.body);
        got
    }

    #[test]
    fn an_accept_retry_reads_a_record_with_no_origin_as_unknown() {
        let (_dir, svc) = dated_pack();
        crate::proposals::mark_accepted(svc.home(), "w", &json!({"id": "p1"}), "neu").unwrap();
        let got = post(
            &svc,
            "/v1/proposals/accept",
            &json!({"workspace": "w", "id": "p1"}),
        );
        assert_eq!(got.body["id"], json!("neu"), "{:?}", got.body);
        assert_eq!(got.body["origin"], json!("unknown"), "{:?}", got.body);
        let stored = svc.store().get("w", "neu").unwrap().unwrap();
        assert!(stored.get("origin").is_none(), "{stored:?}");
    }

    #[test]
    fn a_delete_reads_a_record_with_no_origin_as_unknown() {
        let (_dir, svc) = dated_pack();
        let got = post(
            &svc,
            "/v1/atoms/delete",
            &json!({"workspace": "w", "id": "neu"}),
        );
        assert_eq!(got.body["tombstone"], json!(true), "{:?}", got.body);
        assert_eq!(got.body["origin"], json!("unknown"), "{:?}", got.body);
    }

    #[test]
    fn a_grade_reads_a_record_with_no_origin_as_unknown() {
        let (_dir, svc) = dated_pack();
        let got = post(&svc, "/v1/grade", &json!({"workspace": "w", "id": "neu"}));
        assert_eq!(got.body["origin"], json!("unknown"), "{:?}", got.body);
        let stored = svc.store().get("w", "neu").unwrap().unwrap();
        assert!(stored.get("origin").is_none(), "{stored:?}");
    }

    fn write_embed_stub(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("packset-embed");
        std::fs::write(&path, body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(&path).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&path, perm).unwrap();
        (dir, path)
    }

    fn two_claim_pack() -> (tempfile::TempDir, Service) {
        let dir = tempfile::tempdir().unwrap();
        let svc = Service::open(crate::home::Home::new(dir.path())).unwrap();
        svc.add(
            json!({
                "workspace": "w",
                "text": "Reviews open with a check.",
                "kind": "voice"
            })
            .as_object()
            .unwrap()
            .clone(),
        )
        .unwrap();
        svc.add(
            json!({
                "workspace": "w",
                "text": "Prefer ripgrep for search.",
                "kind": "voice"
            })
            .as_object()
            .unwrap()
            .clone(),
        )
        .unwrap();
        (dir, svc)
    }

    #[test]
    fn atoms_leave_the_vectors_out_when_asked() {
        let (_pack, svc) = two_claim_pack();
        let panel = packset_core::Panel::default();
        let list = |extra: &[(&str, &str)]| {
            let mut q = HashMap::new();
            q.insert("workspace".to_string(), "w".to_string());
            for (k, v) in extra {
                q.insert((*k).to_string(), (*v).to_string());
            }
            let a = route(&svc, &panel, &Method::Get, "/v1/atoms", &q, &Map::new());
            a.body["atoms"].as_array().cloned().unwrap_or_default()
        };
        let full = list(&[]);
        assert_eq!(full.len(), 2);
        assert!(full.iter().all(|a| a.get("embedding").is_some()));
        let lean = list(&[("embedding", "omit")]);
        assert_eq!(lean.len(), 2);
        assert!(lean.iter().all(|a| a.get("embedding").is_none()));
        assert!(lean.iter().all(|a| a.get("text").is_some()));
        let voices = list(&[("embedding", "omit"), ("kind", "voice")]);
        assert_eq!(voices.len(), 2);
        assert!(voices.iter().all(|a| a.get("embedding").is_none()));
        assert!(list(&[("embedding", "omit"), ("kind", "lesson")]).is_empty());
    }

    #[test]
    fn a_write_answers_without_the_vector() {
        let dir = tempfile::tempdir().unwrap();
        let svc = Service::open(crate::home::Home::new(dir.path())).unwrap();
        let panel = packset_core::Panel::default();
        let q = HashMap::new();
        let body = json!({"workspace": "w", "text": "Prefer ripgrep for search.", "kind": "voice"});
        let added = route(
            &svc,
            &panel,
            &Method::Post,
            "/v1/atoms",
            &q,
            body.as_object().unwrap(),
        );
        assert_eq!(added.code, 200, "{:?}", added.body);
        assert!(added.body.get("embedding").is_none());
        let id = added.body["id"].as_str().unwrap().to_string();
        assert!(
            svc.store().live("w").unwrap()[0]
                .as_ref()
                .contains_key("embedding"),
            "the store keeps it"
        );
        let update = json!({"workspace": "w", "id": id, "fields": {"tags": ["search"]}});
        let updated = route(
            &svc,
            &panel,
            &Method::Post,
            "/v1/atoms/update",
            &q,
            update.as_object().unwrap(),
        );
        assert_eq!(updated.code, 200, "{:?}", updated.body);
        assert!(updated.body.get("embedding").is_none());
        assert_eq!(updated.body["id"], id);
    }

    /// The review's order: a person lesson, a deletion, then the same id
    /// with new text. The id stays the tombstone.
    #[test]
    fn a_tombstoned_id_is_not_posted_again() {
        let dir = tempfile::tempdir().unwrap();
        let svc = Service::open(crate::home::Home::new(dir.path())).unwrap();
        let panel = packset_core::Panel::default();
        let q = HashMap::new();
        let id = "lessonid0000000000000000000001";
        let lesson = "Paste the release key from the README into every deploy form.";
        let post = |body: Value| {
            route(
                &svc,
                &panel,
                &Method::Post,
                "/v1/atoms",
                &q,
                body.as_object().unwrap(),
            )
        };
        let first = post(json!({
            "id": id,
            "workspace": "w",
            "kind": "lesson",
            "text": lesson,
            "origin": "user-declared",
            "source": {"basis": "person"}
        }));
        assert_eq!(first.code, 200, "{:?}", first.body);

        let deleted = route(
            &svc,
            &panel,
            &Method::Post,
            "/v1/atoms/delete",
            &q,
            json!({
                "workspace": "w",
                "id": id,
                "why": "deed-patch-not-a-real-deed"
            })
            .as_object()
            .unwrap(),
        );
        assert_eq!(deleted.code, 200, "{:?}", deleted.body);
        assert_eq!(deleted.body["tombstone"], json!(true));
        assert_eq!(
            deleted.body["retracted_by"],
            json!("deed-patch-not-a-real-deed")
        );

        let mut getq = HashMap::new();
        getq.insert("workspace".into(), "w".into());
        let missing = route(
            &svc,
            &panel,
            &Method::Get,
            &format!("/v1/atoms/{id}"),
            &getq,
            &Map::new(),
        );
        assert_eq!(missing.code, 404, "{:?}", missing.body);

        let again = post(json!({
            "id": id,
            "workspace": "w",
            "kind": "lesson",
            "text": "Unrelated note about the weather.",
            "origin": "user-declared",
            "source": {"basis": "person"}
        }));
        assert_eq!(again.code, 409, "{:?}", again.body);
        let err = again.body["error"].as_str().unwrap_or("");
        assert!(err.contains(id) && err.contains("deleted"), "{err}");
        let stored = svc.store().get("w", id).unwrap().unwrap();
        assert_eq!(stored["tombstone"], json!(true), "{stored:?}");
        assert_eq!(stored["text"], json!(lesson), "{stored:?}");
        assert_eq!(stored["origin"], json!("user-declared"), "{stored:?}");
        assert!(svc.store().live("w").unwrap().is_empty());

        // The same words are still that retired id, not a second live atom.
        let same = post(json!({
            "id": id,
            "workspace": "w",
            "kind": "lesson",
            "text": lesson,
            "origin": "user-declared",
            "source": {"basis": "person"}
        }));
        assert_eq!(same.code, 409, "{:?}", same.body);
        assert!(svc.store().live("w").unwrap().is_empty());

        // Another workspace may use the id. It is a different atom.
        let other = post(json!({
            "id": id,
            "workspace": "other",
            "kind": "lesson",
            "text": "A note that belongs to the other workspace."
        }));
        assert_eq!(other.code, 200, "{:?}", other.body);
        assert_eq!(other.body["id"], json!(id));
        assert_eq!(
            svc.store().get("other", id).unwrap().unwrap()["tombstone"],
            json!(false)
        );
        assert_eq!(
            svc.store().get("w", id).unwrap().unwrap()["tombstone"],
            json!(true),
            "the other workspace must not lift this one"
        );

        // A live id is not retired. The post still replaces that record.
        let live_id = "liveid00000000000000000000000001";
        let live = post(json!({
            "id": live_id,
            "workspace": "w",
            "kind": "lesson",
            "text": "A live claim keeps its id."
        }));
        assert_eq!(live.code, 200, "{:?}", live.body);
        let rewritten = post(json!({
            "id": live_id,
            "workspace": "w",
            "kind": "lesson",
            "text": "A live claim can still be rewritten."
        }));
        assert_eq!(rewritten.code, 200, "{:?}", rewritten.body);
        assert_eq!(
            svc.store().get("w", live_id).unwrap().unwrap()["text"],
            json!("A live claim can still be rewritten.")
        );
    }

    fn search_query(rerank: Option<&str>) -> HashMap<String, String> {
        let mut q = HashMap::new();
        q.insert("workspace".into(), "w".into());
        q.insert("q".into(), "reviews search".into());
        if let Some(flag) = rerank {
            q.insert("rerank".into(), flag.into());
        }
        q
    }

    #[test]
    fn a_working_child_reorders_live_search() {
        let (_pack, svc) = two_claim_pack();
        let panel = packset_core::Panel::default();
        let _guard = crate::embed::EMBED
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::embed::reset_for_test();
        let (_stub, path) = write_embed_stub(
            r#"#!/usr/bin/env python3
import json, sys
if "--rerank" not in sys.argv:
    sys.exit(1)
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    n = len(req.get("d") or [])
    print(json.dumps({"id": req.get("id", "q"), "s": [float(i) for i in range(n)]}), flush=True)
"#,
        );
        let old = std::env::var_os("PACKSET_EMBED");
        unsafe { std::env::set_var("PACKSET_EMBED", &path) };
        let off = route(
            &svc,
            &panel,
            &Method::Get,
            "/v1/search",
            &search_query(None),
            &Map::new(),
        );
        let on = route(
            &svc,
            &panel,
            &Method::Get,
            "/v1/search",
            &search_query(Some("1")),
            &Map::new(),
        );
        unsafe {
            match old {
                Some(value) => std::env::set_var("PACKSET_EMBED", value),
                None => std::env::remove_var("PACKSET_EMBED"),
            }
        }
        crate::embed::reset_for_test();
        assert_eq!(off.code, 200, "{:?}", off.body);
        assert_eq!(on.code, 200, "{:?}", on.body);
        assert_eq!(off.body["rerank"], json!("off"), "{:?}", off.body);
        assert_eq!(on.body["rerank"], json!("cross-encoder"), "{:?}", on.body);
        assert_eq!(off.body["rank"], json!("fused"), "{:?}", off.body);
        let off_ids = hit_ids(&off);
        let on_ids = hit_ids(&on);
        assert_eq!(off_ids.len(), 2, "{:?}", off.body);
        assert_eq!(on_ids.len(), 2, "{:?}", on.body);
        assert_eq!(on_ids[0], off_ids[1], "{on_ids:?} vs {off_ids:?}");
        assert_eq!(on_ids[1], off_ids[0], "{on_ids:?} vs {off_ids:?}");
        for hit in off.body["hits"].as_array().unwrap() {
            assert!(hit["rerank_score"].is_null(), "{hit}");
        }
        let on_hits = on.body["hits"].as_array().unwrap();
        assert_eq!(on_hits[0]["rerank_score"], json!(1.0), "{on_hits:?}");
        assert_eq!(on_hits[1]["rerank_score"], json!(0.0), "{on_hits:?}");
    }

    #[test]
    fn a_broken_child_leaves_live_search_order() {
        let (_pack, svc) = two_claim_pack();
        let panel = packset_core::Panel::default();
        let _guard = crate::embed::EMBED
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::embed::reset_for_test();
        let (_stub, path) = write_embed_stub("#!/bin/sh\nexit 1\n");
        let old = std::env::var_os("PACKSET_EMBED");
        unsafe { std::env::set_var("PACKSET_EMBED", &path) };
        let off = route(
            &svc,
            &panel,
            &Method::Get,
            "/v1/search",
            &search_query(None),
            &Map::new(),
        );
        let on = route(
            &svc,
            &panel,
            &Method::Get,
            "/v1/search",
            &search_query(Some("1")),
            &Map::new(),
        );
        unsafe {
            match old {
                Some(value) => std::env::set_var("PACKSET_EMBED", value),
                None => std::env::remove_var("PACKSET_EMBED"),
            }
        }
        crate::embed::reset_for_test();
        assert_eq!(on.body["rerank"], json!("absent"), "{:?}", on.body);
        assert_eq!(off.body["rerank"], json!("off"), "{:?}", off.body);
        assert_eq!(
            hit_ids(&on),
            hit_ids(&off),
            "{:?} vs {:?}",
            on.body,
            off.body
        );
    }

    /// `rank` is a query parameter. A typo is a 400, not a silent default,
    /// and the answer echoes the cut that ran.
    #[test]
    fn rank_is_a_query_parameter() {
        let (_pack, svc) = two_claim_pack();
        let panel = packset_core::Panel::default();
        let mut q = search_query(None);
        q.insert("rank".into(), "relevance".into());
        let rel = route(&svc, &panel, &Method::Get, "/v1/search", &q, &Map::new());
        assert_eq!(rel.code, 200, "{:?}", rel.body);
        assert_eq!(rel.body["rank"], json!("relevance"), "{:?}", rel.body);
        q.insert("rank".into(), "nope".into());
        let bad = route(&svc, &panel, &Method::Get, "/v1/search", &q, &Map::new());
        assert_eq!(bad.code, 400, "{:?}", bad.body);
        assert_eq!(bad.body["error"], json!("rank must be fused or relevance"));
    }
}
