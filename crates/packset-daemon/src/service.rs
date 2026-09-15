//! What the writer does, apart from how a client asked.
//!
//! The HTTP layer decodes and encodes; everything a verb actually means lives
//! here, so the rules can be tested without a socket.

use std::collections::BTreeMap;
use std::sync::Mutex;

use packset_core::clock;
use packset_core::record::{self, AtomError, MEMORY_CAP, USER_CAP};
use serde_json::{json, Map, Value};

use crate::cards;
use crate::home::Home;
use crate::store::{Record, Store};

/// The cap on one attached body.
pub const ATTACH_CAP: usize = 200_000;

/// One workspace's pending attachment.
#[derive(Debug, Clone, Default)]
pub struct Attachment {
    /// The body.
    pub text: String,
    /// What it is, for a reader.
    pub label: String,
}

/// The writer: the store, the cards, and the one-shot attach slots.
/// How many search hits seed an activation.
const ACTIVATION_SEEDS: usize = 5;
/// How far activation spreads along the links.
const ACTIVATION_HOPS: usize = 2;
/// The most claims one consolidation bucket may hold before it is passed
/// over; a shared head of three words that a thousand claims open with is
/// not a rewrite candidate list.
const CONSOLIDATE_BUCKET_MAX: usize = 64;
/// How many of an island's strongest claims fire together when asked.
const FIRE_TOP: usize = 8;

pub struct Service {
    home: Home,
    store: Store,
    attach: Mutex<BTreeMap<String, Attachment>>,
    /// One logical write at a time. LMDB serialises transactions, not the
    /// read-check-write a dedupe or a grade is, so two identical remembers
    /// arriving together must not both be stored.
    writes: Mutex<()>,
}

impl Service {
    /// Open the pack at `home`.
    ///
    /// # Errors
    ///
    /// Fails when the store cannot be opened or the lock is held.
    pub fn open(home: Home) -> anyhow::Result<Self> {
        let store = Store::open(home.root())?;
        Ok(Self {
            home,
            store,
            attach: Mutex::new(BTreeMap::new()),
            writes: Mutex::new(()),
        })
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
    /// [`AtomError`] when the text is a tool dump or the record does not
    /// validate, else the store's.
    pub fn add(&self, atom: Record) -> anyhow::Result<Record> {
        // Validation and the encoder run before the lock: the encode is the
        // slow part of a write and depends on the text alone.
        let atom = self.prepare(atom)?;
        let _write = self.writes.lock().unwrap_or_else(|e| e.into_inner());
        self.add_prepared(atom)
    }

    /// Check a claim and fill the fields that depend on nothing stored.
    fn prepare(&self, mut atom: Record) -> anyhow::Result<Record> {
        let text = atom
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if packset_core::extract::is_tool_dump(&text) {
            anyhow::bail!(AtomError("tool dump is attach, not an atom".into()));
        }
        record::validate(&mut atom).map_err(anyhow::Error::new)?;

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

    /// Store a prepared claim, or return the live one that already says it.
    fn add_prepared(&self, mut atom: Record) -> anyhow::Result<Record> {
        let workspace = atom
            .get("workspace")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let named = atom.get("set").and_then(Value::as_str).map(str::to_string);

        // Compared within its own scope; the shared snapshot is narrowed only
        // when the scope excludes something.
        let snapshot = self.store.live(&workspace)?;
        let narrowed: Vec<Record>;
        let live: &[Record] = match named.as_deref() {
            Some(name) => {
                narrowed = snapshot
                    .iter()
                    .filter(|peer| peer.get("set").and_then(Value::as_str) == Some(name))
                    .cloned()
                    .collect();
                &narrowed
            }
            None if snapshot.iter().any(|peer| peer.contains_key("set")) => {
                narrowed = snapshot
                    .iter()
                    .filter(|peer| !peer.contains_key("set"))
                    .cloned()
                    .collect();
                &narrowed
            }
            None => &snapshot,
        };
        for existing in live {
            if existing.get("text") == atom.get("text")
                && existing.get("kind") == atom.get("kind")
                && existing.get("set") == atom.get("set")
            {
                return Ok(existing.clone());
            }
        }

        let now = clock::utcnow();
        let mut closed: Vec<String> = Vec::new();
        let mut batch = Vec::new();
        if record::is_live(&atom, &now) {
            for existing in live {
                if record::replaces(&atom, existing) {
                    let mut peer = existing.clone();
                    record::close_valid_to(&mut peer, &now);
                    peer.insert("ts".into(), Value::String(clock::utcnow()));
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
        if record::is_live(&atom, &now) {
            // Closed peers stay out of apply_links: a rewrite of links would
            // otherwise write them back without valid_to.
            let remaining: Vec<Record>;
            let peers: &[Record] = if closed.is_empty() {
                live
            } else {
                remaining = live
                    .iter()
                    .filter(|peer| {
                        peer.get("id")
                            .and_then(Value::as_str)
                            .map(|id| !closed.iter().any(|c| c == id))
                            .unwrap_or(true)
                    })
                    .cloned()
                    .collect();
                &remaining
            };
            let rewritten = record::apply_links(&mut atom, peers, record::LINK_THRESHOLD, &now);
            for mut peer in rewritten {
                peer.insert("ts".into(), Value::String(clock::utcnow()));
                batch.push(peer);
            }
        } else if !atom.contains_key("links") {
            atom.insert("links".into(), Value::Array(Vec::new()));
        }
        // A new claim enters the review clock at once; a trust row is not
        // recalled, it is weighed.
        if record::is_live(&atom, &now)
            && atom.get("kind").and_then(Value::as_str) != Some("trust")
            && atom
                .get("due_at")
                .and_then(Value::as_str)
                .unwrap_or("")
                .is_empty()
        {
            record::schedule_review(&mut atom, &now, record::Grade::Initial, None);
        }
        let mut all = vec![atom.clone()];
        all.append(&mut batch);
        self.store.upsert_many(&all)?;
        self.project_atoms(&all);
        Ok(atom)
    }

    /// Fill the vector slot when this seat has an encoder; null otherwise.
    fn encode_into(&self, atom: &mut Record) {
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
        let mut updated = current
            .iter()
            .find(|a| a.get("id").and_then(Value::as_str) == Some(id))
            .cloned()
            .ok_or_else(|| anyhow::Error::new(AtomError(format!("no current atom {id}"))))?;
        for (key, value) in fields {
            updated.insert(key.clone(), value.clone());
        }
        updated.insert("id".into(), Value::String(id.to_string()));
        updated.insert("workspace".into(), Value::String(workspace.to_string()));
        updated.insert("ts".into(), Value::String(clock::utcnow()));
        updated.insert("tombstone".into(), Value::Bool(false));
        record::validate(&mut updated).map_err(anyhow::Error::new)?;

        let now = clock::utcnow();
        let mut batch = Vec::new();
        if record::is_live(&updated, &now) {
            let rewritten =
                record::apply_links(&mut updated, &current, record::LINK_THRESHOLD, &now);
            for mut peer in rewritten {
                peer.insert("ts".into(), Value::String(clock::utcnow()));
                batch.push(peer);
            }
        } else if !updated.contains_key("links") {
            updated.insert("links".into(), Value::Array(Vec::new()));
        }
        let mut all = vec![updated.clone()];
        all.append(&mut batch);
        self.store.upsert_many(&all)?;
        self.project_atoms(&all);
        Ok(updated)
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
            .cloned()
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
        self.update_unlocked(workspace, id, &fields)
    }

    /// Tombstone one atom and drop it from the projection.
    ///
    /// `why` names the deed that withdrew the claim. A retraction cites a deed
    /// or nothing, so unlike an entity it is refused when it is free text: the
    /// point of writing it is that `deedar evidence` can be asked about it, and
    /// a name no deed store answers for is a citation that only looks like one.
    ///
    /// # Errors
    ///
    /// [`AtomError`] when `why` is not a deed accession, else the store's.
    pub fn delete_atom(
        &self,
        workspace: &str,
        id: &str,
        why: Option<&str>,
    ) -> anyhow::Result<Record> {
        let _write = self.writes.lock().unwrap_or_else(|e| e.into_inner());
        let why = match why.map(str::trim).filter(|w| !w.is_empty()) {
            Some(w) if !packset_core::atom::is_accession(w) => {
                return Err(anyhow::Error::new(AtomError(format!(
                    "{w} is not a deed accession; a retraction cites deed-<kind>-<slug> or sha256:<hash>"
                ))))
            }
            other => other,
        };
        let tomb = self.store.delete(workspace, id, why)?;
        let _ = crate::milli::delete(&[id.to_string()], &self.home.milli_dir());
        Ok(tomb)
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
                    "atoms": self.store.current(workspace, Some(&named))?,
                }))
            }
            None => Ok(json!({
                "workspace": workspace,
                "user": cards::read_text(&self.home.user_path()),
                "memory": cards::read_text(&self.home.memory_path(workspace)),
                "atoms": self.store.current(workspace, None)?,
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

    /// Hold one body for a workspace, capped.
    pub fn put_attach(&self, workspace: &str, text: &str, label: &str) -> Value {
        let mut body = text.to_string();
        if body.chars().count() > ATTACH_CAP {
            body = body.chars().take(ATTACH_CAP).collect();
        }
        let slot = Attachment {
            text: body,
            label: label.trim().to_string(),
        };
        let mut held = self.attach.lock().expect("attach lock");
        held.insert(workspace.to_string(), slot.clone());
        json!({"workspace": workspace, "text": slot.text, "label": slot.label})
    }

    /// Take the held body, leaving the slot empty: an attachment is one turn's.
    pub fn take_attach(&self, workspace: &str) -> Option<Attachment> {
        let mut held = self.attach.lock().expect("attach lock");
        held.remove(workspace)
    }

    /// Read the held body without taking it.
    pub fn peek_attach(&self, workspace: &str) -> Option<Attachment> {
        let held = self.attach.lock().expect("attach lock");
        held.get(workspace).cloned()
    }

    /// Keep the projection level with a write: upsert a live atom, delete one
    /// that left the live set. No-op without a search binary.
    fn project_atoms(&self, atoms: &[Record]) {
        let dir = self.home.milli_dir();
        let now = clock::utcnow();
        let mut live = Vec::new();
        let mut dead = Vec::new();
        for atom in atoms {
            let Some(id) = atom.get("id").and_then(Value::as_str) else {
                continue;
            };
            if record::is_live(atom, &now) || record::is_due(atom, &now) {
                live.push(crate::milli::atom_document(atom));
            } else {
                dead.push(id.to_string());
            }
        }
        if !live.is_empty() {
            let _ = crate::milli::upsert(&live, &dir);
        }
        if !dead.is_empty() {
            let _ = crate::milli::delete(&dead, &dir);
        }
    }

    /// Keep the projection's copy of the cards level with a write.
    fn project_cards(&self, workspace: Option<&str>) {
        let dir = self.home.milli_dir();
        let workspace = workspace.unwrap_or("");
        let user = cards::read_text(&self.home.user_path());
        let memory = if workspace.is_empty() {
            String::new()
        } else {
            cards::read_text(&self.home.memory_path(workspace))
        };
        let docs = crate::milli::pack_documents(workspace, &user, &memory, &[]);
        if !docs.is_empty() {
            let _ = crate::milli::upsert(&docs, &dir);
        }
    }

    /// The atoms that were live at `at`, over the `valid_from` / `valid_to` window.
    ///
    /// # Errors
    ///
    /// The store's, or a stamp `parse_millis` will not accept.
    pub fn as_of(&self, workspace: &str, at: &str) -> anyhow::Result<Value> {
        let at =
            clock::canonicalize(at).ok_or_else(|| anyhow::anyhow!("as_of must be a timestamp"))?;
        let atoms = self.store.as_of(workspace, &at)?;
        Ok(json!({ "atoms": atoms, "as_of": at }))
    }

    /// Ranked hits, and which engine produced them: the projection when
    /// present, else the linear scan; any projection failure falls back whole.
    /// `as_of` retrieves the atoms live then; `rerank` runs the cross-encoder
    /// second stage.
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
    ) -> anyhow::Result<Value> {
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
        let (atoms, index, documents) = match dated {
            Some(scan) => {
                let atoms = std::sync::Arc::new(scan?);
                let documents: std::sync::Arc<Vec<Vec<String>>> = std::sync::Arc::new(
                    atoms
                        .iter()
                        .map(packset_core::search::atom_tokens)
                        .collect(),
                );
                let index = std::sync::Arc::new(packset_core::bm25::Index::build(
                    documents.iter().map(Vec::as_slice),
                ));
                (atoms, index, documents)
            }
            None => self.store.searchable(workspace)?,
        };

        if packset_core::search::tokens(query).is_empty() {
            // Nothing to score, so the second stage does not run. Reporting
            // it as off rather than as a stage that ran over an empty list
            // keeps a client from thinking a model was asked.
            return Ok(json!({"hits": [], "engine": "linear", "as_of": as_of, "rerank": "off"}));
        }

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
            now: &now,
        };
        let ranked_terms = packset_core::search::search_bm25(&ask, &index);
        // A third ballot when this seat has an encoder. The two lexical
        // scorers both need the question and the atom to share words, and this
        // one does not, which is the gap it exists to close.
        let ranked_meaning = crate::embed::encode_query(query)
            .map(|vector| packset_core::search::search_dense(&ask, &vector))
            .filter(|hits| !hits.is_empty());

        // The milli projection is live-now. A dated question over a closed
        // window would otherwise miss the atom the retrieve just found.
        let projected = if as_of.is_some() {
            None
        } else {
            crate::milli::search(corpus, query, first_limit, &dir, scope)
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
                    packset_core::search::merge_ballots(&ballots, first_limit, panel, &now),
                    "milli",
                )
            }
            None => {
                let lexical = packset_core::search::search_linear_with(&ask, &documents);
                let mut ballots = vec![lexical, ranked_terms];
                ballots.extend(ranked_meaning);
                let engine = if ballots.len() > 2 { "dense" } else { "linear" };
                (
                    packset_core::search::merge_ballots(&ballots, first_limit, panel, &now),
                    engine,
                )
            }
        };
        // The same stage the locomo arm measures. Off unless asked. An absent
        // or broken reranker leaves the first-stage order, the same way an
        // absent encoder leaves the dense ballot out.
        let stage = if rerank && !ranked.is_empty() {
            match crate::embed::rerank_hits(query, &ranked) {
                Some(reordered) => {
                    ranked = reordered;
                    "cross-encoder"
                }
                None => "absent",
            }
        } else {
            "off"
        };
        ranked.truncate(limit);
        // Due is the clock (`/v1/due`, `ljos due`). Mixing it into search
        // put 212 due personas at score 3.1 on every query.
        Ok(json!({"hits": ranked, "engine": engine, "as_of": as_of, "rerank": stage}))
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
        let atoms = self.store.live(workspace)?;
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
        let atoms = self.store.live(workspace)?;
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
        let _write = self.writes.lock().unwrap_or_else(|e| e.into_inner());
        let live = self.store.live(workspace)?;
        let mut atoms: Vec<Record> = live.iter().cloned().collect();
        let fired: Vec<usize> = ids
            .iter()
            .filter_map(|id| {
                atoms
                    .iter()
                    .position(|a| a.get("id").and_then(Value::as_str) == Some(id.as_str()))
            })
            .collect();
        let changed = packset_core::island::fire(&mut atoms, &fired);
        if !changed.is_empty() {
            let now = clock::utcnow();
            let batch: Vec<Record> = changed
                .iter()
                .map(|&i| {
                    let mut atom = atoms[i].clone();
                    atom.insert("ts".into(), Value::String(now.clone()));
                    atom
                })
                .collect();
            self.store.upsert_many(&batch)?;
            self.project_atoms(&batch);
        }
        Ok(json!({"fired": fired.len(), "changed": changed.len()}))
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
        let mut atoms: Vec<Record> = live.iter().cloned().collect();
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
                touched.insert(*i);
                touched.insert(*j);
            }
            let batch: Vec<Record> = touched
                .into_iter()
                .map(|k| {
                    let mut atom = atoms[k].clone();
                    atom.insert("ts".into(), Value::String(now.clone()));
                    atom
                })
                .collect();
            self.store.upsert_many(&batch)?;
            self.project_atoms(&batch);
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
        let seeds = self.search(workspace, query, ACTIVATION_SEEDS, None, panel, None, false)?;
        let atoms = self.store.live(workspace)?;
        let graph = packset_core::island::Graph::from_atoms(&atoms);
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
        let fired = if fire && !weak {
            let ids: Vec<String> = lit
                .iter()
                .take(FIRE_TOP)
                .filter_map(|(at, _)| atoms[*at].get("id").and_then(Value::as_str))
                .map(str::to_string)
                .collect();
            self.fire(workspace, &ids)?["changed"].as_u64().unwrap_or(0)
        } else {
            0
        };
        Ok(json!({
            "island": island,
            "seeds": weighted.len(),
            "agreed_seeds": agreed,
            "weak": weak,
            "dense": crate::embed::binary().is_some(),
            "hops": ACTIVATION_HOPS,
            "fired": fired,
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
    /// # Errors
    ///
    /// The scan's.
    pub fn status(
        &self,
        workspace: Option<&str>,
        panel: &packset_core::Panel,
    ) -> anyhow::Result<Value> {
        let now = clock::utcnow();
        let mut live: BTreeMap<String, usize> = BTreeMap::new();
        let mut tomb: BTreeMap<String, usize> = BTreeMap::new();
        let mut expired: BTreeMap<String, usize> = BTreeMap::new();
        let mut last_write = String::new();
        for rec in self.store.scan(workspace)? {
            let kind = rec
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            if let Some(ts) = rec.get("ts").and_then(Value::as_str) {
                if ts > last_write.as_str() {
                    last_write = ts.to_string();
                }
            }
            if rec
                .get("tombstone")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                *tomb.entry(kind).or_insert(0) += 1;
            } else if record::is_live(&rec, &now) {
                *live.entry(kind).or_insert(0) += 1;
            } else {
                *expired.entry(kind).or_insert(0) += 1;
            }
        }
        let milli_dir = self.home.milli_dir();
        let index_ready = crate::milli::index_ready(&milli_dir);
        let pin = workspace.map(|w| self.pin(w)).unwrap_or_default();
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
            "tombstone": tomb.values().sum::<usize>(),
            "expired": expired.values().sum::<usize>(),
            "live_by_kind": live,
            "tombstone_by_kind": tomb,
            "expired_by_kind": expired,
            "last_write_ts": if last_write.is_empty() { Value::Null } else { Value::String(last_write) },
            "milli": {
                "binary": milli_binary(),
                "index_dir": milli_dir.display().to_string(),
                "index_ready": index_ready,
            },
            "embedder": {
                "enabled": embed_enabled(),
                "binary": crate::embed::binary().map(|path| path.display().to_string()),
                "available": embed_enabled() && crate::embed::binary().is_some(),
            },
            // Off unless the host asked. The locomo cost lives in the README;
            // status only says whether this writer will spend it.
            "rerank": {
                "enabled": crate::embed::wanted(),
                "available": crate::embed::binary().is_some(),
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
        }))
    }
}

/// Whether the dense-rank embedder is switched on.
///
/// Switched on and present are different questions. The runtime behind the
/// encoder lives in its own binary, so a seat can have it enabled with nothing
/// to run; keyword search does not depend on either.
fn embed_enabled() -> bool {
    let raw = std::env::var("INSIDE_EMBED").unwrap_or_else(|_| "on".into());
    !matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "0" | "off" | "none" | "false" | "no"
    )
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
    fn a_tool_dump_is_refused_as_an_atom() {
        let (_dir, svc) = service();
        let listing = std::iter::once("total 48".to_string())
            .chain((0..7).map(|i| format!("-rw-r--r-- 1 x x 0 Jan 1 00:00 file{i}")))
            .collect::<Vec<_>>()
            .join("\n");
        let err = svc.add(atom(&listing)).unwrap_err();
        assert!(err.to_string().contains("attach"), "{err}");

        // A fenced capture naming a stream is the other shape.
        let fenced = atom("Here is the run:\n```\nstdout: everything fine\n```");
        assert!(svc.add(fenced).is_err());
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
    fn add_seeds_the_review_clock_except_for_trust() {
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
            .search("w", "Borda", 8, None, &panel, None, false)
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

    #[test]
    fn a_retraction_cites_a_deed_or_nothing() {
        let (_dir, svc) = service();
        let stored = svc.add(atom("The overlay landed.")).unwrap();
        let id = stored["id"].as_str().unwrap().to_string();
        let refused = svc.delete_atom("w", &id, Some("because I said so"));
        assert!(refused.is_err(), "free text passed as a citation");
        // Refusing the citation refuses the whole write; the atom is still live.
        assert!(svc
            .delete_atom("w", &id, Some("deed-patch-overlay"))
            .is_ok());
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

    #[test]
    fn an_attachment_is_capped() {
        let (_dir, svc) = service();
        let huge = "x".repeat(ATTACH_CAP + 100);
        let slot = svc.put_attach("w", &huge, "");
        assert_eq!(slot["text"].as_str().unwrap().chars().count(), ATTACH_CAP);
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

    /// The seat search path does not run the measured second stage unless
    /// it is asked. The default is the first-stage ranking a pack already
    /// returns.
    #[test]
    fn search_leaves_the_cross_encoder_off() {
        let (_dir, svc) = service();
        svc.add(atom("Reviews open with a check.")).unwrap();
        let panel = packset_core::Panel::default();
        let found = svc
            .search("w", "reviews", 8, None, &panel, None, false)
            .unwrap();
        assert_eq!(found["rerank"], json!("off"), "{found}");
        assert_eq!(found["hits"].as_array().map(Vec::len), Some(1));
        let empty = svc.search("w", "", 8, None, &panel, None, true).unwrap();
        assert_eq!(empty["rerank"], json!("off"), "{empty}");
        assert!(empty["hits"].as_array().unwrap().is_empty());
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
        let _guard = EMBED.lock().unwrap_or_else(|e| e.into_inner());
        crate::embed::reset_for_test();
        let stub = broken_reranker();
        let old = std::env::var_os("PACKSET_EMBED");
        // Safety: EMBED is held, so no other test mutates this variable.
        unsafe { std::env::set_var("PACKSET_EMBED", &stub.path) };
        let off = svc
            .search("w", "reviews search", 8, None, &panel, None, false)
            .unwrap();
        let on = svc
            .search("w", "reviews search", 8, None, &panel, None, true)
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
        let _guard = EMBED.lock().unwrap_or_else(|e| e.into_inner());
        crate::embed::reset_for_test();
        let stub = scoring_reranker();
        let old = std::env::var_os("PACKSET_EMBED");
        // Safety: EMBED is held, so no other test mutates this variable.
        unsafe { std::env::set_var("PACKSET_EMBED", &stub.path) };
        let off = svc
            .search("w", "reviews search", 8, None, &panel, None, false)
            .unwrap();
        let on = svc
            .search("w", "reviews search", 8, None, &panel, None, true)
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
        let _guard = EMBED.lock().unwrap_or_else(|e| e.into_inner());
        crate::embed::reset_for_test();
        let stub = scoring_reranker();
        let old = std::env::var_os("PACKSET_EMBED");
        // Safety: EMBED is held, so no other test mutates this variable.
        unsafe { std::env::set_var("PACKSET_EMBED", &stub.path) };
        let off = svc
            .search("w", "Prefer reviews", 1, None, &panel, None, false)
            .unwrap();
        let on = svc
            .search("w", "Prefer reviews", 1, None, &panel, None, true)
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

    static EMBED: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
