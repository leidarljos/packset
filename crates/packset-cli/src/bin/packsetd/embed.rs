//! The dense projection as a kept child process (`packset-embed`), one per
//! direction. An absent encoder is a supported state: every failure falls
//! back to the lexical scorers. A vector is derivable from the text, never
//! the store.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};

/// Environment variables naming the encoder.
pub const BIN_VARS: &[&str] = &["PACKSET_EMBED"];

/// The encoder binary this seat would run: `PACKSET_EMBED`, else beside the
/// writer, else on `PATH`. An explicit variable always wins over the kept
/// discovery below, so a seat or test that names an encoder never sees
/// the cache.
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
    found_binary()
}

/// The discovery every call used to repeat: the binary beside this one
/// and the name on `PATH`, neither of which moves under a running writer.
/// Found once and kept, so a burst of searches does not walk the
/// filesystem per request.
fn found_binary() -> Option<PathBuf> {
    static FOUND: OnceLock<Option<PathBuf>> = OnceLock::new();
    FOUND.get_or_init(discover).clone()
}

/// The three places a seat puts the encoder when no variable names it.
fn discover() -> Option<PathBuf> {
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

/// The query encoders side by side. Sized once, the first time a pooled
/// host asks, so a later change to the variable does not resize a live
/// pool.
fn pool_slots() -> &'static [Slot] {
    static POOL: OnceLock<Vec<Slot>> = OnceLock::new();
    POOL.get_or_init(|| (0..query_workers()).map(|_| Mutex::new(None)).collect())
}

/// The query encoders: the first one free answers, else the first slot.
/// The encode path itself does not wait here; see `lock_dense`, which
/// takes whichever slot frees first so no slot idles while searches
/// wait.
fn query_slot() -> &'static Slot {
    let pool = pool_slots();
    for s in pool {
        if let Ok(guard) = s.try_lock() {
            drop(guard);
            return s;
        }
    }
    &pool[0]
}

/// One pass over the pool: the first free slot's guard, if any. A
/// poisoned slot is recovered, the way `wait_for` recovers the rerank
/// slot, rather than failing the encode behind it.
fn poll_slots(slots: &[Slot]) -> Option<std::sync::MutexGuard<'_, Option<Encoder>>> {
    for s in slots {
        match s.try_lock() {
            Ok(guard) => return Some(guard),
            Err(std::sync::TryLockError::Poisoned(p)) => return Some(p.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
    }
    None
}

/// A held query encoder. A single-slot host blocks on its one child, as
/// before; a pooled host spins the pool a millisecond at a time and
/// takes whichever slot frees first. No deadline either way: a dense
/// ballot that gave up under load would silently thin the panel, so a
/// search waits for its encoder the way it always has.
fn lock_dense() -> Option<std::sync::MutexGuard<'static, Option<Encoder>>> {
    if query_workers() <= 1 {
        return dense_slot().lock().ok();
    }
    loop {
        if let Some(guard) = poll_slots(pool_slots()) {
            return Some(guard);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
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

/// Whether a query is waiting on the pump behind the batch now encoding:
/// document batches leave it a slot when one is.
fn queries_pending() -> bool {
    pending()
        .0
        .lock()
        .is_ok_and(|queue| queue.iter().any(|job| job.query))
}

/// How many slots a batch of this side may spread over: one on a
/// single-slot host, so its path stays exactly as it was, else the live
/// pool's size. A document batch leaves one slot free while a query
/// waits, so background writes never head-block an interactive search;
/// with no query waiting it spends the whole pool, and queries always
/// spread. The peek is advisory: a query arriving just after costs one
/// chunk, the same as today.
fn spread_slots(query: bool) -> usize {
    let slots = if query_workers() <= 1 {
        1
    } else {
        pool_slots().len().max(1)
    };
    if !query && slots > 1 && queries_pending() {
        slots - 1
    } else {
        slots
    }
}

/// How a batch spreads over the pool: contiguous index ranges, at most one
/// chunk per slot, covering every text exactly once with no gaps. Fewer
/// texts than slots means fewer chunks; no slot waits on an empty range.
fn plan_chunks(jobs: usize, slots: usize) -> Vec<std::ops::Range<usize>> {
    if jobs == 0 || slots == 0 {
        return Vec::new();
    }
    let parts = jobs.min(slots).max(1);
    let base = jobs / parts;
    let extra = jobs % parts;
    let mut out = Vec::with_capacity(parts);
    let mut start = 0;
    for i in 0..parts {
        let len = base + usize::from(i < extra);
        out.push(start..start + len);
        start += len;
    }
    out
}

/// Encode a batch across the pool: one chunk per free slot at once,
/// reassembled in order. A one-chunk batch encodes exactly as before, so
/// single-slot hosts and lone texts never pay for a thread. A chunk that
/// fails fails the batch, the way one `encode_now` over the whole batch
/// would: a partial answer would silently thin the panel.
fn encode_spread(texts: &[String], query: bool) -> Option<Vec<Vec<f32>>> {
    let ranges = plan_chunks(texts.len(), spread_slots(query));
    if ranges.len() <= 1 {
        return encode_now(texts, query);
    }
    let mut out: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
    out.resize_with(texts.len(), Vec::new);
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(ranges.len());
        for range in &ranges {
            handles.push(scope.spawn(|| encode_now(&texts[range.clone()], query)));
        }
        for (range, handle) in ranges.iter().zip(handles) {
            match handle.join() {
                Ok(Some(vecs)) if vecs.len() == range.len() => {
                    out[range.clone()].clone_from_slice(&vecs);
                }
                _ => return None,
            }
        }
        Some(out)
    })
}

fn run_group(jobs: Vec<Pending>, query: bool) {
    if jobs.is_empty() {
        return;
    }
    let texts: Vec<String> = jobs.iter().map(|j| j.text.clone()).collect();
    // One chunk per free slot rather than the whole batch behind one: the
    // pump still coalesces arrivals, but a pooled host now spends its
    // extra models instead of warming only the first.
    let vecs = encode_spread(&texts, query);
    let mut answers = vecs.unwrap_or_default().into_iter();
    for job in jobs {
        let _ = job.tx.send(answers.next());
    }
}

/// How the last dense encode went: 0 none yet, 1 answered, 2 did not. A
/// binary on disk is not an encoder that answers; one killed for memory
/// leaves every search lexical until the next start succeeds.
static LAST_DENSE: AtomicU8 = AtomicU8::new(0);

/// Whether the last dense encode answered; none before the first.
#[must_use]
pub fn last_dense() -> Option<bool> {
    match LAST_DENSE.load(Ordering::Relaxed) {
        1 => Some(true),
        2 => Some(false),
        _ => None,
    }
}

fn encode_now(texts: &[String], query: bool) -> Option<Vec<Vec<f32>>> {
    let binary = binary()?;
    let mut held = lock_dense()?;
    for _ in 0..2 {
        if held.as_mut().is_none_or(|running| !running.alive()) {
            *held = Encoder::start(&binary, query);
        }
        let Some(running) = held.as_mut() else {
            break;
        };
        if let Some(vectors) = running.encode_batch(texts, query) {
            LAST_DENSE.store(1, Ordering::Relaxed);
            return Some(vectors);
        }
        *held = None;
    }
    LAST_DENSE.store(2, Ordering::Relaxed);
    None
}

pub fn encode(text: &str, query: bool) -> Option<Vec<f32>> {
    if text.trim().is_empty() {
        return None;
    }
    binary()?;
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

/// How long a search waits for a busy cross-encoder before it answers
/// without the second stage.
pub const RERANK_WAIT: std::time::Duration = std::time::Duration::from_millis(400);

/// The slot's lock, if it can be had within `wait`.
fn wait_for(
    slot: &Slot,
    wait: std::time::Duration,
) -> Option<std::sync::MutexGuard<'_, Option<Encoder>>> {
    let until = std::time::Instant::now() + wait;
    loop {
        match slot.try_lock() {
            Ok(guard) => return Some(guard),
            Err(std::sync::TryLockError::Poisoned(p)) => return Some(p.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => {
                if std::time::Instant::now() >= until {
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
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
    // One cross-encoder answers one question at a time. A search that finds
    // it busy for longer than a moment keeps its first-stage order and says
    // the stage was absent, rather than queue behind every other search: six
    // prompts at once made the last wait eight seconds for a two-second pass.
    let mut held = wait_for(rerank_slot(), RERANK_WAIT)?;
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

/// Held while a test points `PACKSET_EMBED` at a stub, so two tests cannot
/// hand each other a child.
#[cfg(test)]
pub(crate) static EMBED: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_busy_cross_encoder_is_skipped_after_a_short_wait() {
        let slot: Slot = Mutex::new(None);
        let held = slot.lock().unwrap();
        let started = std::time::Instant::now();
        assert!(wait_for(&slot, std::time::Duration::from_millis(50)).is_none());
        assert!(started.elapsed() >= std::time::Duration::from_millis(50));
        drop(held);
        assert!(wait_for(&slot, std::time::Duration::from_millis(50)).is_some());
    }

    /// A waiter takes whichever pool slot is free, not the first: with
    /// the first slot held, the poll answers from the second, and with
    /// both free it prefers the first. No threads, no timing.
    #[test]
    fn waiters_take_whichever_pool_slot_is_free() {
        let slots: [Slot; 2] = [Mutex::new(None), Mutex::new(None)];
        let held = slots[0].lock().unwrap();
        {
            let _guard = poll_slots(&slots).expect("the free slot answers");
            assert!(slots[1].try_lock().is_err(), "the poll took the held slot");
        }
        drop(held);
        {
            let _guard = poll_slots(&slots).expect("a slot answers");
            assert!(
                slots[0].try_lock().is_err(),
                "the poll skipped the free first slot"
            );
        }
        assert!(poll_slots(&[]).is_none());
    }

    /// A batch spreads over at most one chunk per slot: contiguous,
    /// gapless ranges covering every text exactly once, never more chunks
    /// than slots and never an empty one. No threads, no model.
    #[test]
    fn a_batch_spreads_over_at_most_one_chunk_per_slot() {
        assert!(plan_chunks(0, 4).is_empty());
        assert!(plan_chunks(5, 0).is_empty());
        assert_eq!(plan_chunks(1, 4), vec![0..1]);
        assert_eq!(plan_chunks(4, 2), vec![0..2, 2..4]);
        assert_eq!(plan_chunks(7, 3), vec![0..3, 3..5, 5..7]);
        let ranges = plan_chunks(10, 4);
        assert_eq!(ranges.len(), 4);
        let mut seen = [false; 10];
        for (i, range) in ranges.iter().enumerate() {
            if i > 0 {
                assert_eq!(range.start, ranges[i - 1].end, "a gap between chunks");
            }
            assert!(!range.is_empty(), "a slot waits on nothing");
            for j in range.clone() {
                assert!(!seen[j], "two chunks share index {j}");
                seen[j] = true;
            }
        }
        assert!(seen.iter().all(|seen| *seen), "a text has no chunk");
    }

    /// A document batch leaves a slot for a waiting query: with a query
    /// on the pump, documents spread over one slot fewer, and with the
    /// queue drained they spend the whole pool again. The queue is
    /// borrowed directly without notifying, so the pump stays asleep and
    /// there is no timing. Serialised on `EMBED` like the other tests
    /// that touch process-global encoder state.
    #[test]
    fn document_batches_leave_a_slot_for_a_waiting_query() {
        let _held = EMBED.lock().unwrap_or_else(|e| e.into_inner());
        let old_workers = std::env::var_os("PACKSET_EMBED_QUERY_WORKERS");
        // SAFETY: EMBED is held, so no other test reads this variable.
        unsafe { std::env::set_var("PACKSET_EMBED_QUERY_WORKERS", "2") };
        let slots = pool_slots().len().max(1);
        let (tx, _rx) = std::sync::mpsc::sync_channel(1);
        {
            let (lock, _) = pending();
            lock.lock().unwrap().push(Pending {
                text: "a waiting query".to_string(),
                query: true,
                tx,
            });
        }
        let throttled = spread_slots(false);
        {
            let (lock, _) = pending();
            lock.lock().unwrap().clear();
        }
        let free = spread_slots(false);
        let queries = spread_slots(true);
        unsafe {
            match old_workers {
                Some(v) => std::env::set_var("PACKSET_EMBED_QUERY_WORKERS", v),
                None => std::env::remove_var("PACKSET_EMBED_QUERY_WORKERS"),
            }
        }
        drop(_held);
        assert!(slots >= 2, "the pool this test spreads over has one slot");
        assert_eq!(throttled, slots - 1, "documents yield a slot to the query");
        assert_eq!(free, slots, "idle documents spend the pool");
        assert_eq!(queries, slots, "queries always spread");
    }

    /// An explicit encoder beats any kept discovery: even if a burst has
    /// already cached the tree lookup, naming `PACKSET_EMBED` answers
    /// from the variable without touching the cache. `/bin/false` never
    /// runs here; `binary` only checks it is executable.
    #[test]
    fn an_explicit_encoder_beats_any_cached_discovery() {
        let _held = EMBED.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var_os("PACKSET_EMBED");
        // SAFETY: EMBED is held, so no other test reads this variable.
        unsafe { std::env::set_var("PACKSET_EMBED", "/bin/false") };
        let found = binary();
        unsafe {
            match old {
                Some(v) => std::env::set_var("PACKSET_EMBED", v),
                None => std::env::remove_var("PACKSET_EMBED"),
            }
        }
        drop(_held);
        assert_eq!(found, Some(std::path::PathBuf::from("/bin/false")));
    }

    /// Discovery finds an encoder on the search path: a planted
    /// `packset-embed` in a directory prepended to `PATH` answers from
    /// `discover` directly, so the kept cache is never primed and no
    /// other test can observe the plant. The plant exits at once, so
    /// even a concurrent spawn would be harmless.
    #[test]
    fn discovery_finds_an_encoder_on_the_path() {
        let _held = EMBED.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let plant = dir.path().join("packset-embed");
        std::fs::write(&plant, "#!/bin/sh\nexit 1\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(&plant).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&plant, perm).unwrap();
        let old_path = std::env::var_os("PATH");
        let mut paths = vec![dir.path().to_path_buf()];
        if let Some(old) = &old_path {
            paths.extend(std::env::split_paths(old));
        }
        // SAFETY: EMBED is held, and every test that spawns a stub holds
        // it too; the plant itself needs no lookup to run.
        unsafe { std::env::set_var("PATH", std::env::join_paths(paths).unwrap()) };
        let found = discover();
        unsafe {
            match old_path {
                Some(v) => std::env::set_var("PATH", v),
                None => std::env::remove_var("PATH"),
            }
        }
        drop(_held);
        assert_eq!(found, Some(plant));
    }

    /// A pooled batch encodes on one child per free slot: four texts with
    /// two workers start two stub children, not one child twice. The
    /// stubs sleep before answering, so the second chunk cannot inherit
    /// the first slot before it is free; two distinct child PIDs are the
    /// assertion. No pump, no timing windows: `encode_spread` joins both
    /// chunks itself. Serialised on `EMBED` with the other stub tests,
    /// and the pool slots are cleared behind it.
    #[test]
    fn a_batch_encodes_on_one_child_per_free_slot() {
        let _held = EMBED.lock().unwrap_or_else(|e| e.into_inner());
        let old_workers = std::env::var_os("PACKSET_EMBED_QUERY_WORKERS");
        let old_embed = std::env::var_os("PACKSET_EMBED");
        // SAFETY: EMBED is held, so no other test reads these variables.
        unsafe { std::env::set_var("PACKSET_EMBED_QUERY_WORKERS", "2") };
        let dir = tempfile::tempdir().unwrap();
        let pids = dir.path().join("pids");
        let stub = dir.path().join("packset-embed");
        std::fs::write(
            &stub,
            format!(
                r#"#!/usr/bin/env python3
import json, os, pathlib, sys, time
log = pathlib.Path(r"{log}")
for line in sys.stdin:
    try:
        asked = json.loads(line)
    except json.JSONDecodeError:
        continue
    with log.open("a") as handle:
        handle.write(str(os.getpid()) + "\n")
    time.sleep(0.2)
    count = len(asked.get("texts", [])) or 1
    if "texts" in asked:
        print(json.dumps({{"vs": [[0.1]] * count}}), flush=True)
    else:
        print(json.dumps({{"v": [0.1]}}), flush=True)
"#,
                log = pids.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(&stub).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&stub, perm).unwrap();
        // SAFETY: EMBED is held, so no other test reads this variable.
        unsafe { std::env::set_var("PACKSET_EMBED", &stub) };
        for slot in pool_slots() {
            if let Ok(mut held) = slot.lock() {
                *held = None;
            }
        }
        let texts = vec![
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
        ];
        let vecs = encode_spread(&texts, true);
        for slot in pool_slots() {
            if let Ok(mut held) = slot.lock() {
                *held = None;
            }
        }
        unsafe {
            match old_workers {
                Some(v) => std::env::set_var("PACKSET_EMBED_QUERY_WORKERS", v),
                None => std::env::remove_var("PACKSET_EMBED_QUERY_WORKERS"),
            }
            match old_embed {
                Some(v) => std::env::set_var("PACKSET_EMBED", v),
                None => std::env::remove_var("PACKSET_EMBED"),
            }
        }
        drop(_held);
        let vecs = vecs.expect("the stubs answer");
        assert_eq!(vecs.len(), 4, "{vecs:?}");
        let lines = std::fs::read_to_string(&pids).unwrap();
        let mut ids: Vec<_> = lines.lines().collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 2, "one batch, two slots, two children: {lines}");
    }

    /// An encoder that is on disk but does not answer is reported as not
    /// answering, not as available.
    #[test]
    fn an_encoder_that_dies_is_recorded_as_not_answering() {
        let _held = EMBED.lock().unwrap_or_else(|e| e.into_inner());
        let before = std::env::var_os("PACKSET_EMBED");
        // SAFETY: EMBED serialises every test that points PACKSET_EMBED.
        unsafe { std::env::set_var("PACKSET_EMBED", "/bin/false") };
        if binary().is_some() {
            assert!(encode_now(&["a text".to_string()], false).is_none());
            assert_eq!(last_dense(), Some(false));
        }
        *dense_slot().lock().unwrap_or_else(|e| e.into_inner()) = None;
        unsafe {
            match before {
                Some(v) => std::env::set_var("PACKSET_EMBED", v),
                None => std::env::remove_var("PACKSET_EMBED"),
            }
        }
    }

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
