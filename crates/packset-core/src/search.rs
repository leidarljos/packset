//! Scoring over a pack: the prefix-and-edit scan, BM25 over the index, the
//! learned ballots, and the fuse that merges them.

use serde_json::{json, Map, Value};

use crate::{clock, record};

/// A query token shorter than this matches exactly or not at all.
pub const SHORT_EXACT: usize = 4;
/// Days over which a hit's recency weight halves.
pub const RECENCY_HALF_LIFE_DAYS: f64 = 14.0;

/// Words carrying no signal, dropped from a query and from the text.
pub const STOP: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "been", "being", "but", "by", "can", "could", "do",
    "does", "for", "from", "if", "in", "is", "it", "its", "me", "my", "no", "not", "of", "on",
    "or", "our", "please", "should", "than", "that", "the", "then", "these", "this", "those", "to",
    "was", "we", "were", "what", "when", "where", "which", "who", "will", "with", "would", "yes",
    "you", "your",
];

/// One atom.
pub type Record = Map<String, Value>;

/// Lowercase tokens with the stopwords dropped.
#[must_use]
/// Whether the lexical path folds a word to its stem. On by default;
/// `PACKSET_STEM=off` turns it off. Measured in the README's retrieval table.
fn stemming() -> bool {
    !matches!(
        std::env::var("PACKSET_STEM")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "off" | "0" | "no" | "false"
    )
}

/// The stemmer, built once. Snowball's English, which is Porter's suffix
/// stripping as its author later revised it (doi:10.1108/eb046814).
fn stemmer() -> &'static rust_stemmers::Stemmer {
    static ENGLISH: std::sync::OnceLock<rust_stemmers::Stemmer> = std::sync::OnceLock::new();
    ENGLISH.get_or_init(|| rust_stemmers::Stemmer::create(rust_stemmers::Algorithm::English))
}

/// Fold one token to its stem. Applied to both index and query, or neither.
#[must_use]
pub fn fold(token: &str) -> String {
    if !stemming() {
        return token.to_string();
    }
    // Digits or a hyphen mark an identifier, version or accession: not stemmed.
    if token
        .bytes()
        .any(|b| b.is_ascii_digit() || b == b'-' || b == b'_')
    {
        return token.to_string();
    }
    stemmer().stem(token).into_owned()
}

pub fn tokens(text: &str) -> Vec<String> {
    let lower = text.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i].is_ascii_alphanumeric() {
            let start = i;
            i += 1;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'-')
            {
                i += 1;
            }
            let token = &lower[start..i];
            // Stopwords are dropped before folding, because the list is of
            // words as written and a stemmer would not leave them matching it.
            if !STOP.contains(&token) {
                out.push(fold(token));
            }
        } else {
            i += 1;
        }
    }
    out
}

/// Paragraphs, or lines when the text is one block.
///
/// A card written as a list of one-line claims would otherwise score as a
/// single paragraph and return the whole file as one hit.
#[must_use]
pub fn paragraphs(text: &str) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            if !current.is_empty() {
                let joined = current.join("\n").trim().to_string();
                if !joined.is_empty() {
                    parts.push(joined);
                }
                current.clear();
            }
        } else {
            current.push(line);
        }
    }
    if !current.is_empty() {
        let joined = current.join("\n").trim().to_string();
        if !joined.is_empty() {
            parts.push(joined);
        }
    }
    if parts.len() <= 1 {
        parts = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
    }
    parts
}

/// Edit distance, saturating at two.
///
/// Two is as far as the caller ever looks, so counting past it would be work
/// nobody reads.
#[must_use]
pub fn edits(a: &str, b: &str) -> usize {
    if a == b {
        return 0;
    }
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len().abs_diff(b.len()) > 1 {
        return 2;
    }
    let (short, long) = if a.len() > b.len() { (b, a) } else { (a, b) };
    if short.len() == long.len() {
        return short.iter().zip(long).filter(|(x, y)| x != y).count();
    }
    let (mut i, mut j, mut diffs) = (0usize, 0usize, 0usize);
    while i < short.len() && j < long.len() {
        if short[i] == long[j] {
            i += 1;
            j += 1;
            continue;
        }
        diffs += 1;
        j += 1;
        if diffs > 1 {
            return diffs;
        }
    }
    diffs + (long.len() - j)
}

/// How well one query token answers to one text token.
#[must_use]
pub fn token_score(query: &str, hay: &str) -> f64 {
    if query.is_empty() || hay.is_empty() {
        return 0.0;
    }
    if hay == query {
        return 4.0;
    }
    // "pr" is not a prefix of "prefers": a short query would match half the
    // pack on prefix alone, so it has to be exact.
    if query.len() < SHORT_EXACT {
        return 0.0;
    }
    if hay.starts_with(query) {
        return 3.0;
    }
    if hay.contains(query) {
        return 2.0;
    }
    if edits(query, hay) <= 1 {
        return 1.5;
    }
    0.0
}

/// Half-life decay on a timestamp. A missing one counts as current.
#[must_use]
pub fn recency(ts: Option<&str>, now: &str) -> f64 {
    let Some(ts) = ts.filter(|t| !t.is_empty()) else {
        return 1.0;
    };
    if clock::parse_millis(ts).is_none() {
        return 1.0;
    }
    let days = clock::elapsed_days(ts, now);
    0.5f64.powf(days / RECENCY_HALF_LIFE_DAYS)
}

/// The best each query token can do against the text, summed.
#[must_use]
pub fn text_score(query_tokens: &[String], text: &str) -> f64 {
    tokens_score(query_tokens, &tokens(text))
}

/// [`text_score`] over a text already tokenised: the best each query token
/// does against any token of the text, summed.
#[must_use]
pub fn tokens_score(query_tokens: &[String], hay: &[String]) -> f64 {
    if hay.is_empty() {
        return 0.0;
    }
    query_tokens
        .iter()
        .map(|q| hay.iter().map(|h| token_score(q, h)).fold(0.0f64, f64::max))
        .sum()
}

fn trust_of(atom: &Record) -> f64 {
    match atom.get("trust") {
        None | Some(Value::Null) => 1.0,
        Some(other) => other.as_f64().unwrap_or(1.0),
    }
}

fn atom_in_set(atom: &Record, set: Option<&str>) -> bool {
    match set {
        None => true,
        Some(name) => atom.get("set").and_then(Value::as_str) == Some(name),
    }
}

fn file_hits(field: &str, text: &str, qtoks: &[String], bias: f64) -> Vec<Value> {
    paragraphs(text)
        .into_iter()
        .filter_map(|para| {
            let score = text_score(qtoks, &para);
            (score != 0.0).then(|| {
                json!({
                    "field": field,
                    "id": Value::Null,
                    "kind": field,
                    "text": para,
                    "score": score + bias,
                })
            })
        })
        .collect()
}

/// The `k` best candidates as (score, ordinal); a hit is built for the
/// survivors only. Order matches `sort_hits`.
struct TopK<'a> {
    k: usize,
    // A min-heap on (score, id) through `Reverse`, so the root is the weakest
    // survivor and is what a stronger candidate displaces.
    heap: std::collections::BinaryHeap<std::cmp::Reverse<Candidate<'a>>>,
}

/// One scored atom, ordered the way the final list is.
#[derive(Debug, Clone, Copy)]
struct Candidate<'a> {
    score: f64,
    id: &'a str,
    ordinal: usize,
}

impl PartialEq for Candidate<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}
impl Eq for Candidate<'_> {}
impl PartialOrd for Candidate<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Candidate<'_> {
    /// Greater is better: higher score, then the id that sorts first.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.score
            .partial_cmp(&other.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| other.id.cmp(self.id))
            .then_with(|| other.ordinal.cmp(&self.ordinal))
    }
}

impl<'a> TopK<'a> {
    fn new(k: usize) -> Self {
        Self {
            k,
            heap: std::collections::BinaryHeap::with_capacity(k + 1),
        }
    }

    fn offer(&mut self, candidate: Candidate<'a>) {
        if self.k == 0 {
            return;
        }
        if self.heap.len() < self.k {
            self.heap.push(std::cmp::Reverse(candidate));
            return;
        }
        if let Some(std::cmp::Reverse(weakest)) = self.heap.peek() {
            if candidate > *weakest {
                self.heap.pop();
                self.heap.push(std::cmp::Reverse(candidate));
            }
        }
    }

    /// Best first.
    fn into_sorted(self) -> Vec<Candidate<'a>> {
        let mut out: Vec<Candidate<'a>> = self.heap.into_iter().map(|r| r.0).collect();
        out.sort_by(|a, b| b.cmp(a));
        out
    }
}

/// The hit a caller reads.
fn atom_hit(atom: &Record, score: f64) -> Value {
    json!({
        "field": "atom",
        "id": atom.get("id").cloned().unwrap_or(Value::Null),
        "kind": atom.get("kind").cloned().unwrap_or(Value::Null),
        "text": atom.get("text").cloned().unwrap_or(Value::Null),
        "ts": atom.get("ts").cloned().unwrap_or(Value::Null),
        "due_at": atom.get("due_at").cloned().unwrap_or(Value::Null),
        "entities": atom.get("entities").cloned().unwrap_or_else(|| Value::Array(Vec::new())),
        "score": score,
    })
}

/// The id an atom sorts by on a tie.
fn id_of(atom: &Record) -> &str {
    atom.get("id").and_then(Value::as_str).unwrap_or("")
}

/// Score descending, then field, then id: a total order.
fn sort_hits(hits: &mut [Value]) {
    hits.sort_by(|a, b| {
        let sa = a["score"].as_f64().unwrap_or(0.0);
        let sb = b["score"].as_f64().unwrap_or(0.0);
        sb.partial_cmp(&sa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                a["field"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(b["field"].as_str().unwrap_or(""))
            })
            .then_with(|| {
                a["id"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(b["id"].as_str().unwrap_or(""))
            })
    });
}

/// What a search is asked, apart from how it is scored.
#[derive(Debug, Clone, Copy)]
pub struct Ask<'a> {
    /// The seat card.
    pub user: &'a str,
    /// The workspace card.
    pub memory: &'a str,
    /// The live atoms to score.
    pub atoms: &'a [Record],
    /// What was asked.
    pub query: &'a str,
    /// How many hits to return.
    pub limit: usize,
    /// The named set to stay inside, if any.
    pub set: Option<&'a str>,
    /// The instant liveness and recency are measured against.
    pub now: &'a str,
}

/// The prefix-and-one-edit scan over a whole pack.
#[must_use]
pub fn search_linear(ask: &Ask<'_>) -> Vec<Value> {
    let documents: Vec<Vec<String>> = ask.atoms.iter().map(atom_tokens).collect();
    search_linear_with(ask, &documents)
}

/// [`search_linear`] over atoms the caller has already tokenised.
///
/// `documents[i]` is [`atom_tokens`] of `ask.atoms[i]`, the same list the
/// writer holds from building the index; an atom past the end of `documents`
/// is tokenised here. The scan then pays for the comparison alone.
#[must_use]
pub fn search_linear_with(ask: &Ask<'_>, documents: &[Vec<String>]) -> Vec<Value> {
    let Ask {
        user,
        memory,
        atoms,
        query,
        limit,
        set,
        now,
    } = *ask;
    let qtoks = tokens(query);
    if qtoks.is_empty() {
        return Vec::new();
    }
    // The seat card outranks the workspace card at equal relevance, because a
    // standing preference applies wherever the question came from.
    let mut hits = file_hits("user", user, &qtoks, 0.5);
    hits.extend(file_hits("memory", memory, &qtoks, 0.25));

    let mut best = TopK::new(limit);
    let mut owned: Vec<String>;
    for (ordinal, atom) in atoms.iter().enumerate() {
        if !record::is_live_at(atom, now) || !atom_in_set(atom, set) {
            continue;
        }
        let hay: &[String] = match documents.get(ordinal) {
            Some(tokens) => tokens,
            None => {
                owned = atom_tokens(atom);
                &owned
            }
        };
        let relevance = tokens_score(&qtoks, hay);
        let due = record::is_due(atom, now);
        if relevance == 0.0 {
            continue;
        }
        let ts = atom.get("ts").and_then(Value::as_str);
        best.offer(Candidate {
            score: relevance
                + 0.1 * trust_of(atom)
                + recency(ts, now)
                + if due { 2.0 } else { 0.0 },
            id: id_of(atom),
            ordinal,
        });
    }
    for candidate in best.into_sorted() {
        hits.push(atom_hit(&atoms[candidate.ordinal], candidate.score));
    }
    sort_hits(&mut hits);
    hits.truncate(limit);
    hits
}

/// The tokens one atom is searchable by: its text and its entities.
///
/// One definition, because the corpus that counts terms and the document that
/// is scored against those counts have to agree about what a document is.
#[must_use]
pub fn atom_tokens(atom: &Record) -> Vec<String> {
    let text = atom.get("text").and_then(Value::as_str).unwrap_or("");
    let mut out = tokens(text);
    if let Some(Value::Array(items)) = atom.get("entities") {
        for item in items {
            let name = item
                .as_str()
                .map_or_else(|| item.to_string(), str::to_string);
            out.extend(tokens(&name));
        }
    }
    out
}

/// The pack scored by BM25 over `index`, a second ballot beside the scan.
/// `index` must have been built over `ask.atoms` in that order. Cards are
/// scored against the same corpus.
#[must_use]
pub fn search_bm25(ask: &Ask<'_>, index: &crate::bm25::Index) -> Vec<Value> {
    search_lexical(ask, index, crate::bm25::Scorer::default())
}

/// Plain Okapi BM25, for a caller measuring against the formula.
#[must_use]
pub fn search_bm25_plain(ask: &Ask<'_>, index: &crate::bm25::Index) -> Vec<Value> {
    search_lexical(ask, index, crate::bm25::Scorer::Bm25)
}

/// The same, in the scoring family the caller names.
#[must_use]
pub fn search_lexical(
    ask: &Ask<'_>,
    index: &crate::bm25::Index,
    scorer: crate::bm25::Scorer,
) -> Vec<Value> {
    let qtoks = tokens(ask.query);
    // Each word asked for once, which is the unweighted query written as a
    // weighted one so both paths score through the same code.
    let weighted: Vec<(String, f64)> = qtoks.into_iter().map(|term| (term, 1.0)).collect();
    bm25_hits(ask, index, &weighted, scorer)
}

/// How many of the first pass's hits the relevance model is estimated from.
const RM3_DOCS: usize = 10;

/// How many terms the model contributes.
const RM3_TERMS: usize = 10;

/// How much of the expanded query stays the words asked for.
const RM3_ALPHA: f64 = 0.5;

/// BM25 with the query expanded from its own first pass; see
/// [`crate::bm25::Index::expand`]. `documents` is the tokenised corpus the
/// index was built over, in that order.
#[must_use]
pub fn search_bm25_expanded(
    ask: &Ask<'_>,
    index: &crate::bm25::Index,
    documents: &[Vec<String>],
) -> Vec<Value> {
    let qtoks = tokens(ask.query);
    if qtoks.is_empty() || index.is_empty() {
        return Vec::new();
    }
    let mut first = index.score(&qtoks);
    first.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    first.truncate(RM3_DOCS);
    let feedback: Vec<(&[String], f64)> = first
        .iter()
        .filter_map(|(ordinal, score)| {
            documents
                .get(*ordinal)
                .map(|tokens| (tokens.as_slice(), *score))
        })
        .collect();
    let expanded = index.expand(&qtoks, &feedback, RM3_TERMS, RM3_ALPHA);
    bm25_hits(ask, index, &expanded, crate::bm25::Scorer::default())
}

/// The hits a weighted query scores, cards and atoms together.
fn bm25_hits(
    ask: &Ask<'_>,
    index: &crate::bm25::Index,
    query: &[(String, f64)],
    scorer: crate::bm25::Scorer,
) -> Vec<Value> {
    let Ask {
        user,
        memory,
        atoms,
        query: _,
        limit,
        set,
        now,
    } = *ask;
    if query.is_empty() || index.is_empty() {
        return Vec::new();
    }

    let mut hits: Vec<Value> = Vec::new();
    for (field, text, bias) in [("user", user, 0.5), ("memory", memory, 0.25)] {
        for para in paragraphs(text) {
            let relevance = index.score_foreign_weighted_by(scorer, query, &tokens(&para));
            if relevance == 0.0 {
                continue;
            }
            hits.push(json!({
                "field": field,
                "id": Value::Null,
                "kind": field,
                "text": para,
                "score": relevance + bias,
            }));
        }
    }

    // Only the best `limit` atoms are built; nothing below them can reach the
    // final cut past the cards.
    let mut best = TopK::new(limit);
    for (ordinal, relevance) in index.score_weighted_by(scorer, query) {
        let Some(atom) = atoms.get(ordinal) else {
            continue;
        };
        if !record::is_live_at(atom, now) || !atom_in_set(atom, set) {
            continue;
        }
        let ts = atom.get("ts").and_then(Value::as_str);
        best.offer(Candidate {
            score: relevance + 0.1 * trust_of(atom) + recency(ts, now),
            id: id_of(atom),
            ordinal,
        });
    }
    for candidate in best.into_sorted() {
        hits.push(atom_hit(&atoms[candidate.ordinal], candidate.score));
    }

    sort_hits(&mut hits);
    hits.truncate(limit);
    hits
}

/// Dot product of two learned sparse vectors, both ascending by index.
#[must_use]
pub fn sparse_dot(left: &[(u32, f32)], right: &[(u32, f32)]) -> f64 {
    let (mut here, mut there) = (0usize, 0usize);
    let mut total = 0.0f64;
    while here < left.len() && there < right.len() {
        match left[here].0.cmp(&right[there].0) {
            std::cmp::Ordering::Less => here += 1,
            std::cmp::Ordering::Greater => there += 1,
            std::cmp::Ordering::Equal => {
                total += f64::from(left[here].1) * f64::from(right[there].1);
                here += 1;
                there += 1;
            }
        }
    }
    total
}

/// Cosine between two vectors, zero when either is empty. Normalised here:
/// a stored vector is not assumed unit length.
#[must_use]
pub fn cosine(left: &[f32], right: &[f32]) -> f64 {
    if left.len() != right.len() || left.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f64;
    let mut left_norm = 0.0f64;
    let mut right_norm = 0.0f64;
    for (a, b) in left.iter().zip(right) {
        dot += f64::from(*a) * f64::from(*b);
        left_norm += f64::from(*a) * f64::from(*a);
        right_norm += f64::from(*b) * f64::from(*b);
    }
    if left_norm <= 0.0 || right_norm <= 0.0 {
        return 0.0;
    }
    dot / (left_norm.sqrt() * right_norm.sqrt())
}

/// The vector an atom carries, if it carries one.
#[must_use]
pub fn embedding_of(atom: &Record) -> Option<Vec<f32>> {
    let items = atom.get("embedding")?.as_array()?;
    let vector: Vec<f32> = items
        .iter()
        .filter_map(|v| v.as_f64().map(|f| f as f32))
        .collect();
    (vector.len() == items.len() && !vector.is_empty()).then_some(vector)
}

/// Late interaction (MaxSim): for each query token the best document token,
/// summed. Zero when either side has no tokens.
#[must_use]
pub fn max_sim(query: &[Vec<f32>], document: &[Vec<f32>]) -> f64 {
    if query.is_empty() || document.is_empty() {
        return 0.0;
    }
    query
        .iter()
        .map(|term| {
            document
                .iter()
                .map(|token| cosine(term, token))
                .fold(f64::MIN, f64::max)
        })
        .filter(|best| *best > f64::MIN)
        .sum()
}

/// The dense ballot. Atoms without a stored vector are skipped, not scored
/// zero: a missing encoder is not a vote.
#[must_use]
pub fn search_dense(ask: &Ask<'_>, query: &[f32]) -> Vec<Value> {
    let Ask {
        atoms,
        limit,
        set,
        now,
        ..
    } = *ask;
    if query.is_empty() {
        return Vec::new();
    }
    let mut best = TopK::new(limit);
    for (ordinal, atom) in atoms.iter().enumerate() {
        if !record::is_live_at(atom, now) || !atom_in_set(atom, set) {
            continue;
        }
        let Some(vector) = embedding_of(atom) else {
            continue;
        };
        let relevance = cosine(query, &vector);
        if relevance <= 0.0 {
            continue;
        }
        let ts = atom.get("ts").and_then(Value::as_str);
        best.offer(Candidate {
            score: relevance + 0.1 * trust_of(atom) + recency(ts, now),
            id: id_of(atom),
            ordinal,
        });
    }
    let mut hits: Vec<Value> = best
        .into_sorted()
        .into_iter()
        .map(|c| atom_hit(&atoms[c.ordinal], c.score))
        .collect();
    sort_hits(&mut hits);
    hits.truncate(limit);
    hits
}

/// Live atoms whose review is due; these lead the answer regardless of query.
#[must_use]
pub fn due_hits(atoms: &[Record], set: Option<&str>, now: &str) -> Vec<Value> {
    let mut hits: Vec<Value> = atoms
        .iter()
        .filter(|atom| {
            record::is_live(atom, now) && atom_in_set(atom, set) && record::is_due(atom, now)
        })
        .map(|atom| {
            let ts = atom.get("ts").and_then(Value::as_str);
            json!({
                "field": "atom",
                "id": atom.get("id").cloned().unwrap_or(Value::Null),
                "kind": atom.get("kind").cloned().unwrap_or(Value::Null),
                "text": atom.get("text").and_then(Value::as_str).unwrap_or(""),
                "ts": ts,
                "due_at": atom.get("due_at").cloned().unwrap_or(Value::Null),
                "score": 2.0 + 0.1 * trust_of(atom) + recency(ts, now),
            })
        })
        .collect();
    hits.sort_by(|a, b| {
        let sa = a["score"].as_f64().unwrap_or(0.0);
        let sb = b["score"].as_f64().unwrap_or(0.0);
        sb.partial_cmp(&sa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                a["id"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(b["id"].as_str().unwrap_or(""))
            })
    });
    hits
}

fn hit_key(hit: &Value) -> (String, String) {
    (
        hit["field"].as_str().unwrap_or("").to_string(),
        hit["id"].as_str().unwrap_or("").to_string(),
    )
}

/// Put the due hits first, then the ranked ones, dropping repeats.
///
/// Due leads because a review-clock hit is not a relevance claim the model
/// is allowed to bury. It does not lead all the way: a due set larger than
/// the window used to take the whole window, and with 63 atoms due against
/// the default limit of 10 every query returned the same ten rows at the
/// same score, the nonsense ones included. The ranked list was computed and
/// then thrown away.
///
/// So due takes at most half the window while there is anything ranked to
/// put in the other half, and it takes the rest only when relevance has
/// nothing left to offer.
#[must_use]
pub fn front_due(due: Vec<Value>, ranked: Vec<Value>, limit: usize) -> Vec<Value> {
    if limit == 0 {
        return Vec::new();
    }
    let due_room = if ranked.is_empty() {
        limit
    } else {
        limit.div_ceil(2)
    };
    let mut seen = std::collections::HashSet::new();
    let mut unique_due: Vec<Value> = Vec::new();
    for hit in due {
        if seen.insert(hit_key(&hit)) {
            unique_due.push(hit);
        }
    }
    let spare = unique_due.split_off(due_room.min(unique_due.len()));
    let mut out = unique_due;
    for hit in ranked {
        if out.len() >= limit {
            break;
        }
        if seen.insert(hit_key(&hit)) {
            out.push(hit);
        }
    }
    // Relevance had its half and did not fill it; the clock takes the rest.
    // These were deduped on the way in, so they need no second check.
    for hit in spare {
        if out.len() >= limit {
            break;
        }
        out.push(hit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scan over cached tokens is the scan, entities and a partial list
    /// included.
    #[test]
    fn the_scan_over_cached_tokens_is_the_scan() {
        let atoms: Vec<Record> = (0..60)
            .map(|n| {
                json!({
                    "id": format!("atom-{n:02}"),
                    "kind": "conclusion",
                    "text": if n % 3 == 0 { "prefer ripgrep for search" } else { "the lease token holder" },
                    "entities": if n % 4 == 0 { json!(["ripgrep", "Search-Tool"]) } else { json!([]) },
                    "ts": "2026-09-10T00:00:00Z",
                })
                .as_object()
                .expect("object")
                .clone()
            })
            .collect();
        let asked = Ask {
            user: "",
            memory: "",
            atoms: &atoms,
            query: "ripgrp search tool",
            limit: 15,
            set: None,
            now: "2026-09-10T00:00:00Z",
        };
        let fresh = search_linear(&asked);
        let documents: Vec<Vec<String>> = atoms.iter().map(atom_tokens).collect();
        assert_eq!(search_linear_with(&asked, &documents), fresh);
        // A partial token list falls back per atom rather than skipping them.
        assert_eq!(search_linear_with(&asked, &documents[..7]), fresh);
        assert!(!fresh.is_empty());
    }

    /// The bounded heap gives the list the full sort gives, ties included.
    #[test]
    fn the_bounded_heap_agrees_with_the_full_sort() {
        let atoms: Vec<Record> = (0..200)
            .map(|n| {
                json!({
                    "id": format!("atom-{:03}", (n * 37) % 200),
                    "kind": "conclusion",
                    "text": "lease token holder",
                })
                .as_object()
                .expect("object")
                .clone()
            })
            .collect();
        // Scores with plenty of ties, and a few negatives that must lose.
        let score_of = |n: usize| ((n % 7) as f64) - 1.0;
        for k in [0usize, 1, 5, 20, 199, 200, 500] {
            let mut best = TopK::new(k);
            for (ordinal, atom) in atoms.iter().enumerate() {
                best.offer(Candidate {
                    score: score_of(ordinal),
                    id: id_of(atom),
                    ordinal,
                });
            }
            let mut fast: Vec<Value> = best
                .into_sorted()
                .into_iter()
                .map(|c| atom_hit(&atoms[c.ordinal], c.score))
                .collect();
            sort_hits(&mut fast);

            let mut slow: Vec<Value> = atoms
                .iter()
                .enumerate()
                .map(|(ordinal, atom)| atom_hit(atom, score_of(ordinal)))
                .collect();
            sort_hits(&mut slow);
            slow.truncate(k);
            assert_eq!(fast, slow, "k = {k}");
        }
    }

    /// The point of late interaction: a question whose terms are answered in
    /// different parts of one document, which pooling averages away.
    #[test]
    fn late_interaction_sums_the_best_match_for_each_query_term() {
        let a = vec![1.0f32, 0.0, 0.0];
        let b = vec![0.0f32, 1.0, 0.0];
        let c = vec![0.0f32, 0.0, 1.0];
        // Both query terms are matched exactly, in different tokens.
        let scored = max_sim(&[a.clone(), b.clone()], &[c.clone(), a.clone(), b.clone()]);
        assert!((scored - 2.0).abs() < 1e-9, "{scored}");
        // One matched, one absent.
        let half = max_sim(&[a.clone(), b.clone()], &[a.clone(), c.clone()]);
        assert!(half < scored, "{half} vs {scored}");
        assert_eq!(max_sim(&[], std::slice::from_ref(&a)), 0.0);
        assert_eq!(max_sim(std::slice::from_ref(&a), &[]), 0.0);
    }

    /// A hit with a key and a score, for the fusion tests.
    fn scored(id: &str, score: f64) -> Value {
        json!({ "field": "atom", "id": id, "text": id, "score": score })
    }

    /// A question about a wedding finds a text that says weddings, which is
    /// the whole reason a lexical retriever stems.
    #[test]
    fn a_question_finds_the_other_form_of_the_word() {
        let asked = tokens("what did she say about the wedding");
        let said = tokens("we talked about weddings and rings");
        assert!(
            asked.iter().any(|t| said.contains(t)),
            "{asked:?} shares nothing with {said:?}"
        );
    }

    /// Both sides fold or neither.
    #[test]
    fn the_index_and_the_query_fold_the_same_way() {
        let index = atom_tokens(
            json!({ "text": "the parser reads manifests", "kind": "conclusion" })
                .as_object()
                .expect("object"),
        );
        for term in tokens("which manifest does the parser read") {
            if term == "manifest" || term == "parser" || term == "read" {
                assert!(index.contains(&term), "{term} is not in {index:?}");
            }
        }
    }

    /// A stemmer is for words. An accession, a version or an identifier is a
    /// name, and stripping a suffix off one merges two distinct things.
    #[test]
    fn a_name_is_not_stemmed() {
        for name in ["deed-patch-notes", "sha256:abc", "v0_9_3", "utf8"] {
            for token in tokens(name) {
                assert_eq!(fold(&token), token, "{name} was folded through {token}");
            }
        }
    }

    /// Only the terms both sides carry count, and the pass depends on both
    /// sides ascending.
    #[test]
    fn shared_terms_are_the_only_ones_that_count() {
        let left = [(1u32, 0.5f32), (4, 2.0), (9, 1.0)];
        let right = [(2u32, 3.0f32), (4, 0.5), (9, 0.25)];
        // 4 and 9 are shared: 2.0*0.5 + 1.0*0.25.
        assert!((sparse_dot(&left, &right) - 1.25).abs() < 1e-9);
        // Nothing shared is nothing, not an error and not a default score.
        assert_eq!(sparse_dot(&left, &[(2u32, 1.0f32), (3, 1.0)]), 0.0);
        assert_eq!(sparse_dot(&[], &right), 0.0);
    }

    /// Every name the panel accepts dispatches to its own voter.
    #[test]
    fn a_named_fuse_runs_the_voter_it_names() {
        // `c` is second on both ballots with most of the score mass: a score
        // voter ranks it first, a rank voter cannot.
        let first = vec![scored("a", 10.0), scored("c", 9.9), scored("b", 1.0)];
        let second = vec![scored("b", 10.0), scored("c", 9.9), scored("a", 1.0)];
        let now = crate::clock::utcnow();

        let order = |name: &str| -> Vec<String> {
            let panel = crate::panel::Panel::named(name, "none", "off").expect("voter");
            merge_ballots(&[first.clone(), second.clone()], 3, &panel, &now)
                .iter()
                .map(|hit| hit["id"].as_str().unwrap_or("").to_string())
                .collect()
        };

        let borda = order("borda");
        assert_eq!(borda.first().map(String::as_str), Some("a"), "{borda:?}");

        // Each score fusion answers with its own ranking, not Borda's.
        for name in ["combsum", "combmnz"] {
            let ranked = order(name);
            assert_eq!(
                ranked.first().map(String::as_str),
                Some("c"),
                "{name}: {ranked:?}"
            );
            assert_ne!(ranked, borda, "{name} is still answering as borda");
        }

        // Every rank voter answers with what its own module answers.
        let lists = [first.clone(), second.clone()];
        let keys: Vec<Vec<String>> = lists.iter().map(|hits| ballot_keys(hits)).collect();
        let k = 3;
        let expected: Vec<(&str, Vec<String>)> = vec![
            ("rrf", crate::rrf::rrf_merge(&keys, RRF_K0)),
            ("dowdall", crate::dowdall::dowdall_merge(&keys, k)),
            ("kemeny", crate::kemeny::kemeny_merge(&keys, k)),
            ("schulze", crate::schulze::schulze_merge(&keys, k)),
            ("copeland", crate::copeland::copeland_merge(&keys, k)),
            ("tideman", crate::tideman::ranked_pairs_merge(&keys, k)),
        ];
        for (name, want) in expected {
            let want: Vec<String> = want
                .iter()
                .map(|key| key.rsplit('\u{0}').next().unwrap_or(key).to_string())
                .collect();
            assert_eq!(order(name), want, "{name} did not answer as its own module");
        }
    }

    /// Position stands in for a weight, so the diversify slot downstream sees a
    /// relevance that decreases with rank whichever voter produced the order.
    #[test]
    fn a_fused_weight_falls_with_position() {
        let ballots = vec![vec!["a".to_string(), "b".to_string(), "c".to_string()]];
        let scored_ballots = vec![vec![
            ("a".to_string(), 1.0),
            ("b".to_string(), 0.5),
            ("c".to_string(), 0.1),
        ]];
        let (ranked, weights) =
            fuse_scores(crate::panel::Fuse::Borda, &ballots, &scored_ballots, 3);
        assert_eq!(ranked, vec!["a", "b", "c"]);
        assert!(weights["a"] > weights["b"] && weights["b"] > weights["c"]);
    }

    /// One question, for the tests that only vary part of it.
    fn ask<'a>(
        user: &'a str,
        memory: &'a str,
        atoms: &'a [Record],
        query: &'a str,
        limit: usize,
        set: Option<&'a str>,
    ) -> Ask<'a> {
        Ask {
            user,
            memory,
            atoms,
            query,
            limit,
            set,
            now: NOW,
        }
    }

    const NOW: &str = "2026-01-01T00:00:00.000Z";

    fn atom(value: Value) -> Record {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn stopwords_leave_the_query() {
        assert_eq!(tokens("what is the parser"), vec!["parser".to_string()]);
        assert!(tokens("the and of").is_empty());
    }

    #[test]
    fn a_one_block_card_splits_on_lines() {
        // Otherwise a card of one-line claims scores as one paragraph and the
        // whole file comes back as a single hit.
        let lines = paragraphs("first claim\nsecond claim\nthird claim\n");
        assert_eq!(lines.len(), 3, "{lines:?}");
        let paras = paragraphs("first block\nstill first\n\nsecond block\n");
        assert_eq!(paras.len(), 2, "{paras:?}");
    }

    #[test]
    fn edit_distance_stops_counting_at_two() {
        assert_eq!(edits("abc", "abc"), 0);
        assert_eq!(edits("abc", "abd"), 1);
        assert_eq!(edits("abc", "abcd"), 1);
        assert_eq!(edits("abc", "xyz"), 3);
        assert_eq!(
            edits("a", "abcdef"),
            2,
            "a far pair is not measured exactly"
        );
    }

    #[test]
    fn a_short_query_is_exact_only() {
        // "pr" is not a prefix of "prefers", or every query matches everything.
        assert_eq!(token_score("pr", "prefers"), 0.0);
        assert_eq!(token_score("pr", "pr"), 4.0);
        assert_eq!(token_score("pref", "prefers"), 3.0);
        assert_eq!(token_score("efer", "prefers"), 2.0);
        assert_eq!(token_score("prefer", "prefers"), 3.0);
        assert_eq!(token_score("prefrs", "prefers"), 1.5, "one edit");
    }

    #[test]
    fn recency_halves_over_the_half_life() {
        let two_weeks_ago = "2025-12-18T00:00:00.000Z";
        let weight = recency(Some(two_weeks_ago), NOW);
        assert!((weight - 0.5).abs() < 1e-9, "{weight}");
        assert_eq!(recency(None, NOW), 1.0);
        assert_eq!(recency(Some(""), NOW), 1.0);
        assert_eq!(
            recency(Some("not a stamp"), NOW),
            1.0,
            "unparsed is current"
        );
    }

    #[test]
    fn the_seat_card_outranks_the_workspace_card_at_equal_relevance() {
        let hits = search_linear(&ask(
            "prefer ripgrep",
            "prefer ripgrep",
            &[],
            "ripgrep",
            10,
            None,
        ));
        assert_eq!(hits[0]["field"], json!("user"), "{hits:?}");
        assert!(
            hits[0]["score"].as_f64().unwrap() > hits[1]["score"].as_f64().unwrap(),
            "{hits:?}"
        );
    }

    #[test]
    fn an_atom_matches_on_its_entities_as_well_as_its_text() {
        let atoms = vec![atom(json!({
            "id": "a", "text": "nothing in the prose", "kind": "voice",
            "entities": ["ripgrep"]
        }))];
        let hits = search_linear(&ask("", "", &atoms, "ripgrep", 10, None));
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0]["id"], json!("a"));
    }

    #[test]
    fn a_due_atom_with_no_overlap_is_on_the_clock_not_in_search() {
        let atoms = vec![atom(json!({
            "id": "a", "text": "utterly unrelated", "kind": "voice",
            "due_at": "2020-01-01T00:00:00.000Z"
        }))];
        assert!(
            search_linear(&ask("", "", &atoms, "ripgrep", 10, None)).is_empty(),
            "due is the clock, not a search hit"
        );
        assert_eq!(due_hits(&atoms, None, NOW).len(), 1);
    }

    #[test]
    fn an_expired_atom_is_not_searched() {
        let atoms = vec![atom(json!({
            "id": "a", "text": "ripgrep here", "kind": "voice",
            "valid_to": "2020-01-01T00:00:00.000Z"
        }))];
        assert!(search_linear(&ask("", "", &atoms, "ripgrep", 10, None)).is_empty());
    }

    #[test]
    fn a_closed_atom_is_found_when_the_ask_is_inside_its_window() {
        let atoms = vec![atom(json!({
            "id": "a", "text": "ripgrep here", "kind": "voice",
            "valid_from": "2023-01-01T00:00:00.000Z",
            "valid_to": "2024-12-01T00:00:00.000Z"
        }))];
        let then = Ask {
            now: "2024-06-01T00:00:00.000Z",
            ..ask("", "", &atoms, "ripgrep", 10, None)
        };
        let hits = search_linear(&then);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0]["id"], json!("a"));
        assert!(search_linear(&ask("", "", &atoms, "ripgrep", 10, None)).is_empty());
    }

    #[test]
    fn a_set_scope_filters_the_atoms_and_not_the_cards() {
        let atoms = vec![
            atom(json!({"id": "in", "text": "ripgrep", "kind": "voice", "set": "review"})),
            atom(json!({"id": "out", "text": "ripgrep", "kind": "voice"})),
        ];
        let hits = search_linear(&ask("", "", &atoms, "ripgrep", 10, Some("review")));
        let ids: Vec<&str> = hits.iter().filter_map(|h| h["id"].as_str()).collect();
        assert_eq!(ids, vec!["in"], "{hits:?}");
    }

    #[test]
    fn an_empty_query_finds_nothing_rather_than_everything() {
        let atoms = vec![atom(json!({"id": "a", "text": "x", "kind": "voice"}))];
        assert!(search_linear(&ask("u", "m", &atoms, "", 10, None)).is_empty());
        assert!(search_linear(&ask("u", "m", &atoms, "the and of", 10, None)).is_empty());
    }

    #[test]
    fn a_due_atom_with_no_query_overlap_is_not_a_search_hit() {
        // 212 due personas were filling every query at score 3.1. Due is
        // the clock (`due_hits`); search is overlap.
        let atoms = vec![atom(json!({
            "id": "persona-a",
            "kind": "persona",
            "text": "Apply Ask number strips and the two-lip close.",
            "due_at": "2020-01-01T00:00:00.000Z"
        }))];
        let hits = search_linear(&ask("", "", &atoms, "zzzzqqqq nonsense token", 10, None));
        assert!(hits.is_empty(), "{hits:?}");
    }

    #[test]
    fn due_hits_lead_and_are_not_repeated_behind_themselves() {
        let atoms = vec![atom(json!({
            "id": "due", "text": "ripgrep", "kind": "voice",
            "due_at": "2020-01-01T00:00:00.000Z"
        }))];
        let due = due_hits(&atoms, None, NOW);
        let ranked = search_linear(&ask("", "", &atoms, "ripgrep", 10, None));
        assert_eq!(due.len(), 1);
        assert_eq!(ranked.len(), 1);
        let merged = front_due(due, ranked, 10);
        assert_eq!(merged.len(), 1, "one atom, one hit: {merged:?}");
    }

    #[test]
    fn a_due_set_bigger_than_the_window_still_leaves_room_for_relevance() {
        // 63 atoms were due against a limit of 10, so every query came back
        // with the same ten due rows at the same score and the ranked list
        // was computed and discarded.
        let due: Vec<Value> = (0..63)
            .map(|n| json!({"field": "atom", "id": format!("due-{n:02}"), "score": 3.1}))
            .collect();
        let ranked: Vec<Value> = (0..10)
            .map(|n| json!({"field": "atom", "id": format!("hit-{n:02}"), "score": 1.0}))
            .collect();
        let merged = front_due(due, ranked, 10);
        assert_eq!(merged.len(), 10);
        let ids: Vec<&str> = merged.iter().map(|h| h["id"].as_str().unwrap()).collect();
        assert_eq!(ids.iter().filter(|i| i.starts_with("due-")).count(), 5);
        assert_eq!(ids.iter().filter(|i| i.starts_with("hit-")).count(), 5);
        assert_eq!(ids[0], "due-00", "the review clock still leads");
    }

    #[test]
    fn due_takes_the_whole_window_when_nothing_ranked() {
        let due: Vec<Value> = (0..20)
            .map(|n| json!({"field": "atom", "id": format!("due-{n:02}"), "score": 3.1}))
            .collect();
        assert_eq!(front_due(due, vec![], 10).len(), 10);
    }

    #[test]
    fn a_short_ranked_list_lets_due_fill_the_rest() {
        let due: Vec<Value> = (0..8)
            .map(|n| json!({"field": "atom", "id": format!("due-{n:02}"), "score": 3.1}))
            .collect();
        let ranked = vec![json!({"field": "atom", "id": "hit-00", "score": 1.0})];
        let merged = front_due(due, ranked, 10);
        assert_eq!(merged.len(), 9, "{merged:?}");
    }

    #[test]
    fn a_zero_limit_answers_nothing() {
        let atoms = vec![atom(json!({"id": "a", "text": "ripgrep", "kind": "voice"}))];
        assert!(search_linear(&ask("", "", &atoms, "ripgrep", 0, None)).is_empty());
        assert!(front_due(vec![json!({})], vec![], 0).is_empty());
    }
}

/// The identity of a hit across ranked lists: field and id, since a prose hit
/// has no id of its own.
#[must_use]
pub fn hit_key_of(hit: &Value) -> String {
    format!(
        "{}\u{0}{}",
        hit["field"].as_str().unwrap_or(""),
        hit["id"].as_str().unwrap_or("")
    )
}

/// Distinct keys of a ranked list, in order.
fn ballot_keys(hits: &[Value]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    hits.iter()
        .map(hit_key_of)
        .filter(|key| seen.insert(key.clone()))
        .collect()
}

/// Reciprocal rank fusion's smoothing constant, as the paper sets it.
const RRF_K0: usize = 60;

/// Run the named voter; a key's weight is its position, since the voters
/// share no score scale. Ties keep first-seen order.
fn fuse_scores(
    fuse: crate::panel::Fuse,
    ballots: &[Vec<String>],
    scored: &[crate::comb::ScoredBallot<String>],
    k: usize,
) -> (Vec<String>, std::collections::HashMap<String, f64>) {
    use crate::panel::Fuse;
    if ballots.is_empty() || k == 0 {
        return (Vec::new(), std::collections::HashMap::new());
    }
    let ranked = match fuse {
        Fuse::Borda => crate::borda::borda_merge(ballots, k),
        Fuse::Rrf => crate::rrf::rrf_merge(ballots, RRF_K0),
        // The two score fusions are the only voters that read the scores; every
        // other one reads position alone.
        Fuse::CombSum => crate::comb::combsum_merge(scored),
        Fuse::CombMnz => crate::comb::combmnz_merge(scored),
        Fuse::Dowdall => crate::dowdall::dowdall_merge(ballots, k),
        Fuse::Kemeny => crate::kemeny::kemeny_merge(ballots, k),
        Fuse::Schulze => crate::schulze::schulze_merge(ballots, k),
        Fuse::Copeland => crate::copeland::copeland_merge(ballots, k),
        Fuse::Tideman => crate::tideman::ranked_pairs_merge(ballots, k),
    };
    let n = ranked.len() as f64;
    let scores = ranked
        .iter()
        .enumerate()
        .map(|(index, key)| (key.clone(), n - index as f64))
        .collect();
    (ranked, scores)
}

/// One ballot as (key, score) pairs, which is what a score fusion needs.
fn scored_ballot(hits: &[Value]) -> crate::comb::ScoredBallot<String> {
    let mut seen = std::collections::HashSet::new();
    hits.iter()
        .filter_map(|hit| {
            let key = hit_key_of(hit);
            seen.insert(key.clone())
                .then(|| (key, hit["score"].as_f64().unwrap_or(0.0)))
        })
        .collect()
}

/// Named fuse, then diversify, then decay, over two ranked lists.
///
/// Not a de-duplicate: two lists that agree about a hit are two votes for it,
/// which is the whole reason to run a panel rather than concatenate.
#[must_use]
pub fn merge_ballots(
    ballots: &[Vec<Value>],
    limit: usize,
    panel: &crate::panel::Panel,
    now: &str,
) -> Vec<Value> {
    if limit == 0 {
        return Vec::new();
    }
    let mut by_key: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    // Reversed, so an earlier list's copy of a hit is the one kept.
    for hits in ballots.iter().rev() {
        for hit in hits {
            by_key.insert(hit_key_of(hit), hit.clone());
        }
    }
    let keys: Vec<Vec<String>> = ballots.iter().map(|hits| ballot_keys(hits)).collect();
    let scored: Vec<crate::comb::ScoredBallot<String>> =
        ballots.iter().map(|hits| scored_ballot(hits)).collect();
    let (mut ranked, scores) = fuse_scores(panel.fuse, &keys, &scored, limit);
    let mut weights: std::collections::HashMap<String, f64> = ranked
        .iter()
        .map(|key| (key.clone(), scores.get(key).copied().unwrap_or(0.0)))
        .collect();

    if panel.decay != crate::panel::Decay::Off {
        let order: std::collections::HashMap<&String, usize> =
            ranked.iter().enumerate().map(|(i, k)| (k, i)).collect();
        for key in &ranked {
            let hit = &by_key[key];
            let source = hit["field"].as_str().unwrap_or("");
            // Since the last review when there was one, else since the write.
            let since = hit["review"]["last"]
                .as_str()
                .filter(|l| !l.is_empty())
                .or_else(|| hit["ts"].as_str());
            let age = since.map_or(0.0, |ts| clock::elapsed_days(ts, now));
            let stability = hit["review"]["stability"]
                .as_f64()
                .unwrap_or(crate::record::DEFAULT_STABILITY);
            if let Some(weight) = weights.get_mut(key) {
                *weight *= panel.decay_weight(source, age, stability);
            }
        }
        let mut sorted = ranked.clone();
        sorted.sort_by(|a, b| {
            let wa = weights[a];
            let wb = weights[b];
            wb.partial_cmp(&wa)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| order[a].cmp(&order[b]))
        });
        ranked = sorted;
    }

    let items: Vec<crate::mmr::Ranked> = ranked
        .iter()
        .map(|key| crate::mmr::Ranked {
            id: key.clone(),
            rel: weights[key],
            tokens: raw_tokens(by_key[key]["text"].as_str().unwrap_or("")),
        })
        .collect();
    let order = panel.rerank(&items, 0.7);
    // How many ballots named each hit, out of how many ran: two lists
    // agreeing is the panel's reason to exist, and a reader that wants
    // only what the ballots agree on can ask for `ballots >= 2`.
    let named: std::collections::HashMap<&String, usize> = keys
        .iter()
        .flat_map(|list| {
            list.iter()
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
        })
        .fold(std::collections::HashMap::new(), |mut m, k| {
            *m.entry(k).or_insert(0) += 1;
            m
        });
    let of = ballots.len();
    // The score a caller reads is the panel's, on one scale; the ballot's own
    // score stays beside it.
    order
        .into_iter()
        .filter_map(|key| {
            let mut hit = by_key.get(&key).cloned()?;
            if let Some(object) = hit.as_object_mut() {
                if let Some(own) = object.get("score").cloned() {
                    object.insert("ballot_score".into(), own);
                }
                object.insert("score".into(), json!(weights[&key]));
                object.insert(
                    "ballots".into(),
                    json!(named.get(&key).copied().unwrap_or(0)),
                );
                object.insert("of".into(), json!(of));
            }
            Some(hit)
        })
        .take(limit)
        .collect()
}

/// Tokens with the stopwords kept, which is what a similarity wants.
///
/// Dropping them here would make two hits that share nothing but "the" look
/// alike to the diversifier.
fn raw_tokens(text: &str) -> std::collections::HashSet<String> {
    let lower = text.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut out = std::collections::HashSet::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i].is_ascii_alphanumeric() {
            let start = i;
            i += 1;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'-')
            {
                i += 1;
            }
            out.insert(lower[start..i].to_string());
        } else {
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod merge_tests {
    use super::*;
    use crate::panel::Panel;
    use serde_json::json;

    const NOW: &str = "2026-01-01T00:00:00.000Z";

    fn hit(field: &str, id: &str, text: &str) -> Value {
        json!({"field": field, "id": id, "text": text, "score": 1.0})
    }

    fn default_panel() -> Panel {
        Panel::named("borda", "none", "off").unwrap()
    }

    #[test]
    fn a_hit_two_lists_agree_on_outranks_one_only_a_leader_named() {
        // Two votes beat one first place, which is the reason to run a panel
        // rather than concatenate the lists.
        let a = vec![hit("atom", "solo", "alpha"), hit("atom", "both", "beta")];
        let b = vec![hit("atom", "both", "beta"), hit("atom", "other", "gamma")];
        let merged = merge_ballots(&[a, b], 10, &default_panel(), NOW);
        assert_eq!(merged[0]["id"], json!("both"), "{merged:?}");
    }

    /// The score on a returned hit is the panel's fused weight, so two hits
    /// from different ballots read on one scale; the ballot's own score is
    /// kept beside it.
    #[test]
    fn a_hit_says_how_many_ballots_named_it() {
        let a = vec![hit("atom", "both", "alpha"), hit("atom", "solo", "beta")];
        let b = vec![hit("atom", "both", "alpha")];
        let merged = merge_ballots(&[a, b], 10, &default_panel(), NOW);
        let both = merged.iter().find(|h| h["id"] == "both").unwrap();
        let solo = merged.iter().find(|h| h["id"] == "solo").unwrap();
        assert_eq!(both["ballots"], json!(2));
        assert_eq!(solo["ballots"], json!(1));
        assert_eq!(both["of"], json!(2));
    }

    #[test]
    fn a_returned_score_is_the_panels() {
        let a = vec![hit("atom", "both", "alpha"), hit("atom", "solo", "beta")];
        let b = vec![hit("atom", "both", "alpha")];
        let merged = merge_ballots(&[a, b], 10, &default_panel(), NOW);
        let both = merged.iter().find(|h| h["id"] == "both").unwrap();
        let solo = merged.iter().find(|h| h["id"] == "solo").unwrap();
        assert!(both["score"].as_f64().unwrap() > solo["score"].as_f64().unwrap());
        assert!(both.get("ballot_score").is_some());
        let scores: Vec<f64> = merged
            .iter()
            .map(|h| h["score"].as_f64().unwrap())
            .collect();
        assert!(scores.windows(2).all(|w| w[0] >= w[1]), "{scores:?}");
    }

    /// A claim reviewed long ago ranks below one reviewed today under the
    /// retrievability slot; off keeps the fused order.
    #[test]
    fn a_stale_claim_sinks_only_when_decay_reads_the_review_clock() {
        let hit = |id: &str, last: &str| {
            json!({
                "id": id, "field": "atoms", "text": "the same claim", "score": 1.0,
                "ts": "2026-01-01T00:00:00.000Z",
                "review": {"last": last, "stability": 1.0}
            })
        };
        let stale = hit("stale", "2025-10-01T00:00:00.000Z");
        let fresh = hit("fresh", NOW);
        let ballot = vec![stale, fresh];
        let off = merge_ballots(std::slice::from_ref(&ballot), 2, &default_panel(), NOW);
        assert_eq!(off[0]["id"], "stale");
        let fsrs = crate::panel::Panel::named("combmnz", "none", "fsrs").unwrap();
        let ranked = merge_ballots(&[ballot], 2, &fsrs, NOW);
        assert_eq!(ranked[0]["id"], "fresh");
        assert_eq!(ranked.len(), 2);
    }

    #[test]
    fn a_prose_hit_with_no_id_does_not_collapse_into_another() {
        let a = vec![hit("user", "", "one"), hit("memory", "", "two")];
        let merged = merge_ballots(&[a], 10, &default_panel(), NOW);
        assert_eq!(merged.len(), 2, "{merged:?}");
    }

    #[test]
    fn a_tie_keeps_the_order_it_was_first_seen_in() {
        // Otherwise two runs over the same data disagree.
        let a = vec![hit("atom", "x", "one"), hit("atom", "y", "two")];
        let b = vec![hit("atom", "y", "two"), hit("atom", "x", "one")];
        let first = merge_ballots(&[a.clone(), b.clone()], 10, &default_panel(), NOW);
        let second = merge_ballots(&[a, b], 10, &default_panel(), NOW);
        assert_eq!(first, second);
        assert_eq!(first[0]["id"], json!("x"), "{first:?}");
    }

    #[test]
    fn the_limit_is_applied_after_the_merge() {
        let a = vec![hit("atom", "a", "one"), hit("atom", "b", "two")];
        let b = vec![hit("atom", "c", "three")];
        assert_eq!(
            merge_ballots(&[a.clone(), b.clone()], 1, &default_panel(), NOW).len(),
            1
        );
        assert!(merge_ballots(&[a, b], 0, &default_panel(), NOW).is_empty());
    }

    #[test]
    fn an_empty_ballot_does_not_erase_the_other() {
        let a = vec![hit("atom", "a", "one")];
        let merged = merge_ballots(&[a, Vec::new()], 10, &default_panel(), NOW);
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn the_diversifier_reads_stopwords_too() {
        // "the" and "of" carry no query signal but they do say two texts look
        // alike, which is a different question.
        let with = raw_tokens("the parser of the header");
        assert!(with.contains("the"), "{with:?}");
        assert!(!tokens("the parser of the header").contains(&"the".to_string()));
    }
}
