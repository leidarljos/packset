//! Atoms in LMDB: key `workspace\0id`, value the record as JSON. A seat's
//! existing `memory.lmdb` opens here unchanged; the NUL makes a workspace
//! scan a prefix scan.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use heed::types::Bytes;
use heed::{Database, Env, EnvOpenOptions};
use packset_core::bm25::Index;
use packset_core::record::{self, AtomError};
use serde_json::{Map, Value};

/// The map writers before this one opened every pack with. A pack can
/// still be opened with it beside the writer: the writer takes the
/// environment this process opened first, and grows its map when a write
/// needs room.
pub const MAP_SIZE: usize = 1024 * 1024 * 1024;

/// The map a pack with nothing on disk opens with: LMDB's own default,
/// `DEFAULT_MAPSIZE` in `mdb.c`.
///
/// The data file holds the pages in use and extends as writes need new
/// pages, up to the map. It is not extended to the map at open: doing
/// that reserved a gibibyte per seat, and a file extended that far came
/// back fully allocated. `ls -ls` reported 1048580 blocks for a store
/// holding one or two claims.
const MAP_FLOOR: usize = 1 << 20;

/// The map at open is the data file times this, and a write that finds
/// the map full grows it by this and runs again. Doubling keeps the
/// remaps logarithmic in the pack: a gibibyte pack remaps ten times from
/// the floor in its life. The writes retried across those remaps redo
/// fewer bytes than the pack holds, the geometric sum, and once the pack
/// is past the floor the address space reserved past the data is never
/// more than the data. A remap waits for every open transaction, so a
/// smaller factor waits more often.
const MAP_GROWTH: usize = 2;

/// The map is kept a multiple of this: the largest page size LMDB meets,
/// arm64 and ppc64 kernels built with 64 KiB pages. It is a multiple of
/// the 4 KiB and 16 KiB pages elsewhere, which is what a resize requires.
const MAP_ALIGN: usize = 64 * 1024;

/// The map to open the data file in `dir` with: [`MAP_GROWTH`] times its
/// size on disk, or [`MAP_FLOOR`] when that is larger.
fn map_for(dir: &Path) -> usize {
    let on_disk = fs::metadata(dir.join("data.mdb"))
        .map(|meta| usize::try_from(meta.len()).unwrap_or(usize::MAX))
        .unwrap_or(0);
    on_disk
        .saturating_mul(MAP_GROWTH)
        .max(MAP_FLOOR)
        .div_ceil(MAP_ALIGN)
        .saturating_mul(MAP_ALIGN)
}

/// Whether a write failed because the map had no room for it.
fn is_map_full(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<heed::Error>(),
        Some(heed::Error::Mdb(heed::MdbError::MapFull))
    )
}

/// Cached atoms, indexes and snapshots, in bytes, when `PACKSET_CACHE_MB`
/// is unset. 128 MiB is the whole of those copies, not the process. A
/// workspace that was just read is kept even when it alone is larger.
pub const DEFAULT_CACHE_BYTES: usize = 128 * 1024 * 1024;

/// Parsed workspaces kept at once when `PACKSET_WORKSPACE_CACHE` is unset.
/// Each one holds its live claims and the index over them. A dense vector
/// is one copy of floats beside the record, not a JSON array on both
/// copies. A seat that opens a checkout a day would otherwise keep every
/// one of them until the process ends.
pub const DEFAULT_WORKSPACE_CACHE: usize = 8;

/// How many workspaces stay parsed. `off` and `none` keep them all, which
/// is what a pack did before the cap.
fn workspace_cache_from_env() -> usize {
    match std::env::var("PACKSET_WORKSPACE_CACHE") {
        Ok(raw) => match raw.trim() {
            "" => DEFAULT_WORKSPACE_CACHE,
            "off" | "none" => 0,
            n => n.parse().unwrap_or(DEFAULT_WORKSPACE_CACHE),
        },
        Err(_) => DEFAULT_WORKSPACE_CACHE,
    }
}

/// The byte cap. `off`, `none` and `0` keep every workspace the count cap
/// allows. Anything else is a number of mebibytes.
fn cache_limit_from_env() -> usize {
    match std::env::var("PACKSET_CACHE_MB") {
        Ok(raw) => match raw.trim() {
            "" => DEFAULT_CACHE_BYTES,
            "off" | "none" | "0" => 0,
            n => n
                .parse::<usize>()
                .map(|mebibytes| mebibytes.saturating_mul(1024 * 1024))
                .unwrap_or(DEFAULT_CACHE_BYTES),
        },
        Err(_) => DEFAULT_CACHE_BYTES,
    }
}

fn value_bytes(value: &Value) -> usize {
    match value {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Number(number) => number.to_string().len(),
        Value::String(text) => text.len(),
        Value::Array(items) => items.iter().fold(items.len() * 8, |sum, item| {
            sum.saturating_add(value_bytes(item))
        }),
        Value::Object(map) => map.iter().fold(0usize, |sum, (key, item)| {
            sum.saturating_add(key.len())
                .saturating_add(16)
                .saturating_add(value_bytes(item))
        }),
    }
}

pub(crate) fn record_bytes(record: &Record) -> usize {
    record.iter().fold(32, |sum, (key, value)| {
        sum.saturating_add(key.len())
            .saturating_add(16)
            .saturating_add(value_bytes(value))
    })
}

fn snapshot_bytes(snap: &Snapshot) -> usize {
    let stored = snap.stored.iter().fold(0usize, |sum, record| {
        sum.saturating_add(record_bytes(record))
    });
    let shown = snap.shown.iter().fold(0usize, |sum, record| {
        sum.saturating_add(record_bytes(record))
    });
    let vectors = snap.vectors.values().fold(0usize, |sum, vector| {
        sum.saturating_add(vector.len().saturating_mul(4))
            .saturating_add(32)
    });
    let originals = snap.originals.values().fold(0usize, |sum, numbers| {
        sum.saturating_add(numbers.len().saturating_mul(24))
            .saturating_add(32)
    });
    stored
        .saturating_add(shown)
        .saturating_add(vectors)
        .saturating_add(originals)
}

fn documents_bytes(documents: &[Vec<String>]) -> usize {
    documents.iter().fold(0usize, |sum, document| {
        sum.saturating_add(24)
            .saturating_add(document.iter().fold(0usize, |inner, token| {
                inner.saturating_add(token.len()).saturating_add(24)
            }))
    })
}

fn searchable_bytes(set: &Searchable) -> usize {
    let (_, (fingerprint, index, documents)) = set;
    let names = fingerprint.iter().fold(0usize, |sum, (id, hash)| {
        sum.saturating_add(id.len())
            .saturating_add(hash.len())
            .saturating_add(16)
    });
    names
        .saturating_add(index.resident_bytes())
        .saturating_add(documents_bytes(documents))
}

/// One atom record.
pub type Record = Map<String, Value>;

/// One workspace's live set at a write count. `stored` is what a write
/// folds into. `shown` is that set with links narrowed to ids present.
/// `vectors` is the one copy of each atom's embedding. The records here
/// omit the key, so a null a client sent still means clear.
struct Snapshot {
    generation: u64,
    stored: Vec<Record>,
    shown: Live,
    dangling: HashSet<String>,
    vectors: Arc<HashMap<String, Arc<[f32]>>>,
    /// The JSON numbers the client stored. Answers copy these. The floats
    /// are for scoring, and rebuilding the array from them is not the
    /// value that was written.
    originals: Arc<HashMap<String, Arc<[serde_json::Number]>>>,
}

/// A workspace's live set, one `Arc` per record.
pub type Live = Arc<Vec<Arc<Record>>>;

/// Records and the vectors that belong to them, from one read.
///
/// A later lookup of the cache can miss the workspace: another reader
/// evicted it, or the scan was not cached because a write landed while
/// it ran. The vectors here are the ones lifted out of these records.
pub struct LiveSnapshot {
    pub atoms: Live,
    pub vectors: Arc<HashMap<String, Arc<[f32]>>>,
    /// JSON numbers for each vector, as stored, for an answer to copy.
    pub originals: Arc<HashMap<String, Arc<[serde_json::Number]>>>,
    pub generation: u64,
    /// Whether this set is the one a writer has published. A scan taken
    /// while a write is held is not installed: the writer is about to
    /// swap its own.
    pub installed: bool,
}

/// Clears the writing flag when the publish lock is dropped.
struct Writing<'a>(&'a AtomicBool);

impl Drop for Writing<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// A workspace's index at one generation: the id and content hash of
/// each atom the index was built over, in order, beside the index and
/// its tokens. The atoms themselves are not held here, so the live set
/// stays unshared between writes and a write edits it in place rather
/// than copying it.
type Searchable = (
    u64,
    (Vec<(String, String)>, Arc<Index>, Arc<Vec<Vec<String>>>),
);

/// The id and a hash of the indexed fields of each atom, the fingerprint
/// an index is keyed on.
///
/// The write stamp is not the fingerprint. It has millisecond resolution,
/// so a rewrite in the same millisecond as the previous write compared
/// equal and the index kept the old tokens.
fn fingerprint(atoms: &[Arc<Record>]) -> Vec<(String, String)> {
    atoms
        .iter()
        .map(|atom| {
            (
                atom.get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                indexed_hash(atom),
            )
        })
        .collect()
}

/// Hash of the text and entities [`packset_core::search::atom_tokens`]
/// reads. Either changing is a different document. The clock is not an
/// input: two writes in one millisecond still compare unequal when the
/// text does.
fn indexed_hash(atom: &Record) -> String {
    // FNV-1a, 128 bits. A collision would leave a stale document, which
    // is the failure this comparison exists to prevent, so 64 bits is
    // too narrow for a pack that keeps growing.
    let mut hash = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58du128;
    let mut mix = |text: &str| {
        let bytes = text.as_bytes();
        let len = (bytes.len() as u64).to_le_bytes();
        for byte in len.into_iter().chain(bytes.iter().copied()) {
            hash ^= u128::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_0000_0000_0000_013b);
        }
    };
    mix(atom.get("text").and_then(Value::as_str).unwrap_or(""));
    if let Some(Value::Array(items)) = atom.get("entities") {
        for item in items {
            match item.as_str() {
                Some(name) => mix(name),
                None => mix(&item.to_string()),
            }
        }
    }
    format!("{hash:032x}")
}

/// When a record was last written: `updated`, else `ts` on a record no
/// write has stamped yet.
fn written_last(atom: &Record) -> &str {
    [UPDATED, "ts"]
        .into_iter()
        .filter_map(|key| atom.get(key).and_then(Value::as_str))
        .find(|stamp| !stamp.is_empty())
        .unwrap_or("")
}

/// The field each write stamps with its time. `ts` is when the atom was
/// first written and no rewrite changes it.
pub const UPDATED: &str = "updated";

/// Keep the time the stored record was created on a rewrite and stamp
/// `updated`. A rewrite of links, a review clock or a closed window used
/// to put the time of the rewrite in `ts`, so the time the atom was
/// written was lost. A stored record from before [`UPDATED`] gets the
/// time [`created_at`] finds, so a write that reaches it before
/// [`Store::restore_created`] does keeps that time too.
fn stamp_write(payload: &mut Record, stored: Option<&Record>, now: &str) {
    if let Some(created) = stored.and_then(created_at) {
        payload.insert("ts".into(), Value::String(created));
    }
    payload.insert(UPDATED.into(), Value::String(now.to_string()));
}

/// When a stored record was created: its `ts`, or for a record from
/// before [`UPDATED`], the earlier time [`created_before_rewrite`] finds.
fn created_at(atom: &Record) -> Option<String> {
    let ts = atom
        .get("ts")
        .and_then(Value::as_str)
        .filter(|ts| !ts.is_empty())?;
    if atom.contains_key(UPDATED) {
        return Some(ts.to_string());
    }
    Some(created_before_rewrite(atom).unwrap_or_else(|| ts.to_string()))
}

/// The time a record written before [`UPDATED`] existed was created, when
/// a rewrite moved its `ts` and the time survives elsewhere on it. A write
/// copies `ts` into `valid_from` when the writer gave none, so a
/// `valid_from` earlier than `ts` is the time the rewrite took. A record
/// with no such `valid_from` falls back to the writer's own clock in
/// `source.at`, when that is earlier than `ts`.
fn created_before_rewrite(atom: &Record) -> Option<String> {
    use packset_core::clock::{canonicalize, parse_millis};
    let ts = parse_millis(atom.get("ts").and_then(Value::as_str)?)?;
    let earlier = |v: Option<&Value>| {
        let stamp = canonicalize(v.and_then(Value::as_str)?)?;
        (parse_millis(&stamp)? < ts).then_some(stamp)
    };
    earlier(atom.get("valid_from"))
        .or_else(|| earlier(atom.get("source").and_then(|s| s.get("at"))))
}

/// A record a writer from before [`UPDATED`] left, stamped: its `ts`
/// becomes `updated`, and `ts` goes back to [`created_at`]. `None` for a
/// record that has `updated` already, or no `ts` to move.
fn stamp_old(mut atom: Record) -> Option<Record> {
    if atom.contains_key(UPDATED) {
        return None;
    }
    let last = atom
        .get("ts")
        .and_then(Value::as_str)
        .filter(|ts| !ts.is_empty())?
        .to_string();
    let created = created_at(&atom)?;
    atom.insert("ts".into(), Value::String(created));
    atom.insert(UPDATED.into(), Value::String(last));
    Some(atom)
}

/// Stored bytes one step of [`Store::restore_created`] reads, stamped or
/// not, and so the most it rewrites. A write that arrives mid-step waits
/// for its commit. Over 50,000 records with 384-d vectors (394 MiB; 8-core
/// Intel Xeon, on 947c498), a `remember` during the pass took 9/12 ms
/// (median/max) at 256 KiB steps, 13/14 ms at 1 MiB and 27/35 ms at 4 MiB,
/// against 8/9 ms after it. The pass took 3.0, 2.4 and 2.4 s. 1 MiB is as
/// fast as 4 MiB for half the wait.
///
/// The pages a step read leave the map before the next step reads, so
/// the pass holds a step of the pack at once, with what the kernel maps
/// around each place the step reads: up to a folio where it maps large
/// folios whole.
const STAMP_STEP_BYTES: usize = 1024 * 1024;

/// Steps of free space the pass needs. LMDB reuses the pages a step frees
/// two commits later, so two steps are in flight, and leaf splits and the
/// branch pages above them add to each. On a tmpfs (8-core Intel Xeon, on
/// 947c498), 50,000 records of 600 B and of 8 KiB (33 and 394 MiB) were
/// each stamped with 3 MiB free, and each stopped with 2 MiB. Without
/// steps, the 33 MiB store needed more than 15 MiB.
const STAMP_ROOM_STEPS: usize = 3;

/// The one field [`Store::restore_created`] reads to pass a record by, so
/// a pack stamped already is not parsed into records again at each start.
#[derive(serde::Deserialize)]
struct Stamp {
    updated: Option<serde::de::IgnoredAny>,
}

impl Store {
    /// Stamp the records a writer from before [`UPDATED`] left, as
    /// [`stamp_old`] does, in steps of [`STAMP_STEP_BYTES`] read. Each step
    /// drops the pages it read from the map, so what the pass holds is set
    /// by a step, not by the pack. A pass that kept what it read held it
    /// all: a start over 791 MiB peaked at 799 MiB that way, and at 16 MiB
    /// dropping each step on 4 KiB pages. Where the kernel maps large
    /// folios whole, a step holds the folios it reads, and the peak grows
    /// slowly with the pack (`measurements/start.txt`).
    ///
    /// packsetd runs this behind its listener, so an upgraded pack answers
    /// while it is rewritten. Each step commits on its own, and a step that
    /// fails leaves its records as they were. Until the pass ends a read
    /// can show a record as it was before the upgrade, which is how it read
    /// then: the cached sets are dropped once, after the last step, not at
    /// each one. A record that has `updated` is passed by, so a second run
    /// writes nothing. Returns how many records were stamped.
    ///
    /// # Errors
    ///
    /// The scan's or the failed step's, after its line is printed.
    pub fn restore_created(&self) -> anyhow::Result<usize> {
        let mut stamped = 0;
        let mut after: Option<Vec<u8>> = None;
        let stopped = loop {
            let mut step: Vec<Vec<u8>> = Vec::new();
            let mut bytes = 0usize;
            let next = match self.each_unstamped(after.as_deref(), |key, len| {
                step.push(key.to_vec());
                bytes = bytes.saturating_add(len);
            }) {
                Ok(next) => next,
                Err(err) => break Some((err, None)),
            };
            if !step.is_empty() {
                match self.stamp_step(&step) {
                    Ok(count) => stamped += count,
                    Err(err) => break Some((err, Some(bytes))),
                }
            }
            self.release_pages();
            match next {
                Some(key) => after = Some(key),
                None => break None,
            }
        };
        if stamped > 0 {
            // After the last commit, as in `forget_workspace`. The publish
            // lock keeps a rebuild from sampling the old generation and then
            // reading the bumped one.
            let _writing = self.mark_writing();
            let _publish = self.publish.write().unwrap_or_else(|e| e.into_inner());
            self.generation.fetch_add(1, Ordering::AcqRel);
        }
        release_heap();
        let Some((err, needs)) = stopped else {
            if stamped > 0 {
                eprintln!("packsetd: stamped {stamped} records written before `updated`");
            }
            return Ok(stamped);
        };
        let mut left = 0usize;
        let mut from: Option<Vec<u8>> = None;
        while let Ok(Some(next)) = self.each_unstamped(from.as_deref(), |_, _| left += 1) {
            self.release_pages();
            from = Some(next);
        }
        // A full disk can read as an I/O error: LMDB reports a short write
        // that way. The room is said whatever the error, since it is the
        // first thing to check.
        let room = needs.map_or_else(String::new, |bytes| {
            format!(
                " Stamping them needs {} free in {}.",
                mebibytes(bytes.saturating_mul(STAMP_ROOM_STEPS)),
                self.db_dir.display()
            )
        });
        eprintln!(
            "packsetd: {left} records from before `updated` stay as they were: {err}.{room} \
             The next start tries again, and writes go on meanwhile."
        );
        Err(err)
    }

    /// Visit each key after `after` whose record has no `updated`, with the
    /// length stored, over one step: [`STAMP_STEP_BYTES`] of records read.
    /// Returns the last key read when the step ends before the store does.
    fn each_unstamped(
        &self,
        after: Option<&[u8]>,
        mut visit: impl FnMut(&[u8], usize),
    ) -> anyhow::Result<Option<Vec<u8>>> {
        use std::ops::Bound;
        let _gate = self.gate();
        let rtxn = self.env.read_txn()?;
        let range = (
            after.map_or(Bound::Unbounded, Bound::Excluded),
            Bound::Unbounded,
        );
        let mut read = 0usize;
        for item in self.db.range(&rtxn, &range)? {
            let (key, raw) = item?;
            let unstamped = matches!(
                serde_json::from_slice::<Stamp>(raw),
                Ok(Stamp { updated: None })
            );
            if unstamped {
                visit(key, raw.len());
            }
            read = read.saturating_add(key.len() + raw.len());
            if read >= STAMP_STEP_BYTES {
                return Ok(Some(key.to_vec()));
            }
        }
        Ok(None)
    }

    /// Stamp one step's records in one transaction and say how many. Each
    /// is read again under the write lock, so a write since the scan is
    /// what is kept.
    fn stamp_step(&self, step: &[Vec<u8>]) -> anyhow::Result<usize> {
        self.write(|wtxn| {
            let mut count = 0;
            for key in step {
                let stored = self
                    .db
                    .get(wtxn, key)?
                    .and_then(|raw| serde_json::from_slice::<Value>(raw).ok());
                let Some(atom) = stored.and_then(|v| match v {
                    Value::Object(map) => stamp_old(map),
                    _ => None,
                }) else {
                    continue;
                };
                self.db.put(wtxn, key, &serde_json::to_vec(&atom)?)?;
                count += 1;
            }
            Ok(count)
        })
    }
}

/// `n` bytes in MiB, to one decimal, for a line a person reads.
fn mebibytes(n: usize) -> String {
    format!("{:.1} MiB", n as f64 / (1024.0 * 1024.0))
}

/// What a search runs over: the snapshot, the inverted index, the tokens
/// the index was built from, and each atom's vector, all shared.
pub type SearchSet = (
    Live,
    Arc<Index>,
    Arc<Vec<Vec<String>>>,
    Arc<HashMap<String, Arc<[f32]>>>,
);

/// The key for one atom.
#[must_use]
pub fn atom_key(workspace: &str, id: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(workspace.len() + id.len() + 1);
    key.extend_from_slice(workspace.as_bytes());
    key.push(0);
    key.extend_from_slice(id.as_bytes());
    key
}

/// The prefix every key in one workspace opens with.
#[must_use]
pub fn workspace_prefix(workspace: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(workspace.len() + 1);
    key.extend_from_slice(workspace.as_bytes());
    key.push(0);
    key
}

/// The first byte of a key in the tombstone index. UTF-8 never holds this
/// byte, so no workspace name opens with it: an index key is never a
/// record's key, and no workspace prefix reaches one.
const TOMBSTONE_INDEX: u8 = 0xFF;

/// The index entry for a record key, or for a workspace prefix: the same
/// bytes behind [`TOMBSTONE_INDEX`]. The value is empty. Every reader that
/// walks the whole database skips a value that is not JSON.
fn tombstone_key(key: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(key.len() + 1);
    out.push(TOMBSTONE_INDEX);
    out.extend_from_slice(key);
    out
}

/// The atom database, plus the lock that makes it one writer.
pub struct Store {
    /// Declared before `_lock`: fields drop in order, and the environment
    /// has to be closed before another opener can take the lock.
    env: Env,
    db: Database<Bytes, Bytes>,
    /// Shared by every transaction, and held alone to grow the map. LMDB
    /// remaps on a resize, and asks that no transaction in the process be
    /// open across it: a reader would be left in the old mapping.
    map_gate: RwLock<()>,
    /// Held for as long as the store is: dropping it drops the lock.
    _lock: HomeLock,
    /// Bumped by every write, so a reader can tell a stale snapshot.
    generation: AtomicU64,
    /// Held by a writer across the commit and the generation bump, and by
    /// a reader that rebuilds a snapshot. The rebuild cannot see a commit
    /// the generation does not include yet, and the generation cannot move
    /// while that read is in the database.
    publish: RwLock<()>,
    /// Unix millis of the last [`Self::release_after_write`].
    last_release: AtomicU64,
    /// One parsed live set per workspace, shared by concurrent readers.
    /// A writer replaces the entry after its commit. A reader clones the
    /// `Arc` and does not take that writer's lock, so the write in
    /// progress serves the set already published.
    live: RwLock<HashMap<String, Snapshot>>,
    /// Set for as long as a writer holds [`Self::publish`], so a reader
    /// can serve the published set instead of waiting on the lock.
    writing: AtomicBool,
    /// The inverted index over one workspace's live atoms, per generation,
    /// kept beside the snapshot its ordinals index.
    terms: RwLock<HashMap<String, Searchable>>,
    /// One index build at a time: readers that find the cache stale wait for
    /// the build in flight and take its result, rather than each building.
    terms_build: Mutex<()>,
    /// Status counts per workspace at one generation, so `status` does not
    /// re-parse the pack on every call: ten thousand atoms scan eighty
    /// milliseconds a status, once per generation instead of once per call.
    counts: RwLock<HashMap<Option<String>, (u64, Counts)>>,
    /// One count at a time, as for the index.
    counts_build: Mutex<()>,
    /// Parsed workspaces kept at once. Zero keeps every workspace a read
    /// has touched.
    workspace_cache: AtomicUsize,
    /// Bytes of snapshots and indexes kept at once. Zero is no byte cap.
    cache_limit: AtomicUsize,
    /// Workspaces dropped by the byte or count cap, for the atom caches
    /// that live beside this store.
    evicted: Mutex<Vec<String>>,
    /// Bumped when a cached workspace is used, so the coldest can be dropped.
    cache_clock: AtomicU64,
    /// The tick of each workspace last read or written.
    cache_used: Mutex<HashMap<String, u64>>,
    /// `memory.lmdb`, so an idle release can advise its pages.
    db_dir: std::path::PathBuf,
    #[cfg(test)]
    counted: AtomicU64,
    #[cfg(test)]
    scanned: AtomicU64,
    #[cfg(test)]
    tombstones_read: AtomicU64,
}

impl Drop for Store {
    /// heed keeps every environment it opens in a table of its own and
    /// closes one only after `prepare_for_closing`. Without it a dropped
    /// store keeps three descriptors, its map and an LMDB thread key until
    /// the process ends, and glibc has 1024 keys.
    fn drop(&mut self) {
        let _closing = self.env.clone().prepare_for_closing();
    }
}

/// What `status` reports over one workspace: live, tombstoned and expired
/// claims by kind, forgotten claims by reason, and the latest stamp.
#[derive(Debug, Clone, Default)]
pub struct Counts {
    /// Live claims by kind.
    pub live: BTreeMap<String, usize>,
    /// Tombstoned claims by kind.
    pub tombstone: BTreeMap<String, usize>,
    /// Expired claims by kind.
    pub expired: BTreeMap<String, usize>,
    /// Forgotten claims by reason.
    pub forgotten: BTreeMap<String, usize>,
    /// The latest stamp seen.
    pub last_write: String,
}

impl Store {
    /// Open the database under `root`, taking the single-writer lock.
    ///
    /// # Errors
    ///
    /// Fails when the home cannot be created, when another process already
    /// holds the lock, or when LMDB refuses the directory.
    pub fn open(root: &Path) -> anyhow::Result<Self> {
        private_home(root)?;
        let lock = take_lock(&root.join("packsetd.lock"))?;
        let db_path = root.join("memory.lmdb");
        fs::create_dir_all(&db_path)?;
        // SAFETY: LMDB maps the file; the contract is that no other process
        // writes it, which the lock above is what enforces. The map starts
        // from the file's size and grows when a write needs room; the file
        // grows with the pages written.
        let env = match unsafe {
            EnvOpenOptions::new()
                .map_size(map_for(&db_path))
                .max_dbs(1)
                .open(&db_path)
        } {
            Ok(env) => env,
            // A store closes its environment on drop, so this is another
            // opener in this process that has not closed its own. heed hands
            // that environment back with the map it was opened with, and a
            // write that needs room grows it. That opener's transactions are
            // outside the map gate, so it must hold none while this store
            // writes.
            Err(heed::Error::BadOpenOptions { env, .. }) => env,
            Err(err) => return Err(err.into()),
        };
        let mut wtxn = env.write_txn()?;
        let db: Database<Bytes, Bytes> = env.create_database(&mut wtxn, None)?;
        wtxn.commit()?;
        // A store created while the file was extended to the map still has
        // that tail. The pages at and below the last used page are the
        // database; the rest can go back to the filesystem. A later write
        // extends the file from the last page.
        trim_to_used_pages(&env, db, &db_path)?;
        Ok(Self {
            env,
            db,
            map_gate: RwLock::new(()),
            _lock: lock,
            generation: AtomicU64::new(0),
            publish: RwLock::new(()),
            last_release: AtomicU64::new(0),
            live: RwLock::new(HashMap::new()),
            writing: AtomicBool::new(false),
            terms: RwLock::new(HashMap::new()),
            terms_build: Mutex::new(()),
            counts: RwLock::new(HashMap::new()),
            counts_build: Mutex::new(()),
            workspace_cache: AtomicUsize::new(workspace_cache_from_env()),
            cache_limit: AtomicUsize::new(cache_limit_from_env()),
            evicted: Mutex::new(Vec::new()),
            cache_clock: AtomicU64::new(1),
            cache_used: Mutex::new(HashMap::new()),
            db_dir: db_path,
            #[cfg(test)]
            counted: AtomicU64::new(0),
            #[cfg(test)]
            scanned: AtomicU64::new(0),
            #[cfg(test)]
            tombstones_read: AtomicU64::new(0),
        })
    }

    /// The map gate, shared. Held for as long as a transaction is open.
    fn gate(&self) -> std::sync::RwLockReadGuard<'_, ()> {
        self.map_gate.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Run `write` in one transaction and commit it. When the map has no
    /// room, the transaction is dropped, the map grows by [`MAP_GROWTH`],
    /// and `write` runs again over the store as it was committed.
    fn write<T>(
        &self,
        mut write: impl FnMut(&mut heed::RwTxn<'_>) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        loop {
            let full_at = {
                let _gate = self.gate();
                let mut attempt = || -> anyhow::Result<T> {
                    let mut wtxn = self.env.write_txn()?;
                    let value = write(&mut wtxn)?;
                    wtxn.commit()?;
                    Ok(value)
                };
                match attempt() {
                    Err(err) if is_map_full(&err) => self.env.info().map_size,
                    done => return done,
                }
            };
            self.grow_map(full_at)?;
        }
    }

    /// Grow the map from `full_at`, the size a write found full, unless a
    /// writer that waited beside this one already grew it.
    fn grow_map(&self, full_at: usize) -> anyhow::Result<()> {
        let _alone = self.map_gate.write().unwrap_or_else(|e| e.into_inner());
        let size = self.env.info().map_size;
        if size > full_at {
            return Ok(());
        }
        let grown = size
            .checked_mul(MAP_GROWTH)
            .ok_or_else(|| anyhow::anyhow!("the map cannot grow past {size} bytes"))?
            .div_ceil(MAP_ALIGN)
            * MAP_ALIGN;
        // SAFETY: the gate is held alone, so no transaction of this store
        // is open, and every one that starts after waits for the gate.
        unsafe { self.env.resize(grown)? };
        Ok(())
    }

    /// Every record in one workspace, or in all of them.
    ///
    /// # Errors
    ///
    /// Fails when the read transaction does.
    pub fn scan(&self, workspace: Option<&str>) -> anyhow::Result<Vec<Record>> {
        #[cfg(test)]
        self.scanned.fetch_add(1, Ordering::SeqCst);
        if workspace == Some("") {
            return Ok(Vec::new());
        }
        let _gate = self.gate();
        let rtxn = self.env.read_txn()?;
        let mut out = Vec::new();
        match workspace {
            Some(name) => {
                let prefix = workspace_prefix(name);
                for item in self.db.prefix_iter(&rtxn, &prefix)? {
                    let (_, raw) = item?;
                    push_record(&mut out, raw);
                }
            }
            None => {
                for item in self.db.iter(&rtxn)? {
                    let (_, raw) = item?;
                    push_record(&mut out, raw);
                }
            }
        }
        Ok(out)
    }

    /// Visit each record without collecting them. Status counts 30k expired
    /// atoms this way instead of holding every JSON value at once.
    ///
    /// # Errors
    ///
    /// Fails when the read transaction does.
    pub fn for_each(
        &self,
        workspace: Option<&str>,
        mut visit: impl FnMut(&Record),
    ) -> anyhow::Result<()> {
        if workspace == Some("") {
            return Ok(());
        }
        let _gate = self.gate();
        let rtxn = self.env.read_txn()?;
        let mut each = |raw: &[u8]| {
            if let Ok(Value::Object(record)) = serde_json::from_slice::<Value>(raw) {
                visit(&record);
            }
        };
        match workspace {
            Some(name) => {
                let prefix = workspace_prefix(name);
                for item in self.db.prefix_iter(&rtxn, &prefix)? {
                    let (_, raw) = item?;
                    each(raw);
                }
            }
            None => {
                for item in self.db.iter(&rtxn)? {
                    let (_, raw) = item?;
                    each(raw);
                }
            }
        }
        Ok(())
    }

    /// One workspace's records but its tombstones, for a read that keeps
    /// only what can be live. A tombstone the index names is passed by on
    /// its key and its value is not parsed. One parsed here was written
    /// before the index; it is entered now, so the next cold read passes it
    /// by too.
    fn scan_past_tombstones(&self, workspace: &str) -> anyhow::Result<Vec<Record>> {
        #[cfg(test)]
        self.scanned.fetch_add(1, Ordering::SeqCst);
        if workspace.is_empty() {
            return Ok(Vec::new());
        }
        let prefix = workspace_prefix(workspace);
        let mut out = Vec::new();
        let mut unindexed = Vec::new();
        {
            let _gate = self.gate();
            let rtxn = self.env.read_txn()?;
            // Both walks are in key order, and an entry is its record's key
            // behind one byte, so the index is read alongside, not looked up.
            // An entry that fails to read ends the index walk: the
            // tombstones past it are parsed and dropped, as they were before.
            let mut buried = self
                .db
                .prefix_iter(&rtxn, &tombstone_key(&prefix))?
                .map_while(Result::ok)
                .map(|(entry, _)| &entry[1..])
                .peekable();
            for item in self.db.prefix_iter(&rtxn, &prefix)? {
                let (key, raw) = item?;
                while buried.next_if(|entry| *entry < key).is_some() {}
                if buried.next_if_eq(&key).is_some() {
                    continue;
                }
                let Ok(Value::Object(atom)) = serde_json::from_slice::<Value>(raw) else {
                    continue;
                };
                if record::is_tombstone(&atom) {
                    #[cfg(test)]
                    self.tombstones_read.fetch_add(1, Ordering::SeqCst);
                    unindexed.push(key.to_vec());
                } else {
                    out.push(atom);
                }
            }
        }
        // The answer stands without the entries. A write that fails leaves
        // the next cold read to parse these again.
        if !unindexed.is_empty() {
            let _ = self.index_tombstones(&unindexed);
        }
        Ok(out)
    }

    /// Enter tombstones written before the index. Each is read again inside
    /// the write, so a record written live since the scan is not entered.
    /// No record changes, so the generation stays and the cached snapshot
    /// is still the store's.
    fn index_tombstones(&self, keys: &[Vec<u8>]) -> anyhow::Result<()> {
        self.write(|wtxn| {
            for key in keys {
                if self.db.get(wtxn, key)?.is_some_and(is_tombstone) {
                    self.db.put(wtxn, &tombstone_key(key), b"")?;
                }
            }
            Ok(())
        })
    }

    /// One record by id, whatever its state.
    ///
    /// # Errors
    ///
    /// Fails when the read transaction does.
    pub fn get(&self, workspace: &str, id: &str) -> anyhow::Result<Option<Record>> {
        if workspace.is_empty() || id.is_empty() {
            return Ok(None);
        }
        let _gate = self.gate();
        let rtxn = self.env.read_txn()?;
        let raw = self.db.get(&rtxn, &atom_key(workspace, id))?;
        Ok(raw.and_then(|bytes| {
            serde_json::from_slice::<Value>(bytes)
                .ok()
                .and_then(|v| v.as_object().cloned())
        }))
    }

    /// Write one record, replacing whatever shared its key.
    ///
    /// # Errors
    ///
    /// Fails when the record has no workspace or id, or when the write does.
    pub fn upsert(&self, atom: &Record) -> anyhow::Result<()> {
        self.upsert_many(std::slice::from_ref(atom)).map(|_| ())
    }

    /// Write several records in one transaction, so a link rewrite lands
    /// whole, and hand back the records as stored. A record already in the
    /// store keeps the `ts` it was stored with, and every record is stamped
    /// with the write's time in `updated`.
    ///
    /// # Errors
    ///
    /// Fails when a record has no workspace or id, or when the write does.
    pub fn upsert_many(&self, atoms: &[Record]) -> anyhow::Result<Vec<Record>> {
        self.upsert_many_at(atoms, &packset_core::clock::utcnow())
    }

    /// Write `atoms` stamped with `now`.
    ///
    /// The clock is an argument so a test can place two writes in one
    /// millisecond. The index follows the text either way.
    fn upsert_many_at(&self, atoms: &[Record], now: &str) -> anyhow::Result<Vec<Record>> {
        // Held until the snapshot is published. A reader that arrives while
        // this is held serves the snapshot already swapped in, and does not
        // take the lock. The flag is cleared after the lock, so the next
        // read rebuilds if this write did not publish.
        let _writing = self.mark_writing();
        let _publish = self.publish.write().unwrap_or_else(|e| e.into_inner());
        let written = self.write(|wtxn| {
            let mut written = Vec::with_capacity(atoms.len());
            for atom in atoms {
                let mut payload = atom.clone();
                if !matches!(payload.get("links"), Some(Value::Array(_))) {
                    payload.insert("links".into(), Value::Array(Vec::new()));
                }
                let workspace = payload
                    .get("workspace")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("record has no workspace"))?;
                let id = payload
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("record has no id"))?;
                let key = atom_key(workspace, id);
                let stored = self
                    .db
                    .get(wtxn, &key)?
                    .and_then(|raw| serde_json::from_slice::<Value>(raw).ok())
                    .and_then(|v| match v {
                        Value::Object(map) => Some(map),
                        _ => None,
                    });
                stamp_write(&mut payload, stored.as_ref(), now);
                let buried = record::is_tombstone(&payload);
                if buried {
                    // Nothing scores a tombstone, and the vector was most of
                    // its bytes.
                    payload.remove("embedding");
                } else {
                    // A fire or a grade rewrites the record from the cache,
                    // which does not carry the vector. A null or an empty
                    // array is a clear. A write that leaves the key off keeps
                    // what is stored.
                    keep_stored_embedding(&mut payload, stored.as_ref());
                }
                let blob = serde_json::to_vec(&Value::Object(payload.clone()))?;
                self.db.put(wtxn, &key, &blob)?;
                if buried {
                    self.db.put(wtxn, &tombstone_key(&key), b"")?;
                } else if stored.as_ref().is_some_and(record::is_tombstone) {
                    self.db.delete(wtxn, &tombstone_key(&key))?;
                }
                written.push(payload);
            }
            Ok(written)
        })?;
        // After the commit, never before: a reader that scans between a bump
        // and its write would otherwise cache the older corpus as the newer.
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.patch_live(&written, generation);
        Ok(written)
    }

    /// Fold a committed write into the cached snapshot instead of dropping it.
    /// Only cached workspaces are patched; the result equals a fresh scan.
    fn patch_live(&self, written: &[Record], generation: u64) {
        let now = packset_core::clock::utcnow();
        let parked: Vec<Parked> = written.iter().map(park).collect();
        let Ok(mut cache) = self.live.write() else {
            return;
        };
        let mut keep = String::new();
        for (workspace, snap) in cache.iter_mut() {
            // One behind is this write; anything else raced and the snapshot
            // is not a base this write can be added to.
            if snap.generation + 1 != generation {
                continue;
            }
            let mut touched = false;
            let mut lean_written = Vec::new();
            {
                let vectors = Arc::make_mut(&mut snap.vectors);
                let originals = Arc::make_mut(&mut snap.originals);
                for item in &parked {
                    if item.lean.get("workspace").and_then(Value::as_str)
                        != Some(workspace.as_str())
                    {
                        continue;
                    }
                    touched = true;
                    let Some(id) = item.lean.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    match &item.vector {
                        Some(vector) => {
                            let same = vectors
                                .get(id)
                                .is_some_and(|old| old.as_ref() == vector.as_ref());
                            if !same {
                                vectors.insert(id.to_string(), Arc::clone(vector));
                            }
                            if let Some(original) = &item.original {
                                let same = originals
                                    .get(id)
                                    .is_some_and(|old| old.as_ref() == original.as_ref());
                                if !same {
                                    originals.insert(id.to_string(), Arc::clone(original));
                                }
                            }
                        }
                        None if item.named => {
                            vectors.remove(id);
                            originals.remove(id);
                        }
                        None => {}
                    }
                    let visible = shown_at(&item.lean, &now);
                    let at = snap
                        .stored
                        .iter()
                        .position(|a| a.get("id").and_then(Value::as_str) == Some(id));
                    match (at, visible) {
                        (Some(i), true) => snap.stored[i] = item.lean.clone(),
                        (Some(i), false) => {
                            snap.stored.remove(i);
                            vectors.remove(id);
                            originals.remove(id);
                        }
                        (None, true) => snap.stored.push(item.lean.clone()),
                        (None, false) => {
                            vectors.remove(id);
                            originals.remove(id);
                        }
                    }
                    lean_written.push(item.lean.clone());
                }
            }
            snap.generation = generation;
            patch_shown(
                &mut snap.shown,
                &snap.stored,
                &lean_written,
                &now,
                &mut snap.dangling,
            );
            if touched {
                keep = workspace.clone();
                self.note_cached(workspace);
            }
        }
        if !keep.is_empty() {
            self.evict_cached(&mut cache, &keep);
        }
    }

    /// Remember that `workspace` was just used.
    fn note_cached(&self, workspace: &str) {
        let tick = self.cache_clock.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut used) = self.cache_used.lock() {
            if let Some(slot) = used.get_mut(workspace) {
                *slot = tick;
            } else {
                used.insert(workspace.to_string(), tick);
            }
        }
    }

    /// Drop the coldest parsed workspaces until both caps hold. `keep` is
    /// the workspace a caller just built and must not be the one dropped.
    /// A count of zero is no count cap. A byte limit of zero is no byte
    /// cap. The index and the status counts leave with the snapshot.
    fn evict_cached(&self, cache: &mut HashMap<String, Snapshot>, keep: &str) {
        let cap = self.workspace_cache.load(Ordering::Relaxed);
        let limit = self.cache_limit.load(Ordering::Relaxed);
        loop {
            let terms_bytes = self
                .terms
                .read()
                .map(|terms| {
                    terms
                        .values()
                        .fold(0usize, |sum, set| sum.saturating_add(searchable_bytes(set)))
                })
                .unwrap_or(0);
            let live_bytes = cache
                .values()
                .fold(0usize, |sum, snap| sum.saturating_add(snapshot_bytes(snap)));
            let over_count = cap != 0 && cache.len() > cap;
            let over_bytes = limit != 0 && live_bytes.saturating_add(terms_bytes) > limit;
            if !over_count && !over_bytes {
                break;
            }
            let coldest = {
                let used = match self.cache_used.lock() {
                    Ok(used) => used,
                    Err(poisoned) => poisoned.into_inner(),
                };
                cache
                    .keys()
                    .filter(|name| name.as_str() != keep)
                    .min_by_key(|name| used.get(*name).copied().unwrap_or(0))
                    .cloned()
            };
            let Some(name) = coldest else {
                break;
            };
            cache.remove(&name);
            if let Ok(mut terms) = self.terms.write() {
                terms.remove(&name);
            }
            if let Ok(mut counts) = self.counts.write() {
                counts.remove(&Some(name.clone()));
            }
            if let Ok(mut used) = self.cache_used.lock() {
                used.remove(&name);
            }
            self.note_evicted(&name);
        }
    }

    fn note_evicted(&self, name: &str) {
        if let Ok(mut evicted) = self.evicted.lock() {
            if !evicted.iter().any(|kept| kept == name) {
                evicted.push(name.to_string());
            }
        }
    }

    /// Workspaces the cap dropped since the last call.
    pub fn take_evicted(&self) -> Vec<String> {
        match self.evicted.lock() {
            Ok(mut evicted) => std::mem::take(&mut *evicted),
            Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
        }
    }

    /// The byte cap. Zero keeps every snapshot the count cap allows.
    pub fn set_cache_limit_bytes(&self, bytes: usize) {
        self.cache_limit.store(bytes, Ordering::Relaxed);
    }

    /// The byte cap. Zero is no cap.
    #[must_use]
    pub fn cache_limit_bytes(&self) -> usize {
        self.cache_limit.load(Ordering::Relaxed)
    }

    /// Bytes of the cached snapshots and their indexes.
    #[must_use]
    pub fn cache_resident_bytes(&self) -> usize {
        let live = match self.live.read() {
            Ok(cache) => cache,
            Err(poisoned) => poisoned.into_inner(),
        };
        let terms = match self.terms.read() {
            Ok(cache) => cache,
            Err(poisoned) => poisoned.into_inner(),
        };
        let snapshots = live
            .values()
            .fold(0usize, |sum, snap| sum.saturating_add(snapshot_bytes(snap)));
        let indexes = terms
            .values()
            .fold(0usize, |sum, set| sum.saturating_add(searchable_bytes(set)));
        snapshots.saturating_add(indexes)
    }

    /// The least recently used cached workspace.
    pub fn coldest_workspace(&self) -> Option<String> {
        let cache = match self.live.read() {
            Ok(cache) => cache,
            Err(poisoned) => poisoned.into_inner(),
        };
        let used = match self.cache_used.lock() {
            Ok(used) => used,
            Err(poisoned) => poisoned.into_inner(),
        };
        cache
            .keys()
            .min_by_key(|name| used.get(*name).copied().unwrap_or(0))
            .cloned()
    }

    /// Drop one workspace's snapshot and index. The atom caches hear about
    /// it from [`Self::take_evicted`].
    pub fn evict_workspace(&self, name: &str) {
        if let Ok(mut cache) = self.live.write() {
            cache.remove(name);
        }
        if let Ok(mut terms) = self.terms.write() {
            terms.remove(name);
        }
        if let Ok(mut counts) = self.counts.write() {
            counts.remove(&Some(name.to_string()));
        }
        if let Ok(mut used) = self.cache_used.lock() {
            used.remove(name);
        }
        self.note_evicted(name);
        release_heap();
    }

    /// Drop indexes the workspace cap no longer has a snapshot for, and any
    /// past that on their own. `keep` was just built.
    fn evict_terms(&self, cache: &mut HashMap<String, Searchable>, keep: &str) {
        let cap = self.workspace_cache.load(Ordering::Relaxed);
        if cap == 0 {
            return;
        }
        while cache.len() > cap {
            let coldest = {
                let used = match self.cache_used.lock() {
                    Ok(used) => used,
                    Err(poisoned) => poisoned.into_inner(),
                };
                cache
                    .keys()
                    .filter(|name| name.as_str() != keep)
                    .min_by_key(|name| used.get(*name).copied().unwrap_or(0))
                    .cloned()
            };
            let Some(name) = coldest else {
                break;
            };
            cache.remove(&name);
        }
    }

    /// Drop status counts the same way. The all-workspace entry stays: it
    /// is one row, not a corpus. The names that left are returned so the
    /// caller can drop them from the use clock after this lock is gone.
    /// A workspace that was only counted used to stay on that clock for
    /// the life of the process.
    fn evict_counts(
        &self,
        cache: &mut HashMap<Option<String>, (u64, Counts)>,
        keep: &str,
    ) -> Vec<String> {
        let mut dropped = Vec::new();
        let cap = self.workspace_cache.load(Ordering::Relaxed);
        if cap == 0 {
            return dropped;
        }
        while cache.keys().filter(|key| key.is_some()).count() > cap {
            let coldest = {
                let used = match self.cache_used.lock() {
                    Ok(used) => used,
                    Err(poisoned) => poisoned.into_inner(),
                };
                cache
                    .keys()
                    .filter_map(|key| key.as_deref())
                    .filter(|name| *name != keep)
                    .min_by_key(|name| used.get(*name).copied().unwrap_or(0))
                    .map(str::to_string)
            };
            let Some(name) = coldest else {
                break;
            };
            cache.remove(&Some(name.clone()));
            dropped.push(name);
        }
        dropped
    }

    /// Take count-only names off the use clock. The counts lock is not
    /// held. Live, then the search index, then the clock: a name still
    /// parsed or indexed keeps its tick.
    fn drop_count_only(&self, names: &[String]) {
        if names.is_empty() {
            return;
        }
        let live = match self.live.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let terms = match self.terms.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut used = match self.cache_used.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        for name in names {
            if !live.contains_key(name) && !terms.contains_key(name) {
                used.remove(name);
            }
        }
    }

    /// The cap, for a test that wants a smaller one than the process default.
    pub fn set_workspace_cache(&self, cap: usize) {
        self.workspace_cache.store(cap, Ordering::Relaxed);
    }

    /// How many workspaces are parsed right now.
    pub fn cached_workspaces(&self) -> usize {
        self.live.read().map(|cache| cache.len()).unwrap_or(0)
    }

    /// How many tombstones a cold read has parsed since the store opened.
    #[cfg(test)]
    pub fn tombstones_read(&self) -> u64 {
        self.tombstones_read.load(Ordering::SeqCst)
    }

    /// How many workspaces the use clock still names.
    #[cfg(test)]
    fn cache_used_len(&self) -> usize {
        self.cache_used.lock().map(|used| used.len()).unwrap_or(0)
    }

    /// Return free heap to the kernel and drop the database's cached pages.
    /// The mapping stays; the next read faults the pages it needs. Called
    /// after a quiet stretch, when the high-water mark of a burst would
    /// otherwise sit in swap until the process ends.
    pub fn release_idle(&self) {
        self.release_pages();
        release_heap();
    }

    /// Return free heap after a write, at most once per
    /// [`WRITE_RELEASE_MS`]. A write parses a body with a vector, encodes
    /// the text and serialises the record, and glibc keeps what those
    /// freed in the worker's arena: a constant corpus of 256 claims
    /// rewritten three times grew the writer by 1.8 MiB a window until the
    /// idle release a minute on. The trim here is a few hundred
    /// microseconds, and a burst of writes pays it every 50 ms, not per
    /// write.
    pub fn release_after_write(&self) {
        let now = unix_millis();
        let last = self.last_release.load(Ordering::Relaxed);
        if now.saturating_sub(last) < WRITE_RELEASE_MS {
            return;
        }
        if self
            .last_release
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            release_heap();
        }
    }

    /// Flush committed pages to disk.
    ///
    /// An owned writer exits from its parent watcher with `process::exit`,
    /// which does not run destructors on the thread that opened this
    /// environment. A commit already syncs; this is the flush a close
    /// would issue, so that exit does not depend on the destructor.
    pub fn sync(&self) -> anyhow::Result<()> {
        self.env.force_sync()?;
        Ok(())
    }

    fn release_pages(&self) {
        // Linux only. The map is read-only: LMDB writes with `write`,
        // not `WRITEMAP`, so there are no dirty pages to discard. This
        // advice is not applied on macOS, where clean file-backed pages
        // are already outside the footprint. Everywhere else the mapped
        // file stays resident.
        #[cfg(target_os = "linux")]
        {
            // Held alone, so no transaction of this store is in the map.
            // Advising it while a reader is mid-page faults that page back
            // from the file, which can be a newer or older version than the
            // snapshot the reader started, and the scan then drops a record.
            // A resize in between would unmap the range, and the addresses
            // could be reused.
            let _gate = self.map_gate.write().unwrap_or_else(|e| e.into_inner());
            // Open a write transaction and drop it at once, so a commit
            // that does not take the gate finishes before pages leave the
            // map. The map is a read-only shared view: LMDB writes with
            // `write`, not `WRITEMAP`. Advising that view reloads the file.
            // Advising the file descriptor does not; the pages stay mapped.
            if self.env.write_txn().is_err() {
                return;
            }
            let Ok(maps) = fs::read_to_string("/proc/self/maps") else {
                return;
            };
            let needle = self.db_dir.join("data.mdb");
            let needle = fs::canonicalize(&needle).unwrap_or(needle);
            let needle = needle.to_string_lossy();
            for line in maps.lines() {
                if !line.contains(needle.as_ref()) {
                    continue;
                }
                let mut fields = line.split_whitespace();
                let (Some(range), Some(perms)) = (fields.next(), fields.next()) else {
                    continue;
                };
                // A private mapping advised this way is discarded as
                // zeroes. Only the shared read-only view is the database.
                if !perms.starts_with("r--s") {
                    continue;
                }
                let Some((start_s, end_s)) = range.split_once('-') else {
                    continue;
                };
                let (Ok(start), Ok(end)) = (
                    usize::from_str_radix(start_s, 16),
                    usize::from_str_radix(end_s, 16),
                ) else {
                    continue;
                };
                if end <= start {
                    continue;
                }
                // SAFETY: the range is this process's shared read-only map
                // of its own database. The bytes stay in the file. The next
                // read faults them back in.
                unsafe {
                    madvise(start as *mut std::ffi::c_void, end - start, MADV_DONTNEED);
                }
            }
        }
    }

    /// The live and due records in one workspace, parsed once per write and
    /// shared; readers take this over [`Store::current`].
    ///
    /// # Errors
    ///
    /// Fails when the scan does.
    pub fn live(&self, workspace: &str) -> anyhow::Result<Live> {
        self.live_versioned(workspace).map(|snap| snap.atoms)
    }

    /// Status counts over one workspace, or over all of them: cached under
    /// the generation the scan ran at, recomputed when a write moved it.
    /// Keyed on the workspace as given, because [`Self::for_each`] visits
    /// nothing for the empty name but everything for no name.
    ///
    /// # Errors
    ///
    /// Fails when the scan does.
    pub fn counts(&self, workspace: Option<&str>) -> anyhow::Result<Counts> {
        let key = workspace.map(str::to_string);
        if let Some(found) = self.counts_cached(&key) {
            return Ok(found);
        }
        let _count = self.counts_build.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(found) = self.counts_cached(&key) {
            return Ok(found);
        }
        #[cfg(test)]
        self.counted.fetch_add(1, Ordering::SeqCst);
        let generation = self.generation.load(Ordering::Acquire);
        let now = packset_core::clock::utcnow();
        let mut counts = Counts::default();
        self.for_each(workspace, |rec| {
            let kind = rec
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let last = written_last(rec);
            if last > counts.last_write.as_str() {
                counts.last_write = last.to_string();
            }
            if rec
                .get("tombstone")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                *counts.tombstone.entry(kind).or_insert(0) += 1;
                if let Some(why) = rec.get("forgotten").and_then(Value::as_str) {
                    *counts.forgotten.entry(why.to_string()).or_insert(0) += 1;
                }
            } else if record::is_live(rec, &now) {
                *counts.live.entry(kind).or_insert(0) += 1;
            } else {
                *counts.expired.entry(kind).or_insert(0) += 1;
            }
        })?;
        let dropped = if let Ok(mut cache) = self.counts.write() {
            cache.insert(key, (generation, counts.clone()));
            if let Some(name) = workspace {
                self.note_cached(name);
                self.evict_counts(&mut cache, name)
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
        self.drop_count_only(&dropped);
        Ok(counts)
    }

    /// Counts kept at the current generation, if any.
    fn counts_cached(&self, key: &Option<String>) -> Option<Counts> {
        let generation = self.generation.load(Ordering::Acquire);
        let cache = self.counts.read().ok()?;
        let (seen, found) = cache.get(key)?;
        (*seen == generation).then(|| found.clone())
    }

    /// Remember that a writer holds [`Self::publish`]. Dropped after the
    /// lock, so a reader observes the flag for the whole hold.
    fn mark_writing(&self) -> Writing<'_> {
        self.writing.store(true, Ordering::Release);
        Writing(&self.writing)
    }

    /// The cached snapshot when it is the generation a writer has published.
    ///
    /// The generation is read while the cache lock is held. A publish bumps
    /// the generation and then updates the snapshot under that lock, so the
    /// two cannot be observed apart.
    fn cached_live(&self, workspace: &str) -> Option<LiveSnapshot> {
        let cache = self.live.read().unwrap_or_else(|e| e.into_inner());
        let generation = self.generation.load(Ordering::Acquire);
        let found = cache.get(workspace)?;
        if found.generation != generation {
            return None;
        }
        self.note_cached(workspace);
        Some(LiveSnapshot {
            atoms: Arc::clone(&found.shown),
            vectors: Arc::clone(&found.vectors),
            originals: Arc::clone(&found.originals),
            generation,
            installed: true,
        })
    }

    /// The snapshot last swapped in, even when a writer has since bumped
    /// the generation and not published the next one yet.
    fn published_live(&self, workspace: &str) -> Option<LiveSnapshot> {
        let cache = self.live.read().unwrap_or_else(|e| e.into_inner());
        let found = cache.get(workspace)?;
        self.note_cached(workspace);
        Some(LiveSnapshot {
            atoms: Arc::clone(&found.shown),
            vectors: Arc::clone(&found.vectors),
            originals: Arc::clone(&found.originals),
            generation: found.generation,
            installed: true,
        })
    }

    /// Scan `workspace` without waiting on a writer and without installing
    /// the result. LMDB readers do not block on the writer's transaction.
    fn scan_live_unpinned(&self, workspace: &str) -> anyhow::Result<LiveSnapshot> {
        let generation = self.generation.load(Ordering::Acquire);
        let now = packset_core::clock::utcnow();
        let scanned: Vec<Record> = self
            .scan_past_tombstones(workspace)?
            .into_iter()
            .filter(|atom| shown_at(atom, &now))
            .collect();
        let mut vectors = HashMap::with_capacity(scanned.len());
        let mut originals = HashMap::with_capacity(scanned.len());
        let mut stored = Vec::with_capacity(scanned.len());
        for record in scanned {
            let parked = park(&record);
            if let Some(id) = parked.lean.get("id").and_then(Value::as_str) {
                if let Some(vector) = parked.vector {
                    vectors.insert(id.to_string(), vector);
                }
                if let Some(original) = parked.original {
                    originals.insert(id.to_string(), original);
                }
            }
            stored.push(parked.lean);
        }
        let (shown, _dangling) = shown_from(&stored);
        Ok(LiveSnapshot {
            atoms: Arc::new(shown),
            vectors: Arc::new(vectors),
            originals: Arc::new(originals),
            generation,
            installed: false,
        })
    }

    /// [`Self::live`] with the generation the set belongs to, and the
    /// vectors lifted out of that same set.
    ///
    /// The vectors are returned with the records even when the scan is
    /// not cached. A reader that then asked the cache for them would
    /// see an empty map: the workspace was evicted, or a write landed
    /// while the scan ran and the snapshot was thrown away.
    ///
    /// # Errors
    ///
    /// Fails when the scan does.
    pub fn live_versioned(&self, workspace: &str) -> anyhow::Result<LiveSnapshot> {
        if let Some(found) = self.cached_live(workspace) {
            return Ok(found);
        }
        // A writer holds the publish lock across its commit. Waiting for
        // that lock is how a burst of readers used to stall behind one
        // remember. Serve the set the writer last swapped in, or scan
        // without installing, and do not take the lock.
        if self.writing.load(Ordering::Acquire) {
            if let Some(found) = self.published_live(workspace) {
                return Ok(found);
            }
            return self.scan_live_unpinned(workspace);
        }
        self.rebuild_live(workspace)
    }

    /// Scan the workspace and publish that snapshot, unless a writer has
    /// one waiting that is already current.
    ///
    /// The publish lock is held for the whole rebuild. The writer holds it
    /// across the commit and the generation bump, so this scan sees exactly
    /// the commits that generation includes. Returning the scan after the
    /// lock was released used to hand a reader one record from a commit
    /// that was not published yet, and the next read then served the
    /// shorter published snapshot.
    fn rebuild_live(&self, workspace: &str) -> anyhow::Result<LiveSnapshot> {
        let snap = {
            let _publish = self.publish.read().unwrap_or_else(|e| e.into_inner());
            if let Some(found) = self.cached_live(workspace) {
                return Ok(found);
            }
            let generation = self.generation.load(Ordering::Acquire);
            // The vector is lifted out of the record here, once. The cached
            // copies omit the key, and the original numbers are kept beside
            // the floats for answers.
            let now = packset_core::clock::utcnow();
            let scanned: Vec<Record> = self
                .scan_past_tombstones(workspace)?
                .into_iter()
                .filter(|atom| shown_at(atom, &now))
                .collect();
            let mut vectors = HashMap::with_capacity(scanned.len());
            let mut originals = HashMap::with_capacity(scanned.len());
            let mut stored = Vec::with_capacity(scanned.len());
            for record in scanned {
                let parked = park(&record);
                if let Some(id) = parked.lean.get("id").and_then(Value::as_str) {
                    if let Some(vector) = parked.vector {
                        vectors.insert(id.to_string(), vector);
                    }
                    if let Some(original) = parked.original {
                        originals.insert(id.to_string(), original);
                    }
                }
                stored.push(parked.lean);
            }
            let (shown, dangling) = shown_from(&stored);
            let shared = Arc::new(shown);
            let vectors = Arc::new(vectors);
            let originals = Arc::new(originals);
            let mut cache = self.live.write().unwrap_or_else(|e| e.into_inner());
            // Another rebuild may have published this generation while we
            // waited for the cache lock. Keep that snapshot: replacing it
            // with this scan would drop a record the other read already
            // handed out.
            if let Some(found) = cache.get(workspace) {
                if found.generation == generation {
                    self.note_cached(workspace);
                    return Ok(LiveSnapshot {
                        atoms: Arc::clone(&found.shown),
                        vectors: Arc::clone(&found.vectors),
                        originals: Arc::clone(&found.originals),
                        generation,
                        installed: true,
                    });
                }
            }
            cache.insert(
                workspace.to_string(),
                Snapshot {
                    generation,
                    stored,
                    shown: Arc::clone(&shared),
                    dangling,
                    vectors: Arc::clone(&vectors),
                    originals: Arc::clone(&originals),
                },
            );
            self.note_cached(workspace);
            self.evict_cached(&mut cache, workspace);
            LiveSnapshot {
                atoms: shared,
                vectors,
                originals,
                generation,
                installed: true,
            }
        };
        // The parse of the log peaked well above what stays live, and glibc
        // keeps that peak in its arenas. The log holds every rewrite, so the
        // peak grew with each one and the writer's size followed the log,
        // not the corpus: 1.8 MiB a window of 256 rewrites, measured. The
        // peak goes back now rather than at the idle release a minute on.
        release_heap();
        // The parsed set is what a reader uses from here. The scan faulted
        // the whole file, and those pages would sit beside the set until
        // the idle release. Drop them now; a later miss faults what it reads.
        self.release_pages();
        Ok(snap)
    }

    /// The vectors cached for `workspace`, one shared copy each. Empty when
    /// the workspace is not parsed. A caller that also has the records
    /// wants [`Self::live_versioned`]: this is a second read, and the
    /// workspace may have left the cache since the records were taken.
    /// The records from [`Self::live`] omit `embedding` where a vector was lifted.
    pub fn vectors(&self, workspace: &str) -> Arc<HashMap<String, Arc<[f32]>>> {
        self.live
            .read()
            .ok()
            .and_then(|cache| cache.get(workspace).map(|snap| Arc::clone(&snap.vectors)))
            .unwrap_or_else(|| Arc::new(HashMap::new()))
    }

    /// One workspace's live atoms and the index over them, as a matched pair;
    /// cards are scored against the same corpus.
    ///
    /// A write patches the live set in place, so the cached set is usually
    /// the new one with records rewritten at their positions and appended at
    /// the end. The index follows the same way: a rewritten record is
    /// re-indexed under its ordinal when its text or entities change, an
    /// appended one is pushed, and only a set whose order moved is rebuilt
    /// from scratch. The change is a hash of those fields. The write stamp
    /// is a millisecond, so it cannot say that a rewrite happened.
    ///
    /// # Errors
    ///
    /// Fails when the scan does.
    pub fn searchable(&self, workspace: &str) -> anyhow::Result<SearchSet> {
        if let Some(found) = self.searchable_cached(workspace)? {
            return Ok(found);
        }
        // One build at a time; a reader that waited looks again first.
        let _build = self.terms_build.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(found) = self.searchable_cached(workspace)? {
            return Ok(found);
        }
        let snap = self.live_versioned(workspace)?;
        let installed = snap.installed;
        let atoms = snap.atoms;
        let generation = snap.generation;
        let vectors = snap.vectors;
        // The stale set is taken out of the cache, so this thread holds its
        // only reference and the index is edited rather than copied.
        let previous = self
            .terms
            .write()
            .ok()
            .and_then(|mut cache| cache.remove(workspace))
            .map(|(_, set)| set);
        let (index, documents) = match previous {
            Some((old_atoms, mut index, mut documents))
                if old_atoms.len() <= atoms.len()
                    && old_atoms.iter().zip(atoms.iter()).all(|((id, _), b)| {
                        Some(id.as_str()) == b.get("id").and_then(Value::as_str)
                    }) =>
            {
                let idx = Arc::make_mut(&mut index);
                let docs = Arc::make_mut(&mut documents);
                for (ordinal, ((_, old_hash), new)) in
                    old_atoms.iter().zip(atoms.iter()).enumerate()
                {
                    if old_hash.as_str() != indexed_hash(new) {
                        let tokens = packset_core::search::atom_tokens(new);
                        idx.replace(ordinal, &docs[ordinal], &tokens);
                        docs[ordinal] = tokens;
                    }
                }
                for atom in &atoms[old_atoms.len()..] {
                    let tokens = packset_core::search::atom_tokens(atom);
                    idx.push(&tokens);
                    docs.push(tokens);
                }
                (index, documents)
            }
            _ => {
                let documents: Vec<Vec<String>> = atoms
                    .iter()
                    .map(|atom| packset_core::search::atom_tokens(atom))
                    .collect();
                let index = Index::build(documents.iter().map(Vec::as_slice));
                (Arc::new(index), Arc::new(documents))
            }
        };
        // Cached under the generation the set was read at: a reader at a
        // later generation patches from it rather than rebuilding. A scan
        // taken while a write was held is not installed; the writer swaps
        // its own set when it commits.
        if installed {
            if let Ok(mut cache) = self.terms.write() {
                cache.insert(
                    workspace.to_string(),
                    (
                        generation,
                        (
                            fingerprint(&atoms),
                            Arc::clone(&index),
                            Arc::clone(&documents),
                        ),
                    ),
                );
                self.evict_terms(&mut cache, workspace);
            }
            if let Ok(mut live) = self.live.write() {
                self.evict_cached(&mut live, workspace);
            }
        }
        Ok((atoms, index, documents, vectors))
    }

    /// The cached index when it matches the live set's generation.
    fn searchable_cached(&self, workspace: &str) -> anyhow::Result<Option<SearchSet>> {
        let snap = self.live_versioned(workspace)?;
        let cached = if let Ok(cache) = self.terms.read() {
            if let Some((seen, (_, index, documents))) = cache.get(workspace) {
                if *seen == snap.generation && snap.atoms.len() == index.len() {
                    Some((Arc::clone(index), Arc::clone(documents)))
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        Ok(cached.map(|(index, documents)| (snap.atoms, index, documents, snap.vectors)))
    }

    /// The atoms that were live at `at`, from a store scan since the snapshot
    /// drops closed windows.
    ///
    /// # Errors
    ///
    /// Fails when the scan does, or when `at` is not a timestamp.
    pub fn as_of(&self, workspace: &str, at: &str) -> anyhow::Result<Vec<Arc<Record>>> {
        let at = packset_core::clock::canonicalize(at)
            .ok_or_else(|| anyhow::anyhow!("as_of must be a timestamp"))?;
        let stored: Vec<Record> = self
            .scan_past_tombstones(workspace)?
            .into_iter()
            .filter(|atom| record::is_live_at(atom, &at))
            .collect();
        Ok(shown_from(&stored).0)
    }

    /// The live and due records in one workspace, as a copy the caller owns.
    ///
    /// # Errors
    ///
    /// Fails when the scan does.
    pub fn current(&self, workspace: &str, set: Option<&str>) -> anyhow::Result<Vec<Record>> {
        let live = self.live(workspace)?;
        Ok(live
            .iter()
            .filter(|atom| {
                set.is_none_or(|name| atom.get("set").and_then(Value::as_str) == Some(name))
            })
            .map(|atom| Record::clone(atom))
            .collect())
    }

    /// Distinct workspace names with their live counts. `global` is always in.
    ///
    /// # Errors
    ///
    /// Fails when the scan does.
    pub fn workspaces(&self) -> anyhow::Result<Vec<(String, usize)>> {
        let now = packset_core::clock::utcnow();
        let mut counts: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        // One record at a time. `scan` kept every JSON value, embeddings
        // included, for the whole pack, and a status line calls this often.
        self.for_each(None, |atom| {
            let Some(name) = atom.get("workspace").and_then(Value::as_str) else {
                return;
            };
            if name.is_empty() {
                return;
            }
            let slot = counts.entry(name.to_string()).or_insert(0);
            if record::is_live(atom, &now) {
                *slot += 1;
            }
        })?;
        // The walk parsed every record. Return what that freed, and drop
        // the file pages the walk faulted.
        release_heap();
        self.release_pages();
        counts.entry("global".into()).or_insert(0);
        Ok(counts.into_iter().collect())
    }

    /// Tombstone one live record, optionally naming the deed that withdrew it.
    ///
    /// The record is carried onto the tombstone, all but its vector, so
    /// `why` lands beside the text it retracts and a bitemporal read gets
    /// both at once. An open window closes at this write. The dated read
    /// is the half-open window, so a moment before this still has the
    /// claim and a moment after does not. A window that was already closed
    /// stays closed. A forget's tombstone has no `valid_to` and stays out
    /// of that read.
    ///
    /// # Errors
    ///
    /// [`AtomError`] when the id is not in the current set, else the write's.
    pub fn delete(&self, workspace: &str, id: &str, why: Option<&str>) -> anyhow::Result<Record> {
        let mut tomb = self
            .current(workspace, None)?
            .into_iter()
            .find(|atom| atom.get("id").and_then(Value::as_str) == Some(id))
            .ok_or_else(|| anyhow::Error::new(AtomError(format!("no current atom {id}"))))?;
        tomb.insert("tombstone".into(), Value::Bool(true));
        let now = packset_core::clock::utcnow();
        // An open window closes at this retraction. One that already ended
        // stays ended, so the claim is not live again for the stretch
        // between that end and this write. The compare is the same string
        // order `is_live` uses.
        let still_open = tomb
            .get("valid_to")
            .and_then(Value::as_str)
            .is_none_or(|until| until.is_empty() || until > now.as_str());
        if still_open {
            record::close_valid_to(&mut tomb, &now);
        }
        if let Some(accession) = why {
            tomb.insert("retracted_by".into(), Value::String(accession.to_string()));
        }
        let mut stored = self.upsert_many(std::slice::from_ref(&tomb))?;
        Ok(stored.pop().unwrap_or(tomb))
    }

    /// Drop every record in one workspace but its tombstones, scratch
    /// cleanup rather than retraction: no deed, the keys are gone. Smoke and
    /// herd runs each write a per-run workspace into the long-lived writer,
    /// so without this every run leaves its atoms behind. A tombstone stays
    /// so a deleted id is not posted again. Returns how many records were
    /// removed.
    ///
    /// # Errors
    ///
    /// Fails when the workspace is empty or the write does.
    pub fn forget_workspace(&self, workspace: &str) -> anyhow::Result<usize> {
        if workspace.is_empty() {
            anyhow::bail!("forget needs a workspace");
        }
        let prefix = workspace_prefix(workspace);
        let keys: Vec<Vec<u8>> = {
            let _gate = self.gate();
            let rtxn = self.env.read_txn()?;
            let mut keys = Vec::new();
            for item in self.db.prefix_iter(&rtxn, &prefix)? {
                let (key, raw) = item?;
                if !is_tombstone(raw) {
                    keys.push(key.to_vec());
                }
            }
            keys
        };
        // The delete, the generation, and the cache drop publish together.
        // A rebuild that saw the delete and then the snapshot from before
        // it would hand a reader a set that grew back. A reader that
        // arrives during the delete serves what is still published.
        let _writing = self.mark_writing();
        let _publish = self.publish.write().unwrap_or_else(|e| e.into_inner());
        if !keys.is_empty() {
            self.write(|wtxn| {
                for key in &keys {
                    self.db.delete(wtxn, key)?;
                }
                Ok(())
            })?;
            // After the commit, never before: as in `upsert_many`, a reader
            // scanning between a bump and its write would cache older data
            // as newer.
            self.generation.fetch_add(1, Ordering::AcqRel);
        }
        if let Ok(mut cache) = self.live.write() {
            cache.remove(workspace);
        }
        if let Ok(mut cache) = self.terms.write() {
            cache.remove(workspace);
        }
        if let Ok(mut cache) = self.counts.write() {
            cache.remove(&Some(workspace.to_string()));
            cache.remove(&None);
        }
        if let Ok(mut used) = self.cache_used.lock() {
            used.remove(workspace);
        }
        Ok(keys.len())
    }
}

/// The live set as a reader sees it: links narrowed to the ids present.
/// Whether the live set shows a record: live, or on the review clock; a
/// tombstone is neither, whatever `due_at` it kept from before it was
/// forgotten.
fn shown_at(atom: &Record, now: &str) -> bool {
    !atom
        .get("tombstone")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        && (record::is_live(atom, now) || record::is_due(atom, now))
}

/// The shown copy of a stored set, links cut to live ids, and the ids the
/// cuts named: the targets a later arrival may restore.
fn shown_from(stored: &[Record]) -> (Vec<Arc<Record>>, HashSet<String>) {
    let live: HashSet<&str> = stored
        .iter()
        .filter_map(|a| a.get("id").and_then(Value::as_str))
        .collect();
    let mut dangling = HashSet::new();
    let shown = stored
        .iter()
        .map(|atom| {
            let mut copy = atom.clone();
            dangling.extend(cut_links(&mut copy, &live));
            Arc::new(copy)
        })
        .collect();
    (shown, dangling)
}

/// Fold one write into the shown copy without rebuilding it: the written
/// records are replaced, removed or appended with their links cut to live
/// ids. A record whose link was cut because its target was absent is
/// re-derived when that target arrives, and every record naming a departed
/// id is re-derived when it departs; `dangling` is the set of ids that cut
/// links name, so an arrival nobody named costs no scan. Equal to
/// `shown_from(stored)`; a shown copy another reader still holds is cloned
/// once by `Arc::make_mut`, an unshared one is edited in place.
fn patch_shown(
    shown: &mut Live,
    stored: &[Record],
    written: &[Record],
    now: &str,
    dangling: &mut HashSet<String>,
) {
    let live: HashSet<&str> = stored
        .iter()
        .filter_map(|a| a.get("id").and_then(Value::as_str))
        .collect();
    let out = Arc::make_mut(shown);
    let mut moved: Vec<String> = Vec::new();
    for record in written {
        let Some(id) = record.get("id").and_then(Value::as_str) else {
            continue;
        };
        let at = out
            .iter()
            .position(|a| a.get("id").and_then(Value::as_str) == Some(id));
        if live.contains(id) && shown_at(record, now) {
            let mut copy = record.clone();
            dangling.extend(cut_links(&mut copy, &live));
            match at {
                Some(i) => out[i] = Arc::new(copy),
                None => {
                    out.push(Arc::new(copy));
                    if dangling.remove(id) {
                        moved.push(id.to_string());
                    }
                }
            }
        } else {
            if let Some(i) = at {
                out.remove(i);
            }
            dangling.insert(id.to_string());
            moved.push(id.to_string());
        }
    }
    if moved.is_empty() {
        return;
    }
    for atom in stored {
        let names_moved = atom
            .get("links")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items
                    .iter()
                    .any(|item| moved.contains(&record::value_text(item)))
            });
        if !names_moved {
            continue;
        }
        let Some(id) = atom.get("id").and_then(Value::as_str) else {
            continue;
        };
        if let Some(i) = out
            .iter()
            .position(|a| a.get("id").and_then(Value::as_str) == Some(id))
        {
            let mut copy = atom.clone();
            dangling.extend(cut_links(&mut copy, &live));
            out[i] = Arc::new(copy);
        }
    }
}

/// Keep only the links that name a live id; the ids of the links cut are
/// returned, since an arrival of one of them restores the link.
fn cut_links(atom: &mut Record, live: &HashSet<&str>) -> Vec<String> {
    let mut cut = Vec::new();
    let kept: Vec<Value> = atom
        .get("links")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| {
                    let id = record::value_text(item);
                    if live.contains(id.as_str()) {
                        true
                    } else {
                        cut.push(id);
                        false
                    }
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    atom.insert("links".into(), Value::Array(kept));
    cut
}

/// Whether a stored value is a tombstone. Its id stays retired, so
/// [`Store::forget_workspace`] keeps it.
fn is_tombstone(raw: &[u8]) -> bool {
    serde_json::from_slice::<Value>(raw)
        .ok()
        .and_then(|record| record.get("tombstone").and_then(Value::as_bool))
        .unwrap_or(false)
}

/// A record as the cache keeps it, and the vector that left it.
struct Parked {
    lean: Record,
    vector: Option<Arc<[f32]>>,
    original: Option<Arc<[serde_json::Number]>>,
    /// The write named `embedding`, so a null clears a vector the cache held.
    named: bool,
}

/// Lift `embedding` out of a clone of `record`. An array of numbers becomes
/// floats and the key leaves the clone. A null on the clone would be the
/// clear a client sends, and a fire copies this record back on a rewrite.
/// Anything that is not a vector stays on the clone.
fn park(record: &Record) -> Parked {
    let mut lean = record.clone();
    let named = lean.contains_key("embedding");
    let (vector, original) = match lean.remove("embedding") {
        Some(Value::Array(items)) if !items.is_empty() => {
            match (floats_of(&items), numbers_of(&items)) {
                (Some(parsed), Some(original)) => (Some(Arc::from(parsed)), Some(original)),
                _ => {
                    lean.insert("embedding".into(), Value::Array(items));
                    (None, None)
                }
            }
        }
        Some(other) => {
            lean.insert("embedding".into(), other);
            (None, None)
        }
        None => (None, None),
    };
    Parked {
        lean,
        vector,
        original,
        named,
    }
}

fn floats_of(items: &[Value]) -> Option<Box<[f32]>> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        out.push(item.as_f64()? as f32);
    }
    Some(out.into_boxed_slice())
}

/// The JSON numbers as parsed. Scoring uses f32; an answer copies these,
/// so 0.1 stays 0.1 and a value f32 cannot hold does not become null.
fn numbers_of(items: &[Value]) -> Option<Arc<[serde_json::Number]>> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        out.push(item.as_number()?.clone());
    }
    Some(Arc::from(out))
}

/// Put the stored JSON numbers back on `atom` for an answer. A record
/// with no cached vector is left as it is.
pub(crate) fn restore_embedding(
    atom: &mut Record,
    originals: &HashMap<String, Arc<[serde_json::Number]>>,
) {
    let Some(id) = atom.get("id").and_then(Value::as_str) else {
        return;
    };
    let Some(numbers) = originals.get(id) else {
        return;
    };
    atom.insert(
        "embedding".into(),
        Value::Array(numbers.iter().cloned().map(Value::Number).collect()),
    );
}

/// A rewrite that did not name `embedding` keeps the array already stored.
/// Null and `[]` are a clear. Restoring over those made a vector impossible
/// to drop, and an update of the text then kept the old meaning.
fn keep_stored_embedding(payload: &mut Record, stored: Option<&Record>) {
    if payload.contains_key("embedding") {
        return;
    }
    let Some(existing) = stored.and_then(|row| row.get("embedding")) else {
        return;
    };
    if existing.as_array().is_some_and(|items| !items.is_empty()) {
        payload.insert("embedding".into(), existing.clone());
    }
}

fn push_record(out: &mut Vec<Record>, raw: &[u8]) {
    if let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(raw) {
        out.push(map);
    }
}

/// Cut `data.mdb` back to the last page in use.
///
/// Pages at and below the last used page hold the database, free pages
/// included. Bytes past that page are the unused tail of a map-sized file.
/// Dropping them returns the disk space; the next write extends the file
/// again, and grows the map when the file reaches it.
///
/// A `lock.mdb` slot owned by another process that is still running keeps
/// the tail. `mdb_stat` maps the file for the length of its read, and
/// shrinking it then faults that process. A slot left by a process that
/// has exited is not a reader: the table keeps it until a later check,
/// and that process is not mapping the file.
fn trim_to_used_pages(env: &Env, db: Database<Bytes, Bytes>, dir: &Path) -> anyhow::Result<()> {
    if another_process_is_reading(dir) {
        return Ok(());
    }
    let info = env.info();
    let page = {
        let rtxn = env.read_txn()?;
        db.stat(&rtxn)?.page_size as u64
    };
    if page == 0 {
        return Ok(());
    }
    let used = (info.last_page_number as u64)
        .saturating_add(1)
        .saturating_mul(page);
    let path = dir.join("data.mdb");
    let file = fs::OpenOptions::new().write(true).open(&path)?;
    let len = file.metadata()?.len();
    if len > used {
        file.set_len(used)?;
    }
    Ok(())
}

/// `true` when `lock.mdb` lists a reader in a process other than this one.
///
/// The table is the one LMDB writes for its POSIX-mutex build, which is
/// how `lmdb-master-sys` is built here: a 64-byte header, a 64-byte writer
/// lock, then one 64-byte slot per reader. The process id sits 8 bytes
/// into the slot. A missing lock file means no reader. A lock file this
/// build cannot read keeps the tail, rather than shrinking under a reader
/// the table failed to name.
fn another_process_is_reading(dir: &Path) -> bool {
    #[cfg(unix)]
    {
        match fs::read(dir.join("lock.mdb")) {
            Ok(bytes) => lock_lists_other_live_reader(&bytes, std::process::id()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => true,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        false
    }
}

/// Magic at the start of an LMDB lock file (`MDB_MAGIC`).
#[cfg(unix)]
const LOCK_MAGIC: u32 = 0xBEEF_C0DE;
/// Reader slots begin after the header and the writer lock, one cache line each.
#[cfg(unix)]
const LOCK_READERS_AT: usize = 128;
#[cfg(unix)]
const LOCK_SLOT: usize = 64;
/// `mrb_pid` follows the 8-byte transaction id in the slot.
#[cfg(unix)]
const LOCK_PID_AT: usize = 8;
/// `mtb_numreaders`, the high-water mark of slots, not the live count.
#[cfg(unix)]
const LOCK_COUNT_AT: usize = 16;

#[cfg(unix)]
fn lock_lists_other_live_reader(bytes: &[u8], self_pid: u32) -> bool {
    let Some(magic) = ne_u32(bytes, 0) else {
        return true;
    };
    if magic != LOCK_MAGIC {
        return true;
    }
    let Some(count) = ne_u32(bytes, LOCK_COUNT_AT) else {
        return true;
    };
    let slots = bytes.len().saturating_sub(LOCK_READERS_AT) / LOCK_SLOT;
    let count = (count as usize).min(slots);
    for i in 0..count {
        let at = LOCK_READERS_AT + i * LOCK_SLOT + LOCK_PID_AT;
        let Some(pid) = ne_i32(bytes, at) else {
            break;
        };
        if pid > 0 && pid as u32 != self_pid && process_alive(pid) {
            return true;
        }
    }
    false
}

#[cfg(unix)]
fn ne_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let end = at.checked_add(4)?;
    let raw: [u8; 4] = bytes.get(at..end)?.try_into().ok()?;
    Some(u32::from_ne_bytes(raw))
}

#[cfg(unix)]
fn ne_i32(bytes: &[u8], at: usize) -> Option<i32> {
    ne_u32(bytes, at).map(|n| n as i32)
}

#[cfg(unix)]
fn process_alive(pid: i32) -> bool {
    // SAFETY: signal 0 delivers nothing. It reports whether `pid` names a
    // process this one is allowed to signal.
    let rc = unsafe { kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(EPERM)
}

#[cfg(unix)]
const EPERM: i32 = 1;

#[cfg(unix)]
extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

/// Create the home, or narrow an existing one, so only its owner can enter
/// it. The atoms, the cards and the search index sit under it in the clear;
/// the owner token guards the port, and this guards the disk.
fn private_home(root: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        if let Some(parent) = root.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        if !root.is_dir() {
            fs::DirBuilder::new().mode(0o700).create(root)?;
        }
        let mode = fs::metadata(root)?.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            // A home this user does not own stays as it is, with a warning:
            // refusing to start would lock the owner out of their own pack.
            if let Err(err) = fs::set_permissions(root, fs::Permissions::from_mode(0o700)) {
                eprintln!(
                    "packsetd: {} is open to other users ({mode:o}) and could not be narrowed: {err}",
                    root.display()
                );
            }
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(root)
    }
}

/// Take the exclusive lock, or say who has it. One writer per `memory.lmdb`.
fn take_lock(path: &Path) -> anyhow::Result<HomeLock> {
    use std::os::fd::AsRawFd;
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    // SAFETY: a libc call on a fd this function owns.
    let taken = unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) };
    if taken != 0 {
        anyhow::bail!("store home is already open");
    }
    Ok(HomeLock(file))
}

/// The `flock` on `packsetd.lock`, released on drop.
///
/// The lock belongs to the open file description, and a child forked while
/// the store is open shares that description until it execs. Closing this
/// descriptor alone leaves the home locked for as long as such a child
/// runs; unlocking releases it whoever else holds the description.
struct HomeLock(File);

impl Drop for HomeLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // SAFETY: a libc call on a fd this lock owns.
        unsafe { flock(self.0.as_raw_fd(), LOCK_UN) };
    }
}

// `<sys/file.h>`; the same values on Linux, macOS and the BSDs.
const LOCK_EX: i32 = 2;
const LOCK_NB: i32 = 4;
const LOCK_UN: i32 = 8;

extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
}

/// How long a burst of writes waits between heap releases.
const WRITE_RELEASE_MS: u64 = 50;

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Ask glibc on Linux to return free arenas to the kernel. A burst
/// leaves that high-water mark in per-thread arenas until asked.
/// musl and the macOS allocator return large freed regions on their
/// own; what stays is fragmentation from small allocations.
/// `malloc_trim` is not linked elsewhere: windows-gnu shares
/// `target_env = "gnu"` and does not provide this symbol.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub(crate) fn release_heap() {
    // SAFETY: trim does not move live allocations.
    let _ = unsafe { malloc_trim(0) };
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub(crate) fn release_heap() {}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
extern "C" {
    fn malloc_trim(pad: usize) -> i32;
}

/// `MADV_DONTNEED`.
#[cfg(target_os = "linux")]
const MADV_DONTNEED: i32 = 4;

#[cfg(target_os = "linux")]
extern "C" {
    fn madvise(addr: *mut std::ffi::c_void, length: usize, advice: i32) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(value: Value) -> Record {
        value.as_object().unwrap().clone()
    }

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        (dir, store)
    }

    #[cfg(unix)]
    #[test]
    fn the_home_is_closed_to_other_users() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let fresh = dir.path().join("new").join("home");
        drop(Store::open(&fresh).unwrap());
        assert_eq!(mode(&fresh), 0o700, "a new home");
        let old = dir.path().join("old");
        fs::create_dir(&old).unwrap();
        fs::set_permissions(&old, fs::Permissions::from_mode(0o755)).unwrap();
        drop(Store::open(&old).unwrap());
        assert_eq!(mode(&old), 0o700, "a home made before this was narrowed");
    }

    #[cfg(unix)]
    fn allocated_bytes(path: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(path).unwrap().blocks() * 512
    }

    /// A fresh store with a few atoms stays a handful of pages, counted as
    /// allocated, `st_blocks * 512`, which a file extended to its map fills.
    #[cfg(unix)]
    #[test]
    fn a_few_atoms_allocate_far_less_than_the_map() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        for (id, text) in [
            ("one", "A claim."),
            ("two", "Another claim."),
            ("three", "A third, so the file has room to grow."),
        ] {
            store
                .upsert(&record(json!({"id": id, "workspace": "w", "text": text})))
                .unwrap();
        }
        assert_eq!(
            store.get("w", "three").unwrap().unwrap()["text"],
            json!("A third, so the file has room to grow.")
        );
        drop(store);
        let path = dir.path().join("memory.lmdb/data.mdb");
        let allocated = allocated_bytes(&path);
        assert!(
            allocated < 64 * 1024 * 1024,
            "three claims allocated {allocated} bytes"
        );
    }

    /// A store whose file already fills the map still opens, keeps its
    /// claims, accepts another write, and gives the unused tail back.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_fully_allocated_map_still_opens() {
        use std::os::fd::AsRawFd;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.lmdb/data.mdb");
        {
            let store = Store::open(dir.path()).unwrap();
            store
                .upsert(&record(
                    json!({"id": "one", "workspace": "w", "text": "kept"}),
                ))
                .unwrap();
        }
        {
            let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
            // SAFETY: `file` is a writable descriptor for this test's data
            // file, and the length is the map the old writer extended to.
            let rc = unsafe { libc::posix_fallocate(file.as_raw_fd(), 0, MAP_SIZE as libc::off_t) };
            assert_eq!(rc, 0, "could not allocate the old map");
        }
        let before = allocated_bytes(&path);
        assert!(
            before > 64 * 1024 * 1024,
            "the fixture did not allocate the map ({before} bytes)"
        );
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(
            store.get("w", "one").unwrap().unwrap()["text"],
            json!("kept")
        );
        store
            .upsert(&record(
                json!({"id": "two", "workspace": "w", "text": "added"}),
            ))
            .unwrap();
        assert_eq!(
            store.get("w", "two").unwrap().unwrap()["text"],
            json!("added"),
            "the map still grows"
        );
        drop(store);
        let after = allocated_bytes(&path);
        assert!(
            after < 64 * 1024 * 1024,
            "opened store still holds {after} bytes (was {before})"
        );
    }

    /// A second process in `lock.mdb`, the way `mdb_stat` sits there, keeps
    /// the allocated tail. Once that process leaves, the next open cuts it.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_foreign_reader_keeps_the_tail() {
        if let Ok(home) = std::env::var("PACKSET_FOREIGN_READER") {
            hold_reader(Path::new(&home));
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.lmdb/data.mdb");
        {
            let store = Store::open(dir.path()).unwrap();
            store
                .upsert(&record(
                    json!({"id": "one", "workspace": "w", "text": "kept"}),
                ))
                .unwrap();
        }
        {
            use std::os::fd::AsRawFd;
            let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
            // SAFETY: `file` is this test's data file, and the length is the
            // map an older writer extended to.
            let rc = unsafe { libc::posix_fallocate(file.as_raw_fd(), 0, MAP_SIZE as libc::off_t) };
            assert_eq!(rc, 0, "could not allocate the old map");
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "store::tests::a_foreign_reader_keeps_the_tail"])
            .env("PACKSET_FOREIGN_READER", dir.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .unwrap();
        let ready = dir.path().join("reader-ready");
        let started = std::time::Instant::now();
        while !ready.exists() {
            if started.elapsed() > std::time::Duration::from_secs(15) {
                let _ = child.kill();
                panic!("reader did not take a slot: {:?}", child.wait());
            }
            if let Some(status) = child.try_wait().unwrap() {
                panic!("reader exited before its slot was published: {status}");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // A fresh thread: this thread already opened and closed the env, and
        // LMDB's thread-local reader slot still points at that lock table.
        // With another process in the table the slot cannot be reused here.
        // A process that is starting, which is when this trim runs, has none.
        let home = dir.path().to_path_buf();
        let data = path.clone();
        let during = std::thread::spawn(move || {
            let store = Store::open(&home).unwrap();
            let text = store.get("w", "one").unwrap().unwrap()["text"].clone();
            drop(store);
            (text, allocated_bytes(&data))
        })
        .join()
        .unwrap();
        assert_eq!(during.0, json!("kept"));
        assert!(
            during.1 > 64 * 1024 * 1024,
            "trimmed under a foreign reader ({} bytes)",
            during.1
        );
        drop(child.stdin.take());
        let status = child.wait().unwrap();
        assert!(status.success(), "reader did not leave cleanly: {status}");
        let home = dir.path().to_path_buf();
        let data = path.clone();
        let after = std::thread::spawn(move || {
            let store = Store::open(&home).unwrap();
            let text = store.get("w", "one").unwrap().unwrap()["text"].clone();
            drop(store);
            assert_eq!(text, json!("kept"));
            allocated_bytes(&data)
        })
        .join()
        .unwrap();
        assert!(
            after < 64 * 1024 * 1024,
            "tail stayed allocated after the reader left ({after} bytes)"
        );
    }

    #[cfg(target_os = "linux")]
    fn hold_reader(home: &Path) {
        use std::io::Read;
        let env = unsafe {
            EnvOpenOptions::new()
                .map_size(MAP_SIZE)
                .max_dbs(1)
                .open(home.join("memory.lmdb"))
                .expect("reader env")
        };
        let rtxn = env.read_txn().expect("reader txn");
        let db = env
            .open_database::<Bytes, Bytes>(&rtxn, None)
            .expect("reader db")
            .expect("unnamed db");
        let _ = db.stat(&rtxn).expect("reader stat");
        fs::write(home.join("reader-ready"), b"1").unwrap();
        let mut buf = [0u8; 1];
        let _ = std::io::stdin().read(&mut buf);
        drop(rtxn);
        drop(env);
    }

    #[test]
    fn the_key_separates_on_a_nul() {
        assert_eq!(atom_key("w", "a"), b"w\0a".to_vec());
        assert_eq!(workspace_prefix("w"), b"w\0".to_vec());
        // A workspace whose name is a prefix of another must not leak into it,
        // which is what the separator buys.
        assert!(!atom_key("wide", "a").starts_with(&workspace_prefix("w")));
    }

    #[test]
    fn a_record_round_trips() {
        let (_dir, store) = store();
        let atom = record(json!({
            "id": "one", "workspace": "w", "kind": "voice",
            "text": "A claim.", "links": ["two"], "unmodelled": {"x": 1}
        }));
        store.upsert(&atom).unwrap();
        let back = store.get("w", "one").unwrap().unwrap();
        assert_eq!(back["text"], json!("A claim."));
        assert_eq!(back["links"], json!(["two"]));
        assert_eq!(back["unmodelled"], json!({"x": 1}), "fields survive");
    }

    #[test]
    fn a_rewrite_keeps_the_time_the_atom_was_written() {
        let (_dir, store) = store();
        let written = "2026-10-01T09:00:00.000Z";
        store
            .upsert(&record(json!({
                "id": "a", "workspace": "w", "text": "alpha", "ts": written,
                "valid_from": written
            })))
            .unwrap();
        let first = store.get("w", "a").unwrap().unwrap();
        assert_eq!(first["ts"], json!(written));
        let stamped = first[UPDATED].as_str().unwrap().to_string();
        // A rewrite carrying a later ts, as the link and review paths did.
        store
            .upsert(&record(json!({
                "id": "a", "workspace": "w", "text": "alpha beta",
                "ts": "2026-10-18T19:31:19.444Z", "valid_from": written
            })))
            .unwrap();
        let back = store.get("w", "a").unwrap().unwrap();
        assert_eq!(back["ts"], json!(written), "a rewrite moved ts");
        assert_eq!(back["text"], json!("alpha beta"));
        assert!(back[UPDATED].as_str().unwrap() >= stamped.as_str());
        let tomb = store.delete("w", "a", None).unwrap();
        assert_eq!(tomb["ts"], json!(written), "a tombstone moved ts");
        assert_eq!(store.get("w", "a").unwrap().unwrap()["ts"], json!(written));
    }

    #[test]
    fn a_record_from_before_updated_gets_its_written_time_back_from_the_pass() {
        let dir = tempfile::tempdir().unwrap();
        let rewritten = "2026-10-18T19:31:19.444Z";
        let rows = [
            // Restamped by a link rewrite; valid_from kept the write.
            json!({"id": "moved", "workspace": "w", "text": "a",
                   "ts": rewritten, "valid_from": "2026-10-16T10:07:21.644Z",
                   "source": {"at": "2026-10-16T10:07:21Z"}}),
            // As `moved`, and written again before the pass reaches it.
            json!({"id": "early", "workspace": "w", "text": "f",
                   "ts": rewritten, "valid_from": "2026-10-16T10:07:21.644Z"}),
            // Never rewritten: ts and valid_from agree.
            json!({"id": "kept", "workspace": "w", "text": "b",
                   "ts": "2026-10-12T08:00:00.000Z",
                   "valid_from": "2026-10-12T08:00:00.000Z"}),
            // No valid_from; source.at is the writer's clock.
            json!({"id": "dated", "workspace": "w", "text": "c",
                   "ts": rewritten, "source": {"at": "2026-10-14T14:15:41Z"}}),
            // A source.at past ts is not the write; valid_from is.
            json!({"id": "ahead", "workspace": "w", "text": "e",
                   "ts": rewritten, "valid_from": "2026-10-18T19:00:00.000Z",
                   "source": {"at": "2026-10-19T00:00:00Z"}}),
            // No stamp to go back to.
            json!({"id": "bare", "workspace": "w", "text": "d", "ts": rewritten}),
        ];
        {
            let store = Store::open(dir.path()).unwrap();
            let mut wtxn = store.env.write_txn().unwrap();
            for row in &rows {
                let key = atom_key("w", row["id"].as_str().unwrap());
                store
                    .db
                    .put(&mut wtxn, &key, &serde_json::to_vec(row).unwrap())
                    .unwrap();
            }
            wtxn.commit().unwrap();
        }
        let store = Store::open(dir.path()).unwrap();
        let got = |id: &str| store.get("w", id).unwrap().unwrap();
        assert_eq!(got("moved")["ts"], json!(rewritten), "the open stamped");
        assert!(!got("moved").contains_key(UPDATED), "the open stamped");
        store
            .upsert(&record(
                json!({"id": "early", "workspace": "w", "text": "g",
                                   "ts": "2026-10-20T00:00:00.000Z"}),
            ))
            .unwrap();
        assert_eq!(got("early")["ts"], json!("2026-10-16T10:07:21.644Z"));
        assert_eq!(store.restore_created().unwrap(), 5);
        assert_eq!(got("early")["text"], json!("g"), "the pass undid a write");
        assert_eq!(got("moved")["ts"], json!("2026-10-16T10:07:21.644Z"));
        assert_eq!(got("moved")[UPDATED], json!(rewritten));
        assert_eq!(got("kept")["ts"], json!("2026-10-12T08:00:00.000Z"));
        assert_eq!(got("kept")[UPDATED], json!("2026-10-12T08:00:00.000Z"));
        assert_eq!(got("dated")["ts"], json!("2026-10-14T14:15:41.000Z"));
        assert_eq!(got("ahead")["ts"], json!("2026-10-18T19:00:00.000Z"));
        assert_eq!(got("bare")["ts"], json!(rewritten));
        assert_eq!(got("bare")[UPDATED], json!(rewritten));
        let before: Vec<Record> = store.scan(Some("w")).unwrap();
        assert_eq!(store.restore_created().unwrap(), 0);
        assert_eq!(
            store.scan(Some("w")).unwrap(),
            before,
            "a second pass rewrote"
        );
    }

    #[test]
    fn a_scan_is_scoped_to_one_workspace() {
        let (_dir, store) = store();
        for (ws, id) in [("w", "a"), ("w", "b"), ("wide", "c")] {
            store
                .upsert(&record(json!({"id": id, "workspace": ws, "text": id})))
                .unwrap();
        }
        let mine = store.scan(Some("w")).unwrap();
        assert_eq!(mine.len(), 2, "{mine:?}");
        assert_eq!(store.scan(Some("wide")).unwrap().len(), 1);
        assert_eq!(store.scan(None).unwrap().len(), 3);
        assert!(store.scan(Some("")).unwrap().is_empty());
    }

    #[test]
    fn status_counts_follow_writes() {
        // `status` reads these, not a fresh scan: zeros before the write,
        // the write visible after, each workspace and the whole home
        // counted on its own.
        let (_dir, store) = store();
        let sum = |c: &Counts| c.live.values().sum::<usize>();
        assert_eq!(sum(&store.counts(Some("w")).unwrap()), 0);
        store
            .upsert(&record(
                json!({"id": "a", "workspace": "w", "kind": "lesson", "text": "A."}),
            ))
            .unwrap();
        let one = store.counts(Some("w")).unwrap();
        assert_eq!(one.live.get("lesson"), Some(&1));
        assert_eq!(sum(&one), 1);
        assert_eq!(sum(&store.counts(Some("w")).unwrap()), 1);
        assert_eq!(sum(&store.counts(Some("x")).unwrap()), 0);
        let all = store.counts(None).unwrap();
        assert_eq!(all.live.get("lesson"), Some(&1));
    }

    #[test]
    fn current_drops_the_expired_and_keeps_the_due() {
        let (_dir, store) = store();
        store
            .upsert(&record(
                json!({"id": "live", "workspace": "w", "text": "a"}),
            ))
            .unwrap();
        store
            .upsert(&record(json!({
                "id": "gone", "workspace": "w", "text": "b",
                "valid_to": "2000-01-01T00:00:00.000Z"
            })))
            .unwrap();
        // Expired for the live set but still on the review clock, which is a
        // different question and keeps it in reach.
        store
            .upsert(&record(json!({
                "id": "due", "workspace": "w", "text": "c",
                "valid_to": "2000-01-01T00:00:00.000Z",
                "due_at": "2000-01-01T00:00:00.000Z"
            })))
            .unwrap();
        let ids: Vec<String> = store
            .current("w", None)
            .unwrap()
            .iter()
            .map(|a| a["id"].as_str().unwrap().to_string())
            .collect();
        assert!(ids.contains(&"live".to_string()), "{ids:?}");
        assert!(ids.contains(&"due".to_string()), "{ids:?}");
        assert!(!ids.contains(&"gone".to_string()), "{ids:?}");
    }

    #[test]
    fn as_of_returns_what_was_live_then() {
        let (_dir, store) = store();
        store
            .upsert(&record(json!({
                "id": "then", "workspace": "w", "text": "old claim",
                "valid_from": "2024-01-01T00:00:00.000Z",
                "valid_to": "2024-12-01T00:00:00.000Z"
            })))
            .unwrap();
        store
            .upsert(&record(json!({
                "id": "now", "workspace": "w", "text": "new claim",
                "valid_from": "2024-12-01T00:00:00.000Z"
            })))
            .unwrap();
        store
            .upsert(&record(json!({
                "id": "tomb", "workspace": "w", "text": "deleted",
                "valid_from": "2024-01-01T00:00:00.000Z",
                "tombstone": true
            })))
            .unwrap();
        let mid = store.as_of("w", "2024-06-01T00:00:00.000Z").unwrap();
        let mid_ids: Vec<&str> = mid.iter().filter_map(|a| a["id"].as_str()).collect();
        assert_eq!(mid_ids, vec!["then"], "{mid:?}");
        let offset = store.as_of("w", "2024-06-01T00:00:00+00:00").unwrap();
        let offset_ids: Vec<&str> = offset.iter().filter_map(|a| a["id"].as_str()).collect();
        assert_eq!(offset_ids, mid_ids, "offset and Z as_of agree");
        let today = store.as_of("w", "2025-06-01T00:00:00.000Z").unwrap();
        let today_ids: Vec<&str> = today.iter().filter_map(|a| a["id"].as_str()).collect();
        assert_eq!(today_ids, vec!["now"], "{today:?}");
        assert!(
            store
                .current("w", None)
                .unwrap()
                .iter()
                .all(|a| a["id"] != json!("then")),
            "live-now still drops the closed window"
        );
    }

    #[test]
    fn current_narrows_links_to_what_it_returned() {
        let (_dir, store) = store();
        store
            .upsert(&record(json!({
                "id": "a", "workspace": "w", "text": "a", "links": ["b", "gone"]
            })))
            .unwrap();
        store
            .upsert(&record(json!({"id": "b", "workspace": "w", "text": "b"})))
            .unwrap();
        let live = store.current("w", None).unwrap();
        let a = live.iter().find(|x| x["id"] == json!("a")).unwrap();
        assert_eq!(a["links"], json!(["b"]), "a dangling link is not returned");
    }

    #[test]
    fn a_set_scope_filters_the_current_view() {
        let (_dir, store) = store();
        store
            .upsert(&record(
                json!({"id": "a", "workspace": "w", "text": "a", "set": "review"}),
            ))
            .unwrap();
        store
            .upsert(&record(json!({"id": "b", "workspace": "w", "text": "b"})))
            .unwrap();
        assert_eq!(store.current("w", Some("review")).unwrap().len(), 1);
        assert_eq!(store.current("w", None).unwrap().len(), 2);
    }

    /// A write while a reader holds the live set copies pointers, not records.
    #[test]
    fn a_write_under_a_reader_shares_the_records_it_did_not_touch() {
        let (_dir, store) = store();
        for id in ["a", "b", "c"] {
            store
                .upsert(&record(json!({"id": id, "workspace": "w", "text": id})))
                .unwrap();
        }
        let held = store.live("w").unwrap();
        store
            .upsert(&record(json!({"id": "d", "workspace": "w", "text": "d"})))
            .unwrap();
        let next = store.live("w").unwrap();
        assert_eq!((held.len(), next.len()), (3, 4));
        for (before, after) in held.iter().zip(next.iter()) {
            assert!(
                Arc::ptr_eq(before, after),
                "{:?} was copied",
                before.get("id")
            );
        }
    }

    /// Overlapping statuses share one count.
    #[test]
    fn overlapping_statuses_after_a_write_count_once() {
        let (_dir, store) = store();
        for n in 0..200 {
            store
                .upsert(&record(
                    json!({"id": format!("a{n}"), "workspace": "w", "text": "x"}),
                ))
                .unwrap();
        }
        let before = store.counted.load(Ordering::SeqCst);
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| store.counts(Some("w")).unwrap());
            }
        });
        assert_eq!(store.counted.load(Ordering::SeqCst) - before, 1);
    }

    /// The cache holds one float vector. The file keeps the JSON array, and
    /// a rewrite that does not send the array does not erase it.
    #[test]
    fn a_cached_atom_keeps_its_vector_once_as_floats() {
        let (_dir, store) = store();
        let embedding: Vec<f64> = (0..384).map(|i| f64::from(i) / 384.0).collect();
        store
            .upsert(&record(json!({
                "id": "a",
                "workspace": "w",
                "text": "a lesson about vectors",
                "embedding": embedding,
            })))
            .unwrap();
        let live = store.live("w").unwrap();
        assert!(
            live[0].get("embedding").is_none(),
            "the cache kept the JSON array"
        );
        let cached = store.vectors("w");
        let got = cached.get("a").expect("the vector was dropped");
        assert_eq!(got.len(), 384);
        assert!((got[1] - (1.0 / 384.0)).abs() < 1e-6);
        let disk = store.get("w", "a").unwrap().unwrap();
        assert_eq!(disk["embedding"].as_array().unwrap().len(), 384);
        store
            .upsert(&record(json!({
                "id": "a",
                "workspace": "w",
                "text": "a lesson about vectors revised",
            })))
            .unwrap();
        let disk = store.get("w", "a").unwrap().unwrap();
        assert_eq!(disk["text"], json!("a lesson about vectors revised"));
        assert_eq!(disk["embedding"].as_array().unwrap().len(), 384);
        assert_eq!(store.vectors("w").get("a").unwrap().len(), 384);
        assert!(store.live("w").unwrap()[0].get("embedding").is_none());
        // The second write reused the float buffer when the numbers matched.
        assert!(Arc::ptr_eq(got, store.vectors("w").get("a").unwrap()));
    }

    /// Null and an empty array are a clear. Omitting the key is not.
    #[test]
    fn a_null_or_empty_embedding_clears_the_stored_vector() {
        let (_dir, store) = store();
        let write = |embedding: Value| {
            store
                .upsert(&record(json!({
                    "id": "a",
                    "workspace": "w",
                    "text": "a lesson about vectors",
                    "embedding": embedding,
                })))
                .unwrap();
        };
        write(json!([0.2, 0.4, 0.6]));
        let snap = store.live_versioned("w").unwrap();
        assert_eq!(snap.vectors.get("a").map(|v| v.len()), Some(3));
        write(Value::Null);
        let disk = store.get("w", "a").unwrap().unwrap();
        assert!(disk["embedding"].is_null(), "{disk:?}");
        assert!(store
            .live_versioned("w")
            .unwrap()
            .vectors
            .get("a")
            .is_none());
        write(json!([0.2, 0.4, 0.6]));
        write(json!([]));
        let disk = store.get("w", "a").unwrap().unwrap();
        assert_eq!(disk["embedding"], json!([]));
        assert!(store
            .live_versioned("w")
            .unwrap()
            .vectors
            .get("a")
            .is_none());
    }

    /// A client vector is answered as stored. f32 would turn 0.1 into its
    /// approximation and 1e39 into null.
    #[test]
    fn an_answer_returns_the_numbers_that_were_stored() {
        let (dir, store) = store();
        let raw = json!([0.1, 1e39, -0.3]);
        store
            .upsert(&record(json!({
                "id": "a",
                "workspace": "w",
                "text": "a client vector",
                "embedding": raw,
            })))
            .unwrap();
        let check = |store: &Store| {
            let snap = store.live_versioned("w").unwrap();
            let mut atom = Record::clone(&snap.atoms[0]);
            restore_embedding(&mut atom, &snap.originals);
            assert_eq!(atom.get("embedding"), Some(&json!([0.1, 1e39, -0.3])));
            assert!(atom["embedding"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item.as_f64().is_some()));
            let disk = store.get("w", "a").unwrap().unwrap();
            assert_eq!(disk.get("embedding"), atom.get("embedding"));
        };
        check(&store);
        drop(store);
        check(&Store::open(dir.path()).unwrap());
    }

    /// Advising the database cold and trimming the heap leaves a claim readable.
    #[test]
    fn releasing_idle_memory_leaves_the_claim_readable() {
        let (_dir, store) = store();
        store
            .upsert(&record(
                json!({"id": "a", "workspace": "w", "text": "a", "kind": "lesson"}),
            ))
            .unwrap();
        #[cfg(target_os = "linux")]
        let path = store.db_dir.join("data.mdb");
        #[cfg(target_os = "linux")]
        let before = mapping_rss_kib(&path);
        store.release_idle();
        #[cfg(target_os = "linux")]
        {
            let after = mapping_rss_kib(&path);
            assert!(
                before > after,
                "database mapping stayed resident ({before} KiB, then {after})"
            );
        }
        assert_eq!(store.get("w", "a").unwrap().unwrap()["text"], json!("a"));
    }

    /// Resident size of one mapping, from `smaps`. Zero when it is not mapped.
    #[cfg(target_os = "linux")]
    fn mapping_rss_kib(path: &Path) -> u64 {
        let Ok(text) = fs::read_to_string("/proc/self/smaps") else {
            return 0;
        };
        let needle = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let needle = needle.to_string_lossy();
        let mut rss = 0;
        let mut matched = false;
        for line in text.lines() {
            if line.contains(needle.as_ref()) {
                matched = true;
                continue;
            }
            if !matched {
                continue;
            }
            if let Some(rest) = line.strip_prefix("Rss:") {
                rss += rest
                    .split_whitespace()
                    .next()
                    .unwrap_or("0")
                    .parse::<u64>()
                    .unwrap_or(0);
                matched = false;
            } else if line.contains('-') {
                matched = false;
            }
        }
        rss
    }

    /// Sixty-four cached 384-d vectors are floats. The JSON arrays they
    /// replaced would have been a few mebibytes. RssAnon is the whole
    /// process, so the measurement runs in a child: a neighbour in the
    /// same test binary charges its own allocations to this number.
    #[cfg(target_os = "linux")]
    #[test]
    fn cached_vectors_do_not_keep_the_json_arrays() {
        if std::env::var_os("PACKSET_RSS_CHILD").is_none() {
            let output = std::process::Command::new(std::env::current_exe().expect("test exe"))
                .env("PACKSET_RSS_CHILD", "1")
                .args([
                    "--exact",
                    "store::tests::cached_vectors_do_not_keep_the_json_arrays",
                    "--test-threads=1",
                    "--quiet",
                ])
                .output()
                .expect("rss child");
            assert!(
                output.status.success(),
                "rss child failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let anon = || {
            let text = std::fs::read_to_string("/proc/self/status").unwrap();
            text.lines()
                .find_map(|line| {
                    let rest = line.strip_prefix("RssAnon:")?;
                    rest.split_whitespace().next()?.parse::<u64>().ok()
                })
                .unwrap_or(0)
        };
        let (_dir, store) = store();
        let start = anon();
        let embedding: Vec<f64> = (0..384).map(|_| 0.01).collect();
        for n in 0..64 {
            store
                .upsert(&record(json!({
                    "id": format!("n{n:04}"),
                    "workspace": "w",
                    "text": format!("claim number {n} is distinct enough"),
                    "kind": "lesson",
                    "embedding": embedding,
                })))
                .unwrap();
        }
        let grown = anon().saturating_sub(start);
        assert!(store
            .live("w")
            .unwrap()
            .iter()
            .all(|atom| atom.get("embedding").is_none()));
        assert!(grown < 2048, "anon grew {grown} KiB for 64 cached vectors");
    }

    /// Counting a workspace notes it, and used to leave that note after
    /// the counts row was evicted. Three counts into a cap of one stay
    /// one entry on the clock.
    #[test]
    fn counting_a_workspace_does_not_keep_it_on_the_clock() {
        let (_dir, store) = store();
        store.set_workspace_cache(1);
        for name in ["a", "b", "c"] {
            store.counts(Some(name)).unwrap();
        }
        assert!(
            store.cache_used_len() <= 1,
            "count-only workspaces stayed on the clock: {}",
            store.cache_used_len()
        );
    }

    /// A cap of two keeps the workspaces just read and drops the cold one.
    /// The dropped workspace is still in the database; the next read builds
    /// it again.
    #[test]
    fn the_workspace_cache_drops_the_coldest_snapshot() {
        let (_dir, store) = store();
        store.set_workspace_cache(2);
        for name in ["a", "b", "c"] {
            store
                .upsert(&record(
                    json!({"id": name, "workspace": name, "text": name, "kind": "lesson"}),
                ))
                .unwrap();
            let before = store.scanned.load(Ordering::SeqCst);
            assert_eq!(store.live(name).unwrap().len(), 1);
            assert!(
                store.scanned.load(Ordering::SeqCst) > before,
                "{name} was served from a cache it had not filled"
            );
        }
        assert_eq!(store.cached_workspaces(), 2);
        let before = store.scanned.load(Ordering::SeqCst);
        assert_eq!(store.live("c").unwrap().len(), 1);
        assert_eq!(store.live("b").unwrap().len(), 1);
        assert_eq!(
            store.scanned.load(Ordering::SeqCst),
            before,
            "the two hot workspaces were parsed again"
        );
        assert_eq!(store.live("a").unwrap().len(), 1);
        assert!(
            store.scanned.load(Ordering::SeqCst) > before,
            "the cold workspace was still parsed"
        );
    }

    #[test]
    fn forgetting_a_workspace_drops_only_its_keys() {
        let (_dir, store) = store();
        for (ws, id) in [("scratch", "a"), ("scratch", "b"), ("seat", "c")] {
            store
                .upsert(&record(json!({"id": id, "workspace": ws, "text": id})))
                .unwrap();
        }
        // Read first, so a cached snapshot is what the forget must clear.
        assert_eq!(store.live("scratch").unwrap().len(), 2);
        let dropped = store.forget_workspace("scratch").unwrap();
        assert_eq!(dropped, 2, "two scratch records were held");
        assert!(store.live("scratch").unwrap().is_empty());
        assert_eq!(
            store.live("seat").unwrap().len(),
            1,
            "the seat kept its claim"
        );
        assert!(store.scan(Some("scratch")).unwrap().is_empty());
        assert_eq!(
            store.forget_workspace("scratch").unwrap(),
            0,
            "twice is empty"
        );
        assert!(store.forget_workspace("").is_err(), "empty forgets nothing");

        store
            .upsert(&record(
                json!({"id": "d", "workspace": "scratch", "text": "d"}),
            ))
            .unwrap();
        store.delete("scratch", "d", None).unwrap();
        assert_eq!(store.forget_workspace("scratch").unwrap(), 0);
        let kept = store.get("scratch", "d").unwrap().unwrap();
        assert_eq!(kept["tombstone"], json!(true), "the tombstone stays");
    }

    #[test]
    fn workspaces_count_the_live_and_always_name_global() {
        let (_dir, store) = store();
        store
            .upsert(&record(json!({"id": "a", "workspace": "w", "text": "a"})))
            .unwrap();
        store
            .upsert(&record(json!({
                "id": "b", "workspace": "w", "text": "b", "tombstone": true
            })))
            .unwrap();
        let found = store.workspaces().unwrap();
        assert!(found.contains(&("w".to_string(), 1)), "{found:?}");
        assert!(
            found.iter().any(|(name, _)| name == "global"),
            "an empty seat still has somewhere to write: {found:?}"
        );
    }

    #[test]
    fn deleting_leaves_a_tombstone_rather_than_a_hole() {
        let (_dir, store) = store();
        store
            .upsert(&record(json!({"id": "a", "workspace": "w", "text": "a"})))
            .unwrap();
        let tomb = store.delete("w", "a", None).unwrap();
        assert_eq!(tomb["tombstone"], json!(true));
        // The record is still there to be read; it has left the live set.
        assert!(store.get("w", "a").unwrap().is_some());
        assert!(store.current("w", None).unwrap().is_empty());
        assert!(
            store.delete("w", "a", None).is_err(),
            "twice is not current"
        );
    }

    #[test]
    fn a_retraction_carries_its_deed_onto_the_tombstone() {
        let (_dir, store) = store();
        store
            .upsert(&record(
                json!({"id": "a", "workspace": "w", "text": "the claim"}),
            ))
            .unwrap();
        let tomb = store.delete("w", "a", Some("deed-patch-overlay")).unwrap();
        assert_eq!(tomb["retracted_by"], json!("deed-patch-overlay"));
        // Both halves read back together: what was withdrawn, and on what.
        assert_eq!(tomb["text"], json!("the claim"));
        let stored = store.get("w", "a").unwrap().unwrap();
        assert_eq!(stored["retracted_by"], json!("deed-patch-overlay"));
    }

    /// A writer before the index left its tombstones with no entry. The
    /// first cold read parses one and enters it; the next passes it by.
    #[test]
    fn a_tombstone_from_before_the_index_is_parsed_once() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = Store::open(dir.path()).unwrap();
            store
                .upsert_many(&[
                    record(json!({"id": "a", "workspace": "w", "text": "a"})),
                    record(json!({"id": "b", "workspace": "w", "text": "b", "tombstone": true})),
                ])
                .unwrap();
            let mut wtxn = store.env.write_txn().unwrap();
            let entry = tombstone_key(&atom_key("w", "b"));
            assert!(store.db.delete(&mut wtxn, &entry).unwrap());
            wtxn.commit().unwrap();
        }
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.live("w").unwrap().len(), 1);
        assert_eq!(store.tombstones_read(), 1);
        drop(store);
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.live("w").unwrap().len(), 1);
        assert_eq!(store.tombstones_read(), 0, "the entry it was given");
        assert_eq!(
            store.get("w", "b").unwrap().unwrap()["tombstone"],
            json!(true)
        );
    }

    /// A record written live over its tombstone leaves the index, so a cold
    /// read shows it.
    #[test]
    fn a_record_written_over_its_tombstone_is_read_cold() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = Store::open(dir.path()).unwrap();
            store
                .upsert(&record(
                    json!({"id": "a", "workspace": "w", "text": "a", "tombstone": true}),
                ))
                .unwrap();
            store
                .upsert(&record(
                    json!({"id": "a", "workspace": "w", "text": "a", "tombstone": false}),
                ))
                .unwrap();
        }
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.live("w").unwrap().len(), 1);
    }

    #[test]
    fn a_second_writer_is_refused_the_home() {
        let dir = tempfile::tempdir().unwrap();
        let _first = Store::open(dir.path()).unwrap();
        let second = Store::open(dir.path());
        assert!(second.is_err(), "one writer is the whole design");
    }

    /// A store opens with a map sized to its file. One write several
    /// times that map, and single writes past it after, land, and the
    /// store opens again with every claim. The map at 1 GiB refused the
    /// write that needed more than it had left.
    #[test]
    fn writes_past_the_map_grow_it() {
        let (dir, store) = store();
        let opened = store.env.info().map_size;
        let claim = |n: usize| {
            record(json!({
                "id": format!("c{n:05}"),
                "workspace": "w",
                "kind": "lesson",
                "text": format!("claim {n} {}", "x".repeat(4096)),
            }))
        };
        let per_claim = serde_json::to_vec(&claim(0)).unwrap().len();
        let at_once = 4 * opened / per_claim;
        let batch: Vec<Record> = (0..at_once).map(claim).collect();
        assert_eq!(store.upsert_many(&batch).unwrap().len(), at_once);
        let one_by_one = at_once + 2 * store.env.info().map_size / per_claim;
        for n in at_once..one_by_one {
            store.upsert(&claim(n)).unwrap();
        }
        assert!(
            (one_by_one * per_claim) > 4 * opened,
            "the writes did not pass the {opened}-byte map"
        );
        assert_eq!(store.scan(Some("w")).unwrap().len(), one_by_one);
        drop(store);
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.scan(Some("w")).unwrap().len(), one_by_one);
        let last = format!("c{:05}", one_by_one - 1);
        assert!(store.get("w", &last).unwrap().unwrap()["text"]
            .as_str()
            .unwrap()
            .starts_with(&format!("claim {}", one_by_one - 1)));
    }

    /// Take the room left in the map with values no reader parses, put
    /// past [`Store::write`] so the map does not grow for them. The key
    /// opens with a byte UTF-8 never holds, so no workspace reaches it.
    fn fill_the_map(store: &Store) {
        let put = |key: &[u8], chunk: usize| -> heed::Result<()> {
            let mut wtxn = store.env.write_txn()?;
            store.db.put(&mut wtxn, key, &vec![0; chunk])?;
            wtxn.commit()
        };
        for chunk in [64 * 1024, 4096, 512] {
            loop {
                let key = [&[0xFE][..], &store.env.info().last_txn_id.to_be_bytes()].concat();
                match put(&key, chunk) {
                    Ok(()) => {}
                    Err(heed::Error::Mdb(heed::MdbError::MapFull)) => break,
                    Err(err) => panic!("the fill failed: {err}"),
                }
            }
        }
    }

    /// The stamp pass and the tombstone index write behind a read, not
    /// for a client, and each finds room when the map is full. The pass
    /// stamps every record a writer from before [`UPDATED`] left, and the
    /// entries the first cold read enters are there at the next open.
    #[test]
    fn a_full_map_grows_for_the_stamp_pass_and_the_tombstone_index() {
        let dir = tempfile::tempdir().unwrap();
        let claims = 32;
        let buried = 400;
        {
            let store = Store::open(dir.path()).unwrap();
            let mut wtxn = store.env.write_txn().unwrap();
            for n in 0..claims {
                let row = json!({"id": format!("c{n:02}"), "workspace": "w",
                                 "text": "x".repeat(4096),
                                 "ts": "2026-10-18T19:31:19.444Z"});
                let key = atom_key("w", row["id"].as_str().unwrap());
                store
                    .db
                    .put(&mut wtxn, &key, &serde_json::to_vec(&row).unwrap())
                    .unwrap();
            }
            for n in 0..buried {
                let id = format!("t{n:03}-{}", "y".repeat(200));
                let row = json!({"id": id, "workspace": "w", "text": "gone",
                                 "tombstone": true,
                                 "ts": "2026-10-18T19:31:19.444Z",
                                 "updated": "2026-10-18T19:31:19.444Z"});
                store
                    .db
                    .put(
                        &mut wtxn,
                        &atom_key("w", &id),
                        &serde_json::to_vec(&row).unwrap(),
                    )
                    .unwrap();
            }
            wtxn.commit().unwrap();
        }
        let store = Store::open(dir.path()).unwrap();
        fill_the_map(&store);
        assert_eq!(store.restore_created().unwrap(), claims);
        fill_the_map(&store);
        assert_eq!(store.live("w").unwrap().len(), claims);
        assert_eq!(store.tombstones_read(), buried as u64);
        drop(store);
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.live("w").unwrap().len(), claims);
        assert_eq!(store.tombstones_read(), 0, "the entries were not kept");
        assert!(store
            .scan(Some("w"))
            .unwrap()
            .iter()
            .all(|atom| atom.contains_key(UPDATED)));
    }

    /// How many times its bound the pack in
    /// [`a_start_holds_one_step_not_the_pack`] is written to.
    /// The smallest retention the bound catches is its headroom: the bound less what a correct start peaks at.
    /// Such a start peaks above its base, so the headroom is under one bound.
    /// On twice the bound, a pass that keeps what it reads ends more than a headroom past the bound.
    /// If a pass keeps one headroom more than it should, the pack still has that much left for it to keep, past the two steps that are live.
    #[cfg(target_os = "linux")]
    const PACK_OVER_BOUND: usize = 2;

    /// What glibc keeps of freed heap at the top before it gives it back:
    /// the default `M_TRIM_THRESHOLD` in mallopt(3). glibc has no call that reads the live value.
    /// Freeing a mapped chunk raises it to twice that chunk (the dynamic mmap threshold in mallopt(3)).
    /// That happened in `measurements/start.txt` when the Lightning Memory-Mapped Database (LMDB) freed its 2 MiB dirty list.
    /// The start child sets it with `mallopt`, which turns that off, so it is this value through the pass.
    #[cfg(target_os = "linux")]
    const TRIM_THRESHOLD: usize = 128 * 1024;

    /// A start over a pack [`PACK_OVER_BOUND`] times [`start_bound`] peaks
    /// under that bound, whether its pass stamps every claim or none. The
    /// bound is for the most one fault mapped of the pack on this host, from [`fault_unit`], measured again before each start, since the pass rewrites the pack.
    /// Its claims are in one workspace, written in key order, so the records of a step lie in one run of the file.
    ///
    /// Linux only: it reads `/proc/self/status`, `/proc/self/smaps`, `/proc/self/mountinfo` and the `transparent_hugepage` files in sysfs, and resets the peak through `/proc/self/clear_refs`.
    /// macOS has none of them, and its footprint leaves out clean file pages, which are what this test counts.
    #[test]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "Linux only: reads /proc/self/status, /proc/self/smaps and sysfs, which macOS does not have"
    )]
    fn a_start_holds_one_step_not_the_pack() {
        #[cfg(target_os = "linux")]
        start_holds_one_step();
    }

    #[cfg(target_os = "linux")]
    fn start_holds_one_step() {
        if let Some(home) = std::env::var_os("PACKSET_START_CHILD") {
            report_start(Path::new(&home));
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("memory.lmdb").join("data.mdb");
        let (key, value) = unstamped(0, 1);
        let record = key.len() + value.len();
        let mut atoms = 0;
        let (disk, stat, mut unit, mut bound, places) = loop {
            let (disk, stat) = write_unstamped_pack(dir.path(), atoms, 1);
            let unit = fault_unit(&data);
            let (bound, places) = start_bound(&stat, record, unit.bytes);
            if disk as usize >= PACK_OVER_BOUND * bound {
                break (disk, stat, unit, bound, places);
            }
            atoms = (PACK_OVER_BOUND * bound)
                .div_ceil(value.len())
                .max(atoms + 1);
        };
        let step = step_bytes(&stat, record) / 1024;
        let disk_kib = disk / 1024;
        log(&format!(
            "start bound on {}, the pack on {}. A tree of depth {} with {} claims, {} leaves, \
             a step of {step} KiB. {} KiB for {places} places, over a {disk_kib} KiB pack, {:.2} bounds.",
            host(),
            mount_of(dir.path()).join(" "),
            stat.depth,
            stat.entries,
            stat.leaf_pages,
            bound / 1024,
            disk as f64 / bound as f64,
        ));
        let mut failed = Vec::new();
        for start in ["stamping", "current"] {
            if start != "stamping" {
                unit = fault_unit(&data);
                bound = start_bound(&stat, record, unit.bytes).0;
            }
            let bound_kib = (bound / 1024) as u64;
            let got = start_in_child(
                dir.path(),
                "store::tests::a_start_holds_one_step_not_the_pack",
            );
            let peak = got.hwm.saturating_sub(got.base);
            let headroom = bound_kib.saturating_sub(peak);
            log(&format!(
                "{start} start: a fault mapped {} KiB, bound {bound_kib} KiB, peak {peak} KiB, \
                 headroom {headroom} KiB ({:.1} steps): {}",
                unit.bytes / 1024,
                headroom as f64 / step as f64,
                unit.line,
            ));
            log(&format!("{start} start: {}", got.residual()));
            log(&format!("{start} start: {}", got.line));
            if !got.passed {
                failed.push(format!("the {start} start failed: {}", got.line));
            }
            if peak >= bound_kib {
                failed.push(format!(
                    "the {start} start peaked {peak} KiB over a {disk_kib} KiB pack, past {bound_kib} KiB"
                ));
            }
            if got.exe != got.exe_base {
                failed.push(format!(
                    "the test binary's resident pages went from {} to {} KiB during the {start} start",
                    got.exe_base, got.exe
                ));
            }
        }
        assert!(failed.is_empty(), "{}", failed.join("; "));
        let store = Store::open(dir.path()).unwrap();
        let all = store.scan(None).unwrap();
        assert_eq!(all.len(), stat.entries);
        assert!(
            all.iter()
                .all(|atom| atom[UPDATED] == json!("2026-10-18T19:31:19.444Z")
                    && atom["ts"] == json!("2026-10-16T10:07:21.644Z")),
            "a claim past the first step was not stamped"
        );
    }

    /// The pages one step of a pack whose tree is `stat` fills, where its claims take `record` bytes as a step counts them.
    #[cfg(target_os = "linux")]
    fn step_bytes(stat: &heed::DatabaseStat, record: usize) -> usize {
        STAMP_STEP_BYTES.div_ceil(record)
            * stat.overflow_pages.div_ceil(stat.entries.max(1))
            * stat.page_size as usize
    }

    /// What a start can hold over its base, of a pack of one workspace
    /// whose tree is `stat` and whose claims take `record` bytes as a step
    /// counts them, where a fault maps `unit` bytes. Returns the bound and
    /// the places in it.
    ///
    /// The step a pass is on is live twice: its records as read through
    /// the map, and the copy its write transaction makes of them, in the
    /// heap until the commit writes it out (LMDB writes with `write`, not
    /// `WRITEMAP`). A step is the pages its records fill.
    ///
    /// Each place in the file a step reads apart from the rest can map a
    /// unit beyond its own bytes, and a page of it rewritten is a page in
    /// the heap. The places are the meta pages at the head of the file,
    /// read as each transaction begins; each level of the tree from the
    /// root to the step's first leaf, apart since a rewritten page moves;
    /// each leaf the step moves on to, with the levels between it and the
    /// root; the two ends of the run its records fill; and the free list's
    /// leaf, with the overflow page that holds the page numbers a step
    /// frees, more than fit in a node, which the next write reads to reuse
    /// them.
    ///
    /// The heap holds up to [`TRIM_THRESHOLD`] more of what the step freed.
    #[cfg(target_os = "linux")]
    fn start_bound(stat: &heed::DatabaseStat, record: usize, unit: usize) -> (usize, usize) {
        const STEPS_LIVE: usize = 2;
        const META: usize = 1;
        const RUN_ENDS: usize = 2;
        const FREE_LIST: usize = 2;
        let page = stat.page_size as usize;
        let per_step = STAMP_STEP_BYTES.div_ceil(record);
        let per_leaf = (stat.entries / stat.leaf_pages.max(1)).max(1);
        let depth = stat.depth as usize;
        let places = META
            + depth
            + per_step.div_ceil(per_leaf) * depth.saturating_sub(1)
            + RUN_ENDS
            + FREE_LIST;
        (
            STEPS_LIVE * step_bytes(stat, record) + places * (unit + page) + TRIM_THRESHOLD,
            places,
        )
    }

    /// The most one fault mapped of a file, and what `smaps` said of it.
    #[cfg(target_os = "linux")]
    struct FaultUnit {
        bytes: usize,
        line: String,
    }

    /// Measure what one fault maps of the file at `path`, as it is in the page cache now.
    /// Each probe maps the whole file afresh, reads one byte and takes that mapping's `Rss` from `/proc/self/smaps`, with `FilePmdMapped`, `AnonHugePages`, `KernelPageSize` and `MMUPageSize`.
    /// The next probe reads the byte that far on, so the probes step through the file a fault at a time.
    /// The unit is the most a probe left resident, and never below a page.
    #[cfg(target_os = "linux")]
    fn fault_unit(path: &Path) -> FaultUnit {
        use std::os::fd::AsRawFd;
        let page = page_size();
        let file = File::open(path).unwrap();
        let len = file.metadata().unwrap().len() as usize;
        let mut most = [0u64; 5];
        let mut probes = 0;
        let mut offset = 0;
        while offset < len {
            // SAFETY: a read-only shared map of a file nothing truncates while the parent runs, unmapped before the next probe.
            let addr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ,
                    libc::MAP_SHARED,
                    file.as_raw_fd(),
                    0,
                )
            };
            assert_ne!(addr, libc::MAP_FAILED, "map {}", path.display());
            // SAFETY: `offset` is inside the map.
            unsafe { std::ptr::read_volatile(addr.cast::<u8>().add(offset)) };
            let at = format!("{:x}-", addr as usize);
            let mut fields = [0u64; 5];
            smaps(|header, field, kib| {
                if header.starts_with(&at) {
                    if let Some(slot) = [
                        "Rss",
                        "FilePmdMapped",
                        "AnonHugePages",
                        "KernelPageSize",
                        "MMUPageSize",
                    ]
                    .iter()
                    .position(|name| *name == field)
                    {
                        fields[slot] = kib;
                    }
                }
            });
            // SAFETY: the map made above, which nothing else references.
            unsafe { libc::munmap(addr, len) };
            for (most, field) in most.iter_mut().zip(fields) {
                *most = (*most).max(field);
            }
            probes += 1;
            offset += (fields[0] as usize * 1024).max(page);
        }
        let [rss, pmd, anon_huge, kernel_page, mmu_page] = most;
        FaultUnit {
            bytes: (rss as usize * 1024).max(page),
            line: format!(
                "the most of {probes} probes: Rss {rss} FilePmdMapped {pmd} \
                 AnonHugePages {anon_huge} KernelPageSize {kernel_page} MMUPageSize {mmu_page} KiB"
            ),
        }
    }

    /// The setting a sysfs choice file has in brackets.
    #[cfg(target_os = "linux")]
    fn chosen(text: &str) -> String {
        text.split_whitespace()
            .find_map(|word| word.strip_prefix('[')?.strip_suffix(']'))
            .unwrap_or(text.trim())
            .to_string()
    }

    /// The type and the options of the filesystem `dir` is on: the
    /// innermost mount above it in `/proc/self/mountinfo`.
    #[cfg(target_os = "linux")]
    fn mount_of(dir: &Path) -> [String; 2] {
        let dir = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        let info = fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
        info.lines()
            .filter_map(|line| {
                let (mount, filesystem) = line.split_once(" - ")?;
                let point = mount.split_whitespace().nth(4)?.replace("\\040", " ");
                let mut filesystem = filesystem.split_whitespace();
                let fstype = filesystem.next()?.to_string();
                let options = filesystem.nth(1).unwrap_or_default().to_string();
                dir.starts_with(&point)
                    .then_some((point.len(), [fstype, options]))
            })
            .max_by_key(|(len, _)| *len)
            .map(|(_, mount)| mount)
            .unwrap_or_default()
    }

    /// The kernel (`uname -r`), the CPU and its `transparent_hugepage` settings.
    #[cfg(target_os = "linux")]
    fn host() -> String {
        let read = |path: &str| {
            fs::read_to_string(path).map_or_else(|_| "unread".into(), |text| chosen(&text))
        };
        let cpu = fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|text| {
                text.lines().find_map(|line| {
                    Some(
                        line.strip_prefix("model name")?
                            .split_once(':')?
                            .1
                            .trim()
                            .to_string(),
                    )
                })
            })
            .unwrap_or_else(|| "unread".into());
        format!(
            "Linux {}, {cpu}, {} CPUs, THP enabled {} shmem_enabled {} hpage_pmd_size {}",
            read("/proc/sys/kernel/osrelease"),
            std::thread::available_parallelism().map_or(0, usize::from),
            read("/sys/kernel/mm/transparent_hugepage/enabled"),
            read("/sys/kernel/mm/transparent_hugepage/shmem_enabled"),
            read("/sys/kernel/mm/transparent_hugepage/hpage_pmd_size"),
        )
    }

    /// Write `line` to stderr past the test harness's capture, so a run
    /// that passes shows it in CI too.
    #[cfg(target_os = "linux")]
    fn log(line: &str) {
        use std::io::Write;
        let _ = writeln!(std::io::stderr(), "{line}");
    }

    /// One claim the size a seat writes, as a writer from before
    /// [`UPDATED`] left it: a sentence, two entities and a 384-d
    /// embedding, the encoder's width, as the f32 an encoder returns
    /// written out as f64. About 8 KiB as JSON. The claim is in workspace
    /// `n % workspaces`.
    #[cfg(target_os = "linux")]
    fn unstamped(n: usize, workspaces: usize) -> (Vec<u8>, Vec<u8>) {
        let mut state = (n as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let embedding: Vec<f64> = (0..384)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                f64::from((state >> 40) as f32 / (1u32 << 24) as f32 - 0.5)
            })
            .collect();
        let workspace = format!("git:github.com/example/pack-{}", n % workspaces);
        let id = format!("a{n:07}");
        let row = json!({
            "id": id,
            "workspace": workspace,
            "kind": "lesson",
            "text": format!("Claim {n}: the parser keeps header {} when overlay {} applies.", n % 97, n % 31),
            "entities": ["seat:ljos", format!("n{n}")],
            "links": [],
            "ts": "2026-10-18T19:31:19.444Z",
            "valid_from": "2026-10-16T10:07:21.644Z",
            "embedding": embedding,
        });
        (atom_key(&workspace, &id), serde_json::to_vec(&row).unwrap())
    }

    /// Write unstamped claims straight into a home's database until it
    /// holds `atoms`, spread round-robin over `workspaces`, and return the
    /// size of the data file and the shape of its tree.
    #[cfg(target_os = "linux")]
    fn write_unstamped_pack(
        home: &Path,
        atoms: usize,
        workspaces: usize,
    ) -> (u64, heed::DatabaseStat) {
        let dir = home.join("memory.lmdb");
        fs::create_dir_all(&dir).unwrap();
        // SAFETY: a fresh directory no other process has open. The map is
        // address space only; the file grows with the pages written.
        let env = unsafe {
            EnvOpenOptions::new()
                .map_size(1 << 36)
                .max_dbs(1)
                .open(&dir)
                .unwrap()
        };
        let mut wtxn = env.write_txn().unwrap();
        let db: Database<Bytes, Bytes> = env.create_database(&mut wtxn, None).unwrap();
        let written = db.len(&wtxn).unwrap() as usize;
        for n in written..atoms {
            let (key, blob) = unstamped(n, workspaces);
            db.put(&mut wtxn, &key, &blob).unwrap();
            if n % 1000 == 999 {
                wtxn.commit().unwrap();
                wtxn = env.write_txn().unwrap();
            }
        }
        wtxn.commit().unwrap();
        let stat = db.stat(&env.read_txn().unwrap()).unwrap();
        env.prepare_for_closing().wait();
        (fs::metadata(dir.join("data.mdb")).unwrap().len(), stat)
    }

    /// A field of `/proc/self/status`, in KiB.
    #[cfg(target_os = "linux")]
    fn status_kib(field: &str) -> u64 {
        let text = fs::read_to_string("/proc/self/status").unwrap();
        text.lines()
            .find_map(|line| {
                let rest = line.strip_prefix(field)?.strip_prefix(':')?;
                rest.split_whitespace().next()?.parse().ok()
            })
            .unwrap_or(0)
    }

    /// Visit each field of `/proc/self/smaps` that is in KiB, with the header line of its mapping.
    /// The file is read a line at a time, so the read leaves no heap of its size behind to count in the next reading.
    #[cfg(target_os = "linux")]
    fn smaps(mut visit: impl FnMut(&str, &str, u64)) {
        use std::io::BufRead;
        let Ok(smaps) = File::open("/proc/self/smaps") else {
            return;
        };
        let mut header = String::new();
        for line in std::io::BufReader::new(smaps).lines().map_while(Result::ok) {
            let mut words = line.split_whitespace();
            let Some(first) = words.next() else {
                continue;
            };
            match first.strip_suffix(':') {
                None => header = line,
                Some(field) => {
                    if let (Some(kib), Some("kB")) = (words.next(), words.next()) {
                        visit(&header, field, kib.parse().unwrap_or(0));
                    }
                }
            }
        }
    }

    /// What this process has resident of its maps of `path`, in KiB: the `Rss` of each mapping in `/proc/self/smaps` that names it.
    #[cfg(target_os = "linux")]
    fn mapped_kib(path: &Path) -> u64 {
        let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let path = path.to_string_lossy();
        let mut kib = 0;
        smaps(|header, field, value| {
            if field == "Rss" && header.trim_end().ends_with(path.as_ref()) {
                kib += value;
            }
        });
        kib
    }

    /// `Rss` by mapping, in KiB: the file name a mapping names, its bracketed name such as `[heap]`, or `anon`.
    #[cfg(target_os = "linux")]
    fn resident_by_mapping() -> BTreeMap<String, u64> {
        let mut by = BTreeMap::new();
        smaps(|header, field, kib| {
            if field != "Rss" {
                return;
            }
            let name = header
                .split_whitespace()
                .nth(5)
                .map_or("anon", |name| name.rsplit('/').next().unwrap_or(name));
            *by.entry(name.to_string()).or_insert(0) += kib;
        });
        by
    }

    /// Start the writer over `home` in a child of this test binary and
    /// return the child's report. RSS is the whole process, so a
    /// neighbouring test would otherwise charge its own allocations to it.
    #[cfg(target_os = "linux")]
    fn start_in_child(home: &Path, test: &str) -> StartReport {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .env("PACKSET_START_CHILD", home)
            .args([
                "--exact",
                test,
                "--include-ignored",
                "--nocapture",
                "--test-threads=1",
                "--quiet",
            ])
            .output()
            .expect("start child");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let line = stdout
            .lines()
            .find_map(|line| line.strip_prefix("start "))
            .unwrap_or_else(|| {
                panic!(
                    "start child said nothing: {stdout} {}",
                    String::from_utf8_lossy(&output.stderr)
                )
            });
        let field = |name: &str| -> u64 {
            line.split_whitespace()
                .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
                .and_then(|value| value.parse().ok())
                .unwrap_or(0)
        };
        StartReport {
            opened: !line.starts_with("failed"),
            passed: line.starts_with("ok"),
            line: line.to_string(),
            open_us: field("open_us"),
            pass_ms: field("pass_ms"),
            base: field("base"),
            rss: field("rss"),
            anon_base: field("anon_base"),
            anon: field("anon"),
            file_base: field("file_base"),
            file: field("file"),
            shmem_base: field("shmem_base"),
            shmem: field("shmem"),
            hwm: field("hwm"),
            exe_base: field("exe_base"),
            exe: field("exe"),
            mdb: field("mdb"),
            smaps_base: field("smaps_base"),
            smaps: field("smaps"),
        }
    }

    /// What one start cost, in KiB and milliseconds. `base` is the child's
    /// resident size before the open, and the `_base` fields are its parts then.
    #[cfg(target_os = "linux")]
    #[derive(Debug)]
    struct StartReport {
        opened: bool,
        passed: bool,
        line: String,
        open_us: u64,
        pass_ms: u64,
        base: u64,
        rss: u64,
        anon_base: u64,
        anon: u64,
        file_base: u64,
        file: u64,
        shmem_base: u64,
        shmem: u64,
        hwm: u64,
        exe_base: u64,
        exe: u64,
        mdb: u64,
        smaps_base: u64,
        smaps: u64,
    }

    #[cfg(target_os = "linux")]
    impl StartReport {
        /// By kind and by file, what the start left resident over its base after the pass.
        fn residual(&self) -> String {
            let grew = |after: u64, before: u64| after as i64 - before as i64;
            let file = grew(self.file, self.file_base);
            let shmem = grew(self.shmem, self.shmem_base);
            let exe = grew(self.exe, self.exe_base);
            format!(
                "{} KiB over base after the pass by VmRSS, {} by smaps: RssAnon {:+}, \
                 RssFile {file:+} and RssShmem {shmem:+} (test binary {exe:+}, data file {:+}, \
                 the rest {:+}) KiB",
                grew(self.rss, self.base),
                grew(self.smaps, self.smaps_base),
                grew(self.anon, self.anon_base),
                self.mdb,
                file + shmem - exe - self.mdb as i64,
            )
        }
    }

    /// The child's half of [`start_in_child`]: what packsetd runs at start, `Service::open` before it binds and the `stamp` pass behind the listener, one after the other, then what it cost.
    ///
    /// Warm starts come first.
    /// Each one goes through [`measure_start`] over a copy of the pack beside it, on its filesystem.
    /// So the warm starts run the code the measured start runs, readings and all.
    /// The warm-up ends once a start leaves the resident pages of this test binary no higher than before, as [`warm_up`] counts them.
    /// So that code is resident by the time the test reads `base`.
    /// The parent fails the test if the measured start then grows them.
    /// The peak is reset through `clear_refs` at the base, so `hwm` is the measured start's alone.
    ///
    /// `exe` is what this test binary has resident, `mdb` what the pack's data file has after the pass, and `resident` the change in `Rss` of each mapping that changed from the base, so the pages a start leaves can be told apart from the pack's.
    /// `smaps_base` and `smaps` are the sum of `Rss` over every mapping.
    /// The kernel counts that exactly, where `VmRSS` and `VmHWM` are per-CPU counters read without their CPUs' pending counts (`get_mm_counter` in include/linux/mm.h), so the two can differ.
    #[cfg(target_os = "linux")]
    fn report_start(home: &Path) {
        #[cfg(target_env = "gnu")]
        {
            let threshold = libc::c_int::try_from(TRIM_THRESHOLD).unwrap();
            // SAFETY: sets one allocator parameter; nothing is allocated across the call.
            let set = unsafe { libc::mallopt(libc::M_TRIM_THRESHOLD, threshold) };
            assert_eq!(set, 1, "mallopt(M_TRIM_THRESHOLD) refused");
        }
        let exe = std::env::current_exe().unwrap();
        let data = home.join("memory.lmdb").join("data.mdb");
        let pages = fs::metadata(&exe)
            .unwrap()
            .len()
            .div_ceil(page_size() as u64) as usize;
        let rounds = warm_up(
            pages,
            || mapped_kib(&exe),
            |round| {
                let dir = tempfile::tempdir_in(home.parent().unwrap_or(home)).unwrap();
                fs::create_dir_all(dir.path().join("memory.lmdb")).unwrap();
                fs::copy(&data, dir.path().join("memory.lmdb").join("data.mdb")).unwrap();
                let _ = measure_start(dir.path(), &exe, round);
            },
        );
        println!("start {}", measure_start(home, &exe, rounds));
    }

    /// Run `round` until one leaves `resident` no higher than it found it, and return how many ran.
    /// A round that does not stop maps at least one more of the binary's `pages`.
    /// So while nothing is evicted, the rounds end by `pages + 1`.
    /// Eviction can lower `resident` between rounds, so `pages + 1` also caps them.
    #[cfg(target_os = "linux")]
    fn warm_up(
        pages: usize,
        mut resident: impl FnMut() -> u64,
        mut round: impl FnMut(usize),
    ) -> usize {
        for rounds in 0..=pages {
            let before = resident();
            round(rounds);
            if resident() <= before {
                return rounds + 1;
            }
        }
        pages + 1
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn warm_up_stops_once_a_round_leaves_the_binary_as_it_found_it() {
        let mut readings = [40, 52, 52, 52].into_iter();
        let mut ran = Vec::new();
        let rounds = warm_up(100, || readings.next().unwrap(), |round| ran.push(round));
        assert_eq!(rounds, 2);
        assert_eq!(ran, [0, 1]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn warm_up_stops_when_a_round_ends_with_fewer_pages_resident() {
        let mut readings = [52, 60, 60, 44].into_iter();
        assert_eq!(warm_up(100, || readings.next().unwrap(), |_| {}), 2);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn warm_up_ends_past_the_binary_page_count_when_eviction_keeps_it_growing() {
        let pages = 5;
        let mut grown = false;
        let resident = || {
            grown = !grown;
            if grown {
                40
            } else {
                44
            }
        };
        let mut ran = 0;
        assert_eq!(warm_up(pages, resident, |_| ran += 1), pages + 1);
        assert_eq!(ran, pages + 1);
    }

    /// The size of a page, from sysconf.
    #[cfg(target_os = "linux")]
    fn page_size() -> usize {
        // SAFETY: sysconf reads a constant and touches no memory of ours.
        usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap()
    }

    /// One start over `home`, as [`report_start`] reports it, after `warm` warm starts.
    #[cfg(target_os = "linux")]
    fn measure_start(home: &Path, exe: &Path, warm: usize) -> String {
        let resident_base = resident_by_mapping();
        let smaps_base: u64 = resident_base.values().sum();
        let exe_base = mapped_kib(exe);
        // proc(5): 5 resets the peak resident size to the current one.
        fs::write("/proc/self/clear_refs", "5").unwrap();
        let (anon_base, file_base, shmem_base) = (
            status_kib("RssAnon"),
            status_kib("RssFile"),
            status_kib("RssShmem"),
        );
        let base = status_kib("VmRSS");
        let started = std::time::Instant::now();
        let opened = crate::service::Service::open(crate::home::Home::new(home));
        let open_us = started.elapsed().as_micros();
        let service = match opened {
            Ok(service) => service,
            Err(err) => {
                return format!(
                    "failed open_us={open_us} base={base} hwm={} err={err:#}",
                    status_kib("VmHWM")
                );
            }
        };
        let started = std::time::Instant::now();
        let passed = service.store().restore_created();
        let pass_ms = started.elapsed().as_millis();
        let (rss, hwm, anon, file, shmem) = (
            status_kib("VmRSS"),
            status_kib("VmHWM"),
            status_kib("RssAnon"),
            status_kib("RssFile"),
            status_kib("RssShmem"),
        );
        let exe_after = mapped_kib(exe);
        let mdb = mapped_kib(&service.store().db_dir.join("data.mdb"));
        let resident = resident_by_mapping();
        let smaps_after: u64 = resident.values().sum();
        let grew: Vec<String> = resident
            .into_iter()
            .filter_map(|(name, kib)| {
                let was = resident_base.get(&name).copied().unwrap_or(0);
                (kib != was).then(|| format!("{name}:{:+}", kib as i64 - was as i64))
            })
            .collect();
        format!(
            "{} open_us={open_us} pass_ms={pass_ms} base={base} rss={rss} hwm={hwm} \
             anon_base={anon_base} anon={anon} file_base={file_base} file={file} \
             shmem_base={shmem_base} shmem={shmem} exe_base={exe_base} exe={exe_after} mdb={mdb} \
             smaps_base={smaps_base} smaps={smaps_after} warm={warm} resident={}{}",
            if passed.is_ok() { "ok" } else { "stopped" },
            grew.join(","),
            passed
                .err()
                .map_or_else(String::new, |err| format!(" err={err:#}")),
        )
    }

    /// What start costs as the pack grows, for `measurements/start.txt`.
    ///
    /// ```console
    /// $ cargo test --release -p packset-daemon --lib start_cost_by_pack_size -- --ignored --nocapture
    /// ```
    ///
    /// Each pack is written as a writer from before [`UPDATED`] left it,
    /// so the first start stamps every claim. The second start is the
    /// same pack once it is current. The claims are spread over eight
    /// workspaces written in turn, so a scan in key order reads the file
    /// out of order, as a pack written by several checkouts is read.
    /// `PACKSET_START_SIZES` names the sizes.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "a measurement: a gibibyte of disk at the largest size"]
    fn start_cost_by_pack_size() {
        if let Some(home) = std::env::var_os("PACKSET_START_CHILD") {
            report_start(Path::new(&home));
            return;
        }
        let sizes =
            std::env::var("PACKSET_START_SIZES").unwrap_or_else(|_| "10000,50000,100000".into());
        println!("| atoms | data.mdb MiB | start | open ms | pass ms | base MiB | VmRSS MiB | RssAnon MiB | RssFile MiB | VmHWM MiB | peak over base KiB |");
        println!("|---|---|---|---|---|---|---|---|---|---|---|");
        let mib = |kib: u64| kib / 1024;
        for atoms in sizes
            .split(',')
            .filter_map(|s| s.trim().parse::<usize>().ok())
        {
            let dir = tempfile::tempdir().unwrap();
            let (disk, _) = write_unstamped_pack(dir.path(), atoms, 8);
            for start in ["first", "second"] {
                let got = start_in_child(dir.path(), "store::tests::start_cost_by_pack_size");
                let err = got.line.split_once(" err=").map_or("", |(_, err)| err);
                if !got.opened {
                    println!(
                        "| {atoms} | {} | {start} | did not open: {err} | | {} | | | | {} | {} |",
                        disk >> 20,
                        mib(got.base),
                        mib(got.hwm),
                        got.hwm.saturating_sub(got.base),
                    );
                    continue;
                }
                let pass = if got.passed {
                    got.pass_ms.to_string()
                } else {
                    format!("{}, stopped: {err}", got.pass_ms)
                };
                println!(
                    "| {atoms} | {} | {start} | {:.1} | {pass} | {} | {} | {} | {} | {} | {} |",
                    disk >> 20,
                    got.open_us as f64 / 1000.0,
                    mib(got.base),
                    mib(got.rss),
                    mib(got.anon),
                    mib(got.file + got.shmem),
                    mib(got.hwm),
                    got.hwm.saturating_sub(got.base),
                );
            }
        }
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;
    use serde_json::json;

    fn record(value: Value) -> Record {
        value.as_object().unwrap().clone()
    }

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        (dir, store)
    }

    #[test]
    fn a_write_is_visible_to_the_next_read() {
        let (_dir, store) = store();
        assert!(store.live("w").unwrap().is_empty());
        store
            .upsert(&record(json!({"id": "a", "workspace": "w", "text": "a"})))
            .unwrap();
        assert_eq!(store.live("w").unwrap().len(), 1, "the snapshot went stale");
        store
            .upsert(&record(json!({"id": "b", "workspace": "w", "text": "b"})))
            .unwrap();
        assert_eq!(store.live("w").unwrap().len(), 2);
    }

    #[test]
    fn a_repeated_read_hands_back_the_same_snapshot() {
        let (_dir, store) = store();
        store
            .upsert(&record(json!({"id": "a", "workspace": "w", "text": "a"})))
            .unwrap();
        let first = store.live("w").unwrap();
        let second = store.live("w").unwrap();
        assert!(
            Arc::ptr_eq(&first, &second),
            "two readers should share one parse"
        );
        store
            .upsert(&record(json!({"id": "b", "workspace": "w", "text": "b"})))
            .unwrap();
        let third = store.live("w").unwrap();
        assert!(!Arc::ptr_eq(&first, &third), "a write invalidates it");
    }

    #[test]
    fn a_tombstone_leaves_the_snapshot() {
        let (_dir, store) = store();
        store
            .upsert(&record(json!({"id": "a", "workspace": "w", "text": "a"})))
            .unwrap();
        assert_eq!(store.live("w").unwrap().len(), 1);
        store.delete("w", "a", None).unwrap();
        assert!(
            store.live("w").unwrap().is_empty(),
            "a delete must invalidate too"
        );
    }

    #[test]
    fn one_workspace_write_does_not_serve_another_stale() {
        let (_dir, store) = store();
        store
            .upsert(&record(json!({"id": "a", "workspace": "one", "text": "a"})))
            .unwrap();
        assert_eq!(store.live("one").unwrap().len(), 1);
        assert!(store.live("two").unwrap().is_empty());
        store
            .upsert(&record(json!({"id": "b", "workspace": "two", "text": "b"})))
            .unwrap();
        assert_eq!(store.live("two").unwrap().len(), 1);
        assert_eq!(store.live("one").unwrap().len(), 1, "still correct");
    }

    #[test]
    fn the_index_follows_appends_rewrites_and_removals() {
        let (_dir, store) = store();
        for (id, text) in [
            ("a", "alpha beta"),
            ("b", "beta gamma"),
            ("c", "gamma delta"),
        ] {
            store
                .upsert(&record(json!({"id": id, "workspace": "w", "text": text})))
                .unwrap();
            assert_index_matches_a_fresh_build(&store, "w");
        }
        // A rewrite keeps its ordinal; the terms it dropped leave the index.
        store
            .upsert(&record(json!({
                "id": "b", "workspace": "w", "text": "epsilon zeta", "ts": "2030-01-01T00:00:00.000Z"
            })))
            .unwrap();
        assert_index_matches_a_fresh_build(&store, "w");
        // A removal moves the order, so the set is rebuilt.
        store.delete("w", "a", None).unwrap();
        assert_index_matches_a_fresh_build(&store, "w");
        store
            .upsert(&record(
                json!({"id": "d", "workspace": "w", "text": "delta eta"}),
            ))
            .unwrap();
        assert_index_matches_a_fresh_build(&store, "w");
    }

    #[test]
    fn a_rewrite_in_the_same_millisecond_updates_the_index() {
        let (_dir, store) = store();
        // Both writes are stamped with one instant. A clock that has
        // moved on would hide the bug this guards.
        let now = "2026-10-22T00:00:00.000Z";
        store
            .upsert_many_at(
                &[record(json!({
                    "id": "a", "workspace": "w", "text": "alpha beta"
                }))],
                now,
            )
            .unwrap();
        store
            .upsert_many_at(
                &[record(json!({
                    "id": "b", "workspace": "w", "text": "beta gamma"
                }))],
                now,
            )
            .unwrap();
        assert_index_matches_a_fresh_build(&store, "w");
        store
            .upsert_many_at(
                &[record(json!({
                    "id": "b", "workspace": "w", "text": "epsilon zeta"
                }))],
                now,
            )
            .unwrap();
        let rewritten = store.get("w", "b").unwrap().unwrap();
        assert_eq!(
            rewritten[UPDATED],
            json!(now),
            "the rewrite stayed in that millisecond"
        );
        assert_index_matches_a_fresh_build(&store, "w");
        // Entities are part of the document. The stamp is still `now`.
        store
            .upsert_many_at(
                &[record(json!({
                    "id": "b", "workspace": "w", "text": "epsilon zeta",
                    "entities": ["deed-index-flake"]
                }))],
                now,
            )
            .unwrap();
        let with_entity = store.get("w", "b").unwrap().unwrap();
        assert_eq!(with_entity[UPDATED], json!(now));
        assert_index_matches_a_fresh_build(&store, "w");
    }

    /// The index served after a write scores every term as an index built
    /// from scratch over the same atoms would.
    fn assert_index_matches_a_fresh_build(store: &Store, workspace: &str) {
        let (atoms, index, documents, _vectors) = store.searchable(workspace).unwrap();
        let fresh_docs: Vec<Vec<String>> = atoms
            .iter()
            .map(|atom| packset_core::search::atom_tokens(atom))
            .collect();
        assert_eq!(*documents, fresh_docs, "the cached tokens drifted");
        let fresh = Index::build(fresh_docs.iter().map(Vec::as_slice));
        assert_eq!(index.len(), fresh.len());
        assert!((index.average_length() - fresh.average_length()).abs() < 1e-9);
        for term in fresh_docs.iter().flatten().chain(
            ["alpha", "beta", "gamma", "zeta"]
                .iter()
                .map(|t| t.to_string())
                .collect::<Vec<_>>()
                .iter(),
        ) {
            assert!(
                (index.idf(term) - fresh.idf(term)).abs() < 1e-9,
                "idf of {term} drifted: {} vs {}",
                index.idf(term),
                fresh.idf(term)
            );
            assert_eq!(
                index.occurrences_of(term),
                fresh.occurrences_of(term),
                "occurrences of {term} drifted"
            );
        }
    }

    /// The whole safety argument for patching: whatever the snapshot says
    /// after a write has to be what a scan of the database would say.
    fn assert_matches_a_fresh_scan(store: &Store, workspace: &str) {
        let patched: Vec<Value> = store
            .live(workspace)
            .unwrap()
            .iter()
            .map(|a| Value::Object(Record::clone(a)))
            .collect();
        // Force the next read to derive from the database rather than the
        // cache, and compare what comes back.
        store.live.write().unwrap().clear();
        let fresh: Vec<Value> = store
            .live(workspace)
            .unwrap()
            .iter()
            .map(|a| Value::Object(Record::clone(a)))
            .collect();
        assert_eq!(
            patched, fresh,
            "the patched snapshot drifted from the store"
        );
    }

    #[test]
    fn a_patched_snapshot_says_what_a_fresh_scan_says() {
        let (_dir, store) = store();
        // Read first, so there is a cached snapshot for the writes to fold
        // into rather than nothing to patch.
        assert!(store.live("w").unwrap().is_empty());

        store
            .upsert(&record(json!({"id": "a", "workspace": "w", "text": "a"})))
            .unwrap();
        assert_matches_a_fresh_scan(&store, "w");

        // An update in place.
        store
            .upsert(&record(
                json!({"id": "a", "workspace": "w", "text": "changed"}),
            ))
            .unwrap();
        assert_eq!(store.live("w").unwrap()[0]["text"], json!("changed"));
        assert_matches_a_fresh_scan(&store, "w");

        // A record that leaves the live set has to leave the snapshot.
        store
            .upsert(&record(json!({
                "id": "a", "workspace": "w", "text": "changed",
                "valid_to": "2000-01-01T00:00:00.000Z"
            })))
            .unwrap();
        assert!(store.live("w").unwrap().is_empty());
        assert_matches_a_fresh_scan(&store, "w");

        // And one that is expired but still due stays, because the review
        // clock is a separate question.
        store
            .upsert(&record(json!({
                "id": "b", "workspace": "w", "text": "b",
                "valid_to": "2000-01-01T00:00:00.000Z",
                "due_at": "2000-01-01T00:00:00.000Z"
            })))
            .unwrap();
        assert_eq!(store.live("w").unwrap().len(), 1);
        assert_matches_a_fresh_scan(&store, "w");
    }

    #[test]
    fn a_patch_narrows_links_the_way_a_scan_does() {
        let (_dir, store) = store();
        assert!(store.live("w").unwrap().is_empty());
        store
            .upsert(&record(json!({
                "id": "a", "workspace": "w", "text": "a", "links": ["b"]
            })))
            .unwrap();
        // `b` does not exist, so the link must not be reported.
        assert_eq!(store.live("w").unwrap()[0]["links"], json!([]));
        assert_matches_a_fresh_scan(&store, "w");

        store
            .upsert(&record(json!({"id": "b", "workspace": "w", "text": "b"})))
            .unwrap();
        assert_matches_a_fresh_scan(&store, "w");
    }

    #[test]
    fn a_write_to_one_workspace_leaves_another_alone() {
        let (_dir, store) = store();
        store
            .upsert(&record(json!({"id": "a", "workspace": "one", "text": "a"})))
            .unwrap();
        assert_eq!(store.live("one").unwrap().len(), 1);
        assert!(store.live("two").unwrap().is_empty());
        store
            .upsert(&record(json!({"id": "b", "workspace": "two", "text": "b"})))
            .unwrap();
        assert_matches_a_fresh_scan(&store, "one");
        assert_matches_a_fresh_scan(&store, "two");
    }

    #[test]
    fn a_delete_is_visible_and_matches_a_scan() {
        let (_dir, store) = store();
        store
            .upsert(&record(json!({"id": "a", "workspace": "w", "text": "a"})))
            .unwrap();
        assert_eq!(store.live("w").unwrap().len(), 1);
        store.delete("w", "a", None).unwrap();
        assert!(store.live("w").unwrap().is_empty());
        assert_matches_a_fresh_scan(&store, "w");
    }

    /// A reader that has seen a live set never then sees one that skipped
    /// a write.
    ///
    /// `PACKSET_SNAPSHOT_STRESS`, set to a number, repeats the race that
    /// many times in this process so it can be run in a loop.
    #[test]
    fn readers_racing_a_writer_never_see_a_snapshot_that_skips_a_write() {
        let rounds = std::env::var("PACKSET_SNAPSHOT_STRESS")
            .ok()
            .and_then(|raw| raw.trim().parse::<usize>().ok())
            .filter(|rounds| *rounds > 0)
            .unwrap_or(1);
        for _ in 0..rounds {
            race_one_writer_against_readers();
        }
    }

    fn race_one_writer_against_readers() {
        const READERS: usize = 8;
        const WRITES: usize = 400;
        let (dir, store) = store();
        let store = Arc::new(store);
        let _ = dir;
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let writer = {
            let store = Arc::clone(&store);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                for i in 0..WRITES {
                    store
                        .upsert(&record(json!({
                            "id": format!("a{i}"), "workspace": "w", "text": "x"
                        })))
                        .unwrap();
                }
                stop.store(true, std::sync::atomic::Ordering::Release);
            })
        };

        let readers: Vec<_> = (0..READERS)
            .map(|_| {
                let store = Arc::clone(&store);
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    let mut high = 0usize;
                    while !stop.load(std::sync::atomic::Ordering::Acquire) {
                        let seen = store.live("w").unwrap().len();
                        assert!(seen >= high, "went backwards: {seen} after {high}");
                        high = seen;
                    }
                    // The writer has stopped, so this read is the published set.
                    let seen = store.live("w").unwrap().len();
                    assert!(seen >= high, "went backwards: {seen} after {high}");
                })
            })
            .collect();

        writer.join().unwrap();
        for reader in readers {
            reader.join().unwrap();
        }
        assert_eq!(store.live("w").unwrap().len(), WRITES, "every write landed");
    }

    /// Two fat snapshots do not both stay when the byte cap fits one.
    /// The colder workspace is read from the database again.
    #[test]
    fn a_small_cache_budget_drops_the_colder_snapshot() {
        let (_dir, store) = store();
        store.set_workspace_cache(0);
        let fat = "x".repeat(8_000);
        store
            .upsert(&record(json!({
                "id": "a", "workspace": "cold", "text": fat
            })))
            .unwrap();
        assert_eq!(store.live("cold").unwrap().len(), 1);
        let one = store.cache_resident_bytes();
        assert!(one > 0, "the snapshot was not counted");
        store.set_cache_limit_bytes(one);
        store
            .upsert(&record(json!({
                "id": "b", "workspace": "hot", "text": fat
            })))
            .unwrap();
        assert_eq!(store.live("hot").unwrap().len(), 1);
        assert_eq!(
            store.cached_workspaces(),
            1,
            "both snapshots stayed under a one-snapshot cap"
        );
        let before = store.scanned.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(store.live("cold").unwrap().len(), 1);
        assert!(
            store.scanned.load(std::sync::atomic::Ordering::SeqCst) > before,
            "the cold snapshot was still cached"
        );
    }

    /// N readers, while a write holds the publish lock, each answer from
    /// the snapshot already swapped in. None of them wait the write out.
    #[test]
    fn a_burst_of_readers_does_not_wait_on_a_held_write() {
        const READERS: usize = 20;
        let (_dir, store) = store();
        let store = Arc::new(store);
        store
            .upsert(&record(
                json!({"id": "a", "workspace": "w", "text": "alpha"}),
            ))
            .unwrap();
        assert_eq!(store.live("w").unwrap().len(), 1);
        let _indexed = store.searchable("w").unwrap();

        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let writing = Arc::clone(&store);
        let holder = std::thread::spawn(move || {
            let _flag = writing.mark_writing();
            let _publish = writing.publish.write().unwrap_or_else(|e| e.into_inner());
            // The generation moves before the snapshot does. A reader that
            // rebuilds here waits on this lock; one that serves the
            // published set does not.
            let _bumped = writing
                .generation
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            ready_tx.send(()).unwrap();
            let _ = release_rx.recv();
        });
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("the write did not take the lock");

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        for _ in 0..READERS {
            let store = Arc::clone(&store);
            let done_tx = done_tx.clone();
            std::thread::spawn(move || {
                let started = std::time::Instant::now();
                let live = store.live("w").map(|atoms| atoms.len());
                let search = store.searchable("w").map(|set| set.0.len());
                let _ = done_tx.send((started.elapsed(), live, search));
            });
        }
        drop(done_tx);
        let mut answered = 0usize;
        let deadline = std::time::Duration::from_millis(500);
        while answered < READERS {
            match done_rx.recv_timeout(deadline) {
                Ok((took, live, search)) => {
                    assert!(
                        took < deadline,
                        "a reader waited {took:?} on the held write"
                    );
                    assert_eq!(live.unwrap(), 1);
                    assert_eq!(search.unwrap(), 1);
                    answered += 1;
                }
                Err(_) => {
                    let _ = release_tx.send(());
                    holder.join().ok();
                    panic!(
                        "{answered} of {READERS} readers answered; the rest blocked on the write"
                    );
                }
            }
        }
        let _ = release_tx.send(());
        holder.join().unwrap();
    }
}
