//! The `/v1` surface.
//!
//! Loopback only, and `127.0.0.1` rather than `localhost`: the name resolves
//! to whatever the resolver says, which on a dual-stack seat is not always the
//! interface the writer bound. The address is the contract.
//!
//! This layer decodes and encodes and nothing else. What a verb means lives in
//! [`crate::service`].

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use packset_core::record::AtomError;
use serde_json::{json, Map, Value};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::cards::WriteError;
use crate::service::Service;

/// The address the writer will bind, and no other.
pub const LOOPBACK: &str = "127.0.0.1";
/// The port the clients look for.
pub const DEFAULT_PORT: u16 = 8761;
/// Workers when `PACKSET_WORKERS` is unset. Not the core count.
pub const DEFAULT_WORKERS: usize = 4;
/// The ceiling on workers. 32 threads each kept a 64 MB malloc arena
/// (2 GB idle) on a 32-thread laptop.
pub const MAX_WORKERS: usize = 8;

/// How many requests this writer will answer at once.
///
/// A thread per connection is fine until something loops on the socket, and
/// then it is an unbounded number of threads on a seat that has other work to
/// do. A fixed pool pulling from one queue answers the same requests and makes
/// a burst wait instead of a machine swap.
#[must_use]
pub fn worker_count() -> usize {
    if let Some(raw) = std::env::var_os("PACKSET_WORKERS") {
        if let Some(n) = raw.to_str().and_then(|s| s.trim().parse::<usize>().ok()) {
            if n > 0 {
                return n.min(MAX_WORKERS);
            }
        }
    }
    DEFAULT_WORKERS
}

/// How many searches or writes may run at once.
///
/// One worker stays out of that work when the pool has more than one, so
/// `/health`, `/v1/status` and `/v1/workspaces` still answer during a burst.
/// A single worker cannot split. The pool does not grow, and [`MAX_WORKERS`]
/// stays the ceiling.
#[must_use]
pub fn dear_limit(workers: usize) -> usize {
    match workers {
        0 | 1 => 1,
        n => n - 1,
    }
}

/// Health, status and the workspace list. Everything else can encode, scan
/// or take the write lock, and waits on [`dear_limit`].
fn is_cheap(method: &Method, url: &str) -> bool {
    if method != &Method::Get {
        return false;
    }
    let path = url.split('?').next().unwrap_or(url);
    matches!(
        path,
        "/health" | "/v1/status" | "/v1/workspaces" | "/__inside_memd/health"
    )
}

/// Searches and writes in flight, and the ones waiting for a slot.
struct Lane<T> {
    limit: usize,
    in_flight: usize,
    parked: VecDeque<T>,
}

impl<T> Lane<T> {
    fn new(workers: usize) -> Self {
        Self {
            limit: dear_limit(workers),
            in_flight: 0,
            parked: VecDeque::new(),
        }
    }

    /// Run `job` now, or park it when the dear slots are full.
    fn begin(&mut self, job: T) -> Option<T> {
        if self.in_flight < self.limit {
            self.in_flight += 1;
            Some(job)
        } else {
            self.parked.push_back(job);
            None
        }
    }

    /// The next parked job, which keeps this slot. `None` releases it.
    fn end(&mut self) -> Option<T> {
        if let Some(next) = self.parked.pop_front() {
            Some(next)
        } else {
            self.in_flight = self.in_flight.saturating_sub(1);
            None
        }
    }

    fn parked(&self) -> usize {
        self.parked.len()
    }
}

fn lock_lane(lane: &Mutex<Lane<Request>>) -> std::sync::MutexGuard<'_, Lane<Request>> {
    lane.lock().unwrap_or_else(|e| e.into_inner())
}

/// Pull requests until the listener is gone. A cheap request runs on the
/// worker that took it. A search or a write runs only while a dear slot is
/// free; otherwise it waits, and this worker goes back to the queue.
fn worker<F>(server: &Server, lane: &Mutex<Lane<Request>>, mut run: F)
where
    F: FnMut(Request),
{
    while let Ok(request) = server.recv() {
        if is_cheap(request.method(), request.url()) {
            run(request);
            continue;
        }
        let admitted = {
            let mut lane = lock_lane(lane);
            lane.begin(request)
        };
        let Some(mut request) = admitted else {
            continue;
        };
        loop {
            run(request);
            let next = {
                let mut lane = lock_lane(lane);
                lane.end()
            };
            match next {
                Some(parked) => request = parked,
                None => break,
            }
        }
    }
}

fn spawn_workers<F>(
    server: &Arc<Server>,
    workers: usize,
    run: F,
) -> (Arc<Mutex<Lane<Request>>>, Vec<std::thread::JoinHandle<()>>)
where
    F: Fn(Request) + Send + Sync + 'static,
{
    let run = Arc::new(run);
    let lane = Arc::new(Mutex::new(Lane::new(workers)));
    let mut handles = Vec::with_capacity(workers);
    for _ in 0..workers {
        let server = Arc::clone(server);
        let run = Arc::clone(&run);
        let lane = Arc::clone(&lane);
        handles.push(std::thread::spawn(move || {
            worker(&server, &lane, |request| run(request));
        }));
    }
    (lane, handles)
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
pub fn serve(
    service: Arc<Service>,
    panel: packset_core::Panel,
    host: &str,
    port: u16,
) -> anyhow::Result<()> {
    if host != LOOPBACK {
        anyhow::bail!("packsetd listens on {LOOPBACK} only");
    }
    let server = Arc::new(
        Server::http((host, port))
            .map_err(|e| anyhow::anyhow!("cannot bind {host}:{port}: {e}"))?,
    );
    let workers = worker_count();
    eprintln!("packsetd: listening on http://{host}:{port} with {workers} workers");
    let panel = Arc::new(panel);

    // The pool is the balance, and it does not grow. A search or a write may
    // hold every worker but one. That one answers health, status and the
    // workspace list. The loop ends when the listener is gone, which is how
    // this process stops.
    let (_lane, handles) = spawn_workers(&server, workers, move |request| {
        handle(&service, &panel, request);
    });
    for handle in handles {
        let _ = handle.join();
    }
    Ok(())
}

fn handle(service: &Service, panel: &packset_core::Panel, mut request: Request) {
    let url = request.url().to_string();
    let (path, query) = split_query(&url);
    let method = request.method().clone();

    if method == Method::Get && path == "/health" {
        let response = Response::from_string("packsetd ok").with_header(text_plain());
        let _ = request.respond(response);
        return;
    }

    let body = if matches!(method, Method::Post | Method::Put) {
        match read_json(&mut request) {
            Ok(map) => map,
            Err(message) => {
                respond(request, &Answer::err(400, message));
                return;
            }
        }
    } else {
        Map::new()
    };

    let answer = route(service, panel, &method, path, &query, &body);
    respond(request, &answer);
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
                let lean = |atom: &Map<String, Value>| {
                    let mut atom = atom.clone();
                    if omit {
                        atom.remove("embedding");
                    }
                    Value::Object(atom)
                };
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
                        Ok(atoms) => {
                            let kind = query
                                .get("kind")
                                .map(String::as_str)
                                .filter(|k| !k.is_empty());
                            if kind.is_none() && !omit {
                                Answer::ok(json!({ "atoms": atoms.as_ref() }))
                            } else {
                                Answer::ok(json!({
                                    "atoms": atoms
                                        .iter()
                                        .filter(|a| kind.is_none_or(|k| {
                                            a.get("kind").and_then(Value::as_str) == Some(k)
                                        }))
                                        .map(lean)
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
                    Ok(Some(atom)) => match as_of_stamp(query) {
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
                match as_of_stamp(query) {
                    Err(a) => a,
                    Ok(at) => answer(service.search(
                        &workspace,
                        &q,
                        limit,
                        set.map(String::as_str),
                        panel,
                        at.as_deref(),
                        rerank,
                    )),
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
                    Ok(atoms) => Answer::ok(json!({
                        "atoms": packset_core::recall::recall(
                            &atoms,
                            &seeds,
                            &hints,
                            limit,
                            &packset_core::clock::utcnow(),
                        )
                    })),
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
            cheap_answer(service.accept(workspace, id).map(Value::Object))
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
        (Method::Post, "/v1/atoms") => answer(service.add(body.clone())),
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
            answer(service.update(workspace, id, fields))
        }
        (Method::Post, "/v1/atoms/delete") => {
            let (Some(workspace), Some(id)) = (
                body.get("workspace").and_then(Value::as_str),
                body.get("id").and_then(Value::as_str),
            ) else {
                return Answer::err(400, "workspace and id required");
            };
            let why = body.get("why").and_then(Value::as_str);
            answer(service.delete_atom(workspace, id, why).map(Value::Object))
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
            answer(service.grade(workspace, id, recalled))
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
fn answer<T: Into<Value>>(result: anyhow::Result<T>) -> Answer {
    match result {
        Ok(value) => Answer::ok(value.into()),
        Err(e) => Answer::err(400, root_message(&e)),
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

fn read_json(request: &mut Request) -> Result<Map<String, Value>, String> {
    let mut raw = String::new();
    request
        .as_reader()
        .read_to_string(&mut raw)
        .map_err(|e| e.to_string())?;
    if raw.trim().is_empty() {
        return Ok(Map::new());
    }
    let value: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
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

fn text_plain() -> Header {
    Header::from_bytes(&b"Content-Type"[..], &b"text/plain"[..]).expect("static header")
}

fn application_json() -> Header {
    Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).expect("static header")
}

fn respond(request: Request, answer: &Answer) {
    let body = serde_json::to_string(&answer.body).unwrap_or_else(|_| "{}".into());
    let response = Response::from_string(body)
        .with_status_code(answer.code)
        .with_header(application_json());
    let _ = request.respond(response);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_workers_is_four_not_core_count() {
        assert_eq!(DEFAULT_WORKERS, 4);
        assert_eq!(MAX_WORKERS, 8);
        assert_eq!(dear_limit(DEFAULT_WORKERS), 3);
        assert_eq!(dear_limit(1), 1);
    }

    #[test]
    fn health_status_and_workspaces_stay_off_the_search_lane() {
        assert!(is_cheap(&Method::Get, "/health"));
        assert!(is_cheap(&Method::Get, "/v1/status?workspace=seat"));
        assert!(is_cheap(&Method::Get, "/v1/workspaces"));
        assert!(is_cheap(&Method::Get, "/__inside_memd/health"));
        assert!(!is_cheap(
            &Method::Get,
            "/v1/search?workspace=seat&q=fusion"
        ));
        assert!(!is_cheap(&Method::Get, "/v1/atoms?workspace=seat"));
        assert!(!is_cheap(&Method::Post, "/health"));
        assert!(!is_cheap(&Method::Post, "/v1/atoms"));
    }

    #[test]
    fn a_full_pool_parks_the_next_search_until_one_finishes() {
        let mut lane = Lane::new(4);
        assert_eq!(lane.limit, 3);
        assert!(lane.begin(1).is_some());
        assert!(lane.begin(2).is_some());
        assert!(lane.begin(3).is_some());
        assert!(lane.begin(4).is_none());
        assert_eq!(lane.parked(), 1);
        assert_eq!(lane.in_flight, 3);
        assert_eq!(lane.end(), Some(4));
        assert_eq!(lane.in_flight, 3);
        assert_eq!(lane.end(), None);
        assert_eq!(lane.in_flight, 2);
    }

    /// Two workers, one dear slot. A health check returns while a search
    /// still holds that slot, and the second search stays parked.
    #[test]
    fn health_answers_while_a_search_holds_the_other_worker() {
        use std::io::{Read, Write};
        use std::net::TcpStream;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        const HOLD: Duration = Duration::from_millis(400);

        let server = Arc::new(Server::http("127.0.0.1:0").expect("bind"));
        let addr = match server.server_addr() {
            tiny_http::ListenAddr::IP(addr) => addr,
            #[allow(unreachable_patterns)]
            _ => panic!("packsetd listens on an ip"),
        };
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (entered_tx, entered_rx) = mpsc::channel();
        let running_h = Arc::clone(&running);
        let peak_h = Arc::clone(&peak);
        let (lane, handles) = spawn_workers(&server, 2, move |request| {
            let url = request.url().to_string();
            let path = url.split('?').next().unwrap_or(&url);
            if path == "/health" {
                let _ = request.respond(Response::from_string("ok"));
                return;
            }
            let now = running_h.fetch_add(1, Ordering::SeqCst) + 1;
            peak_h.fetch_max(now, Ordering::SeqCst);
            let _ = entered_tx.send(());
            std::thread::sleep(HOLD);
            running_h.fetch_sub(1, Ordering::SeqCst);
            let _ = request.respond(Response::from_string("search"));
        });

        fn get(addr: std::net::SocketAddr, path: &str) -> Duration {
            let started = Instant::now();
            let mut stream = TcpStream::connect(addr).expect("connect");
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("timeout");
            let req = format!("GET {path} HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n");
            stream.write_all(req.as_bytes()).expect("write");
            let mut buf = [0u8; 512];
            let n = stream.read(&mut buf).expect("read");
            let text = String::from_utf8_lossy(&buf[..n]);
            assert!(text.contains("200"), "{text}");
            started.elapsed()
        }

        let search = std::thread::spawn(move || get(addr, "/v1/search?workspace=seat&q=one"));
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("search started");
        let search_late = std::thread::spawn(move || get(addr, "/v1/search?workspace=seat&q=two"));
        let wait_parked = Instant::now();
        while lock_lane(&lane).parked() == 0 {
            assert!(
                wait_parked.elapsed() < Duration::from_secs(2),
                "the second search was not parked"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let health = get(addr, "/health");
        let late = search_late.join().expect("late search");
        let first = search.join().expect("first search");

        assert!(
            health < Duration::from_millis(200),
            "health waited {health:?} behind a search"
        );
        assert!(first >= HOLD, "the holding search returned in {first:?}");
        assert_eq!(peak.load(Ordering::SeqCst), 1);
        assert!(late >= HOLD, "the parked search returned in {late:?}");

        for _ in 0..handles.len() {
            server.unblock();
        }
        for handle in handles {
            let _ = handle.join();
        }
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
        let off_ids = hit_ids(&off);
        let on_ids = hit_ids(&on);
        assert_eq!(off_ids.len(), 2, "{:?}", off.body);
        assert_eq!(on_ids.len(), 2, "{:?}", on.body);
        assert_eq!(on_ids[0], off_ids[1], "{on_ids:?} vs {off_ids:?}");
        assert_eq!(on_ids[1], off_ids[0], "{on_ids:?} vs {off_ids:?}");
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
}
