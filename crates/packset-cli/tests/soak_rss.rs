//! Anonymous RSS of a real packsetd.
//!
//! `resident_memory_stops_climbing_once_the_cache_is_full` is the cache cap:
//! more checkouts than the cache holds, then a quiet stretch. The field soak
//! is the other shape. Several clients write at once, a fake encoder returns
//! 384-d vectors, more workspaces are touched than the cache holds, claims
//! name a set, and attachments sit near the body cap. Idle release stays at
//! its default minute, and the clients do not pause for it. The run fails
//! when anonymous memory plus swap is still climbing after the warm-up.
//! The constant-corpus soak is the vector check the field soak cannot
//! make: one set of claims, a new vector each window, and a limit on
//! how much anonymous memory that set may cost.

#![cfg(target_os = "linux")]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;

/// `store::DEFAULT_WORKSPACE_CACHE`. The child is started without
/// `PACKSET_WORKSPACE_CACHE`, so a default that keeps every workspace
/// fails this test.
const HELD: usize = 8;
/// Checkouts opened after the cache is full. Twice the cap, so an
/// uncapped writer retains about twice what the first wave did.
const EXTRA: usize = 16;
const ATOMS: usize = 24;

struct Writer {
    child: Child,
    url: String,
    home: tempfile::TempDir,
}

impl Drop for Writer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Sample {
    rss: u64,
    anon: u64,
    swap: u64,
}

impl Sample {
    /// Heap that is still charged to the process, in swap or not.
    fn held(&self) -> u64 {
        self.anon + self.swap
    }
}

fn resident(pid: u32) -> Sample {
    let text = std::fs::read_to_string(format!("/proc/{pid}/status"))
        .unwrap_or_else(|err| panic!("status of {pid}: {err}"));
    let mut rss = 0;
    let mut anon = 0;
    let mut swap = 0;
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let kb = rest
            .split_whitespace()
            .next()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0);
        match key {
            "VmRSS" => rss = kb,
            "RssAnon" => anon = kb,
            "VmSwap" => swap = kb,
            _ => {}
        }
    }
    Sample { rss, anon, swap }
}

/// Wait out `PACKSET_IDLE_TRIM` and keep the lowest sample. The idle thread
/// wakes once a second, so the first read can still be the pre-trim number.
fn settled(pid: u32) -> Sample {
    std::thread::sleep(Duration::from_millis(2200));
    let mut best = resident(pid);
    for _ in 0..4 {
        std::thread::sleep(Duration::from_millis(400));
        let next = resident(pid);
        if next.held() < best.held() {
            best = next;
        }
    }
    best
}

fn writer() -> Writer {
    let home = tempfile::tempdir().unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_packsetd"));
    cmd.args(["--port", &port.to_string(), "--home"])
        .arg(home.path().join("pack"))
        .env("HOME", home.path())
        .env("PACKSET_AUTH", "off")
        .env("PACKSET_IDLE_TRIM", "1")
        .env("INSIDE_EMBED", "off")
        .env_remove("PACKSET_EMBED")
        .env_remove("PACKSET_MILLI")
        .env_remove("PACKSET_WORKSPACE_CACHE")
        .env_remove("PACKSET_SHAPE_CAP")
        .env_remove("PACKSET_FIRE_CAP")
        .env_remove("PACKSET_BACKFILL_CAP")
        .env_remove("PACKSET_HOME")
        .env_remove("PACKSET_URL")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().unwrap();
    let url = format!("http://127.0.0.1:{port}");
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if ureq::get(&format!("{url}/health")).call().is_ok() {
            return Writer { child, url, home };
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("packsetd did not answer on {url}");
}

/// Words that no other claim in the soak shares, so a write is a new atom
/// and not a rewrite of the one before it.
fn text(workspace: usize, n: usize) -> String {
    (0..36)
        .map(|word| format!("s{workspace}c{n}w{word}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn call(result: Result<ureq::Response, ureq::Error>, what: &str) {
    match result {
        Ok(response) => {
            response.into_string().expect(what);
        }
        Err(err) => panic!("{what}: {err}"),
    }
}

fn fill(url: &str, workspace: &str, index: usize) {
    for n in 0..ATOMS {
        call(
            ureq::post(&format!("{url}/v1/atoms")).send_json(json!({
                "workspace": workspace,
                "kind": "lesson",
                "origin": "user-declared",
                "level": "explicit",
                "text": text(index, n),
                "entities": ["seat:grok", format!("repo:soak-{index}")],
            })),
            "write",
        );
    }
    let q = format!("s{index}c0w0");
    call(
        ureq::get(&format!("{url}/v1/search"))
            .query("workspace", workspace)
            .query("q", &q)
            .query("limit", "8")
            .call(),
        "search",
    );
    call(
        ureq::get(&format!("{url}/v1/atoms"))
            .query("workspace", workspace)
            .query("embedding", "omit")
            .call(),
        "list",
    );
    call(
        ureq::get(&format!("{url}/v1/recall"))
            .query("workspace", workspace)
            .query("q", &q)
            .call(),
        "recall",
    );
    call(
        ureq::get(&format!("{url}/v1/activate"))
            .query("workspace", workspace)
            .query("q", &q)
            .query("fire", "1")
            .query("limit", "8")
            .call(),
        "activate",
    );
}

fn steady(url: &str, n: usize) {
    let workspace = format!("git:github.com/example/soak-{}", n % HELD);
    let q = format!("s{}c0w{}", n % HELD, n % ATOMS);
    call(
        ureq::get(&format!("{url}/v1/search"))
            .query("workspace", &workspace)
            .query("q", &q)
            .query("limit", "8")
            .call(),
        "steady search",
    );
    call(
        ureq::get(&format!("{url}/v1/atoms"))
            .query("workspace", &workspace)
            .query("embedding", "omit")
            .call(),
        "steady list",
    );
}

fn workspace(index: usize) -> String {
    format!("git:github.com/example/soak-{index}")
}

#[test]
fn resident_memory_stops_climbing_once_the_cache_is_full() {
    let writer = writer();
    assert!(writer.home.path().join("pack").is_dir());
    let pid = writer.child.id();
    let start = settled(pid);
    for index in 0..HELD {
        fill(&writer.url, &workspace(index), index);
    }
    let held = settled(pid);
    for index in HELD..(HELD + EXTRA) {
        fill(&writer.url, &workspace(index), index);
    }
    let extra = settled(pid);
    for n in 0..40 {
        steady(&writer.url, n);
    }
    let first = settled(pid);
    for n in 40..80 {
        steady(&writer.url, n);
    }
    let second = settled(pid);

    let filled = held.held().saturating_sub(start.held());
    let more = extra.held().saturating_sub(held.held());
    let climb = second.held().saturating_sub(first.held());
    let report = format!(
        "KiB held=anon+swap start={} held={} extra={} steady1={} steady2={} (rss {}, {}, {}, {}, {})",
        start.held(),
        held.held(),
        extra.held(),
        first.held(),
        second.held(),
        start.rss,
        held.rss,
        extra.rss,
        first.rss,
        second.rss,
    );
    // The second wave is twice as many checkouts as the first. An uncapped
    // writer keeps them and `more` is about twice `filled`. Past the cap the
    // cold checkouts are dropped, so they must come in under the wave that
    // stayed.
    assert!(
        filled > 0 && more < filled,
        "anon grew {more} KiB over {EXTRA} checkouts after the cache filled, against {filled} KiB for the first {HELD}; {report}"
    );
    // A steady stretch on the hot checkouts allocates nothing new. Two
    // windows in a row are the climb the laptop saw overnight.
    assert!(
        climb < 2048,
        "anon grew {climb} KiB across a steady stretch; {report}"
    );
}

/// Hot workspaces, the default cache. They are written after the extras
/// so they stay parsed and a wave patches them instead of rebuilding.
const HOT: usize = 8;
/// Workspaces beyond the cache, touched first in a wave so the hot set
/// evicts them. A wave does not fit.
const BEYOND: usize = 4;
/// Clients writing and searching at once. Health probes are not the load.
const CLIENTS: usize = 4;
/// Fresh claims each hot workspace receives per window. Enough that a
/// cache still holding the JSON vectors climbs by more than the noise.
const WAVE: usize = 8;
/// Attachments stored before the first window, past the slot cap of 64,
/// so a later window replaces slots instead of introducing them.
const WARM_ATTACH: usize = 72;
/// Just under `service::ATTACH_CAP`.
const ATTACH_NEAR: usize = 180_000;
/// Anonymous growth allowed from the first window to the third. The
/// same load with the vectors still stored as JSON climbed about 14
/// MiB. A float cache stays under this. A few mebibytes of the
/// allowance is the allocator, which does not return a busy thread's
/// high-water mark.
const FIELD_SLOPE_KIB: u64 = 12 * 1024;

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

fn write_encoder(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("fake-embed.py");
    std::fs::write(&path, FAKE_ENCODER).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

fn field_writer() -> Writer {
    let home = tempfile::tempdir().unwrap();
    let encoder = write_encoder(home.path());
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let bin = std::env::var("PACKSET_SOAK_BIN")
        .unwrap_or_else(|_| env!("CARGO_BIN_EXE_packsetd").to_string());
    let mut cmd = Command::new(bin);
    cmd.args(["--port", &port.to_string(), "--home"])
        .arg(home.path().join("pack"))
        .env("HOME", home.path())
        .env("PACKSET_AUTH", "off")
        .env("PACKSET_EMBED", &encoder)
        .env_remove("INSIDE_EMBED")
        .env_remove("PACKSET_IDLE_TRIM")
        .env_remove("PACKSET_MILLI")
        .env_remove("PACKSET_WORKSPACE_CACHE")
        .env_remove("PACKSET_SHAPE_CAP")
        .env_remove("PACKSET_FIRE_CAP")
        .env_remove("PACKSET_BACKFILL_CAP")
        .env_remove("PACKSET_HOME")
        .env_remove("PACKSET_URL")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().unwrap();
    let url = format!("http://127.0.0.1:{port}");
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if ureq::get(&format!("{url}/health")).call().is_ok() {
            return Writer { child, url, home };
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("packsetd did not answer on {url}");
}

fn field_workspace(index: usize) -> String {
    format!("git:github.com/example/field-{index}")
}

fn attach_workspace(index: usize) -> String {
    format!("git:github.com/example/attach-{index}")
}

fn post(url: &str, path: &str, body: serde_json::Value, what: &str) {
    call(ureq::post(&format!("{url}{path}")).send_json(body), what);
}

fn across(jobs: usize, work: &(impl Fn(usize) + Sync)) {
    std::thread::scope(|scope| {
        for worker in 0..CLIENTS {
            scope.spawn(move || {
                let mut index = worker;
                while index < jobs {
                    work(index);
                    index += CLIENTS;
                }
            });
        }
    });
}

fn write_claim(url: &str, workspace: usize, n: usize) {
    let set = if n % 2 == 0 { "notes" } else { "refs" };
    post(
        url,
        "/v1/atoms",
        json!({
            "workspace": field_workspace(workspace),
            "kind": "lesson",
            "origin": "user-declared",
            "level": "explicit",
            "set": set,
            "text": text(workspace, n),
            "entities": ["seat:grok", format!("repo:field-{workspace}")],
        }),
        "field write",
    );
}

fn write_wave(url: &str, wave: usize) {
    across(BEYOND, &|offset| {
        write_claim(url, HOT + offset, wave * WAVE);
    });
    across(HOT, &|workspace| {
        for n in 0..WAVE {
            write_claim(url, workspace, wave * WAVE + n);
        }
        let q = format!("s{workspace}c0w0");
        call(
            ureq::get(&format!("{url}/v1/search"))
                .query("workspace", &field_workspace(workspace))
                .query("q", &q)
                .query("limit", "8")
                .call(),
            "field search",
        );
    });
}

fn attach_wave(url: &str, body: &str, from: usize, count: usize) {
    across(count, &|offset| {
        let workspace = attach_workspace(from + offset);
        post(
            url,
            "/v1/attach",
            json!({
                "workspace": workspace,
                "label": "transcript",
                "text": body,
            }),
            "field attach",
        );
    });
}

/// Embeddings, set-scoped writes, attachments near the cap, and more
/// workspaces than the cache, under clients that do not let the writer
/// go idle. The warm-up fills the attach slots. The three windows keep
/// writing into the hot workspaces. Anonymous memory may move by a few
/// mebibytes as those workspaces are parsed. A slope past that is a
/// vector the cache kept.
#[test]
fn anonymous_memory_stays_flat_under_field_load() {
    let writer = field_writer();
    let pid = writer.child.id();
    let body = "x".repeat(ATTACH_NEAR);
    write_wave(&writer.url, 0);
    attach_wave(&writer.url, &body, 0, WARM_ATTACH);
    let listed = ureq::get(&format!("{}/v1/atoms", writer.url))
        .query("workspace", &field_workspace(0))
        .call()
        .expect("list")
        .into_string()
        .expect("list body");
    let parsed: serde_json::Value = serde_json::from_str(&listed).expect("list json");
    let atoms = parsed.get("atoms").cloned().unwrap_or(parsed);
    let embedded = atoms[0]["embedding"].as_array().map(Vec::len).unwrap_or(0);
    assert_eq!(
        embedded,
        384,
        "the fake encoder did not land a vector: {}",
        &listed.chars().take(240).collect::<String>()
    );
    let mut held = [0u64; 3];
    for (window, sample) in held.iter_mut().enumerate() {
        write_wave(&writer.url, window + 1);
        std::thread::sleep(Duration::from_millis(200));
        *sample = resident(pid).held();
    }
    let slope = held[2].saturating_sub(held[0]);
    let report = format!(
        "KiB anon+swap windows {} -> {} -> {} slope {slope}",
        held[0], held[1], held[2]
    );
    eprintln!("field soak {report}");
    assert!(
        slope < FIELD_SLOPE_KIB,
        "anon grew {slope} KiB across three windows; {report}"
    );
}

/// Workspaces that stay inside the default cache, so a rewrite patches
/// them instead of dropping the snapshot.
const STEADY_HOT: usize = 8;
/// Claims kept in each of those workspaces. The count does not change
/// between windows; only the text and its vector do.
const STEADY_CLAIMS: usize = 32;
/// Anonymous memory this corpus may add, from before the first write to
/// the last rewrite. The float cache stays under it. The same claims with
/// a JSON array on both record copies sit above it: measured near 24 MiB
/// against about 13 MiB here. A slope across rewrites does not separate
/// them, because a rewrite frees the previous array either way, and the
/// field soak's slope is the new claims plus one constant vector.
const STEADY_CORPUS_KIB: u64 = 18 * 1024;

const VARIED_ENCODER: &str = r#"#!/usr/bin/env python3
import hashlib, json, sys

def vec(text):
    digest = hashlib.sha256(text.encode()).digest()
    out = []
    for i in range(384):
        b = digest[i % 32]
        out.append(((b + 1) / 255.0) * (i + 1) / 384.0)
    return out

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    texts = msg.get("texts")
    if texts is None:
        reply = {"v": vec(msg.get("text") or "")}
    else:
        reply = {"vs": [vec(text) for text in texts]}
    sys.stdout.write(json.dumps(reply) + "\n")
    sys.stdout.flush()
"#;

fn steady_workspace(index: usize) -> String {
    format!("git:github.com/example/steady-{index}")
}

fn steady_text(workspace: usize, n: usize, window: usize) -> String {
    (0..16)
        .map(|word| format!("k{workspace}n{n}g{window}t{word}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn steady_writer() -> Writer {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("fake-embed.py");
    std::fs::write(&path, VARIED_ENCODER).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let bin = std::env::var("PACKSET_SOAK_BIN")
        .unwrap_or_else(|_| env!("CARGO_BIN_EXE_packsetd").to_string());
    let mut cmd = Command::new(bin);
    cmd.args(["--port", &port.to_string(), "--home"])
        .arg(home.path().join("pack"))
        .env("HOME", home.path())
        .env("PACKSET_AUTH", "off")
        .env("PACKSET_EMBED", &path)
        .env_remove("INSIDE_EMBED")
        .env_remove("PACKSET_IDLE_TRIM")
        .env_remove("PACKSET_MILLI")
        .env_remove("PACKSET_WORKSPACE_CACHE")
        .env_remove("PACKSET_SHAPE_CAP")
        .env_remove("PACKSET_FIRE_CAP")
        .env_remove("PACKSET_BACKFILL_CAP")
        .env_remove("PACKSET_HOME")
        .env_remove("PACKSET_URL")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().unwrap();
    let url = format!("http://127.0.0.1:{port}");
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if ureq::get(&format!("{url}/health")).call().is_ok() {
            return Writer { child, url, home };
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("packsetd did not answer on {url}");
}

fn write_steady(url: &str, window: usize) {
    for workspace in 0..STEADY_HOT {
        for n in 0..STEADY_CLAIMS {
            post(
                url,
                "/v1/atoms",
                json!({
                    "id": format!("s{workspace}n{n}"),
                    "workspace": steady_workspace(workspace),
                    "kind": "lesson",
                    "origin": "user-declared",
                    "level": "explicit",
                    "text": steady_text(workspace, n, window),
                    "entities": [format!("mark:k{workspace}n{n}")],
                }),
                "steady write",
            );
        }
    }
}

fn steady_count(url: &str, workspace: usize) -> (usize, Option<f64>) {
    let listed = ureq::get(&format!("{url}/v1/atoms"))
        .query("workspace", &steady_workspace(workspace))
        .call()
        .expect("steady list")
        .into_string()
        .expect("steady list body");
    let parsed: serde_json::Value = serde_json::from_str(&listed).expect("steady list json");
    let atoms = parsed
        .get("atoms")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let first = atoms.first().and_then(|atom| {
        atom.get("embedding")
            .and_then(|v| v.as_array())
            .and_then(|items| items.first())
            .and_then(|n| n.as_f64())
    });
    (atoms.len(), first)
}

/// One corpus, rewritten with a new vector each window. The claim count
/// stays put. What the cache spends on that set is the vector cost; a
/// JSON copy of every embedding does not fit under the limit.
#[test]
fn anonymous_memory_stays_flat_when_the_corpus_does() {
    let writer = steady_writer();
    let pid = writer.child.id();
    let base = resident(pid).held();
    write_steady(&writer.url, 0);
    let (count, first) = steady_count(&writer.url, 0);
    assert_eq!(
        count, STEADY_CLAIMS,
        "the corpus did not stay one set of claims"
    );
    assert!(first.is_some(), "the varied encoder did not land a vector");
    let mut held = [0u64; 3];
    let mut seen = [0.0f64; 3];
    for (window, sample) in held.iter_mut().enumerate() {
        write_steady(&writer.url, window + 1);
        let (count, first) = steady_count(&writer.url, 0);
        assert_eq!(count, STEADY_CLAIMS, "a rewrite changed the corpus size");
        seen[window] = first.unwrap_or(0.0);
        std::thread::sleep(Duration::from_millis(200));
        *sample = resident(pid).held();
    }
    assert!(
        seen[0] != seen[1] || seen[1] != seen[2],
        "the encoder returned one vector for every text: {seen:?}"
    );
    let grown = held[2].saturating_sub(base);
    let slope = held[2].saturating_sub(held[0]);
    let report = format!(
        "KiB anon+swap base {base} windows {} -> {} -> {} grown {grown} slope {slope}",
        held[0], held[1], held[2]
    );
    eprintln!("steady soak {report}");
    assert!(
        grown < STEADY_CORPUS_KIB,
        "anon grew {grown} KiB for a constant corpus; {report}"
    );
}
