//! Retrieval quality against LoCoMo's relevance judgements
//! (doi:10.48550/arXiv.2402.17753): 1986 questions over ten conversations,
//! each labelled with the turns that answer it. Turns are loaded as atoms, so
//! this measures the scorer, not what should have been remembered, and it is
//! recall of labelled evidence with no model in the loop, not the end-to-end
//! answer accuracy the memory papers report. Category 5 (unanswerable) is
//! excluded.
//!
//! ```console
//! $ curl -sSLO https://raw.githubusercontent.com/snap-research/locomo/main/data/locomo10.json
//! $ cargo run --release -p packset-daemon --example locomo -- locomo10.json
//! ```
//!
//! Knobs, all off by default: `PACKSET_LOCOMO_RERANK` (cross-encoder second
//! stage, a forward pass per candidate), `PACKSET_LOCOMO_LATE` (per-token
//! arms), `PACKSET_LOCOMO_WALK` (link-graph walk), `PACKSET_LOCOMO_CONVERSATIONS`
//! (score the first N; not comparable to a full run), `PACKSET_LOCOMO_CACHE`
//! (directory for encodings).

use std::collections::BTreeSet;

use packset_core::bm25::{Index, Scorer};
use packset_core::panel::Panel;
use packset_core::search::{self, Ask, Record};
use serde_json::{json, Value};

/// Cut-offs to report. A pack hands a model a small context, so the small ones
/// are the ones that matter.
const CUTOFFS: &[usize] = &[1, 5, 10, 20];

/// LoCoMo's adversarial category: the conversation does not answer it.
const ADVERSARIAL: i64 = 5;

/// One question and the turns that answer it.
struct Question {
    text: String,
    evidence: BTreeSet<String>,
    category: i64,
}

/// One conversation: its turns as atoms, and its questions.
struct Conversation {
    atoms: Vec<Record>,
    questions: Vec<Question>,
}

/// What one arm scored, summed over questions.
#[derive(Default, Clone)]
struct Tally {
    /// Fraction of labelled evidence inside the cut-off, summed.
    recall: Vec<f64>,
    /// Questions with at least one labelled turn inside the cut-off.
    hit: Vec<usize>,
    /// Discounted gain at `NDCG_CUT`, summed, against the ideal ordering.
    ndcg: f64,
    /// Questions counted.
    asked: usize,
}

/// Where NDCG is reported. The published number on this benchmark is at five.
const NDCG_CUT: usize = 5;

/// nDCG of a ranking under binary relevance.
fn ndcg_at(ranked: &[String], evidence: &BTreeSet<String>, cut: usize) -> f64 {
    let discount = |place: usize| 1.0 / ((place + 2) as f64).log2();
    let gain: f64 = ranked
        .iter()
        .take(cut)
        .enumerate()
        .filter(|(_, id)| evidence.contains(*id))
        .map(|(place, _)| discount(place))
        .sum();
    let ideal: f64 = (0..cut.min(evidence.len())).map(discount).sum();
    if ideal <= 0.0 {
        0.0
    } else {
        gain / ideal
    }
}

impl Tally {
    fn new() -> Self {
        Self {
            recall: vec![0.0; CUTOFFS.len()],
            hit: vec![0; CUTOFFS.len()],
            ndcg: 0.0,
            asked: 0,
        }
    }

    fn add(&mut self, ranked: &[String], evidence: &BTreeSet<String>) {
        self.asked += 1;
        self.ndcg += ndcg_at(ranked, evidence, NDCG_CUT);
        for (slot, cut) in CUTOFFS.iter().enumerate() {
            let seen: BTreeSet<&String> = ranked.iter().take(*cut).collect();
            let found = evidence.iter().filter(|id| seen.contains(id)).count();
            self.recall[slot] += found as f64 / evidence.len() as f64;
            if found > 0 {
                self.hit[slot] += 1;
            }
        }
    }

    fn merge(&mut self, other: &Self) {
        self.asked += other.asked;
        self.ndcg += other.ndcg;
        for slot in 0..CUTOFFS.len() {
            self.recall[slot] += other.recall[slot];
            self.hit[slot] += other.hit[slot];
        }
    }
}

/// The id a hit names, which for these atoms is the dialogue turn.
fn hit_ids(hits: &[Value]) -> Vec<String> {
    hits.iter()
        .filter_map(|hit| hit.get("id").and_then(Value::as_str).map(str::to_string))
        .collect()
}

fn conversations(raw: &Value) -> Vec<Conversation> {
    let mut out = Vec::new();
    for sample in raw.as_array().map(Vec::as_slice).unwrap_or_default() {
        let mut atoms: Vec<Record> = Vec::new();
        let Some(conversation) = sample.get("conversation").and_then(Value::as_object) else {
            continue;
        };
        // Sessions are numbered keys beside their date, so take the arrays.
        let mut sessions: Vec<(&String, &Value)> = conversation
            .iter()
            .filter(|(key, value)| key.starts_with("session_") && value.is_array())
            .collect();
        sessions.sort_by_key(|(key, _)| {
            key.trim_start_matches("session_")
                .parse::<u32>()
                .unwrap_or(u32::MAX)
        });
        for (_, turns) in sessions {
            for turn in turns.as_array().map(Vec::as_slice).unwrap_or_default() {
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
                // No `entities`: the speaker as the only entity would link
                // every pair of one person's turns.
                let atom = json!({
                    "id": id,
                    "workspace": "locomo",
                    "kind": "conclusion",
                    "text": format!("{speaker}: {text}"),
                });
                atoms.push(atom.as_object().expect("object").clone());
            }
        }

        let questions = sample
            .get("qa")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(|qa| {
                let text = qa.get("question").and_then(Value::as_str)?.to_string();
                let evidence: BTreeSet<String> = qa
                    .get("evidence")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect();
                Some(Question {
                    text,
                    evidence,
                    category: qa.get("category").and_then(Value::as_i64).unwrap_or(0),
                })
            })
            .collect();
        out.push(Conversation { atoms, questions });
    }
    out
}

/// The arms at session granularity under both protocols: rank turns and read
/// off the session, or index the session as one document.
const PROTOCOLS: &[&str] = &[
    "turn bm25",
    "turn bm25+",
    "session bm25",
    "session bm25+",
    "session bm25 rm3",
    "passage bm25",
    "passage bm25+",
    "passage dirichlet",
    "passage bm25+ & dirichlet",
    "turn dense",
    "session bm25 + turn dense",
    "passage bm25 + turn dense",
    "passage bm25+ + turn dense",
    "passage dense",
    "passage bm25+ + passage dense",
    "session bm25 + turn late",
    "session bm25 + turn m3 sparse",
    "turn splade",
    "passage bm25+ + turn splade",
    "passage bm25+ + turn dense, reranked",
];

/// The arms compared, each a way of turning one question into a ranking.
const ARMS: &[&str] = &[
    "lexical",
    "bm25",
    "bm25 rm3",
    "dense",
    "lexical+bm25 (shipped)",
    "lexical+bm25+dense",
    "bm25+dense",
    "bm25 rm3+dense",
    "late",
    "bm25+late",
    "m3 pooled",
    "bm25+m3 pooled",
    "m3 sparse",
    "bm25+m3 sparse",
    "m3 sparse+late",
];

/// The diversify slot's three settings over the leading arm. LoCoMo scores
/// recall of labelled evidence, so a redundancy suppressor has nothing to be
/// credited for here; the README carries the numbers. Decay is not swept:
/// recency does not predict relevance on this corpus.
const DIVERSIFIERS: &[&str] = &["mmr", "dpp", "none"];

/// Every fusion the panel accepts over one pair of ballots, from one encode.
const VOTERS: &[&str] = &[
    "borda", "rrf", "combsum", "combmnz", "dowdall", "kemeny", "schulze", "copeland", "tideman",
];

/// The voters swept again over the shipped ballots; five, since the pairwise
/// voters degenerate on two ballots and the sweep above covers them.
const SHIPPED_VOTERS: &[&str] = &["borda", "rrf", "combsum", "combmnz", "dowdall"];

/// Where encodings are kept between runs, when `PACKSET_LOCOMO_CACHE` names a
/// directory; the encode dominates a run.
fn cache_dir() -> Option<std::path::PathBuf> {
    let raw = std::env::var_os("PACKSET_LOCOMO_CACHE")?;
    let dir = std::path::PathBuf::from(raw);
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// The model the vectors came from, so two models never share a file.
fn model_name() -> String {
    std::env::var("PACKSET_EMBED_MODEL").unwrap_or_else(|_| "default".to_string())
}

/// Rows of floats, length-prefixed, so a ragged set reads back as it was
/// written and a truncated file fails the count check rather than the scoring.
fn write_rows(path: &std::path::Path, rows: &[Vec<f32>]) {
    // A run whose encoder died halfway has rows that are empty rather than
    // wrong, and caching those would make the next run read a failure as an
    // answer. An absent encoder is a supported state; a cached absence is not.
    if rows.is_empty() || rows.iter().any(Vec::is_empty) {
        return;
    }
    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    for row in rows {
        bytes.extend_from_slice(&(row.len() as u32).to_le_bytes());
        for value in row {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
    }
    // Written beside and renamed, so a cancelled run leaves no half file that
    // the next one would read as complete.
    let temporary = path.with_extension("part");
    if std::fs::write(&temporary, &bytes).is_ok() {
        let _ = std::fs::rename(&temporary, path);
    }
}

/// A group of rows per item, with both counts so a truncated file fails its
/// check.
fn write_groups(path: &std::path::Path, groups: &[Vec<Vec<f32>>]) {
    if groups.is_empty() || groups.iter().any(Vec::is_empty) {
        return;
    }
    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(&(groups.len() as u32).to_le_bytes());
    for group in groups {
        bytes.extend_from_slice(&(group.len() as u32).to_le_bytes());
        for row in group {
            bytes.extend_from_slice(&(row.len() as u32).to_le_bytes());
            for value in row {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
        }
    }
    let temporary = path.with_extension("part");
    if std::fs::write(&temporary, &bytes).is_ok() {
        let _ = std::fs::rename(&temporary, path);
    }
}

fn read_groups(path: &std::path::Path, expected: usize) -> Option<Vec<Vec<Vec<f32>>>> {
    let bytes = std::fs::read(path).ok()?;
    let mut at = 0usize;
    let mut take = |width: usize| -> Option<&[u8]> {
        let slice = bytes.get(at..at + width)?;
        at += width;
        Some(slice)
    };
    let count = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
    if count != expected {
        return None;
    }
    let mut groups = Vec::with_capacity(count);
    for _ in 0..count {
        let tokens = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
        let mut group = Vec::with_capacity(tokens);
        for _ in 0..tokens {
            let width = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
            let raw = take(width * 4)?;
            group.push(
                raw.as_chunks::<4>()
                    .0
                    .iter()
                    .copied()
                    .map(f32::from_le_bytes)
                    .collect(),
            );
        }
        groups.push(group);
    }
    Some(groups)
}

/// Learned term weights as one row: index, weight, index, weight, all f32.
fn flatten_sparse(sparse: &packset_daemon::embed::Sparse) -> Vec<f32> {
    let mut out = Vec::with_capacity(sparse.len() * 2);
    for (index, weight) in sparse {
        out.push(*index as f32);
        out.push(*weight);
    }
    out
}

fn unflatten_sparse(row: Vec<f32>) -> packset_daemon::embed::Sparse {
    row.as_chunks::<2>()
        .0
        .iter()
        .map(|pair| (pair[0] as u32, pair[1]))
        .collect()
}

fn read_rows(path: &std::path::Path, expected: usize) -> Option<Vec<Vec<f32>>> {
    let bytes = std::fs::read(path).ok()?;
    let mut at = 0usize;
    let mut take = |width: usize| -> Option<&[u8]> {
        let slice = bytes.get(at..at + width)?;
        at += width;
        Some(slice)
    };
    let count = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
    if count != expected {
        return None;
    }
    let mut rows = Vec::with_capacity(count);
    for _ in 0..count {
        let width = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
        let raw = take(width * 4)?;
        rows.push(
            raw.as_chunks::<4>()
                .0
                .iter()
                .copied()
                .map(f32::from_le_bytes)
                .collect(),
        );
    }
    Some(rows)
}

/// How many conversations to score; all ten by default.
fn conversation_cap() -> Option<usize> {
    std::env::var("PACKSET_LOCOMO_CONVERSATIONS")
        .ok()
        .and_then(|raw| raw.trim().parse().ok())
        .filter(|n| *n > 0)
}

/// Whether to run the restart walk; off unless asked, since it touches every
/// edge every round and measured below letting the ranking continue.
fn walk_wanted() -> bool {
    std::env::var("PACKSET_LOCOMO_WALK").is_ok_and(|v| !v.is_empty() && v != "0")
}

fn late_wanted() -> bool {
    std::env::var("PACKSET_LOCOMO_LATE").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// Whether the SPLADE++ learned-sparse ballot runs; BGE-M3's sparse head is a
/// side output of a dense model and rides with the late arms instead.
fn splade_wanted() -> bool {
    std::env::var("PACKSET_LOCOMO_SPLADE").is_ok_and(|v| !v.is_empty() && v != "0")
}

fn rerank_wanted() -> bool {
    std::env::var("PACKSET_LOCOMO_RERANK").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// How deep the second stage reads. Same window `/v1/search` uses.
const RERANK_DEPTH: usize = packset_daemon::embed::RERANK_DEPTH;

/// Reorder the top of a ranking by the stage `/v1/search` runs, timed.
fn reranked(question: &str, hits: &[Value], spent: &mut std::time::Duration) -> Vec<Value> {
    let start = std::time::Instant::now();
    let out = packset_daemon::embed::rerank_hits(question, hits).unwrap_or_else(|| hits.to_vec());
    *spent += start.elapsed();
    out
}

/// Where the one-hop comparison is made: half the places are retrieved and the
/// rest are filled, either by the ranking continuing or by neighbours.
const HOP_CUT: usize = 20;

/// The session a dialogue turn belongs to, from `D<session>:<turn>`.
fn session_of(id: &str) -> &str {
    id.split_once(':').map_or(id, |(session, _)| session)
}

/// A turn ranking read as a session ranking, best turn first.
fn sessions_of(ranked: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    ranked
        .iter()
        .map(|id| session_of(id).to_string())
        .filter(|session| seen.insert(session.clone()))
        .collect()
}

/// How many turns a passage covers; set, not fitted to the benchmark.
const WINDOW: usize = 6;

/// How far one window starts after the last: half a window, so a match
/// spanning a boundary is whole in the next.
const STRIDE: usize = 3;

/// A conversation as overlapping windows of adjacent turns: passage evidence
/// (Callan, doi:10.1007/978-1-4471-2099-5_31). Each window carries its session
/// id, so a window ranking collapses to a session ranking.
fn passage_documents(atoms: &[Record]) -> Vec<Record> {
    // Grouped by session rather than by run of adjacent atoms, so a session
    // whose turns are not contiguous still yields one series of windows and
    // window ids stay unique.
    let mut by_room: Vec<(String, Vec<&str>)> = Vec::new();
    let mut where_room: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for atom in atoms {
        let id = atom.get("id").and_then(Value::as_str).unwrap_or_default();
        let room = session_of(id).to_string();
        let line = atom.get("text").and_then(Value::as_str).unwrap_or_default();
        match where_room.get(&room) {
            Some(at) => by_room[*at].1.push(line),
            None => {
                where_room.insert(room.clone(), by_room.len());
                by_room.push((room, vec![line]));
            }
        }
    }

    let mut out = Vec::new();
    for (room, turns) in by_room {
        let mut at = 0usize;
        loop {
            let end = (at + WINDOW).min(turns.len());
            let text = turns[at..end].join("\n");
            // The window's own id names the session and where it starts, so a
            // hit can be read back to a session and the ones from a session
            // stay distinct.
            out.push(
                json!({
                    "id": format!("{room}#{at}"),
                    "workspace": "locomo",
                    "kind": "conclusion",
                    "text": text,
                })
                .as_object()
                .expect("object")
                .clone(),
            );
            if end == turns.len() {
                break;
            }
            at += STRIDE;
        }
    }
    out
}

/// The session a window id names, which is everything before the `#`.
fn room_of_window(id: &str) -> &str {
    id.split_once('#').map_or(id, |(room, _)| room)
}

/// A conversation's sessions as one document each, in order.
fn session_documents(atoms: &[Record]) -> Vec<Record> {
    let mut order: Vec<String> = Vec::new();
    let mut bodies: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for atom in atoms {
        let id = atom.get("id").and_then(Value::as_str).unwrap_or_default();
        let room = session_of(id).to_string();
        let line = atom.get("text").and_then(Value::as_str).unwrap_or_default();
        match bodies.get_mut(&room) {
            Some(held) => {
                held.push('\n');
                held.push_str(line);
            }
            None => {
                order.push(room.clone());
                bodies.insert(room, line.to_string());
            }
        }
    }
    order
        .into_iter()
        .map(|room| {
            let text = bodies.remove(&room).unwrap_or_default();
            json!({
                "id": room,
                "workspace": "locomo",
                "kind": "conclusion",
                "text": text,
            })
            .as_object()
            .expect("object")
            .clone()
        })
        .collect()
}

/// A turn ranking read as a session ranking: each session keeps the place of
/// its best turn, which is the maximum-similarity protocol the papers state.
fn collapse(hits: &[Value]) -> Vec<Value> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for hit in hits {
        let id = hit.get("id").and_then(Value::as_str).unwrap_or_default();
        let room = session_of(id).to_string();
        if !seen.insert(room.clone()) {
            continue;
        }
        let mut copy = hit.clone();
        copy["id"] = json!(room);
        out.push(copy);
    }
    out
}

/// A window ranking read as a session ranking, best window first.
fn collapse_windows(hits: &[Value]) -> Vec<Value> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for hit in hits {
        let id = hit.get("id").and_then(Value::as_str).unwrap_or_default();
        let room = room_of_window(id).to_string();
        if !seen.insert(room.clone()) {
            continue;
        }
        let mut copy = hit.clone();
        copy["id"] = json!(room);
        out.push(copy);
    }
    out
}

/// Rank the corpus by late interaction, in the same hit shape as the others.
///
/// Ordinals line up with `ask.atoms`, the way the BM25 index's do.
fn rank_late(ask: &Ask<'_>, query: &[Vec<f32>], documents: &[Vec<Vec<f32>>]) -> Vec<Value> {
    let mut hits: Vec<Value> = Vec::new();
    for (ordinal, atom) in ask.atoms.iter().enumerate() {
        let Some(tokens) = documents.get(ordinal) else {
            continue;
        };
        let relevance = search::max_sim(query, tokens);
        if relevance <= 0.0 {
            continue;
        }
        hits.push(json!({
            "field": "atom",
            "id": atom.get("id").cloned().unwrap_or(Value::Null),
            "kind": atom.get("kind").cloned().unwrap_or(Value::Null),
            "text": atom.get("text").cloned().unwrap_or(Value::Null),
            "score": relevance,
        }));
    }
    sort_by_score(&mut hits);
    hits.truncate(ask.limit);
    hits
}

/// Rank by the learned term weights the same pass returned: a dot product
/// over shared vocabulary entries.
fn rank_sparse(
    ask: &Ask<'_>,
    query: &packset_daemon::embed::Sparse,
    documents: &[packset_daemon::embed::Sparse],
) -> Vec<Value> {
    let mut hits: Vec<Value> = Vec::new();
    for (ordinal, atom) in ask.atoms.iter().enumerate() {
        let Some(weights) = documents.get(ordinal).filter(|held| !held.is_empty()) else {
            continue;
        };
        let relevance = search::sparse_dot(query, weights);
        if relevance <= 0.0 {
            continue;
        }
        hits.push(json!({
            "field": "atom",
            "id": atom.get("id").cloned().unwrap_or(Value::Null),
            "kind": atom.get("kind").cloned().unwrap_or(Value::Null),
            "text": atom.get("text").cloned().unwrap_or(Value::Null),
            "score": relevance,
        }));
    }
    sort_by_score(&mut hits);
    hits.truncate(ask.limit);
    hits
}

/// Best score first, then the id, so a tie is broken the same way every run.
fn sort_by_score(hits: &mut [Value]) {
    hits.sort_by(|a, b| {
        b["score"]
            .as_f64()
            .unwrap_or(0.0)
            .partial_cmp(&a["score"].as_f64().unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                a["id"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(b["id"].as_str().unwrap_or(""))
            })
    });
}

/// Rank by cosine against vectors held beside the atoms rather than inside
/// them, which is what the per-token encoder's pooled output needs.
fn rank_pooled(ask: &Ask<'_>, query: &[f32], documents: &[Vec<f32>]) -> Vec<Value> {
    let mut hits: Vec<Value> = Vec::new();
    for (ordinal, atom) in ask.atoms.iter().enumerate() {
        let Some(vector) = documents.get(ordinal).filter(|v| !v.is_empty()) else {
            continue;
        };
        let relevance = search::cosine(query, vector);
        if relevance <= 0.0 {
            continue;
        }
        hits.push(json!({
            "field": "atom",
            "id": atom.get("id").cloned().unwrap_or(Value::Null),
            "kind": atom.get("kind").cloned().unwrap_or(Value::Null),
            "text": atom.get("text").cloned().unwrap_or(Value::Null),
            "score": relevance,
        }));
    }
    sort_by_score(&mut hits);
    hits.truncate(ask.limit);
    hits
}

/// The stored link graph as positions, built once per conversation.
struct Graph {
    ids: Vec<String>,
    at: std::collections::HashMap<String, usize>,
    edges: Vec<Vec<usize>>,
}

impl Graph {
    fn of(atoms: &[Record]) -> Self {
        // One pass, so a node's position in `ids` is its position in `edges`:
        // an atom without an id would otherwise shift every edge after it.
        let held: Vec<&Record> = atoms
            .iter()
            .filter(|atom| atom.get("id").and_then(Value::as_str).is_some())
            .collect();
        let ids: Vec<String> = held
            .iter()
            .filter_map(|atom| atom.get("id").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
        let at: std::collections::HashMap<String, usize> = ids
            .iter()
            .enumerate()
            .map(|(index, id)| (id.clone(), index))
            .collect();
        let edges = held
            .iter()
            .map(|atom| {
                atom.get("links")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                    .iter()
                    .filter_map(Value::as_str)
                    .filter_map(|peer| at.get(peer).copied())
                    .collect()
            })
            .collect();
        Self { ids, at, edges }
    }

    /// Personalised PageRank from a seeded restart distribution, as HippoRAG
    /// uses it (doi:10.48550/arXiv.2405.14831).
    fn walk(&self, seeds: &[String], damping: f64, rounds: usize) -> Vec<(usize, f64)> {
        let n = self.ids.len();
        let mut restart = vec![0.0f64; n];
        let mut mass = 0.0;
        // Weighted by where the ranking put it, so the top seed pulls hardest.
        for (place, id) in seeds.iter().enumerate() {
            if let Some(index) = self.at.get(id) {
                let weight = 1.0 / (place + 1) as f64;
                restart[*index] += weight;
                mass += weight;
            }
        }
        if mass == 0.0 {
            return Vec::new();
        }
        for value in &mut restart {
            *value /= mass;
        }
        let mut rank = restart.clone();
        let mut next = vec![0.0f64; n];
        for _ in 0..rounds {
            next.iter_mut().for_each(|value| *value = 0.0);
            let mut dangling = 0.0;
            for (node, peers) in self.edges.iter().enumerate() {
                if peers.is_empty() {
                    dangling += rank[node];
                    continue;
                }
                let share = rank[node] / peers.len() as f64;
                for peer in peers {
                    next[*peer] += share;
                }
            }
            for (index, value) in next.iter_mut().enumerate() {
                // A node with no links returns its mass to the restart, which
                // keeps the walk a distribution rather than leaking it.
                *value = damping * (*value + dangling * restart[index])
                    + (1.0 - damping) * restart[index];
            }
            std::mem::swap(&mut rank, &mut next);
        }
        let mut order: Vec<(usize, f64)> = rank.into_iter().enumerate().collect();
        order.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        order
    }

    /// The ranking's own places kept, the rest filled by the walk.
    fn expand(&self, ranked: &[String], limit: usize) -> Vec<String> {
        let mut out: Vec<String> = ranked.to_vec();
        let mut seen: BTreeSet<&str> = ranked.iter().map(String::as_str).collect();
        for (index, score) in self.walk(ranked, WALK_DAMPING, WALK_ROUNDS) {
            if out.len() >= limit {
                break;
            }
            if score <= 0.0 {
                break;
            }
            let id = &self.ids[index];
            if seen.insert(id.as_str()) {
                out.push(id.clone());
            }
        }
        out
    }
}

/// How much of the walk's mass stays on the graph rather than restarting.
const WALK_DAMPING: f64 = 0.5;

/// Rounds of the walk. The graph is bounded at eight neighbours a node, so the
/// distribution settles well inside this.
const WALK_ROUNDS: usize = 20;

/// Follow each hit's stored links once, appending neighbours behind the hits.
fn one_hop(ranked: &[String], atoms: &[Record], limit: usize) -> Vec<String> {
    let links: std::collections::HashMap<&str, Vec<&str>> = atoms
        .iter()
        .filter_map(|atom| {
            let id = atom.get("id").and_then(Value::as_str)?;
            let peers = atom
                .get("links")
                .and_then(Value::as_array)?
                .iter()
                .filter_map(Value::as_str)
                .collect();
            Some((id, peers))
        })
        .collect();
    let mut out: Vec<String> = ranked.to_vec();
    let mut seen: BTreeSet<String> = ranked.iter().cloned().collect();
    for id in ranked {
        for peer in links
            .get(id.as_str())
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            if out.len() >= limit {
                return out;
            }
            if seen.insert((*peer).to_string()) {
                out.push((*peer).to_string());
            }
        }
    }
    out
}

/// One table: a row per name, recall then hit at every cut-off.
fn table(names: &[&str], tallies: &[Tally]) {
    print!("{:<22}", "arm");
    for cut in CUTOFFS {
        print!("{:>10}", format!("R@{cut}"));
    }
    for cut in CUTOFFS {
        print!("{:>10}", format!("hit@{cut}"));
    }
    print!("{:>10}", format!("nDCG@{NDCG_CUT}"));
    println!();
    println!("{}", "-".repeat(32 + CUTOFFS.len() * 20));
    for (slot, name) in names.iter().enumerate() {
        let tally = &tallies[slot];
        // An arm that was switched off was asked nothing, and a row of zeros
        // reads as a method that found nothing rather than one that did not
        // run.
        if tally.asked == 0 {
            continue;
        }
        let counted = tally.asked.max(1) as f64;
        print!("{name:<22}");
        for value in &tally.recall {
            print!("{:>10.3}", value / counted);
        }
        for value in &tally.hit {
            print!("{:>10.3}", *value as f64 / counted);
        }
        print!("{:>10.3}", tally.ndcg / counted);
        println!();
    }
}

fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "locomo10.json".to_string());
    let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
    let mut corpus = conversations(&raw);
    anyhow::ensure!(!corpus.is_empty(), "no conversations in {path}");
    if let Some(cap) = conversation_cap() {
        corpus.truncate(cap);
        println!("scoring {} of the conversations, as asked", corpus.len());
    }

    let now = packset_core::clock::utcnow();
    let shipped = Panel::named("borda", "mmr", "off")?;
    let panels: Vec<Panel> = VOTERS
        .iter()
        .map(|name| Panel::named(name, "mmr", "off"))
        .collect::<Result<_, _>>()?;
    let mut totals: Vec<Tally> = ARMS.iter().map(|_| Tally::new()).collect();
    let mut by_session: Vec<Tally> = ARMS.iter().map(|_| Tally::new()).collect();
    let mut voted: Vec<Tally> = VOTERS.iter().map(|_| Tally::new()).collect();
    let mut voted_sessions: Vec<Tally> = VOTERS.iter().map(|_| Tally::new()).collect();
    let mut protocol: Vec<Tally> = PROTOCOLS.iter().map(|_| Tally::new()).collect();
    let diversifiers: Vec<Panel> = DIVERSIFIERS
        .iter()
        .map(|name| Panel::named("combsum", name, "off"))
        .collect::<Result<_, _>>()?;
    let mut diversified: Vec<Tally> = DIVERSIFIERS.iter().map(|_| Tally::new()).collect();
    let mut room_voted: Vec<Tally> = VOTERS.iter().map(|_| Tally::new()).collect();
    // The pair the seat actually ships, swept the same way, because a default
    // argued from a pair the seat does not use is an argument about a
    // different retriever.
    let shipped_panels: Vec<Panel> = SHIPPED_VOTERS
        .iter()
        .map(|name| Panel::named(name, "mmr", "off"))
        .collect::<Result<_, _>>()?;
    let mut shipped_voted: Vec<Tally> = SHIPPED_VOTERS.iter().map(|_| Tally::new()).collect();
    let mut shipped_sessions: Vec<Tally> = SHIPPED_VOTERS.iter().map(|_| Tally::new()).collect();
    let mut ranking = Tally::new();
    let mut hopped = Tally::new();
    let mut walked = Tally::new();
    let mut turns = 0usize;
    let mut adversarial = 0usize;
    let mut linked = 0usize;
    let mut widest = 0usize;
    let encoder = packset_daemon::embed::binary();
    match &encoder {
        Some(path) => println!("encoder: {}", path.display()),
        None => println!("encoder: absent, so the dense arms will be empty"),
    }
    let encoder = encoder.is_some();
    let mut questions: std::collections::HashMap<String, Vec<f32>> =
        std::collections::HashMap::new();
    let walking = walk_wanted();
    if walking {
        println!("restart walk: on, which is most of the time a run takes");
    }
    let late = encoder && late_wanted();
    if late {
        println!("late interaction: on, one vector per token");
    }
    let reranking = encoder && rerank_wanted();
    let mut rerank_spent = std::time::Duration::ZERO;
    if reranking {
        println!(
            "second stage: on, a cross-encoder over the top {RERANK_DEPTH}, which is a forward pass a candidate a question"
        );
    }
    let mut late_questions: std::collections::HashMap<String, Vec<Vec<f32>>> =
        std::collections::HashMap::new();
    // The same model's pooled output, so late interaction can be compared with
    // the model held fixed rather than against a different one.
    let mut m3_questions: std::collections::HashMap<String, Vec<f32>> =
        std::collections::HashMap::new();
    // And the learned term weights, from that same pass.
    let splade = encoder && splade_wanted();
    if splade {
        println!("learned sparse: on, SPLADE++ rather than a dense model's side output");
    }
    let mut splade_questions: std::collections::HashMap<String, packset_daemon::embed::Sparse> =
        std::collections::HashMap::new();
    let mut sparse_questions: std::collections::HashMap<String, packset_daemon::embed::Sparse> =
        std::collections::HashMap::new();

    for (nth, conversation) in corpus.iter_mut().enumerate() {
        turns += conversation.atoms.len();
        // The write path builds the graph; `apply_links` bounds the peer side.
        let mut stored: Vec<Record> = Vec::with_capacity(conversation.atoms.len());
        for atom in &conversation.atoms {
            let mut fresh = atom.clone();
            let rewritten = packset_core::record::apply_links(
                &mut fresh,
                &stored,
                packset_core::record::LINK_THRESHOLD,
                &now,
            );
            for peer in rewritten {
                let Some(id) = peer.get("id").and_then(Value::as_str).map(str::to_string) else {
                    continue;
                };
                if let Some(slot) = stored
                    .iter_mut()
                    .find(|held| held.get("id").and_then(Value::as_str) == Some(id.as_str()))
                {
                    *slot = peer;
                }
            }
            stored.push(fresh);
        }
        conversation.atoms = stored;
        linked += conversation
            .atoms
            .iter()
            .map(|atom| {
                atom.get("links")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len)
            })
            .sum::<usize>();
        widest = widest.max(
            conversation
                .atoms
                .iter()
                .map(|atom| {
                    atom.get("links")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len)
                })
                .max()
                .unwrap_or(0),
        );
        // Built only when the walk runs: the adjacency costs a pass over every
        // atom's links, which a run that is not walking has no use for.
        let graph = walking.then(|| Graph::of(&conversation.atoms));
        let documents: Vec<Vec<String>> =
            conversation.atoms.iter().map(search::atom_tokens).collect();
        let index = Index::build(documents.iter().map(Vec::as_slice));
        // The same corpus with the session as the document rather than the turn.
        let session_corpus = session_documents(&conversation.atoms);
        let room_documents: Vec<Vec<String>> =
            session_corpus.iter().map(search::atom_tokens).collect();
        let room_index = Index::build(room_documents.iter().map(Vec::as_slice));
        // And once more with the passage as the document, which is the middle
        // the two above are the ends of.
        let mut passage_corpus = passage_documents(&conversation.atoms);
        let passage_tokens: Vec<Vec<String>> =
            passage_corpus.iter().map(search::atom_tokens).collect();
        // The passage protocol on the dense side too: both scorers on both units.
        if encoder {
            let model = model_name();
            let file = cache_dir().map(|dir| dir.join(format!("{model}-windows-{nth}.vec")));
            let cached = file
                .as_deref()
                .and_then(|path| read_rows(path, passage_corpus.len()));
            let vectors = cached.unwrap_or_else(|| {
                let fresh: Vec<Vec<f32>> = passage_corpus
                    .iter()
                    .map(|window| {
                        let text = window
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        packset_daemon::embed::encode_document(text).unwrap_or_default()
                    })
                    .collect();
                if let Some(path) = file.as_deref() {
                    write_rows(path, &fresh);
                }
                fresh
            });
            for (window, vector) in passage_corpus.iter_mut().zip(vectors) {
                if !vector.is_empty() {
                    window.insert(
                        "embedding".into(),
                        Value::Array(vector.into_iter().map(|f| json!(f)).collect()),
                    );
                }
            }
        }
        // SPLADE++ over turns, cached the way the other sparse weights are.
        let mut splade_atoms: Vec<packset_daemon::embed::Sparse> = Vec::new();
        if splade {
            let file = cache_dir().map(|dir| dir.join(format!("splade-atoms-{nth}.vec")));
            let count = conversation.atoms.len();
            match file.as_deref().and_then(|p| read_rows(p, count)) {
                Some(rows) => splade_atoms = rows.into_iter().map(unflatten_sparse).collect(),
                None => {
                    for atom in &conversation.atoms {
                        let text = atom.get("text").and_then(Value::as_str).unwrap_or_default();
                        splade_atoms
                            .push(packset_daemon::embed::encode_sparse(text).unwrap_or_default());
                    }
                    if let Some(path) = file.as_deref() {
                        let flat: Vec<Vec<f32>> = splade_atoms.iter().map(flatten_sparse).collect();
                        write_rows(path, &flat);
                    }
                }
            }
            let qfile = cache_dir().map(|dir| dir.join(format!("splade-questions-{nth}.vec")));
            let asked = match qfile
                .as_deref()
                .and_then(|p| read_rows(p, conversation.questions.len()))
            {
                Some(rows) => rows.into_iter().map(unflatten_sparse).collect::<Vec<_>>(),
                None => {
                    let fresh: Vec<packset_daemon::embed::Sparse> = conversation
                        .questions
                        .iter()
                        .map(|q| packset_daemon::embed::encode_sparse(&q.text).unwrap_or_default())
                        .collect();
                    if let Some(path) = qfile.as_deref() {
                        let flat: Vec<Vec<f32>> = fresh.iter().map(flatten_sparse).collect();
                        write_rows(path, &flat);
                    }
                    fresh
                }
            };
            for (question, weights) in conversation.questions.iter().zip(asked) {
                if !weights.is_empty() {
                    splade_questions.insert(question.text.clone(), weights);
                }
            }
        }
        let passage_index = Index::build(passage_tokens.iter().map(Vec::as_slice));

        // One encode of the corpus and one of the questions, through the kept
        // child the daemon uses. Absent encoder means the dense arms are empty
        // and the lexical ones still report, which is the seat's own fallback.
        if encoder {
            let model = model_name();
            let held = cache_dir();
            let atom_file = held
                .as_ref()
                .map(|dir| dir.join(format!("{model}-atoms-{nth}.vec")));
            let question_file = held
                .as_ref()
                .map(|dir| dir.join(format!("{model}-questions-{nth}.vec")));

            let cached = atom_file
                .as_deref()
                .and_then(|path| read_rows(path, conversation.atoms.len()));
            let vectors = cached.unwrap_or_else(|| {
                let fresh: Vec<Vec<f32>> = conversation
                    .atoms
                    .iter()
                    .map(|atom| {
                        let text = atom.get("text").and_then(Value::as_str).unwrap_or_default();
                        packset_daemon::embed::encode_document(text).unwrap_or_default()
                    })
                    .collect();
                if let Some(path) = atom_file.as_deref() {
                    write_rows(path, &fresh);
                }
                fresh
            });
            for (atom, vector) in conversation.atoms.iter_mut().zip(vectors) {
                if vector.is_empty() {
                    continue;
                }
                atom.insert(
                    "embedding".into(),
                    Value::Array(vector.into_iter().map(|f| json!(f)).collect()),
                );
            }

            let cached = question_file
                .as_deref()
                .and_then(|path| read_rows(path, conversation.questions.len()));
            let asked = cached.unwrap_or_else(|| {
                let fresh: Vec<Vec<f32>> = conversation
                    .questions
                    .iter()
                    .map(|question| {
                        packset_daemon::embed::encode_query(&question.text).unwrap_or_default()
                    })
                    .collect();
                if let Some(path) = question_file.as_deref() {
                    write_rows(path, &fresh);
                }
                fresh
            });
            for (question, vector) in conversation.questions.iter().zip(asked) {
                if vector.is_empty() {
                    continue;
                }
                questions.insert(question.text.clone(), vector);
            }
        }

        // Per-token vectors live beside the atoms rather than inside them: a
        // thousand floats per token would not fit an atom's JSON without
        // making every other read pay for it.
        let mut late_atoms: Vec<Vec<Vec<f32>>> = Vec::new();
        let mut m3_atoms: Vec<Vec<f32>> = Vec::new();
        let mut sparse_atoms: Vec<packset_daemon::embed::Sparse> = Vec::new();
        if late {
            // Cached like the pooled vectors; three files from one pass.
            let held = cache_dir();
            let (tokens_file, pooled_file, sparse_file) = match held.as_ref() {
                Some(dir) => (
                    Some(dir.join(format!("m3-tokens-{nth}.vec"))),
                    Some(dir.join(format!("m3-pooled-{nth}.vec"))),
                    Some(dir.join(format!("m3-sparse-{nth}.vec"))),
                ),
                None => (None, None, None),
            };
            let count = conversation.atoms.len();
            let cached = tokens_file
                .as_deref()
                .and_then(|path| read_groups(path, count))
                .zip(pooled_file.as_deref().and_then(|p| read_rows(p, count)))
                .zip(sparse_file.as_deref().and_then(|p| read_rows(p, count)));
            match cached {
                Some(((tokens, pooled), weights)) => {
                    late_atoms = tokens;
                    m3_atoms = pooled;
                    // Sparse round-trips as pairs flattened into one row, so
                    // it rides the same format the others use.
                    sparse_atoms = weights.into_iter().map(unflatten_sparse).collect();
                }
                None => {
                    for atom in &conversation.atoms {
                        let text = atom.get("text").and_then(Value::as_str).unwrap_or_default();
                        let (tokens, pooled, weights) =
                            packset_daemon::embed::encode_late(text).unwrap_or_default();
                        late_atoms.push(tokens);
                        m3_atoms.push(pooled);
                        sparse_atoms.push(weights);
                    }
                    if let Some(path) = tokens_file.as_deref() {
                        write_groups(path, &late_atoms);
                    }
                    if let Some(path) = pooled_file.as_deref() {
                        write_rows(path, &m3_atoms);
                    }
                    if let Some(path) = sparse_file.as_deref() {
                        let flat: Vec<Vec<f32>> = sparse_atoms.iter().map(flatten_sparse).collect();
                        write_rows(path, &flat);
                    }
                }
            }
            for question in &conversation.questions {
                if late_questions.contains_key(question.text.as_str()) {
                    continue;
                }
                if let Some((tokens, pooled, weights)) =
                    packset_daemon::embed::encode_late(&question.text)
                {
                    late_questions.insert(question.text.clone(), tokens);
                    m3_questions.insert(question.text.clone(), pooled);
                    sparse_questions.insert(question.text.clone(), weights);
                }
            }
        }

        for question in &conversation.questions {
            if question.category == ADVERSARIAL || question.evidence.is_empty() {
                adversarial += 1;
                continue;
            }
            let ask = Ask {
                user: "",
                memory: "",
                atoms: &conversation.atoms,
                query: &question.text,
                // Ranked deeper than the deepest cut-off, so a cut-off is a cut
                // rather than the whole list.
                limit: CUTOFFS[CUTOFFS.len() - 1],
                set: None,
                kind: None,
                now: &now,
            };
            // The depth every arm that collapses a ranking into sessions is
            // given, so what the protocol table compares is protocols.
            let deep_ask = Ask {
                limit: ask.limit * WINDOW,
                ..ask
            };
            let lexical = search::search_linear(&ask);
            let terms = search::search_bm25_plain(&ask, &index);
            let fed = search::search_bm25_expanded(&ask, &index, &documents);
            let meaning = questions
                .get(question.text.as_str())
                .map(|vector| search::search_dense(&ask, vector))
                .unwrap_or_default();
            let interaction = late_questions
                .get(question.text.as_str())
                .map(|tokens| rank_late(&ask, tokens, &late_atoms))
                .unwrap_or_default();
            let m3_pooled = m3_questions
                .get(question.text.as_str())
                .map(|vector| rank_pooled(&ask, vector, &m3_atoms))
                .unwrap_or_default();
            let m3_sparse = sparse_questions
                .get(question.text.as_str())
                .map(|weights| rank_sparse(&ask, weights, &sparse_atoms))
                .unwrap_or_default();

            // Same budget: half the places to the ranking, the rest to either
            // the ranking continuing or the neighbours.

            let deep = hit_ids(&search::merge_ballots(
                &[lexical.clone(), terms.clone()],
                HOP_CUT,
                &shipped,
                &now,
            ));
            ranking.add(&deep, &question.evidence);
            let shallow: Vec<String> = deep.iter().take(HOP_CUT / 2).cloned().collect();
            hopped.add(
                &one_hop(&shallow, &conversation.atoms, HOP_CUT),
                &question.evidence,
            );
            if let Some(graph) = graph.as_ref() {
                walked.add(&graph.expand(&shallow, HOP_CUT), &question.evidence);
            }

            for (slot, arm) in ARMS.iter().enumerate() {
                let ranked = match *arm {
                    "lexical" => hit_ids(&lexical),
                    "bm25" => hit_ids(&terms),
                    "bm25 rm3" => hit_ids(&fed),
                    "dense" => hit_ids(&meaning),
                    "late" => hit_ids(&interaction),
                    "m3 pooled" => hit_ids(&m3_pooled),
                    "m3 sparse" => hit_ids(&m3_sparse),
                    other => {
                        let ballots = match other {
                            "bm25+late" => vec![terms.clone(), interaction.clone()],
                            "bm25+m3 pooled" => vec![terms.clone(), m3_pooled.clone()],
                            "bm25+m3 sparse" => vec![terms.clone(), m3_sparse.clone()],
                            "m3 sparse+late" => vec![m3_sparse.clone(), interaction.clone()],
                            "bm25+dense" => vec![terms.clone(), meaning.clone()],
                            "bm25 rm3+dense" => vec![fed.clone(), meaning.clone()],
                            "lexical+bm25+dense" => {
                                vec![lexical.clone(), terms.clone(), meaning.clone()]
                            }
                            _ => vec![lexical.clone(), terms.clone()],
                        };
                        hit_ids(&search::merge_ballots(&ballots, ask.limit, &shipped, &now))
                    }
                };
                totals[slot].add(&ranked, &question.evidence);
                let rooms: BTreeSet<String> = question
                    .evidence
                    .iter()
                    .map(|id| session_of(id).to_string())
                    .collect();
                by_session[slot].add(&sessions_of(&ranked), &rooms);
            }

            // The strongest pair this run has, fused every way the panel
            // knows. Same ballots, same questions, one difference.
            let pair = if interaction.is_empty() {
                if meaning.is_empty() {
                    vec![lexical.clone(), terms.clone()]
                } else {
                    vec![terms.clone(), meaning.clone()]
                }
            } else {
                vec![terms.clone(), interaction.clone()]
            };
            let rooms: BTreeSet<String> = question
                .evidence
                .iter()
                .map(|id| session_of(id).to_string())
                .collect();
            for (slot, panel) in panels.iter().enumerate() {
                let ranked = hit_ids(&search::merge_ballots(&pair, ask.limit, panel, &now));
                voted[slot].add(&ranked, &question.evidence);
                voted_sessions[slot].add(&sessions_of(&ranked), &rooms);
            }
            let shipped_pair = if meaning.is_empty() {
                vec![lexical.clone(), terms.clone()]
            } else {
                vec![lexical.clone(), terms.clone(), meaning.clone()]
            };
            for (slot, panel) in shipped_panels.iter().enumerate() {
                let ranked = hit_ids(&search::merge_ballots(
                    &shipped_pair,
                    ask.limit,
                    panel,
                    &now,
                ));
                shipped_voted[slot].add(&ranked, &question.evidence);
                shipped_sessions[slot].add(&sessions_of(&ranked), &rooms);
            }

            // The same question against a corpus of sessions, so what varies
            // between these arms is what a document is.
            let asking = Ask {
                atoms: &session_corpus,
                ..ask
            };
            let room_terms = search::search_bm25_plain(&asking, &room_index);
            let room_fed = search::search_bm25_expanded(&asking, &room_index, &room_documents);
            // A window ranking is longer than a session ranking, because one
            // session contributes several windows and only its best survives
            // the collapse.
            let passage_ask = Ask {
                atoms: &passage_corpus,
                ..deep_ask
            };
            let passage_by = |scorer| {
                let mut hits = collapse_windows(&search::search_lexical(
                    &passage_ask,
                    &passage_index,
                    scorer,
                ));
                hits.truncate(ask.limit);
                hits
            };
            let passage_hits = passage_by(Scorer::Bm25);
            // The dense scorer over the same windows, read the same way.
            let passage_dense = {
                let mut hits = questions
                    .get(question.text.as_str())
                    .map(|vector| collapse_windows(&search::search_dense(&passage_ask, vector)))
                    .unwrap_or_default();
                hits.truncate(ask.limit);
                hits
            };
            // And the learned-sparse ballot that is actually a sparse model.
            let by_splade = collapse(
                &splade_questions
                    .get(question.text.as_str())
                    .map(|weights| rank_sparse(&deep_ask, weights, &splade_atoms))
                    .unwrap_or_default(),
            );
            // The two the literature says are better than the one above, on
            // the arm where the defect they fix is the arm's own shape.
            let passage_floored = passage_by(Scorer::Bm25Plus);
            let passage_likely = passage_by(Scorer::Dirichlet);
            // Read as sessions from a ranking deep enough that the collapse
            // still fills the deepest cut-off; every collapsing arm gets the
            // same budget.
            let by_turn = collapse(&search::search_bm25_plain(&deep_ask, &index));
            // The same protocol with the floor under an occurrence, so the
            // scorer is compared at every granularity rather than only where
            // it was expected to help.
            let by_turn_floored =
                collapse(&search::search_lexical(&deep_ask, &index, Scorer::Bm25Plus));
            let room_floored = search::search_lexical(&asking, &room_index, Scorer::Bm25Plus);
            let by_meaning = collapse(
                &questions
                    .get(question.text.as_str())
                    .map(|vector| search::search_dense(&deep_ask, vector))
                    .unwrap_or_default(),
            );
            let by_late = collapse(
                &late_questions
                    .get(question.text.as_str())
                    .map(|tokens| rank_late(&deep_ask, tokens, &late_atoms))
                    .unwrap_or_default(),
            );
            let by_sparse = collapse(
                &sparse_questions
                    .get(question.text.as_str())
                    .map(|weights| rank_sparse(&deep_ask, weights, &sparse_atoms))
                    .unwrap_or_default(),
            );
            for (slot, arm) in PROTOCOLS.iter().enumerate() {
                let ranked = match *arm {
                    "turn bm25" => hit_ids(&by_turn),
                    "turn bm25+" => hit_ids(&by_turn_floored),
                    "session bm25" => hit_ids(&room_terms),
                    "session bm25+" => hit_ids(&room_floored),
                    "session bm25 rm3" => hit_ids(&room_fed),
                    "passage bm25" => hit_ids(&passage_hits),
                    "passage bm25+" => hit_ids(&passage_floored),
                    "passage dirichlet" => hit_ids(&passage_likely),
                    // Two lexical ballots, which the panel has never had: one
                    // formula is not a lexical opinion.
                    "passage bm25+ & dirichlet" => hit_ids(&search::merge_ballots(
                        &[passage_floored.clone(), passage_likely.clone()],
                        ask.limit,
                        &shipped,
                        &now,
                    )),
                    "passage bm25+ + turn dense" => hit_ids(&search::merge_ballots(
                        &[passage_floored.clone(), by_meaning.clone()],
                        ask.limit,
                        &shipped,
                        &now,
                    )),
                    "passage dense" => hit_ids(&passage_dense),
                    "passage bm25+ + passage dense" => hit_ids(&search::merge_ballots(
                        &[passage_floored.clone(), passage_dense.clone()],
                        ask.limit,
                        &shipped,
                        &now,
                    )),
                    "turn splade" => {
                        if !splade {
                            continue;
                        }
                        hit_ids(&by_splade)
                    }
                    "passage bm25+ + turn splade" => {
                        if !splade {
                            continue;
                        }
                        hit_ids(&search::merge_ballots(
                            &[passage_floored.clone(), by_splade.clone()],
                            ask.limit,
                            &shipped,
                            &now,
                        ))
                    }
                    "passage bm25+ + turn dense, reranked" => {
                        if !reranking {
                            continue;
                        }
                        let first = search::merge_ballots(
                            &[passage_floored.clone(), by_meaning.clone()],
                            ask.limit,
                            &shipped,
                            &now,
                        );
                        hit_ids(&reranked(&question.text, &first, &mut rerank_spent))
                    }
                    "turn dense" => hit_ids(&by_meaning),
                    "passage bm25 + turn dense" => hit_ids(&search::merge_ballots(
                        &[passage_hits.clone(), by_meaning.clone()],
                        ask.limit,
                        &shipped,
                        &now,
                    )),
                    "session bm25 + turn late" => hit_ids(&search::merge_ballots(
                        &[room_terms.clone(), by_late.clone()],
                        ask.limit,
                        &shipped,
                        &now,
                    )),
                    "session bm25 + turn m3 sparse" => hit_ids(&search::merge_ballots(
                        &[room_terms.clone(), by_sparse.clone()],
                        ask.limit,
                        &shipped,
                        &now,
                    )),
                    _ => hit_ids(&search::merge_ballots(
                        &[room_terms.clone(), by_meaning.clone()],
                        ask.limit,
                        &shipped,
                        &now,
                    )),
                };
                protocol[slot].add(&ranked, &rooms);
            }
            // The fusion question on the strongest pair, with the passage
            // ranking as the lexical half.
            let room_pair = if by_late.is_empty() {
                vec![passage_floored.clone(), by_meaning.clone()]
            } else {
                vec![passage_floored.clone(), by_late.clone()]
            };
            for (slot, panel) in panels.iter().enumerate() {
                let ranked = hit_ids(&search::merge_ballots(&room_pair, ask.limit, panel, &now));
                room_voted[slot].add(&ranked, &rooms);
            }
            // And the slot beside the fuse, on the same pair, fused the way
            // the table above says is strongest.
            for (slot, panel) in diversifiers.iter().enumerate() {
                let ranked = hit_ids(&search::merge_ballots(&room_pair, ask.limit, panel, &now));
                diversified[slot].add(&ranked, &rooms);
            }
        }
    }

    let asked = totals[0].asked;
    println!(
        "{} conversations, {turns} turns, {asked} answerable questions \
         ({adversarial} adversarial or unlabelled, excluded)",
        corpus.len()
    );
    println!(
        "link graph: {linked} edges, {:.1} per turn, widest {widest}",
        linked as f64 / turns.max(1) as f64
    );
    if reranking {
        println!(
            "second stage: {:.1}s wall for {asked} questions, depth {RERANK_DEPTH}, a forward pass a candidate",
            rerank_spent.as_secs_f64()
        );
    }
    println!();
    table(ARMS, &totals);

    println!();
    println!("the same rankings read as sessions, which is the unit the");
    println!("retrieval papers on this benchmark score.");
    println!();
    println!(
        "read off the arms above, so off a ranking cut at {} turns: the top",
        CUTOFFS[CUTOFFS.len() - 1]
    );
    println!("turns cluster in a few rooms, so these arms answer with fewer");
    println!("sessions than they were asked for. Not comparable with the protocol");
    println!("table below, which gives every collapsing arm the depth to fill:");
    println!();
    table(ARMS, &by_session);

    let swept = if late {
        "bm25+late"
    } else if encoder {
        "bm25+dense"
    } else {
        "lexical+bm25"
    };
    println!();
    println!("{swept}, fused every way the panel knows, by turn:");
    println!();
    table(VOTERS, &voted);
    println!();
    println!("the same, by session:");
    println!();
    table(VOTERS, &voted_sessions);

    println!();
    println!(
        "the ballots the seat ships ({}), fused every way, by turn:",
        if encoder {
            "lexical + bm25 + dense"
        } else {
            "lexical + bm25"
        }
    );
    println!();
    table(SHIPPED_VOTERS, &shipped_voted);
    println!();
    println!("the same, by session:");
    println!();
    table(SHIPPED_VOTERS, &shipped_sessions);

    println!();
    println!("a session as the document, against a session read off a turn");
    println!("ranking, which are two protocols behind one word:");
    println!();
    table(PROTOCOLS, &protocol);
    println!();
    println!(
        "{}, fused every way the panel knows:",
        if late {
            "passage bm25+ + turn late"
        } else {
            "passage bm25+ + turn dense"
        }
    );
    println!();
    table(VOTERS, &room_voted);

    println!();
    println!("the same pair fused by combsum, diversified three ways. This slot");
    println!("reorders every answer and had never been measured. It changes");
    println!("nothing here, and this benchmark cannot see what it is for:");
    println!();
    table(DIVERSIFIERS, &diversified);

    let slot = CUTOFFS.iter().position(|cut| *cut == HOP_CUT).expect("cut");
    let counted = ranking.asked.max(1) as f64;
    println!();
    println!(
        "the last {} places of {HOP_CUT}, given to the ranking or to the graph:",
        HOP_CUT / 2
    );
    println!(
        "  ranking continues      R@{HOP_CUT} {:.3}   hit@{HOP_CUT} {:.3}",
        ranking.recall[slot] / counted,
        ranking.hit[slot] as f64 / counted
    );
    println!(
        "  neighbours of the top  R@{HOP_CUT} {:.3}   hit@{HOP_CUT} {:.3}",
        hopped.recall[slot] / counted,
        hopped.hit[slot] as f64 / counted
    );
    if walking {
        println!(
            "  a restart walk from it R@{HOP_CUT} {:.3}   hit@{HOP_CUT} {:.3}",
            walked.recall[slot] / counted,
            walked.hit[slot] as f64 / counted
        );
    }

    let mut merged = Tally::new();
    merged.merge(&totals[0]);
    anyhow::ensure!(
        merged.asked == asked,
        "the arms answered different questions"
    );
    Ok(())
}
