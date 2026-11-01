//! The retrieval dump for LoCoMo (doi:10.48550/arXiv.2402.17753): for every
//! question, the turns the lexical ballot and the fused panel rank first,
//! written as JSON lines for `scripts/longmemeval_qa.py --bench locomo`
//! to hand a reader. Retrieval and reading are two measurements; this file is
//! the seam between them, as `longmemeval` writes for its benchmark.
//!
//! ```console
//! $ PACKSET_LOCOMO_DUMP=locomo-dump.jsonl cargo run --release -p packset-daemon --example locomo_dump -- locomo10.json
//! ```

use std::collections::BTreeSet;
use std::io::{Read, Write};

use packset_core::bm25::Index;
use packset_core::panel::Panel;
use packset_core::search::{atom_tokens, cosine, merge_ballots, Record};
use serde_json::{json, Value};

const CUTOFFS: &[usize] = &[1, 5, 10];
const FUSE_DEPTH: usize = 50;
const DUMP_DEPTH: usize = 20;

struct Turn {
    id: String,
    text: String,
    tokens: Vec<String>,
}

struct Question {
    text: String,
    answer: String,
    category: i64,
    evidence: BTreeSet<String>,
}

fn tokens(text: &str) -> Vec<String> {
    let record: Record = json!({"text": text})
        .as_object()
        .cloned()
        .unwrap_or_default();
    atom_tokens(&record)
}

fn conversations(raw: &Value) -> Vec<(Vec<Turn>, Vec<Question>)> {
    let mut out = Vec::new();
    for sample in raw.as_array().map(Vec::as_slice).unwrap_or_default() {
        let Some(conversation) = sample.get("conversation").and_then(Value::as_object) else {
            continue;
        };
        let mut sessions: Vec<(&String, &Value)> = conversation
            .iter()
            .filter(|(key, value)| key.starts_with("session_") && value.is_array())
            .collect();
        sessions.sort_by_key(|(key, _)| {
            key.trim_start_matches("session_")
                .parse::<u32>()
                .unwrap_or(u32::MAX)
        });
        let mut turns = Vec::new();
        for (_, list) in sessions {
            for turn in list.as_array().map(Vec::as_slice).unwrap_or_default() {
                let (Some(id), Some(text)) = (
                    turn.get("dia_id").and_then(Value::as_str),
                    turn.get("text").and_then(Value::as_str),
                ) else {
                    continue;
                };
                let speaker = turn
                    .get("speaker")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let text = format!("{speaker}: {text}");
                turns.push(Turn {
                    id: id.to_string(),
                    tokens: tokens(&text),
                    text,
                });
            }
        }
        let questions = sample
            .get("qa")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(|qa| {
                Some(Question {
                    text: qa.get("question").and_then(Value::as_str)?.to_string(),
                    answer: match qa.get("answer") {
                        Some(Value::String(s)) => s.clone(),
                        Some(other) => other.to_string(),
                        None => String::new(),
                    },
                    category: qa.get("category").and_then(Value::as_i64).unwrap_or(0),
                    evidence: qa
                        .get("evidence")
                        .and_then(Value::as_array)
                        .map(Vec::as_slice)
                        .unwrap_or_default()
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect(),
                })
            })
            .collect();
        out.push((turns, questions));
    }
    out
}

fn ranked(scored: &mut [(usize, f64)]) {
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
}

/// The fused ranking with the panel's score on each ordinal, for the
/// test-time-learning reading, which reweighs the fused hits by a review
/// clock the reader's own grades move.
fn fused_scored(lexical: &[(usize, f64)], dense: &[(usize, f64)]) -> Vec<(usize, f64)> {
    let ballot = |r: &[(usize, f64)]| -> Vec<Value> {
        r.iter()
            .take(FUSE_DEPTH)
            .map(|(i, s)| json!({"field": "atom", "id": i.to_string(), "text": "", "score": s}))
            .collect()
    };
    let panel = Panel::named("combmnz", "none", "off").expect("a shipped panel");
    let now = packset_core::clock::utcnow();
    merge_ballots(&[ballot(lexical), ballot(dense)], FUSE_DEPTH, &panel, &now)
        .iter()
        .filter_map(|hit| {
            Some((
                hit["id"].as_str()?.parse().ok()?,
                hit["score"].as_f64().unwrap_or(0.0),
            ))
        })
        .collect()
}

fn cache_dir() -> Option<std::path::PathBuf> {
    let dir = std::path::PathBuf::from(std::env::var_os("PACKSET_LME_CACHE")?);
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

fn read_rows(path: &std::path::Path, n: usize) -> Option<Vec<Vec<f32>>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .read_to_end(&mut bytes)
        .ok()?;
    let mut rows = Vec::with_capacity(n);
    let mut at = 0;
    for _ in 0..n {
        let len = u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?) as usize;
        at += 4;
        let mut row = Vec::with_capacity(len);
        for _ in 0..len {
            row.push(f32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
            at += 4;
        }
        rows.push(row);
    }
    Some(rows)
}

fn write_rows(path: &std::path::Path, rows: &[Vec<f32>]) {
    let mut bytes = Vec::new();
    for row in rows {
        bytes.extend_from_slice(&(row.len() as u32).to_le_bytes());
        for x in row {
            bytes.extend_from_slice(&x.to_le_bytes());
        }
    }
    if let Ok(mut f) = std::fs::File::create(path) {
        let _ = f.write_all(&bytes);
    }
}

fn vectors(nth: usize, turns: &[Turn]) -> Vec<Vec<f32>> {
    let model = std::env::var("PACKSET_EMBED_MODEL").unwrap_or_else(|_| "default".into());
    let file = cache_dir().map(|d| d.join(format!("{model}-locomo-c{nth}.bin")));
    if let Some(rows) = file.as_deref().and_then(|p| read_rows(p, turns.len())) {
        return rows;
    }
    let fresh: Vec<Vec<f32>> = turns
        .iter()
        .map(|t| packset_daemon::embed::encode_document(&t.text).unwrap_or_default())
        .collect();
    if let Some(path) = file.as_deref() {
        write_rows(path, &fresh);
    }
    fresh
}

#[derive(Default)]
struct Tally {
    asked: usize,
    hit: Vec<usize>,
}

impl Tally {
    fn add(&mut self, ranked: &[String], evidence: &BTreeSet<String>) {
        if self.hit.is_empty() {
            self.hit = vec![0; CUTOFFS.len()];
        }
        self.asked += 1;
        for (slot, &cut) in CUTOFFS.iter().enumerate() {
            if ranked.iter().take(cut).any(|r| evidence.contains(r)) {
                self.hit[slot] += 1;
            }
        }
    }
    fn row(&self, name: &str) -> String {
        let mut line = format!("| {name} | {} |", self.asked);
        for slot in 0..CUTOFFS.len() {
            line.push_str(&format!(
                " {:.3} |",
                self.hit.get(slot).copied().unwrap_or(0) as f64 / self.asked.max(1) as f64
            ));
        }
        line
    }
}

fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "locomo10.json".to_string());
    let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
    let corpus = conversations(&raw);
    anyhow::ensure!(!corpus.is_empty(), "no conversations in {path}");
    let encoder = packset_daemon::embed::binary().is_some();
    println!(
        "encoder: {}",
        if encoder {
            "present, lexical and fused arms"
        } else {
            "absent, lexical arm only"
        }
    );
    let mut dump = std::env::var_os("PACKSET_LOCOMO_DUMP")
        .map(|p| std::fs::File::create(p).expect("dump file"));
    let mut lex_tally = Tally::default();
    let mut fused_tally = Tally::default();
    let started = std::time::Instant::now();
    for (nth, (turns, questions)) in corpus.iter().enumerate() {
        let index = Index::build(turns.iter().map(|t| t.tokens.as_slice()));
        let vecs = if encoder {
            vectors(nth, turns)
        } else {
            Vec::new()
        };
        for (qn, q) in questions.iter().enumerate() {
            let mut lex = index.score(&tokens(&q.text));
            ranked(&mut lex);
            let lex_ids: Vec<String> = lex.iter().map(|(i, _)| turns[*i].id.clone()).collect();
            lex_tally.add(&lex_ids, &q.evidence);
            let mut retrieved =
                json!({"turns": lex_ids.iter().take(DUMP_DEPTH).collect::<Vec<_>>()});
            if encoder {
                let query = packset_daemon::embed::encode_query(&q.text).unwrap_or_default();
                let mut den: Vec<(usize, f64)> = vecs
                    .iter()
                    .enumerate()
                    .filter(|(_, v)| !v.is_empty())
                    .map(|(i, v)| (i, cosine(&query, v)))
                    .filter(|(_, s)| *s > 0.0)
                    .collect();
                ranked(&mut den);
                let scored = fused_scored(&lex, &den);
                let fused_ids: Vec<String> =
                    scored.iter().map(|(i, _)| turns[*i].id.clone()).collect();
                fused_tally.add(&fused_ids, &q.evidence);
                retrieved["turns fused"] =
                    json!(fused_ids.iter().take(DUMP_DEPTH).collect::<Vec<_>>());
                // The whole fused list with scores: what a reading that
                // reweighs by a review clock starts from.
                retrieved["turns fused scored"] = json!(scored
                    .iter()
                    .map(|(i, s)| json!([turns[*i].id, s]))
                    .collect::<Vec<_>>());
            }
            if let Some(file) = dump.as_mut() {
                let line = json!({
                    "conversation": nth,
                    "question_index": qn,
                    "question": q.text,
                    "answer": q.answer,
                    "category": q.category,
                    "evidence": q.evidence,
                    "retrieved": retrieved,
                });
                writeln!(file, "{line}")?;
            }
        }
        eprintln!(
            "conversation {} of {}, {:.0}s",
            nth + 1,
            corpus.len(),
            started.elapsed().as_secs_f64()
        );
    }
    println!("\nLoCoMo, turn granularity, evidence hit\n\n| arm | asked | hit@1 | hit@5 | hit@10 |\n|---|---|---|---|---|");
    println!("{}", lex_tally.row("turns bm25+"));
    if encoder {
        println!("{}", fused_tally.row("turns fused"));
    }
    Ok(())
}
