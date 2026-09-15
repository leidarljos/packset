//! The dense projection as a kept child process (`packset-embed`), one per
//! direction. An absent encoder is a supported state: every failure falls
//! back to the lexical scorers. A vector is derivable from the text, never
//! the store.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::sync_channel;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};

/// Environment variables naming the encoder.
pub const BIN_VARS: &[&str] = &["PACKSET_EMBED"];

/// The encoder binary this seat would run: `PACKSET_EMBED`, else beside the
/// writer, else on `PATH`.
#[must_use]
pub fn binary() -> Option<PathBuf> {
    for var in BIN_VARS {
        if let Some(raw) = std::env::var_os(var) {
            let path = PathBuf::from(raw);
            if is_executable(&path) {
                return Some(path);
            }
        }
    }
    let here = std::env::current_exe().ok()?;
    // Beside this binary, which is where a seat that installs the pair puts it.
    if let Some(beside) = here
        .parent()
        .map(|dir| dir.join("packset-embed"))
        .filter(|path| is_executable(path))
    {
        return Some(beside);
    }
    if let Some(root) = here.parent().and_then(Path::parent).and_then(Path::parent) {
        for candidate in [
            root.join("bin/packset-embed"),
            root.join("crates/packset-embed/target/release/packset-embed"),
        ] {
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    which("packset-embed")
}

fn which(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|path| is_executable(path))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// A running encoder: one child, one line in, one line out.
struct Encoder {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
}

impl Encoder {
    fn start(binary: &Path, query: bool) -> Option<Self> {
        // One child encodes both sides. `--query` is still accepted by the
        // binary for old callers; this writer sends `query` per line instead.
        let _ = query;
        let mut command = Command::new(binary);
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let stdin = child.stdin.take()?;
        let stdout = BufReader::new(child.stdout.take()?);
        Some(Self {
            child,
            stdin,
            stdout,
        })
    }

    fn start_rerank(binary: &Path) -> Option<Self> {
        let mut child = Command::new(binary)
            .arg("--rerank")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let stdin = child.stdin.take()?;
        let stdout = BufReader::new(child.stdout.take()?);
        Some(Self {
            child,
            stdin,
            stdout,
        })
    }

    /// One question against many candidates in one line, one score each; the
    /// child packs the batch into one forward pass.
    fn rerank(&mut self, question: &str, candidates: &[String]) -> Option<Vec<f32>> {
        let asked = serde_json::json!({ "id": "q", "q": question, "d": candidates });
        let reply = self.ask_json(&asked.to_string())?;
        let parsed: Value = serde_json::from_str(reply.trim()).ok()?;
        let scores: Vec<f32> = parsed
            .get("s")?
            .as_array()?
            .iter()
            .map(|v| v.as_f64().unwrap_or(0.0) as f32)
            .collect();
        // A short answer is a mismatch between what was asked and what came
        // back, and padding it would silently score the tail as zero.
        (scores.len() == candidates.len()).then_some(scores)
    }

    fn start_sparse(binary: &Path) -> Option<Self> {
        let mut child = Command::new(binary)
            .arg("--sparse")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let stdin = child.stdin.take()?;
        let stdout = BufReader::new(child.stdout.take()?);
        Some(Self {
            child,
            stdin,
            stdout,
        })
    }

    /// One line in, the learned term weights out.
    fn encode_sparse(&mut self, text: &str) -> Option<Sparse> {
        let reply = self.ask(text)?;
        let parsed: Value = serde_json::from_str(reply.trim()).ok()?;
        let s = parsed.get("s")?;
        let indices = s.get("i")?.as_array()?;
        let weights = s.get("w")?.as_array()?;
        if indices.len() != weights.len() {
            return None;
        }
        let mut pairs: Sparse = indices
            .iter()
            .zip(weights)
            .filter_map(|(i, w)| Some((i.as_u64()? as u32, w.as_f64()? as f32)))
            .collect();
        // Ascending by index, which is what lets two of them intersect in one
        // pass; the model does not promise an order.
        pairs.sort_unstable_by_key(|(index, _)| *index);
        Some(pairs)
    }

    fn start_late(binary: &Path) -> Option<Self> {
        let mut child = Command::new(binary)
            .arg("--late")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let stdin = child.stdin.take()?;
        let stdout = BufReader::new(child.stdout.take()?);
        Some(Self {
            child,
            stdin,
            stdout,
        })
    }

    /// One line in, one line carrying all three forms out.
    fn encode_tokens(&mut self, text: &str) -> Option<(Vec<Vec<f32>>, Vec<f32>, Sparse)> {
        let reply = self.ask(text)?;
        let parsed: Value = serde_json::from_str(reply.trim()).ok()?;
        let rows = parsed.get("t")?.as_array()?;
        let tokens: Vec<Vec<f32>> = rows
            .iter()
            .filter_map(|row| {
                let vector: Vec<f32> = row
                    .as_array()?
                    .iter()
                    .filter_map(|v| v.as_f64().map(|f| f as f32))
                    .collect();
                (!vector.is_empty()).then_some(vector)
            })
            .collect();
        let pooled: Vec<f32> = parsed
            .get("v")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_f64().map(|f| f as f32))
                    .collect()
            })
            .unwrap_or_default();
        let sparse = parsed
            .get("s")
            .map(|raw| {
                let indices = raw
                    .get("i")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|value| value.as_u64().map(|index| index as u32));
                let weights = raw
                    .get("w")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|value| value.as_f64().map(|weight| weight as f32));
                indices.zip(weights).collect()
            })
            .unwrap_or_default();
        let mut sparse: Sparse = sparse;
        sparse.sort_unstable_by_key(|(index, _)| *index);
        (!tokens.is_empty()).then_some((tokens, pooled, sparse))
    }

    /// Write one request and read its one-line reply.
    fn ask(&mut self, text: &str) -> Option<String> {
        self.ask_text(text, false)
    }

    fn ask_text(&mut self, text: &str, query: bool) -> Option<String> {
        let line = json!({ "id": "0", "text": text, "query": query });
        self.ask_json(&line.to_string())
    }

    /// One line written, one line read back.
    fn ask_json(&mut self, line: &str) -> Option<String> {
        writeln!(self.stdin, "{line}").ok()?;
        self.stdin.flush().ok()?;
        let mut reply = String::new();
        if self.stdout.read_line(&mut reply).ok()? == 0 {
            return None;
        }
        Some(reply)
    }

    fn encode(&mut self, text: &str) -> Option<Vec<f32>> {
        self.encode_as(text, false)
    }

    fn encode_batch(&mut self, texts: &[String], query: bool) -> Option<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Some(Vec::new());
        }
        if texts.len() == 1 {
            return self.encode_as(&texts[0], query).map(|v| vec![v]);
        }
        let line = json!({ "id": "b", "query": query, "texts": texts });
        let reply = self.ask_json(&line.to_string())?;
        let parsed: Value = serde_json::from_str(reply.trim()).ok()?;
        let rows = parsed.get("vs")?.as_array()?;
        let out: Vec<Vec<f32>> = rows
            .iter()
            .filter_map(|row| {
                Some(
                    row.as_array()?
                        .iter()
                        .filter_map(|x| x.as_f64().map(|f| f as f32))
                        .collect(),
                )
            })
            .collect();
        (out.len() == texts.len()).then_some(out)
    }

    fn encode_as(&mut self, text: &str, query: bool) -> Option<Vec<f32>> {
        let reply = self.ask_text(text, query)?;
        let parsed: Value = serde_json::from_str(reply.trim()).ok()?;
        let vector: Vec<f32> = parsed
            .get("v")?
            .as_array()?
            .iter()
            .filter_map(|v| v.as_f64().map(|f| f as f32))
            .collect();
        (!vector.is_empty()).then_some(vector)
    }

    /// Whether the child is still there to be asked.
    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Learned term weights: which vocabulary entries a text activates, and how
/// much. Ascending by index, so two of them intersect in one pass.
pub type Sparse = Vec<(u32, f32)>;

/// One kept encoder. Query and document share it; the prefix is per line.
type Slot = Mutex<Option<Encoder>>;

fn slot(_query: bool) -> &'static Slot {
    dense_slot()
}

/// The one dense child. `PACKSET_EMBED_QUERY_WORKERS` > 1 is extra models
/// in RAM for hosts that asked.
fn dense_slot() -> &'static Slot {
    if query_workers() <= 1 {
        static ONE: OnceLock<Slot> = OnceLock::new();
        return ONE.get_or_init(|| Mutex::new(None));
    }
    query_slot()
}

/// Encode one text, or nothing when this seat has no working encoder. A dead
/// child is replaced once and the text retried.
#[must_use]
/// How many query encoders the writer keeps. One encoder is one model in
/// RAM. Default 1. `PACKSET_EMBED_QUERY_WORKERS` raises it; a pool of two
/// was 4 GB on a laptop that also held a document encoder.
fn query_workers() -> usize {
    std::env::var("PACKSET_EMBED_QUERY_WORKERS")
        .ok()
        .and_then(|raw| raw.trim().parse().ok())
        .filter(|n: &usize| *n >= 1)
        .unwrap_or(1)
}

/// The query encoders: the first one free answers; when all are busy the
/// caller waits on the first, which keeps every slot warm and none idle.
fn query_slot() -> &'static Slot {
    static POOL: OnceLock<Vec<Slot>> = OnceLock::new();
    let pool = POOL.get_or_init(|| (0..query_workers()).map(|_| Mutex::new(None)).collect());
    for s in pool {
        if let Ok(guard) = s.try_lock() {
            drop(guard);
            return s;
        }
    }
    &pool[0]
}

/// Start every query encoder now, side by side, so the first agents to ask
/// at once do not each pay a model load. Each probe holds one slot while it
/// runs, which is what makes the pool spread rather than stack.
pub fn warm_queries() {
    let workers = query_workers();
    let hands: Vec<_> = (0..workers)
        .map(|_| std::thread::spawn(|| encode_query("the pack is open")))
        .collect();
    for hand in hands {
        let _ = hand.join();
    }
}

struct Pending {
    text: String,
    query: bool,
    tx: std::sync::mpsc::SyncSender<Option<Vec<f32>>>,
}

fn pending() -> &'static (Mutex<Vec<Pending>>, Condvar) {
    static Q: OnceLock<(Mutex<Vec<Pending>>, Condvar)> = OnceLock::new();
    Q.get_or_init(|| (Mutex::new(Vec::new()), Condvar::new()))
}

fn ensure_pump() {
    static START: OnceLock<()> = OnceLock::new();
    START.get_or_init(|| {
        let _ = std::thread::Builder::new()
            .name("packset-embed-pump".into())
            .spawn(pump);
    });
}

fn pump() {
    let (lock, cv) = pending();
    loop {
        let mut held = match lock.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        while held.is_empty() {
            held = match cv.wait(held) {
                Ok(g) => g,
                Err(_) => return,
            };
        }
        drop(held);
        std::thread::sleep(Duration::from_millis(2));
        let batch = match lock.lock() {
            Ok(mut g) => std::mem::take(&mut *g),
            Err(_) => return,
        };
        dispatch(batch);
    }
}

fn dispatch(batch: Vec<Pending>) {
    let mut queries = Vec::new();
    let mut docs = Vec::new();
    for job in batch {
        if job.query {
            queries.push(job);
        } else {
            docs.push(job);
        }
    }
    run_group(queries, true);
    run_group(docs, false);
}

fn run_group(jobs: Vec<Pending>, query: bool) {
    if jobs.is_empty() {
        return;
    }
    let texts: Vec<String> = jobs.iter().map(|j| j.text.clone()).collect();
    let vecs = encode_now(&texts, query);
    let mut answers = vecs.unwrap_or_default().into_iter();
    for job in jobs {
        let _ = job.tx.send(answers.next());
    }
}

fn encode_now(texts: &[String], query: bool) -> Option<Vec<Vec<f32>>> {
    let binary = binary()?;
    let mut held = dense_slot().lock().ok()?;
    for _ in 0..2 {
        if held.as_mut().is_none_or(|running| !running.alive()) {
            *held = Encoder::start(&binary, query);
        }
        let running = held.as_mut()?;
        if let Some(vectors) = running.encode_batch(texts, query) {
            return Some(vectors);
        }
        *held = None;
    }
    None
}

pub fn encode(text: &str, query: bool) -> Option<Vec<f32>> {
    if text.trim().is_empty() {
        return None;
    }
    if binary().is_none() {
        return None;
    }
    ensure_pump();
    let (tx, rx) = sync_channel(1);
    {
        let (lock, cv) = pending();
        let mut q = lock.lock().ok()?;
        q.push(Pending {
            text: text.to_string(),
            query,
            tx,
        });
        cv.notify_one();
    }
    rx.recv().ok()?
}

/// Encode one query.
#[must_use]
pub fn encode_query(text: &str) -> Option<Vec<f32>> {
    encode(text, true)
}

/// Encode one atom's text.
#[must_use]
pub fn encode_document(text: &str) -> Option<Vec<f32>> {
    encode(text, false)
}

/// The kept encoder for the per-token form, which is a third child.
fn late_slot() -> &'static Slot {
    static LATE: OnceLock<Slot> = OnceLock::new();
    LATE.get_or_init(|| Mutex::new(None))
}

/// Encode one text three ways from one pass: a vector per token, the pooled
/// vector, and learned term weights. Read by the retrieval benchmark only.
#[must_use]
pub fn encode_late(text: &str) -> Option<(Vec<Vec<f32>>, Vec<f32>, Sparse)> {
    if text.trim().is_empty() {
        return None;
    }
    let binary = binary()?;
    let mut held = late_slot().lock().ok()?;
    for attempt in 0..2 {
        if held.as_mut().is_none_or(|running| !running.alive()) {
            *held = Encoder::start_late(&binary);
        }
        let running = held.as_mut()?;
        if let Some(both) = running.encode_tokens(text) {
            return Some(both);
        }
        *held = None;
        if attempt == 1 {
            return None;
        }
    }
    None
}

/// The kept learned-sparse encoder, a fifth child.
fn sparse_slot() -> &'static Slot {
    static SPARSE: OnceLock<Slot> = OnceLock::new();
    SPARSE.get_or_init(|| Mutex::new(None))
}

/// Learned term weights from SPLADE (doi:10.1145/3404835.3463098), a model
/// trained for the weights, as opposed to the sparse head [`encode_late`]
/// returns beside a dense vector. Read by the benchmark, not the writer.
#[must_use]
pub fn encode_sparse(text: &str) -> Option<Sparse> {
    if text.trim().is_empty() {
        return None;
    }
    let binary = binary()?;
    let mut held = sparse_slot().lock().ok()?;
    for attempt in 0..2 {
        if held.as_mut().is_none_or(|running| !running.alive()) {
            *held = Encoder::start_sparse(&binary);
        }
        let running = held.as_mut()?;
        if let Some(weights) = running.encode_sparse(text) {
            return Some(weights);
        }
        *held = None;
        if attempt == 1 {
            return None;
        }
    }
    None
}

/// How deep the second stage reads: the deepest cut-off the locomo table scores.
pub const RERANK_DEPTH: usize = 20;

/// Whether the live search path runs the second stage by default. Off unless
/// asked; the same spellings `/v1/search?rerank=` accepts.
#[must_use]
pub fn wanted() -> bool {
    flag_on(&std::env::var("PACKSET_RERANK").unwrap_or_default())
}

/// Whether one request runs the stage: the query flag over the host default.
#[must_use]
pub fn requested(query: Option<&str>) -> bool {
    match query.map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => flag_on(raw),
        None => wanted(),
    }
}

fn flag_on(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes"
    )
}

/// Reorder the head of a ranking by cross-encoder scores. `scores` must cover
/// `RERANK_DEPTH.min(hits.len())`; the tail keeps its order; ties are stable.
#[must_use]
pub fn apply_rerank(hits: &[Value], scores: &[f32]) -> Option<Vec<Value>> {
    let depth = RERANK_DEPTH.min(hits.len());
    if scores.len() != depth {
        return None;
    }
    let mut head: Vec<(f32, Value)> = scores
        .iter()
        .copied()
        .zip(hits[..depth].iter().cloned())
        .collect();
    head.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut out: Vec<Value> = head.into_iter().map(|(_, hit)| hit).collect();
    out.extend_from_slice(&hits[depth..]);
    Some(out)
}

/// Reorder the top of a ranking by a cross-encoder; `None` when there is no
/// working reranker, so the caller keeps the ranking and says so.
#[must_use]
pub fn rerank_hits(question: &str, hits: &[Value]) -> Option<Vec<Value>> {
    if hits.is_empty() {
        return Some(Vec::new());
    }
    let depth = RERANK_DEPTH.min(hits.len());
    let candidates: Vec<String> = hits[..depth]
        .iter()
        .map(|hit| {
            hit.get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    let scores = rerank(question, &candidates)?;
    apply_rerank(hits, &scores)
}

/// The kept cross-encoder, a fourth child.
fn rerank_slot() -> &'static Slot {
    static RERANK: OnceLock<Slot> = OnceLock::new();
    RERANK.get_or_init(|| Mutex::new(None))
}

/// Cross-encoder scores for every candidate against the question
/// (doi:10.48550/arXiv.1901.04085), in the caller's order. Not the panel's
/// `rerank`, which is diversification.
#[must_use]
pub fn rerank(question: &str, candidates: &[String]) -> Option<Vec<f32>> {
    if question.trim().is_empty() {
        return None;
    }
    // Nothing to score is not a failure, and it must not cost a model call.
    if candidates.is_empty() {
        return Some(Vec::new());
    }
    let binary = binary()?;
    let mut held = rerank_slot().lock().ok()?;
    for attempt in 0..2 {
        if held.as_mut().is_none_or(|running| !running.alive()) {
            *held = Encoder::start_rerank(&binary);
        }
        let running = held.as_mut()?;
        if let Some(scores) = running.rerank(question, candidates) {
            return Some(scores);
        }
        *held = None;
        if attempt == 1 {
            return None;
        }
    }
    None
}

#[cfg(test)]
pub fn reset_for_test() {
    for slot in [dense_slot(), rerank_slot(), late_slot(), sparse_slot()] {
        if let Ok(mut held) = slot.lock() {
            *held = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_that_is_not_a_program_is_not_an_encoder() {
        assert!(!is_executable(&PathBuf::from("/nonexistent/packset-embed")));
        assert!(!is_executable(&PathBuf::from("/etc")));
    }

    #[test]
    fn empty_text_is_never_sent_to_a_model() {
        assert!(encode("", false).is_none());
        assert!(encode("   ", true).is_none());
    }

    /// No candidates is an empty ballot, not a missing reranker.
    #[test]
    fn nothing_to_rerank_is_an_empty_ballot_and_not_a_failure() {
        assert_eq!(rerank("which search tool", &[]), Some(Vec::new()));
        // An empty question is refused before any child is started, the same
        // way an empty text is never sent to an encoder.
        assert!(rerank("", &["a candidate".to_string()]).is_none());
        assert!(rerank("   ", &["a candidate".to_string()]).is_none());
    }

    /// A short reply is refused, not padded with zeros.
    #[test]
    fn a_short_reply_is_a_mismatch_rather_than_a_ranking() {
        let Ok(mut child) = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
        else {
            return;
        };
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return;
        };
        // `cat` echoes what it is given, so the reply carries the request's
        // own fields and no `s` at all: a well-formed line that is not an
        // answer.
        let mut echoing = Encoder {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        };
        let candidates = vec!["one".to_string(), "two".to_string()];
        assert!(echoing.rerank("a question", &candidates).is_none());
    }

    #[test]
    fn a_child_that_exits_is_not_alive() {
        let Ok(mut child) = Command::new("true").stdout(Stdio::piped()).spawn() else {
            return;
        };
        let _ = child.wait();
        assert!(matches!(child.try_wait(), Ok(Some(_))));
    }

    fn hit(id: &str, text: &str) -> Value {
        json!({ "id": id, "text": text })
    }

    /// The second stage is off unless the host asks. An unset env is the
    /// shipped default; a process that already exported PACKSET_RERANK is a
    /// different seat and this test does not speak for it.
    #[test]
    fn the_second_stage_is_off_unless_asked() {
        if std::env::var_os("PACKSET_RERANK").is_some() {
            return;
        }
        assert!(!wanted());
        assert!(!requested(None));
        assert!(!requested(Some("")));
        assert!(!requested(Some("0")));
        assert!(!requested(Some("on")));
        assert!(requested(Some("1")));
        assert!(requested(Some("true")));
        assert!(requested(Some("yes")));
    }

    #[test]
    fn apply_rerank_promotes_the_higher_score_and_keeps_the_tail() {
        let hits: Vec<Value> = (0..22)
            .map(|i| hit(&format!("h{i}"), &format!("text {i}")))
            .collect();
        let mut scores = vec![0.0f32; RERANK_DEPTH];
        scores[0] = 0.1;
        scores[1] = 0.9;
        let ranked = apply_rerank(&hits, &scores).expect("length matches");
        assert_eq!(ranked[0]["id"], json!("h1"));
        assert_eq!(ranked[1]["id"], json!("h0"));
        assert_eq!(ranked[2]["id"], json!("h2"));
        assert_eq!(ranked[20]["id"], json!("h20"));
        assert_eq!(ranked[21]["id"], json!("h21"));
        assert_eq!(ranked.len(), 22);
    }

    #[test]
    fn a_tie_keeps_the_first_stage_order() {
        let hits = vec![hit("first", "a"), hit("second", "b")];
        let ranked = apply_rerank(&hits, &[0.5, 0.5]).expect("length matches");
        assert_eq!(ranked[0]["id"], json!("first"));
        assert_eq!(ranked[1]["id"], json!("second"));
    }

    #[test]
    fn a_short_score_list_is_refused_rather_than_padded() {
        let hits = vec![hit("a", "a"), hit("b", "b")];
        assert!(apply_rerank(&hits, &[0.9]).is_none());
    }

    #[test]
    fn nothing_to_reorder_is_an_empty_ranking() {
        assert_eq!(rerank_hits("which search tool", &[]), Some(Vec::new()));
    }

    /// A stub child that scores later candidates higher, then `apply_rerank`.
    #[test]
    fn a_child_that_scores_the_tail_first_reorders_the_head() {
        let script =
            std::env::temp_dir().join(format!("packset-rerank-stub-{}.py", std::process::id()));
        let body = concat!(
            "#!/usr/bin/env python3\n",
            "import json, sys\n",
            "for line in sys.stdin:\n",
            "    line = line.strip()\n",
            "    if not line:\n",
            "        continue\n",
            "    req = json.loads(line)\n",
            "    d = req.get('d', [])\n",
            "    print(json.dumps({'id': req.get('id', 'q'), 's': list(range(len(d)))}), flush=True)\n",
        );
        if std::fs::write(&script, body).is_err() {
            return;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755));
        }
        let Ok(mut child) = Command::new(&script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
        else {
            let _ = std::fs::remove_file(&script);
            return;
        };
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = std::fs::remove_file(&script);
            return;
        };
        let mut enc = Encoder {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        };
        let candidates = vec!["first".into(), "second".into()];
        let scores = enc.rerank("a question", &candidates);
        drop(enc);
        let _ = std::fs::remove_file(&script);
        let scores = scores.expect("stub scored");
        let hits = vec![hit("a", "first"), hit("b", "second")];
        let ranked = apply_rerank(&hits, &scores).expect("length matches");
        assert_eq!(ranked[0]["id"], json!("b"));
        assert_eq!(ranked[1]["id"], json!("a"));
    }
}
