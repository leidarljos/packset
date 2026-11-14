//! The dense projection as a kept child process (`packset-embed`), one per
//! direction. An absent encoder is a supported state: every failure falls
//! back to the lexical scorers. `INSIDE_EMBED=off` and `--no-embed` are
//! that state on purpose: no child is started. A vector is derivable from
//! the text, never the store. The writer starts the child when it binds
//! its listener ([`warm_at_listen`]), so the first search is not the load.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};

/// Environment variables naming the encoder.
pub const BIN_VARS: &[&str] = &["PACKSET_EMBED"];

/// Whether this seat runs the encoder.
///
/// On unless the seat turns it off. `INSIDE_EMBED` set to `off`, `0`,
/// `none`, `false` or `no` is the environment form; `packsetd --no-embed`
/// is the flag. Those spellings are the ones the other host switches
/// already take (`PACKSET_STEM` takes `off`, `0`, `no` and `false`; the
/// caps take `off` and `none`), so a seat does not learn a second dialect
/// for this one variable. Unset means on: a pack that has an encoder keeps
/// the dense ballot.
#[must_use]
pub fn enabled() -> bool {
    let raw = std::env::var("INSIDE_EMBED").unwrap_or_else(|_| "on".into());
    !encoder_switched_off(no_embed_flag(), &raw)
}

/// `--no-embed` on this process. The arguments do not change after start.
fn no_embed_flag() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| std::env::args().any(|arg| arg == "--no-embed"))
}

/// The one switch, split so a test can name the flag without the process
/// having been started with it.
fn encoder_switched_off(no_embed_flag: bool, env_raw: &str) -> bool {
    no_embed_flag
        || matches!(
            env_raw.trim().to_ascii_lowercase().as_str(),
            "0" | "off" | "none" | "false" | "no"
        )
}

/// The encoder binary this seat would run: `PACKSET_EMBED`, else beside the
/// writer, else on `PATH`. An explicit variable always wins over the kept
/// discovery below, so a seat or test that names an encoder never sees
/// the cache. A seat that switched the encoder off has nothing to run:
/// this returns before either lookup, so no caller starts a child.
#[must_use]
pub fn binary() -> Option<PathBuf> {
    if !enabled() {
        return None;
    }
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
/// and the name on `PATH`. A binary found there does not move under a
/// running writer, so it is kept for good and a burst of searches does
/// not walk the filesystem per request. A miss is kept for
/// [`LOOK_AGAIN`]: a binary can be installed after the writer starts.
fn found_binary() -> Option<PathBuf> {
    static FOUND: Kept = Kept::new(LOOK_AGAIN);
    FOUND.get(discover)
}

/// How long a miss is kept before discovery walks again. A burst of
/// searches in one sitting shares one walk of `PATH`. Five seconds covers
/// that burst, and a binary installed during the sitting is found on the
/// next question.
pub const LOOK_AGAIN: Duration = Duration::from_secs(5);

/// The last answer, found or not, and when.
pub struct Kept {
    again: Duration,
    seen: Mutex<Option<(std::time::Instant, Option<PathBuf>)>>,
}

impl Kept {
    pub const fn new(again: Duration) -> Self {
        Self {
            again,
            seen: Mutex::new(None),
        }
    }

    /// The kept answer, or `discover`'s when there is none to keep.
    pub fn get(&self, discover: impl FnOnce() -> Option<PathBuf>) -> Option<PathBuf> {
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        match &*seen {
            Some((_, Some(found))) => return Some(found.clone()),
            Some((at, None)) if at.elapsed() < self.again => return None,
            _ => {}
        }
        let found = discover();
        *seen = Some((std::time::Instant::now(), found.clone()));
        found
    }
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
/// RAM. `PACKSET_EMBED_QUERY_WORKERS` used to raise it; a pool of two was
/// 4 GB on a laptop that also held a document encoder, and a cold start
/// under pressure spawned one child per search. Concurrent searches wait
/// on this one child. The variable is ignored.
fn query_workers() -> usize {
    1
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
    touch_embed();
    let mut held = lock_dense()?;
    for _ in 0..2 {
        // A child that is already up is not a load. Only a spawn is timed,
        // and only when it answers: that is the next cold start's budget.
        let cold = held.as_mut().is_none_or(|running| !running.alive());
        let started = std::time::Instant::now();
        if cold {
            *held = Encoder::start(&binary, query);
        }
        let Some(running) = held.as_mut() else {
            break;
        };
        if let Some(vectors) = running.encode_batch(texts, query) {
            if cold {
                remember_load(started.elapsed());
            }
            LAST_DENSE.store(1, Ordering::Relaxed);
            return Some(vectors);
        }
        *held = None;
    }
    // The child's stderr goes nowhere, so a runtime it cannot load would
    // otherwise leave every search lexical with nothing in the log. Say it
    // once per outage; the next encode that answers ends the outage.
    if LAST_DENSE.swap(2, Ordering::Relaxed) != 2 {
        eprintln!(
            "packsetd: the encoder {} did not answer; search is lexical until it does (run it by hand to see why)",
            binary.display()
        );
    }
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

/// A query vector when an encoder is up.
///
/// A down encoder is not restarted on this call: the search ranks by
/// words and a child is started aside. A live encoder that another
/// request holds is waited on, so the dense ballot is not dropped just
/// because the slot is busy.
#[must_use]
pub fn try_encode_query(text: &str) -> Option<Vec<f32>> {
    if text.trim().is_empty() || !enabled() {
        return None;
    }
    if LAST_DENSE.load(Ordering::Relaxed) == 2 {
        return None;
    }
    binary()?;
    let held = match dense_slot().try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            if LAST_DENSE.load(Ordering::Relaxed) != 1 {
                return None;
            }
            match dense_slot().lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            }
        }
    };
    encode_if_up(held, text)
}

/// Encode with `held` when the child is alive. Otherwise leave this
/// request and, if a child has never come up, start one aside.
fn encode_if_up(
    mut held: std::sync::MutexGuard<'static, Option<Encoder>>,
    text: &str,
) -> Option<Vec<f32>> {
    let alive = held.as_mut().is_some_and(|running| running.alive());
    if !alive {
        let failed = held.is_some();
        *held = None;
        drop(held);
        if failed {
            if LAST_DENSE.swap(2, Ordering::Relaxed) != 2 {
                eprintln!(
                    "packsetd: the encoder is down; search is ranked by words until it answers"
                );
            }
            return None;
        }
        if binary().is_some() {
            warm_in_background();
        }
        return None;
    }
    let owned = text.to_string();
    match held
        .as_mut()?
        .encode_batch(std::slice::from_ref(&owned), true)
    {
        Some(mut vectors) => {
            LAST_DENSE.store(1, Ordering::Relaxed);
            touch_embed();
            vectors.pop()
        }
        None => {
            *held = None;
            LAST_DENSE.store(2, Ordering::Relaxed);
            None
        }
    }
}

/// One warm at a time. Cleared when that warm's thread ends, including
/// when it panics, so a later search can start the encoder again.
static STARTING: AtomicBool = AtomicBool::new(false);
/// Unix milliseconds when the current load began. Zero means it is not
/// loading: not started, already answered, or it failed.
static LOAD_STARTED_MS: AtomicU64 = AtomicU64::new(0);
/// Last cold-start budget, in milliseconds. Zero means it has not been
/// read yet. A positive value is either [`UNMEASURED_LOAD_MS`] or the
/// number in the cache file.
static LOAD_BUDGET_MS: AtomicU64 = AtomicU64::new(0);

/// Budget used until this seat has recorded a cold start.
///
/// One second. It is a ceiling for an encoder that has not been timed
/// here, not a measurement of this load. The first cold start that
/// answers writes its own milliseconds over it.
pub const UNMEASURED_LOAD_MS: u64 = 1_000;

/// Clears the warm flag when the start thread ends.
struct StopWarm;

impl Drop for StopWarm {
    fn drop(&mut self) {
        LOAD_STARTED_MS.store(0, Ordering::Release);
        STARTING.store(false, Ordering::Release);
    }
}

fn mark_loading() {
    LOAD_STARTED_MS.store(unix_ms().max(1), Ordering::Release);
}

/// The embed cache, matching `packset-embed`: `PACKSET_EMBED_CACHE`, else
/// `$XDG_CACHE_HOME/packset/embed`, else `~/.cache/packset/embed`.
fn embed_cache_dir() -> Option<PathBuf> {
    if let Some(named) = std::env::var_os("PACKSET_EMBED_CACHE").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(named));
    }
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(xdg).join("packset").join("embed"));
    }
    let home = std::env::var_os("HOME").filter(|d| !d.is_empty())?;
    Some(
        PathBuf::from(home)
            .join(".cache")
            .join("packset")
            .join("embed"),
    )
}

fn load_file() -> Option<PathBuf> {
    Some(embed_cache_dir()?.join("load-ms"))
}

/// Milliseconds in `load-ms`, when the file holds a positive integer.
fn read_load_file() -> Option<u64> {
    let text = std::fs::read_to_string(load_file()?).ok()?;
    let ms: u64 = text.trim().parse().ok()?;
    (ms > 0).then_some(ms)
}

fn load_budget_ms() -> u64 {
    let cached = LOAD_BUDGET_MS.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }
    let ms = read_load_file().unwrap_or(UNMEASURED_LOAD_MS);
    match LOAD_BUDGET_MS.compare_exchange(0, ms, Ordering::Relaxed, Ordering::Relaxed) {
        Ok(_) => ms,
        Err(current) => current,
    }
}

/// Remember a cold start that answered. The next load estimates from this.
///
/// The file is one decimal integer and a newline, replaced by rename so a
/// reader never sees a partial write. A unit test that has not named a
/// cache does not write one: the file belongs to a seat.
fn remember_load(elapsed: Duration) {
    let ms = u64::try_from(elapsed.as_millis())
        .unwrap_or(u64::MAX)
        .max(1);
    LOAD_BUDGET_MS.store(ms, Ordering::Relaxed);
    #[cfg(test)]
    if std::env::var_os("PACKSET_EMBED_CACHE").is_none() {
        return;
    }
    let Some(path) = load_file() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, format!("{ms}\n")).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// `budget` minus the time already spent, or zero when the budget is used up.
fn estimate_remaining(budget_ms: u64, elapsed_ms: u64) -> u64 {
    budget_ms.saturating_sub(elapsed_ms)
}

/// Estimated milliseconds left in the encoder load, while it is running
/// and has not answered.
///
/// The budget is the last cold start that answered, read from `load-ms`
/// in the embed cache: the milliseconds from spawning the encoder to its
/// first vector. The estimate is that budget minus the time since this
/// load began, and zero when the budget is already spent. With no file
/// yet, the budget is [`UNMEASURED_LOAD_MS`], a ceiling, not a measurement.
///
/// `None` once the encoder has answered, once it has failed, and when no
/// load is running. A missing binary never starts a load.
#[must_use]
pub fn remaining_ms() -> Option<u64> {
    if last_dense().is_some() {
        return None;
    }
    let started = LOAD_STARTED_MS.load(Ordering::Acquire);
    if started == 0 {
        return None;
    }
    let elapsed = unix_ms().saturating_sub(started);
    Some(estimate_remaining(load_budget_ms(), elapsed))
}

/// Start the query encoder now that the listener is bound.
///
/// Returns at once. The model load runs on `packset-embed-start`, so the
/// accept loop and `/health` do not wait. The encoder switched off starts
/// nothing. A missing binary starts nothing and is not logged: search
/// stays lexical. A binary that does not answer is logged once, from the
/// warm, and search stays lexical.
pub fn warm_at_listen() {
    if !enabled() {
        return;
    }
    warm_in_background();
}

/// Start the encoder off the request that asked. One start at a time.
/// A failure stays recorded, so the next search does not spawn again.
fn warm_in_background() {
    if STARTING.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("packset-embed-start".into())
        .spawn(|| {
            let _done = StopWarm;
            if binary().is_none() {
                return;
            }
            mark_loading();
            let _ = encode_now(std::slice::from_ref(&".".to_string()), true);
        });
    if spawned.is_err() {
        STARTING.store(false, Ordering::Release);
    }
}

/// Minutes before an idle encoder child is stopped. `u64::MAX` reads
/// [`PACKSET_EMBED_IDLE_MIN`].
static IDLE_MINUTES: AtomicU64 = AtomicU64::new(u64::MAX);
/// Unix milliseconds of the last encode or rerank. Zero means none yet.
static LAST_EMBED: AtomicU64 = AtomicU64::new(0);

/// `minutes` is how long an encoder may sit unused. Zero leaves it up.
pub fn set_idle_minutes(minutes: u64) {
    IDLE_MINUTES.store(minutes, Ordering::Relaxed);
}

/// How long an encoder child may sit with no call.
///
/// `PACKSET_EMBED_IDLE_MIN` overrides the five minute default. `off` and
/// `none` leave the child up.
#[must_use]
pub fn idle_limit() -> Duration {
    let set = IDLE_MINUTES.load(Ordering::Relaxed);
    if set != u64::MAX {
        return Duration::from_secs(set.saturating_mul(60));
    }
    match std::env::var("PACKSET_EMBED_IDLE_MIN") {
        Ok(raw) => match raw.trim() {
            "off" | "none" => Duration::ZERO,
            other => other
                .parse::<u64>()
                .map(|minutes| Duration::from_secs(minutes.saturating_mul(60)))
                .unwrap_or(Duration::from_secs(5 * 60)),
        },
        Err(_) => Duration::from_secs(5 * 60),
    }
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn touch_embed() {
    LAST_EMBED.store(unix_ms().max(1), Ordering::Relaxed);
    ensure_idle_watch();
}

fn ensure_idle_watch() {
    static START: OnceLock<()> = OnceLock::new();
    START.get_or_init(|| {
        let _ = std::thread::Builder::new()
            .name("packset-embed-idle".into())
            .spawn(|| loop {
                std::thread::sleep(Duration::from_secs(30));
                let _ = reap_if_older_than(idle_limit());
            });
    });
}

/// Stop the encoder children when the last call is older than `limit`.
///
/// The next encode starts a child again. A limit of zero does nothing.
/// The clock is read again after the slot is taken, so a call that arrived
/// while this waited is not stopped.
#[must_use]
pub fn reap_if_older_than(limit: Duration) -> bool {
    if limit.is_zero() {
        return false;
    }
    let limit_ms = u64::try_from(limit.as_millis()).unwrap_or(u64::MAX);
    let last = LAST_EMBED.load(Ordering::Relaxed);
    if last == 0 || unix_ms().saturating_sub(last) < limit_ms {
        return false;
    }
    let mut dense = match dense_slot().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let last = LAST_EMBED.load(Ordering::Relaxed);
    if last == 0 || unix_ms().saturating_sub(last) < limit_ms {
        return false;
    }
    *dense = None;
    drop(dense);
    for slot in [rerank_slot(), late_slot(), sparse_slot()] {
        match slot.lock() {
            Ok(mut held) => *held = None,
            Err(poisoned) => *poisoned.into_inner() = None,
        }
    }
    LAST_DENSE.store(0, Ordering::Relaxed);
    LAST_EMBED.store(0, Ordering::Relaxed);
    true
}

#[cfg(test)]
fn dense_child_alive() -> bool {
    match dense_slot().lock() {
        Ok(mut held) => held.as_mut().is_some_and(|child| child.alive()),
        Err(poisoned) => poisoned
            .into_inner()
            .as_mut()
            .is_some_and(|child| child.alive()),
    }
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

/// How deep the second stage reads. LoCoMo's published cut-offs are 1, 5,
/// 10 and 20, and 20 is the last of them (`CUTOFFS` in the locomo example).
/// Each candidate is a forward pass, so the stage stops where that table
/// stops.
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
    let mut out: Vec<Value> = head
        .into_iter()
        .map(|(score, mut hit)| {
            if let Some(object) = hit.as_object_mut() {
                object.insert("rerank_score".into(), json!(score));
            }
            hit
        })
        .collect();
    // The tail was not scored. The field is null there, same as a hit
    // from a search that did not run the stage.
    out.extend(hits[depth..].iter().cloned().map(|mut hit| {
        if let Some(object) = hit.as_object_mut() {
            object.insert("rerank_score".into(), Value::Null);
        }
        hit
    }));
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
/// without the second stage. A staged query on loopback was p50 244 ms
/// (`docs/orgmode/explanation.org`, 2026-10-08). 400 ms sits above that, so
/// a pass already running usually returns inside the wait, and the search
/// then answers with the first-stage order. Six prompts at once made the
/// last wait eight seconds for a two-second pass when each stood behind
/// the one before it.
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
    // The cross-encoder is the same child as the dense ballot. The switch
    // that leaves the encoder unstarted leaves this stage unstarted too.
    if !enabled() {
        return None;
    }
    let binary = binary()?;
    touch_embed();
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
    LAST_DENSE.store(0, Ordering::Relaxed);
    LOAD_STARTED_MS.store(0, Ordering::Relaxed);
    LOAD_BUDGET_MS.store(0, Ordering::Relaxed);
    STARTING.store(false, Ordering::Relaxed);
    for slot in [dense_slot(), rerank_slot(), late_slot(), sparse_slot()] {
        if let Ok(mut held) = slot.lock() {
            *held = None;
        }
    }
}

/// A load already underway, for a status test. `budget_ms` of zero means
/// the next read comes from the cache file again.
#[cfg(test)]
pub fn set_load_for_test(started_unix_ms: u64, budget_ms: u64) {
    LOAD_STARTED_MS.store(started_unix_ms, Ordering::Release);
    LOAD_BUDGET_MS.store(budget_ms, Ordering::Relaxed);
}

/// What the last dense call did, for a status test.
#[cfg(test)]
pub fn set_dense_for_test(answered: Option<bool>) {
    let value = match answered {
        Some(true) => 1,
        Some(false) => 2,
        None => 0,
    };
    LAST_DENSE.store(value, Ordering::Relaxed);
}

/// Held while a test points `PACKSET_EMBED` at a stub, so two tests cannot
/// hand each other a child.
#[cfg(test)]
pub(crate) static EMBED: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_down_encoder_is_not_asked_again() {
        let _held = EMBED.lock().unwrap_or_else(|err| err.into_inner());
        let previous = LAST_DENSE.swap(2, Ordering::Relaxed);
        struct Restore(u8);
        impl Drop for Restore {
            fn drop(&mut self) {
                LAST_DENSE.store(self.0, Ordering::Relaxed);
            }
        }
        let _restore = Restore(previous);
        let started = std::time::Instant::now();
        assert!(try_encode_query("a question the encoder must not see").is_none());
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "a down encoder was waited on: {:?}",
            started.elapsed()
        );
    }

    /// The child is stopped after it has sat unused, and another can be
    /// started for the next call. The limit is the argument, not the
    /// process default. The stub is handed to the slot directly, so the
    /// test does not retarget `PACKSET_EMBED` for every other test.
    #[test]
    fn an_idle_encoder_stops_and_the_next_call_starts_it() {
        let _held = EMBED.lock().unwrap_or_else(|err| err.into_inner());
        reset_for_test();
        let previous = LAST_DENSE.load(Ordering::Relaxed);
        struct Restore(u8);
        impl Drop for Restore {
            fn drop(&mut self) {
                reset_for_test();
                LAST_EMBED.store(0, Ordering::Relaxed);
                LAST_DENSE.store(self.0, Ordering::Relaxed);
            }
        }
        let _restore = Restore(previous);
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("packset-embed");
        std::fs::write(
            &stub,
            r#"#!/usr/bin/env python3
import json, sys
for line in sys.stdin:
    print(json.dumps({"v": [0.25, 0.5]}), flush=True)
"#,
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(&stub).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&stub, perm).unwrap();
        let started = Encoder::start(&stub, true).expect("the stub starts");
        {
            let mut slot = dense_slot().lock().unwrap_or_else(|err| err.into_inner());
            *slot = Some(started);
        }
        {
            let mut slot = dense_slot().lock().unwrap_or_else(|err| err.into_inner());
            let vectors = slot
                .as_mut()
                .expect("the encoder was not installed")
                .encode_batch(&["a sentence".into()], true);
            assert_eq!(vectors, Some(vec![vec![0.25, 0.5]]));
        }
        assert!(dense_child_alive(), "the encoder was not started");
        LAST_EMBED.store(1, Ordering::Relaxed);
        assert!(reap_if_older_than(Duration::from_secs(60)));
        assert!(!dense_child_alive(), "the idle encoder was left up");
        let mut again = Encoder::start(&stub, true).expect("the stub starts again");
        let vectors = again.encode_batch(&["a sentence".into()], true);
        assert_eq!(vectors, Some(vec![vec![0.25, 0.5]]));
    }

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

    /// A found binary is kept for good; a miss, only until the wait runs out.
    #[test]
    fn a_found_binary_is_kept_and_a_miss_is_looked_for_again() {
        let looked = std::cell::Cell::new(0);
        let missing = || {
            looked.set(looked.get() + 1);
            None
        };
        let slow = Kept::new(Duration::from_secs(60));
        assert_eq!(slow.get(missing), None);
        assert_eq!(slow.get(missing), None);
        assert_eq!(
            looked.get(),
            1,
            "a miss was looked for again inside the wait"
        );

        let quick = Kept::new(Duration::ZERO);
        assert_eq!(quick.get(|| None), None);
        let installed = PathBuf::from("/opt/packset-embed");
        assert_eq!(
            quick.get(|| Some(installed.clone())),
            Some(installed.clone())
        );
        assert_eq!(
            quick.get(|| panic!("a found binary was looked for again")),
            Some(installed)
        );
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

    /// One encoder, whatever `PACKSET_EMBED_QUERY_WORKERS` says. Documents
    /// and queries share that child, so a cold start cannot load the model
    /// twice. The queue is borrowed directly without notifying, so the
    /// pump stays asleep.
    #[test]
    fn one_encoder_is_shared_by_documents_and_queries() {
        let _held = EMBED.lock().unwrap_or_else(|e| e.into_inner());
        let old_workers = std::env::var_os("PACKSET_EMBED_QUERY_WORKERS");
        // SAFETY: EMBED is held, so no other test reads this variable.
        unsafe { std::env::set_var("PACKSET_EMBED_QUERY_WORKERS", "4") };
        let slots = pool_slots().len();
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
        assert_eq!(slots, 1, "a second encoder was kept");
        assert_eq!(query_workers(), 1);
        assert_eq!(throttled, 1);
        assert_eq!(free, 1);
        assert_eq!(queries, 1);
    }

    /// An explicit encoder beats any kept discovery: even if a burst has
    /// already cached the tree lookup, naming `PACKSET_EMBED` answers
    /// from the variable without touching the cache. `false` never runs
    /// here; `binary` only checks it is executable. It is taken from
    /// `PATH` because macOS keeps it in `/usr/bin` and has no `/bin/false`.
    #[test]
    fn an_explicit_encoder_beats_any_cached_discovery() {
        let _held = EMBED.lock().unwrap_or_else(|e| e.into_inner());
        let program = which("false").expect("false is on PATH");
        let old = std::env::var_os("PACKSET_EMBED");
        // SAFETY: EMBED is held, so no other test reads this variable.
        unsafe { std::env::set_var("PACKSET_EMBED", &program) };
        let found = binary();
        unsafe {
            match old {
                Some(v) => std::env::set_var("PACKSET_EMBED", v),
                None => std::env::remove_var("PACKSET_EMBED"),
            }
        }
        drop(_held);
        assert_eq!(found, Some(program));
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

    /// Concurrent cold starts share one child. Four searches ask at once,
    /// the stub sleeps before it answers, and one pid is written. A second
    /// child would be a second model. Serialised on `EMBED`.
    #[test]
    fn concurrent_cold_starts_share_one_encoder() {
        let _held = EMBED.lock().unwrap_or_else(|e| e.into_inner());
        let old_embed = std::env::var_os("PACKSET_EMBED");
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
    text = asked.get("text") or ""
    if text.startswith("spread "):
        with log.open("a") as handle:
            handle.write(str(os.getpid()) + "\n")
        time.sleep(0.2)
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
        *dense_slot().lock().unwrap_or_else(|e| e.into_inner()) = None;
        LAST_DENSE.store(0, Ordering::Relaxed);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
        let mut handles = Vec::new();
        for i in 0..4 {
            let barrier = std::sync::Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                encode_now(&[format!("spread {i}")], true)
            }));
        }
        let mut answered = 0usize;
        for handle in handles {
            if handle.join().unwrap().is_some() {
                answered += 1;
            }
        }
        *dense_slot().lock().unwrap_or_else(|e| e.into_inner()) = None;
        unsafe {
            match old_embed {
                Some(v) => std::env::set_var("PACKSET_EMBED", v),
                None => std::env::remove_var("PACKSET_EMBED"),
            }
        }
        drop(_held);
        assert_eq!(answered, 4, "a search was not answered");
        let lines = std::fs::read_to_string(&pids).unwrap();
        let mut ids: Vec<_> = lines.lines().collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(
            ids.len(),
            1,
            "cold starts spawned more than one encoder: {lines}"
        );
    }

    /// An encoder that is on disk but does not answer is reported as not
    /// answering, not as available.
    #[test]
    fn an_encoder_that_dies_is_recorded_as_not_answering() {
        let _held = EMBED.lock().unwrap_or_else(|e| e.into_inner());
        let program = which("false").expect("false is on PATH");
        let before = std::env::var_os("PACKSET_EMBED");
        // SAFETY: EMBED serialises every test that points PACKSET_EMBED.
        unsafe { std::env::set_var("PACKSET_EMBED", &program) };
        // A test that does not hold EMBED can leave a stub child here while
        // another test names the stub; that child would answer for this one.
        *dense_slot().lock().unwrap_or_else(|e| e.into_inner()) = None;
        let named = binary();
        let encoded = encode_now(&["a text".to_string()], false);
        let dense = last_dense();
        *dense_slot().lock().unwrap_or_else(|e| e.into_inner()) = None;
        unsafe {
            match before {
                Some(v) => std::env::set_var("PACKSET_EMBED", v),
                None => std::env::remove_var("PACKSET_EMBED"),
            }
        }
        drop(_held);
        assert_eq!(named, Some(program));
        assert!(encoded.is_none());
        assert_eq!(dense, Some(false));
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

    /// `INSIDE_EMBED=off` and `--no-embed` are one switch. The off
    /// spellings are the ones the other host switches already take.
    /// An empty value and `on` leave the encoder on, which is the
    /// unset default.
    #[test]
    fn the_encoder_switch_is_the_flag_or_the_off_spellings() {
        assert!(encoder_switched_off(true, "on"));
        assert!(encoder_switched_off(true, "yes"));
        assert!(encoder_switched_off(false, "off"));
        assert!(encoder_switched_off(false, "OFF"));
        assert!(encoder_switched_off(false, " 0 "));
        assert!(encoder_switched_off(false, "none"));
        assert!(encoder_switched_off(false, "false"));
        assert!(encoder_switched_off(false, "no"));
        assert!(!encoder_switched_off(false, "on"));
        assert!(!encoder_switched_off(false, ""));
        assert!(!encoder_switched_off(false, "yes"));
    }

    /// A named encoder that would answer is not started when the switch
    /// is off, and a question with candidates does not start the
    /// cross-encoder either. Status reads the same switch through
    /// `enabled`.
    #[test]
    fn a_switched_off_encoder_is_not_started() {
        let _held = EMBED.lock().unwrap_or_else(|e| e.into_inner());
        let old_embed = std::env::var_os("PACKSET_EMBED");
        let old_inside = std::env::var_os("INSIDE_EMBED");
        let dir = tempfile::tempdir().unwrap();
        let sentinel = dir.path().join("started");
        let stub = dir.path().join("stub-embed");
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\nprintf started > '{}'\nexit 1\n",
                sentinel.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(&stub).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&stub, perm).unwrap();
        // SAFETY: EMBED is held, so no other test reads these variables.
        unsafe {
            std::env::set_var("PACKSET_EMBED", &stub);
            std::env::set_var("INSIDE_EMBED", "off");
        }
        reset_for_test();
        let encoded = encode_query("a question the encoder must not see");
        let scored = rerank("a question", &["one claim".to_string()]);
        let on = enabled();
        unsafe {
            match old_embed {
                Some(value) => std::env::set_var("PACKSET_EMBED", value),
                None => std::env::remove_var("PACKSET_EMBED"),
            }
            match old_inside {
                Some(value) => std::env::set_var("INSIDE_EMBED", value),
                None => std::env::remove_var("INSIDE_EMBED"),
            }
        }
        reset_for_test();
        drop(_held);
        assert!(!on, "status would read the encoder as on");
        assert!(encoded.is_none());
        assert!(scored.is_none());
        assert!(
            !sentinel.exists(),
            "the encoder child was started with the switch off"
        );
    }

    /// Listening does not start a child the seat switched off, and status
    /// has nothing to count down.
    #[test]
    fn a_switched_off_encoder_is_not_warmed_at_listen() {
        let _held = EMBED.lock().unwrap_or_else(|e| e.into_inner());
        let old_embed = std::env::var_os("PACKSET_EMBED");
        let old_inside = std::env::var_os("INSIDE_EMBED");
        let dir = tempfile::tempdir().unwrap();
        let sentinel = dir.path().join("started");
        let stub = dir.path().join("stub-embed");
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\nprintf started > '{}'\nexit 1\n",
                sentinel.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(&stub).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&stub, perm).unwrap();
        unsafe {
            std::env::set_var("PACKSET_EMBED", &stub);
            std::env::set_var("INSIDE_EMBED", "off");
        }
        reset_for_test();
        warm_at_listen();
        std::thread::sleep(Duration::from_millis(50));
        let left = remaining_ms();
        let started = sentinel.exists();
        unsafe {
            match old_embed {
                Some(value) => std::env::set_var("PACKSET_EMBED", value),
                None => std::env::remove_var("PACKSET_EMBED"),
            }
            match old_inside {
                Some(value) => std::env::set_var("INSIDE_EMBED", value),
                None => std::env::remove_var("INSIDE_EMBED"),
            }
        }
        reset_for_test();
        drop(_held);
        assert!(
            left.is_none(),
            "a switched-off encoder was loading: {left:?}"
        );
        assert!(
            !started,
            "the encoder child was started with the switch off"
        );
    }

    /// The countdown is the recorded cold start minus the time already
    /// spent. Zero once that budget is gone. The file is what the next
    /// process reads; with none, the budget is the documented ceiling.
    #[test]
    fn remaining_time_is_the_recorded_cold_start_minus_elapsed() {
        assert_eq!(estimate_remaining(1_000, 400), 600);
        assert_eq!(estimate_remaining(1_000, 1_000), 0);
        assert_eq!(estimate_remaining(1_000, 1_500), 0);

        let _held = EMBED.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let old_cache = std::env::var_os("PACKSET_EMBED_CACHE");
        unsafe { std::env::set_var("PACKSET_EMBED_CACHE", dir.path()) };
        reset_for_test();
        let unmeasured = load_budget_ms();
        std::fs::write(dir.path().join("load-ms"), "400\n").unwrap();
        LOAD_BUDGET_MS.store(0, Ordering::Relaxed);
        let recorded = load_budget_ms();
        std::fs::write(dir.path().join("load-ms"), "nope\n").unwrap();
        LOAD_BUDGET_MS.store(0, Ordering::Relaxed);
        let garbage = load_budget_ms();
        remember_load(Duration::from_millis(250));
        LOAD_BUDGET_MS.store(0, Ordering::Relaxed);
        let written = load_budget_ms();
        let file = std::fs::read_to_string(dir.path().join("load-ms")).unwrap();
        unsafe {
            match old_cache {
                Some(value) => std::env::set_var("PACKSET_EMBED_CACHE", value),
                None => std::env::remove_var("PACKSET_EMBED_CACHE"),
            }
        }
        reset_for_test();
        drop(_held);
        assert_eq!(unmeasured, UNMEASURED_LOAD_MS);
        assert_eq!(recorded, 400);
        assert_eq!(garbage, UNMEASURED_LOAD_MS);
        assert_eq!(written, 250);
        assert_eq!(file.trim(), "250");
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
        assert!((ranked[0]["rerank_score"].as_f64().unwrap() - 0.9).abs() < 1e-6);
        assert!((ranked[1]["rerank_score"].as_f64().unwrap() - 0.1).abs() < 1e-6);
        assert!(ranked[20]["rerank_score"].is_null());
        assert!(ranked[21]["rerank_score"].is_null());
    }

    #[test]
    fn a_tie_keeps_the_first_stage_order() {
        let hits = vec![hit("first", "a"), hit("second", "b")];
        let ranked = apply_rerank(&hits, &[0.5, 0.5]).expect("length matches");
        assert_eq!(ranked[0]["id"], json!("first"));
        assert_eq!(ranked[1]["id"], json!("second"));
        assert_eq!(ranked[0]["rerank_score"], json!(0.5));
        assert_eq!(ranked[1]["rerank_score"], json!(0.5));
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
