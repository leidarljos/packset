//! What a pack costs as it grows, and the shape of the graph inside it.
//!
//! Two questions, because the answer to the second explains the first: a pack
//! whose atoms all link to each other is quadratic in its own size, and every
//! read pays for that whether or not the caller wanted the links.
//!
//! ```console
//! $ cargo run --release -p packset-daemon --example bench -- [sizes]
//! ```
//!
//! Sizes default to 50,200,1000,4000. This drives the service directly rather
//! than the socket, so it measures the writer and not the transport.

use std::time::Instant;

use packset_daemon::{Home, Service};
use serde_json::{json, Map, Value};

/// Subjects repeat often enough that the link rule fires. A corpus where
/// nothing overlaps would measure nothing.
const SUBJECTS: &[&str] = &[
    "parser",
    "header",
    "overlay",
    "ripgrep",
    "review",
    "manifest",
    "record",
    "token",
    "commit",
    "branch",
    "index",
    "atom",
    "workspace",
    "daemon",
    "search",
    "pack",
];
const WORKSPACE: &str = "git:github.com/HaoZeke/vissue";

fn atom(index: usize) -> Map<String, Value> {
    let subject = SUBJECTS[index % SUBJECTS.len()];
    json!({
        "workspace": WORKSPACE,
        "kind": "conclusion",
        "about_peer": "bench",
        "by_peer": "bench",
        "text": format!("The {subject} number {index} settled the question."),
        "entities": [subject, format!("n{index}")],
    })
    .as_object()
    .expect("object")
    .clone()
}

/// Median milliseconds over `reps` runs.
fn timed(mut call: impl FnMut(), reps: usize) -> f64 {
    let mut times: Vec<f64> = Vec::with_capacity(reps);
    for _ in 0..reps {
        let start = Instant::now();
        call();
        times.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(|a, b| a.partial_cmp(b).expect("no nan"));
    times[times.len() / 2]
}

fn main() -> anyhow::Result<()> {
    let sizes: Vec<usize> = std::env::args()
        .nth(1)
        .map(|raw| raw.split(',').filter_map(|s| s.parse().ok()).collect())
        .unwrap_or_else(|| vec![50, 200, 1000, 4000]);

    let dir = tempfile::tempdir()?;
    let service = Service::open(Home::new(dir.path()))?;

    println!(
        "{:>7} {:>9} {:>9} {:>9} {:>9} {:>9} {:>10} {:>9} {:>7}",
        "atoms", "search", "worst", "recall", "list", "write", "links", "per atom", "widest"
    );
    println!("{}", "-".repeat(84));

    let panel = packset_core::Panel::from_env()?;
    let mut made = 0usize;
    for target in sizes {
        while made < target {
            service.add(atom(made))?;
            made += 1;
        }

        let now = packset_core::clock::utcnow();
        let mut worst = 0.0f64;
        let search = timed(
            || {
                let start = Instant::now();
                service
                    .search(WORKSPACE, "parser", 16, None, &panel, None, false, None)
                    .expect("search");
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                if ms > worst {
                    worst = ms;
                }
            },
            20,
        );

        let recall = timed(
            || {
                let atoms = service.store().live(WORKSPACE).expect("live");
                let picked = packset_core::recall::recall(
                    &atoms,
                    &[],
                    &packset_core::recall::Hints::default(),
                    Some(64),
                    &now,
                    None,
                );
                std::hint::black_box(picked);
            },
            20,
        );

        let list = timed(
            || {
                let atoms = service.store().live(WORKSPACE).expect("live");
                let _ = serde_json::to_string(atoms.as_ref()).expect("json");
            },
            10,
        );

        // A write, then the first read after it, which rebuilds the snapshot.
        let mut counter = made;
        let write = timed(
            || {
                service.add(atom(counter)).expect("add");
                counter += 1;
                let start = Instant::now();
                service
                    .search(WORKSPACE, "parser", 16, None, &panel, None, false, None)
                    .expect("search");
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                if ms > worst {
                    worst = ms;
                }
            },
            10,
        );
        made = counter;

        let atoms = service.store().live(WORKSPACE)?;
        let degrees: Vec<usize> = atoms
            .iter()
            .map(|a| a.get("links").and_then(Value::as_array).map_or(0, Vec::len))
            .collect();
        let links: usize = degrees.iter().sum();
        // The widest neighbourhood, because the average hides the atom
        // everything links to, and that is the one a walk falls into.
        let widest = degrees.iter().copied().max().unwrap_or(0);
        let per_atom = if atoms.is_empty() {
            0.0
        } else {
            links as f64 / atoms.len() as f64
        };
        println!(
            "{made:>7} {search:>8.1}m {worst:>8.1}m {recall:>8.1}m {list:>8.1}m \
             {write:>8.1}m {links:>10} {per_atom:>9.1} {widest:>7}"
        );
    }
    Ok(())
}
