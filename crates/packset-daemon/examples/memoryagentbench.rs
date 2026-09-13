//! Retrieval on MemoryAgentBench (doi:10.48550/arXiv.2507.05257): one long
//! record injected once, many questions asked of it. Two of its four
//! competencies are a retriever's to win: accurate retrieval (document QA,
//! LongMemEval, EventQA) and conflict resolution (FactConsolidation: a list
//! of facts in which a later fact overwrites an earlier one about the same
//! subject). The harness chunks each record the way the benchmark does,
//! ranks chunks for every question under the pack's ballots, and writes the
//! seam a reader model answers from. Nothing reads here.
//!
//! ```console
//! $ # the parquet files from ai-hyz/MemoryAgentBench as JSONL, one row a line
//! $ PACKSET_MAB_DUMP=mab.jsonl PACKSET_MAB_CHUNKS=mab-chunks \
//!     cargo run --release -p packset-daemon --example memoryagentbench -- data/mab
//! ```
//!
//! The retrieval number reported here is a proxy: whether one of the answer
//! strings appears in the retrieved text. It is exact for the fact lists and
//! the document QA, where the answer is a span, and weak for EventQA and the
//! chat questions, where it is a sentence; the reader's accuracy over the
//! dump is the benchmark's own metric.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use packset_core::bm25::Index;
use packset_core::panel::Panel;
use packset_core::search::{atom_tokens, cosine, merge_ballots, Record};
use serde_json::{json, Value};

/// Words per chunk. The benchmark chunks by 512 model tokens; a token is
/// about three quarters of a word, so this is the same span in words.
const CHUNK_WORDS: usize = 384;
/// Hits handed to each fuse ballot.
const FUSE_DEPTH: usize = 50;
/// Documents kept per arm in the dump.
const KEEP: usize = 10;
const CUTOFFS: &[usize] = &[1, 5, 10];

struct Row {
    split: String,
    nth: usize,
    source: String,
    questions: Vec<String>,
    answers: Vec<Vec<String>>,
    kinds: Vec<String>,
    dates: Vec<String>,
    docs: Vec<Document>,
}

struct Document {
    /// Position in the record, first is 0.
    position: usize,
    text: String,
    tokens: Vec<String>,
}

fn tokens(text: &str) -> Vec<String> {
    let record: Record = json!({"text": text})
        .as_object()
        .cloned()
        .unwrap_or_default();
    atom_tokens(&record)
}

fn strings(v: &Value) -> Vec<String> {
    match v {
        Value::Array(items) => items
            .iter()
            .map(|i| match i {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect(),
        Value::String(s) => vec![s.clone()],
        Value::Null => Vec::new(),
        other => vec![other.to_string()],
    }
}

/// A fact list is one document a fact; everything else is chunked by words.
fn documents(source: &str, context: &str) -> Vec<Document> {
    let doc = |position: usize, text: String| Document {
        position,
        tokens: tokens(&text),
        text,
    };
    if source.starts_with("factconsolidation") {
        return context
            .lines()
            .map(str::trim)
            .filter(|l| l.chars().next().is_some_and(|c| c.is_ascii_digit()) && l.contains(". "))
            .enumerate()
            .map(|(i, l)| doc(i, l.to_string()))
            .collect();
    }
    let words: Vec<&str> = context.split_whitespace().collect();
    words
        .chunks(CHUNK_WORDS)
        .enumerate()
        .map(|(i, w)| doc(i, w.join(" ")))
        .collect()
}

fn rows(dir: &Path, split: &str) -> anyhow::Result<Vec<Row>> {
    let path = dir.join(format!("{split}.jsonl"));
    let text =
        std::fs::read_to_string(&path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    let mut out = Vec::new();
    for (nth, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(line)?;
        let meta = &v["metadata"];
        let source = meta["source"].as_str().unwrap_or("unknown").to_string();
        let context = v["context"].as_str().unwrap_or("");
        let questions = strings(&v["questions"]);
        let answers: Vec<Vec<String>> = v["answers"]
            .as_array()
            .map(|a| a.iter().map(strings).collect())
            .unwrap_or_default();
        let kinds = strings(&meta["question_types"]);
        let dates = strings(&meta["question_dates"]);
        let docs = documents(&source, context);
        out.push(Row {
            split: split.to_string(),
            nth,
            source,
            questions,
            answers,
            kinds,
            dates,
            docs,
        });
    }
    Ok(out)
}

fn lexical(query: &str, index: &Index) -> Vec<(usize, f64)> {
    let mut scored = index.score(&tokens(query));
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    scored
}

fn dense(query: &[f32], vectors: &[Vec<f32>]) -> Vec<(usize, f64)> {
    let mut scored: Vec<(usize, f64)> = vectors
        .iter()
        .enumerate()
        .filter(|(_, v)| !v.is_empty())
        .map(|(i, v)| (i, cosine(query, v)))
        .filter(|(_, s)| *s > 0.0)
        .collect();
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    scored
}

fn fused(lexical: &[(usize, f64)], dense: &[(usize, f64)]) -> Vec<(usize, f64)> {
    let ballot = |ranked: &[(usize, f64)]| -> Vec<Value> {
        ranked
            .iter()
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

/// The top hits reordered latest first: what the pack's timeline reading
/// hands a reader, the newest statement of a fact before the older ones.
fn latest_first(ranked: &[(usize, f64)], docs: &[Document]) -> Vec<usize> {
    let mut top: Vec<usize> = ranked.iter().take(KEEP).map(|(i, _)| *i).collect();
    top.sort_by(|a, b| docs[*b].position.cmp(&docs[*a].position));
    top
}

/// The object of a templated fact: the words after the relation's last
/// function word (`Roy Rogers is married to | John McVie`, `The capital of
/// Romania is | Rajanpur`). A multi-hop question chains through it: the
/// object of the fact about the subject it names is the subject of the
/// fact that answers it.
fn object_of(text: &str) -> Option<String> {
    let body = text.split_once(". ").map_or(text, |(_, rest)| rest);
    let words: Vec<&str> = body.split_whitespace().collect();
    let cut = words
        .iter()
        .rposition(|w| {
            let w = w.to_ascii_lowercase();
            matches!(
                w.trim_end_matches(','),
                "is" | "of" | "in" | "to" | "by" | "at" | "for" | "with" | "as"
            )
        })
        .map(|i| i + 1)?;
    if cut >= words.len() {
        return None;
    }
    Some(
        words[cut..]
            .join(" ")
            .trim_end_matches(['.', ','])
            .to_string(),
    )
}

/// Hops in the two-hop arm: how many first-hop facts lend their object as
/// a second query.
const HOP_SEEDS: usize = 3;

/// Which facts are live once each later fact closes the earlier one it
/// replaces, by the pack's own rule (`packset_core::record::same_head`):
/// the same opening words, a new object. The list's numbering is dropped
/// before the comparison, as a claim's text carries none.
fn live(docs: &[Document]) -> Vec<bool> {
    let heads: Vec<Vec<String>> = docs
        .iter()
        .map(|d| {
            let body = d
                .text
                .split_once(". ")
                .map_or(d.text.as_str(), |(_, rest)| rest);
            packset_core::record::head_tokens(body)
        })
        .collect();
    let mut alive = vec![true; docs.len()];
    for i in 0..docs.len() {
        for j in 0..i {
            if alive[j] && packset_core::record::same_head(&heads[i], &heads[j]) {
                alive[j] = false;
            }
        }
    }
    alive
}

fn cache_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("PACKSET_MAB_CACHE")?);
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

fn write_rows(path: &Path, rows: &[Vec<f32>]) {
    let Ok(mut file) = std::fs::File::create(path) else {
        return;
    };
    let _ = file.write_all(&(rows.len() as u64).to_le_bytes());
    for row in rows {
        let _ = file.write_all(&(row.len() as u64).to_le_bytes());
        for v in row {
            let _ = file.write_all(&v.to_le_bytes());
        }
    }
}

fn read_rows(path: &Path, expected: usize) -> Option<Vec<Vec<f32>>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .read_to_end(&mut bytes)
        .ok()?;
    let mut at = 0usize;
    let count = u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?) as usize;
    at += 8;
    if count != expected {
        return None;
    }
    let mut rows = Vec::with_capacity(count);
    for _ in 0..count {
        let len = u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?) as usize;
        at += 8;
        let mut row = Vec::with_capacity(len);
        for _ in 0..len {
            row.push(f32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
            at += 4;
        }
        rows.push(row);
    }
    Some(rows)
}

fn vectors(row: &Row) -> Vec<Vec<f32>> {
    let model = std::env::var("PACKSET_EMBED_MODEL").unwrap_or_else(|_| "default".into());
    let file = cache_dir().map(|d| d.join(format!("{model}-{}-{}.bin", row.split, row.nth)));
    if let Some(rows) = file.as_deref().and_then(|p| read_rows(p, row.docs.len())) {
        return rows;
    }
    let fresh: Vec<Vec<f32>> = row
        .docs
        .iter()
        .map(|d| packset_daemon::embed::encode_document(&d.text).unwrap_or_default())
        .collect();
    if let Some(path) = file.as_deref() {
        write_rows(path, &fresh);
    }
    fresh
}

fn normal(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether an answer string appears in the top `k` documents' text.
fn bearing(top: &[usize], docs: &[Document], answers: &[String], k: usize) -> bool {
    let text = top
        .iter()
        .take(k)
        .map(|i| format!(" {} ", normal(&docs[*i].text)))
        .collect::<String>();
    answers
        .iter()
        .map(|a| normal(a))
        .filter(|a| !a.is_empty())
        .any(|a| text.contains(&a))
}

#[derive(Clone)]
struct Tally {
    asked: usize,
    hit: Vec<usize>,
}

impl Tally {
    fn new() -> Self {
        Self {
            asked: 0,
            hit: vec![0; CUTOFFS.len()],
        }
    }
    fn add(&mut self, top: &[usize], docs: &[Document], answers: &[String]) {
        self.asked += 1;
        for (n, k) in CUTOFFS.iter().enumerate() {
            if bearing(top, docs, answers, *k) {
                self.hit[n] += 1;
            }
        }
    }
    fn line(&self, label: &str) -> String {
        let mut s = format!("| {label} | {} |", self.asked);
        for n in 0..CUTOFFS.len() {
            s.push_str(&format!(
                " {:.3} |",
                self.hit[n] as f64 / self.asked.max(1) as f64
            ));
        }
        s
    }
}

fn main() -> anyhow::Result<()> {
    let dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "data/mab".to_string()),
    );
    let splits: Vec<String> = std::env::var("PACKSET_MAB_SPLITS")
        .unwrap_or_else(|_| "Accurate_Retrieval,Conflict_Resolution".to_string())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let cap: Option<usize> = std::env::var("PACKSET_MAB_ROWS")
        .ok()
        .and_then(|c| c.parse().ok());
    let encoder = packset_daemon::embed::binary().is_some();
    println!(
        "encoder: {}",
        if encoder {
            "present, dense and fused arms run"
        } else {
            "absent, lexical arms only"
        }
    );
    let mut dump =
        std::env::var_os("PACKSET_MAB_DUMP").map(|p| std::fs::File::create(p).expect("dump file"));
    let chunks_dir = std::env::var_os("PACKSET_MAB_CHUNKS").map(PathBuf::from);
    if let Some(d) = &chunks_dir {
        std::fs::create_dir_all(d)?;
    }
    let mut arms: Vec<String> = vec!["lexical".into()];
    if encoder {
        arms.push("dense".into());
        arms.push("fused".into());
    }
    let mut fact_arms: Vec<String> = Vec::new();
    if encoder {
        fact_arms.push("fused latest".into());
        fact_arms.push("fused live".into());
        fact_arms.push("fused live hop2".into());
    } else {
        fact_arms.push("lexical latest".into());
        fact_arms.push("lexical live".into());
        fact_arms.push("lexical live hop2".into());
    }
    let started = std::time::Instant::now();
    // (source, arm) -> tally
    let mut tallies: BTreeMap<(String, String), Tally> = BTreeMap::new();
    let mut asked_total = 0usize;
    for split in &splits {
        let mut all = rows(&dir, split)?;
        if let Some(c) = cap {
            all.truncate(c);
        }
        for row in &all {
            let facts = row.source.starts_with("factconsolidation");
            println!(
                "{split} row {} {}: {} documents, {} questions",
                row.nth,
                row.source,
                row.docs.len(),
                row.questions.len()
            );
            if let Some(d) = &chunks_dir {
                let texts: Vec<&str> = row.docs.iter().map(|d| d.text.as_str()).collect();
                std::fs::write(
                    d.join(format!("{split}-{}.json", row.nth)),
                    serde_json::to_string(&texts)?,
                )?;
            }
            let index = Index::build(row.docs.iter().map(|d| d.tokens.as_slice()));
            let vecs = if encoder { vectors(row) } else { Vec::new() };
            let alive = if facts { live(&row.docs) } else { Vec::new() };
            for (q, question) in row.questions.iter().enumerate() {
                let answers = row.answers.get(q).cloned().unwrap_or_default();
                let mut retrieved: BTreeMap<String, Vec<usize>> = BTreeMap::new();
                let lex = lexical(question, &index);
                let mut record = |arm: &str, top: Vec<usize>| {
                    tallies
                        .entry((row.source.clone(), arm.to_string()))
                        .or_insert_with(Tally::new)
                        .add(&top, &row.docs, &answers);
                    retrieved.insert(arm.to_string(), top.into_iter().take(KEEP).collect());
                };
                let firsts = |r: &[(usize, f64)]| -> Vec<usize> {
                    r.iter().take(KEEP).map(|(i, _)| *i).collect()
                };
                record("lexical", firsts(&lex));
                let best: Vec<(usize, f64)> = if encoder {
                    let query = packset_daemon::embed::encode_query(question).unwrap_or_default();
                    let den = dense(&query, &vecs);
                    record("dense", firsts(&den));
                    let fus = fused(&lex, &den);
                    record("fused", firsts(&fus));
                    fus
                } else {
                    lex.clone()
                };
                if facts {
                    let base = if encoder { "fused" } else { "lexical" };
                    record(&format!("{base} latest"), latest_first(&best, &row.docs));
                    // Only live facts answer, latest first among them.
                    let living: Vec<(usize, f64)> = best
                        .iter()
                        .filter(|(i, _)| alive.get(*i).copied().unwrap_or(true))
                        .copied()
                        .collect();
                    record(&format!("{base} live"), latest_first(&living, &row.docs));
                    // Two hops over the live facts: the objects of the first
                    // hop's strongest facts are asked about in turn, and the
                    // second hop's live facts follow the first's. A question
                    // about the spouse of the author of a book reaches the
                    // author's fact, then the spouse's, by the object it
                    // named, with every superseded fact already closed.
                    let mut chain: Vec<usize> =
                        living.iter().take(HOP_SEEDS).map(|(i, _)| *i).collect();
                    let mut second: Vec<(usize, f64)> = Vec::new();
                    for &i in chain.clone().iter() {
                        let Some(object) = object_of(&row.docs[i].text) else {
                            continue;
                        };
                        let hop_query = format!("{question} {object}");
                        let hop_lex = lexical(&hop_query, &index);
                        let hop = if encoder {
                            let v =
                                packset_daemon::embed::encode_query(&hop_query).unwrap_or_default();
                            fused(&hop_lex, &dense(&v, &vecs))
                        } else {
                            hop_lex
                        };
                        second.extend(
                            hop.into_iter()
                                .filter(|(j, _)| alive.get(*j).copied().unwrap_or(true))
                                .filter(|(j, _)| !chain.contains(j))
                                .take(HOP_SEEDS),
                        );
                    }
                    second.sort_by(|a, b| {
                        b.1.partial_cmp(&a.1)
                            .unwrap_or(std::cmp::Ordering::Equal)
                            .then_with(|| a.0.cmp(&b.0))
                    });
                    for (j, _) in second {
                        if !chain.contains(&j) && chain.len() < KEEP {
                            chain.push(j);
                        }
                    }
                    for (j, _) in living.iter().skip(HOP_SEEDS) {
                        if !chain.contains(j) && chain.len() < KEEP {
                            chain.push(*j);
                        }
                    }
                    record(&format!("{base} live hop2"), chain);
                }
                asked_total += 1;
                if let Some(file) = dump.as_mut() {
                    let line = json!({
                        "split": split,
                        "row": row.nth,
                        "source": row.source,
                        "question_index": q,
                        "question": question,
                        "answers": answers,
                        "question_type": row.kinds.get(q).cloned().unwrap_or_default(),
                        "question_date": row.dates.get(q).cloned().unwrap_or_default(),
                        "retrieved": retrieved,
                    });
                    writeln!(file, "{line}")?;
                }
            }
            eprintln!(
                "{} questions so far, {:.0}s",
                asked_total,
                started.elapsed().as_secs_f64()
            );
        }
    }
    let mut header = String::from("| source | arm | asked |");
    for cut in CUTOFFS {
        header.push_str(&format!(" answer in top {cut} |"));
    }
    println!("\nanswer-bearing retrieval (a proxy; the reader's accuracy is the metric)\n");
    println!("{header}");
    println!("|{}", "---|".repeat(3 + CUTOFFS.len()));
    let mut all_arms: Vec<String> = arms.clone();
    all_arms.extend(fact_arms.iter().cloned());
    for ((source, arm), tally) in &tallies {
        println!("{}", tally.line(&format!("{source} | {arm}")));
    }
    // One line per arm over every source it ran on.
    for arm in &all_arms {
        let mut sum = Tally::new();
        for ((_, a), t) in &tallies {
            if a == arm {
                sum.asked += t.asked;
                for n in 0..CUTOFFS.len() {
                    sum.hit[n] += t.hit[n];
                }
            }
        }
        if sum.asked > 0 {
            println!("{}", sum.line(&format!("all | {arm}")));
        }
    }
    println!(
        "\n{} questions in {:.0}s",
        asked_total,
        started.elapsed().as_secs_f64()
    );
    Ok(())
}
