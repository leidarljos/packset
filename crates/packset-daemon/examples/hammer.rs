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

use std::time::{Duration, Instant};

use packset_client::PacksetClient;

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let at = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[at]
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
                    let text = format!(
                        "Client {c} learned fact {n} in run {run}. It weighs {} grams.",
                        n * 7 + c
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
                        if client.search(&workspace, &q, 5).is_err() {
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
    if shared {
        // Every write was a distinct claim; the pack must hold them all.
        let client = PacksetClient::new(url);
        let live = client.atoms(&format!("hammer-{run}"))?.len();
        let written = clients * ops;
        println!("shared workspace: {live} live of {written} written");
        if live != written {
            eprintln!("{} writes lost or merged", written.saturating_sub(live));
            std::process::exit(1);
        }
    }
    if errors > 0 {
        std::process::exit(1);
    }
    Ok(())
}
