//! Session retrieval on LongMemEval_S (doi:10.48550/arXiv.2410.10813): 500
//! questions, each over about fifty chat sessions, with the sessions that
//! answer it labelled. Turns are loaded as atoms, so this measures the lexical
//! scorer under three document protocols and nothing else: no model reads
//! the retrieved context, and nothing is extracted on write. Abstention
//! questions (`_abs`) have no answer session and are excluded.
//!
//! ```console
//! $ curl -sL -o longmemeval_s.json https://huggingface.co/datasets/xiaowu0162/longmemeval/resolve/main/longmemeval_s
//! $ cargo run --release -p packset-daemon --example longmemeval -- longmemeval_s.json
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};

use packset_core::bm25::Index;
use packset_core::panel::Panel;
use packset_core::search::{atom_tokens, cosine, merge_ballots, Record};
use serde_json::{json, Value};

const CUTOFFS: &[usize] = &[1, 5, 10];

/// Passage windows: six turns, stride three, as the LoCoMo protocol table settled.
const WINDOW: usize = 6;
const STRIDE: usize = 3;

struct Question {
    id: String,
    kind: String,
    text: String,
    date: String,
    answer: String,
    /// Session id, its date as the benchmark writes it, then its turns.
    sessions: Vec<(String, String, Vec<String>)>,
    answers: BTreeSet<String>,
}

/// Days since the epoch of a benchmark date, `2023/05/20 (Sat) 02:21`,
/// good enough to take differences between two of them.
fn days_of(date: &str) -> Option<f64> {
    let mut parts = date.split_whitespace();
    let ymd = parts.next()?;
    let hm = parts.nth(1).unwrap_or("00:00");
    let mut it = ymd.split('/');
    let y: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next()?.parse().ok()?;
    let d: i64 = it.next()?.parse().ok()?;
    let mut t = hm.split(':');
    let h: f64 = t.next()?.parse().ok()?;
    let mi: f64 = t.next()?.parse().ok()?;
    Some(civil_days(y, m, d) + (h * 60.0 + mi) / 1440.0)
}

/// Days from civil (Howard Hinnant).
fn civil_days(y: i64, m: i64, d: i64) -> f64 {
    let (y, m) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe - 719_468) as f64
}

/// The civil date of a day count, for month arithmetic.
fn civil_of(days: f64) -> (i64, i64, i64) {
    let z = days.floor() as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

const MONTHS: &[&str] = &[
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

/// The window of days a question points at, when it names a time: an
/// explicit date, a month with or without a day and year, a count of
/// days, weeks, months or years ago, or last week, month or year. This is
/// the pack's validity window read off the question: what was true then,
/// not what is recent. `None` when the question names no time.
fn window_of(question: &str, asked: f64) -> Option<(f64, f64)> {
    let lower = question.to_lowercase();
    let tokens: Vec<&str> = lower
        .split(|c: char| !(c.is_alphanumeric() || c == '/'))
        .filter(|t| !t.is_empty())
        .collect();
    let (qy, qm, _) = civil_of(asked);
    // An explicit date.
    for t in &tokens {
        let parts: Vec<&str> = t.split('/').collect();
        if let [y, m, d] = parts.as_slice() {
            if let (Ok(y), Ok(m), Ok(d)) = (y.parse::<i64>(), m.parse::<i64>(), d.parse::<i64>()) {
                if (1..=12).contains(&m) && (1..=31).contains(&d) {
                    let day = civil_days(y, m, d);
                    return Some((day, day + 1.0));
                }
            }
        }
    }
    // A count of units ago.
    for (i, t) in tokens.iter().enumerate() {
        if *t == "ago" && i >= 2 {
            let unit = match tokens[i - 1].trim_end_matches('s') {
                "day" => 1.0,
                "week" => 7.0,
                "month" => 30.4,
                "year" => 365.25,
                _ => continue,
            };
            let n: f64 = match tokens[i - 2] {
                "a" | "an" | "one" => 1.0,
                "two" => 2.0,
                "three" => 3.0,
                "four" => 4.0,
                "five" => 5.0,
                "six" => 6.0,
                other => other.parse().ok()?,
            };
            let point = asked - n * unit;
            return Some((point - unit / 2.0, point + unit / 2.0));
        }
    }
    // Last or this week, month, year; yesterday.
    for (i, t) in tokens.iter().enumerate() {
        if *t == "yesterday" {
            return Some((asked - 2.0, asked));
        }
        if (*t == "last" || *t == "this" || *t == "past") && i + 1 < tokens.len() {
            let span = match tokens[i + 1] {
                "week" => 7.0,
                "month" => 30.4,
                "year" => 365.25,
                _ => continue,
            };
            return Some(if *t == "last" {
                (asked - 2.0 * span, asked - span)
            } else {
                (asked - span, asked)
            });
        }
    }
    // A month, with a day and a year when given.
    for (i, t) in tokens.iter().enumerate() {
        let Some(m) = MONTHS
            .iter()
            .position(|name| name == t || (t.len() >= 3 && name.starts_with(t) && t.len() == 3))
        else {
            continue;
        };
        let m = m as i64 + 1;
        let day = tokens
            .get(i + 1)
            .and_then(|d| {
                d.trim_end_matches(|c: char| c.is_alphabetic())
                    .parse::<i64>()
                    .ok()
            })
            .filter(|d| (1..=31).contains(d));
        let year = tokens
            .iter()
            .skip(i)
            .take(4)
            .find_map(|y| y.parse::<i64>().ok().filter(|y| (1990..=2100).contains(y)))
            .unwrap_or(if m > qm { qy - 1 } else { qy });
        return Some(match day {
            Some(d) => (civil_days(year, m, d), civil_days(year, m, d) + 1.0),
            None => {
                let start = civil_days(year, m, 1);
                let end = if m == 12 {
                    civil_days(year + 1, 1, 1)
                } else {
                    civil_days(year, m + 1, 1)
                };
                (start, end)
            }
        });
    }
    None
}

/// The fused ranking with the sessions inside the question's window scored
/// twice: a filter the question itself asks for, and nothing when it names
/// no time. This is the pack's as-of read over the benchmark.
fn windowed(question: &Question, docs: &[Document], fused: &[(usize, f64)]) -> Vec<(usize, f64)> {
    let Some(asked) = days_of(&question.date) else {
        return fused.to_vec();
    };
    let Some((start, end)) = window_of(&question.text, asked) else {
        return fused.to_vec();
    };
    let inside: BTreeSet<&str> = question
        .sessions
        .iter()
        .filter(|(_, date, _)| days_of(date).is_some_and(|d| d >= start - 1.0 && d < end + 1.0))
        .map(|(sid, _, _)| sid.as_str())
        .collect();
    let mut scaled: Vec<(usize, f64)> = fused
        .iter()
        .map(|(i, s)| {
            let boost = if inside.contains(docs[*i].session.as_str()) {
                2.0
            } else {
                1.0
            };
            (*i, s * boost)
        })
        .collect();
    scaled.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    scaled
}

fn questions(raw: &Value) -> Vec<Question> {
    let Some(items) = raw.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|q| {
            let id = q["question_id"].as_str()?;
            if id.ends_with("_abs") {
                return None;
            }
            let ids = q["haystack_session_ids"].as_array()?;
            let dates = q["haystack_dates"].as_array().cloned().unwrap_or_default();
            let sessions = q["haystack_sessions"].as_array()?;
            let sessions = ids
                .iter()
                .enumerate()
                .zip(sessions)
                .filter_map(|((n, sid), turns)| {
                    let date = dates
                        .get(n)
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let turns: Vec<String> = turns
                        .as_array()?
                        .iter()
                        .filter_map(|t| {
                            let role = t["role"].as_str().unwrap_or("");
                            let content = t["content"].as_str()?;
                            Some(format!("{role}: {content}"))
                        })
                        .collect();
                    Some((sid.as_str()?.to_string(), date, turns))
                })
                .collect();
            let answers = q["answer_session_ids"]
                .as_array()?
                .iter()
                .filter_map(|a| a.as_str().map(str::to_string))
                .collect();
            Some(Question {
                id: id.to_string(),
                kind: q["question_type"].as_str().unwrap_or("?").to_string(),
                text: q["question"].as_str()?.to_string(),
                date: q["question_date"].as_str().unwrap_or("").to_string(),
                answer: q["answer"].as_str().unwrap_or("").to_string(),
                sessions,
                answers,
            })
        })
        .collect()
}

fn tokens(text: &str) -> Vec<String> {
    let record: Record = json!({"text": text})
        .as_object()
        .cloned()
        .unwrap_or_default();
    atom_tokens(&record)
}

/// One document: the session it belongs to, its text, its tokens.
struct Document {
    session: String,
    text: String,
    tokens: Vec<String>,
}

/// Documents for one protocol.
fn documents(question: &Question, protocol: &str) -> Vec<Document> {
    let doc = |sid: &str, text: String| Document {
        session: sid.to_string(),
        tokens: tokens(&text),
        text,
    };
    let mut out = Vec::new();
    for (sid, _, turns) in &question.sessions {
        match protocol {
            "turns" => {
                for turn in turns {
                    out.push(doc(sid, turn.clone()));
                }
            }
            "windows" => {
                if turns.len() <= WINDOW {
                    out.push(doc(sid, turns.join("\n")));
                } else {
                    let mut start = 0;
                    while start < turns.len() {
                        let end = (start + WINDOW).min(turns.len());
                        out.push(doc(sid, turns[start..end].join("\n")));
                        if end == turns.len() {
                            break;
                        }
                        start += STRIDE;
                    }
                }
            }
            _ => out.push(doc(sid, turns.join("\n"))),
        }
    }
    out
}

/// BM25+ over the documents: ordinal and score, best first.
fn lexical(question: &Question, docs: &[Document]) -> Vec<(usize, f64)> {
    let index = Index::build(docs.iter().map(|d| d.tokens.as_slice()));
    let mut scored = index.score(&tokens(&question.text));
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    scored
}

/// Cosine over document vectors: ordinal and score, best first.
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

/// Two rankings fused by the shipped panel (CombMNZ): ordinals with the
/// fused score, for arms that scale it.
fn fused_scored(
    lexical: &[(usize, f64)],
    dense: &[(usize, f64)],
    limit: usize,
) -> Vec<(usize, f64)> {
    let ballot = |ranked: &[(usize, f64)]| -> Vec<Value> {
        ranked
            .iter()
            .take(limit)
            .map(|(i, s)| json!({"field": "atom", "id": i.to_string(), "text": "", "score": s}))
            .collect()
    };
    let panel = Panel::named("combmnz", "none", "off").expect("a shipped panel");
    let now = packset_core::clock::utcnow();
    merge_ballots(&[ballot(lexical), ballot(dense)], limit, &panel, &now)
        .iter()
        .filter_map(|hit| {
            Some((
                hit["id"].as_str()?.parse().ok()?,
                hit["score"].as_f64().unwrap_or(0.0),
            ))
        })
        .collect()
}

/// The fused ranking with each document's score scaled by the shipped
/// temporal slot (`PACKSET_DECAY=on`, a fourteen-day half-life) for the age
/// of its session at the question's date. This is the forgetting the
/// benchmark can see: a knowledge-update question wants the latest
/// session, a single-session question does not care, and the table by type
/// says what recency buys and costs.
fn recency(question: &Question, docs: &[Document], fused: &[(usize, f64)]) -> Vec<(usize, f64)> {
    let panel = Panel::named("combmnz", "none", "on").expect("a shipped panel");
    let asked = days_of(&question.date);
    let age_of: BTreeMap<&str, f64> = question
        .sessions
        .iter()
        .map(|(sid, date, _)| {
            let age = match (asked, days_of(date)) {
                (Some(a), Some(d)) => (a - d).max(0.0),
                _ => 0.0,
            };
            (sid.as_str(), age)
        })
        .collect();
    let mut scaled: Vec<(usize, f64)> = fused
        .iter()
        .map(|(i, s)| {
            let age = age_of
                .get(docs[*i].session.as_str())
                .copied()
                .unwrap_or(0.0);
            (*i, s * panel.decay_weight("session", age, 1.0))
        })
        .collect();
    scaled.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    scaled
}

/// How many nearest sessions each session links to in the island graph.
const ISLAND_LINKS: usize = 5;
/// How far activation spreads from the seeds, as in the writer.
const ISLAND_HOPS: usize = 2;

/// The fused ranking spread along the pack's kind of link graph. Sessions
/// link to their [`ISLAND_LINKS`] nearest by dense cosine, the cosine is
/// the link weight, and the fused top documents seed spreading activation
/// over that graph (`packset_core::island::activate`, the writer's own
/// island). A session the question does not name but that stands close to
/// several that it does gains activation; a seed with no neighbours keeps
/// its own. This is the island the seat reads at a sitting, measured on the
/// benchmark: does the cluster beat the hits.
fn island(vectors: &[Vec<f32>], fused: &[(usize, f64)], limit: usize) -> Vec<(usize, f64)> {
    let mut atoms: Vec<Record> = Vec::with_capacity(vectors.len());
    for (i, v) in vectors.iter().enumerate() {
        let mut near: Vec<(usize, f64)> = if v.is_empty() {
            Vec::new()
        } else {
            dense(v, vectors)
                .into_iter()
                .filter(|(j, _)| *j != i)
                .take(ISLAND_LINKS)
                .collect()
        };
        near.sort_by_key(|(j, _)| *j);
        let links: Vec<Value> = near.iter().map(|(j, _)| json!(j.to_string())).collect();
        let weights: serde_json::Map<String, Value> = near
            .iter()
            .map(|(j, w)| (j.to_string(), json!(*w)))
            .collect();
        let mut atom = Record::new();
        atom.insert("id".into(), json!(i.to_string()));
        atom.insert("links".into(), Value::Array(links));
        atom.insert("link_weights".into(), Value::Object(weights));
        atoms.push(atom);
    }
    let graph = packset_core::island::Graph::from_atoms(&atoms);
    let seeds: Vec<(usize, f64)> = fused.iter().take(limit).copied().collect();
    packset_core::island::activate(&graph, &seeds, ISLAND_HOPS)
}

/// Sessions in the order their best document ranks.
fn collapse(order: impl Iterator<Item = usize>, docs: &[Document]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for ordinal in order {
        let sid = &docs[ordinal].session;
        if seen.insert(sid.clone()) {
            out.push(sid.clone());
        }
    }
    out
}

/// One protocol's documents and their lexical ranking, kept for the dense arms.
type Scored = (Vec<Document>, Vec<(usize, f64)>);

/// How many documents each ballot hands the fuse.
const FUSE_DEPTH: usize = 50;

fn cache_dir() -> Option<std::path::PathBuf> {
    let dir = std::path::PathBuf::from(std::env::var_os("PACKSET_LME_CACHE")?);
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

fn write_rows(path: &std::path::Path, rows: &[Vec<f32>]) {
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

fn read_rows(path: &std::path::Path, expected: usize) -> Option<Vec<Vec<f32>>> {
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

/// Document vectors for one question and protocol, from the cache when the
/// count matches, else encoded and cached.
fn vectors(nth: usize, protocol: &str, docs: &[Document]) -> Vec<Vec<f32>> {
    let model = std::env::var("PACKSET_EMBED_MODEL").unwrap_or_else(|_| "default".into());
    let file = cache_dir().map(|d| d.join(format!("{model}-{protocol}-q{nth}.bin")));
    if let Some(rows) = file.as_deref().and_then(|p| read_rows(p, docs.len())) {
        return rows;
    }
    let fresh: Vec<Vec<f32>> = docs
        .iter()
        .map(|d| packset_daemon::embed::encode_document(&d.text).unwrap_or_default())
        .collect();
    if let Some(path) = file.as_deref() {
        write_rows(path, &fresh);
    }
    fresh
}

#[derive(Default, Clone)]
struct Tally {
    asked: usize,
    /// Any answer session inside the cut-off.
    hit: Vec<usize>,
    /// Fraction of answer sessions inside the cut-off, summed.
    recall: Vec<f64>,
}

impl Tally {
    fn new() -> Self {
        Self {
            asked: 0,
            hit: vec![0; CUTOFFS.len()],
            recall: vec![0.0; CUTOFFS.len()],
        }
    }

    fn add(&mut self, ranked: &[String], answers: &BTreeSet<String>) {
        self.asked += 1;
        for (slot, &cut) in CUTOFFS.iter().enumerate() {
            let top: BTreeSet<&String> = ranked.iter().take(cut).collect();
            let found = answers.iter().filter(|a| top.contains(a)).count();
            if found > 0 {
                self.hit[slot] += 1;
            }
            self.recall[slot] += found as f64 / answers.len().max(1) as f64;
        }
    }

    fn row(&self, name: &str) -> String {
        let mut line = format!("| {name} | {} |", self.asked);
        for slot in 0..CUTOFFS.len() {
            line.push_str(&format!(
                " {:.3} | {:.3} |",
                self.hit[slot] as f64 / self.asked.max(1) as f64,
                self.recall[slot] / self.asked.max(1) as f64
            ));
        }
        line
    }
}

const PROTOCOLS: &[&str] = &["turns", "windows", "sessions"];

/// The protocol the dense and fused arms run on. Turns and windows are too
/// many to encode on a CPU: a hundred questions of windows did not finish in
/// four hours, fifty session documents a question do.
const DENSE_PROTOCOLS: &[&str] = &["sessions"];

fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "longmemeval_s.json".to_string());
    let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
    let mut asked = questions(&raw);
    anyhow::ensure!(!asked.is_empty(), "no questions in {path}");
    if let Some(cap) = std::env::var("PACKSET_LME_QUESTIONS")
        .ok()
        .and_then(|c| c.parse().ok())
    {
        asked.truncate(cap);
        println!("scoring {} questions, as asked", asked.len());
    }
    let encoder = packset_daemon::embed::binary().is_some();
    println!(
        "encoder: {}",
        if encoder {
            "present, dense and fused arms run on session documents"
        } else {
            "absent, lexical arms only"
        }
    );
    let mut arms: Vec<String> = PROTOCOLS.iter().map(|p| p.to_string()).collect();
    // `PACKSET_LME_RERANK=1` adds the cross-encoder arm: the windows of the
    // fused top sessions are scored against the question by the reranker
    // (bge-reranker-base unless `PACKSET_RERANK_MODEL` says otherwise) and
    // the sessions reordered by their best window. A window fits the
    // reranker's input; a whole session does not.
    let rerank = encoder && std::env::var_os("PACKSET_LME_RERANK").is_some();
    if encoder {
        for p in DENSE_PROTOCOLS {
            arms.push(format!("{p} dense"));
            arms.push(format!("{p} fused"));
            arms.push(format!("{p} fused recency"));
            arms.push(format!("{p} fused window"));
            arms.push(format!("{p} fused island"));
            if rerank {
                arms.push(format!("{p} fused rerank"));
            }
        }
    }
    // `PACKSET_LME_DUMP` names a JSONL file: one line a question with the
    // session ids each arm retrieved, for a reader model to answer from.
    // Retrieval and reading are two measurements; this file is the seam.
    let mut dump =
        std::env::var_os("PACKSET_LME_DUMP").map(|p| std::fs::File::create(p).expect("dump file"));
    let started = std::time::Instant::now();
    let mut overall: Vec<Tally> = arms.iter().map(|_| Tally::new()).collect();
    let mut by_kind: BTreeMap<String, Vec<Tally>> = BTreeMap::new();
    for (nth, question) in asked.iter().enumerate() {
        let mut slot = 0usize;
        let mut retrieved: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut record = |ranked: &[String], slot: usize| {
            overall[slot].add(ranked, &question.answers);
            by_kind
                .entry(question.kind.clone())
                .or_insert_with(|| arms.iter().map(|_| Tally::new()).collect())[slot]
                .add(ranked, &question.answers);
            retrieved.insert(
                arms[slot].clone(),
                ranked.iter().take(10).cloned().collect(),
            );
        };
        let mut kept: BTreeMap<&str, Scored> = BTreeMap::new();
        for protocol in PROTOCOLS {
            let docs = documents(question, protocol);
            let lex = lexical(question, &docs);
            record(&collapse(lex.iter().map(|(i, _)| *i), &docs), slot);
            slot += 1;
            kept.insert(protocol, (docs, lex));
        }
        if encoder {
            let query = packset_daemon::embed::encode_query(&question.text).unwrap_or_default();
            for protocol in DENSE_PROTOCOLS {
                let (docs, lex) = &kept[protocol];
                let vecs = vectors(nth, protocol, docs);
                let den = dense(&query, &vecs);
                record(&collapse(den.iter().map(|(i, _)| *i), docs), slot);
                slot += 1;
                let fused_docs = fused_scored(lex, &den, FUSE_DEPTH);
                let fused_sessions = collapse(fused_docs.iter().map(|(i, _)| *i), docs);
                record(&fused_sessions, slot);
                slot += 1;
                let aged = recency(question, docs, &fused_docs);
                record(&collapse(aged.iter().map(|(i, _)| *i), docs), slot);
                slot += 1;
                let framed = windowed(question, docs, &fused_docs);
                record(&collapse(framed.iter().map(|(i, _)| *i), docs), slot);
                slot += 1;
                let lit = island(&vecs, &fused_docs, CUTOFFS[CUTOFFS.len() - 1]);
                record(&collapse(lit.iter().map(|(i, _)| *i), docs), slot);
                slot += 1;
                if rerank {
                    let top: Vec<&str> = fused_sessions
                        .iter()
                        .take(packset_daemon::embed::RERANK_DEPTH)
                        .map(String::as_str)
                        .collect();
                    let (windows, _) = &kept["windows"];
                    let candidates: Vec<&Document> = windows
                        .iter()
                        .filter(|d| top.contains(&d.session.as_str()))
                        .collect();
                    let texts: Vec<String> = candidates.iter().map(|d| d.text.clone()).collect();
                    let reranked = match packset_daemon::embed::rerank(&question.text, &texts) {
                        Some(scores) if scores.len() == candidates.len() => {
                            let mut best: BTreeMap<&str, f32> = BTreeMap::new();
                            for (d, s) in candidates.iter().zip(&scores) {
                                let e = best.entry(d.session.as_str()).or_insert(f32::MIN);
                                if *s > *e {
                                    *e = *s;
                                }
                            }
                            let mut order: Vec<(&str, f32)> = best.into_iter().collect();
                            order.sort_by(|a, b| {
                                b.1.partial_cmp(&a.1)
                                    .unwrap_or(std::cmp::Ordering::Equal)
                                    .then_with(|| a.0.cmp(b.0))
                            });
                            let mut out: Vec<String> =
                                order.into_iter().map(|(s, _)| s.to_string()).collect();
                            // The tail keeps the fused order, as the daemon does.
                            for s in &fused_sessions {
                                if !out.contains(s) {
                                    out.push(s.clone());
                                }
                            }
                            out
                        }
                        _ => fused_sessions.clone(),
                    };
                    record(&reranked, slot);
                    slot += 1;
                }
            }
        }
        if let Some(file) = dump.as_mut() {
            let line = json!({
                "question_id": question.id,
                "question_type": question.kind,
                "question": question.text,
                "question_date": question.date,
                "answer": question.answer,
                "answer_session_ids": question.answers,
                "retrieved": retrieved,
            });
            writeln!(file, "{line}").expect("dump line");
        }
        if (nth + 1) % 50 == 0 {
            eprintln!(
                "{} questions, {:.0}s",
                nth + 1,
                started.elapsed().as_secs_f64()
            );
        }
    }
    let header = {
        let mut h = String::from("| arm | asked |");
        for cut in CUTOFFS {
            h.push_str(&format!(" hit@{cut} | recall@{cut} |"));
        }
        h
    };
    println!("\nLongMemEval_S, session granularity\n");
    println!("{header}");
    println!("|---|---|{}", "---|---|".repeat(CUTOFFS.len()));
    for (slot, arm) in arms.iter().enumerate() {
        println!("{}", overall[slot].row(arm));
    }
    for (kind, tallies) in &by_kind {
        println!("\n{kind}\n\n{header}");
        println!("|---|---|{}", "---|---|".repeat(CUTOFFS.len()));
        for (slot, arm) in arms.iter().enumerate() {
            println!("{}", tallies[slot].row(arm));
        }
    }
    println!(
        "\n{} questions in {:.1}s",
        asked.len(),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}
