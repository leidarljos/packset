//! A synthetic longitudinal corpus for the decay slot: does a claim the seat
//! kept recalling outrank paraphrases it wrote later and never used?
//!
//! Every topic has four claims that share its words. One, written early, is
//! recalled on its review clock across the run; the other three are written
//! late and never reviewed. At the end the topic is asked as a cue. Recency
//! favours the late paraphrases, the review clock favours the kept claim, and
//! a lexical scorer cannot tell them apart. LoCoMo and LongMemEval carry no
//! review history, so this is the corpus that can measure the slot at all.
//!
//! Beside the three decay slots two orderings the review clock competes
//! with: LRU, the newest hit first, which is what a cache would do; and
//! keep-testing, the recall path's own order, which spends the budget on
//! due claims before the neighbourhood. `FORGETTING_JSON=path` writes the
//! table as JSON beside the printed one.
//!
//! ```console
//! $ cargo run --release -p packset-daemon --example forgetting
//! $ FORGETTING_JSON=results/forgetting.json cargo run --release -p packset-daemon --example forgetting
//! ```

use packset_core::bm25::Index;
use packset_core::panel::Panel;
use packset_core::recall::{recall, Hints};
use packset_core::record::{schedule_review, Grade};
use packset_core::search::{atom_tokens, merge_ballots, Record};
use serde_json::{json, Value};

const TOPICS: usize = 300;
const PARAPHRASES: usize = 3;
const DAYS: i64 = 180;
const BASE: &str = "2026-01-01T00:00:00.000Z";

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn day(t: i64) -> String {
    packset_core::clock::shift(BASE, t * 86_400).expect("a stamp")
}

fn claim(topic: usize, variant: usize, written: i64) -> Record {
    let text = format!(
        "On topic{topic} the settled answer is variant{variant}. It was checked against detail{variant}."
    );
    let mut atom: Record = json!({
        "id": format!("t{topic}-v{variant}"),
        "field": "atom",
        "kind": "lesson",
        "text": text,
        "ts": day(written),
    })
    .as_object()
    .cloned()
    .expect("an object");
    schedule_review(&mut atom, &day(written), Grade::Initial, None);
    atom
}

/// Recall the kept claim whenever it falls due, up to `until`.
fn keep_recalling(atom: &mut Record, until: i64) {
    for _ in 0..64 {
        let due = atom["due_at"].as_str().unwrap_or("").to_string();
        let Some(due_ms) = packset_core::clock::parse_millis(&due) else {
            break;
        };
        let base_ms = packset_core::clock::parse_millis(BASE).unwrap_or(0);
        let due_day = (due_ms - base_ms) / 86_400_000;
        if due_day > until {
            break;
        }
        schedule_review(atom, &due, Grade::Recalled, None);
    }
}

fn hits_for(cue: &[String], index: &Index, atoms: &[Record]) -> Vec<Value> {
    let mut scored = index.score(cue);
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    scored
        .into_iter()
        .take(20)
        .map(|(ordinal, score)| {
            let mut hit = atoms[ordinal].clone();
            hit.insert("score".into(), json!(score));
            Value::Object(hit)
        })
        .collect()
}

fn main() -> anyhow::Result<()> {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut atoms: Vec<Record> = Vec::new();
    for topic in 0..TOPICS {
        let early = rng.below(30) as i64;
        let mut kept = claim(topic, 0, early);
        keep_recalling(&mut kept, DAYS);
        let mut group = vec![kept];
        for variant in 1..=PARAPHRASES {
            let late = 150 + rng.below(30) as i64;
            group.push(claim(topic, variant, late));
        }
        // Stored in a random order, so a tie broken by position does not
        // hand the kept claim the first place for free.
        for i in (1..group.len()).rev() {
            let j = rng.below(i as u64 + 1) as usize;
            group.swap(i, j);
        }
        atoms.extend(group);
    }
    let documents: Vec<Vec<String>> = atoms.iter().map(atom_tokens).collect();
    let index = Index::build(documents.iter().map(Vec::as_slice));
    let now = day(DAYS);
    let panels = [
        (
            "off (lexical only)",
            Panel::named("combmnz", "none", "off")?,
        ),
        (
            "on (14-day half-life on age)",
            Panel::named("combmnz", "none", "on")?,
        ),
        (
            "fsrs (retrievability)",
            Panel::named("combmnz", "none", "fsrs")?,
        ),
    ];
    println!(
        "{TOPICS} topics, one kept claim written in the first 30 days and recalled on its clock, {PARAPHRASES} paraphrases written after day 150 and never reviewed; asked on day {DAYS}\n"
    );
    println!("| ordering | kept claim first | mean rank of the kept claim |");
    println!("|---|---|---|");
    let mut rows: Vec<Value> = Vec::new();
    let mut report = |name: &str, ranks: &[usize]| {
        let first = ranks.iter().filter(|r| **r == 1).count();
        let kept_first = first as f64 / TOPICS as f64;
        let mean_rank = ranks.iter().sum::<usize>() as f64 / TOPICS as f64;
        println!("| {name} | {kept_first:.3} | {mean_rank:.2} |");
        rows.push(json!({"ordering": name, "kept_first": kept_first, "mean_rank": mean_rank}));
    };
    let rank_of = |ranked: &[Value], topic: usize| -> usize {
        let kept = format!("t{topic}-v0");
        ranked
            .iter()
            .position(|h| h["id"].as_str() == Some(kept.as_str()))
            .map_or(10, |r| r + 1)
    };
    let cue_of = |topic: usize| format!("topic{topic} settled answer");
    for (name, panel) in &panels {
        let ranks: Vec<usize> = (0..TOPICS)
            .map(|topic| {
                let cue = atom_tokens(
                    json!({"text": cue_of(topic)})
                        .as_object()
                        .expect("an object"),
                );
                let ranked = merge_ballots(&[hits_for(&cue, &index, &atoms)], 10, panel, &now);
                rank_of(&ranked, topic)
            })
            .collect();
        report(name, &ranks);
    }
    // LRU: the lexical hits, newest write first. What a cache would keep.
    let off = Panel::named("combmnz", "none", "off")?;
    let ranks: Vec<usize> = (0..TOPICS)
        .map(|topic| {
            let cue = atom_tokens(
                json!({"text": cue_of(topic)})
                    .as_object()
                    .expect("an object"),
            );
            let mut ranked = merge_ballots(&[hits_for(&cue, &index, &atoms)], 10, &off, &now);
            ranked.sort_by(|a, b| {
                b["ts"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(a["ts"].as_str().unwrap_or(""))
            });
            rank_of(&ranked, topic)
        })
        .collect();
    report("lru (newest hit first)", &ranks);
    // Keep-testing: the recall path as the seat runs it, seeded by the
    // lexical hits the way the writer seeds it, the due queue ahead of the
    // cue's neighbourhood and the seeds, recency within each.
    let ranks: Vec<usize> = (0..TOPICS)
        .map(|topic| {
            let cue = atom_tokens(
                json!({"text": cue_of(topic)})
                    .as_object()
                    .expect("an object"),
            );
            let seeds: Vec<String> = hits_for(&cue, &index, &atoms)
                .iter()
                .take(10)
                .filter_map(|h| h["id"].as_str().map(str::to_string))
                .collect();
            let hints = Hints {
                text: cue_of(topic),
                entities: Vec::new(),
            };
            let picked = recall(&atoms, &seeds, &hints, Some(10), &now);
            let ranked: Vec<Value> = picked.into_iter().map(Value::Object).collect();
            rank_of(&ranked, topic)
        })
        .collect();
    report("keep-testing (recall: due first)", &ranks);
    if let Ok(path) = std::env::var("FORGETTING_JSON") {
        let body = json!({
            "topics": TOPICS,
            "paraphrases": PARAPHRASES,
            "days": DAYS,
            "asked_on": now,
            "orderings": rows,
        });
        std::fs::write(&path, serde_json::to_string_pretty(&body)?)?;
        eprintln!("wrote {path}");
    }
    Ok(())
}
