//! What the writer does, apart from how a client asked.
//!
//! The HTTP layer decodes and encodes; everything a verb actually means lives
//! here, so the rules can be tested without a socket.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use packset_core::clock;
use packset_core::record::{self, AtomError, MEMORY_CAP, USER_CAP};
use serde_json::{json, Map, Value};

use crate::cards;
use crate::home::Home;
use crate::store::{Record, Store};

/// The cap on one attached body.
pub const ATTACH_CAP: usize = 200_000;
/// How many workspaces may hold an attachment at once. A slot that is
/// only peeked used to stay for the life of the process, one per
/// workspace the seat had ever named.
pub const ATTACH_SLOTS: usize = 64;
/// How long a peeked attachment may stay.
const ATTACH_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

/// One workspace's pending attachment.
#[derive(Debug, Clone, Default)]
pub struct Attachment {
    /// The body.
    pub text: String,
    /// What it is, for a reader.
    pub label: String,
}

/// An attachment and when it was stored. The clock is not part of the
/// answer a client reads.
struct HeldAttach {
    body: Attachment,
    at: std::time::Instant,
}

fn purge_attach(held: &mut BTreeMap<String, HeldAttach>, now: std::time::Instant) {
    held.retain(|_, slot| now.saturating_duration_since(slot.at) < ATTACH_TTL);
}

/// How many search hits seed an activation. Five is the recall@5 cut the
/// retrieval tables report, and it is large enough that two ballots can
/// agree on more than one hit. Fewer than two agreed seeds is a weak
/// island and is not fired. Seeding the long tail would spread from hits
/// the cue barely matched.
const ACTIVATION_SEEDS: usize = 5;
/// How far activation spreads. Each hop keeps half
/// ([`packset_core::island::HOP_DECAY`]), so two hops still carry a quarter
/// before fan-out. A third hop would carry an eighth, and a hub washes
/// that out. The island verb reports this spread.
const ACTIVATION_HOPS: usize = 2;
/// The most claims one consolidation bucket may hold before it is passed
/// over; a shared head of three words that a thousand claims open with is
/// not a rewrite candidate list.
const CONSOLIDATE_BUCKET_MAX: usize = 64;
/// How many of an island's strongest claims fire together when asked.
/// [`packset_core::island::FIRE_LINK_MAX`] is the most links one fire will
/// add, the same cap as a claim's neighbourhood. A larger set would ask
/// for edges the fire rule drops.
const FIRE_TOP: usize = packset_core::island::FIRE_LINK_MAX;

/// Entities with `horizon:standing` and without `horizon:transient`.
/// A recalled review and a consolidation survivor both call this.
fn promoted_entities(atom: &Record) -> Value {
    let horizon =
        packset_core::atom::promotion_horizon(record::origin_of(atom), record::content_of(atom));
    let mut list: Vec<Value> = atom
        .get("entities")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    list.retain(|v| v.as_str().is_none_or(|s| !s.starts_with("horizon:")));
    list.push(Value::String(format!("horizon:{horizon}")));
    Value::Array(list)
}
/// How long the same claims stay fired: a second fire inside the hour is
/// held. Follow-ups in one sitting must not tighten the same links twice.
/// The next hour may fire again.
const FIRE_WINDOW: std::time::Duration = std::time::Duration::from_secs(3600);

/// Record `key` unless it is already inside the window. Expired keys go,
/// then the oldest past `cap`. `cap` of zero keeps every unexpired key.
fn hold_key(recent: &mut HashMap<String, std::time::Instant>, key: String, cap: usize) -> bool {
    let now = std::time::Instant::now();
    if recent
        .get(&key)
        .is_some_and(|at| now.saturating_duration_since(*at) < FIRE_WINDOW)
    {
        return true;
    }
    recent.retain(|_, at| now.saturating_duration_since(*at) < FIRE_WINDOW);
    recent.insert(key, now);
    if cap > 0 && recent.len() > cap {
        let mut oldest: Vec<(String, std::time::Instant)> =
            recent.iter().map(|(k, at)| (k.clone(), *at)).collect();
        oldest.sort_by_key(|(_, at)| *at);
        let extra = recent.len() - cap;
        for (gone, _) in oldest.into_iter().take(extra) {
            recent.remove(&gone);
        }
    }
    false
}

/// The writer: the store, the cards, and the one-shot attach slots.
pub struct Service {
    home: Home,
    store: Store,
    /// Small appends admitted under memory pressure, durable before the
    /// answer is sent. Folded once the pressure mode clears.
    journal: Mutex<crate::journal::AppendLog>,
    /// Shared with the admission gate so a full log is refused there.
    log_usage: Arc<crate::admit::LogUsage>,
    attach: Mutex<BTreeMap<String, HeldAttach>>,
    /// One logical write at a time. LMDB serialises transactions, not the
    /// read-check-write a dedupe or a grade is, so two identical remembers
    /// arriving together must not both be stored.
    writes: Mutex<()>,
    /// Most live claims a workspace holds; zero is no cap. Past it, the
    /// least retrievable forgettable claims are tombstoned on the write
    /// that crossed it, so a herd of seats writing into one pack cannot
    /// grow it without bound.
    live_cap: usize,
    /// The day each workspace was last swept for neglect, so a write sweeps
    /// at most once a day.
    swept: Mutex<HashMap<String, String>>,
    /// What the replacement and linking rules read of each claim, by
    /// workspace and then by id. A claim's text never changes under its
    /// id, so a write tokenises the pack once, not once per write. Capped
    /// by dropping a cold workspace whole. One workspace larger than the
    /// cap keeps the shapes it has: dropping its own ids and clearing the
    /// shaped mark rebuilt the pack on every later write. A forgotten id
    /// leaves with its workspace's postings, which name only that
    /// workspace, so the same client id in another workspace is untouched.
    shapes: RwLock<HashMap<String, WorkspaceShapes>>,
    /// Most shapes kept. Zero is no cap. A single workspace may sit over
    /// it; two workspaces are trimmed back to it.
    shape_cap: AtomicUsize,
    /// Bumped when a shape is used, so a cold workspace can be dropped whole.
    shape_clock: AtomicU64,
    /// Workspaces whose live set has been shaped once in this process; after
    /// that every write shapes what it wrote, so the postings stay whole
    /// without a pass over the pack. Cleared when that workspace is dropped
    /// to make room, including when it alone is over the byte cap.
    shaped: Mutex<BTreeSet<String>>,
    /// Live claims by workspace and set. A write used to walk the whole
    /// workspace under the lock to find the set it belongs to; the dedup
    /// stage of that walk grew with the workspace. This map is patched as
    /// claims are added, updated and deleted, and a write reads its own set.
    live_sets: Mutex<HashMap<String, WorkspaceLive>>,
    /// When each set of claims last fired together, by workspace and the
    /// sorted ids: a second fire within [`FIRE_WINDOW`] is held, so several
    /// seats or personas closing sittings on one issue tighten its links
    /// once, not once each.
    fired_recently: Mutex<HashMap<String, std::time::Instant>>,
    /// Cue titles held the same way, in their own map. A burst of cues
    /// used to push fire keys out of the one shared cap, and the next
    /// fire of those claims tightened the links again inside the hour.
    cued_recently: Mutex<HashMap<String, std::time::Instant>>,
    /// Most keys kept in each of the fire map and the cue map. Zero is
    /// no cap.
    fire_cap: AtomicUsize,
    /// Projection writes waiting for the search index, in arrival order. A
    /// write queues its documents and returns; one batch reaches the index
    /// binary a moment later, or before the next read of it. Projecting
    /// each write on its own ran the indexer once per claim, and a fill of
    /// ten thousand claims spent most of its time there.
    projection: Arc<Mutex<Vec<ProjectionOp>>>,
    /// Held while queued writes are applied to the index, so a search waits
    /// for a flush in flight and a write never does. The queue's own lock
    /// is held only to push, to take or to count.
    projecting: Arc<Mutex<()>>,
    /// Whether a flush of the queue is already scheduled.
    flush_scheduled: Arc<AtomicBool>,
}

/// The stage clock of one write, written as a line to the file
/// `PACKSET_TRACE_WRITES` names when it is set; nothing otherwise. A fill
/// that grows slower with the pack is read from these lines, not guessed.
struct WriteTrace {
    path: Option<std::path::PathBuf>,
    last: std::time::Instant,
    stages: Vec<(&'static str, std::time::Duration)>,
}

impl WriteTrace {
    fn begin() -> Self {
        Self {
            path: std::env::var_os("PACKSET_TRACE_WRITES").map(std::path::PathBuf::from),
            last: std::time::Instant::now(),
            stages: Vec::new(),
        }
    }

    fn add(&mut self, stage: &'static str, took: std::time::Duration) {
        if self.path.is_some() {
            self.stages.push((stage, took));
        }
    }

    fn mark(&mut self, stage: &'static str) {
        if self.path.is_some() {
            let now = std::time::Instant::now();
            self.stages.push((stage, now - self.last));
            self.last = now;
        }
    }

    fn finish(&self, id: &str, live: usize) {
        let Some(path) = &self.path else {
            return;
        };
        use std::io::Write;
        let total: std::time::Duration = self.stages.iter().map(|(_, d)| *d).sum();
        let mut line = format!("{id} live={live} total={:.1}ms", total.as_secs_f64() * 1e3);
        for (stage, took) in &self.stages {
            line.push_str(&format!(" {stage}={:.1}", took.as_secs_f64() * 1e3));
        }
        line.push('\n');
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

/// The postings a shape is filed under: its tokens as they are, its first
/// head word and its entities behind markers so a name never meets a token.
fn posting_terms(shape: &record::Shape) -> Vec<String> {
    let mut terms: BTreeSet<String> = shape.tokens.iter().cloned().collect();
    if let Some(head) = head_key(shape) {
        terms.insert(head);
    }
    terms.extend(shape.entities.iter().map(|e| format!("e:{e}")));
    terms.into_iter().collect()
}

/// The posting a shared head is found under: the first `HEAD_MIN` head
/// words, which two claims must open with alike; a shorter head shares
/// with nothing.
fn head_key(shape: &record::Shape) -> Option<String> {
    (shape.head.len() >= record::HEAD_MIN)
        .then(|| format!("h:{}", shape.head[..record::HEAD_MIN].join(" ")))
}

/// The live set with mail left out. Islands, hubs and activation are memory.
fn memory_only(atoms: &[Arc<Record>]) -> Vec<Arc<Record>> {
    atoms
        .iter()
        .filter(|atom| {
            record::in_memory(atom.get("kind").and_then(Value::as_str).unwrap_or(""), None)
        })
        .cloned()
        .collect()
}

/// Whether a stored claim says what the new one says: same text, kind and
/// set, and for a rule the same pattern and verdict. Two rules that share a
/// reason and differ in glob or verdict are two laws.
fn same_claim(existing: &Record, atom: &Record) -> bool {
    // A repeated letter is a second atom. The text is the record.
    if existing
        .get("kind")
        .and_then(Value::as_str)
        .is_some_and(record::is_mail)
    {
        return false;
    }
    // The same words about another issue, agent or subject are another
    // claim: four notes that read alike on four tickets stay four records,
    // as `record::replaces_shaped` already keeps them apart.
    existing.get("text") == atom.get("text")
        && existing.get("kind") == atom.get("kind")
        && existing.get("set") == atom.get("set")
        && existing.get("pattern") == atom.get("pattern")
        && existing.get("verdict") == atom.get("verdict")
        && record::IDENTITY_KEYS
            .iter()
            .all(|key| existing.get(*key) == atom.get(*key))
}

/// One change the search index has yet to see.
#[derive(Debug, Clone)]
enum ProjectionOp {
    Upsert(Value),
    Delete(String),
}

/// How long the projection waits after the last write before it is indexed.
/// A burst is indexed once when it pauses. 200 ms is longer than the gap
/// between two writes of one turn and shorter than the pause between turns.
/// A search does not wait this out: a read flushes first.
const PROJECTION_DELAY: std::time::Duration = std::time::Duration::from_millis(200);
/// The longest a steady stream of writes keeps the index waiting. The
/// indexer is one process run per flush. On the default four workers, a
/// run every few seconds leaves the encoder its share; a run every write
/// did not. Three seconds is that bound.
const PROJECTION_MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(3);
/// Writes that end the wait early. A turn is a handful of atoms. 512 is
/// past any one turn, so a bulk import flushes in chunks instead of holding
/// the queue until [`PROJECTION_MAX_WAIT`].
const PROJECTION_BATCH: usize = 512;

/// Apply queued projection ops to the index at `dir`, consecutive runs of
/// one kind as one batch, in arrival order.
fn flush_projection_ops(ops: Vec<ProjectionOp>, dir: &std::path::Path) {
    let heavy = ops.len() >= PROJECTION_BATCH;
    let mut upserts: Vec<Value> = Vec::new();
    let mut deletes: Vec<String> = Vec::new();
    let mut last_upsert = true;
    for op in ops {
        match op {
            ProjectionOp::Upsert(doc) => {
                if !last_upsert && !deletes.is_empty() {
                    let _ = crate::milli::delete(&deletes, dir);
                    deletes.clear();
                }
                last_upsert = true;
                upserts.push(doc);
            }
            ProjectionOp::Delete(id) => {
                if last_upsert && !upserts.is_empty() {
                    let _ = crate::milli::upsert(&upserts, dir);
                    upserts.clear();
                }
                last_upsert = false;
                deletes.push(id);
            }
        }
    }
    if !upserts.is_empty() {
        let _ = crate::milli::upsert(&upserts, dir);
    }
    if !deletes.is_empty() {
        let _ = crate::milli::delete(&deletes, dir);
    }
    // A bulk import holds every document of the chunk. The copies are
    // gone; give the arenas back rather than keeping the high-water mark.
    if heavy {
        crate::store::release_heap();
    }
}

/// Apply what the queue holds. The ops are taken while the index is held,
/// so two flushes never apply out of order, and a search that waited for a
/// flush in flight then applies every write after it.
fn flush_queued(pending: &Mutex<Vec<ProjectionOp>>, projecting: &Mutex<()>, dir: &std::path::Path) {
    let _applying = projecting.lock().unwrap_or_else(|e| e.into_inner());
    let ops = std::mem::take(&mut *pending.lock().unwrap_or_else(|e| e.into_inner()));
    flush_projection_ops(ops, dir);
}

/// The live cap when `PACKSET_LIVE_CAP` says nothing: twenty thousand.
/// A query over ten thousand atoms is p50 31 to 64 ms on loopback
/// (`docs/orgmode/explanation.org`, 2026-10-08, release build). Twice that
/// pack, at the same cost again, is still about a tenth of a second, which
/// is what a hook can spend before it delays a prompt.
pub const DEFAULT_LIVE_CAP: usize = 20_000;

/// How many missed reviews, each past twice the claim's interval, tombstone
/// a forgettable claim that was never recalled and never fired. The first
/// miss lapses it and halves its stability. The second lapses the
/// rescheduled claim. The third forgets it. A habit, preference, goal,
/// prediction or rule is never forgotten this way.
pub const NEGLECT_LIMIT: u64 = 3;

/// Kinds the cap may forget. A standing choice, a rule, a reading, a goal, a
/// trust row and a persona are what the seat is; a lesson it has not
/// recalled is what it can afford to lose.
pub const FORGETTABLE_KINDS: &[&str] = &[
    "lesson",
    "conclusion",
    "summary",
    "note",
    "card_line",
    "belief",
    "voice",
    "cache-pointer",
    "correction",
];

/// Shapes and postings of one workspace. The postings name only this
/// workspace's ids.
#[derive(Default)]
struct WorkspaceShapes {
    by_id: HashMap<String, Shaped>,
    postings: HashMap<String, Vec<String>>,
}

/// One kept shape: the tokens, and when they were last used.
struct Shaped {
    shape: Arc<record::Shape>,
    used: AtomicU64,
}

/// Which set a live claim is compared in. A missing `set` is its own group.
/// A value that is not a string is neither: the snapshot filter skipped it
/// for both a named set and the unset group.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum SetGroup {
    Unset,
    Named(String),
    Other,
}

/// Live claims of one set, in the order a snapshot lists them. Dedupe of a
/// claim with no tokens walks this order and keeps the first match.
#[derive(Default)]
struct SetLive {
    order: Vec<Arc<Record>>,
    at: HashMap<String, usize>,
}

impl SetLive {
    fn remove_at(&mut self, id: &str) {
        let Some(i) = self.at.remove(id) else {
            return;
        };
        self.order.remove(i);
        for idx in self.at.values_mut() {
            if *idx > i {
                *idx -= 1;
            }
        }
    }
}

/// One workspace's live claims, split by set.
#[derive(Default)]
struct WorkspaceLive {
    sets: HashMap<SetGroup, SetLive>,
    /// The set each live id sits in.
    place: HashMap<String, SetGroup>,
    /// Soonest `valid_to` that can still drop a claim, when any indexed
    /// claim has one still open.
    ///
    /// A write walks the set only once `now` reaches this stamp. Walking
    /// every member on every write took 2.60 ms of the snapshot stage at
    /// 20,000 claims, against 0.00 ms with no walk
    /// (`measurements/ljos-11jv/notes.txt`). A claim still shown after its
    /// `valid_to` is on the review clock, and that clock does not drop it
    /// later, so a stamp that has passed is not kept here.
    earliest_expiry: Option<String>,
    /// The claim that held [`Self::earliest_expiry`] left, or its stamp
    /// moved later. The next write walks once and stores the new soonest.
    expiry_dirty: bool,
}

impl WorkspaceLive {
    /// A claim the snapshot would show: not a tombstone, and either live or
    /// still on the review clock.
    fn shown(atom: &Record, now: &str) -> bool {
        !record::is_tombstone(atom) && (record::is_live(atom, now) || record::is_due(atom, now))
    }

    fn group_of(atom: &Record) -> SetGroup {
        match atom.get("set") {
            None => SetGroup::Unset,
            Some(Value::String(name)) => SetGroup::Named(name.clone()),
            Some(_) => SetGroup::Other,
        }
    }

    /// The group a new claim is compared against. A set that is not a string
    /// yields no name, and the snapshot's unset arm keeps claims that omit
    /// the key.
    fn query_group(atom: &Record) -> SetGroup {
        match atom.get("set") {
            Some(Value::String(name)) => SetGroup::Named(name.clone()),
            _ => SetGroup::Unset,
        }
    }

    fn insert_shown(&mut self, atom: &Record) {
        let Some(id) = atom.get("id").and_then(Value::as_str) else {
            return;
        };
        let previous = self.indexed_valid_to(id);
        let group = Self::group_of(atom);
        let members = self.sets.entry(group.clone()).or_default();
        let shared = Arc::new(atom.clone());
        if let Some(&i) = members.at.get(id) {
            members.order[i] = shared;
        } else {
            members.at.insert(id.to_string(), members.order.len());
            members.order.push(shared);
            self.place.insert(id.to_string(), group);
        }
        if let Some(until) = Self::valid_to_of(atom) {
            self.consider_expiry(until);
        }
        if let Some(old) = previous {
            if Self::valid_to_of(atom) != Some(old.as_str())
                && self.earliest_expiry.as_deref() == Some(old.as_str())
            {
                self.expiry_dirty = true;
            }
        }
    }

    fn upsert_written(&mut self, mut record: Record, now: &str) {
        let Some(id) = record.get("id").and_then(Value::as_str).map(str::to_string) else {
            return;
        };
        if !Self::shown(&record, now) {
            self.remove_id(&id);
            return;
        }
        // The snapshot parks the vector. Dedupe reads the text, and a second
        // copy of the floats would sit beside the store's.
        record.remove("embedding");
        let new_until = Self::valid_to_of(&record).map(str::to_string);
        let previous = self.indexed_valid_to(&id);
        let group = Self::group_of(&record);
        if self.place.get(&id).is_some_and(|old| old != &group) {
            self.remove_id(&id);
        }
        let members = self.sets.entry(group.clone()).or_default();
        let shared = Arc::new(record);
        if let Some(&i) = members.at.get(&id) {
            members.order[i] = shared;
        } else {
            members.at.insert(id.clone(), members.order.len());
            members.order.push(shared);
            self.place.insert(id, group);
        }
        if let Some(until) = new_until.as_deref() {
            self.consider_expiry(until);
        }
        // The soonest stamp moved later, or left this claim. The cache still
        // names it, so the next write walks and stores the new soonest.
        if let Some(old) = previous {
            if new_until.as_deref() != Some(old.as_str())
                && self.earliest_expiry.as_deref() == Some(old.as_str())
            {
                self.expiry_dirty = true;
            }
        }
    }

    fn remove_id(&mut self, id: &str) {
        let held = self.indexed_valid_to(id);
        let Some(group) = self.place.remove(id) else {
            return;
        };
        if let Some(members) = self.sets.get_mut(&group) {
            members.remove_at(id);
            if members.order.is_empty() {
                self.sets.remove(&group);
            }
        }
        if let Some(until) = held {
            if self.earliest_expiry.as_deref() == Some(until.as_str()) {
                self.expiry_dirty = true;
            }
        }
    }

    /// `valid_to` as the live check reads it: a missing or empty stamp does
    /// not close the window.
    fn valid_to_of(atom: &Record) -> Option<&str> {
        match atom.get("valid_to") {
            Some(Value::String(stamp)) if !stamp.is_empty() => Some(stamp.as_str()),
            _ => None,
        }
    }

    /// The indexed claim's `valid_to`, when it has one.
    fn indexed_valid_to(&self, id: &str) -> Option<String> {
        let group = self.place.get(id)?;
        let members = self.sets.get(group)?;
        let &i = members.at.get(id)?;
        Self::valid_to_of(&members.order[i]).map(str::to_string)
    }

    /// Lower the cached stamp when `until` closes sooner than it.
    fn consider_expiry(&mut self, until: &str) {
        match self.earliest_expiry.as_deref() {
            Some(have) if until >= have => {}
            _ => self.earliest_expiry = Some(until.to_string()),
        }
    }

    /// Drop links that name an id the live set no longer has. The snapshot
    /// shows a claim with those links already cut.
    fn cut_links_in(record: &mut Record, live: impl Fn(&str) -> bool) {
        let Some(Value::Array(items)) = record.get("links") else {
            return;
        };
        let kept: Vec<Value> = items
            .iter()
            .filter(|item| live(record::value_text(item).as_str()))
            .cloned()
            .collect();
        if kept.len() != items.len() {
            record.insert("links".into(), Value::Array(kept));
        }
    }

    fn cut_every_link(&mut self) {
        let live: HashSet<String> = self.place.keys().cloned().collect();
        for members in self.sets.values_mut() {
            for record in &mut members.order {
                let mut copy = Record::clone(record);
                let before = copy.get("links").cloned();
                Self::cut_links_in(&mut copy, |id| live.contains(id));
                if copy.get("links") != before.as_ref() {
                    *record = Arc::new(copy);
                }
            }
        }
    }

    /// Drop claims [`Self::shown`] would now refuse.
    ///
    /// A claim is indexed when it is written. `valid_to` can pass later,
    /// with no write of that claim, and the claim stays in the set. The
    /// next write would then return it as a duplicate, offer it as a link
    /// or a replacement, and count it in the live cap. The readers of this
    /// map (`records_in`, `first_same`, `link_peers`, `indexed_records`,
    /// `indexed_len`, the shape pass and reconcile) run after this pass.
    /// It drops what a rebuilt snapshot would, and cuts links that named
    /// a claim that left.
    fn prune_expired(&mut self, now: &str) {
        // `valid_to` is exclusive, the same comparison as `atom::is_live`:
        // the window is still open while the stamp is greater than `now`.
        let due = self.expiry_dirty
            || self
                .earliest_expiry
                .as_deref()
                .is_some_and(|until| until <= now);
        if !due {
            return;
        }
        let mut stale: Vec<String> = Vec::new();
        let mut soonest: Option<String> = None;
        for rec in self.sets.values().flat_map(|members| members.order.iter()) {
            if !Self::shown(rec, now) {
                if let Some(id) = rec.get("id").and_then(Value::as_str) {
                    stale.push(id.to_string());
                }
                continue;
            }
            let Some(until) = Self::valid_to_of(rec) else {
                continue;
            };
            if until <= now {
                continue;
            }
            match soonest.as_deref() {
                Some(have) if until >= have => {}
                _ => soonest = Some(until.to_string()),
            }
        }
        if !stale.is_empty() {
            for id in &stale {
                self.remove_id(id);
            }
            self.cut_every_link();
        }
        self.earliest_expiry = soonest;
        self.expiry_dirty = false;
    }
}

/// Shapes kept when `PACKSET_SHAPE_CAP` is unset. The smallest power of
/// two that holds two workspaces at [`DEFAULT_LIVE_CAP`], so one checkout
/// stays shaped and a second still fits.
pub const DEFAULT_SHAPE_CAP: usize = (DEFAULT_LIVE_CAP * 2).next_power_of_two();

/// Keys kept in the fire map, and the same number in the cue map, when
/// `PACKSET_FIRE_CAP` is unset. One prompt a second across [`FIRE_WINDOW`]
/// is 3600 keys. This is the next power of two above that, so the hour
/// fits. The cue path used to insert without dropping expired keys, and
/// then shared this cap with fires, so a burst of titles pushed fires out.
pub const DEFAULT_FIRE_CAP: usize = (FIRE_WINDOW.as_secs() as usize).next_power_of_two();

fn fire_cap_from_env() -> usize {
    match std::env::var("PACKSET_FIRE_CAP") {
        Ok(raw) => match raw.trim() {
            "" => DEFAULT_FIRE_CAP,
            "off" | "none" => 0,
            n => n.parse().unwrap_or(DEFAULT_FIRE_CAP),
        },
        Err(_) => DEFAULT_FIRE_CAP,
    }
}

fn shape_cap_from_env() -> usize {
    match std::env::var("PACKSET_SHAPE_CAP") {
        Ok(raw) => match raw.trim() {
            "" => DEFAULT_SHAPE_CAP,
            "off" | "none" => 0,
            n => n.parse().unwrap_or(DEFAULT_SHAPE_CAP),
        },
        Err(_) => DEFAULT_SHAPE_CAP,
    }
}

fn live_cap_from_env() -> usize {
    match std::env::var("PACKSET_LIVE_CAP") {
        Ok(raw) => match raw.trim() {
            "" => DEFAULT_LIVE_CAP,
            "off" | "none" => 0,
            n => n.parse().unwrap_or(DEFAULT_LIVE_CAP),
        },
        Err(_) => DEFAULT_LIVE_CAP,
    }
}

/// The start-up line when `PACKSET_HOLD_AGENT_WRITES` is still set.
///
/// The name used to file an agent or hook write as a proposal until an
/// accept. That hold is gone, so any non-empty value is ignored and the
/// writer says so once, at open, where a seat who left it set can see it.
fn ignored_hold_notice(raw: Option<&str>) -> Option<&'static str> {
    let set = raw.is_some_and(|value| !value.trim().is_empty());
    set.then_some(
        "packsetd: PACKSET_HOLD_AGENT_WRITES is set and ignored; agent and hook writes are stored at once",
    )
}

/// The provenance line rides on the answer beside a warning the record
/// already carries, such as an unknown origin. The stored record keeps
/// its own line.
fn with_provenance_warning(atom: &mut Record, line: Option<String>) {
    let Some(line) = line else {
        return;
    };
    let joined = atom
        .get("warning")
        .and_then(Value::as_str)
        .filter(|prev| !prev.is_empty() && !prev.contains(line.as_str()))
        .map(|prev| format!("{prev} {line}"))
        .unwrap_or(line);
    atom.insert("warning".into(), Value::String(joined));
}

/// Whether `origin` is missing, null, or an empty string.
fn origin_is_blank(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => true,
        Some(Value::String(text)) if text.is_empty() => true,
        _ => false,
    }
}

/// A retraction's window, read as [`packset_core::atom::is_live_at`] would
/// read a record that is not a tombstone.
///
/// That predicate treats every tombstone as absent. That is right for a
/// forget: the words are gone and there is no `valid_to`. A retraction
/// keeps the words and closes the window at `valid_to`. Passing the
/// tombstone bit as false reuses the same half-open window without
/// letting a forget back into a dated read.
fn retraction_live_at(atom: &Record, at: &str) -> bool {
    if !record::is_tombstone(atom) {
        return false;
    }
    let Some(until) = atom
        .get("valid_to")
        .and_then(Value::as_str)
        .filter(|until| !until.is_empty())
    else {
        return false;
    };
    let from = atom
        .get("valid_from")
        .and_then(Value::as_str)
        .filter(|from| !from.is_empty())
        .or_else(|| {
            atom.get("ts")
                .and_then(Value::as_str)
                .filter(|ts| !ts.is_empty())
        });
    packset_core::atom::is_live_at(false, from, Some(until), at)
}

fn shape_bytes(shape: &record::Shape) -> usize {
    fn set_bytes(set: &BTreeSet<String>) -> usize {
        set.iter().fold(set.len().saturating_mul(40), |sum, text| {
            sum.saturating_add(text.len())
        })
    }
    set_bytes(&shape.tokens)
        .saturating_add(set_bytes(&shape.entities))
        .saturating_add(set_bytes(&shape.gate))
        .saturating_add(
            shape
                .head
                .iter()
                .fold(shape.head.len().saturating_mul(24), |sum, text| {
                    sum.saturating_add(text.len())
                }),
        )
        .saturating_add(32)
}

fn journal_cap_from_env() -> usize {
    match std::env::var("PACKSET_APPEND_LOG_MB") {
        Ok(raw) => match raw.trim() {
            "" => crate::journal::DEFAULT_CAP,
            n => n
                .parse::<usize>()
                .map(|mebibytes| mebibytes.saturating_mul(1024 * 1024).max(1))
                .unwrap_or(crate::journal::DEFAULT_CAP),
        },
        Err(_) => crate::journal::DEFAULT_CAP,
    }
}

impl Service {
    /// Open the pack at `home`.
    ///
    /// # Errors
    ///
    /// Fails when the store cannot be opened or the lock is held.
    pub fn open(home: Home) -> anyhow::Result<Self> {
        if let Some(line) =
            ignored_hold_notice(std::env::var("PACKSET_HOLD_AGENT_WRITES").ok().as_deref())
        {
            eprintln!("{line}");
        }
        let store = Store::open(home.root())?;
        let cap = journal_cap_from_env();
        let log_usage = Arc::new(crate::admit::LogUsage::new(cap));
        let mut journal = crate::journal::AppendLog::open(home.root().join("append.log"), cap)?;
        for bytes in journal.records()? {
            if let Ok(record) = serde_json::from_slice::<Record>(&bytes) {
                store.upsert(&record)?;
            }
        }
        journal.clear()?;
        log_usage.note(0);
        Ok(Self {
            home,
            store,
            journal: Mutex::new(journal),
            log_usage,
            attach: Mutex::new(BTreeMap::new()),
            writes: Mutex::new(()),
            live_cap: live_cap_from_env(),
            shapes: RwLock::new(HashMap::new()),
            shape_cap: AtomicUsize::new(shape_cap_from_env()),
            shape_clock: AtomicU64::new(1),
            shaped: Mutex::new(BTreeSet::new()),
            live_sets: Mutex::new(HashMap::new()),
            swept: Mutex::new(HashMap::new()),
            fired_recently: Mutex::new(HashMap::new()),
            cued_recently: Mutex::new(HashMap::new()),
            fire_cap: AtomicUsize::new(fire_cap_from_env()),
            projection: Arc::new(Mutex::new(Vec::new())),
            projecting: Arc::new(Mutex::new(())),
            flush_scheduled: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Queue changes for the search index and schedule one flush for them
    /// and whatever arrives in the next moment.
    fn queue_projection(&self, ops: Vec<ProjectionOp>) {
        if ops.is_empty() {
            return;
        }
        if let Ok(mut pending) = self.projection.lock() {
            pending.extend(ops);
        }
        // Reindex is maintenance. Under pressure the ops wait, and the
        // next quiet request flushes them.
        if crate::admit::under_pressure() || self.log_usage.heavy.load(Ordering::Relaxed) {
            return;
        }
        self.schedule_projection();
    }

    /// Start one flush of the queued projection, if one is not already
    /// running and there is something to write.
    fn schedule_projection(&self) {
        let pending_len = self.projection.lock().map(|p| p.len()).unwrap_or(0);
        if pending_len == 0 || self.flush_scheduled.swap(true, Ordering::AcqRel) {
            return;
        }
        let pending = Arc::clone(&self.projection);
        let projecting = Arc::clone(&self.projecting);
        let scheduled = Arc::clone(&self.flush_scheduled);
        let dir = self.home.milli_dir();
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let mut seen = 0usize;
            loop {
                std::thread::sleep(PROJECTION_DELAY);
                let len = pending.lock().map(|p| p.len()).unwrap_or(0);
                let quiet = len == seen;
                seen = len;
                if quiet || len >= PROJECTION_BATCH || started.elapsed() >= PROJECTION_MAX_WAIT {
                    break;
                }
            }
            scheduled.store(false, Ordering::Release);
            flush_queued(&pending, &projecting, &dir);
        });
    }

    /// Bring the search index level with every write so far, now. A read
    /// of the index calls this first, so a search sees its own writes.
    pub fn flush_projection(&self) {
        flush_queued(&self.projection, &self.projecting, &self.home.milli_dir());
    }

    /// Forgetting by neglect: a claim left due for longer than twice its
    /// interval and never graded is what a person would have forgotten. The
    /// sweep lapses it as a missed review does, halving its stability, and
    /// counts the neglect; a forgettable claim neglected [`NEGLECT_LIMIT`]
    /// times, never once recalled and never fired is tombstoned, marked
    /// `forgotten: neglect`. A habit, preference, goal, prediction or rule
    /// lapses but is never forgotten this way, and the sweep skips a trust
    /// row, persona or outcome. Returns how many lapsed and how many were
    /// forgotten.
    ///
    /// # Errors
    ///
    /// The store's.
    pub fn sweep(&self, workspace: &str) -> anyhow::Result<Value> {
        let _write = self.writes.lock().unwrap_or_else(|e| e.into_inner());
        let now = clock::utcnow();
        let live = self.store.live(workspace)?;
        let mut lapsed: Vec<Record> = Vec::new();
        let mut forgotten: Vec<Record> = Vec::new();
        for atom in live.iter() {
            if !record::is_due(atom, &now) {
                continue;
            }
            let kind = atom.get("kind").and_then(Value::as_str).unwrap_or("");
            if record::UNREVIEWED_KINDS.contains(&kind) {
                continue;
            }
            let due_at = atom.get("due_at").and_then(Value::as_str).unwrap_or(&now);
            let review = atom.get("review").and_then(Value::as_object);
            let interval_days = review
                .and_then(|r| r.get("interval_s"))
                .and_then(Value::as_f64)
                .map_or(1.0, |secs| (secs / 86_400.0).max(1.0));
            let overdue = clock::elapsed_days(due_at, &now);
            if overdue <= 2.0 * interval_days {
                continue;
            }
            let neglected = review
                .and_then(|r| r.get("neglected"))
                .and_then(Value::as_u64)
                .unwrap_or(0)
                + 1;
            let recalls = review.map_or(0, record::recalls_of);
            let used = review
                .and_then(|r| r.get("used"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let mut changed = Record::clone(atom);
            if neglected >= NEGLECT_LIMIT
                && recalls == 0
                && used == 0
                && FORGETTABLE_KINDS.contains(&kind)
                && changed.get("pinned").and_then(Value::as_bool) != Some(true)
            {
                record::forget(&mut changed, "neglect");
                forgotten.push(changed);
                continue;
            }
            record::schedule_review(&mut changed, &now, record::Grade::Lapsed, None);
            if let Some(Value::Object(r)) = changed.get_mut("review") {
                r.insert("neglected".into(), json!(neglected));
            }
            lapsed.push(changed);
        }
        let mut all = lapsed.clone();
        all.extend(forgotten.iter().cloned());
        if !all.is_empty() {
            let stored = self.store.upsert_many(&all)?;
            self.project_atoms(&stored);
            self.patch_live_sets(workspace, &stored);
        }
        let forgotten_ids: Vec<String> = forgotten
            .iter()
            .filter_map(|atom| atom.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();
        self.unshape(workspace, &forgotten_ids);
        if let Ok(mut swept) = self.swept.lock() {
            swept.insert(workspace.to_string(), now[..10].to_string());
        }
        Ok(json!({
            "lapsed": lapsed.len(),
            "forgotten": forgotten.len(),
            "forgotten_ids": forgotten.iter().filter_map(|a| a.get("id").cloned()).collect::<Vec<_>>(),
        }))
    }

    /// Sweep a workspace on the first write of a day; the lock is not held.
    fn sweep_if_due(&self, workspace: &str) {
        let today = clock::utcnow()[..10].to_string();
        let due = self
            .swept
            .lock()
            .map(|m| m.get(workspace) != Some(&today))
            .unwrap_or(false);
        if due {
            let _ = self.sweep(workspace);
        }
    }

    /// The shape of a stored claim, computed the first time it is asked for.
    fn shape_of(&self, atom: &Record) -> Arc<record::Shape> {
        let id = atom.get("id").and_then(Value::as_str).unwrap_or("");
        let workspace = atom.get("workspace").and_then(Value::as_str).unwrap_or("");
        if !id.is_empty() {
            if let Ok(index) = self.shapes.read() {
                if let Some(found) = index.get(workspace).and_then(|kept| kept.by_id.get(id)) {
                    found.used.store(
                        self.shape_clock.fetch_add(1, Ordering::Relaxed),
                        Ordering::Relaxed,
                    );
                    return Arc::clone(&found.shape);
                }
            }
        }
        let shape = Arc::new(record::Shape::of(atom));
        if !id.is_empty() {
            let fresh = self
                .shapes
                .write()
                .map(|mut index| {
                    let kept = index.entry(workspace.to_string()).or_default();
                    let inserted = kept
                        .by_id
                        .insert(
                            id.to_string(),
                            Shaped {
                                shape: Arc::clone(&shape),
                                used: AtomicU64::new(
                                    self.shape_clock.fetch_add(1, Ordering::Relaxed),
                                ),
                            },
                        )
                        .is_none();
                    if inserted {
                        for term in posting_terms(&shape) {
                            kept.postings.entry(term).or_default().push(id.to_string());
                        }
                    }
                    inserted
                })
                .unwrap_or(false);
            if fresh {
                self.evict_shapes(workspace);
            }
        }
        shape
    }

    /// Drop shapes whose claims have left this workspace's live set, and the
    /// postings that named them. A tombstone, a closed window and a forget
    /// all end here. Only this workspace's map is walked. The ids are the
    /// index's, so the check does not scan the records.
    fn reconcile_shapes(&self, workspace: &str) {
        let stale: Vec<String> = {
            let Ok(live) = self.live_sets.lock() else {
                return;
            };
            let Some(ids) = live.get(workspace).map(|kept| &kept.place) else {
                return;
            };
            let Ok(index) = self.shapes.read() else {
                return;
            };
            let Some(kept) = index.get(workspace) else {
                return;
            };
            kept.by_id
                .keys()
                .filter(|id| !ids.contains_key(id.as_str()))
                .cloned()
                .collect()
        };
        self.unshape(workspace, &stale);
    }

    /// Forget these ids in one workspace's shape map and its posting lists.
    fn unshape(&self, workspace: &str, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        let gone: HashSet<&str> = ids.iter().map(String::as_str).collect();
        let Ok(mut index) = self.shapes.write() else {
            return;
        };
        let Some(kept) = index.get_mut(workspace) else {
            return;
        };
        for id in ids {
            kept.by_id.remove(id);
        }
        kept.postings.retain(|_, held| {
            held.retain(|id| !gone.contains(id.as_str()));
            !held.is_empty()
        });
        if kept.by_id.is_empty() {
            index.remove(workspace);
        }
    }

    /// Keep the shape map inside its cap by dropping another workspace
    /// whole. That workspace is shaped again the next time it is written.
    /// When this workspace alone is over the cap its shapes stay: dropping
    /// them would rebuild the pack on the next write, and again on the one
    /// after that.
    fn evict_shapes(&self, protect_workspace: &str) {
        let cap = self.shape_cap.load(Ordering::Relaxed);
        if cap == 0 {
            return;
        }
        loop {
            let Ok(index) = self.shapes.read() else {
                return;
            };
            let total: usize = index.values().map(|kept| kept.by_id.len()).sum();
            if total <= cap {
                return;
            }
            let other = index
                .iter()
                .filter(|(name, _)| name.as_str() != protect_workspace)
                .min_by_key(|(_, kept)| {
                    kept.by_id
                        .values()
                        .map(|shape| shape.used.load(Ordering::Relaxed))
                        .max()
                        .unwrap_or(0)
                })
                .map(|(name, _)| name.clone());
            drop(index);
            let Some(workspace) = other else {
                return;
            };
            self.drop_workspace_shapes(&workspace);
        }
    }

    /// Drop atom caches the store has already evicted, then drop further
    /// cached structures while the snapshots, indexes, shapes and live
    /// sets together sit over the byte cap. The last workspace is not
    /// exempt: a structure that does not fit is dropped, and the next
    /// read builds it again only when it fits.
    pub(crate) fn maintain_cache(&self) {
        self.sync_pressure();
        for name in self.store.take_evicted() {
            self.drop_live_sets(&name);
            self.drop_workspace_shapes(&name);
        }
        let limit = self.store.cache_limit_bytes();
        if limit == 0 {
            return;
        }
        self.store.trim_to_cap();
        for _ in 0..64 {
            if self.cache_bytes_now() <= limit {
                break;
            }
            if self.store.cached_workspaces() > 1 {
                if let Some(name) = self.store.coldest_workspace() {
                    self.store.evict_workspace(&name);
                    self.drop_live_sets(&name);
                    self.drop_workspace_shapes(&name);
                    continue;
                }
            }
            if self.drop_one_shapes() {
                continue;
            }
            if self.drop_one_live() {
                continue;
            }
            let before = self.store.cache_resident_bytes();
            self.store.trim_to_cap();
            if self.store.cache_resident_bytes() < before {
                continue;
            }
            break;
        }
    }

    /// Fold the append log once pressure has cleared, and let a deferred
    /// reindex run. While pressure holds, the log stays so its cap is the
    /// bound on admitted appends.
    fn sync_pressure(&self) {
        let pressed =
            crate::admit::under_pressure() || self.log_usage.heavy.load(Ordering::Relaxed);
        if pressed {
            return;
        }
        if let Ok(mut log) = self.journal.lock() {
            if !log.is_empty() {
                let _ = log.clear();
                self.log_usage.note(0);
            }
        }
        self.schedule_projection();
    }

    /// The admission gate reads this, so a full log is refused before the
    /// request runs.
    #[must_use]
    pub fn log_usage(&self) -> Arc<crate::admit::LogUsage> {
        Arc::clone(&self.log_usage)
    }

    /// The cap, for a test that wants a smaller log than the process default.
    pub fn set_append_cap(&self, cap: usize) {
        if let Ok(mut log) = self.journal.lock() {
            log.set_cap(cap.max(1));
            self.log_usage.cap.store(cap.max(1), Ordering::Relaxed);
            self.log_usage.note(log.len());
        }
    }

    /// Drop one workspace's shapes, including when it is the only one.
    /// The shaped mark stays, so the next write shapes the new claim and
    /// does not rebuild the pack that did not fit.
    fn drop_one_shapes(&self) -> bool {
        let name = {
            let Ok(index) = self.shapes.read() else {
                return false;
            };
            index
                .iter()
                .max_by_key(|(_, kept)| kept.by_id.len())
                .map(|(name, _)| name.clone())
        };
        let Some(name) = name else {
            return false;
        };
        if let Ok(mut index) = self.shapes.write() {
            index.remove(&name);
        }
        true
    }

    /// Drop one workspace's live-set copy. The next write reads the store.
    fn drop_one_live(&self) -> bool {
        let name = {
            let Ok(live) = self.live_sets.lock() else {
                return false;
            };
            live.iter()
                .max_by_key(|(_, kept)| {
                    kept.sets.values().map(|set| set.order.len()).sum::<usize>()
                })
                .map(|(name, _)| name.clone())
        };
        let Some(name) = name else {
            return false;
        };
        self.drop_live_sets(&name);
        true
    }

    fn cache_bytes_now(&self) -> usize {
        self.store
            .cache_resident_bytes()
            .saturating_add(self.atom_cache_bytes())
    }

    /// Bytes of the snapshot, index and atom caches after a trim.
    #[must_use]
    pub fn resident_cache_bytes(&self) -> usize {
        self.maintain_cache();
        self.cache_bytes_now()
    }

    fn atom_cache_bytes(&self) -> usize {
        let live = match self.live_sets.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let atoms = live.values().fold(0usize, |sum, workspace| {
            let sets = workspace.sets.values().fold(0usize, |inner, set| {
                let records = set.order.iter().fold(0usize, |bytes, record| {
                    bytes.saturating_add(crate::store::record_bytes(record))
                });
                let index = set
                    .at
                    .keys()
                    .fold(set.at.len().saturating_mul(32), |bytes, id| {
                        bytes.saturating_add(id.len())
                    });
                inner.saturating_add(records).saturating_add(index)
            });
            let place = workspace.place.iter().fold(
                workspace.place.len().saturating_mul(48),
                |bytes, (id, _)| bytes.saturating_add(id.len()),
            );
            sum.saturating_add(sets).saturating_add(place)
        });
        drop(live);
        let shapes = match self.shapes.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let shaped = shapes.values().fold(0usize, |sum, kept| {
            let posts = kept.postings.iter().fold(0usize, |inner, (key, ids)| {
                inner
                    .saturating_add(key.len().saturating_add(64))
                    .saturating_add(ids.len().saturating_mul(48))
                    .saturating_add(ids.iter().fold(0usize, |n, id| n.saturating_add(id.len())))
            });
            let bodies = kept.by_id.iter().fold(0usize, |inner, (id, shaped)| {
                inner
                    .saturating_add(id.len().saturating_add(64))
                    .saturating_add(shape_bytes(&shaped.shape))
            });
            sum.saturating_add(bodies).saturating_add(posts)
        });
        atoms.saturating_add(shaped)
    }

    /// Drop every shape filed under `workspace` and mark it unshaped, so
    /// the next write there rebuilds the postings from the live set.
    fn drop_workspace_shapes(&self, workspace: &str) {
        if let Ok(mut index) = self.shapes.write() {
            index.remove(workspace);
        }
        if let Ok(mut shaped) = self.shaped.lock() {
            shaped.remove(workspace);
        }
    }

    /// The cap, for a test that wants a smaller one than the process default.
    pub fn set_shape_cap(&self, cap: usize) {
        self.shape_cap.store(cap, Ordering::Relaxed);
    }

    /// `true` when `key` already fired inside the window. Otherwise it is
    /// recorded, expired keys are dropped, and the oldest keys past the cap
    /// go too.
    fn fire_held(&self, key: String) -> bool {
        let Ok(mut recent) = self.fired_recently.lock() else {
            return false;
        };
        hold_key(&mut recent, key, self.fire_cap.load(Ordering::Relaxed))
    }

    /// The cue map, capped on its own so a burst of titles cannot push a
    /// fire out of the hour.
    fn cue_held(&self, key: String) -> bool {
        let Ok(mut recent) = self.cued_recently.lock() else {
            return false;
        };
        hold_key(&mut recent, key, self.fire_cap.load(Ordering::Relaxed))
    }

    /// The cap, for a test that wants a smaller one than the process default.
    pub fn set_fire_cap(&self, cap: usize) {
        self.fire_cap.store(cap, Ordering::Relaxed);
    }

    /// How many fire keys are held right now.
    #[cfg(test)]
    fn fired_len(&self) -> usize {
        self.fired_recently.lock().map(|map| map.len()).unwrap_or(0)
    }

    /// How many cue keys are held right now.
    #[cfg(test)]
    fn cued_len(&self) -> usize {
        self.cued_recently.lock().map(|map| map.len()).unwrap_or(0)
    }

    /// Record a cue the way activate does, without building an island.
    #[cfg(test)]
    fn hold_cue(&self, workspace: &str, query: &str) -> bool {
        self.cue_held(format!(
            "cue\u{0}{workspace}\u{0}\u{0}{}",
            query.trim().to_lowercase()
        ))
    }

    /// How many shapes are kept right now.
    #[cfg(test)]
    fn shape_count(&self) -> usize {
        self.shapes
            .read()
            .map(|index| index.values().map(|kept| kept.by_id.len()).sum())
            .unwrap_or(0)
    }

    /// Shape every live claim of a workspace the first time this process
    /// writes to it, so the postings cover the pack; afterwards each write
    /// shapes what it wrote. The claims come from the set index, which is
    /// the workspace, not one set of it.
    fn shape_workspace_once(&self, workspace: &str) {
        let first = self
            .shaped
            .lock()
            .map(|mut set| set.insert(workspace.to_string()))
            .unwrap_or(false);
        if !first {
            return;
        }
        let records: Vec<Arc<Record>> = {
            let Ok(live) = self.live_sets.lock() else {
                return;
            };
            let Some(kept) = live.get(workspace) else {
                return;
            };
            kept.sets
                .values()
                .flat_map(|members| members.order.iter().cloned())
                .collect()
        };
        for existing in &records {
            self.shape_of(existing);
        }
    }

    /// The live claims a new claim could replace or duplicate, in id order.
    /// The overlap rule needs a token jaccard of 0.6, so a candidate carries
    /// at least six tenths of the new claim's tokens, counted over the
    /// postings; the head rule's candidates share its first head word, the
    /// correction rule's an entity, and `supersedes` names the rest. Only
    /// this set's claims are returned.
    fn replace_candidates(
        &self,
        workspace: &str,
        group: &SetGroup,
        atom: &Record,
        shape: &record::Shape,
    ) -> Vec<Arc<Record>> {
        let mut wanted: BTreeSet<String> = BTreeSet::new();
        if let Ok(index) = self.shapes.read() {
            if let Some(kept) = index.get(workspace) {
                let postings = &kept.postings;
                // A claim that carries `need` of the tokens misses at most
                // `tokens - need` of them, so it sits in at least one of the
                // shortest `tokens - need + 1` posting lists. Those lists are the
                // candidates; the long lists of the common words are not walked,
                // and the rule itself decides each candidate.
                let need = ((shape.tokens.len() as f64) * 0.6).ceil().max(1.0) as usize;
                let mut lists: Vec<&Vec<String>> = shape
                    .tokens
                    .iter()
                    .filter_map(|token| postings.get(token))
                    .collect();
                lists.sort_by_key(|ids| ids.len());
                let take = shape.tokens.len().saturating_sub(need) + 1;
                for ids in lists.iter().take(take) {
                    for id in ids.iter() {
                        wanted.insert(id.clone());
                    }
                }
                let mut exact: Vec<String> =
                    shape.entities.iter().map(|e| format!("e:{e}")).collect();
                if let Some(head) = head_key(shape) {
                    exact.push(head);
                }
                for term in exact {
                    if let Some(ids) = postings.get(&term) {
                        for id in ids {
                            wanted.insert(id.clone());
                        }
                    }
                }
            }
        }
        match atom.get("supersedes") {
            Some(Value::Array(ids)) => {
                for id in ids {
                    wanted.insert(record::value_text(id));
                }
            }
            Some(Value::String(id)) => {
                wanted.insert(id.clone());
            }
            _ => {}
        }
        self.records_in(workspace, group, &wanted)
    }

    /// Build the set index from the snapshot the first time this process
    /// writes the workspace. Later writes patch it.
    fn ensure_live_sets(&self, workspace: &str) -> anyhow::Result<()> {
        if self
            .live_sets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(workspace)
        {
            return Ok(());
        }
        let snapshot = self.store.live(workspace)?;
        let mut built = WorkspaceLive::default();
        for atom in snapshot.iter() {
            built.insert_shown(atom.as_ref());
        }
        self.live_sets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(workspace.to_string(), built);
        Ok(())
    }

    /// Drop indexed claims whose window has closed since they were written.
    ///
    /// The set index is patched, not rebuilt, so a claim stays until a
    /// later write of that same claim. This runs at the start of a write,
    /// before the live length is read.
    fn prune_expired(&self, workspace: &str) {
        let mut index = self.live_sets.lock().unwrap_or_else(|e| e.into_inner());
        let Some(kept) = index.get_mut(workspace) else {
            return;
        };
        kept.prune_expired(&clock::utcnow());
    }

    /// Fold committed records into the set index. A workspace that has not
    /// been indexed yet is left alone: the next write reads the store.
    fn patch_live_sets(&self, workspace: &str, written: &[Record]) {
        let Ok(mut index) = self.live_sets.lock() else {
            return;
        };
        let Some(kept) = index.get_mut(workspace) else {
            return;
        };
        let now = clock::utcnow();
        let mut removed = false;
        let mut touched: Vec<String> = Vec::new();
        for record in written {
            let id = record.get("id").and_then(Value::as_str).unwrap_or("");
            let was = kept.place.contains_key(id);
            kept.upsert_written(record.clone(), &now);
            if kept.place.contains_key(id) {
                touched.push(id.to_string());
            } else if was {
                removed = true;
            }
        }
        // Links are cut after the whole batch is in the set, so a claim
        // written beside the one that names it is still live when the cut runs.
        if removed {
            kept.cut_every_link();
        } else {
            let mut edits: Vec<(SetGroup, usize, Record)> = Vec::new();
            for id in &touched {
                let Some(group) = kept.place.get(id).cloned() else {
                    continue;
                };
                let Some(&i) = kept.sets.get(&group).and_then(|members| members.at.get(id)) else {
                    continue;
                };
                let Some(record) = kept
                    .sets
                    .get(&group)
                    .and_then(|members| members.order.get(i))
                else {
                    continue;
                };
                let mut copy = Record::clone(record);
                let before = copy.get("links").cloned();
                let place = &kept.place;
                WorkspaceLive::cut_links_in(&mut copy, |link| place.contains_key(link));
                if copy.get("links") != before.as_ref() {
                    edits.push((group, i, copy));
                }
            }
            for (group, i, copy) in edits {
                if let Some(members) = kept.sets.get_mut(&group) {
                    members.order[i] = Arc::new(copy);
                }
            }
        }
    }

    /// Drop the set index for a workspace that was forgotten.
    fn drop_live_sets(&self, workspace: &str) {
        if let Ok(mut index) = self.live_sets.lock() {
            index.remove(workspace);
        }
    }

    /// How many live claims the index holds, or the snapshot's length when
    /// this workspace has not been indexed.
    fn indexed_len(&self, workspace: &str) -> anyhow::Result<usize> {
        if let Ok(index) = self.live_sets.lock() {
            if let Some(kept) = index.get(workspace) {
                return Ok(kept.place.len());
            }
        }
        Ok(self.store.live(workspace)?.len())
    }

    /// The live claims, from the index when this workspace has one.
    fn indexed_records(&self, workspace: &str) -> anyhow::Result<Vec<Arc<Record>>> {
        if let Ok(index) = self.live_sets.lock() {
            if let Some(kept) = index.get(workspace) {
                return Ok(kept
                    .sets
                    .values()
                    .flat_map(|members| members.order.iter().cloned())
                    .collect());
            }
        }
        let live = self.store.live(workspace)?;
        Ok(live.iter().cloned().collect())
    }

    /// `ids` that are live in `group`, in id order.
    fn records_in(
        &self,
        workspace: &str,
        group: &SetGroup,
        ids: &BTreeSet<String>,
    ) -> Vec<Arc<Record>> {
        let Ok(index) = self.live_sets.lock() else {
            return Vec::new();
        };
        let Some(members) = index.get(workspace).and_then(|kept| kept.sets.get(group)) else {
            return Vec::new();
        };
        ids.iter()
            .filter_map(|id| members.at.get(id).map(|&i| Arc::clone(&members.order[i])))
            .collect()
    }

    /// The first claim in the set that says what `atom` says. The order is
    /// the snapshot's, so a claim with no tokens still meets the same one.
    fn first_same(&self, workspace: &str, group: &SetGroup, atom: &Record) -> Option<Arc<Record>> {
        let index = self.live_sets.lock().unwrap_or_else(|e| e.into_inner());
        let members = index.get(workspace)?.sets.get(group)?;
        members
            .order
            .iter()
            .find(|existing| same_claim(existing, atom))
            .cloned()
    }

    /// The same service with another live cap; zero is none.
    #[must_use]
    pub fn with_live_cap(mut self, cap: usize) -> Self {
        self.live_cap = cap;
        self
    }

    /// Most live claims a workspace holds before the least retrievable are
    /// forgotten; zero is no cap.
    #[must_use]
    pub fn live_cap(&self) -> usize {
        self.live_cap
    }

    /// Hold a workspace at the live cap: tombstone the least retrievable
    /// forgettable claims until it fits. Retrievability is the review
    /// model's, from the last review or the write and the claim's
    /// stability, so a lesson recalled last week outlives one written a
    /// month ago and never asked for. Returns how many were forgotten.
    fn enforce_cap(&self, workspace: &str, now: &str) -> anyhow::Result<usize> {
        if self.live_cap == 0 {
            return Ok(0);
        }
        let len = self.indexed_len(workspace)?;
        let over = len.saturating_sub(self.live_cap);
        if over == 0 {
            return Ok(0);
        }
        let live = self.indexed_records(workspace)?;
        let mut ranked: Vec<(f64, String, Record)> = live
            .iter()
            .filter(|a| {
                FORGETTABLE_KINDS.contains(&a.get("kind").and_then(Value::as_str).unwrap_or(""))
                    && a.get("pinned").and_then(Value::as_bool) != Some(true)
            })
            .map(|a| {
                let review = a.get("review");
                let last = review
                    .and_then(|r| r.get("last"))
                    .and_then(Value::as_str)
                    .or_else(|| a.get("ts").and_then(Value::as_str))
                    .unwrap_or(now);
                let stability = review
                    .and_then(|r| r.get("stability"))
                    .and_then(Value::as_f64)
                    .unwrap_or(record::DEFAULT_STABILITY);
                let r =
                    packset_core::decay::retrievability(clock::elapsed_days(last, now), stability);
                (r, last.to_string(), Record::clone(a))
            })
            .collect();
        ranked.sort_by(|a, b| {
            a.0.partial_cmp(&b.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.cmp(&b.1))
        });
        let tombs: Vec<Record> = ranked
            .into_iter()
            .take(over)
            .map(|(_, _, mut a)| {
                record::forget(&mut a, "the live cap");
                a
            })
            .collect();
        if tombs.is_empty() {
            return Ok(0);
        }
        let tombs = self.store.upsert_many(&tombs)?;
        self.project_atoms(&tombs);
        self.patch_live_sets(workspace, &tombs);
        let forgotten: Vec<String> = tombs
            .iter()
            .filter_map(|atom| atom.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();
        self.unshape(workspace, &forgotten);
        Ok(tombs.len())
    }

    /// The pack home.
    #[must_use]
    pub fn home(&self) -> &Home {
        &self.home
    }

    /// The atom store.
    #[must_use]
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Store one atom, or return the live one with the same text, kind and set.
    ///
    /// # Errors
    ///
    /// [`AtomError`] when the record does not validate, else the store's.
    /// Sync `atom` into the append log. [`crate::journal::Full`] when the
    /// cap would be passed, which the HTTP layer answers as 503.
    fn journal_append(&self, atom: &Record) -> anyhow::Result<usize> {
        let bytes = serde_json::to_vec(atom).unwrap_or_default();
        let mut log = self.journal.lock().unwrap_or_else(|err| err.into_inner());
        let prior = log.len();
        match log.append(&bytes) {
            Ok(()) => {
                self.log_usage.note(log.len());
                Ok(prior)
            }
            Err(crate::journal::AppendFailure::Full(full)) => Err(anyhow::Error::new(full)),
            Err(crate::journal::AppendFailure::Io(err)) => Err(anyhow::Error::new(err)),
        }
    }

    fn journal_rewind(&self, prior: usize) {
        if let Ok(mut log) = self.journal.lock() {
            if log.rewind(prior).is_ok() {
                self.log_usage.note(log.len());
            }
        }
    }

    pub fn add(&self, atom: Record) -> anyhow::Result<Record> {
        // Validation and the encoder run before the lock: the encode is the
        // slow part of a write and depends on the text alone.
        self.maintain_cache();
        let started = std::time::Instant::now();
        let atom = self.prepare(atom)?;
        let prepared = started.elapsed();
        let journaled = if crate::admit::under_pressure() {
            Some(self.journal_append(&atom)?)
        } else {
            None
        };
        if journaled.is_none() {
            if let Some(ws) = atom.get("workspace").and_then(Value::as_str) {
                self.sweep_if_due(ws);
            }
        }
        let _write = self.writes.lock().unwrap_or_else(|e| e.into_inner());
        let mut trace = WriteTrace::begin();
        trace.add("prepare", prepared);
        let written = self.add_prepared_traced(atom, trace, false);
        if let (Some(prior), Err(_)) = (journaled, &written) {
            self.journal_rewind(prior);
        }
        self.store.release_after_write();
        written
    }

    /// Check a claim and fill the fields that depend on nothing stored.
    fn prepare(&self, mut atom: Record) -> anyhow::Result<Record> {
        record::validate(&mut atom).map_err(anyhow::Error::new)?;
        atom.remove(record::PROVENANCE_FLOOR);

        if atom
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .is_empty()
        {
            atom.insert("id".into(), Value::String(new_id()));
        }
        if atom
            .get("ts")
            .and_then(Value::as_str)
            .unwrap_or("")
            .is_empty()
        {
            atom.insert("ts".into(), Value::String(clock::utcnow()));
        }
        if atom
            .get("valid_from")
            .and_then(Value::as_str)
            .unwrap_or("")
            .is_empty()
        {
            let from = atom
                .get("ts")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("")
                .to_string();
            atom.insert("valid_from".into(), Value::String(from));
        }
        atom.insert("tombstone".into(), Value::Bool(false));
        atom.entry("embedding").or_insert(Value::Null);
        self.encode_into(&mut atom);
        Ok(atom)
    }

    /// Store an accepted claim, or return the live one that already says it.
    /// This is the one write that may raise the provenance of a record it
    /// replaces.
    fn add_prepared(&self, atom: Record) -> anyhow::Result<Record> {
        self.add_prepared_traced(atom, WriteTrace::begin(), true)
    }

    /// A post whose id a tombstone in this workspace still holds is stored
    /// under a fresh id. The tombstone keeps the old one, so a deleted atom
    /// does not come back under its old id. Returns the live record a post
    /// would write over, or none when the id is free or was just retired.
    ///
    /// The key is workspace and id together, so the same id in another
    /// workspace is a different atom. A forget keeps the tombstones, so a
    /// forgotten workspace's other ids may be posted again and these not.
    fn refuse_retired(&self, atom: &mut Record) -> anyhow::Result<Option<Record>> {
        let workspace = atom
            .get("workspace")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let id = atom
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if workspace.is_empty() || id.is_empty() {
            return Ok(None);
        }
        let Some(existing) = self.store.get(&workspace, &id)? else {
            return Ok(None);
        };
        if existing
            .get("tombstone")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            // The line a client prints. It names the id that stayed a
            // tombstone; the record's own id is the fresh one. validate
            // may already have written the line for an unknown origin or
            // text past the soft cap. That line stays, and this one follows
            // it.
            atom.insert("id".into(), Value::String(new_id()));
            let retired = format!(
                "{id} is retired; stored under a new id. A deleted atom does not come back under its old id"
            );
            let line = match atom.get("warning").and_then(Value::as_str) {
                Some(prev) if !prev.is_empty() => format!("{prev} {retired}"),
                _ => retired,
            };
            atom.insert("warning".into(), Value::String(line));
            return Ok(None);
        }
        Ok(Some(existing))
    }

    /// [`Self::add_prepared`] with the stage clock a write's trace reads.
    fn add_prepared_traced(
        &self,
        mut atom: Record,
        mut trace: WriteTrace,
        accepted: bool,
    ) -> anyhow::Result<Record> {
        // Before a proposal is filed, and before a duplicate is answered:
        // a retired id is moved aside even when the text matches what was
        // deleted, so this post does not write over the tombstone.
        let over = self.refuse_retired(&mut atom)?;
        let workspace = atom
            .get("workspace")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        // The set index is filled from the snapshot once. After that a write
        // reads its own set, not every live claim in the workspace.
        // Membership was decided at insert. Drop claims the snapshot would
        // no longer show before the length is read, or an expired claim
        // counts toward the cap and answers a duplicate of the same words.
        self.ensure_live_sets(&workspace)?;
        self.prune_expired(&workspace);
        let snapshot_len = self.indexed_len(&workspace)?;
        trace.mark("snapshot");
        // Dedupe and replacement stay inside the set the new claim names.
        // Shape maintenance does not: a write in one set used to hand
        // reconcile only that set's ids, and every other set's tokens were
        // dropped and never rebuilt.
        let group = WorkspaceLive::query_group(&atom);
        let shape = record::Shape::of(&atom);
        self.reconcile_shapes(&workspace);
        self.shape_workspace_once(&workspace);
        let candidates = self.replace_candidates(&workspace, &group, &atom, &shape);
        // A duplicate shares every token, so it is among the candidates; a
        // claim with no tokens is compared against the whole set.
        let same_text = if shape.tokens.is_empty() {
            self.first_same(&workspace, &group, &atom)
        } else {
            candidates
                .iter()
                .find(|existing| same_claim(existing, &atom))
                .cloned()
        };
        if let Some(existing) = same_text {
            return Ok(Record::clone(&existing));
        }
        let provenance_warning = if let Some(stored) = over.as_ref().filter(|_| !accepted) {
            record::hold_provenance(stored, &mut atom, "a post over an existing id")
        } else {
            None
        };

        trace.mark("dedup");
        let now = clock::utcnow();
        let mut closed: Vec<String> = Vec::new();
        let mut batch = Vec::new();
        if record::is_live(&atom, &now) {
            for existing in &candidates {
                let theirs = self.shape_of(existing);
                if record::replaces_shaped(&atom, &shape, existing, &theirs) {
                    let mut peer = Record::clone(existing);
                    record::close_valid_to(&mut peer, &now);
                    if let Some(id) = peer.get("id").and_then(Value::as_str) {
                        closed.push(id.to_string());
                    }
                    batch.push(peer);
                }
            }
            if !closed.is_empty() {
                let supersedes = atom
                    .entry("supersedes")
                    .or_insert_with(|| Value::Array(Vec::new()));
                if let Value::Array(ids) = supersedes {
                    for id in &closed {
                        if !ids.iter().any(|v| v.as_str() == Some(id)) {
                            ids.push(Value::String(id.clone()));
                        }
                    }
                }
            }
        }
        trace.mark("replace");
        let mail = atom
            .get("kind")
            .and_then(Value::as_str)
            .is_some_and(record::is_mail);
        if record::is_live(&atom, &now) && !mail {
            // Closed peers stay out of apply_links: a rewrite of links would
            // otherwise write them back without valid_to. Only a peer that
            // shares a name can be linked, and only its links can be
            // re-selected when it fills; the rest of the pack is not read.
            let narrowed = self.link_peers(&workspace, &group, &shape, &closed);
            let narrowed_refs: Vec<&Record> = narrowed.iter().map(AsRef::as_ref).collect();
            let rewritten =
                record::apply_links_among(&mut atom, &narrowed_refs, record::LINK_THRESHOLD, &now);
            batch.extend(rewritten);
        } else if !atom.contains_key("links") {
            atom.insert("links".into(), Value::Array(Vec::new()));
        }
        // A new claim enters the review clock at once; a trust row is not
        // recalled, it is weighed.
        if record::is_live(&atom, &now)
            && !record::UNREVIEWED_KINDS
                .contains(&atom.get("kind").and_then(Value::as_str).unwrap_or(""))
            && atom
                .get("due_at")
                .and_then(Value::as_str)
                .unwrap_or("")
                .is_empty()
        {
            record::schedule_review(&mut atom, &now, record::Grade::Initial, None);
        }
        trace.mark("links");
        let mut all = vec![atom.clone()];
        all.append(&mut batch);
        let all = self.store.upsert_many(&all)?;
        self.patch_live_sets(&workspace, &all);
        if let Some(stored) = all.first() {
            atom = stored.clone();
        }
        self.shape_of(&atom);
        // Closed peers are no longer live. Dropping their tokens here keeps
        // the next write from waiting on a reconcile to notice.
        if !closed.is_empty() {
            self.unshape(&workspace, &closed);
        }
        trace.mark("upsert");
        self.project_atoms(&all);
        trace.mark("project");
        let forgot = self.enforce_cap(&workspace, &now)?;
        trace.mark("cap");
        if forgot > 0 {
            atom.insert("forgot".into(), json!(forgot));
        }
        // The provenance line is the answer's. A warning already on the
        // record, such as an unknown origin, stays stored and shares the line.
        with_provenance_warning(&mut atom, provenance_warning);
        trace.finish(
            atom.get("id").and_then(Value::as_str).unwrap_or(""),
            snapshot_len,
        );
        Ok(atom)
    }

    /// The peers a new claim can link to, with the claims those peers
    /// already link to: the claims sharing an entity with it, closed under
    /// their links, so re-selection sees every candidate it would have seen
    /// over the whole pack.
    fn link_peers(
        &self,
        workspace: &str,
        group: &SetGroup,
        shape: &record::Shape,
        closed: &[String],
    ) -> Vec<Arc<Record>> {
        if shape.entities.is_empty() {
            return Vec::new();
        }
        // The peers sharing an entity come from the postings; a closed peer
        // is passed over. Ids outside this set are not candidates.
        let mut seeds: BTreeSet<String> = BTreeSet::new();
        if let Ok(index) = self.shapes.read() {
            let Some(kept) = index.get(workspace) else {
                return Vec::new();
            };
            let postings = &kept.postings;
            for entity in &shape.entities {
                if let Some(ids) = postings.get(&format!("e:{entity}")) {
                    for id in ids {
                        if closed.iter().any(|c| c == id) {
                            continue;
                        }
                        seeds.insert(id.clone());
                    }
                }
            }
        }
        let Ok(index) = self.live_sets.lock() else {
            return Vec::new();
        };
        let Some(members) = index.get(workspace).and_then(|kept| kept.sets.get(group)) else {
            return Vec::new();
        };
        if members.order.is_empty() {
            return Vec::new();
        }
        let mut wanted: BTreeSet<String> = BTreeSet::new();
        let mut linked: Vec<String> = Vec::new();
        for id in &seeds {
            if let Some(&i) = members.at.get(id) {
                wanted.insert(id.clone());
                linked.extend(record::links_of(&members.order[i]));
            }
        }
        for id in &linked {
            if closed.iter().any(|c| c == id) {
                continue;
            }
            if members.at.contains_key(id) {
                wanted.insert(id.clone());
            }
        }
        wanted
            .into_iter()
            .filter_map(|id| members.at.get(&id).map(|&i| Arc::clone(&members.order[i])))
            .collect()
    }

    /// Fill the vector slot when this seat has an encoder; null otherwise.
    fn encode_into(&self, atom: &mut Record) {
        // Embed backfill is maintenance. A small append under pressure is
        // stored without a vector; search ranks it by words until a quiet
        // write fills the vector.
        if crate::admit::under_pressure() || self.log_usage.heavy.load(Ordering::Relaxed) {
            return;
        }
        let text = atom.get("text").and_then(Value::as_str).unwrap_or_default();
        let Some(vector) = crate::embed::encode_document(text) else {
            return;
        };
        atom.insert(
            "embedding".into(),
            Value::Array(vector.into_iter().map(|f| json!(f)).collect()),
        );
    }

    /// Merge `fields` into one current atom.
    ///
    /// # Errors
    ///
    /// [`AtomError`] when the id is not current or the result does not
    /// validate, else the store's.
    pub fn update(
        &self,
        workspace: &str,
        id: &str,
        fields: &Map<String, Value>,
    ) -> anyhow::Result<Record> {
        let _write = self.writes.lock().unwrap_or_else(|e| e.into_inner());
        self.update_unlocked(workspace, id, fields)
    }

    fn update_unlocked(
        &self,
        workspace: &str,
        id: &str,
        fields: &Map<String, Value>,
    ) -> anyhow::Result<Record> {
        let current = self.store.live(workspace)?;
        let stored = current
            .iter()
            .find(|a| a.get("id").and_then(Value::as_str) == Some(id))
            .ok_or_else(|| anyhow::Error::new(AtomError(format!("no current atom {id}"))))?;
        // Null and "" are what validate defaults to user-declared, which
        // is a rise from any other origin.
        if fields
            .get("origin")
            .is_some_and(|raw| origin_is_blank(Some(raw)))
        {
            return Err(anyhow::Error::new(AtomError(
                "unknown atom origin: None".into(),
            )));
        }
        // An update that does not name origin must not gain the default.
        // Validate writes user-declared for a missing field, which is right
        // for a new post and wrong for a record that never had one.
        let blank_origin =
            if fields.contains_key("origin") || !origin_is_blank(stored.get("origin")) {
                None
            } else {
                Some(stored.get("origin").cloned())
            };
        let text_changed = fields
            .get("text")
            .is_some_and(|next| stored.get("text") != Some(next));
        let mut updated = Record::clone(stored);
        for (key, value) in fields {
            updated.insert(key.clone(), value.clone());
        }
        // The cached record has no embedding key. A new sentence without a
        // new vector would otherwise keep the old one, and dense search
        // would score the new text with the old meaning.
        if text_changed && !fields.contains_key("embedding") {
            updated.insert("embedding".into(), Value::Null);
        }
        updated.insert("id".into(), Value::String(id.to_string()));
        updated.insert("workspace".into(), Value::String(workspace.to_string()));
        updated.insert("tombstone".into(), Value::Bool(false));
        record::validate(&mut updated).map_err(anyhow::Error::new)?;
        if let Some(previous) = blank_origin {
            match previous {
                None => {
                    updated.remove("origin");
                }
                Some(value) => {
                    updated.insert("origin".into(), value);
                }
            }
        }
        let provenance_warning = record::hold_provenance(stored, &mut updated, "an update");

        let now = clock::utcnow();
        let mut batch = Vec::new();
        if record::is_live(&updated, &now) {
            let peers: Vec<&Record> = current.iter().map(AsRef::as_ref).collect();
            let rewritten =
                record::apply_links_among(&mut updated, &peers, record::LINK_THRESHOLD, &now);
            batch.extend(rewritten);
        } else if !updated.contains_key("links") {
            updated.insert("links".into(), Value::Array(Vec::new()));
        }
        let mut all = vec![updated.clone()];
        all.append(&mut batch);
        let all = self.store.upsert_many(&all)?;
        self.project_atoms(&all);
        self.patch_live_sets(workspace, &all);
        let mut written = all.into_iter().next().unwrap_or(updated);
        // The provenance line is the answer's. A warning already on the
        // record, such as an unknown origin, stays stored and shares the line.
        with_provenance_warning(&mut written, provenance_warning);
        Ok(written)
    }

    /// Move one atom along the review clock.
    ///
    /// # Errors
    ///
    /// As [`Service::update`].
    pub fn grade(&self, workspace: &str, id: &str, recalled: bool) -> anyhow::Result<Record> {
        let _write = self.writes.lock().unwrap_or_else(|e| e.into_inner());
        let mut atom = self
            .store
            .live(workspace)?
            .iter()
            .find(|a| a.get("id").and_then(Value::as_str) == Some(id))
            .map(|a| Record::clone(a))
            .ok_or_else(|| anyhow::Error::new(AtomError(format!("no current atom {id}"))))?;
        let grade = if recalled {
            record::Grade::Recalled
        } else {
            record::Grade::Lapsed
        };
        record::schedule_review(&mut atom, &clock::utcnow(), grade, None);
        let mut fields = Map::new();
        fields.insert("due_at".into(), atom["due_at"].clone());
        fields.insert("review".into(), atom["review"].clone());
        // A recalled review is rehearsal. The episode becomes a standing
        // rule. A lapse is not rehearsal and leaves the horizon alone.
        if recalled {
            fields.insert("entities".into(), promoted_entities(&atom));
        }
        self.update_unlocked(workspace, id, &fields)
    }

    /// Tombstone one atom and drop it from the projection.
    ///
    /// `why` names the deed that withdrew the claim when it is a deed
    /// accession, and that accession is `retracted_by`, as before. Plain text
    /// is the seat's reason. It is kept on the tombstone as `why_text`, with
    /// one warning line, so the retraction is stored either way. A sentence
    /// does not go in `retracted_by`: that field is a citation, and a name no
    /// deed store answers for would only look like one.
    ///
    /// # Errors
    ///
    /// The store's, including when the id is not current.
    pub fn delete_atom(
        &self,
        workspace: &str,
        id: &str,
        why: Option<&str>,
    ) -> anyhow::Result<Record> {
        let _write = self.writes.lock().unwrap_or_else(|e| e.into_inner());
        let why = why.map(str::trim).filter(|w| !w.is_empty());
        let (cited, plain) = match why {
            Some(w) if packset_core::atom::is_accession(w) => (Some(w), None),
            other => (None, other),
        };
        let mut tomb = self.store.delete(workspace, id, cited)?;
        if let Some(text) = plain {
            // `delete` writes `why` only into `retracted_by`. The plain
            // reason is a second write of the tombstone just stored, which
            // is the whole migration: the record is the schema.
            tomb.insert("why_text".into(), Value::String(text.to_string()));
            tomb.insert(
                "warning".into(),
                Value::String(format!(
                    "{text} is not a deed accession; kept as why_text. A retraction cites deed-<kind>-<slug> or sha256:<hash>"
                )),
            );
            let mut stored = self.store.upsert_many(std::slice::from_ref(&tomb))?;
            tomb = stored.pop().unwrap_or(tomb);
        }
        self.queue_projection(vec![ProjectionOp::Delete(id.to_string())]);
        self.patch_live_sets(workspace, std::slice::from_ref(&tomb));
        self.unshape(workspace, &[id.to_string()]);
        Ok(tomb)
    }

    /// Drop a scratch workspace's atoms whole: no deed, the keys are gone,
    /// and a tombstone stays so its id is still retired. Retraction stays
    /// `delete_atom`, which names the deed that withdrew the claim; this is
    /// what a per-run scratch workspace calls on its way out, so the
    /// long-lived writer does not keep every smoke and herd run's atoms.
    /// Returns the workspace and how many records were removed.
    ///
    /// # Errors
    ///
    /// Fails when the workspace is empty, else the store's.
    pub fn forget_workspace(&self, workspace: &str) -> anyhow::Result<Value> {
        if workspace.trim().is_empty() {
            anyhow::bail!("forget needs a workspace");
        }
        let _write = self.writes.lock().unwrap_or_else(|e| e.into_inner());
        let ids: Vec<String> = self
            .store
            .scan(Some(workspace))?
            .iter()
            .filter_map(|atom| atom.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();
        let forgotten = self.store.forget_workspace(workspace)?;
        self.queue_projection(ids.iter().cloned().map(ProjectionOp::Delete).collect());
        self.drop_workspace_shapes(workspace);
        self.drop_live_sets(workspace);
        if let Ok(mut swept) = self.swept.lock() {
            swept.remove(workspace);
        }
        if let Ok(mut held) = self.attach.lock() {
            held.remove(workspace);
        }
        if let Ok(mut recent) = self.fired_recently.lock() {
            let direct = format!("{workspace}\0");
            recent.retain(|key, _| !key.starts_with(&direct));
        }
        if let Ok(mut recent) = self.cued_recently.lock() {
            let cue = format!("cue\0{workspace}\0");
            recent.retain(|key, _| !key.starts_with(&cue));
        }
        Ok(json!({"workspace": workspace, "forgotten": forgotten}))
    }

    /// The workspace pack, or the same shape scoped to one set.
    ///
    /// Always `user` / `memory` / `atoms`, so a client splices one shape
    /// whether or not a set is pinned.
    ///
    /// # Errors
    ///
    /// [`AtomError`] for a bad set name, else the store's.
    pub fn pack(&self, workspace: &str, set: Option<&str>) -> anyhow::Result<Value> {
        let current = |set: Option<&str>| -> anyhow::Result<Vec<Record>> {
            let snap = self.store.live_versioned(workspace)?;
            let mut atoms: Vec<Record> = snap
                .atoms
                .iter()
                .filter(|atom| {
                    set.is_none_or(|name| atom.get("set").and_then(Value::as_str) == Some(name))
                })
                .map(|atom| Record::clone(atom))
                .collect();
            atoms.iter_mut().for_each(|atom| {
                record::fill_origin(atom);
                crate::store::put_stored_embedding(atom, &self.store, workspace);
            });
            Ok(atoms)
        };
        match set {
            Some(raw) => {
                let named = packset_core::set_name::check(raw)
                    .map_err(|e| anyhow::Error::new(AtomError(e)))?;
                Ok(json!({
                    "workspace": workspace,
                    "set": named,
                    "user": cards::read_text(&self.home.set_user_path(workspace, &named)),
                    "memory": cards::read_text(&self.home.set_memory_path(workspace, &named)),
                    "instructions": cards::read_text(
                        &self.home.set_instructions_path(workspace, &named)
                    ),
                    "atoms": current(Some(&named))?,
                }))
            }
            None => Ok(json!({
                "workspace": workspace,
                "user": cards::read_text(&self.home.user_path()),
                "memory": cards::read_text(&self.home.memory_path(workspace)),
                "atoms": current(None)?,
            })),
        }
    }

    /// The active set for a workspace, or empty when nothing is pinned.
    #[must_use]
    pub fn pin(&self, workspace: &str) -> String {
        let raw = cards::read_text(&self.home.pin_path(workspace));
        raw.lines()
            .next()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .and_then(|line| packset_core::set_name::check(line).ok())
            .unwrap_or_default()
    }

    /// Pin a set, or clear the pin when the name is empty.
    ///
    /// # Errors
    ///
    /// [`AtomError`] for a bad name, else the write's.
    pub fn set_pin(&self, workspace: &str, name: &str) -> anyhow::Result<String> {
        let _write = self.writes.lock().unwrap_or_else(|e| e.into_inner());
        let path = self.home.pin_path(workspace);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if name.trim().is_empty() {
            std::fs::write(&path, "")?;
            return Ok(String::new());
        }
        let stored =
            packset_core::set_name::check(name).map_err(|e| anyhow::Error::new(AtomError(e)))?;
        std::fs::write(&path, format!("{stored}\n"))?;
        Ok(stored)
    }

    /// The pin plus that set's standing instructions.
    ///
    /// # Errors
    ///
    /// Never, in practice: an unreadable set reads as empty.
    pub fn pin_payload(&self, workspace: &str) -> anyhow::Result<Value> {
        let named = self.pin(workspace);
        let instructions = if named.is_empty() {
            String::new()
        } else {
            cards::read_text(&self.home.set_instructions_path(workspace, &named))
        };
        Ok(json!({
            "workspace": workspace,
            "set": named,
            "instructions": instructions,
        }))
    }

    /// Write whichever of a set's three cards the body carried.
    ///
    /// Absent and empty are different: a key that is not there leaves that card
    /// alone, and a key set to an empty string clears it.
    ///
    /// # Errors
    ///
    /// [`AtomError`] for a bad set name, else the write's.
    pub fn write_set(
        &self,
        workspace: &str,
        name: &str,
        body: &Map<String, Value>,
    ) -> Result<String, cards::WriteError> {
        let stored = packset_core::set_name::check(name).map_err(AtomError)?;
        for (key, path) in [
            ("user", self.home.set_user_path(workspace, &stored)),
            ("memory", self.home.set_memory_path(workspace, &stored)),
            (
                "instructions",
                self.home.set_instructions_path(workspace, &stored),
            ),
        ] {
            let Some(value) = body.get(key) else { continue };
            let text = value.as_str().unwrap_or("");
            let cap = if key == "memory" {
                MEMORY_CAP
            } else {
                USER_CAP
            };
            cards::write_capped(&path, text, cap)?;
        }
        Ok(stored)
    }

    /// Write the seat card, archiving and refusing on overflow.
    ///
    /// # Errors
    ///
    /// The write's, with overflow distinguishable so the caller can answer 413.
    pub fn set_user(&self, text: &str) -> Result<(), cards::WriteError> {
        match cards::write_capped(&self.home.user_path(), text, USER_CAP) {
            Err(cards::WriteError::Overflow(o)) => {
                // Archived first, so refused text is not lost; an archive
                // failure is reported over the overflow.
                self.archive("global", text)?;
                Err(cards::WriteError::Overflow(o))
            }
            Ok(()) => {
                self.project_cards(None);
                Ok(())
            }
            other => other,
        }
    }

    /// Write a workspace card, archiving and refusing on overflow.
    ///
    /// # Errors
    ///
    /// As [`Service::set_user`].
    pub fn set_memory(&self, workspace: &str, text: &str) -> Result<(), cards::WriteError> {
        match cards::write_capped(&self.home.memory_path(workspace), text, MEMORY_CAP) {
            Err(cards::WriteError::Overflow(o)) => {
                self.archive(workspace, text)?;
                Err(cards::WriteError::Overflow(o))
            }
            Ok(()) => {
                self.project_cards(Some(workspace));
                Ok(())
            }
            other => other,
        }
    }

    /// Append to today's archive for a workspace.
    ///
    /// # Errors
    ///
    /// The write's.
    pub fn archive(&self, workspace: &str, text: &str) -> Result<(), cards::WriteError> {
        let day = clock::utcnow()[..10].to_string();
        let path = self.home.archive_path(workspace, &day);
        cards::add_entry(&path, text, 1_000_000)
    }

    /// Hold one body for a workspace, capped. Expired slots go first. A
    /// new workspace past the slot cap drops the oldest one.
    pub fn put_attach(&self, workspace: &str, text: &str, label: &str) -> Value {
        let body = packset_core::prose::prefix_chars(text, ATTACH_CAP).to_string();
        let slot = Attachment {
            text: body,
            label: label.trim().to_string(),
        };
        let mut held = self.attach.lock().expect("attach lock");
        let now = std::time::Instant::now();
        purge_attach(&mut held, now);
        if !held.contains_key(workspace) && held.len() >= ATTACH_SLOTS {
            if let Some(oldest) = held
                .iter()
                .min_by_key(|(_, slot)| slot.at)
                .map(|(name, _)| name.clone())
            {
                held.remove(&oldest);
            }
        }
        held.insert(
            workspace.to_string(),
            HeldAttach {
                body: slot.clone(),
                at: now,
            },
        );
        json!({"workspace": workspace, "text": slot.text, "label": slot.label})
    }

    /// Take the held body, leaving the slot empty: an attachment is one turn's.
    pub fn take_attach(&self, workspace: &str) -> Option<Attachment> {
        let mut held = self.attach.lock().expect("attach lock");
        purge_attach(&mut held, std::time::Instant::now());
        held.remove(workspace).map(|slot| slot.body)
    }

    /// Read the held body without taking it.
    pub fn peek_attach(&self, workspace: &str) -> Option<Attachment> {
        let mut held = self.attach.lock().expect("attach lock");
        purge_attach(&mut held, std::time::Instant::now());
        held.get(workspace).map(|slot| slot.body.clone())
    }

    /// Move one slot's clock back, so a test can expire it without waiting.
    #[cfg(test)]
    fn age_attach(&self, workspace: &str, ago: std::time::Duration) {
        let mut held = self.attach.lock().expect("attach lock");
        if let Some(slot) = held.get_mut(workspace) {
            slot.at = std::time::Instant::now()
                .checked_sub(ago)
                .unwrap_or_else(std::time::Instant::now);
        }
    }

    /// Keep the projection level with a write: upsert a live atom, delete one
    /// that left the live set. No-op without a search binary.
    fn project_atoms(&self, atoms: &[Record]) {
        let now = clock::utcnow();
        let mut ops = Vec::with_capacity(atoms.len());
        for atom in atoms {
            let Some(id) = atom.get("id").and_then(Value::as_str) else {
                continue;
            };
            if record::is_live(atom, &now) || record::is_due(atom, &now) {
                ops.push(ProjectionOp::Upsert(crate::milli::atom_document(atom)));
            } else {
                ops.push(ProjectionOp::Delete(id.to_string()));
            }
        }
        self.queue_projection(ops);
    }

    /// Keep the projection's copy of the cards level with a write.
    fn project_cards(&self, workspace: Option<&str>) {
        let workspace = workspace.unwrap_or("");
        let user = cards::read_text(&self.home.user_path());
        let memory = if workspace.is_empty() {
            String::new()
        } else {
            cards::read_text(&self.home.memory_path(workspace))
        };
        let docs = crate::milli::pack_documents::<Record>(workspace, &user, &memory, &[]);
        self.queue_projection(docs.into_iter().map(ProjectionOp::Upsert).collect());
    }

    /// The atoms that were live at `at`, over the `valid_from` / `valid_to` window.
    ///
    /// # Errors
    ///
    /// The store's, or a stamp `parse_millis` will not accept.
    pub fn as_of(&self, workspace: &str, at: &str) -> anyhow::Result<Value> {
        let at =
            clock::canonicalize(at).ok_or_else(|| anyhow::anyhow!("as_of must be a timestamp"))?;
        let mut atoms = self.store.as_of(workspace, &at)?;
        // The store's dated scan skips tombstones, so a forget is not parsed
        // into the answer. A retraction is a tombstone whose window still
        // answers: the text and the deed stay, and this is the read that
        // returns them. Search keeps the store's scan, which drops the
        // tombstone, so a live search does not list the claim.
        for record in self.store.scan(Some(workspace))? {
            if retraction_live_at(&record, &at) {
                atoms.push(Arc::new(record));
            }
        }
        let atoms: Vec<_> = atoms.iter().map(|atom| record::shown(atom)).collect();
        Ok(json!({ "atoms": atoms, "as_of": at }))
    }

    /// Ranked hits, and which engine produced them: the projection when
    /// present, else the linear scan; any projection failure falls back whole.
    /// `as_of` retrieves the atoms live then; `rerank` runs the cross-encoder
    /// second stage. The rank is the shipped order.
    ///
    /// # Errors
    ///
    /// [`AtomError`] for a bad set name, a stamp `parse_millis` will not
    /// accept, else the store's.
    #[allow(clippy::too_many_arguments)]
    pub fn search(
        &self,
        workspace: &str,
        query: &str,
        limit: usize,
        set: Option<&str>,
        panel: &packset_core::Panel,
        as_of: Option<&str>,
        rerank: bool,
        kind: Option<&str>,
    ) -> anyhow::Result<Value> {
        self.search_ranked(
            workspace,
            query,
            limit,
            set,
            panel,
            as_of,
            rerank,
            kind,
            packset_core::search::Rank::Fused,
        )
    }

    /// [`Self::search`] with an explicit rank. `Rank::Relevance` chooses
    /// the top k by lexical, BM25 and cosine, with no half-life and no
    /// retrievability.
    ///
    /// # Errors
    ///
    /// As [`Self::search`].
    #[allow(clippy::too_many_arguments)]
    pub fn search_ranked(
        &self,
        workspace: &str,
        query: &str,
        limit: usize,
        set: Option<&str>,
        panel: &packset_core::Panel,
        as_of: Option<&str>,
        rerank: bool,
        kind: Option<&str>,
        rank: packset_core::search::Rank,
    ) -> anyhow::Result<Value> {
        self.maintain_cache();
        let named = match set {
            Some(raw) => Some(
                packset_core::set_name::check(raw).map_err(|e| anyhow::Error::new(AtomError(e)))?,
            ),
            None => None,
        };
        let scope = named.as_deref();
        // A named set swaps the prose for that set's cards. The atom list stays
        // the whole live set, because the scope is a filter in the scorer and
        // not a smaller corpus.
        let (user, memory) = match scope {
            Some(name) => (
                cards::read_text(&self.home.set_user_path(workspace, name)),
                cards::read_text(&self.home.set_memory_path(workspace, name)),
            ),
            None => (
                cards::read_text(&self.home.user_path()),
                cards::read_text(&self.home.memory_path(workspace)),
            ),
        };
        // The snapshot and the index over it come as a pair: an ordinal in the
        // index means a position in that snapshot and in no other. A dated
        // retrieve cannot use the live cache: that cache already dropped the
        // closed window.
        let as_of = match as_of {
            Some(raw) => Some(
                clock::canonicalize(raw)
                    .ok_or_else(|| anyhow::anyhow!("as_of must be a timestamp"))?,
            ),
            None => None,
        };
        let now = as_of.clone().unwrap_or_else(clock::utcnow);
        let dated = as_of.as_deref().map(|at| self.store.as_of(workspace, at));
        let (atoms, index, documents, vectors) = match dated {
            Some(scan) => {
                let atoms: crate::store::Live = std::sync::Arc::new(scan?);
                let documents: std::sync::Arc<Vec<Vec<String>>> = std::sync::Arc::new(
                    atoms
                        .iter()
                        .map(|atom| packset_core::search::atom_tokens(atom))
                        .collect(),
                );
                let index = std::sync::Arc::new(packset_core::bm25::Index::build(
                    documents.iter().map(Vec::as_slice),
                ));
                // A dated read is not cached. Its vectors stay on the records.
                (atoms, index, documents, Arc::new(HashMap::new()))
            }
            None => self.store.searchable(workspace)?,
        };

        if packset_core::search::tokens(query).is_empty() {
            // Nothing to score, so the second stage does not run. Reporting
            // it as off rather than as a stage that ran over an empty list
            // keeps a client from thinking a model was asked.
            return Ok(json!({
                "hits": [],
                "engine": "linear",
                "as_of": as_of,
                "rerank": "off",
                "rank": rank.as_str(),
            }));
        }

        // The index sees every write so far before it is asked.
        self.flush_projection();
        let dir = self.home.milli_dir();
        let corpus = crate::milli::Corpus {
            workspace,
            user: &user,
            memory: &memory,
            atoms: &atoms,
        };
        // Two scorers over the same pack, because they are strong at different
        // queries: one finds an atom through a typo or a prefix and weighs every
        // word alike, the other weighs a word by how much it narrows the pack
        // down and normalises for length. The panel is what turns the two
        // rankings into one, and two lists agreeing about a hit is a vote for
        // it rather than a duplicate.
        // When the second stage will read the head, the first stage has to
        // return at least that many, or nothing below `limit` can be promoted.
        let first_limit = if rerank {
            limit.max(crate::embed::RERANK_DEPTH)
        } else {
            limit
        };
        let ask = packset_core::search::Ask {
            user: &user,
            memory: &memory,
            atoms: &atoms,
            query,
            limit: first_limit,
            set: scope,
            kind,
            now: &now,
        };
        let ranked_terms = packset_core::search::search_bm25_ranked(&ask, &index, rank);
        // A third ballot when this seat has an encoder. The two lexical
        // scorers both need the question and the atom to share words, and this
        // one does not, which is the gap it exists to close.
        // `vectors` is the snapshot these atoms were read with. A second
        // cache lookup can miss the workspace and drop the ballot. A dated
        // read has no side table, and the scorer reads the record instead.
        // The milli projection is live-now. A dated question over a closed
        // window would otherwise miss the atom the retrieve just found.
        // Without an encoder the projection is the only lexical voice besides
        // BM25, and on LoCoMo through this writer it ranked worse than the
        // pack's own scorer (hit@1 0.267 against 0.342) and searched slower.
        // A seat with no encoder reads the pack unless it named the binary.
        //
        // The projection is a child process. It runs beside the encoder
        // attempt. A down or busy encoder is not waited on: this request
        // is ranked by words, and a child that is not already up is started
        // aside rather than in the request.
        let projection_may_count =
            as_of.is_none() && (crate::milli::named() || crate::embed::binary().is_some());
        let (ranked_meaning, projected_early) = std::thread::scope(|scope_| {
            let projecting = projection_may_count.then(|| {
                scope_.spawn(|| crate::milli::search(corpus, query, first_limit, &dir, scope, kind))
            });
            // The dense ballot is the encoder's, when one is already up
            // and free. `INSIDE_EMBED=off` and `--no-embed` leave it out.
            // A miss leaves the two word rankings.
            let meaning = if crate::embed::enabled() {
                crate::embed::try_encode_query(query)
                    .map(|vector| {
                        packset_core::search::search_dense_at(&ask, &vector, |ordinal| {
                            let atom = atoms.get(ordinal)?;
                            if let Some(parsed) = packset_core::search::embedding_of(atom) {
                                return Some(parsed);
                            }
                            let id = atom.get("id").and_then(Value::as_str)?;
                            if let Some(cached) = vectors.get(id) {
                                return Some(cached.as_ref().to_vec());
                            }
                            self.store.stored_vector(workspace, id)
                        })
                    })
                    .filter(|hits| !hits.is_empty())
            } else {
                None
            };
            let projected = projecting.and_then(|handle| handle.join().ok()).flatten();
            (meaning, projected)
        });
        let meaning = ranked_meaning.clone();
        let projected = if as_of.is_some() || (ranked_meaning.is_none() && !crate::milli::named()) {
            None
        } else {
            projected_early
        };
        let (mut ranked, engine) = match projected {
            Some(atom_hits) => {
                // Prose always comes from the pack, so the index copy of a card
                // can be stale without anyone reading it.
                let prose = packset_core::search::search_linear(&packset_core::search::Ask {
                    atoms: &[],
                    ..ask
                });
                let mut ballots = vec![prose, atom_hits, ranked_terms];
                ballots.extend(ranked_meaning);
                (
                    packset_core::search::merge_ballots_ranked(
                        &ballots,
                        first_limit,
                        panel,
                        &now,
                        rank,
                    ),
                    "milli",
                )
            }
            None => {
                let lexical =
                    packset_core::search::search_linear_with_ranked(&ask, &documents, rank);
                let mut ballots = vec![lexical, ranked_terms];
                ballots.extend(ranked_meaning);
                let engine = if ballots.len() > 2 { "dense" } else { "linear" };
                (
                    packset_core::search::merge_ballots_ranked(
                        &ballots,
                        first_limit,
                        panel,
                        &now,
                        rank,
                    ),
                    engine,
                )
            }
        };
        // The same stage the locomo arm measures. Off unless asked. An absent
        // or broken reranker leaves the first-stage order, the same way an
        // absent encoder leaves the dense ballot out. One hit cannot be
        // reordered, so asking the model about it would only spend a
        // forward pass to return the same order: that stage settles.
        let stage = if rerank && ranked.len() > 1 {
            match crate::embed::rerank_hits(query, &ranked) {
                Some(reordered) => {
                    ranked = reordered;
                    "cross-encoder"
                }
                None => "absent",
            }
        } else if rerank && ranked.len() == 1 {
            "settled"
        } else {
            "off"
        };
        ranked.truncate(limit);
        // The cross-encoder's own number, null when this hit was not scored.
        // The other fields are the first stage's.
        for hit in &mut ranked {
            if let Some(object) = hit.as_object_mut() {
                object
                    .entry("rerank_score".to_string())
                    .or_insert(Value::Null);
            }
        }
        if let Some(dense) = &meaning {
            packset_core::search::mark_dense(&mut ranked, dense);
        }
        // Due is the clock (`/v1/due`, `ljos due`). Mixing it into search
        // put 212 due personas at score 3.1 on every query.
        Ok(json!({
            "hits": ranked,
            "engine": engine,
            "as_of": as_of,
            "rerank": stage,
            "rank": rank.as_str(),
        }))
    }

    /// Mine one archived day into proposals.
    ///
    /// # Errors
    ///
    /// The miner's, or the store's.
    pub fn compact(
        &self,
        workspace: &str,
        day: Option<&str>,
        transcript: Option<&str>,
    ) -> anyhow::Result<Value> {
        let live = self.store.live(workspace)?;
        let proposed =
            crate::proposals::compact_day(&self.home, workspace, day, &live, transcript, new_id)?;
        Ok(json!({"n": proposed.len(), "proposals": proposed}))
    }

    /// Propose one claim from one piece of text.
    ///
    /// # Errors
    ///
    /// The miner's, or [`AtomError`] when there is nothing to propose.
    pub fn propose(&self, body: &Map<String, Value>) -> anyhow::Result<Value> {
        let workspace = body
            .get("workspace")
            .and_then(Value::as_str)
            .filter(|w| !w.is_empty())
            .ok_or_else(|| anyhow::Error::new(AtomError("workspace required".into())))?;
        let text = body.get("text").and_then(Value::as_str).unwrap_or("");
        let when = body
            .get("when")
            .and_then(Value::as_str)
            .filter(|w| !w.is_empty())
            .unwrap_or("onDemand");
        let job = body
            .get("job")
            .and_then(Value::as_str)
            .filter(|j| !j.is_empty())
            .unwrap_or("extract");
        let transcript = body.get("transcript").and_then(Value::as_str);
        let live = self.store.live(workspace)?;
        let wall = crate::proposals::fence(&self.home, workspace, &live);
        let rec = crate::proposals::propose(
            &self.home,
            crate::proposals::Mining {
                workspace,
                job,
                when,
                wall: &wall,
                transcript,
            },
            text,
            new_id,
        )?;
        rec.ok_or_else(|| anyhow::Error::new(AtomError("nothing to propose".into())))
    }

    /// Turn an accepted proposal into a stored atom.
    ///
    /// # Errors
    ///
    /// The miner's, or the store's.
    pub fn accept(&self, workspace: &str, proposal_id: &str) -> anyhow::Result<Record> {
        let _write = self.writes.lock().unwrap_or_else(|e| e.into_inner());
        // A retry after the atom was stored returns that atom. It does not
        // open another proposal and it does not write a second claim.
        if let Some(atom_id) = crate::proposals::accepted_atom(&self.home, workspace, proposal_id) {
            if let Some(atom) = self.store.get(workspace, &atom_id)? {
                return Ok(atom);
            }
        }
        let (atom, rec) = crate::proposals::accept(&self.home, workspace, proposal_id)?;
        let stored = self.add_prepared(self.prepare(atom)?)?;
        let atom_id = stored.get("id").and_then(Value::as_str).unwrap_or("");
        crate::proposals::mark_accepted(&self.home, workspace, &rec, atom_id)?;
        Ok(stored)
    }

    /// The islands of a workspace: the link graph's communities, largest
    /// first, each as the atoms it holds.
    ///
    /// # Errors
    ///
    /// The store's.
    pub fn islands(&self, workspace: &str) -> anyhow::Result<Value> {
        let atoms = memory_only(&self.store.live(workspace)?);
        let graph = packset_core::island::Graph::from_atoms(&atoms);
        // Communities by modularity; label propagation stands beside it so
        // the two can be compared on the same pack.
        let found = packset_core::island::communities(&graph);
        let modularity = packset_core::island::modularity(&graph, &found);
        let propagated = packset_core::island::islands(&graph);
        let propagated_modularity = packset_core::island::modularity(&graph, &propagated);
        let islands: Vec<Value> = found
            .into_iter()
            .map(|members| {
                let signature = packset_core::island::signature(&graph, &members);
                let atoms: Vec<Value> = members
                    .iter()
                    .map(|&i| {
                        json!({
                            "id": atoms[i].get("id").cloned().unwrap_or(Value::Null),
                            "kind": atoms[i].get("kind").cloned().unwrap_or(Value::Null),
                            "text": atoms[i].get("text").cloned().unwrap_or(Value::Null),
                        })
                    })
                    .collect();
                json!({"size": members.len(), "signature": format!("{signature:016x}"), "atoms": atoms})
            })
            .collect();
        Ok(json!({
            "islands": islands,
            "atoms": atoms.len(),
            "method": "modularity",
            "modularity": modularity,
            "propagation": {"islands": propagated.len(), "modularity": propagated_modularity},
        }))
    }

    /// The claims the link graph turns on, highest first: a weighted
    /// PageRank over the links. What matters in the pack by its own
    /// connections, before any query.
    ///
    /// # Errors
    ///
    /// The store's.
    pub fn hubs(&self, workspace: &str, limit: usize) -> anyhow::Result<Value> {
        let atoms = memory_only(&self.store.live(workspace)?);
        let graph = packset_core::island::Graph::from_atoms(&atoms);
        let hubs: Vec<Value> = packset_core::island::hubs(&graph)
            .into_iter()
            .take(limit)
            .map(|(i, score)| {
                json!({
                    "id": atoms[i].get("id").cloned().unwrap_or(Value::Null),
                    "kind": atoms[i].get("kind").cloned().unwrap_or(Value::Null),
                    "text": atoms[i].get("text").cloned().unwrap_or(Value::Null),
                    "score": score,
                    "links": atoms[i].get("links").and_then(Value::as_array).map_or(0, Vec::len),
                })
            })
            .collect();
        Ok(json!({"hubs": hubs, "atoms": atoms.len()}))
    }

    /// Claims that fired together: every pair's link gains weight, their
    /// other links lose a little, and a missing link is made. Returns how
    /// many records changed.
    ///
    /// # Errors
    ///
    /// The store's.
    pub fn fire(&self, workspace: &str, ids: &[String]) -> anyhow::Result<Value> {
        self.fire_as(workspace, ids, None)
    }

    /// [`Self::fire`] through a persona's lens: the weights move under its
    /// name, the shared ones stand.
    ///
    /// # Errors
    ///
    /// The store's.
    pub fn fire_as(
        &self,
        workspace: &str,
        ids: &[String],
        lens: Option<&str>,
    ) -> anyhow::Result<Value> {
        let _write = self.writes.lock().unwrap_or_else(|e| e.into_inner());
        let mut key: Vec<&str> = ids.iter().map(String::as_str).collect();
        key.sort_unstable();
        key.dedup();
        let key = format!(
            "{workspace}\u{0}{}\u{0}{}",
            lens.unwrap_or(""),
            key.join("\u{0}")
        );
        if self.fire_held(key) {
            return Ok(json!({"fired": 0, "changed": 0, "held": true}));
        }
        let live = self.store.live(workspace)?;
        let mut atoms: Vec<Record> = live.iter().map(|a| Record::clone(a)).collect();
        let fired: Vec<usize> = ids
            .iter()
            .filter_map(|id| {
                atoms
                    .iter()
                    .position(|a| a.get("id").and_then(Value::as_str) == Some(id.as_str()))
            })
            .collect();
        let mut changed = packset_core::island::fire_as(&mut atoms, &fired, lens);
        // A fired claim served the work it was fired for: count the use in
        // its review record, so the sweep does not forget it by neglect.
        let once: std::collections::BTreeSet<usize> = fired.iter().copied().collect();
        for &i in &once {
            if let Some(Value::Object(r)) = atoms[i].get_mut("review") {
                let used = r.get("used").and_then(Value::as_u64).unwrap_or(0) + 1;
                r.insert("used".into(), json!(used));
                if !changed.contains(&i) {
                    changed.push(i);
                }
            }
        }
        if !changed.is_empty() {
            let batch: Vec<Record> = changed.iter().map(|&i| atoms[i].clone()).collect();
            let batch = self.store.upsert_many(&batch)?;
            self.project_atoms(&batch);
            self.patch_live_sets(workspace, &batch);
        }
        Ok(json!({"fired": fired.len(), "changed": changed.len(), "held": false}))
    }

    /// Consolidate the live set: in the order they were written, every
    /// claim that replaces an earlier one (`record::replaces`: an explicit
    /// `supersedes`, a correction sharing an entity, a rewrite, or a new
    /// object under the same head) closes the earlier one's window and
    /// names it. What a write does on arrival, run over what is already
    /// held, for a pack written before the rule or filled by import. With
    /// `apply` false nothing is written; the pairs are reported.
    ///
    /// # Errors
    ///
    /// The store's.
    pub fn consolidate(&self, workspace: &str, apply: bool) -> anyhow::Result<Value> {
        let _write = self.writes.lock().unwrap_or_else(|e| e.into_inner());
        let live = self.store.live(workspace)?;
        let mut atoms: Vec<Record> = live.iter().map(|a| Record::clone(a)).collect();
        atoms.sort_by(|a, b| {
            a.get("ts")
                .and_then(Value::as_str)
                .unwrap_or("")
                .cmp(b.get("ts").and_then(Value::as_str).unwrap_or(""))
        });
        let now = clock::utcnow();
        // Candidates share their first words or an entity; the rule is then
        // asked of each pair. A pack of ten thousand claims is buckets of a
        // few, not fifty million comparisons, and the nudge that counts the
        // pairs on every prompt stays cheap. A rewrite that shares neither
        // is not seen here, as it is not seen by a read.
        let mut buckets: std::collections::HashMap<String, Vec<usize>> =
            std::collections::HashMap::new();
        let mut keys_of: Vec<Vec<String>> = Vec::with_capacity(atoms.len());
        for (i, atom) in atoms.iter().enumerate() {
            let text = atom.get("text").and_then(Value::as_str).unwrap_or("");
            let head = record::head_tokens(text);
            let mut keys = Vec::new();
            if head.len() >= record::HEAD_MIN {
                keys.push(format!("h:{}", head[..record::HEAD_MIN].join(" ")));
            }
            // Only entities the claim carries; the ones read off its text
            // would put every claim that says "the" into one bucket and
            // make this a pass over every pair.
            if let Some(Value::Array(items)) = atom.get("entities") {
                for entity in items.iter().filter_map(Value::as_str) {
                    keys.push(format!("e:{}", entity.to_lowercase()));
                }
            }
            for key in &keys {
                buckets.entry(key.clone()).or_default().push(i);
            }
            keys_of.push(keys);
        }
        let mut open = vec![true; atoms.len()];
        let mut pairs: Vec<(usize, usize)> = Vec::new();
        for i in 0..atoms.len() {
            if !record::is_live(&atoms[i], &now) {
                open[i] = false;
                continue;
            }
            let mut candidates: Vec<usize> = keys_of[i]
                .iter()
                .filter_map(|key| buckets.get(key))
                // A bucket the size of the pack is no bucket.
                .filter(|members| members.len() <= CONSOLIDATE_BUCKET_MAX)
                .flat_map(|members| members.iter().copied())
                .filter(|&j| j < i && open[j])
                .collect();
            candidates.sort_unstable();
            candidates.dedup();
            for j in candidates {
                if open[j] && record::replaces(&atoms[i], &atoms[j]) {
                    open[j] = false;
                    pairs.push((i, j));
                }
            }
        }
        let closed: Vec<Value> = pairs
            .iter()
            .map(|(i, j)| {
                json!({
                    "old": atoms[*j].get("id").cloned().unwrap_or(Value::Null),
                    "old_text": atoms[*j].get("text").cloned().unwrap_or(Value::Null),
                    "new": atoms[*i].get("id").cloned().unwrap_or(Value::Null),
                    "new_text": atoms[*i].get("text").cloned().unwrap_or(Value::Null),
                })
            })
            .collect();
        if apply && !pairs.is_empty() {
            let mut touched: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
            for (i, j) in &pairs {
                record::close_valid_to(&mut atoms[*j], &now);
                let old_id = atoms[*j]
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let supersedes = atoms[*i]
                    .entry("supersedes")
                    .or_insert_with(|| Value::Array(Vec::new()));
                if let Value::Array(ids) = supersedes {
                    if !ids.iter().any(|v| v.as_str() == Some(old_id.as_str())) {
                        ids.push(Value::String(old_id));
                    }
                }
                // The claim that replaced another is the rule that survived.
                let entities = promoted_entities(&atoms[*i]);
                atoms[*i].insert("entities".into(), entities);
                touched.insert(*i);
                touched.insert(*j);
            }
            let batch: Vec<Record> = touched.into_iter().map(|k| atoms[k].clone()).collect();
            let batch = self.store.upsert_many(&batch)?;
            self.project_atoms(&batch);
            self.patch_live_sets(workspace, &batch);
        }
        Ok(json!({
            "live": live.len(),
            "closed": closed.len(),
            "applied": apply,
            "pairs": closed,
        }))
    }

    /// The memories a cue activates: the top search hits as seeds, spread
    /// two hops along the links, strongest first. With `fire`, the top
    /// [`FIRE_TOP`] of them fire together.
    ///
    /// # Errors
    ///
    /// As [`Service::search`], else the store's.
    pub fn activate(
        &self,
        workspace: &str,
        query: &str,
        limit: usize,
        panel: &packset_core::Panel,
        fire: bool,
    ) -> anyhow::Result<Value> {
        self.activate_as(workspace, query, limit, panel, fire, None)
    }

    /// [`Self::activate`] through a persona's lens: the spread follows the
    /// weights that persona wrote, and a fire writes them.
    ///
    /// # Errors
    ///
    /// The search's or the store's.
    pub fn activate_as(
        &self,
        workspace: &str,
        query: &str,
        limit: usize,
        panel: &packset_core::Panel,
        fire: bool,
        lens: Option<&str>,
    ) -> anyhow::Result<Value> {
        let seeds = self.search(
            workspace,
            query,
            ACTIVATION_SEEDS,
            None,
            panel,
            None,
            false,
            None,
        )?;
        let atoms = memory_only(&self.store.live(workspace)?);
        let graph = packset_core::island::Graph::from_atoms_as(&atoms, lens);
        let weighted: Vec<(usize, f64)> = seeds["hits"]
            .as_array()
            .map(|hits| {
                hits.iter()
                    .filter_map(|hit| {
                        let id = hit["id"].as_str()?;
                        let at = graph.position(id)?;
                        Some((
                            at,
                            hit["score"].as_f64().unwrap_or(1.0).max(f64::MIN_POSITIVE),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        // Seeds two scorers agreed on, when two or more ran. Activation
        // from weak seeds flows to the best-connected cluster, whatever
        // the cue was; an island seeded that way is reported weak and is
        // not fired, because firing it wires the wrong links tighter.
        let agreed = seeds["hits"]
            .as_array()
            .map(|hits| {
                hits.iter()
                    .filter(|hit| {
                        let of = hit["of"].as_u64().unwrap_or(1);
                        let named = hit["ballots"].as_u64().unwrap_or(1);
                        of < 2 || named >= 2
                    })
                    .filter(|hit| {
                        hit["id"]
                            .as_str()
                            .and_then(|id| graph.position(id))
                            .is_some()
                    })
                    .count()
            })
            .unwrap_or(0);
        let weak = agreed < 2;
        let lit = packset_core::island::activate(&graph, &weighted, ACTIVATION_HOPS);
        let strongest = lit.first().map_or(1.0, |(_, a)| *a);
        let island: Vec<Value> = lit
            .iter()
            .take(limit)
            .map(|(at, activation)| {
                json!({
                    "id": atoms[*at].get("id").cloned().unwrap_or(Value::Null),
                    "kind": atoms[*at].get("kind").cloned().unwrap_or(Value::Null),
                    "text": atoms[*at].get("text").cloned().unwrap_or(Value::Null),
                    "ts": atoms[*at].get("ts").cloned().unwrap_or(Value::Null),
                    "activation": activation / strongest,
                    "seed": weighted.iter().any(|(s, _)| s == at),
                })
            })
            .collect();
        let (fired, held) = if fire && !weak {
            // The cue holds the island for the hour as the claims do: a
            // closing that grew the island by a lesson would otherwise fire
            // a new set for the same title.
            let cue_key = format!(
                "cue\u{0}{workspace}\u{0}{}\u{0}{}",
                lens.unwrap_or(""),
                query.trim().to_lowercase()
            );
            let cue_held = self.cue_held(cue_key);
            if cue_held {
                (0, true)
            } else {
                let ids: Vec<String> = lit
                    .iter()
                    .take(FIRE_TOP)
                    .filter_map(|(at, _)| atoms[*at].get("id").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect();
                let fired = self.fire_as(workspace, &ids, lens)?;
                (
                    fired["changed"].as_u64().unwrap_or(0),
                    fired["held"].as_bool().unwrap_or(false),
                )
            }
        } else {
            (0, false)
        };
        Ok(json!({
            "island": island,
            "seeds": weighted.len(),
            "agreed_seeds": agreed,
            "weak": weak,
            "dense": crate::embed::binary().is_some(),
            "hops": ACTIVATION_HOPS,
            "fired": fired,
            "held": held,
            "as": lens.unwrap_or(""),
        }))
    }

    /// The open proposals for a workspace.
    #[must_use]
    pub fn proposals(&self, workspace: &str) -> Vec<Value> {
        crate::proposals::list_open(&self.home, workspace)
    }

    /// Every deed accession cited by a live atom in a workspace, sorted.
    ///
    /// The accession is the only identifier that crosses the tracker, the pack
    /// and the deed store, so a pack has to be able to list its own citations
    /// the way a tracker does. A product cited by one atom and by nothing else
    /// is exactly the citation that goes stale unnoticed.
    ///
    /// # Errors
    ///
    /// The store's.
    pub fn accessions(&self, workspace: &str) -> anyhow::Result<Vec<String>> {
        let atoms = self.store.live(workspace)?;
        let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for atom in atoms.iter() {
            let Some(entities) = atom.get("entities").and_then(Value::as_array) else {
                continue;
            };
            for entity in entities {
                let Some(text) = entity.as_str() else {
                    continue;
                };
                let text = text.trim();
                if record::is_accession(text) {
                    seen.insert(text);
                }
            }
        }
        Ok(seen.into_iter().map(ToString::to_string).collect())
    }

    /// The live atoms that cite one deed accession.
    ///
    /// The other direction of [`Service::accessions`], and the pack's half of
    /// the backwards walk: a tracker answers which issues cite a product, and
    /// this answers which remembered claims do. Neither store opens the other,
    /// so what composes them is a caller holding one accession.
    ///
    /// # Errors
    ///
    /// The store's.
    pub fn citers(&self, workspace: &str, accession: &str) -> anyhow::Result<Vec<Value>> {
        let wanted = accession.trim();
        if wanted.is_empty() {
            return Ok(Vec::new());
        }
        let atoms = self.store.live(workspace)?;
        Ok(atoms
            .iter()
            .filter(|atom| {
                atom.get("entities")
                    .and_then(Value::as_array)
                    .is_some_and(|entities| {
                        entities
                            .iter()
                            .filter_map(Value::as_str)
                            .any(|entity| entity.trim() == wanted)
                    })
            })
            .map(|atom| {
                json!({
                    "id": atom.get("id").cloned().unwrap_or(Value::Null),
                    "kind": atom.get("kind").cloned().unwrap_or(Value::Null),
                    "text": atom.get("text").cloned().unwrap_or(Value::Null),
                    "ts": atom.get("ts").cloned().unwrap_or(Value::Null),
                })
            })
            .collect())
    }

    /// Seat home, atom counts by kind, pin, index and embedder.
    ///
    /// While the encoder is loading, `embedder.remaining_ms` is the
    /// estimated milliseconds left. The field is absent once the encoder
    /// is answering or unavailable.
    ///
    /// # Errors
    ///
    /// The scan's.
    pub fn status(
        &self,
        workspace: Option<&str>,
        panel: &packset_core::Panel,
    ) -> anyhow::Result<Value> {
        // Counts come from the per-generation cache, not a fresh scan:
        // status runs constantly (doctor, the CLI, hooks asking what the
        // pack holds) and a full parse per call is what made it linear in
        // the pack. The milli, embedder and panel rows below stay live.
        let counts = self.store.counts(workspace)?;
        let live = counts.live;
        let tomb = counts.tombstone;
        let expired = counts.expired;
        let forgotten = counts.forgotten;
        let last_write = counts.last_write;
        let milli_dir = self.home.milli_dir();
        let index_ready = crate::milli::index_ready(&milli_dir);
        let pin = workspace.map(|w| self.pin(w)).unwrap_or_default();
        let answering_binary =
            crate::embed::binary().is_some() && crate::embed::last_dense() != Some(false);
        // A binary that did not answer its last call is not available,
        // whatever is on disk: the ranking is lexical until it does.
        // `remaining_ms` is only present while a load is still running.
        let mut embedder = json!({
            "enabled": embed_enabled(),
            "binary": crate::embed::binary().map(|path| path.display().to_string()),
            "available": embed_enabled() && answering_binary,
            "answering": crate::embed::last_dense(),
        });
        if let Some(ms) = crate::embed::remaining_ms() {
            embedder["remaining_ms"] = json!(ms);
        }
        Ok(json!({
            "home": self.home.root().display().to_string(),
            // Which build is answering. The version does not move between
            // releases and the code does, so a seat comparing an installed
            // daemon against a repository needs the commit to compare.
            "version": env!("CARGO_PKG_VERSION"),
            "commit": env!("PACKSET_COMMIT"),
            "workspace": workspace.unwrap_or(""),
            "set": pin,
            "live": live.values().sum::<usize>(),
            "live_cap": self.live_cap,
            "tombstone": tomb.values().sum::<usize>(),
            "expired": expired.values().sum::<usize>(),
            "live_by_kind": live,
            "tombstone_by_kind": tomb,
            "forgotten_by_reason": forgotten,
            "expired_by_kind": expired,
            "last_write_ts": if last_write.is_empty() { Value::Null } else { Value::String(last_write) },
            "milli": {
                "binary": milli_binary(),
                "index_dir": milli_dir.display().to_string(),
                "index_ready": index_ready,
            },
            "embedder": embedder,
            // Off unless the host asked. The locomo cost lives in the README;
            // status only says whether this writer will spend it.
            "rerank": {
                "enabled": crate::embed::wanted(),
                // The reranker is the same binary: one that cannot load its
                // runtime for the encoder cannot load it for the reranker.
                "available": answering_binary,
                "depth": crate::embed::RERANK_DEPTH,
            },
            // Which voters are running, because the panel is host
            // configuration a client cannot see and a wrong one changes every
            // answer without changing any of them into an error.
            "panel": {
                "fuse": panel.fuse.as_str(),
                "diversify": panel.diversify.as_str(),
                "decay": panel.decay.as_str(),
            },
            // Which top-k cuts this writer honours. An older writer omits
            // `search`, which is how a client falls back to the shipped order.
            "search": {
                "ranks": ["fused", "relevance"],
            },
        }))
    }
}

/// Whether the dense-rank embedder is switched on.
///
/// The switch is [`crate::embed::enabled`]: `INSIDE_EMBED` and
/// `packsetd --no-embed`. Switched on and present are different questions.
/// The runtime behind the encoder lives in its own binary, so a seat can
/// have it enabled with nothing to run; keyword search does not depend on
/// either.
fn embed_enabled() -> bool {
    crate::embed::enabled()
}

/// The search binary as `/v1/status` reports it.
///
/// One lookup, shared with the search path, so status cannot say the
/// projection is available while search fails to find it.
fn milli_binary() -> Value {
    crate::milli::binary().map_or(Value::Null, |p| Value::String(p.display().to_string()))
}

/// A fresh atom id: thirty-two hex characters, the shape already in the store.
fn new_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0u128, |d| d.as_nanos());
    let pid = u128::from(std::process::id());
    let counter = {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        u128::from(NEXT.fetch_add(1, Ordering::Relaxed))
    };
    // Not a v4 uuid and not claiming to be: unique on this seat is the whole
    // requirement, and the store keys on workspace and id together.
    let mut state = nanos ^ (pid << 64) ^ (counter << 32);
    let mut out = String::with_capacity(32);
    for _ in 0..32 {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let nibble = ((state >> 64) & 0xf) as u8;
        out.push(char::from_digit(u32::from(nibble), 16).unwrap_or('0'));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> (tempfile::TempDir, Service) {
        let dir = tempfile::tempdir().unwrap();
        let svc = Service::open(Home::new(dir.path())).unwrap();
        (dir, svc)
    }

    #[test]
    fn an_agent_derived_lesson_is_live_at_once_with_its_origin_and_source() {
        let (_dir, svc) = service();
        let mut lesson = atom("The fuse is CombMNZ for this seat.");
        lesson.insert("kind".into(), json!("lesson"));
        lesson.insert("origin".into(), json!("agent-derived"));
        lesson.insert(
            "source".into(),
            json!({"via": "ljos", "harness": "codex", "seat": "codex", "session": "codex@abc", "at": "2026-10-14T10:00:00Z", "reason": "the user said so"}),
        );
        let stored = svc.add(lesson).unwrap();
        assert_eq!(stored["origin"], json!("agent-derived"));
        assert_eq!(stored["source"]["reason"], json!("the user said so"));
        assert_eq!(svc.store.live("w").unwrap().len(), 1);
        assert!(crate::proposals::list_open(&svc.home, "w").is_empty());
    }

    /// An admission proposal, the line a writer that held agent writes appended.
    fn plant_admission(home: &Home) -> &'static str {
        let path = crate::proposals::proposals_path(home, "w");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let rec = json!({
            "schema": "inside.proposal/v1",
            "id": "p-held",
            "workspace": "w",
            "text": "The fuse is CombMNZ for this seat.",
            "origin": "agent-derived",
            "hook": true,
            "job": "admission",
            "when": "onDemand",
            "status": "open",
            "ts": "2026-08-20T00:00:00Z",
            "span": "The fuse is CombMNZ for this seat.",
            "verdict": "SUPPORTED",
            "transcript": "",
            "why": "agent-derived",
            "atom": {
                "schema": "inside.atom/v1",
                "id": "heldatom00000000000000000000001",
                "workspace": "w",
                "text": "The fuse is CombMNZ for this seat.",
                "kind": "lesson",
                "level": "explicit",
                "origin": "agent-derived",
                "about_peer": "rgoswami",
                "by_peer": "hermes",
                "entities": ["seat:inky", "issue:demo-amz9", "horizon:transient"],
                "source": {"via": "ljos", "harness": "inky", "basis": "agent"},
                "hook": true,
                "embedding": [0.1, 0.2],
                "ts": "2026-08-20T00:00:00Z",
                "valid_from": "2026-08-20T00:00:00Z",
                "tombstone": false,
                "trust": 1.0
            }
        });
        let line = serde_json::to_string(&rec).unwrap();
        std::fs::write(&path, format!("{line}\n")).unwrap();
        "p-held"
    }

    #[test]
    fn the_byte_cap_counts_shapes_and_holds_for_one_workspace() {
        let (_dir, svc) = service();
        let shape = record::Shape::of(&atom("alpha beta gamma"));
        assert!(shape_bytes(&shape) >= "alpha".len() + "beta".len() + "gamma".len());
        svc.store.set_cache_limit_bytes(0);
        for i in 0..8 {
            let text = (0..12)
                .map(|word| format!("w{i}t{word}"))
                .collect::<Vec<_>>()
                .join(" ");
            svc.add(atom(&text)).unwrap();
        }
        svc.store.searchable("w").unwrap();
        let store_only = svc.store.cache_resident_bytes();
        let full = svc.resident_cache_bytes();
        assert!(svc.shape_count() > 0, "the claims were not shaped");
        assert!(
            full > store_only,
            "shapes and live sets were not counted: {full} vs {store_only}"
        );
        let cap = (full / 3).max(1);
        svc.store.set_cache_limit_bytes(cap);
        let held = svc.resident_cache_bytes();
        assert!(held <= cap, "held {held} cap {cap} full {full}");
        assert_eq!(svc.store.live("w").unwrap().len(), 8);
        let after = svc.resident_cache_bytes();
        assert!(after <= cap, "a read left {after} cached, cap {cap}");
    }

    #[test]
    fn a_small_append_under_pressure_is_journaled_and_a_full_log_is_refused() {
        let (dir, svc) = service();
        let _pressure = crate::admit::PressureGuard::enter();
        svc.set_append_cap(8_000);
        let stored = svc.add(atom("A short lesson about the fuse.")).unwrap();
        let id = stored
            .get("id")
            .and_then(Value::as_str)
            .unwrap()
            .to_string();
        let path = dir.path().join("append.log");
        let bytes = std::fs::read(&path).unwrap();
        assert!(
            bytes
                .windows(id.len())
                .any(|window| window == id.as_bytes()),
            "the lesson was not in the log"
        );
        assert_eq!(svc.store.live("w").unwrap().len(), 1);
        let err = svc.add(atom(&"x".repeat(9_000))).unwrap_err();
        assert!(
            err.downcast_ref::<crate::journal::Full>().is_some(),
            "{err}"
        );
        drop(_pressure);
        svc.maintain_cache();
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            0,
            "the log was kept after pressure"
        );
    }

    #[test]
    fn a_crashed_append_is_replayed_when_the_pack_opens() {
        let dir = tempfile::tempdir().unwrap();
        let record = serde_json::to_vec(&json!({
            "id": "kept",
            "workspace": "w",
            "text": "the lesson that must survive"
        }))
        .unwrap();
        let mut log =
            crate::journal::AppendLog::open(dir.path().join("append.log"), 1_000_000).unwrap();
        log.append(&record).unwrap();
        drop(log);
        let svc = Service::open(Home::new(dir.path())).unwrap();
        let live = svc.store.live("w").unwrap();
        assert!(
            live.iter()
                .any(|atom| atom.get("id").and_then(Value::as_str) == Some("kept")),
            "the journaled lesson was not replayed"
        );
        assert_eq!(
            std::fs::metadata(dir.path().join("append.log"))
                .unwrap()
                .len(),
            0
        );
    }

    fn atom(text: &str) -> Record {
        json!({
            "workspace": "w",
            "text": text,
            "kind": "voice",
            "about_peer": "rgoswami",
            "by_peer": "hermes"
        })
        .as_object()
        .unwrap()
        .clone()
    }

    /// Text T in set s, then an unrelated claim with no set, then T in s
    /// again: one claim. Reconciling shapes from the set being written
    /// dropped every other set's tokens, and the second T was stored again.
    #[test]
    fn a_repeated_claim_in_a_set_stays_one_after_another_set_is_written() {
        let (_dir, svc) = service();
        let mut claim = atom("The token T is the claim that must stay one.");
        claim.insert("set".into(), json!("s"));
        let stored = svc.add(claim.clone()).unwrap();
        let id = stored["id"].as_str().unwrap().to_string();
        svc.add(atom("An unrelated claim sits in no set at all."))
            .unwrap();
        let again = svc.add(claim).unwrap();
        assert_eq!(again["id"].as_str().unwrap(), id, "{again:?}");
        let copies = svc
            .store
            .live("w")
            .unwrap()
            .iter()
            .filter(|peer| {
                peer.get("text").and_then(Value::as_str)
                    == Some("The token T is the claim that must stay one.")
            })
            .count();
        assert_eq!(copies, 1, "the second write stored another copy of T");
    }

    /// A thousand claims, split across two sets and none, dedupe and replace
    /// inside their own set. A retry is the same id, a rewrite closes only
    /// the claim it rewrites, and the other set is unchanged.
    #[test]
    fn a_thousand_claims_keep_one_copy_and_close_only_their_own_rewrites() {
        let (_dir, svc) = service();
        let mut ids = Vec::with_capacity(1000);
        for i in 0..1000 {
            let mut row = atom(&format!(
                "Claim {i:04} keeps its own sentence in the fixture."
            ));
            row.insert("entities".into(), json!([format!("item-{i:04}")]));
            match i % 3 {
                0 => {
                    row.insert("set".into(), json!("alpha"));
                }
                1 => {
                    row.insert("set".into(), json!("beta"));
                }
                _ => {}
            }
            let stored = svc.add(row).unwrap();
            ids.push(stored["id"].as_str().unwrap().to_string());
        }
        assert_eq!(svc.store().live("w").unwrap().len(), 1000);

        for i in [0usize, 1, 2, 500, 999] {
            let mut row = atom(&format!(
                "Claim {i:04} keeps its own sentence in the fixture."
            ));
            row.insert("entities".into(), json!([format!("item-{i:04}")]));
            match i % 3 {
                0 => {
                    row.insert("set".into(), json!("alpha"));
                }
                1 => {
                    row.insert("set".into(), json!("beta"));
                }
                _ => {}
            }
            let again = svc.add(row).unwrap();
            assert_eq!(
                again["id"].as_str().unwrap(),
                ids[i],
                "retry of {i} stored a second copy"
            );
        }
        assert_eq!(
            svc.store().live("w").unwrap().len(),
            1000,
            "a retry grew the pack"
        );

        let mut moved = atom("Claim 0000 keeps its own sentence in the fixture.");
        moved.insert("entities".into(), json!(["item-0000"]));
        moved.insert("set".into(), json!("beta"));
        let other = svc.add(moved).unwrap();
        assert_ne!(
            other["id"].as_str().unwrap(),
            ids[0],
            "the same words in another set replaced the first"
        );

        let mut closed = Vec::new();
        for n in 0..10 {
            let i = n * 3;
            let mut row = atom(&format!(
                "Claim {i:04} keeps its own sentence in the fixture now."
            ));
            row.insert("entities".into(), json!([format!("item-{i:04}")]));
            row.insert("set".into(), json!("alpha"));
            let stored = svc.add(row).unwrap();
            let supersedes = stored["supersedes"].as_array().cloned().unwrap_or_default();
            assert!(
                supersedes
                    .iter()
                    .any(|v| v.as_str() == Some(ids[i].as_str())),
                "rewrite of {i} did not close it: {stored:?}"
            );
            assert_ne!(stored["id"].as_str().unwrap(), ids[i].as_str());
            closed.push(ids[i].clone());
        }

        let live = svc.store().live("w").unwrap();
        assert_eq!(live.len(), 1001, "the live set is not the scan's");
        for id in &closed {
            assert!(
                live.iter()
                    .all(|peer| peer.get("id").and_then(Value::as_str) != Some(id.as_str())),
                "{id} stayed live after its rewrite"
            );
        }
        let beta = live
            .iter()
            .filter(|peer| peer.get("set").and_then(Value::as_str) == Some("beta"))
            .count();
        let alpha = live
            .iter()
            .filter(|peer| peer.get("set").and_then(Value::as_str) == Some("alpha"))
            .count();
        // 334 alpha and 333 beta in the thousand, plus the copy that moved
        // into beta, and ten alpha rewrites that replace rather than add.
        assert_eq!(beta, 334, "beta changed under an alpha rewrite");
        assert_eq!(alpha, 334, "alpha did not keep one live claim per item");
    }

    /// Firing rewrites the claims from the cache, which does not carry the
    /// vector. The array on disk has to survive that rewrite.
    #[test]
    fn firing_keeps_the_stored_vector() {
        let (_dir, svc) = service();
        let embedding = json!([0.2, 0.4, 0.6]);
        for (id, links) in [("a", json!(["b"])), ("b", json!(["a"]))] {
            svc.store
                .upsert(
                    json!({
                        "id": id,
                        "workspace": "w",
                        "text": format!("claim {id} stands"),
                        "kind": "lesson",
                        "embedding": embedding,
                        "links": links,
                        "tombstone": false,
                    })
                    .as_object()
                    .unwrap(),
                )
                .unwrap();
        }
        assert!(svc.store.live("w").unwrap()[0].get("embedding").is_none());
        svc.fire("w", &["a".into(), "b".into()]).unwrap();
        for id in ["a", "b"] {
            let disk = svc.store.get("w", id).unwrap().unwrap();
            assert_eq!(
                disk["embedding"].as_array().map(Vec::len),
                Some(3),
                "{id} lost its vector: {disk:?}"
            );
            assert_eq!(svc.store.stored_vector("w", id).unwrap().len(), 3);
        }
    }

    /// A new sentence drops the old vector. A new vector sent with it is
    /// kept, and a field that is not the text leaves the vector alone.
    /// Null clears without a text change.
    #[test]
    fn an_update_that_changes_the_text_drops_the_old_vector() {
        let (_dir, svc) = service();
        svc.store
            .upsert(
                json!({
                    "id": "a",
                    "workspace": "w",
                    "text": "the old sentence about the river",
                    "kind": "lesson",
                    "origin": "user-declared",
                    "embedding": [0.2, 0.4, 0.6],
                    "tombstone": false,
                })
                .as_object()
                .unwrap(),
            )
            .unwrap();
        let mut fields = Map::new();
        fields.insert("text".into(), json!("the new sentence about the mountain"));
        svc.update("w", "a", &fields).unwrap();
        let disk = svc.store.get("w", "a").unwrap().unwrap();
        assert!(disk["embedding"].is_null(), "old vector kept: {disk:?}");
        assert!(svc.store.stored_vector("w", "a").is_none());

        fields.insert("embedding".into(), json!([0.1, 0.2, 0.3]));
        fields.insert(
            "text".into(),
            json!("the newer sentence about the mountain"),
        );
        svc.update("w", "a", &fields).unwrap();
        let disk = svc.store.get("w", "a").unwrap().unwrap();
        assert_eq!(disk["embedding"], json!([0.1, 0.2, 0.3]));

        let mut quiet = Map::new();
        quiet.insert("level".into(), json!("explicit"));
        svc.update("w", "a", &quiet).unwrap();
        let disk = svc.store.get("w", "a").unwrap().unwrap();
        assert_eq!(disk["embedding"], json!([0.1, 0.2, 0.3]));

        let mut clear = Map::new();
        clear.insert("embedding".into(), Value::Null);
        svc.update("w", "a", &clear).unwrap();
        let disk = svc.store.get("w", "a").unwrap().unwrap();
        assert!(disk["embedding"].is_null(), "{disk:?}");
        assert!(svc.store.stored_vector("w", "a").is_none());
    }

    /// A pack answers a client vector as it was stored, not as f32.
    #[test]
    fn a_pack_returns_a_client_vector_as_stored() {
        let (_dir, svc) = service();
        svc.store
            .upsert(
                json!({
                    "id": "a",
                    "workspace": "w",
                    "text": "a client vector stays as written",
                    "kind": "lesson",
                    "origin": "user-declared",
                    "embedding": [0.1, 1e39, -0.3],
                    "tombstone": false,
                })
                .as_object()
                .unwrap(),
            )
            .unwrap();
        let packed = svc.pack("w", None).unwrap();
        let atoms = packed["atoms"].as_array().unwrap();
        let atom = atoms.iter().find(|row| row["id"] == "a").unwrap();
        assert_eq!(atom["embedding"], json!([0.1, 1e39, -0.3]));
    }

    /// A deleted claim leaves no shape, and a third workspace does not keep
    /// the first two when the cap is two.
    #[test]
    fn shapes_follow_the_live_set_and_the_cap() {
        let (_dir, svc) = service();
        let stored = svc
            .add(atom("The first claim is the one that is deleted."))
            .unwrap();
        let id = stored
            .get("id")
            .and_then(Value::as_str)
            .unwrap()
            .to_string();
        assert_eq!(svc.shape_count(), 1);
        svc.delete_atom("w", &id, None).unwrap();
        assert_eq!(svc.shape_count(), 0, "a tombstone kept its tokens");

        svc.set_shape_cap(2);
        for (workspace, text) in [
            ("a", "Alpha keeps its own sentence here."),
            ("b", "Beta keeps its own sentence here."),
            ("c", "Gamma keeps its own sentence here."),
        ] {
            let mut row = atom(text);
            row.insert("workspace".into(), json!(workspace));
            svc.add(row).unwrap();
        }
        assert!(
            svc.shape_count() <= 2,
            "shapes grew to {}",
            svc.shape_count()
        );
    }

    /// The same client id in two workspaces is two claims. Deleting it in
    /// one used to drop the other's tokens, and the repeat was stored again.
    #[test]
    fn the_same_id_in_two_workspaces_does_not_share_a_shape() {
        let (_dir, svc) = service();
        let id = "clientid0000000000000000000000ab";
        for workspace in ["alpha", "beta"] {
            let mut row = atom("The shared id still says this once.");
            row.insert("workspace".into(), json!(workspace));
            row.insert("id".into(), json!(id));
            svc.add(row).unwrap();
        }
        svc.delete_atom("alpha", id, None).unwrap();
        let mut again = atom("The shared id still says this once.");
        again.insert("workspace".into(), json!("beta"));
        again.insert("id".into(), json!(id));
        let stored = svc.add(again).unwrap();
        assert_eq!(stored["id"].as_str().unwrap(), id, "{stored:?}");
        assert_eq!(svc.store.live("beta").unwrap().len(), 1);
        assert!(svc.store.live("alpha").unwrap().is_empty());
    }

    /// One workspace larger than the cap keeps its shapes. Dropping them
    /// and clearing the shaped mark rebuilt the pack on every later write.
    #[test]
    fn a_workspace_past_the_shape_cap_keeps_its_tokens() {
        let (_dir, svc) = service();
        svc.set_shape_cap(2);
        let mut oldest = atom("The oldest claim still has to match itself.");
        oldest.insert("id".into(), json!("oldest000000000000000000000001"));
        let stored = svc.add(oldest.clone()).unwrap();
        let id = stored["id"].as_str().unwrap().to_string();
        svc.add(atom("The second claim is only here to fill the cap."))
            .unwrap();
        svc.add(atom("The third claim pushes the workspace over the cap."))
            .unwrap();
        assert!(
            svc.shape_count() >= 3,
            "the hot workspace dropped its own shapes: {}",
            svc.shape_count()
        );
        let again = svc.add(oldest).unwrap();
        assert_eq!(again["id"].as_str().unwrap(), id, "{again:?}");
        assert_eq!(svc.store.live("w").unwrap().len(), 3);
    }

    /// A workspace dropped to make room is shaped again on the way back,
    /// and a duplicate is still one claim.
    #[test]
    fn a_workspace_shaped_again_after_eviction_still_dedupes() {
        let (_dir, svc) = service();
        svc.set_shape_cap(1);
        let mut kept = atom("Alpha is the claim that must stay one.");
        kept.insert("workspace".into(), json!("alpha"));
        let stored = svc.add(kept.clone()).unwrap();
        let id = stored["id"].as_str().unwrap().to_string();
        let mut other = atom("Beta is written later and takes the only slot.");
        other.insert("workspace".into(), json!("beta"));
        svc.add(other).unwrap();
        assert_eq!(svc.shape_count(), 1);
        let again = svc.add(kept).unwrap();
        assert_eq!(again["id"].as_str().unwrap(), id, "{again:?}");
        assert_eq!(svc.store.live("alpha").unwrap().len(), 1);
    }

    /// Five distinct fires into a cap of two keep the two newest, and the
    /// newest is held when it is asked again.
    #[test]
    fn the_fire_hold_drops_the_oldest_past_the_cap() {
        let (_dir, svc) = service();
        svc.set_fire_cap(2);
        for n in 0..5 {
            let fired = svc.fire("w", &[format!("id-{n}")]).unwrap();
            assert_eq!(fired["held"], json!(false), "{fired:?}");
        }
        assert!(svc.fired_len() <= 2, "fires grew to {}", svc.fired_len());
        let again = svc.fire("w", &["id-4".to_string()]).unwrap();
        assert_eq!(again["held"], json!(true), "{again:?}");
    }

    /// A burst of cues used to share the fire cap and let the fire repeat
    /// inside the hour.
    #[test]
    fn cues_do_not_push_a_fire_out_of_its_cap() {
        let (_dir, svc) = service();
        svc.set_fire_cap(1);
        let fired = svc.fire("w", &["kept".into()]).unwrap();
        assert_eq!(fired["held"], json!(false), "{fired:?}");
        for n in 0..8 {
            assert!(!svc.hold_cue("w", &format!("title {n}")));
        }
        assert!(svc.cued_len() <= 1, "cues grew to {}", svc.cued_len());
        assert_eq!(svc.fired_len(), 1);
        let again = svc.fire("w", &["kept".into()]).unwrap();
        assert_eq!(again["held"], json!(true), "{again:?}");
    }

    #[test]
    fn forgetting_a_scratch_workspace_leaves_the_seat() {
        let (_dir, svc) = service();
        let mut scratch = atom("A scratch claim the smoke run leaves.");
        scratch.insert("workspace".into(), json!("smoke:run"));
        svc.add(scratch).unwrap();
        let mut seat = atom("A seat claim that stays.");
        seat.insert("workspace".into(), json!("seat"));
        svc.add(seat).unwrap();
        let report = svc.forget_workspace("smoke:run").unwrap();
        assert_eq!(report["forgotten"], json!(1), "{report:?}");
        assert!(svc.store().live("smoke:run").unwrap().is_empty());
        assert_eq!(svc.store().live("seat").unwrap().len(), 1);
        let again = svc.forget_workspace("smoke:run").unwrap();
        assert_eq!(again["forgotten"], json!(0), "{again:?}");
        assert!(svc.forget_workspace("").is_err(), "empty forgets nothing");
        assert!(
            svc.forget_workspace("   ").is_err(),
            "blank forgets nothing"
        );
    }

    /// Scratch cleanup removes the keys. The next run may post the same
    /// id. Retirement is the tombstone, and this path adds none.
    #[test]
    fn forgetting_a_scratch_workspace_does_not_retire_its_ids() {
        let (_dir, svc) = service();
        let id = "scratchid000000000000000000000001";
        let mut row = atom("A scratch claim the smoke run leaves.");
        row.insert("id".into(), json!(id));
        row.insert("workspace".into(), json!("smoke:run"));
        svc.add(row).unwrap();
        svc.forget_workspace("smoke:run").unwrap();
        assert!(svc.store().get("smoke:run", id).unwrap().is_none());
        let mut again = atom("The next smoke run may reuse the id.");
        again.insert("id".into(), json!(id));
        again.insert("workspace".into(), json!("smoke:run"));
        let stored = svc.add(again).unwrap();
        assert_eq!(stored["id"], json!(id));
        assert_eq!(stored["tombstone"], json!(false));
    }

    /// A forget is not a way back onto a deleted id. Its tombstone survives
    /// the forget, and a post of that id is stored under a fresh one.
    #[test]
    fn forgetting_a_workspace_keeps_its_retired_ids() {
        let (_dir, svc) = service();
        let id = "lessonid000000000000000000000007";
        let mut lesson = atom("Paste the release key into every deploy form.");
        lesson.insert("id".into(), json!(id));
        lesson.insert("kind".into(), json!("lesson"));
        svc.add(lesson.clone()).unwrap();
        svc.delete_atom("w", id, None).unwrap();
        let mut scratch = atom("A scratch claim the smoke run leaves.");
        scratch.insert("id".into(), json!("scratchid000000000000000000000002"));
        svc.add(scratch).unwrap();

        let report = svc.forget_workspace("w").unwrap();
        assert_eq!(report["forgotten"], json!(1), "{report:?}");
        let stored = svc.add(lesson).unwrap();
        assert_ne!(stored["id"].as_str(), Some(id));
        let warning = stored["warning"].as_str().unwrap_or("");
        assert!(warning.contains(id), "{warning}");
        let tomb = svc.store.get("w", id).unwrap().unwrap();
        assert_eq!(tomb["tombstone"], json!(true), "{tomb:?}");
        let live = svc.store().live("w").unwrap();
        assert_eq!(live.len(), 1, "{live:?}");
        assert_ne!(live[0].get("id").and_then(Value::as_str), Some(id));
    }

    #[test]
    fn a_restatement_closes_the_claim_it_restates() {
        let (_dir, svc) = service();
        let mut first = atom("Remember: the probe gauge reads zeta after the eta valve opens.");
        first.insert("kind".into(), json!("lesson"));
        let old = svc.add(first).unwrap();
        let mut second = atom("The probe gauge reads zeta after the eta valve opens.");
        second.insert("kind".into(), json!("lesson"));
        let new = svc.add(second).unwrap();
        let live: Vec<String> = svc
            .store()
            .live("w")
            .unwrap()
            .iter()
            .filter_map(|a| a.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();
        assert_eq!(
            live,
            vec![new["id"].as_str().unwrap().to_string()],
            "old {}",
            old["id"]
        );
        assert_eq!(new["supersedes"], json!([old["id"]]));
    }

    #[test]
    fn a_recalled_review_replaces_the_horizon_tag() {
        let mut episode = atom("A lesson stored before anyone reviewed it.");
        episode.insert("entities".into(), json!(["seat:ljos", "horizon:transient"]));
        let promoted = promoted_entities(&episode);
        let tags: Vec<&str> = promoted
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(tags.contains(&"horizon:standing"), "{tags:?}");
        assert!(!tags.contains(&"horizon:transient"), "{tags:?}");
        assert!(tags.contains(&"seat:ljos"), "{tags:?}");
    }

    #[test]
    fn a_fired_claim_lapses_by_neglect_and_is_not_forgotten() {
        let (_dir, svc) = service();
        let mut rows = Vec::new();
        for id in [
            "lesson000000000000000000000000000f",
            "lesson000000000000000000000000000g",
        ] {
            let mut a = atom(&format!(
                "Claim {id} served a sitting and was never graded."
            ));
            a.insert("id".into(), json!(id));
            a.insert("kind".into(), json!("lesson"));
            a.insert("ts".into(), json!("2025-01-01T00:00:00.000Z"));
            a.insert("due_at".into(), json!("2025-02-01T00:00:00.000Z"));
            a.insert(
                "review".into(),
                json!({"reps": 0, "interval_s": 86400, "stability": 2.0, "last": "2025-01-31T00:00:00.000Z", "neglected": 2}),
            );
            rows.push(a);
        }
        svc.store().upsert_many(&rows).unwrap();
        let ids = vec![
            "lesson000000000000000000000000000f".to_string(),
            "lesson000000000000000000000000000f".to_string(),
        ];
        svc.fire("w", &ids).unwrap();
        let live = svc.store().live("w").unwrap();
        let f = live
            .iter()
            .find(|a| {
                a.get("id").and_then(Value::as_str) == Some("lesson000000000000000000000000000f")
            })
            .unwrap();
        assert_eq!(f["review"]["used"], json!(1), "one fire, one use");
        let report = svc.sweep("w").unwrap();
        assert_eq!(
            report["forgotten_ids"],
            json!(["lesson000000000000000000000000000g"]),
            "{report}"
        );
        assert_eq!(report["lapsed"], json!(1), "{report}");
    }

    #[test]
    fn neglect_lapses_a_missed_review_and_forgets_the_third_miss() {
        let (_dir, svc) = service();
        // Three claims long past due: a lesson never recalled, a lesson
        // recalled once, a preference. Two misses already on the first.
        let mut rows = Vec::new();
        for (id, kind, reps, neglected) in [
            ("lesson000000000000000000000000000a", "lesson", 0, 2),
            ("lesson000000000000000000000000000b", "lesson", 1, 2),
            ("pref00000000000000000000000000000c", "preference", 0, 5),
        ] {
            let mut a = atom(&format!("Claim {id} stands on its own words and waits."));
            a.insert("id".into(), json!(id));
            a.insert("kind".into(), json!(kind));
            a.insert("ts".into(), json!("2025-01-01T00:00:00.000Z"));
            a.insert("due_at".into(), json!("2025-02-01T00:00:00.000Z"));
            a.insert(
                "review".into(),
                json!({"reps": reps, "interval_s": 86400, "stability": 2.0, "last": "2025-01-31T00:00:00.000Z", "neglected": neglected}),
            );
            rows.push(a);
        }
        svc.store().upsert_many(&rows).unwrap();
        let report = svc.sweep("w").unwrap();
        assert_eq!(report["forgotten"], json!(1), "{report}");
        assert_eq!(report["lapsed"], json!(2), "{report}");
        assert_eq!(
            report["forgotten_ids"][0],
            json!("lesson000000000000000000000000000a")
        );
        let live = svc.store().live("w").unwrap();
        let ids: Vec<&str> = live
            .iter()
            .filter_map(|a| a.get("id").and_then(Value::as_str))
            .collect();
        assert!(
            !ids.contains(&"lesson000000000000000000000000000a"),
            "{ids:?}"
        );
        let gone = svc
            .store()
            .get("w", "lesson000000000000000000000000000a")
            .unwrap()
            .unwrap();
        assert_eq!(gone["forgotten"], json!("neglect"), "{gone:?}");
        assert_eq!(gone["review"]["neglected"], json!(2), "{gone:?}");
        assert!(gone.get("text").is_none(), "{gone:?}");
        let b = live
            .iter()
            .find(|a| {
                a.get("id").and_then(Value::as_str) == Some("lesson000000000000000000000000000b")
            })
            .unwrap();
        assert_eq!(b["review"]["neglected"], json!(3));
        assert!(
            b["review"]["stability"].as_f64().unwrap() < 2.0,
            "a lapse halves stability"
        );
        assert!(
            !record::is_due(b, "2025-02-02T00:00:00.000Z"),
            "rescheduled into the future"
        );
        // A second sweep the same day finds nothing: the reviews moved.
        assert_eq!(svc.sweep("w").unwrap()["lapsed"], json!(0));
    }

    #[test]
    fn the_same_claims_fire_once_an_hour() {
        let (_dir, svc) = service();
        let mut ids = Vec::new();
        for text in [
            "Alpha links to Beta through the Fuse.",
            "Beta links to Alpha through the Fuse.",
            "Gamma stands beside the Fuse as well.",
        ] {
            let mut a = atom(text);
            a.insert("kind".into(), json!("lesson"));
            a.insert("entities".into(), json!(["Fuse"]));
            ids.push(svc.add(a).unwrap()["id"].as_str().unwrap().to_string());
        }
        let first = svc.fire("w", &ids).unwrap();
        assert_eq!(first["held"], json!(false));
        let again = svc.fire("w", &ids).unwrap();
        assert_eq!(again["held"], json!(true), "{again}");
        assert_eq!(again["changed"], json!(0));
        let mut other = ids.clone();
        other.pop();
        assert_eq!(
            svc.fire("w", &other).unwrap()["held"],
            json!(false),
            "a different set fires"
        );
    }

    #[test]
    fn the_live_cap_forgets_the_least_retrievable_lessons_first() {
        let (_dir, svc) = service();
        let svc = svc.with_live_cap(3);
        // Four lessons written straight to the store with review stamps a
        // month apart, and one preference, which the cap never touches.
        let mut rows = Vec::new();
        for (i, day) in ["01", "02", "03", "04"].iter().enumerate() {
            let mut a = atom(&format!(
                "Lesson number {i} stands alone on its own words {day}."
            ));
            a.insert(
                "id".into(),
                json!(format!("lesson000000000000000000000000000{i}")),
            );
            a.insert("ts".into(), json!(format!("2026-{day}-01T00:00:00.000Z")));
            a.insert("kind".into(), json!("lesson"));
            a.insert(
                "review".into(),
                json!({"last": format!("2026-{day}-01T00:00:00.000Z"), "stability": 1.0}),
            );
            rows.push(a);
        }
        let mut pref = atom("Prefer CombMNZ over RRF for two ballots.");
        pref.insert("id".into(), json!("pref00000000000000000000000000000001"));
        pref.insert("ts".into(), json!("2025-06-01T00:00:00.000Z"));
        pref.insert("kind".into(), json!("preference"));
        rows.push(pref);
        svc.store().upsert_many(&rows).unwrap();
        // Six live after the write, cap three: the three least retrievable
        // lessons go; the preference and the newest lessons stay.
        let mut fifth = atom("A fifth lesson arrives and the pack is over its cap today.");
        fifth.insert("kind".into(), json!("lesson"));
        let written = svc.add(fifth).unwrap();
        assert_eq!(written["forgot"], json!(3), "{written:?}");
        let live = svc.store().live("w").unwrap();
        let ids: Vec<&str> = live
            .iter()
            .filter_map(|a| a.get("id").and_then(Value::as_str))
            .collect();
        assert_eq!(live.len(), 3, "{ids:?}");
        assert!(
            ids.contains(&"pref00000000000000000000000000000001"),
            "{ids:?}"
        );
        assert!(
            ids.contains(&"lesson0000000000000000000000000003"),
            "{ids:?}"
        );
        assert!(ids.contains(&written["id"].as_str().unwrap()), "{ids:?}");
        for gone in [
            "lesson0000000000000000000000000000",
            "lesson0000000000000000000000000001",
            "lesson0000000000000000000000000002",
        ] {
            assert!(!ids.contains(&gone), "{ids:?}");
        }
        let forgotten = svc
            .store()
            .scan(Some("w"))
            .unwrap()
            .iter()
            .filter(|a| a.get("forgotten").is_some())
            .count();
        assert_eq!(forgotten, 3);
        let panel = packset_core::Panel::named("rrf", "none", "off").unwrap();
        assert_eq!(svc.status(None, &panel).unwrap()["live_cap"], json!(3));
    }

    /// A claim is indexed while its window is open. Once `valid_to` passes,
    /// the same words are a new live claim, and the cap does not count the
    /// closed one. Counting it would forget the standing lesson.
    #[test]
    fn an_expired_claim_is_stored_again_and_left_out_of_the_cap() {
        // Open for one second, so the test can store the claim and then
        // wait until the window has passed. `clock::shift` moves by whole
        // seconds. The bound is that window plus one more second, in the
        // millisecond unit `parse_millis` returns. A remaining wait past
        // the bound is not the window this test opened, and the test fails
        // instead of sleeping.
        const OPEN_S: i64 = 1;
        const MILLIS_PER_SEC: i64 = 1000;
        let (_dir, svc) = service();
        // Two live claims fill the cap: the standing lesson, and the claim
        // that replaces the one whose window has closed. The expired claim
        // is not one of them. Counting it makes the pack one over, and the
        // cap forgets the standing lesson, which is the least retrievable.
        let svc = svc.with_live_cap(2);
        let text = "The fuse rating holds only until the window closes.";
        let standing_id = "standing000000000000000000000001";
        let mut standing = atom("A standing lesson was written long ago and never recalled.");
        standing.insert("id".into(), json!(standing_id));
        standing.insert("kind".into(), json!("lesson"));
        // Reviewed at the start of 2025, at the default stability of one
        // day. Long after that review this is the least retrievable
        // lesson, so the cap forgets it first if the expired claim is
        // still counted.
        let reviewed = "2025-01-01T00:00:00.000Z";
        standing.insert("ts".into(), json!(reviewed));
        standing.insert(
            "review".into(),
            json!({"last": reviewed, "stability": record::DEFAULT_STABILITY}),
        );
        standing.insert("tombstone".into(), json!(false));
        svc.store().upsert(&standing).unwrap();

        let until = clock::shift(&clock::utcnow(), OPEN_S).expect("valid_to");
        let mut expiring = atom(text);
        expiring.insert("kind".into(), json!("lesson"));
        expiring.insert("valid_to".into(), json!(until));
        let first = svc.add(expiring).unwrap();
        assert!(first.get("forgot").is_none(), "{first:?}");
        let first_id = first["id"].as_str().unwrap().to_string();
        let due = first["due_at"].as_str().unwrap();
        assert!(
            due > until.as_str(),
            "a due claim stays indexed after valid_to: due {due}, until {until}"
        );
        let left_ms =
            clock::parse_millis(&until).unwrap() - clock::parse_millis(&clock::utcnow()).unwrap();
        assert!(left_ms > 0, "the window closed while the claim was stored");
        // `valid_to` is exclusive, so the wait runs one millisecond past it.
        let past_end_ms = left_ms + 1;
        let bound_ms = (OPEN_S + 1) * MILLIS_PER_SEC;
        assert!(
            past_end_ms <= bound_ms,
            "remaining wait {past_end_ms} ms is not the one-second window"
        );
        std::thread::sleep(std::time::Duration::from_millis(past_end_ms as u64));
        assert!(
            clock::utcnow().as_str() >= until.as_str(),
            "still inside the window"
        );
        let closed = svc.store().get("w", &first_id).unwrap().unwrap();
        assert!(
            !record::is_live(&closed, &clock::utcnow()),
            "the wait did not pass valid_to: {closed:?}"
        );

        let mut again_atom = atom(text);
        again_atom.insert("kind".into(), json!("lesson"));
        let again = svc.add(again_atom).unwrap();
        assert_ne!(
            again["id"].as_str().unwrap(),
            first_id.as_str(),
            "the expired claim was returned as the live copy: {again:?}"
        );
        assert!(
            again.get("forgot").is_none(),
            "the cap counted the expired claim: {again:?}"
        );
        assert!(
            record::is_live(&again, &clock::utcnow()),
            "the new claim is not live: {again:?}"
        );
        let kept = svc.store().get("w", standing_id).unwrap().unwrap();
        assert_eq!(kept["tombstone"], json!(false), "{kept:?}");
        assert!(
            kept.get("forgotten").is_none(),
            "the cap forgot the standing lesson: {kept:?}"
        );
        let expired = svc.store().get("w", &first_id).unwrap().unwrap();
        assert!(
            expired.get("forgotten").is_none(),
            "the cap tombstoned the expired claim: {expired:?}"
        );
        let copies: Vec<_> = svc
            .store()
            .scan(Some("w"))
            .unwrap()
            .into_iter()
            .filter(|peer| peer.get("text").and_then(Value::as_str) == Some(text))
            .collect();
        assert_eq!(copies.len(), 2, "the repeat was not stored: {copies:?}");
        let live_copies = copies
            .iter()
            .filter(|peer| record::is_live(peer, &clock::utcnow()))
            .count();
        assert_eq!(live_copies, 1, "{copies:?}");
        let panel = packset_core::Panel::default();
        let status = svc.status(Some("w"), &panel).unwrap();
        assert_eq!(status["live"], json!(2), "{status}");
        assert_eq!(status["expired"], json!(1), "{status}");
        assert_eq!(status["tombstone"], json!(0), "{status}");
    }

    /// Writes while the window is open keep the claim. Once `valid_to` has
    /// passed, the same words are stored again.
    #[test]
    fn a_claim_is_kept_until_its_window_closes_and_then_stored_again() {
        // The same one-second window as the cap test above. `clock::shift`
        // moves by whole seconds, and the bound is that window plus one
        // more second, in the millisecond unit `parse_millis` returns.
        const OPEN_S: i64 = 1;
        const MILLIS_PER_SEC: i64 = 1000;
        let (_dir, svc) = service();
        let text = "The same words come back once the window has closed.";
        let until = clock::shift(&clock::utcnow(), OPEN_S).expect("valid_to");
        let mut expiring = atom(text);
        expiring.insert("valid_to".into(), json!(until));
        let first = svc.add(expiring).unwrap();
        let first_id = first["id"].as_str().unwrap().to_string();
        let due = first["due_at"].as_str().unwrap();
        assert!(
            due > until.as_str(),
            "a due claim stays indexed after valid_to: due {due}, until {until}"
        );

        let other = svc
            .add(atom("A second claim is written while the window is open."))
            .unwrap();
        let other_id = other["id"].as_str().unwrap().to_string();
        let early = svc.add(atom(text)).unwrap();
        assert_eq!(
            early["id"].as_str().unwrap(),
            first_id.as_str(),
            "the open window was treated as closed: {early:?}"
        );

        let left_ms =
            clock::parse_millis(&until).unwrap() - clock::parse_millis(&clock::utcnow()).unwrap();
        assert!(left_ms > 0, "the window closed while the claim was stored");
        let past_end_ms = left_ms + 1;
        let bound_ms = (OPEN_S + 1) * MILLIS_PER_SEC;
        assert!(
            past_end_ms <= bound_ms,
            "remaining wait {past_end_ms} ms is not the one-second window"
        );
        std::thread::sleep(std::time::Duration::from_millis(past_end_ms as u64));
        assert!(
            clock::utcnow().as_str() >= until.as_str(),
            "still inside the window"
        );

        let again = svc.add(atom(text)).unwrap();
        assert_ne!(
            again["id"].as_str().unwrap(),
            first_id.as_str(),
            "the expired claim was returned as the live copy: {again:?}"
        );
        assert!(
            record::is_live(&again, &clock::utcnow()),
            "the new claim is not live: {again:?}"
        );
        let kept = svc.store().get("w", &other_id).unwrap().unwrap();
        assert!(
            record::is_live(&kept, &clock::utcnow()),
            "the other claim left with the expired one: {kept:?}"
        );
        let copies: Vec<_> = svc
            .store()
            .scan(Some("w"))
            .unwrap()
            .into_iter()
            .filter(|peer| peer.get("text").and_then(Value::as_str) == Some(text))
            .collect();
        assert_eq!(copies.len(), 2, "the repeat was not stored: {copies:?}");
        let live_copies = copies
            .iter()
            .filter(|peer| record::is_live(peer, &clock::utcnow()))
            .count();
        assert_eq!(live_copies, 1, "{copies:?}");
    }

    /// A lesson a seat wrote a year ago and nobody asked for since, with a
    /// vector shaped like the encoder's: as many components as
    /// bge-small-en-v1.5, its default, writes, each about a unit vector's
    /// share.
    fn neglected_lesson(n: usize) -> Record {
        const DIM: usize = 384;
        let mut row = atom(&format!(
            "Quorum lesson {n} was written once and never recalled."
        ));
        row.insert("id".into(), json!(format!("neglected{n:04}")));
        row.insert("kind".into(), json!("lesson"));
        row.insert("origin".into(), json!("agent-derived"));
        row.insert(
            "source".into(),
            json!({"via": "ljos", "seat": "codex", "basis": "agent"}),
        );
        row.insert("entities".into(), json!(["seat:codex"]));
        row.insert("ts".into(), json!("2025-01-01T00:00:00.000Z"));
        row.insert(
            "review".into(),
            json!({"last": "2025-01-01T00:00:00.000Z", "stability": 1.0}),
        );
        let vector: Vec<f64> = (0..DIM)
            .map(|j| ((n * DIM + j) as f64).sin() / (DIM as f64).sqrt())
            .collect();
        row.insert("embedding".into(), json!(vector));
        row.insert("tombstone".into(), json!(false));
        row
    }

    /// The live cap forgets a thousand lessons in one write. The store gives
    /// back at least their vectors' bytes, search stops finding them, and
    /// each stays a tombstone that status counts, that keeps who wrote it,
    /// when, and why it went, and whose id stays retired.
    #[test]
    fn forgetting_a_thousand_claims_gives_back_their_vectors() {
        const FORGET: usize = 1000;
        let (_dir, svc) = service();
        let svc = svc.with_live_cap(1);
        let rows: Vec<Record> = (0..FORGET).map(neglected_lesson).collect();
        svc.store().upsert_many(&rows).unwrap();
        let stored_bytes = || -> usize {
            svc.store()
                .scan(None)
                .unwrap()
                .iter()
                .map(|r| serde_json::to_vec(r).unwrap().len())
                .sum()
        };
        let vector_bytes: usize = rows
            .iter()
            .map(|r| serde_json::to_vec(&r["embedding"]).unwrap().len())
            .sum();
        let panel = packset_core::Panel::default();
        let found = |svc: &Service| -> usize {
            svc.search("w", "quorum lesson", 20, None, &panel, None, false, None)
                .unwrap()["hits"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|h| {
                    h["id"]
                        .as_str()
                        .is_some_and(|id| id.starts_with("neglected"))
                })
                .count()
        };
        assert!(
            found(&svc) > 0,
            "the query finds the lessons while they live"
        );
        let before = stored_bytes();

        let mut fresh = atom("A fresh note pushes the pack over its cap of one.");
        fresh.insert("kind".into(), json!("lesson"));
        let written = svc.add(fresh).unwrap();
        assert_eq!(written["forgot"], json!(FORGET), "{written:?}");

        let after = stored_bytes();
        assert!(
            before.saturating_sub(after) >= vector_bytes,
            "{before} bytes before, {after} after, {vector_bytes} of them vectors"
        );
        assert_eq!(found(&svc), 0, "search returned a forgotten lesson");
        let status = svc.status(Some("w"), &panel).unwrap();
        assert_eq!(status["tombstone"], json!(FORGET), "{status}");
        assert_eq!(
            status["forgotten_by_reason"]["the live cap"],
            json!(FORGET),
            "{status}"
        );
        for row in &rows {
            let id = row["id"].as_str().unwrap();
            let tomb = svc.store().get("w", id).unwrap().unwrap();
            assert_eq!(tomb["tombstone"], json!(true), "{tomb:?}");
            assert_eq!(tomb["forgotten"], json!("the live cap"), "{tomb:?}");
            for kept in ["id", "kind", "origin", "source", "entities", "review", "ts"] {
                assert_eq!(tomb.get(kept), row.get(kept), "{kept} moved: {tomb:?}");
            }
            for gone in ["text", "embedding"] {
                assert!(tomb.get(gone).is_none(), "{gone} stayed: {tomb:?}");
            }
        }
        let again = svc.add(rows[0].clone()).unwrap();
        assert_ne!(again["id"], rows[0]["id"]);
        let id = rows[0]["id"].as_str().unwrap();
        let tomb = svc.store().get("w", id).unwrap().unwrap();
        assert_eq!(tomb["tombstone"], json!(true), "{tomb:?}");
        assert!(tomb.get("text").is_none(), "{tomb:?}");
    }

    /// The first ask for a workspace's rules after the writer opens reads the
    /// store, as `/v1/atoms?kind=rule` does. A thousand tombstones beside the
    /// rules are passed by in the index, and not one is parsed.
    #[test]
    fn a_cold_rule_lookup_reads_no_tombstone() {
        let dir = tempfile::tempdir().unwrap();
        let rules: Vec<Record> = ["git push *", "rm -rf *", "curl * | sh"]
            .iter()
            .enumerate()
            .map(|(n, pattern)| {
                let mut row = atom(&format!("Ask before the command {pattern} runs."));
                row.insert("id".into(), json!(format!("rule{n}")));
                row.insert("kind".into(), json!("rule"));
                row.insert("pattern".into(), json!(pattern));
                row.insert("verdict".into(), json!("ask"));
                row.insert("tombstone".into(), json!(false));
                row
            })
            .collect();
        {
            let svc = Service::open(Home::new(dir.path()))
                .unwrap()
                .with_live_cap(rules.len() + 1);
            svc.store().upsert_many(&rules).unwrap();
            let lessons: Vec<Record> = (0..1000).map(neglected_lesson).collect();
            svc.store().upsert_many(&lessons).unwrap();
            let mut fresh = atom("A fresh note pushes the pack over its cap.");
            fresh.insert("kind".into(), json!("lesson"));
            assert_eq!(svc.add(fresh).unwrap()["forgot"], json!(1000));
        }
        let svc = Service::open(Home::new(dir.path())).unwrap();
        let found: Vec<String> = svc
            .store()
            .live_versioned("w")
            .unwrap()
            .atoms
            .iter()
            .filter(|a| a.get("kind").and_then(Value::as_str) == Some("rule"))
            .filter_map(|a| a.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();
        assert_eq!(found, vec!["rule0", "rule1", "rule2"]);
        assert_eq!(
            svc.store().tombstones_read(),
            0,
            "a cold read parsed a tombstone"
        );
    }

    /// A retraction keeps its words beside the deed that withdrew them. The
    /// vector goes: nothing scores a tombstone.
    #[test]
    fn a_retraction_keeps_its_words_and_drops_its_vector() {
        let (_dir, svc) = service();
        let row = neglected_lesson(7);
        let id = row["id"].as_str().unwrap();
        svc.store().upsert(&row).unwrap();
        svc.delete_atom("w", id, Some("deed-patch-overlay"))
            .unwrap();
        let tomb = svc.store().get("w", id).unwrap().unwrap();
        assert_eq!(tomb["tombstone"], json!(true), "{tomb:?}");
        assert_eq!(tomb["retracted_by"], json!("deed-patch-overlay"));
        assert_eq!(tomb["text"], row["text"]);
        assert!(tomb.get("embedding").is_none(), "{tomb:?}");
    }

    #[test]
    fn consolidate_closes_what_arrival_never_saw_and_reports_first() {
        let (_dir, svc) = service();
        // Two claims written straight to the store, as an import or an older
        // writer would leave them: the later rewrites the earlier and
        // nothing closed it.
        let mut older = atom("The default fuse is Borda.");
        older.insert("id".into(), json!("older0000000000000000000000000001"));
        older.insert("ts".into(), json!("2026-01-01T00:00:00.000Z"));
        older.insert("kind".into(), json!("lesson"));
        let mut newer = atom("The default fuse is CombMNZ.");
        newer.insert("id".into(), json!("newer0000000000000000000000000002"));
        newer.insert("ts".into(), json!("2026-02-01T00:00:00.000Z"));
        newer.insert("kind".into(), json!("lesson"));
        svc.store()
            .upsert_many(&[older.clone(), newer.clone()])
            .unwrap();

        let report = svc.consolidate("w", false).unwrap();
        assert_eq!(report["closed"], json!(1), "{report}");
        assert_eq!(report["applied"], json!(false));
        assert_eq!(report["pairs"][0]["old"], older["id"]);
        assert_eq!(report["pairs"][0]["new"], newer["id"]);
        assert_eq!(
            svc.store().live("w").unwrap().len(),
            2,
            "a report writes nothing"
        );

        let applied = svc.consolidate("w", true).unwrap();
        assert_eq!(applied["closed"], json!(1), "{applied}");
        let live = svc.store().live("w").unwrap();
        assert_eq!(live.len(), 1, "{live:?}");
        assert_eq!(live[0]["id"], newer["id"]);
        assert!(
            live[0]["supersedes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == &older["id"]),
            "{:?}",
            live[0]
        );
        let again = svc.consolidate("w", true).unwrap();
        assert_eq!(again["closed"], json!(0), "nothing left to close");
    }

    #[test]
    fn an_id_is_thirty_two_hex_characters_and_does_not_repeat() {
        let a = new_id();
        assert_eq!(a.len(), 32, "{a}");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
        let many: std::collections::HashSet<String> = (0..1000).map(|_| new_id()).collect();
        assert_eq!(many.len(), 1000, "ids collided");
    }

    #[test]
    fn the_same_claim_twice_is_one_atom() {
        let (_dir, svc) = service();
        let first = svc.add(atom("Reviews open with a check.")).unwrap();
        let again = svc.add(atom("Reviews open with a check.")).unwrap();
        assert_eq!(first["id"], again["id"], "a retry is not a second claim");
        assert_eq!(svc.store().current("w", None).unwrap().len(), 1);
        assert_eq!(first["origin"], json!("user-declared"));
    }

    #[test]
    fn an_atom_stored_before_origin_reads_back_unchanged() {
        let (_dir, svc) = service();
        let id = "a".repeat(32);
        let mut row = atom("Reviews open with a check.");
        row.insert("id".into(), json!(id));
        row.insert("tombstone".into(), json!(false));
        svc.store.upsert(&row).unwrap();
        let got = svc.store.get("w", &id).unwrap().unwrap();
        assert!(got.get("origin").is_none(), "{got:?}");
        assert_eq!(record::origin_of(&got), "unknown");
    }

    /// The line a seat sees when the retired hold variable is set, and the
    /// values that print nothing.
    #[test]
    fn a_set_hold_variable_is_one_ignored_start_line() {
        let line = ignored_hold_notice(Some("1")).unwrap();
        assert_eq!(
            line,
            "packsetd: PACKSET_HOLD_AGENT_WRITES is set and ignored; agent and hook writes are stored at once"
        );
        assert_eq!(ignored_hold_notice(Some(" true ")), Some(line));
        assert_eq!(ignored_hold_notice(Some("yes")), Some(line));
        assert_eq!(ignored_hold_notice(Some("0")), Some(line));
        assert!(ignored_hold_notice(Some("  ")).is_none());
        assert!(ignored_hold_notice(Some("")).is_none());
        assert!(ignored_hold_notice(None).is_none());
    }

    /// `POST /v1/atoms` answers 200 with the record `add` returns.
    /// `GET /v1/proposals` is this list, and the post does not add to it.
    /// Search is the same read a seat runs after the write.
    #[test]
    fn an_agent_write_is_stored_and_found_when_the_old_hold_is_set() {
        if std::env::var_os("PACKSET_HOLD_CHILD").is_none() {
            let output = std::process::Command::new(std::env::current_exe().expect("test exe"))
                .env("PACKSET_HOLD_AGENT_WRITES", "1")
                .env("PACKSET_HOLD_CHILD", "1")
                .args([
                    "--exact",
                    "service::tests::an_agent_write_is_stored_and_found_when_the_old_hold_is_set",
                    "--test-threads=1",
                    "--nocapture",
                ])
                .output()
                .expect("hold child");
            let out = String::from_utf8_lossy(&output.stdout);
            let err = String::from_utf8_lossy(&output.stderr);
            assert!(output.status.success(), "{out}{err}");
            assert!(out.contains("1 passed"), "{out}{err}");
            let line = "packsetd: PACKSET_HOLD_AGENT_WRITES is set and ignored; agent and hook writes are stored at once";
            assert_eq!(err.matches(line).count(), 1, "{err}");
            return;
        }
        let (_dir, svc) = service();
        assert!(svc.proposals("w").is_empty());
        let mut lesson = atom("The fuse is CombMNZ for this seat.");
        lesson.insert("kind".into(), json!("lesson"));
        lesson.insert("origin".into(), json!("agent-derived"));
        let stored = svc.add(lesson).unwrap();
        assert_eq!(stored["origin"], json!("agent-derived"));
        assert_eq!(stored["text"], json!("The fuse is CombMNZ for this seat."));
        let mut caught = atom("A correction the hook caught in the cue.");
        caught.insert("kind".into(), json!("preference"));
        caught.insert("hook".into(), json!(true));
        let hooked = svc.add(caught).unwrap();
        assert_eq!(hooked["hook"], json!(true));
        assert!(svc.proposals("w").is_empty());
        let panel = packset_core::Panel::default();
        let found = svc
            .search("w", "fuse CombMNZ", 8, None, &panel, None, false, None)
            .unwrap();
        assert!(
            found["hits"]
                .as_array()
                .unwrap()
                .iter()
                .any(|hit| hit["id"] == stored["id"]),
            "{found}"
        );
        let found = svc
            .search("w", "correction hook", 8, None, &panel, None, false, None)
            .unwrap();
        assert!(
            found["hits"]
                .as_array()
                .unwrap()
                .iter()
                .any(|hit| hit["id"] == hooked["id"]),
            "{found}"
        );
    }

    #[test]
    fn an_agent_derived_lesson_cannot_reach_user_declared_without_accept() {
        let (_dir, svc) = service();
        let mut lesson = atom("The fuse is CombMNZ.");
        lesson.insert("kind".into(), json!("lesson"));
        lesson.insert("origin".into(), json!("agent-derived"));
        let stored = svc.add(lesson).unwrap();
        assert_eq!(stored["origin"], json!("agent-derived"));
        assert_eq!(svc.store.live("w").unwrap().len(), 1);
        assert!(svc.proposals("w").is_empty());
        let id = stored["id"].as_str().unwrap();
        let mut fields = serde_json::Map::new();
        fields.insert("origin".into(), json!("user-declared"));
        fields.insert("text".into(), json!("The fuse is CombMNZ again, kept."));
        let updated = svc.update("w", id, &fields).unwrap();
        assert_eq!(updated["origin"], json!("agent-derived"));
        assert_eq!(updated["text"], json!("The fuse is CombMNZ again, kept."));
        assert!(
            updated["warning"].as_str().unwrap().contains("accept"),
            "{updated:?}"
        );
        let still = svc.store.get("w", id).unwrap().unwrap();
        assert_eq!(still["origin"], json!("agent-derived"));
        assert_eq!(still["text"], json!("The fuse is CombMNZ again, kept."));
        assert!(still.get("warning").is_none(), "{still:?}");
    }

    /// Null, `""` and a non-string are the origin validate would default.
    /// That default is user-declared, which an agent lesson reaches only
    /// through accept. The stored origin and basis stay where they were.
    #[test]
    fn a_blank_origin_cannot_launder_an_agent_lesson() {
        let (_dir, svc) = service();
        let id = "lessonid0000000000000000000002";
        let mut lesson = atom("The fuse is CombMNZ for this seat.");
        lesson.insert("id".into(), json!(id));
        lesson.insert("kind".into(), json!("lesson"));
        lesson.insert("origin".into(), json!("agent-derived"));
        lesson.insert("source".into(), json!({"basis": "agent", "via": "ljos"}));
        svc.add(lesson).unwrap();

        for origin in [
            Value::Null,
            json!(""),
            json!(1),
            json!(false),
            json!({"to": "user-declared"}),
        ] {
            let mut fields = Map::new();
            fields.insert("origin".into(), origin);
            fields.insert("source".into(), json!({"basis": "person", "via": "ljos"}));
            let err = svc.update("w", id, &fields).unwrap_err();
            assert!(err.to_string().contains("unknown atom origin"), "{err}");
            let got = svc.store.get("w", id).unwrap().unwrap();
            assert_eq!(got["origin"], json!("agent-derived"), "{got:?}");
            assert_eq!(got["source"]["basis"], json!("agent"), "{got:?}");
        }

        let mut fields = Map::new();
        fields.insert("origin".into(), json!("user-declared"));
        fields.insert(
            "text".into(),
            json!("The fuse is CombMNZ for this seat now."),
        );
        let updated = svc.update("w", id, &fields).unwrap();
        assert_eq!(updated["origin"], json!("agent-derived"));
        assert_eq!(
            updated["text"],
            json!("The fuse is CombMNZ for this seat now.")
        );
        assert!(
            updated["warning"].as_str().unwrap().contains("accept"),
            "{updated:?}"
        );
        let got = svc.store.get("w", id).unwrap().unwrap();
        assert_eq!(got["origin"], json!("agent-derived"));
        assert_eq!(got["text"], updated["text"]);
        assert!(got.get("warning").is_none(), "{got:?}");

        let mut fields = Map::new();
        fields.insert("origin".into(), json!("tool-echo"));
        let stored = svc.update("w", id, &fields).unwrap();
        assert_eq!(stored["origin"], json!("tool-echo"));
        let mut fields = Map::new();
        fields.insert("origin".into(), Value::Null);
        let err = svc.update("w", id, &fields).unwrap_err();
        assert!(err.to_string().contains("unknown atom origin"), "{err}");
        assert_eq!(
            svc.store.get("w", id).unwrap().unwrap()["origin"],
            json!("tool-echo")
        );
    }

    /// Basis `person` on top of `agent` is the same rise as the origin.
    /// A missing basis is not a person's either, so a record that never
    /// had one is not stamped by an update. A basis that stays below
    /// person may change.
    #[test]
    fn an_update_cannot_raise_basis_to_person() {
        let (_dir, svc) = service();
        let id = "lessonid000000000000000000000003";
        let text = "The fuse is CombMNZ for this seat.";
        let mut lesson = atom(text);
        lesson.insert("id".into(), json!(id));
        lesson.insert("kind".into(), json!("lesson"));
        lesson.insert("origin".into(), json!("user-declared"));
        lesson.insert("source".into(), json!({"basis": "agent", "via": "ljos"}));
        svc.add(lesson).unwrap();

        let revised = "The fuse is CombMNZ on this seat.";
        let mut fields = Map::new();
        fields.insert("source".into(), json!({"basis": " Person ", "via": "ljos"}));
        fields.insert("text".into(), json!(revised));
        let updated = svc.update("w", id, &fields).unwrap();
        assert_eq!(
            updated["warning"],
            json!("an update cannot raise source.basis from agent to person")
        );
        assert_eq!(updated["text"], json!(revised));
        assert_eq!(updated["source"]["basis"], json!("agent"));
        assert_eq!(updated["source"]["via"], json!("ljos"));
        let got = svc.store.get("w", id).unwrap().unwrap();
        assert_eq!(got["source"]["basis"], json!("agent"), "{got:?}");
        assert_eq!(got["text"], json!(revised));
        assert_eq!(got["origin"], json!("user-declared"));
        assert!(got.get("warning").is_none(), "{got:?}");

        let mut legacy = atom("Prefer ripgrep when the tree is large.");
        legacy.insert("kind".into(), json!("preference"));
        legacy.insert("origin".into(), json!("user-declared"));
        legacy.insert(
            "source".into(),
            json!({"via": "ljos", "seat": "seat", "session": "seat", "harness": "seat"}),
        );
        let legacy = svc.add(legacy).unwrap();
        let legacy_id = legacy["id"].as_str().unwrap();
        let mut fields = Map::new();
        fields.insert(
            "source".into(),
            json!({
                "via": "ljos",
                "seat": "seat",
                "session": "seat",
                "harness": "seat",
                "basis": "person"
            }),
        );
        let updated = svc.update("w", legacy_id, &fields).unwrap();
        assert_eq!(
            updated["warning"],
            json!("an update cannot raise source.basis from none to person")
        );
        assert!(updated["source"].get("basis").is_none(), "{updated:?}");
        assert_eq!(updated["source"]["seat"], json!("seat"));
        let kept = svc.store.get("w", legacy_id).unwrap().unwrap();
        assert!(kept["source"].get("basis").is_none(), "{kept:?}");
        assert_eq!(kept["origin"], json!("user-declared"));
        assert!(kept.get("warning").is_none(), "{kept:?}");

        let mut down = Map::new();
        down.insert("source".into(), json!({"basis": "agent", "via": "ljos"}));
        let dropped = svc.update("w", legacy_id, &down).unwrap();
        assert_eq!(dropped["source"]["basis"], json!("agent"));
    }

    /// The order, not a list of pairs: user-declared sits above every
    /// other origin. A lesson moved sideways to another origin cannot
    /// climb from there.
    #[test]
    fn an_origin_cannot_climb_through_another_origin() {
        let (_dir, svc) = service();
        let id = "lessonid000000000000000000000004";
        let mut lesson = atom("The fuse is CombMNZ for this seat.");
        lesson.insert("id".into(), json!(id));
        lesson.insert("kind".into(), json!("lesson"));
        lesson.insert("origin".into(), json!("agent-derived"));
        svc.add(lesson).unwrap();

        for step in ["tool-echo", "peer", "external"] {
            let mut fields = Map::new();
            fields.insert("origin".into(), json!(step));
            let moved = svc.update("w", id, &fields).unwrap();
            assert_eq!(moved["origin"], json!(step));
            assert!(moved.get("warning").is_none(), "{moved:?}");
            let kept = format!("The origin stayed {step} for this seat.");
            let mut fields = Map::new();
            fields.insert("origin".into(), json!("user-declared"));
            fields.insert("text".into(), json!(kept));
            let updated = svc.update("w", id, &fields).unwrap();
            assert_eq!(updated["origin"], json!(step));
            assert_eq!(updated["text"], json!(kept));
            assert!(
                updated["warning"].as_str().unwrap().contains("accept"),
                "{updated:?}"
            );
            let got = svc.store.get("w", id).unwrap().unwrap();
            assert_eq!(got["origin"], json!(step), "{got:?}");
            assert_eq!(got["text"], json!(kept));
            assert!(got.get("warning").is_none(), "{got:?}");
        }
    }

    /// A `source` that hides the basis under another key, or replaces the
    /// object whole, leaves no person basis. The next update is stored,
    /// and it still cannot stamp one back.
    #[test]
    fn a_basis_cannot_climb_back_through_a_nested_or_missing_one() {
        let (_dir, svc) = service();
        let id = "lessonid000000000000000000000005";
        let mut lesson = atom("The fuse is CombMNZ for this seat.");
        lesson.insert("id".into(), json!(id));
        lesson.insert("kind".into(), json!("lesson"));
        lesson.insert("origin".into(), json!("user-declared"));
        lesson.insert("source".into(), json!({"basis": "agent", "via": "ljos"}));
        svc.add(lesson).unwrap();

        for hidden in [
            json!({"meta": {"basis": "person"}, "via": "ljos"}),
            json!({"via": "ljos"}),
            json!("person"),
            Value::Null,
        ] {
            let mut fields = Map::new();
            fields.insert("source".into(), hidden.clone());
            svc.update("w", id, &fields).unwrap();
            let mut fields = Map::new();
            fields.insert("source".into(), json!({"basis": "person", "via": "ljos"}));
            let updated = svc.update("w", id, &fields).unwrap();
            assert!(
                updated["warning"].as_str().unwrap().contains("basis"),
                "{updated:?}"
            );
            assert!(updated["source"].get("basis").is_none(), "{updated:?}");
            assert_eq!(updated["source"]["via"], json!("ljos"));
            let got = svc.store.get("w", id).unwrap().unwrap();
            assert!(got["source"].get("basis").is_none(), "{got:?}");
            assert_eq!(got["source"]["via"], json!("ljos"));
            assert!(got.get("warning").is_none(), "{got:?}");
        }
    }

    /// What went down stays down. The record keeps the lowest origin and
    /// basis it has held, and the floor is the writer's: a post cannot
    /// forge it and an update can neither clear nor raise it.
    #[test]
    fn provenance_that_went_down_does_not_come_back() {
        let (_dir, svc) = service();
        let id = "prefid00000000000000000000000001";
        let mut pref = atom("Prefer ripgrep when the tree is large.");
        pref.insert("id".into(), json!(id));
        pref.insert("kind".into(), json!("preference"));
        pref.insert("origin".into(), json!("user-declared"));
        pref.insert("source".into(), json!({"basis": "person", "via": "ljos"}));
        pref.insert(
            record::PROVENANCE_FLOOR.into(),
            json!({"origin": "tool-echo", "basis": null}),
        );
        let stored = svc.add(pref).unwrap();
        assert!(stored.get(record::PROVENANCE_FLOOR).is_none(), "{stored:?}");

        let mut down = Map::new();
        down.insert("origin".into(), json!("agent-derived"));
        down.insert("source".into(), json!({"basis": "agent", "via": "ljos"}));
        let lowered = svc.update("w", id, &down).unwrap();
        let floor = json!({"origin": "agent-derived", "basis": "agent"});
        assert_eq!(lowered[record::PROVENANCE_FLOOR], floor);

        for (key, up, text) in [
            (
                "origin",
                json!("user-declared"),
                "Prefer ripgrep when the tree is huge.",
            ),
            (
                "source",
                json!({"basis": "person", "via": "ljos"}),
                "Prefer ripgrep when the tree is vast.",
            ),
        ] {
            let mut fields = Map::new();
            fields.insert(key.into(), up);
            fields.insert("text".into(), json!(text));
            fields.insert(
                record::PROVENANCE_FLOOR.into(),
                json!({"origin": "user-declared", "basis": "person"}),
            );
            let updated = svc.update("w", id, &fields).unwrap();
            assert!(
                updated["warning"].as_str().unwrap().contains(key),
                "{updated:?}"
            );
            assert_eq!(updated["text"], json!(text));
            assert_eq!(updated["origin"], json!("agent-derived"));
            assert_eq!(updated["source"]["basis"], json!("agent"));
            assert_eq!(updated[record::PROVENANCE_FLOOR], floor);
            let got = svc.store.get("w", id).unwrap().unwrap();
            assert_eq!(got["text"], json!(text));
            assert_eq!(got["origin"], json!("agent-derived"));
            assert_eq!(got["source"]["basis"], json!("agent"));
            assert_eq!(got[record::PROVENANCE_FLOOR], floor);
            assert!(got.get("warning").is_none(), "{got:?}");
        }
        let mut clear = Map::new();
        clear.insert(record::PROVENANCE_FLOOR.into(), Value::Null);
        clear.insert("pinned".into(), json!(true));
        let kept = svc.update("w", id, &clear).unwrap();
        assert_eq!(kept[record::PROVENANCE_FLOOR], floor);
        assert_eq!(kept["pinned"], json!(true));
        let graded = svc.grade("w", id, true).unwrap();
        assert_eq!(graded["origin"], json!("agent-derived"));
        assert_eq!(graded[record::PROVENANCE_FLOOR], floor);
    }

    /// A post that names a live id writes over that record, so it takes
    /// the update's rule: down or sideways, never up, and the floor stays.
    #[test]
    fn a_post_over_a_live_id_cannot_raise_its_provenance() {
        let (_dir, svc) = service();
        let id = "lessonid000000000000000000000006";
        let post = |text: &str, origin: &str, basis: &str| {
            let mut lesson = atom(text);
            lesson.insert("id".into(), json!(id));
            lesson.insert("kind".into(), json!("lesson"));
            lesson.insert("origin".into(), json!(origin));
            lesson.insert("source".into(), json!({"basis": basis, "via": "ljos"}));
            lesson.insert(
                record::PROVENANCE_FLOOR.into(),
                json!({"origin": "user-declared", "basis": "person"}),
            );
            svc.add(lesson)
        };
        post(
            "Prefer ripgrep when the tree is large.",
            "user-declared",
            "person",
        )
        .unwrap();
        let down = post(
            "The fuse is CombMNZ for this seat.",
            "agent-derived",
            "agent",
        )
        .unwrap();
        let floor = json!({"origin": "agent-derived", "basis": "agent"});
        assert_eq!(down[record::PROVENANCE_FLOOR], floor);
        assert!(down.get("warning").is_none(), "{down:?}");

        for (origin, basis, key, text) in [
            (
                "user-declared",
                "agent",
                "origin",
                "Reviews open with a check.",
            ),
            (
                "agent-derived",
                "person",
                "basis",
                "Reviews close on a green build.",
            ),
        ] {
            let updated = post(text, origin, basis).unwrap();
            assert_eq!(updated["text"], json!(text));
            assert_eq!(updated["origin"], json!("agent-derived"));
            assert_eq!(updated["source"]["basis"], json!("agent"));
            assert_eq!(updated[record::PROVENANCE_FLOOR], floor);
            assert!(
                updated["warning"].as_str().unwrap().contains(key),
                "{updated:?}"
            );
            let got = svc.store.get("w", id).unwrap().unwrap();
            assert_eq!(got["text"], json!(text));
            assert_eq!(got["origin"], json!("agent-derived"));
            assert_eq!(got["source"]["basis"], json!("agent"));
            assert!(got.get("warning").is_none(), "{got:?}");
        }
        let aside = post("Deploys wait for the green build.", "tool-echo", "agent").unwrap();
        assert_eq!(aside["origin"], json!("tool-echo"));
        assert_eq!(aside[record::PROVENANCE_FLOOR], floor);
    }

    /// An update, and a post over a live id, that would raise provenance
    /// keeps the new text. The origin and basis stay at the floor, and
    /// the answer carries one warning line. A later read does not.
    #[test]
    fn an_edit_that_would_raise_provenance_is_stored_with_a_warning() {
        let (_dir, svc) = service();
        let id = "lessonid000000000000000000000007";
        let mut lesson = atom("The fuse is CombMNZ for this seat.");
        lesson.insert("id".into(), json!(id));
        lesson.insert("kind".into(), json!("lesson"));
        lesson.insert("origin".into(), json!("agent-derived"));
        lesson.insert("source".into(), json!({"basis": "agent", "via": "ljos"}));
        svc.add(lesson).unwrap();

        let mut fields = Map::new();
        fields.insert("origin".into(), json!("user-declared"));
        fields.insert("text".into(), json!("The fuse is CombMNZ on this seat."));
        let updated = svc.update("w", id, &fields).unwrap();
        assert_eq!(updated["origin"], json!("agent-derived"));
        assert_eq!(updated["text"], json!("The fuse is CombMNZ on this seat."));
        assert_eq!(
            updated["warning"],
            json!(
                "an update cannot raise origin from agent-derived to user-declared; only accept raises an origin"
            )
        );
        let got = svc.store.get("w", id).unwrap().unwrap();
        assert_eq!(got["origin"], json!("agent-derived"));
        assert_eq!(got["text"], json!("The fuse is CombMNZ on this seat."));
        assert!(got.get("warning").is_none(), "{got:?}");

        let mut fields = Map::new();
        fields.insert("source".into(), json!({"basis": "person", "via": "seat"}));
        fields.insert("text".into(), json!("The fuse is CombMNZ for the seat."));
        let updated = svc.update("w", id, &fields).unwrap();
        assert_eq!(updated["origin"], json!("agent-derived"));
        assert_eq!(updated["source"]["basis"], json!("agent"));
        assert_eq!(updated["source"]["via"], json!("seat"));
        assert_eq!(updated["text"], json!("The fuse is CombMNZ for the seat."));
        assert_eq!(
            updated["warning"],
            json!("an update cannot raise source.basis from agent to person")
        );
        let got = svc.store.get("w", id).unwrap().unwrap();
        assert_eq!(got["source"]["basis"], json!("agent"));
        assert_eq!(got["source"]["via"], json!("seat"));
        assert_eq!(got["text"], updated["text"]);
        assert!(got.get("warning").is_none(), "{got:?}");

        let mut post = atom("Reviews open with a check.");
        post.insert("id".into(), json!(id));
        post.insert("kind".into(), json!("lesson"));
        post.insert("origin".into(), json!("user-declared"));
        post.insert("source".into(), json!({"basis": "person", "via": "ljos"}));
        let posted = svc.add(post).unwrap();
        assert_eq!(posted["text"], json!("Reviews open with a check."));
        assert_eq!(posted["origin"], json!("agent-derived"));
        assert_eq!(posted["source"]["basis"], json!("agent"));
        assert_eq!(posted["source"]["via"], json!("ljos"));
        assert_eq!(
            posted["warning"],
            json!(
                "a post over an existing id cannot raise origin from agent-derived to user-declared; only accept raises an origin; a post over an existing id cannot raise source.basis from agent to person"
            )
        );
        let got = svc.store.get("w", id).unwrap().unwrap();
        assert_eq!(got["text"], json!("Reviews open with a check."));
        assert_eq!(got["origin"], json!("agent-derived"));
        assert_eq!(got["source"]["basis"], json!("agent"));
        assert!(got.get("warning").is_none(), "{got:?}");
    }

    /// Accept of an admission proposal already on file stores it as
    /// user-declared. Putting that origin back to agent-derived stays down.
    #[test]
    fn an_accepted_admission_put_back_to_agent_derived_stays_there() {
        let (_dir, svc) = service();
        let proposal = plant_admission(&svc.home);
        let accepted = svc.accept("w", proposal).unwrap();
        assert_eq!(accepted["origin"], json!("user-declared"));
        let id = accepted["id"].as_str().unwrap();

        let mut back = Map::new();
        back.insert("origin".into(), json!("agent-derived"));
        let relabelled = svc.update("w", id, &back).unwrap();
        assert_eq!(relabelled["origin"], json!("agent-derived"));
        let mut up = Map::new();
        up.insert("origin".into(), json!("user-declared"));
        up.insert(
            "text".into(),
            json!("The fuse is CombMNZ for this seat, revised."),
        );
        let kept = svc.update("w", id, &up).unwrap();
        assert_eq!(kept["origin"], json!("agent-derived"));
        assert_eq!(
            kept["text"],
            json!("The fuse is CombMNZ for this seat, revised.")
        );
        assert!(
            kept["warning"].as_str().unwrap().contains("accept"),
            "{kept:?}"
        );
        let stored = svc.store.get("w", id).unwrap().unwrap();
        assert_eq!(stored["origin"], json!("agent-derived"));
        assert_eq!(stored["text"], kept["text"]);
        assert!(stored.get("warning").is_none(), "{stored:?}");
    }

    /// A record written before origin existed stays without the field.
    /// An update of another field must not stamp user-declared on it.
    #[test]
    fn an_update_leaves_a_missing_origin_unset() {
        let (_dir, svc) = service();
        let id = "a".repeat(32);
        let mut row = atom("Reviews open with a check.");
        row.insert("id".into(), json!(id));
        row.insert("tombstone".into(), json!(false));
        svc.store.upsert(&row).unwrap();
        let mut fields = Map::new();
        fields.insert("pinned".into(), json!(true));
        let updated = svc.update("w", &id, &fields).unwrap();
        assert!(updated.get("origin").is_none(), "{updated:?}");
        let got = svc.store.get("w", &id).unwrap().unwrap();
        assert!(got.get("origin").is_none(), "{got:?}");
        assert_eq!(got["pinned"], json!(true));
        assert_eq!(record::origin_of(&got), "unknown");

        let mut fields = Map::new();
        fields.insert("origin".into(), Value::Null);
        let err = svc.update("w", &id, &fields).unwrap_err();
        assert!(err.to_string().contains("unknown atom origin"), "{err}");
        assert!(svc
            .store
            .get("w", &id)
            .unwrap()
            .unwrap()
            .get("origin")
            .is_none());
        let mut fields = Map::new();
        fields.insert("origin".into(), json!(""));
        let err = svc.update("w", &id, &fields).unwrap_err();
        assert!(err.to_string().contains("unknown atom origin"), "{err}");
        assert!(svc
            .store
            .get("w", &id)
            .unwrap()
            .unwrap()
            .get("origin")
            .is_none());

        // No origin is the bottom of the order. Naming one is held down,
        // and the rest of the update is kept. The field stays unwritten.
        for (named, text) in [
            ("user-declared", "Reviews open with one check."),
            ("agent-derived", "Reviews open with two checks."),
        ] {
            let mut fields = Map::new();
            fields.insert("origin".into(), json!(named));
            fields.insert("text".into(), json!(text));
            let updated = svc.update("w", &id, &fields).unwrap();
            assert!(updated.get("origin").is_none(), "{updated:?}");
            assert_eq!(updated["text"], json!(text));
            assert!(
                updated["warning"].as_str().unwrap().contains("origin"),
                "{updated:?}"
            );
            let got = svc.store.get("w", &id).unwrap().unwrap();
            assert!(got.get("origin").is_none(), "{got:?}");
            assert_eq!(got["text"], json!(text));
            assert!(got.get(record::PROVENANCE_FLOOR).is_none(), "{got:?}");
            assert!(got.get("warning").is_none(), "{got:?}");
        }
    }

    #[test]
    fn accepting_an_admission_on_file_keeps_its_body_and_a_retry_is_that_atom() {
        let (_dir, svc) = service();
        let id = plant_admission(&svc.home);
        assert_eq!(svc.proposals("w").len(), 1);
        let stored = svc.accept("w", id).unwrap();
        assert_eq!(stored["kind"], json!("lesson"));
        assert_eq!(stored["level"], json!("explicit"));
        assert_eq!(stored["origin"], json!("user-declared"));
        assert_eq!(stored["text"], json!("The fuse is CombMNZ for this seat."));
        assert_eq!(stored["source"]["harness"], json!("inky"));
        assert_ne!(stored.get("hook"), Some(&json!(true)));
        let tags: Vec<&str> = stored["entities"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(tags.contains(&"issue:demo-amz9"), "{tags:?}");
        assert!(svc.proposals("w").is_empty());
        let retry = svc.accept("w", id).unwrap();
        assert_eq!(retry["id"], stored["id"]);
        let live = svc.store.live("w").unwrap();
        assert_eq!(live.len(), 1, "{live:?}");
        let mut typed = atom("The fuse is CombMNZ for this seat.");
        typed.insert("kind".into(), json!("lesson"));
        let posted = svc.add(typed).unwrap();
        assert_eq!(
            posted["id"], stored["id"],
            "the same text already live is not a second atom"
        );
        assert_eq!(svc.store.live("w").unwrap().len(), 1);
        let panel = packset_core::Panel::default();
        let found = svc
            .search("w", "fuse CombMNZ", 8, None, &panel, None, false, None)
            .unwrap();
        assert!(
            found["hits"]
                .as_array()
                .unwrap()
                .iter()
                .any(|hit| hit["id"] == stored["id"]),
            "{found}"
        );
    }

    #[test]
    fn a_hook_write_is_stored_at_once() {
        let (_dir, svc) = service();
        let mut caught = atom("A correction the hook caught in the cue.");
        caught.insert("kind".into(), json!("preference"));
        caught.insert("hook".into(), json!(true));
        let stored = svc.add(caught).unwrap();
        assert_eq!(stored["kind"], json!("preference"));
        assert_eq!(stored["hook"], json!(true));
        assert_eq!(svc.store.live("w").unwrap().len(), 1);
        assert!(svc.proposals("w").is_empty());
    }

    #[test]
    fn a_hook_write_is_live_and_an_echo_promotes_at_its_origin() {
        let (_dir, svc) = service();
        let mut caught = atom("A correction the hook caught in the cue.");
        caught.insert("hook".into(), json!(true));
        let stored_hook = svc.add(caught).unwrap();
        assert_eq!(stored_hook["hook"], json!(true));
        assert_eq!(svc.store.live("w").unwrap().len(), 1);
        assert!(svc.proposals("w").is_empty());

        let mut echo = atom("A lesson stored before anyone reviewed it.");
        echo.insert("origin".into(), json!("tool-echo"));
        echo.insert("content".into(), json!("authoritative"));
        echo.insert("entities".into(), json!(["seat:ljos", "horizon:transient"]));
        let stored = svc.add(echo).unwrap();
        let graded = svc
            .grade("w", stored["id"].as_str().unwrap(), true)
            .unwrap();
        let tags: Vec<&str> = graded["entities"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(tags.contains(&"horizon:tool-echo"), "{tags:?}");
        assert!(!tags.contains(&"horizon:standing"), "{tags:?}");
    }

    #[test]
    fn mail_members_and_receipts_are_each_kept() {
        let (_dir, svc) = service();
        let group = |member: &str| {
            let mut row = atom(&format!(
                "group {member}{member}{member}{member}{member}{member}{member}{member}\nname smoketeam\nmember {member}\nby inky\nat 2026-10-09T21:00:00Z"
            ));
            row.insert("kind".into(), json!("group"));
            row.insert(
                "entities".into(),
                json!(["seat:inky", "group:smoketeam", format!("member:{member}")]),
            );
            row
        };
        for member in ["a", "b", "c"] {
            svc.add(group(member)).unwrap();
        }
        let groups: Vec<String> = svc
            .store()
            .live("w")
            .unwrap()
            .iter()
            .filter(|a| a["kind"] == "group")
            .map(|a| a["text"].as_str().unwrap_or("").to_string())
            .collect();
        assert_eq!(groups.len(), 3, "{groups:?}");
        assert!(groups.iter().any(|t| t.contains("member a")));
        assert!(groups.iter().any(|t| t.contains("member c")));

        let receipt = |msg: &str| {
            let mut row = atom(&format!(
                "read {msg}\nby scratch\nat 2026-10-09T21:01:00Z\nmsg {msg}{msg}"
            ));
            row.insert("kind".into(), json!("receipt"));
            row.insert(
                "entities".into(),
                json!(["seat:scratch", "from:scratch", format!("receipt:{msg}")]),
            );
            row
        };
        svc.add(receipt("one")).unwrap();
        svc.add(receipt("two")).unwrap();
        let receipts = svc
            .store()
            .live("w")
            .unwrap()
            .iter()
            .filter(|a| a["kind"] == "receipt")
            .count();
        assert_eq!(receipts, 2, "two receipts stay two");

        let letter = "msg 0123456789abcdef0123456789abcdef\nfrom inky\nto scratch\n\nThe fuse is CombMNZ. Look again. Ask once more.";
        let mut first_letter = atom(letter);
        first_letter.insert("kind".into(), json!("message"));
        let mut second_letter = atom(letter);
        second_letter.insert("kind".into(), json!("message"));
        let stored = svc.add(first_letter).unwrap();
        let again = svc.add(second_letter).unwrap();
        assert_ne!(
            stored["id"], again["id"],
            "the same letter twice is two atoms"
        );

        let panel = packset_core::Panel::default();
        let hits = svc
            .search("w", "CombMNZ", 8, None, &panel, None, false, None)
            .unwrap();
        let ids: Vec<&str> = hits["hits"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|h| h["id"].as_str())
            .collect();
        assert!(
            !ids.contains(&stored["id"].as_str().unwrap()),
            "mail is not an ordinary hit: {hits}"
        );
        let named = svc
            .search(
                "w",
                "CombMNZ",
                8,
                None,
                &panel,
                None,
                false,
                Some("message"),
            )
            .unwrap();
        let named_ids: Vec<&str> = named["hits"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|h| h["id"].as_str())
            .collect();
        assert!(
            named_ids.contains(&stored["id"].as_str().unwrap())
                || named_ids.contains(&again["id"].as_str().unwrap()),
            "kind message returns the letter: {named}"
        );
    }

    #[test]
    fn rules_with_one_reason_and_two_patterns_are_two_atoms() {
        let (_dir, svc) = service();
        let mut a = atom("Cloudflare Pages settings change through the dashboard.");
        a.insert("kind".into(), json!("rule"));
        a.insert(
            "pattern".into(),
            json!("*api.cloudflare.com*/pages/projects/*-X PATCH*"),
        );
        a.insert("verdict".into(), json!("ask"));
        let mut b = a.clone();
        b.insert(
            "pattern".into(),
            json!("*-X PATCH*api.cloudflare.com*/pages/projects/*"),
        );
        let mut c = a.clone();
        c.insert("verdict".into(), json!("deny"));
        let a1 = svc.add(a.clone()).unwrap();
        let b1 = svc.add(b).unwrap();
        let c1 = svc.add(c).unwrap();
        let a2 = svc.add(a).unwrap();
        assert_ne!(a1["id"], b1["id"], "a second glob is a second rule");
        assert_ne!(a1["id"], c1["id"], "a second verdict is a second rule");
        assert_eq!(a1["id"], a2["id"], "the same rule twice is one atom");
        assert_eq!(svc.store().current("w", None).unwrap().len(), 3);
    }

    #[test]
    fn a_set_scoped_claim_is_not_a_duplicate_of_an_unscoped_one() {
        let (_dir, svc) = service();
        let plain = svc.add(atom("Reviews open with a check.")).unwrap();
        let mut scoped = atom("Reviews open with a check.");
        scoped.insert("set".into(), json!("review"));
        let scoped = svc.add(scoped).unwrap();
        assert_ne!(plain["id"], scoped["id"]);
        assert_eq!(svc.store().current("w", None).unwrap().len(), 2);
    }

    #[test]
    fn the_same_text_on_another_issue_is_another_claim() {
        let (_dir, svc) = service();
        let mut ids = Vec::new();
        for issue in ["ljos-cjd8", "ljos-nr6q", "ljos-ensq", "ljos-wwbc"] {
            let mut note = atom("Code is on the pull request. CI runs the tests.");
            note.insert("issue".into(), json!(issue));
            ids.push(svc.add(note).unwrap()["id"].clone());
        }
        ids.sort_by_key(|id| id.as_str().unwrap_or("").to_string());
        ids.dedup();
        assert_eq!(ids.len(), 4, "{ids:?}");
        assert_eq!(svc.store().current("w", None).unwrap().len(), 4);

        // The same text on the same issue is still the record already held.
        let mut again = atom("Code is on the pull request. CI runs the tests.");
        again.insert("issue".into(), json!("ljos-cjd8"));
        let again = svc.add(again).unwrap();
        assert!(ids.contains(&again["id"]), "{again:?}");
        assert_eq!(svc.store().current("w", None).unwrap().len(), 4);
    }

    #[test]
    fn a_tool_dump_is_stored_with_its_label() {
        let (_dir, svc) = service();
        let listing = std::iter::once("total 48".to_string())
            .chain((0..7).map(|i| format!("-rw-r--r-- 1 x x 0 Jan 1 00:00 file{i}")))
            .collect::<Vec<_>>()
            .join("\n");
        let stored = svc.add(atom(&listing)).unwrap();
        assert_eq!(stored["labels"], json!([packset_core::record::TOOL_DUMP]));

        // A fenced capture naming a stream is the other shape.
        let fenced = atom("Here is the run:\n```\nstdout: everything fine\n```");
        let stored = svc.add(fenced).unwrap();
        assert_eq!(stored["labels"], json!([packset_core::record::TOOL_DUMP]));
    }

    #[test]
    fn linking_is_symmetric_across_a_write() {
        let (_dir, svc) = service();
        let mut one = atom("The Parser reads the Header.");
        one.insert("entities".into(), json!(["Parser", "Header"]));
        let one = svc.add(one).unwrap();
        let mut two = atom("The Header comes before the Parser body.");
        two.insert("entities".into(), json!(["Parser", "Header"]));
        let two = svc.add(two).unwrap();

        let live = svc.store().current("w", None).unwrap();
        let first = live.iter().find(|a| a["id"] == one["id"]).unwrap();
        let second = live.iter().find(|a| a["id"] == two["id"]).unwrap();
        assert_eq!(second["links"], json!([one["id"].as_str().unwrap()]));
        assert_eq!(
            first["links"],
            json!([two["id"].as_str().unwrap()]),
            "the peer was rewritten, not just the newcomer"
        );
    }

    #[test]
    fn no_rewrite_moves_the_time_an_atom_was_written() {
        let (_dir, svc) = service();
        let written = "2026-01-01T00:00:00.000Z";
        let mut one = atom("The Parser reads the Header.");
        one.insert("entities".into(), json!(["Parser", "Header"]));
        one.insert("ts".into(), json!(written));
        let one = svc.add(one).unwrap();
        let id = one["id"].as_str().unwrap().to_string();
        let ts_of = || svc.store().get("w", &id).unwrap().unwrap()["ts"].clone();
        assert_eq!(ts_of(), json!(written));

        // The second write rewrites the first one's links.
        let mut two = atom("The Header comes before the Parser body.");
        two.insert("entities".into(), json!(["Parser", "Header"]));
        svc.add(two).unwrap();
        let linked = svc.store().get("w", &id).unwrap().unwrap();
        assert_eq!(linked["links"].as_array().map(Vec::len), Some(1));
        assert_eq!(linked["ts"], json!(written), "a link rewrite moved ts");
        assert!(linked["updated"].as_str().unwrap() > written);

        let graded = svc.grade("w", &id, true).unwrap();
        assert_eq!(graded["ts"], json!(written), "a grade moved ts");

        let mut fields = Map::new();
        fields.insert("pinned".into(), json!(true));
        fields.insert("ts".into(), json!("2026-10-18T00:00:00.000Z"));
        let updated = svc.update("w", &id, &fields).unwrap();
        assert_eq!(updated["ts"], json!(written), "an update moved ts");
        assert_eq!(ts_of(), json!(written));

        svc.delete_atom("w", &id, None).unwrap();
        assert_eq!(ts_of(), json!(written), "a tombstone moved ts");
    }

    #[test]
    fn a_contrary_remember_closes_the_live_window() {
        let (_dir, svc) = service();
        let mut old = atom("The default fuse is Borda.");
        old.insert("entities".into(), json!(["fuse", "Borda"]));
        let old = svc.add(old).unwrap();
        let mut neu = atom("The default fuse is CombMNZ.");
        neu.insert("entities".into(), json!(["fuse", "CombMNZ"]));
        let neu = svc.add(neu).unwrap();
        let now = packset_core::clock::utcnow();
        let live: Vec<_> = svc
            .store()
            .current("w", None)
            .unwrap()
            .into_iter()
            .filter(|a| packset_core::record::is_live(a, &now))
            .collect();
        assert_eq!(live.len(), 1, "the old claim is no longer live");
        assert_eq!(live[0]["id"], neu["id"]);
        let closed = svc
            .store()
            .get("w", old["id"].as_str().unwrap())
            .unwrap()
            .expect("the closed atom stays on disk");
        assert!(
            closed.get("valid_to").and_then(Value::as_str).is_some(),
            "{closed:?}"
        );
        let supersedes = neu["supersedes"].as_array().expect("supersedes");
        assert!(
            supersedes.iter().any(|v| v.as_str() == old["id"].as_str()),
            "{neu:?}"
        );
        let found = svc
            .search(
                "w",
                "Borda",
                8,
                None,
                &packset_core::Panel::default(),
                None,
                false,
                None,
            )
            .unwrap();
        let hits = found["hits"].as_array().expect("hits");
        assert!(
            hits.iter()
                .all(|h| h.get("id").and_then(Value::as_str) != old["id"].as_str()),
            "search filters the closed atom: {found}"
        );
        let found_new = svc
            .search(
                "w",
                "CombMNZ",
                8,
                None,
                &packset_core::Panel::default(),
                None,
                false,
                None,
            )
            .unwrap();
        let new_hits = found_new["hits"].as_array().expect("hits");
        assert!(
            new_hits
                .iter()
                .any(|h| h.get("id").and_then(Value::as_str) == neu["id"].as_str()),
            "{found_new}"
        );
        let linked_to_closed = neu
            .get("links")
            .and_then(Value::as_array)
            .is_some_and(|links| links.iter().any(|v| v.as_str() == old["id"].as_str()));
        assert!(!linked_to_closed, "a close is not a link: {neu:?}");
    }

    #[test]
    fn add_writes_the_start_of_the_window() {
        let (_dir, svc) = service();
        let stored = svc.add(atom("Reviews open with a check.")).unwrap();
        assert!(
            stored.get("valid_from").and_then(Value::as_str).is_some(),
            "{stored:?}"
        );
    }

    #[test]
    fn add_seeds_the_review_clock_except_for_weighed_records() {
        let (_dir, svc) = service();
        let stored = svc.add(atom("Reviews open with a check.")).unwrap();
        let due = stored.get("due_at").and_then(Value::as_str).unwrap_or("");
        assert!(!due.is_empty(), "{stored:?}");
        assert_eq!(stored["review"]["reps"], 0);
        let mut row = atom("a weighs b.");
        row.insert("kind".into(), "trust".into());
        row.insert("from".into(), "a".into());
        row.insert("to".into(), "b".into());
        row.insert("weight".into(), 0.5.into());
        let stored = svc.add(row).unwrap();
        assert!(stored.get("due_at").is_none(), "{stored:?}");
        let mut persona = atom("A voter with a view of its own.");
        persona.insert("kind".into(), "persona".into());
        persona.insert("name".into(), "rev-honest".into());
        persona.insert("view".into(), "Reject habitat leaks.".into());
        persona.insert("anchor".into(), 0.2.into());
        let stored = svc.add(persona).unwrap();
        assert!(stored.get("due_at").is_none(), "{stored:?}");
        let mut outcome = atom("p-1 closed on ship.");
        outcome.insert("kind".into(), "outcome".into());
        outcome.insert("issue".into(), "p-1".into());
        outcome.insert("choice".into(), "ship".into());
        let stored = svc.add(outcome).unwrap();
        assert!(stored.get("due_at").is_none(), "{stored:?}");
    }

    #[test]
    fn a_dated_retrieve_returns_the_atom_that_was_live_then() {
        let (_dir, svc) = service();
        svc.store()
            .upsert(
                &json!({
                    "id": "old",
                    "workspace": "w",
                    "text": "The default fuse is Borda.",
                    "kind": "voice",
                    "valid_from": "2024-01-01T00:00:00.000Z",
                    "valid_to": "2024-12-01T00:00:00.000Z"
                })
                .as_object()
                .unwrap()
                .clone(),
            )
            .unwrap();
        svc.store()
            .upsert(
                &json!({
                    "id": "neu",
                    "workspace": "w",
                    "text": "The default fuse is CombMNZ.",
                    "kind": "voice",
                    "valid_from": "2024-12-01T00:00:00.000Z"
                })
                .as_object()
                .unwrap()
                .clone(),
            )
            .unwrap();
        let then = svc.as_of("w", "2024-06-01T00:00:00.000Z").unwrap();
        let then_ids: Vec<&str> = then["atoms"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|a| a["id"].as_str())
            .collect();
        assert_eq!(then_ids, vec!["old"], "{then}");
        assert_eq!(then["as_of"], json!("2024-06-01T00:00:00.000Z"));
        let offset = svc.as_of("w", "2024-06-01T00:00:00+00:00").unwrap();
        let offset_ids: Vec<&str> = offset["atoms"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|a| a["id"].as_str())
            .collect();
        assert_eq!(offset_ids, then_ids, "{offset}");
        assert_eq!(offset["as_of"], json!("2024-06-01T00:00:00.000Z"));
        let later = svc.as_of("w", "2025-01-01T00:00:00.000Z").unwrap();
        let later_ids: Vec<&str> = later["atoms"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|a| a["id"].as_str())
            .collect();
        assert_eq!(later_ids, vec!["neu"], "{later}");

        let panel = packset_core::Panel::default();
        let hits = svc
            .search(
                "w",
                "Borda",
                8,
                None,
                &panel,
                Some("2024-06-01T00:00:00.000Z"),
                false,
                None,
            )
            .unwrap();
        let hit_ids: Vec<&str> = hits["hits"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|h| h["id"].as_str())
            .collect();
        assert_eq!(hit_ids, vec!["old"], "{hits}");
        assert_eq!(hits["as_of"], json!("2024-06-01T00:00:00.000Z"));
        let offset_hits = svc
            .search(
                "w",
                "Borda",
                8,
                None,
                &panel,
                Some("2024-06-01T00:00:00+00:00"),
                false,
                None,
            )
            .unwrap();
        let offset_hit_ids: Vec<&str> = offset_hits["hits"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|h| h["id"].as_str())
            .collect();
        assert_eq!(offset_hit_ids, hit_ids, "{offset_hits}");
        assert_eq!(offset_hits["as_of"], json!("2024-06-01T00:00:00.000Z"));
        let now_hits = svc
            .search("w", "Borda", 8, None, &panel, None, false, None)
            .unwrap();
        let now_ids: Vec<&str> = now_hits["hits"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|h| h["id"].as_str())
            .collect();
        assert!(
            !now_ids.contains(&"old"),
            "live-now search still drops it: {now_hits}"
        );
        let refused = svc
            .search(
                "w",
                "Borda",
                8,
                None,
                &panel,
                Some("not-a-date"),
                false,
                None,
            )
            .unwrap_err();
        assert!(
            refused.to_string().contains("as_of must be a timestamp"),
            "{refused}"
        );
    }

    #[test]
    fn updating_a_missing_atom_says_so() {
        let (_dir, svc) = service();
        let err = svc.update("w", "nope", &Map::new()).unwrap_err();
        assert_eq!(err.to_string(), "no current atom nope");
    }

    #[test]
    fn grading_moves_the_review_clock_and_not_the_live_window() {
        let (_dir, svc) = service();
        let stored = svc.add(atom("Reviews open with a check.")).unwrap();
        let id = stored["id"].as_str().unwrap();
        let graded = svc.grade("w", id, true).unwrap();
        assert!(graded["due_at"].is_string(), "{graded:?}");
        assert_eq!(graded["review"]["reps"], json!(1));
        assert!(
            graded.get("valid_to").is_none() || graded["valid_to"].is_null(),
            "the live window is a different question"
        );
    }

    #[test]
    fn the_pack_is_one_shape_scoped_or_not() {
        let (_dir, svc) = service();
        svc.add(atom("Reviews open with a check.")).unwrap();
        let plain = svc.pack("w", None).unwrap();
        for key in ["workspace", "user", "memory", "atoms"] {
            assert!(plain.get(key).is_some(), "{key} missing from {plain}");
        }
        assert!(plain.get("set").is_none());

        let scoped = svc.pack("w", Some("Review")).unwrap();
        assert_eq!(scoped["set"], json!("review"), "the name is normalized");
        for key in ["workspace", "user", "memory", "atoms", "instructions"] {
            assert!(scoped.get(key).is_some(), "{key} missing from {scoped}");
        }
        assert!(svc.pack("w", Some("../etc")).is_err());
    }

    #[test]
    fn a_pin_round_trips_and_clears() {
        let (_dir, svc) = service();
        assert_eq!(svc.pin("w"), "");
        assert_eq!(svc.set_pin("w", "Review").unwrap(), "review");
        assert_eq!(svc.pin("w"), "review");
        assert_eq!(svc.set_pin("w", "").unwrap(), "");
        assert_eq!(svc.pin("w"), "");
        assert!(svc.set_pin("w", "../etc").is_err());
    }

    #[test]
    fn an_overflowing_card_is_archived_before_it_is_refused() {
        let (_dir, svc) = service();
        // Plain short sentences: the point is the length, and prose the
        // archive would itself refuse tests something else.
        let long = "One small claim. ".repeat(USER_CAP / 8);
        let err = svc.set_user(&long).unwrap_err();
        assert!(matches!(err, cards::WriteError::Overflow(_)), "{err}");
        // Refused, not lost: the day file has it for the miner.
        let day = clock::utcnow()[..10].to_string();
        let archived = cards::read_text(&svc.home().archive_path("global", &day));
        assert!(
            archived.contains("One small claim."),
            "the overflow was dropped rather than archived"
        );
    }

    /// Plain text is the reason, kept beside the claim it retracts. An
    /// accession stays a citation and gains no warning.
    #[test]
    fn a_plain_retraction_keeps_its_reason() {
        let (_dir, svc) = service();
        let stored = svc.add(atom("The overlay landed.")).unwrap();
        let id = stored["id"].as_str().unwrap().to_string();
        let tomb = svc
            .delete_atom("w", &id, Some("because I said so"))
            .unwrap();
        assert_eq!(tomb["tombstone"], json!(true), "{tomb:?}");
        assert_eq!(tomb["why_text"], json!("because I said so"));
        assert_eq!(tomb["text"], json!("The overlay landed."));
        assert!(tomb.get("retracted_by").is_none(), "{tomb:?}");
        assert_eq!(
            tomb["warning"],
            json!(
                "because I said so is not a deed accession; kept as why_text. A retraction cites deed-<kind>-<slug> or sha256:<hash>"
            )
        );
        let kept = svc.store().get("w", &id).unwrap().unwrap();
        assert_eq!(kept["why_text"], json!("because I said so"));
        assert_eq!(kept["tombstone"], json!(true));

        let stored = svc.add(atom("The overlay landed again.")).unwrap();
        let id = stored["id"].as_str().unwrap().to_string();
        let cited = svc
            .delete_atom("w", &id, Some("deed-patch-overlay"))
            .unwrap();
        assert_eq!(cited["retracted_by"], json!("deed-patch-overlay"));
        assert!(cited.get("why_text").is_none(), "{cited:?}");
        assert!(cited.get("warning").is_none(), "{cited:?}");

        let stored = svc.add(atom("The overlay landed a third time.")).unwrap();
        let id = stored["id"].as_str().unwrap().to_string();
        let blank = svc.delete_atom("w", &id, Some("   ")).unwrap();
        assert!(blank.get("why_text").is_none(), "{blank:?}");
        assert!(blank.get("retracted_by").is_none(), "{blank:?}");
        assert!(blank.get("warning").is_none(), "{blank:?}");
        assert_eq!(blank["tombstone"], json!(true));
    }

    #[test]
    fn an_attachment_is_one_shot() {
        let (_dir, svc) = service();
        svc.put_attach("w", "a log body", "build.log");
        assert_eq!(svc.peek_attach("w").unwrap().text, "a log body");
        assert_eq!(svc.peek_attach("w").unwrap().label, "build.log");
        assert_eq!(svc.take_attach("w").unwrap().text, "a log body");
        assert!(
            svc.take_attach("w").is_none(),
            "context for the next turn, not for every turn after it"
        );
    }

    /// Peeked slots expire, and a new workspace past the cap drops the oldest.
    #[test]
    fn attach_slots_expire_and_stay_inside_the_cap() {
        let (_dir, svc) = service();
        for n in 0..ATTACH_SLOTS {
            svc.put_attach(&format!("w{n}"), "body", "note");
        }
        assert!(svc.peek_attach("w0").is_some());
        svc.put_attach("extra", "newer", "note");
        assert!(
            svc.peek_attach("w0").is_none(),
            "the oldest slot stayed past the cap"
        );
        assert_eq!(svc.peek_attach("extra").unwrap().text, "newer");
        svc.age_attach("extra", ATTACH_TTL);
        assert!(
            svc.peek_attach("extra").is_none(),
            "a slot past the hour was still held"
        );
    }

    #[test]
    fn an_attachment_is_capped() {
        let (_dir, svc) = service();
        let huge = "x".repeat(ATTACH_CAP + 100);
        let slot = svc.put_attach("w", &huge, "");
        assert_eq!(slot["text"].as_str().unwrap().chars().count(), ATTACH_CAP);
    }

    /// U+2024 is three bytes. A byte cut at the character cap falls inside
    /// the leader when it is the last character kept.
    #[test]
    fn an_attachment_cut_keeps_the_leader_whole() {
        let (_dir, svc) = service();
        let mut body = "x".repeat(ATTACH_CAP - 1);
        body.push('\u{2024}');
        body.push('y');
        let slot = svc.put_attach("w", &body, "note");
        let text = slot["text"].as_str().unwrap();
        assert_eq!(text.chars().count(), ATTACH_CAP);
        assert!(text.ends_with('\u{2024}'), "{text:?}");
        assert!(!text.contains('y'));
    }

    #[test]
    fn status_counts_by_kind_and_names_the_home() {
        let (_dir, svc) = service();
        let stored = svc.add(atom("Reviews open with a check.")).unwrap();
        svc.store()
            .delete("w", stored["id"].as_str().unwrap(), None)
            .unwrap();
        let mut second = atom("Prefer ripgrep for search.");
        second.insert("kind".into(), json!("preference"));
        svc.add(second).unwrap();

        let panel = packset_core::Panel::named("rrf", "none", "off").unwrap();
        let status = svc.status(Some("w"), &panel).unwrap();
        assert_eq!(status["live"], json!(1));
        // The panel is host configuration a client cannot see, so status is
        // where an operator finds out which voters answered.
        assert_eq!(status["panel"]["fuse"], json!("rrf"));
        // A build that cannot say which commit it is says so, rather than
        // saying nothing and reading as current.
        assert!(!status["commit"].as_str().unwrap_or_default().is_empty());
        assert_eq!(status["panel"]["diversify"], json!("none"));
        assert_eq!(status["tombstone"], json!(1));
        assert_eq!(status["live_by_kind"]["preference"], json!(1));
        assert_eq!(status["workspace"], json!("w"));
        assert!(status["home"].is_string());
        assert!(status["last_write_ts"].is_string());
        assert_eq!(status["rerank"]["depth"], json!(crate::embed::RERANK_DEPTH));
        if std::env::var_os("PACKSET_RERANK").is_none() {
            assert_eq!(status["rerank"]["enabled"], json!(false));
        }
    }

    /// A binary that cannot load its runtime is neither an encoder nor a
    /// reranker, and status says so for both once it has failed a call.
    #[test]
    fn a_dead_encoder_is_no_reranker_either() {
        let dir = tempfile::tempdir().unwrap();
        let svc = Service::open(crate::home::Home::new(dir.path())).unwrap();
        let panel = packset_core::Panel::default();
        let _guard = crate::embed::EMBED
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::embed::reset_for_test();
        let stub = broken_reranker();
        let old = std::env::var_os("PACKSET_EMBED");
        // Safety: EMBED is held, so no other test mutates this variable.
        unsafe { std::env::set_var("PACKSET_EMBED", &stub.path) };
        let before = svc.status(None, &panel).unwrap();
        let encoded = crate::embed::encode_query("a question");
        let after = svc.status(None, &panel).unwrap();
        unsafe {
            match old {
                Some(value) => std::env::set_var("PACKSET_EMBED", value),
                None => std::env::remove_var("PACKSET_EMBED"),
            }
        }
        crate::embed::reset_for_test();
        drop(_guard);
        assert!(encoded.is_none());
        assert_eq!(before["rerank"]["available"], json!(true), "{before}");
        assert_eq!(after["embedder"]["answering"], json!(false), "{after}");
        assert_eq!(after["embedder"]["available"], json!(false), "{after}");
        assert_eq!(after["rerank"]["available"], json!(false), "{after}");
    }

    /// `remaining_ms` is there while a load is running, and gone once the
    /// encoder is answering or unavailable. The clock is set, so this does
    /// not wait on a model.
    #[test]
    fn status_reports_remaining_ms_only_while_the_encoder_loads() {
        let _guard = crate::embed::EMBED
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::embed::reset_for_test();
        let old_inside = std::env::var_os("INSIDE_EMBED");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        crate::embed::set_load_for_test(now.saturating_sub(200), 5_000);
        let (_dir, svc) = service();
        let panel = packset_core::Panel::default();
        let loading = svc.status(None, &panel).unwrap();
        crate::embed::set_dense_for_test(Some(true));
        let answering = svc.status(None, &panel).unwrap();
        crate::embed::set_dense_for_test(Some(false));
        let failed = svc.status(None, &panel).unwrap();
        crate::embed::reset_for_test();
        unsafe { std::env::set_var("INSIDE_EMBED", "off") };
        let off = svc.status(None, &panel).unwrap();
        unsafe {
            match old_inside {
                Some(value) => std::env::set_var("INSIDE_EMBED", value),
                None => std::env::remove_var("INSIDE_EMBED"),
            }
        }
        crate::embed::reset_for_test();
        drop(_guard);

        assert!(loading["embedder"]["answering"].is_null(), "{loading}");
        let ms = loading["embedder"]["remaining_ms"]
            .as_u64()
            .expect("remaining_ms while loading");
        assert!((4_000..=5_000).contains(&ms), "{ms} from {loading}");
        assert_eq!(
            answering["embedder"]["answering"],
            json!(true),
            "{answering}"
        );
        assert!(
            answering["embedder"].get("remaining_ms").is_none(),
            "{answering}"
        );
        assert_eq!(failed["embedder"]["answering"], json!(false), "{failed}");
        assert_eq!(failed["embedder"]["available"], json!(false), "{failed}");
        assert!(failed["embedder"].get("remaining_ms").is_none(), "{failed}");
        assert!(off["embedder"]["answering"].is_null(), "{off}");
        assert_eq!(off["embedder"]["enabled"], json!(false), "{off}");
        assert_eq!(off["embedder"]["available"], json!(false), "{off}");
        assert!(off["embedder"].get("remaining_ms").is_none(), "{off}");
    }

    /// `INSIDE_EMBED=off` leaves the encoder unstarted. An encoder is named
    /// and would answer, so a switch that only hid a missing binary would
    /// still put a dense ballot on the search. Write and search answer, the
    /// engine stays lexical, and status reads the encoder as off. No
    /// `packset-embed` is required on `PATH`: the named binary is the one
    /// that must not start.
    #[test]
    fn with_the_encoder_off_search_stays_lexical_and_status_says_so() {
        let _guard = crate::embed::EMBED
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::embed::reset_for_test();
        let dir = tempfile::tempdir().unwrap();
        let sentinel = dir.path().join("started");
        let stub = dir.path().join("stub-embed");
        std::fs::write(
            &stub,
            format!(
                r#"#!/usr/bin/env python3
import json, pathlib, sys
pathlib.Path(r"{sentinel}").write_text("started\n")
if "--rerank" in sys.argv:
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        req = json.loads(line)
        n = len(req.get("d") or [])
        print(json.dumps({{"id": req.get("id", "q"), "s": [float(i) for i in range(n)]}}), flush=True)
else:
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        print(json.dumps({{"v": [0.2, 0.1]}}), flush=True)
"#,
                sentinel = sentinel.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(&stub).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&stub, perm).unwrap();
        let old_embed = std::env::var_os("PACKSET_EMBED");
        let old_inside = std::env::var_os("INSIDE_EMBED");
        // SAFETY: EMBED is held, so no other test reads these variables.
        unsafe {
            std::env::set_var("PACKSET_EMBED", &stub);
            std::env::set_var("INSIDE_EMBED", "off");
        }
        let (_home, svc) = service();
        svc.add(atom("Reviews open with a check.")).unwrap();
        svc.add(atom("Prefer ripgrep for search.")).unwrap();
        let panel = packset_core::Panel::default();
        let found = svc
            .search("w", "reviews search", 8, None, &panel, None, true, None)
            .unwrap();
        let status = svc.status(Some("w"), &panel).unwrap();
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
        crate::embed::reset_for_test();
        drop(_guard);
        assert_eq!(status["embedder"]["enabled"], json!(false), "{status}");
        assert_eq!(status["embedder"]["available"], json!(false), "{status}");
        assert!(
            status["embedder"]["answering"].is_null(),
            "a dense call ran: {status}"
        );
        assert!(status["embedder"]["binary"].is_null(), "{status}");
        assert_eq!(status["rerank"]["available"], json!(false), "{status}");
        assert_ne!(found["engine"], json!("dense"), "{found}");
        assert_eq!(found["rerank"], json!("absent"), "{found}");
        let texts: Vec<_> = found["hits"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|hit| hit["text"].as_str())
            .collect();
        assert!(
            texts.iter().any(|text| text.contains("Reviews")),
            "{texts:?}"
        );
        assert!(
            texts.iter().any(|text| text.contains("ripgrep")),
            "{texts:?}"
        );
        assert!(
            !sentinel.exists(),
            "the encoder child was started with the switch off"
        );
        for stored in svc.store.live("w").unwrap().iter() {
            let embedding = stored.get("embedding");
            assert!(
                embedding.is_none() || embedding.is_some_and(Value::is_null),
                "a write stored a vector with the encoder off: {stored:?}"
            );
        }
    }

    /// Twenty-four fresh lessons and one from January, all sharing the
    /// query words. The shipped order cuts the older lesson. Relevance
    /// mode returns it inside k. Status names the cut so an older writer,
    /// which has no `search` object, can be told apart.
    #[test]
    fn relevance_returns_the_older_lesson_the_default_cuts() {
        let (_dir, svc) = service();
        // The fresh lessons share the query words and nothing else, so none
        // is a rewrite of another. The january lesson is the short one and
        // is held with twice the trust: BM25 likes it, the scan puts it a
        // tenth ahead rather than tied, and the half-life still puts the
        // fresh ones ahead. Tied, it would go behind them as the older write.
        let old = {
            let mut row = atom("Zeta quorum kept.");
            row.insert("id".into(), json!("a-january"));
            row.insert("kind".into(), json!("lesson"));
            row.insert("trust".into(), json!(2.0));
            row.insert("ts".into(), json!("2025-01-15T00:00:00.000Z"));
            row.insert("entities".into(), json!([]));
            row
        };
        svc.add(old).unwrap();
        for i in 0..24 {
            let mut row = atom(&format!(
                "Zeta quorum n{i:02}a n{i:02}b n{i:02}c n{i:02}d n{i:02}e n{i:02}f."
            ));
            row.insert("id".into(), json!(format!("f-{i:02}")));
            row.insert("kind".into(), json!("lesson"));
            row.insert("entities".into(), json!([]));
            svc.add(row).unwrap();
        }
        assert_eq!(svc.store().live("w").unwrap().len(), 25);
        let panel = packset_core::Panel::default();
        let fused = svc
            .search("w", "zeta quorum", 20, None, &panel, None, false, None)
            .unwrap();
        let rel = svc
            .search_ranked(
                "w",
                "zeta quorum",
                20,
                None,
                &panel,
                None,
                false,
                None,
                packset_core::search::Rank::Relevance,
            )
            .unwrap();
        let ids = |body: &Value| -> Vec<String> {
            body["hits"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|h| h["id"].as_str().map(str::to_string))
                .collect()
        };
        let fused_ids = ids(&fused);
        let rel_ids = ids(&rel);
        assert_eq!(fused["rank"], json!("fused"), "{fused}");
        assert_eq!(rel["rank"], json!("relevance"), "{rel}");
        assert_eq!(fused_ids.len(), 20, "{fused_ids:?}");
        assert!(
            !fused_ids.iter().any(|id| id == "a-january"),
            "default order changed: {fused_ids:?}"
        );
        assert!(
            rel_ids.iter().any(|id| id == "a-january"),
            "relevance dropped the january lesson: {rel_ids:?}"
        );
        let status = svc.status(Some("w"), &panel).unwrap();
        let ranks = status["search"]["ranks"].as_array().unwrap();
        assert!(ranks.iter().any(|v| v == "fused"));
        assert!(ranks.iter().any(|v| v == "relevance"));
    }

    /// The seat search path does not run the measured second stage unless
    /// it is asked. The default is the first-stage ranking a pack already
    /// returns.
    #[test]
    fn search_leaves_the_cross_encoder_off() {
        let (_dir, svc) = service();
        svc.add(atom("Reviews open with a check.")).unwrap();
        let panel = packset_core::Panel::default();
        let found = svc
            .search("w", "reviews", 8, None, &panel, None, false, None)
            .unwrap();
        assert_eq!(found["rerank"], json!("off"), "{found}");
        assert_eq!(found["hits"].as_array().map(Vec::len), Some(1));
        assert!(found["hits"][0]["rerank_score"].is_null(), "{found}");
        let empty = svc
            .search("w", "", 8, None, &panel, None, true, None)
            .unwrap();
        assert_eq!(empty["rerank"], json!("off"), "{empty}");
        assert!(empty["hits"].as_array().unwrap().is_empty());
    }

    /// Claims that carry vectors rank by their text, the same order the
    /// scorer returns over those records. The vectors stay stored.
    #[test]
    fn search_ranks_the_stored_text_the_way_the_scorer_does() {
        let (_dir, svc) = service();
        let embedding = json!([0.2, 0.4, 0.6, 0.1]);
        for (id, text) in [
            ("alpha", "The quartz lens sits on the bench."),
            ("beta", "The river gate opens at dawn."),
        ] {
            let mut claim = atom(text);
            claim.insert("id".into(), json!(id));
            claim.insert("embedding".into(), embedding.clone());
            svc.store.upsert(&claim).unwrap();
        }
        let panel = packset_core::Panel::default();
        let found = svc
            .search("w", "quartz lens", 5, None, &panel, None, false, None)
            .unwrap();
        let got: Vec<&str> = found["hits"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|hit| hit["id"].as_str())
            .collect();
        let live = svc.store.live("w").unwrap();
        let documents: Vec<Vec<String>> = live
            .iter()
            .map(|atom| packset_core::search::atom_tokens(atom))
            .collect();
        let index = packset_core::bm25::Index::build(documents.iter().map(Vec::as_slice));
        let now = packset_core::clock::utcnow();
        let ask = packset_core::search::Ask {
            user: "",
            memory: "",
            atoms: live.as_slice(),
            query: "quartz lens",
            limit: 5,
            set: None,
            kind: None,
            now: &now,
        };
        let lexical = packset_core::search::search_linear_with_ranked(
            &ask,
            &documents,
            packset_core::search::Rank::Fused,
        );
        let terms = packset_core::search::search_bm25_ranked(
            &ask,
            &index,
            packset_core::search::Rank::Fused,
        );
        let merged = packset_core::search::merge_ballots_ranked(
            &[lexical, terms],
            5,
            &panel,
            &now,
            packset_core::search::Rank::Fused,
        );
        let expect: Vec<&str> = merged.iter().filter_map(|hit| hit["id"].as_str()).collect();
        assert_eq!(got, expect, "{found}");
        assert_eq!(got.first().copied(), Some("alpha"));
        for id in ["alpha", "beta"] {
            assert_eq!(
                svc.store.get("w", id).unwrap().unwrap()["embedding"]
                    .as_array()
                    .map(Vec::len),
                Some(4)
            );
        }
    }

    /// A requested rerank over a lone hit never reaches the model: one
    /// candidate cannot be reordered, so the stage settles it without a
    /// forward pass. The stub records any invocation in a sentinel file;
    /// its absence is the assertion the model was not asked.
    #[test]
    fn a_lone_hit_settles_the_rerank_without_a_model_call() {
        let (_dir, svc) = service();
        svc.add(atom("Reviews open with a check.")).unwrap();
        let panel = packset_core::Panel::default();
        let _guard = crate::embed::EMBED
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::embed::reset_for_test();
        let dir = tempfile::tempdir().unwrap();
        let sentinel = dir.path().join("called");
        let stub = dir.path().join("packset-embed");
        // The dense ballot still asks its encoder; only a `--rerank`
        // invocation may touch the sentinel.
        std::fs::write(
            &stub,
            format!(
                r#"#!/usr/bin/env python3
import json, pathlib, sys
if "--rerank" in sys.argv:
    pathlib.Path(r"{sentinel}").touch()
    sys.exit(1)
for line in sys.stdin:
    print(json.dumps({{"v": [0.1]}}), flush=True)
"#,
                sentinel = sentinel.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(&stub).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&stub, perm).unwrap();
        let old = std::env::var_os("PACKSET_EMBED");
        // Safety: EMBED is held, so no other test mutates this variable.
        unsafe { std::env::set_var("PACKSET_EMBED", &stub) };
        let off = svc
            .search("w", "reviews", 8, None, &panel, None, false, None)
            .unwrap();
        let on = svc
            .search("w", "reviews", 8, None, &panel, None, true, None)
            .unwrap();
        unsafe {
            match old {
                Some(value) => std::env::set_var("PACKSET_EMBED", value),
                None => std::env::remove_var("PACKSET_EMBED"),
            }
        }
        crate::embed::reset_for_test();
        drop(_guard);
        assert_eq!(on["rerank"], json!("settled"), "{on}");
        assert!(!sentinel.exists(), "the model was asked about one hit");
        // The reranked head is deeper than the plain limit, so the fused
        // scores disagree past the last digits; the order is the ranking.
        assert_eq!(on["hits"][0]["id"], off["hits"][0]["id"], "{on} vs {off}");
        assert_eq!(on["hits"].as_array().map(Vec::len), Some(1));
    }

    /// A requested stage with no working reranker leaves the first-stage
    /// order and says so. Silently reordering by nothing would be worse
    /// than leaving the stage off.
    #[test]
    fn a_requested_rerank_without_an_encoder_leaves_the_ranking() {
        let (_dir, svc) = service();
        svc.add(atom("Reviews open with a check.")).unwrap();
        svc.add(atom("Prefer ripgrep for search.")).unwrap();
        let panel = packset_core::Panel::default();
        // Point at a program that is not a reranker, so PATH cannot supply
        // a real packset-embed and turn this into a model call.
        let _guard = crate::embed::EMBED
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::embed::reset_for_test();
        let stub = broken_reranker();
        let old = std::env::var_os("PACKSET_EMBED");
        // Safety: EMBED is held, so no other test mutates this variable.
        unsafe { std::env::set_var("PACKSET_EMBED", &stub.path) };
        let off = svc
            .search("w", "reviews search", 8, None, &panel, None, false, None)
            .unwrap();
        let on = svc
            .search("w", "reviews search", 8, None, &panel, None, true, None)
            .unwrap();
        unsafe {
            match old {
                Some(value) => std::env::set_var("PACKSET_EMBED", value),
                None => std::env::remove_var("PACKSET_EMBED"),
            }
        }
        crate::embed::reset_for_test();
        drop(_guard);
        assert_eq!(on["rerank"], json!("absent"), "{on}");
        assert_eq!(off["rerank"], json!("off"));
        // Recency is a function of now, so two searches a moment apart
        // disagree in the last digits of the score. The order is the
        // ranking, and that is what a missing stage must not change.
        let ids = |found: &Value| {
            found["hits"]
                .as_array()
                .unwrap()
                .iter()
                .map(|hit| hit["id"].clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&on), ids(&off), "{on} vs {off}");
    }

    /// A working child that implements the same protocol as
    /// `packset-embed --rerank` reorders the first-stage list.
    #[test]
    fn a_stub_cross_encoder_reorders_the_first_stage() {
        let (_dir, svc) = service();
        svc.add(atom("Reviews open with a check.")).unwrap();
        svc.add(atom("Prefer ripgrep for search.")).unwrap();
        let panel = packset_core::Panel::default();
        let _guard = crate::embed::EMBED
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::embed::reset_for_test();
        let stub = scoring_reranker();
        let old = std::env::var_os("PACKSET_EMBED");
        // Safety: EMBED is held, so no other test mutates this variable.
        unsafe { std::env::set_var("PACKSET_EMBED", &stub.path) };
        let off = svc
            .search("w", "reviews search", 8, None, &panel, None, false, None)
            .unwrap();
        let on = svc
            .search("w", "reviews search", 8, None, &panel, None, true, None)
            .unwrap();
        unsafe {
            match old {
                Some(value) => std::env::set_var("PACKSET_EMBED", value),
                None => std::env::remove_var("PACKSET_EMBED"),
            }
        }
        crate::embed::reset_for_test();
        drop(_guard);
        assert_eq!(on["rerank"], json!("cross-encoder"), "{on}");
        let off_ids: Vec<_> = off["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["id"].clone())
            .collect();
        let on_ids: Vec<_> = on["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["id"].clone())
            .collect();
        assert_eq!(off_ids.len(), 2, "{off}");
        assert_eq!(on_ids.len(), 2, "{on}");
        // The stub scores the last candidate highest, so the first-stage
        // tail becomes the head.
        assert_eq!(on_ids[0], off_ids[1], "{on} vs {off}");
        assert_eq!(on_ids[1], off_ids[0], "{on} vs {off}");
    }

    /// The first stage has to return the locomo window or a `limit` below
    /// that window cannot be promoted into view.
    #[test]
    fn a_short_limit_still_reranks_the_measured_window() {
        let (_dir, svc) = service();
        svc.add(atom("Reviews open with a check.")).unwrap();
        svc.add(atom("Prefer ripgrep for search.")).unwrap();
        svc.add(atom("Prefer fd for finding files.")).unwrap();
        let panel = packset_core::Panel::default();
        let _guard = crate::embed::EMBED
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::embed::reset_for_test();
        let stub = scoring_reranker();
        let old = std::env::var_os("PACKSET_EMBED");
        // Safety: EMBED is held, so no other test mutates this variable.
        unsafe { std::env::set_var("PACKSET_EMBED", &stub.path) };
        let off = svc
            .search("w", "Prefer reviews", 1, None, &panel, None, false, None)
            .unwrap();
        let on = svc
            .search("w", "Prefer reviews", 1, None, &panel, None, true, None)
            .unwrap();
        unsafe {
            match old {
                Some(value) => std::env::set_var("PACKSET_EMBED", value),
                None => std::env::remove_var("PACKSET_EMBED"),
            }
        }
        crate::embed::reset_for_test();
        drop(_guard);
        assert_eq!(on["rerank"], json!("cross-encoder"), "{on}");
        assert_eq!(on["hits"].as_array().map(Vec::len), Some(1), "{on}");
        assert_eq!(off["hits"].as_array().map(Vec::len), Some(1), "{off}");
        // The stub scores later candidates higher. With first_limit at the
        // measured depth, the last of the three can become the only hit.
        assert_ne!(on["hits"][0]["id"], off["hits"][0]["id"], "{on} vs {off}");
    }

    struct StubEmbed {
        path: std::path::PathBuf,
        _dir: tempfile::TempDir,
    }

    fn broken_reranker() -> StubEmbed {
        write_stub(
            r#"#!/bin/sh
exit 1
"#,
        )
    }

    fn scoring_reranker() -> StubEmbed {
        write_stub(
            r#"#!/usr/bin/env python3
import json, sys
if "--rerank" not in sys.argv:
    sys.exit(1)
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    n = len(req.get("d") or [])
    print(json.dumps({"id": req.get("id", "q"), "s": [float(i) for i in range(n)]}), flush=True)
"#,
        )
    }

    fn write_stub(body: &str) -> StubEmbed {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("packset-embed");
        std::fs::write(&path, body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(&path).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&path, perm).unwrap();
        StubEmbed { path, _dir: dir }
    }

    /// One accession, cited by one atom and not the other.
    #[test]
    fn only_the_atom_that_cites_an_accession_is_named() {
        let (_dir, svc) = service();
        let mut cites = atom("The overlay landed as a frozen deed.");
        cites.insert("entities".into(), json!(["deed-patch-overlay", "overlay"]));
        let stored = svc.add(cites).unwrap();
        let mut elsewhere = atom("The parser was rewritten.");
        elsewhere.insert("entities".into(), json!(["parser"]));
        svc.add(elsewhere).unwrap();

        let found = svc.citers("w", "deed-patch-overlay").unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0]["id"], stored["id"]);
    }

    /// An accession nothing cites is an empty answer, not a missing one: that
    /// is the whole point of asking.
    #[test]
    fn an_uncited_accession_names_nobody() {
        let (_dir, svc) = service();
        let mut cites = atom("The overlay landed as a frozen deed.");
        cites.insert("entities".into(), json!(["deed-patch-overlay"]));
        svc.add(cites).unwrap();
        assert!(svc.citers("w", "deed-nothing-here").unwrap().is_empty());
        assert!(svc.citers("w", "  ").unwrap().is_empty());
    }

    /// A prefix is not a citation. `deed-patch-overlay-v2` is a different
    /// product, and naming it as a citer of the first would be a wrong answer
    /// that looks right.
    #[test]
    fn a_longer_accession_is_not_a_citation_of_the_shorter_one() {
        let (_dir, svc) = service();
        let mut cites = atom("The second overlay landed.");
        cites.insert("entities".into(), json!(["deed-patch-overlay-v2"]));
        svc.add(cites).unwrap();
        assert!(svc.citers("w", "deed-patch-overlay").unwrap().is_empty());
    }

    /// Both directions of the join, over one pack.
    #[test]
    fn what_a_pack_cites_and_who_cites_it_agree() {
        let (_dir, svc) = service();
        let mut cites = atom("The overlay landed as a frozen deed.");
        cites.insert(
            "entities".into(),
            json!(["deed-patch-overlay", "sha256:abc"]),
        );
        svc.add(cites).unwrap();

        let listed = svc.accessions("w").unwrap();
        assert_eq!(listed, vec!["deed-patch-overlay", "sha256:abc"]);
        for accession in listed {
            assert_eq!(svc.citers("w", &accession).unwrap().len(), 1, "{accession}");
        }
    }
}
