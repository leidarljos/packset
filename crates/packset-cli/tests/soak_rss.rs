//! Anonymous RSS stops climbing once the parsed-workspace cache is full.
//!
//! A seat that opens another checkout used to keep every one of them parsed,
//! and the heap high-water mark stayed after the seat went quiet. This drives
//! a real packsetd across more workspaces than the default cache holds, waits
//! out the idle release, and fails if that memory is still growing.

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
