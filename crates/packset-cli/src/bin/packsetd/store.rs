//! Atoms in LMDB: key `workspace\0id`, value the record as JSON. A seat's
//! existing `memory.lmdb` opens here unchanged; the NUL makes a workspace
//! scan a prefix scan.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use heed::types::Bytes;
use heed::{Database, Env, EnvOpenOptions};
use packset_core::bm25::Index;
use packset_core::record::{self, AtomError};
use serde_json::{Map, Value};

/// The map the environment can grow into. Growing it is compatible;
/// shrinking it below what is stored is not.
///
/// The data file holds the pages in use and extends as writes need new
/// pages, up to this map. It is not extended to the map at open: doing
/// that reserved a gibibyte per seat, and a file extended that far came
/// back fully allocated. `ls -ls` reported 1048580 blocks for a store
/// holding one or two claims.
pub const MAP_SIZE: usize = 1024 * 1024 * 1024;

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

/// Keep the stored `ts` on a rewrite and stamp `updated`. A rewrite of
/// links, a review clock or a closed window used to put the time of the
/// rewrite in `ts`, so the time the atom was written was lost.
fn stamp_write(payload: &mut Record, stored: Option<&Record>, now: &str) {
    if let Some(created) = stored
        .and_then(|old| old.get("ts"))
        .and_then(Value::as_str)
        .filter(|ts| !ts.is_empty())
    {
        payload.insert("ts".into(), Value::String(created.to_string()));
    }
    payload.insert(UPDATED.into(), Value::String(now.to_string()));
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

/// Stamp every record a write before [`UPDATED`] left: its `ts` becomes
/// `updated`, and `ts` goes back to the time it was created where
/// [`created_before_rewrite`] still finds it. A record that has `updated`
/// is left alone, so a second open writes nothing.
fn restore_created(env: &Env, db: Database<Bytes, Bytes>) -> anyhow::Result<usize> {
    let mut fixed: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    {
        let rtxn = env.read_txn()?;
        for item in db.iter(&rtxn)? {
            let (key, raw) = item?;
            let Ok(Value::Object(mut atom)) = serde_json::from_slice::<Value>(raw) else {
                continue;
            };
            if atom.contains_key(UPDATED) {
                continue;
            }
            let Some(last) = atom
                .get("ts")
                .and_then(Value::as_str)
                .filter(|ts| !ts.is_empty())
                .map(str::to_string)
            else {
                continue;
            };
            if let Some(created) = created_before_rewrite(&atom) {
                atom.insert("ts".into(), Value::String(created));
            }
            atom.insert(UPDATED.into(), Value::String(last));
            fixed.push((key.to_vec(), serde_json::to_vec(&Value::Object(atom))?));
        }
    }
    if fixed.is_empty() {
        return Ok(0);
    }
    let mut wtxn = env.write_txn()?;
    for (key, blob) in &fixed {
        db.put(&mut wtxn, key, blob)?;
    }
    wtxn.commit()?;
    Ok(fixed.len())
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

/// The atom database, plus the lock that makes it one writer.
pub struct Store {
    env: Env,
    db: Database<Bytes, Bytes>,
    /// Held open for as long as the store is: dropping it drops the lock.
    _lock: File,
    /// Bumped by every write, so a reader can tell a stale snapshot.
    generation: AtomicU64,
    /// One parsed live set per workspace, shared by concurrent readers.
    live: RwLock<HashMap<String, Snapshot>>,
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
        // writes it, which the lock above is what enforces. The map is
        // [`MAP_SIZE`]; the file grows with the pages written.
        let env = unsafe {
            EnvOpenOptions::new()
                .map_size(MAP_SIZE)
                .max_dbs(1)
                .open(&db_path)?
        };
        let mut wtxn = env.write_txn()?;
        let db: Database<Bytes, Bytes> = env.create_database(&mut wtxn, None)?;
        wtxn.commit()?;
        let restored = restore_created(&env, db)?;
        if restored > 0 {
            eprintln!("packsetd: stamped {restored} records written before `updated`");
        }
        // A store created while the file was extended to the map still has
        // that tail. The pages at and below the last used page are the
        // database; the rest can go back to the filesystem. A later write
        // extends the file from the last page.
        trim_to_used_pages(&env, db, &db_path)?;
        Ok(Self {
            env,
            db,
            _lock: lock,
            generation: AtomicU64::new(0),
            live: RwLock::new(HashMap::new()),
            terms: RwLock::new(HashMap::new()),
            terms_build: Mutex::new(()),
            counts: RwLock::new(HashMap::new()),
            counts_build: Mutex::new(()),
            workspace_cache: AtomicUsize::new(workspace_cache_from_env()),
            cache_clock: AtomicU64::new(1),
            cache_used: Mutex::new(HashMap::new()),
            db_dir: db_path,
            #[cfg(test)]
            counted: AtomicU64::new(0),
            #[cfg(test)]
            scanned: AtomicU64::new(0),
        })
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

    /// One record by id, whatever its state.
    ///
    /// # Errors
    ///
    /// Fails when the read transaction does.
    pub fn get(&self, workspace: &str, id: &str) -> anyhow::Result<Option<Record>> {
        if workspace.is_empty() || id.is_empty() {
            return Ok(None);
        }
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
        let mut written = Vec::with_capacity(atoms.len());
        let mut wtxn = self.env.write_txn()?;
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
                .get(&wtxn, &key)?
                .and_then(|raw| serde_json::from_slice::<Value>(raw).ok())
                .and_then(|v| match v {
                    Value::Object(map) => Some(map),
                    _ => None,
                });
            stamp_write(&mut payload, stored.as_ref(), now);
            // A fire or a grade rewrites the record from the cache, which
            // does not carry the vector. A null or an empty array is a
            // clear. A write that leaves the key off keeps what is stored.
            keep_stored_embedding(&mut payload, stored.as_ref());
            let blob = serde_json::to_vec(&Value::Object(payload.clone()))?;
            self.db.put(&mut wtxn, &key, &blob)?;
            written.push(payload);
        }
        wtxn.commit()?;
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
                self.note_cached(workspace);
            }
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

    /// Drop the coldest parsed workspaces until the cap holds. `keep` is the
    /// workspace a caller just built and must not be the one dropped. Zero
    /// is no cap. The index and the status counts leave with the snapshot:
    /// they are the other copies of the same corpus.
    fn evict_cached(&self, cache: &mut HashMap<String, Snapshot>, keep: &str) {
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
            if let Ok(mut terms) = self.terms.write() {
                terms.remove(&name);
            }
            if let Ok(mut counts) = self.counts.write() {
                counts.remove(&Some(name.clone()));
            }
            if let Ok(mut used) = self.cache_used.lock() {
                used.remove(&name);
            }
        }
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
    #[cfg(test)]
    pub fn cached_workspaces(&self) -> usize {
        self.live.read().map(|cache| cache.len()).unwrap_or(0)
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

    fn release_pages(&self) {
        // Linux only. The same advice is not applied on macOS: there
        // `MADV_DONTNEED` can discard dirty pages. Everywhere else this
        // function is a no-op and the mapped file stays resident.
        #[cfg(target_os = "linux")]
        {
            // Open a write transaction and drop it at once, so a commit
            // already in progress finishes before pages leave the map.
            // The lock is not held across the advice, and it does not
            // need to be. The map is a read-only shared view: LMDB writes
            // with `write`, not `WRITEMAP`. Advising that view reloads
            // the file. Advising the file descriptor does not; the pages
            // stay mapped.
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
        let generation = self.generation.load(Ordering::Acquire);
        if let Ok(cache) = self.live.read() {
            if let Some(found) = cache.get(workspace) {
                if found.generation == generation {
                    self.note_cached(workspace);
                    return Ok(LiveSnapshot {
                        atoms: Arc::clone(&found.shown),
                        vectors: Arc::clone(&found.vectors),
                        originals: Arc::clone(&found.originals),
                        generation,
                    });
                }
            }
        }
        // Built outside the write lock, so a slow parse does not hold up a
        // reader whose own workspace is current. The vector is lifted out
        // of the record here, once. The cached copies omit the key, and the
        // original numbers are kept beside the floats for answers.
        let now = packset_core::clock::utcnow();
        let scanned: Vec<Record> = self
            .scan(Some(workspace))?
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
        // Cached only if nothing committed while the scan ran. The caller
        // still receives the vectors this scan lifted: they are not a
        // second read of a cache that may not hold them.
        if self.generation.load(Ordering::Acquire) == generation {
            if let Ok(mut cache) = self.live.write() {
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
            }
        }
        Ok(LiveSnapshot {
            atoms: shared,
            vectors,
            originals,
            generation,
        })
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
        // later generation patches from it rather than rebuilding.
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
            .scan(Some(workspace))?
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
        for atom in self.scan(None)? {
            let Some(name) = atom.get("workspace").and_then(Value::as_str) else {
                continue;
            };
            if name.is_empty() {
                continue;
            }
            let slot = counts.entry(name.to_string()).or_insert(0);
            if record::is_live(&atom, &now) {
                *slot += 1;
            }
        }
        counts.entry("global".into()).or_insert(0);
        Ok(counts.into_iter().collect())
    }

    /// Tombstone one live record, optionally naming the deed that withdrew it.
    ///
    /// The whole record is carried onto the tombstone, so `why` lands beside
    /// the text it retracts and a bitemporal read gets both at once.
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
        if !keys.is_empty() {
            let mut wtxn = self.env.write_txn()?;
            for key in &keys {
                self.db.delete(&mut wtxn, key)?;
            }
            wtxn.commit()?;
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
/// again, up to [`MAP_SIZE`].
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
fn take_lock(path: &Path) -> anyhow::Result<File> {
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
    Ok(file)
}

const LOCK_EX: i32 = 2;
const LOCK_NB: i32 = 4;

extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
}

/// Ask glibc to return free arenas to the kernel. A burst allocates the
/// high-water mark into per-thread arenas and, without this, holds it
/// until the process ends. musl has no `malloc_trim`. macOS can relieve
/// a zone, but that call is not exercised here, so on musl and on macOS
/// this is a no-op and the high-water mark stays until the process ends.
#[cfg(target_env = "gnu")]
fn release_heap() {
    // SAFETY: trim does not move live allocations.
    let _ = unsafe { malloc_trim(0) };
}

#[cfg(not(target_env = "gnu"))]
fn release_heap() {}

#[cfg(target_env = "gnu")]
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

    /// Open a home this test closed a moment ago. A test running beside
    /// this one can fork a child (`git`, `cat`, the reader in
    /// `a_foreign_reader_keeps_the_tail`) while the earlier store is open.
    /// Until that child execs, it holds a copy of the lock file's open
    /// description, and with it the `flock`, so the lock is right to
    /// refuse. The reopen waits for the child to exec, up to five seconds.
    fn reopen(home: &Path) -> Store {
        let started = std::time::Instant::now();
        loop {
            match Store::open(home) {
                Ok(store) => return store,
                Err(err)
                    if err.to_string() == "store home is already open"
                        && started.elapsed() < std::time::Duration::from_secs(5) =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(err) => panic!("reopen {}: {err}", home.display()),
            }
        }
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

    /// A fresh store with a few atoms stays a handful of pages. The map is
    /// still [`MAP_SIZE`], so this is the allocated size, `st_blocks * 512`.
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
    #[cfg(unix)]
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
        let store = reopen(dir.path());
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
    #[cfg(unix)]
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
            let store = reopen(&home);
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
            let store = reopen(&home);
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

    #[cfg(unix)]
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
    fn a_record_from_before_updated_gets_its_written_time_back_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let rewritten = "2026-10-18T19:31:19.444Z";
        let rows = [
            // Restamped by a link rewrite; valid_from kept the write.
            json!({"id": "moved", "workspace": "w", "text": "a",
                   "ts": rewritten, "valid_from": "2026-10-16T10:07:21.644Z",
                   "source": {"at": "2026-10-16T10:07:21Z"}}),
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
        let store = reopen(dir.path());
        let got = |id: &str| store.get("w", id).unwrap().unwrap();
        assert_eq!(got("moved")["ts"], json!("2026-10-16T10:07:21.644Z"));
        assert_eq!(got("moved")[UPDATED], json!(rewritten));
        assert_eq!(got("kept")["ts"], json!("2026-10-12T08:00:00.000Z"));
        assert_eq!(got("kept")[UPDATED], json!("2026-10-12T08:00:00.000Z"));
        assert_eq!(got("dated")["ts"], json!("2026-10-14T14:15:41.000Z"));
        assert_eq!(got("ahead")["ts"], json!("2026-10-18T19:00:00.000Z"));
        assert_eq!(got("bare")["ts"], json!(rewritten));
        assert_eq!(got("bare")[UPDATED], json!(rewritten));
        let before: Vec<Record> = store.scan(Some("w")).unwrap();
        drop(store);
        let again = reopen(dir.path());
        assert_eq!(
            again.scan(Some("w")).unwrap(),
            before,
            "a second open rewrote"
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
        check(&reopen(dir.path()));
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
    /// replaced would have been a few mebibytes.
    #[cfg(target_os = "linux")]
    #[test]
    fn cached_vectors_do_not_keep_the_json_arrays() {
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

    #[test]
    fn a_second_writer_is_refused_the_home() {
        let dir = tempfile::tempdir().unwrap();
        let _first = Store::open(dir.path()).unwrap();
        let second = Store::open(dir.path());
        assert!(second.is_err(), "one writer is the whole design");
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

    #[test]
    fn readers_racing_a_writer_never_see_a_snapshot_that_skips_a_write() {
        // Generation read before the scan, bumped before the write: a snapshot
        // built across a write is stale, never mislabelled.
        let (dir, store) = store();
        let store = Arc::new(store);
        let _ = dir;
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let writer = {
            let store = Arc::clone(&store);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                for i in 0..200 {
                    store
                        .upsert(&record(json!({
                            "id": format!("a{i}"), "workspace": "w", "text": "x"
                        })))
                        .unwrap();
                }
                stop.store(true, std::sync::atomic::Ordering::Release);
            })
        };

        let readers: Vec<_> = (0..4)
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
                })
            })
            .collect();

        writer.join().unwrap();
        for reader in readers {
            reader.join().unwrap();
        }
        assert_eq!(store.live("w").unwrap().len(), 200, "every write landed");
    }
}
