//! Listings and searches must read an atom's record and its vector from
//! one snapshot. `live` and `vectors` used to be two cache lookups, so a
//! workspace evicted between them, or a scan that was not cached, answered
//! `embedding: null` and dropped the dense ballot.

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// More than `store::DEFAULT_WORKSPACE_CACHE`, so a rotation evicts.
const WORKSPACES: usize = 12;
const WRITERS: usize = 3;
/// Seeded claims per workspace. A scan of them overlaps a writer.
const SEED: usize = 8;

struct Writer {
    child: Child,
    url: String,
    home: tempfile::TempDir,
}

impl Drop for Writer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // The directory owns the encoder script and the pack. It has to
        // outlive the process, so the field is read here.
        let _ = self.home.path();
    }
}

const FAKE_ENCODER: &str = r#"#!/usr/bin/env python3
import json, sys
VEC = [0.01] * 384
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    texts = msg.get("texts")
    if texts is None:
        reply = {"v": VEC}
    else:
        reply = {"vs": [VEC for _ in texts]}
    sys.stdout.write(json.dumps(reply) + "\n")
    sys.stdout.flush()
"#;

fn workspace(index: usize) -> String {
    format!("git:github.com/example/rot-{index}")
}

fn text(workspace: usize, n: usize) -> String {
    (0..24)
        .map(|word| format!("r{workspace}c{n}w{word}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn start() -> Writer {
    let home = tempfile::tempdir().unwrap();
    let encoder = home.path().join("fake-embed.py");
    std::fs::write(&encoder, FAKE_ENCODER).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&encoder).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&encoder, perms).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let err_path = home.path().join("packsetd.err");
    let err = std::fs::File::create(&err_path).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_packsetd"));
    cmd.args(["--port", &port.to_string(), "--home"])
        .arg(home.path().join("pack"))
        .env("HOME", home.path())
        .env("PACKSET_AUTH", "off")
        .env("PACKSET_EMBED", &encoder)
        .env("PACKSET_IDLE_TRIM", "off")
        .env_remove("INSIDE_EMBED")
        .env_remove("PACKSET_MILLI")
        .env_remove("PACKSET_WORKSPACE_CACHE")
        .env_remove("PACKSET_HOME")
        .env_remove("PACKSET_URL")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(err));
    let mut child = cmd.spawn().unwrap();
    let url = format!("http://127.0.0.1:{port}");
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if ureq::get(&format!("{url}/health")).call().is_ok() {
            return Writer { child, url, home };
        }
        if child.try_wait().ok().flatten().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    let logged = std::fs::read_to_string(&err_path).unwrap_or_default();
    panic!("packsetd did not answer on {url}: {logged}");
}

fn post(url: &str, body: Value) {
    match ureq::post(&format!("{url}/v1/atoms")).send_json(body) {
        Ok(response) => {
            response.into_string().expect("write body");
        }
        Err(err) => panic!("write: {err}"),
    }
}

fn seed(url: &str) {
    for index in 0..WORKSPACES {
        for n in 0..SEED {
            post(
                url,
                json!({
                    "workspace": workspace(index),
                    "kind": "lesson",
                    "origin": "user-declared",
                    "level": "explicit",
                    "text": text(index, n),
                    "entities": ["seat:grok", format!("repo:rot-{index}")],
                }),
            );
        }
    }
}

fn listing(url: &str, index: usize) -> Vec<Value> {
    let response = ureq::get(&format!("{url}/v1/atoms"))
        .query("workspace", &workspace(index))
        .call()
        .unwrap_or_else(|err| panic!("list {index}: {err}"));
    let text = response.into_string().expect("list body");
    let parsed: Value = serde_json::from_str(&text).expect("list json");
    parsed
        .get("atoms")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn has_vector(atom: &Value) -> bool {
    atom.get("embedding")
        .and_then(Value::as_array)
        .is_some_and(|items| items.len() == 384 && items.iter().all(|item| item.as_f64().is_some()))
}

/// Twelve workspaces rotate through a cache of eight while three writers
/// keep committing. A listing or a search that reads the records and the
/// vectors separately loses the embedding.
#[test]
fn listings_and_searches_keep_every_embedding() {
    let writer = start();
    seed(&writer.url);
    let sample = listing(&writer.url, 0);
    assert!(
        sample.len() >= SEED && sample.iter().all(has_vector),
        "the encoder did not land before the race: n={}",
        sample.len()
    );

    let url = writer.url.clone();
    let readers_done = Arc::new(AtomicUsize::new(0));
    let lost_atoms = Arc::new(AtomicUsize::new(0));
    let seen_atoms = Arc::new(AtomicUsize::new(0));
    let lost_searches = Arc::new(AtomicUsize::new(0));
    let seen_searches = Arc::new(AtomicUsize::new(0));

    std::thread::scope(|scope| {
        for writer_id in 0..WRITERS {
            let url = url.clone();
            let readers_done = Arc::clone(&readers_done);
            scope.spawn(move || {
                let mut n = 0usize;
                while readers_done.load(Ordering::Relaxed) < 2 {
                    let index = (writer_id + n) % WORKSPACES;
                    post(
                        &url,
                        json!({
                            "workspace": workspace(index),
                            "kind": "lesson",
                            "origin": "user-declared",
                            "level": "explicit",
                            "text": text(index, SEED + n + writer_id * 10_000),
                            "entities": ["seat:grok", format!("repo:rot-{index}")],
                        }),
                    );
                    n += 1;
                }
            });
        }

        let url_list = url.clone();
        let lost_atoms = Arc::clone(&lost_atoms);
        let seen_atoms = Arc::clone(&seen_atoms);
        let readers_done_list = Arc::clone(&readers_done);
        scope.spawn(move || {
            for _ in 0..24 {
                for index in 0..WORKSPACES {
                    for atom in listing(&url_list, index) {
                        seen_atoms.fetch_add(1, Ordering::Relaxed);
                        if !has_vector(&atom) {
                            lost_atoms.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
            readers_done_list.fetch_add(1, Ordering::Relaxed);
        });

        let url_search = url.clone();
        let lost_searches = Arc::clone(&lost_searches);
        let seen_searches = Arc::clone(&seen_searches);
        let readers_done_search = Arc::clone(&readers_done);
        scope.spawn(move || {
            for n in 0..300 {
                let index = n % WORKSPACES;
                let response = ureq::get(&format!("{url_search}/v1/search"))
                    .query("workspace", &workspace(index))
                    .query("q", &format!("r{index}c0w0"))
                    .query("limit", "5")
                    .call()
                    .unwrap_or_else(|err| panic!("search {index}: {err}"));
                let text = response.into_string().expect("search body");
                let parsed: Value = serde_json::from_str(&text).expect("search json");
                seen_searches.fetch_add(1, Ordering::Relaxed);
                let hits = parsed.get("hits").and_then(Value::as_array);
                let dense = hits.is_some_and(|hits| {
                    !hits.is_empty() && hits.iter().any(|hit| hit.get("dense_rank").is_some())
                });
                if !dense {
                    lost_searches.fetch_add(1, Ordering::Relaxed);
                }
            }
            readers_done_search.fetch_add(1, Ordering::Relaxed);
        });
    });

    let lost_atoms = lost_atoms.load(Ordering::Relaxed);
    let seen_atoms = seen_atoms.load(Ordering::Relaxed);
    let lost_searches = lost_searches.load(Ordering::Relaxed);
    let seen_searches = seen_searches.load(Ordering::Relaxed);
    assert!(
        seen_atoms > 0 && seen_searches == 300,
        "the race did not run"
    );
    assert!(
        lost_atoms == 0 && lost_searches == 0,
        "{lost_atoms} of {seen_atoms} listed atoms came back without a vector; {lost_searches} of {seen_searches} searches lost the dense ballot"
    );
}
