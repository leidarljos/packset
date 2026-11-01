//! Many clients at once against one writer: throughput, latency percentiles
//! and errors for a mixed remember/search load.
//!
//! ```console
//! $ PACKSET_URL=http://127.0.0.1:8761 cargo run --release -p packset-daemon --example hammer -- 16 200
//! ```
//!
//! The first argument is the number of clients, the second the operations
//! each performs. Every client remembers unique claims and searches for them,
//! two searches per remember, in its own workspace; with `HAMMER_SHARED=1`
//! every client writes into one workspace, as a herd of seats does, and the
//! run ends by counting the live claims there against what was written.
//! `HAMMER_RERANK=1` has every search ask for the cross-encoder. A probe asks
//! for the listing a status line reads, waits 250 ms between asks, and counts
//! the answers within 300 ms.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use packset_client::PacksetClient;

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let at = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[at]
}

/// A pronounceable pseudo-word from a seed, so claims share no tokens.
fn pseudo(seed: usize) -> String {
    const C: &[u8] = b"bdfgklmnprstvz";
    const V: &[u8] = b"aeiou";
    let mut x = seed.wrapping_mul(2_654_435_761) | 1;
    let mut word = String::new();
    for i in 0..6 {
        let set = if i % 2 == 0 { C } else { V };
        word.push(set[x % set.len()] as char);
        x /= set.len().max(2);
        x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
    }
    word
}

/// The listing without vectors, asked the way the status line asks it.
fn ask_like_a_status_line(url: &str, workspace: &str) -> Option<Duration> {
    let patience = Duration::from_millis(300);
    let started = Instant::now();
    let addr = url.trim_start_matches("http://").parse().ok()?;
    let mut stream = std::net::TcpStream::connect_timeout(&addr, patience).ok()?;
    stream.set_read_timeout(Some(patience)).ok()?;
    write!(
        stream,
        "GET /v1/atoms?workspace={workspace}&embedding=omit HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut answer = Vec::new();
    stream.read_to_end(&mut answer).ok()?;
    let took = started.elapsed();
    (answer.starts_with(b"HTTP/1.1 200") && took <= patience).then_some(took)
}

fn main() -> anyhow::Result<()> {
    let clients: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(8);
    let ops: usize = std::env::args()
        .nth(2)
        .and_then(|a| a.parse().ok())
        .unwrap_or(100);
    let url = std::env::var("PACKSET_URL").unwrap_or_else(|_| "http://127.0.0.1:8761".into());
    let run = std::process::id();
    let shared = std::env::var_os("HAMMER_SHARED").is_some();
    let rerank = std::env::var_os("HAMMER_RERANK").is_some();
    let done = Arc::new(AtomicBool::new(false));
    let probe = {
        let (url, done) = (url.clone(), Arc::clone(&done));
        let workspace = if shared {
            format!("hammer-{run}")
        } else {
            format!("hammer-{run}-0")
        };
        std::thread::spawn(move || {
            let mut asked = 0usize;
            let mut answered = Vec::new();
            while !done.load(Ordering::SeqCst) {
                asked += 1;
                answered.extend(ask_like_a_status_line(&url, &workspace));
                std::thread::sleep(Duration::from_millis(250));
            }
            (asked, answered)
        })
    };
    let started = Instant::now();
    let handles: Vec<_> = (0..clients)
        .map(|c| {
            let url = url.clone();
            std::thread::spawn(move || {
                let client = PacksetClient::new(url);
                let workspace = if shared {
                    format!("hammer-{run}")
                } else {
                    format!("hammer-{run}-{c}")
                };
                let mut writes = Vec::new();
                let mut reads = Vec::new();
                let mut errors = 0usize;
                for n in 0..ops {
                    // Pseudo-words distinct per claim: a herd's lessons
                    // differ, and two that shared their tokens would be one
                    // claim to the overlap rule, which is the rule working.
                    let text = format!(
                        "Client {c} learned fact {n} in run {run}: {} {} {}.",
                        pseudo(c * 7919 + n * 31 + 1),
                        pseudo(c * 7919 + n * 31 + 2),
                        pseudo(c * 7919 + n * 31 + 3)
                    );
                    // A seat's name rides in the entities, as ljos writes it.
                    let atom = serde_json::json!({
                        "schema": "inside.atom/v1", "kind": "lesson", "level": "explicit",
                        "text": text, "workspace": workspace,
                        "entities": [format!("seat:hammer-{c}")],
                    });
                    let t = Instant::now();
                    if client.post_atom(&atom).is_err() {
                        errors += 1;
                    }
                    writes.push(t.elapsed());
                    for q in [format!("fact {n}"), format!("client {c} grams")] {
                        let t = Instant::now();
                        if client.search_opts(&workspace, &q, 5, None, rerank).is_err() {
                            errors += 1;
                        }
                        reads.push(t.elapsed());
                    }
                }
                (writes, reads, errors)
            })
        })
        .collect();
    let mut writes = Vec::new();
    let mut reads = Vec::new();
    let mut errors = 0usize;
    for handle in handles {
        let (w, r, e) = handle.join().expect("client thread");
        writes.extend(w);
        reads.extend(r);
        errors += e;
    }
    let wall = started.elapsed();
    done.store(true, Ordering::SeqCst);
    let (asked, mut answered) = probe.join().expect("probe thread");
    answered.sort();
    writes.sort();
    reads.sort();
    let total = writes.len() + reads.len();
    println!(
        "{clients} clients x {ops} ops: {total} requests in {:.2}s, {:.0} req/s, {errors} errors",
        wall.as_secs_f64(),
        total as f64 / wall.as_secs_f64()
    );
    for (name, lat) in [("remember", &writes), ("search", &reads)] {
        println!(
            "{name:9} p50 {:>7.1?}  p95 {:>7.1?}  p99 {:>7.1?}  max {:>7.1?}",
            percentile(lat, 0.5),
            percentile(lat, 0.95),
            percentile(lat, 0.99),
            percentile(lat, 1.0)
        );
    }
    println!(
        "status line: {} of {asked} answered within 300 ms, p50 {:.1?}  p95 {:.1?}",
        answered.len(),
        percentile(&answered, 0.5),
        percentile(&answered, 0.95)
    );
    if shared {
        // Every write was a distinct claim: each is live, or closed by a
        // later one that says the same; none may vanish.
        let client = PacksetClient::new(url);
        let workspace = format!("hammer-{run}");
        let status = client.status(Some(&workspace))?;
        let live = status["live"].as_u64().unwrap_or(0) as usize;
        let closed = status["expired"].as_u64().unwrap_or(0) as usize;
        let written = clients * ops;
        println!("shared workspace: {live} live + {closed} closed of {written} written");
        if live + closed != written {
            eprintln!("{} writes lost", written.saturating_sub(live + closed));
            std::process::exit(1);
        }
    }
    if errors > 0 {
        std::process::exit(1);
    }
    Ok(())
}
