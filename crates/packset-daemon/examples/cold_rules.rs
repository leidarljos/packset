//! What a cold rule lookup costs in a workspace that has forgotten claims.
//!
//! A seat asks for its rules (`GET /v1/atoms?kind=rule`) before a tool runs.
//! The first ask after the writer opens, or after the workspace has left the
//! parsed cache, reads the workspace from the store. This builds a pack of
//! live rules beside claims the live cap forgot, then opens it again and
//! again and times that first ask each time.
//!
//! ```console
//! $ cargo run --release -p packset-daemon --example cold_rules -- [tombstones] [home]
//! ```
//!
//! A `home` that already holds a pack is read as it stands, so a pack an
//! older writer left can be timed on this one. Otherwise the pack is built
//! in a temporary home. The first opening in a process also returns the
//! heap the build left to the system, so the median is the number to read.

use std::time::Instant;

use packset_daemon::{Home, Service, Store};
use serde_json::{json, Map, Value};

const WORKSPACE: &str = "git:github.com/leidarljos/packset";
/// The ticket's pack: ten thousand tombstones.
const DEFAULT_TOMBSTONES: usize = 10_000;
/// A seat's own deny and ask rules. The count changes the size of the
/// answer, not what the tombstones beside it cost.
const RULES: usize = 32;
/// bge-small-en-v1.5, the encoder's default model, writes 384 floats.
const DIM: usize = 384;
/// Openings timed. Odd, so the median is one of them.
const ROUNDS: usize = 7;

fn object(value: Value) -> Map<String, Value> {
    value.as_object().cloned().expect("an object")
}

fn rule(n: usize) -> Map<String, Value> {
    object(json!({
        "workspace": WORKSPACE,
        "id": format!("rule{n:04}"),
        "kind": "rule",
        "text": format!("Ask before running tool number {n}."),
        "pattern": format!("tool-{n} *"),
        "verdict": "ask",
        "tombstone": false,
    }))
}

/// A lesson written a year ago and never reviewed, with a vector stored the
/// way the encoder's are: f32 components of a unit-scale vector, written as
/// JSON numbers. Only the stored length matters here, not the direction.
fn lesson(n: usize, rng: &mut u64) -> Map<String, Value> {
    let vector: Vec<Value> = (0..DIM)
        .map(|_| {
            *rng ^= *rng << 13;
            *rng ^= *rng >> 7;
            *rng ^= *rng << 17;
            // The top 24 bits, as many as an f32 holds exactly, in [-1, 1).
            let unit = (*rng >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0;
            json!(unit / (DIM as f32).sqrt())
        })
        .collect();
    object(json!({
        "workspace": WORKSPACE,
        "id": format!("lesson{n:06}"),
        "kind": "lesson",
        "origin": "agent-derived",
        "source": {"via": "ljos", "seat": "bench", "basis": "agent"},
        "text": format!("Lesson number {n} was written once and never asked for again."),
        "ts": "2025-11-01T00:00:00.000Z",
        "review": {"last": "2025-11-01T00:00:00.000Z", "stability": 1.0},
        "embedding": vector,
        "tombstone": false,
    }))
}

fn build(root: &std::path::Path, tombstones: usize) -> anyhow::Result<()> {
    let svc = Service::open(Home::new(root))?.with_live_cap(0);
    let rules: Vec<_> = (0..RULES).map(rule).collect();
    svc.store().upsert_many(&rules)?;
    // Any nonzero seed starts xorshift; a fixed one builds the same pack twice.
    let mut rng = 0x9e37_79b9_7f4a_7c15_u64;
    let lessons: Vec<_> = (0..tombstones).map(|n| lesson(n, &mut rng)).collect();
    svc.store().upsert_many(&lessons)?;
    // The live cap is how a pack comes to hold this many tombstones: one
    // write over the cap forgets the least retrievable lessons.
    let svc = svc.with_live_cap(RULES + 1);
    let fresh = object(json!({
        "workspace": WORKSPACE,
        "kind": "lesson",
        "text": "The newest lesson pushes the pack over its cap.",
    }));
    let written = svc.add(fresh)?;
    println!(
        "built: {RULES} rules, {} lessons forgotten by the live cap",
        written.get("forgot").and_then(Value::as_u64).unwrap_or(0)
    );
    Ok(())
}

fn describe(root: &std::path::Path) -> anyhow::Result<()> {
    let store = Store::open(root)?;
    let records = store.scan(Some(WORKSPACE))?;
    let tombs: Vec<&Map<String, Value>> = records
        .iter()
        .filter(|r| r.get("tombstone").and_then(Value::as_bool) == Some(true))
        .collect();
    let with_vector = tombs
        .iter()
        .filter(|r| {
            r.get("embedding")
                .and_then(Value::as_array)
                .is_some_and(|v| !v.is_empty())
        })
        .count();
    let with_text = tombs.iter().filter(|r| r.contains_key("text")).count();
    let bytes: usize = records
        .iter()
        .map(|r| serde_json::to_vec(r).map_or(0, |b| b.len()))
        .sum();
    let tomb_bytes: usize = tombs
        .iter()
        .map(|r| serde_json::to_vec(r).map_or(0, |b| b.len()))
        .sum();
    println!(
        "pack: {} records, {} tombstones ({with_vector} with a vector, {with_text} with text)",
        records.len(),
        tombs.len()
    );
    println!(
        "stored JSON: {:.1} MiB, of which tombstones {:.1} MiB ({} bytes each)",
        bytes as f64 / 1_048_576.0,
        tomb_bytes as f64 / 1_048_576.0,
        tomb_bytes.checked_div(tombs.len()).unwrap_or(0)
    );
    Ok(())
}

/// Open the store and time the first rule lookup, as `/v1/atoms` answers
/// `kind=rule`: the live snapshot, narrowed to rules.
fn cold_lookup(root: &std::path::Path) -> anyhow::Result<(f64, usize)> {
    let store = Store::open(root)?;
    let started = Instant::now();
    let snap = store.live_versioned(WORKSPACE)?;
    let rules: Vec<Value> = snap
        .atoms
        .iter()
        .filter(|a| a.get("kind").and_then(Value::as_str) == Some("rule"))
        .map(|a| Value::Object(Map::clone(a)))
        .collect();
    Ok((started.elapsed().as_secs_f64() * 1e3, rules.len()))
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let tombstones = args
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(DEFAULT_TOMBSTONES);
    let kept = args.next().map(std::path::PathBuf::from);
    let scratch = tempfile::tempdir()?;
    let root = kept.clone().unwrap_or_else(|| scratch.path().to_path_buf());
    if !root.join("memory.lmdb").exists() {
        build(&root, tombstones)?;
    }
    let mut times = Vec::with_capacity(ROUNDS);
    for round in 1..=ROUNDS {
        let (ms, rules) = cold_lookup(&root)?;
        println!("opening {round}: {rules} rules in {ms:.2} ms");
        times.push(ms);
    }
    times.sort_by(f64::total_cmp);
    println!(
        "cold rule lookup: median {:.2} ms, fastest {:.2} ms, slowest {:.2} ms over {ROUNDS} openings",
        times[ROUNDS / 2],
        times[0],
        times[ROUNDS - 1]
    );
    describe(&root)
}
