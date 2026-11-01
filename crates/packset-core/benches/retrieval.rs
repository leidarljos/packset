//! What a question costs against a pack of n atoms.
//!
//! Three things are measured because three things are paid on every search:
//! building the inverted index when the pack changed, scoring a query against
//! it, and fusing the ballots into one answer. The diversify slot is measured
//! by name because the DPP kernel is quadratic in the candidates and the
//! answer set is what bounds it.

use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use packset_core::bm25::{Index, Scorer};
use packset_core::panel::Panel;
use packset_core::search::{self, Ask, Record};
use serde_json::json;

const WORDS: &[&str] = &[
    "lease",
    "generation",
    "fence",
    "token",
    "holder",
    "quiet",
    "reclaim",
    "node",
    "graph",
    "deed",
    "accession",
    "manifest",
    "satchel",
    "proof",
    "head",
    "bridge",
    "passage",
    "window",
    "scorer",
    "ballot",
    "panel",
    "fuse",
    "voter",
    "tracker",
    "issue",
    "blocker",
    "parent",
    "plan",
    "claim",
    "prefer",
    "ripgrep",
    "search",
    "the",
    "a",
    "of",
    "and",
    "to",
    "for",
    "with",
    "over",
];

/// A deterministic pack: short written claims of twelve words.
fn pack(n: usize) -> Vec<Record> {
    let mut state = 0x9E37_79B9u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    (0..n)
        .map(|at| {
            let text: Vec<&str> = (0..12)
                .map(|_| WORDS[(next() % WORDS.len() as u64) as usize])
                .collect();
            json!({
                "id": format!("atom-{at}"),
                "workspace": "bench",
                "kind": "conclusion",
                "text": text.join(" "),
                "ts": "2026-09-10T00:00:00Z",
            })
            .as_object()
            .expect("object")
            .clone()
        })
        .collect()
}

fn ask<'a>(atoms: &'a [Record]) -> Ask<'a> {
    Ask {
        user: "",
        memory: "",
        atoms,
        query: "which lease token does the holder reclaim",
        limit: 20,
        set: None,
        kind: None,
        now: "2026-09-10T00:00:00Z",
    }
}

fn index_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("index build over n atoms");
    group.measurement_time(Duration::from_secs(6));
    for n in [1_000usize, 10_000, 100_000] {
        let atoms = pack(n);
        let docs: Vec<Vec<String>> = atoms.iter().map(search::atom_tokens).collect();
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| Index::build(docs.iter().map(Vec::as_slice)))
        });
    }
    group.finish();
}

fn query(c: &mut Criterion) {
    let mut group = c.benchmark_group("one question against n atoms");
    group.measurement_time(Duration::from_secs(6));
    for n in [1_000usize, 10_000, 100_000] {
        let atoms = pack(n);
        let docs: Vec<Vec<String>> = atoms.iter().map(search::atom_tokens).collect();
        let index = Index::build(docs.iter().map(Vec::as_slice));
        let asked = ask(&atoms);
        for scorer in [Scorer::Bm25, Scorer::Bm25Plus, Scorer::Dirichlet] {
            group.bench_with_input(BenchmarkId::new(scorer.token(), n), &n, |b, _| {
                b.iter(|| search::search_lexical(&asked, &index, scorer))
            });
        }
        group.bench_with_input(BenchmarkId::new("linear scan", n), &n, |b, _| {
            b.iter(|| search::search_linear(&asked))
        });
        // The path the writer takes: the atoms were tokenised to build the
        // index, and the scan scores by those tokens rather than making them
        // again.
        group.bench_with_input(
            BenchmarkId::new("linear scan, cached tokens", n),
            &n,
            |b, _| b.iter(|| search::search_linear_with(&asked, &docs)),
        );
    }
    group.finish();
}

fn fuse(c: &mut Criterion) {
    let mut group = c.benchmark_group("fuse two ballots of 20, diversified");
    let atoms = pack(10_000);
    let docs: Vec<Vec<String>> = atoms.iter().map(search::atom_tokens).collect();
    let index = Index::build(docs.iter().map(Vec::as_slice));
    let asked = ask(&atoms);
    let left = search::search_lexical(&asked, &index, Scorer::Bm25Plus);
    let right = search::search_linear(&asked);
    let ballots = [left, right];
    for diversify in ["mmr", "dpp", "none"] {
        let panel = Panel::named("combsum", diversify, "off").expect("panel");
        group.bench_function(diversify, |b| {
            b.iter(|| search::merge_ballots(&ballots, 20, &panel, asked.now))
        });
    }
    group.finish();
}

criterion_group!(benches, index_build, query, fuse);
criterion_main!(benches);
