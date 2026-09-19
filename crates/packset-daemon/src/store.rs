//! Atoms in LMDB: key `workspace\0id`, value the record as JSON. A seat's
//! existing `memory.lmdb` opens here unchanged; the NUL makes a workspace
//! scan a prefix scan.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use heed::types::Bytes;
use heed::{Database, Env, EnvFlags, EnvOpenOptions};
use packset_core::bm25::Index;
use packset_core::record::{self, AtomError};
use serde_json::{Map, Value};

/// The map size the environment opens with. Growing it is compatible;
/// shrinking it below what is stored is not.
pub const MAP_SIZE: usize = 1024 * 1024 * 1024;

/// One atom record.
pub type Record = Map<String, Value>;

/// One workspace's live set at a write count: as stored (what a write folds
/// into) and as shown (links narrowed to ids present).
type Snapshot = (u64, Vec<Record>, Arc<Vec<Record>>, HashSet<String>);

/// A workspace's index at one generation: the id and stamp of each atom
/// the index was built over, in order, beside the index and its tokens.
/// The atoms themselves are not held here, so the live set stays unshared
/// between writes and a write edits it in place rather than copying it.
type Searchable = (
    u64,
    (Vec<(String, String)>, Arc<Index>, Arc<Vec<Vec<String>>>),
);

/// The id and stamp of each atom, the fingerprint an index is keyed on.
fn fingerprint(atoms: &[Record]) -> Vec<(String, String)> {
    atoms
        .iter()
        .map(|a| {
            (
                a.get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                a.get("ts")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            )
        })
        .collect()
}

/// What a search runs over: the snapshot, the inverted index, and the tokens
/// the index was built from, all shared.
pub type SearchSet = (Arc<Vec<Record>>, Arc<Index>, Arc<Vec<Vec<String>>>);

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
}

impl Store {
    /// Open the database under `root`, taking the single-writer lock.
    ///
    /// # Errors
    ///
    /// Fails when the home cannot be created, when another process already
    /// holds the lock, or when LMDB refuses the directory.
    pub fn open(root: &Path) -> anyhow::Result<Self> {
        fs::create_dir_all(root)?;
        let lock = take_lock(&root.join("packsetd.lock"))?;
        let db_path = root.join("memory.lmdb");
        fs::create_dir_all(&db_path)?;
        // SAFETY: LMDB maps the file; the contract is that no other process
        // writes it, which the lock above is what enforces.
        let env = unsafe {
            EnvOpenOptions::new()
                .map_size(MAP_SIZE)
                .max_dbs(1)
                .flags(EnvFlags::WRITE_MAP)
                .open(&db_path)?
        };
        let mut wtxn = env.write_txn()?;
        let db: Database<Bytes, Bytes> = env.create_database(&mut wtxn, None)?;
        wtxn.commit()?;
        Ok(Self {
            env,
            db,
            _lock: lock,
            generation: AtomicU64::new(0),
            live: RwLock::new(HashMap::new()),
            terms: RwLock::new(HashMap::new()),
            terms_build: Mutex::new(()),
        })
    }

    /// Every record in one workspace, or in all of them.
    ///
    /// # Errors
    ///
    /// Fails when the read transaction does.
    pub fn scan(&self, workspace: Option<&str>) -> anyhow::Result<Vec<Record>> {
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
        self.upsert_many(std::slice::from_ref(atom))
    }

    /// Write several records in one transaction, so a link rewrite lands whole.
    ///
    /// # Errors
    ///
    /// Fails when a record has no workspace or id, or when the write does.
    pub fn upsert_many(&self, atoms: &[Record]) -> anyhow::Result<()> {
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
            let blob = serde_json::to_vec(&Value::Object(payload.clone()))?;
            self.db.put(&mut wtxn, &key, &blob)?;
        }
        wtxn.commit()?;
        // After the commit, never before: a reader that scans between a bump
        // and its write would otherwise cache the older corpus as the newer.
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.patch_live(atoms, generation);
        Ok(())
    }

    /// Fold a committed write into the cached snapshot instead of dropping it.
    /// Only cached workspaces are patched; the result equals a fresh scan.
    fn patch_live(&self, written: &[Record], generation: u64) {
        let now = packset_core::clock::utcnow();
        let Ok(mut cache) = self.live.write() else {
            return;
        };
        for (workspace, (seen, stored, shown, dangling)) in cache.iter_mut() {
            // One behind is this write; anything else raced and the snapshot
            // is not a base this write can be added to.
            if *seen + 1 != generation {
                continue;
            }
            for record in written {
                if record.get("workspace").and_then(Value::as_str) != Some(workspace.as_str()) {
                    continue;
                }
                let Some(id) = record.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let visible = shown_at(record, &now);
                let at = stored
                    .iter()
                    .position(|a| a.get("id").and_then(Value::as_str) == Some(id));
                match (at, visible) {
                    (Some(i), true) => stored[i] = record.clone(),
                    (Some(i), false) => {
                        stored.remove(i);
                    }
                    (None, true) => stored.push(record.clone()),
                    (None, false) => {}
                }
            }
            *seen = generation;
            patch_shown(shown, stored, written, &now, dangling);
        }
    }

    /// The live and due records in one workspace, parsed once per write and
    /// shared; readers take this over [`Store::current`].
    ///
    /// # Errors
    ///
    /// Fails when the scan does.
    pub fn live(&self, workspace: &str) -> anyhow::Result<Arc<Vec<Record>>> {
        self.live_versioned(workspace).map(|(shown, _)| shown)
    }

    /// [`Self::live`] with the generation the set belongs to, for a cache
    /// keyed on it.
    ///
    /// # Errors
    ///
    /// Fails when the scan does.
    pub fn live_versioned(&self, workspace: &str) -> anyhow::Result<(Arc<Vec<Record>>, u64)> {
        let generation = self.generation.load(Ordering::Acquire);
        if let Ok(cache) = self.live.read() {
            if let Some((seen, _stored, shown, _dangling)) = cache.get(workspace) {
                if *seen == generation {
                    return Ok((Arc::clone(shown), generation));
                }
            }
        }
        // Built outside the write lock, so a slow parse does not hold up a
        // reader whose own workspace is current.
        let now = packset_core::clock::utcnow();
        let stored: Vec<Record> = self
            .scan(Some(workspace))?
            .into_iter()
            .filter(|atom| shown_at(atom, &now))
            .collect();
        let (shown, dangling) = shown_from(&stored);
        let shared = Arc::new(shown);
        // Cached only if nothing committed while the scan ran.
        if self.generation.load(Ordering::Acquire) == generation {
            if let Ok(mut cache) = self.live.write() {
                cache.insert(
                    workspace.to_string(),
                    (generation, stored, Arc::clone(&shared), dangling),
                );
            }
        }
        Ok((shared, generation))
    }

    /// One workspace's live atoms and the index over them, as a matched pair;
    /// cards are scored against the same corpus.
    ///
    /// A write patches the live set in place, so the cached set is usually
    /// the new one with records rewritten at their positions and appended at
    /// the end. The index follows the same way: a rewritten record is
    /// re-indexed under its ordinal, an appended one is pushed, and only a
    /// set whose order moved is rebuilt from scratch.
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
        let (atoms, generation) = self.live_versioned(workspace)?;
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
                for (ordinal, ((_, old_ts), new)) in old_atoms.iter().zip(atoms.iter()).enumerate()
                {
                    if Some(old_ts.as_str()) != new.get("ts").and_then(Value::as_str) {
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
                    .map(packset_core::search::atom_tokens)
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
        }
        Ok((atoms, index, documents))
    }

    /// The cached index when it matches the live set's generation.
    fn searchable_cached(&self, workspace: &str) -> anyhow::Result<Option<SearchSet>> {
        let (atoms, generation) = self.live_versioned(workspace)?;
        if let Ok(cache) = self.terms.read() {
            if let Some((seen, (_, index, documents))) = cache.get(workspace) {
                if *seen == generation && atoms.len() == index.len() {
                    return Ok(Some((atoms, Arc::clone(index), Arc::clone(documents))));
                }
            }
        }
        Ok(None)
    }

    /// The atoms that were live at `at`, from a store scan since the snapshot
    /// drops closed windows.
    ///
    /// # Errors
    ///
    /// Fails when the scan does, or when `at` is not a timestamp.
    pub fn as_of(&self, workspace: &str, at: &str) -> anyhow::Result<Vec<Record>> {
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
        Ok(match set {
            None => live.as_ref().clone(),
            Some(name) => live
                .iter()
                .filter(|atom| atom.get("set").and_then(Value::as_str) == Some(name))
                .cloned()
                .collect(),
        })
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
        tomb.insert("ts".into(), Value::String(packset_core::clock::utcnow()));
        if let Some(accession) = why {
            tomb.insert("retracted_by".into(), Value::String(accession.to_string()));
        }
        self.upsert(&tomb)?;
        Ok(tomb)
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
fn shown_from(stored: &[Record]) -> (Vec<Record>, HashSet<String>) {
    let live: HashSet<&str> = stored
        .iter()
        .filter_map(|a| a.get("id").and_then(Value::as_str))
        .collect();
    let mut dangling = HashSet::new();
    let mut shown = stored.to_vec();
    for atom in &mut shown {
        dangling.extend(cut_links(atom, &live));
    }
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
    shown: &mut Arc<Vec<Record>>,
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
                Some(i) => out[i] = copy,
                None => {
                    out.push(copy);
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
            out[i] = copy;
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

fn push_record(out: &mut Vec<Record>, raw: &[u8]) {
    if let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(raw) {
        out.push(map);
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

    /// The index served after a write scores every term as an index built
    /// from scratch over the same atoms would.
    fn assert_index_matches_a_fresh_build(store: &Store, workspace: &str) {
        let (atoms, index, documents) = store.searchable(workspace).unwrap();
        let fresh_docs: Vec<Vec<String>> = atoms
            .iter()
            .map(packset_core::search::atom_tokens)
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
            .map(|a| Value::Object(a.clone()))
            .collect();
        // Force the next read to derive from the database rather than the
        // cache, and compare what comes back.
        store.live.write().unwrap().clear();
        let fresh: Vec<Value> = store
            .live(workspace)
            .unwrap()
            .iter()
            .map(|a| Value::Object(a.clone()))
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
